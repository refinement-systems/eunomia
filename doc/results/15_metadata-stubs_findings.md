# Findings 15 — std-port 4.3: fs metadata + errno decision table + unsupported stubs

Task 4.3 of `doc/plans/2_plan-std-revised.md` (findings #15), the last sub-phase of the
filesystem track. It turns the 4.1 first-cut fs surface into a real one: `fs::metadata`
now reports directories, every storaged `ErrorCode` maps to a specific `io::ErrorKind`,
a path-confinement escape gets a distinct errno from a malformed name, and the
`Unsupported` surface is a confirmed clean sweep. Proof of life: the QEMU fs gate
(`scripts/fs-smoke-test.sh`) prints the new `[stdfs] dotdot resolves; escape->denied,
malformed->invalid`, `[stdfs] metadata ok`, and `STD4 PASS`, reaped `exited(0)` →
`FS SMOKE TEST PASS`.

## What shipped

- **`eunomia-sys/src/io_error.rs` — the full rev2§4.9 errno decision table.** The 4.1
  `classify` "first cut" (which folded the fs band into `NotFound`/`PermissionDenied`/
  `InvalidInput`/`Uncategorized`) is replaced by the complete map. Six `Kind` variants
  were **appended** (discriminants 7–12, so the ABI-locked 0–6 stay put):
  `NotADirectory`, `ReadOnlyFilesystem`, `StaleNetworkFileHandle`, `InvalidFilename`,
  `NotConnected`, `ResourceBusy` — each named to match its `io::ErrorKind` target. The
  `FS` oracle table and the `message` labels were updated; two labels tightened
  (`BadHandle` → "bad storage handle", `Stale` → "storage handle revoked (generation
  mismatch)"). No `verus!{}` here — host-tested by proptest, per the module's own design.
- **`vendor/rust/library/std/src/sys/io/error/eunomia.rs` — lockstep.**
  `decode_error_kind` gained the six arms `7..=12` mirroring the new `Kind`
  discriminants (all six are *stable* `io::ErrorKind`s, 1.83–1.87, usable inside std).
- **`eunomia-sys/src/path.rs` (verified) — the escape/malformed split.** `resolve` now
  returns `Result<ResolvedPath, RejectReason>` (`RejectReason::{Escape, Malformed}`)
  instead of `Option`. A depth-0 `..` (rev2§2.3 confinement escape) → `Err(Escape)`; a
  NUL / > 255-byte / too-deep component → `Err(Malformed)`. The verified theorem is
  unchanged (`ensures r matches Ok(p) ==> well_formed_resolved`); the error arm carries
  no obligation, so totality + well-formedness hold as before.
- **`eunomia-sys/src/fs.rs` — errno translation + the metadata probe.** `resolve_path`
  now returns `Result<_, i64>`, mapping `Escape → ERR_FS_DENIED` (`PermissionDenied`)
  and `Malformed → ERR_FS_BAD_PATH` (`InvalidFilename`); all seven callers were updated.
  New `#[repr(C)] Meta { code, size, is_dir }` + `metadata()` resolve a path's kind by
  probing `Stat` then `List` (see Problems for the corrected semantics).
- **`eunomia-sys/src/pal.rs` — the seam.** New `__eunomia_fs_metadata(path) -> fs::Meta`
  shim (a one-line forward).
- **`vendor/rust/library/std/src/sys/fs/eunomia.rs` — directory-aware metadata + doc
  sweep.** A `#[repr(C)] FsMeta` mirror + `__eunomia_fs_metadata` in the extern block;
  `stat_attr` and `File::file_attr` route through it (both `is_dir: false` hard-codes
  dropped). `stat`/`lstat` stay thin over `stat_attr`; `stat_size`/`__eunomia_fs_stat`
  stay for the `File::open`/`seek(End)` size-only paths. The module doc header was
  refreshed (metadata carries dir type; mtime/atime stay deferred; the `Unsupported`
  surface confirmed). No functional change to the stub set — the 4.1 stubs
  (symlink/hard_link/readlink/canonicalize, permissions/`set_perm`/`set_times`,
  `truncate`, `mkdir`/`rmdir`, locks, `duplicate`) already return `Unsupported`.
- **Tests / fuzz.** The fuzz differential (`fuzz/fuzz_targets/path.rs`) and the always-run
  `tests/path_proptest.rs` now check the reject **reason** (escape vs malformed) via a
  `Result<_, tag>` oracle, not just accept/reject; `tests/fuzz_regressions.rs` pins the
  specific escape/malformed cases; `tests/fuzz_corpus.rs` follows the `Option`→`Result`
  change. Two corpus seeds added (`seed-escape-dotdot`, `seed-malformed-nul`).
- **`user/stdfs` + `scripts/fs-smoke-test.sh`.** The QEMU fixture asserts directory/file
  `metadata`, escape → `PermissionDenied`, and malformed → `InvalidFilename` end-to-end.

## The decision table

| raw code | source | `Kind` | `io::ErrorKind` |
|---|---|---|---|
| `ERR_FS_NOT_FOUND` | `Response::NotFound` | `NotFound` | `NotFound` |
| `ERR_FS_NO_SUCH_SNAPSHOT` | `NoSuchSnapshot` | `NotFound` | `NotFound` |
| `ERR_FS_DENIED` | `Denied` (+ confinement escape) | `PermissionDenied` | `PermissionDenied` |
| `ERR_FS_BAD_PATH` | `BadPath` + malformed-path reject | `InvalidFilename` | `InvalidFilename` |
| `ERR_FS_NOT_A_DIR` | `NotADir` | `NotADirectory` | `NotADirectory` |
| `ERR_FS_READ_ONLY` | `ReadOnly` | `ReadOnlyFilesystem` | `ReadOnlyFilesystem` |
| `ERR_FS_STALE` | `Stale` | `StaleNetworkFileHandle` | `StaleNetworkFileHandle` |
| `ERR_FS_PINNED` | `Pinned` | `ResourceBusy` | `ResourceBusy` |
| `ERR_FS_NO_SESSION` | client (no session) | `NotConnected` | `NotConnected` |
| `ERR_FS_BAD_HANDLE` / `ERR_FS_BAD_TICKET` / `ERR_FS_BAD_OFFSET` | `BadHandle`/`BadTicket`/`BadOffset` | `InvalidInput` | `InvalidInput` |
| `ERR_FS_INTERNAL` | `Internal` + client transport | `Uncategorized` | `Uncategorized` |

## Decisions (and the alternatives)

- **Rich, specific errno mapping (chosen) vs the 4.1 minimal set.** Each of the 11
  `ErrorCode` variants maps to its nearest *stable* `io::ErrorKind`. Rejected the
  minimal keep-the-small-`Kind`-set alternative: the specific kinds (`NotADirectory`,
  `ReadOnlyFilesystem`, …) are all stable and let callers pattern-match meaningfully,
  matching upstream's unix arm. Cost: six appended `Kind` variants + the lockstep arms.
- **`Stale` → `StaleNetworkFileHandle`, `Pinned` → `ResourceBusy` — documented
  nearest-analogs, not verification properties.** Per the plan, these two have no clean
  POSIX analog. `Stale` is a rev2§2.2 handle generation-mismatch (mass-revoke), *not* a
  network FS — `StaleNetworkFileHandle` (ESTALE) is the closest std kind; `Pinned` is a
  rev2§4.7 tag pin refusing deletion ≈ EBUSY → `ResourceBusy`. Both choices are
  documented at their `Kind` variant and here; neither is proven.
- **Escape/malformed split done now (chosen) vs deferred.** The user elected to do the
  4.2-noted refinement in 4.3: teach the verified `resolve` to report *why* it rejected
  so a confinement escape becomes `PermissionDenied` (rev2§2.3 "unnameable → denied").
  Rejected keeping both as `ERR_FS_BAD_PATH`. This touched verified code (see below).
- **Directory metadata via a client-side `Stat`→`List` probe (chosen) vs a wire
  extension.** Keeping the logic in the seam crate (thin PAL) and probing over the
  existing protocol avoids a storaged `Response`/`PROTO_VERSION` change — consistent
  with mtime staying a deferred wire extension. Cost: a disclosed large-directory limit.
- **A `#[repr(C)]` `Meta`/`FsMeta` struct across the seam (chosen) vs an `i64` sentinel.**
  A named `#[repr(C)]` struct returned by value is self-documenting and sound across
  `extern "Rust"` (same rustc/std, the `Vec<u8>`-return precedent); a size-or-sentinel
  `i64` would be fragile.

## Problems hit

- **`Stat` of a directory returns `Err(BadPath)`, not `NotFound`.** The initial probe
  (probe `List` only on a `Stat` `NotFound`) failed the QEMU gate: `fs::metadata("docs")`
  came back `-260` (BadPath). Reading `storage-server/src/lib.rs`, the `Stat` handler
  does a content `read`, which for a directory returns `StoreError::NotAFile` →
  `ErrorCode::BadPath`; only a genuinely absent path returns `Ok(None)` → `NotFound`.
  Corrected the probe to trigger `List` on `Err(BadPath | NotADir)` and treat `NotFound`
  as absent — which is *cleaner*: an absent path keeps its precise `NotFound` errno with
  no wasted `List`.
- **`-Zbuild-std` did not rebuild the vendored std after editing it.** The user binaries
  relinked a stale `libstd` rlib (hours old), so the new `decode_error_kind`/fs arm
  didn't take effect (malformed reported `Uncategorized`). Fix: `rm -rf target/user` +
  `touch kernel/build.rs` to force `build.rs` to re-run the sub-cargo, which then rebuilt
  std from the edited sources. (Recorded for the forward-port runbook — editing a
  vendored std arm needs a forced std rebuild.)
- **A broken `wait_for` grep pattern, not a code bug.** The metadata marker was
  `metadata dir+file ok`; the harness pattern `dir\+file` is "one-or-more `r`" in GNU BRE
  and never matches the literal `+`. Simplified the marker to `[stdfs] metadata ok`.

## Verification record

- **Host roundtrip gate (the plan's 4.3 gate):** `cargo test -p eunomia-sys` → **22**
  lib tests (incl. the refined `fs_band_is_exact_and_disjoint_from_syscall_band` and the
  fixed `unmapped_codes_are_uncategorized` proptest, which now excludes both the `ABI`
  and `FS` bands — a latent bug the 4.1 fs band introduced), **5** `fuzz_regressions`
  (reason-aware), **3** `path_proptest` (now checking the reject reason), **1**
  `fuzz_corpus`. All pass.
- **Verus:** `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` →
  `16 verified, 0 errors` (count unchanged — the `resolve` edit is return-type only).
  Perf per `doc/guidelines/verus.md` §10: re-derived before/after with
  `scripts/verus-baseline.sh eunomia-sys` on the pre- and post-change trees.
  `path::resolve` rlimit **218169 → 231432** (+~6%, attributable to the `Result` /
  `RejectReason` return type — a *feature* change carrying the reject reason, not a
  perf-motivated one; the crate SMT total is flat at 87–88 ms). No spec was weakened.
- **Fuzz:** `cargo +nightly fuzz run path -- -max_total_time=30` → **9.48M** runs, no
  failures (the reason-aware differential holds). Corpus replay green under host test.
- **Target build:** `cd kernel && cargo build` compiles the std arms, the target-gated
  `fs.rs`/`pal.rs`, and cross-builds `user/stdfs`.
- **QEMU end-to-end:** `scripts/fs-smoke-test.sh` → `FS SMOKE TEST PASS` with
  `escape->denied, malformed->invalid`, `metadata ok`, `STD4 PASS`, `exited(0)`.
- **Formatting:** `cargo fmt --check`, `scripts/verusfmt.sh --check` (for path.rs),
  the fuzz and stdfs manifests — all clean.

## Surface left unsupported or trusted (and why)

- **mtime/atime stay `Unsupported`.** `FileAttr::{modified,accessed,created}` remain
  `unsupported()` — mtime is a mandatory rev2§4.9 field absent from the wire protocol
  (a deferred storage-wire extension per the plan's Deferred-work); atime does not exist
  (rev2§4.9).
- **The `Unsupported` stub set** (symlink/hard_link/readlink/canonicalize; permissions/
  `set_perm`/`set_times`; `truncate`/`set_len`; `mkdir`/`rmdir`; file locks; `duplicate`)
  is unchanged from 4.1 — 4.3 only confirmed it. These are `Unsupported` by construction
  (rev2§4.9 has none of it), not stubs awaiting implementation.
- **Subtree confinement is fuzz/test-routed at dispatch, not proven (the plan's mandated
  note).** Verus proves `path::resolve`'s *output* well-formedness — every accepted
  component is a storable name and **no `..` survives**, so prepending it to the handle
  subtree cannot escape. But that the *dispatch* actually confines (that an escape is
  denied rather than silently walked) is checked by the fuzz differential (the reason
  oracle) and the QEMU `escape → PermissionDenied` assertion, **not** by a proof. The
  reject-reason bucket (escape vs malformed) is likewise a fuzz/proptest property, not a
  Verus obligation.

## Ledger

No change to `doc/guidelines/verus_trusted-base.md`: the trusted-seam **tally stays 14**,
the `eunomia-sys` Baseline row **stays 16 verified / 0 errors** (the `resolve` return-type
edit is count-neutral), and **no new seam** was added. `io_error.rs` carries no `verus!{}`
obligation; the errno table is a host-tested (proptest) policy artifact, per its module
design. The metadata probe and the std arms are trusted marshalling shells over the
verified surfaces (the `sys/fs` posture), auditable against `pal/unsupported`.

## Follow-ups

- **`BadPath` server overloading.** storaged folds both "malformed path" and some
  not-found/not-a-file conditions into `ErrorCode::BadPath`; the client maps `BadPath` to
  `InvalidFilename`. Disentangling the server codes (so a not-found isn't labeled a bad
  filename) is a storaged-side refinement, not done here.
- **Large-directory metadata.** `metadata()` on a directory whose `List` listing overflows
  one 256-byte message probes as `ERR_FS_INTERNAL` — the same cap `readdir` discloses,
  resolved by the deferred bulk-window data plane (rev2§3.1).
- **mtime on the wire** (deferred): would make `File::metadata().modified()` real.
- **Forward-port note:** editing a vendored std arm requires a forced `-Zbuild-std`
  rebuild (`rm -rf target/user` + `touch kernel/build.rs`) — cargo does not detect the
  sysroot-source change. Worth capturing in the 6.3 runbook.
- **(Unrelated, observed)** `urt/src/tls.rs` is not in `scripts/verusfmt.sh`'s SKIP list
  yet verusfmt 0.7.2 reshapes the plain-Rust comments around its single `verus!{}` block
  (the documented mishandling). CI does not gate verusfmt, so this is a latent local-only
  drift, left untouched here to keep this change scoped.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
