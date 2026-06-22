# B12E findings — neighborhood-only re-chunk on flush

**Phase:** B12E (`doc/plans/13_b12-detail.md`), the **independent** sub-phase of B12 (it touches
the flush's chunk-*splicing*, not its scheduling, so it does not depend on B12A–D and they do not
depend on it). It closes the last flush-path conformance gap of B12:

- **M-7 — flush re-chunks whole dirty files.** `flush_ref` rebuilt every dirty file from scratch:
  read the whole old content, apply the overlay, and re-chunk the entire result
  (`make_file_entry` → `store_file` → `boundaries(whole_file)`). rev1§4.3 step 3 mandates
  re-chunking only the **affected neighborhood**: "back up one chunk before the first dirty byte,
  run the chunker forward, and stop when an emitted boundary coincides with an existing one (CDC
  self-synchronization guarantees this within a few chunks). A 200-byte edit in a 1 GiB file yields
  ~2–4 new chunks." B12E implements that and retires the disclosed-MVP `//!` bullet.

It is **format-stable** and adds **no Verus** (cas gate held at **65/0**). It makes **no
`StoreOptions` API change**; the blast radius is `cas` only (the chunker, overlay, file, and store
modules) — `mkfs`, `storage-server`/lib, `virtio-blk`, and `storaged` are untouched.

**Decisions exercised (from the plan):**
- **Design decision 2 — proptest + Miri, no new Verus chokepoint.** The splice is plain-Rust over
  the already-verified codec substrate; the cas gate holds at 65/0. The correctness guard is a
  canonical-form *oracle* proptest, not a proof (rev1§4.1's canonical form is the spec; B13 will
  *prove* it — B12E only shows this flush path *matches* it).
- **Design decision 1 — format-stable.** The spliced chunk list uses the exact same on-disk
  encoding as `store_file`; no `SB_VERSION` bump, no corpus regen.

## The key correctness fact: the chunker restarts its fingerprint at each chunk start

`cas/src/chunk.rs:78` (`find_boundary`) sets `fp = 0` at offset 0 of every chunk it scans, so a cut
depends **only on bytes since the previous cut**, never on bytes before it. Two consequences make
the splice provably canonical:

1. **Resuming the chunker at an old (hence canonical) boundary reproduces the canonical cuts
   forward.** This is the self-synchronization the existing `realigns_after_prefix_edit_on_random_data`
   / `shared_suffix_boundaries_agree_after_first_common` tests already exercise (chunk.rs:194-325).
2. **Unchanged prefix/suffix chunks are themselves canonical cuts of the new content.** By induction
   from offset 0 over identical bytes, every old boundary at or before the first dirty byte is also
   a new-content boundary; symmetrically for the common suffix (shifted by the length delta).

So the result is **byte-for-byte the canonical chunking of the new content** — identical to
`store_file(new)` — but only the chunks the edit actually disturbed are hashed.

## What landed

**`cas/src/chunk.rs`:**
- **`pub(crate) fn next_cut(params, data) -> usize`** — a single-step wrapper over the private
  `find_boundary` (returns `data.len()` for the final sub-min chunk). Lets the splice re-chunk
  forward one chunk at a time and **stop at realignment** instead of scanning the whole tail.

**`cas/src/overlay.rs`:**
- **`FileOverlay::first_write_offset() -> Option<u64>`** — the first changed byte (`writes` is a
  sorted `BTreeMap`, so `keys().next()`), bounding the re-chunked region.
- **`FileOverlay::apply`** now borrows `base: &[u8]` (was `Vec<u8>`) so the caller keeps the pre-edit
  bytes alive alongside the new content for the suffix diff. The three overlay-test callers and the
  `Store::read` caller updated mechanically.

**`cas/src/file.rs`:**
- **`store_file_neighborhood(store, params, old, old_bytes, new, first_dirty) -> Content`** — the
  splice. Falls back to whole-file `store_file` whenever there is nothing to reuse (new content
  inlines, or `old` was not itself a readable chunk list). Otherwise:
  1. Parse the old chunk list (`chunk_list_entries`) and build cumulative old boundaries `old_cut`.
  2. **Prefix:** keep every chunk before the one holding `first_dirty`, then **back up one more
     chunk** (rev1§4.3) → `resume` there.
  3. **Suffix:** `common_suffix_len(new, old_bytes)` gives the unchanged trailing run; everything
     past `s_new = new_len − common` is identical content shifted by `delta = new_len − old_len`.
  4. Run `next_cut` forward from `resume`, hashing each fresh chunk, until an emitted boundary
     `pos ≥ s_new` maps onto an old boundary (`old_cut.binary_search(pos − delta)`) — then splice
     in the remaining old chunks and stop. `pos == new_len` always realigns (`old_len` is a
     boundary), so the loop terminates with worst-case "re-chunk to the end, no suffix reuse."
  5. Encode the spliced `(hash, len)` list with the **same format** as `store_file`.
- **`fn common_suffix_len(a, b) -> usize`** — longest common suffix, a plain byte scan.

**`cas/src/store.rs`:**
- **`flush_ref`** now keeps the old entry's `Content` + materialized base bytes (`reuse`) when there
  is a real base to diff against (not a fresh create / unlink-then-write, not a directory), then
  calls `store_file_neighborhood` for the reuse case and whole-file `store_file` for the fresh case,
  building the `Entry` inline. The `make_file_entry` import was dropped here (still used elsewhere);
  `store_file` + `store_file_neighborhood` imported instead.
- **MVP-disclosure retired:** the `//!` bullet "Flush rebuilds whole dirty files instead of
  re-chunking only the affected neighborhood (rev1§4.3 step 3) …" (store.rs:21-23) is removed.

## Verification (all green, run locally)

| Check | Result |
|---|---|
| `cargo test -p cas` | **88 lib** (85 prior + 3 new) + 9 fuzz_corpus + 10 fuzz_regressions — all pass |
| `cargo verus verify -p cas --no-default-features` | **65 verified, 0 errors** — unchanged |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --lib neighborhood_matches_whole_file` | **1 passed, 0 failed** (155 s) — no UB in the splice/index/binary-search arithmetic; proptest capped at 4 cases under `cfg(miri)` |
| `… miri test -p cas --test fuzz_regressions --test fuzz_corpus` | **9 + 10 passed** — codec/mount/GC corpora unaffected (B12E is format-stable) |
| aarch64 cross-build (`cd kernel && cargo build`) | clean (pre-existing kcore unused-import warnings only) — `storaged` links the reshaped `flush_ref` |
| `scripts/run-demo.sh` (QEMU, mkfs image + virtio-blk) | green: `[storaged] store mounted → serving`, then a `write docs/smoke hello / sync / cat → hello`, **re-write** `write docs/smoke world / sync / cat → world`, `ls`, `df` round-trip — flush through the rewired path |

New tests (`cas/src/file.rs mod tests`):
- **`neighborhood_matches_whole_file`** (proptest, 256/4) — **the load-bearing guard**: for an
  arbitrary edit (overwrite/extend) to a multi-chunk file, `store_file_neighborhood(...)` equals
  the canonical `store_file(new)` byte-for-byte (same `Content` ⇒ same chunk-list hash) and reads
  back as the new content. Behavior-preserving, only cheaper. This is also the Miri UB witness.
- **`neighborhood_rechunks_only_the_edit`** (deterministic, `#[cfg_attr(miri, ignore)]`) — M-7
  write-amplification headline: a one-byte flip in a 128 KiB / **447-chunk** file re-hashes only
  **3** chunks (two fresh chunks around the edit + the chunk-list object) and still produces the
  canonical result — `nb_puts ≤ 8` and `nb_puts < total_chunks`.
- **`neighborhood_reuses_prefix`** (deterministic, `#[cfg_attr(miri, ignore)]`) — an interior edit
  in the middle chunk hashes strictly fewer chunks than a whole-file re-chunk, guaranteed by prefix
  reuse *alone* (independent of whether CDC realignment reuses the suffix).

The two perf-metric tests use large files and are skipped under the interpreted-BLAKE3 Miri run; the
splice arithmetic they exercise is identical to the proptest's, which *does* run under Miri.

## Key findings

1. **The win is hashing CPU, not stored bytes — content-addressing already deduped the storage.**
   Because chunks are content-addressed, a whole-file re-chunk re-`put`s identical unchanged chunks
   and they dedup to zero new objects — so *storage* write-amplification was **already** minimal.
   What whole-file re-chunk wasted was **CPU**: running the chunker and BLAKE3 over the entire file
   every flush. B12E's measurable win (and M-7's real meaning) is the count of chunks **hashed** —
   `store.put` calls — cut from O(file) to O(edit): observed **3** vs **447** for a one-byte edit.
   The write-amplification test therefore counts `put` calls (a `CountingStore` wrapping `MemStore`),
   not `store.len()` growth, which would have shown no difference.

2. **Resync uses a single global `delta` + an `s_new` guard, which is correct even for scattered
   edits.** `delta = new_len − old_len` is the shift only of the *trailing* unchanged run; the
   `pos ≥ s_new` guard ensures we only splice the old suffix *past the last edit*, where the local
   shift equals the global delta. Mid-file unchanged spans between two edits are simply re-chunked
   (the chunker realigns there too, but we don't try to reuse them) — correct, just fewer savings
   for multi-edit files. Overlay writes never shrink or move bytes, so `delta ≥ 0` always.

3. **`apply` had to borrow its base — a deliberate ~2× peak-memory trade.** The suffix diff needs
   the old bytes *and* the new content simultaneously, so `apply` now clones `base` into the result
   (when not fresh) instead of consuming it, leaving `base` alive for `common_suffix_len`. Peak
   memory during a flush is ~2× the file (old + new) rather than ~1×. Accepted for the MVP and
   recorded; the genuinely-I/O-frugal version is the next finding.

4. **Read I/O is *not* reduced — only hashing and the chunker scan are.** `flush_ref` still
   `read_file`s the whole old content and `apply`s the full new content (both O(file)); only the
   BLAKE3 hashing and the chunker scan are bounded to the neighborhood. Avoiding the read entirely
   (a sparse overlay that never materializes unchanged regions) is a larger change and is **out of
   scope** — recorded, not a gap. The M-7 acceptance metric ("newly-hashed chunks bounded") is met.

5. **The crash-injection proptest was deliberately *not* extended.** B12E changes *how a file entry
   is built within* a flush, not the durability path: `flush_ref` still calls the same `tree::put` +
   `commit`, and the spliced chunk list is canonically identical to the whole-file one. The
   all-acked-survives invariant (`CommitProtocol`'s `AckedWritesRecoverable` + the B7 `Recover`
   property, exercised by `crash_recovery_preserves_acked_state`) is unchanged and already witnessed.
   Per the plan, B12E's selective path is the *canonical-form oracle*, not a new durability path.

6. **`next_cut` (not `boundaries`) is what makes it O(edit).** Calling `boundaries(&new[resume..])`
   would re-scan the whole tail with the chunker (O(file) CPU again); the single-step `next_cut`
   loop stops the moment it realigns, so the chunker scan is bounded to the neighborhood too — the
   spec's "stop when an emitted boundary coincides with an existing one."

## Ledger

No edit to `doc/guidelines/verus_trusted-base.md`: B12E adds no verified surface and no new trusted
seam, and the cas baseline (`cargo verus verify -p cas --no-default-features` → **65/0**, ledger
line 158) is held unchanged — consistent with B12A–D, which also left the ledger untouched (the
flush policy is plain-Rust scheduler/codec-shaping code below the Verus line, test-routed per Design
decision 2). The only disclosure retirement is the store.rs `//!` "whole dirty files" bullet.

## Out of scope for B12E (recorded so it is not mistaken for a gap)

- **The remaining B12 item — B12F:** the rev1§4.4 recommended defaults (S-9: 30 s `staleness_ns`,
  50% `wal_watermark`, 8 MiB per-ref / 128 MiB global / 64 MiB WAL) + the refuse-not-panic
  `format`/`mkfs` contract (S-10). B12E does not touch defaults.
- **Reducing the flush's read/apply O(file) I/O** via a sparse overlay (finding 4) — future
  optimization; B12E bounds only hashing and the chunker scan.
- **The prolly-tree canonical-form *proof*** — Phase B13. B12E *uses* the canonical form as its
  oracle; B13 *proves* it. Distinct, complementary.
- **A verified ring/splice core, async `FULL` backpressure, rename/file-id-keyed overlays** — the
  rest of B12 / later phases. The cas gate is held at 65/0.
