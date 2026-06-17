//! The fixed, hand-defined message header (spec §3.7). Every IPC message is
//! this header followed by a postcard-encoded body. The header layout **never
//! migrates** — it is the layer that makes every other layer migratable — so
//! it is a byte-exact, hand-written codec (no serde), 10 bytes, little-endian:
//!
//! ```text
//!   off 0  proto    : u8    protocol id
//!   off 1  version  : u8    protocol version
//!   off 2  opcode   : u16   request/response selector
//!   off 4  flags    : u16   message flags
//!   off 6  body_len : u32   length of the postcard body that follows
//! ```
//!
//! `decode` is a *total bijection* on exactly `HEADER_SIZE` bytes: it does no
//! field-value validation (a server validates `proto`/`version`/`opcode`
//! against what it speaks — spec §3.7's "unknown opcode yields an error" is a
//! dispatch concern, not the header layer's), which keeps `encode`∘`decode`
//! the identity.
//!
//! **Verified by Verus** (plan doc/plans/3_verus-rewrite_phase7-detail.md §7a —
//! the §4.7 host-chokepoint pilot). The exec [`Header::decode`]/[`Header::encode`]
//! are tied to the ghost [`spec_decode`]/[`spec_encode`] by their `ensures`, and
//! [`lemma_decode_encode`]/[`lemma_encode_decode`] prove the bijection ∀ — both
//! directions of `encode`∘`decode` = id, and decode's totality / accept-iff-length.
//! This supersedes the bounded Kani harnesses that lived in `crate::proofs`.
//!
//! The codec is written with explicit mask/shift arithmetic (not
//! `to_le_bytes`/`from_le_bytes` + `copy_from_slice`, which Verus does not spec)
//! so the proof reasons natively over `|`/`<<`/`&` and `vstd` stays ghost-only —
//! no `vstd` exec call survives erasure into the alloc-free user binaries. The
//! bytes produced are identical to the previous `to_le_bytes` form.

use vstd::prelude::*;

verus! {

/// Wire size of the fixed header, in bytes.
pub const HEADER_SIZE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub proto: u8,
    pub version: u8,
    pub opcode: u16,
    pub flags: u16,
    pub body_len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// `buf` was not exactly `HEADER_SIZE` bytes — too short, or trailing
    /// bytes after the fixed header.
    BadLength,
}

/// Ghost model of [`Header::encode`]: the little-endian byte layout as a `Seq`.
/// Each multi-byte field is split low-to-high, matching `to_le_bytes`.
pub open spec fn spec_encode(h: Header) -> Seq<u8> {
    seq![
        h.proto,
        h.version,
        (h.opcode & 0xff) as u8,
        ((h.opcode >> 8) & 0xff) as u8,
        (h.flags & 0xff) as u8,
        ((h.flags >> 8) & 0xff) as u8,
        (h.body_len & 0xff) as u8,
        ((h.body_len >> 8) & 0xff) as u8,
        ((h.body_len >> 16) & 0xff) as u8,
        ((h.body_len >> 24) & 0xff) as u8,
    ]
}

/// Ghost model of [`Header::decode`]: `Ok` iff exactly `HEADER_SIZE` bytes,
/// reassembling each little-endian field; `Err(BadLength)` otherwise (short
/// input and trailing bytes alike). Total over every byte string.
pub open spec fn spec_decode(s: Seq<u8>) -> Result<Header, HeaderError> {
    if s.len() == HEADER_SIZE {
        Ok(Header {
            proto: s[0],
            version: s[1],
            opcode: (s[2] as u16) | ((s[3] as u16) << 8),
            flags: (s[4] as u16) | ((s[5] as u16) << 8),
            body_len: (s[6] as u32) | ((s[7] as u32) << 8) | ((s[8] as u32) << 16)
                | ((s[9] as u32) << 24),
        })
    } else {
        Err(HeaderError::BadLength)
    }
}

impl Header {
    pub fn encode(&self) -> (b: [u8; HEADER_SIZE])
        ensures
            b@ == spec_encode(*self),
    {
        broadcast use vstd::array::group_array_axioms;
        let b: [u8; HEADER_SIZE] = [
            self.proto,
            self.version,
            (self.opcode & 0xff) as u8,
            ((self.opcode >> 8) & 0xff) as u8,
            (self.flags & 0xff) as u8,
            ((self.flags >> 8) & 0xff) as u8,
            (self.body_len & 0xff) as u8,
            ((self.body_len >> 8) & 0xff) as u8,
            ((self.body_len >> 16) & 0xff) as u8,
            ((self.body_len >> 24) & 0xff) as u8,
        ];
        assert(b@ =~= spec_encode(*self));
        b
    }

    /// Decode exactly `HEADER_SIZE` bytes. Rejects any other length (short
    /// input *and* trailing bytes); otherwise total over all byte values.
    pub fn decode(buf: &[u8]) -> (r: Result<Header, HeaderError>)
        ensures
            r == spec_decode(buf@),
            r is Ok <==> buf@.len() == HEADER_SIZE,
    {
        broadcast use vstd::slice::group_slice_axioms;
        if buf.len() != HEADER_SIZE {
            return Err(HeaderError::BadLength);
        }
        Ok(Header {
            proto: buf[0],
            version: buf[1],
            opcode: (buf[2] as u16) | ((buf[3] as u16) << 8),
            flags: (buf[4] as u16) | ((buf[5] as u16) << 8),
            body_len: (buf[6] as u32) | ((buf[7] as u32) << 8) | ((buf[8] as u32) << 16)
                | ((buf[9] as u32) << 24),
        })
    }
}

/// `decode`∘`encode` is the identity on `Header` values: every header round-trips
/// through its `HEADER_SIZE`-byte encoding.
pub proof fn lemma_decode_encode(h: Header)
    ensures
        spec_decode(spec_encode(h)) == Ok::<Header, HeaderError>(h),
{
    let s = spec_encode(h);
    assert(s.len() == HEADER_SIZE);
    // bit_vector reasons over plain fixed-width vars, not struct field projections.
    let op = h.opcode; let fl = h.flags; let bl = h.body_len;
    // Each multi-byte field reassembles from its split bytes (low | high<<k).
    assert(((op & 0xff) as u8 as u16) | (((op >> 8) & 0xff) as u8 as u16) << 8 == op)
        by (bit_vector);
    assert(((fl & 0xff) as u8 as u16) | (((fl >> 8) & 0xff) as u8 as u16) << 8 == fl)
        by (bit_vector);
    assert(((bl & 0xff) as u8 as u32)
        | (((bl >> 8) & 0xff) as u8 as u32) << 8
        | (((bl >> 16) & 0xff) as u8 as u32) << 16
        | (((bl >> 24) & 0xff) as u8 as u32) << 24
        == bl) by (bit_vector);
}

/// `encode`∘`decode` is the identity on `HEADER_SIZE`-byte strings: decoding any
/// exact-length buffer and re-encoding reproduces it. Together with
/// [`lemma_decode_encode`] this makes the codec a total bijection between
/// `Header` values and `HEADER_SIZE`-byte strings.
pub proof fn lemma_encode_decode(s: Seq<u8>)
    requires
        s.len() == HEADER_SIZE,
    ensures
        spec_encode(spec_decode(s)->Ok_0) == s,
{
    let s2 = s[2]; let s3 = s[3];
    let s4 = s[4]; let s5 = s[5];
    let s6 = s[6]; let s7 = s[7]; let s8 = s[8]; let s9 = s[9];
    // The split of a reassembled field recovers the original bytes.
    assert((((s2 as u16) | ((s3 as u16) << 8)) & 0xff) as u8 == s2) by (bit_vector);
    assert(((((s2 as u16) | ((s3 as u16) << 8)) >> 8) & 0xff) as u8 == s3) by (bit_vector);
    assert((((s4 as u16) | ((s5 as u16) << 8)) & 0xff) as u8 == s4) by (bit_vector);
    assert(((((s4 as u16) | ((s5 as u16) << 8)) >> 8) & 0xff) as u8 == s5) by (bit_vector);
    // bit_vector sees only the variables in the asserted expression (not the
    // surrounding `let`), so the reassembly is written inline here.
    assert((((s6 as u32) | ((s7 as u32) << 8) | ((s8 as u32) << 16) | ((s9 as u32) << 24))
        & 0xff) as u8 == s6) by (bit_vector);
    assert(((((s6 as u32) | ((s7 as u32) << 8) | ((s8 as u32) << 16) | ((s9 as u32) << 24))
        >> 8) & 0xff) as u8 == s7) by (bit_vector);
    assert(((((s6 as u32) | ((s7 as u32) << 8) | ((s8 as u32) << 16) | ((s9 as u32) << 24))
        >> 16) & 0xff) as u8 == s8) by (bit_vector);
    assert(((((s6 as u32) | ((s7 as u32) << 8) | ((s8 as u32) << 16) | ((s9 as u32) << 24))
        >> 24) & 0xff) as u8 == s9) by (bit_vector);
    assert(spec_encode(spec_decode(s)->Ok_0) =~= s);
}

} // verus!

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = Header { proto: 0x51, version: 2, opcode: 7, flags: 0x8000, body_len: 123 };
        assert_eq!(Header::decode(&h.encode()), Ok(h));
    }

    #[test]
    fn wrong_length_rejected() {
        assert_eq!(Header::decode(&[0u8; HEADER_SIZE - 1]), Err(HeaderError::BadLength));
        assert_eq!(Header::decode(&[0u8; HEADER_SIZE + 1]), Err(HeaderError::BadLength));
    }
}
