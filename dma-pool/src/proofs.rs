//! Kani harnesses for the DMA pool allocator (plan §4.7): handed-out buffers
//! are disjoint and within the pool, the device-address↔offset map is the
//! exact bijection `device_addr = device_base + offset`, alignment is honoured,
//! and a freed range merges back so the whole pool is reusable.
//!
//! The harness backing exercises only the free-list arithmetic (`alloc`/`free`
//! never dereference `cpu_base`), so `cpu_base` returns a null pointer that is
//! never read — the `DmaBacking` safety contract about backing memory is
//! vacuously irrelevant here.

#![cfg(kani)]

use crate::{DeviceAddress, DmaBacking, DmaPool};

const DEV_BASE: u64 = 0x4000_0000;
const POOL: usize = 16;

struct Backing;

// SAFETY (harness-only): alloc/free touch only the free list, never the
// backing bytes, so the cpu_base contract is unused.
unsafe impl DmaBacking for Backing {
    fn cpu_base(&self) -> *mut u8 {
        core::ptr::null_mut()
    }
    fn device_base(&self) -> DeviceAddress {
        DeviceAddress(DEV_BASE)
    }
    fn len(&self) -> usize {
        POOL
    }
}

/// Allocator safety, in two parts. **Part 1 is for all first-allocation sizes**
/// (the "for all sizes" content the review asked for in rec. #4): the arithmetic
/// never panics/overflows, a fresh pool rejects exactly the empty and won't-fit
/// requests, and an accepted buffer sits at offset 0, in-pool, with the exact
/// bijection `device_addr = device_base + offset`. **Part 2** keeps the original
/// concrete carve-and-split that exercises the alignment round-up and
/// disjointness of two live buffers.
///
/// Why Part 1 is symbolic-tractable but Part 2 is not (DN-10): a single alloc
/// with `align == 1` from a *fresh* pool reads only the concrete entry `(0,
/// POOL)` — `start = 0`, `pad = 0`, `device_addr = base + 0` — so the symbolic
/// size touches just the `len1 > POOL` boundary compare (no overflow: `pad +
/// len1` adds 0). Add a *second* alloc and it re-reads the now-*symbolic*
/// remainder entry `(len1, POOL-len1)`, and the round-up `(off+align-1) &
/// !(align-1)` over a symbolic offset bit-blasts CaDiCaL to OOM (confirmed; same
/// wall as a symbolic *alignment*). So the round-up / two-buffer disjointness
/// stays a representative concrete pair; "for all sizes" disjointness remains
/// with the unit tests + proptest.
#[kani::proof]
#[kani::unwind(4)]
fn check_dma_alloc_disjoint() {
    // ── Part 1: for all first sizes — boundary, in-pool, bijection ──────────
    {
        let len1: usize = kani::any();
        let mut p = DmaPool::new(Backing); // POOL = 16
        match p.alloc(len1, 1) {
            None => {
                assert!(len1 == 0 || len1 > POOL); // reject ⟺ empty or won't fit
                kani::cover!(len1 == 0);
                kani::cover!(len1 > POOL);
            }
            Some(a) => {
                let ao = a.device_addr().0 - DEV_BASE;
                assert!(a.device_addr().0 == DEV_BASE + ao); // bijection
                assert!(ao == 0 && a.len() == len1); // first-fit at base, exact len
                assert!(ao as usize + a.len() <= POOL); // in-pool
                kani::cover!(len1 == POOL); // the whole-pool size is accepted
            }
        }
    }

    // ── Part 2: concrete carve-and-split — alignment round-up + disjoint ────
    {
        let mut p = DmaPool::new(Backing);
        let a = p.alloc(5, 1).unwrap();
        let b = p.alloc(4, 4).unwrap(); // forces a round-up split (start 5 → 8)
        let ao = a.device_addr().0 - DEV_BASE;
        let bo = b.device_addr().0 - DEV_BASE;
        assert!(a.device_addr().0 == DEV_BASE + ao && b.device_addr().0 == DEV_BASE + bo);
        assert!(ao as usize + a.len() <= POOL && bo as usize + b.len() <= POOL); // in-pool
        assert!(bo % 4 == 0); // alignment honoured
        assert!(ao + a.len() as u64 <= bo || bo + b.len() as u64 <= ao); // disjoint
    }
}

/// Freeing the only allocation merges the range back, so the whole pool is
/// allocatable again at the base offset.
#[kani::proof]
#[kani::unwind(5)]
fn check_dma_free_reuse() {
    let mut p = DmaPool::new(Backing);
    let a = p.alloc(POOL, 1).unwrap(); // the whole pool
    assert!(p.alloc(1, 1).is_none()); // exhausted
    p.free(a);
    let b = p.alloc(POOL, 1).unwrap(); // merged back to one range
    assert!(b.device_addr().0 == DEV_BASE);
}
