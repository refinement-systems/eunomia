//! `-Z function-contracts` / `-Z loop-contracts` research spike (review-2
//! rec. #6, `doc/results/18_kani-findings-15.md`). **Off the pinned CI path** —
//! gated on the `kani_contracts` feature, run only by `scripts/deep-verify.sh
//! contracts`, which passes the unstable `-Z` flags these attributes need.
//!
//! The question: can Kani function/loop contracts close review-2 residuals 1–3
//! (recursive container teardown, `revoke` over arbitrary trees, multi-op
//! composition), which are unbounded/recursive and so OOM the bounded harnesses
//! (DN-12)? The idea is modular: give the recursion seam (`delete`/`obj_unref`)
//! a contract, then *replace* the recursive call by that contract
//! (`stub_verified`) instead of unrolling it; give `revoke`'s loop an invariant
//! instead of unwinding it. Findings record how far 0.67.0 gets.

#![cfg(all(kani, feature = "kani_contracts"))]

use crate::cspace::{self, Cap, CapKind, CapSlot, CSpaceObj, ObjHeader, Rights};
use crate::notification::NotifObj;

/// Baseline: does `-Z function-contracts` verify a refcount-discipline contract
/// on kcore at all? `unref_cspace`'s modified object is a *direct pointer
/// parameter*, so `modifies(cs)` is expressible; `requires refs >= 2` keeps it
/// on the non-destroy path (write set = `*cs` only, no recursion). Expected to
/// verify — the positive control the harder targets are measured against.
#[kani::proof_for_contract(cspace::unref_cspace)]
fn contract_unref_cspace_refcount() {
    let mut obj = CSpaceObj { hdr: ObjHeader { refs: kani::any() }, num_slots: 0 };
    let cs: *mut CSpaceObj = core::ptr::addr_of_mut!(obj);
    let mut env = crate::proofs::ghost::GhostEnv::new();
    unsafe {
        cspace::unref_cspace(cs, &mut env);
    }
}

/// The recursion-seam target: `delete` on an *isolated leaf* notification cap
/// (no parent/children/siblings — the structurally simplest input). Even here
/// `delete` writes the notification header (`obj_unref` decrements it), which
/// `modifies(slot)` does not cover and which is not nameable from `delete`'s
/// signature (the pointer lives *inside* the cap). The destructors are stubbed
/// (DN-4) so the leaf never recurses. We expect a `modifies`-clause violation —
/// the documented wall (`18_kani-findings-15.md`); if a future Kani expresses
/// the embedded-object write set, this is where the recursion-break would build.
#[kani::proof_for_contract(cspace::delete)]
#[kani::stub(crate::cspace::destroy_cspace, super::stubs::no_destroy_cspace)]
#[kani::stub(crate::channel::destroy_channel, super::stubs::no_destroy_channel)]
#[kani::stub(crate::thread::destroy_tcb, super::stubs::no_destroy_tcb)]
fn contract_delete_leaf() {
    let mut notif = crate::proofs::world::empty_notif();
    // refs >= 2 so obj_unref decrements without taking the destroy arm.
    notif.hdr.refs = 2;
    let n: *mut NotifObj = core::ptr::addr_of_mut!(notif);
    let mut slot = CapSlot::empty();
    slot.cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
    let s: *mut CapSlot = core::ptr::addr_of_mut!(slot);
    let mut env = crate::proofs::ghost::GhostEnv::new();
    unsafe {
        cspace::delete(s, &mut env);
    }
}

// Residual 2 (`revoke` over a tree) was also attempted with a `-Z loop-contracts`
// `#[kani::loop_invariant(...)]` on the outer walk. It is NOT committed: the
// attribute sits on a loop *expression*, so `cfg_attr`-gating it to this feature
// trips `error[E0658]: custom attributes cannot be applied to expressions` —
// applying it would force crate-wide `#![feature(stmt_expr_attributes, …)]` into
// the production source, and even then the loop body's `delete` call carries the
// same modifies wall as `contract_delete_leaf`. Recorded in
// `doc/results/18_kani-findings-15.md` rather than left as dead/broken code.
