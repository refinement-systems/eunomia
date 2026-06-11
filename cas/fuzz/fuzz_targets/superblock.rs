#![no_main]
//! Raw superblock decode: arbitrary bytes through `Superblock::decode`,
//! no fix-up. The rejection path (bad magic / version / checksum) is
//! itself code under test, and almost every random input lands there — so
//! this target proves the decoder is total, while `superblock_fixup`
//! covers the field-extraction logic behind the checksum gate.
use libfuzzer_sys::fuzz_target;

use cas::disk::Superblock;

fuzz_target!(|data: &[u8]| {
    let _ = Superblock::decode(data);
});
