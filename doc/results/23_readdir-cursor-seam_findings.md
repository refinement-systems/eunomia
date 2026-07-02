# Findings 23 — readdir cursor seam (C1.1, review finding 8)

Task **C1.1** of `doc/plans/3_plan-std-correction.md`, acting on finding 8 of the
independent review (`doc/results/22_std-port-review.md`): the readdir path carried a
bespoke second serialization across the `__eunomia_*` bridge. This is also the "full
fix" that finding 20-1 deferred — its `RD_ENTRY_HEAD == 11` guard was explicitly
"partial, by design … whose full fix needs the bridge to carry structured data"
(`doc/results/20-1_cross-bridge-lockstep-guard_findings.md`).

**Headline:** the flat `[tag][kind][size:u64 LE][name_len:u16 LE][name]` readdir buffer is
**deleted on both sides** and replaced by a cursor protocol in the existing seam
vocabulary — a snapshot handle plus a `#[repr(C)]` head (the `Meta`/`FsMeta` posture). No
byte layout crosses the bridge anymore, so there is no codec to verify and none to keep in
lockstep: the hand-rolled `parse_listing` decoder and the `RD_*` encoder/guard are gone,
the PAL arm is a genuine thin delegator, and one of the three review-kept cross-bridge
duplications ceases to exist. The `__eunomia_*` symbol set goes **38 → 40**; no verified
count moves (eunomia-sys stays **16**); the ledger tally is untouched.

## The problem (from finding 8)

storaged already sends structured `Response::Listing(Vec<DirEnt>)` over the sanctioned
postcard body seam. `eunomia-sys` postcard-decoded it, then **re-encoded** it into a flat
`Vec<u8>` purely to cross the `extern "Rust"` bridge, and the std PAL's `parse_listing`
**re-decoded** it. Verifying the encoder was impossible to make total — std cannot depend
on `vstd`, so the PAL decoder would stay unverified forever — and the module docs' "thin
marshalling shell" / "no byte-parsing logic" claims were therefore false.

## Decision — restructure the seam, don't verify the codec (Global decision 1)

Replace the one `Vec<u8>` shim with three, keeping the listing structured the whole way:

- `__eunomia_fs_readdir_open(path: &[u8]) -> i64` — runs the `List` round-trip, stashes the
  postcard-decoded `Vec<DirEnt>` snapshot behind an integer handle (`>= 0`), or returns a
  negative fs code. An error surfaces at `read_dir` time, exactly as the old tagged buffer
  reported it (not mid-iteration).
- `__eunomia_fs_readdir_next(handle: i64, name_buf: &mut [u8]) -> DirEntMeta` — copies one
  entry's name into the caller's buffer and returns the `#[repr(C)]` head
  `{ code: i64, kind: u8, size: u64, name_len: u16 }`. `code`: `0` = entry, `1` = end,
  `< 0` = fs code. `kind`: `0` = file, `1` = dir.
- `__eunomia_fs_readdir_close(handle: i64)` — frees the slot, from the std `ReadDir` drop.

Snapshot semantics are unchanged: today's flat buffer was already the same whole-listing
snapshot; it now lives as `Vec<DirEnt>` behind the handle instead of as bytes.

*Rejected:* verifying the flat encoder in place — it leaves the PAL decoder unverifiable,
the thinness claim still false, and the cross-bridge byte duplication alive (Global
decision 1). *Rejected:* keeping the cursor on the std side and passing an index per call —
the snapshot must live somewhere across calls anyway, and a server-side cursor keeps the
head signature index-free and the std `ReadDir` a bare `{ parent, handle }`.

## What shipped

- **New host module `eunomia-sys/src/readdir.rs`** (not `cfg(bare_metal)`): the `#[repr(C)]`
  `DirEntMeta` head + `end()`/`err()` constructors, and the pure
  `entry_head(kind, size, name, name_buf)` name-copy arithmetic — core-only, no `DirEnt`
  (a target-only type), so it host-unit-tests. Six tests cover file/dir heads, empty name,
  exact-255 fit, the over-long refusal (buffer untouched → `ERR_FS_INTERNAL`, never
  truncated), and the end/err heads. `fs.rs` re-exports `DirEntMeta` so the shim returns
  `fs::DirEntMeta`, paralleling `fs::Meta`.
- **`eunomia-sys/src/fs.rs`:** deleted the flat encoder `readdir`/`err_buf` and the
  `RD_OK`/`RD_ERR`/`RD_ENTRY_HEAD` consts + `const _: () = assert!(… == 11)` guard. Added a
  spinlock-guarded snapshot table (`ReadDirTable { lock: SpinLock, slots:
  UnsafeCell<Vec<Option<DirHandle>>> }` + `unsafe impl Sync`, the `urt::random` `STATE`
  posture), and `readdir_open`/`readdir_next`/`readdir_close`. `next` maps `DirEnt` to
  `(kind, size, name)` and calls `entry_head`; the cursor advances on every consumed entry
  (including an over-long refusal, so a resilient consumer terminates). Module doc rewritten
  so "no byte-parsing logic lives here" is true.
- **`eunomia-sys/src/pal.rs`:** the one `__eunomia_fs_readdir` shim replaced by the three
  one-line delegators.
- **std `vendor/rust/library/std/src/sys/fs/eunomia.rs`:** the mirrored `#[repr(C)]`
  `FsDirEntMeta`, `ReadDir` reduced to `{ parent, handle }` (`RawEntry` deleted),
  `Iterator::next` pulling one entry per `readdir_next`, `impl Drop for ReadDir` calling
  `readdir_close`, and `readdir()` opening the handle. `parse_listing` deleted; module doc
  + extern-block comment rewritten.
- **`doc/guidelines/forward-port.md`:** §3.5 fs enumeration now lists
  `readdir_open,readdir_next,readdir_close`; §5's cross-bridge-duplication note drops
  `readdir-layout` (now `STATUS_PANIC` and `io_error::Kind` only). The new
  `DirEntMeta`/`FsDirEntMeta` head is an ordinary un-guarded `#[repr(C)]` twin like
  `Meta`/`FsMeta`, which that note already does not track.

## Verification record

- **Host tests** — `cargo test -p eunomia-sys`: `35 passed; 0 failed` (incl. the six new
  `readdir::tests`).
- **Target compile** — `cargo build -p eunomia-sys --target aarch64-unknown-none`
  (exercises the `cfg(bare_metal)` `fs.rs`/`pal.rs`): clean. Full `cd kernel && cargo build`
  (build-std rebuilt std from the edited source, user binaries linked): clean — a clean link
  is itself the extern-decl ↔ shim lockstep check.
- **Verus** — `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys`:
  `verification results:: 16 verified, 0 errors` (real run, line present; no `verus!{}`
  touched). Binary `0.2026.06.07.cd03505`, toolchain `1.95.0` (matches the pin).
- **Symbol lockstep** — declared (all vendored-std arms) vs defined (`pal.rs`) `__eunomia_*`
  sets: empty symmetric difference, count **40** on both sides.
- **QEMU** — `scripts/fs-smoke-test.sh`: `FS SMOKE TEST PASS` / `STD4 PASS` (create/write/
  read/**readdir**/rename/remove/sync at EL0). `scripts/std-smoke-test.sh`: `STD SMOKE TEST
  PASS`, incl. "shell on std — write/cat/**ls** over std::fs". Both run under the group-kill
  timeout wrapper (CLAUDE.md); no orphaned QEMU.
- **fmt** — `cargo fmt --check` clean. No `verus!{}` touched ⇒ no `verusfmt`/baseline.

## Surface left trusted

- The `DirEntMeta`/`FsDirEntMeta` `#[repr(C)]` head is a review-coupled twin with no
  compile-time cross-check — the accepted `Meta`/`FsMeta` posture, not a bespoke codec. The
  layout is the compiler's `#[repr(C)]` of primitive fields; a field reorder on one side
  would be caught only by review (as for `FsMeta`). This is strictly less trusted surface
  than the deleted flat buffer, which duplicated a full serialization.
- The over-long-name refusal defends against a storaged that violates the 255-byte
  component bound the verified path resolver enforces — a should-never-happen server
  invariant break, mapped to `ERR_FS_INTERNAL`. The name buffer is a client capacity
  choice, not a wire-lockstep constant: an under-sized buffer refuses (never desyncs).

## Follow-ups

- The single-256-byte-message listing cap is unchanged (a directory whose listing overflows
  one message still errors `ERR_FS_INTERNAL`) — it awaits the bulk data plane (rev2§3.1),
  the same disclosed limit as before. The snapshot table would let a future chunked `List`
  accumulate across messages behind the handle without touching the seam shape.
- Verified write-direction `le-bytes` is now moot for readdir (Deferred work in the plan):
  C1.1 deletes the last hand-rolled LE encoder that would have used it.
