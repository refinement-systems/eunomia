//! Kernel object core: the architecture-independent kernel object machinery —
//! capability spaces and the CDT, untyped retype arithmetic, IPC channels,
//! notifications, thread objects and reports, timer lists, and the
//! address-space *data* type. Extracted from the `kernel` crate so it
//! builds for the host, where Verus deductively verifies its invariants
//! (the `verus` CI job) and ordinary `cargo test` runs the host unit tests.
//!
//! Layering rules that keep this crate verifiable (CI-grepped):
//!   1. No `asm!`, no `global_asm!`, no MMIO addresses, no register access.
//!   2. No integer→pointer casts — in fact no raw pointers at all in the
//!      verified core: every kernel object and cap slot is reached through an
//!      opaque [`id::ObjId`]/[`id::SlotId`] handle resolved by the
//!      [`store::Store`] seam. The `kernel` shell
//!      maps a handle to an address at its one sanctioned `unsafe` boundary.
//!   3. Hardware effects and the scheduler also live behind [`store::Store`]
//!      (it folds in the former `Env` seam). The kernel implements them with
//!      the real `tlbi`/`dsb` sequences and ready queues.
//!
//! The kernel is single-core and non-preemptible (IRQs masked at EL1), so
//! whoever runs kernel code has exclusive access to all kernel objects; the
//! operations here read as pure functions over the abstract indexed store.
#![cfg_attr(not(test), no_std)]
// Clippy is not a CI gate for this project. These lints fire inside
// `verus!{}` verified exec code, where the flagged forms are deliberate —
// explicit arithmetic and control-flow that Verus reasons about directly
// (`x = x + y`, an explicit `match` rather than `?`, `a % n == 0` rather than
// `is_multiple_of`, explicit range bounds), wide-but-cohesive verified
// signatures, and raw-pointer object accessors documented with prose pre/post
// comments rather than a `# Safety` heading. Refactoring verified code to
// satisfy them would be cosmetic churn in verified code, so they are
// suppressed, not applied.
#![allow(
    clippy::assign_op_pattern,
    clippy::collapsible_match,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::missing_safety_doc,
    clippy::question_mark,
    clippy::result_unit_err,
    clippy::too_many_arguments
)]

// Verus: the deductive-proof tier for kcore.
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
pub mod irq;
pub mod notification;
pub mod ready;
pub mod store;
pub mod sysabi;
pub mod thread;
pub mod timer;
pub mod untyped;

// The array-backed `Store` + executable contract checks for the cspace ops.
#[cfg(test)]
mod test_store;
