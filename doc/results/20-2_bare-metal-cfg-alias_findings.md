# Findings 20-2 — the `bare_metal` cfg alias (the last Findings #20 follow-up)

Task: the third and final follow-up left by **std-port 6.2**
(`doc/results/20_fuzz-pal-audit_findings.md`, "Follow-ups"). Status of the three:
(1) the cas verified-codec corpora enrichment — **done** in PR #289; (2) the
cross-bridge lockstep guard — **done** in 20-1 / PR #290; (3) the **os-cfg sprawl →
`bare_metal` cfg alias** — this task, previously deferred as cosmetic/high-blast-radius
and now completed at the user's request.

**Headline:** the `all(target_arch = "aarch64", any(target_os = "none", target_os =
"eunomia"))` condition that repeated across **44 source `#[cfg]` sites** in four
*verified* crates (`eunomia-sys`, `urt`, `loader`, `ipc`) now has a single source: a
`bare_metal` cfg emitted by a per-crate `build.rs`. Every site collapses to `bare_metal`
/ `not(bare_metal)` (the `target_arch`/`target_os` scaffolding folded into the alias),
leaving only the surviving `not(test)`/`not(loom)`/`not(shuttle)`/`test` predicates. The
`user/*` build scripts (11, in three near-duplicate shapes) are deduplicated onto one
shared `user/build_common.rs`. No verified count moved (eunomia-sys 16, ipc 71, loader
30, urt 29); **ledger tally stays 14**.

## The decision — arch-folded alias, `include!` helper (chosen with the user)

- **Semantics: fold in aarch64.** `build.rs` emits `bare_metal` when
  `CARGO_CFG_TARGET_OS ∈ {none, eunomia}` **and** `CARGO_CFG_TARGET_ARCH == "aarch64"`.
  This is safe because the only bare-metal target is aarch64: the ~15 previously
  arch-free `any(target_os …)` sites in `eunomia-sys` gain an implicit, always-true
  aarch64 requirement (unchanged on host *and* target), while the 24 arch-coupled sites
  shed their `all(target_arch …)` wrapper entirely. *Rejected:* a pure-OS alias that
  keeps the `all(target_arch = "aarch64", bare_metal)` wrappers — faithful but leaves the
  noise the follow-up set out to remove.
- **Scope: verified crates + user build scripts.** Both halves the #20 census named.
- **`rustc-check-cfg` from `build.rs`.** Each `build.rs` unconditionally emits
  `cargo:rustc-check-cfg=cfg(bare_metal)`, co-locating the declaration with the emission,
  so the crates' `[lints.rust] unexpected_cfgs` allowlists need no `bare_metal` entry.
- **User dedup via `include!("../build_common.rs")`, not a shared crate.** The `user/*`
  are separate mini-workspaces; a loose helper file `include!`d by each build script
  avoids a new crate and cross-workspace build-dependency edges. Verified (empirically)
  that cargo recompiles+reruns a build script when its `include!`d file changes, and each
  script also emits `cargo:rerun-if-changed=../build_common.rs` for the explicit trigger.
  *Rejected:* a `build-dependencies` crate (heavier: a new crate + a manifest edit on all
  11 users).

## What shipped

- **`bare_metal` alias in the 4 verified crates.** New `build.rs` in `eunomia-sys`,
  `urt`, `loader`, `ipc` (none had one). The 44 source sites rewritten: bare/arch-coupled
  positives → `#[cfg(bare_metal)]`; negations → `#[cfg(not(bare_metal))]`; inner module
  attrs → `#![cfg(bare_metal)]`; the model-gated forms keep their other predicates
  (`all(not(test), bare_metal)`, `any(test, bare_metal)`, `all(not(loom), not(shuttle),
  bare_metal)`). Files: `eunomia-sys/src/{lib,bootstrap,stdio,console,syscall,random,
  thread,pal,fs,futex,tls}.rs`, `urt/src/{lib,time,futex,lock}.rs`, `loader/src/lib.rs`,
  `ipc/src/sys.rs`. Post-change grep confirms zero `target_os`/`target_arch` cfg clauses
  remain in the four `src/` trees.
- **The one manifest site that stays `target_os` (by necessity).**
  `eunomia-sys/Cargo.toml`'s `[target.'cfg(any(target_os = "eunomia", target_os =
  "none"))'.dependencies]` (pulling `urt`) is unchanged: Cargo evaluates `[target.'cfg']`
  against the real target spec, **not** build-script-emitted cfgs, so `bare_metal` can
  never match there. Its `[lints.rust]` comment was rewritten to say so, and its
  `cfg(target_os, values("eunomia"))` check-cfg value kept (the crate still names the
  custom OS in that manifest edge).
- **Dead check-cfg pruned from the other three.** `urt`, `loader`, `ipc` no longer
  reference `target_os` anywhere (source or manifest), so their now-dead
  `cfg(target_os, values("eunomia"))` check-cfg entries were removed and the comments
  rewritten to describe `cfg(bare_metal)` (build.rs) instead.
- **`user/*` build-script dedup.** New `user/build_common.rs` exposes `is_bare_metal()`,
  `link_el0_image_bins()`/`link_el0_image()`, and `rerun_inputs()` (each
  `#[allow(dead_code)]` since a given script uses a subset). All 11 `user/*/build.rs`
  now `include!` it and compose the helpers to reproduce their prior behavior exactly:
  the four bin-with-host-harness crates (init/selftest/storaged/console) gate + bin-scope;
  `shell` keeps its `libtests` block and gates plain args; the six no-host-path crates
  (stdfs/coretests/stdio/stdsmoke/hello/alloctests) link unconditionally. Category A's
  old `TARGET.contains(...)` substring test unifies onto `shell`'s more precise
  `CARGO_CFG_TARGET_OS` check (equivalent for the real triples).

## Verification record

| Gate | Command | Result |
|---|---|---|
| verus pin | `verus --version` | `0.2026.06.07.cd03505`, Toolchain 1.95.0 ✓ |
| verus — eunomia-sys (cold) | `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` | **16 verified, 0 errors** (unchanged) |
| verus — ipc (cold) | `cargo clean -p ipc && cargo verus verify -p ipc` | **71 verified, 0 errors** (unchanged) |
| verus — loader (cold) | `cargo clean -p loader && cargo verus verify -p loader --no-default-features` | **30 verified, 0 errors** (unchanged) |
| verus — urt (cold) | `cargo clean -p urt && cargo verus verify -p urt` | **29 verified, 0 errors** (unchanged; `freelist` 30, untouched, re-verified alongside) |
| host tests | `cargo test -p eunomia-sys -p ipc -p loader -p urt` | **all pass** |
| user host tests | `cargo test --manifest-path user/{storaged,shell}/Cargo.toml` | **storaged 6, shell 28 pass** (host link unaffected by the build-script dedup) |
| target build | `cd kernel && cargo build` | **builds** (~19 s; the 4 crates + all `user/*` binaries via kernel/build.rs, bare_metal ON) |
| QEMU boot smoke | `scripts/run-demo.sh` (perl process-group harness) | `[storaged] store mounted` → `serving`; `write`/`sync`/`cat`/`ls`/`df` echo; no panic |
| std smoke | `scripts/std-smoke-test.sh` | **STD SMOKE TEST PASS** (STD2/32/33/34/35/52/51/53 all green) |
| formatting | `cargo fmt --check` + `scripts/verusfmt.sh --check` + per-`user/*` `cargo fmt --check` + `rustfmt --check user/build_common.rs` | **clean** |

The change is Verus-invisible: a host verify build excludes `bare_metal`-gated code
exactly as it excluded `target_os`-gated code, so the four crates' obligations are
byte-identical and their counts unchanged — the proof that no obligation moved (no
proof-perf run needed, since no `verus!{}` interior changed).

## Surface left trusted / follow-ups

- **`eunomia-sys/Cargo.toml`'s `[target.'cfg(target_os …)']` dependency edge stays**
  on `target_os` — a manifest cfg a build-script cfg cannot express (documented inline).
  This is the sole remaining `target_os` reference in the four crates and is intrinsic to
  how Cargo resolves target dependencies, not sprawl.
- With this, **all three Findings #20 follow-ups are closed.**

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

No new seam, no Baseline count moves: the change is plain-Rust `#[cfg]` attributes +
build scripts, entirely outside every `verus!{}` block, adding no verification
obligation. **Tally stays 14.**

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
