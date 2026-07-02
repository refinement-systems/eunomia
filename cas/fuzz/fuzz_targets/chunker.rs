// SPDX-License-Identifier: 0BSD
#![no_main]
//! FastCDC chunker invariants on arbitrary input (rev2§4.1). Boundaries decide
//! chunk hashes, so they are a format property: deterministic, chunks
//! concatenate back to the input, and every chunk is within bounds (only
//! the final one may fall below `min`). proptest covers this too; coverage
//! guidance is cheap insurance over the gear-hash state machine.
use libfuzzer_sys::fuzz_target;

use cas::chunk::{boundaries, Chunker, ChunkerParams};

// Small params so cases stay fast; identical code paths to production.
const PARAMS: ChunkerParams = ChunkerParams {
    min: 64,
    avg: 256,
    max: 1024,
};

fuzz_target!(|data: &[u8]| {
    let cuts = boundaries(&PARAMS, data);
    assert_eq!(
        cuts,
        boundaries(&PARAMS, data),
        "chunking is non-deterministic"
    );

    // Cut positions are strictly increasing and end exactly at the input.
    let mut prev = 0usize;
    for (i, &cut) in cuts.iter().enumerate() {
        let len = cut - prev;
        assert!(cut > prev, "non-advancing cut");
        assert!(len <= PARAMS.max, "chunk exceeds max");
        if i + 1 != cuts.len() {
            assert!(len > PARAMS.min, "interior chunk at/below min");
        }
        prev = cut;
    }
    assert_eq!(prev, data.len(), "chunks do not cover the input");

    // Streaming path agrees with the one-shot helper and concatenates back.
    let mut streamed = Vec::new();
    let mut concat = Vec::new();
    let mut pos = 0usize;
    let mut chunker = Chunker::with_params(PARAMS);
    let mut record = |c: &[u8]| {
        pos += c.len();
        streamed.push(pos);
        concat.extend_from_slice(c);
    };
    chunker.push(data, &mut record);
    chunker.flush(record);
    assert_eq!(streamed, cuts, "streaming boundaries disagree with helper");
    assert_eq!(concat, data, "streamed chunks do not concatenate to input");
});
