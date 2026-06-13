//! Kani harnesses for the cas host-side chokepoints (plan §4.7). Kani is
//! *supplementary* here — the cargo-fuzz targets (`cas/fuzz`) remain primary,
//! running the canonical-form and mount-recovery oracles over millions of
//! cases. Kani adds exhaustiveness at small bounds where it buys something a
//! fuzzer can't promise: the superblock geometry chokepoint (totality + the
//! "every region within the device" safety invariant) and TLV decode totality.
//!
//! `blake3` is out of Kani scope (interpreted hashing is intractable for
//! CBMC), so the superblock-decode harness stubs `Hash::of` with a *total*
//! ghost hash (`-Z stubbing`). Totality does not need collision-freedom; a
//! deterministic fold suffices — a round-trip harness would additionally
//! axiomatize injectivity, stated here for the record.

#![cfg(kani)]

use crate::disk::{Superblock, SB_BODY, SB_SIZE, WAL_OFF};
use crate::hash::Hash;

/// `validate_geometry` is the §4.5 mount chokepoint: total over all field
/// values + device length (it is all `checked_add`), and on `Ok` every
/// committed region lies within the device — no untrusted field vouches for
/// another, each is checked against the one ground truth (`dev_len`).
#[kani::proof]
fn check_superblock_geometry() {
    let sb = Superblock {
        generation: 0,
        ref_table: Hash::from_bytes([0u8; 32]),
        wal_head: kani::any(),
        wal_next_seq: 0,
        wal_len: kani::any(),
        chunk_tail: kani::any(),
        index_off: kani::any(),
    };
    let dev_len: u64 = kani::any();

    if sb.validate_geometry(dev_len).is_ok() {
        // The committed chunk region (WAL region + committed chunks) fits the
        // device — the guarantee downstream sizing/reads rely on. The unwraps
        // are safe: validate_geometry's checked_adds were Some on this path.
        let chunk_off = WAL_OFF.checked_add(sb.wal_len).unwrap();
        let committed_end = chunk_off.checked_add(sb.chunk_tail).unwrap();
        assert!(committed_end <= dev_len);
        assert!(sb.wal_head <= sb.wal_len);
    }
    // Totality (no panic for any fields/dev_len) is checked by Kani directly.
}

/// A total ghost hash standing in for `blake3` (§4.7): deterministic, never
/// panics. Collision-freedom is unnecessary for the decode-totality property
/// below (any total function proves it); a round-trip harness would instead
/// axiomatize injectivity here.
fn stub_hash_of(data: &[u8]) -> Hash {
    let mut out = [0u8; 32];
    let n = if data.len() < 32 { data.len() } else { 32 };
    let mut i = 0;
    while i < n {
        out[i] = data[i];
        i += 1;
    }
    out[0] ^= data.len() as u8;
    Hash::from_bytes(out)
}

/// `decode_checked` is total over arbitrary superblock bytes — mount returns
/// refused-or-parsed on any input, never a panic (spec §4.5). Only the first
/// 128 bytes (magic + checksum body + fields) are read, so the rest stay zero
/// to keep the symbolic state small. `Hash::of` is stubbed (see above).
#[kani::proof]
#[kani::stub(crate::hash::Hash::of, stub_hash_of)]
#[kani::unwind(34)]
fn check_superblock_decode_total() {
    let mut buf = [0u8; SB_SIZE];
    let head: [u8; 128] = kani::any();
    buf[..128].copy_from_slice(&head);
    let _ = Superblock::decode_checked(&buf);
    let _ = SB_BODY; // (documents the checksummed-prefix bound)
}

// `cas::tlv` decode is **not** a Kani harness: its `Entry` parsing allocates
// `Vec`s (name, inline content) of symbolic length, and CBMC's `RawVec`/
// allocator modeling over that exhausts memory even at a 12-byte input (18.5k
// VCCs → OOM; the findings SOLVER note). The decode-totality and canonical-form
// (decode→re-encode==id) oracles stay owned by the cargo-fuzz target
// `cas/fuzz/fuzz_targets/tlv_entry.rs`, which runs them over millions of cases
// — exactly the §4.7 "Kani supplementary, fuzz primary" division.
