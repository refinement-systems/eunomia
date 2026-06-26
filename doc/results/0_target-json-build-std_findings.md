# Findings 0 — Phase 0.1: `aarch64-unknown-eunomia` target + build-std wiring

Task 0.1 of `doc/plans/1_plan-rust-std-port.md`: create the custom bare-metal
target whose `os` is `eunomia`, point the userspace build at it, and prove the
existing **`no_std`** stack still boots in QEMU — *without* pulling in std yet.

## Outcome

Green. All six `user/*` binaries now build for `aarch64-unknown-eunomia` via
build-std (`core,compiler_builtins,alloc`), link at the rev2§5 EL0 base
`0x80000000`, and the full stack boots in QEMU printing over `debug-log` with the
real `svc #0` syscall path active. No std, no PAL, no verus change.

## Decisions taken (and alternatives rejected)

- **Target spec at `targets/aarch64-unknown-eunomia.json`** (not repo root).
  User-chosen this session; groups future per-binary specs in one place. build.rs
  passes it as an absolute path (`root.join("targets").join("…json")`), because a
  custom-JSON `--target` is resolved relative to the cargo invocation cwd
  (`user/<pkg>`) while cargo names the artifact dir by the file **stem**
  (`aarch64-unknown-eunomia`) — the two roles are split in `build_user()`.
- **Spec generated from `aarch64-unknown-none-softfloat`, one semantic edit:**
  add `"os": "eunomia"`. The built-in softfloat spec omits `os` because its value
  `"none"` equals rustc's default; adding the key is exactly what makes
  `target_os = "eunomia"`. Everything else inherited verbatim — `abi`/`rustc-abi`
  `softfloat`, `features: +v8a,+strict-align,-neon` (EL0 trap frames save no SIMD,
  same reason as the kernel), `panic-strategy: abort` (already abort),
  `llvm-target: aarch64-unknown-none` (LLVM needs no knowledge of `eunomia`; it
  emits a bare ELF), `data-layout`, `relocation-model: static`.
  Generating (vs. hand-copying) keeps `data-layout` matched to the toolchain.
- **`-Zbuild-std` stays `core,compiler_builtins,alloc` for 0.1.** User-chosen.
  The plan's deliverable text shows the *final* flag `…,std,panic_abort`, but
  0.1's own gate is only "core+alloc build; a no_std binary boots." std cannot
  compile for `os=eunomia` until 0.2 adds the `restricted_std` allowlist entry +
  copies the unsupported PAL — and `sys/alloc/mod.rs` / `sys/io/error/mod.rs` have
  no `_` arm, so `-Zbuild-std=std` would hard-fail the build today. The single
  edit point is parameterized so 0.2 just appends `,std,panic_abort`.
- **Widen the os cfgs (`none → any(none, eunomia)`), don't flip them.** The
  rename had to be additive: `os=none` must keep working for the kernel (which
  compiles `loader`) and for any manual `--target aarch64-unknown-none-softfloat`
  user build. Widening is purely additive and leaves the host/verus build
  byte-identical; flipping to `eunomia`-only would have been fragile and would
  have perturbed the kernel's `loader` build.
- **Kept the JSON + `-Zjson-target-spec` approach** rather than retreating to a
  built-in triple. A custom os is the whole point (std's `cfg_select!` PAL arms
  key on `target_os` in 0.2+). The Tier-3 upstreaming that would remove the JSON
  is explicitly deferred in the plan.

## Problems hit and how they were solved

1. **The os rename ripples far past "edit build.rs."** The project encodes
   "bare-metal Eunomia target" *everywhere* as `target_os = "none"`. Switching the
   user binaries to `os=eunomia` silently flips those false. Found and widened
   **7 source sites** to `any(target_os = "none", target_os = "eunomia")`:
   - `ipc/src/sys.rs` — the real `svc #0` `mod imp` and its negated host-stub
     `mod imp`. **Load-bearing**: left unwidened, `os=eunomia` selects the
     `unreachable!("Eunomia syscall on a non-Eunomia target")` stub and every
     syscall (`debug_write`, `chan_send`, …) traps — a binary that builds but does
     nothing.
   - `loader/src/lib.rs` — `pub mod spawn` (init/shell spawn programs through it;
     an unwidened gate makes `use loader::spawn` an unresolved import).
   - `urt/src/lib.rs` — `pub mod spawn`; `urt/src/time.rs` — `cntvct()`,
     `cntfrq()`, `now_utc_ns()` (the `Instant`/`SystemTime` register path).
2. **Per-crate linker-script gates were inconsistent and `-none`-keyed.** Each
   `user/*/build.rs` applies its `link.ld` (entry `0x80000000`) + `-zmax-page-size`
   only for the bare-metal target, but via three different shapes:
   - `target.contains("-none")` — `init`, `storaged`, `console`, `selftest`
     → widened to `… || target.contains("eunomia")`.
   - `CARGO_CFG_TARGET_OS != "none"` early-return — `shell` → accept
     `"none"` or `"eunomia"`.
   - **unconditional** — `hello` (applies the script for *any* target). Left as-is
     (it already covers eunomia; hello has no host tests so the over-broad gate is
     harmless), but the inconsistency is noted as a follow-up.
   Verified the fix worked by reading the ELF entry: `init`/`ushell`/`hello` all
   report entry `0x80000000` (linker script applied; the hazard cleared).
3. **Nightly now gates `.json` targets behind `-Zjson-target-spec`.** First kernel
   build failed: ``.json` target specs require -Zjson-target-spec to be added to
   the cargo invocation`. Added `.arg("-Zjson-target-spec")` to `build_user()`
   (we are already on `-Z` unstable via build-std).
4. **`check-cfg` warnings for the unknown `eunomia` os value.** On host/none
   builds, rustc's check-cfg doesn't know `target_os = "eunomia"`, emitting
   `unexpected_cfgs` warnings from `ipc`/`loader`/`urt` (and the kernel's `loader`
   compile). All three already carry a `[lints.rust] unexpected_cfgs` check-cfg
   list (for loom/shuttle/verus cfgs); appended `'cfg(target_os, values("eunomia"))'`
   to each. Confirmed warnings cleared on rebuild.

## Verification record (the 0.1 gate)

Toolchain: `rustc 1.98.0-nightly (beae78130 2026-06-09)`,
`cargo 1.98.0-nightly (0b1123a48 2026-06-01)`, default `nightly-aarch64-apple-darwin`
(no `rust-toolchain.toml` pin in-tree).

- **Host build** `cargo build -p ipc -p loader -p urt` → exit 0, **0** `eunomia`
  check-cfg warnings after the lint extension.
- **Host tests** `cargo test -p ipc -p loader -p urt` → all green
  (`33`, `12`, `3`, `3`, `2`, `22` passed across the suites; `0 failed`).
- **core+alloc build for the target** `cd kernel && cargo build` →
  `Finished … in 25.56s`, exit 0. Artifacts present under
  `target/user/aarch64-unknown-eunomia/release/`: `hello selftest init storaged
  ushell console`, all `ELF 64-bit LSB executable, ARM aarch64`. ELF entry
  `0x80000000` confirmed (`llvm-readelf -h`).
- **QEMU boot, prints via debug-log** `scripts/run-demo.sh` under the CLAUDE.md
  group-kill Perl harness (120 s deadline; `timeout` is unavailable on this host).
  Log showed, in order: `boot: init ELF loaded, entry 0x80000000` →
  `[init] wiring the system` → `[init] system up` → `[console] serving` →
  `[storaged] virtio-blk up` → `[storaged] store mounted` → `[storaged] serving` →
  `eunomia>` prompt; piped commands all worked (`write docs/smoke hello`→`ok`,
  `sync`→`ok`, `cat docs/smoke`→`hello`, `ls docs`, `df`); no panic/`Corrupt`/
  `unwrap`; QEMU killed cleanly (`terminating on signal 15`).
- **Formatting** `cargo fmt` (root members) + `cargo fmt --manifest-path
  user/<c>/Cargo.toml` for the five edited user crates; `cargo fmt -- --check`
  clean for all. `cargo fmt` reflowed the widened `#[cfg(all(…))]` attributes onto
  three lines each (they exceeded the width limit) — semantics unchanged; no
  unrelated reformatting entered the diff.

## Verus / trusted-base posture

Phase 0.1 touched **no `verus!{}` code** and made **no ledger change**. The
widened cfg `any(target_os = "none", target_os = "eunomia")` evaluates **false**
on the host target the verus gate runs on (`target_os = "macos"`), so the
host/verus build is byte-unchanged — no proof re-run required (as the approved
plan stated). 0.1 is not a PAL-touching task: it adds no delegation shell, so the
per-task thinness / inverse-leak check does not apply here.

## Surface left trusted / not-yet-built (and why)

- **std is not built** for `os=eunomia` (deferred to 0.2 by decision above).
- The new target spec is **plain JSON config**, not verifiable code; its
  `data-layout` is trusted to match the toolchain (re-generate on a toolchain
  bump). This is standard custom-target maintenance, not a new trusted seam.

## Follow-ups

- **0.2** flips the single build-std edit point to `…,std,panic_abort`, adds
  `|| target_os == "eunomia"` to std's `restricted_std` allowlist, and copies
  `sys/pal/unsupported` → `sys/pal/eunomia`.
- **Toolchain pin:** no `rust-toolchain.toml` today — build-std relies on the
  rustup default being nightly, and the JSON `data-layout` is coupled to it. 0.2
  pins the std-build toolchain; flag both then.
- **os-cfg sprawl:** "bare-metal" is now duplicated as `any(none, eunomia)` across
  7 source sites + 5 build scripts. A single build.rs-emitted `#[cfg(bare_metal)]`
  alias would centralize it — deferred (would add build.rs to ipc/urt).
- **`hello/build.rs`** applies its linker script unconditionally, unlike the other
  five gated build scripts. Harmless today; align for consistency if hello ever
  gains host tests.
- **`-Zjson-target-spec` / JSON upkeep** disappears if/when
  `aarch64-unknown-eunomia` is upstreamed to `rustc_target` (Tier-3, deferred in
  the plan).
- A pre-existing build-std `future-incompat` warning about the vendored `core`
  source (rustup's copy) appears during the user builds — unrelated to 0.1.
