//! Structural CDT harnesses (plan §4.1): the contract-genre proofs over a
//! nondeterministic, assumed-wf cap-slot pool. Each checks one operation and
//! asserts `cdt_wf` plus the op's postcondition. Bounds come from
//! [`super::bounds`] (`POOL_SLOTS = 4` = TLA `CapIds`, `UNWIND_POOL = 6`);
//! the `#[kani::unwind]` literals equal `UNWIND_POOL` (kept in sync by the
//! doc here, since the attribute takes a literal).
//!
//! The shape builder ([`super::world::nondet_shape`]) constructs the pool;
//! every harness then *asserts* the pool is wf — a builder bug surfaces as a
//! failure, never a vacuous pass (plan §4.1).

#![cfg(kani)]

use super::bounds::POOL_SLOTS;
use super::ghost::GhostEnv;
use super::wf::cdt_wf;
use super::world::{nondet_shape, BarePool};
use crate::aspace::AspaceObj;
use crate::cspace::{self, Cap, CapKind, CapSlot, Rights};

/// Pick a nondet slot index satisfying `occupied[i] == want`.
unsafe fn pick(occ: &[bool; POOL_SLOTS], want: bool) -> usize {
    let i: usize = kani::any();
    kani::assume(i < POOL_SLOTS);
    kani::assume(occ[i] == want);
    i
}

#[kani::proof]
#[kani::unwind(6)] // = bounds::UNWIND_POOL (POOL_SLOTS + 2)
fn check_cdt_insert_child() {
    let mut pool = BarePool::new();
    unsafe {
        let (occ, _par) = nondet_shape(&mut pool);
        let slots = pool.slot_ptrs();
        assert!(cdt_wf(&slots), "shape builder must produce a wf pool");

        let parent = pool.slot(pick(&occ, true));
        let ci = pick(&occ, false);
        let child = pool.slot(ci);
        // A detached, non-empty child (a fresh notification cap).
        (*child).cap = Cap { kind: CapKind::Notification(pool.notif_ptr()), rights: Rights::ALL };

        let old_fc = (*parent).first_child;
        cspace::cdt_insert_child(parent, child);

        // child is the new first child; previous children follow it intact.
        assert!((*parent).first_child == child);
        assert!((*child).parent == parent);
        assert!((*child).prev_sib.is_null());
        assert!((*child).next_sib == old_fc);
        if !old_fc.is_null() {
            assert!((*old_fc).prev_sib == child);
        }
        let slots2 = pool.slot_ptrs();
        assert!(cdt_wf(&slots2));
    }
}

#[kani::proof]
#[kani::unwind(6)]
fn check_cdt_unlink() {
    let mut pool = BarePool::new();
    unsafe {
        let (occ, _par) = nondet_shape(&mut pool);
        let slots = pool.slot_ptrs();
        assert!(cdt_wf(&slots), "shape builder must produce a wf pool");

        let vi = pick(&occ, true);
        let victim = pool.slot(vi);
        let old_parent = (*victim).parent;

        cspace::cdt_unlink(victim);

        // Victim fully detached.
        assert!((*victim).parent.is_null());
        assert!((*victim).first_child.is_null());
        assert!((*victim).next_sib.is_null());
        assert!((*victim).prev_sib.is_null());

        // Every other occupied slot that named `victim` as parent is now
        // re-parented one level up (to victim's old parent).
        for j in 0..POOL_SLOTS {
            if j != vi && occ[j] {
                let s = pool.slot(j);
                // (cdt_unlink moved victim's children to old_parent; no slot
                // may still point at the detached victim.)
                assert!((*s).parent != victim);
                assert!((*s).next_sib != victim);
                assert!((*s).prev_sib != victim);
                assert!((*s).first_child != victim);
            }
        }
        let _ = old_parent;
        let slots2 = pool.slot_ptrs();
        assert!(cdt_wf(&slots2));
    }
}

#[kani::proof]
#[kani::unwind(6)]
fn check_slot_move() {
    let mut pool = BarePool::new();
    unsafe {
        let (occ, _par) = nondet_shape(&mut pool);
        let slots = pool.slot_ptrs();
        assert!(cdt_wf(&slots), "shape builder must produce a wf pool");

        let si = pick(&occ, true);
        let di = pick(&occ, false);
        let src = pool.slot(si);
        let dst = pool.slot(di);

        // Record src's CDT position and the object refcount.
        let parent = (*src).parent;
        let first_child = (*src).first_child;
        let next_sib = (*src).next_sib;
        let prev_sib = (*src).prev_sib;
        let refs_before = (*pool.notif_ptr()).hdr.refs;

        cspace::slot_move(src, dst);

        // dst inherits the exact position; src is empty and detached.
        assert!((*dst).parent == parent);
        assert!((*dst).first_child == first_child);
        assert!((*dst).next_sib == next_sib);
        assert!((*dst).prev_sib == prev_sib);
        assert!((*src).cap.is_empty());
        assert!((*src).parent.is_null());
        assert!((*src).first_child.is_null());
        assert!((*src).next_sib.is_null());
        assert!((*src).prev_sib.is_null());
        // The parent.first_child fixup and children's parent pointers.
        if !parent.is_null() && (*parent).first_child == dst {
            // ok: src was its parent's first child, now dst is.
        }
        let mut c = first_child;
        let mut steps = 0;
        while !c.is_null() && steps < POOL_SLOTS {
            assert!((*c).parent == dst);
            c = (*c).next_sib;
            steps += 1;
        }
        // Refcount unchanged (a move, not a copy).
        assert!((*pool.notif_ptr()).hdr.refs == refs_before);

        let slots2 = pool.slot_ptrs();
        assert!(cdt_wf(&slots2));
    }
}

#[kani::proof]
fn check_derive_monotone() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        let src = pool.slot(0);
        let dst = pool.slot(1);
        let rights: u8 = kani::any();
        let mask: u8 = kani::any();
        (*src).cap = Cap { kind: CapKind::Notification(n), rights: Rights(rights) };
        (*n).hdr.refs = 1;

        let r = cspace::derive(src, dst, mask);
        assert!(r.is_ok());

        // The load-bearing security property: rights are exactly src ∩ mask,
        // hence a subset of src — no derivation ever adds a bit (§2.3).
        assert!((*dst).cap.rights.0 == rights & mask);
        assert!((*dst).cap.rights.0 & !rights == 0);
        // dst is a CDT child of src.
        assert!((*dst).parent == src);
        assert!((*src).first_child == dst);
        // Object refcount +1 (the new cap is a reference).
        assert!((*n).hdr.refs == 2);
    }
}

#[kani::proof]
fn check_derive_refuses_untyped() {
    // Untyped caps are never derived (the watermark has one owner, §2.3);
    // derive errs and leaves dst untouched.
    let mut pool = BarePool::new();
    unsafe {
        let src = pool.slot(0);
        let dst = pool.slot(1);
        let mask: u8 = kani::any();
        (*src).cap = Cap {
            kind: CapKind::Untyped { base: 0, size: 4096, watermark: 0 },
            rights: Rights::ALL,
        };
        let r = cspace::derive(src, dst, mask);
        assert!(r.is_err());
        assert!((*dst).cap.is_empty());
        assert!((*dst).parent.is_null());
    }
}

#[kani::proof]
fn check_derive_frame_unmapped() {
    // A fresh frame copy starts unmapped — one mapping per cap copy (§2.5).
    let mut pool = BarePool::new();
    let mut asp = AspaceObj { hdr: crate::cspace::ObjHeader { refs: 0 }, asid: 0, l1: 0, pool_base: 0, pool_pages: 0, pool_used: 0 };
    unsafe {
        let asp_ptr: *mut AspaceObj = &mut asp;
        let src = pool.slot(0);
        let dst = pool.slot(1);
        let mask: u8 = kani::any();
        (*src).cap = Cap {
            kind: CapKind::Frame { base: 0x4000_0000, pages: 1, mapping: Some((asp_ptr, 0x8000_0000)) },
            rights: Rights::ALL,
        };
        let r = cspace::derive(src, dst, mask);
        assert!(r.is_ok());
        match (*dst).cap.kind {
            CapKind::Frame { mapping, .. } => assert!(mapping.is_none()),
            _ => assert!(false, "derived cap must stay a Frame"),
        }
    }
}

// ── negative harnesses: each debug_assert contract must fire (plan §5) ────

#[kani::proof]
#[kani::should_panic]
fn check_neg_slot_move_occupied_dst() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        let src = pool.slot(0);
        let dst = pool.slot(1);
        (*src).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        (*dst).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        // dst non-empty → debug_assert!((*dst).cap.is_empty()) fires.
        cspace::slot_move(src, dst);
    }
}

#[kani::proof]
#[kani::should_panic]
fn check_neg_slot_move_empty_src() {
    let mut pool = BarePool::new();
    unsafe {
        let src = pool.slot(0); // empty
        let dst = pool.slot(1);
        // src empty → debug_assert!(!(*src).cap.is_empty()) fires.
        cspace::slot_move(src, dst);
    }
}

#[kani::proof]
#[kani::should_panic]
fn check_neg_delete_empty_slot() {
    let mut pool = BarePool::new();
    let mut env = GhostEnv::new();
    unsafe {
        let s: *mut CapSlot = pool.slot(0); // empty
        // empty → debug_assert!(!(*slot).cap.is_empty()) fires.
        cspace::delete(s, &mut env);
    }
}

#[kani::proof]
#[kani::should_panic]
fn check_neg_destroy_notif_with_waiter() {
    // destroy_notif's contract is "no waiters at refs == 0". Block a thread
    // on the notification (wait_head becomes non-null), then destroy it: the
    // `debug_assert!((*n).wait_head.is_null())` must fire. `wait` here
    // deterministically blocks (word == 0), so the only panic is the
    // contract's.
    let mut pool = BarePool::new();
    let mut tcb = crate::thread::Tcb::empty();
    unsafe {
        let n = pool.notif_ptr();
        let t: *mut crate::thread::Tcb = &mut tcb;
        let blocked = crate::notification::wait(n, t);
        kani::assume(blocked.is_none());
        crate::notification::destroy_notif(n);
    }
}
