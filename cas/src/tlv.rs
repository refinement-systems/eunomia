// SPDX-License-Identifier: 0BSD
//! Single directory-entry TLV codec (spec rev2§4.9), exposed as a standalone
//! encode/decode pair for the canonical-form fuzz oracle.
//!
//! Entries normally live only inside prolly-tree leaf nodes; this module
//! lifts one entry's deterministic TLV out so a fuzzer can drive the
//! decoder on raw bytes and re-encode the result. The format promises
//! *exactly one encoding per logical entry* (sorted optional tags, absent
//! fields contributing zero bytes, no slack), so `encode(decode(b)) == b`
//! for every `b` the decoder accepts — the strongest, cheapest property
//! the format affords. `decode` here adds the whole-buffer-consumed check
//! that the in-node decoder gets from its surrounding node framing.

use crate::prolly::{decode_entry, encode_entry, Entry, FormatError, Reader};
use alloc::vec::Vec;

/// Decode exactly one entry, requiring the entire buffer to be consumed
/// (trailing bytes are an error — a single entry has one encoding).
pub fn decode(buf: &[u8]) -> Result<Entry, FormatError> {
    let mut r = Reader { buf, pos: 0 };
    let entry = decode_entry(&mut r)?;
    if !r.done() {
        return Err(FormatError::BadEntry("entry trailing bytes"));
    }
    Ok(entry)
}

/// Encode one entry to its canonical TLV bytes.
pub fn encode(entry: &Entry) -> Vec<u8> {
    let mut out = Vec::new();
    encode_entry(entry, &mut out);
    out
}
