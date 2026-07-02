// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

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
