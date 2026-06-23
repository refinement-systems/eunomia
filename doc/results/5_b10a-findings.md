# B10A — verified `grow_pool` monotone pool extension (findings)

Working notes from the implementation of **Phase B10A** (`doc/plans/10_b10-detail.md`,
sub-phase B10A — the Verus core). Records what landed, the one place the realization
diverged from the plan's prose, the proof shape, and the verification facts worth
keeping. Closes the **verification core** of audit **M-2** (aspace pool top-up);
conforms rev1§2.5 ("accepts top-ups"). B10B (the `AspaceTopUp` syscall + abutment
carve) and B10C (teardown/accounting tests) are follow-ons that consume B10A's lemma.

---

## 0. Headline

All B10A gates green:

- `cargo verus verify -p kcore` — **384 verified, 0 errors** (was 381; +3 = the three
  new proof fns below). No existing proof, spec fn, or trusted seam changed; the 7
  `external_body` + 6 `assume_specification` seams are untouched.
- `cargo test -p kcore` — **105 passed** (103 prior + 2 new `grow_pool` host tests).
- `cd kernel && cargo build` (aarch64-none-softfloat) — green; the shell `grow_pool`
  wrapper compiles. (Only the 3 pre-existing unused-import warnings in `ready.rs`/
  `timer.rs` remain — not from this change.)

## What landed

- `kcore/src/aspace.rs` — three new `proof fn`s in the tree-lemma region (right after
  `lemma_link_l1_lookup`):
  - `lemma_pool_index_widen` — `pool_index_spec`'s accept set only widens with
    `pool_len`: a descriptor resolving in-range under `old_len` resolves to the **same**
    index under any `new_len >= old_len`. The computed offset `(pa-pool_base)/PAGE` is
    `pool_len`-independent; only the `>= pool_len` bound uses it. Empty proof body —
    `pool_index_spec` is `closed` but transparent within its own module, so the SMT
    solver discharges it directly.
  - `lemma_grow_pool` — the monotone-widening theorem: appending zeroed tables and
    growing `pool_len` (with `l1`, `pool_used`, the leaf partition, and the first
    `old_len` tables fixed) preserves `pt_wf` **and changes no `pt_lookup`**. `pub`, so
    B10B/B10C and the shell can cite it.
  - `lemma_grow_pool_lookup` — the per-VA core of the lookup-stability frame.
- `kernel/src/aspace.rs` — `pub unsafe fn grow_pool(this, add)`: the trusted int→ptr
  shell wrapper (zero the `add` fresh tables at `pool_base + old_len*PAGE`, bump
  `pool_pages`). `#[allow(dead_code)]` until B10B's `Sys::AspaceTopUp` handler calls it.
- `kcore/src/test_store.rs` — `map_in_grow_pool_continues` (exhaust → grow → map
  succeeds; the M-2 functional acceptance) and `map_in_grow_pool_lookup_stable` (a
  pre-grow mapping resolves to the identical leaf/PTE after grow, and stays stable after
  a new map into the grown tail).
- `doc/guidelines/verus_trusted-base.md` — scope paragraph names `grow_pool`; kcore
  baseline 381 → 384.

## Finding 1 — the verified artifact is a proof lemma, not an exec op

The plan prose says the shell "calls the verified kcore op." In the actual page-table
model that is not the right shape, and the realization is a **`proof fn`** instead:

- Pool growth is a **slice-length change**. The trusted shell's `pool_view`
  (`kernel/src/aspace.rs`) rebuilds `&mut [[u64;512]]` from the *current* `pool_pages`
  on every call, so after the shell bumps `pool_pages` the existing `map`/`unmap` path
  automatically sees the larger pool — there is **no map-path edit and no runtime work
  for kcore to do**. An exec fn over `&mut [[u64;512]]` cannot change a slice's length
  anyway.
- So the verified deliverable is `lemma_grow_pool`: the machine-checked justification
  that the shell's growth keeps `map_in`'s `pt_wf` precondition true. It is the same
  verified content the plan intended ("a small verified kcore op … proving the extension
  keeps `pt_wf` and every mapping intact"), realized faithfully and counted in the gate;
  there is simply no exec stub.

Consequence for B10B: the handler does the int→ptr work (zero + bump `pool_pages` via
the shell `grow_pool`) and the soundness rests on `lemma_grow_pool` — exactly the
verified/trusted split `map_in` already uses (the shell `map` is unverified; `map_in` is).

## Finding 2 — the proof is a near-verbatim reuse of `lemma_link_l1`

`lemma_grow_pool`/`lemma_grow_pool_lookup` are modeled directly on the existing
`lemma_link_l1`/`lemma_link_l1_lookup` pair — same `choose` of the `leaves` witness,
same one-`assert forall … implies … by {}`-per-clause structure over `pt_wf_leveled`'s
(a)/(b1)/(b2)/(c1)/(c2), same per-`w` lookup helper. This is the concrete reason B10 is
M/medium and not a page-table-model rewrite: the `pt_wf` invariant is *already*
quantified over `pool_len`, and the walker / `pool_index_spec` / `alloc_table` /
`map_in` are reused **unchanged**.

The **one** genuinely new ingredient the link lemmas lack: the two states being related
have **different `pool.len()`** (`old_len` vs `new_len`). In `lemma_link_l1_lookup` both
walks share one `pl` (it asserts `pool.len() == pooln.len()` to align the two
`pool_index_spec` calls). Here that alignment does *not* hold, so every level of the
walk bridges `pool_index_spec(.., old_len, .)` to `pool_index_spec(.., new_len, .)` via
`lemma_pool_index_widen`. The bridge is sound because every live descriptor targets an
index `< pool_used <= old_len`, so it is in-range under both lengths and resolves to the
identical value. All five `pt_wf` clauses, plus the lookup walk, went through with no
arithmetic beyond that one widening lemma — and the widening lemma needed **no** proof
body. No `nonlinear_arith`/`bit_vector` was required (unlike `lemma_desc_roundtrip`).

`pool_geom_ok` is intentionally **not** a `requires` of `lemma_grow_pool`: neither
`pt_wf` nor `pt_lookup` references it. `map_in`'s separate `pool_geom_ok` requirement is
discharged by the trusted shell (B10B's abutment carve), not by the growth lemma.

## Finding 3 — host test: model the contiguous extension with `Vec::extend`

The host tests reuse the existing `map_fixture(npools)` helper. A 2-table pool is
exactly one 1-page map (L2 + L3); a second map into a different **1 GiB** region
(`va + (1<<30)`, a distinct `spec_l1_index`) needs a fresh L2 + L3, so it returns
`NeedMemory` on the full pool. Growth is modeled by
`pool.extend(core::iter::repeat([0u64;512]).take(n))` — precisely what `pool_view`
reconstructs after the shell bumps `pool_pages` — after which the same map succeeds.
Useful incidental fact confirmed by the test: when the pool is **exactly** full
(`pool_used == pool_len`), a failing `map_in` allocates nothing (`pool_used` unchanged),
because `alloc_table` returns `NeedMemory` on its first call — so the test needs no
assumption about partial-allocation rollback.

## Verification facts

- Verus pin unchanged (`doc/guidelines/verus.md`): Verus `0.2026.06.07.cd03505`, vstd
  `=0.0.0-2026-05-31-0205`, rust `1.95.0`. Each new `proof fn` adds exactly 1 to the
  `cargo verus verify -p kcore` count (381 → 384).
- `closed spec fn` (`pool_index_spec`, `pt_wf`, `pt_wf_leveled`, `pt_lookup`) are opaque
  *outside* their module but transparent *within* it — so the new in-module lemmas
  reason over their bodies with no `reveal`, matching the existing aspace lemmas.
- The trigger notes Verus prints during the run are pre-existing (`channel.rs:1557`,
  `cspace.rs:9416`), not from the new lemmas; the new `assert forall` blocks all carry
  explicit `#![trigger …]` annotations mirroring `pt_wf_leveled`'s.
