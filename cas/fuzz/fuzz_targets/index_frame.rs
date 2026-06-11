#![no_main]
//! The format-v2 durable index: the hash→extent map plus the free-extent
//! list, one frame (§4.2 items 3–4). We fuzz `decode_index` directly —
//! the frame's content hash is verified one layer up in `Store::mount`, so
//! the decoder itself sees unauthenticated bytes.
//!
//! Oracle — note the deliberate weakening. Unlike the TLV entry codec,
//! `decode_index` rebuilds two `BTreeMap`s, which *normalize* key order and
//! collapse duplicates. So it is canonical only up to key ordering, not
//! byte-canonical: an input with entries in a different order, or with a
//! repeated key, decodes successfully but re-encodes to different bytes.
//! Asserting byte equality here would therefore flag ordering the format
//! never durably produces (the encoder always emits sorted maps, and the
//! frame is content-addressed over those exact bytes). So we assert
//! round-trip *stability* instead — decode∘encode∘decode == decode — the
//! same asymmetry the postcard wire bodies carry, and for the same reason:
//! nothing downstream hashes a *logical* index, only the encoder's bytes.
use libfuzzer_sys::fuzz_target;

use cas::disk::{decode_index, encode_index};

fuzz_target!(|data: &[u8]| {
    let Ok((entries, free)) = decode_index(data) else { return };
    // Re-encode with no padding (pad is extent-fill slack, not part of the
    // logical value) and confirm the encoder emits decodable, stable bytes.
    let bytes = encode_index(&entries, &free, 0);
    let (e2, f2) = decode_index(&bytes).expect("encoder emitted an undecodable index");
    assert_eq!((entries, free), (e2, f2), "index decode is not round-trip stable");
});
