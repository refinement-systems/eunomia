# Findings — Phase 2.1: std entry + argv/env + io-error map

Task 2.1 of `doc/plans/1_plan-rust-std-port.md` — the first std PAL surface that
*does something*. A non-crt0 `_start` receives the slot-0 bootstrap block (rev2§5.1),
runs it through the **verified** `loader::startup::decode`, stashes argv/env, then
calls the compiler-generated `main`; the `sys/args`, `sys/env`, and `sys/io/error`
PAL arms resolve `std::env::args`/`vars`/`io::Error` to real eunomia data. No new
`verus!{}` obligation and no new trusted seam — the tally stays **14**.

## What shipped

- **`eunomia-sys`** (the seam crate), three new plain-Rust modules (no `verus!{}`):
  - `src/bootstrap.rs` — `init()` (recv on `grant::BOOTSTRAP_CHANNEL`, verified-decode,
    stash a `Startup<'static>` borrowing a `'static` buffer) + `startup()`/`argv()`/
    `env()` accessors. The `user/hello`/`user/storaged` `recv_blocking`-then-`decode`
    pattern, lifted into the seam. Host test drives the decode/stash/accessor path over
    a real EUS1 block (the `chan_recv` `svc` is the trusted shell, exercised in QEMU).
  - `src/io_error.rs` — `classify(i64) -> Kind` (`#[repr(u8)]`) + `message(i64) ->
    &'static str`, the total map from the 12 ABI `ERR_*` codes. **Proptested**:
    totality ∀ `i64`, the exact ABI table, non-ABI ⇒ `Uncategorized`.
  - `src/pal.rs` — the `#[no_mangle] extern "Rust"` shims std links against (gated to
    `target_os = eunomia/none`), each a one-line delegation: `__eunomia_bootstrap_init`,
    `__eunomia_argv`, `__eunomia_env`, `__eunomia_thread_exit`, `__eunomia_io_classify`,
    `__eunomia_io_message`.
- **`vendor/rust`** std PAL (the trusted term-for-term shell):
  - `sys/pal/eunomia/mod.rs` — the non-crt0 `_start`: `__eunomia_bootstrap_init()` →
    `main(0, null, 0)` → `__eunomia_thread_exit(code)` (the `motor_start` template).
  - `sys/args/eunomia.rs` + dispatch — `args()` builds `OsString` from the stashed argv
    byte-strings via the internal `FromInner`/`Buf` path (eunomia os_str is bytes).
  - `sys/env/eunomia.rs` + dispatch — `env()`/`getenv()` split `KEY=VALUE` on the first
    `=`; `setenv`/`unsetenv` unsupported (no producer until 5.2).
  - `sys/io/error/eunomia.rs` + dispatch — moved eunomia out of the `generic` group;
    `decode_error_kind`/`error_string` translate the seam's `Kind`/message;
    `errno()=0`, `is_interrupted=false` (microkernel, no ambient errno, no signals).
- **Ledger** (`doc/guidelines/verus_trusted-base.md`): an addendum to the eunomia-sys
  routing note covering `bootstrap`/`io_error`/`pal` as host-tested/trusted-shell
  surface; tally stays 14.

## Decisions (and rejected alternatives)

- **std reaches the seam via an `extern "Rust"` bridge, not a sysroot dependency.**
  This is the plan's documented fallback, taken because the **primary (moto-rt-style
  sysroot path-dep) is genuinely blocked**: `eunomia-sys`'s verified deps pull `vstd`,
  whose `verus_builtin` (the `builtin` lib) **cannot be built as a `rustc-dep-of-std`
  sysroot crate** — `cargo build -Zbuild-std` fails with `error[E0463]: can't find
  crate for core` on `verus_builtin` once it enters std's sysroot graph (it has no
  `rustc-std-workspace-core` plumbing and is an external pinned crate we cannot patch).
  `vstd` is a hard compile-time dep of `loader`/`ipc`/`le-bytes` (it provides the
  `verus!{}` macro), so it cannot be dropped, and `bootstrap` must call the verified
  `loader::startup::decode` — so std cannot depend on the chain. The bridge (the
  `__rust_alloc` pattern) keeps `eunomia-sys` + its verus tree a **normal** dependency
  of the std binary (where it builds fine, exactly as it does for the no_std user
  binaries today); std only declares undefined `extern "Rust"` symbols.
  - *Cost:* a std binary must link `eunomia-sys` and force the link with
    `extern crate eunomia_sys;` (the global-allocator ergonomics). Documented for 5.3.
  - *Why not duplicate the svc asm + decode into the PAL instead:* the decode must be
    the **verified** one; duplicating it into the trusted shell is exactly what the
    thinness rule forbids. The bridge keeps a single verified source of truth.
- **`OsString` from raw bytes via the internal `FromInner`/`Buf` path** —
  `OsString::from_inner(Buf::from_inner(bytes.to_vec()))` (`crate::sys::os_str::Buf`,
  `crate::sys::FromInner`). eunomia's os_str is the **bytes** encoding (the `_` arm in
  `sys/os_str/mod.rs`), so this is lossless and needs **no `os/eunomia/ffi.rs`** (the
  plan's "OsStr is bytes" decision). Rejected: `OsString::from(String)` (motor's path)
  — it would lossily round-trip non-UTF-8 argv through `str`.
- **The io-error policy lives host-tested in `eunomia-sys`; the PAL is a thin
  translator.** `classify` returns a `#[repr(u8)] Kind` discriminant across the bridge;
  the PAL maps `u8 → io::ErrorKind` (a 6-arm match kept in lockstep with the enum's
  fixed discriminants). Keeps the tested policy the single source of truth; the magic
  numbers are localized and commented. The fs `ErrorCode` set extends `classify` in 4.3.
- **Scope: the live `env::args()` QEMU demo is deferred to the Phase-2 GATE.** Building
  `OsString`s allocates, and the allocator is still the 0.2 null stub until 2.2 (user
  confirmed clean 2.1/2.2 separation). 2.1's achievable gate is **links + host proptest
  + no-regression boot**; the live argv print rides on 2.2 (alloc) + 2.3 (stdio).
- **Whole `Startup` stashed, not just argv/env**, so 2.3 (stdout slot), 2.4 (time va),
  4.x (root handle) read grants from the same `bootstrap::startup()` via `grant::*`.

## Problems hit and how they were solved

- **`verus_builtin` is not sysroot-buildable** (above) — discovered by building a
  throwaway std canary with the sysroot path-dep; pivoted to the bridge, which built
  clean on the first try. The verus tree compiles for the target as a normal dep (it
  already does, for the no_std binaries), just not as a *sysroot* dep.
- **No duplicate-`_start` clash.** The std PAL now `#[no_mangle]`-defines `_start`; the
  concern was the no_std `user/*` binaries (which also define `_start`). Confirmed clean:
  `#![no_std]` binaries don't link std, so std's `_start`/`__eunomia_*` symbols are
  never pulled. `cd kernel && cargo build` builds all six no_std binaries + the kernel.

## Verification record

Toolchain `nightly-2026-06-26` (== `vendor/rust` `bd08c9e7…`); Verus binary
`0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Links** — a throwaway std `fn main()` (referencing `env::args`/`var`/`vars` +
  `process::exit`) built for `aarch64-unknown-eunomia` via the `kernel/build.rs` flags
  (`-Zbuild-std=…,std,panic_abort`, `__CARGO_TESTS_ONLY_SRC_ROOT=vendor/rust/library`)
  → clean ELF, **no `cannot find entry symbol _start` warning**; `e_entry = 0x80000000`
  = `_start`; `_start`, `__eunomia_argv`/`_bootstrap_init`/`_env`/`_thread_exit` all
  defined (`T`) by linking `eunomia-sys`. (The canary is throwaway — removed; no
  permanent std user binary until 5.3.)
- **Host tests** — `cargo test -p eunomia-sys` → **13 passed, 0 failed** (the original
  8 + `bootstrap::stashes_decoded_argv_and_env` + the four io-error tests).
- **No-regression boot** — `cd kernel && cargo build` (all six no_std binaries + kernel,
  only the pre-existing `core` future-incompat warning), then `scripts/run-demo.sh`
  under the `CLAUDE.md` Perl group-kill harness (150 s): `[init] system up` →
  `[console] serving` → `[storaged] store mounted` → `serving` → `eunomia>`;
  `write docs/smoke hello` → `ok`, `cat docs/smoke` → `hello`, `ls docs`, `df` —
  no panic/`Corrupt`/`unwrap`; QEMU killed cleanly (signal 15).
- **Verus** — `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` →
  **`eunomia-sys: 7 verified, 0 errors`** (unchanged; transitive `loader: 29`
  unchanged). The new plain-Rust modules add zero obligations. `loader`/`ipc`/`le-bytes`
  Cargo.tomls were **not** touched (the bridge needs no `rustc-dep-of-std` plumbing), so
  their gates are byte-identical.
- **Formatting** — `cargo fmt -p eunomia-sys -- --check` and `scripts/verusfmt.sh
  --check` both exit 0. The `vendor/rust` edits keep upstream rustfmt style (the fork
  never runs our fmt/verus gates).

## Surface left trusted / unsupported (and why)

- **The std PAL arms** (`sys/pal/eunomia`, the `eunomia.rs` arms) — the trusted shell,
  `kernel/`-over-`kcore` posture: a submodule fork that by construction never runs the
  gate; every non-trivial step delegates across the bridge to a verified/host-tested
  `eunomia-sys` surface. Auditable by inspection vs `pal/unsupported` (the standing
  per-task §11 thinness check; the consolidating audit is 6.2).
- **The `extern "Rust"` bridge symbols** — pure delegation; the `svc` (in
  `__eunomia_thread_exit`) is the category-(d) trusted asm already covered by the
  thread-lifecycle shell seam.
- **`setenv`/`unsetenv`** — `Unsupported` (no env producer / no shared environ yet,
  5.2). **`env::vars`** is empty until a producer emits env entries (5.2).

## Follow-ups

- **2.2 unblocks the live `env::args()` QEMU demo** (allocator) — the Phase-2 GATE.
- **Phase 5.3** (first std user binary) must `extern crate eunomia_sys;` to satisfy the
  bridge; consider a `#[macro]` or a tiny `eunomia-rt` glue crate to make that ergonomic.
- **`abort_internal` override** to `thread_exit(STATUS_PANIC)` is **2.3** (still
  `intrinsics::abort()` here) — a std-binary panic does not yet reap as `STATUS_PANIC`.
- The bridge's `extern "Rust"` ABI is unstable across rustc versions, but both sides are
  built by one toolchain in one build, so it is sound; noted for the 6.3 forward-port
  runbook.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
