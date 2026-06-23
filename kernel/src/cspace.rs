//! Kernel-side capability-space surface: the object machinery lives in
//! [`kcore::cspace`] (host-buildable); this module re-exports it and
//! supplies the `KernelStore`-bound wrappers for the few ops that fire events or
//! tear objects down — wrapping the raw `*mut CapSlot` call sites into
//! [`SlotId`](kcore::id::SlotId) handles. Call sites elsewhere in the kernel see
//! the same `cspace::delete(slot)` / `cspace::revoke_step(slot, budget)`
//! signatures as before.

pub use kcore::cspace::*;

use crate::store::KernelStore;
use kcore::id::{ObjId, SlotId};

/// See [`kcore::cspace::delete`].
pub unsafe fn delete(slot: *mut CapSlot) {
    kcore::cspace::delete(&mut KernelStore, SlotId(slot as u64));
}

/// See [`kcore::cspace::map_frame`] — the verified cap-side map record. Records the
/// `(asp, va)` mapping on the unmapped frame cap at `slot` and bumps the aspace refcount,
/// driving the page-table write through `KernelStore::aspace_map`.
pub unsafe fn map_frame(
    slot: *mut CapSlot,
    asp: ObjId,
    va: u64,
    perms: u64,
) -> Result<(), kcore::aspace::MapError> {
    kcore::cspace::map_frame(&mut KernelStore, SlotId(slot as u64), asp, va, perms)
}

/// See [`kcore::cspace::revoke_step`]. Does at most `budget` leaf-deletions of
/// the revoke walk and returns [`RevokeStatus::Done`] when the subtree is empty or
/// [`RevokeStatus::More`] when the budget is exhausted with descendants remaining —
/// the bounded quantum the `CapRevoke` handler maps to `0` / `ERR_AGAIN`. The
/// kernel drives only this bounded form, not the unbounded
/// [`kcore::cspace::revoke`]: the revoke surface is preemptible (rev2§2.2, rev2§5.4).
pub unsafe fn revoke_step(slot: *mut CapSlot, budget: usize) -> RevokeStatus {
    kcore::cspace::revoke_step(&mut KernelStore, SlotId(slot as u64), budget)
}

// `unref_aspace` / `unref_cspace` take a store and are reached only from
// inside kcore (thread teardown), so the kernel needs no wrappers for them;
// they remain available as `kcore::cspace::unref_*` if a future shell path
// wants them.
