# Findings 1 — Phase 0.2: std knows the `aarch64-unknown-eunomia` target

Task 0.2 of `doc/plans/1_plan-rust-std-port.md`: make the Rust **standard
library** compile for `os = eunomia`, all-unsupported, so a `fn main(){}` std
binary links — pin the std-build toolchain to the vendored fork, add the
`restricted_std` allowlist entry, and copy the `unsupported` PAL to a new
`eunomia` PAL.

## Outcome

Green. `cd kernel && cargo build` compiles std (all-unsupported) for
`aarch64-unknown-eunomia` from the vendored fork and still builds all six `no_std`
`user/*` binaries + the kernel; a throwaway std `fn main(){}` (driving `Vec` +
`String`) links to an aarch64 ELF; the existing stack boots green under QEMU; the
verus gate and host tests are unchanged. No `verus!{}` code touched, no ledger
change.

## Decisions taken (and alternatives rejected)

- **Toolchain pin = submodule bumped to `bd08c9e7` + `nightly-2026-06-26` (an exact
  compiler↔source match).** The submodule was at `eb6346c` (2026-06-26), but the
  matching upstream nightly (`nightly-2026-06-27`) was not yet published (UTC was
  still 2026-06-26 ~22:15 when this ran; nightlies cut at 00:00 UTC and publish
  hours later). The newest available nightly, `nightly-2026-06-26`, is **exactly**
  rustc commit `bd08c9e71874a81670fe3938dbf85148e42c2b96`, a clean upstream
  ancestor of `eb6346c` (157 commits earlier). User-confirmed choice. The fork
  branch was rebased to start at `bd08c9e7`, giving zero compiler/source skew.
  - *Rejected — keep `eb6346c`, wait for `nightly-2026-06-27`:* blocks the task for
    hours, and even then the nightly would be `eb6346c` + later commits (compiler ≥
    source, not an exact commit match).
  - *Rejected — keep `eb6346c` + `nightly-2026-06-26`:* the compiler would be 157
    commits (incl. an LLVM 23 bump + intrinsic changes) *older* than the std source
    it builds — philosophically backwards and a real feature-mismatch risk (this is
    exactly the `diagnostic_on_unmatched_args` failure seen below when the wrong,
    older toolchain was used).

- **`__CARGO_TESTS_ONLY_SRC_ROOT` points at `vendor/rust/library`, not the repo
  root.** This is the cargo override that redirects build-std off rustup's
  `rust-src` to our edited source. Empirically, cargo reads
  `<__CARGO_TESTS_ONLY_SRC_ROOT>/Cargo.toml` **directly** (no `/library` append).
  Pointing it at the monorepo root made cargo pick up `vendor/rust/Cargo.toml` —
  the *whole-compiler* workspace (miri, rustc, …) — and fail with `cannot specify
  features for packages outside of workspace / a workspace member with a similar
  name exists: miri`. `vendor/rust/library/Cargo.toml` is the self-contained std
  workspace: the `rustc-std-workspace-{core,alloc,std}` shims live under `library/`
  and its `[patch.crates-io]` is library-relative, so `compiler-builtins`,
  `backtrace`, `stdarch`, `portable-simd` all resolve. (rustup's packaged
  `rust-src` differs from the full monorepo only in that its root holds just
  `{library, src}` and *no* root `Cargo.toml`.)

- **The toolchain pin is kernel-scoped (`kernel/rust-toolchain.toml`), never
  root.** A root pin would drag the `verus` gate (needs Rust 1.95.0) and the host
  test suite (stable) off their own toolchains. Verified isolation: from the repo
  root the active toolchain is still the default nightly; only under `kernel/` is
  it `nightly-2026-06-26`. The `user/*` sub-builds get the pin for free — they run
  with a cwd outside `kernel/` (so they don't discover the file), but
  `kernel/build.rs` spawns the *active toolchain's* `cargo` (the `CARGO` env) and
  inherits its `RUSTC`/`RUSTUP_TOOLCHAIN`, none scrubbed, so they use the same
  toolchain regardless of cwd.

- **Only the `user/*` builds are redirected to `vendor/rust`; the kernel's own
  build-std stays on rustup `rust-src`.** The kernel compiles only `core` +
  `compiler_builtins` (unedited), and rustup's `rust-src` for `nightly-2026-06-26`
  *is* `bd08c9e7` — byte-identical to our submodule base — so redirecting it would
  buy nothing while adding coupling. (The plan flagged redirecting the kernel as
  "consistency, not necessity"; chose not to.) `kernel/.cargo/config.toml` is
  therefore unchanged.

- **The `eunomia` PAL is a verbatim copy of `unsupported`** (`sys/pal/eunomia/`
  `mod.rs` + `common.rs`), so the thinness audit is trivial. `abort_internal` stays
  `core::intrinsics::abort()`; Phase 2.3 overrides `abort_internal`/`exit` to
  `thread_exit(STATUS_PANIC)`/`thread_exit(code)`.

- **Per-module arms — minimum to link all-unsupported, reusing existing impls:**
  - `sys/alloc/mod.rs` (no `_` fallback) → new `sys/alloc/eunomia.rs`: a `System`
    `GlobalAlloc` whose `alloc`/`realloc`/`alloc_zeroed` return null and `dealloc`
    is a no-op (the honest "no heap yet"; `urt::Heap` backing is Phase 2.2).
  - `sys/io/error/mod.rs` (no `_` fallback) → reuse the existing `generic` module
    (added `eunomia` to its `any(...)` arm — no new file).
  - `sys/random/mod.rs` (empty `_ => {}`) → reuse `unsupported` for both
    `fill_bytes` and `hashmap_random_keys`, and add `eunomia` to the generic
    `hashmap_random_keys` `#[cfg(not(any(...)))]` exclusion (else it would be
    defined twice). Required at all, not just "for HashMap": std's generic
    `hashmap_random_keys` is cfg-included for eunomia and references `fill_bytes`,
    so std won't even compile without an arm. Real DRBG is Phase 3.4.
  - `sys/thread_local/mod.rs` → the **`no_threads`** top-level arm + the matching
    no-op `guard::enable` arm (same set uefi/zkvm/trusty/vexos use). This is the
    plan's single-threaded "global-statics fallback" interim; it sidesteps the
    OS-key TLS path whose `key` dispatcher has only an empty `_ => {}` arm. Real
    TLS (TPIDR + `urt::slots` key table) is Phases 3.1/3.5.
  - Everything else (`args`, `env`, `stdio`, `time`, `os_str`, `paths`, `exit`,
    `pipe`, `fd`, `personality`, `net`, `sync/*`) has a `_ =>` fallback and routes
    to `unsupported` automatically.

## Problems hit and how they were solved

1. **`nightly-2026-06-27` did not exist yet** → bumped the submodule to the
   exactly-matching `bd08c9e7`/`nightly-2026-06-26` (decision above).
2. **`cannot specify features … / miri`** → `__CARGO_TESTS_ONLY_SRC_ROOT` was
   pointing at the monorepo root; pointed it at `library/` (decision above).
3. **`unknown feature 'diagnostic_on_unmatched_args'`** — a red herring: a *manual*
   repro of the user build ran plain `cargo` in `user/hello`, which used the
   machine's **default** nightly (`beae78130`, 2026-06-09), older than the
   `bd08c9e7` source. Re-running with the pin cleared it. Confirms the
   exact-match pin is load-bearing.
4. **`unresolved imports crate::sys::thread_local::key::{LazyKey, set}`** — eunomia
   fell through to the OS-key TLS impl whose `key` arm is the empty `_ => {}`. Fixed
   by selecting `no_threads` + the no-op guard (decision above).
5. **`library/backtrace` (a nested submodule) was uninitialized** — std includes it
   unconditionally via `library/std/src/lib.rs` `#[path = "../../backtrace/src/lib.rs"]`,
   so build-std fails to find the file. Initialized it locally; CI inits it
   targeted (see below). `compiler-builtins`, `stdarch`, `portable-simd` are
   in-tree and were already populated.

## Verification record (the 0.2 gate)

Toolchain: `nightly-2026-06-26` == rustc commit `bd08c9e7…` (verified
`rustc +nightly-2026-06-26 --version --verbose`), matching the `vendor/rust`
submodule commit exactly. Verus binary `0.2026.06.07.cd03505` (pin unchanged).

- **std compiles all-unsupported** — `cd kernel && cargo build` → exit 0. Log shows
  `std v0.0.0 (…/vendor/rust/library/std)` compiled; all six `user/*` ELFs + the
  kernel built. (Pre-existing `future-incompat` warning about the `core` source,
  unrelated to 0.2 — same one noted in Findings 0.)
- **`fn main(){}` links** — a throwaway std bin (`Vec<String>` + `String`) built
  with the build.rs flags + `__CARGO_TESTS_ONLY_SRC_ROOT=…/vendor/rust/library` +
  `RUSTUP_TOOLCHAIN=nightly-2026-06-26` → exit 0, a 5520-byte
  `ELF 64-bit LSB executable, ARM aarch64`. The only linker note is
  `rust-lld: cannot find entry symbol _start; not setting start address` —
  **expected**: the non-crt0 `_start` shim is Phase 2.1's deliverable; the binary
  still links (all std symbols resolved). Not committed (no boot canary belongs in
  0.2).
- **No regression of the `no_std` stack** — `scripts/run-demo.sh` under the
  `CLAUDE.md` group-kill Perl harness (150 s): `[init] system up` →
  `[console] serving` → `[storaged] store mounted` → `[storaged] serving` →
  `eunomia>`; `write docs/smoke hello` → `ok`, `cat docs/smoke` → `hello`; no
  panic/`Corrupt`/`unwrap`; QEMU killed cleanly (`terminating on signal 15`; the
  harness deadline exit 124 is expected).
- **Verus untouched** — `cargo clean -p kcore && cargo verus verify -p kcore` →
  `verification results:: 406 verified, 0 errors` (unchanged; the kernel-scoped pin
  does not leak into the root-run gate).
- **Host tests** — `cargo test -p loader -p urt -p ipc` → all pass
  (33/12/3/3/2/22; 0 failed), same counts as Findings 0.
- **Toolchain isolation** — `rustup show active-toolchain`: repo root =
  `nightly-aarch64-apple-darwin (default)`; `kernel/` = `nightly-2026-06-26
  (overridden by kernel/rust-toolchain.toml)`.
- **Formatting** — `cargo fmt -p kernel` then `cargo fmt -p kernel -- --check`
  clean (rustfmt reflowed the new `.env(…)` onto multiple lines). The `vendor/rust`
  edits keep upstream formatting (the fork never runs our fmt/verus gates).

## Files changed

- **`vendor/rust`** (fork branch off `bd08c9e7`): `library/std/build.rs`
  (`restricted_std` allowlist += eunomia); `library/std/src/sys/pal/eunomia/`
  (new, copy of `unsupported`); `library/std/src/sys/pal/mod.rs` (eunomia arm);
  `library/std/src/sys/alloc/mod.rs` + new `alloc/eunomia.rs`;
  `library/std/src/sys/io/error/mod.rs` (eunomia → `generic`);
  `library/std/src/sys/random/mod.rs` (eunomia → `unsupported` + exclusion);
  `library/std/src/sys/thread_local/mod.rs` (eunomia → `no_threads` + no-op guard).
- **eunomiaos**: `kernel/build.rs` (build-std += `std,panic_abort`;
  `__CARGO_TESTS_ONLY_SRC_ROOT` redirect; `rerun-if-changed` += vendored std);
  `kernel/rust-toolchain.toml` (new pin); `.github/workflows/ci.yml` (`on-os`:
  targeted vendor/rust + backtrace checkout, pinned nightly); the `vendor/rust`
  submodule pointer; this findings doc.

## CI

The `on-os` job checks out only what build-std needs — `vendor/rust`
**non-recursively** (skipping its huge `llvm-project`/`cargo`/`miri` submodules)
plus the `library/backtrace` nested submodule — and pins
`dtolnay/rust-toolchain@master` to `nightly-2026-06-26` + `rust-src`. No CI env is
needed for the source redirect: it rides `kernel/build.rs`. Other jobs
(`host-tests`, `verus`, `model*`, `concurrency`, `layering`) are untouched — they
build host crates from the root, use neither build-std nor `vendor/rust`, and don't
discover `kernel/rust-toolchain.toml`.

## Verus / trusted-base posture

0.2 added **no `verus!{}` code** and makes **no ledger change**. The `vendor/rust`
PAL is the trusted, term-for-term shell described by the plan's resolving principle:
a submodule fork that by construction never runs `cargo verus verify`, the same
posture `kernel/` holds over `kcore`. Per-task thinness/inverse-leak check: the
`eunomia` PAL arms hold **zero new logic** — a verbatim copy of `unsupported`, reuse
of the existing `generic`/`unsupported`/`no_threads` impls, and a null `alloc`
stub — all auditable by inspection against `pal/unsupported`.

## Surface left unsupported / trusted (and why)

- **All PAL surfaces are unsupported stubs** (alloc → null, random → panic/addr,
  io/error → generic, thread_local → no_threads, everything else `_` →
  `unsupported`). This is the deliberate 0.2 posture; real impls arrive in
  Phases 2–5 and are verified-on-arrival per the plan.
- **No process entry (`_start`)** yet — Phase 2.1. The link gate tolerates the
  `cannot find entry symbol _start` note.
- **The target JSON / `__CARGO_TESTS_ONLY_SRC_ROOT` redirect** are plain build
  config, not verifiable code; they are standard custom-target maintenance (the
  redirect is a test-only cargo var — see follow-ups), not a new trusted seam.

## Follow-ups

- **Forward-port the pin pair.** When `nightly-2026-06-27`+ publishes and the team
  wants `vendor/rust` at `eb6346c` (or later), re-establish the exact
  `(submodule commit ↔ nightly)` match and bump both `vendor/rust` and
  `kernel/rust-toolchain.toml` together. The determination procedure (install the
  candidate nightly, match `rustc --version --verbose` `commit-hash`) is the
  runbook (Phase 6.3).
- **`__CARGO_TESTS_ONLY_SRC_ROOT` is an undocumented, test-only cargo variable.**
  If cargo changes its semantics, the redirect needs revisiting; Tier-3
  upstreaming of `aarch64-unknown-eunomia` (deferred in the plan) removes both it
  and the JSON.
- Phase 1+ replaces the stubs: `_start`/argv/env (2.1), real alloc (2.2), stdio +
  exit terminus (2.3), time (2.4), TLS/threads/locks (3.x), fs (4.x).
- The os-cfg sprawl noted in Findings 0 (`any(none, eunomia)` across ~7 sites) is
  still open.
