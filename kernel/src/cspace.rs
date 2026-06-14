//! Kernel-side capability-space surface: the object machinery lives in
//! [`kcore::cspace`] (host-buildable, plan §4.1); this module re-exports it and
//! supplies the `KernelStore`-bound wrappers for the few ops that fire events or
//! tear objects down — wrapping the raw `*mut CapSlot` call sites into
//! [`SlotId`](kcore::id::SlotId) handles. Call sites elsewhere in the kernel see
//! the same `cspace::delete(slot)` / `cspace::revoke(slot)` signatures as before.

pub use kcore::cspace::*;

use crate::store::KernelStore;
use kcore::id::SlotId;

/// See [`kcore::cspace::delete`].
pub unsafe fn delete(slot: *mut CapSlot) {
    kcore::cspace::delete(&mut KernelStore, SlotId(slot as u64));
}

/// See [`kcore::cspace::revoke`].
pub unsafe fn revoke(slot: *mut CapSlot) {
    kcore::cspace::revoke(&mut KernelStore, SlotId(slot as u64));
}

// `unref_aspace` / `unref_cspace` take a store and are reached only from
// inside kcore (thread teardown), so the kernel needs no wrappers for them;
// they remain available as `kcore::cspace::unref_*` if a future shell path
// wants them.
