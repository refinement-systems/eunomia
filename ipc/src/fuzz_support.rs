// SPDX-License-Identifier: 0BSD
//! Fuzz/test support for the wire codec: a representative *boring* body
//! type and the round-trip oracle, shared by the cargo-fuzz target
//! (`ipc/fuzz/fuzz_targets/wire_decode.rs`), the corpus-replay test
//! (`tests/fuzz_corpus.rs`), and the seed generator
//! (`examples/gen_ipc_corpus.rs`).
//!
//! `fuzzing`-feature-gated — not part of the production API. There is no real
//! server body type until the session/protocol work; until then
//! this stand-in exercises the codec over scalars, a string, a byte vector, and
//! several enum variants.

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

use crate::header::Header;
use crate::wire::{self, WireError};

/// A deliberately boring message body (rev2§3.7): owned fields, no borrows, an
/// externally-tagged enum, no `flatten`/untagged/non-string-keyed maps — the
/// subset that maps 1:1 onto any IDL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DemoMsg {
    Ping,
    Open { name: String, flags: u32 },
    Read { handle: u32, offset: u64, len: u32 },
    Data(Vec<u8>),
    Error(u16),
}

/// Demo protocol id / version stamped into the header by [`encode_demo`].
const PROTO: u8 = 0xDE;
const VERSION: u8 = 1;

/// Encode a `DemoMsg` into a full wire message (header + postcard body). A
/// value that round-trips through `decode_demo` always re-encodes, so the
/// oracle's `expect` cannot fire on bounded input.
pub fn encode_demo(m: &DemoMsg) -> Vec<u8> {
    wire::encode(PROTO, VERSION, 0, 0, m).expect("DemoMsg encodes")
}

/// Decode arbitrary bytes as a `DemoMsg` message. **Total** — the fuzz target
/// and corpus replay rely on it never panicking.
pub fn decode_demo(buf: &[u8]) -> Result<(Header, DemoMsg), WireError> {
    wire::decode::<DemoMsg>(buf)
}
