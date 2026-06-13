# Kani verification findings — part 10 (strengthen `check_dma_alloc_disjoint`)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `11_kani-findings-9.md`.
This part implements recommendation #4 of the conformance review
(`9_kani-review.md`): *strengthen or retire `check_dma_alloc_disjoint`* — the
thinnest harness in the suite, which proved disjoint / in-pool / bijection /
alignment for **one** concrete pair of allocations (sizes 5 and 4), "only
marginally beyond the existing unit test." The standing caveat and design notes
(DN-1…DN-13) apply unchanged.

## Outcome: strengthened (the "for all sizes" branch landed)

The harness now has two parts (`dma-pool/src/proofs.rs`):

- **Part 1 — for *all* first-allocation sizes** (the new content). `len1 =
  kani::any()` (unbounded `usize`), one `alloc(len1, 1)` on a fresh pool, then:
  a reject happens **iff** `len1 == 0 || len1 > POOL`; an accepted buffer sits at
  offset 0, is in-pool (`offset + len ≤ POOL`), returns exactly `len1`, and
  carries the exact bijection `device_addr == device_base + offset`. The
  allocator arithmetic is proven panic/overflow-free over the whole `usize`
  domain. Three `kani::cover!`s (`len1 == 0`, `len1 == POOL`, `len1 > POOL`)
  guard the empty-reject, whole-pool-accept, and won't-fit-reject paths against
  vacuity (and ride the rec-#3 CI cover-guard).
- **Part 2 — concrete carve-and-split** (retained): `alloc(5,1)` then
  `alloc(4,4)` exercises the alignment round-up (`start 5 → 8`) and proves the
  two live buffers are disjoint, in-pool, aligned, and bijective.

`VERIFICATION: SUCCESSFUL`, **3/3 covers SATISFIED, ~0.5 s.** `check_dma_free_reuse`
(unchanged) still verifies; the full `cargo kani -p dma-pool` cover tally is
`N == M` (the rec-#3 CI guard passes). No defect found.

## Why Part 1 is tractable but a fully-symbolic version is not (DN-10, refined)

DN-10 had recorded that symbolic-size dma allocation OOMs CBMC. Reading
`DmaPool::alloc` (`dma-pool/src/lib.rs`) pinned down *exactly* where the wall is,
and that a slice of "for all sizes" sits safely on this side of it:

- A **single** `alloc(len1, 1)` on a **fresh** pool reads only the *concrete*
  free entry `(0, POOL)`: `start = (0+0) & !0 = 0`, `pad = 0`, `device_addr =
  base + 0`. So the symbolic `len1` feeds just the `pad + len1 > flen` →
  `len1 > POOL` boundary compare (no overflow — the add is `0 + len1`). Tiny SAT;
  verifies in ~0.5 s even with `len1` fully 64-bit-symbolic.
- A **second** alloc is the wall. After the first carve the free entry becomes
  `(len1, POOL - len1)` — now *symbolic*. The second alloc re-reads it and
  computes the round-up `(off + align - 1) & !(align - 1)` over that symbolic
  `off`; bit-blasting that 64-bit mask-and-add OOMs CaDiCaL (measured here:
  `Runtime Post-process 87 s` then out-of-memory, even with `len1` *value*-bounded
  to `< 2·POOL` — the constraint shrinks the value, not the bit-width). This is
  the same blow-up DN-10 saw for a symbolic *alignment*; the symbolic offset
  reaches the identical mask.

So the round-up arithmetic and two-buffer disjointness stay a **representative
concrete** pair (Part 2); "for all sizes" disjointness of two live buffers
remains owned by the unit tests + proptest. The DN-10 bullet in
`8_kani-findings-7.md` is updated with this refinement.

## What this buys over the old harness

The old harness proved the four invariants at exactly `(5, 4)`. Part 1 now
proves the **accept/reject boundary, in-pool, exact-length, and bijection for
every `usize` size**, plus arithmetic-safety over the whole domain — the
"for all" content the review wanted, and a genuine step past the concrete unit
test. The alignment/disjointness round-up that genuinely needs a symbolic offset
is left concrete *and labelled as such*, per the review's "strengthen **or** say
plainly it's representative" — here it is strengthened where tractable and
honest where not.

## Status of recommendation #4

✅ Strengthened. `check_dma_alloc_disjoint` gained real for-all-sizes coverage
(boundary + in-pool + bijection + totality) at ~0.5 s; the symbolic-offset
round-up stays concrete with the precise DN-10 reason recorded. No allocator
logic changed — harness + docs only.
