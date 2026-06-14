//! Kernel object core (plan §2): the architecture-independent kernel object
//! machinery — capability spaces and the CDT, untyped retype arithmetic,
//! IPC channels, notifications, thread objects and reports, timer lists, and
//! the address-space *data* type. Extracted from the `kernel` crate so it
//! builds for the host, where Kani verifies its invariants (plan §4) and
//! ordinary `cargo test` runs the well-formedness predicates as unit tests.
//!
//! Layering rules that keep this crate verifiable (plan §2.2; CI-grepped):
//!   1. No `asm!`, no `global_asm!`, no MMIO addresses, no register access.
//!   2. No integer→pointer casts. Every raw pointer entering kcore is
//!      produced by the caller — the `kernel` shell from physical addresses
//!      at its one sanctioned boundary (`untyped::retype`), and Kani
//!      harnesses / host tests from ordinary Rust allocations — so CBMC only
//!      ever sees provenance-carrying pointers.
//!   3. Hardware effects and the scheduler live behind the [`env::Env`]
//!      trait. The kernel implements it with the real `tlbi`/`dsb` sequences
//!      and ready queues; the Kani/host ghost impl records calls so
//!      harnesses can *assert* against them.
//!
//! The kernel is single-core and non-preemptible (IRQs masked at EL1), so
//! whoever runs kernel code has exclusive access to all kernel objects; the
//! raw-pointer dereferences here rely on that plus the per-function
//! ownership contracts.
#![cfg_attr(not(test), no_std)]

// Verus (plan doc/plans/3_verus-rewrite.md, phase 0): the deductive-proof tier
// for kcore. `vstd::prelude` supplies the `verus!{}` macro + ghost vocabulary the
// proofs use (starting with `untyped::carve`); Verus requires it imported at the
// crate root. In an ordinary build the macro erases ghost code, so this import is
// otherwise unused — hence the allow.
#[allow(unused_imports)]
use vstd::prelude::*;

pub mod aspace;
pub mod channel;
pub mod cspace;
pub mod env;
pub mod id;
pub mod notification;
pub mod store;
pub mod sysabi;
pub mod thread;
pub mod timer;
pub mod untyped;

/// Proof infrastructure and Kani harnesses (plan §4.1). **Disabled during the
/// Verus phase-1 arena rewrite** (`doc/plans/3_verus-rewrite.md`): these
/// harnesses are written against the old raw-pointer model and are superseded
/// by the Verus proofs that land per-subsystem in phases 2–5, which delete them.
/// Gated behind an off-by-default feature so the per-PR `kani` job (kcore leg)
/// and `cargo test` stay green on the new model; the boot tests are the phase-1
/// gate (plan §9). Re-enabling is not intended — phase 2 removes this module.
#[cfg(all(any(kani, test), feature = "legacy_ptr_harness"))]
pub mod proofs;
