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

//! Little-endian byte split/reassemble identities for the hand-written codecs
//! (spec rev2§3.7). The header and session wire forms encode `u16`/`u32` fields
//! as explicit `|`/`<<`/`&` mask-shift arithmetic (not `to_le_bytes`, which Verus
//! does not spec), so each codec-bijection lemma needs the same two facts per
//! width: reassembling a value from its little-endian byte split recovers the
//! value, and splitting a reassembled value recovers each byte. These four
//! `by (bit_vector)` lemmas state those facts once so [`crate::header`] and
//! [`crate::session`] cite them instead of re-deriving them inline.
//!
//! Recipe form (`doc/guidelines/verus.md` §6): `by (bit_vector)` on the
//! signature, the fact stated as an unconditional `ensures`, empty body.
#[allow(unused_imports)]
use vstd::prelude::*;

verus! {

/// Reassembling a `u16` from its little-endian byte split recovers the value.
pub(crate) proof fn lemma_u16_le_reassemble(x: u16)
    by (bit_vector)
    ensures
        ((x & 0xff) as u8 as u16) | (((x >> 8) & 0xff) as u8 as u16) << 8 == x,
{
}

/// Splitting `(b0 | b1<<8)` back into little-endian bytes recovers `b0`, `b1`.
pub(crate) proof fn lemma_u16_le_split_bytes(b0: u8, b1: u8)
    by (bit_vector)
    ensures
        (((b0 as u16) | ((b1 as u16) << 8)) & 0xff) as u8 == b0,
        ((((b0 as u16) | ((b1 as u16) << 8)) >> 8) & 0xff) as u8 == b1,
{
}

/// Reassembling a `u32` from its little-endian byte split recovers the value.
pub(crate) proof fn lemma_u32_le_reassemble(x: u32)
    by (bit_vector)
    ensures
        ((x & 0xff) as u8 as u32) | (((x >> 8) & 0xff) as u8 as u32) << 8 | (((x >> 16)
            & 0xff) as u8 as u32) << 16 | (((x >> 24) & 0xff) as u8 as u32) << 24 == x,
{
}

/// Splitting `(b0 | b1<<8 | b2<<16 | b3<<24)` back into bytes recovers each byte.
pub(crate) proof fn lemma_u32_le_split_bytes(b0: u8, b1: u8, b2: u8, b3: u8)
    by (bit_vector)
    ensures
        (((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24))
            & 0xff) as u8 == b0,
        ((((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) >> 8)
            & 0xff) as u8 == b1,
        ((((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) >> 16)
            & 0xff) as u8 == b2,
        ((((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) >> 24)
            & 0xff) as u8 == b3,
{
}

} // verus!
