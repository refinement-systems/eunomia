//! Notification objects (spec §3.6): a machine word of signal bits plus a
//! FIFO waiter queue. Signalers OR bits in; a waiter receives the whole
//! accumulated word, which clears. Event delivery never allocates — the
//! waiter queue is intrusive through the TCBs.

use crate::cspace::ObjHeader;
use crate::env::Env;
use crate::thread::{Tcb, ThreadState};
use core::ptr;

#[repr(C)]
pub struct NotifObj {
    pub hdr: ObjHeader,
    pub word: u64,
    pub(crate) wait_head: *mut Tcb,
    pub(crate) wait_tail: *mut Tcb,
}

impl NotifObj {
    /// pre:  memory at `this` writable.
    /// post: clear word, no waiters, refs = 1 (creator cap).
    pub unsafe fn init(this: *mut NotifObj) {
        this.write(NotifObj {
            hdr: ObjHeader { refs: 1 },
            word: 0,
            wait_head: ptr::null_mut(),
            wait_tail: ptr::null_mut(),
        });
    }
}

/// OR bits into the word and deliver to the first waiter, if any.
/// Safe from any kernel context (syscall, timer IRQ, channel binding).
///
/// post: either a waiter was dequeued, made Runnable, with the whole word
///       in its return register and word == 0 — or no waiter existed and
///       the word accumulates.
pub unsafe fn signal<E: Env>(n: *mut NotifObj, bits: u64, env: &mut E) {
    (*n).word |= bits;
    if (*n).word == 0 || (*n).wait_head.is_null() {
        return;
    }
    let t = (*n).wait_head;
    (*n).wait_head = (*t).qnext;
    if (*n).wait_head.is_null() {
        (*n).wait_tail = ptr::null_mut();
    }
    (*t).qnext = ptr::null_mut();
    (*t).wait_notif = ptr::null_mut();
    (*t).frame.x[0] = (*n).word;
    (*n).word = 0;
    // The waiter held a ref while queued (it has no cap in hand mid-wait).
    (*n).hdr.refs -= 1;
    env.make_runnable(t);
}

/// Wait: consume the word if nonzero, else block the current thread.
///
/// post: Some(word≠0) and word cleared — or None, with `cur` Blocked,
///       queued FIFO, holding one ref on n (released on wake or teardown).
pub unsafe fn wait(n: *mut NotifObj, cur: *mut Tcb) -> Option<u64> {
    if (*n).word != 0 {
        let w = (*n).word;
        (*n).word = 0;
        return Some(w);
    }
    (*cur).state = ThreadState::BlockedNotif;
    (*cur).wait_notif = n;
    (*cur).qnext = ptr::null_mut();
    if (*n).wait_tail.is_null() {
        (*n).wait_head = cur;
    } else {
        (*(*n).wait_tail).qnext = cur;
    }
    (*n).wait_tail = cur;
    (*n).hdr.refs += 1;
    None
}

/// Unlink a waiter (thread teardown path).
pub unsafe fn remove_waiter(n: *mut NotifObj, t: *mut Tcb) {
    let mut cur = (*n).wait_head;
    let mut prev: *mut Tcb = ptr::null_mut();
    while !cur.is_null() {
        if cur == t {
            if prev.is_null() {
                (*n).wait_head = (*cur).qnext;
            } else {
                (*prev).qnext = (*cur).qnext;
            }
            if (*n).wait_tail == t {
                (*n).wait_tail = prev;
            }
            (*t).qnext = ptr::null_mut();
            (*t).wait_notif = ptr::null_mut();
            (*n).hdr.refs -= 1;
            return;
        }
        prev = cur;
        cur = (*cur).qnext;
    }
}

/// pre:  refs == 0 — no caps, no bindings, no armed timers, no waiters
///       (waiters hold refs, so a notification with blocked waiters
///       cannot reach zero; they stay blocked until killed, accepted MVP
///       behaviour).
pub unsafe fn destroy_notif(n: *mut NotifObj) {
    debug_assert!((*n).wait_head.is_null());
}
