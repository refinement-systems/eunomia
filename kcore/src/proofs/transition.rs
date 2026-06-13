//! Transition-system harness (plan §4.1): the direct re-check of the
//! CapRevocation TLC result on real code — start from an `Init` state, run K
//! nondeterministically chosen operations from the action alphabet, and
//! assert every invariant after every step. This exercises the *compositions*
//! the single-op contract harnesses miss.
//!
//! Scope, honestly (plan §4.1, §9, and findings DN-4): the alphabet here is
//! the non-recursive CDT-builder core — `derive` and `slot_move` — over a
//! bare pool of notification caps, asserting `cdt_wf` (TypeOK) and the
//! refcount census (RefCountSound) after every step. The destructive ops
//! (`delete`, `revoke`) and the object-creating/-consuming ops (retype, send,
//! recv, bind, thread_exit/fault, reset) are deliberately *not* mixed into
//! this nondet sequence:
//!
//! - `delete`/`revoke` dispatch through `obj_unref`, whose recursive
//!   `destroy_*` teardown CBMC cannot fold away (DN-4); a nondet op sequence
//!   that *might* delete at each step multiplies that intractable formula by
//!   the branching and blows past the CI budget. Their composition is checked
//!   directly by `check_revoke` — itself a four-`derive`-then-`revoke`
//!   sequence asserting the same invariants — and the single-op
//!   `check_delete_reparent`.
//! - the channel/thread/untyped ops need objects whose per-step state and
//!   teardown have the same cost; each is covered by its own contract
//!   harness.
//!
//! So this is the composition check for the part that stays tractable; the
//! TLA invariants it carries (`cdt_wf`, the census) are exactly the model's,
//! and the ordering it explores — derive/move interleavings the contract
//! harnesses see only one of — is the value it adds.

#![cfg(kani)]

use super::bounds::POOL_SLOTS;
use super::wf::cdt_wf;
use super::world::BarePool;
use crate::cspace::{self, Cap, CapKind, Rights};

/// Op-sequence length. K = 2 already interleaves two distinct ops over the
/// pool (the composition the single-op harnesses cannot reach) and stays
/// comfortably inside the per-harness CI budget; K = 3 also verifies but sits
/// right at the ~5-min ceiling (~297 s locally), too close for slower CI
/// runners (plan §3, §8 — raise the bound here when the budget grows).
const K: usize = 2;

fn notif_cap(n: *mut crate::notification::NotifObj) -> Cap {
    Cap { kind: CapKind::Notification(n), rights: Rights::ALL }
}

/// Occupied (non-empty) slot count = the live notification's refcount census
/// (every occupied slot holds a cap to the one notification; derive adds a
/// reference, move relocates one).
unsafe fn occupied(pool: &mut BarePool) -> u32 {
    let mut c = 0;
    for i in 0..POOL_SLOTS {
        if !(*pool.slot(i)).cap.is_empty() {
            c += 1;
        }
    }
    c
}

unsafe fn step(pool: &mut BarePool) {
    let op: u8 = kani::any();
    kani::assume(op < 2);
    let a: usize = kani::any();
    kani::assume(a < POOL_SLOTS);
    let b: usize = kani::any();
    kani::assume(b < POOL_SLOTS);

    if a == b || (*pool.slot(a)).cap.is_empty() || !(*pool.slot(b)).cap.is_empty() {
        return; // ill-typed pick (TLA actions are guarded); a no-op step
    }
    match op {
        0 => {
            // derive a → b (a new child cap; refcount +1)
            let _ = cspace::derive(pool.slot(a), pool.slot(b), Rights::ALL.0);
        }
        _ => {
            // move a → b (relocate the cap, CDT position preserved)
            cspace::slot_move(pool.slot(a), pool.slot(b));
        }
    }
}

#[kani::proof]
#[kani::unwind(6)] // POOL_SLOTS (4) + 2 covers the cdt_wf / census scans
fn check_cdt_transition_system() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        // Init: a single root cap (TLA `Init`: live = {InitCap}).
        (*pool.slot(0)).cap = notif_cap(n);
        (*n).hdr.refs = 1;

        for _ in 0..K {
            step(&mut pool);

            // cdt_wf (TypeOK) and the refcount census (RefCountSound) — the
            // TLA invariants — after every step.
            let slots = pool.slot_ptrs();
            assert!(cdt_wf(&slots));
            assert!((*n).hdr.refs == occupied(&mut pool));
        }
    }
}
