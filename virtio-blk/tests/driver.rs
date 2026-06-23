//! Driver tests against the fake virtio-mmio device, ending with the
//! whole cas storage engine running over the driver — the same code that
//! later binds to real MMIO in QEMU.

use cas::store::{Store, StoreOptions};
use dma_pool::host::{HostBacking, SharedMem};
use dma_pool::DmaPool;
use virtio_blk::blockdev::VirtioBlockDev;
use virtio_blk::fake::FakeBlock;
use virtio_blk::{VirtioBlk, VirtioError, SECTOR};

const DEV_BASE: u64 = 0x4000_0000;

fn make_driver(sectors: usize) -> VirtioBlk<FakeBlock, HostBacking> {
    let mem = SharedMem::new(256 * 1024);
    let fake = FakeBlock::new(mem.clone(), DEV_BASE, sectors);
    let pool = DmaPool::new(HostBacking {
        mem,
        device_base: DEV_BASE,
    });
    VirtioBlk::new(fake, pool, 64 * 1024).unwrap()
}

#[test]
fn probe_negotiates_and_reads_capacity() {
    let blk = make_driver(1000);
    assert_eq!(blk.capacity_sectors(), 1000);
}

#[test]
fn sector_roundtrip_and_flush() {
    let mut blk = make_driver(64);
    let data: Vec<u8> = (0..SECTOR * 3).map(|i| (i % 251) as u8).collect();
    blk.write_sectors(5, &data).unwrap();
    let mut back = vec![0u8; SECTOR * 3];
    blk.read_sectors(5, &mut back).unwrap();
    assert_eq!(back, data);
    blk.flush().unwrap();

    // Reads of untouched sectors are zero.
    let mut zero = vec![0xAAu8; SECTOR];
    blk.read_sectors(60, &mut zero).unwrap();
    assert!(zero.iter().all(|&b| b == 0));
}

#[test]
fn blockdev_adapter_handles_unaligned_io() {
    let dev_inner = make_driver(256);
    let mut dev = VirtioBlockDev::new(dev_inner);
    use cas::dev::BlockDev;

    assert_eq!(dev.len(), 256 * SECTOR as u64);

    // Model: a plain byte array.
    let mut model = vec![0u8; 256 * SECTOR];
    let cases: &[(u64, usize)] = &[
        (0, 10),
        (511, 2),      // straddles a sector boundary
        (513, 1500),   // partial head, full middle, partial tail
        (700, 65_536), // spans multiple max_transfer chunks
        (130_000, 3),
    ];
    for (i, &(off, len)) in cases.iter().enumerate() {
        let data: Vec<u8> = (0..len).map(|j| ((i * 37 + j) % 251) as u8).collect();
        dev.write(off, &data)
            .unwrap_or_else(|e| panic!("case {i} (off={off}, len={len}): {e}"));
        model[off as usize..off as usize + len].copy_from_slice(&data);
    }
    let mut back = vec![0u8; 256 * SECTOR];
    dev.read(0, &mut back).unwrap();
    assert_eq!(back, model);

    // Reading past the end of the device must fail, not wrap.
    let mut over = [0u8; 4];
    assert!(dev.read(256 * SECTOR as u64 - 2, &mut over).is_err());
}

// rev2§4.5: the driver carries a defensive LBA-vs-capacity bound, so an
// out-of-range transfer is refused locally (`OutOfRange`) *before* any device
// round-trip — distinct from the device's own `DeviceError`, which a real
// round-trip past the end would surface.
#[test]
fn lba_past_capacity_refused_locally() {
    let mut blk = make_driver(64);
    let cap = blk.capacity_sectors();
    assert_eq!(cap, 64);

    // The exact last sector is in range: end == capacity is allowed.
    let mut last = vec![0u8; SECTOR];
    blk.read_sectors(cap - 1, &mut last).unwrap();
    blk.write_sectors(cap - 1, &last).unwrap();

    // One sector starting one past the end → local refusal (not DeviceError),
    // for both directions, and with no device round-trip.
    let mut over = vec![0u8; SECTOR];
    assert_eq!(
        blk.read_sectors(cap, &mut over),
        Err(VirtioError::OutOfRange)
    );
    assert_eq!(blk.write_sectors(cap, &over), Err(VirtioError::OutOfRange));

    // A multi-sector transfer that starts in range but whose tail crosses the
    // end is refused as a whole.
    let mut multi = vec![0u8; SECTOR * 4];
    assert_eq!(
        blk.read_sectors(cap - 2, &mut multi),
        Err(VirtioError::OutOfRange)
    );

    // An adversarial lba near u64::MAX refuses via checked add rather than
    // wrapping back into a valid-looking range.
    assert_eq!(
        blk.read_sectors(u64::MAX, &mut last),
        Err(VirtioError::OutOfRange)
    );
}

// Native-only: drives interpreted BLAKE3 through the whole cas engine — hours
// under Miri. The Miri target is the *driver* (driver.rs + ring_props.rs +
// async_complete.rs); the storage engine has its own Miri sweep in `cas`.
#[cfg_attr(miri, ignore)]
#[test]
fn storage_engine_runs_over_virtio() {
    let opts = StoreOptions {
        wal_len: 32 * 1024,
        chunker: cas::chunk::ChunkerParams {
            min: 64,
            avg: 256,
            max: 1024,
        },
        global_budget: 64 * 1024,
        ..StoreOptions::default()
    };
    let p =
        |parts: &[&str]| -> Vec<Vec<u8>> { parts.iter().map(|s| s.as_bytes().to_vec()).collect() };

    // 4 MiB virtio disk, full stack on top.
    let dev = VirtioBlockDev::new(make_driver(8192));
    let mut store = Store::format(dev, opts).unwrap();
    store.create_ref(b"main").unwrap();
    let big: Vec<u8> = (0..100_000u32).flat_map(|i| i.to_le_bytes()).collect();
    store.write(b"main", &p(&["data.bin"]), 0, &big, 1).unwrap();
    let snap = store
        .snapshot(b"main", b"test", b"", cas::disk::CLASS_KEEP, 50)
        .unwrap();
    store
        .write(b"main", &p(&["data.bin"]), 0, b"CLOBBERED", 2)
        .unwrap();
    store.sync_all().unwrap();

    // Remount over the same virtio device: recovery path runs through
    // the driver too.
    let store2 = Store::mount(store.into_dev(), opts).unwrap();
    let head = store2.read(b"main", &p(&["data.bin"])).unwrap().unwrap();
    assert_eq!(&head[..9], b"CLOBBERED");
    let root = store2.snapshot_root(b"main", snap).unwrap();
    assert_eq!(
        store2
            .read_at_root(&root, &p(&["data.bin"]))
            .unwrap()
            .unwrap(),
        big
    );
}
