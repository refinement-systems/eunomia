// SPDX-License-Identifier: 0BSD
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
