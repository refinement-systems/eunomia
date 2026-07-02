// SPDX-License-Identifier: 0BSD
//! Userspace runtime: a global allocator over a static in-image heap.
//!
//! The allocator is first-fit with address-ordered two-sided coalescing. Its
//! free-list critical section is serialized by a yielding spinlock ([`lock`]) so
//! in-process threads (rev2§5.3) can allocate concurrently on the one process
//! `static HEAP`; the lock's mutual exclusion is Loom-certified (never Verus — an
//! Acquire/Release protocol, `doc/guidelines/verification.md`). The heap lives in
//! .bss, so the loader maps and zeroes it with the RW segment — no untyped or
//! mapping calls needed to get a heap.
//!
//! The free list is **side-stored, not intrusive**: the arena `[u8; N]` is
//! pure storage (handed to callers, never holding allocator metadata), and the
//! free extents live in a separate `freelist::FreeList<HEAP_RANGES>` field — a
//! sorted, pairwise-disjoint list of `(offset, len)` extents over `[0, N)`. The
//! allocation algorithm (first-fit search, alignment round-up, split,
//! two-sided coalesce) is therefore the **Verus-verified** `FreeList`
//! arithmetic of the shared `freelist` crate (rev2§6). The only `unsafe` left in
//! the allocator is a three-step arena seam: `UnsafeCell` → `&mut`,
//! `offset → *mut u8` via `base.add(off)` (in-arena by `alloc`'s `ensures`), and
//! `*mut u8 → offset` on dealloc — the same trusted byte-region boundary the
//! DMA-pool wrapper has, kept honest by Miri+proptest.
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
//!   - **`MAX_ALIGN = 128`.** The arena base is `align(128)`, so `base.add(off)`
//!     meets any `layout.align() <= 128` (every standard allocation, plus the
//!     AArch64 cache line — `std::sync::mpsc`'s cache-line-padded channel block, and
//!     common SIMD). A larger request (e.g. a page) returns null (clean OOM).
//!
//! Usage in a process binary:
//!   #[global_allocator]
//!   static HEAP: urt::Heap<{ 2 * 1024 * 1024 }> = urt::Heap::new();
// `no_std` for every real build; under `cargo test` the crate links `std` so the
// wrapper proptests can use `std::panic::catch_unwind` + the panic hook (the
// fragmentation-cap leak path's `debug_assert!` witness). Verus verification is
// not a test build, so it still sees `no_std`. Same idiom as `dma-pool`.
#![cfg_attr(not(test), no_std)]
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

pub mod lock;
// The per-process entropy DRBG. Portable plain-Rust arithmetic
// over the Loom-certified `lock::SpinLock`; its per-word byte serialization is
// Verus-verified (`random::u64_to_le`, proven equal to `le_bytes::u64_le`), while
// randomness quality stays off the proof surface (only that serialization and the
// seed decode in `loader` are mechanized).
// Gated off the loom/shuttle model builds only: it holds a process-global
// `static` over the *const* `SpinLock::new()`, which those builds drop, and it
// has no interleaving model of its own to run there (the lock it reuses is
// already modeled). Present for the target, plain `cargo test`, and Miri.
#[cfg(not(any(loom, shuttle)))]
pub mod random;
pub mod slots;
// The verified thread-local key table: a `KeyTable` over the
// verified `slots::SlotAlloc`, plus a plain per-key destructor registry guarded by
// the Loom-certified `lock::SpinLock`. Gated off the loom/shuttle model builds for
// the same reason as `random` — it holds a process-global `static` over the *const*
// `SpinLock::new()` those builds drop, and reuses the already-modeled lock rather
// than adding its own interleaving model. Present for the target, plain `cargo
// test`, Miri, and verus.
#[cfg(not(any(loom, shuttle)))]
pub mod tls;
// Pure thread-stack geometry (host-tested); `thread` (bare-metal) drives it.
pub mod thread_layout;
pub mod time;

// The spawn-lifecycle helper issues syscalls, so it only exists on the
// bare-metal target; `slots` is pure bookkeeping and host-tested.
#[cfg(bare_metal)]
pub mod spawn;

// The in-process thread primitive issues syscalls, so it is
// bare-metal-only like `spawn`. Its host-reachable invariant — the stack-VA / slot
// arithmetic — is host-tested in `thread_layout`; the syscall path is witnessed by
// the QEMU spawn smoke.
#[cfg(bare_metal)]
pub mod thread;

// The `sys::futex` backend. Unlike `lock`, `futex` is not fully
// portable — its park primitive is either the kernel notification (target) or a
// std/loom/shuttle `Mutex`+`Condvar` parker (the model), so it is compiled only for
// the real target and for the host test/loom/shuttle models, and absent on a plain
// no_std host build (verus, plain `cargo build`), where nothing references it.
#[cfg(any(test, bare_metal))]
pub mod futex;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

use lock::SpinLock;

// The verified free-list core (shared with dma-pool). The
// heap's whole allocation algorithm is this proof — see the module doc.
use freelist::FreeList;

/// Free-extent fragmentation cap: the side-stored `FreeList` is a fixed array of
/// this many `(offset, len)` extents. Disclosed MVP bound.
const HEAP_RANGES: usize = 1024;
/// Arena granularity: every allocation is rounded up to this, so carved offsets
/// stay 16-aligned (the minimum alignment every Rust allocation expects).
const MIN_ALIGN: usize = 16;
/// Arena base alignment (the `#[repr(align)]` below). `base.add(off)` meets any
/// `layout.align() <= MAX_ALIGN`; a larger request is refused with null. 128 (the
/// AArch64 cache line) so cache-line-padded std structures allocate — notably
/// `std::sync::mpsc`, whose 128-aligned channel block every libtest run needs.
/// A page-aligned (4096) request is still refused.
const MAX_ALIGN: usize = 128;

#[repr(C, align(128))] // = MAX_ALIGN, so base.add(off) satisfies layout.align() <= 128.
pub struct Heap<const N: usize> {
    /// Pure storage now — handed to callers, never holds allocator metadata.
    mem: UnsafeCell<[u8; N]>,
    /// The verified free list, side-stored. `None` until the first `alloc`
    /// builds it lazily; `None`'s all-zero representation keeps the static in
    /// `.bss` (the loader zeroes it with the RW segment).
    fl: UnsafeCell<Option<FreeList<HEAP_RANGES>>>,
    /// Serializes the `fl` critical section so in-process threads (rev2§5.3) can
    /// allocate concurrently on the one process heap. All-zero (unlocked) keeps
    /// the static in `.bss` alongside `mem`/`fl`.
    lock: SpinLock,
}

// Mutual exclusion by the heap spinlock (`lock.rs`, Loom-certified): `alloc` and
// `dealloc` hold `lock` across the whole `fl` access, so the `UnsafeCell` interior
// is never reached by two threads at once. In-process threads (`thread::spawn`)
// allocate concurrently, so the lock — not a single-threaded-by-construction
// argument — is what keeps that access sound.
unsafe impl<const N: usize> Sync for Heap<N> {}

impl<const N: usize> Heap<N> {
    // loom's / shuttle's `AtomicU32::new` (behind `SpinLock::new`) is not `const`,
    // so a model build drops `const`; the body is identical. The real heap is the
    // `.bss` `static`, where const construction (all-zero = unlocked + empty) is
    // load-bearing — the loader maps+zeroes it, no runtime init.
    #[cfg(all(not(loom), not(shuttle)))]
    pub const fn new() -> Self {
        Heap {
            mem: UnsafeCell::new([0; N]),
            fl: UnsafeCell::new(None),
            lock: SpinLock::new(),
        }
    }

    #[cfg(any(loom, shuttle))]
    pub fn new() -> Self {
        Heap {
            mem: UnsafeCell::new([0; N]),
            fl: UnsafeCell::new(None),
            lock: SpinLock::new(),
        }
    }

    /// Borrow the free list, building the fresh full-arena state on first use.
    /// `FreeList::new(N)` `ensures` the single extent `[0, N)` + `wf` (proven);
    /// `HEAP_RANGES >= 1` satisfies its `N >= 1` requirement. The caller must hold
    /// [`Self::lock`] — `alloc`/`dealloc` are the only callers and both do.
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
        // Hold the lock across the whole free-list access: concurrent allocation
        // by in-process threads (rev2§5.3) is serialized here.
        let _guard = self.lock.lock();
        let fl = self.fl_mut();
        // `align.max(MIN_ALIGN) >= 16 > 0` discharges FreeList::alloc's sole
        // `align > 0` precondition (it computes `off % align`).
        match fl.alloc(need, align.max(MIN_ALIGN)) {
            // The lone raw-pointer formation: `off + need <= N` by alloc's
            // `ensures`, and `off` is `align.max(16)`-aligned, so over a
            // 128-aligned base the address meets `layout.align() <= 128`.
            Some(off) => (self.mem.get() as *mut u8).add(off),
            None => ptr::null_mut(), // OOM / fragmentation cap / no fit
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // Identical rounding to `alloc`, so the extent round-trips exactly.
        let need = layout.size().max(1).next_multiple_of(MIN_ALIGN);
        let off = (p as usize) - (self.mem.get() as usize);
        // Serialize the free-list mutation against concurrent alloc/dealloc.
        let _guard = self.lock.lock();
        let fl = self.fl_mut();
        // Decision 3: at the cap, leak rather than abort a free. FreeList::free's
        // no-merge arm calls insert_at, which would index free[N] (out of bounds)
        // when nfree == N; the guard makes that a safe leak (the freed bytes are
        // just never re-handed-out). A heap must never abort a dealloc, so the
        // witness is a debug_assert, compiled out in release.
        if fl.is_full() {
            debug_assert!(
                false,
                "urt heap: free-list at fragmentation cap; block leaked"
            );
            return;
        }
        // Double-free / overlap guard. `is_allocated` is the verified accessor
        // (in `freelist`); in release, correctness rests on in-process trust
        // plus freelist's verified postconditions — the pointer was handed out
        // by our own `alloc`, so `off+need<=N` and arena membership hold by
        // construction from the matching alloc round-trip. Unlike dma-pool
        // (whose `DmaBuf` is `Copy`, forgeable across pools, justifying a hard
        // assert at a cross-pool boundary), a hard assert here would abort on a
        // drop-unwind path, violating the "dealloc must never abort" invariant
        // (lib.rs:30-31,154). `debug_assert!` is the correct tier: a
        // debug-build witness, compiled out in release.
        debug_assert!(
            fl.is_allocated(off, need),
            "urt heap: double free / overlap"
        );
        fl.free(off, need);
    }

    // realloc is the default GlobalAlloc impl (alloc-new + copy + dealloc-old).
}

// Gated off the model builds: these construct `static H: Heap<_> = Heap::new()`,
// which needs the `const fn new()` that loom/shuttle drop (their `AtomicU32::new`
// is not const). The heap is not itself Loom-modeled — only its `lock` is, in
// `lock::loom_tests`; the arena stays a Miri+proptest seam.
#[cfg(all(test, not(loom), not(shuttle)))]
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
        // align > MAX_ALIGN (128) cannot be met by the arena base → clean OOM,
        // not UB. Below the cap (align <= 128) is exercised by alloc_free_reuse.
        static H: Heap<8192> = Heap::new();
        unsafe {
            let l = Layout::from_size_align(64, 256).unwrap();
            assert!(H.alloc(l).is_null());
            // A request at exactly the cap (128, the AArch64 cache line —
            // `std::sync::mpsc`'s block) still succeeds.
            let ok = Layout::from_size_align(64, 128).unwrap();
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

    // ---- wrapper Miri + proptest tier (mirrors dma-pool). --------
    //
    // The verified `freelist::FreeList` proves the allocation *arithmetic*; these
    // properties prove the wrapper *drivers* — alloc → write the bytes →
    // dealloc/realloc over a real arena through the `UnsafeCell` + `base.add(off)`
    // seam — are sound under randomized sequences, with Miri as the UB oracle (the
    // rev2§6 "everything gets Miri + proptest" baseline the heap had never met).

    use proptest::prelude::*;
    use std::panic;

    /// A per-allocation-unique fill: `31` is invertible mod 256, so distinct
    /// `tag`s never share a byte sequence — an aliasing write from one live block
    /// into another is therefore always caught as a content mismatch.
    fn pattern(tag: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| tag.wrapping_mul(31).wrapping_add(i as u32) as u8)
            .collect()
    }

    /// Run `f`, swallowing any panic message. The fragmentation-cap leak path
    /// (Property 3) fires its `debug_assert!(false, …)` witness in debug builds,
    /// and the default hook would flood the output across hundreds of frees. The
    /// cap witness panics at `dealloc` entry *before* mutating the free list, and
    /// the closures only call `&self` methods of `Heap`, so nothing observes a torn
    /// invariant across the unwind (hence `AssertUnwindSafe` over the `UnsafeCell`).
    fn catch_silent<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let r = panic::catch_unwind(panic::AssertUnwindSafe(f));
        panic::set_hook(prev);
        r
    }

    /// The wrapper rounds every request up to `MIN_ALIGN`, so the carved extent is
    /// `need` bytes (≥ the requested `size`). Filling the *whole* extent — not just
    /// `size` — makes the disjointness oracle tight: any overlap of two carved
    /// extents corrupts a neighbour's pattern even when the `size`-prefixes miss.
    fn need_of(layout: Layout) -> usize {
        layout.size().max(1).next_multiple_of(MIN_ALIGN)
    }

    /// Write `pat` through the raw allocation pointer (a scoped raw copy: no `&mut
    /// [u8]` is held, so two live blocks never alias under Miri's borrow model).
    unsafe fn fill(p: *mut u8, pat: &[u8]) {
        ptr::copy_nonoverlapping(pat.as_ptr(), p, pat.len());
    }

    /// Read a block's `len` bytes back into a fresh `Vec` (a scoped shared view).
    unsafe fn snapshot(p: *const u8, len: usize) -> Vec<u8> {
        core::slice::from_raw_parts(p, len).to_vec()
    }

    /// One live allocation in the proptest model: its pointer, the `Layout` it was
    /// requested with (needed to `dealloc`/`realloc` it), and the unique pattern
    /// filling its whole carved extent (the no-perturbation oracle).
    #[derive(Clone)]
    struct Live {
        p: *mut u8,
        layout: Layout,
        pat: Vec<u8>,
    }

    #[derive(Debug, Clone)]
    enum Op {
        Alloc { size: usize, align_log2: u32 },
        Dealloc { idx: usize },
        Realloc { idx: usize, new_size: usize },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            // align = 1<<log2 ≤ 128 = MAX_ALIGN, so the request is never over-aligned.
            (1usize..=256, 0u32..=7).prop_map(|(size, align_log2)| Op::Alloc { size, align_log2 }),
            any::<usize>().prop_map(|idx| Op::Dealloc { idx }),
            (any::<usize>(), 1usize..=256)
                .prop_map(|(idx, new_size)| Op::Realloc { idx, new_size }),
        ]
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep
        // (mirrors cas/src/file.rs). urt has no blake3, so Miri is fast.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Property 1 — alloc/dealloc/realloc round-trip + disjointness. Every live
        /// block stays in-arena, is `align`-aligned, and its whole carved extent is
        /// disjoint from every other live block (the wrapper corollary of
        /// `lemma_two_allocs_disjoint`); a write to one never perturbs another. A
        /// pointer-arithmetic bug (overlap, off-by-one split, mis-coalesce) breaks
        /// this, and Miri validates the raw `base.add(off)` accesses underneath.
        #[test]
        fn alloc_dealloc_realloc_roundtrip(ops in prop::collection::vec(op_strategy(), 0..64)) {
            const LEN: usize = 4096;
            let h = Heap::<LEN>::new();
            let base = h.mem.get() as *mut u8 as usize;
            let mut live: Vec<Live> = Vec::new();
            let mut tag: u32 = 1;

            for op in ops {
                match op {
                    Op::Alloc { size, align_log2 } => {
                        let align = 1usize << align_log2;
                        let layout = Layout::from_size_align(size, align).unwrap();
                        let need = need_of(layout);
                        let p = unsafe { h.alloc(layout) };
                        if !p.is_null() {
                            let addr = p as usize;
                            prop_assert_eq!(addr % align, 0, "misaligned alloc");
                            prop_assert!(
                                addr >= base && addr + need <= base + LEN,
                                "carved extent out of arena"
                            );
                            // Disjoint from every other live block's carved extent.
                            for other in &live {
                                let oa = other.p as usize;
                                let ob = oa + other.pat.len();
                                prop_assert!(
                                    addr + need <= oa || ob <= addr,
                                    "overlapping live blocks"
                                );
                            }
                            let pat = pattern(tag, need);
                            tag = tag.wrapping_add(1);
                            unsafe { fill(p, &pat) };
                            prop_assert_eq!(unsafe { snapshot(p, need) }, pat.clone());
                            live.push(Live { p, layout, pat });
                        }
                    }
                    Op::Dealloc { idx } => {
                        if !live.is_empty() {
                            let blk = live.remove(idx % live.len());
                            // Still intact at free time (no other op perturbed it).
                            prop_assert_eq!(
                                unsafe { snapshot(blk.p, blk.pat.len()) },
                                blk.pat.clone()
                            );
                            unsafe { h.dealloc(blk.p, blk.layout) };
                        }
                    }
                    Op::Realloc { idx, new_size } => {
                        if !live.is_empty() {
                            let i = idx % live.len();
                            let blk = live[i].clone();
                            prop_assert_eq!(
                                unsafe { snapshot(blk.p, blk.pat.len()) },
                                blk.pat.clone()
                            );
                            let np = unsafe { h.realloc(blk.p, blk.layout, new_size) };
                            if np.is_null() {
                                // The default realloc leaves the old block untouched.
                                prop_assert_eq!(
                                    unsafe { snapshot(blk.p, blk.pat.len()) },
                                    blk.pat.clone()
                                );
                            } else {
                                let nlayout =
                                    Layout::from_size_align(new_size, blk.layout.align()).unwrap();
                                let need = need_of(nlayout);
                                let pat = pattern(tag, need);
                                tag = tag.wrapping_add(1);
                                unsafe { fill(np, &pat) };
                                live[i] = Live { p: np, layout: nlayout, pat };
                            }
                        }
                    }
                }
                // After every op: no live block was perturbed by another's write.
                for blk in &live {
                    prop_assert_eq!(unsafe { snapshot(blk.p, blk.pat.len()) }, blk.pat.clone());
                }
            }
        }

        /// Property 2 — exhaustion + coalescing, end-to-end through the wrapper.
        /// Fill the heap with fixed-size blocks until `alloc` returns null (never a
        /// bad pointer at capacity), free everything, then re-allocate (near) the
        /// full span — two-sided coalescing restored the single extent.
        #[test]
        fn exhaustion_then_coalesce(block in 16usize..=256) {
            const LEN: usize = 4096;
            let h = Heap::<LEN>::new();
            let l = Layout::from_size_align(block, 16).unwrap();
            let mut ps: Vec<*mut u8> = Vec::new();
            loop {
                let p = unsafe { h.alloc(l) };
                if p.is_null() {
                    break;
                }
                ps.push(p);
            }
            prop_assert!(!ps.is_empty());
            // Capacity reached: a further alloc is still null, not a bad pointer.
            let over = unsafe { h.alloc(l) };
            prop_assert!(over.is_null());
            for p in &ps {
                unsafe { h.dealloc(*p, l) };
            }
            // Everything coalesced back: a single near-full allocation fits again.
            let span = Layout::from_size_align(LEN - 64, 16).unwrap();
            let full = unsafe { h.alloc(span) };
            prop_assert!(!full.is_null());
        }
    }

    proptest! {
        // The fragmentation-cap leg drives `nfree` toward HEAP_RANGES (1024), so a
        // single case is a full ~2050-block carve; keep Miri to one case (the
        // deterministic `capped_heap` tests above also exercise the exact at-cap
        // path under Miri), native to a modest sweep for fragmentation-pattern
        // breadth.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 1 } else { 64 },
            ..ProptestConfig::default()
        })]

        /// Property 3 — fragmentation cap never UB. Fully carve a heap into 16-byte
        /// blocks, then free a random subset; each non-adjacent free adds a free
        /// extent, marching `nfree` toward (and possibly to) HEAP_RANGES. Each
        /// `dealloc` either records a new extent or, at the cap, leaks safely —
        /// never UB, never an abort. Miri is the oracle: an unguarded
        /// `FreeList::free` indexing `free[N]` would be an immediate Miri error
        /// here, not a silent pass (the randomized companion to the deterministic
        /// `dealloc_at_cap_*` tests).
        #[test]
        fn fragmentation_cap_never_ub(free_mask in prop::collection::vec(any::<bool>(), 0..2050)) {
            const BLOCKS: usize = 2050;
            let h = Heap::<{ BLOCKS * 16 }>::new();
            let l = Layout::from_size_align(16, 16).unwrap();
            let mut ps: Vec<*mut u8> = Vec::with_capacity(BLOCKS);
            for _ in 0..BLOCKS {
                let p = unsafe { h.alloc(l) };
                prop_assert!(!p.is_null());
                ps.push(p);
            }
            // Free the masked subset (each index at most once → no double-free).
            // Each free is Ok (new extent) or a clean cap-leak (debug witness
            // panics, caught here; release leaks silently) — never UB, never abort.
            for (i, &do_free) in free_mask.iter().enumerate() {
                if i < ps.len() && do_free {
                    let p = ps[i];
                    let _ = catch_silent(|| unsafe { h.dealloc(p, l) });
                }
            }
            // The allocator keeps serving after the churn.
            let _ = unsafe { h.alloc(l) };
        }
    }
}
