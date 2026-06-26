# 16 — Verus findings: chunk-list decoder lift (Phase 3.2)

Date: 2026-06-26. Crates: `cas`. This is a temporary intermediate record per CLAUDE.md;
it is not referenced from comments, specs, or guidelines.

## Purpose

Phase 3.2 of `doc/plans/0_verus-improvements.md` lifts `cas/src/file.rs::chunk_list_entries`
— previously plain Rust built on the §8-forbidden `u32::from_le_bytes` / `try_into().unwrap()`
/ range-slice forms — into a verified `verus!{}` island that decodes the on-disk chunk-list
object `[MAGIC][count u32][ (32-byte hash, u32 len) × count ]` into a `Hash`-free image,
total over arbitrary bytes and framing each accepted buffer exactly against one layout spec.
The object is read by both the file read path and the rev2§4.6 GC mark walk, so its decoder
is an adversarial-input parser (`verus.md` §8/§9); this is the strictly-stronger guarantee
(no-panic + canonical framing ∀ bytes), and as a side effect it removes the prior plain-Rust
`5 + count*36` `usize` overflow hazard (the verified loop never computes that product — §5).

## What changed

- New `verus!{}` island in `cas/src/file.rs` (the file had none): the `Hash`-free image
  `RawChunkRef { [u8; 32], u32 }`; the layout specs `chunk_ref_bytes` / `chunk_refs_bytes`
  (back-recursive, `decreases`) / `chunk_list_bytes`; `lemma_chunk_refs_push`; the total
  decoder `decode_chunk_list` (`ensures r matches Ok(rs) ==> chunk_list_bytes(rs@) == buf@`)
  and the encoder `encode_chunk_list` / `encode_chunk_ref`. It mirrors `prolly.rs`'s
  `decode_node` / `encode_node_leaf` recipe (a `fits`-guarded per-step loop with a running
  `buf@.subrange(5, pos) == chunk_refs_bytes(refs@)` concat invariant + a trailing-bytes
  reject), so no nonlinear `36*count` arithmetic enters the proof. **Totality + framing only,
  no injectivity** over the digest bytes (as `decode_node` does over the opaque `is_boundary`).
- Six `cas/src/prolly.rs` helpers made `pub(crate)` so the file.rs island reuses them instead
  of duplicating verified code (`read_arr32`, `push_arr32`, `push_u32_le`, `fits`, `lemma_cat`,
  and the plain-Rust `tlv_err`). These are SMT-neutral visibility bumps — zero obligation change.
- The plain-Rust shells now delegate, signatures unchanged: `chunk_list_entries` calls
  `decode_chunk_list` and wraps each `[u8; 32]` back into `Hash` (`Hash::from_bytes`, the one
  §9 seam point); `store_file` / `store_file_neighborhood` build a `Vec<RawChunkRef>`
  (`*hash.as_bytes()`, the transparent unwrap) and call `encode_chunk_list` — byte-for-byte the
  same object, now routed through the single `chunk_list_bytes` layout.
- Added the always-compiled `chunk_list_entries_strictness` host test (bad magic / short header
  / empty / trailing byte / truncated final ref / over-count all rejected, well-formed buffer
  round-trips).

## Decision — inline the per-ref reads rather than a `decode_chunk_ref` helper

The first draft factored a `decode_chunk_ref(buf, off)` helper with `requires off + 36 <=
buf@.len()`. That failed to verify with *possible arithmetic underflow/overflow* on the exec
`off + 32`: the helper's bound is a ghost `int` fact, and with no exec `usize` length witness
in scope (the helper never calls `buf.len()` nor indexes `buf` directly — it delegates to
`read_arr32` / `read_u32_le`), the solver could not bound `off + 32 <= usize::MAX`. Verified
count at that point was `79 verified, 1 error`. The fix mirrors `decode_node`'s **internal-node
loop**, which inlines its per-child reads precisely so the loop's `fits(pos, 36, len)` —
`len` an exec `usize` from `buf.len()` — bounds the offset arithmetic. Inlining the two reads
into `decode_chunk_list`'s loop discharges it (`pos + 32 < pos + 36 <= len <= usize::MAX`); the
fusion nudge (`assert(chunk_ref_bytes(rr) =~= buf@.subrange(pos, pos+36))` after the inner
`lemma_cat`) moves into the loop body. `encode_chunk_ref` keeps its helper form — push-only,
no offset arithmetic, no overflow obligation.

## Finding — `cas (77)` cross-references in two sibling ledger rows were stale since Phase 2.3

Phase 2.3 (finding 15) dropped cas `77 → 71` when the `le-bytes` machinery moved to the shared
crate, and updated the **cas row** cell accordingly — but it left the `cas (77)` cross-references
in the **virtio-blk** row ("re-verifies its gated deps … cas (77)") and the **storage-server**
row ("… under the alloc prelude (cas 77, ipc 71)") pointing at the pre-2.3 number. Those
cross-references are meant to equal the cas standalone count, so they were stale by 6 for two
phases. This change re-measures cas at `79` and updates both cross-references (plus the cas row)
to match. The Task-13 routing note's `cas Baseline rises 75 → 77` (verus_trusted-base.md ~line
252) is an accurate *historical* record of Task 13's delta (it predates Phase 2.3's −6) and is
left unchanged. A Phase 1.2 anchor sweep should fold these sibling cross-references into its
"code is authoritative" reconciliation so they cannot drift independently of the cas row again.

## Count accounting

| crate | before | after | Δ | note |
|-------|-------:|------:|--:|------|
| `cas` (`--no-default-features`) | 71 | 79 | +8 | additive island; no existing obligation touched |

The `+8` are the new island's obligations (the decoder, the two encoders, `lemma_chunk_refs_push`,
the recursive `chunk_refs_bytes` termination, and the associated checks). The non-recursive `open`
specs (`chunk_ref_bytes`, `chunk_list_bytes`) carry no obligation. No spec was weakened, no
obligation dropped, no `ensures` loosened, no input coverage narrowed; **no trusted seam added**
(the `Hash` wrap stays the existing plain-Rust delegator, no `external_body`) — tally stays 14.

## Verification (all cold; `cargo clean` first, a present `verification results::` line = real run)

Verus `0.2026.06.07.cd03505`, toolchain `1.95.0` (the pinned binary).

- Before (base tree): `cargo clean -p cas && cargo verus verify -p cas --no-default-features`
  → `cas: 71 verified, 0 errors` (and `le-bytes: 6`).
- After: same command → `cas: 79 verified, 0 errors`. JSON timing (`--time-expanded
  --output-json`, `success=true`): the new functions' `rlimit-count` peaks at
  `encode_chunk_list` ≈ 0.52M (≈1.7% of the ~30M default ceiling), `decode_chunk_list` ≈ 0.13M,
  the rest far less — so **no `rlimit` is sized** (the `le-bytes`-row posture). Existing
  obligations are byte-identical (purely additive change + SMT-neutral visibility bumps), so no
  regression on the rest of the surface.
- Worst-context (alloc prelude): `cargo clean && cargo verus verify -p virtio-blk` re-verifies
  the gated deps under `vstd[alloc]` → `cas: 79`, `le-bytes: 6`, `freelist: 30`, `dma-pool: 0`,
  `virtio-blk: 3` — all `0 errors`. The island passes in the worst context too.
- `cargo build -p cas` clean; `cargo test -p cas` → 134 + 9 + 10 passed, 0 failed (the new
  `chunk_list_entries_strictness` test plus `file_roundtrip` / `neighborhood_matches_whole_file`
  / `dedup_identical_content` exercise the decode path and the encoder restructure + the
  `Hash`↔`[u8;32]` seam).
