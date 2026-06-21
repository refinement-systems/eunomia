//! Userspace runtime: a global allocator over a static in-image heap.
//!
//! Eunomia processes are single-threaded; the allocator is first-fit with
//! address-ordered two-sided coalescing, no locking. The heap lives in .bss,
//! so the loader maps and zeroes it with the RW segment — no untyped or
//! mapping calls needed to get a heap.
//!
//! The free list is **side-stored, not intrusive**: the arena `[u8; N]` is
//! pure storage (handed to callers, never holding allocator metadata), and the
//! free extents live in a separate `freelist::FreeList<HEAP_RANGES>` field — a
//! sorted, pairwise-disjoint list of `(offset, len)` extents over `[0, N)`. The
//! allocation algorithm (first-fit search, alignment round-up, split,
//! two-sided coalesce) is therefore the **Verus-verified** `FreeList`
//! arithmetic (rev1§6; the shared core extracted in B11A,
//! `doc/plans/12_b11-detail.md`). The only `unsafe` left in the allocator is a
//! three-step arena seam: `UnsafeCell` → `&mut`, `offset → *mut u8` via
//! `base.add(off)` (in-arena by `alloc`'s `ensures`), and `*mut u8 → offset` on
//! dealloc — the same trusted byte-region boundary the DMA-pool wrapper has,
//! kept honest by Miri+proptest (the wrapper tier lands in B11C).
//!
//! MVP simplifications, recorded:
//!   - **Fragmentation cap `HEAP_RANGES = 1024`.** The side-stored free list is
//!     a fixed `[(usize, usize); 1024]` array, so at most 1024 free extents can
//!     coexist (the intrusive list it replaced had no such bound, at the cost of
//!     being unverifiable). The extent count equals the number of gaps between
//!     live allocations; reaching 1024 needs >1024 simultaneous non-adjacent
//!     holes, unreachable for the small, short-lived userspace processes here
//!     (shell's 1 MiB and storaged's 3 MiB heaps). Disclosed bound, tunable.
//!   - **`dealloc` at the cap leaks the block.** `FreeList::free` requires
//!     `nfree < N`, so at the cap the wrapper cannot record the freed region. A
//!     heap must never abort a `dealloc`, so the wrapper returns without
//!     recording it — those bytes are simply never reused (safe: the free-list
//!     invariant "every listed extent is truly free" is preserved). A
//!     `debug_assert!` fires as a debug-build witness; release is a silent leak.
//!     (A future `free_or_coalesce` that admits a merging free at the cap would
//!     shrink the leak window; out of scope here.)
//!   - **`MAX_ALIGN = 64`.** The arena base is `align(64)`, so `base.add(off)`
//!     meets any `layout.align() <= 64` (every standard allocation, plus
//!     cache-line / common SIMD). A larger request returns null (clean OOM).
//!
//! Usage in a process binary:
//!   #[global_allocator]
//!   static HEAP: urt::Heap<{ 2 * 1024 * 1024 }> = urt::Heap::new();

#![no_std]
// Clippy is not a CI gate: both fire in `verus!{}` verified exec code where the
// explicit forms are deliberate — `x = x + y` and the explicit saturating-subtract
// branch (a hand-spelled `.saturating_sub`, which Verus has no model for). Fixing
// them would refactor verified code cosmetically.
#![allow(clippy::assign_op_pattern, clippy::implicit_saturating_sub)]

// Verus, the deductive-proof tier for the host-side userspace bookkeeping.
// `vstd::prelude` supplies the `verus!{}` macro + ghost vocabulary the `slots`
// proof uses; Verus requires it imported at the crate root. In an ordinary build
// the macro erases ghost code, so this import is otherwise unused — hence the
// allow (same as kcore/src/lib.rs, ipc/src/lib.rs).
#[allow(unused_imports)]
use vstd::prelude::*;

pub mod slots;
pub mod time;

// The spawn-lifecycle helper issues syscalls, so it only exists on the
// bare-metal target; `slots` is pure bookkeeping and host-tested.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod spawn;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

// The verified free-list core (shared with dma-pool; extracted in B11A). The
// heap's whole allocation algorithm is this proof — see the module doc.
use freelist::FreeList;

/// Free-extent fragmentation cap: the side-stored `FreeList` is a fixed array of
/// this many `(offset, len)` extents (Design decision 3). Disclosed MVP bound.
const HEAP_RANGES: usize = 1024;
/// Arena granularity: every allocation is rounded up to this, so carved offsets
/// stay 16-aligned (the minimum alignment every Rust allocation expects).
const MIN_ALIGN: usize = 16;
/// Arena base alignment (the `#[repr(align)]` below). `base.add(off)` meets any
/// `layout.align() <= MAX_ALIGN`; a larger request is refused with null.
const MAX_ALIGN: usize = 64;

#[repr(C, align(64))] // = MAX_ALIGN, so base.add(off) satisfies layout.align() <= 64.
pub struct Heap<const N: usize> {
    /// Pure storage now — handed to callers, never holds allocator metadata.
    mem: UnsafeCell<[u8; N]>,
    /// The verified free list, side-stored. `None` until the first `alloc`
    /// builds it lazily; `None`'s all-zero representation keeps the static in
    /// `.bss` (the loader zeroes it with the RW segment).
    fl: UnsafeCell<Option<FreeList<HEAP_RANGES>>>,
}

// Single-threaded processes; no concurrent access by construction.
unsafe impl<const N: usize> Sync for Heap<N> {}

impl<const N: usize> Heap<N> {
    pub const fn new() -> Self {
        Heap {
            mem: UnsafeCell::new([0; N]),
            fl: UnsafeCell::new(None),
        }
    }

    /// Borrow the free list, building the fresh full-arena state on first use.
    /// `FreeList::new(N)` `ensures` the single extent `[0, N)` + `wf` (proven);
    /// `HEAP_RANGES >= 1` satisfies its `N >= 1` requirement.
    unsafe fn fl_mut(&self) -> &mut FreeList<HEAP_RANGES> {
        (*self.fl.get()).get_or_insert_with(|| FreeList::new(N))
    }
}

impl<const N: usize> Default for Heap<N> {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl<const N: usize> GlobalAlloc for Heap<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        // Over-aligned beyond the arena base: a clean OOM, not UB (Decision 3).
        if align > MAX_ALIGN {
            return ptr::null_mut();
        }
        // Round up to MIN_ALIGN so carved offsets stay 16-aligned; `.max(1)`
        // keeps `need > 0` (FreeList::alloc returns None for n == 0).
        let need = layout.size().max(1).next_multiple_of(MIN_ALIGN);
        let fl = self.fl_mut();
        // `align.max(MIN_ALIGN) >= 16 > 0` discharges FreeList::alloc's sole
        // `align > 0` precondition (it computes `off % align`).
        match fl.alloc(need, align.max(MIN_ALIGN)) {
            // The lone raw-pointer formation: `off + need <= N` by alloc's
            // `ensures`, and `off` is `align.max(16)`-aligned, so over a
            // 64-aligned base the address meets `layout.align() <= 64`.
            Some(off) => (self.mem.get() as *mut u8).add(off),
            None => ptr::null_mut(), // OOM / fragmentation cap / no fit
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // Identical rounding to `alloc`, so the extent round-trips exactly.
        let need = layout.size().max(1).next_multiple_of(MIN_ALIGN);
        let off = (p as usize) - (self.mem.get() as usize);
        let fl = self.fl_mut();
        // Decision 3: at the cap, leak rather than abort a free. FreeList::free's
        // no-merge arm calls insert_at, which would index free[N] (out of bounds)
        // when nfree == N; the guard makes that a safe leak (the freed bytes are
        // just never re-handed-out). A heap must never abort a dealloc, so the
        // witness is a debug_assert, compiled out in release.
        if fl.is_full() {
            debug_assert!(false, "urt heap: free-list at fragmentation cap; block leaked");
            return;
        }
        // Double-free / overlap guard. `is_allocated` is the verified accessor
        // (in `freelist`); heap input is trusted in-process (note 4 of the B11
        // verification tier), so this is a debug_assert — release rests on
        // `core`'s correctness, the same line dma-pool draws.
        debug_assert!(fl.is_allocated(off, need), "urt heap: double free / overlap");
        fl.free(off, need);
    }

    // realloc is the default GlobalAlloc impl (alloc-new + copy + dealloc-old).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_reuse() {
        static H: Heap<65536> = Heap::new();
        unsafe {
            let l = Layout::from_size_align(1000, 8).unwrap();
            let a = H.alloc(l);
            let b = H.alloc(l);
            assert!(!a.is_null() && !b.is_null() && a != b);
            H.dealloc(a, l);
            H.dealloc(b, l);
            // After two-sided coalescing, a near-full-heap allocation fits again.
            let big = Layout::from_size_align(60000, 16).unwrap();
            let c = H.alloc(big);
            assert!(!c.is_null());
            H.dealloc(c, big);
        }
    }

    #[test]
    fn exhaustion_returns_null() {
        static H: Heap<4096> = Heap::new();
        unsafe {
            let l = Layout::from_size_align(8192, 8).unwrap();
            assert!(H.alloc(l).is_null());
        }
    }

    #[test]
    fn over_alignment_returns_null() {
        // align > MAX_ALIGN (64) cannot be met by the arena base → clean OOM,
        // not UB. Below the cap (align <= 64) is exercised by alloc_free_reuse.
        static H: Heap<4096> = Heap::new();
        unsafe {
            let l = Layout::from_size_align(64, 128).unwrap();
            assert!(H.alloc(l).is_null());
            // A 64-aligned request of the same size still succeeds.
            let ok = Layout::from_size_align(64, 64).unwrap();
            assert!(!H.alloc(ok).is_null());
        }
    }

    /// Fragment a heap to exactly `HEAP_RANGES` free extents and return the
    /// heap plus a *victim* block whose dealloc would need a 1025th extent (its
    /// neighbours are live, so it cannot merge). Fully carved into 16-byte
    /// blocks (no trailing free extent), so the extent count is exact: freeing
    /// the first 1024 even-indexed blocks drives `nfree` 1→1024 with no merges.
    fn capped_heap() -> (&'static Heap<{ 2050 * 16 }>, *mut u8, Layout) {
        static H: Heap<{ 2050 * 16 }> = Heap::new();
        let l = Layout::from_size_align(16, 16).unwrap();
        let mut ptrs = [ptr::null_mut::<u8>(); 2050];
        unsafe {
            for slot in ptrs.iter_mut() {
                *slot = H.alloc(l);
                assert!(!slot.is_null());
            }
            // Free even-indexed blocks 0,2,…,2046 — each isolated by its live
            // odd neighbours → 1024 non-adjacent extents (no merges, no panic:
            // the cap check is at dealloc entry, so the 1024th free still runs).
            for i in (0..2048).step_by(2) {
                H.dealloc(ptrs[i], l);
            }
        }
        // Block 2048 (offset 32768): neighbours 2047 and 2049 are live, so its
        // free cannot merge → it would be the over-cap 1025th extent.
        (&H, ptrs[2048], l)
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "fragmentation cap")]
    fn dealloc_at_cap_witness_in_debug() {
        let (h, victim, l) = capped_heap();
        // Debug: the leak-path witness fires (a controlled panic, never UB/abort).
        unsafe { h.dealloc(victim, l) };
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn dealloc_at_cap_leaks_in_release() {
        let (h, victim, l) = capped_heap();
        unsafe {
            // Release: the debug_assert is compiled out → silent safe leak; the
            // call must return without aborting.
            h.dealloc(victim, l);
            // The allocator keeps serving other requests after the leak.
            assert!(!h.alloc(l).is_null());
        }
    }
}
