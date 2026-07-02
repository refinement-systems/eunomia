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
