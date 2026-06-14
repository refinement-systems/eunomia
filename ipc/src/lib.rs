//! Userspace IPC crate — shared by every server (spec §3.5, §3.7).
//!
//! Responsibilities (M1+):
//!   - Async send/recv over kernel channels
//!   - FULL backpressure and retry
//!   - Valuable-cap ack protocol
//!   - Postcard message (de)serialisation (module-private)
//!   - Lost-wakeup discipline around the notification object
//!
//! The reactor API is epoll-shaped — `register(source, signals, key)` —
//! implemented over notification bit-groups for M1 and upgraded to the
//! kernel wait-set object when that lands (spec §3.6).

#![cfg_attr(not(feature = "std"), no_std)]

// The model + its sync seam are host-only (std-backed) and compiled solely for
// the test/loom/shuttle builds (plan §3.2); production stays no_std.
#[cfg(any(test, loom, shuttle))]
extern crate std;

pub mod endpoint;
pub mod header;
pub mod reactor;
pub mod sys;
pub mod transport;

// The server-facing surface (§4.1, §4.2): the typed non-blocking primitives and
// the epoll-shaped reactor, over the kernel transport seam.
pub use endpoint::{Endpoint, Message, MAX_PAYLOAD};
pub use reactor::{Key, Reactor, RegisterErr, Signals};
pub use transport::{
    Chan, Event, RecvErr, RecvOk, SendErr, SyscallTransport, Transport,
};

/// The cfg-swappable concurrency seam + the deterministic in-memory kernel
/// (`ModelTransport`) the Shuttle/Loom harnesses drive (plan §3.2–§3.4). Not in
/// the production no_std build.
#[cfg(any(test, loom, shuttle))]
mod sync;
#[cfg(any(test, loom, shuttle))]
pub mod model;

/// Kani harnesses (plan §4.7), compiled only under `cargo kani`.
#[cfg(kani)]
mod proofs;
