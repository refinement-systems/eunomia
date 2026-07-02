// SPDX-License-Identifier: 0BSD
//! Userspace IPC crate — shared by every server (spec rev2§3.5, rev2§3.7).
//!
//! Responsibilities:
//!   - Non-blocking send/recv over kernel channels
//!   - `Full` backpressure: blocking + bounded-retry send (no executor
//!     exists, so there is no `async`/`.await` form)
//!   - The epoll-shaped reactor: multiplex sources behind one wait, bits hidden
//!   - Valuable-cap ack protocol
//!   - Postcard message (de)serialisation (module-private, behind the `wire`
//!     feature so alloc-free binaries stay minimal)
//!   - Session admission quota
//!   - Lost-wakeup discipline around the notification object (rev2§3.6)
//!
//! The reactor API is epoll-shaped — `register(source, signals, key)` —
//! implemented over notification bit-groups, and can be upgraded to the
//! kernel wait-set object when that lands (spec rev2§3.6).
#![cfg_attr(not(feature = "std"), no_std)]
// Clippy is not a CI gate: this fires in `verus!{}` verified exec
// code where the explicit `x = x + y` form is what Verus reasons about — fixing
// it would refactor verified code for cosmetic gain.
#![allow(clippy::assign_op_pattern)]

// Verus is the deductive-proof tier for the host chokepoints. `vstd::prelude`
// supplies the `verus!{}` macro + ghost vocabulary the `header` proof uses; Verus
// requires it imported at the crate root. In an ordinary build the macro erases
// ghost code, so this import is otherwise unused — hence the allow (same as
// kcore/src/lib.rs).
#[allow(unused_imports)]
use vstd::prelude::*;

// alloc rides with the `wire` feature: the wire codec (rev2§3.7) (de)serializes
// owned, variable-length bodies, so it needs the heap (urt provides it on the
// OS), exactly like storage-server. Minimal binaries that use only `ipc::sys`
// (hello/selftest/init) build without `wire` and stay no-alloc. The model + its
// sync seam are host-only (std-backed) and compiled solely for the
// test/loom/shuttle builds.
#[cfg(feature = "wire")]
extern crate alloc;
#[cfg(any(test, loom, shuttle))]
extern crate std;

pub mod endpoint;
pub mod header;
// Little-endian split/reassemble bit identities shared by the `header` and
// `session` codec lemmas; crate-internal proof helpers, not part of the surface.
pub(crate) mod le_bytes;
pub mod reactor;
pub mod session;
pub mod sys;
pub mod transport;
#[cfg(feature = "wire")]
pub mod wire;

// The server-facing surface: the typed non-blocking primitives, the
// epoll-shaped reactor, and the wire codec, over the kernel transport seam.
pub use endpoint::{Endpoint, Message, MAX_PAYLOAD};
pub use reactor::{Key, Reactor, RegisterErr, Signals};
pub use session::{
    admit_connect, connect, negotiate, version_ok, Admission, ConnectErr, ConnectReq, GrantReply,
    VersionRange, WindowGrant, PROTOCOL_VERSION,
};
pub use transport::{Chan, Event, RecvErr, RecvOk, SendErr, SyscallTransport, Transport};
#[cfg(feature = "wire")]
pub use wire::{decode, encode, WireError};

/// Representative body type + round-trip oracle for fuzzing the wire codec;
/// not part of the production API.
#[cfg(feature = "fuzzing")]
pub mod fuzz_support;

#[cfg(any(test, loom, shuttle))]
pub mod model;
/// The cfg-swappable concurrency seam + the deterministic in-memory kernel
/// (`ModelTransport`) the Shuttle/Loom harnesses drive. Not in the production
/// no_std build.
#[cfg(any(test, loom, shuttle))]
mod sync;
