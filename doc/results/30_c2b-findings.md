# C2B findings — `Store::rename` + `WalOp::Rename` + verified-decode extension + crash recovery

Phase **C2B** (`doc/plans/18_c2-detail.md`), built on C2A (the overlay re-key to an
ephemeral `FileId`, PR #170). Branch `c2b-store-rename-wal`, based on `origin/main` @ `94d317e`.

## What landed

1. **`Overlay::rename` — the O(1) name swap** (`cas/src/overlay.rs`). A dirty file's rename
   moves the name pointers (`by_name`/`names`), tombstones the source, and overwrites the
   destination last-write-wins — the per-id interval map in `by_id` **never moves** (the
   headline rev1§4.9 property). `origin` is untouched, so flush still reads the pre-edit base at
   the original name. Plus `open_for_rename` (bring a clean committed file into the overlay so it
   moves via the same mechanism) and `rename_dir` + a `dir_renames` map for directory moves.

2. **`WalOp::Rename` (tag 3) on the verified WAL decode** (`cas/src/disk.rs`, `cas/src/store.rs`).
   New op `Rename { ref_name, from, to, mtime }`; `encode_payload`/`decode_payload` grew a tag-3
   arm (two length-prefixed paths + mtime). The **verified** structural mirror `s_payload_ok`
   and its exec twin `e_payload_ok` each gained a tag-3 arm (two `s_path`/`e_path` walks), and the
   ∀-bytes equality (`ensures r == s_payload_ok(pay@)`) **re-proved with no change to the count or
   the seam**.

3. **Flush + read base-origin** (`cas/src/store.rs`). `flush_ref` reads each file's base from its
   `origin` against the **pre-flush root snapshot**, writes at the current name, and removes
   sources that differ from their current name. `read` likewise consults `origin` so a
   renamed-but-unflushed read sees the committed bytes at the original name.

4. **`SB_VERSION` 4 → 5 + refuse-old** (`cas/src/disk.rs`). A pre-C2B binary refuses a store that
   may carry tag-3 WAL records (`format_v4_image_is_refused_with_a_version_error`) rather than
   decode one as a torn tail and silently drop a renamed-unflushed write. `mkfs`/`Store::format`
   write v5 automatically (no `mkfs` change).

5. **`Store::rename`** (`cas/src/store.rs`): validate both endpoints, classify the source
   (live dirty file / clean committed file / directory / absent), and `log_then_apply`. A
   directory rename drains the ref first (DD4), logs, applies, flushes, and commits — synchronous
   and durable at once.

## Verification

- `cargo verus verify -p cas --no-default-features` → **80 verified, 0 errors** (unchanged).
- `cargo test -p cas` → all pass (142 lib + integration tests, incl. the new rename/crash/version
  tests); `cargo test -p storage-server` → all pass.
- Quick Miri UB pass (`--test fuzz_regressions --test fuzz_corpus`) clean — the new tag-3 corpus
  seed `cas/fuzz/corpus/wal_replay_scan/rename` replays UB-free.
- Full `cas` Miri nextest sweep clean (incl. the rename-extended overlay model proptest and the
  rename crash proptest, capped at 4 cases under `cfg(miri)`).

## Verus total: why it stayed 80, not "higher"

The plan anticipated the tag-3 walk might lift the count. It didn't: `s_payload_ok`/`e_payload_ok`
are existing top-level verified items, and adding a third tag arm makes those functions bigger
without adding new obligations. The discharge is the same `ensures r == s_payload_ok(pay@)`,
re-proved over the larger body. No new `spec fn`/`proof fn`, **no new trusted seam** —
`wal_checksum_ok` (BLAKE3) remains the lone uninterpreted part of the record seam.

## Design resolutions worth recording

- **`read` must consult `origin`, not the current path (DD3, easy to miss).** A renamed dirty
  file `a→b` keeps its committed base at `a` until flush; reading `b` must apply the interval map
  over the bytes at `origin = a`, or every non-overwritten committed byte is lost. Both `read`
  and `flush_ref` now read base from `fo.origin()`. `FileOverlay.origin` gained a `pub fn
  origin()` getter (it was a private, `#[allow(dead_code)]` field in C2A).

- **Flush ordering — `a→b` then a fresh write to `a`.** The naive "remove the rename source"
  would delete the freshly recreated `a`. The fix: read all file bases against the **immutable
  pre-flush root snapshot** (so removals can't precede a base read), and compute removals as
  `tombstones ∪ {differing origins}` **minus the live destination names** — a destination a live
  file is about to (re)create is never removed. Test: `rename_then_recreate_source_keeps_both`.

- **Directory rename — `dir_renames` + flush-first, classified at apply (the subtlest piece).**
  A directory has no dirty bytes, so its move can't be a `FileOverlay`. `apply_to_overlay`
  classifies the rename source: a live overlay file takes the O(1) swap; otherwise it looks the
  source up in the committed tree — a file is opened into the overlay (moves via `origin`), a
  directory is recorded in `dir_renames` and executed at flush as a `tree::remove`+`tree::put`
  detach/reattach. This made `apply_to_overlay` fallible (a tree lookup), propagated through its
  three call sites (the two `log_then_apply` paths + mount replay). `Store::rename` drains the
  ref first for a directory (DD4) so nothing dirty hides under the source. Crash-safe because the
  path-keyed `Rename` record replays through the same classification. Tests:
  `rename_directory_detach_reattach`, `acked_unflushed_rename_replays_after_crash`.

- **`StoreError::NotFound` added.** The store treats absent files as `Ok(None)`/no-op elsewhere,
  but a rename of a nonexistent source must fail before the WAL (an acked record must have
  something to move). `store_err` in `storage-server` maps it via the existing `_ => Internal`
  catch-all for now; C2D adds the proper wire arm with the rename dispatch.

- **No spec edit needed.** rev1§4.3/§4.9/§8.2 already describe file-id keying, O(1) rename,
  O(depth) directory moves, and cross-subtree denial as blessed design (C2 conforms); the C2A
  code comment that carried the deferral is already gone. rev1§6.1(e) carries no numeric verify
  total — that lives in the trusted-base ledger, whose Baselines and `wal_checksum_ok` rows were
  updated (count unchanged, note the tag-3 extension, no new seam; line refs refreshed).

## Negative controls (anti-theater)

- `acked_unflushed_rename_replays_after_crash` builds two otherwise-identical stores — one that
  issues the rename, one that doesn't — crashes and remounts both, and asserts the recovered
  states **differ** (`b`=content/`a`=absent vs `b`=absent/`a`=content). If replay ignored the
  `Rename` record, the two would be identical.
- The overlay model proptest (`overlay_matches_path_model`) and the store crash proptest
  (`crash_recovery_preserves_renamed_state`) check against a path-keyed reference that *does*
  follow renames; a model that forgot to move the destination would mispredict and fail.

## Out of scope (per the plan, unchanged)

The wire `Request::Rename` + `mv` shell built-in (C2D); unlink-while-open + the open-handle
interleaving proptest (C2C); cross-ref rename; the dirty-descendant directory-rename
prefix-update optimization (C2B flushes the ref first instead).
