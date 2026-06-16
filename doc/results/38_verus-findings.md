# Verus findings 18 — Phase 5c: `range_mapped_in` + the page-table partial-map model

Plan: `doc/plans/3_verus-rewrite.md` (§4.5 + §7 step 5) and its decomposition
`doc/plans/3_verus-rewrite_phase5-detail.md` (§5c). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36` (phase 5a — the sysabi decoder),
`37` (phase 5b — the §2.5 PTE isolation theorem). This is the **third** sub-phase of
phase 5 and the one the detail plan flagged as the chief design risk: the **first kcore
Verus reasoning over concrete Rust slices**, landing the new **page-table partial-map
model** (`pool_index_spec`/`pt_lookup`/`pt_wf`) with its simplest, read-only consumer —
`range_mapped_in`, the predicate the syscall layer trusts before dereferencing user
pointers (`kernel/src/aspace.rs`). 5d (`map_in`) and 5e (`unmap_in`) build on the model
settled here (the 3b→3d / 4a→4b discipline).

**Outcome.** `cargo verus verify -p kcore`: **166 verified, 0 errors** (was 155 after 5b;
`+11`). The `+11`: the five model `spec fn`s (`pool_index_spec`, `pt_lookup`, `page_ok`,
`pt_wf`, `pool_index_resolves`), the three ported exec functions (`pool_index`, `lookup`,
`range_mapped_in`), and the strengthened `< 512` bounds on the three index helpers.
`cargo test -p kcore`: **55 passed** (was 49; `+6` — the first executable
`range_mapped_in` host checks: fully-mapped RW, read-only-rejects-write, a hole, a missing
L3 table, the `len == 0` / out-of-range edges, and the `va + len` overflow edge). The
aarch64 `kernel` cross-build is unchanged (ghost erasure; the public signatures of
`map_in`/`unmap_in`/`range_mapped_in` are untouched — only `ensures` were added).

**5c adds no `external_body`.** `pool_index`/`lookup`/`range_mapped_in` and the model are
**fully proven** — nothing assumed. Phase 5 stays the first phase since phase 2 to add
zero trusted residue (detail §0; 3e left `destroy_channel`/`signal`, 4e left
`destroy_tcb`). **No termination theorem of the revoke/delete kind** (detail §1.3): the
walk is fixed 3-level depth; the only `decreases` is `range_mapped_in`'s one `while` loop.

**The slice-reasoning surprise was the representation, not the algorithm — exactly as
flagged.** The model and the `range_mapped_in` loop both proved out, but three concrete
Verus-mechanics points (§2) were the actual work: the `Seq<[u64; 512]>`-not-`Seq<Seq<u64>>`
choice, the `closed`-spec-fn visibility fix, and the clamped `decreases` for a stride loop
that overshoots its bound.

---

## 1. What closed

- **`range_mapped_in` — full functional equivalence, ∀ `(va, len, write)`** (`aspace.rs`).
  The §4.5 predicate as a theorem rather than a sampled assert:
  - `len == 0 ⇒` result `== (USER_VA_BASE ≤ va < USER_VA_END)` (bare membership, no walk);
  - `len != 0` and the request overflowing or escaping `[USER_VA_BASE, USER_VA_END)` `⇒ !r`
    (the `va.checked_add(len)` overflow edge included, via `int` arithmetic in the `ensures`
    to dodge the wrap);
  - otherwise `r ⇔ ∀` page `p` in `[va & !PAGE_MASK, va+len)`, page-aligned: `page_ok(p)` —
    i.e. `pt_lookup(p)` is `Some(pte)` with `pte != 0` and (`write ⇒ (pte>>6)&0b11 == 0b01`).
    The `(pte>>6)&0b11 == 0b01` writability test is exactly the 5b `pte_encode` AP theorem
    (the deliberate bridge, doc 37 §1).

- **The page-table partial-map model** — the §4.5 ghost `Map<va_page, pte>` in pointwise
  form (the per-node idiom, doc 27 §3 / doc 29 §1), the analog of phase 3's FIFO `Seq` and
  phase 4's `waiter_seq`/`timer_seq`:
  - `pool_index_spec(pool_base, pool_len, desc)` — the descriptor → pool-index addressing
    primitive (`pool_index`'s spec mirror).
  - `pt_lookup(l1, pool, pool_base, va)` — the spec walk `l1 → l2 → l3`, returning the leaf
    PTE value (so `Some(0)` = "tables present, page empty") or `None` if any level is absent.
  - `page_ok` — the per-page present-and-(maybe)-writable predicate the `forall` ranges over.
  - `pt_wf` — the table-pool well-formedness (the `chan_wf`/`notif_wf`/`timer_wf` analog):
    **(a) accounting**, **(b) closure** (every present table descriptor resolves to a pool
    index `< pool_used`), and **(c) tree-shape / no-aliasing** (distinct present descriptors
    point to distinct pool indices). **Designed here, consumed by 5d/5e** — no 5c op
    establishes or preserves it (read-only `range_mapped_in` does not need it), so it is a
    definition only; 5d validates/refines it against `map_in` (the "add the clause when the
    op needs it" discipline, doc 27 §3). Its inner-level no-aliasing clause is deliberately
    deferred to where `map_in`'s leaf-write frame lemma pays for it.

- **`pool_index` / `lookup` ported, bridged to the model.** `pool_index` proven equal to
  `pool_index_spec`; `lookup` proven equal to `pt_lookup`, with the `l3 < pool.len() &&
  e < 512` bounds the read-only walk needs so `range_mapped_in` can index `pool[l3][e]`
  safely. Both `?` in `lookup` spelled as explicit `match`/early-return (the 5a convention
  — control flow stays in the verified fragment).

- **The index helpers gained `r < 512`.** `l1_index`/`l2_index`/`l3_index` now `ensure
  r < 512` (a one-line `by (bit_vector)` each), so every `l1[..]` / `pool[..][..]` slice
  index in `lookup`/`range_mapped_in` is in-bounds by the callee's contract. Consumed by 5d
  too.

---

## 2. Verus mechanics worth keeping

- **Model the pool over the *natural* slice view `Seq<[u64; 512]>`, not `Seq<Seq<u64>>`.**
  The detail plan's illustrative `pt_lookup` signature used `Seq<Seq<u64>>`, which would
  force a `deep_view` conversion (`&[[u64;512]]` views as `Seq<[u64;512]>`, and
  `[u64;512]: DeepView` only yields `Seq<u64>` via `deep_view`). Defining the model over
  `Seq<[u64; 512]>` (= `pool@` exactly) sidesteps that entirely: `pool@[i][j]` resolves to
  the array `spec_index` (`ArrayAdditionalSpecFns`), which is the *same* spec value as the
  exec read `pool[i][j]`, so the bridge is definitional with no `deep_view` lemma. The
  enabling broadcast is `vstd::{slice::group_slice_axioms, array::group_array_axioms}`
  (slice `len`/index + `[T;N]@.len() == N`), revealed per-function where the indexing
  happens.

- **A `pub` function whose contract names internal descriptor bits needs `closed` spec
  fns, not narrowing.** 5b resolved "a `pub` fn's `ensures` cannot name a `pub(crate)`
  const" by narrowing the encoders to `pub(crate)` (doc 37 §2). That escape is unavailable
  for `range_mapped_in`: it is **cross-crate** (the kernel shell calls it), so it must stay
  `pub`, yet its contract must mention the model, which transitively names the `pub(crate)`
  descriptor bits (`ADDR_MASK`, `DESC_TABLE`). The fix is to make the model `pub closed
  spec fn`: the *name* is public (so a `pub` `ensures` may reference it) but the *body* is
  module-private (so the internal consts never leak into the exported signature). Within
  the `aspace` module the closed bodies stay transparent to the solver, so the
  `lookup`/`range_mapped_in` proofs unfold them freely. The first use of `closed` in kcore,
  and the right tool whenever a public operation's spec is stated in internal terms. (The
  `pub open spec fn` form Verus rejected with "in pub open spec function, cannot refer to
  private const item".)

- **A stride loop that overshoots its bound needs a *clamped* `decreases`.** Every prior
  kcore `while` increments by 1 to an exact bound (`decreases 4 - c`, `decreases
  ws0.len() - k`) and never overshoots. `range_mapped_in` steps `page` by `PAGE` from a
  page-aligned start toward an arbitrary `end`, so the last iteration drives `page > end`
  and the obvious measure `end - page` goes **negative**, which the int-`decreases`
  well-foundedness rejects ("decreases not satisfied at end of loop"). The fix is a
  conditional measure `if page < end { (end - page) as int } else { 0int }`: non-negative
  always, and still strictly decreasing across the body (the overshoot iteration drops it to
  0). Worth keeping for any future page/extent stride loop.

- **`PAGE - 1` is `int` in spec position — name the `u64` mask.** A bare `PAGE - 1` in a
  spec expression is integer subtraction, so `!(PAGE - 1)` (page-align-down) and `p & (PAGE
  - 1)` (alignment test) fail to type-check (`! `/`&` are undefined on `int`). A `pub const
  PAGE_MASK: u64 = PAGE - 1` (const-eval'd to `u64`) keeps the bitwise ops `u64`. The
  alignment facts then isolate into one-line `by (bit_vector)` steps: `(va & !m) ≤ va` and
  `(va & !m) & m == 0` hold for *symbolic* `m` (no need to pin `PAGE_MASK`), while "the only
  aligned page in `[prev, prev+PAGE)` is `prev`" needs `PAGE_MASK == 4095` and `page == prev
  + 4096` pinned (the alignment stride is `2^12`-specific). The 5b "isolate the hard step"
  discipline (doc 37 §2).

- **The loop computes the `forall`; the early `return false` witnesses its failure.** The
  invariant carries "every aligned page below the cursor is `page_ok`". Fall-through (`page
  ≥ end`) gives the full `forall` (the cursor passed `end`); each early `return false`
  asserts `!page_ok(page)` for the current page, and because that `page_ok(…, page, …)` term
  triggers the postcondition's `forall`, the solver instantiates at `page` and derives the
  biconditional's `!RHS` automatically — no explicit existential introduction needed. The
  bad page is bound out of the `match` (not a guard) so `pool[l3][e]` is relatable to the
  spec leaf `pool@[l3][e]` via `lookup`'s in-bounds `ensures`. The `u64 → usize` cast in
  `pool_index` was kept lossless by comparing in `u64` *before* the cast (`off < pool_len as
  u64 ≤ usize::MAX`), so no crate-wide `global size_of usize` directive was needed.

---

## 3. What 5c does **not** touch (carried forward)

Per detail §1.5 / §4, 5c ports the read-only aspace walker and the model; it adds no
`external_body`, no `Store`-seam touch (the tables are passed-in slices), and does not
establish/preserve `pt_wf`. Still ahead in phase 5:

- **5d** — `map_in` (the two-pass walk-alloc; the tree-shape no-aliasing frame lemma — the
  load-bearing proof and chief risk; adds the `barrier_after_map` `ExStore` contract;
  consumes `pt_wf`, `lemma_user_va_l1_index`, and `pte_encode`'s isolation `ensures`).
- **5e** — `unmap_in` + the TLBI/barrier effect-ordering ghost log + the phase-5 closeout
  (the `CLAUDE.md` `### Verus` / §6-tier-table update covering 5a–5e at once; the
  already-discharged §7-step-5 clauses; the reaffirmed cross-object-teardown phase).

The cross-object teardown and the full `refcount_sound` census remain the recommended
dedicated phase **after** phase 5 (unblocked once aspace's walker is ported, §1.5).
