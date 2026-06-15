//! Kernel object core (plan §2): the architecture-independent kernel object
//! machinery — capability spaces and the CDT, untyped retype arithmetic,
//! IPC channels, notifications, thread objects and reports, timer lists, and
//! the address-space *data* type. Extracted from the `kernel` crate so it
//! builds for the host, where Verus deductively verifies its invariants
//! (plan §4, the `verus` CI job) and ordinary `cargo test` runs the host
//! unit tests.
//!
//! Layering rules that keep this crate verifiable (plan §2.2; CI-grepped):
//!   1. No `asm!`, no `global_asm!`, no MMIO addresses, no register access.
//!   2. No integer→pointer casts — in fact no raw pointers at all in the
//!      verified core: every kernel object and cap slot is reached through an
//!      opaque [`id::ObjId`]/[`id::SlotId`] handle resolved by the
//!      [`store::Store`] seam (the arena rewrite, plan §3). The `kernel` shell
//!      maps a handle to an address at its one sanctioned `unsafe` boundary.
//!   3. Hardware effects and the scheduler also live behind [`store::Store`]
//!      (it folds in the former `Env` seam). The kernel implements them with
//!      the real `tlbi`/`dsb` sequences and ready queues.
//!
//! The kernel is single-core and non-preemptible (IRQs masked at EL1), so
//! whoever runs kernel code has exclusive access to all kernel objects; the
//! operations here read as pure functions over the abstract indexed store.
#![cfg_attr(not(test), no_std)]

// Verus (plan doc/plans/3_verus-rewrite.md): the deductive-proof tier for kcore.
// `vstd::prelude` supplies the `verus!{}` macro + ghost vocabulary the proofs use
// (the untyped::carve geometry, the cspace/CDT contracts); Verus requires it
// imported at the crate root. In an ordinary build the macro erases ghost code,
// so this import is otherwise unused — hence the allow.
#[allow(unused_imports)]
use vstd::prelude::*;

pub mod aspace;
pub mod channel;
pub mod cspace;
pub mod id;
pub mod notification;
pub mod store;
pub mod sysabi;
pub mod thread;
pub mod timer;
pub mod untyped;

// The array-backed `Store` + executable contract checks for the `external_body`
// cspace ops (plan §3 host-test; doc/results/22 §4).
#[cfg(test)]
mod test_store;
