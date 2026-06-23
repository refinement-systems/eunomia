# B15B findings — shell non-I/O logic host tests (runtime-module split + golden/proptest core)

**Phase:** B15B of `doc/plans/16_b15-detail.md` (baseline test backfill), the "prize"
sub-phase: give the shell's pure, syscall-independent logic real unit/property coverage. Closes
most of the `user/*` half of the audit §4.2 gap (`doc/results/0_audit_rev0.md:514-517`; the five
user binaries had no automated tests, only QEMU boot output). **Test-only and
behaviour-preserving**: no on-disk byte change, no wire change, no public type any other crate
consumes, no spec edit, no Verus/TLA gate touched (the shell is rev1§6 Baseline tier, not a
`rev1§6.1` proof-boundary seam; the seam tally stays **14**). The shipped aarch64 `ushell` binary
is byte-for-byte unchanged.

## Pre-implementation findings (from exploration)

1. **The plan's Design-decision-2 mechanism was insufficient as written; gating *just* the
   bare-metal items does not make the shell host-buildable.** The plan assumed `#[cfg(not(test))]`
   on `_start`/`#[global_allocator]`/`#[panic_handler]` would suffice because "`ipc::sys` has a
   host-stub branch … so the crates link on the host" (`16_b15-detail.md:148-150`). That is true
   for `ipc` and `loader`, but **not** for the shell's full surface:
   - **`urt::time::cntvct()`** (`urt/src/time.rs:378-384`) and `cntfrq()` (`:387-393`) are gated
     `#[cfg(all(target_arch = "aarch64", target_os = "none"))]` with **no host fallback** — they
     simply do not exist off-target. The shell calls `cntvct()` in `cmd_date` (`main.rs:306`).
   - **`urt::spawn`** is `#[cfg(all(target_arch = "aarch64", target_os = "none"))] pub mod spawn;`
     (`urt/src/lib.rs:69`), so `urt::spawn::{Exit, SpawnRec}` (imported at `main.rs:31`, used in
     `Spawner`/`print_exit`) is absent on the host.
   B15 must **not** modify `urt` (a verified-surface crate). So the entire syscall/spawn/clock
   runtime — not just the bare-metal items — has to be excluded from the host test build. (`ipc`
   and `loader` *do* build host-side via the `ipc::sys` stubs, confirming the obstruction is
   precisely `urt::time::cntvct` + `urt::spawn`.)

2. **`#[cfg(not(test))]` gating idiom + the row type.** `#![cfg_attr(not(test), no_std)]` is the
   house idiom (`kcore/src/lib.rs:22`, `urt/src/lib.rs:49`). The retention row type is
   `storage_server::SnapInfo { id, timestamp, provenance, message, class }` (`storage-server/src/lib.rs:249`),
   `pub` with public fields and `Clone`/`PartialEq`, so it is directly constructible in tests.

3. **The shell's `build.rs` applies the bare-metal linker script unconditionally.** It emitted
   `cargo:rustc-link-arg=-T…/link.ld` + `-zmax-page-size=4096` for *every* link, including a host
   test harness (and even a bin's own unit-test build, so `rustc-link-arg-bins` is **not** enough —
   it still covers `(bin "ushell" test)`). The host link then fails with
   `clang: unknown argument: -zmax-page-size`.

4. **proptest conventions.** proptest is the `"1"` dev-dependency house standard; the idiom is a
   `#[cfg(test)] mod tests` in `src/` with `cases: if cfg!(miri) { 4 } else { 256 }` and
   `prop_assert*` (`cas/src/overlay.rs:201`, B15A's `mkfs/src/tests.rs`). B15B follows it.

## What landed

**Runtime-module split (confirmed with the user), the realization of Design decision 2 at module
granularity.** One gate, clean `use` graph, no `allow(dead_code)`, and it makes Design decision 3's
"host-tested logic vs QEMU-gated I/O" split visible as code structure. This is *not* the rejected
bin+lib split (still one bin crate, no `[lib]` target, no manifest target change).

1. **`user/shell/src/main.rs`** — now the crate root: `#![cfg_attr(not(test), no_std)]` /
   `#![cfg_attr(not(test), no_main)]`, `extern crate alloc;`, the host-safe `use`s
   (`alloc::vec::Vec`, `storage_server::SnapInfo`), `#[cfg(not(test))] mod runtime;`, the **pure
   logic** (`pub(crate)`), and `#[cfg(test)] mod tests;`. The formatting *computation* is split
   from the *write*: pure `fmt_num`/`fmt_num_pad`/`fmt_hex`/`fmt_utc` write into an `&mut Vec<u8>`;
   `civil_from_days`/`parse_path`/`parse_u64`/`fault_class` move here unchanged; the retention
   *selection* is extracted into `prune_victims(rows: &[SnapInfo], keep_n) -> Vec<u64>`.
2. **`user/shell/src/runtime.rs` (new)** — every QEMU-gated item moved verbatim: the spawn-slot
   constants, `out`, the `out_num`/`out_hex`/`out_utc` wrappers (now `fmt_*` + `out`), `request`,
   `read_file`, `report`, all `cmd_*` (`cmd_prune` keeps the IPC loop over `prune_victims`'s
   result), `Spawner`+impl, `dispatch`, `_start`, `on_panic`, `#[global_allocator]`. Compiled only
   off-test (and on aarch64); `_start`'s `#[no_mangle]`/`#[link_section]` export the entry by
   symbol regardless of module. Behaviour byte-for-byte unchanged.
3. **`user/shell/build.rs`** — gate the bare-metal linker flags on
   `CARGO_CFG_TARGET_OS == "none"`, so they apply only to the Eunomia target. The aarch64 binary's
   link args are unchanged; the host test harness links with the platform default.
4. **`user/shell/Cargo.toml`** — `[dev-dependencies] proptest = "1"` (never enters the aarch64
   build).

**Tests — `#[cfg(test)] mod tests` (`user/shell/src/tests.rs`).** A `days_from_civil` reference
(the inverse Howard-Hinnant half, written independently) anchors the date round-trips so they are
real checks, not restatements; its golden day numbers are well-known UNIX epoch-day constants.

- **`civil_from_days`** — golden `0→(1970,1,1)`, `10957→(2000,1,1)`, `11016→(2000,2,29)` (leap),
  `24855→(2038,1,19)` (Y2038), `47482→(2100,1,1)`, `47541→(2100,3,1)` (century non-leap, no Feb 29);
  inverse round-trip over `0..=213_503` days (the full u64-ns range, year ≤ 2554).
- **`fmt_utc`** — golden ISO-8601 strings (epoch, sub-second precision, leap-day end-of-day with
  full 9-digit fraction, noon non-leap-century); property over arbitrary `u64` ns: always the
  fixed 30-byte `YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ` shape (separators at fixed indices, digits
  elsewhere) and parses back to the input ns.
- **`fmt_num`/`fmt_num_pad`/`fmt_hex`** — golden bytes (incl. `u64::MAX`, zero-pad, the
  `0xA300_0000` time VA).
- **`parse_path`** — goldens (`//a///b/→[a,b]`, `""`/`"/"→[]`); property: every component
  non-empty and `'/'`-free, join-then-reparse is a fixed point.
- **`parse_u64`** — goldens + rejects (empty, sign, spaces, trailing non-digit); the `u64::MAX`
  boundary; format→parse round-trip property; a non-digit-rejection property. The overflow
  non-guard is *documented*, not triggered (an over-`u64::MAX` digit string panics under debug
  overflow-checks — a forward note, this is typed shell input, not a wire decoder).
- **`fault_class`** — golden ESR_EL1 per EC/DFSC branch (translation/permission/access-flag/
  address-size/abort fallbacks; the `0x3C` mask ignoring fault level; the `exception` fallback).
- **`prune_victims`** — golden + property: count `= candidates.saturating_sub(keep_n)`, the
  selection is the oldest-first prefix of candidates, no `keep`-class (`class==0`) row is ever a
  victim, `keep_n ≥ candidates → []`.
- **`reference_has_teeth`** (negative control) — the days↔civil reference rejects a wrong date and
  a tampered expectation.

## Verification

| Check | Result |
|---|---|
| `cargo test --manifest-path user/shell/Cargo.toml` | **green** — 17 tests (10 golden/unit + 6 proptest + negative control) |
| `cargo fmt --manifest-path user/shell/Cargo.toml` | clean (mini-workspace; root `cargo fmt` skips it) |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --manifest-path user/shell/Cargo.toml` | **all 17 pass, no UB** (the whole pure tier is Miri-able; no FS/syscalls) |
| `cd kernel && cargo build` (aarch64) | links the full stack incl. `ushell` (only pre-existing kcore warnings) |
| `scripts/run-demo.sh` QEMU boot smoke (timeout harness) | green — store mounts + serves; `date`/`write`/`cat`/`snap`/`snaps`/`prune`/`df`/`ls` behave; no panic/`Corrupt` |

**End-to-end teeth check:** with a temporary off-by-one injected into `civil_from_days`
(`era * 400 + 1`), both `civil_from_days_golden` and `civil_from_days_round_trips` failed (the
property shrank to `days = 0`); reverted after confirming. So the oracle catches a real date-math
regression, not just a tampered expectation.

## Ledger / scope

- **No seam, no gate change.** The shell is rev1§6 Baseline tier, not a `rev1§6.1` proof-boundary
  seam; B15B adds no `external_body`/`assume_specification` (tally stays **14**), no Verus, no TLA,
  no Loom. The kcore/cas/ipc/freelist/dma-pool/urt Verus gates and the three TLA models are held by
  not touching them.
- **Not added to the standing CLAUDE.md Miri sweep.** The pure logic is `unsafe`-free; it is fully
  Miri-able and was Miri-replayed once here, but the standing sweep stays scoped to the
  `unsafe`-heavy crates (same posture as mkfs in B15A). The shell tests carry the `cfg!(miri)`
  case-count idiom for portability.
- **Host-tested vs QEMU-gated (Design decision 3).** *Host-tested:* the date math/formatting,
  parsers, fault classifier, retention policy. *QEMU-boot-gated (by design):* every `sys::*`
  interaction — the spawn/reap loop, `request`/IPC, the REPL, `_start`. The split is now visible in
  the source (`main.rs` pure logic vs `#[cfg(not(test))] mod runtime`).
- **Behaviour-preserving.** The `out_*` wrappers emit the identical bytes (a transient formatting
  `Vec` is the only difference, not observable); `cmd_prune` deletes the identical ids in the
  identical order; the aarch64 link args and the on-disk/wire formats are unchanged. The QEMU boot
  confirms the shipped tool is unchanged.

## Out of scope (recorded, not gaps)

- **B15C** (storaged/init/selftest startup-block parsing host tests) — separate and **untouched**;
  the rev1§2.7 decode-discipline floor, independently landable.
- **The shell's syscall I/O** (spawn/reap, `request`/IPC, the REPL, the MMIO-free `cmd_date` clock
  read) — rests on the QEMU boot smoke, the integration gate the parent plan keeps.
- **`loader::prepare` page-rounding** — Phase B3, not B15.
