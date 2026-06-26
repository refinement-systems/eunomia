//! A free-list over a contiguous block of a process's own cspace slots.
//!
//! A parent that spawns children needs to recycle the cspace slots that
//! held the children's object caps. The subtlety the rev2§5.1 reclaim loop
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
//!
//! **Verified by Verus.** The properties hold ∀ `cap` and `WORDS`: every
//! [`SlotAlloc::alloc`] hands out an in-window slot that
//! was free and is now used (so successive allocations are distinct — a corollary
//! of the modular contract, not a bounded drain loop); exhaustion is exact
//! ([`SlotAlloc::alloc`] returns `None` ⟺ no slot is free); [`SlotAlloc::free`]
//! makes a slot free again (the post-revoke reuse), with the **double-free
//! precondition** (`!is_free_spec`) a contract-checked impossibility; and
//! [`SlotAlloc::alloc_range`] hands out a contiguous in-window run that was free
//! and is now used. The bitmap bit-test `free[i/64] & (1<<(i%64))` is related to
//! the per-slot `SlotAlloc::is_free_spec` predicate by `by (bit_vector)` frame
//! lemmas, and the `.find().map()` combinators are restructured into explicit
//! invariant-carrying loops (the shape kcore's `aspace` walk-loops already take).
use vstd::prelude::*;

verus! {

/// A slot allocator over `[base, base + cap)`, one bit per slot.
pub struct SlotAlloc<const WORDS: usize> {
    base: u32,
    cap: usize,
    /// bit i set ⇒ slot `base + i` is free.
    free: [u64; WORDS],
}

impl<const WORDS: usize> SlotAlloc<WORDS> {
    /// Well-formedness: the bitmap is wide enough for `cap`, and the whole
    /// window fits the `u32` slot-id space (so `base + i` never overflows). The
    /// invariant `new` establishes and every op preserves. `closed` because the
    /// body reads the private `base`/`cap`/`free` fields — opaque outside the
    /// module, with the body visible to in-module proofs.
    pub closed spec fn wf(self) -> bool {
        &&& self.free@.len() == WORDS
        &&& self.cap <= WORDS * 64
        &&& self.base as int + self.cap as int <= u32::MAX as int
    }

    /// Slot `i` (relative to `base`) is free ⇔ its bitmap bit is set. Meaningful
    /// for `0 <= i < cap` (then `i / 64 < WORDS`, so the word index is in range).
    /// `closed` for the same reason as [`SlotAlloc::wf`].
    pub closed spec fn is_free_spec(self, i: int) -> bool {
        self.free@[i / 64] & (1u64 << ((i % 64) as u64)) != 0
    }

    /// The window base (first slot id). A `closed` accessor so the public
    /// contracts can speak of the slot-id arithmetic without exposing the field.
    pub closed spec fn spec_base(self) -> u32 {
        self.base
    }

    /// The window width (slot count), as a ghost int. `closed`, as [`SlotAlloc::spec_base`].
    pub closed spec fn spec_cap(self) -> int {
        self.cap as int
    }

    /// All of `[base, base + cap)` free. `cap` must fit the bitmap and the
    /// window the `u32` slot-id space.
    pub fn new(base: u32, cap: usize) -> (a: SlotAlloc<WORDS>)
        requires
            cap <= WORDS * 64,
            base as int + cap as int <= u32::MAX as int,
        ensures
            a.wf(),
            a.spec_base() == base,
            a.spec_cap() == cap as int,
            forall|j: int| 0 <= j < cap ==> a.is_free_spec(j),
    {
        broadcast use vstd::array::group_array_axioms;

        let mut a = SlotAlloc { base, cap, free: [0u64;WORDS] };
        let mut i = 0;
        while i < cap
            invariant
                a.wf(),
                a.base == base,
                a.cap == cap,
                i <= cap,
                forall|j: int| 0 <= j < i ==> a.is_free_spec(j),
            decreases cap - i,
        {
            a.set(i, true);
            i += 1;
        }
        a
    }

    fn is_free(&self, i: usize) -> (r: bool)
        requires
            self.wf(),
            i < self.cap,
        ensures
            r == self.is_free_spec(i as int),
    {
        broadcast use vstd::array::group_array_axioms;

        proof {
            lemma_index_split(i, WORDS);
        }
        let wi: usize = i / 64;
        let bi: u64 = (i % 64) as u64;
        self.free[wi] & (1u64 << bi) != 0
    }

    fn set(&mut self, i: usize, free: bool)
        requires
            old(self).wf(),
            i < old(self).cap,
        ensures
            final(self).wf(),
            final(self).base == old(self).base,
            final(self).cap == old(self).cap,
            final(self).is_free_spec(i as int) == free,
            forall|j: int|
                0 <= j < old(self).cap && j != i as int ==> final(self).is_free_spec(j) == old(
                    self,
                ).is_free_spec(j),
    {
        broadcast use vstd::array::group_array_axioms;

        proof {
            lemma_index_split(i, WORDS);
        }
        let w: usize = i / 64;
        let bi: u64 = (i % 64) as u64;
        let b: u64 = 1u64 << bi;
        let old_word = self.free[w];
        if free {
            self.free[w] = old_word | b;
        } else {
            self.free[w] = old_word & !b;
        }
        proof {
            // The written word now reads `free` at bit `bi`, and every other
            // bit of that word is untouched; words other than `w` are untouched.
            lemma_set_bit(old_word, bi);
            assert forall|j: int| 0 <= j < old(self).cap && j != i as int implies self.is_free_spec(
                j,
            ) == old(self).is_free_spec(j) by {
                lemma_index_split(j as usize, WORDS);
                if j / 64 == w as int {
                    // same word, different bit position
                    lemma_bit_other(old_word, bi, (j % 64) as u64);
                }  // else: a different word, untouched by the assignment

            }
        }
    }

    /// One free slot, marked used, or None when the window is full.
    pub fn alloc(&mut self) -> (r: Option<u32>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).spec_base() == old(self).spec_base(),
            final(self).spec_cap() == old(self).spec_cap(),
            match r {
                Some(s) => {
                    &&& old(self).spec_base() <= s
                    &&& (s - old(self).spec_base()) < old(self).spec_cap()
                    &&& old(self).is_free_spec((s - old(self).spec_base()) as int)
                    &&& !final(self).is_free_spec((s - old(self).spec_base()) as int)
                    &&& forall|j: int|
                        0 <= j < old(self).spec_cap() && j != (s - old(self).spec_base()) as int
                            ==> final(self).is_free_spec(j) == old(self).is_free_spec(j)
                },
                None => {
                    &&& forall|j: int| 0 <= j < old(self).spec_cap() ==> !old(self).is_free_spec(j)
                    &&& forall|j: int|
                        0 <= j < old(self).spec_cap() ==> final(self).is_free_spec(j) == old(
                            self,
                        ).is_free_spec(j)
                },
            },
    {
        let mut i: usize = 0;
        while i < self.cap
            invariant
                self.wf(),
                self.base == old(self).base,
                self.cap == old(self).cap,
                self.free@ == old(self).free@,
                i <= self.cap,
                forall|j: int| 0 <= j < i ==> !self.is_free_spec(j),
            decreases self.cap - i,
        {
            if self.is_free(i) {
                self.set(i, false);
                return Some(self.base + i as u32);
            }
            i += 1;
        }
        // Exhausted: the scan found no free slot, and it never wrote `self`
        // (free@ unchanged), so `is_free_spec` agrees with entry everywhere.
        proof {
            assert(self.free@ == old(self).free@);
            assert(i == self.cap);
            assert forall|j: int| 0 <= j < self.cap implies (!old(self).is_free_spec(j)
                && self.is_free_spec(j) == old(self).is_free_spec(j)) by {
                assert(self.free@[j / 64] == old(self).free@[j / 64]);
                assert(!self.is_free_spec(j));
            }
        }
        None
    }

    /// `n` contiguous free slots, marked used; returns the first index.
    /// Spawn wants a contiguous block (the loader lays object caps out by
    /// `base + k`), so this is the primitive, not repeated `alloc`.
    pub fn alloc_range(&mut self, n: u32) -> (r: Option<u32>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            final(self).spec_base() == old(self).spec_base(),
            final(self).spec_cap() == old(self).spec_cap(),
            match r {
                Some(start) => {
                    &&& n > 0
                    &&& old(self).spec_base() <= start
                    &&& (start - old(self).spec_base()) + n <= old(self).spec_cap()
                    &&& forall|j: int|
                        (start - old(self).spec_base()) <= j < (start - old(self).spec_base()) + n
                            ==> old(self).is_free_spec(j)
                    &&& forall|j: int|
                        (start - old(self).spec_base()) <= j < (start - old(self).spec_base()) + n
                            ==> !final(self).is_free_spec(j)
                    &&& forall|j: int|
                        0 <= j < old(self).spec_cap() && !((start - old(self).spec_base()) <= j < (
                        start - old(self).spec_base()) + n) ==> final(self).is_free_spec(j) == old(
                            self,
                        ).is_free_spec(j)
                },
                None => forall|j: int|
                    0 <= j < old(self).spec_cap() ==> final(self).is_free_spec(j) == old(
                        self,
                    ).is_free_spec(j),
            },
    {
        let n_us = n as usize;
        if n == 0 || n_us > self.cap {
            return None;
        }
        let mut start: usize = 0;
        while start <= self.cap - n_us
            invariant
                self.wf(),
                self.base == old(self).base,
                self.cap == old(self).cap,
                self.free@ == old(self).free@,
                1 <= n_us <= self.cap,
                n_us == n,
            decreases self.cap - start,
        {
            let mut k: usize = 0;
            while k < n_us && self.is_free(start + k)
                invariant
                    self.wf(),
                    self.base == old(self).base,
                    self.cap == old(self).cap,
                    self.free@ == old(self).free@,
                    1 <= n_us <= self.cap,
                    n_us == n,
                    start + n_us <= self.cap,
                    0 <= k <= n_us,
                    forall|j: int| start <= j < start + k ==> self.is_free_spec(j),
                decreases n_us - k,
            {
                k += 1;
            }
            if k == n_us {
                // [start, start+n) are all free; mark them used. The scan never
                // wrote `self` (free@ unchanged), so `is_free_spec` agrees with
                // entry everywhere — pin that, and that the run was free at entry.
                proof {
                    assert forall|j: int| 0 <= j < self.cap implies self.is_free_spec(j) == old(
                        self,
                    ).is_free_spec(j) by {}
                    assert forall|j: int| start <= j < start + n_us implies old(self).is_free_spec(
                        j,
                    ) by {}
                }
                let mut m: usize = 0;
                while m < n_us
                    invariant
                        self.wf(),
                        self.base == old(self).base,
                        self.cap == old(self).cap,
                        1 <= n_us <= self.cap,
                        n_us == n,
                        start + n_us <= self.cap,
                        0 <= m <= n_us,
                        forall|j: int| start <= j < start + n_us ==> old(self).is_free_spec(j),
                        forall|j: int| start <= j < start + m ==> !self.is_free_spec(j),
                        forall|j: int|
                            0 <= j < self.cap && !(start <= j < start + n_us) ==> self.is_free_spec(
                                j,
                            ) == old(self).is_free_spec(j),
                    decreases n_us - m,
                {
                    self.set(start + m, false);
                    m += 1;
                }
                // The returned id is `base + start`; relative index is `start`.
                proof {
                    assert(self.base as int + self.cap as int <= u32::MAX as int);
                    assert(start < self.cap);
                }
                return Some(self.base + start as u32);
            }
            start += 1;
        }
        None
    }

    /// Return one slot to the free list. Caller's contract: the kernel slot
    /// is already empty (revoke/delete ran) and the bookkeeping slot is
    /// currently allocated. A double-free (`!is_free_spec` violated) is a
    /// contract-checked impossibility — it would hand a live slot out twice;
    /// `SlotAlloc::debug_check_free` keeps the runtime (debug) panic witness.
    pub fn free(&mut self, slot: u32)
        requires
            old(self).wf(),
            old(self).spec_base() <= slot,
            (slot - old(self).spec_base()) < old(self).spec_cap(),
            !old(self).is_free_spec((slot - old(self).spec_base()) as int),
        ensures
            final(self).wf(),
            final(self).spec_base() == old(self).spec_base(),
            final(self).spec_cap() == old(self).spec_cap(),
            final(self).is_free_spec((slot - old(self).spec_base()) as int),
            forall|j: int|
                0 <= j < old(self).spec_cap() && j != (slot - old(self).spec_base()) as int
                    ==> final(self).is_free_spec(j) == old(self).is_free_spec(j),
    {
        let i = (slot - self.base) as usize;
        self.debug_check_free(i);
        self.set(i, true);
    }

    /// Runtime-only double-free guard (debug builds): a slot returned to the
    /// free list must be currently allocated. `external_body` so Verus does not
    /// see the `debug_assert!` (it lowers to a `panic!`, which Verus forbids in
    /// exec code); the *static* guarantee is [`SlotAlloc::free`]'s `!is_free_spec`
    /// precondition. The host `double_free_panics` test pins this runtime witness.
    #[verifier::external_body]
    fn debug_check_free(&self, i: usize) {
        debug_assert!(i < self.cap, "slot index {i} outside the allocator window");
        debug_assert!(!self.is_free(i), "double free of slot index {i}");
    }

    /// Return `[first, first + n)` (the counterpart of `alloc_range`).
    pub fn free_range(&mut self, first: u32, n: u32)
        requires
            old(self).wf(),
            old(self).spec_base() <= first,
            (first - old(self).spec_base()) + n <= old(self).spec_cap(),
            forall|j: int|
                (first - old(self).spec_base()) <= j < (first - old(self).spec_base()) + n ==> !old(
                    self,
                ).is_free_spec(j),
        ensures
            final(self).wf(),
            final(self).spec_base() == old(self).spec_base(),
            final(self).spec_cap() == old(self).spec_cap(),
    {
        let mut k: u32 = 0;
        while k < n
            invariant
                self.wf(),
                self.base == old(self).base,
                self.cap == old(self).cap,
                k <= n,
                old(self).base <= first,
                (first - old(self).base) + n <= self.cap,
                forall|j: int|
                    (first - old(self).base) + k <= j < (first - old(self).base) + n
                        ==> !self.is_free_spec(j),
            decreases n - k,
        {
            self.free(first + k);
            k += 1;
        }
    }
}

// ── Bit-frame lemmas (the bitmap ↔ free-set bridge, `by (bit_vector)`) ──
/// Splitting an in-range slot index: the word index is in bounds and the bit
/// position is `< 64`. (`i < WORDS*64 ⟹ i/64 < WORDS`, and `i%64 < 64`.)
proof fn lemma_index_split(i: usize, words: usize)
    requires
        i < words * 64,
    ensures
        i / 64 < words,
        i % 64 < 64,
{
    assert(i / 64 < words) by (nonlinear_arith)
        requires
            i < words * 64,
    ;
}

/// Writing bit `k` of `x` reads back set when ORed in, clear when masked out.
proof fn lemma_set_bit(x: u64, k: u64)
    by (bit_vector)
    requires
        k < 64,
    ensures
        (x | (1u64 << k)) & (1u64 << k) != 0,
        (x & !(1u64 << k)) & (1u64 << k) == 0,
{
}

/// Writing bit `k` of `x` leaves every other bit `m != k` of the word untouched.
proof fn lemma_bit_other(x: u64, k: u64, m: u64)
    by (bit_vector)
    requires
        k < 64,
        m < 64,
        k != m,
    ensures
        (x | (1u64 << k)) & (1u64 << m) == x & (1u64 << m),
        (x & !(1u64 << k)) & (1u64 << m) == x & (1u64 << m),
{
}

} // verus!
impl<const WORDS: usize> SlotAlloc<WORDS> {
    /// Free-slot count — for tests and the on-OS leak assertion. Bookkeeping
    /// only (not a verified obligation), so it keeps its iterator form outside
    /// the `verus!{}` block.
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
