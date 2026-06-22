# B13C findings — verification-grade canonical-form proptest + fuzz

**Phase:** B13C of `doc/plans/14_b13-detail.md` (prolly-tree canonical-form
verification), the **test-only** must-do. It is the *guard of record* for the
headline rev1§4.1 property — *same logical contents ⇒ same root hash, regardless
of edit order* — which is BLAKE3-dependent and therefore deliberately **not**
mechanized (rev1§6 routes the prolly tree's concrete shape to the baseline tier;
Design decision 1). B13C raises that guard from light sampling to a real
adversarial sweep, resolving Design decision 4 (chunker scope).

The pre-B13C `canonical_form` proptest used `arb_entries(64)`, which **never
built a multi-level tree, never fired the `MAX_NODE_ENTRIES = 128` forced-boundary
cap, and never climbed the spine** — the central blind spot the audit flagged.
B13C now spans multi-level shapes × many edit orders × churn, adds a whole-tree
decode-then-re-encode oracle (the lossless internal-node level B13A's single-leaf
oracle could not reach), promotes split-locality to a depth-scaled proptest, and
adds the chunker-selection symmetry proptest.

It is **purely test-additive and format-stable**: no production `cas` code
changed (only `#[cfg(test)]` modules and the `tree_node` fuzz binary), so the
**cas gate stays 80/0**, the interpreted-hash seam **tally stays 14**, and no
on-disk bytes / split constants / hashes moved. The committed fuzz corpora replay
unchanged — the strengthened oracles *tighten* what they are checked against. The
ledger + module-doc reconciliation is **B13D**'s job; this doc is the
strengthened-test description B13D cites.

## Design decisions exercised

- **DD1 (layered bar).** B13C is Track T: the BLAKE3-dependent shape is
  test-routed, made verification-grade. It does not touch the Verus surface
  (Tracks V1/V2 landed in B13A/B13B); the gate is unchanged.
- **DD2 (spine stays plain Rust; 255-level `expect` disclosed, not removed).**
  The deep deterministic test (20 000 entries, root level ≥ 2) probes the spine
  climb on the real path; the `expect` is a disclosed structural backstop
  (≈ (1/32)^255), recorded in the ledger by B13D, never tripped here.
- **DD4 (prolly-scoped; chunker gets one symmetry proptest, no rebuild).**
  `chunker_selection_symmetry` asserts `boundaries` is a pure function of the
  data and `store_file`'s inline-vs-chunk selection is content-determined — no
  FastCDC Verus, no chunker rebuild (it already meets the baseline bar).

## What landed

### `cas/src/prolly.rs` `mod tests`

1. **`canonical_form` strengthened.** Entry counts widened to
   `arb_entries(cfg!(miri) ? 64 : 320)` (churn `… : 64`), native cases raised
   **256 → 1024**. Same shuffle + interleaved insert-then-remove churn. New
   per-case **coverage guard**: when `entries.len() > 128` the root must be
   internal (`tree_depth(..) ≥ 1`) — > 128 entries force ≥ 2 leaf nodes via the
   cap regardless of hash, so this deterministically witnesses the multi-level /
   spine path, not just samples it.
2. **`canonical_form_deep` (new, deterministic, `#[cfg_attr(miri, ignore)]`).**
   The non-regressing depth guard the proptest can't aggregate. Builds a
   20 000-entry directory — ⌈N / MAX_NODE_ENTRIES⌉ = 157 > 128 leaf nodes, so the
   cap **alone** forces a second internal level ⇒ a deterministic, never-flaky
   root level ≥ 2 (≥ 3 levels). Asserts the depth, edit-order independence across
   ascending/descending/shuffled builds **and** churn, and whole-tree
   `save → load → save` stability at depth.
3. **`roundtrip` widened** to `arb_entries(cfg!(miri) ? 64 : 320)` so the
   identity covers the **internal-node** decode→re-encode level (separator-key
   discipline + spine), not just single leaves.
4. **`split_locality` (new proptest; promotes `structural_sharing_on_small_edit`).**
   Over many shapes (`arb_entries(cfg!(miri) ? 64 : 1024)`) and edit sites, a
   one-entry remove/content-rewrite rewrites **O(depth)** nodes, asserted against
   a **depth-scaled** bound `4 * (depth + 1)` (not the old fixed ≤ 8). The
   deterministic `structural_sharing_on_small_edit` (1000 entries, ≤ 8) stays as
   the regression anchor.
5. **`decoder_rejects_garbage` extended** to `0..1024` bytes and to also exercise
   the shallow GC-walk `parse_node` entry point (totality on hostile bytes), not
   just `Dir::load`.
6. **`tree_depth` test helper** — the root node's `decode_node` level field is
   the tree height; used by the three depth assertions above.

### `cas/src/file.rs` `mod tests`

7. **`chunker_selection_symmetry` (new proptest).** `boundaries(&p, &data)` is a
   pure function of the data, and `store_file` twice on the same data yields an
   equal `Content` whose variant follows the INLINE_MAX rule (≤ 512 → `Inline`,
   > 512 → `ChunkList`).

### `cas/fuzz/fuzz_targets/tree_node.rs`

8. **Whole-tree oracle added.** Keeps the shallow `parse_node` totality + leaf
   canonical re-encode oracle; **adds** a second oracle that carves the fuzz
   bytes into directory entries (sanitized names; ≤ 400 entries so a single
   iteration can still cross the 128-cap), builds a `Dir`, and asserts
   `save → load → save` is the identity over the whole, possibly multi-level
   tree — the lossless internal-node level. Uses only public API; no corpus
   regen (the committed seeds replay through the unchanged corpus harness).

## Verification

| Check | Result |
|---|---|
| `cargo verus verify -p cas --no-default-features` | **80 verified, 0 errors** — unchanged (test-only) |
| `cargo test -p cas` | **106 lib** (103 prior + 3 new: `canonical_form_deep`, `split_locality`, `chunker_selection_symmetry`) + 9 fuzz_corpus + 10 fuzz_regressions — all pass (~3 s) |
| `PROPTEST_CASES=8000 cargo test -p cas --lib -- canonical_form split_locality` | green (no flakiness at 8000 cases; locality bound has margin) |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --lib` | _pending — fill on completion_ |
| `cargo +nightly fuzz build tree_node` + 62 282-run short fuzz | builds; whole-tree oracle holds, no crash; regenerated corpus discarded (format-stable) |
| `cargo fmt` + `cargo fmt --manifest-path cas/fuzz/Cargo.toml` | clean |
| QEMU smoke | not required — no production code changed (`storaged` links the unchanged `cas` lib); B13A/B13B already established the decoder/partition on the boot path |

## Key findings

1. **A guaranteed multi-level coverage assertion comes from the cap, not the
   hash.** Picking N = 20 000 so ⌈N/128⌉ > 128 leaf nodes makes the
   forced-boundary cap *alone* require a second internal level — a depth ≥ 2 that
   holds for any hash, so `canonical_form_deep`'s depth assert is deterministic
   and can never flake. This is the clean way to assert "a ≥ 3-level tree was
   built" without a hash-cooperation argument.
2. **Proptests can't aggregate "did any case reach depth d"; coverage is split.**
   Each case is independent, so "some case hit 3 levels" isn't expressible. B13C
   splits it: a per-case conditional guard in `canonical_form` (> 128 entries ⇒
   internal root) witnesses the cap-fired path across the sweep, and the
   deterministic deep test carries the ≥ 3-level guarantee.
3. **Split-locality must be depth-scaled because of cap-cascades.** A content
   edit flips that item's boundary bit; inside a > 128 no-boundary run a flip
   shifts that run's ⌈len/128⌉ cap-cuts, so the rewrite is O(depth) but not a
   fixed constant. Empirically `3 * (depth + 1)` survives 8000 cases (worst
   single case ≈ 1.3×); B13C ships `4 * (depth + 1)` for margin while keeping the
   O(depth) ≪ O(N) locality claim.
4. **`TlvErr` is not `Debug`, so depth reads go through a `match` helper.**
   `decode_node(..).unwrap()` won't compile; `tree_depth` matches `Ok((level,_))`
   the way the existing `node_leaf_decode_encode_roundtrip` test does.
5. **The fuzz target carries two oracles on one input.** The raw bytes feed the
   shallow leaf-canonical re-encode oracle *and* are carved into a `Dir` for the
   whole-tree round-trip — the internal-node lossless level a single-node oracle
   can't reach. The committed corpus replays unchanged; the run's new corpus
   entries (and a proptest-regressions seed from a deliberately-too-tight
   bound-probe) were discarded to keep the change format-stable.

## Out of scope (recorded so it is not mistaken for a gap)

- **Verus over the BLAKE3-dependent concrete tree shape** — out of scope per
  Design decision 1 / verus.md (would drag interpreted BLAKE3 into the proof);
  this sweep is its baseline-tier guard of record.
- **Ledger + `prolly.rs` module-doc reconciliation** — B13D. The cas gate stays
  80/0 and the seam tally stays 14; B13C adds no Verus and no trusted seam.
- **Chunker Verus / rebuild** — Design decision 4; only the one symmetry proptest
  was added.
