# Findings — Phase 2.2: GlobalAlloc

Task 2.2 of `doc/plans/1_plan-rust-std-port.md` — clears the GlobalAlloc blocker.
`sys/alloc/mod.rs` has **no `_` fallback arm**, so the eunomia arm is mandatory; it
shipped from 0.2/2.1 as a stub returning `null`. This wires std's `System` allocator
to the process-global `urt::Heap<N>` (whose allocation arithmetic is Verus-verified via
`freelist`), so `Box`/`Vec`/`String`/`format!` work in a std binary on-target. No new
`verus!{}` obligation and no new trusted seam — the tally stays **14**.

## What shipped

- **`vendor/rust`** std PAL (the trusted term-for-term shell):
  - `sys/alloc/eunomia.rs` — replaces the null stub. A local narrow `unsafe extern
    "Rust"` block declaring `__eunomia_alloc`/`__eunomia_dealloc`, and `GlobalAlloc for
    System` delegating to them (each call wrapped in `unsafe {}`, the module inherits
    `#![forbid(unsafe_op_in_unsafe_fn)]`). Only `alloc`+`dealloc` — `realloc`/
    `alloc_zeroed` use `GlobalAlloc`'s defaults, mirroring `urt::Heap`. The motor arm
    is the structural twin.
- **`eunomia-sys`** (the seam crate):
  - `src/pal.rs` — a process-global `static HEAP: urt::Heap<{ heap::HEAP_BYTES }> =
    urt::Heap::new();` (plain `static`; interior `UnsafeCell` + urt's `unsafe impl
    Sync`; all-zero `new()` lands it in `.bss`, mapped+zeroed by the loader) plus the
    two `#[no_mangle] extern "Rust"` shims delegating to it.
  - `src/heap.rs` (new, **un-gated** so host-testable) — `HEAP_BYTES`: a compile-time
    const, 1 MiB default, `EUNOMIA_HEAP_BYTES`-overridable via a `const fn` decimal
    parser (`parse_dec`) that panics in const-eval (a build error) on empty/non-digit/
    overflow. Host tests cover parse totality, const-context use, the default, and the
    three rejections.
  - `Cargo.toml` — `urt` added as a **target-gated** dep (`cfg(any(target_os =
    "eunomia", target_os = "none"))`, matching `pal.rs`'s own gate) so the host
    `verify`/`test` graph stays byte-identical.
  - `src/lib.rs` — `mod heap;`.
- **Ledger** (`doc/guidelines/verus_trusted-base.md`): a std-port-2.2 addendum to the
  `eunomia-sys` routing note — the GlobalAlloc shim is trusted shell over the verified
  `urt::Heap`/`freelist`; the arena byte-region is the existing Miri+proptest seam (not
  one of the 14); inverse-leak vacuous; tally stays 14.

## Decisions (and rejected alternatives)

- **`System` is backed by a single `urt::Heap<N>` static in `eunomia-sys`, not a
  per-binary `#[global_allocator]`.** With no `#[global_allocator]` declared, the
  compiler routes `__rust_alloc → __rdl_alloc → System.alloc` (`std/src/alloc.rs`), so
  backing `System` makes the heap active with **zero attribute** in the consuming
  binary — exactly the motor model (`moto_rt::alloc`). One heap, no wasted `.bss`.
  - *Rejected:* each std binary declaring `#[global_allocator] static H: urt::Heap<N>`
    (today's no_std posture). It gives literal per-binary `N`, but `System` would still
    need a backing (its impl must link), and any eunomia-sys `System` heap static would
    then be **wasted committed `.bss`** alongside the override (reservation == committed
    RAM, no demand paging). The seam makes `System` itself the clean single allocator.
- **Per-binary `N` = `option_env!` hybrid, default 1 MiB** (user-chosen). A default
  const overridable at compile time by `EUNOMIA_HEAP_BYTES`, to be threaded per binary
  via `kernel/build.rs`'s existing `build_user(..., envs)` mechanism (wired with the
  first std binary, 5.3). `N` is committed RAM at spawn, so a per-binary reservation
  knob. 1 MiB matches the shell's current no_std heap.
  - *Rejected:* a cargo feature selecting a size — features are additive, the wrong
    model for a scalar (would need `compile_error!` guards against double-selection).
- **Implement only `alloc`+`dealloc`; inherit the default `realloc`
  (alloc+copy+dealloc) and `alloc_zeroed` (alloc+memset).** Mirrors `urt::Heap` exactly.
  Disclosed MVP cost: **no in-place grow** (motor overrides these for perf; we accept
  the defaults, correct but a copy on every `Vec` regrow).
- **Pass `core::alloc::Layout` by value across the seam, not `(size, align)`.** One
  `core` is built once, so both sides share the identical type; passing the already-
  validated `Layout` preserves its invariant and avoids an
  `Layout::from_size_align_unchecked` re-construction in the shim (an inverse-leak
  smell). The seam already carries richer types (`&'static [&'static [u8]]`).
- **OOM/abort is documented, not overridden, here.** The `abort_internal()` →
  `__eunomia_thread_exit(STATUS_PANIC)` override is Phase 2.3 (it ships paired with the
  `exit()` override and 2.3's debug-log `panic_output`); a clean 2.1/2.2 split was
  already chosen. See *Surface left trusted* for the traced chain.

## Problems hit and how they were solved

- **`#![forbid(unsafe_op_in_unsafe_fn)]` (build-breaker).** `sys/alloc/mod.rs` forbids
  it crate-wide, cascading into the arm. A bare call to the `unsafe extern "Rust"` seam
  fns inside `unsafe fn alloc`/`dealloc` does not compile; both are wrapped in an inner
  `unsafe {}` block (the `sys/args`/`sys/env` arm pattern).
- **Host dead-code warnings.** `HEAP_BYTES`/`parse_dec` are consumed only by the
  target-gated `pal.rs` and `cfg(test)`, so a host **non-test** lib build flags them
  unused. Suppressed with `#[allow(dead_code)]` + a rationale comment on each (they are
  live on the target build and under test).
- **LTO eliminated `__eunomia_dealloc` in the release canary.** `process::exit` skips
  drops, so the release build had no reachable dealloc path and LTO dropped the symbol.
  Confirmed it links by also building the canary in the dev profile (no LTO) with
  explicit `drop`s + `black_box` — both `__eunomia_alloc` and `__eunomia_dealloc` then
  resolve as `T`.
- **`verusfmt --check` pre-existing false-positive.** It flags `eunomia-sys/src/{
  bootstrap,io_error}.rs` — files this task did **not** touch — because `verusfmt.sh`
  selects via `git grep -l 'verus!'` and those files' *doc comments* contain the literal
  "`No verus!{}` obligation". Confirmed pre-existing on `main` (fails identically with
  this task's edits stashed); CI does **not** run verusfmt (only `cargo fmt --check`,
  which is clean). Not introduced here; the unrelated 2.1 files are left untouched.

## Verification record

Toolchain `nightly-2026-06-26` (== `vendor/rust` `bd08c9e7…`); Verus binary
`0.2026.06.07.cd03505`, toolchain `1.95.0`; verusfmt 0.7.2.

- **Verus (authoritative, cold)** — clean + verify each:
  - `cargo clean -p freelist && cargo verus verify -p freelist` → **30 verified, 0
    errors**.
  - `cargo clean -p urt && cargo verus verify -p urt` → **25 verified, 0 errors**.
  - `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` → **7 verified, 0
    errors** (own count unchanged — `heap.rs`/`pal.rs` add no `verus!{}`; the `urt` dep
    re-verifies urt/freelist transitively).
- **urt Miri sweep** — `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest
  run -p urt -j4` → **22 tests run: 22 passed, 0 skipped** (the heap arena proptests —
  `alloc_dealloc_realloc_roundtrip`, `exhaustion_then_coalesce`,
  `fragmentation_cap_never_ub`, the cap-leak witness — all green).
- **Host tests** — `cargo test -p eunomia-sys` → **19 passed, 0 failed** (the prior 13
  + the 6 new `heap::tests`).
- **Bridge/build check** (throwaway std canary, the 2.1 posture; removed after): a `fn
  main` with `extern crate eunomia_sys;` allocating `Vec`/`String`/`Box`/`format!`,
  built for `aarch64-unknown-eunomia` via the `kernel/build.rs` flags
  (`-Zbuild-std=…,std,panic_abort`, `__CARGO_TESTS_ONLY_SRC_ROOT=vendor/rust/library`).
  Clean link, **no undefined symbols**; `__eunomia_alloc`/`__eunomia_dealloc` defined
  `T`; `_start` the entry (`0x80000000`); the `urt::Heap<100000>` (`Kj100000` = 1 MiB,
  confirming the const-generic resolved to the default) `GlobalAlloc` impl present;
  `pal::HEAP` in `.bss`.
- **No-regression boot** — `cd kernel && cargo build` (kernel + all six no_std binaries;
  the vendored std with the new arm rebuilds for every user sub-build) clean, then
  `scripts/run-demo.sh` under the `CLAUDE.md` Perl group-kill harness (180 s): `[init]
  system up` → `[console] serving` → `[storaged] store mounted` → `serving`; `write
  docs/smoke hello` → `ok`, `cat docs/smoke` → `hello`, `ls docs`, `df` — no panic/
  `Corrupt`/`unwrap`; QEMU killed cleanly (signal 15).
- **Formatting** — `cargo fmt -p eunomia-sys -- --check` clean (the authority);
  `vendor/rust` keeps upstream rustfmt style. `verusfmt --check` — pre-existing
  false-positive only (above), no new flag.

The **live** `Box`/`Vec`/`String`/`Instant`/`SystemTime` QEMU assertion is the combined
**Phase-2 GATE** (after 2.3 wires stdio so `println!` works), not 2.2 (no stdout yet).

## Surface left trusted / unsupported (and why)

- **The `sys/alloc/eunomia.rs` arm + the `pal.rs` `HEAP` shims** — trusted term-for-term
  shell, the `kernel/`-over-`kcore` posture. The allocation **algorithm** is the
  Verus-verified `freelist` (30/0, transitively); the arena byte-region
  (`UnsafeCell<[u8; N]>` + `base.add(off)`) is the existing **Miri+proptest** seam that
  is *not* one of the 14. The §11 inverse-leak check is **vacuous**: `urt::Heap::alloc`
  has no `requires` and is total over every `Layout` (null on
  over-`MAX_ALIGN`/exhaustion/cap), so the shim re-establishes no precondition.
- **OOM → abort is a raw trap until 2.3.** Traced chain: `System.alloc → null` →
  `handle_alloc_error` → `__rust_alloc_error_handler` → `rust_oom` →
  `default_alloc_error_hook` → `process::abort()` → `crate::sys::abort_internal()` →
  `sys::pal::eunomia::abort_internal()` = `core::intrinsics::abort()` → **raw aarch64
  trap**. The chain funnels into the single PAL chokepoint `abort_internal()` (it is
  interceptable — not libc `abort()`), which **Phase 2.3** overrides to
  `__eunomia_thread_exit(STATUS_PANIC)` so OOM reaps like a panic. Until then OOM is a
  raw fault, not `STATUS_PANIC`.
- **Disclosed MVP heap bounds** (in the arm comment + `urt`'s module doc): single-thread
  **no lock** (`unsafe impl Sync`; Phase 3 adds a lock or per-thread arenas);
  `MAX_ALIGN = 64` (a page-aligned/over-64 request → null = clean OOM, not UB);
  fragmentation cap 1024 (a second, independent limit; a `dealloc` at the cap **leaks**);
  OOM is a **hard abort** (not a graceful `Err`); `N` is a reservation committed at spawn
  (no demand paging), not a ceiling.

## Follow-ups

- **2.3** overrides PAL `abort_internal()`/`exit()` → `thread_exit(STATUS_PANIC)` /
  `thread_exit(code)`, so OOM/panic reap correctly; then the combined Phase-2 GATE.
- **5.3** threads `EUNOMIA_HEAP_BYTES` through `kernel/build.rs` for std binaries (the
  per-binary `N` knob's producer side) and needs `extern crate eunomia_sys;` ergonomics;
  the shell on std needs a larger `N` than its no_std 1 MiB.
- **Deferred** (plan): growable heap (a `heap` named grant + `sbrk` via retype/map,
  folding under the Store/aspace page-table-join seam) — replaces the fixed `.bss`
  reservation when an input-proportional consumer appears; the frag-cap leak window.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
