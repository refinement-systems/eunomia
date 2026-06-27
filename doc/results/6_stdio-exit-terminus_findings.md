# Findings — Phase 2.3: std stdio (debug-log) + exit terminus

Task 2.3 of `doc/plans/1_plan-rust-std-port.md` — clears the last two blockers before a
std binary is observable and reaps correctly. `sys/stdio/mod.rs` had no eunomia arm
(`println!` was a no-op), and `sys/exit.rs`'s eunomia path fell to `_ =>
intrinsics::abort()` (so `process::exit(0)` *crashed*). This wires stdout/stderr (and
panic last-words) to the kernel `DebugWrite` debug-log path — a disclosed, **temporary
deviation from the rev2§2 capability model** (rev2§2.7, the rev2§7 / C-M9 pre-console
scaffold; replaced for stdout/stdin by the console channel in 5.1, retained only for
panic last-words) — and overrides the PAL `exit()` / `abort_internal()` to the
`thread_exit` terminus so a std binary's panic/OOM reaps as `STATUS_PANIC` and a clean
exit reaps as its code, preserving the rev2§5.1 reaper contract. No new `verus!{}`
obligation and no new trusted seam — the tally stays **14**.

## What shipped

- **`eunomia-sys`** (the seam crate; host-tested logic):
  - `src/stdio.rs` (new, un-gated like `heap.rs`) — `DEBUG_WRITE_MAX = 1024` (the kernel
    cap), a pure `chunks` splitter (`buf.chunks(DEBUG_WRITE_MAX.max(1))`), and a
    target-gated `write(buf) -> usize` that issues one `DebugWrite` per chunk and reports
    the full length (infallible/best-effort). Two host tests: `cap_matches_kernel` (pins
    `DEBUG_WRITE_MAX` against the kernel literal) and `chunks_never_exceed_cap_and_reassemble`
    (exhaustive over `0..=3*cap+5`: every chunk ≤ cap and non-empty, chunks reassemble to
    the input, count == `len.div_ceil(cap)`).
  - `src/pal.rs` — one more `#[no_mangle] extern "Rust"` shim, `__eunomia_stdio_write`,
    delegating to `stdio::write`. (`__eunomia_thread_exit` already existed from 2.1;
    `syscall.rs` is unchanged — the chunker reuses `syscall::debug_write`.)
  - `src/lib.rs` — `mod stdio;`.
- **`vendor/rust`** std PAL (the trusted term-for-term shell):
  - `sys/stdio/eunomia.rs` (new) — separate `Stdin`/`Stdout`/`Stderr` structs (the
    `motor` shape, so 5.1 re-points only the `Stdout`/`Stderr` *bodies*). `Stdout`/`Stderr`
    `write` → `Ok(unsafe { __eunomia_stdio_write(buf) })`, `flush` → `Ok(())`; `Stdin`
    is the `unsupported` `io::Read` surface verbatim (EOF until 5.1). `STDIN_BUF_SIZE = 0`,
    `is_ebadf` → `true` (vacuous; our writes never fail). `panic_output()` returns a
    dedicated `PanicWriter` (debug-log) so 5.1 keeps panic last-words here while moving
    `Stdout`/`Stderr` to the console.
  - `sys/stdio/mod.rs` — a `target_os = "eunomia"` arm before the `_` catch-all.
  - `sys/exit.rs` — a `target_os = "eunomia"` arm: `__eunomia_thread_exit(code as u32 as u64)`.
  - `sys/pal/eunomia/common.rs` — `abort_internal()` now calls
    `__eunomia_thread_exit(u64::MAX)` (== `STATUS_PANIC`), replacing `intrinsics::abort()`.
  - `sys/pal/eunomia/mod.rs` — `_start` zero-extends (`code as u32 as u64`) in lockstep
    with the new `exit()` arm.
- **Ledger** (`doc/guidelines/verus_trusted-base.md`): a std-port-2.3 paragraph appended
  to the eunomia-sys routing note — `stdio` is trusted shell over a host-tested seam
  (chunking = §11 inverse-leak re-establishment of the kernel cap); the two PAL overrides
  preserve the rev2§5.1 reaper contract; the EL0 debug-log use is the disclosed temporary
  rev2§2 deviation. **No new seam; tally stays 14.**

## Decisions (and rejected alternatives)

- **Chunking lives in `eunomia-sys`, not the PAL — and it is mandatory, not politeness.**
  The kernel **rejects** an over-long `DebugWrite` outright: `kernel/src/syscall.rs`'s
  `Sys::DebugWrite` arm does `if !user_range_ok(ptr, len) || len > 1024 { return
  Some(ERR_FAULT); }` — a single `println!` over 1024 bytes would emit **zero** bytes.
  So `write` splits into ≤1024-byte chunks before the `svc`. Per the thinness rule this
  logic lives in the host-tested seam; the PAL arm is a one-line delegate. This is the
  §11 inverse-leak re-establishment of the kernel's `len ≤ 1024` precondition at the seam.
  - *Rejected:* chunking in the PAL `io::Write::write` — that would put real logic in the
    trusted shell, which is meant to hold none.
- **Zero-extend the `i32` exit code in *both* `exit()` and `_start` (`code as u32 as
  u64`).** `-1i32 as u64 == u64::MAX == STATUS_PANIC`, so a sign-extending conversion
  would make `process::exit(-1)` (or `main` returning -1) reap as a *crash*,
  indistinguishable from a panic (the shell matches `Exit::Exited(STATUS_PANIC)` exactly).
  Zero-extending keeps the top half clear, so `STATUS_PANIC` is reachable only via
  `abort_internal`. `_start` (shipped in 2.1) was fixed in the same change so the two
  exit routes agree (confirmed with the user).
  - *Rejected:* matching `_start`'s original `code as u64` (preserves the collision);
    fixing only `exit()` (the two routes would then disagree).
- **Separate `Stdin`/`Stdout`/`Stderr` structs (the `motor` shape), not `type Stderr =
  Stdout` (the `unsupported` alias).** 5.1 splits stderr (`NAME_STDERR`) from stdout
  (console); separate structs let 5.1 re-point only the bodies without reshaping types.
- **`panic_output` returns a dedicated `PanicWriter`, not `Some(Stderr::new())`.** 5.1
  keeps panic last-words on debug-log (rev2§7 C-M9) while moving `Stderr` to the console;
  a distinct type isolates `panic_output` from that change.
- **Override at the PAL chokepoint, fixing panic + OOM + `process::abort` at once.**
  `panic!`/OOM/`process::abort()` all funnel through `crate::sys::abort_internal` (panic
  → `__rust_abort` → `process::abort` → here), so the single `common.rs` override makes
  every abnormal stop reap as `STATUS_PANIC`. (The 2.2 findings traced the OOM chain into
  this exact chokepoint, deferring the override to 2.3.)
- **Best-effort, infallible writes.** The debug-log path has no backpressure and is a
  silent no-op when the kernel lacks the `debug-log` feature, so `write` always returns
  `buf.len()` (std's `write_all` never loops). Real errors/backpressure arrive with the
  5.1 console channel.

## Problems hit and how they were solved

- **The plain `cd kernel && cargo build` did not exercise the new arms.** It finished in
  ~2.5 s recompiling only the `kernel` crate (the build-std graph was cached), so it was
  not a real compile check of `sys/stdio/eunomia.rs` / the exit/abort arms. Resolved with
  the prescribed **throwaway std canary** (a fresh `--target-dir`), which recompiled
  `std v0.0.0` from scratch and linked it — the decisive compile + link + symbol check.
- **`llvm-tools` was not installed** for the pinned nightly, so `llvm-nm`/`llvm-objdump`
  were absent. `rustup component add llvm-tools --toolchain nightly-2026-06-26` provided
  them.
- **Keeping `abort_internal` reachable for the symbol check.** A canary that only calls
  `process::exit(0)` would let the linker drop `abort_internal`. A
  `if std::hint::black_box(false) { std::process::abort(); }` guard keeps it linked
  without aborting at runtime.

## Verification record

Toolchain `nightly-2026-06-26` (== `vendor/rust` `bd08c9e7…`) for the cross-build; Verus
binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Host tests** — `cargo test -p eunomia-sys` → **21 passed, 0 failed** (the prior 19 +
  the two new `stdio::tests`).
- **Verus (authoritative, cold)** — `cargo clean -p eunomia-sys && cargo verus verify -p
  eunomia-sys` → **7 verified, 0 errors** (result line present == a real run). eunomia-sys's
  own count is unchanged — the new plain-Rust `stdio.rs` and the `pal` shim add no
  `verus!{}`.
- **Throwaway std build/link canary** (removed after) — a `fn main()` with `extern crate
  eunomia_sys;` doing `println!`/`eprintln!` + heap `Vec`/`String` + `process::exit(0)`
  (and a `black_box`-guarded `process::abort()`), built for `aarch64-unknown-eunomia` via
  the `kernel/build.rs` flags (`-Zbuild-std=…,std,panic_abort`,
  `__CARGO_TESTS_ONLY_SRC_ROOT=vendor/rust/library`). **`std v0.0.0` recompiled from
  scratch and linked clean, no undefined symbols.** Symbol/instruction checks
  (`llvm-nm`/`llvm-objdump`):
  - entry `start address: 0x80000000` == `_start` (`T`);
  - `__eunomia_stdio_write` and `__eunomia_thread_exit` defined (`T`), alongside
    `__eunomia_alloc`/`__eunomia_dealloc`/`__eunomia_bootstrap_init`;
  - `abort_internal` disassembles to `mov x0, #-0x1` (== `u64::MAX` == `STATUS_PANIC`)
    then `bl __eunomia_thread_exit` — **no `udf`/`brk`/`intrinsics::abort` trap**;
  - `sys::exit::exit` disassembles to `mov w0, w8` (a 32-bit move that **zero-extends**
    into `x0` — the `code as u32 as u64`) then `bl __eunomia_thread_exit`, confirming
    `exit(-1)` → `0x0000_0000_FFFF_FFFF`, never `STATUS_PANIC`.
- **Kernel build** — `cd kernel && cargo build` clean (the vendored std with the new arms
  rebuilds for every user sub-build via `build.rs`'s
  `rerun-if-changed=vendor/rust/library/std/src`).
- **No-regression boot** — `scripts/spawn-test.sh` (under the `CLAUDE.md` Perl
  group-kill backstop) → **SPAWN TEST PASS**: runloop 100/100 slots reclaimed,
  exit(42)/exit(7) propagated, fault demo `faulted(translation, 0xdead0000)` then clean
  re-spawn, **panic demo `panicked` (reserved status), not exited(254)**, time grant
  `time-ok`, no BSS-LEAK / no unexpected PANIC. The no_std panic→`STATUS_PANIC` reaper
  path (which the 2.3 std override mirrors) is unregressed.
- **Formatting** — `cargo fmt -p eunomia-sys -- --check` clean (the authority);
  `vendor/rust` keeps upstream rustfmt style. No `verusfmt` (no `verus!{}` added; the
  `stdio.rs` doc-comment deliberately avoids the literal `verus!{}` token that triggers
  `verusfmt.sh`'s known false-positive).

The **live** `println!`/panic-reap assertion is deferred to the combined **Phase-2 GATE**
(after 2.4 wires time) and the std `hello` at 5.3 — there is no std `user/*` binary yet
(all are no_std), so the live `Exit::Exited(STATUS_PANIC)` reap can only be observed once
a std binary is spawned and reaped. This matches how 2.1/2.2 deferred their live demos. No
`.github/workflows/ci.yml` edit belongs to 2.3.

## Surface left trusted / unsupported (and why)

- **The `sys/stdio/eunomia.rs` arm + the two PAL overrides (`sys/exit.rs`,
  `common.rs::abort_internal`, `_start`)** — the trusted term-for-term shell, the
  `kernel/`-over-`kcore` posture (a submodule fork that by construction never runs the
  gate). Each step delegates to the host-tested seam (`__eunomia_stdio_write` /
  `__eunomia_thread_exit`). The §11 inverse-leak check: the stdio arm re-establishes the
  kernel's `len ≤ 1024` precondition via the seam's chunking; the exit/abort arms
  re-establish nothing (`thread_exit` is total over every `u64`).
- **The EL0 debug-log path for stdout/stderr** — a disclosed **temporary deviation from
  rev2§2** (rev2§2.7), the rev2§7 / C-M9 pre-console scaffold. Closed for the user-facing
  path by 5.1 (the console channel); panic last-words stay on debug-log
  (kernel-diagnostic).
- **`STATUS_PANIC = u64::MAX` duplicated as a literal in `common.rs`** — std cannot depend
  on the seam crate (vstd/verus_builtin is not sysroot-buildable), the same posture as the
  `ERR_*` discriminants in `sys/io/error/eunomia.rs`. Kept in lockstep with
  `eunomia-sys/src/syscall.rs`.
- **Stdin is EOF and the `Unsupported` fs/stdin surface stays as upstream** — stdin is
  deliberately unassigned until the 5.1 console (rev2§7).

## Follow-ups

- **5.1** moves stdout/stdin onto the `user/console` channel: re-point the `Stdout`/
  `Stderr` write bodies (console / `NAME_STDERR`), keep `PanicWriter`/debug-log for panic
  last-words, raise `STDIN_BUF_SIZE` to `DEFAULT_BUF_SIZE`, and add `NAME_STDERR`.
- **2.4** wires time (`Instant`/`SystemTime`); then the combined Phase-2 GATE asserts
  `println!`/`format!`/`Vec`/`Box`/`String`/`Instant`/`SystemTime` live in QEMU. **5.3**
  rewrites `hello` on std and validates the live `STATUS_PANIC` reap.
- **The kernel cap `1024` is a bare literal** (`kernel/src/syscall.rs`, no shared `kcore`
  const), pinned here only by `cap_matches_kernel` — a drift hazard. A small follow-up
  could hoist a named const both sides cite.
- **`debug-log`-feature-off ⇒ silent stdout.** On a kernel built without the `debug-log`
  feature, `println!` produces no output (the seam still returns `buf.len()`, so std does
  not error or spin). Dev images are default-on; the guaranteed sink is the 5.1 console.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
