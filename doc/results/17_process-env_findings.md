# Findings 17 — process/env polish + env producer (std-port 5.2)

Populates the last missing link in the std environment path: a **producer**. The
consumer half (verified `loader::startup` env arena → `eunomia-sys` bootstrap stash →
`__eunomia_env` bridge → `sys/env/eunomia.rs` `KEY=VALUE` split) shipped in 2.1 and was
host-tested, but nobody ever called `Startup::push_env`, so every startup block shipped
`nenv = 0` and `std::env::vars()` was empty in a spawned std binary. 5.2 makes init
define a base environment, the shell inherit it and forward it to every child (POSIX
inheritance), and gives `std::env::temp_dir()` a real, non-panicking arm. Proven live by
a new `STD52 PASS` arm in the QEMU smoke gate. `process::exit`/`abort_internal` (2.3) and
`process::Command` (intentionally `Unsupported`) needed re-confirmation only, no code
change. No `verus!{}` code is touched and no new trusted seam — the tally stays **14**.

## What shipped

1. **`user/init/src/main.rs`** — `const BASE_ENV: &[&[u8]] = &[b"PATH=/bin", b"TMPDIR=/tmp",
   b"TERM=eunomia"]`; `build_shell_block` pushes it via `push_env` before `encode`. init is
   the single definition point of the system environment. Test `shell_block_carries_named_grants`
   extended to assert `nenv == 3` and `s.env[..] == BASE_ENV`.
2. **`user/shell/src/runtime.rs`** — the inherited-env stash. The boot buffer is promoted
   from a local `[u8; 256]` to a `static mut SHELL_BOOT` (mirroring `eunomia-sys/bootstrap.rs`),
   so the decoded env slices are genuinely `'static`; `_start` decodes a `'static` view and
   stashes `s.env[..s.nenv]` into `static mut SHELL_ENV`/`SHELL_NENV`. `shell_env() ->
   &'static [&'static [u8]]` exposes it. `spawn_inner` passes `shell_env()` to
   `build_child_block`, so **every** child inherits the environment.
3. **`user/shell/src/main.rs`** — `build_child_block` gains an `env: &[&[u8]]` parameter and a
   `for &e in env { s.push_env(e)?; }` loop after the argv loop (`push_env`/`encode` errors
   already map to a clean `RunErr::Startup` spawn refusal). Doc comment updated (env is now
   populated, not "left empty").
4. **`user/shell/src/tests.rs`** — the `build_child_block` signature change threaded through the
   `child_block` helper and all ~11 call sites (env `&[]` where env-agnostic); new positive test
   `build_child_block_forwards_env` (a 3-entry env round-trips through `decode` alongside argv +
   grants).
5. **`vendor/rust`'s `sys/paths/eunomia.rs` (new) + `sys/paths/mod.rs`** — a `temp_dir()` that
   resolves `TMPDIR` from the environment with a `/tmp` fallback (the unix policy), and a
   `target_os = "eunomia"` `cfg_select!` arm pulling **only** `temp_dir` from eunomia and
   `getcwd`/`chdir`/`current_exe`/`split_paths`/`join_paths`/`home_dir` from `unsupported`. This
   replaces the `_`→`unsupported` fall-through whose `temp_dir()` **panicked**
   (`panic!("no filesystem on this platform")`) — an infallible std function must return a path.
6. **`user/stdsmoke/src/main.rs`** — a new `env` arm: reads `env::var("PATH")`/`"TERM"`/`"TMPDIR"`
   (exact values), iterates `env::vars()` (non-empty, keys present), and asserts `env::temp_dir()
   == /tmp`; prints `[stdsmoke] env ok …` + `STD52 PASS`, `exit(12/13/14)` + an `env-bad` line on
   any mismatch.
7. **`scripts/std-smoke-test.sh`** — drives `run bin/stdsmoke env` (after the `tls` arm), waits for
   the `env start`/`env ok`/`STD52 PASS` markers, and adds the `STD52 PASS` + `env-bad` failure
   assertions and the summary line.

## Decisions & rejected alternatives

- **Env model — inheritance (init → shell → child), not fixed-per-producer.** init defines the
  base environment once; the shell stashes what it received and forwards it verbatim. Rejected the
  simpler "shell emits its own fixed static env to children" because it duplicates the literals
  across init and shell (drift risk) and is not true inheritance. The cost — a boot-time stash in
  the shell — is small and reuses the proven `bootstrap.rs` `static mut` + `addr_of!` init-once
  pattern. Forward-compatible with a future shell `export`/env-set built-in once the shell moves to
  std (5.3).
- **`temp_dir` — env-based (`TMPDIR`, `/tmp` fallback), not fixed `/tmp`.** Ties `temp_dir` to the
  new env: the smoke arm asserts `temp_dir() == /tmp` *because* inheritance delivered `TMPDIR`. The
  arm reads a non-`TMPDIR` var (`PATH`/`TERM`) as well, so the check proves inheritance rather than
  the `/tmp` fallback silently masking a broken chain. `current_dir`/`set_current_dir` stay
  `unsupported` (they already return `Err(Unsupported)`, not panic — the handle-relative,
  no-ambient-cwd posture, rev2§4.9). The real `NAME_TMP` writable-scratch subtree grant stays
  deferred (no subtree is carved today; `TMPDIR` rides the env instead).
- **`process::Command`, `exit`, `abort` — no code change.** `exit(code)` →
  `__eunomia_thread_exit(code as u32 as u64)` and `abort_internal()` → `__eunomia_thread_exit(u64::MAX
  == STATUS_PANIC)` shipped in 2.3 (re-confirmed by reading `sys/exit.rs` + `sys/pal/eunomia/common.rs`).
  `Command` routes to `sys/process/unsupported.rs` via the `_` arm — the intended posture (Global
  decision: a native capability-rich spawn API, not emulated fork/exec). `setenv`/`unsetenv` stay
  `Unsupported` (no shared mutable environ).

## Problems hit

- **`dangerous_implicit_autorefs` lint (deny-by-default) on slicing a raw-pointer deref.**
  `&(*addr_of!(SHELL_ENV))[..n]` forms an implicit autoref of `*raw_ptr` before indexing, which the
  compiler rejects. Fixed by binding an explicit array reference first
  (`let arr: &'static [..; MAX_ENV] = &*addr_of!(SHELL_ENV); &arr[..n]`), and likewise for the boot
  buffer view. No behavior change — just makes the reference explicit.
- **Stash soundness (the central pitfall, avoided by design).** The decoded `Startup`'s env slices
  borrow the boot buffer; stashing slices of a *local* boot buffer into a `static` would be
  use-after-return UB. Promoting the boot buffer itself to `static mut SHELL_BOOT` makes the slices
  genuinely `'static`. Written once in `_start` before the REPL/any spawn; the shell is
  single-threaded (it spawns child processes, not threads), so no concurrent access — the same
  init-once discipline as `bootstrap::commit`.

## Verification record

Toolchain `nightly-2026-06-26`; Verus binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Host tests:** `cargo test --manifest-path user/init/Cargo.toml` → **5 passed**;
  `cargo test --manifest-path user/shell/Cargo.toml` → **31 passed** (incl. the new
  `build_child_block_forwards_env`); `cargo test -p loader -p eunomia-sys` → all green (the
  `loader::startup` env round-trip + `bootstrap::stashes_decoded_argv_and_env` already cover the
  consumer half).
- **Cross build:** `cd kernel && cargo build` — rebuilds std from the edited `sys/paths` and
  cross-builds `stdsmoke`; clean (only the pre-existing `core` future-incompat warning).
- **Verus (unchanged):** the diff touches no `verus!{}` source (loader/startup.rs and eunomia-sys
  are byte-identical; the producer calls live in the ungated `user/*` crates), so obligation counts
  are the baseline. `cargo clean -p loader -p eunomia-sys` then `cargo verus verify` →
  **loader: 30 verified, 0 errors**; **eunomia-sys: 16 verified, 0 errors** (these are the current
  tree's counts; the plan doc's 29/7 predate findings #14/#16).
- **Formatting:** `cargo fmt --check` and `scripts/verusfmt.sh --check` both exit 0 (root + the
  `user/{init,shell,stdsmoke}` manifests formatted; the `vendor/rust` edits keep upstream style,
  mirroring `sys/paths/motor.rs`).
- **QEMU gate:** `scripts/std-smoke-test.sh` (under the `CLAUDE.md` group-kill harness) →
  **`STD SMOKE TEST PASS`** with the new **`STD52 PASS — env inherited init→shell→child
  (PATH/TMPDIR/TERM); env::temp_dir=/tmp`** and every existing arm
  (`STD2/STD32/STD33/STD34/STD35/STD51`) plus the panic reap still green. This is the live witness
  that env inheritance (init → shell stash → child block → `eunomia-sys` stash → std
  `env::var`/`env::vars`) and the env-based `temp_dir` work end to end, and that the added env stays
  within the 256-byte `MAX_BLOCK` (the tightest child — thread+console, 8 grants + argv + 3-entry
  env — is ~162 bytes).

## Trusted / unsupported surface (and why)

- **`sys/paths/eunomia.rs`** — the trusted PAL shell, `kernel/`-over-`kcore` posture. Per-task
  §11 thinness + inverse-leak check vs `pal/unsupported`: `temp_dir()` is a one-line delegation to
  `crate::env::var_os("TMPDIR")` (no arithmetic, no parsing, no business logic — the policy is a
  `map_or_else` fallback constant); every other paths surface (`getcwd`/`chdir`/`current_exe`/
  `split_paths`/`join_paths`/`home_dir`) is re-exported unchanged from `unsupported`, so the arm
  adds zero new logic and re-establishes no verified `requires`. The tally **stays 14**.
- **`process::Command` / `setenv` / `unsetenv`** — `Unsupported` by design (native spawn, no shared
  mutable environ), not a missing arm.
- **The shell env stash** — plain-Rust `static mut` + `addr_of!` marshalling over the verified
  `loader::startup::decode` (the untrusted-byte boundary is the verified decoder); the copy is
  ordinary bookkeeping over the validated structure, host-covered indirectly by the decode
  round-trip and directly by the QEMU gate.

## Follow-ups

- **Real `NAME_TMP` scratch-subtree grant** (deferred): once a writable scratch subtree is carved,
  `temp_dir` could resolve to a granted handle-relative root rather than a name string; `TMPDIR` in
  the env is the MVP bridge.
- **Shell-set / mutable env** (when the shell moves to std, 5.3): an `export`-style built-in and a
  mutable environ would let `setenv`/`unsetenv` become supported; today they stay `Unsupported`.
- The base env is a **fixed 3-entry set** defined in init; if it grows toward the 256-byte block
  budget on the tightest (thread+console) child, spawns degrade to a clean `RunErr::Startup`
  refusal (never a panic) — keep it small.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not referenced
from code, specs, or guidelines.
