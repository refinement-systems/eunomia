//! Kernel-side scheduler (spec rev2§1, rev2§5.4). The thread *object* — TCB layout,
//! trap frame, report state machine, binding slots — lives in
//! [`kcore::thread`] (re-exported below); this module keeps the
//! architectural half: the ready-queue *backing* (the `READY`/`READY_BITMAP`
//! statics + the by-handle accessors the verified ops run against), the context
//! switch, `CURRENT`, the idle WFI loop, and ASID-tagged TTBR0 activation.
//!
//! The ready-queue list *logic* (enqueue/dequeue/unqueue/`top_ready`) is the
//! Verus-verified [`kcore::ready`] ops; the `enqueue`/`dequeue`/`top_ready`/
//! `unqueue_ready` wrappers below are thin pointer-convert + `kcore::ready::*`
//! calls via [`KernelStore`]. The scheduler *policy* (`maybe_switch`) and the asm
//! context switch stay trusted shell (rev2§6.1(d)).
//!
//! Strict fixed-priority preemptive scheduling: 32 levels, round-robin
//! within a level on the periodic tick, idle is a WFI loop at priority 0.
//! Single-core; the kernel is non-preemptible (IRQs masked at EL1), so the
//! scheduler is only ever invoked at exception boundaries.
//!
//! The trap frame lives on the kernel stack during an exception;
//! `maybe_switch` copies frames between the stack and TCBs on a context
//! switch, so the asm restore path never needs to know which thread won.

pub use kcore::thread::*;

use crate::store::KernelStore;
use core::ptr;
use kcore::cspace::CapSlot;
use kcore::id::{ObjId, SlotId};

// Canonical definition lives in kcore::sysabi (shared with the syscall
// decoder's priority-range check).
pub use kcore::sysabi::NUM_PRIOS;

// Handle ⇄ pointer for the ready-queue links, which are architectural shell
// state (this file owns the queues) but read/write the converted
// `Tcb.qnext: Option<ObjId>` field.
#[inline]
unsafe fn as_tcb(o: Option<ObjId>) -> *mut Tcb {
    o.map_or(ptr::null_mut(), |h| h.0 as *mut Tcb)
}
#[inline]
unsafe fn tcb_id(t: *mut Tcb) -> Option<ObjId> {
    if t.is_null() {
        None
    } else {
        Some(ObjId(t as u64))
    }
}

/// See [`kcore::thread::report_terminal`].
pub unsafe fn report_terminal(t: *mut Tcb, r: Report) {
    kcore::thread::report_terminal(&mut KernelStore, ObjId(t as u64), r);
}

/// See [`kcore::thread::bind`].
pub unsafe fn bind(t: *mut Tcb, which: usize, notif_src: *mut CapSlot, bits: u64) {
    let src = if notif_src.is_null() {
        None
    } else {
        Some(SlotId(notif_src as u64))
    };
    kcore::thread::bind(&mut KernelStore, ObjId(t as u64), which, src, bits);
}

/// See [`kcore::thread::set_priority`]. Routes spawn's priority write through the
/// verified setter, which *makes the refusal itself*: an over-ceiling `prio`
/// returns `Err` and leaves the TCB untouched (the rev2§6.1(d) spawn-time gate),
/// an accepted one lands in the TCB under a machine-checked
/// `priority == prio (<= ceiling)` instead of a raw `(*tp).priority` store.
/// Callers map `Err` to `ERR_PERM`.
pub unsafe fn set_priority(t: *mut Tcb, prio: u8, ceiling: u8) -> Result<(), ()> {
    kcore::thread::set_priority(&mut KernelStore, ObjId(t as u64), prio, ceiling)
}

struct Queue {
    head: *mut Tcb,
    tail: *mut Tcb,
}

const EMPTY_QUEUE: Queue = Queue {
    head: ptr::null_mut(),
    tail: ptr::null_mut(),
};

static mut READY: [Queue; NUM_PRIOS] = [EMPTY_QUEUE; NUM_PRIOS];
static mut READY_BITMAP: u32 = 0;
static mut CURRENT: *mut Tcb = ptr::null_mut();

// By-handle accessors over the ready-queue statics — the realization of the
// `Store::ready_*` seam (kernel/src/store.rs) the verified `kcore::ready` ops run
// against. Trusted shell: the ObjId↔`*mut Tcb` conversion is the same `as_tcb`/`tcb_id`
// link the notif/timer queues use (rev2§6.1(d), Honesty note 3).
pub(crate) unsafe fn ready_head_at(level: usize) -> Option<ObjId> {
    tcb_id(READY[level].head)
}
pub(crate) unsafe fn set_ready_head_at(level: usize, h: Option<ObjId>) {
    READY[level].head = as_tcb(h);
}
pub(crate) unsafe fn ready_tail_at(level: usize) -> Option<ObjId> {
    tcb_id(READY[level].tail)
}
pub(crate) unsafe fn set_ready_tail_at(level: usize, t: Option<ObjId>) {
    READY[level].tail = as_tcb(t);
}
pub(crate) unsafe fn ready_bitmap_get() -> u32 {
    READY_BITMAP
}
pub(crate) unsafe fn ready_bitmap_set(b: u32) {
    READY_BITMAP = b;
}

pub unsafe fn current() -> *mut Tcb {
    CURRENT
}

pub unsafe fn set_current(t: *mut Tcb) {
    CURRENT = t;
}

/// Append a thread to the tail of its priority level (the verified
/// [`kcore::ready::ready_enqueue`]). pre: t not on any queue; t.priority < NUM_PRIOS.
/// post: t Runnable at the tail of its priority level.
pub unsafe fn enqueue(t: *mut Tcb) {
    kcore::ready::ready_enqueue(&mut KernelStore, ObjId(t as u64));
}

/// Pop the head of `prio`'s ready list (the verified [`kcore::ready::ready_dequeue`]).
/// Only `maybe_switch` calls it, always on a known-non-empty `top`, so the result
/// is `Some`; an empty level maps to a null pointer.
unsafe fn dequeue(prio: usize) -> *mut Tcb {
    as_tcb(kcore::ready::ready_dequeue(&mut KernelStore, prio))
}

/// Highest ready priority, or None if no thread is ready (the verified
/// [`kcore::ready::top_ready`] bit-scan).
unsafe fn top_ready() -> Option<usize> {
    kcore::ready::top_ready(&KernelStore)
}

/// Remove a Runnable thread from its ready queue (the scheduler half of the
/// old `unqueue`; the notification-wait half lives in
/// [`kcore::thread::destroy_tcb`], which calls this through the
/// `Store::unqueue_ready` seam). The verified arbitrary-position splice
/// [`kcore::ready::ready_unqueue`]. pre: t.state == Runnable.
pub(crate) unsafe fn unqueue_ready(t: *mut Tcb) {
    kcore::ready::ready_unqueue(&mut KernelStore, ObjId(t as u64));
}

/// Scheduling decision at exception exit. `frame` is the trap frame on the
/// kernel stack. `preempt_equal` distinguishes the tick (round-robin among
/// equals) from other exits (switch only to strictly higher priority or
/// when the current thread stopped running).
///
/// post: CURRENT is Running and *frame holds its register state.
pub unsafe fn maybe_switch(frame: *mut TrapFrame, preempt_equal: bool) {
    let cur = CURRENT;
    let cur_running = !cur.is_null() && (*cur).state == ThreadState::Running;
    let Some(top) = top_ready() else {
        // Idle is always Runnable when not Running, so an empty bitmap
        // means the current thread (possibly idle) keeps the CPU.
        debug_assert!(cur_running);
        return;
    };

    if cur_running {
        let cp = (*cur).priority as usize;
        if top < cp || (top == cp && !preempt_equal) {
            return;
        }
        (*cur).frame = *frame;
        enqueue(cur);
    } else if !cur.is_null() {
        // Blocked/halted/faulted threads already had their frame saved
        // by whoever changed their state — which is this exception, so
        // save it now.
        (*cur).frame = *frame;
    }

    let next = dequeue(top);
    (*next).state = ThreadState::Running;
    CURRENT = next;
    *frame = (*next).frame;
    activate_aspace(next);
}

/// Point TTBR0 at the thread's translation tables. ASID tagging makes
/// the switch flush-free; the shared kernel entries keep EL1 mapped
/// across it.
pub unsafe fn activate_aspace(t: *mut Tcb) {
    let ttbr = match (*t).aspace {
        None => crate::mmu::kernel_ttbr0(),
        // Architectural shell owns the aspace object; resolve the handle to
        // the pointer `crate::aspace::ttbr0` expects.
        Some(a) => crate::aspace::ttbr0(a.0 as *mut crate::aspace::AspaceObj),
    };
    core::arch::asm!("msr ttbr0_el1, {v}", "isb", v = in(reg) ttbr);
}
