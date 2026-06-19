//! cfg-selected concurrency primitives for the IPC model + harnesses:
//! `std` by default, `loom` under `--cfg loom`,
//! `shuttle` under `--cfg shuttle`. Mirrors `urt`'s concurrency seam.
//!
//! The production `no_std` crate uses **none** of this — it is single-threaded
//! and talks to the real kernel through `SyscallTransport`. This module (and
//! its only user, `crate::model`) is compiled only for the model/harnesses
//! (`#[cfg(any(test, loom, shuttle))]` in `lib.rs`), where the host's `std` is
//! available regardless of the crate's `no_std` attribute.

#[cfg(loom)]
pub use loom::sync::{Arc, Condvar, Mutex};
// `thread` is harness-only (the ModelTransport itself spawns nothing), so it is
// `test`-gated — otherwise the non-test library build sees an unused re-export.
#[cfg(all(test, loom))]
pub use loom::thread;

#[cfg(shuttle)]
pub use shuttle::sync::{Arc, Condvar, Mutex};
#[cfg(all(test, shuttle))]
pub use shuttle::thread;

#[cfg(all(not(loom), not(shuttle)))]
pub use std::sync::{Arc, Condvar, Mutex};
#[cfg(all(test, not(loom), not(shuttle)))]
pub use std::thread;
