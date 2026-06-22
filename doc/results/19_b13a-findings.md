# B13A findings — directory node decoder lifted into the verified Verus surface

**Phase:** B13A of `doc/plans/14_b13-detail.md` (prolly-tree canonical-form
verification), the "must-do, clean Verus win, do first" sub-phase. Resolves the plan's
Design decisions 1 (the layered bar — Verus for the Hash-free machinery) and 2 (leave the
multi-level `Dir::save` spine plain Rust).

B13A lifts the directory **node decoder** (`parse_node`/`load_node` in
`cas/src/prolly.rs`) into the verified `verus!{}` surface — the one CAS on-disk decoder
that was still plain Rust while every sibling (superblock, WAL record, the single-entry TLV
codec) is already Verus-total. The node decoder is now proven **total over arbitrary bytes**
(the no-panic theorem) and a **leaf** node's decode is proven **canonical** (the consumed
bytes equal `canonical_leaf_bytes`, i.e. decode-then-re-encode reproduces the input — the
rev1§6 oracle and rev1§4.9 "exactly one encoding per logical leaf node", at the node grain).

It is **format-stable** (no on-disk byte change, no `SB_VERSION` bump, no corpus regen),
adds **no new trusted seam** (Hash-free, composes on the already-verified `decode_raw`), and
**raises the cas gate from 65 to 73** (+8). Internal nodes get **totality only** (see
finding 3); the BLAKE3-dependent tree *shape* and the multi-level spine stay test-routed
(B13C) / disclosed plain Rust (Design decision 2) — recorded so neither is mistaken for
mechanized.

## Design decisions exercised

- **DD1 (layered bar).** The node decoder is the Hash-free, mechanizable half — done in
  Verus. The hash-determined concrete tree shape is *not* touched here (B13C's proptest
  sweep is its guard of record).
- **DD2 (leave the spine plain Rust).** `Dir::save`'s `while nodes.len() > 1` climb and its
  255-level `expect` are untouched — B13A verifies one node's *decode*, not the multi-level
  *build*. The backstop remains a disclosed structural guard (≈ (1/32)^255).

## What landed (all in `cas/src/prolly.rs`)

1. **Spec model.** `entries_bytes(es: Seq<RawEntry>)` — the back-recursive fold of each
   entry's `canonical_bytes` — and `canonical_leaf_bytes(es)` = `[0][count u32][entries…]`,
   the canonical byte image of a whole leaf node. `lemma_entries_push` is the one-step
   unfold the decode/encode loops cite to restore their running concat invariant.
2. **`decode_raw` offset-parameterized.** `decode_raw(buf)` → `decode_raw(buf, start)`,
   `requires start <= buf@.len()`, `ensures … canonical_bytes(e) == buf@.subrange(start,
   start+k)`. This is the `decode_frame(wal, off)` idiom (store.rs) — thread an offset, do
   **not** range-slice (verus.md §8). The existing read helpers were already
   offset-parametric, so the change was mechanical (the name read + the assembly
   `subrange`/`lemma_cat` chain re-based on `start`); the proof re-verified unchanged.
3. **`decode_node` (verified, the headline).** Parses `[level u8][count u32][items…]` into
   the Hash-free `(u8, RawNodeBody)` image. Leaf items (`level == 0`) decode via a
   `decode_raw(buf, pos)` loop carrying `buf@.subrange(5, pos) == entries_bytes(parsed)`
   (`decreases count - i`); the whole buffer must be consumed. `ensures` (leaf): `lvl == 0
   && canonical_leaf_bytes(es@) == buf@`. Totality is the verified body itself.
4. **`encode_node_leaf` (verified).** Produces exactly `canonical_leaf_bytes` for a leaf —
   the encode half, so the round-trip composes both directions.
5. **Running decoders rewired (B7 "running code = proved code").** `parse_node` and
   `load_node` now call `decode_node`; the `Hash` wrap + `validate_entry` (which only shrink
   the accept set) stay plain Rust, and the cross-node discipline (level-matches-parent,
   empty-only-at-root, separator-key) stays in `load_node` (it needs root-ness/recursion).
   Public signatures (`parse_node`, `load_node`, `Dir::load`, `NodeRefs`, `Reader`,
   `decode_entry`/`encode_entry`) are unchanged; `tlv.rs`/`disk.rs` untouched.

## Verification

| Check | Result |
|---|---|
| `cargo verus verify -p cas --no-default-features` | **73 verified, 0 errors** (was 65; +8, no new seam) |
| `cargo test -p cas` | **96 lib** (89 prior + 7 new node tests) + 9 fuzz_corpus + 10 fuzz_regressions — all pass |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --test fuzz_regressions --test fuzz_corpus` | clean (9 + 10, no UB) |
| `cargo +nightly fuzz build` (cas) | all targets build; corpora replay (`tree_node`/`mount_recovery`) green, no regen |
| `cargo build -p cas --examples` | `gen_cas_corpus` (uses `parse_node`) compiles against the refactored API |
| `cd kernel && cargo build` (aarch64) | links `storaged` (pulls `cas`) — the `verus!{}` node decoder erases and cross-compiles unchanged |

### New tests (`mod tests`)
- `node_decoder_rejects_overwide_count` / `_trailing_bytes` / `_truncated_header` — via
  `parse_node`, pinning the rejection messages behind the verified totality.
- `node_decoder_rejects_level_mismatch` / `_separator_mismatch` / `_empty_non_root` — via
  hand-crafted `MemStore` trees through `Dir::load` (the cross-node discipline).
- `node_leaf_decode_encode_roundtrip` — `decode_node`→`encode_node_leaf` reproduces a real
  (BLAKE3) leaf node's bytes exactly.

## Key findings

1. **Offset-threading, not range-slicing, is the house idiom and the lower-risk path.**
   The codebase's `decode_frame(wal, off)` (store.rs:642) explicitly indexes `buf[off+k]`
   "rather than range-slicing, so the proof stays first-order." Parameterizing `decode_raw`
   with a `start` offset followed the same grain, kept the proof first-order, and re-verified
   first try — versus subslicing `&buf[pos..]` and bridging vstd's closed subslice specs.
2. **The leaf round-trip proof is the `extend_bytes`/`run_len` pattern, one grain up.** A
   running concat invariant (`buf[5..pos] == entries_bytes(parsed)`), restored each
   iteration by `lemma_entries_push` (one fold unfold) + `lemma_cat` (subrange join), then a
   two-`lemma_cat` header assembly. No new proof machinery — the idioms existed at the entry
   grain. The whole sub-phase verified on the first verifier run (65 → 73).
3. **Internal nodes are totality-only by necessity, not omission.** `parse_node` lowers each
   internal separator key into the child hash (`NodeRefs::Children`), so there is no lossless
   single-node internal re-encoder to state a canonical-round-trip against. `decode_node`
   keeps the keys (`RawChild`) so `load_node` can still check the separator discipline, but
   the internal **lossless** level is B13C's whole-tree `save→load→save` oracle, not B13A.
4. **Error classification preserved exactly.** A `TlvErr::BadNode` variant was added so
   "node too wide" / "trailing bytes" still surface as `FormatError::BadNode(...)` and
   entry-content errors still surface as `BadEntry(...)` — verified by the new rejection
   tests. The only behavioral nuance: for an input that is *both* wrong-level and malformed,
   `decode_node` now rejects the malformation before `load_node` reaches the level check (a
   different error variant on a doubly-broken input); no test or caller depends on it.

## Ledger (`doc/guidelines/verus_trusted-base.md`)

- Baselines cas row: **65 → 73 verified, 0 errors** (the +8 enumerated).
- Verified-surface scope: the directory node decoder (`decode_node` total + leaf canonical
  round-trip, `encode_node_leaf`) added beside "the single-entry TLV codec".
- **Seam tally unchanged at 13** — B13A adds no `external_body`/`assume_specification`
  (`rg "external_body|assume_specification" cas/` still 2 in cas: `checksum_ok`,
  `wal_checksum_ok`).

The *full* mechanized-vs-test-routed prolly note and the `prolly.rs` module-doc
reconciliation are **B13D's** job (they await B13C's strengthened proptest description);
B13A's ledger change is the gate bump + scope sentence only.

## Out of scope (recorded so it is not mistaken for a gap)

- **Concrete tree shape / headline canonical-form across edit orders** — BLAKE3-dependent,
  test-routed at the rev1§6 baseline tier; B13C makes that sweep verification-grade.
- **The partition core (`build_level`'s cut logic)** — B13B (stretch; +1 `is_boundary`
  seam if taken). Encode-side structure is currently test-routed.
- **The multi-level `Dir::save` spine + its 255-level `expect`** — Design decision 2: no
  provable termination metric without modeling the hash; disclosed structural backstop.
- **Internal-node lossless single-node re-encode** — finding 3; B13C's whole-tree oracle.
