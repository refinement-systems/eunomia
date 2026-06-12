//! A free-list over a contiguous block of a process's own cspace slots.
//!
//! A parent that spawns children needs to recycle the cspace slots that
//! held the children's object caps. The subtlety the §5.1 reclaim loop
//! turns on: those slots become free *because `cap_revoke` nulled them* —
//! this allocator's bookkeeping must therefore agree with kernel behaviour,
//! never run ahead of it. Free a range only after the revoke that emptied
//! the kernel slots has returned; then `alloc`/`alloc_range` may hand the
//! same indices out again (the "slot reusable after revoke" property, which
//! a spawn/reclaim loop on the OS witnesses and the unit tests below pin on
//! the host).
//!
//! Plain bitmap, no kernel calls — pure userspace bookkeeping, host-tested.
//! The window holds up to `WORDS * 64` slots; `cap` (≤ that) is the actual
//! number in play, so the same `WORDS` covers any cspace that fits.

/// A slot allocator over `[base, base + cap)`, one bit per slot.
pub struct SlotAlloc<const WORDS: usize> {
    base: u32,
    cap: usize,
    /// bit i set ⇒ slot `base + i` is free.
    free: [u64; WORDS],
}

impl<const WORDS: usize> SlotAlloc<WORDS> {
    /// All of `[base, base + cap)` free. `cap` must fit the bitmap.
    pub fn new(base: u32, cap: usize) -> Self {
        assert!(cap <= WORDS * 64, "cap exceeds the bitmap width");
        let mut a = SlotAlloc { base, cap, free: [0u64; WORDS] };
        for i in 0..cap {
            a.set(i, true);
        }
        a
    }

    fn is_free(&self, i: usize) -> bool {
        self.free[i / 64] & (1u64 << (i % 64)) != 0
    }

    fn set(&mut self, i: usize, free: bool) {
        let (w, b) = (i / 64, 1u64 << (i % 64));
        if free {
            self.free[w] |= b;
        } else {
            self.free[w] &= !b;
        }
    }

    /// One free slot, marked used, or None when the window is full.
    pub fn alloc(&mut self) -> Option<u32> {
        (0..self.cap).find(|&i| self.is_free(i)).map(|i| {
            self.set(i, false);
            self.base + i as u32
        })
    }

    /// `n` contiguous free slots, marked used; returns the first index.
    /// Spawn wants a contiguous block (the loader lays object caps out by
    /// `base + k`), so this is the primitive, not repeated `alloc`.
    pub fn alloc_range(&mut self, n: u32) -> Option<u32> {
        let n = n as usize;
        if n == 0 || n > self.cap {
            return None;
        }
        (0..=self.cap - n)
            .find(|&start| (start..start + n).all(|i| self.is_free(i)))
            .map(|start| {
                for i in start..start + n {
                    self.set(i, false);
                }
                self.base + start as u32
            })
    }

    /// Return one slot to the free list. Caller's contract: the kernel slot
    /// is already empty (revoke/delete ran). Double-free is a bug and panics
    /// in debug — it would hand a live slot out twice.
    pub fn free(&mut self, slot: u32) {
        let i = (slot - self.base) as usize;
        debug_assert!(i < self.cap, "slot {slot} outside the allocator window");
        debug_assert!(!self.is_free(i), "double free of slot {slot}");
        self.set(i, true);
    }

    /// Return `[first, first + n)` (the counterpart of `alloc_range`).
    pub fn free_range(&mut self, first: u32, n: u32) {
        for k in 0..n {
            self.free(first + k);
        }
    }

    /// Free-slot count — for tests and the on-OS leak assertion.
    pub fn available(&self) -> usize {
        (0..self.cap).filter(|&i| self.is_free(i)).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_free_reuse_same_slots() {
        let mut a = SlotAlloc::<1>::new(8, 32);
        assert_eq!(a.available(), 32);
        let r = a.alloc_range(5).unwrap();
        assert_eq!(r, 8);
        assert_eq!(a.available(), 27);
        // Freeing the range returns exactly those indices to the pool, so
        // the next identical request reuses them — the post-revoke reuse
        // the spawn loop depends on.
        a.free_range(r, 5);
        assert_eq!(a.available(), 32);
        let r2 = a.alloc_range(5).unwrap();
        assert_eq!(r2, r);
    }

    #[test]
    fn contiguous_search_skips_holes() {
        let mut a = SlotAlloc::<1>::new(0, 16);
        // Carve a hole: take 0..4 and 4..8, free 6..8 leaves [6,16) free
        // but [4,6) used — an 11-wide request fails, 10 lands at 6.
        let _ = a.alloc_range(4).unwrap(); // 0..4
        let mid = a.alloc_range(4).unwrap(); // 4..8
        a.free_range(mid + 2, 2); // free 6..8
        assert!(a.alloc_range(11).is_none());
        assert_eq!(a.alloc_range(10).unwrap(), 6);
    }

    #[test]
    fn exhaustion_returns_none() {
        let mut a = SlotAlloc::<1>::new(100, 4);
        assert_eq!(a.alloc_range(4).unwrap(), 100);
        assert!(a.alloc().is_none());
        assert!(a.alloc_range(1).is_none());
        a.free(101);
        assert_eq!(a.alloc().unwrap(), 101);
    }

    #[test]
    fn spans_multiple_words() {
        // 100 slots needs two words; allocation must cross the 64 boundary.
        let mut a = SlotAlloc::<2>::new(0, 100);
        assert_eq!(a.available(), 100);
        let r = a.alloc_range(100).unwrap();
        assert_eq!(r, 0);
        assert_eq!(a.available(), 0);
        a.free_range(60, 10); // straddles word 0/1
        assert_eq!(a.alloc_range(10).unwrap(), 60);
    }

    #[test]
    #[should_panic]
    fn double_free_panics() {
        let mut a = SlotAlloc::<1>::new(0, 8);
        let s = a.alloc().unwrap();
        a.free(s);
        a.free(s);
    }
}
