//! Kernel-side capability-space surface: the object machinery lives in
//! [`kcore::cspace`] (host-buildable, Kani-verified, plan §4.1); this module
//! re-exports it and supplies the `KernelEnv`-bound wrappers for the few ops
//! that fire events or tear objects down. Call sites elsewhere in the kernel
//! see the same `cspace::delete(slot)` / `cspace::revoke(slot)` signatures
//! as before.

pub use kcore::cspace::*;

use crate::env::KernelEnv;

/// See [`kcore::cspace::delete`].
pub unsafe fn delete(slot: *mut CapSlot) {
    kcore::cspace::delete(slot, &mut KernelEnv);
}

/// See [`kcore::cspace::revoke`].
pub unsafe fn revoke(slot: *mut CapSlot) {
    kcore::cspace::revoke(slot, &mut KernelEnv);
}

// `unref_aspace` / `unref_cspace` take an env and are reached only from
// inside kcore (thread teardown), so the kernel needs no wrappers for them;
// they remain available as `kcore::cspace::unref_*` if a future shell path
// wants them.
