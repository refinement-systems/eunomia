//! Teardown harnesses (plan §4.1): `revoke` and `delete` over concrete
//! scenario worlds, with the refcount census and the CDT-visible homes a
//! revoke must reach (cspace slots, in-flight channel queue slots, TCB
//! binding slots).
//!
//! ## CBMC tractability limit and how DN-4 is closed (`-Z stubbing`)
//!
//! `delete` dispatches through `obj_unref`, whose `match` on the cap kind is
//! read from slot memory. CBMC does not constant-fold that kind, so when a
//! `delete` is the *top-level* entry it explores *every* arm — including the
//! recursive container teardowns `destroy_cspace` / `destroy_channel` /
//! `destroy_tcb`, which loop over slot counts and recurse back into `delete`.
//! Even a concrete `Frame` cap triggers this (the discriminant is symbolic
//! once stored to and reloaded from a slot), unrolling a formula that never
//! finishes within the CI budget (plan §8).
//!
//! The closure (finding DN-4, `doc/results/2_kani-findings.md`) splits the
//! obligation into two real Kani proofs that compose:
//!
//! - **the teardown bodies, proven by direct calls** (no top-level
//!   `obj_unref`, so no recursion blowup): `check_destroy_cspace` here
//!   (a dying cspace deletes every resident), `check_destroy_channel` (§4.3),
//!   and `check_thread_teardown` (§4.4). These are the structural analog of
//!   one another and run in seconds.
//! - **the `delete`/`obj_unref` dispatch, proven with the recursive arms
//!   stubbed** to no-ops (the shared [`super::stubs`] module):
//!   `check_delete_frame` (the
//!   §2.5 mapped-frame unmap + aspace unref, whose frame-specific logic lives
//!   in `delete` itself and calls none of the stubbed teardowns) and
//!   `check_delete_cspace` (the `CapKind::CSpace => destroy_cspace` routing).
//!   Stubbing cuts only the recursion CBMC cannot prune cheaply; the bodies it
//!   would have re-derived are the direct proofs above.
//!
//! Fire-before-reclaim ordering on a real `delete` is the separate
//! `check_teardown_fire_safe` (§4.3, TSpec `ChannelFireSafe`); the `delete`
//! source order (peer-closed before `obj_unref`) and `m1-test.sh` step 6 back
//! the universal claim (DN-2). The remaining honest residual is **deeply
//! nested** container teardown (a container whose resident is itself a live
//! container → multi-level `delete → destroy_* → delete`), which stays
//! TSpec + QEMU covered (`spawn-test.sh` reclaim loop); the proofs here cover
//! one level of recursion with leaf (notification) residents.

#![cfg(kani)]

use super::bounds::POOL_SLOTS;
use super::ghost::{GhostEnv, GhostEvent};
use super::wf::{cdt_wf, refcount_sound};
use super::world::{BarePool, World};
use crate::cspace::{self, Cap, CapKind, Rights};
use crate::thread::BIND_EXIT;

#[kani::proof]
#[kani::unwind(30)] // covers the 28-slot census scans + the revoke walk
fn check_revoke() {
    let mut w = World::new();
    unsafe {
        let n = w.notif(0);
        // Root cap P in cs0 slot 0 (a notification, so deletes touch one
        // refcount and nothing else — no channel/aspace teardown noise).
        let p = w.cspace_slot(0, 0);
        (*p).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        (*n).hdr.refs = 1;

        // Derived descendants parked across the three CDT-visible homes
        // revoke must reach (§2.2): a cspace slot, an in-flight channel
        // queue slot, and a TCB binding slot — plus a grandchild.
        let mask = Rights::ALL.0;
        assert!(cspace::derive(p, w.cspace_slot(0, 1), mask).is_ok());
        {
            // Make ring(0,0,0) an in-window queued slot so chan_wf holds.
            let ch = w.channel();
            (*ch).head[0] = 0;
            (*ch).count[0] = 1;
        }
        assert!(cspace::derive(p, w.ring_cap(0, 0, 0), mask).is_ok());
        assert!(cspace::derive(p, w.bind_slot(0, BIND_EXIT), mask).is_ok());
        assert!(cspace::derive(w.cspace_slot(0, 1), w.cspace_slot(0, 2), mask).is_ok());
        // P plus four derived caps reference n. (The world starts sound from
        // World::new, and derive maintains refs, so the single census below —
        // post-revoke — is the meaningful one.)
        assert!((*n).hdr.refs == 5);

        cspace::revoke(p, &mut w.env);

        // P survives with no descendants; every parked descendant is purged.
        assert!(!(*p).cap.is_empty());
        assert!((*p).first_child.is_null());
        assert!((*w.cspace_slot(0, 1)).cap.is_empty());
        assert!((*w.cspace_slot(0, 2)).cap.is_empty());
        assert!((*w.ring_cap(0, 0, 0)).cap.is_empty());
        assert!((*w.bind_slot(0, BIND_EXIT)).cap.is_empty());
        // The queue slot was emptied in place, not dequeued (§3.4 null-slot
        // rule): count/head untouched.
        {
            let ch = w.channel();
            assert!((*ch).count[0] == 1);
            assert!((*ch).head[0] == 0);
        }
        // Only P's reference remains.
        assert!((*n).hdr.refs == 1);
        // The census is sound after the whole subtree was reclaimed.
        assert!(refcount_sound(&mut w));
    }
}

#[kani::proof]
#[kani::unwind(6)]
fn check_delete_reparent() {
    // delete = cdt_unlink + obj_unref. A concrete chain r → mid → leaf of
    // notification caps (so the teardown stays tractable, see DN-4): delete
    // mid; leaf survives, re-parented one level up to r; mid empties; the
    // object refcount drops by one.
    let mut pool = BarePool::new();
    let mut env = GhostEnv::new();
    unsafe {
        let n = pool.notif_ptr();
        let r = pool.slot(0);
        let mid = pool.slot(1);
        let leaf = pool.slot(2);
        let cap = |n| Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        (*r).cap = cap(n);
        (*mid).cap = cap(n);
        (*leaf).cap = cap(n);
        cspace::cdt_insert_child(r, mid);
        cspace::cdt_insert_child(mid, leaf);
        (*n).hdr.refs = 3;

        cspace::delete(mid, &mut env);

        assert!((*mid).cap.is_empty());
        assert!((*mid).parent.is_null());
        assert!((*leaf).parent == r); // re-parented one level up
        assert!((*r).first_child == leaf);
        assert!((*n).hdr.refs == 2); // mid's cap released

        let slots = pool.slot_ptrs();
        let _ = POOL_SLOTS;
        assert!(cdt_wf(&slots));
    }
}

/// `check_delete_frame` (plan §4.1, §2.5): deleting a *mapped* frame cap
/// unmaps it and drops the aspace reference the mapping held — the one
/// revocation story for shared memory. The frame-specific logic lives in
/// `delete` itself (the `CapKind::Frame { mapping: Some(..) }` branch →
/// `aspace_unmap` + `unref_aspace`); `obj_unref(Frame)` is a no-op and calls
/// none of the stubbed teardowns, so the stubs only neutralize the infeasible
/// recursion arms (DN-4) without weakening what the frame path proves.
#[kani::proof]
#[kani::unwind(6)]
#[kani::stub(crate::cspace::destroy_cspace, super::stubs::no_destroy_cspace)]
#[kani::stub(crate::channel::destroy_channel, super::stubs::no_destroy_channel)]
#[kani::stub(crate::thread::destroy_tcb, super::stubs::no_destroy_tcb)]
fn check_delete_frame() {
    let mut w = World::new();
    unsafe {
        let asp = w.aspace(0);
        (*asp).hdr.refs = 1; // the mapping holds the only reference
        let va: u64 = 0x4800_0000;
        let pages: u64 = 1;
        let s = w.cspace_slot(0, 0);
        (*s).cap = Cap {
            kind: CapKind::Frame { base: 0x4000_0000, pages, mapping: Some((asp, va)) },
            rights: Rights::ALL,
        };

        cspace::delete(s, &mut w.env);

        assert!((*s).cap.is_empty());
        assert!((*s).parent.is_null());
        // The unmap fired exactly once with the cap's own (asp, va, pages)…
        assert!(w.env.count(GhostEvent::AspaceUnmap(asp, va, pages)) == 1);
        // …the aspace ref dropped to zero, so it was destroyed once…
        assert!((*asp).hdr.refs == 0);
        assert!(w.env.count(GhostEvent::AspaceDestroy(asp)) == 1);
        // …and the unmap preceded the destroy (§2.5 ordering).
        assert!(w.env.ordered_before(
            GhostEvent::AspaceUnmap(asp, va, pages),
            GhostEvent::AspaceDestroy(asp),
        ));
    }
}

/// `check_destroy_cspace` (plan §4.1): a dying cspace deletes every resident
/// cap, releasing each designated object's refcount — "a dying cspace deletes
/// all residents." Residents are notification caps (loop/recursion-free
/// teardown), so the `destroy_cspace` scan over `CS_SLOTS` stays tractable,
/// the same shape that made `check_destroy_channel` (§4.3) tractable. Empty
/// slots (1, 3) exercise the skip path. Drives `destroy_cspace` directly.
#[kani::proof]
#[kani::unwind(6)]
fn check_destroy_cspace() {
    let mut w = World::new();
    unsafe {
        let cs = w.cspace(1);
        let n0 = w.notif(0);
        let n1 = w.notif(1);
        let s0 = w.cspace_slot(1, 0);
        let s2 = w.cspace_slot(1, 2);
        let cap = |n| Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        (*s0).cap = cap(n0);
        (*s2).cap = cap(n1);
        (*n0).hdr.refs = 1;
        (*n1).hdr.refs = 1;

        cspace::destroy_cspace(cs, &mut w.env);

        // Every resident emptied…
        assert!((*s0).cap.is_empty());
        assert!((*s2).cap.is_empty());
        // …and each notification's only ref released → object destroyed.
        assert!((*n0).hdr.refs == 0);
        assert!((*n1).hdr.refs == 0);
    }
}

/// `check_delete_cspace` (plan §4.1): the `obj_unref` *dispatch* DN-4 flagged,
/// on real code. Deleting the last cap to a cspace must drop its refcount to
/// zero and route through the `CapKind::CSpace(p) => destroy_cspace(p)` match
/// arm. `destroy_cspace` is stubbed (DN-4: the live recursion is what makes a
/// `delete` entry intractable), so this proves the *routing* — `obj_unref`
/// finds the header, decrements to zero, and reaches the cspace teardown arm —
/// while the teardown *body* (residents emptied, their objects unref'd) is the
/// real proof `check_destroy_cspace` above. The two compose: a real
/// `delete`-of-a-cspace-cap is "dispatch reaches destroy_cspace" ∘ "destroy_cspace
/// tears down residents", each a Kani proof.
#[kani::proof]
#[kani::unwind(6)]
#[kani::stub(crate::cspace::destroy_cspace, super::stubs::no_destroy_cspace)]
#[kani::stub(crate::channel::destroy_channel, super::stubs::no_destroy_channel)]
#[kani::stub(crate::thread::destroy_tcb, super::stubs::no_destroy_tcb)]
fn check_delete_cspace() {
    let mut w = World::new();
    unsafe {
        let cs1 = w.cspace(1);
        // The cap under test: cspace(0) slot 0 holds the last ref to cs1.
        let s = w.cspace_slot(0, 0);
        (*s).cap = Cap { kind: CapKind::CSpace(cs1), rights: Rights::ALL };
        (*cs1).hdr.refs = 1;

        cspace::delete(s, &mut w.env);

        assert!((*s).cap.is_empty()); // cap deleted, CDT detached
        assert!((*s).parent.is_null());
        // refs hit zero → obj_unref took the zero-ref branch and, cs1 being a
        // cspace, the only feasible arm is destroy_cspace (stubbed here).
        assert!((*cs1).hdr.refs == 0);
    }
}
