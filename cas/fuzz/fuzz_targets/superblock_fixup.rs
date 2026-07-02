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

#![no_main]
//! Superblock decode *behind* the integrity gate. A mutation fuzzer can
//! never forge the body checksum, so without help it explores the
//! "checksum mismatch → None" branch forever and the field-extraction code
//! is never reached. `fixup_superblock_checksum` re-seals magic + version +
//! checksum over the mutated body, so the fuzzer's edits land on the
//! decoded fields. On a successful decode we also confirm the result is
//! stable: re-encoding it and decoding again yields the same superblock.
use libfuzzer_sys::fuzz_target;

use cas::disk::{Superblock, SB_SIZE};
use cas::fuzz_support::fixup_superblock_checksum;

fuzz_target!(|data: &[u8]| {
    let mut block = [0u8; SB_SIZE];
    let n = data.len().min(SB_SIZE);
    block[..n].copy_from_slice(&data[..n]);
    fixup_superblock_checksum(&mut block);
    if let Some(sb) = Superblock::decode(&block) {
        assert_eq!(
            Superblock::decode(&sb.encode()),
            Some(sb),
            "superblock decode is not round-trip stable",
        );
    }
});
