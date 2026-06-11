//! Regression tests for findings surfaced by the storage-server and cas
//! fuzz targets, rooted in cas. Each pins the hardened behavior so the
//! finding cannot silently regress. See doc/results/1_fuzzing-findings.md.

use cas::chunk::ChunkerParams;
use cas::dev::MemDev;
use cas::store::{Store, StoreError, StoreOptions};

fn fresh() -> Store<MemDev> {
    let opts = StoreOptions {
        wal_len: 4096,
        chunker: ChunkerParams { min: 64, avg: 256, max: 1024 },
        overlay_budget: 16 * 1024,
    };
    let mut store = Store::format(MemDev::new(64 * 1024), opts).unwrap();
    store.create_ref(b"main").unwrap();
    store
}

/// FINDING OVL-1 (fixed): a write at `offset` near u64::MAX used to panic
/// with an arithmetic overflow in `FileOverlay::insert` (`off + data.len()`,
/// overlay.rs), reachable from a `Write` request — a client with write
/// access could crash the storage server with one message (found by the
/// `request_dispatch` target). `Store::write` now rejects the extent before
/// it reaches the WAL.
#[test]
fn ovl1_write_offset_overflow_rejected() {
    let mut store = fresh();
    let path = vec![b"f".to_vec()];
    let r = store.write(b"main", &path, u64::MAX, b"x", 1);
    assert!(matches!(r, Err(StoreError::WriteOutOfRange)), "got {r:?}");
    // The store must survive the rejection intact.
    store.write(b"main", &path, 0, b"hello", 2).unwrap();
    assert_eq!(store.read(b"main", &path).unwrap(), Some(b"hello".to_vec()));
}

/// Companion to OVL-1: an extent that cannot overflow but exceeds the chunk
/// region capacity is rejected too — accepting it would ack a WAL record
/// that can never flush (and would materialize the whole extent in
/// `FileOverlay::apply`).
#[test]
fn ovl1_write_extent_beyond_capacity_rejected() {
    let mut store = fresh();
    let path = vec![b"f".to_vec()];
    let r = store.write(b"main", &path, 1 << 40, b"x", 1);
    assert!(matches!(r, Err(StoreError::WriteOutOfRange)), "got {r:?}");
    store.write(b"main", &path, 0, b"hello", 2).unwrap();
    assert_eq!(store.read(b"main", &path).unwrap(), Some(b"hello".to_vec()));
}
