#![no_main]
//! The durable reference table (rev1§4.1, rev1§4.7): per-ref roots +
//! revocation generation + edit version, the snapshot rows, and the tags.
//! `RefTable::decode` is plain Rust with `FormatError` decode discipline and,
//! until B5A, was reached only indirectly through `mount_recovery`; this fuzzes
//! the decoder directly. Its content hash is verified one layer up in
//! `Store::mount`, so the decoder itself sees unauthenticated bytes.
//!
//! Oracle — the same deliberate weakening as `index_frame`. `RefTable::decode`
//! rebuilds three `BTreeMap`s, which *normalize* key order and collapse
//! duplicates, so the codec is canonical only up to key ordering, not
//! byte-canonical: an input with rows in a different order, or with a repeated
//! key, decodes successfully but re-encodes to different bytes. Asserting byte
//! equality would flag ordering the format never durably produces (the encoder
//! always emits sorted maps, and the frame is content-addressed over those
//! exact bytes). So we assert round-trip *stability* — decode∘encode∘decode ==
//! decode — the property the guarded-batch edit version rides on: the same
//! logical table always serializes to the same bytes.
use libfuzzer_sys::fuzz_target;

use cas::disk::RefTable;

fuzz_target!(|data: &[u8]| {
    let Ok(table) = RefTable::decode(data) else {
        return;
    };
    let bytes = table.encode();
    let table2 = RefTable::decode(&bytes).expect("encoder emitted an undecodable ref table");
    assert_eq!(table, table2, "ref table decode is not round-trip stable");
});
