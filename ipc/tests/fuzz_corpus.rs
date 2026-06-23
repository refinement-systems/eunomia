//! Replay the committed `wire_decode` corpus through the same decoder the
//! cargo-fuzz target drives (mirrors `cas/tests/fuzz_corpus.rs`). This keeps
//! every fuzz-discovered (and seed) input alive as an ordinary test even where
//! libFuzzer doesn't run, and makes `cargo miri test -p ipc --features fuzzing`
//! UB-check each one.
//!
//! `fuzzing`-gated: under a plain `cargo test --workspace` (feature off) this
//! file compiles to nothing; the fuzz.yml job runs it with `--features fuzzing`.

#![cfg(feature = "fuzzing")]

use std::fs;
use std::path::PathBuf;

use ipc::fuzz_support::{decode_demo, encode_demo};
use ipc::{ConnectReq, GrantReply};

fn corpus_files(target: &str) -> Vec<Vec<u8>> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus");
    dir.push(target);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new(); // corpus not present (fresh checkout): skip
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| fs::read(e.path()).unwrap())
        .collect()
}

#[test]
fn wire_decode() {
    for data in corpus_files("wire_decode") {
        // Decode is total (no panic). Any value that decodes survives a
        // re-encode/re-decode unchanged — round-trip stability (postcard is not
        // byte-canonical, so we compare the value, not the bytes).
        if let Ok((_, m)) = decode_demo(&data) {
            let bytes = encode_demo(&m);
            let (_, m2) = decode_demo(&bytes).expect("re-encoded message must decode");
            assert_eq!(m, m2, "non-stable message in corpus");
        }
    }
}

#[test]
fn connect_decode() {
    for data in corpus_files("connect_decode") {
        // The session connect codecs are fixed-width and byte-canonical (unlike
        // the postcard body above), so an accepted input re-encodes to the exact
        // same bytes — the runtime witness for the Verus bijection lemmas in
        // session.rs. Both decoders are total: a malformed buffer is `None`, not
        // a panic.
        if let Some(req) = ConnectReq::decode(&data) {
            assert_eq!(&req.encode()[..], &data[..], "ConnectReq not byte-stable");
        }
        if let Some(reply) = GrantReply::decode(&data) {
            let (buf, n) = reply.encode();
            assert_eq!(&buf[..n], &data[..], "GrantReply not byte-stable");
        }
    }
}
