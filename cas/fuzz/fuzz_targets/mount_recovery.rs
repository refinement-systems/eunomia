#![no_main]
//! Mount = crash recovery (§4.5), as a parser of hostile disks. Arbitrary
//! bytes are presented as one whole image over the fake block device;
//! `Store::mount` must return Ok or Err and never panic. `MemDev`
//! bounds-checks every access, so an out-of-range read mount attempts
//! becomes a `StoreError`, not UB — that itself is part of the property
//! ("never read out of the device's bounds").
//!
//! On a successful mount we run one cheap consistency pass: list every
//! ref's root and walk to each snapshot root. This is §4.5's "either a
//! commit completed or it didn't" as an executable, coverage-guided
//! property — adversarial input the crash-injection proptest never tries.
//! Seed this target with mkfs-built minimal images and crash artifacts.
//!
//! Note on allocation: mount sizes a buffer from the index frame's length
//! header before checking it against the real device length, so a valid
//! superblock pointing at a frame that claims a huge length is a
//! length-driven allocation. Run with a low `-rss_limit_mb`; an RSS kill
//! is a finding ("allocation must be bounded by remaining input length").
use libfuzzer_sys::fuzz_target;

use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};

fuzz_target!(|data: &[u8]| {
    let dev = MemDev::from_bytes(data.to_vec());
    let Ok(store) = Store::mount(dev, StoreOptions::default()) else {
        return;
    };
    let refs: Vec<Vec<u8>> = store.refs().map(|(n, _)| n.clone()).collect();
    for name in &refs {
        let _ = store.list(name, &Vec::new());
        let snaps: Vec<u64> = store.snapshots(name).map(|s| s.id).collect();
        for id in snaps {
            if let Ok(root) = store.snapshot_root(name, id) {
                let _ = store.list_dir_node(&root);
            }
        }
    }
});
