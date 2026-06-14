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

// alloc rides with the `wire` feature: the wire codec (§3.7) (de)serializes
// owned, variable-length bodies, so it needs the heap (urt provides it on the
// OS), exactly like storage-server. Minimal binaries that use only `ipc::sys`
// (hello/selftest/init) build without `wire` and stay no-alloc. The model + its
// sync seam are host-only (std-backed) and compiled solely for the
// test/loom/shuttle builds (plan §3.2).
#[cfg(feature = "wire")]
extern crate alloc;
#[cfg(any(test, loom, shuttle))]
extern crate std;

pub mod endpoint;
pub mod header;
pub mod reactor;
pub mod session;
pub mod sys;
pub mod transport;
#[cfg(feature = "wire")]
pub mod wire;

// The server-facing surface (§4.1, §4.2, §4.5): the typed non-blocking
// primitives, the epoll-shaped reactor, and the wire codec, over the kernel
// transport seam.
pub use endpoint::{Endpoint, Message, MAX_PAYLOAD};
pub use reactor::{Key, Reactor, RegisterErr, Signals};
pub use session::{admit_connect, Admission, ConnectErr, ConnectReq, GrantReply, WindowGrant};
pub use transport::{
    Chan, Event, RecvErr, RecvOk, SendErr, SyscallTransport, Transport,
};
#[cfg(feature = "wire")]
pub use wire::{decode, encode, WireError};

/// Representative body type + round-trip oracle for fuzzing the wire codec
/// (§5.4); not part of the production API.
#[cfg(feature = "fuzzing")]
pub mod fuzz_support;

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
