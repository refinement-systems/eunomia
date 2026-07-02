// SPDX-License-Identifier: 0BSD
//! Regression test for a finding surfaced by storage-server/fuzz.
//!
//! The root cause lives in cas (`cas/tests/fuzz_regressions.rs`); this is
//! the end-to-end view that makes the security claim concrete.

use cas::chunk::ChunkerParams;
use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use storage_server::{wire, ErrorCode, Request, Response, Server, SessionId};

fn fresh() -> (Server<MemDev>, SessionId) {
    let opts = StoreOptions {
        wal_len: 4096,
        chunker: ChunkerParams {
            min: 64,
            avg: 256,
            max: 1024,
        },
        global_budget: 16 * 1024,
        ..StoreOptions::default()
    };
    let mut store = Store::format(MemDev::new(64 * 1024), opts).unwrap();
    store.create_ref(b"main").unwrap();
    let mut server = Server::new(store, 0xA5A5_A5A5);
    let grant = server.root_grant(b"main").unwrap();
    let session = server.open_session(vec![grant]);
    (server, session)
}

/// Write-offset overflow, end to end: a client holding a write handle could
/// crash the server with one `Write` whose `offset` is near u64::MAX — the
/// request decoded, dispatch passed the rights check, and the store's
/// overlay overflowed `off + data.len()`. Dispatch turns it into an error
/// `Response` and the server keeps serving.
#[test]
fn ovl1_dispatch_write_offset_overflow_rejected() {
    let (mut server, session) = fresh();
    let req = Request::Write {
        handle: 0,
        path: vec![b"f".to_vec()],
        offset: u64::MAX,
        data: vec![1],
    };
    let bytes = wire::encode_request(&req, wire::PROTO_VERSION).unwrap();
    let decoded = wire::decode_request(&bytes, wire::PROTO_VERSION).unwrap();
    let resp = server.handle(session, decoded, 1_000);
    assert_eq!(resp, Response::Err(ErrorCode::BadOffset));
    // The server must survive: a sane write on the same session succeeds.
    let ok = Request::Write {
        handle: 0,
        path: vec![b"f".to_vec()],
        offset: 0,
        data: b"hello".to_vec(),
    };
    assert_eq!(server.handle(session, ok, 1_001), Response::Ok);
}
