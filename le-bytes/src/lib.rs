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

//! `le-bytes` — the verified read-direction little-endian byte machinery, shared
//! by every decoder that reads fixed-width little-endian fields off a `&[u8]`
//! buffer (cas's node decoder, loader's ELF `parse`, rev2§6/§8).
//!
//! Three layers, one per width (16/32/64):
//!
//! - [`u16_le`]/[`u32_le`]/[`u64_le`] — the canonical little-endian byte image of
//!   a value, as a `Seq<u8>` (the spec the encoders write against and the readers
//!   prove they consumed).
//! - `lemma_u{16,32,64}_le_bytes` — the empty-bodied `by (bit_vector)` identities
//!   bridging the readers' bit-construction form (`v = b0 | (b1<<8) | …`) to the
//!   `u*_le` shift-extraction form (`v as u8`, `(v >> 8) as u8`, …). Stated once
//!   per width per `doc/guidelines/verus.md` §6 so callers cite the result rather
//!   than re-bit-blasting the identity at every reader.
//! - [`read_u16_le`]/[`read_u32_le`]/[`read_u64_le`] — the exec readers (explicit
//!   index + shift, not `from_le_bytes`/`try_into` which Verus does not spec),
//!   each carrying `requires off+N <= buf@.len()` / `ensures the consumed bytes
//!   are exactly `u*_le(v)`.
//!
//! No backing, no alloc — pure arithmetic over `&[u8]`, so it rides into the
//! userspace cross-build unchanged. The specs are `open` (cross-crate visible) so
//! a consuming decoder's own encode-side specs can cite `u*_le` by full path.
#![cfg_attr(not(any(feature = "std", test)), no_std)]

use vstd::prelude::*;

verus! {

// ── Spec: the canonical little-endian byte image of a fixed-width value ───────
pub open spec fn u16_le(x: u16) -> Seq<u8> {
    seq![x as u8, (x >> 8) as u8]
}

pub open spec fn u32_le(x: u32) -> Seq<u8> {
    seq![x as u8, (x >> 8) as u8, (x >> 16) as u8, (x >> 24) as u8]
}

pub open spec fn u64_le(x: u64) -> Seq<u8> {
    seq![
        x as u8,
        (x >> 8) as u8,
        (x >> 16) as u8,
        (x >> 24) as u8,
        (x >> 32) as u8,
        (x >> 40) as u8,
        (x >> 48) as u8,
        (x >> 56) as u8,
    ]
}

// ── Little-endian byte-split identities (cited by the exec readers below) ────
//
// The readers build `v = b0 | (b1<<8) | …` (bit-construction form) but the
// `u*_le` spec extracts `v as u8`, `(v >> 8) as u8`, … (shift form); the two
// agree only by `bit_vector`. Each lemma takes the constructed `v` plus its
// source bytes (the construction fed in via `requires`) and states the per-byte
// `(v >> 8k) as u8 == bk` extraction. Following the `doc/guidelines/verus.md` §6
// recipe — `by (bit_vector)` on the signature, the facts as `ensures`, empty
// body — keeps the readers free of inline `bit_vector` queries.
/// `v == b0 | b1<<8` splits back into its little-endian bytes `b0`, `b1`.
pub proof fn lemma_u16_le_bytes(v: u16, b0: u8, b1: u8)
    by (bit_vector)
    requires
        v == (b0 as u16) | ((b1 as u16) << 8),
    ensures
        v as u8 == b0,
        (v >> 8) as u8 == b1,
{
}

/// `v == b0 | b1<<8 | b2<<16 | b3<<24` splits back into its bytes `b0..b3`.
pub proof fn lemma_u32_le_bytes(v: u32, b0: u8, b1: u8, b2: u8, b3: u8)
    by (bit_vector)
    requires
        v == (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24),
    ensures
        v as u8 == b0,
        (v >> 8) as u8 == b1,
        (v >> 16) as u8 == b2,
        (v >> 24) as u8 == b3,
{
}

/// `v == b0 | b1<<8 | … | b7<<56` splits back into its bytes `b0..b7`.
pub proof fn lemma_u64_le_bytes(
    v: u64,
    b0: u8,
    b1: u8,
    b2: u8,
    b3: u8,
    b4: u8,
    b5: u8,
    b6: u8,
    b7: u8,
)
    by (bit_vector)
    requires
        v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24) | ((
        b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56),
    ensures
        v as u8 == b0,
        (v >> 8) as u8 == b1,
        (v >> 16) as u8 == b2,
        (v >> 24) as u8 == b3,
        (v >> 32) as u8 == b4,
        (v >> 40) as u8 == b5,
        (v >> 48) as u8 == b6,
        (v >> 56) as u8 == b7,
{
}

// ── Exec byte readers (explicit index + shift, not `from_le_bytes`/`try_into`),
//    each citing the matching byte-split identity above ────────────────────────
pub fn read_u16_le(buf: &[u8], off: usize) -> (v: u16)
    requires
        off + 2 <= buf@.len(),
    ensures
        buf@.subrange(off as int, off as int + 2) == u16_le(v),
{
    broadcast use vstd::slice::group_slice_axioms;

    let b0 = buf[off];
    let b1 = buf[off + 1];
    let v: u16 = (b0 as u16) | ((b1 as u16) << 8);
    proof {
        lemma_u16_le_bytes(v, b0, b1);
    }
    assert(buf@.subrange(off as int, off as int + 2) =~= u16_le(v));
    v
}

pub fn read_u32_le(buf: &[u8], off: usize) -> (v: u32)
    requires
        off + 4 <= buf@.len(),
    ensures
        buf@.subrange(off as int, off as int + 4) == u32_le(v),
{
    broadcast use vstd::slice::group_slice_axioms;

    let b0 = buf[off];
    let b1 = buf[off + 1];
    let b2 = buf[off + 2];
    let b3 = buf[off + 3];
    let v: u32 = (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24);
    proof {
        lemma_u32_le_bytes(v, b0, b1, b2, b3);
    }
    assert(buf@.subrange(off as int, off as int + 4) =~= u32_le(v));
    v
}

pub fn read_u64_le(buf: &[u8], off: usize) -> (v: u64)
    requires
        off + 8 <= buf@.len(),
    ensures
        buf@.subrange(off as int, off as int + 8) == u64_le(v),
{
    broadcast use vstd::slice::group_slice_axioms;

    let b0 = buf[off];
    let b1 = buf[off + 1];
    let b2 = buf[off + 2];
    let b3 = buf[off + 3];
    let b4 = buf[off + 4];
    let b5 = buf[off + 5];
    let b6 = buf[off + 6];
    let b7 = buf[off + 7];
    let v: u64 = (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24) | ((
    b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    proof {
        lemma_u64_le_bytes(v, b0, b1, b2, b3, b4, b5, b6, b7);
    }
    assert(buf@.subrange(off as int, off as int + 8) =~= u64_le(v));
    v
}

} // verus!
