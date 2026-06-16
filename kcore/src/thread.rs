//! Thread objects and their terminal reports (spec §5.1, §5.3).
//!
//! kcore owns the thread *object*: the TCB layout, the trap frame (plain
//! data), the report state machine, the on-exit/on-fault binding slots, and
//! the waiter-queue links. The *scheduler* — ready queues, `maybe_switch`,
//! the context switch, `CURRENT`, the idle WFI loop — stays in the `kernel`
//! crate (`kernel/src/thread.rs`); it touches the TCB fields directly and
//! reaches the object logic here for teardown via the [`Store`] seam.
//!
//! Single-core; the kernel is non-preemptible (IRQs masked at EL1), so the
//! scheduler is only ever invoked at exception boundaries.

use crate::cspace::{self, CapKind, CapSlot, ObjHeader};
use crate::id::{ObjId, SlotId};
use crate::store::Store;
use vstd::prelude::*;
// `StoreSpec` (the `external_trait_extension`) must be in scope to resolve
// `store.tcb_view()`/`notif_view()`/… in the §4d contracts, and `TcbView` appears in
// `bind`'s `ensures`; both erase in a normal build, so they are otherwise unused here
// (the doc/results/26 §2.3 idiom).
#[allow(unused_imports)]
use crate::cspace::{StoreSpec, TcbView};

/// The terminal report record (§5.1), preallocated in the TCB so death
/// delivery never allocates (§3.6). One transition ever: Running →
/// Exited | Faulted — suspend-on-fault means no second fault, and a
/// halted thread never runs again, but `report_terminal` guards anyway
/// so the state machine doesn't depend on scheduler invariants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Report {
    Running,
    Exited(u64),
    Faulted { cause: u64, far: u64 },
}

/// Saved EL0 register state. Layout is known to the exception asm:
/// x0..x30 at byte offsets 8*i, then sp_el0, elr, spsr. 272 bytes,
/// 16-aligned.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr: u64,
    pub spsr: u64,
}

impl TrapFrame {
    pub const fn zeroed() -> TrapFrame {
        TrapFrame { x: [0; 31], sp_el0: 0, elr: 0, spsr: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    /// Created, never started.
    Inactive,
    /// In a ready queue.
    Runnable,
    /// The current thread.
    Running,
    /// Waiting on a notification word (§3.6).
    BlockedNotif,
    /// Exited or killed; never scheduled again.
    Halted,
    /// Took an unhandled fault; suspended, not destroyed (§5.3).
    Faulted,
}

#[repr(C)]
pub struct Tcb {
    pub hdr: ObjHeader,
    pub frame: TrapFrame,
    pub state: ThreadState,
    pub priority: u8,
    pub cspace: Option<ObjId>,
    /// Translation tables this thread runs under; `None` = the boot
    /// identity map (idle, the M1 scaffold threads).
    pub aspace: Option<ObjId>,
    /// Ready-queue / notification-wait-queue link (a thread is on at most
    /// one queue, disambiguated by `state`).
    pub qnext: Option<ObjId>,
    pub wait_notif: Option<ObjId>,
    pub report: Report,
    /// on-exit / on-fault binding slots (§5.1): real, CDT-visible cap
    /// slots holding moved-in notification caps, exactly like channel
    /// queue slots — so revoking the notification's lineage sees through
    /// the TCB and empties the slot, and a thread-death firing can only
    /// ever find a live object or an empty slot, never a freed one.
    pub bind_slots: [CapSlot; 2],
    pub bind_bits: [u64; 2],
}

impl Tcb {
    /// Const constructor for boot-static TCBs (init, idle).
    pub const fn empty() -> Tcb {
        Tcb {
            hdr: ObjHeader { refs: 1 },
            frame: TrapFrame::zeroed(),
            state: ThreadState::Inactive,
            priority: 0,
            cspace: None,
            aspace: None,
            qnext: None,
            wait_notif: None,
            report: Report::Running,
            bind_slots: [CapSlot::empty(), CapSlot::empty()],
            bind_bits: [0, 0],
        }
    }

    /// pre:  memory at `this` writable, sized size_of::<Tcb>().
    /// post: inactive thread, refs = 1 (creator cap).
    pub unsafe fn init(this: *mut Tcb) {
        this.write(Tcb::empty());
    }
}

verus! {

pub const BIND_EXIT: usize = 0;
pub const BIND_FAULT: usize = 1;

/// Record the terminal report and fire the matching binding (§5.1).
/// pre:  r is Exited or Faulted; the caller has already moved t out of
///       Running (Halted / Faulted).
/// post: first call wins — the record holds r and the binding fired
///       exactly once; later calls are no-ops. An empty binding slot is
///       one the holder never configured or one revoke already cleared:
///       signaling nothing is a no-op (§5.1). A non-empty slot's cap
///       holds a ref, so the notification it names is necessarily live.
///
/// Verified (plan §4d, doc/results/34): the two §5.1 properties.
/// **ReportMonotone** — the `report != Running` guard makes the transition
/// Running → Exited|Faulted happen **at most once** and terminal states absorbing
/// (a later call is a no-op, the store untouched). **FireSafe** — discharged *by the
/// body verifying*: the empty-slot path fires nothing, and on the notification-cap
/// path the `requires` (a cap-in-slot designates a live, `notif_wf` notification —
/// the first cspace-slot installment of `refcount_sound`, scoped per the per-op-delta
/// precedent, **not** the full census) exactly meets `signal`'s preconditions, so the
/// fired object is provably live, never freed memory. The `wait_notif != Some(nn)`
/// clause is what makes the dying thread provably *not* the woken waiter, so its own
/// report survives the fire.
pub fn report_terminal<S: Store>(store: &mut S, t: ObjId, r: Report)
    requires
        old(store).tcb_view().dom().contains(t),
        !(r matches Report::Running),
        old(store).tcb_view()[t].bind_slots.len() == 2,
        ({
            let which: int = if r matches Report::Exited(_) { BIND_EXIT as int } else { BIND_FAULT as int };
            let slot = old(store).tcb_view()[t].bind_slots[which];
            &&& old(store).slot_view().dom().contains(slot)
            &&& (cspace::cap_notif(old(store).slot_view()[slot].cap) matches Some(nn) ==> {
                    &&& old(store).notif_view().dom().contains(nn)
                    &&& cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), nn)
                    &&& (old(store).notif_view()[nn].wait_head is Some
                            ==> old(store).refs_view().dom().contains(nn) && old(store).refs_view()[nn] > 0)
                    &&& old(store).tcb_view()[t].wait_notif != Some(nn)
                })
        }),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        // ReportMonotone — absorbing: an already-terminal report ⇒ a no-op.
        !(old(store).tcb_view()[t].report matches Report::Running) ==> {
            &&& final(store).refs_view() == old(store).refs_view()
            &&& final(store).notif_view() == old(store).notif_view()
            &&& final(store).tcb_view() == old(store).tcb_view()
        },
        // ReportMonotone — the one transition: a Running report becomes `r` (and any
        // later call hits the absorbing path above — "at most one transition").
        old(store).tcb_view()[t].report matches Report::Running ==>
            final(store).tcb_view()[t].report == r,
{
    match store.tcb_report(t) {
        Report::Running => {}
        _ => { return; }
    }
    store.set_tcb_report(t, r);
    let which = match r {
        Report::Exited(_) => BIND_EXIT,
        Report::Faulted { .. } => BIND_FAULT,
        Report::Running => { return; }
    };
    let slot = store.tcb_bind_slot(t, which);
    let cap = store.slot(slot).cap;
    if let CapKind::Notification(n) = cap.kind {
        let bits = store.tcb_bind_bits(t, which);
        proof {
            // The cap is a notification, so the §4d `requires` conditional fires at `nn = n`.
            assert(cspace::cap_notif(cap) == Some(n));
            // `set_tcb_report` inserts at the resident key `t`, so the TCB domain is
            // unchanged (extensional).
            assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
            // `set_tcb_report` differs from entry only at `t`'s report; `t` is not a
            // waiter on `n` (`requires`), so `n`'s queue well-formedness survives —
            // discharging `signal`'s `notif_wf` precondition.
            cspace::lemma_notif_wf_frame(
                old(store).notif_view(), old(store).tcb_view(),
                store.notif_view(), store.tcb_view(), n);
        }
        crate::notification::signal(store, n, bits);
    }
}

/// Configure a binding slot (holder-configured, §3.6): the caller's
/// notification cap MOVES into the TCB slot (§3.4 — duplicate first to
/// keep access), preserving its CDT position so revocation sees it.
/// Rebinding deletes the displaced cap; a `None` src just unbinds.
///
/// pre:  which < 2; notif_src is `None` or a slot holding a notification
///       cap owned by the caller.
///
/// Verified (plan §4d, doc/results/34): the analog of `channel::bind` (3e) in shape
/// (release old / install new / set bits), but — unlike the refcount-only channel
/// binding — the TCB bind slots are CDT-visible cap slots, so it composes the real
/// `cspace::delete` (the §4d-strengthened notification-cap frame) + the verified
/// `cspace::slot_move`. The bind slot ends holding the moved cap (or empty on a `None`
/// src); `bind_bits[which]` is updated; the object views are framed; `cspace_wf` is
/// preserved. The displaced-notif refs `-1` rides the host test (`check_thread_bind`),
/// not the verified contract (`delete` omits `refs_view`).
pub fn bind<S: Store>(store: &mut S, t: ObjId, which: usize, notif_src: Option<SlotId>, bits: u64)
    requires
        cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        // `delete` (the displaced-bind-cap teardown, the first mutation) requires
        // `refcount_sound` (§6a/§1.3) and `caps_consistent` (§6d foundation); both hold
        // unmutated from entry to that call.
        cspace::refcount_sound(old(store)),
        cspace::caps_consistent(old(store)),
        old(store).tcb_view().dom().contains(t),
        which < 2,
        old(store).tcb_view()[t].bind_bits.len() == 2,
        old(store).tcb_view()[t].bind_slots.len() == 2,
        old(store).slot_view().dom().contains(old(store).tcb_view()[t].bind_slots[which as int]),
        // The displaced cap is empty or a notification, so the `delete` takes the §4d
        // clean notification-cap frame (a bind slot only ever holds a notification cap).
        cspace::is_empty_cap(old(store).slot_view()[old(store).tcb_view()[t].bind_slots[which as int]].cap)
            || cspace::cap_notif(old(store).slot_view()[old(store).tcb_view()[t].bind_slots[which as int]].cap) is Some,
        notif_src matches Some(src) ==> {
            &&& old(store).slot_view().dom().contains(src)
            &&& src != old(store).tcb_view()[t].bind_slots[which as int]
            &&& !cspace::is_empty_cap(old(store).slot_view()[src].cap)
        },
    ensures
        cspace::cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        // `bind_bits[which]` updated; the rest of `t`'s TCB and every other TCB fixed.
        final(store).tcb_view() == old(store).tcb_view().insert(
            t,
            TcbView {
                bind_bits: old(store).tcb_view()[t].bind_bits.update(which as int, bits),
                ..old(store).tcb_view()[t]
            }),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        // The slot effect, split on `notif_src` (read off `delete` + `slot_move`).
        notif_src matches Some(src) ==> {
            &&& final(store).slot_view()[old(store).tcb_view()[t].bind_slots[which as int]].cap
                    == old(store).slot_view()[src].cap
            &&& cspace::is_empty_cap(final(store).slot_view()[src].cap)
            &&& forall|x: SlotId| old(store).slot_view().dom().contains(x)
                    && x != old(store).tcb_view()[t].bind_slots[which as int] && x != src
                    ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap
        },
        notif_src is None ==> {
            &&& cspace::is_empty_cap(final(store).slot_view()[old(store).tcb_view()[t].bind_slots[which as int]].cap)
            &&& forall|x: SlotId| old(store).slot_view().dom().contains(x)
                    && x != old(store).tcb_view()[t].bind_slots[which as int]
                    ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap
        },
{
    let slot = store.tcb_bind_slot(t, which);
    if !cspace::cap_is_empty(store.slot(slot).cap) {
        crate::cspace::delete(store, slot);
    }
    store.set_tcb_bind_bits(t, which, bits);
    if let Some(src) = notif_src {
        crate::cspace::slot_move(store, src, slot);
    }
}

} // verus!

verus! {

/// pre:  refs == 0 (last cap gone).
/// post: t off every queue and never scheduled again. If t is CURRENT the
///       exception exit path will switch away; the TCB memory stays valid
///       until its donor untyped is revoked and reset, which requires
///       deleting this very cap chain first — so no dangling CURRENT.
///
/// Destroying a still-running thread produces NO report and fires
/// nothing: destruction is the parent acting, not the thread dying, and
/// the parent needs no letter about its own revoke (§5.1). The record
/// only ever transitions on the thread's own exit or fault.
///
/// The unqueue split (plan §2.2): a Runnable thread is removed from the
/// ready structure through `Store::unqueue_ready` (the scheduler is
/// kernel-side); a thread blocked on a notification is unlinked here,
/// since the waiter queue is a kcore object.
///
/// **Assumed, host-test-checked (plan §4e — the declared scope-out, §1.4).** Its
/// body recurses through the still-`external_body` `cspace::delete` and the
/// plain-Rust `unref_cspace`/`unref_aspace` (the cross-object teardown, deferred to
/// the post-phase-5 census phase), so — like `delete`/`channel::destroy_channel` —
/// it carries an `external_body` contract checked against its real body in
/// `test_store.rs` (`check_destroy_tcb`), not a Verus body proof. The contract is the
/// robustly-true structural core: `t` ends `Halted` with its queue link and both
/// binding slots cleared, **its report UNCHANGED** (destruction fires no report,
/// §5.1), and `cspace_wf` preserved. `unqueue_ready` therefore needs no Verus
/// contract (the body is unverified) — a small simplification of the §1.3 note.
// **Refcount census (plan §6a).** The contract now also requires and preserves
// `refcount_sound` and states the `count_nonempty` non-increase 6d's measure needs:
// the bind-cap deletes drop `slot_refs`, and the `unref_cspace`/`unref_aspace`
// releases drop `thread_hold_refs`, each matched by its `-1` (6d closes the body).
// Stated now (still `external_body`, host-checked) so `obj_unref` (6c) verifies
// against the final contract.
#[verifier::external_body]
pub fn destroy_tcb<S: Store>(store: &mut S, t: ObjId)
    requires
        cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        cspace::refcount_sound(old(store)),
        old(store).tcb_view().dom().contains(t),
        old(store).tcb_view()[t].bind_slots.len() == 2,
        old(store).slot_view().dom().contains(old(store).tcb_view()[t].bind_slots[0]),
        old(store).slot_view().dom().contains(old(store).tcb_view()[t].bind_slots[1]),
        // Cap→object consistency (plan §6d foundation): the body deletes the two bind-slot
        // caps (notification caps) and unrefs the cspace/aspace, so it needs their objects
        // well-formed. Assumed here (`external_body`), discharged by the body PR;
        // host-checked (`check_destroy_tcb`).
        cspace::caps_consistent(old(store)),
    ensures
        cspace::cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        cspace::count_nonempty(final(store).slot_view())
            <= cspace::count_nonempty(old(store).slot_view()),
        cspace::refcount_sound(final(store)),
        cspace::caps_consistent(final(store)),
        cspace::only_empties(old(store).slot_view(), final(store).slot_view()),
        final(store).tcb_view().dom().contains(t),
        final(store).tcb_view()[t].state == ThreadState::Halted,
        final(store).tcb_view()[t].qnext is None,
        // The report is untouched — destruction is the parent acting, not the thread
        // dying, so the record never transitions here (§5.1).
        final(store).tcb_view()[t].report == old(store).tcb_view()[t].report,
        // Both binding slots emptied (their caps die with the TCB by CDT cleanup).
        cspace::is_empty_cap(
            final(store).slot_view()[old(store).tcb_view()[t].bind_slots[0]].cap),
        cspace::is_empty_cap(
            final(store).slot_view()[old(store).tcb_view()[t].bind_slots[1]].cap),
{
    if store.tcb_state(t) == ThreadState::Runnable {
        store.unqueue_ready(t);
    } else if store.tcb_state(t) == ThreadState::BlockedNotif {
        if let Some(wn) = store.tcb_wait_notif(t) {
            crate::notification::remove_waiter(store, wn, t);
        }
    }
    store.set_tcb_qnext(t, None);
    store.set_tcb_state(t, ThreadState::Halted);
    // Binding caps die with the TCB by ordinary CDT cleanup, exactly as
    // queued caps die with their channel (§3.4).
    for i in 0..2 {
        let s = store.tcb_bind_slot(t, i);
        if !store.slot(s).cap.is_empty() {
            crate::cspace::delete(store, s);
        }
    }
    if let Some(cs) = store.tcb_cspace(t) {
        crate::cspace::unref_cspace(store, cs);
        store.set_tcb_cspace(t, None);
    }
    if let Some(a) = store.tcb_aspace(t) {
        crate::cspace::unref_aspace(store, a);
        store.set_tcb_aspace(t, None);
    }
}

} // verus!
