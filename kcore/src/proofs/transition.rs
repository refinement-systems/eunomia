//! Transition harnesses (plan §4.1): re-checking the CapRevocation TLC result
//! on real code by running the action alphabet and asserting every invariant
//! after the transition — the compositions / arbitrary-state coverage the
//! single-op contract harnesses miss. Two genres live here, split by what the
//! destructive ops cost CBMC (finding DN-4 / DN-12):
//!
//! ## 1. Additive K-step sequence — `check_cdt_transition_system`
//!
//! A fixed `Init` (one root cap) then K nondet **`derive`/`slot_move`** steps,
//! asserting `cdt_wf` (`TypeOK`) and the refcount census (`RefCountSound`)
//! after each. These two ops are pure CDT pointer surgery (no `obj_unref`, no
//! `Env`), so the K-step product stays tractable; this is the
//! *multi-step composition* check (interleavings the single-op harnesses see
//! only one of).
//!
//! ## 2. Inductive single-step over a nondet shape — `check_delete_step`
//!
//! The destructive ops **cannot** join the K-step nondet sequence: `delete`/
//! `revoke` dispatch through `obj_unref`'s symbolic-discriminant match, and a
//! could-delete branch at *each* of K steps unrolls that into a formula CBMC
//! OOMs on (measured: the 4-op alphabet OOMs at K=2, ~9M SAT vars) — and CBMC
//! can no longer bound the CDT walks without `cdt_wf` as an assumption, so it
//! also emits spurious unwinding-assertion failures (finding DN-12). DN-4's
//! stubs make a *single* concrete delete tractable, not a nondet multi-step
//! one.
//!
//! So `delete` is checked *inductively* instead — one op over a
//! **nondeterministic, asserted-wf** CDT shape ([`super::world::nondet_shape`],
//! the genre of [`super::cdt`]). That covers *all* wf shapes the bounds admit
//! (strictly stronger than the states reachable-from-one-root at any fixed K),
//! which is the soundest realization of "re-run the TLC delete action on the
//! real code." `cdt_wf` subsumes TLA `LiveParent` (an occupied slot's non-null
//! parent must be occupied — an empty parent fails the empty⇒detached /
//! first-child checks), so the post-state `cdt_wf` *is* the LiveParent
//! re-check; the dead-slot assertion is `DeadNowhere`. The DN-4 stubs
//! ([`super::stubs`]) keep the single op tractable; the pool holds only
//! notification caps so the stubbed arms are infeasible and `destroy_notif`
//! (the real teardown) is an unstubbed no-op.
//!
//! `revoke` does **not** get an inductive harness here: its nested leaf-first
//! walk over a *symbolic* tree shape OOMs CBMC (the concrete-tree `check_revoke`
//! alone is ~193 s; over a nondet shape it blows the budget — finding DN-12).
//! Revoke's transition coverage therefore stays the concrete `check_revoke`
//! (a derive×4-then-revoke sequence over a fixed 5-cap tree, asserting the same
//! invariants). `send`/`recv` are already a transition harness (`check_ring_fifo`,
//! §4.3, K = 4); `retype` and the object-creating ops need World-level objects
//! the `BarePool` does not model. So this file is the CDT alphabet only.

#![cfg(kani)]

use super::bounds::POOL_SLOTS;
use super::ghost::GhostEnv;
use super::wf::cdt_wf;
use super::world::{nondet_shape, pick, BarePool};
use crate::cspace::{self, Cap, CapKind, Rights};

/// Op-sequence length for the additive harness. Raised from 2 → 3 (review
/// `9_kani-review.md` rec. #2 / §3 "4–6 steps" policy): K = 3 interleaves
/// three derive/move ops over the pool. Sourced from [`super::bounds::K_STEPS`]
/// so the `KANI_DEEP` knob (and any future bump) is a one-line change there,
/// bounded by the per-harness CI budget (~5 min, plan §3, §8).
const K: usize = super::bounds::K_STEPS;

fn notif_cap(n: *mut crate::notification::NotifObj) -> Cap {
    Cap { kind: CapKind::Notification(n), rights: Rights::ALL }
}

/// Occupied (non-empty) slot count = the live notification's refcount census
/// (every occupied slot holds one cap to the single notification).
unsafe fn occupied(pool: &mut BarePool) -> u32 {
    let mut c = 0;
    for i in 0..POOL_SLOTS {
        if !(*pool.slot(i)).cap.is_empty() {
            c += 1;
        }
    }
    c
}

// ── 1. additive K-step composition ────────────────────────────────────────

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
    // Guard against the step degenerating to all-no-ops (rec. #3): a real
    // derive and a real move must each execute past the guard on some path.
    kani::cover!(op == 0);
    kani::cover!(op == 1);
    match op {
        0 => {
            // derive a → b (a new child cap; refcount +1) — TLA `Copy`.
            let _ = cspace::derive(pool.slot(a), pool.slot(b), Rights::ALL.0);
        }
        _ => {
            // move a → b (relocate the cap, CDT position preserved).
            cspace::slot_move(pool.slot(a), pool.slot(b));
        }
    }
}

#[kani::proof]
// Unwind covers the K-loop (≤ K+1) and the POOL_SLOTS census / wf scans:
// 6 at the CI bounds (POOL_SLOTS=4, K=3), 8 under `kani_deep` (POOL_SLOTS=6,
// K=4). cfg_attr because `#[kani::unwind]` takes a literal, not `UNWIND_POOL`.
#[cfg_attr(not(feature = "kani_deep"), kani::unwind(6))]
#[cfg_attr(feature = "kani_deep", kani::unwind(8))]
fn check_cdt_transition_system() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        // Init: a single root cap (TLA `Init`: live = {InitCap}).
        (*pool.slot(0)).cap = notif_cap(n);
        (*n).hdr.refs = 1;

        for _ in 0..K {
            step(&mut pool);

            // cdt_wf (TypeOK ⊇ LiveParent) and the refcount census
            // (RefCountSound) — the TLA invariants — after every step.
            let slots = pool.slot_ptrs();
            assert!(cdt_wf(&slots));
            assert!((*n).hdr.refs == occupied(&mut pool));
        }
    }
}

// ── 2. inductive single-step destructive ops over a nondet wf shape ─────────

/// `check_delete_step` (plan §4.1, TLA single-cap delete): deleting *any* cap
/// of *any* wf shape empties + detaches that slot, releases exactly one object
/// reference, and preserves `cdt_wf` (so survivors' parents stay live —
/// LiveParent — and the children re-parent up one level). Generalizes the
/// concrete `check_delete_reparent` to all shapes the bounds admit.
#[kani::proof]
// 6 at CI bounds (POOL_SLOTS=4), 8 under `kani_deep` (POOL_SLOTS=6): the
// nondet-shape build + cdt_wf / census scans range over POOL_SLOTS slots.
#[cfg_attr(not(feature = "kani_deep"), kani::unwind(6))]
#[cfg_attr(feature = "kani_deep", kani::unwind(8))]
#[kani::stub(crate::cspace::destroy_cspace, super::stubs::no_destroy_cspace)]
#[kani::stub(crate::channel::destroy_channel, super::stubs::no_destroy_channel)]
#[kani::stub(crate::thread::destroy_tcb, super::stubs::no_destroy_tcb)]
fn check_delete_step() {
    let mut pool = BarePool::new();
    let mut env = GhostEnv::new();
    unsafe {
        let (occ, _par) = nondet_shape(&mut pool);
        let slots = pool.slot_ptrs();
        assert!(cdt_wf(&slots), "shape builder must produce a wf pool");
        let n = pool.notif_ptr();
        let refs_before = (*n).hdr.refs;

        let ci = pick(&occ, true);
        cspace::delete(pool.slot(ci), &mut env);

        // The deleted slot is dead: empty and fully detached (DeadNowhere).
        assert!((*pool.slot(ci)).cap.is_empty());
        assert!((*pool.slot(ci)).parent.is_null());
        // Exactly one reference released; the census still holds (RefCountSound).
        assert!((*n).hdr.refs == refs_before - 1);
        assert!((*n).hdr.refs == occupied(&mut pool));
        // TypeOK (⊇ LiveParent) preserved over the real teardown path.
        let slots2 = pool.slot_ptrs();
        assert!(cdt_wf(&slots2));
    }
}

