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
//! The IPC wire body decoder on arbitrary bytes (spec rev2§3.7).
//! Oracle: `decode` is total (never panics — that is the whole point of the
//! target), and any value that decodes survives a re-encode/re-decode
//! unchanged. We compare the value, not the bytes: postcard varints are not
//! guaranteed minimal, so a re-encode may differ byte-for-byte from a
//! non-canonical input while still denoting the same message (round-trip
//! stability, not byte-canonical form). Trailing-byte rejection and the framing
//! checks are covered by the `wire` unit tests; here the decoder just must not
//! crash on anything.
use libfuzzer_sys::fuzz_target;

use ipc::fuzz_support::{decode_demo, encode_demo};

fuzz_target!(|data: &[u8]| {
    if let Ok((_, m)) = decode_demo(data) {
        let bytes = encode_demo(&m);
        let (_, m2) = decode_demo(&bytes).expect("a re-encoded message must decode");
        assert_eq!(m, m2, "decode/encode is not a round-trip");
    }
});
