# B13B findings — verified prolly partition core

**Phase:** B13B of `doc/plans/14_b13-detail.md` (prolly-tree canonical-form
verification), the "stretch" sub-phase. Resolves the plan's Design decision 3 (model the
split predicate as an opaque trusted-total `is_boundary` seam, prove the partition *around*
it, never *through*). The **full core landed** — no fall-back was needed.

B13B mechanizes the *encode-side* structural property the audit named load-bearing: that
`build_level`'s node-cutting is a **lossless, ordered partition** whose blocks are cut only
at a content-defined boundary or the `MAX_NODE_ENTRIES` cap, and are non-empty and ≤ MAX —
**for any predicate**, so it holds under the real (BLAKE3) `is_boundary` without the proof
ever modeling BLAKE3 (the Hash-free half of rev1§4.1's "tree shape is a function of the
contents"). The hash-determined *concrete shape* (which contents map to which root hash)
stays test-routed at the rev1§6 baseline tier — explicitly so, in the module doc and ledger.

It is **format-stable** (the verified cut points are byte-for-byte the ones `build_level`
used before B13B — confirmed by the unchanged `canonical_form`/`roundtrip` oracle and the
green QEMU smoke), adds **one** new trusted seam (`is_boundary`, the 3rd CAS interpreted-hash;
tally 13→14), and **raises the cas gate from 73 to 80** (+7).

## Design decisions exercised

- **DD3 (opaque-predicate seam).** `is_boundary` moved into `verus!{}` as a trusted-total
  `#[verifier::external_body]` with an `uninterp spec fn is_boundary_spec` twin — the same
  boundary as `checksum_ok`/`wal_checksum_ok` (totality + determinism only, **no
  injectivity**). The partition core is proven over `is_boundary_spec`, so it proves *less*
  than the concrete boundary set (structure for any predicate) at the cost of only the one
  totality seam — never an injective-hash ghost.
- **DD1 (layered bar) / DD2 (spine stays plain Rust).** B13B verifies one level's
  *partition*, not the multi-level *climb*: `Dir::save`'s `while nodes.len() > 1` loop and
  its 255-level `expect` are untouched (no provable termination metric without modeling the
  hash). The verified `split_points` drives the cut points; the node assembly + `store.put`
  I/O stay plain Rust (B7 "connect verified cores to plain-Rust I/O").

## What landed (all in `cas/src/prolly.rs`)

1. **The `is_boundary` seam.** `uninterp spec fn is_boundary_spec(item: Seq<u8>) -> bool`
   + the `external_body` exec `is_boundary` with `ensures b == is_boundary_spec(item_bytes@)`.
   The BLAKE3 body is unchanged; the `SPLIT_MASK` const it names stays outside the block (it
   appears only in the unverified body).
2. **`boundary_flags` (verified seam consumer).** Maps a level's item byte-images to a
   `Vec<bool>` faithfully reflecting `is_boundary_spec` per item — so the seam has a real
   verified consumer (it is not dead weight).
3. **`split_points` (the partition core).** Pure, Hash-free, over `Seq<bool>`. Returns the
   **end-index list** (cumulative block ends). Its `ensures`, for any `flags`:
   - **conservation/order** — ends strictly increase from ≥ 1 to `flags.len()` (so the
     blocks tile `[0, n)` losslessly and in order);
   - **well-formedness** — `ends[0] ≤ MAX_NODE_ENTRIES`, every gap `≤ MAX_NODE_ENTRIES`,
     every block non-empty;
   - **boundary discipline** — every block whose end `< flags.len()` ends at a boundary
     item (`flags[end-1]`) or exactly at the cap (`end - block_start == MAX_NODE_ENTRIES`).
4. **`flatten_blocks` + `lemma_partition_flatten`.** The conservation lemma made explicit:
   for a monotone end-list ending at `items.len()`, concatenating the blocks reproduces
   `items` exactly — "no item dropped, duplicated, or reordered." Generic over the item type
   (so it covers both leaf entries and internal child slots), proven by induction via the
   `subrange(0,a)+subrange(a,b)==subrange(0,b)` step (`lemma_flatten_covers`).
5. **`build_level` rewired.** `save`/`build_level` now carry each level's items as parallel
   `keys`/`byte_images` `Vec`s (clone-free vs. the old tuple `Vec`), call
   `boundary_flags` → `split_points`, and drive the I/O loop over the **proven** cut points.
   Public signatures of `Dir`, `save`, `load`, `parse_node` are unchanged.

## Verification

| Check | Result |
|---|---|
| `cargo verus verify -p cas --no-default-features` | **80 verified, 0 errors** (was 73; +7) |
| `cargo test -p cas` | **103 lib** (96 prior + 7 new partition tests) + 9 fuzz_corpus + 10 fuzz_regressions — all pass |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --test fuzz_regressions --test fuzz_corpus` | clean (no UB; format-stable) |
| `MIRIFLAGS=… cargo +nightly miri test -p cas --lib -- split_points boundary_flags build_level_fires` | clean (7 tests, incl. a 400-entry multi-level build + real-BLAKE3 `is_boundary` under Miri) |
| `scripts/fuzz.sh smoke 0 cas` | all 12 cas targets build + corpora replay green (`tree_node`/`mount_recovery` drive `build_level`), no regen |
| `cargo fmt --check` | clean |
| QEMU smoke (`scripts/run-demo.sh`) | green — `store mounted` → `serving`; `write`/`sync`/`cat`/`ls`/`df` correct (`cat` returns the stored bytes); no panic/`Corrupt` |

### New tests (`mod tests`)
- `split_points_forced_cap` / `_every_item_is_boundary` / `_single_item` / `_mixed` /
  `_exactly_cap` — pin the concrete cut points for controlled flag patterns (the cap fires
  at exactly 128; conservation/≤MAX observed on the output).
- `boundary_flags_faithful_to_predicate` — `boundary_flags` matches `is_boundary` item-by-item.
- `build_level_fires_multi_level_and_roundtrips` — a 400-entry directory forces the cap and
  the spine climb (internal root), and round-trips (the format-stable witness on the real path).

## Key findings

1. **The cut-index representation kept conservation a single subrange concat.** Modeling the
   partition as an *end-index list* (not nested `Seq<Seq<_>>`) made `flatten_blocks` a
   back-recursive `subrange` concat and the conservation proof a one-step-per-block induction
   — avoiding the `deep_view`/per-element bridge `Seq<Seq<u8>>` would force (verus.md §2).
   The plan called this (line 511); it held up.
2. **`i == n ==> start == n` is the invariant that proves the final block always flushes.**
   The loop allows an open block (`start < i`) mid-scan, so a generic invariant can't show
   the tail is emitted post-loop. The clause `i == n ==> start == n` is maintainable
   *because* the cut condition explicitly includes `i + 1 == n` (the last item always cuts):
   the not-cut branch implies `i + 1 != n`, so the loop can only reach `i == n` through a
   cut that set `start = n`. That single clause discharges `ends.last() == flags.len()` and
   `ends.len() ≥ 1` at exit.
3. **`int <` misparses as a turbofish (verus.md §6/§12).** `(... as int) < flags@.len()`
   failed to parse (`int<…>` read as generics). Flipping to `flags@.len() > (... as int)`
   fixed it — the same gotcha the guideline flags for inline `bit_vector` requires-clauses,
   here in a quantifier body.
4. **Nested spec-fn unfold needs explicit fuel at the recursion base.** `flatten_blocks` of
   a length-1 end-list unfolds to `flatten_blocks(drop_last) + subrange`, but the inner
   `flatten_blocks(drop_last)` (length 0 → `empty`) needed a second unfold; spelling out
   `assert(ends.drop_last().len() == 0)` + `assert(flatten_blocks(…) == empty)` discharged
   it. The whole core then verified — the `split_points` loop invariant + the conservation
   induction were the only substantive proof work.
5. **No injectivity, no determinism *theorem* needed.** "equal inputs ⇒ identical partition"
   holds *definitionally* — `split_points` is a pure function of `flags`, and `flags` is a
   pure function of the items via the deterministic seam. The discipline `ensures` captures
   "cut only at a boundary or the cap"; nothing beyond the seam's totality is trusted.

## Ledger (`doc/guidelines/verus_trusted-base.md`)

- Baselines cas row: **73 → 80 verified, 0 errors** (the +7 enumerated:
  `split_points`/`boundary_flags`/`block_start` + `flatten_blocks`/`lemma_flatten_covers`/
  `lemma_partition_flatten`).
- §(2) "out-of-scope total function" table: new `is_boundary` row (BLAKE3 split rule,
  trusted total, no injectivity, paired with `is_boundary_spec`, host tests named).
- **Tally 13 → 14** (3rd CAS interpreted-hash; heading + closing tally line updated). Ground
  truth re-derived: 8 `#[verifier::external_body]` + 6 `assume_specification` = 14.
- Verified-surface scope: the **level partition core** added beside the node decoder.
- `prolly.rs` module doc: a verification paragraph stating the mechanized half (node decoder
  total + partition conservation/discipline over the opaque seam) vs. the test-routed half
  (the concrete BLAKE3-determined tree shape).

The remaining B13D reconciliation (the full GC-sufficiency-style test-routed note keyed to
B13C's strengthened proptests) is unchanged scope — B13C/B13D are separate sub-phases.

## Out of scope (recorded so it is not mistaken for a gap)

- **Concrete tree shape / headline canonical-form across edit orders** — BLAKE3-dependent,
  test-routed at the rev1§6 baseline tier; B13C makes that sweep verification-grade. B13B
  proves the partition for *any* predicate, not *which* items boundary.
- **The multi-level `Dir::save` spine + its 255-level `expect`** — DD2: no provable
  termination metric without modeling the hash; disclosed structural backstop (≈ (1/32)^255).
- **End-to-end composition of conservation into node-byte/hash equality** — the I/O loop
  that assembles node bytes and calls `store.put` stays plain Rust; the conservation theorem
  is over the item *index*/image sequence (B13A verifies the per-leaf encode). The
  whole-tree byte-stability is the `roundtrip`/`canonical_form` oracle (B13C strengthens it).
