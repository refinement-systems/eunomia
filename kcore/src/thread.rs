//! Thread objects and their terminal reports (spec §5.1, §5.3).
//!
//! kcore owns the thread *object*: the TCB layout, the trap frame (plain
//! data), the report state machine, the on-exit/on-fault binding slots, and
//! the waiter-queue links. The *scheduler* — ready queues, `maybe_switch`,
//! the context switch, `CURRENT`, the idle WFI loop — stays in the `kernel`
//! crate (`kernel/src/thread.rs`); it touches the TCB fields directly and
//! reaches the object logic here for teardown via the [`Env`] seam.
//!
//! Single-core; the kernel is non-preemptible (IRQs masked at EL1), so the
//! scheduler is only ever invoked at exception boundaries.

use crate::cspace::{CapKind, CapSlot, CSpaceObj, ObjHeader};
use crate::env::Env;
use core::ptr;

pub const BIND_EXIT: usize = 0;
pub const BIND_FAULT: usize = 1;

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
    pub cspace: *mut CSpaceObj,
    /// Translation tables this thread runs under; null = the boot
    /// identity map (idle, the M1 scaffold threads).
    pub aspace: *mut crate::aspace::AspaceObj,
    /// Ready-queue / notification-wait-queue link (a thread is on at most
    /// one queue, disambiguated by `state`).
    pub qnext: *mut Tcb,
    pub wait_notif: *mut crate::notification::NotifObj,
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
            cspace: ptr::null_mut(),
            aspace: ptr::null_mut(),
            qnext: ptr::null_mut(),
            wait_notif: ptr::null_mut(),
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

/// Record the terminal report and fire the matching binding (§5.1).
/// pre:  r is Exited or Faulted; the caller has already moved t out of
///       Running (Halted / Faulted).
/// post: first call wins — the record holds r and the binding fired
///       exactly once; later calls are no-ops. An empty binding slot is
///       one the holder never configured or one revoke already cleared:
///       signaling nothing is a no-op (§5.1). A non-empty slot's cap
///       holds a ref, so the notification it names is necessarily live.
pub unsafe fn report_terminal<E: Env>(t: *mut Tcb, r: Report, env: &mut E) {
    if (*t).report != Report::Running {
        return;
    }
    (*t).report = r;
    let which = match r {
        Report::Exited(_) => BIND_EXIT,
        Report::Faulted { .. } => BIND_FAULT,
        Report::Running => return,
    };
    let slot = ptr::addr_of_mut!((*t).bind_slots[which]);
    if let CapKind::Notification(n) = (*slot).cap.kind {
        crate::notification::signal(n, (*t).bind_bits[which], env);
    }
}

/// Configure a binding slot (holder-configured, §3.6): the caller's
/// notification cap MOVES into the TCB slot (§3.4 — duplicate first to
/// keep access), preserving its CDT position so revocation sees it.
/// Rebinding deletes the displaced cap; a null src just unbinds.
///
/// pre:  which < 2; notif_src is null or a slot holding a notification
///       cap owned by the caller.
pub unsafe fn bind<E: Env>(t: *mut Tcb, which: usize, notif_src: *mut CapSlot, bits: u64, env: &mut E) {
    let slot = ptr::addr_of_mut!((*t).bind_slots[which]);
    if !(*slot).cap.is_empty() {
        crate::cspace::delete(slot, env);
    }
    (*t).bind_bits[which] = bits;
    if !notif_src.is_null() {
        crate::cspace::slot_move(notif_src, slot);
    }
}

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
/// ready structure through `Env::unqueue_ready` (the scheduler is kernel-
/// side); a thread blocked on a notification is unlinked here, since the
/// waiter queue is a kcore object.
pub unsafe fn destroy_tcb<E: Env>(t: *mut Tcb, env: &mut E) {
    if (*t).state == ThreadState::Runnable {
        env.unqueue_ready(t);
    } else if (*t).state == ThreadState::BlockedNotif && !(*t).wait_notif.is_null() {
        crate::notification::remove_waiter((*t).wait_notif, t);
    }
    (*t).qnext = ptr::null_mut();
    (*t).state = ThreadState::Halted;
    // Binding caps die with the TCB by ordinary CDT cleanup, exactly as
    // queued caps die with their channel (§3.4).
    for i in 0..2 {
        let s = ptr::addr_of_mut!((*t).bind_slots[i]);
        if !(*s).cap.is_empty() {
            crate::cspace::delete(s, env);
        }
    }
    if !(*t).cspace.is_null() {
        crate::cspace::unref_cspace((*t).cspace, env);
        (*t).cspace = ptr::null_mut();
    }
    if !(*t).aspace.is_null() {
        crate::cspace::unref_aspace((*t).aspace, env);
        (*t).aspace = ptr::null_mut();
    }
}
