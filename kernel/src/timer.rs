//! Time (spec §2.6) and timer objects (spec §1, §3.6).
//!
//! Monotonic time is the ARM generic virtual timer: CNTVCT readable from
//! EL0 (zero-syscall clock), CNTV programs the kernel tick. Timer objects
//! are caps to program a deadline that signals a bound notification;
//! expiry is checked on the periodic 10 ms tick, so deadline resolution is
//! one tick at MVP.

use crate::cspace::ObjHeader;
use crate::notification::{self, NotifObj};
use core::arch::asm;
use core::ptr;

pub const TICK_HZ: u64 = 100;

#[repr(C)]
pub struct TimerObj {
    pub hdr: ObjHeader,
    armed: bool,
    deadline: u64,
    notif: *mut NotifObj,
    bits: u64,
    next: *mut TimerObj,
}

static mut ARMED_HEAD: *mut TimerObj = ptr::null_mut();

impl TimerObj {
    /// pre:  memory at `this` writable.
    /// post: disarmed, refs = 1 (creator cap).
    pub unsafe fn init(this: *mut TimerObj) {
        this.write(TimerObj {
            hdr: ObjHeader { refs: 1 },
            armed: false,
            deadline: 0,
            notif: ptr::null_mut(),
            bits: 0,
            next: ptr::null_mut(),
        });
    }
}

pub fn counter() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {v}, cntvct_el0", v = out(reg) v) };
    v
}

pub fn freq() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {v}, cntfrq_el0", v = out(reg) v) };
    v
}

/// Arm (or re-arm) a timer: signal `bits` on `notif` once the counter
/// passes `deadline`. The armed timer holds a ref on the notification.
pub unsafe fn arm(t: *mut TimerObj, notif: *mut NotifObj, bits: u64, deadline: u64) {
    disarm(t);
    (*notif).hdr.refs += 1;
    (*t).notif = notif;
    (*t).bits = bits;
    (*t).deadline = deadline;
    (*t).armed = true;
    (*t).next = ARMED_HEAD;
    ARMED_HEAD = t;
}

pub unsafe fn disarm(t: *mut TimerObj) {
    if !(*t).armed {
        return;
    }
    let mut cur = ARMED_HEAD;
    let mut prev: *mut TimerObj = ptr::null_mut();
    while !cur.is_null() {
        if cur == t {
            if prev.is_null() {
                ARMED_HEAD = (*cur).next;
            } else {
                (*prev).next = (*cur).next;
            }
            break;
        }
        prev = cur;
        cur = (*cur).next;
    }
    (*(*t).notif).hdr.refs -= 1;
    (*t).notif = ptr::null_mut();
    (*t).armed = false;
    (*t).next = ptr::null_mut();
}

/// Tick-time expiry sweep. O(armed timers) per tick — fine at MVP scale.
pub unsafe fn check_expired(now: u64) {
    let mut cur = ARMED_HEAD;
    while !cur.is_null() {
        let next = (*cur).next;
        if (*cur).deadline <= now {
            let notif = (*cur).notif;
            let bits = (*cur).bits;
            disarm(cur);
            notification::signal(notif, bits);
        }
        cur = next;
    }
}

/// pre:  refs == 0.
pub unsafe fn destroy_timer(t: *mut TimerObj) {
    disarm(t);
}

// ── Kernel tick ─────────────────────────────────────────────────────────

fn tick_interval() -> u64 {
    freq() / TICK_HZ
}

pub fn start_tick() {
    unsafe {
        // Let EL0 read the virtual counter (time page basis, §2.6).
        asm!("msr cntkctl_el1, {v}", v = in(reg) 0b10u64); // EL0VCTEN
        rearm_tick();
        // CNTV_CTL: ENABLE=1, IMASK=0.
        asm!("msr cntv_ctl_el0, {v}", v = in(reg) 1u64);
    }
}

pub fn rearm_tick() {
    unsafe {
        asm!("msr cntv_tval_el0, {v}", v = in(reg) tick_interval());
    }
}
