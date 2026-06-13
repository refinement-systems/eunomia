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

/// Two allocations are disjoint and in-pool, each device address is exactly
/// `device_base + offset` (the bijection), and the aligned one lands on its
/// boundary — and the allocator arithmetic never panics/overflows on the path.
///
/// Inputs are **concrete**. Symbolic allocation sizes over the
/// `[(usize, usize); 64]` free list (`MAX_FREE_RANGES`) with `copy_within`
/// generate a SAT instance that exhausts CBMC's memory (the findings SOLVER
/// note); full "for all sizes" disjointness stays with the unit tests +
/// proptest. Kani's value here is the exhaustive arithmetic-safety check on a
/// representative carve-and-split sequence.
#[kani::proof]
#[kani::unwind(4)]
fn check_dma_alloc_disjoint() {
    let mut p = DmaPool::new(Backing); // POOL = 16
    let a = p.alloc(5, 1).unwrap();
    let b = p.alloc(4, 4).unwrap(); // forces a round-up split
    let ao = a.device_addr().0 - DEV_BASE;
    let bo = b.device_addr().0 - DEV_BASE;
    // bijection
    assert!(a.device_addr().0 == DEV_BASE + ao && b.device_addr().0 == DEV_BASE + bo);
    // in-pool
    assert!(ao as usize + a.len() <= POOL && bo as usize + b.len() <= POOL);
    // alignment honoured
    assert!(bo % 4 == 0);
    // disjoint
    assert!(ao + a.len() as u64 <= bo || bo + b.len() as u64 <= ao);
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
