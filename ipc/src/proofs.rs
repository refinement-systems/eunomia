//! Kani harnesses for the fixed message header (plan §4.7): `decode` is total
//! over all byte strings and rejects any non-`HEADER_SIZE` length (short input
//! and trailing bytes alike), and `encode`/`decode` are mutually inverse — the
//! bijection that lets the header layer stay byte-stable while every layer
//! above it migrates (spec §3.7).

#![cfg(kani)]

use crate::header::{Header, HeaderError, HEADER_SIZE};

/// `decode` never panics for any input, and accepts **iff** the length is
/// exactly `HEADER_SIZE` (trailing-byte / short-input rejection).
#[kani::proof]
#[kani::unwind(20)]
fn check_header_decode_total() {
    // A nondet buffer of nondet length up to 2*HEADER_SIZE (covers short,
    // exact, and trailing-byte cases).
    let bytes: [u8; 2 * HEADER_SIZE] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 2 * HEADER_SIZE);
    let r = Header::decode(&bytes[..len]);
    assert!(r.is_ok() == (len == HEADER_SIZE));
    if len != HEADER_SIZE {
        assert!(r == Err(HeaderError::BadLength));
    }
    // Guard against an over-constraining `assume` collapsing this to one case
    // (rec. #3): both the accept (exact length) and reject (short/trailing)
    // outcomes must be reachable.
    kani::cover!(r.is_ok());
    kani::cover!(r.is_err());
}

/// `encode`∘`decode` is the identity in both directions — a total bijection
/// between `Header` values and `HEADER_SIZE`-byte strings.
#[kani::proof]
fn check_header_roundtrip() {
    // value → bytes → value
    let h = Header {
        proto: kani::any(),
        version: kani::any(),
        opcode: kani::any(),
        flags: kani::any(),
        body_len: kani::any(),
    };
    assert!(Header::decode(&h.encode()) == Ok(h));

    // bytes → value → bytes
    let b: [u8; HEADER_SIZE] = kani::any();
    let decoded = Header::decode(&b).unwrap(); // always Ok at exact length
    assert!(decoded.encode() == b);
}
