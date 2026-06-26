//! `freelist` — the verified free-list core, shared by `dma-pool` (rev2§2.5) and
//! the `urt` heap allocator (rev2§6).
//!
//! [`FreeList<N>`] is a sorted, pairwise-disjoint list of free extents
//! `(offset, len)` over a pool `[0, len)`, where `N` is the fixed fragmentation
//! cap. No backing, no pointers — pure arithmetic a wrapper delegates
//! allocation/return to, so the disjointness theorem lives here and each crate's
//! trusted hardware/byte-region seam stays out of the proof. The properties hold
//! ∀ pool length, request size, and alignment: every [`FreeList::alloc`] hands out
//! an in-pool, aligned offset whose region was free and is now used, with coverage
//! elsewhere unchanged (the `FreeList::covers` frame). **Two live allocations are
//! therefore disjoint ∀** — the verified `lemma_two_allocs_disjoint`.
//! [`FreeList::free`] returns a region to the list (the two-sided adjacency merge),
//! making it allocatable again. The array-splice bookkeeping is factored into the
//! verified helpers `remove_at`/`insert_at` (explicit shift loops in place of
//! `copy_within`, which has no Verus model). The alignment round-up is computed in
//! modular form (`off + (align - off%align)%align`) rather than the bit-mask
//! `(off+align-1) & !(align-1)`: behaviourally identical, but `start % align == 0`
//! is then pure `vstd::arithmetic` and needs no `by (bit_vector)`.
//!
//! This crate is the single home of that ~1300-line proof; `dma-pool` and the
//! `urt` heap both depend on it.

#![cfg_attr(not(any(feature = "std", test)), no_std)]

use vstd::prelude::*;

verus! {

/// The verified free-list core: a sorted, pairwise-disjoint list of free
/// extents `(offset, len)` over a pool `[0, len)`. `N` is the fixed
/// fragmentation cap (`MAX_FREE_RANGES`). No backing, no pointers — the pure
/// arithmetic `DmaPool` delegates allocation/return to, so the disjointness
/// theorem lives here and the PA seam stays out of the proof.
pub struct FreeList<const N: usize> {
    /// Total pool length; the upper bound every extent lives under. Only the
    /// ghost `spec_len`/`wf` read it, so it is exec-write-only once `verus!{}`
    /// erases — hence `dead_code` allowed.
    #[allow(dead_code)]
    len: usize,
    /// `free[0..nfree]`: free extents, sorted by offset, non-empty,
    /// non-adjacent (a strict gap between consecutive extents).
    free: [(usize, usize); N],
    nfree: usize,
}

impl<const N: usize> FreeList<N> {
    /// The pool length, as a ghost int — a `closed` accessor so the public
    /// contracts speak of the offset arithmetic without exposing the field.
    pub closed spec fn spec_len(self) -> int {
        self.len as int
    }

    /// The live extent count, as a ghost int — a `closed` accessor so the public
    /// `free` contract can state the not-full precondition without exposing the
    /// field.
    pub closed spec fn spec_nfree(self) -> int {
        self.nfree as int
    }

    /// Position `p` lies inside extent `k` (`[off, off+len)`). The atom the
    /// `covers` existential and the splice frame proofs are written against.
    pub closed spec fn ext_has(self, k: int, p: int) -> bool {
        self.free@[k].0 as int <= p < self.free@[k].0 as int + self.free@[k].1 as int
    }

    /// Position `p` lies inside some live free extent (the free *set*). The
    /// model the disjointness theorem is stated against. `closed` as above.
    pub closed spec fn covers(self, p: int) -> bool {
        exists|k: int| #![trigger self.ext_has(k, p)] 0 <= k < self.nfree && self.ext_has(k, p)
    }

    /// Well-formedness: bitmap-wide-enough, every extent non-empty and within
    /// `[0, len)`, the list strictly sorted with a gap between neighbours (the
    /// merged-canonical invariant every op preserves). `closed`.
    pub closed spec fn wf(self) -> bool {
        &&& self.free@.len() == N
        &&& self.nfree <= N
        &&& (forall|k: int| 0 <= k < self.nfree ==> #[trigger] self.free@[k].1 > 0)
        &&& (forall|k: int| 0 <= k < self.nfree
                ==> #[trigger] self.free@[k].0 as int + self.free@[k].1 as int <= self.len as int)
        &&& (forall|k: int| #![trigger self.free@[k].0, self.free@[k].1]
                0 <= k < self.nfree - 1
                ==> (self.free@[k].0 as int + self.free@[k].1 as int)
                        < self.free@[k + 1].0 as int)
    }

    /// Sortedness is transitive AND strict: for `j < k`, extent `j` ends
    /// strictly before extent `k` begins (the gap survives the chain because
    /// each extent is non-empty). The pairwise-disjointness fact the `covers`
    /// frame rests on (an extent `j != i` cannot cover a position in extent `i`),
    /// and the strict form is what re-establishes a junction gap after a splice.
    proof fn lemma_chain(self, j: int, k: int)
        requires
            self.wf(),
            0 <= j < k < self.nfree,
        ensures
            (self.free@[j].0 as int + self.free@[j].1 as int) < self.free@[k].0 as int,
        decreases k - j,
    {
        if j + 1 < k {
            self.lemma_chain(j, k - 1);
            // free[j].end < free[k-1].start <= free[k-1].end < free[k].start.
            assert((self.free@[k - 1].0 as int + self.free@[k - 1].1 as int)
                < self.free@[k].0 as int);
        }
    }

    /// Extents `j` and `k` (`j != k`) are disjoint: no position lies in both.
    proof fn lemma_disjoint(self, j: int, k: int, p: int)
        requires
            self.wf(),
            0 <= j < self.nfree,
            0 <= k < self.nfree,
            j != k,
            self.ext_has(j, p),
        ensures
            !self.ext_has(k, p),
    {
        if j < k {
            self.lemma_chain(j, k);
        } else {
            self.lemma_chain(k, j);
        }
    }

    /// The modular round-up `start = off + pad` (where `pad` rounds `off` up to
    /// the next multiple of `align`) lands a multiple of `align`, with the pad
    /// strictly under `align`. The exec uses this instead of the bit-mask
    /// `(off+align-1) & !(align-1)`: behaviourally identical, but `start % align
    /// == 0` is then pure `vstd::arithmetic` — no `by (bit_vector)`.
    proof fn lemma_round_up_aligned(off: int, align: int, pad: int)
        requires
            align > 0,
            0 <= off,
            pad == (if off % align == 0 { 0int } else { align - off % align }),
        ensures
            (off + pad) % align == 0,
            0 <= pad < align,
    {
        vstd::arithmetic::div_mod::lemma_mod_bound(off, align);
        vstd::arithmetic::div_mod::lemma_fundamental_div_mod(off, align);
        let q = off / align;
        let r = off % align;
        // off == align * q + r, 0 <= r < align.
        if r == 0 {
            assert(off + pad == q * align + 0) by (nonlinear_arith)
                requires off == align * q, pad == 0;
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod_converse_mod(
                off + pad, align, q, 0,
            );
        } else {
            assert(off + pad == (q + 1) * align + 0) by (nonlinear_arith)
                requires off == align * q + r, pad == align - r;
            vstd::arithmetic::div_mod::lemma_fundamental_div_mod_converse_mod(
                off + pad, align, q + 1, 0,
            );
        }
    }

    /// Remove the extent at index `g`, shifting `(g, nfree)` down one. Pure
    /// index bookkeeping — does NOT re-establish `wf` (the caller does, knowing
    /// the specific merge situation); it only pins the element correspondence.
    fn remove_at(&mut self, g: usize)
        requires
            g < old(self).nfree,
            old(self).nfree <= N,
            old(self).free@.len() == N,
        ensures
            final(self).len == old(self).len,
            final(self).nfree == old(self).nfree - 1,
            final(self).free@.len() == N,
            forall|k: int| 0 <= k < g ==> final(self).free@[k] == old(self).free@[k],
            forall|k: int| g <= k < final(self).nfree
                ==> final(self).free@[k] == old(self).free@[k + 1],
    {
        broadcast use vstd::array::group_array_axioms;
        let top = self.nfree - 1;
        let mut j = g;
        while j < top
            invariant
                g <= j <= top,
                top + 1 == self.nfree,
                self.len == old(self).len,
                self.nfree == old(self).nfree,
                self.free@.len() == N,
                self.nfree <= N,
                forall|k: int| 0 <= k < g ==> self.free@[k] == old(self).free@[k],
                forall|k: int| g <= k < j ==> self.free@[k] == old(self).free@[k + 1],
                forall|k: int| j <= k < self.nfree ==> self.free@[k] == old(self).free@[k],
            decreases top - j,
        {
            self.free[j] = self.free[j + 1];
            j += 1;
        }
        self.nfree -= 1;
    }

    /// Open a gap at index `g` (shift `[g, nfree)` up one) and place `val`.
    /// Pure bookkeeping, like [`FreeList::remove_at`] — caller re-establishes `wf`.
    fn insert_at(&mut self, g: usize, val: (usize, usize))
        requires
            old(self).nfree < N,
            g <= old(self).nfree,
            old(self).free@.len() == N,
        ensures
            final(self).len == old(self).len,
            final(self).nfree == old(self).nfree + 1,
            final(self).free@.len() == N,
            final(self).free@[g as int] == val,
            forall|k: int| 0 <= k < g ==> final(self).free@[k] == old(self).free@[k],
            forall|k: int| g < k < final(self).nfree
                ==> final(self).free@[k] == old(self).free@[k - 1],
    {
        broadcast use vstd::array::group_array_axioms;
        let mut j = self.nfree;
        while j > g
            invariant
                g <= j <= self.nfree,
                self.nfree < N,
                self.len == old(self).len,
                self.nfree == old(self).nfree,
                self.free@.len() == N,
                forall|k: int| 0 <= k < j ==> self.free@[k] == old(self).free@[k],
                forall|k: int| j < k <= self.nfree ==> self.free@[k] == old(self).free@[k - 1],
            decreases j,
        {
            self.free[j] = self.free[j - 1];
            j -= 1;
        }
        self.free[g] = val;
        self.nfree += 1;
    }

    /// A fresh pool: all of `[0, len)` free in one extent (or empty when
    /// `len == 0`). `N >= 1` so the single extent has somewhere to live.
    pub fn new(len: usize) -> (r: FreeList<N>)
        requires
            N >= 1,
        ensures
            r.wf(),
            r.spec_len() == len as int,
            forall|p: int| r.covers(p) <==> (0 <= p < len as int),
    {
        broadcast use vstd::array::group_array_axioms;
        let mut free = [(0usize, 0usize); N];
        let nfree;
        if len == 0 {
            nfree = 0;
        } else {
            free[0] = (0, len);
            nfree = 1;
        }
        let r = FreeList { len, free, nfree };
        assert forall|p: int| r.covers(p) <==> (0 <= p < len as int) by {
            if 0 <= p < len as int {
                // len > 0 here, so nfree == 1 and extent 0 is exactly [0, len).
                assert(r.free@[0].0 == 0 && r.free@[0].1 == len);
                assert(r.ext_has(0, p));
                assert(r.covers(p));
            }
            if r.covers(p) {
                let k = choose|k: int| 0 <= k < r.nfree && r.ext_has(k, p);
                // The only possible witness is k == 0 (nfree <= 1), and extent 0
                // is [0, len) (or there is no extent at all when len == 0).
                assert(r.free@[0].0 == 0 && r.free@[0].1 == len);
            }
        }
        r
    }

    /// The list is at its fragmentation cap. The exec witness for [`FreeList::free`]'s
    /// `spec_nfree() < N` precondition: the wrapper is erased plain Rust and cannot
    /// read the `closed` spec, so it asserts `!is_full()` (which, with the always-held
    /// `wf` invariant `nfree <= N`, establishes `nfree < N`) before delegating.
    pub fn is_full(&self) -> (r: bool)
        ensures
            r == (self.spec_nfree() == N as int),
    {
        self.nfree == N
    }

    /// `[off, off+n)` is wholly allocated — no position lies in any free extent. The
    /// exec witness for [`FreeList::free`]'s no-double-free / no-overlap precondition
    /// (`forall p in [off, off+n): !covers(p)`). O(nfree) over the `<= N` extents; the
    /// loop invariant carries per-extent disjointness, mirroring `free`'s own
    /// covers-reasoning (an extent overlaps `[off, off+n)` iff it covers a point in it).
    #[verifier::spinoff_prover]
    pub fn is_allocated(&self, off: usize, n: usize) -> (r: bool)
        requires
            self.wf(),
            n > 0,
            // discharged at the wrapper call site (off + n <= backing.len() == spec_len);
            // keeps the exec `off + n` below from overflowing.
            off as int + n as int <= self.spec_len(),
        ensures
            r == (forall|p: int| off <= p < off + n ==> !self.covers(p)),
    {
        broadcast use vstd::array::group_array_axioms;
        let mut k: usize = 0;
        while k < self.nfree
            invariant
                self.wf(),
                n > 0,
                off as int + n as int <= self.spec_len(),
                0 <= k <= self.nfree,
                // every scanned extent is disjoint from [off, off+n).
                forall|j: int| #![trigger self.free@[j].0, self.free@[j].1]
                    0 <= j < k
                    ==> (self.free@[j].0 as int + self.free@[j].1 as int <= off as int
                            || off as int + n as int <= self.free@[j].0 as int),
            decreases self.nfree - k,
        {
            let o = self.free[k].0;
            let l = self.free[k].1;
            // [o, o+l) overlaps [off, off+n) iff o+l > off && o < off+n. `o+l` is
            // overflow-safe by `wf`; `off+n` by the precondition.
            if o + l > off && o < off + n {
                // The overlap covers a point of [off, off+n) — exhibit it.
                assert(!(forall|p: int| off <= p < off + n ==> !self.covers(p))) by {
                    let q: int = if o <= off { off as int } else { o as int };
                    assert(self.free@[k as int].1 > 0);
                    assert(self.ext_has(k as int, q));
                    assert(self.covers(q));
                }
                return false;
            }
            k += 1;
        }
        // k == nfree: every extent is disjoint from [off, off+n), so no point is covered.
        assert forall|p: int| off <= p < off + n implies !self.covers(p) by {
            if self.covers(p) {
                let j = choose|j: int| 0 <= j < self.nfree && self.ext_has(j, p);
                assert(self.ext_has(j, p));
                // invariant (k == nfree) gives j disjoint from [off, off+n), which
                // contradicts ext_has(j, p) with p in [off, off+n).
                assert(self.free@[j].0 as int + self.free@[j].1 as int <= off as int
                    || off as int + n as int <= self.free@[j].0 as int);
            }
        }
        true
    }

    /// Carve `n` aligned bytes from the first extent that fits. `Some(start)`
    /// gives an in-pool, `align`-aligned offset whose `[start, start+n)` was
    /// free and is now used, every other position's coverage unchanged. `None`
    /// only frames `covers` (first-fit + the `N`-cap may refuse with space left,
    /// so — unlike a bitmap allocator — `None` is *not* an exact-exhaustion claim).
    #[verifier::spinoff_prover]
    pub fn alloc(&mut self, n: usize, align: usize) -> (r: Option<usize>)
        requires
            old(self).wf(),
            align > 0,
        ensures
            final(self).wf(),
            final(self).spec_len() == old(self).spec_len(),
            match r {
                Some(start) => {
                    &&& start as int + n as int <= final(self).spec_len()
                    &&& start as int % align as int == 0
                    &&& (forall|p: int| start <= p < start + n ==> old(self).covers(p))
                    &&& (forall|p: int| start <= p < start + n ==> !final(self).covers(p))
                    &&& (forall|p: int| !(start <= p < start + n)
                            ==> final(self).covers(p) == old(self).covers(p))
                },
                None => forall|p: int| final(self).covers(p) == old(self).covers(p),
            },
    {
        broadcast use vstd::array::group_array_axioms;
        if n == 0 {
            return None;
        }
        let mut i: usize = 0;
        while i < self.nfree
            invariant
                self.wf(),
                self.len == old(self).len,
                self.nfree == old(self).nfree,
                self.free@ == old(self).free@,
                0 <= i <= self.nfree,
                n > 0,
                align > 0,
            decreases self.nfree - i,
        {
            let off = self.free[i].0;
            let flen = self.free[i].1;
            let rem = off % align;
            let pad = if rem == 0 { 0 } else { align - rem };
            if pad > flen || n > flen - pad {
                i += 1;
                continue;
            }
            // pad <= flen and pad + n <= flen, so the carve fits in extent i.
            let start = off + pad;
            let rest_off = start + n;
            let rest_len = flen - pad - n;
            proof {
                Self::lemma_round_up_aligned(off as int, align as int, pad as int);
            }
            // Ghost handles to the pre-carve extent i (self still == entry here).
            let ghost oi = old(self).free@[i as int];
            if pad == 0 && rest_len == 0 {
                self.remove_at(i);
                proof {
                    Self::alloc_proof_remove(*self, *old(self), i as int, off as int,
                        flen as int);
                    // remove's frame is over [off, off+flen); here pad == 0 and
                    // rest_len == 0, so start == off and n == flen — the carved
                    // region the alloc postcondition names.
                    assert(start == off);
                    assert(n == flen);
                }
            } else if pad > 0 && rest_len == 0 {
                self.free[i] = (off, pad);
                proof {
                    Self::alloc_proof_set(*self, *old(self), i as int, off as int, flen as int,
                        off as int, pad as int, start as int, n as int);
                }
            } else if pad == 0 && rest_len > 0 {
                self.free[i] = (rest_off, rest_len);
                proof {
                    Self::alloc_proof_set(*self, *old(self), i as int, off as int, flen as int,
                        rest_off as int, rest_len as int, start as int, n as int);
                }
            } else {
                if self.nfree == N {
                    i += 1;
                    continue;
                }
                self.free[i] = (off, pad);
                self.insert_at(i + 1, (rest_off, rest_len));
                proof {
                    Self::alloc_proof_split(*self, *old(self), i as int, off as int, flen as int,
                        pad as int, rest_off as int, rest_len as int, start as int, n as int);
                }
            }
            proof {
                // POST4 (aligned): from lemma_round_up_aligned's (off+pad)%align == 0.
                assert(start as int == off as int + pad as int);
                assert(start as int % align as int == 0);
                // POST3 (in-pool): start+n = off+pad+n <= off+flen <= len (carve fits,
                // extent i is in bounds by old's wf).
                assert(old(self).free@[i as int].0 as int + old(self).free@[i as int].1 as int
                    <= old(self).len as int);
                assert(off as int + flen as int <= old(self).len as int);
                assert(pad as int + n as int <= flen as int);
                assert(start as int + n as int <= self.spec_len());
            }
            return Some(start);
        }
        // Loop exhausted without a fit: self was never mutated, so coverage is
        // identical to entry (same `free@` and `nfree` ⇒ same `ext_has` ⇒ same `covers`).
        proof {
            assert forall|p: int| self.covers(p) == old(self).covers(p) by {
                assert forall|k: int| self.ext_has(k, p) == old(self).ext_has(k, p) by {
                    assert(self.free@[k] == old(self).free@[k]);
                }
            }
        }
        None
    }

    /// `alloc` carve-arm frame, the single-extent case: old extent `i` =
    /// `[off, off+flen)` is replaced in place by the one new extent `[a, a+b)`
    /// (a prefix or suffix of it), removing the complementary interval
    /// `R = [rs, rs+rn)`. Proves `wf` survives and `covers` changes by exactly
    /// `R` (the (T,F) pad-keep and (F,T) rest-keep arms).
    proof fn alloc_proof_set(new: FreeList<N>, old: FreeList<N>, i: int, off: int, flen: int,
        a: int, b: int, rs: int, rn: int)
        requires
            old.wf(),
            0 <= i < old.nfree,
            old.free@[i].0 as int == off,
            old.free@[i].1 as int == flen,
            new.len == old.len,
            new.nfree == old.nfree,
            new.free@.len() == N,
            new.free@[i].0 as int == a,
            new.free@[i].1 as int == b,
            forall|k: int| 0 <= k < new.nfree && k != i ==> new.free@[k] == old.free@[k],
            b > 0,
            rn > 0,
            (a == off && rs == a + b && rs + rn == off + flen)
                || (rs == off && rs + rn == a && a + b == off + flen),
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
            forall|p: int| rs <= p < rs + rn ==> old.covers(p),
            forall|p: int| rs <= p < rs + rn ==> !new.covers(p),
            forall|p: int| !(rs <= p < rs + rn) ==> new.covers(p) == old.covers(p),
    {
        // new ext i and R both sit inside old ext i, and are disjoint.
        assert(off <= a && a + b <= off + flen);
        assert(off <= rs && rs + rn <= off + flen);
        assert(a + b <= rs || rs + rn <= a);
        // wf: only extent i changed; the two junctions i-1, i use old's gaps.
        assert(new.wf()) by {
            if 0 <= i - 1 {
                assert((old.free@[i - 1].0 as int + old.free@[i - 1].1 as int)
                    < old.free@[i].0 as int);
            }
            if i + 1 < new.nfree {
                assert((old.free@[i].0 as int + old.free@[i].1 as int)
                    < old.free@[i + 1].0 as int);
            }
        }
        // POST5: R lies in old extent i.
        assert forall|p: int| rs <= p < rs + rn implies old.covers(p) by {
            assert(old.ext_has(i, p));
        }
        // POST6: nothing covers R any more.
        assert forall|p: int| rs <= p < rs + rn implies !new.covers(p) by {
            assert forall|k: int| 0 <= k < new.nfree implies !new.ext_has(k, p) by {
                if k != i {
                    assert(old.ext_has(i, p));
                    old.lemma_disjoint(i, k, p);
                }
            }
        }
        // POST7: coverage elsewhere is unchanged.
        assert forall|p: int| !(rs <= p < rs + rn) implies new.covers(p) == old.covers(p) by {
            if old.covers(p) {
                let j = choose|j: int| 0 <= j < old.nfree && old.ext_has(j, p);
                if j == i {
                    assert(new.ext_has(i, p));
                } else {
                    assert(new.ext_has(j, p));
                }
                assert(new.covers(p));
            }
            if new.covers(p) {
                let k = choose|k: int| 0 <= k < new.nfree && new.ext_has(k, p);
                if k == i {
                    assert(old.ext_has(i, p));
                } else {
                    assert(old.ext_has(k, p));
                }
                assert(old.covers(p));
            }
        }
    }

    /// `alloc` carve-arm frame, the whole-extent case: old extent `i` =
    /// `[off, off+flen)` is removed entirely (the (F,F) exact-fit arm; the carved
    /// region `R` is all of extent `i`). Proves `wf` survives and `covers` loses
    /// exactly `R`.
    proof fn alloc_proof_remove(new: FreeList<N>, old: FreeList<N>, i: int, off: int, flen: int)
        requires
            old.wf(),
            0 <= i < old.nfree,
            old.free@[i].0 as int == off,
            old.free@[i].1 as int == flen,
            new.len == old.len,
            new.nfree == old.nfree - 1,
            new.free@.len() == N,
            forall|k: int| 0 <= k < i ==> new.free@[k] == old.free@[k],
            forall|k: int| i <= k < new.nfree ==> new.free@[k] == old.free@[k + 1],
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
            forall|p: int| off <= p < off + flen ==> old.covers(p),
            forall|p: int| off <= p < off + flen ==> !new.covers(p),
            forall|p: int| !(off <= p < off + flen) ==> new.covers(p) == old.covers(p),
    {
        // wf: kept extents keep their geometry; the junction at i-1 closes over the
        // removed extent via the strict chain old[i-1].end < old[i+1].start.
        assert(new.wf()) by {
            if 0 <= i - 1 && i + 1 < old.nfree {
                old.lemma_chain(i - 1, i + 1);
            }
        }
        assert forall|p: int| off <= p < off + flen implies old.covers(p) by {
            assert(old.ext_has(i, p));
        }
        assert forall|p: int| off <= p < off + flen implies !new.covers(p) by {
            assert forall|k: int| 0 <= k < new.nfree implies !new.ext_has(k, p) by {
                assert(old.ext_has(i, p));
                if k < i {
                    old.lemma_disjoint(i, k, p);
                } else {
                    old.lemma_disjoint(i, k + 1, p);
                }
            }
        }
        assert forall|p: int| !(off <= p < off + flen) implies new.covers(p) == old.covers(p) by {
            if old.covers(p) {
                let j = choose|j: int| 0 <= j < old.nfree && old.ext_has(j, p);
                if j < i {
                    assert(new.ext_has(j, p));
                } else {
                    // j != i (that would put p in the removed [off,off+flen)).
                    assert(new.ext_has(j - 1, p));
                }
                assert(new.covers(p));
            }
            if new.covers(p) {
                let k = choose|k: int| 0 <= k < new.nfree && new.ext_has(k, p);
                if k < i {
                    assert(old.ext_has(k, p));
                } else {
                    assert(old.ext_has(k + 1, p));
                }
                assert(old.covers(p));
            }
        }
    }

    /// `alloc` carve-arm frame, the split case: old extent `i` = `[off, off+flen)`
    /// becomes two — the pad `[off, off+pad)` at `i` and the remainder
    /// `[rest_off, off+flen)` at `i+1` — with everything above shifted up one
    /// (the (T,T) arm). The carved region `R = [off+pad, rest_off)`. Dispatches to
    /// two spinoff'd halves so neither blows the per-function rlimit.
    proof fn alloc_proof_split(new: FreeList<N>, old: FreeList<N>, i: int, off: int, flen: int,
        pad: int, rest_off: int, rest_len: int, start: int, n: int)
        requires
            old.wf(),
            old.nfree < N,
            0 <= i < old.nfree,
            old.free@[i].0 as int == off,
            old.free@[i].1 as int == flen,
            new.len == old.len,
            new.nfree == old.nfree + 1,
            new.free@.len() == N,
            new.free@[i].0 as int == off,
            new.free@[i].1 as int == pad,
            new.free@[i + 1].0 as int == rest_off,
            new.free@[i + 1].1 as int == rest_len,
            forall|k: int| 0 <= k < i ==> new.free@[k] == old.free@[k],
            forall|k: int| i + 1 < k < new.nfree ==> new.free@[k] == old.free@[k - 1],
            pad > 0,
            rest_len > 0,
            start == off + pad,
            rest_off == start + n,
            n > 0,
            rest_off + rest_len == off + flen,
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
            forall|p: int| start <= p < start + n ==> old.covers(p),
            forall|p: int| start <= p < start + n ==> !new.covers(p),
            forall|p: int| !(start <= p < start + n) ==> new.covers(p) == old.covers(p),
    {
        Self::split_wf(new, old, i, off, flen, pad, rest_off, rest_len, start, n);
        Self::split_covers(new, old, i, off, flen, pad, rest_off, rest_len, start, n);
    }

    /// `wf` survives the split (the sorted/non-empty/in-bounds conjuncts).
    proof fn split_wf(new: FreeList<N>, old: FreeList<N>, i: int, off: int, flen: int,
        pad: int, rest_off: int, rest_len: int, start: int, n: int)
        requires
            old.wf(),
            old.nfree < N,
            0 <= i < old.nfree,
            old.free@[i].0 as int == off,
            old.free@[i].1 as int == flen,
            new.len == old.len,
            new.nfree == old.nfree + 1,
            new.free@.len() == N,
            new.free@[i].0 as int == off,
            new.free@[i].1 as int == pad,
            new.free@[i + 1].0 as int == rest_off,
            new.free@[i + 1].1 as int == rest_len,
            forall|k: int| 0 <= k < i ==> new.free@[k] == old.free@[k],
            forall|k: int| i + 1 < k < new.nfree ==> new.free@[k] == old.free@[k - 1],
            pad > 0,
            rest_len > 0,
            start == off + pad,
            rest_off == start + n,
            n > 0,
            rest_off + rest_len == off + flen,
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
    {
        // Sortedness, junction by junction: i-1→i and i+1→i+2 close over old's
        // adjacent gaps; i→i+1 is the n>0 gap between the two new pieces.
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree - 1 implies
            (new.free@[k].0 as int + new.free@[k].1 as int) < new.free@[k + 1].0 as int by {
            if k + 1 < i {
                assert((old.free@[k].0 as int + old.free@[k].1 as int) < old.free@[k + 1].0 as int);
            } else if k == i - 1 {
                assert((old.free@[i - 1].0 as int + old.free@[i - 1].1 as int)
                    < old.free@[i].0 as int);
            } else if k == i {
                // off+pad < rest_off == off+pad+n.
            } else if k == i + 1 {
                assert((old.free@[i].0 as int + old.free@[i].1 as int)
                    < old.free@[i + 1].0 as int);
            } else {
                assert((old.free@[k - 1].0 as int + old.free@[k - 1].1 as int)
                    < old.free@[k].0 as int);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].1 > 0 by {
            if k < i {
                assert(old.free@[k].1 > 0);
            } else if k > i + 1 {
                assert(old.free@[k - 1].1 > 0);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].0 as int + new.free@[k].1 as int <= new.len as int by {
            assert(old.free@[i].0 as int + old.free@[i].1 as int <= old.len as int);
            if k < i {
                assert(old.free@[k].0 as int + old.free@[k].1 as int <= old.len as int);
            } else if k > i + 1 {
                assert(old.free@[k - 1].0 as int + old.free@[k - 1].1 as int <= old.len as int);
            }
        }
        assert(new.wf());
    }

    /// `covers` changes by exactly the carved `R = [start, start+n)` across the split.
    #[verifier::spinoff_prover]
    proof fn split_covers(new: FreeList<N>, old: FreeList<N>, i: int, off: int, flen: int,
        pad: int, rest_off: int, rest_len: int, start: int, n: int)
        requires
            old.wf(),
            0 <= i < old.nfree,
            old.free@[i].0 as int == off,
            old.free@[i].1 as int == flen,
            new.nfree == old.nfree + 1,
            new.free@[i].0 as int == off,
            new.free@[i].1 as int == pad,
            new.free@[i + 1].0 as int == rest_off,
            new.free@[i + 1].1 as int == rest_len,
            forall|k: int| 0 <= k < i ==> new.free@[k] == old.free@[k],
            forall|k: int| i + 1 < k < new.nfree ==> new.free@[k] == old.free@[k - 1],
            pad > 0,
            rest_len > 0,
            start == off + pad,
            rest_off == start + n,
            n > 0,
            rest_off + rest_len == off + flen,
        ensures
            forall|p: int| start <= p < start + n ==> old.covers(p),
            forall|p: int| start <= p < start + n ==> !new.covers(p),
            forall|p: int| !(start <= p < start + n) ==> new.covers(p) == old.covers(p),
    {
        assert forall|p: int| start <= p < start + n implies old.covers(p) by {
            assert(old.ext_has(i, p));
        }
        assert forall|p: int| start <= p < start + n implies !new.covers(p) by {
            assert forall|k: int| 0 <= k < new.nfree implies !new.ext_has(k, p) by {
                if k == i || k == i + 1 {
                    // p in R is below rest_off and at/above start: in neither new piece.
                } else if k < i {
                    assert(old.ext_has(i, p));
                    old.lemma_disjoint(i, k, p);
                } else {
                    assert(old.ext_has(i, p));
                    old.lemma_disjoint(i, k - 1, p);
                }
            }
        }
        assert forall|p: int| !(start <= p < start + n) implies new.covers(p) == old.covers(p) by {
            if old.covers(p) {
                let j = choose|j: int| 0 <= j < old.nfree && old.ext_has(j, p);
                if j < i {
                    assert(new.ext_has(j, p));
                } else if j == i {
                    if p < start {
                        assert(new.ext_has(i, p));
                    } else {
                        assert(new.ext_has(i + 1, p));
                    }
                } else {
                    assert(new.ext_has(j + 1, p));
                }
                assert(new.covers(p));
            }
            if new.covers(p) {
                let k = choose|k: int| 0 <= k < new.nfree && new.ext_has(k, p);
                if k < i {
                    assert(old.ext_has(k, p));
                } else if k == i || k == i + 1 {
                    assert(old.ext_has(i, p));
                } else {
                    assert(old.ext_has(k - 1, p));
                }
                assert(old.covers(p));
            }
        }
    }

    /// `free` no-merge case: the returned region `[off, off+n)` is inserted as a
    /// fresh extent at `i`, with a strict gap on both sides. `covers` gains
    /// exactly `[off, off+n)`.
    #[verifier::spinoff_prover]
    // Sized for the worst re-verification context (doc/guidelines/verus.md §10);
    // after phase 5.1/5.2/6.2 trigger reductions the no-alloc consumption (~358k)
    // is the highest of the two contexts (alloc ~287k).
    #[verifier::rlimit(1)]
    proof fn free_insert(new: FreeList<N>, old: FreeList<N>, i: int, off: int, n: int)
        requires
            old.wf(),
            old.nfree < N,
            0 <= i <= old.nfree,
            new.len == old.len,
            new.nfree == old.nfree + 1,
            new.free@.len() == N,
            new.free@[i].0 as int == off,
            new.free@[i].1 as int == n,
            forall|k: int| 0 <= k < i ==> new.free@[k] == old.free@[k],
            forall|k: int| i < k < new.nfree ==> new.free@[k] == old.free@[k - 1],
            n > 0,
            off + n <= old.len,
            0 < i ==> (old.free@[i - 1].0 as int + old.free@[i - 1].1 as int) < off,
            i < old.nfree ==> off + n < old.free@[i].0 as int,
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
            forall|p: int| off <= p < off + n ==> new.covers(p),
            forall|p: int| !(off <= p < off + n) ==> new.covers(p) == old.covers(p),
    {
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree - 1 implies
            (new.free@[k].0 as int + new.free@[k].1 as int) < new.free@[k + 1].0 as int by {
            if k + 1 < i {
                assert((old.free@[k].0 as int + old.free@[k].1 as int) < old.free@[k + 1].0 as int);
            } else if k == i - 1 {
                // old[i-1].end < off == new[i].start.
            } else if k == i {
                // off+n < old[i].start == new[i+1].start.
            } else {
                assert((old.free@[k - 1].0 as int + old.free@[k - 1].1 as int)
                    < old.free@[k].0 as int);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].1 > 0 by {
            if k < i {
                assert(old.free@[k].1 > 0);
            } else if k > i {
                assert(old.free@[k - 1].1 > 0);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].0 as int + new.free@[k].1 as int <= new.len as int by {
            if k < i {
                assert(old.free@[k].0 as int + old.free@[k].1 as int <= old.len as int);
            } else if k > i {
                assert(old.free@[k - 1].0 as int + old.free@[k - 1].1 as int <= old.len as int);
            }
        }
        assert(new.wf());
        Self::free_covers_insert(new, old, i, off, n);
    }

    /// `covers` half of [`FreeList::free_insert`] (split out for rlimit).
    #[verifier::spinoff_prover]
    proof fn free_covers_insert(new: FreeList<N>, old: FreeList<N>, i: int, off: int, n: int)
        requires
            0 <= i <= old.nfree,
            new.nfree == old.nfree + 1,
            new.free@[i].0 as int == off,
            new.free@[i].1 as int == n,
            forall|k: int| 0 <= k < i ==> new.free@[k] == old.free@[k],
            forall|k: int| i < k < new.nfree ==> new.free@[k] == old.free@[k - 1],
            n > 0,
        ensures
            forall|p: int| off <= p < off + n ==> new.covers(p),
            forall|p: int| !(off <= p < off + n) ==> new.covers(p) == old.covers(p),
    {
        assert forall|p: int| off <= p < off + n implies new.covers(p) by {
            assert(new.ext_has(i, p));
        }
        assert forall|p: int| !(off <= p < off + n) implies new.covers(p) == old.covers(p) by {
            if old.covers(p) {
                let j = choose|j: int| 0 <= j < old.nfree && old.ext_has(j, p);
                if j < i {
                    assert(new.ext_has(j, p));
                } else {
                    assert(new.ext_has(j + 1, p));
                }
                assert(new.covers(p));
            }
            if new.covers(p) {
                let k = choose|k: int| 0 <= k < new.nfree && new.ext_has(k, p);
                if k < i {
                    assert(old.ext_has(k, p));
                } else if k > i {
                    assert(old.ext_has(k - 1, p));
                }
                // k == i ⇒ p ∈ [off, off+n), excluded by hypothesis.
                assert(old.covers(p) || (off <= p < off + n));
            }
        }
    }

    /// `free` single-merge case: old extent `g` is widened in place to `E` by
    /// absorbing the adjacent returned region `[off, off+n)` (a right-extension
    /// when `g` is the left neighbour, a left-extension when `g` is the right
    /// neighbour). `covers` gains exactly `[off, off+n)`.
    #[verifier::spinoff_prover]
    // Sized for the worst re-verification context (doc/guidelines/verus.md §10);
    // alloc context (~259k) is the highest of the two contexts (no-alloc ~245k).
    #[verifier::rlimit(1)]
    proof fn free_replace(new: FreeList<N>, old: FreeList<N>, g: int, off: int, n: int,
        eoff: int, elen: int)
        requires
            old.wf(),
            0 <= g < old.nfree,
            new.len == old.len,
            new.nfree == old.nfree,
            new.free@.len() == N,
            new.free@[g].0 as int == eoff,
            new.free@[g].1 as int == elen,
            forall|k: int| 0 <= k < new.nfree && k != g ==> new.free@[k] == old.free@[k],
            elen > 0,
            n > 0,
            off + n <= old.len,
            // E = old[g] ∪ [off,off+n), adjacent: right-extension or left-extension.
            (eoff == old.free@[g].0 as int
                && old.free@[g].0 as int + old.free@[g].1 as int == off
                && eoff + elen == off + n)
            || (eoff == off && off + n == old.free@[g].0 as int
                && eoff + elen == old.free@[g].0 as int + old.free@[g].1 as int),
            // E keeps the strict gaps to its surviving neighbours.
            0 < g ==> (old.free@[g - 1].0 as int + old.free@[g - 1].1 as int) < eoff,
            g + 1 < old.nfree ==> eoff + elen < old.free@[g + 1].0 as int,
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
            forall|p: int| off <= p < off + n ==> new.covers(p),
            forall|p: int| !(off <= p < off + n) ==> new.covers(p) == old.covers(p),
    {
        assert(off <= eoff + elen && eoff <= off);
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree - 1 implies
            (new.free@[k].0 as int + new.free@[k].1 as int) < new.free@[k + 1].0 as int by {
            if k + 1 < g || k > g {
                assert((old.free@[k].0 as int + old.free@[k].1 as int) < old.free@[k + 1].0 as int);
            }
            // k == g-1 and k == g use the two gap requires.
        }
        assert forall|k: int| #![trigger new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].1 > 0 by {
            if k != g {
                assert(old.free@[k].1 > 0);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].0 as int + new.free@[k].1 as int <= new.len as int by {
            if k != g {
                assert(old.free@[k].0 as int + old.free@[k].1 as int <= old.len as int);
            }
            // k == g: eoff+elen is old[g].end or off+n, both <= len.
            assert(old.free@[g].0 as int + old.free@[g].1 as int <= old.len as int);
        }
        assert(new.wf());
        Self::free_covers_replace(new, old, g, off, n, eoff, elen);
    }

    /// `covers` half of [`FreeList::free_replace`] (split out for rlimit).
    proof fn free_covers_replace(new: FreeList<N>, old: FreeList<N>, g: int, off: int, n: int,
        eoff: int, elen: int)
        requires
            0 <= g < old.nfree,
            new.nfree == old.nfree,
            new.free@[g].0 as int == eoff,
            new.free@[g].1 as int == elen,
            forall|k: int| 0 <= k < new.nfree && k != g ==> new.free@[k] == old.free@[k],
            n > 0,
            (eoff == old.free@[g].0 as int
                && old.free@[g].0 as int + old.free@[g].1 as int == off
                && eoff + elen == off + n)
            || (eoff == off && off + n == old.free@[g].0 as int
                && eoff + elen == old.free@[g].0 as int + old.free@[g].1 as int),
        ensures
            forall|p: int| off <= p < off + n ==> new.covers(p),
            forall|p: int| !(off <= p < off + n) ==> new.covers(p) == old.covers(p),
    {
        assert forall|p: int| off <= p < off + n implies new.covers(p) by {
            assert(new.ext_has(g, p));
        }
        assert forall|p: int| !(off <= p < off + n) implies new.covers(p) == old.covers(p) by {
            if old.covers(p) {
                let j = choose|j: int| 0 <= j < old.nfree && old.ext_has(j, p);
                if j == g {
                    assert(new.ext_has(g, p));
                } else {
                    assert(new.ext_has(j, p));
                }
                assert(new.covers(p));
            }
            if new.covers(p) {
                let k = choose|k: int| 0 <= k < new.nfree && new.ext_has(k, p);
                if k == g {
                    assert(old.ext_has(g, p) || (off <= p < off + n));
                } else {
                    assert(old.ext_has(k, p));
                }
                assert(old.covers(p) || (off <= p < off + n));
            }
        }
    }

    /// `free` two-merge case: old extents `i-1` and `i` plus the returned region
    /// fuse into one extent `E` at `i-1`; everything above `i` shifts down one.
    /// `covers` gains exactly `[off, off+n)`. Implemented as `set(i-1, E)` then
    /// `remove_at(i)`, so the correspondence is the remove-shift over a list whose
    /// `i-1` already holds `E`.
    #[verifier::spinoff_prover]
    // Sized for the worst re-verification context (doc/guidelines/verus.md §10);
    // after phase 5.1/5.2/6.2 trigger reductions the no-alloc consumption (~183k)
    // is the highest of the two contexts (alloc ~173k).
    #[verifier::rlimit(1)]
    proof fn free_both(new: FreeList<N>, old: FreeList<N>, i: int, off: int, n: int,
        eoff: int, elen: int)
        requires
            old.wf(),
            1 <= i < old.nfree,
            new.len == old.len,
            new.nfree == old.nfree - 1,
            new.free@.len() == N,
            new.free@[i - 1].0 as int == eoff,
            new.free@[i - 1].1 as int == elen,
            forall|k: int| 0 <= k < i - 1 ==> new.free@[k] == old.free@[k],
            forall|k: int| i - 1 < k < new.nfree ==> new.free@[k] == old.free@[k + 1],
            elen > 0,
            n > 0,
            // E = old[i-1] ∪ [off,off+n) ∪ old[i], both joins adjacent.
            eoff == old.free@[i - 1].0 as int,
            old.free@[i - 1].0 as int + old.free@[i - 1].1 as int == off,
            off + n == old.free@[i].0 as int,
            eoff + elen == old.free@[i].0 as int + old.free@[i].1 as int,
        ensures
            new.wf(),
            new.spec_len() == old.spec_len(),
            forall|p: int| off <= p < off + n ==> new.covers(p),
            forall|p: int| !(off <= p < off + n) ==> new.covers(p) == old.covers(p),
    {
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree - 1 implies
            (new.free@[k].0 as int + new.free@[k].1 as int) < new.free@[k + 1].0 as int by {
            if k + 1 < i - 1 {
                assert((old.free@[k].0 as int + old.free@[k].1 as int) < old.free@[k + 1].0 as int);
            } else if k == i - 2 {
                assert((old.free@[i - 2].0 as int + old.free@[i - 2].1 as int)
                    < old.free@[i - 1].0 as int);
            } else if k == i - 1 {
                // E.end == old[i].end < old[i+1].start == new[i].start.
                old.lemma_chain(i, i + 1);
            } else {
                assert((old.free@[k + 1].0 as int + old.free@[k + 1].1 as int)
                    < old.free@[k + 2].0 as int);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].1 > 0 by {
            if k < i - 1 {
                assert(old.free@[k].1 > 0);
            } else if k > i - 1 {
                assert(old.free@[k + 1].1 > 0);
            }
        }
        assert forall|k: int| #![trigger new.free@[k].0, new.free@[k].1] 0 <= k < new.nfree implies
            new.free@[k].0 as int + new.free@[k].1 as int <= new.len as int by {
            assert(old.free@[i].0 as int + old.free@[i].1 as int <= old.len as int);
            if k < i - 1 {
                assert(old.free@[k].0 as int + old.free@[k].1 as int <= old.len as int);
            } else if k > i - 1 {
                assert(old.free@[k + 1].0 as int + old.free@[k + 1].1 as int <= old.len as int);
            }
        }
        assert(new.wf());
        Self::free_covers_both(new, old, i, off, n, eoff, elen);
    }

    /// `covers` half of [`FreeList::free_both`] (split out for rlimit).
    #[verifier::spinoff_prover]
    proof fn free_covers_both(new: FreeList<N>, old: FreeList<N>, i: int, off: int, n: int,
        eoff: int, elen: int)
        requires
            old.wf(),
            1 <= i < old.nfree,
            new.nfree == old.nfree - 1,
            new.free@[i - 1].0 as int == eoff,
            new.free@[i - 1].1 as int == elen,
            forall|k: int| 0 <= k < i - 1 ==> new.free@[k] == old.free@[k],
            forall|k: int| i - 1 < k < new.nfree ==> new.free@[k] == old.free@[k + 1],
            eoff == old.free@[i - 1].0 as int,
            old.free@[i - 1].0 as int + old.free@[i - 1].1 as int == off,
            off + n == old.free@[i].0 as int,
            eoff + elen == old.free@[i].0 as int + old.free@[i].1 as int,
        ensures
            forall|p: int| off <= p < off + n ==> new.covers(p),
            forall|p: int| !(off <= p < off + n) ==> new.covers(p) == old.covers(p),
    {
        assert forall|p: int| off <= p < off + n implies new.covers(p) by {
            assert(new.ext_has(i - 1, p));
        }
        assert forall|p: int| !(off <= p < off + n) implies new.covers(p) == old.covers(p) by {
            if old.covers(p) {
                let j = choose|j: int| 0 <= j < old.nfree && old.ext_has(j, p);
                if j < i - 1 {
                    assert(new.ext_has(j, p));
                } else if j == i - 1 || j == i {
                    // p ∈ old[i-1] ∪ old[i] ⊆ E (the carved [off,off+n) is excluded).
                    assert(new.ext_has(i - 1, p));
                } else {
                    assert(new.ext_has(j - 1, p));
                }
                assert(new.covers(p));
            }
            if new.covers(p) {
                let k = choose|k: int| 0 <= k < new.nfree && new.ext_has(k, p);
                if k < i - 1 {
                    assert(old.ext_has(k, p));
                } else if k == i - 1 {
                    // p ∈ E = old[i-1] ∪ [off,off+n) ∪ old[i].
                    assert(old.ext_has(i - 1, p) || (off <= p < off + n) || old.ext_has(i, p));
                } else {
                    assert(old.ext_has(k + 1, p));
                }
                assert(old.covers(p) || (off <= p < off + n));
            }
        }
    }

    /// Return `[off, off+n)` to the free list, merging with adjacent extents.
    /// Caller's contract: the region is in-pool and currently NOT free (it was
    /// handed out by [`FreeList::alloc`]) — so the merges are adjacency-only,
    /// never overlap. Ensures the region is free again, coverage elsewhere
    /// unchanged — the reuse a revoke-then-reclaim loop turns on.
    ///
    /// Structured for verifiability (`copy_within` has no Verus model): the merge
    /// result is computed from *original* indices — left/right merges
    /// are independent (both pivot on `off+n` being the merged region's end) — so
    /// the surgery is one in-place `set` (single merge), a `set`+`remove_at` (both
    /// merges), or one `insert_at` (no merge), rather than three nested
    /// `copy_within`s.
    #[verifier::spinoff_prover]
    pub fn free(&mut self, off: usize, n: usize)
        requires
            old(self).wf(),
            // The list is not full — the `nfree < MAX_FREE_RANGES` overflow guard as a
            // static precondition. Only the no-merge case grows the list; the merge
            // cases keep or shrink it.
            old(self).spec_nfree() < N as int,
            n > 0,
            off as int + n as int <= old(self).spec_len(),
            forall|p: int| off <= p < off + n ==> !old(self).covers(p),
        ensures
            final(self).wf(),
            final(self).spec_len() == old(self).spec_len(),
            forall|p: int| off <= p < off + n ==> final(self).covers(p),
            forall|p: int| !(off <= p < off + n)
                ==> final(self).covers(p) == old(self).covers(p),
    {
        broadcast use vstd::array::group_array_axioms;
        let mut i: usize = 0;
        while i < self.nfree && self.free[i].0 < off
            invariant
                self.wf(),
                self.len == old(self).len,
                self.nfree == old(self).nfree,
                self.free@ == old(self).free@,
                0 <= i <= self.nfree,
                forall|k: int| #![trigger self.free@[k].0] 0 <= k < i ==> self.free@[k].0 < off,
            decreases self.nfree - i,
        {
            i += 1;
        }
        // Geometry: the returned region cannot overlap a free extent (precondition),
        // so the left neighbour ends at/before off and the right neighbour starts
        // at/after off+n. Merges are the equality (adjacency) cases.
        proof {
            // self was never mutated by the search, so reason via old(self).
            if i > 0 {
                // old[i-1].0 < off (loop invariant); off can't be inside old[i-1]
                // (precondition), so old[i-1] ends at or before off.
                assert((old(self).free@[i as int - 1].0 as int) < off as int);
                assert((old(self).free@[i as int - 1].0 as int
                        + old(self).free@[i as int - 1].1 as int) <= off as int) by {
                    if (old(self).free@[i as int - 1].0 as int
                            + old(self).free@[i as int - 1].1 as int) > off as int {
                        assert(old(self).ext_has(i as int - 1, off as int));
                        assert(old(self).covers(off as int));
                        assert(!old(self).covers(off as int));
                    }
                }
            }
            if i < self.nfree {
                // Loop exit: old[i].0 >= off; the precondition forbids it being < off+n.
                assert(off as int <= old(self).free@[i as int].0 as int);
                assert(old(self).free@[i as int].0 as int >= off + n) by {
                    if (old(self).free@[i as int].0 as int) < off + n {
                        assert(old(self).ext_has(i as int, old(self).free@[i as int].0 as int));
                        assert(old(self).covers(old(self).free@[i as int].0 as int));
                        assert(!old(self).covers(old(self).free@[i as int].0 as int));
                    }
                }
            }
        }
        let left = i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == off;
        let right = i < self.nfree && self.free[i].0 == off + n;
        let ghost old_self = *old(self);
        if !left && !right {
            self.insert_at(i, (off, n));
            proof {
                Self::free_insert(*self, old_self, i as int, off as int, n as int);
            }
        } else if left && !right {
            let foff = self.free[i - 1].0;
            let flen = self.free[i - 1].1 + n;
            self.free[i - 1] = (foff, flen);
            proof {
                Self::free_replace(*self, old_self, (i - 1) as int, off as int, n as int,
                    foff as int, flen as int);
            }
        } else if !left && right {
            let foff = off;
            let flen = n + self.free[i].1;
            self.free[i] = (foff, flen);
            proof {
                Self::free_replace(*self, old_self, i as int, off as int, n as int,
                    foff as int, flen as int);
            }
        } else {
            let foff = self.free[i - 1].0;
            let flen = self.free[i - 1].1 + n + self.free[i].1;
            self.free[i - 1] = (foff, flen);
            self.remove_at(i);
            proof {
                Self::free_both(*self, old_self, i as int, off as int, n as int,
                    foff as int, flen as int);
            }
        }
    }
}

/// **Disjointness ∀.** Two buffers carved by successive [`FreeList::alloc`]s
/// are disjoint — for *all* sizes and alignments. A pure corollary of `alloc`'s
/// contract: `fl1` is the pool *after* the first carve, so `alloc`'s ensures give
/// `![a]` covered in `fl1` (the first buffer's region was freshly used) yet the
/// second buffer's region was carved from still-covered `fl1` space — a shared
/// position would have to be both covered and not, so the intervals cannot overlap.
proof fn lemma_two_allocs_disjoint<const N: usize>(
    fl1: FreeList<N>,
    a: int,
    na: int,
    b: int,
    nb: int,
)
    requires
        na > 0,
        nb > 0,
        // alloc #1 ensured this over its returned [a, a+na) (`!final.covers`, final = fl1).
        forall|p: int| a <= p < a + na ==> !fl1.covers(p),
        // alloc #2 required this over its returned [b, b+nb) (`old.covers`, old = fl1).
        forall|p: int| b <= p < b + nb ==> fl1.covers(p),
    ensures
        a + na <= b || b + nb <= a,
{
    if a + na > b && b + nb > a {
        // The intervals would overlap; max(a, b) is an integer in both, so it is
        // simultaneously covered (it lies in [b, b+nb)) and not (it lies in
        // [a, a+na)) — a contradiction, so this branch is vacuous.
        let p: int = if a >= b { a } else { b };
        assert(fl1.covers(p));
        assert(!fl1.covers(p));
    }
}

/// **Mechanical teeth for [`lemma_two_allocs_disjoint`].** Drives two real
/// [`FreeList::alloc`] carves and threads their actual `ensures` — the first's
/// `!covers` over its freshly-used region, the second's `covers` over the region
/// it carved from that same post-first state — into the lemma, discharging its
/// `requires` from code rather than from a doc comment. A drift in `alloc`'s
/// coverage `ensures` breaks verification here, which the `dma-pool`/`urt`
/// runtime wrapper-corollary proptests cannot catch.
#[allow(dead_code)] // run by the `two_allocs_disjoint` host test; the verify/normal
                    // build cfg's that caller out (cf. the `len` field above).
fn two_allocs_disjoint_teeth<const N: usize>(
    fl: &mut FreeList<N>,
    na: usize,
    align_a: usize,
    nb: usize,
    align_b: usize,
)
    requires
        old(fl).wf(),
        na > 0,
        nb > 0,
        align_a > 0,
        align_b > 0,
{
    let r1 = fl.alloc(na, align_a);
    let ghost fl1 = *fl; // post-carve-#1 == pre-carve-#2 state
    let r2 = fl.alloc(nb, align_b);
    if r1.is_some() && r2.is_some() {
        // Both carves succeeded; `a`/`b` are their returned offsets, ghost since
        // they feed only the proof. alloc #1's `!final.covers` over [a, a+na) and
        // alloc #2's `old.covers` over [b, b+nb) are exactly the lemma's premises,
        // both about the shared snapshot `fl1`.
        let ghost a = match r1 {
            Some(a) => a as int,
            None => 0,
        };
        let ghost b = match r2 {
            Some(b) => b as int,
            None => 0,
        };
        proof {
            lemma_two_allocs_disjoint::<N>(fl1, a, na as int, b, nb as int);
        }
        assert(a + na as int <= b || b + nb as int <= a);
    }
}

} // verus!

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessor_sanity() {
        // is_full flips at the cap; is_allocated flips across a free/alloc of a region.
        // (N is a concrete cap here; dma-pool's MAX_FREE_RANGES stayed in dma-pool.)
        let mut fl = FreeList::<64>::new(128);
        assert!(!fl.is_full());
        let start = fl.alloc(16, 1).unwrap();
        assert!(fl.is_allocated(start, 16)); // just carved -> allocated
        fl.free(start, 16);
        assert!(!fl.is_allocated(start, 16)); // freed -> not allocated
    }

    #[test]
    fn two_allocs_disjoint() {
        // Run the verified teeth helper through a real double carve so it is
        // reached by real code (the verify build cfg's this caller out).
        let mut fl = FreeList::<64>::new(128);
        two_allocs_disjoint_teeth::<64>(&mut fl, 16, 8, 32, 16);
    }
}
