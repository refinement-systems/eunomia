# C-M9-C — shell I/O onto the console channel + debug-syscall retirement + closeout

Implements sub-phase **C-M9-C** of `doc/plans/19_cm9-detail.md` — the headline of
the userspace-console track: the shell does **all** terminal I/O over the console
channel, the EL0 ambient input syscall is removed, the output syscalls are
re-scoped to a build-gated kernel-diagnostic path, the kernel UART is demoted to
output-only, and the spec/ledger close out. This **closes audit S-8 / M-9 [high]**
for the user-facing path. Branch `cm9c-shell-rewire`, off `origin/main` @ `9eefb2a`
(which already contained C-M9-A/#174 and C-M9-B/#176).

The one verified-surface touch is deleting the total `DebugGetc` decode arm:
`cargo verus verify -p kcore` is **389 verified, 0 errors** (unchanged), and the
`external_body`/`assume_specification` tally is **untouched at 14** — C-M9 adds no
seam.

> **C-M9-B had already moved the shell's *input* onto the channel** (a live
> RX-draining driver starves `debug_getc` — they share the one UART; see
> `doc/results/32_cm9b-findings.md`). So C-M9-C is the *output* half plus the
> syscall retirement and closeout.

## What landed

1. **Shell output + echo onto the `stdout` channel** (`user/shell/`). A new
   `resolve_stdout_slot` (`main.rs`) resolves the `NAME_STDOUT` grant; `_start`
   stores it in a `STDOUT_SLOT` atomic and makes an unbound `stdout` fatal
   alongside the unbound-`stdin` check. `out()` now chunks the buffer at
   `MSG_PAYLOAD` (256) and `chan_send`s each chunk (ERR_FULL → yield), replacing
   `sys::debug_write`; the per-byte echo `sys::debug_putc(b)` becomes `out(&[b])`.
   The REPL's echo/line-editing logic is unchanged — only the transport moves
   (Design decision 1). The console driver, unchanged, drains these as TX bytes.
2. **`DebugGetc` (opcode 20) removed** — the variant + decode arm
   (`kcore/src/sysabi.rs`), the handler (`kernel/src/syscall.rs`), and the libcall
   (`ipc/src/sys.rs`). Opcode 20 now decodes to `UnknownCall`; a regression
   assertion pins it (`sysabi.rs` `validation_rejects`). This is the lone verified
   edit; `decode` stays total.
3. **`DebugPutc`/`DebugWrite` gated behind a new `debug-log` feature**
   (default-on). The decoder still produces both variants (so the verified surface
   is untouched beyond `DebugGetc`); the **kernel handler bodies**
   (`kernel/src/syscall.rs`) and the **ipc libcalls** (`ipc/src/sys.rs`) are
   `#[cfg(feature = "debug-log")]`-gated. With the feature off the EL0 debug-output
   path is inert — the production gate.
4. **Kernel UART demoted to output-only** (`kernel/src/uart.rs`). `getc()` +
   `FR_RXFE` are deleted (the `DebugGetc` handler was their only caller); the
   header is re-commented as the kernel-internal panic/fault/boot diagnostic path.
5. **Spec + ledger closeout** — non-normative status notes in rev1§2.7 and rev1§7
   (driver landed; `getc` removed; `putc`/`write` build-gated; UART
   kernel-internal); a parenthetical on the kcore Baselines row recording the arm
   removal, 389/0 re-verify, and tally-unchanged-at-14.

## The `debug-log` feature design (the load-bearing mechanism)

The goal (Design decision 5): close the ambient **input** hole outright, re-scope
the **output** syscalls to a disclosed, build-gated kernel-diagnostic path — the
servers' boot diagnostics (`[init] system up`, `[storaged] store mounted`, …) run
*before* any console exists, so they cannot move to the channel yet.

The mechanism fell cleanly out of the dependency graph: **every `user/*` binary
and `urt` declare `ipc = { path = "../../ipc" }` with default features on**, and
the kernel does not depend on ipc. So:

- **`ipc`** gains `default = ["debug-log"]` + `debug-log = []`, gating the
  `debug_putc`/`debug_write` libcalls. All 31 server call sites inherit the
  feature transitively — **no per-binary plumbing** — and keep their diagnostics.
- **`kernel`** gains its own `default = ["debug-log"]` + `debug-log = []`, gating
  the handler bodies. This is the **authoritative production gate**: a single
  toggle, `cd kernel && cargo build --no-default-features`, makes the two EL0
  debug-output syscalls inert no-ops regardless of what userspace emits.

The handler arms stay present (the decoder still yields the variants) with
conditional bodies — `#[cfg(feature = "debug-log")]` real write / `#[cfg(not)]`
no-op — so the `execute` match stays exhaustive under both feature states.

All existing builds keep default features (`cd kernel && cargo build`,
`--features m1-test`, the spawn/boot/run-demo scripts), so behaviour is
byte-identical; only an explicit `--no-default-features` flips the gate.

## `out()` chunking + the `diag()` / `out()` split

- **Chunking.** `MSG_PAYLOAD = 256` (`kcore::channel`) and the console's TX recv
  buffer is `[0u8; 256]`. `cmd_cat` emits a whole-file `Vec` in one `out()`, so
  `out()` loops `s.chunks(256)` and `chan_send`s each — any length streams in
  FIFO order. (The old `debug_write` capped at 1024 and silently `ERR_FAULT`ed
  past it; chunking removes that cap.) Backpressure: the console runs at priority
  6 (> shell 4), so an `ERR_FULL` yield is drained promptly; RX and TX use
  opposite channel queues, so there is no deadlock.
- **`diag()` vs `out()`.** The shell's *only* remaining debug-syscall use is a
  `diag()` helper (`sys::debug_write`) for **pre-console fatals** (carve-spawn
  failure, unbound stdin/stdout) and the **panic handler** — failures that fire
  before the channel is usable, or during a panic when the channel may be the
  cause. This is exactly rev1§7's "kept, if at all, only for kernel-internal panic
  reporting" and Design decision 6.3's "via the kernel-diagnostic path." The
  acceptance "the shell calls neither output syscall" holds for the **user-facing
  REPL path** (banner/prompt/results/echo — all on the channel); the diagnostic
  escape is the sanctioned exception, not a back door. (The shell already had a
  `debug_write` panic handler pre-C-M9-C, so this adds no new dependency.)

## Deviations from the literal plan

- **`uart::getc` + `FR_RXFE` deleted, not just re-commented.** The plan said
  `uart.rs` has "no functional change," but deleting the `DebugGetc` handler
  orphans `getc()` (its only caller) → a dead-code warning. Removing it is the
  honest reflection of the demotion (the kernel no longer reads the UART) and
  keeps the build warning-free. Everything else in `uart.rs` (`Uart`/`putc`/
  `Write`) is unchanged.
- **Findings file is `33_`, not the plan's `33_cm9c` vs a fresh number.** The
  `origin/main` sync pulled in `33_c3b-findings.md` (parallel C3 track). The
  per-track convention (cm9a=31, cm9b=32) makes **`33_cm9c`** the right name; the
  number is shared with the C3 track, matching the established pattern (31_c2c +
  31_cm9a, 32_c3a + 32_cm9b already coexist).

## Verification

- `cargo verus verify -p kcore` → **389 verified, 0 errors** (unchanged; the
  `DebugGetc` arm deletion cannot lower a total-arm count). `cargo verus verify -p
  ipc` → **69 verified, 0 errors** (unchanged by this phase — the gated libcalls
  are not verus items; the count is 69 post-C3, not the ledger's pre-C3 62).
- `cargo test --manifest-path user/shell/Cargo.toml` → 26 passed (new
  `resolve_stdout_slot` golden/absent assertions + existing logic).
- `cargo test --manifest-path user/init/Cargo.toml` → 5 passed
  (`shell_block_carries_named_grants` still green); console host tests → 5 passed
  (unaffected); kcore `sysabi` tests → 3 passed (incl. the opcode-20 regression).
- Cross-build: `cd kernel && cargo build` (default) links every `user/*` binary;
  `cargo build --features m1-test` green; **`cargo build --no-default-features`
  green** (the production gate — handler bodies compile out, match stays
  exhaustive).
- **`scripts/spawn-test.sh`** — **PASS**: runloop 100/100, slots reclaimed,
  exit/fault/panic/time demos green, no BSS-LEAK, no unexpected PANIC. Input
  flows stdin → PL011 RX → console IRQ → console → `chan_recv(stdin)` → shell.
- **`scripts/run-demo.sh`** (process-group timeout harness) — green **and fully
  interactive over the channel**:

  ```
  [init] system up
  [console] serving
  [storaged] store mounted
  [storaged] serving
  Eunomia shell - type help
  eunomia> help
  ls cat write mv rm sync run runloop date
  snap snaps rollback snapdel keep prune gc df help
  eunomia> write docs/smoke hello-over-the-channel
  ok
  eunomia> cat docs/smoke
  hello-over-the-channel
  eunomia> ls docs
  readme  (53 bytes)
  smoke  (22 bytes)
  eunomia> df
  chunk region: 31359 used / 66020737 free of 66052096 bytes
  eunomia> date
  2026-06-23T05:34:06.035515008Z
  ```

  Every prompt/echo/result crossed the `stdout` channel (shell → `chan_send` →
  console → UART); `cat` returns exactly what `write` stored — output integrity
  end to end. No panic/`Corrupt`/fault.
- **No-console negative control** (anti-theater, Design decision 6.3): with the
  `stdin`/`stdout` grants temporarily disabled in init, the boot reached
  `[console] serving` / `[storaged] serving` and the shell then printed
  `[shell] FATAL: stdin unbound (console not wired)` via `diag()` and exited
  cleanly — **no** silent `debug_getc` fallback (it is gone), **no** invisible
  hang. The temporary edit was reverted (init host tests green afterward).

## State after C-M9-C

The shell does **all** terminal I/O over the console channel; **no EL0
user-facing path** uses the kernel debug-UART syscalls. `DebugGetc` is gone;
`DebugPutc`/`DebugWrite` exist only as the `debug-log`-gated kernel-diagnostic
path for pre-console server logging + panic reporting. The kernel UART is
output-only and kernel-internal. Audit **S-8** and **M-9 [high]** are closed for
the user-facing console.

## Out of scope (carried forward, per the parent plan)

- Routing **server** boot diagnostics through the console and removing the output
  syscalls entirely — needs a kernel boot-log buffer (they predate the console).
- A fully-building **production userspace image** (per-`user/*` `debug-log` off) —
  the mechanism exists (the feature); the authoritative production gate is the
  kernel feature. Wiring `--no-default-features` through each user manifest is
  follow-on.
- Kernel-internal UART path (panic/fault/boot), TX interrupts, driver line
  discipline, a typed console wire protocol — all explicitly out of scope.
