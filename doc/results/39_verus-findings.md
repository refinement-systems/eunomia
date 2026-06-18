# Verus findings 19 — Phase 5d: `map_in` (the two-pass walk-allocate)

Plan: `doc/plans/3_verus-rewrite.md` (§4.5 + §7 step 5) and its decomposition
`doc/plans/3_verus-rewrite_phase5-detail.md` (§5d). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`38` (phase 5a–5c — the sysabi
decoder, the §2.5 PTE isolation theorem, and `range_mapped_in` + the page-table
partial-map model). This is the **fourth** sub-phase of phase 5 and the one the detail
plan flagged as the hardest and chief risk: `map_in`, the §4.5 address-space mapper, with
its **tree-shape no-aliasing** frame — the analog of the channel FIFO (3d) and the waiter
queue (4b), but the **first verified code that mutates concrete Rust slices** (5c was
read-only). 5e (`unmap_in` + the TLBI effect-log + closeout) is the remaining sub-phase.

**Outcome.** `cargo verus verify -p kcore`: **214 verified, 0 errors** (was 166 after 5c;
`+48`). The `+48`: the refined model (`pt_wf_leveled`, `pt_leaf_slot`, `pt_wf_leaf`,
`spec_pte_encode`, `pg`, `pool_geom_ok`), the ported exec ops (`pa_of_table`, `alloc_table`,
`walk_alloc`, `map_in`), and ~14 proof lemmas (the two link lemmas + their per-`w` cores,
the descriptor round-trip, the tree-shape distinctness theorem, the leaf-write frame, the
two-pass step lemma, the per-page range/alignment lemmas). `cargo test -p kcore`: **62
passed** (was 55; `+7` — the first executable `map_in` host checks: single/multi-page, the
L2-carry, atomic `AlreadyMapped`, `NeedMemory`, RO-rejects-write, and a randomized
multi-range sweep asserting the no-clobber frame at scale). The aarch64 `kernel`
cross-build is unchanged (ghost erasure; `map_in`/`pte_encode` public signatures untouched —
only `ensures`/ghost added, plus the presence-test refinement below).

**5d adds no `external_body`.** `pa_of_table`/`alloc_table`/`walk_alloc`/`map_in` and every
lemma are **fully proven** — nothing assumed. Phase 5 stays the first phase since phase 2 to
add zero trusted residue. **No termination theorem** (detail §1.3): `walk_alloc` is
straight-line, both passes are bounded `for`-loops; Verus discharges termination
automatically.

---

## 1. What closed

- **The §4.5 `pt_wf` level refinement — the model change `map_in` needed.** The 5c `pt_wf`
  quantified closure over *all* used tables, which is **unsatisfiable once a real mapping is
  installed**: a leaf (L3) PTE has `DESC_PAGE == DESC_TABLE == 0b11`, so its frame-PA address
  field would (wrongly) be required to resolve back into the pool. The fix is a **level
  partition** (`pt_wf_leveled` carries a `leaves: Set<nat>` of L3 table indices; the rest of
  `[0, pool_used)` are L2 tables) — closure and no-aliasing apply to the **inner (L2)** tables
  only; leaf tables hold frame PTEs and are unconstrained. The partition is **existentially
  quantified inside `pt_wf`**, so the public signature stays
  `(l1, pool, pool_base, pool_used, pool_len)` and no ghost arg leaks to the kernel shell
  (which calls `map_in` with no `leaves`). This is the load-bearing invariant — the §4.5
  analog of CDT acyclicity.

- **`map_in` — the full §4.5 contract, ∀ `(pa, va, pages, perms)`** (against `pt_wf`):
  - `!va_range_ok ⇒ Err(BadVa)`; pool exhaustion ⇒ `Err(NeedMemory)`; any already-mapped page
    ⇒ `Err(AlreadyMapped)` with **no leaf written** (pass 1 walks/checks before pass 2 writes);
  - **the two-pass theorem:** because pass 1 walked and allocated the tables along the whole
    range, every pass-2 `walk_alloc` finds them present and **allocates nothing** (the new
    `walk_alloc` ensure `pt_lookup(old, va) is Some ⇒ r is Ok`) — pass 2 cannot return
    `NeedMemory`;
  - on `Ok`: ∀ `i < pages`, `pt_lookup(va + i·PAGE) == Some(spec_pte_encode(pa + i·PAGE, perms))`
    (**adds exactly the requested pages**); `pt_wf` preserved; `pool_used` monotone;
  - **the no-overwrite frame:** every page mapped (nonzero) before is preserved — *not* "every
    `pt_lookup` literally unchanged": creating a fresh table legitimately flips a sibling VA's
    `pt_lookup` from `None` to `Some(0)` (still unmapped), but **no nonzero PTE is ever
    clobbered**.

- **`walk_alloc` — the linchpin**, proven against the model via two bespoke link lemmas:
  - `lemma_link_l1` (link a fresh, zeroed L2 table into an empty L1 slot): preserves `pt_wf`
    and **changes no `pt_lookup`** (the new table is empty, so a walk that enters it dead-ends);
  - `lemma_link_l2` (link a fresh L3 leaf into an empty inner-table slot): preserves `pt_wf`
    (`pu` joins `leaves`) and **frames every present page** — the freshness argument (the new
    index `pu` is distinct from every existing descriptor target, all `< pu` by closure) is
    what re-establishes both injectivity clauses without per-pair reasoning.
  - `walk_alloc` exposes `pt_leaf_slot(va) == Some((l3, e))` and `pt_wf_leaf(…, l3)` (the L3
    table is a leaf) so `map_in`'s leaf write can preserve `pt_wf` (a frame PTE must land in a
    leaf table, never an inner one — the same `DESC_PAGE == DESC_TABLE` hazard).

- **`lemma_distinct_pages_slots` — "the page table is a tree, not a DAG" (the chief 5d
  theorem).** Two distinct page-aligned user VAs resolve to **distinct** leaf slots; the proof
  runs the no-aliasing clauses backwards (equal slots force, via `(c2)`, the same L2 table +
  entry, then via `(c1)` the same L1 entry → equal index triple → equal VA, contradiction).
  This is what makes `map_in`'s per-page leaf writes non-interfering and the no-clobber frame
  go through.

- **`lemma_leaf_write` — the leaf-write frame.** Writing a frame PTE into the leaf slot `va`
  resolves to preserves `pt_wf` (the slot is in a leaf table, excluded from closure/no-aliasing)
  and frames every page resolving to a *different* slot — a slot-based locality argument that
  `map_in` combines with the distinctness theorem.

- **`pa_of_table` round-trip + `pool_geom_ok`.** A table descriptor built from
  `pa_of_table(idx)` resolves back to exactly `idx` (`lemma_desc_roundtrip`); the geometry
  precondition (`pool_base` page-aligned, the whole pool inside the 48-bit output-address
  field) is the kernel shell's by-construction property, now a `requires`.

- **`barrier_after_map` `ExStore` contract** (the one Store-seam touch, §1.4): a trivial
  all-views-framed `ensures`. Because it takes neither page-table slice, `map_in`'s page-table
  postcondition is independent of it; it exists only to make the call legal in verified code.

---

## 2. Verus mechanics worth keeping

- **The presence test moved from `== 0` to `& DESC_TABLE != DESC_TABLE`.** `walk_alloc` (and
  thus `map_in`) now tests the descriptor *tag*, matching `lookup`/`pt_lookup`. For a
  well-formed table (entries are `0` or table descriptors — the only states the kernel builds)
  this is **identical** to the old `== 0`; it also makes the walker total (a non-descriptor is
  treated as absent and re-allocated rather than chased). A behaviour-preserving refinement,
  not a behaviour change, for the kernel's inputs.

- **The no-aliasing frame is provable by *freshness*, not pairwise case-split.** `pt_wf`'s
  injectivity clauses `(c1)`/`(c2)` are 2- and 4-variable quantifiers, but **preserving** them
  under a link never needs pairwise reasoning: the freshly allocated index equals the old
  high-water mark `pu`, and every existing descriptor target is `< pu` by closure, so the new
  target is distinct from all of them — a single bounded fact. The 4-variable `(c2)` only gets
  *instantiated at concrete slots* inside `lemma_distinct_pages_slots`; it never auto-fires
  destructively. This is what kept the tree-shape proof tractable.

- **`< identifier` in an `assert … by(…) requires` clause hits the generic-parse ambiguity.**
  `requires idx < pool_len;` fails to parse (the `<` opens a turbofish-style argument list),
  while `va < 0x80…` (a literal RHS) and `pool_len > idx` (flipped) parse fine. Flip bare `<`
  comparisons to `>` in `bit_vector`/`nonlinear_arith` `requires`. (Function-level
  `requires`/`ensures` are unaffected.)

- **The `usize → u64` cast bridge needs the value bound in `u64` form.** `(x as usize) == (y
  as usize) ⇒ x == y` for `x, y: u64` only discharges once Verus has `x < 512` / `y < 512` as
  **`u64`** facts (a one-line `by (bit_vector)`); the `spec_*_index < 512` (a `usize` fact) is
  not enough. Needed to bridge the `spec_l*_index` equalities to the raw bit-fields in the
  tree theorem.

- **Facts established *before* a loop are invisible *inside* it — `old()` bridges must be loop
  invariants.** `let ghost pl = pool.len()` and `pt_lookup`-frame facts proven before a `while`
  do **not** survive into the body (Verus cuts the context to the invariant). The function
  postconditions (`final(pool).len() == old(pool).len()`, the nonzero frame in terms of
  `old(l1)@`) therefore failed at *early-exit returns inside the loop* until
  `pool.len() == old(pool).len()`, `l1_0 == old(l1)@`, `pool_0 == old(pool)@`, and
  `*pool_used >= *old(pool_used)` were added **as loop invariants**. `old()` *is* usable in a
  loop invariant (it means function entry). The single subtlest debugging point of 5d.

- **The early-exit frame composes `S0 → loop-head → post-`walk_alloc``.** A `?`/`return`
  *after* `walk_alloc` mutated the tables needs the no-clobber frame re-established for the
  current state: the loop invariant gives `S0 → head` (present-preserved) and `walk_alloc`'s
  frame gives `head → current`, composed in an explicit `assert forall` at the return.
  `walk_alloc`'s frame was strengthened from "nonzero preserved" to "**present (`Some`)
  preserved**" so the pass-1 `Some(0)` invariant survives table allocation.

- **`==>` vs `implies` in `assert forall`.** `assert forall|w| P(w) ==> Q(w) by {…}` does *not*
  assume `P(w)` in the body (Verus warns); use `implies` when the body needs the antecedent.

---

## 3. What 5d does **not** touch (carried forward)

Per detail §1.5 / §4, 5d ports the `map_in` machinery and the tree model; it adds no
`external_body` and no cross-object teardown. Still ahead:

- **5e** — `unmap_in` (the read-mostly leaf-clear counterpart) + the §4.5 TLBI/barrier
  effect-ordering ghost log on the `ExStore` seam (`tlb_invalidate_page`/`barrier_after_unmap`,
  still `unimplemented!()` in `ArrayStore`) + the phase-5 closeout (the `CLAUDE.md` `### Verus`
  / §6-tier-table update covering 5a–5e at once; the already-discharged §7-step-5 clauses; the
  reaffirmed, now-unblocked cross-object-teardown phase).

The cross-object teardown and the full `refcount_sound` census remain the recommended
dedicated phase **after** phase 5 (unblocked once aspace's walker is ported, §1.5).
