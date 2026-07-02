// SPDX-License-Identifier: 0BSD
#![no_main]
//! The request dispatch seam. Bytes go through the *same* entry the real
//! server loop uses — `wire::decode_request` then `Server::handle` — rather
//! than per-opcode functions, so a new protocol opcode is fuzzed the day
//! it's added with zero new harness code. The decoded request is dispatched
//! against a small, freshly built store holding one ref (with content and a
//! snapshot) and a full-rights root session at handle 0, so requests reach
//! real store mutations. The property: dispatch never panics; every request
//! maps to a `Response`.
//!
//! Ticket redemption (the one bearer-token path, `RedeemTicket`) is an
//! opcode here, so this target also covers it — the server keeps no
//! separate ticket *parser* to fuzz; a redeemed `[u8; 16]` is a map lookup.
use libfuzzer_sys::fuzz_target;

use cas::chunk::ChunkerParams;
use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use storage_server::{wire, HandleEntry, HandleTarget, Server, SessionId, R_ENUMERATE, R_READ};

const NOW: u64 = 1_000;

// Install a few diverse handles at low ids (0,1,2) so mutation-reachable
// small handle values land on *valid, varied* targets instead of bouncing
// off BadHandle — a full-rights ref root, a read-only subtree view, and a
// read-only snapshot. Larger/garbage handles still exercise the rejection
// path; a single handle would starve most dispatch paths.
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
    store
        .write(
            b"main",
            &vec![b"etc".to_vec(), b"conf".to_vec()],
            0,
            b"hello",
            1,
        )
        .unwrap();
    store.sync_all().unwrap();
    store
        .snapshot(b"main", b"seed", b"v1", cas::disk::CLASS_AUTO, 10)
        .unwrap();
    let gen = store
        .refs()
        .find(|(n, _)| n.as_slice() == b"main")
        .unwrap()
        .1
        .generation;
    let snap_root = store.snapshot_root(b"main", 1).unwrap();

    let mut server = Server::new(store, 0xA5A5_A5A5);
    let grants = vec![
        server.root_grant(b"main").unwrap(),
        HandleEntry {
            target: HandleTarget::Ref {
                name: b"main".to_vec(),
                subtree: vec![b"etc".to_vec()],
                gen_at_grant: gen,
            },
            rights: R_READ | R_ENUMERATE,
        },
        HandleEntry {
            target: HandleTarget::Snapshot { root: snap_root },
            rights: R_READ | R_ENUMERATE,
        },
    ];
    let session = server.open_session(grants);
    (server, session)
}

fuzz_target!(|data: &[u8]| {
    // Decode at the storage wire version (rev2§3.7): the fuzzer also
    // explores the stamped version byte, and a frame at any other version is
    // refused cleanly (`WireError::Version`) before dispatch — never a panic.
    let Ok(req) = wire::decode_request(data, wire::PROTO_VERSION) else {
        return;
    };
    let (mut server, session) = fresh();
    let _ = server.handle(session, req, NOW);
});
