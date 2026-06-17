//! Userspace IPC crate — shared by every server (spec §3.5, §3.7).
//!
//! Responsibilities (M1+):
//!   - Non-blocking send/recv over kernel channels (§4.1)
//!   - `Full` backpressure: blocking + bounded-retry send (§4.3; no executor
//!     exists, so there is no `async`/`.await` form)
//!   - The epoll-shaped reactor: multiplex sources behind one wait, bits hidden (§4.2)
//!   - Valuable-cap ack protocol (§4.4)
//!   - Postcard message (de)serialisation (module-private, behind the `wire`
//!     feature so alloc-free binaries stay minimal; §4.5)
//!   - Session admission quota (§4.6)
//!   - Lost-wakeup discipline around the notification object (§3.6)
//!
//! The reactor API is epoll-shaped — `register(source, signals, key)` —
//! implemented over notification bit-groups for M1 and upgraded to the
//! kernel wait-set object when that lands (spec §3.6).

#![cfg_attr(not(feature = "std"), no_std)]

// Verus (plan doc/plans/3_verus-rewrite.md phase 7a): the deductive-proof tier
// for the §4.7 host chokepoints. `vstd::prelude` supplies the `verus!{}` macro +
// ghost vocabulary the `header` proof uses; Verus requires it imported at the
// crate root. In an ordinary build the macro erases ghost code, so this import is
// otherwise unused — hence the allow (same as kcore/src/lib.rs).
#[allow(unused_imports)]
use vstd::prelude::*;

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
