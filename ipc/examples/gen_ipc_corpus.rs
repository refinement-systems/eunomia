//! Seed-corpus generator for the ipc/fuzz targets (mirrors gen_cas_corpus).
//! Emits valid wire messages built with the real encoder, plus a few edge
//! inputs, into `ipc/fuzz/corpus/<target>/`, so every fuzz run (and the
//! committed-corpus replay test) starts warm on real shapes the random search
//! struggles to reach (postcard varints, exact framing).
//!
//! Run: `cargo run -p ipc --example gen_ipc_corpus --features fuzzing`

use std::fs;
use std::path::PathBuf;

use ipc::fuzz_support::{encode_demo, DemoMsg};

fn corpus_dir(target: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fuzz");
    p.push("corpus");
    p.push(target);
    p
}

fn write_seed(target: &str, name: &str, bytes: &[u8]) {
    let dir = corpus_dir(target);
    fs::create_dir_all(&dir).unwrap();
    let mut p = dir;
    p.push(name);
    fs::write(&p, bytes).unwrap();
}

fn main() {
    // Valid messages — one per DemoMsg variant.
    let msgs = [
        ("ping", DemoMsg::Ping),
        ("open", DemoMsg::Open { name: "etc/conf".into(), flags: 0x1 }),
        ("read", DemoMsg::Read { handle: 3, offset: 7, len: 100 }),
        ("data", DemoMsg::Data(vec![1, 2, 3, 4])),
        ("error", DemoMsg::Error(5)),
    ];
    for (name, m) in &msgs {
        write_seed("wire_decode", name, &encode_demo(m));
    }

    // Edge inputs the decoder must handle without panicking.
    write_seed("wire_decode", "empty", &[]);
    // A well-formed header declaring a zero-length body (no variant byte → the
    // postcard decode fails cleanly, not a panic).
    write_seed("wire_decode", "header_only", &[0xDE, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    // A valid message with one trailing byte (the rev1§3.7 rejection path).
    let mut trailing = encode_demo(&DemoMsg::Ping);
    trailing.push(0);
    write_seed("wire_decode", "trailing", &trailing);

    println!("wrote {} seeds to ipc/fuzz/corpus/wire_decode/", msgs.len() + 3);
}
