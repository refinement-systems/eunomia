//! Timer objects (spec §1, §3.6): a cap to program a deadline that signals
//! a bound notification. kcore owns the armed-timer *list* — insert, unlink,
//! and the expiry sweep — operating on the list head through the [`Env`]
//! seam; the head itself (`ARMED_HEAD`) is a kernel static, and the
//! generic-timer register access (`CNTVCT`/`CNTV`, the tick) stays in the
//! `kernel` crate (`kernel/src/timer.rs`). Expiry is checked on the periodic
//! tick, so deadline resolution is one tick at MVP.

use crate::cspace::ObjHeader;
use crate::env::Env;
use crate::notification::{self, NotifObj};
use core::ptr;

#[repr(C)]
pub struct TimerObj {
    pub hdr: ObjHeader,
    pub(crate) armed: bool,
    pub(crate) deadline: u64,
    pub(crate) notif: *mut NotifObj,
    pub(crate) bits: u64,
    pub(crate) next: *mut TimerObj,
}

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

/// Arm (or re-arm) a timer: signal `bits` on `notif` once the counter
/// passes `deadline`. The armed timer holds a ref on the notification.
pub unsafe fn arm<E: Env>(
    t: *mut TimerObj,
    notif: *mut NotifObj,
    bits: u64,
    deadline: u64,
    env: &mut E,
) {
    disarm(t, env);
    (*notif).hdr.refs += 1;
    (*t).notif = notif;
    (*t).bits = bits;
    (*t).deadline = deadline;
    (*t).armed = true;
    (*t).next = env.timer_armed_head();
    env.set_timer_armed_head(t);
}

pub unsafe fn disarm<E: Env>(t: *mut TimerObj, env: &mut E) {
    if !(*t).armed {
        return;
    }
    let mut cur = env.timer_armed_head();
    let mut prev: *mut TimerObj = ptr::null_mut();
    while !cur.is_null() {
        if cur == t {
            if prev.is_null() {
                env.set_timer_armed_head((*cur).next);
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
pub unsafe fn check_expired<E: Env>(now: u64, env: &mut E) {
    let mut cur = env.timer_armed_head();
    while !cur.is_null() {
        let next = (*cur).next;
        if (*cur).deadline <= now {
            let notif = (*cur).notif;
            let bits = (*cur).bits;
            disarm(cur, env);
            notification::signal(notif, bits, env);
        }
        cur = next;
    }
}

/// pre:  refs == 0.
pub unsafe fn destroy_timer<E: Env>(t: *mut TimerObj, env: &mut E) {
    disarm(t, env);
}
