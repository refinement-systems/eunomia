// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! The proptest tier rev2§6 mandates for the driver: descriptor-chain
//! construction, used/avail-ring arithmetic, and `u16` index wrap. The driver
//! logic is pure sequential ring arithmetic over the fake's shared memory, so
//! proptest (with a Miri replay for UB) is the load-bearing tier — there is no
//! Verus/Loom obligation (the device is not a Rust thread; the host fake is
//! single-threaded by `SharedMem`'s contract).

use dma_pool::host::{HostBacking, SharedMem};
use dma_pool::DmaPool;
use proptest::prelude::*;
use virtio_blk::fake::FakeBlock;
use virtio_blk::{avail_ring_slot, capacity_check, VirtioBlk, SECTOR};

const DEV_BASE: u64 = 0x4000_0000;
/// Disk geometry for the behavioural properties. `MAX_K` sectors of headroom
/// is kept above every generated LBA so a transfer never runs off the end.
const SECTORS: u64 = 64;
const MAX_K: usize = 8;

/// A driver over a fake in the default (synchronous) mode.
fn make_driver(sectors: usize) -> VirtioBlk<FakeBlock, HostBacking> {
    let mem = SharedMem::new(256 * 1024);
    let fake = FakeBlock::new(mem.clone(), DEV_BASE, sectors);
    let pool = DmaPool::new(HostBacking {
        mem,
        device_base: DEV_BASE,
    });
    VirtioBlk::new(fake, pool, 64 * 1024).unwrap()
}

#[derive(Debug, Clone)]
enum Op {
    Write { lba: u64, data: Vec<u8> },
    Read { lba: u64, k: usize },
    Flush,
}

/// LBAs are kept in `0..SECTORS - MAX_K` so `lba + k <= SECTORS` for any
/// `k <= MAX_K`, i.e. every transfer is in-bounds.
fn op_strategy() -> impl Strategy<Value = Op> {
    let lba = 0u64..(SECTORS - MAX_K as u64);
    prop_oneof![
        (lba.clone(), 1usize..=MAX_K)
            .prop_flat_map(|(lba, k)| {
                (
                    Just(lba),
                    proptest::collection::vec(any::<u8>(), k * SECTOR),
                )
            })
            .prop_map(|(lba, data)| Op::Write { lba, data }),
        (lba, 1usize..=MAX_K).prop_map(|(lba, k)| Op::Read { lba, k }),
        Just(Op::Flush),
    ]
}

/// Overflow-free `u128` mirror of `virtio_blk::out_of_range` (a ghost `spec fn`,
/// so not callable from exec test code): a transfer is out of range when its
/// last sector `lba + len/SECTOR` exceeds `capacity`. Independent of the
/// production `checked_add` path, so it gives the `capacity_check` proptests
/// real teeth.
fn out_of_range_oracle(lba: u64, len: usize, capacity: u64) -> bool {
    lba as u128 + (len / SECTOR) as u128 > capacity as u128
}

proptest! {
    // Miri: a handful of cases cover the same chain/ring paths; native keeps
    // the full sweep (mirrors cas/src/file.rs, storage-server rights_lattice).
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// Property 1 — descriptor-chain round-trip. Driving real requests through
    /// the fake exercises head/data/status chain construction, the
    /// `DESC_F_NEXT`/`DESC_F_WRITE` flags, and the status byte (read+write =
    /// 3-desc chain, flush = 2-desc chain). The driver must agree with a plain
    /// `Vec<u8>` model of the disk.
    #[test]
    fn chain_roundtrip(ops in proptest::collection::vec(op_strategy(), 1..24)) {
        let mut blk = make_driver(SECTORS as usize);
        let mut model = vec![0u8; SECTORS as usize * SECTOR];
        for op in ops {
            match op {
                Op::Write { lba, data } => {
                    blk.write_sectors(lba, &data).unwrap();
                    let off = lba as usize * SECTOR;
                    model[off..off + data.len()].copy_from_slice(&data);
                }
                Op::Read { lba, k } => {
                    let mut back = vec![0u8; k * SECTOR];
                    blk.read_sectors(lba, &mut back).unwrap();
                    let off = lba as usize * SECTOR;
                    prop_assert_eq!(&back[..], &model[off..off + k * SECTOR]);
                }
                Op::Flush => blk.flush().unwrap(),
            }
        }
    }

    /// Property 2 — `avail_ring_slot` is always in-bounds of the avail buffer
    /// (`pool.alloc(6 + 2*qsize, 2)`: flags + idx + `qsize`-entry ring +
    /// used_event), for every `u16` index and queue size the driver uses.
    #[test]
    fn avail_ring_slot_in_bounds(idx in any::<u16>(), qsize in 1u16..=8) {
        let slot = avail_ring_slot(idx, qsize);
        prop_assert!(slot >= 4);
        prop_assert!(slot + 2 <= 6 + 2 * qsize as usize);
    }

    /// Property 4 — the Verus-verified `capacity_check` (rev2§4.5 LBA bound)
    /// agrees with an independent `u128` oracle that cannot wrap, for every
    /// `(lba, len, capacity)`. The oracle shares none of `checked_add`'s
    /// production arithmetic, so a wrong refusal in either direction fails here.
    #[test]
    fn capacity_check_matches_oracle(
        lba in any::<u64>(),
        len in 0usize..=1 << 20,
        capacity in any::<u64>(),
    ) {
        prop_assert_eq!(
            capacity_check(lba, len, capacity).is_err(),
            out_of_range_oracle(lba, len, capacity)
        );
    }

    /// Property 4 (continued) — pin `lba` to the `u64::MAX` neighbourhood so the
    /// `checked_add` wrap edge (rarely hit by a uniform `lba`) is exercised: the
    /// last-sector sum overflows u64 and must still refuse, not alias a valid LBA.
    #[test]
    fn capacity_check_high_lba_refuses(
        lba in (u64::MAX - 4096)..=u64::MAX,
        len in 0usize..=1 << 20,
        capacity in any::<u64>(),
    ) {
        prop_assert_eq!(
            capacity_check(lba, len, capacity).is_err(),
            out_of_range_oracle(lba, len, capacity)
        );
    }
}

/// Property 4 (boundaries) — the `capacity_check` edge cases, asserted with
/// teeth (each accept and each refusal pinned). Mirrors the integration test
/// `lba_past_capacity_refused_locally`, but on the pure verified helper.
#[test]
fn capacity_check_boundaries_have_teeth() {
    // len == 0 ⇒ 0 sectors: Ok iff lba <= capacity.
    assert!(capacity_check(64, 0, 64).is_ok());
    assert!(capacity_check(65, 0, 64).is_err());
    // end == capacity is in range; end == capacity + 1 is refused.
    assert!(capacity_check(63, SECTOR, 64).is_ok()); // 63 + 1 == 64
    assert!(capacity_check(64, SECTOR, 64).is_err()); // 64 + 1 == 65 > 64

    // A multi-sector transfer whose tail crosses the end is refused as a whole.
    assert!(capacity_check(62, SECTOR * 4, 64).is_err()); // 62 + 4 == 66 > 64

    // An adversarial lba near u64::MAX refuses via checked_add, never wraps.
    assert!(capacity_check(u64::MAX, SECTOR, 64).is_err());
    assert!(capacity_check(u64::MAX, 0, u64::MAX).is_ok()); // exact end, no overflow
}

/// Property 2 (continued) — the `u16` index wraps cleanly: stepping
/// `wrapping_add(1)` for `1 << 16` ticks returns to the start, and the ring
/// slot stays in range across the whole cycle. Proven on the pure helper, not
/// via 65536 device ops, so it is cheap under Miri's interpreter.
#[test]
fn avail_index_wraps_consistently() {
    for qsize in 1u16..=8 {
        let mut idx = 0u16;
        for _ in 0..(1u32 << 16) {
            let slot = avail_ring_slot(idx, qsize);
            assert!(slot >= 4 && slot + 2 <= 6 + 2 * qsize as usize);
            idx = idx.wrapping_add(1);
        }
        assert_eq!(idx, 0, "u16 index returns to start after 1<<16 steps");
    }
}

/// Property 3 — index wrap behaviourally (native scale). Issue far more than
/// `queue_size` (and more than `1 << 16`) small requests so both `avail_idx`
/// and the fake's `used_idx` wrap; every request must still complete `Ok` with
/// its data intact — a desync between the rings would mis-poll (hang) or return
/// stale bytes. Native scale; under Miri this drops to a handful of requests
/// (the wrap *arithmetic* is covered purely by `avail_index_wraps_consistently`).
#[test]
fn index_wrap_no_desync() {
    let mut blk = make_driver(SECTORS as usize);
    let count: u32 = if cfg!(miri) { 4 } else { 70_000 };
    let mut sector = [0u8; SECTOR];
    for i in 0..count {
        sector[0] = i as u8;
        sector[1] = (i >> 8) as u8;
        sector[2] = (i >> 16) as u8;
        let lba = u64::from(i % SECTORS as u32);
        blk.write_sectors(lba, &sector).unwrap();
        let mut back = [0u8; SECTOR];
        blk.read_sectors(lba, &mut back).unwrap();
        assert_eq!(
            [back[0], back[1], back[2]],
            [sector[0], sector[1], sector[2]]
        );
    }
}
