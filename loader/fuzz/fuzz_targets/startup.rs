// SPDX-License-Identifier: 0BSD
#![no_main]
//! Startup-block decode on arbitrary bytes (rev2§5.1). The block is the first
//! message on a child's bootstrap channel, decoded in `_start` before anything
//! else exists, so a malformed block must be refused, never a crash (rev2§2.7).
//! Property set: `decode` never panics; the counts it reports are within the
//! fixed arenas; every borrowed argv/env slice lies inside the input (nothing it
//! hands back can index out of `data`); and any block it accepts re-encodes and
//! re-decodes back equal (the codec round-trips on the decoded shape). The
//! decoder uses fixed-size arenas, so there is no length-field-driven allocation
//! to bound — this re-checks the invariants `decode` claims, defending against a
//! future refactor that forgets one.
//!
//! Run: `cargo +nightly fuzz run startup`.
use libfuzzer_sys::fuzz_target;

use loader::startup;

fuzz_target!(|data: &[u8]| {
    let Some(s) = startup::decode(data) else {
        return;
    };

    // Counts stay within the arenas the decoder fills.
    assert!(s.ngrants <= startup::MAX_GRANTS, "grant count over the cap");
    assert!(s.nargv <= startup::MAX_ARGV, "argv count over the cap");
    assert!(s.nenv <= startup::MAX_ENV, "env count over the cap");

    // Every borrowed argv/env slice points inside the input slice — nothing the
    // decoder returns can index out of `data`.
    let range = data.as_ptr_range();
    for v in s.argv[..s.nargv].iter().chain(&s.env[..s.nenv]) {
        if !v.is_empty() {
            let r = v.as_ptr_range();
            assert!(
                r.start >= range.start && r.end <= range.end,
                "decoded byte-string escapes the input buffer",
            );
        }
    }

    // Codec agreement: when a decoded block fits the message budget it
    // re-encodes and re-decodes back equal. (The fuzzer may feed > 256 bytes,
    // so a decoded block can legitimately overflow the budget on re-encode —
    // that is a clean `Err`, not a bug; only the fitting case round-trips.) A
    // future encode/decode divergence fails here.
    let mut buf = [0u8; startup::MAX_BLOCK];
    if let Ok(n) = startup::encode(&s, &mut buf) {
        let again = startup::decode(&buf[..n]).expect("re-decode of an encoded block failed");
        assert!(again == s, "encode→decode round-trip changed the block");
    }
});
