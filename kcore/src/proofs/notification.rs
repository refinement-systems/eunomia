//! Notification harnesses (plan §4.4): signal/wait word delivery, the FIFO
//! waiter queue, and waiter unlink. These are the implementation-only side of
//! the CapRevocation refcount discipline — a waiter holds one notification ref
//! while blocked (`wait` takes it, `signal`/`remove_waiter` release it) — and
//! the FIFO wake order the `Sched` seam (`Env::make_runnable`, §2.2 rule 4)
//! must preserve.
//!
//! State is stack-allocated `Tcb`s + a standalone `NotifObj` + a `GhostEnv`,
//! not the full `World`: these harnesses need three waiters (a *middle* for the
//! unlink case) but `World` carries only `NTHREADS = 2`, and the §4.3 cost
//! lesson says scope the harness to exactly the objects it touches. Liveness is
//! read straight off `(*n).hdr.refs` rather than the 28-slot World census.

#![cfg(kani)]

use super::ghost::{GhostEnv, GhostEvent};
use super::world::empty_notif;
use crate::cspace::{Cap, CapKind, Rights};
use crate::notification;
use crate::thread::{Tcb, ThreadState};
use core::ptr;

fn notif_cap(n: *mut notification::NotifObj) -> Cap {
    Cap { kind: CapKind::Notification(n), rights: Rights::ALL }
}

/// `check_signal_wait` (plan §4.4): signal ORs bits into the word; a no-waiter
/// signal accumulates; `wait` on a nonzero word consumes it without blocking;
/// and a blocked waiter is woken with the *whole* word (cleared), releasing its
/// ref and recording exactly one `make_runnable`.
#[kani::proof]
#[kani::unwind(4)]
fn check_signal_wait() {
    let mut env = GhostEnv::new();
    let mut nobj = empty_notif();
    let mut t = Tcb::empty();
    unsafe {
        let n = ptr::addr_of_mut!(nobj);
        let tp = ptr::addr_of_mut!(t);
        (*n).hdr.refs = 0;

        // (a) two no-waiter signals accumulate (bitwise OR).
        let b1: u64 = kani::any();
        let b2: u64 = kani::any();
        kani::assume((b1 | b2) != 0); // so (b) below has a word to consume
        notification::signal(n, b1, &mut env);
        notification::signal(n, b2, &mut env);
        assert!((*n).word == b1 | b2);
        assert!(env.count(GhostEvent::MakeRunnable(tp)) == 0); // nobody woken

        // (b) wait on a nonzero word consumes it and does not block.
        assert!(notification::wait(n, tp) == Some(b1 | b2));
        assert!((*n).word == 0);
        assert!((*tp).state != ThreadState::BlockedNotif);
        assert!((*n).hdr.refs == 0); // consumed immediately, no ref taken

        // (c) block, then signal: the whole word is delivered and cleared, the
        //     waiter's ref released, one make_runnable recorded.
        assert!(notification::wait(n, tp).is_none());
        assert!((*tp).state == ThreadState::BlockedNotif);
        assert!((*n).hdr.refs == 1);
        let bits: u64 = kani::any();
        kani::assume(bits != 0); // signal only wakes when the word is nonzero
        notification::signal(n, bits, &mut env);
        assert!((*tp).frame.x[0] == bits);
        assert!((*n).word == 0);
        assert!((*n).hdr.refs == 0);
        assert!(env.count(GhostEvent::MakeRunnable(tp)) == 1);
    }
}

/// `check_waiter_fifo` (plan §4.4): three waiters blocked in order 0,1,2 are
/// woken in that same order (wake order = block order, witnessed by the ghost
/// `make_runnable` log), and each block/wake keeps the notification refcount
/// exact.
#[kani::proof]
#[kani::unwind(6)]
fn check_waiter_fifo() {
    let mut env = GhostEnv::new();
    let mut nobj = empty_notif();
    let mut t0 = Tcb::empty();
    let mut t1 = Tcb::empty();
    let mut t2 = Tcb::empty();
    unsafe {
        let n = ptr::addr_of_mut!(nobj);
        let p0 = ptr::addr_of_mut!(t0);
        let p1 = ptr::addr_of_mut!(t1);
        let p2 = ptr::addr_of_mut!(t2);
        (*n).hdr.refs = 0;

        assert!(notification::wait(n, p0).is_none());
        assert!(notification::wait(n, p1).is_none());
        assert!(notification::wait(n, p2).is_none());
        assert!((*n).hdr.refs == 3); // each blocked waiter holds a ref

        let b: u64 = kani::any();
        kani::assume(b != 0);
        notification::signal(n, b, &mut env);
        notification::signal(n, b, &mut env);
        notification::signal(n, b, &mut env);

        // wake order == block order.
        assert!(env.ordered_before(GhostEvent::MakeRunnable(p0), GhostEvent::MakeRunnable(p1)));
        assert!(env.ordered_before(GhostEvent::MakeRunnable(p1), GhostEvent::MakeRunnable(p2)));
        // all three refs released; queue empty.
        assert!((*n).hdr.refs == 0);
        assert!((*n).wait_head.is_null() && (*n).wait_tail.is_null());
    }
}

/// `check_remove_waiter` (plan §4.4): unlinking head / middle / tail (and the
/// `wait_tail` fixup) leaves the queue correctly relinked in original order,
/// nulls the removed TCB's links, and releases one ref. Removing a thread that
/// is not queued is a no-op.
#[kani::proof]
#[kani::unwind(5)]
fn check_remove_waiter() {
    let mut nobj = empty_notif();
    let mut t0 = Tcb::empty();
    let mut t1 = Tcb::empty();
    let mut t2 = Tcb::empty();
    let mut absent = Tcb::empty();
    unsafe {
        let n = ptr::addr_of_mut!(nobj);
        let p0 = ptr::addr_of_mut!(t0);
        let p1 = ptr::addr_of_mut!(t1);
        let p2 = ptr::addr_of_mut!(t2);
        let pa = ptr::addr_of_mut!(absent);
        (*n).hdr.refs = 0;
        notification::wait(n, p0);
        notification::wait(n, p1);
        notification::wait(n, p2);
        assert!((*n).hdr.refs == 3);

        // victim 0/1/2 = head/middle/tail; 3 = a thread never queued.
        let victim: usize = kani::any();
        kani::assume(victim < 4);
        let vp = match victim {
            0 => p0,
            1 => p1,
            2 => p2,
            _ => pa,
        };

        notification::remove_waiter(n, vp);

        if victim < 3 {
            assert!((*vp).qnext.is_null());
            assert!((*vp).wait_notif.is_null());
            assert!((*n).hdr.refs == 2);

            // Walk the survivors: exactly two, in original relative order, with
            // wait_tail and the final qnext fixed up.
            let mut seen = [ptr::null_mut(); 3];
            let mut k = 0;
            let mut cur = (*n).wait_head;
            while !cur.is_null() && k < 3 {
                seen[k] = cur;
                k += 1;
                cur = (*cur).qnext;
            }
            assert!(k == 2);
            let (e0, e1) = match victim {
                0 => (p1, p2),
                1 => (p0, p2),
                _ => (p0, p1),
            };
            assert!(seen[0] == e0 && seen[1] == e1);
            assert!((*n).wait_tail == e1);
            assert!((*e1).qnext.is_null());
        } else {
            // absent thread: queue and refcount untouched.
            assert!((*n).hdr.refs == 3);
            assert!((*n).wait_head == p0 && (*n).wait_tail == p2);
        }
    }
}
