//! Teardown harnesses (plan §4.1): `revoke` and `delete` over concrete
//! scenario worlds, with the refcount census and the CDT-visible homes a
//! revoke must reach (cspace slots, in-flight channel queue slots, TCB
//! binding slots).
//!
//! ## CBMC tractability limit (finding DN-4)
//!
//! `delete` dispatches through `obj_unref`, whose `match` on the cap kind is
//! read from slot memory. CBMC does not constant-fold that kind, so it
//! explores *every* arm — including the recursive container teardowns
//! `destroy_cspace` / `destroy_channel`, which loop over (symbolic) slot
//! counts and recurse back into `delete`. For a deleted **notification** cap
//! the feasible teardown (`destroy_notif`) has no loops and no recursion, so
//! a *concrete* scenario stays tractable (`check_revoke` ≈ 193 s,
//! `check_delete_reparent` below). But deleting a frame, channel, or cspace
//! cap — or adding a nondet CDT shape on top of a delete — unrolls the
//! recursive teardown into an intractable formula (> many minutes), past the
//! CI budget (plan §8). The harnesses for those specific behaviours are
//! therefore **not** Kani proofs here; what covers them instead:
//!
//! - **frame-unmap-on-delete (§2.5)** — the `delete` source (the
//!   `Frame { mapping: Some(..) }` branch calling `aspace_unmap` +
//!   `unref_aspace`) and `scripts/spawn-test.sh`, which maps and unmaps the
//!   per-child time-page frame on every spawn/reclaim cycle in QEMU.
//! - **fire-before-reclaim / `ChannelFireSafe` (TSpec, DN-2)** — the
//!   TLC-checked `CapRevocation` TSpec, the source order in `cspace::delete`
//!   (peer-closed fires before `obj_unref`), and the `m1-test.sh` step-6
//!   runtime witness.
//! - **container teardown + armed-timer disarm** — `scripts/spawn-test.sh`
//!   (the reclaim loop tears down each child's cspace/threads) and the
//!   source. `check_revoke` already exercises the `delete` path end to end.
//!
//! Lifting this limit (e.g. `-Z stubbing` the `destroy_*` recursion, or a
//! function-contract on `obj_unref`) is recorded as future work in the
//! findings doc rather than worked around with an unsound bound.

#![cfg(kani)]

use super::bounds::POOL_SLOTS;
use super::ghost::GhostEnv;
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
