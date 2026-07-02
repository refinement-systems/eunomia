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
use ipc::{ConnectReq, GrantReply, VersionRange, WindowGrant};

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
        (
            "open",
            DemoMsg::Open {
                name: "etc/conf".into(),
                flags: 0x1,
            },
        ),
        (
            "read",
            DemoMsg::Read {
                handle: 3,
                offset: 7,
                len: 100,
            },
        ),
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
    write_seed(
        "wire_decode",
        "header_only",
        &[0xDE, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    );
    // A valid message with one trailing byte (the rev2§3.7 rejection path).
    let mut trailing = encode_demo(&DemoMsg::Ping);
    trailing.push(0);
    write_seed("wire_decode", "trailing", &trailing);

    println!(
        "wrote {} seeds to ipc/fuzz/corpus/wire_decode/",
        msgs.len() + 3
    );

    // The session connect codecs (rev2§3.5/§3.7), driven by the `connect_decode`
    // target. Fixed-width, byte-canonical forms the random search rarely hits by
    // chance (the tag bytes + exact REQ_LEN/GRANT_LEN framing).
    let req_single = ConnectReq::for_window(4096).encode();
    write_seed("connect_decode", "req_single", &req_single);
    // A multi-version offer [1,4] — the negotiation case (the version bytes carry).
    write_seed(
        "connect_decode",
        "req_range",
        &ConnectReq::new(8192, VersionRange::new(1, 4)).encode(),
    );
    // A grant reply carrying the negotiated version 3 (its used GRANT_LEN prefix).
    let (gbuf, gn) = GrantReply::Grant(
        WindowGrant {
            window: 0,
            size: 8192,
        },
        3,
    )
    .encode();
    write_seed("connect_decode", "grant", &gbuf[..gn]);
    // A refusal (the 1-byte REFUSED_LEN prefix).
    let (rbuf, rn) = GrantReply::Refused.encode();
    write_seed("connect_decode", "refused", &rbuf[..rn]);

    // Edge inputs both connect decoders must reject without panicking.
    write_seed("connect_decode", "empty", &[]);
    // Right length (REQ_LEN), wrong tag → decodes to None.
    let mut wrong_tag = req_single;
    wrong_tag[0] = 0xFF;
    write_seed("connect_decode", "req_wrong_tag", &wrong_tag);
    // A valid request with one trailing byte (length mismatch → None).
    let mut req_trailing = req_single.to_vec();
    req_trailing.push(0);
    write_seed("connect_decode", "req_trailing", &req_trailing);
    // Boundary requests: the smallest window, and a single-version offer [2,2]
    // (lo == hi) — the degenerate negotiation range.
    write_seed(
        "connect_decode",
        "req_min_window",
        &ConnectReq::for_window(1).encode(),
    );
    write_seed(
        "connect_decode",
        "req_single_version",
        &ConnectReq::new(1, VersionRange::new(2, 2)).encode(),
    );
    // A request truncated by one byte (short of REQ_LEN → None): the low-side
    // length-rejection edge, complementing the trailing-byte high-side edge.
    write_seed(
        "connect_decode",
        "req_short",
        &req_single[..req_single.len() - 1],
    );
    // Grant/refusal with one trailing byte: past GRANT_LEN / REFUSED_LEN → None
    // (canonical framing rejects any length mismatch, per the bijection proof).
    let mut grant_trailing = gbuf[..gn].to_vec();
    grant_trailing.push(0);
    write_seed("connect_decode", "grant_trailing", &grant_trailing);
    let mut refused_trailing = rbuf[..rn].to_vec();
    refused_trailing.push(0);
    write_seed("connect_decode", "refused_trailing", &refused_trailing);

    println!("wrote 12 seeds to ipc/fuzz/corpus/connect_decode/");
}
