// SPDX-License-Identifier: 0BSD
//! Async-completion tests: the fake completes a request *after* the driver
//! has already polled a stale used-index, so `try_complete` runs over a real
//! stale→fresh transition. This is the coverage that guards against a
//! non-volatile, hoistable used-ring load: the synchronous fake processes the
//! queue *inside* `QUEUE_NOTIFY`, so by the first poll the used-index is
//! already advanced and the loop body never runs.
//! Deferred mode forces the stale poll, exercising `poll_used`/`try_complete`
//! and the `Acquire` fence across the transition.
//!
//! Negative control on the test: delete the `device_step()` interleave and the
//! request never completes — `try_complete()` stays `None`, so the
//! `Some(Ok(()))` assertions fail (and the blocking `complete()` would spin
//! forever). The first `try_complete().is_none()` is the positive proof that
//! at least one stale poll occurred, i.e. that `submit` did not complete
//! eagerly.

use dma_pool::host::{HostBacking, SharedMem};
use dma_pool::DmaPool;
use virtio_blk::fake::FakeBlock;
use virtio_blk::{VirtioBlk, REQ_FLUSH, REQ_IN, REQ_OUT, SECTOR};

const DEV_BASE: u64 = 0x4000_0000;

/// A driver over a fake in **deferred** mode: `QUEUE_NOTIFY` only stages the
/// queue; the test runs it explicitly with `device_step()`.
fn deferred_driver(sectors: usize) -> VirtioBlk<FakeBlock, HostBacking> {
    let mem = SharedMem::new(256 * 1024);
    let fake = FakeBlock::new(mem.clone(), DEV_BASE, sectors);
    let pool = DmaPool::new(HostBacking {
        mem,
        device_base: DEV_BASE,
    });
    let mut blk = VirtioBlk::new(fake, pool, 64 * 1024).unwrap();
    blk.mmio_mut().set_deferred(true);
    blk
}

#[test]
fn write_completes_after_stale_poll() {
    let mut blk = deferred_driver(64);
    let data: Vec<u8> = (0..SECTOR * 2).map(|i| (i % 251) as u8).collect();
    blk.write_data(&data);
    blk.submit(REQ_OUT, 7, data.len(), false);

    // Device has not run yet: the poll observes a stale used-index.
    assert!(
        blk.try_complete().is_none(),
        "submit must not complete eagerly"
    );

    // Let the device drain the staged queue, then the next poll completes.
    blk.mmio_mut().device_step();
    assert_eq!(blk.try_complete(), Some(Ok(())));

    let off = 7 * SECTOR;
    assert_eq!(&blk.mmio_mut().disk[off..off + data.len()], &data[..]);
}

#[test]
fn read_completes_after_stale_poll() {
    let mut blk = deferred_driver(64);
    let data: Vec<u8> = (0..SECTOR * 3).map(|i| ((i * 7) % 251) as u8).collect();
    // Preload the disk through the fake (does not go through QUEUE_NOTIFY).
    let off = 9 * SECTOR;
    blk.mmio_mut().disk[off..off + data.len()].copy_from_slice(&data);

    blk.submit(REQ_IN, 9, data.len(), true);
    assert!(
        blk.try_complete().is_none(),
        "submit must not complete eagerly"
    );
    blk.mmio_mut().device_step();
    assert_eq!(blk.try_complete(), Some(Ok(())));

    let mut back = vec![0u8; data.len()];
    blk.read_data(&mut back);
    assert_eq!(back, data);
}

#[test]
fn flush_completes_after_stale_poll() {
    // The 2-descriptor (no-data) chain.
    let mut blk = deferred_driver(64);
    blk.submit(REQ_FLUSH, 0, 0, false);
    assert!(
        blk.try_complete().is_none(),
        "submit must not complete eagerly"
    );
    blk.mmio_mut().device_step();
    assert_eq!(blk.try_complete(), Some(Ok(())));
    assert_eq!(blk.mmio_mut().flush_count, 1);
}
