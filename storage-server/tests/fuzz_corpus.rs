//! Replay the committed request_dispatch corpus through the real
//! decode-and-dispatch seam, so seed and fuzz-found inputs stay alive as an
//! ordinary test and `cargo miri test` UB-checks the postcard decoder on
//! each. Dispatch itself (which hashes/commits) is skipped under Miri.

use std::fs;
use std::path::PathBuf;

use cas::chunk::ChunkerParams;
use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use storage_server::{wire, HandleEntry, HandleTarget, Request, Server, R_ENUMERATE, R_READ};

fn corpus_files(target: &str) -> Vec<Vec<u8>> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus");
    dir.push(target);
    match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| fs::read(e.path()).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn fresh() -> (Server<MemDev>, u64) {
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
    // Same handle layout as the request_dispatch harness (handles 0,1,2).
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

#[test]
fn request_dispatch() {
    for data in corpus_files("request_dispatch") {
        let Ok(req) = wire::decode_request(&data) else {
            continue;
        };
        if cfg!(miri) {
            continue; // decoder exercised above; skip the hashing dispatch
        }
        let (mut server, session) = fresh();
        let _ = server.handle(session, req, 1_000);
    }
}

/// Generator for the C2D `Request::Rename` corpus seed
/// (`corpus/request_dispatch/rename`): a single encoded `Rename` request so the
/// `request_dispatch` fuzzer — and the Miri replay above — exercise the new
/// rename decode+dispatch path. `#[ignore]`d because it rewrites a committed
/// file; run explicitly to regenerate:
/// `cargo test -p storage-server --test fuzz_corpus gen_rename_corpus_seed -- --ignored`.
#[test]
#[ignore = "regenerates a committed corpus seed"]
fn gen_rename_corpus_seed() {
    let bytes = wire::encode_request(&Request::Rename {
        handle: 0,
        from: vec![b"a".to_vec()],
        to: vec![b"b".to_vec()],
    })
    .unwrap();
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus/request_dispatch");
    fs::write(dir.join("rename"), &bytes).unwrap();
}
