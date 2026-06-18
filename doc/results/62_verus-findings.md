# Verus findings 42 — Phase 7f: `cas::disk` superblock + the Kani-tier retirement

Plan: `doc/plans/3_verus-rewrite.md` (§4.7, §7 step 6) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7f). Prior increment: `61`
(phase 7e — `dma-pool`). This increment is the sixth and **last** host-chokepoint
migration: the on-disk superblock chokepoint in `cas/src/disk.rs` — from Kani
(SB head bytes symbolic, `Hash::of` `-Z stubbing`'d) to Verus (unbounded ∀ buffer
bytes / field values / device length). `cas` was the **last crate on Kani**, so
7f also resolves the master plan's holdout question (`cas::disk` ports cleanly —
no `Vec` in the two functions — so it is **not** a holdout) and **retires the
`kani` CI job entirely**. With 7f, every §4.7 host chokepoint is proven unbounded
in Verus and Kani leaves the project's CI.

`cargo verus verify -p cas --no-default-features`: **11 verified, 0 errors**.
`cargo test -p cas`: green (the `verus!{}` block erases — `superblock_roundtrip_and_tearing`
and the MNT-1 forged-field-rejection regressions run the same code through the
rewritten parsers). `cargo test --workspace --exclude kernel`: green (the `cas`
consumers `storage-server`/`mkfs`/`virtio-blk`/`storaged` use only the unchanged
public API — same `Superblock`/`SbError` signatures, same `pub const`s). `cd
kernel && cargo build` + a forced `user/storaged` aarch64 rebuild: green — the new
`verus!{}` block erases into storaged's no_std cross-build (vstd already arrived
transitively via `ipc`/`urt` since 7a, so this edge adds no new binary). `cargo
kani`: **gone** — the whole job is removed (`cas/src/proofs.rs` deleted, its two
harnesses were the last in the project).

---

## 1. What was bounded, and the two ∀ theorems

`cas/src/proofs.rs` held two harnesses:

- `check_superblock_geometry` — `validate_geometry` totality (`kani::any()` fields
  + `dev_len`, all `checked_add`) and the safety invariant "committed region ⊆
  device" on the `Ok` path. Symbolic, but bounded to the harness's single
  constructed `Superblock` shape.
- `check_superblock_decode_total` — `decode_checked` totality over 128 symbolic
  head bytes, with `#[kani::unwind(34)]` and `Hash::of` stubbed by a total ghost
  hash (`-Z stubbing`; "totality needs no collision-freedom" — the harness's own
  note).

Phase 7f makes both ∀:

- **`validate_geometry_fields`** (the verified core `validate_geometry` delegates
  to): `ensures (r is Ok) <==> geometry_ok(..)` and `r is Ok ==> WAL_OFF + wal_len
  + chunk_tail <= dev_len` — totality plus the region-within-device invariant for
  *all* field values and device lengths. The `<==>` is **overflow-exact**: the
  spec is stated over `int`, and the exec's `checked_add` rejections coincide
  exactly with the cases where a clause would wrap past `u64::MAX >= dev_len`.
- **`decode_checked_fields`** (the verified core `decode_checked` delegates to):
  verifying it at all **is** the totality theorem — Verus proves every
  fixed-offset read in bounds and every shift/`|` non-overflowing for all inputs.
  Unbounded; no `unwind`.

The unit tests + the cargo-fuzz targets (`superblock`, `mount_recovery`, …) stay
as differential/regression coverage (§5 discipline).

## 2. The shape: verified byte-parsing core / trusted blake3+`Hash` seam

The 7e split (verify the arithmetic, trust the seam) maps cleanly onto a decoder:
the **panic risk Kani checked is the byte/index arithmetic**, and the one piece
that *can't* enter Verus is blake3 (interpreted hashing — exactly what Kani
`-Z stubbing`'d). So:

- A `verus!{}` block holds the verified core: `validate_geometry_fields`,
  `decode_checked_fields`, the read helpers (`magic_ok`, `read_u32_le`,
  `read_u64_le`, `read_arr32`), the `geometry_ok` spec, a Verus-native
  (`Hash`-free) `RawSuperblock { generation, ref_table: [u8;32], wal_head, … }`,
  and `SbError` (moved in so its variants are constructible in verified code).
- `decode_checked_fields` returns `RawSuperblock` (plain integers + `[u8;32]`) —
  **no `Hash`, no `Superblock`** in verified code, so neither needs an
  `external_type_specification`.
- `Superblock::decode_checked` stays plain Rust: a thin, trivially-total wrapper
  that calls the verified core and wraps `f.ref_table` into a `Hash` (
  `Hash::from_bytes` and the struct literal never panic). `Superblock::validate_geometry`
  likewise delegates to `validate_geometry_fields`.
- The blake3 checksum gate is **one `#[verifier::external_body]` helper**,
  `checksum_ok(buf)` — assumed total under `requires buf@.len() == SB_SIZE`
  (Verus does not look inside the `Hash::of` call or the slice `==`). This is the
  honest line: blake3 is the assumed-total boundary, the same one Kani drew with
  `-Z stubbing`. Totality needs no collision-freedom.

This avoids the heavier alternative the plan flagged (bring `Hash` in via
`external_type_specification` + `assume_specification` for `of`/`as_bytes`/`from_bytes`
and verify `decode_checked` directly) — the integer-core extraction is strictly
less machinery and keeps the `Hash` type off the proof surface entirely.

## 3. The decode-totality recipe (the 7a `ipc::header` pattern, reused)

The read helpers use **explicit byte indexing + mask/shift**, never
`from_le_bytes`/`try_into().unwrap()`/slice `==` (all unspecced by Verus — the 7a
finding):

- `read_u32_le`/`read_u64_le`: `(buf[off] as uN) | ((buf[off+1] as uN) << 8) | …`,
  each `requires off + width <= buf@.len()`, `broadcast use vstd::slice::group_slice_axioms`.
  The shifts are non-overflowing for free (a `u8` cast is `<= 255`, so
  `<< 56` stays in `u64`) — no `by (bit_vector)` needed, since totality (not a
  round-trip value spec) is the obligation.
- `magic_ok`: the slice `&buf[0..8] != SB_MAGIC` becomes eight per-byte equalities
  against numeric literals.
- `read_arr32`: the `[u8;32]` is built as a 32-element array literal of `buf[off+k]`
  (no `try_into().unwrap()`), each index in bounds from `off + 32 <= buf@.len()`.
- `decode_checked_fields` itself opens with `broadcast use vstd::slice::group_slice_axioms`
  so the entry `buf.len()` links to `buf@.len()` before the helper calls (the
  helper preconditions are stated as `buf@.len() == SB_SIZE`, established by the
  length guard).

## 4. Two toolchain gotchas worth recording

- **Byte-char literals are an "Unsupported constant type".** `buf[0] == b'E'`
  fails inside `verus!{}`; the numeric form `buf[0] == 0x45u8` is fine. (The 7a
  header never compared a magic, so this is new to the decoder family.)
- **A `const` declared outside `verus!{}` is invisible to it** — "cannot use
  function `cas::disk::SB_SIZE` which is … declared outside the verus! macro."
  The four consts the verified code names (`SB_SIZE`, `WAL_OFF`, `SB_VERSION`,
  `CHUNK_HEADER`) are **moved into the block**; they erase to ordinary `pub const`s
  at the same module path, so external references (`store.rs`, the chunk-frame
  code, `encode`) are unchanged. `SB_BODY` stays outside — it is named only inside
  the `external_body` `checksum_ok`, whose body Verus does not analyse. (header.rs
  sidestepped this by defining `HEADER_SIZE` inside the block from the start;
  cas's consts pre-existed and are shared, so they had to be relocated, not
  duplicated.)

## 5. The holdout decision and the Kani-tier retirement

The plan left one question open: is `cas::disk` a Kani holdout, or does Kani
retire wholesale? **Answer: not a holdout.** Neither target touches `Vec` (the
`Vec`/`BTreeMap` weight in `cas` is in the index/WAL/reftable decoders, which stay
cargo-fuzz-primary and are out of 7f scope); the only friction was the `Hash`
type, sidestepped by the integer-core split (§2). So, per the user-confirmed
scope and the §5/per-phase "delete the subsumed harness in the same PR"
discipline, this PR **removes the entire `kani` CI job** — cas was its last
target, so what remained would have been an empty job tripping the cover-vacuity
guard. Gone with it: the pinned-`cargo-kani-0.67.0` cache + install steps and the
`kani::cover!` vacuity guard.

What stays for a later closeout (phase 7g / 9, doc-only): the spec `§6` tier-table
edit, the `0_kani-rewrite.md` closeout banner, and the `cas::tlv` *additive* Verus
proof (no Kani harness to delete there — already fuzz-primary; recommend deferring
unless cheap). The Kani historical record (`doc/results/2…8_kani-findings*.md`)
is untouched.

## 6. What changed

- `cas/src/disk.rs` — one `verus!{}` block holding the four relocated consts, the
  `geometry_ok` spec, `validate_geometry_fields`, `RawSuperblock`, `SbError`
  (moved in), `magic_ok`/`read_u32_le`/`read_u64_le`/`read_arr32`, the
  `external_body` `checksum_ok`, and `decode_checked_fields`; `Superblock::validate_geometry`
  and `Superblock::decode_checked` rewritten as thin delegators; `use
  vstd::prelude::*;`. `encode`, the struct `Superblock`, `SB_MAGIC`/`SB_BODY`, the
  index/WAL/reftable/chunk code, and the unit tests stay outside, verbatim.
- `cas/src/proofs.rs` — **deleted** (`check_superblock_geometry`,
  `check_superblock_decode_total` — the last Kani harnesses in the project).
- `cas/src/lib.rs` — `#[cfg(kani)] mod proofs;` removed.
- `cas/Cargo.toml` — `vstd` dep + `[package.metadata.verus] verify = true` added;
  `unexpected_cfgs` swaps `cfg(kani)` for the Verus cfgs.
- `.github/workflows/ci.yml` — `verus` job: `cargo verus verify -p cas
  --no-default-features` appended (+comment, +header-comment update); `kani` job:
  **removed entirely** (replaced by a retirement note), plus the stale "the kani
  job also prizes" reference in the `concurrency` job re-pointed to `verus`.
- `CLAUDE.md` — the `cargo verus` examples (+`-p cas`) and the dropped `cargo kani`
  example; the `kani`/`verus` CI bullets; the verification-tiers table rows; the
  `### Verus` / `### Kani` prose (cas on Verus; Kani retired from CI).

## 7. Next

**7g / phase 9 — closeout (doc-only):** spec `2_spec_rev2.md` §6 tier table, the
`0_kani-rewrite.md` closeout banner, and the `cas::tlv` additive-proof decision.
**Phase 8 — `cas::store`** recovery-core extraction + `AckedWritesRecoverable` on
the pure decision function (master plan §4.8) remains the last verification target.
§2–§4's integer-core/seam split, the explicit-indexing decode recipe, the
const-relocation rule, and the byte-literal gotcha carry forward.
