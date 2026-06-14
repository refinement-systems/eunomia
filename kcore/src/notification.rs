//! Notification objects (spec §3.6): a machine word of signal bits plus a
//! FIFO waiter queue. Signalers OR bits in; a waiter receives the whole
//! accumulated word, which clears. Event delivery never allocates — the
//! waiter queue is intrusive through the TCBs.

use crate::cspace::ObjHeader;
use crate::id::ObjId;
use crate::store::Store;
use crate::thread::ThreadState;

#[repr(C)]
pub struct NotifObj {
    pub hdr: ObjHeader,
    pub word: u64,
    pub wait_head: Option<ObjId>,
    pub wait_tail: Option<ObjId>,
}

impl NotifObj {
    /// pre:  memory at `this` writable.
    /// post: clear word, no waiters, refs = 1 (creator cap).
    ///
    /// Production construction/layout helper: places the object at a
    /// caller-supplied pointer. Not part of the Store-based core.
    pub unsafe fn init(this: *mut NotifObj) {
        this.write(NotifObj {
            hdr: ObjHeader { refs: 1 },
            word: 0,
            wait_head: None,
            wait_tail: None,
        });
    }
}

/// OR bits into the word and deliver to the first waiter, if any.
/// Safe from any kernel context (syscall, timer IRQ, channel binding).
///
/// post: either a waiter was dequeued, made Runnable, with the whole word
///       in its return register and word == 0 — or no waiter existed and
///       the word accumulates.
pub fn signal<S: Store>(store: &mut S, n: ObjId, bits: u64) {
    let word = store.notif_word(n) | bits;
    store.set_notif_word(n, word);
    let head = store.notif_wait_head(n);
    if word == 0 || head.is_none() {
        return;
    }
    let t = head.unwrap();
    let next = store.tcb_qnext(t);
    store.set_notif_wait_head(n, next);
    if next.is_none() {
        store.set_notif_wait_tail(n, None);
    }
    store.set_tcb_qnext(t, None);
    store.set_tcb_wait_notif(t, None);
    store.set_tcb_retval(t, word);
    store.set_notif_word(n, 0);
    // The waiter held a ref while queued (it has no cap in hand mid-wait).
    store.set_obj_refs(n, store.obj_refs(n) - 1);
    store.make_runnable(t);
}

/// Wait: consume the word if nonzero, else block the current thread.
///
/// post: Some(word≠0) and word cleared — or None, with `cur` Blocked,
///       queued FIFO, holding one ref on n (released on wake or teardown).
pub fn wait<S: Store>(store: &mut S, n: ObjId, cur: ObjId) -> Option<u64> {
    let word = store.notif_word(n);
    if word != 0 {
        store.set_notif_word(n, 0);
        return Some(word);
    }
    store.set_tcb_state(cur, ThreadState::BlockedNotif);
    store.set_tcb_wait_notif(cur, Some(n));
    store.set_tcb_qnext(cur, None);
    match store.notif_wait_tail(n) {
        None => store.set_notif_wait_head(n, Some(cur)),
        Some(tail) => store.set_tcb_qnext(tail, Some(cur)),
    }
    store.set_notif_wait_tail(n, Some(cur));
    store.set_obj_refs(n, store.obj_refs(n) + 1);
    None
}

/// Unlink a waiter (thread teardown path).
pub fn remove_waiter<S: Store>(store: &mut S, n: ObjId, t: ObjId) {
    let mut cur = store.notif_wait_head(n);
    let mut prev: Option<ObjId> = None;
    while let Some(c) = cur {
        if c == t {
            let next = store.tcb_qnext(c);
            match prev {
                None => store.set_notif_wait_head(n, next),
                Some(p) => store.set_tcb_qnext(p, next),
            }
            if store.notif_wait_tail(n) == Some(t) {
                store.set_notif_wait_tail(n, prev);
            }
            store.set_tcb_qnext(t, None);
            store.set_tcb_wait_notif(t, None);
            store.set_obj_refs(n, store.obj_refs(n) - 1);
            return;
        }
        prev = cur;
        cur = store.tcb_qnext(c);
    }
}

/// pre:  refs == 0 — no caps, no bindings, no armed timers, no waiters
///       (waiters hold refs, so a notification with blocked waiters
///       cannot reach zero; they stay blocked until killed, accepted MVP
///       behaviour).
pub fn destroy_notif<S: Store>(store: &mut S, n: ObjId) {
    debug_assert!(store.notif_wait_head(n).is_none());
    let _ = store;
    let _ = n;
}
