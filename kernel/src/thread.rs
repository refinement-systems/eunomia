//! Threads and the scheduler (spec §1, §5.4).
//!
//! Strict fixed-priority preemptive scheduling: 32 levels, round-robin
//! within a level on the periodic tick, idle is a WFI loop at priority 0.
//! Single-core; the kernel is non-preemptible (IRQs masked at EL1), so the
//! scheduler is only ever invoked at exception boundaries.
//!
//! The trap frame lives on the kernel stack during an exception;
//! `maybe_switch` copies frames between the stack and TCBs on a context
//! switch, so the asm restore path never needs to know which thread won.

use crate::cspace::{CSpaceObj, ObjHeader};
use core::ptr;

pub const NUM_PRIOS: usize = 32;

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
    /// Ready-queue / notification-wait-queue link (a thread is on at most
    /// one queue, disambiguated by `state`).
    pub qnext: *mut Tcb,
    pub wait_notif: *mut crate::notification::NotifObj,
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
            qnext: ptr::null_mut(),
            wait_notif: ptr::null_mut(),
        }
    }

    /// pre:  memory at `this` writable, sized size_of::<Tcb>().
    /// post: inactive thread, refs = 1 (creator cap).
    pub unsafe fn init(this: *mut Tcb) {
        this.write(Tcb {
            hdr: ObjHeader { refs: 1 },
            frame: TrapFrame::zeroed(),
            state: ThreadState::Inactive,
            priority: 0,
            cspace: ptr::null_mut(),
            qnext: ptr::null_mut(),
            wait_notif: ptr::null_mut(),
        });
    }
}

struct Queue {
    head: *mut Tcb,
    tail: *mut Tcb,
}

const EMPTY_QUEUE: Queue = Queue { head: ptr::null_mut(), tail: ptr::null_mut() };

static mut READY: [Queue; NUM_PRIOS] = [EMPTY_QUEUE; NUM_PRIOS];
static mut READY_BITMAP: u32 = 0;
static mut CURRENT: *mut Tcb = ptr::null_mut();

pub unsafe fn current() -> *mut Tcb {
    CURRENT
}

pub unsafe fn set_current(t: *mut Tcb) {
    CURRENT = t;
}

/// pre:  t not on any queue; t.priority < NUM_PRIOS.
/// post: t Runnable at the tail of its priority level.
pub unsafe fn enqueue(t: *mut Tcb) {
    let prio = (*t).priority as usize;
    (*t).state = ThreadState::Runnable;
    (*t).qnext = ptr::null_mut();
    let q = &mut READY[prio];
    if q.tail.is_null() {
        q.head = t;
    } else {
        (*q.tail).qnext = t;
    }
    q.tail = t;
    READY_BITMAP |= 1 << prio;
}

unsafe fn dequeue(prio: usize) -> *mut Tcb {
    let q = &mut READY[prio];
    let t = q.head;
    q.head = (*t).qnext;
    if q.head.is_null() {
        q.tail = ptr::null_mut();
        READY_BITMAP &= !(1 << prio);
    }
    (*t).qnext = ptr::null_mut();
    t
}

/// Highest ready priority, or None if no thread is ready.
unsafe fn top_ready() -> Option<usize> {
    if READY_BITMAP == 0 {
        None
    } else {
        Some(31 - READY_BITMAP.leading_zeros() as usize)
    }
}

/// Remove t from whatever queue it is on (slow path: teardown only).
unsafe fn unqueue(t: *mut Tcb) {
    if (*t).state == ThreadState::Runnable {
        let q = &mut READY[(*t).priority as usize];
        let mut cur = q.head;
        let mut prev: *mut Tcb = ptr::null_mut();
        while !cur.is_null() {
            if cur == t {
                if prev.is_null() {
                    q.head = (*cur).qnext;
                } else {
                    (*prev).qnext = (*cur).qnext;
                }
                if q.tail == t {
                    q.tail = prev;
                }
                break;
            }
            prev = cur;
            cur = (*cur).qnext;
        }
        if q.head.is_null() {
            READY_BITMAP &= !(1 << (*t).priority as usize);
        }
    } else if (*t).state == ThreadState::BlockedNotif && !(*t).wait_notif.is_null() {
        crate::notification::remove_waiter((*t).wait_notif, t);
    }
    (*t).qnext = ptr::null_mut();
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
}

/// pre:  refs == 0 (last cap gone).
/// post: t off every queue and never scheduled again. If t is CURRENT the
///       exception exit path will switch away; the TCB memory stays valid
///       until its donor untyped is revoked and reset, which requires
///       deleting this very cap chain first — so no dangling CURRENT.
pub unsafe fn destroy_tcb(t: *mut Tcb) {
    unqueue(t);
    (*t).state = ThreadState::Halted;
    if CURRENT == t {
        // The exit path's maybe_switch sees a non-Running current and
        // picks someone else; idle is always there.
    }
}
