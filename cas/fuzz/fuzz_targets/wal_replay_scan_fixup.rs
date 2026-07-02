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
//! WAL replay scan *behind* the per-record checksum gate. Raw bytes almost
//! never form a valid record, so the unguarded scanner stops at offset 0
//! and the payload decoder (op tags, path/ref length fields, the
//! `decode_payload` parser) is never exercised. `fixup_wal_chain` re-seals
//! the region into a valid record chain, so the fuzzer drives mutations
//! into record *bodies* — where the length-prefixed path and data fields
//! live. Same canonical and termination checks as the raw scan.
use libfuzzer_sys::fuzz_target;

use cas::disk::WalOp;
use cas::fuzz_support::fixup_wal_chain;

fuzz_target!(|data: &[u8]| {
    let mut region = data.to_vec();
    fixup_wal_chain(&mut region);
    let mut off = 0usize;
    while off < region.len() {
        let Some((seq, op, rlen)) = WalOp::decode_record(&region[off..]) else {
            break;
        };
        let re = op.encode_record(seq);
        assert_eq!(
            re.as_slice(),
            &region[off..off + rlen],
            "WAL record decode is not canonical",
        );
        off += rlen;
    }
});
