//! Thread-report harnesses (plan §4.4): the terminal-report state machine
//! (`report_terminal`) and thread teardown (`destroy_tcb`). These are the
//! implementation mirrors of two TLA properties (plan §3):
//!
//! - **`ReportMonotone`** — `Running → Exited|Faulted` happens at most once,
//!   and terminal states are absorbing. `report_terminal` guards on
//!   `report != Running`; `check_report_monotone` proves that guard total over
//!   any sequence of (repeated) terminal calls.
//! - **`FireSafe`** — a binding only ever fires into a *live* notification: the
//!   slot is either empty (the holder never configured it, or a revoke cleared
//!   it — firing nothing is a no-op) or holds a notification cap, whose ref
//!   keeps the object live through the fire. `check_bind_fire_safe` proves
//!   `report_terminal` never touches a freed object.
//!
//! Plus `check_thread_teardown`: destroying a thread unlinks it from its
//! notification wait queue and releases the ref, and **produces no report**
//! (§5.1: destruction is the parent acting, not the thread dying).
//!
//! State is stack-allocated (a `Tcb` under test, a `NotifObj`, a `GhostEnv`),
//! with `cspace`/`aspace` left null so `destroy_tcb` does not recurse into
//! container teardown (the DN-4 tractability wall).

#![cfg(kani)]

use super::ghost::{GhostEnv, GhostEvent};
use super::world::empty_notif;
use crate::cspace::{Cap, CapKind, Rights};
use crate::notification::{self, NotifObj};
use crate::thread::{self, Report, Tcb, ThreadState, BIND_EXIT, BIND_FAULT};
use core::ptr;

/// Op-sequence length for the monotonicity harness (= `bounds::K_STEPS`):
/// enough repeated terminal calls to exercise the absorbing guard.
const K: usize = 3;

fn notif_cap(n: *mut NotifObj) -> Cap {
    Cap { kind: CapKind::Notification(n), rights: Rights::ALL }
}

/// A nondeterministic terminal report (`Exited` or `Faulted`).
fn nondet_report() -> Report {
    if kani::any() {
        Report::Exited(kani::any())
    } else {
        Report::Faulted { cause: kani::any(), far: kani::any() }
    }
}

/// `check_report_monotone` (plan §4.4, TLA `ReportMonotone`): over any sequence
/// of terminal `report_terminal` calls, the report transitions away from
/// `Running` exactly once (then is fixed — terminal states absorbing) and the
/// bound notification fires at most once.
#[kani::proof]
#[kani::unwind(5)]
fn check_report_monotone() {
    let mut env = GhostEnv::new();
    let mut nobj = empty_notif();
    let mut waiter = Tcb::empty();
    let mut t = Tcb::empty();
    unsafe {
        let n = ptr::addr_of_mut!(nobj);
        let tp = ptr::addr_of_mut!(t);
        let wp = ptr::addr_of_mut!(waiter);

        // Bind both terminal slots to n, so whichever report kind fires first
        // signals n; a blocked waiter makes each fire observable.
        (*tp).bind_slots[BIND_EXIT].cap = notif_cap(n);
        (*tp).bind_slots[BIND_FAULT].cap = notif_cap(n);
        (*tp).bind_bits[BIND_EXIT] = 0b01;
        (*tp).bind_bits[BIND_FAULT] = 0b10;
        (*n).hdr.refs = 2; // the two bind-slot caps
        assert!(notification::wait(n, wp).is_none()); // refs → 3

        let mut first: Option<Report> = None;
        for _ in 0..K {
            let r = nondet_report();
            if first.is_none() {
                first = Some(r);
            }
            thread::report_terminal(tp, r, &mut env);
            // Absorbing: the record holds the first terminal value forever.
            assert!((*tp).report == first.unwrap());
        }
        // Fired at most once (exactly once here, since the first call always
        // transitions a Running thread with a bound slot).
        assert!(env.count(GhostEvent::MakeRunnable(wp)) <= 1);
    }
}

/// `check_bind_fire_safe` (plan §4.4, TLA `FireSafe`): `report_terminal` only
/// ever reads a binding slot that is empty or holds a live notification — never
/// a freed object. Nondet over report kind and slot occupancy.
#[kani::proof]
#[kani::unwind(3)]
fn check_bind_fire_safe() {
    let mut env = GhostEnv::new();
    let mut nobj = empty_notif();
    let mut t = Tcb::empty();
    unsafe {
        let n = ptr::addr_of_mut!(nobj);
        let tp = ptr::addr_of_mut!(t);

        let r = nondet_report();
        let which = match r {
            Report::Exited(_) => BIND_EXIT,
            _ => BIND_FAULT,
        };

        // Slot is either empty (a revoke cleared it / never configured) or
        // holds a notification cap — whose ref keeps n live through the fire.
        let occupied: bool = kani::any();
        if occupied {
            (*tp).bind_slots[which].cap = notif_cap(n);
            (*tp).bind_bits[which] = 0b1;
            (*n).hdr.refs = 1;
        } else {
            (*n).hdr.refs = 0;
        }
        let refs_before = (*n).hdr.refs;

        thread::report_terminal(tp, r, &mut env);

        if occupied {
            // Fired into the live n: ref unchanged by the signal (no waiter),
            // word set — never a touch of freed memory.
            assert!((*n).word == 0b1);
            assert!((*n).hdr.refs == refs_before);
        } else {
            // Empty slot ⇒ a no-op (signaling nothing), n untouched.
            assert!((*n).word == 0);
        }
        assert!((*tp).report == r); // transitioned once
    }
}

/// `check_thread_teardown` (plan §4.4, §5.1): destroying a thread blocked on a
/// notification unlinks it and releases its ref, halts it, and produces **no
/// report** — destruction is the parent acting, not the thread dying.
#[kani::proof]
#[kani::unwind(4)]
fn check_thread_teardown() {
    let mut env = GhostEnv::new();
    let mut nobj = empty_notif();
    let mut t = Tcb::empty();
    unsafe {
        let n = ptr::addr_of_mut!(nobj);
        let tp = ptr::addr_of_mut!(t);
        (*n).hdr.refs = 0;

        // t blocked on n (one waiter ref); cspace/aspace null and bind slots
        // empty, so destroy_tcb invokes no `obj_unref` teardown — keeping it
        // off the DN-4 recursion wall. (The bind-cap delete path destroy_tcb
        // also walks is exercised by the §4.1 delete harnesses; here the novel
        // property is the notification-waiter unlink and the no-report rule.)
        assert!(notification::wait(n, tp).is_none());
        assert!((*tp).state == ThreadState::BlockedNotif);
        assert!((*n).hdr.refs == 1);
        (*tp).hdr.refs = 0; // destroy precondition: last cap gone
        let report_before = (*tp).report;

        thread::destroy_tcb(tp, &mut env);

        // Waiter unlinked from the notification and its ref released; halted.
        assert!((*tp).wait_notif.is_null());
        assert!((*n).wait_head.is_null());
        assert!((*tp).state == ThreadState::Halted);
        assert!((*n).hdr.refs == 0);
        // Destruction produces NO report and fires nothing (§5.1).
        assert!(report_before == Report::Running);
        assert!((*tp).report == Report::Running);
        assert!(env.count(GhostEvent::MakeRunnable(tp)) == 0);
    }
}
