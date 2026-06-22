# B15A findings — mkfs directory-walk coverage (bin→lib + name rule + walk/determinism oracle)

**Phase:** B15A of `doc/plans/16_b15-detail.md` (baseline test backfill), the headline
sub-phase: give `mkfs`'s `populate` directory walk real unit/property coverage. Closes the
`mkfs` half of the audit §4.2 gap (`doc/results/0_audit_rev0.md:514-517`). **Test-only**: no
on-disk byte change, no wire change, no public type any other crate consumes, no spec edit,
no Verus/TLA gate touched (mkfs is rev1§6 Baseline tier, not a proof-boundary seam; the seam
tally stays **14**).

## Pre-implementation findings (from exploration)

1. **`Store<D: BlockDev>` is fully generic, so the walk is testable in-process with `MemDev`.**
   `cas/src/store.rs:1451` declares `pub struct Store<D: BlockDev>`; the whole API lives in
   `impl<D: BlockDev> Store<D>` (`:1474`) — `format`, `mount`, `create_ref`, `write`, `read`,
   `snapshot`, `snapshots`, `snapshot_root`, `read_at_root` are all generic over the bound.
   `MemDev` (`cas/src/dev.rs:58`, in-memory, not feature-gated) and `FileDev` (`:105`,
   `std`-only) are interchangeable `BlockDev` impls. The fuzz suite already uses
   `Store<MemDev>` (`cas/tests/fuzz_regressions.rs:27`). Signatures that shape the oracle:
   `write(ref: &[u8], path: &Path, off: u64, data: &[u8], mtime: u64)` and
   `read(ref: &[u8], path: &Path) -> Result<Option<Vec<u8>>>`, where `Path = Vec<Vec<u8>>`
   (`cas/src/overlay.rs:16`). Therefore `populate` only needs generalizing from
   `Store<FileDev>` to `Store<D>` — `run()` still infers `D = FileDev`.

2. **Platform finding — the dev host is macOS (Darwin/APFS); it constrains which rejected
   names the *walk* test can materialize.** A `'/'` or NUL byte cannot appear in a real
   filename on any OS; non-UTF-8 names may be rejected by APFS; APFS is also case-insensitive
   by default (so `A`/`a` siblings would collide). So the **walk** proptest draws only
   FS-materializable names:
   - **accepted** = non-empty strings over `[a-z0-9]` (printable ASCII, case-collision-free);
   - **rejected** = names containing a control char in `0x01..0x20` or `0x7F` (each a valid
     single-byte UTF-8 scalar the FS will create, but rejected by the printable rule).
   The `'/'`-in-name and non-UTF-8 rejections are covered **exhaustively** by the pure
   **`name_acceptable` proptest** (no FS at all), which is their correct home. This split is a
   deliberate test-design decision, not a coverage gap.

3. **proptest conventions.** proptest is a `"1"` dev-dependency (`cas/Cargo.toml`,
   `urt/Cargo.toml`); the house idiom is `#[cfg(test)] mod tests` *in `src/`* with
   `#![proptest_config(ProptestConfig { cases: if cfg!(miri) { 4 } else { N }, ..ProptestConfig::default() })]`
   and `prop_assert!`/`prop_assert_eq!` (e.g. `cas/src/overlay.rs:218-223`,
   `urt/src/time.rs:561-565`). B15A follows it: the new tests live in `mkfs/src/lib.rs`'s
   `#[cfg(test)] mod tests`, not a `tests/` file (the existing `tests/image.rs` integration
   tests stay).

4. **The two existing `mkfs` tests don't exercise the walk as logic.** Both spawn the binary
   (`CARGO_BIN_EXE_mkfs`) and assert at the image level — `built_image_mounts_and_matches_source`
   (one two-file tree) and `refuses_undersized_image_cleanly` (B12's S-10 test). Neither
   varies tree shape, hits a skip branch, or checks ordering/count. B15A leaves both intact
   (they also cover the `main`/`ExitCode` path) and adds the property coverage in-process.

## What landed

**bin → bin+lib split (`mkfs/`), behaviour-preserving.**

1. **`mkfs/src/lib.rs` (new).** `mtime_nanos`, `populate`, `run` moved here as `pub`, plus:
   - **`pub fn name_acceptable(name: &OsStr) -> Option<&str>`** — the rev1§4.9 rule factored
     out of the old inline `main.rs:34-43` filter: accept iff valid UTF-8, every byte in
     `0x20..0x7F`, no `'/'`. `populate` calls it; on `None` it `eprintln`-skips (the two
     prior skip messages collapse into one — stderr text only, not tested, not on-disk; the
     *skip behaviour* is byte-identical).
   - **`pub fn populate<D: BlockDev>(store: &mut Store<D>, …)`** — generalized from
     `Store<FileDev>` to `Store<D>` (the sole signature change; `run()` still infers
     `D = FileDev`). This is what lets the host tests drive it against `MemDev`.
   - **`pub fn batch_store_options() -> StoreOptions`** — the one-shot-build tuning extracted
     from `run()` so `run` *and* the walk proptest share the identical config (no drift).
2. **`mkfs/src/main.rs`** — reduced to a thin `fn main() -> ExitCode` over `mkfs::run()`. The
   bin target `mkfs` still exists, so `tests/image.rs`'s `CARGO_BIN_EXE_mkfs` still resolves.
3. **`mkfs/Cargo.toml`** — `[dev-dependencies] proptest = "1"`.

**Tests — `#[cfg(test)] mod tests` in `mkfs/src/lib.rs`** (`mkfs/src/tests.rs`, house
convention; the existing `tests/image.rs` integration tests are untouched):

- **`name_acceptable_golden_boundaries`** — the `0x1F`/`0x20`/`0x7E`/`0x7F` boundaries,
  control chars, interior `'/'`, and non-UTF-8 (`0xFF`, `0x80`, an invalid 2-byte seq).
- **`name_acceptable_empty_is_vacuously_accepted`** — pins the vacuous empty-name case.
- **`name_acceptable_matches_rule`** (proptest, Miri-able, 256/4) — over arbitrary bytes,
  `name_acceptable(b).is_some()` **iff** `from_utf8(b).is_ok() && all 0x20..0x7F && no '/'`.
  This is the home of the `'/'` and non-UTF-8 rejections (not FS-materializable as names).
- **`walk_maps_tree_faithfully`** (proptest, native, 256/4) — generate an in-memory `Node`
  tree (`Dir`/`File`/`Symlink`), materialize to a fresh temp dir (RAII-cleaned `TempTree`),
  `populate` a `MemDev` store, and check the oracle via `check_mount → Result<(), String>`:
  content fidelity (every accepted regular file reads back its bytes), skip discipline
  (rejected-named entries, symlinks, and whole subtrees under a rejected ancestor read
  back `None`), count (`== accepted regular files`), and totality (`populate` returns `Ok`
  on every adversarial tree — refuse-not-crash).
- **`walk_is_creation_order_independent`** (proptest, native, 256/4) — the rev1§6
  canonical-form prose: the same logical tree materialized in opposite creation orders mounts
  to identical logical contents (mkfs's half; the prolly half is B13). Compares *logical
  contents*, not image bytes (the snapshot row carries a `SystemTime::now` timestamp).
- **`oracle_has_teeth`** (unit, the negative control) — `check_mount` rejects a tampered
  expectation along all three axes (corrupted contents, present-claimed-absent, wrong count).

## Verification

| Check | Result |
|---|---|
| `cargo test -p mkfs` | **green** — 6 new lib tests + 2 existing `image.rs` integration tests + 0 doctests |
| `cargo fmt -p mkfs` | clean (root-workspace member; root fmt covers it) |
| `cargo build -p mkfs` | builds (lib + bin auto-detected) |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p mkfs name_acceptable` | **3 name tests pass, no UB** (the Miri-able tier; the walk uses real `read_dir`, stays native) |
| `cd kernel && cargo build` (aarch64) | links the stack (only pre-existing warnings) |
| `scripts/run-demo.sh` (QEMU boot smoke, timeout harness) | green — `mkfs` builds the boot image; store mounts and serves, shell commands echo, no panic/`Corrupt` |

**End-to-end teeth check:** with a temporary one-byte corruption injected into `populate`'s
write path, `walk_maps_tree_faithfully` failed and shrank to the minimal counterexample (a
single file `a` with content `[0]`); reverted after confirming. So the oracle catches a real
walk regression, not just a tampered helper input.

## Ledger / scope

- **No seam, no gate change.** mkfs is rev1§6 Baseline tier, not a `rev1§6.1` proof-boundary
  seam; B15A adds no `external_body`/`assume_specification` (tally stays **14**), no Verus,
  no TLA, no Loom. The kcore/cas/ipc/freelist/dma-pool/urt Verus gates and the three TLA
  models are held by not touching them.
- **Not added to the standing CLAUDE.md Miri sweep.** The walk is `unsafe`-free and its CAS
  write/mount path is already Miri-covered under `-p cas`; the Miri-able mkfs tier is
  `name_acceptable`. The walk proptest carries the `cfg!(miri)` case-count idiom for
  portability but mkfs is not in the swept set.
- **Behaviour-preserving.** No on-disk byte change, no wire change, no public type any other
  crate consumes. The `image.rs` mount test (mkfs output → mount → read-back) and the QEMU
  boot prove the shipped tool is unchanged.

## Out of scope (recorded, not gaps)

- **B15B/B15C** (shell + storaged/init/selftest host tests) — separate, independent.
- **`loader::prepare` page-rounding** — Phase B3, not B15.
- **mkfs S-10 refusal** — already landed in B12 (`tests/image.rs:55-81`).
