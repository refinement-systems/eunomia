//! Regression reproducer for a finding surfaced by storage-server/fuzz.
//!
//! Documents a *currently unfixed* finding (fixing is out of scope for the
//! fuzzing work). Written `#[should_panic]` so it passes today by asserting
//! the bug still bites, and fails the moment it is fixed. The root cause
//! lives in cas (`cas/tests/fuzz_regressions.rs::ovl1_*`); this is the
//! end-to-end view that makes the security claim concrete. See
//! doc/results/1_fuzzing-findings.md.

use cas::chunk::ChunkerParams;
use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use storage_server::{wire, Request, Server, SessionId};

fn fresh() -> (Server<MemDev>, SessionId) {
    let opts = StoreOptions {
        wal_len: 4096,
        chunker: ChunkerParams { min: 64, avg: 256, max: 1024 },
        overlay_budget: 16 * 1024,
    };
    let mut store = Store::format(MemDev::new(64 * 1024), opts).unwrap();
    store.create_ref(b"main").unwrap();
    let mut server = Server::new(store, 0xA5A5_A5A5);
    let grant = server.root_grant(b"main").unwrap();
    let session = server.open_session(vec![grant]);
    (server, session)
}

/// FINDING OVL-1 (unfixed), end to end: a client holding a write handle
/// crashes the server with one `Write` whose `offset` is near u64::MAX —
/// the request decodes, dispatch passes the rights check, and the store's
/// overlay overflows `off + data.len()`. When fixed, dispatch should turn
/// this into an error `Response`; flip this test to assert that.
#[test]
#[should_panic(expected = "overflow")]
fn ovl1_dispatch_write_offset_overflow_panics() {
    let (mut server, session) = fresh();
    let req = Request::Write { handle: 0, path: vec![b"f".to_vec()], offset: u64::MAX, data: vec![1] };
    let bytes = wire::encode_request(&req).unwrap();
    let decoded = wire::decode_request(&bytes).unwrap();
    let _ = server.handle(session, decoded, 1_000);
}
