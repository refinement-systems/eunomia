//! Timer objects (spec §1, §3.6): a cap to program a deadline that signals
//! a bound notification. kcore owns the armed-timer *list* — insert, unlink,
//! and the expiry sweep — operating on the list head through the [`Store`]
//! seam; the head itself (`ARMED_HEAD`) is a kernel static, and the
//! generic-timer register access (`CNTVCT`/`CNTV`, the tick) stays in the
//! `kernel` crate (`kernel/src/timer.rs`). Expiry is checked on the periodic
//! tick, so deadline resolution is one tick at MVP.

use crate::cspace::ObjHeader;
use crate::id::ObjId;
use crate::notification;
use crate::store::Store;

#[repr(C)]
pub struct TimerObj {
    pub hdr: ObjHeader,
    pub(crate) armed: bool,
    pub(crate) deadline: u64,
    pub(crate) notif: Option<ObjId>,
    pub(crate) bits: u64,
    pub(crate) next: Option<ObjId>,
}

impl TimerObj {
    /// pre:  memory at `this` writable.
    /// post: disarmed, refs = 1 (creator cap).
    pub unsafe fn init(this: *mut TimerObj) {
        this.write(TimerObj {
            hdr: ObjHeader { refs: 1 },
            armed: false,
            deadline: 0,
            notif: None,
            bits: 0,
            next: None,
        });
    }
}

/// Arm (or re-arm) a timer: signal `bits` on `notif` once the counter
/// passes `deadline`. The armed timer holds a ref on the notification.
pub fn arm<S: Store>(store: &mut S, t: ObjId, notif: ObjId, bits: u64, deadline: u64) {
    disarm(store, t);
    store.set_obj_refs(notif, store.obj_refs(notif) + 1);
    store.set_timer_notif(t, Some(notif));
    store.set_timer_bits(t, bits);
    store.set_timer_deadline(t, deadline);
    store.set_timer_armed(t, true);
    store.set_timer_next(t, store.timer_armed_head());
    store.set_timer_armed_head(Some(t));
}

pub fn disarm<S: Store>(store: &mut S, t: ObjId) {
    if !store.timer_armed(t) {
        return;
    }
    let mut cur = store.timer_armed_head();
    let mut prev: Option<ObjId> = None;
    while let Some(c) = cur {
        if c == t {
            let cnext = store.timer_next(c);
            match prev {
                None => store.set_timer_armed_head(cnext),
                Some(p) => store.set_timer_next(p, cnext),
            }
            break;
        }
        prev = cur;
        cur = store.timer_next(c);
    }
    // When armed, `notif` was set by `arm` and is always present.
    if let Some(n) = store.timer_notif(t) {
        store.set_obj_refs(n, store.obj_refs(n) - 1);
    }
    store.set_timer_notif(t, None);
    store.set_timer_armed(t, false);
    store.set_timer_next(t, None);
}

/// Tick-time expiry sweep. O(armed timers) per tick — fine at MVP scale.
pub fn check_expired<S: Store>(store: &mut S, now: u64) {
    let mut cur = store.timer_armed_head();
    while let Some(c) = cur {
        let next = store.timer_next(c);
        if store.timer_deadline(c) <= now {
            // Read the firing target before `disarm` clears it.
            let notif = store.timer_notif(c);
            let bits = store.timer_bits(c);
            disarm(store, c);
            if let Some(n) = notif {
                notification::signal(store, n, bits);
            }
        }
        cur = next;
    }
}

/// pre:  refs == 0.
pub fn destroy_timer<S: Store>(store: &mut S, t: ObjId) {
    disarm(store, t);
}
