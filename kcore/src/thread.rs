//! Thread objects and their terminal reports (spec rev1§5.1, rev1§5.3).
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
// `store.tcb_view()`/`notif_view()`/… in the verified contracts, and `TcbView` appears
// in `bind`'s `ensures`; both erase in a normal build, so they are otherwise unused here.
#[allow(unused_imports)]
use crate::cspace::{StoreSpec, TcbView};

/// The terminal report record (rev1§5.1), preallocated in the TCB so death
/// delivery never allocates (rev1§3.6). One transition ever: Running →
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
        TrapFrame {
            x: [0; 31],
            sp_el0: 0,
            elr: 0,
            spsr: 0,
        }
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
    /// Waiting on a notification word (rev1§3.6).
    BlockedNotif,
    /// Exited or killed; never scheduled again.
    Halted,
    /// Took an unhandled fault; suspended, not destroyed (rev1§5.3).
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
    /// on-exit / on-fault binding slots (rev1§5.1): real, CDT-visible cap
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

/// Record the terminal report and fire the matching binding (rev1§5.1).
/// pre:  r is Exited or Faulted; the caller has already moved t out of
///       Running (Halted / Faulted).
/// post: first call wins — the record holds r and the binding fired
///       exactly once; later calls are no-ops. An empty binding slot is
///       one the holder never configured or one revoke already cleared:
///       signaling nothing is a no-op (rev1§5.1). A non-empty slot's cap
///       holds a ref, so the notification it names is necessarily live.
///
/// Verus proves the two rev1§5.1 properties.
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
            // The cap is a notification, so the `requires` conditional fires at `nn = n`.
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

/// Set a thread's rev1§5.4 run priority, bounded by the spawner's cap ceiling —
/// **refusing** the write when the requested priority exceeds the ceiling.
/// The spawn path passes `ceiling = cap_max_prio(thread_cap)`, so the rev1§5.4
/// cap-attenuation is carried into the model and the *refusal decision* is a
/// machine-checked branch here, not an unverified shell `if` (rev1§6.1(d)): an
/// over-ceiling request returns `Err` and leaves the thread untouched, an
/// accepted one writes the priority through the `tcb_view` seam with a reachable
/// post-state `priority == prio` (hence `<= ceiling`). Only the trusted
/// `Store`-trait realization of `set_tcb_priority` remains, the same posture as
/// every other setter (`set_tcb_report`, `set_tcb_bind_bits`).
///
/// Frames every other view unchanged in both branches (the mutual-frame
/// discipline), so a spawn that calls this leaves cspace/refs/channels/notifs/
/// timers untouched whether it accepts or refuses.
pub fn set_priority<S: Store>(store: &mut S, t: ObjId, prio: u8, ceiling: u8) -> (res: Result<(), ()>)
    requires
        old(store).tcb_view().dom().contains(t),
    ensures
        // Accepted: the written priority is exactly `prio`, hence within the cap
        // ceiling — a reachable `ensures`, not a shell promise.
        res is Ok ==> {
            &&& prio <= ceiling
            &&& final(store).tcb_view() == old(store).tcb_view().insert(
                    t, TcbView { priority: prio, ..old(store).tcb_view()[t] })
            &&& final(store).tcb_view()[t].priority == prio
            &&& final(store).tcb_view()[t].priority <= ceiling
        },
        // Refused: the request was over-ceiling and the thread is untouched.
        res is Err ==> {
            &&& prio > ceiling
            &&& final(store).tcb_view() == old(store).tcb_view()
        },
        // Every non-TCB view is framed in both branches (`set_tcb_priority`
        // frames them on the accept; the refuse mutates nothing).
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
{
    if prio > ceiling {
        return Err(());
    }
    store.set_tcb_priority(t, prio);
    Ok(())
}

/// Configure a binding slot (holder-configured, rev1§3.6): the caller's
/// notification cap MOVES into the TCB slot (rev1§3.4 — duplicate first to
/// keep access), preserving its CDT position so revocation sees it.
/// Rebinding deletes the displaced cap; a `None` src just unbinds.
///
/// pre:  which < 2; notif_src is `None` or a slot holding a notification
///       cap owned by the caller.
///
/// Verus proves this the analog of `channel::bind` in shape (release old /
/// install new / set bits), but — unlike the refcount-only channel
/// binding — the TCB bind slots are CDT-visible cap slots, so it composes the real
/// `cspace::delete` (the notification-cap frame) + the verified
/// `cspace::slot_move`. The bind slot ends holding the moved cap (or empty on a `None`
/// src); `bind_bits[which]` is updated; the object views are framed; `cspace_wf` is
/// preserved.
///
/// **Refcount census.** Exports `refcount_sound(final)`. The displaced-notification `-1` is
/// proven inside `delete` (which re-establishes `refcount_sound`); `set_tcb_bind_bits` writes
/// a non-census field and `slot_move` relocates a cap without changing any object's census,
/// so soundness carries to the exit.
pub fn bind<S: Store>(store: &mut S, t: ObjId, which: usize, notif_src: Option<SlotId>, bits: u64)
    requires
        cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        // `delete` (the displaced-bind-cap teardown, the first mutation) requires
        // `refcount_sound`, `caps_consistent`, and the endpoint-cap census; all hold
        // unmutated from entry to that call.
        cspace::refcount_sound(old(store)),
        cspace::caps_consistent(old(store)),
        cspace::end_caps_sound(old(store)),
        cspace::census_dom_complete(old(store)),
        old(store).tcb_view().dom().contains(t),
        which < 2,
        old(store).tcb_view()[t].bind_bits.len() == 2,
        old(store).tcb_view()[t].bind_slots.len() == 2,
        old(store).slot_view().dom().contains(old(store).tcb_view()[t].bind_slots[which as int]),
        // The displaced cap is empty or a notification, so the `delete` takes the
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
        // Refcount soundness is preserved: the displaced-cap `delete` re-establishes it
        // (and proves the `-1`), `set_tcb_bind_bits` writes a non-census field, and the
        // notification-cap `slot_move` relocates a cap without changing any object's census.
        cspace::refcount_sound(final(store)),
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
    proof {
        // `refcount_sound` holds here: `delete` re-establishes it (its `ensures`), and the
        // empty-slot path is a no-op that leaves the entry census untouched.
        assert(cspace::refcount_sound(store));
    }
    let ghost st1 = *store;
    store.set_tcb_bind_bits(t, which, bits);
    proof {
        // `bind_bits` is no census term: it preserves every thread's chain fields
        // (`waiter_refs`, via `lemma_waiter_refs_frame_fields`) and cspace/aspace
        // (`thread_hold`, via `lemma_thread_hold_frame`), and frames slot/chan/notif/timer.
        assert(st1.tcb_view().dom().contains(t));
        assert(store.tcb_view().dom() == st1.tcb_view().dom());
        assert forall|x: ObjId| #[trigger] cspace::obj_census(store, x)
            == cspace::obj_census(&st1, x) by {
            cspace::lemma_waiter_refs_frame_fields(st1.notif_view(), st1.tcb_view(),
                store.tcb_view(), x);
            cspace::lemma_thread_hold_frame(st1.tcb_view(), store.tcb_view(), x);
        }
        cspace::lemma_refcount_sound_from_census_eq(&st1, store);
    }
    if let Some(src) = notif_src {
        let ghost st2 = *store;
        crate::cspace::slot_move(store, src, slot);
        proof {
            // The cap relocates `src` → `slot`, so `slot_refs`/`frame_map_refs` are preserved
            // (`lemma_cap_move_census`); `slot_move` frames chan/notif/tcb/timer, so the other
            // four census terms are unchanged. Hence the census — and `refcount_sound` — carries.
            assert(store.chan_view() == st2.chan_view());
            assert(store.notif_view() == st2.notif_view());
            assert(store.tcb_view() == st2.tcb_view());
            assert(store.timer_view() == st2.timer_view());
            assert forall|x: ObjId| #[trigger] cspace::obj_census(store, x)
                == cspace::obj_census(&st2, x) by {
                cspace::lemma_cap_move_census(st2.slot_view(), store.slot_view(), src, slot, x);
            }
            cspace::lemma_refcount_sound_from_census_eq(&st2, store);
        }
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
/// the parent needs no letter about its own revoke (rev1§5.1). The record
/// only ever transitions on the thread's own exit or fault.
///
/// The unqueue split: a Runnable thread is removed from the
/// ready structure through `Store::unqueue_ready` (the scheduler is
/// kernel-side); a thread blocked on a notification is unlinked here,
/// since the waiter queue is a kcore object.
///
/// The teardown (detach → halt → bind-slot `delete`s → clear-before-unref cspace/aspace)
/// has a fully proven Verus body verified against the full contract. The contract's
/// structural core: `t` ends `Halted` with its queue link and both binding slots cleared,
/// **its report UNCHANGED** (destruction fires no report, rev1§5.1), and `cspace_wf`
/// preserved. `unqueue_ready` needs no Verus contract (its body is unverified).
//
// **Refcount census.** The contract requires and preserves `refcount_sound` and states the
// `count_nonempty` non-increase the recursion's measure needs: the bind-cap deletes drop
// `slot_refs`, and the `unref_cspace`/`unref_aspace` releases drop `thread_hold_refs`, each
// matched by its `-1`. The body opens the cross-module cycle
// `destroy_tcb → unref_cspace → destroy_cspace → delete → obj_unref → destroy_tcb`, closed under
// the shared lexicographic measure `(count_nonempty(slot_view), height)` with `destroy_tcb = 3`
// (its calls to `delete`/`unref_cspace`/`unref_aspace` are count-flat-or-dropping, the descent by
// height). The halted subject `t`'s `report`/`state`/`qnext` survive the recursion because once
// halted with `wait_notif` cleared it is dead (`refs[t] == 0`) and queue-detached, so the
// `dead_tcb_frozen` frame fixes its TCB; the census rides the **clear-before-unref** discipline
// (`lemma_census_after_hold_clear` opens the off-by-one window `unref_cspace`/`unref_aspace`
// consume).
// `spinoff_prover`: giving `CapKind::Thread` its rev1§5.4 `max_prio` ceiling adds a datatype
// field + the `is_thread_cap_for`/`cap_max_prio` axioms to this module's shared SMT batch,
// shifting Z3's resource accounting for this borderline body. Isolating `destroy_tcb` into its
// own Z3 instance is the standard Verus headroom fix.
//
// `rlimit`: surfacing `priority` in `TcbView` adds yet another field to every `tcb_view()` term
// this teardown carries, pushing the isolated body just past the default 10s budget on some
// platforms. Raising its private resource cap (it is already on its own Z3 instance, so no other
// proof is affected) restores the margin — the proof itself is unchanged.
#[verifier::spinoff_prover]
#[verifier::rlimit(30)]
pub fn destroy_tcb<S: Store>(store: &mut S, t: ObjId)
    requires
        cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        cspace::refcount_sound(old(store)),
        // `t` is already dead (its last designating cap is gone — `obj_unref` calls this only at
        // `refs[t] == 0`). Needed so the cross-object teardown's `dead_tcb_frozen` frame applies to
        // `t` itself, preserving `t`'s `report`/`state`/`qnext` across the recursive
        // `unref_cspace`/`delete`.
        old(store).refs_view().dom().contains(t),
        old(store).refs_view()[t] == 0,
        old(store).tcb_view().dom().contains(t),
        old(store).tcb_view()[t].bind_slots.len() == 2,
        old(store).slot_view().dom().contains(old(store).tcb_view()[t].bind_slots[0]),
        old(store).slot_view().dom().contains(old(store).tcb_view()[t].bind_slots[1]),
        // Cap→object consistency: the body deletes the two bind-slot caps (notification caps)
        // and unrefs the cspace/aspace, so it needs their objects well-formed.
        cspace::caps_consistent(old(store)),
        // The endpoint-cap census: the body's bind-slot `delete`s thread it (the bind caps are
        // notifications, but `delete` requires it unconditionally).
        cspace::end_caps_sound(old(store)),
        // Refs-domain completeness: the body's `delete`s thread it.
        cspace::census_dom_complete(old(store)),
        // The bound cspace is resident-wf: `unref_cspace` needs it to
        // drive the at-zero `destroy_cspace`. The TCB's own Thread cap is already gone by the
        // time this destructor runs, so `obj_unref` supplies it (sourced, in turn, from
        // `delete`'s `caps_consistent` over the live Thread cap).
        old(store).tcb_view()[t].cspace matches Some(cs) ==>
            cspace::cspace_resident_wf(old(store), cs),
        // Waiter-coherence: if `t` is blocked, its `wait_notif` names a
        // `notif_wf` notification — the precondition the BlockedNotif branch's `remove_waiter`
        // needs. Same provenance as the cspace fact: `obj_unref` ← `delete`'s `caps_consistent`
        // over the (now-deleted) live Thread cap's `cap_consistent` clause.
        old(store).tcb_view()[t].state == ThreadState::BlockedNotif ==>
            (old(store).tcb_view()[t].wait_notif matches Some(wn) ==>
                cspace::notif_wf(old(store).notif_view(), old(store).tcb_view(), wn)),
    ensures
        cspace::cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        cspace::count_nonempty(final(store).slot_view())
            <= cspace::count_nonempty(old(store).slot_view()),
        cspace::refcount_sound(final(store)),
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
        cspace::census_dom_complete(final(store)),
        cspace::only_empties(old(store).slot_view(), final(store).slot_view()),
        // Residency is immutable: the bind-cap `delete`s, `unref_cspace`/`unref_aspace`, and
        // `set_tcb_*` all frame `cspace_view` (a destroyed cspace keeps its residency map —
        // its resident caps are emptied, not re-homed), so `obj_unref`'s Thread arm carries it.
        final(store).cspace_view() == old(store).cspace_view(),
        // The channel skeleton (`ring_cap`/`depth`/dom) is immutable: the body deletes bind
        // caps and unrefs cspace/aspace, never touching channel layout.
        cspace::chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        final(store).tcb_view().dom().contains(t),
        final(store).tcb_view()[t].state == ThreadState::Halted,
        final(store).tcb_view()[t].qnext is None,
        // The report is untouched — destruction is the parent acting, not the thread
        // dying, so the record never transitions here (rev1§5.1).
        final(store).tcb_view()[t].report == old(store).tcb_view()[t].report,
        // Both binding slots emptied (their caps die with the TCB by CDT cleanup).
        cspace::is_empty_cap(
            final(store).slot_view()[old(store).tcb_view()[t].bind_slots[0]].cap),
        cspace::is_empty_cap(
            final(store).slot_view()[old(store).tcb_view()[t].bind_slots[1]].cap),
        // Dead, queue-detached TCBs *other than `t`* are frozen. `t` itself is excepted — the
        // body rewrites `tcb[t]` (halts it). `obj_unref`'s Thread arm composes this with
        // `dec_ref` to carry the base `dead_tcb_frozen` up the recursion.
        forall|x: ObjId|
            x != t ==> #[trigger] cspace::dead_tcb_frozen_at(old(store), final(store), x),
        // The home maps are framed: residency immutable, channel skeleton fixed, and the TCB
        // domain + every `bind_slots` survive (the detach/halt/clear edits keep both, the
        // deletes + `unref_cspace` carry them). `obj_unref`'s Thread arm reads it off.
        cspace::home_views_frozen(old(store), final(store)),
        // Home-frame provenance: this destructor empties only `t`'s two bind slots (each homed
        // in `t`) and the homed slots `unref_cspace`'s `destroy_cspace` clears — so every
        // un-homed slot keeps its cap. `obj_unref`'s Thread arm reads it off.
        cspace::unhomed_frozen_free(old(store), final(store)),
        // Dual provenance: every emptied slot was a home handle of a dead object. The two bind
        // slots are homed by `t` itself (dead throughout: `refs[t] == 0`); `unref_cspace`'s cleared
        // residents carry their own witness. `obj_unref`'s Thread arm reads it off.
        cspace::emptied_via_dead_home_free(old(store), final(store)),
        // "Dead stays dead" across the whole teardown (every step decrements/removes objects).
        cspace::refs_death_persist(old(store), final(store)),
    decreases cspace::count_nonempty(old(store).slot_view()), 3int
{
    let ghost st0 = *store;
    let ghost report0 = store.tcb_view()[t].report;
    let ghost cs_opt = store.tcb_view()[t].cspace;
    let ghost a_opt = store.tcb_view()[t].aspace;
    let ghost bs0 = store.tcb_view()[t].bind_slots[0];
    let ghost bs1 = store.tcb_view()[t].bind_slots[1];

    // ── 1. Detach: pull `t` off the ready queue (scheduler-side) or its notification's
    //    waiter chain (a kcore object). Both leave the seven object views — and `refs[t]` —
    //    otherwise intact; the BlockedNotif path additionally splices `t` out of `wn`'s FIFO. ──
    if matches!(store.tcb_state(t), ThreadState::Runnable) {
        store.unqueue_ready(t);
        proof {
            cspace::lemma_dead_tcb_frozen_refl(&st0, store);
            cspace::lemma_thread_off_all_chains(store, t);   // Runnable ⇒ not BlockedNotif
            cspace::lemma_sysinv_frame_equal_views(&st0, store);  // `unqueue_ready` frames every view
            // `unqueue_ready` frames `refs`, so death persists.
            cspace::lemma_refs_death_persist_from_refs_eq(&st0, store);
        }
    } else if matches!(store.tcb_state(t), ThreadState::BlockedNotif) {
        if let Some(wn) = store.tcb_wait_notif(t) {
            proof {
                // `remove_waiter`'s refs side-condition: a non-empty queue forces `refs[wn] > 0`
                // (a waiter ⇒ `census(wn) >= 1`, pinned by `refcount_sound` + `census_dom_complete`).
                if store.notif_view()[wn].wait_head is Some {
                    cspace::lemma_waiter_refs_pos_from_head(store.notif_view(), store.tcb_view(), wn);
                    cspace::lemma_in_refs_from_census(store, wn);
                }
            }
            crate::notification::remove_waiter(store, wn, t);
            proof {
                // `refcount_sound` rides the splice (`census_delta_frozen`); `refs[t]` is untouched
                // — `wn != t` (a queued `t` makes `refs[wn] >= 1 != 0 == refs[t]`; an unqueued `t`
                // leaves the whole store fixed), so the splice's only `refs` edit misses `t`. The
                // other invariants ride `remove_waiter`'s conditional ensures (antecedents hold).
                cspace::lemma_refcount_sound_from_frozen(&st0, store);
                assert(cspace::caps_consistent(store));
                assert(cspace::end_caps_sound(store));
                assert(cspace::census_dom_complete(store));
                let ws0 = cspace::waiter_seq(st0.notif_view(), st0.tcb_view(), wn);
                if ws0.contains(t) {
                    assert(cspace::waiter_refs(st0.notif_view(), st0.tcb_view(), wn) >= 1);
                    cspace::lemma_in_refs_from_census(&st0, wn);
                    assert(wn != t);
                }
                assert(store.refs_view()[t] == 0);
                // `t` is off every chain: present ⇒ `wait_notif` cleared to `None`; absent ⇒ still
                // `Some(wn)` but provably off `wn`'s queue (the only chain it could be on), with
                // `wn` still `notif_wf` — the third disjunct of `lemma_thread_off_all_chains`.
                if store.tcb_view()[t].wait_notif is Some {
                    // `wait_notif` still `Some` ⇒ the present arm (which clears it) didn't fire ⇒
                    // `!ws0.contains(t)` ⇒ the absent arm froze `notif`/`tcb`.
                    assert(!ws0.contains(t));
                    assert(store.notif_view() == st0.notif_view());
                    assert(store.tcb_view() == st0.tcb_view());
                }
                cspace::lemma_thread_off_all_chains(store, t);
            }
        } else {
            proof {
                cspace::lemma_dead_tcb_frozen_refl(&st0, store);
                cspace::lemma_thread_off_all_chains(store, t);   // wait_notif None
                cspace::lemma_sysinv_frame_equal_views(&st0, store);
                cspace::lemma_refs_death_persist_from_refs_eq(&st0, store);
            }
        }
    } else {
        proof {
            cspace::lemma_dead_tcb_frozen_refl(&st0, store);
            cspace::lemma_thread_off_all_chains(store, t);   // not BlockedNotif
            cspace::lemma_sysinv_frame_equal_views(&st0, store);
            cspace::lemma_refs_death_persist_from_refs_eq(&st0, store);
        }
    }
    let ghost st_detach = *store;
    proof {
        // After the detach: the slot/cspace views are pinned, `t`'s holds/report survived, the
        // four system invariants hold (the detach is a signal-shaped splice or a no-op), and `t`
        // is off every waiter chain (established per-branch above, carried to the `st_detach` snapshot).
        assert(store.tcb_view()[t].cspace == cs_opt);
        assert(store.tcb_view()[t].aspace == a_opt);
        assert(store.tcb_view()[t].report == report0);
        assert(store.refs_view()[t] == 0);
        assert forall|o: ObjId, ws: Seq<ObjId>|
            cspace::waiter_chain(st_detach.notif_view(), st_detach.tcb_view(), o, ws)
            implies !ws.contains(t) by {}
        // Home-frame base: the detach frames `slot_view` (free) and the home maps (home) —
        // `unqueue_ready` frames every view; `remove_waiter` keeps `slot_view`/`cspace_view`/the
        // channel skeleton and the TCB domain + `bind_slots`.
        assert(store.slot_view() == st0.slot_view());
        cspace::lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
        assert(cspace::home_views_frozen(&st0, store));
        // Dual base: the detach frames `slot_view`, so it empties no slot (free frame refl);
        // `refs_death_persist(st0, st_detach)` was established per detach branch above.
        cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
    }

    // ── 2. Halt: clear `t`'s queue/wait links and mark it Halted (report untouched — rev1§5.1).
    //    This makes `t` *dead and queue-detached* (`refs[t] == 0` ∧ `wait_notif is None`), so the
    //    `dead_tcb_frozen` frame fixes `t`'s TCB across every recursive teardown call below. ──
    store.set_tcb_qnext(t, None);
    store.set_tcb_wait_notif(t, None);
    store.set_tcb_state(t, ThreadState::Halted);
    let ghost st_halt = *store;
    proof {
        // `t` is off every waiter chain (Halted ⇒ not BlockedNotif), so the halt edit froze the
        // census (`lemma_census_frame_thread_halt`) — hence `refcount_sound`/`census_dom_complete`
        // ride it — and `caps_consistent` rides via the halt-clear frame (`refs[t] == 0` ⇒ no
        // live `Thread(t)` cap; `t` off all chains both sides).
        // The three `set_tcb_*` setters re-insert the in-domain key `t`, so the tcb domain is
        // unchanged (the dom precondition the halt-frame lemmas need).
        assert(st_detach.tcb_view().dom().contains(t));
        assert(store.tcb_view().dom() =~= st_detach.tcb_view().dom());
        // `t` off all chains: at `st_detach` (carried from the detach branches), and at `st_halt`
        // (now Halted ⇒ not BlockedNotif).
        cspace::lemma_thread_off_all_chains(store, t);
        cspace::lemma_census_frame_thread_halt(&st_detach, store, t);
        cspace::lemma_refcount_sound_from_census_eq(&st_detach, store);
        cspace::lemma_no_live_thread_cap_from_dead(&st_detach, t);
        cspace::lemma_caps_consistent_frame_thread_halt_clear(&st_detach, store, t);
        // `census_dom_complete`/`end_caps_sound` ride the halt (census frozen + `refs` fixed; the
        // endpoint census reads only the framed chan/slot views).
        assert forall|o: ObjId| #[trigger] cspace::obj_census(store, o) >= 1
            implies store.refs_view().dom().contains(o) by {
            assert(cspace::obj_census(&st_detach, o) == cspace::obj_census(store, o));
            cspace::lemma_in_refs_from_census(&st_detach, o);
        }
        assert(cspace::end_caps_sound(store));
        // except-`t` dead-frame of the halt (only `t` moved, `refs` fixed).
        cspace::lemma_dead_tcb_frozen_except_single_t(&st_detach, store, t);
        // and the detach's except-`t` frame (full ⇒ except-`t`), composed to `st0 → st_halt`.
        cspace::lemma_dead_tcb_frozen_to_except(&st0, &st_detach, t);
        cspace::lemma_dead_tcb_frozen_except_trans(&st0, &st_detach, store, t);
        // The halt frames `slot_view` (free) and the home maps (the `set_tcb_*` setters keep
        // `cspace_view`/`chan_view` and the TCB domain + `bind_slots`); compose onto `st0`.
        assert(store.slot_view() == st_detach.slot_view());
        cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_detach, store);
        assert(cspace::home_views_frozen(&st_detach, store));
        cspace::lemma_unhomed_frozen_free_trans(&st0, &st_detach, store);
        cspace::lemma_home_views_frozen_trans(&st0, &st_detach, store);
        // Dual: the halt frames `slot_view` (free refl) and `refs` (death-persist refl, the
        // `set_tcb_*` setters never touch `refs`); compose onto `st0`.
        assert(store.refs_view() == st_detach.refs_view());
        cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st_detach, store);
        cspace::lemma_refs_death_persist_from_refs_eq(&st_detach, store);
        cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_detach, store);
        cspace::lemma_refs_death_persist_trans(&st0, &st_detach, store);
    }

    // ── 3. Delete the two binding caps (they die with the TCB by ordinary CDT cleanup, exactly
    //    as queued caps die with their channel, rev1§3.4). Each `delete` is a visible cluster member;
    //    `t` (dead + detached) is frozen across it, so its TCB survives. ──
    // The bind-slot `delete`s carry the **full** `dead_tcb_frozen(st_halt, ·)` (a `delete` ensures
    // it), composed across the two by `_trans`; this fixes the dead+detached subject `t` itself,
    // and weakens to the except-`t` running frame at the end.
    let s0 = store.tcb_bind_slot(t, 0);
    if !cspace::cap_is_empty(store.slot(s0).cap) {
        crate::cspace::delete(store, s0);
        proof {
            // `s0` is `t`'s bind slot 0 (homed), so `delete`'s target-aware frame is already
            // target-free; `delete` exports `home_views_frozen`.
            assert(cspace::homed_in_tcb(&st_halt, s0)) by {
                assert(st_halt.tcb_view()[t].bind_slots[0] == s0);
            }
            cspace::lemma_unhomed_frozen_free_from_homed(&st_halt, store, s0);
            // Dual: `t` homes `s0` (bind slot 0) at `st_halt`, and `t` is dead (`refs[t] == 0`,
            // monotone-preserved by `delete`'s `refs_death_persist`), so the directly-deleted `s0`
            // carries the death witness `t`. Lift `delete`'s target-aware frame to the free frame.
            assert(cspace::homes_in_tcb(&st_halt, t, s0)) by {
                assert(st_halt.tcb_view().dom().contains(t));
                assert(st_halt.tcb_view()[t].bind_slots[0] == s0);
            }
            assert(cspace::homes(&st_halt, t, s0));
            assert(cspace::dead_obj(&st_halt, t));   // `refs[t] == 0` at `st_halt`
            assert(cspace::dead_obj(store, t));       // `delete`'s `refs_death_persist`
            cspace::lemma_emptied_via_dead_home_free_from_homed(&st_halt, store, s0, t);
        }
    } else {
        proof {
            cspace::lemma_dead_tcb_frozen_refl(&st_halt, store);
            cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_halt, store);
            cspace::lemma_home_views_frozen_refl(&st_halt, store);
            // Dual: the slot was already empty (no `delete`) — free + death-persist refl.
            cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st_halt, store);
            cspace::lemma_refs_death_persist_from_refs_eq(&st_halt, store);
        }
    }
    let ghost st_d0 = *store;
    proof {
        // Compose the bind-slot-0 delete onto the running `st0` frame.
        cspace::lemma_unhomed_frozen_free_trans(&st0, &st_halt, &st_d0);
        cspace::lemma_home_views_frozen_trans(&st0, &st_halt, &st_d0);
        // Dual: compose the bind-slot-0 delete's frame onto the running `st0` frame.
        cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_halt, &st_d0);
        cspace::lemma_refs_death_persist_trans(&st0, &st_halt, &st_d0);
        assert(cspace::dead_tcb_frozen(&st_halt, &st_d0));
        // `t` (dead + detached at `st_halt`) is frozen across the first delete, so it is still in
        // `tcb.dom()` with its `bind_slots` intact; `bs1` is still a live slot (delete preserves
        // `slot.dom`). These are the second `delete`'s preconditions.
        assert(cspace::dead_tcb_frozen_at(&st_halt, store, t));
        assert(store.tcb_view().dom().contains(t));
        assert(store.tcb_view()[t].bind_slots == st_halt.tcb_view()[t].bind_slots);
        assert(store.slot_view().dom().contains(bs1));
    }
    let s1 = store.tcb_bind_slot(t, 1);
    if !cspace::cap_is_empty(store.slot(s1).cap) {
        crate::cspace::delete(store, s1);
        proof {
            // `s1` is `t`'s bind slot 1 (homed) — same homed-lift as bind slot 0.
            assert(cspace::homed_in_tcb(&st_d0, s1)) by {
                assert(st_d0.tcb_view()[t].bind_slots[1] == s1);
            }
            cspace::lemma_unhomed_frozen_free_from_homed(&st_d0, store, s1);
            // Dual: `t` homes `s1` (bind slot 1) at `st_d0`, dead there and after `delete`.
            assert(cspace::homes_in_tcb(&st_d0, t, s1)) by {
                assert(st_d0.tcb_view().dom().contains(t));
                assert(st_d0.tcb_view()[t].bind_slots[1] == s1);
            }
            assert(cspace::homes(&st_d0, t, s1));
            assert(cspace::dead_obj(&st_d0, t));   // `refs[t] == 0` at `st_d0`
            assert(cspace::dead_obj(store, t));      // `delete`'s `refs_death_persist`
            cspace::lemma_emptied_via_dead_home_free_from_homed(&st_d0, store, s1, t);
        }
    } else {
        proof {
            cspace::lemma_dead_tcb_frozen_refl(&st_d0, store);
            cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_d0, store);
            cspace::lemma_home_views_frozen_refl(&st_d0, store);
            cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st_d0, store);
            cspace::lemma_refs_death_persist_from_refs_eq(&st_d0, store);
        }
    }
    proof {
        // Compose the bind-slot-1 delete onto the running `st0` frame.
        cspace::lemma_unhomed_frozen_free_trans(&st0, &st_d0, store);
        cspace::lemma_home_views_frozen_trans(&st0, &st_d0, store);
        // Dual: compose the bind-slot-1 delete's frame onto the running `st0` frame.
        cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_d0, store);
        cspace::lemma_refs_death_persist_trans(&st0, &st_d0, store);
        cspace::lemma_dead_tcb_frozen_trans(&st_halt, &st_d0, store);
        // `t` survived both deletes (dead + detached at `st_halt` ⇒ `dead_tcb_frozen_at` fixes it),
        // and both bind slots ended empty (each delete empties, `only_empties` keeps the other).
        assert(cspace::dead_tcb_frozen_at(&st_halt, store, t));
        assert(store.tcb_view()[t].state == ThreadState::Halted);
        assert(store.tcb_view()[t].qnext is None);
        assert(store.tcb_view()[t].report == report0);
        assert(store.tcb_view()[t].cspace == cs_opt);
        assert(store.tcb_view()[t].aspace == a_opt);
        assert(cspace::is_empty_cap(store.slot_view()[bs0].cap));
        assert(cspace::is_empty_cap(store.slot_view()[bs1].cap));
        assert(store.refs_view()[t] == 0);
        cspace::lemma_thread_off_all_chains(store, t);
        // running except-`t`(st0, store): except-`t`(st0, st_halt) ∘ (st_halt → store full).
        cspace::lemma_dead_tcb_frozen_to_except(&st_halt, store, t);
        cspace::lemma_dead_tcb_frozen_except_trans(&st0, &st_halt, store, t);
    }

    // ── 4. Release the cspace/aspace holds, **clearing the field before the unref** so the census
    //    drops by one at the held object *before* `refs` does — the `census_off_by_one` window
    //    `unref_cspace`/`unref_aspace` consume (clear-after-unref would leave `refcount_sound`
    //    transiently false the wrong way). Behavior-equivalent: the destructors never read `tcb[t]`. ──
    if let Some(cs) = store.tcb_cspace(t) {
        let ghost st_pre_cs = *store;
        proof {
            // `unref_cspace` needs `cspace_resident_wf(·, cs)`: it holds at entry (the contract's
            // conditional clause, `cs == old.tcb[t].cspace`) and survives the teardown (every op
            // frames `cspace_view` and preserves `slot_view.dom()`).
            assert(st0.tcb_view()[t].cspace == Some(cs));
            assert(cspace::cspace_resident_wf(&st0, cs));
            assert(store.cspace_view() == st0.cspace_view());
            assert(store.slot_view().dom() == st0.slot_view().dom());
            assert(cspace::cspace_resident_wf(store, cs));
        }
        store.set_tcb_cspace(t, None);
        let ghost st_csclear = *store;
        proof {
            assert(st_pre_cs.tcb_view().dom().contains(t));
            assert(store.tcb_view().dom() =~= st_pre_cs.tcb_view().dom());
            cspace::lemma_thread_off_all_chains(&st_pre_cs, t);
            cspace::lemma_thread_off_all_chains(store, t);
            cspace::lemma_census_after_hold_clear(&st_pre_cs, store, t, cs);
            cspace::lemma_no_live_thread_cap_from_dead(&st_pre_cs, t);
            cspace::lemma_caps_consistent_frame_thread_halt_clear(&st_pre_cs, store, t);
            assert(cspace::cspace_resident_wf(store, cs));   // clear frames cspace_view + slot
            assert(cspace::end_caps_sound(store));           // clear frames chan + slot
            cspace::lemma_dead_tcb_frozen_except_single_t(&st_pre_cs, store, t);
            cspace::lemma_dead_tcb_frozen_except_trans(&st0, &st_pre_cs, store, t);
            // `t`'s report/state/qnext survive the single-field clear.
            assert(store.tcb_view()[t].state == ThreadState::Halted);
            assert(store.tcb_view()[t].qnext is None);
            assert(store.tcb_view()[t].report == report0);
            assert(store.refs_view()[t] == 0);
            // The cspace-field clear frames `slot_view` (free) and the home maps; compose.
            assert(store.slot_view() == st_pre_cs.slot_view());
            cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_pre_cs, store);
            assert(cspace::home_views_frozen(&st_pre_cs, store));
            cspace::lemma_unhomed_frozen_free_trans(&st0, &st_pre_cs, store);
            cspace::lemma_home_views_frozen_trans(&st0, &st_pre_cs, store);
            // Dual: the cspace-field clear frames `slot_view` (free refl) and `refs`
            // (death-persist refl, `set_tcb_cspace` never touches `refs`); compose onto `st0`.
            assert(store.refs_view() == st_pre_cs.refs_view());
            cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st_pre_cs, store);
            cspace::lemma_refs_death_persist_from_refs_eq(&st_pre_cs, store);
            cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_pre_cs, store);
            cspace::lemma_refs_death_persist_trans(&st0, &st_pre_cs, store);
        }
        crate::cspace::unref_cspace(store, cs);
        proof {
            // full frame (`unref_cspace` ensures) ⇒ `t` frozen + running except-`t`.
            assert(cspace::dead_tcb_frozen_at(&st_csclear, store, t));
            cspace::lemma_dead_tcb_frozen_to_except(&st_csclear, store, t);
            cspace::lemma_dead_tcb_frozen_except_trans(&st0, &st_csclear, store, t);
            // `unref_cspace` exports the free + home frames; compose onto `st0`.
            cspace::lemma_unhomed_frozen_free_trans(&st0, &st_csclear, store);
            cspace::lemma_home_views_frozen_trans(&st0, &st_csclear, store);
            // Dual: `unref_cspace` exports the free + death-persist frames; compose onto `st0`.
            cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_csclear, store);
            cspace::lemma_refs_death_persist_trans(&st0, &st_csclear, store);
        }
    }
    proof {
        assert(store.tcb_view()[t].state == ThreadState::Halted);
        assert(store.tcb_view()[t].qnext is None);
        assert(store.tcb_view()[t].report == report0);
        assert(store.refs_view()[t] == 0);
        cspace::lemma_thread_off_all_chains(store, t);
    }
    if let Some(a) = store.tcb_aspace(t) {
        let ghost st_pre_as = *store;
        store.set_tcb_aspace(t, None);
        let ghost st_asclear = *store;
        proof {
            assert(st_pre_as.tcb_view().dom().contains(t));
            assert(store.tcb_view().dom() =~= st_pre_as.tcb_view().dom());
            cspace::lemma_thread_off_all_chains(&st_pre_as, t);
            cspace::lemma_thread_off_all_chains(store, t);
            cspace::lemma_census_after_hold_clear_aspace(&st_pre_as, store, t, a);
            cspace::lemma_no_live_thread_cap_from_dead(&st_pre_as, t);
            cspace::lemma_caps_consistent_frame_thread_halt_clear(&st_pre_as, store, t);
            assert(cspace::end_caps_sound(store));
            cspace::lemma_dead_tcb_frozen_except_single_t(&st_pre_as, store, t);
            cspace::lemma_dead_tcb_frozen_except_trans(&st0, &st_pre_as, store, t);
            assert(store.tcb_view()[t].state == ThreadState::Halted);
            assert(store.tcb_view()[t].qnext is None);
            assert(store.tcb_view()[t].report == report0);
            assert(store.refs_view()[t] == 0);
            // The aspace-field clear frames `slot_view` (free) and the home maps; compose.
            assert(store.slot_view() == st_pre_as.slot_view());
            cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_pre_as, store);
            assert(cspace::home_views_frozen(&st_pre_as, store));
            cspace::lemma_unhomed_frozen_free_trans(&st0, &st_pre_as, store);
            cspace::lemma_home_views_frozen_trans(&st0, &st_pre_as, store);
            // Dual: the aspace-field clear frames `slot_view` (free refl) and `refs`
            // (death-persist refl); compose onto `st0`.
            assert(store.refs_view() == st_pre_as.refs_view());
            cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st_pre_as, store);
            cspace::lemma_refs_death_persist_from_refs_eq(&st_pre_as, store);
            cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_pre_as, store);
            cspace::lemma_refs_death_persist_trans(&st0, &st_pre_as, store);
        }
        crate::cspace::unref_aspace(store, a);
        proof {
            assert(cspace::dead_tcb_frozen_at(&st_asclear, store, t));
            cspace::lemma_dead_tcb_frozen_to_except(&st_asclear, store, t);
            cspace::lemma_dead_tcb_frozen_except_trans(&st0, &st_asclear, store, t);
            // `unref_aspace` frames `slot_view` + every object view — free + home refl; compose.
            cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_asclear, store);
            cspace::lemma_home_views_frozen_refl(&st_asclear, store);
            cspace::lemma_unhomed_frozen_free_trans(&st0, &st_asclear, store);
            cspace::lemma_home_views_frozen_trans(&st0, &st_asclear, store);
            // Dual: `unref_aspace` exports the free + death-persist frames; compose onto `st0`.
            cspace::lemma_emptied_via_dead_home_free_trans(&st0, &st_asclear, store);
            cspace::lemma_refs_death_persist_trans(&st0, &st_asclear, store);
        }
    }
}

} // verus!
