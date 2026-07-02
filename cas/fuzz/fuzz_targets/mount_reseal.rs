// SPDX-License-Identifier: 0BSD
#![no_main]
//! Mount as a *total* function over arbitrary device contents, sealed or
//! not — `Ok` or `Err`, never a panic, never an allocation unbounded by
//! the device length.
//!
//! The raw `mount_recovery` target cannot test that contract behind the
//! superblock checksum: every body mutation dies at the gate, so a clean
//! run there is evidence the checksum works, not that the geometry
//! validation behind it is safe. And the checksum is integrity, not
//! authenticity — there is no secret in it, so anyone who can place bytes
//! on a device (restored backup, copied image, the USB stick) re-seals it
//! in microseconds. `reseal_image` plays exactly that adversary: every
//! checksum and content hash mount verifies is recomputed, no geometry
//! field is repaired, nothing the image cannot physically hold is sealed.
//! The mutations therefore land on the offset/length fields mount actually
//! consumes, and the contract needs no threat-model carve-out left to rot.
//!
//! Run with a low `-malloc_limit_mb` (fuzz.sh hunt sets 128): a single
//! oversized allocation is a finding — allocations must be bounded by the
//! device length, which here is the input length. Keep `mount_recovery`
//! too: the rejection path of an unsealed image is itself code under test.
use libfuzzer_sys::fuzz_target;

use cas::dev::MemDev;
use cas::fuzz_support::reseal_image;
use cas::store::{Store, StoreOptions};

fuzz_target!(|data: &[u8]| {
    let mut img = data.to_vec();
    reseal_image(&mut img);
    let dev = MemDev::from_bytes(img);
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
