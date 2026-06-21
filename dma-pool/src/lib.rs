//! DmaPool — the single place in the system where physical addresses are
//! visible (rev1§2.5).
//!
//! Drivers are written against this crate and never see a PA: buffers are
//! labeled with opaque `DeviceAddress`es (what the device dereferences)
//! and accessed by the CPU only through pool-mediated slices. The backing
//! is abstract: on the OS it is a frame mapping whose PA is read through
//! the `phys-read` rights bit (init grants that bit only to the pool's
//! holder); on the host it is plain memory with a fake device base.
//!
//! When the IO-space object lands (rev1§2.5 committed upgrade), the backing
//! swaps to IOVA-labeled mappings and no driver changes.
//!
//! MVP allocator: first-fit free list with merge-on-free. The pool is
//! bounded and persistent for the driver's lifetime — the steady state
//! needs zero mapping operations per request.
//!
//! **Verified by Verus.** The free-list arithmetic is the self-contained
//! `freelist::FreeList` — extracted from this crate in B11A
//! (`doc/plans/12_b11-detail.md`, Design decision 2) so `dma-pool` and the `urt`
//! heap share one proof. The [`DmaPool`] wrapper that touches the trusted hardware
//! seam (`DmaBacking`, raw-pointer slices, device addresses) stays plain Rust —
//! which is the honest line, since `dma-pool` *is* "the single place PAs are
//! visible", so the PA/backing boundary is exactly the trusted seam. The properties
//! `freelist` proves hold ∀ pool length, request size, and alignment: every
//! `FreeList::alloc` hands out an in-pool, aligned offset whose region was free and
//! is now used, with coverage elsewhere unchanged. **Two live buffers are therefore
//! disjoint ∀** — a corollary demonstrated by `freelist`'s `lemma_two_allocs_disjoint`.
//! `FreeList::free` returns a region to the list (the two-sided adjacency merge),
//! making it allocatable again. The alignment round-up is modular
//! (`off + (align - off%align)%align`) so `start % align == 0` is pure
//! `vstd::arithmetic` and needs no `by (bit_vector)`.

#![cfg_attr(not(any(feature = "std", test)), no_std)]
// Clippy is not a CI gate: the `DmaBacking` seam is a device-size
// trait where `is_empty` is meaningless, and its `unsafe` methods are documented
// with prose contracts rather than a `# Safety` heading. Suppressed, not applied.
#![allow(clippy::len_without_is_empty, clippy::missing_safety_doc)]

// The verified free-list core, extracted to the shared `freelist` crate in B11A
// (Design decision 2). `DmaPool` wraps it; the algorithm proof lives there now.
use freelist::FreeList;

/// Opaque device-visible address — never dereference on the CPU side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAddress(pub u64);

/// The memory behind a pool. Implementors are the privileged boundary:
/// only they ever know real physical addresses.
///
/// Safety contract: `cpu_base` points at `len` writable bytes that stay
/// valid and pinned for the backing's lifetime, and `device_base` is the
/// address under which the device sees byte 0 of that region.
pub unsafe trait DmaBacking {
    fn cpu_base(&self) -> *mut u8;
    fn device_base(&self) -> DeviceAddress;
    fn len(&self) -> usize;
}

/// A buffer carved out of the pool. Offsets are pool-relative; the
/// device address is precomputed so drivers never do PA arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBuf {
    offset: usize,
    len: usize,
    device_addr: DeviceAddress,
}

impl DmaBuf {
    pub fn device_addr(&self) -> DeviceAddress {
        self.device_addr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

const MAX_FREE_RANGES: usize = 64;

pub struct DmaPool<B: DmaBacking> {
    backing: B,
    fl: FreeList<MAX_FREE_RANGES>,
}

impl<B: DmaBacking> DmaPool<B> {
    pub fn new(backing: B) -> DmaPool<B> {
        let len = backing.len();
        DmaPool {
            fl: FreeList::new(len),
            backing,
        }
    }

    pub fn alloc(&mut self, len: usize, align: usize) -> Option<DmaBuf> {
        // Hard backstop for `FreeList::alloc`'s sole `align > 0` precondition (it
        // computes `start % align`, a divide-by-zero on `align == 0`). Ordered before
        // the power-of-two `debug_assert!` so the soundness check wins in release too.
        assert!(align != 0, "dma-pool: zero alignment");
        debug_assert!(align.is_power_of_two());
        let start = self.fl.alloc(len, align)?;
        Some(DmaBuf {
            offset: start,
            len,
            device_addr: DeviceAddress(self.backing.device_base().0 + start as u64),
        })
    }

    pub fn free(&mut self, buf: DmaBuf) {
        // The wrapper is erased plain Rust, so `FreeList::free`'s verified
        // preconditions are not checked statically here — discharge each at runtime.
        // A bad `DmaBuf` (wrong pool, out of extent, double-freed) is a trusted-driver
        // bug (rev1§2.5 isolation TCB), so the backstop is a defined panic; this
        // restores the original `assert!(nfree < MAX_FREE_RANGES)` the audit found
        // demoted to a no-op Verus precondition.
        assert!(buf.len > 0, "dma-pool: zero-length buffer"); // n > 0
                                                              // nfree < N: `!is_full()` plus the always-held `wf` invariant (`nfree <= N`).
        assert!(
            !self.fl.is_full(),
            "dma-pool: free-list fragmentation cap (MAX_FREE_RANGES)"
        );
        // off + n <= len (== spec_len): the same arena predicate `range_ptr` uses, and
        // it establishes `is_allocated`'s `off + n <= spec_len` precondition — so it
        // must precede the `is_allocated` call below.
        assert!(
            buf.offset
                .checked_add(buf.len)
                .is_some_and(|e| e <= self.backing.len()),
            "dma-pool: buffer outside pool arena"
        );
        // !covers: `is_allocated`'s `ensures` makes this exactly `free`'s no-double-free
        // / no-overlap precondition.
        assert!(
            self.fl.is_allocated(buf.offset, buf.len),
            "dma-pool: double free / overlap"
        );
        self.fl.free(buf.offset, buf.len);
    }

    /// The CPU pointer at `buf.offset + offset`, after proving the `len`-byte
    /// access lies wholly inside this pool's backing. This is the one place a raw
    /// pointer into DMA memory is formed (rev1§2.5: the single place PAs are
    /// visible), so the soundness obligation of every `from_raw_parts`/
    /// `read_volatile` below is discharged here, ONCE, for any `DmaBuf` — foreign
    /// or not. `DmaBuf` is `Copy` with private fields, so a buffer carved from a
    /// *different* pool can reach this method; the checked bounds make that a
    /// defined panic (a driver bug inside the isolation TCB, rev1§2.5), never the
    /// out-of-bounds read/write the audit flagged.
    fn range_ptr(&self, buf: &DmaBuf, offset: usize, len: usize) -> *mut u8 {
        // (a) The sub-range must lie within the buffer's own extent. `read`/
        //     `write` get this bound for free from slice indexing on `bytes`/
        //     `bytes_mut`; checking it here gives `read_volatile` the same
        //     buffer-level bound (it forms the pointer directly).
        let sub_end = offset
            .checked_add(len)
            .expect("dma-pool: sub-range length overflows usize");
        assert!(sub_end <= buf.len, "dma-pool: sub-range outside buffer");
        // (b) The accessed range must lie within this pool's backing. This is the
        //     foreign-`DmaBuf` guard: a buffer from a larger pool whose range
        //     overruns this (smaller) pool is rejected here instead of forming an
        //     out-of-bounds slice.
        let abs_end = buf
            .offset
            .checked_add(sub_end)
            .expect("dma-pool: buffer range overflows usize");
        assert!(
            abs_end <= self.backing.len(),
            "dma-pool: buffer range outside pool arena"
        );
        // SAFETY: `buf.offset + offset <= abs_end <= backing.len()`, and the access
        // spans `len` bytes ending at `abs_end`. `cpu_base()` addresses
        // `backing.len()` valid, pinned bytes (the `DmaBacking` contract), so the
        // returned pointer is in-range for `len` bytes — exactly what the
        // `from_raw_parts` / `read_volatile` callers below require.
        unsafe { self.backing.cpu_base().add(buf.offset + offset) }
    }

    /// CPU view of a buffer (or a sub-range of it). The pool mediates all
    /// CPU access; drivers never hold raw pointers into DMA memory.
    pub fn bytes(&self, buf: &DmaBuf) -> &[u8] {
        // Volatile-correctness note: QEMU DMA is host memcpy and
        // cache-coherent (rev1§2.5 real-hardware debt: cache maintenance owed
        // with real hardware, alongside barriers tighter than these).
        let p = self.range_ptr(buf, 0, buf.len);
        unsafe { core::slice::from_raw_parts(p, buf.len) }
    }

    pub fn bytes_mut(&mut self, buf: &DmaBuf) -> &mut [u8] {
        let p = self.range_ptr(buf, 0, buf.len);
        unsafe { core::slice::from_raw_parts_mut(p, buf.len) }
    }

    pub fn write(&mut self, buf: &DmaBuf, offset: usize, data: &[u8]) {
        // Bound inherited from `bytes_mut` (`range_ptr`): the `[offset..]` index
        // panics on overrun of the validated `&mut [u8]`, never UB.
        self.bytes_mut(buf)[offset..offset + data.len()].copy_from_slice(data);
    }

    pub fn read(&self, buf: &DmaBuf, offset: usize, out: &mut [u8]) {
        // Bound inherited from `bytes` (`range_ptr`): the `[offset..]` index
        // panics on overrun of the validated `&[u8]`, never UB.
        out.copy_from_slice(&self.bytes(buf)[offset..offset + out.len()]);
    }

    /// Volatile CPU load of a device-written field — the used-ring index, an
    /// ISR-style flag. Plain `read()`/`bytes()` are non-volatile loads the
    /// optimizer may hoist out of a spin loop (it cannot see the device's
    /// concurrent write), so a poll on them can never observe completion; this
    /// re-reads memory every call. Order the payload the field gates with an
    /// `Acquire` fence on the caller side. (rev1§2.5: the real-hardware
    /// cache-maintenance/barrier debt is separate and tracked there; on the
    /// QEMU target memory is coherent and only the compiler hazard is live.)
    pub fn read_volatile(&self, buf: &DmaBuf, offset: usize, out: &mut [u8]) {
        let base = self.range_ptr(buf, offset, out.len());
        for (i, b) in out.iter_mut().enumerate() {
            *b = unsafe { base.add(i).read_volatile() };
        }
    }
}

/// Host-side backing for tests and host tooling: plain heap memory with
/// a synthetic device base. The "device" (a fake) is handed the same
/// memory out of band.
#[cfg(any(feature = "std", test))]
pub mod host {
    use super::*;
    use std::cell::UnsafeCell;
    use std::rc::Rc;

    #[derive(Clone)]
    pub struct SharedMem(Rc<UnsafeCell<Vec<u8>>>);

    impl SharedMem {
        pub fn new(len: usize) -> SharedMem {
            SharedMem(Rc::new(UnsafeCell::new(vec![0u8; len])))
        }

        /// The fake device's side of the DMA region.
        ///
        /// Safety: test harnesses are single-threaded and never hold
        /// overlapping slices across driver calls — the same discipline
        /// real DMA imposes (device and CPU coordinate via the rings).
        pub unsafe fn raw(&self) -> *mut u8 {
            (*self.0.get()).as_mut_ptr()
        }

        pub fn len(&self) -> usize {
            unsafe { (*self.0.get()).len() }
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }

    pub struct HostBacking {
        pub mem: SharedMem,
        pub device_base: u64,
    }

    unsafe impl DmaBacking for HostBacking {
        fn cpu_base(&self) -> *mut u8 {
            unsafe { self.mem.raw() }
        }

        fn device_base(&self) -> DeviceAddress {
            DeviceAddress(self.device_base)
        }

        fn len(&self) -> usize {
            self.mem.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host::*;
    use super::*;
    use proptest::prelude::*;
    use std::panic;

    fn pool(len: usize) -> DmaPool<HostBacking> {
        DmaPool::new(HostBacking {
            mem: SharedMem::new(len),
            device_base: 0x4000_0000,
        })
    }

    #[test]
    fn alloc_respects_alignment_and_device_base() {
        let mut p = pool(4096);
        let a = p.alloc(10, 1).unwrap();
        let b = p.alloc(64, 256).unwrap();
        assert_eq!(a.device_addr().0, 0x4000_0000);
        assert_eq!(b.device_addr().0 % 256, 0);
        assert!(b.device_addr().0 >= 0x4000_0000 + 10);
    }

    #[test]
    fn exhaustion_and_free_merge() {
        let mut p = pool(1024);
        let a = p.alloc(512, 16).unwrap();
        let b = p.alloc(512, 16).unwrap();
        assert!(p.alloc(1, 1).is_none());
        p.free(a);
        p.free(b);
        // Merged back into one range covering everything.
        let c = p.alloc(1024, 16).unwrap();
        assert_eq!(c.device_addr().0, 0x4000_0000);
    }

    #[test]
    fn data_roundtrip_and_device_view() {
        let mem = SharedMem::new(4096);
        let mut p = DmaPool::new(HostBacking {
            mem: mem.clone(),
            device_base: 0,
        });
        let buf = p.alloc(16, 4).unwrap();
        p.write(&buf, 0, b"dma works fine!!");
        // The "device" sees the same bytes at the device address.
        let dev_view =
            unsafe { core::slice::from_raw_parts(mem.raw().add(buf.device_addr().0 as usize), 16) };
        assert_eq!(dev_view, b"dma works fine!!");
        let mut back = [0u8; 16];
        p.read(&buf, 0, &mut back);
        assert_eq!(&back, b"dma works fine!!");
    }

    // --- B4A: extent-guarded CPU access (rev1§2.5) ---

    /// A 64-byte buffer allocated from an 8192-byte pool at an offset past 256,
    /// so its range lies entirely outside a 256-byte pool — the audit's
    /// cross-pool UB witness ("a `DmaBuf` from a larger pool used against a
    /// smaller pool"). `DmaBuf` is `Copy` and outlives its pool, exactly as the
    /// escaped buffer would.
    fn foreign_buf() -> DmaBuf {
        let mut big = pool(8192);
        let _pad = big.alloc(1024, 1).unwrap(); // push the next alloc to offset 1024
        let far = big.alloc(64, 1).unwrap();
        assert!(far.offset > 256 && far.offset + far.len > 256);
        far
    }

    #[test]
    #[should_panic(expected = "outside pool arena")]
    fn cross_pool_bytes_panics() {
        let small = pool(256);
        let _ = small.bytes(&foreign_buf());
    }

    #[test]
    #[should_panic(expected = "outside pool arena")]
    fn cross_pool_bytes_mut_panics() {
        let mut small = pool(256);
        let _ = small.bytes_mut(&foreign_buf());
    }

    #[test]
    #[should_panic(expected = "outside pool arena")]
    fn cross_pool_read_volatile_panics() {
        let small = pool(256);
        let mut out = [0u8; 4];
        small.read_volatile(&foreign_buf(), 0, &mut out);
    }

    #[test]
    fn subrange_exact_boundary_roundtrips() {
        let mut p = pool(4096);
        let buf = p.alloc(16, 1).unwrap();
        p.write(&buf, 0, b"0123456789abcdef"); // offset 0 + len 16 == buf.len
        let mut tail = [0u8; 8];
        p.read(&buf, 8, &mut tail); // 8 + 8 == 16: exact end
        assert_eq!(&tail, b"89abcdef");
        let mut v = [0u8; 4];
        p.read_volatile(&buf, 12, &mut v); // 12 + 4 == 16: exact end
        assert_eq!(&v, b"cdef");
    }

    #[test]
    #[should_panic(expected = "outside buffer")]
    fn read_volatile_subrange_overrun_panics() {
        let mut p = pool(4096);
        let buf = p.alloc(16, 1).unwrap();
        // 12 + 8 = 20 > buf.len (16) but well within the 4096-byte pool, so only
        // range_ptr's buffer-level bound catches it — not the pool bound.
        let mut out = [0u8; 8];
        p.read_volatile(&buf, 12, &mut out);
    }

    #[test]
    #[should_panic]
    fn read_subrange_overrun_panics() {
        let mut p = pool(4096);
        let buf = p.alloc(16, 1).unwrap();
        let mut out = [0u8; 8];
        p.read(&buf, 12, &mut out); // 12 + 8 > 16: slice-index panic via bytes()
    }

    #[test]
    #[should_panic]
    fn write_subrange_overrun_panics() {
        let mut p = pool(4096);
        let buf = p.alloc(16, 1).unwrap();
        p.write(&buf, 12, b"00000000"); // 12 + 8 > 16: slice-index panic via bytes_mut()
    }

    // --- B4B: MAX_FREE_RANGES backstop + discharged FreeList preconditions ---

    #[test]
    #[should_panic(expected = "fragmentation cap")]
    fn full_list_backstop_panics() {
        // 130 one-byte buffers fill the pool exactly (offsets 0..130).
        let mut p = pool(130);
        let bufs: Vec<DmaBuf> = (0..130).map(|_| p.alloc(1, 1).unwrap()).collect();
        // Free every even offset below 127 -> 64 non-adjacent free extents (the
        // odd-offset buffers between them stay allocated, so nothing merges),
        // driving nfree to MAX_FREE_RANGES (64).
        for b in &bufs {
            if b.offset < 127 && b.offset % 2 == 0 {
                p.free(*b);
            }
        }
        // Offset 128 has allocated neighbours (127, 129), so freeing it cannot merge:
        // it would be the 65th extent. Pre-B4B this was a raw self.free[N]
        // index-out-of-bounds; the restored backstop must panic here instead.
        let victim = *bufs.iter().find(|b| b.offset == 128).unwrap();
        p.free(victim);
    }

    #[test]
    #[should_panic(expected = "double free")]
    fn double_free_panics() {
        let mut p = pool(4096);
        let b = p.alloc(64, 64).unwrap();
        p.free(b);
        // `DmaBuf` is `Copy`; the region is now free, so is_allocated is false and the
        // no-double-free guard fires (closing the overlap -> aliasing &mut UB chain).
        p.free(b);
    }

    #[test]
    fn live_free_is_reallocatable() {
        let mut p = pool(4096);
        let a = p.alloc(128, 16).unwrap();
        let off = a.offset;
        p.free(a); // a single free of a live buffer succeeds ...
        let b = p.alloc(128, 16).unwrap();
        assert_eq!(b.offset, off); // ... and the space is re-allocatable.
    }

    #[test]
    #[should_panic(expected = "zero-length buffer")]
    fn zero_length_free_panics() {
        let mut p = pool(4096);
        let bogus = DmaBuf {
            offset: 0,
            len: 0,
            device_addr: DeviceAddress(0x4000_0000),
        };
        p.free(bogus);
    }

    #[test]
    #[should_panic(expected = "zero alignment")]
    fn zero_alignment_alloc_panics() {
        // align == 0 would divide-by-zero in FreeList::alloc; the hard assert (before
        // the power-of-two debug_assert) catches it in release too.
        let mut p = pool(4096);
        let _ = p.alloc(16, 0);
    }

    // (`accessor_sanity` — the lone FreeList-only test — moved to the `freelist`
    // crate with the proof in B11A.)

    // --- B4C: wrapper proptest tier + Miri UB oracle (rev1§6) ---
    //
    // The verified `FreeList` proves the arithmetic; these properties prove the
    // wrapper drivers actually use — alloc -> bytes/read/write/read_volatile ->
    // free, over a real `HostBacking` — is sound under randomized sequences, with
    // Miri as the oracle for the raw slices `range_ptr` forms. Kept inline (not a
    // `tests/*.rs` file) so the private `DmaBuf.offset` and the `test`-cfg
    // `HostBacking` are both in scope, and `cargo +nightly miri test -p dma-pool`
    // covers it directly. See doc/plans/4_b4-detail.md §B4C.

    const DEVICE_BASE: u64 = 0x4000_0000;

    /// Run `f`, swallowing any panic message — the panic paths (cross-pool,
    /// fragmentation cap) are exercised hundreds of times and the default hook
    /// would flood the output. `DmaPool<HostBacking>` is not `UnwindSafe` (it
    /// holds `Rc<UnsafeCell<..>>`), so we assert it: every closure below either
    /// only reads, or panics in `DmaPool::{free,bytes,..}` *before* any mutation,
    /// so no observer sees a torn invariant across the unwind.
    fn catch_silent<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let r = panic::catch_unwind(panic::AssertUnwindSafe(f));
        panic::set_hook(prev);
        r
    }

    /// A per-allocation-unique fill: `31` is invertible mod 256, so distinct
    /// `tag`s (the monotonic alloc counter, <= 64 per sequence) never share a byte
    /// sequence — an aliasing write from one live buffer into another is therefore
    /// always caught as a content mismatch.
    fn pattern(tag: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| tag.wrapping_mul(31).wrapping_add(i as u32) as u8)
            .collect()
    }

    #[derive(Debug, Clone)]
    enum Op {
        Alloc { len: usize, align_log2: u32 },
        Free { idx: usize },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            (1usize..=256, 0u32..=8).prop_map(|(len, align_log2)| Op::Alloc { len, align_log2 }),
            any::<usize>().prop_map(|idx| Op::Free { idx }),
        ]
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep
        // (mirrors cas/src/file.rs). dma-pool has no blake3, so Miri is fast.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Property 1 — alloc/free/access round-trip + disjointness. Every live
        /// buffer stays in-pool, carries its own (aligned) device address, is
        /// disjoint from every other live buffer (the wrapper corollary of
        /// `lemma_two_allocs_disjoint`), and a write to one never perturbs another.
        #[test]
        fn alloc_free_access_roundtrip(ops in prop::collection::vec(op_strategy(), 0..64)) {
            const LEN: usize = 4096;
            let mut p = DmaPool::new(HostBacking {
                mem: SharedMem::new(LEN),
                device_base: DEVICE_BASE,
            });
            // (buf, expected-bytes) for each currently-live buffer.
            let mut live: Vec<(DmaBuf, Vec<u8>)> = Vec::new();
            let mut tag: u32 = 1;

            for op in ops {
                match op {
                    Op::Alloc { len, align_log2 } => {
                        let align = 1usize << align_log2;
                        if let Some(buf) = p.alloc(len, align) {
                            prop_assert_eq!(buf.len, len);
                            prop_assert!(buf.offset + buf.len <= LEN);
                            prop_assert_eq!(buf.device_addr().0, DEVICE_BASE + buf.offset as u64);
                            prop_assert_eq!(buf.device_addr().0 % align as u64, 0u64);
                            // Disjoint from every other live buffer.
                            for (other, _) in &live {
                                prop_assert!(
                                    buf.offset + buf.len <= other.offset
                                        || other.offset + other.len <= buf.offset,
                                    "overlapping live buffers: {:?} vs {:?}",
                                    buf,
                                    other
                                );
                            }
                            // Write a unique pattern; read it back three ways.
                            let pat = pattern(tag, len);
                            tag = tag.wrapping_add(1);
                            p.write(&buf, 0, &pat);
                            prop_assert_eq!(p.bytes(&buf), &pat[..]);
                            let mut back = vec![0u8; len];
                            p.read(&buf, 0, &mut back);
                            prop_assert_eq!(&back, &pat);
                            let vlen = len.min(16);
                            let mut vbuf = vec![0u8; vlen];
                            p.read_volatile(&buf, 0, &mut vbuf);
                            prop_assert_eq!(&vbuf[..], &pat[..vlen]);
                            live.push((buf, pat));
                        }
                    }
                    Op::Free { idx } => {
                        if !live.is_empty() {
                            let (buf, pat) = live.remove(idx % live.len());
                            prop_assert_eq!(p.bytes(&buf), &pat[..]); // still intact
                            p.free(buf);
                        }
                    }
                }
                // After every op: no live buffer was perturbed by another's write.
                for (buf, pat) in &live {
                    prop_assert_eq!(p.bytes(buf), &pat[..]);
                }
            }
        }

        /// Property 2 — cross-pool safety, never UB. A buffer carved from pool A,
        /// applied to a differently-sized pool B, either round-trips (in-bounds)
        /// or panics — never an out-of-bounds slice. Miri is the oracle on the
        /// in-bounds-foreign path: reverting `range_ptr`'s `abs_end <=
        /// backing.len()` bound would make `b.bytes(&buf)` form a slice past B's
        /// backing — an immediate Miri error right here. So this guards a real
        /// hole, not a tautology (the B4 plan's oracle-sanity, documented rather
        /// than committed as an unsound variant that would break the Miri sweep).
        #[test]
        fn cross_pool_never_ub(
            a_len in 64usize..=4096,
            b_len in 64usize..=4096,
            pad in 0usize..4096,
            req_len in 1usize..=256,
            align_log2 in 0u32..=8,
        ) {
            let mut a = DmaPool::new(HostBacking {
                mem: SharedMem::new(a_len),
                device_base: DEVICE_BASE,
            });
            let b = DmaPool::new(HostBacking {
                mem: SharedMem::new(b_len),
                device_base: 0x8000_0000,
            });
            // Push the carve to a varied offset inside A, so foreign buffers land
            // at many positions relative to B's end (some past it, some within).
            let pad = pad % a_len; // a_len >= 64
            if pad > 0 {
                let _ = a.alloc(pad, 1);
            }
            let buf = match a.alloc(req_len, 1usize << align_log2) {
                Some(buf) => buf,
                None => return Ok(()),
            };
            // Independent oracle: in-bounds for B iff the whole extent fits in B.
            let in_bounds = buf.offset.checked_add(buf.len).is_some_and(|e| e <= b_len);
            if in_bounds {
                // Miri validates the foreign-but-in-bounds slice formation + reads.
                let s = b.bytes(&buf);
                prop_assert_eq!(s.len(), buf.len);
                let _ = s.iter().fold(0u8, |acc, &x| acc ^ x); // touch every byte
                let mut out = vec![0u8; buf.len];
                b.read_volatile(&buf, 0, &mut out);
            } else {
                let res = catch_silent(|| b.bytes(&buf).len());
                prop_assert!(res.is_err());
            }
        }

        /// Property 3 — fragmentation backstop never UB. Fragment a small pool by
        /// freeing a chosen subset of 1-byte buffers; each `free` either succeeds
        /// or panics cleanly. Under Miri this confirms the restored
        /// `assert!(!is_full())` fires before any `self.free[N]` out-of-bounds
        /// index (the randomized companion to `full_list_backstop_panics`).
        #[test]
        fn fragmentation_backstop_never_ub(free_mask in prop::collection::vec(any::<bool>(), 0..200)) {
            const LEN: usize = 200;
            let mut p = DmaPool::new(HostBacking {
                mem: SharedMem::new(LEN),
                device_base: DEVICE_BASE,
            });
            // Fill the pool with 1-byte buffers (offsets 0..LEN).
            let bufs: Vec<DmaBuf> = (0..LEN).map_while(|_| p.alloc(1, 1)).collect();
            // Free the masked subset; each non-adjacent free adds a free extent,
            // marching nfree toward (and possibly past) MAX_FREE_RANGES. Each index
            // is freed at most once, so no double-free — only the cap can panic.
            for (i, &do_free) in free_mask.iter().enumerate() {
                if i < bufs.len() && do_free {
                    let buf = bufs[i];
                    // Ok (carved a new extent) or Err (clean cap panic) — never UB.
                    let _ = catch_silent(|| p.free(buf));
                }
            }
        }
    }
}
