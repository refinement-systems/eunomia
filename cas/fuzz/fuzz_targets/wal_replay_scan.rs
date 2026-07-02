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
//! The WAL replay scanner over an arbitrary region — the same loop
//! `Store::mount` runs (rev2§4.5). The interesting unit isn't one record but
//! the scan: it must never panic, never read past the region, accept
//! exactly a checksum-valid prefix, and terminate. Each accepted record is
//! also checked against the canonical oracle: a record's bytes are fully
//! determined, so the consumed prefix must re-encode to itself.
use libfuzzer_sys::fuzz_target;

use cas::disk::WalOp;

fuzz_target!(|data: &[u8]| {
    let mut off = 0usize;
    while off < data.len() {
        let Some((seq, op, rlen)) = WalOp::decode_record(&data[off..]) else {
            break;
        };
        let re = op.encode_record(seq);
        assert_eq!(
            re.as_slice(),
            &data[off..off + rlen],
            "WAL record decode is not canonical",
        );
        off += rlen; // rlen >= WAL_HEADER > 0, so the scan always advances
    }
});
