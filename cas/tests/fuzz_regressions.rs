//! Regression reproducers for findings surfaced by the storage-server and
//! cas fuzz targets, rooted in cas.
//!
//! These document *currently unfixed* findings (fixing is out of scope for
//! the fuzzing work). Written `#[should_panic]` so they pass today by
//! asserting the bug still bites, and fail the moment it is fixed. See
//! doc/results/1_fuzzing-findings.md.

use cas::chunk::ChunkerParams;
use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};

/// FINDING OVL-1 (unfixed): a write at `offset` near u64::MAX panics with an
/// arithmetic overflow in `FileOverlay::insert` (`off + data.len()`,
/// overlay.rs). Reachable from a `Write` request — a client with write
/// access can crash the storage server with one message (found by the
/// `request_dispatch` target). When fixed, the write should be rejected
/// (e.g. a bounds error) rather than panic; flip this test to assert that.
#[test]
#[should_panic(expected = "overflow")]
fn ovl1_write_offset_overflow_panics() {
    let opts = StoreOptions {
        wal_len: 4096,
        chunker: ChunkerParams { min: 64, avg: 256, max: 1024 },
        overlay_budget: 16 * 1024,
    };
    let mut store = Store::format(MemDev::new(64 * 1024), opts).unwrap();
    store.create_ref(b"main").unwrap();
    let _ = store.write(b"main", &vec![b"f".to_vec()], u64::MAX, b"x", 1);
}
