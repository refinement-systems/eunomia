#![no_main]
//! Mount = crash recovery (rev0§4.5), as a parser of hostile disks. Arbitrary
//! bytes are presented as one whole image over the fake block device;
//! `Store::mount` must return Ok or Err and never panic. `MemDev`
//! bounds-checks every access, so an out-of-range read mount attempts
//! becomes a `StoreError`, not UB — that itself is part of the property
//! ("never read out of the device's bounds").
//!
//! On a successful mount we run one cheap consistency pass: list every
//! ref's root and walk to each snapshot root. This is rev0§4.5's "either a
//! commit completed or it didn't" as an executable, coverage-guided
//! property — adversarial input the crash-injection proptest never tries.
//! Seed this target with mkfs-built minimal images and crash artifacts.
//!
//! Note on allocation: every mount-time allocation is sized by a field
//! that `Superblock::validate_geometry` has already bounded by the real
//! device length — but only the `mount_reseal`
//! sibling can actually drive those fields, since this target's mutations
//! die at the superblock checksum. Run both with a low `-malloc_limit_mb`;
//! an allocation kill is a finding ("allocation must be bounded by
//! remaining input length").
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
