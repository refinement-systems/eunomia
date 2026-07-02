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

//! Seed-corpus generator for the storage-server/fuzz targets. Emits one
//! valid wire-encoded request per opcode so the dispatch fuzzer starts from
//! structurally-valid messages and mutates their *fields* (offsets,
//! lengths, handles) rather than having to assemble a valid frame from
//! noise. Run: `cargo run -p storage-server --example gen_storage_corpus`.

use std::fs;
use std::path::PathBuf;

use storage_server::{wire, Request};

fn corpus_dir(target: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fuzz");
    p.push("corpus");
    p.push(target);
    fs::create_dir_all(&p).unwrap();
    p
}

fn write_seed(target: &str, name: &str, bytes: &[u8]) {
    let mut p = corpus_dir(target);
    p.push(name);
    fs::write(&p, bytes).unwrap();
    println!("  {target}/{name}: {} bytes", bytes.len());
}

fn path(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|p| p.to_vec()).collect()
}

fn main() {
    println!("seeding storage-server fuzz corpora:");

    // Handle 0 is the full-rights root grant the dispatch harness installs.
    let reqs: &[(&str, Request)] = &[
        (
            "read",
            Request::Read {
                handle: 0,
                path: path(&[b"etc", b"conf"]),
                offset: 0,
                len: 64,
            },
        ),
        (
            "write",
            Request::Write {
                handle: 0,
                path: path(&[b"etc", b"conf"]),
                offset: 0,
                data: b"hi".to_vec(),
            },
        ),
        (
            "unlink",
            Request::Unlink {
                handle: 0,
                path: path(&[b"etc", b"conf"]),
            },
        ),
        (
            "list",
            Request::List {
                handle: 0,
                path: path(&[b"etc"]),
            },
        ),
        (
            "open_child",
            Request::OpenChild {
                handle: 0,
                path: path(&[b"etc"]),
                rights_mask: 0xFF,
            },
        ),
        ("close", Request::Close { handle: 0 }),
        ("sync", Request::Sync { handle: 0 }),
        (
            "snapshot",
            Request::Snapshot {
                handle: 0,
                message: b"m".to_vec(),
                class: 1,
            },
        ),
        ("list_snapshots", Request::ListSnapshots { handle: 0 }),
        (
            "open_snapshot",
            Request::OpenSnapshot {
                handle: 0,
                snap_id: 1,
                path: path(&[]),
                rights_mask: 0xFF,
            },
        ),
        (
            "rollback",
            Request::Rollback {
                handle: 0,
                snap_id: 1,
            },
        ),
        ("revoke_ref", Request::RevokeRef { handle: 0 }),
        (
            "mint_ticket",
            Request::MintTicket {
                handle: 0,
                ttl_nanos: 1_000_000,
            },
        ),
        (
            "redeem_ticket",
            Request::RedeemTicket { ticket: [0xAB; 16] },
        ),
        (
            "stat",
            Request::Stat {
                handle: 0,
                path: path(&[b"etc", b"conf"]),
            },
        ),
        ("enumerate", Request::EnumerateSession),
        (
            "delete_snapshot",
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: 1,
            },
        ),
        (
            "set_class",
            Request::SetClass {
                handle: 0,
                snap_id: 1,
                class: 0,
            },
        ),
        ("gc", Request::Gc { handle: 0 }),
        ("statfs", Request::Statfs { handle: 0 }),
        (
            "tag",
            Request::Tag {
                handle: 0,
                name: b"release".to_vec(),
                snap_id: 1,
            },
        ),
        (
            "untag",
            Request::Untag {
                handle: 0,
                name: b"release".to_vec(),
            },
        ),
        ("list_tags", Request::ListTags { handle: 0 }),
        // Handles 1 (read-only "etc" subtree) and 2 (read-only snapshot)
        // installed by the dispatch harness — seed their low ids too.
        (
            "read_subtree_h1",
            Request::Read {
                handle: 1,
                path: path(&[b"conf"]),
                offset: 0,
                len: 64,
            },
        ),
        (
            "list_snapshot_h2",
            Request::List {
                handle: 2,
                path: path(&[]),
            },
        ),
    ];
    for (name, req) in reqs {
        let bytes = wire::encode_request(req, wire::PROTO_VERSION)
            .expect("seed request must fit the wire envelope");
        write_seed("request_dispatch", name, &bytes);
    }

    // The structured target consumes raw bytes through Arbitrary, not the
    // wire decoder, so seeds are just byte material of a few shapes.
    write_seed("structured_request", "zeros", &[0u8; 24]);
    write_seed("structured_request", "ones", &[0xFFu8; 24]);
    write_seed(
        "structured_request",
        "mixed",
        b"\x01\x00\x05etc\x04conf\x00\x02hi",
    );

    println!("done.");
}
