//! cfg-selected concurrency primitives for the IPC model + harnesses (plan
//! `doc/plans/2_ipc.md` §3.2): `std` by default, `loom` under `--cfg loom`,
//! `shuttle` under `--cfg shuttle`. Mirrors `urt`'s proven Phase-1 seam.
//!
//! The production `no_std` crate uses **none** of this — it is single-threaded
//! and talks to the real kernel through `SyscallTransport`. This module (and
//! its only user, `crate::model`) is compiled only for the model/harnesses
//! (`#[cfg(any(test, loom, shuttle))]` in `lib.rs`), where the host's `std` is
//! available regardless of the crate's `no_std` attribute.

#[cfg(loom)]
pub use loom::sync::{Arc, Condvar, Mutex};
#[cfg(loom)]
pub use loom::thread;

#[cfg(shuttle)]
pub use shuttle::sync::{Arc, Condvar, Mutex};
#[cfg(shuttle)]
pub use shuttle::thread;

#[cfg(all(not(loom), not(shuttle)))]
pub use std::sync::{Arc, Condvar, Mutex};
#[cfg(all(not(loom), not(shuttle)))]
pub use std::thread;
