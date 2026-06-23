# C-M9-B — init wiring: real-boot PL011 grant + spawn the console + wire the console↔shell channel

Implements sub-phase **C-M9-B** of `doc/plans/19_cm9-detail.md` (the userspace
PL011 console track, closing audit S-8 / M-9). C-M9-A had landed the `user/console`
driver binary and its host tests (PR #174) but nothing spawned it; C-M9-B brings it
into the running system. **Trusted-shell only — no kcore/verified edit** (the
`DebugGetc` deletion is C-M9-C): `cargo verus verify -p kcore` is **389 verified, 0
errors**, unchanged, and the `external_body`/`assume_specification` tally is
untouched.

> **Scope note — the shell's *input* moved onto the channel here, not in C-M9-C.**
> The plan's A→B→C split assumed the shell could keep using the `debug_getc`
> scaffold after the console spawns. That is **impossible**: the moment the driver
> enables PL011 RX and drains the FIFO, it starves `debug_getc` (they share the one
> UART). CI's `scripts/spawn-test.sh` — which pipes shell commands and waits for the
> shell to run them — proved this by hanging. So C-M9-B had to absorb the
> **input half** of C-M9-C's shell rewiring: the REPL now reads keystrokes from the
> `stdin` channel. The **output half** (echo/`out` → `chan_send(stdout)`) and the
> syscall retirement (`DebugGetc` removal, `DebugPutc`/`DebugWrite` gating, spec
> closeout) remain C-M9-C — they are CI-independent.

## What landed

1. **Un-gated the kernel boot grant** (`kernel/src/main.rs`). Real `user/init` now
   holds the PL011 MMIO frame (`0x0900_0000`, `READ|WRITE|PHYS`) and IRQ-handler cap
   (`CapKind::Irq(irq::pl011_objid())`, `READ|WRITE`) at cspace slots **62/63**,
   resolving the deferral B-IRQ-C recorded in that file. A new
   `#[cfg(not(feature = "m1-test"))]` block writes 62/63; the existing
   `#[cfg(feature = "m1-test")]` block keeps writing 23/24 for the embedded EL0
   exerciser. The two `cfg`s are mutually exclusive, so each build writes only the
   slots its init consumes and the green m1-test path is byte-identical.
2. **init spawns the console before the shell** (`user/init/src/main.rs`). It
   creates a bootstrap channel + the console↔shell channel, `cap_copy`s + maps the
   PL011 frame into the driver's aspace, hands it a bare wake notification and the
   delegated IRQ cap, sends a startup block carrying the MMIO VA as a
   `NAME_PL011_MMIO` region grant, and starts it at priority 6.
3. **init populates `stdin`/`stdout`** — both name the one console-channel endpoint
   the shell holds (rev1§5.1 "same channel under both names").
4. **The shell reads input from the `stdin` channel** (`user/shell/`). A new
   `resolve_stdin_slot` resolves the endpoint from the startup table; an `Stdin`
   buffer recvs console messages and hands the REPL one byte at a time — the exact
   `debug_getc` shape the loop already consumed. An unbound `stdin` is now **fatal**
   (no silent ambient fallback — the driver would have stolen the FIFO anyway). The
   shell's *output* and echo still use `debug_write`/`debug_putc` for now (a single
   TX writer — unchanged output behaviour); C-M9-C moves them onto the channel.

## Slot layout (Design decision 4 — the load-bearing sign-off)

Measured LOAD-segment counts (`llvm-objdump -p`): storaged 3 → init spawn scratch
20–26; shell 3 → 40–46; console 2 → its own scratch 50–55. So init's free regions
are slot 19, 27–39, 47–61, 62–63. Chosen layout:

| Purpose | init slot(s) | notes |
|---|---|---|
| PL011 MMIO frame (kernel grant) | `CONSOLE_FRAME = 62` | contiguous top pair, clear of every spawn range by construction |
| PL011 IRQ cap (kernel grant) | `CONSOLE_IRQ = 63` | |
| console bootstrap channel | `CON_BOOT_A = 30` / `CON_BOOT_B = 31` | init keeps A to send the block; B → console slot 0 |
| console↔shell channel | `CON_A = 32` / `CON_B = 33` | A → console slot 1; B → shell slot 6 |
| console wake notif | `CON_NOTIF = 34` | → console slot 2 |
| PL011 frame copy | `CON_FRAME_COPY = 35` | mapped into the console aspace at `PL011_VA = 0xA000_0000` |
| console spawn scratch | `CON_SPAWN_BASE = 50` | window 50–55 (2 segs), ≥3-slot margin above shell's 46, clear of the 62/63 grant |

The working slots (30–35) sit ≥3 slots above storaged's scratch top (26); the spawn
window (50+) sits ≥3 slots above the shell's scratch top (46). Tight-but-margined
packing, consistent with init's existing hand-packed cspace style; the **grant**
slots get the by-construction safety the plan called for.

**Console child cspace** (size 8, what its `_start` reads at fixed slots): 0 =
bootstrap channel, 1 = console↔shell channel, 2 = wake notif, 3 = PL011 IRQ cap —
the first real-boot use of an IRQ cap (B-IRQ delivered the mechanism; C-M9 is its
first non-timer consumer).

**Console priority = 6** — above storaged (5) and the shell (4), so an
interrupt-driven keystroke preempts in-progress server work and reaches the shell
promptly. The driver blocks on its reactor otherwise, so it cannot starve them.

## Corrections vs the exploration / plan

- **Shell endpoint slot is 6, not 3.** An exploration draft suggested installing the
  shell's console endpoint at cspace slot 3, but the shell carves `EVENT_NOTIF` (slot
  3) and `DONATION` (slot 4) from its pool at startup (`user/shell/src/runtime.rs`).
  Slots 6,7 are the genuinely free ones (5 = re-grantable time cap, 8+ = spawn
  window). `SHELL_CONSOLE_SLOT = 6`.

## Four issues C-M9-B surfaced (and fixed)

The first three were latent because C-M9-A produced the console ELF but never
*spawned* it, so the spawn path and the live RX path were first exercised here.

1. **The console linked at the wrong address.** Every other `user/*` binary ships a
   `build.rs` + `link.ld` placing it at the rev1§5 process base `0x80000000`; the
   console had neither, so it linked at the toolchain default `0x00200000`. The first
   spawn attempt failed at `spawn::prepare` (`SpawnError::Sys` — `map` of a segment at
   the wrong VA). **Fix:** added `user/console/build.rs` (the storaged-style form,
   gated on the `-none` target and scoped to `-bins` so the host-test link is
   untouched) and `user/console/link.ld` (verbatim the shell/storaged script). After
   the fix the console links at `0x80000000` (2 LOAD segments) and its host tests
   still pass.
2. **A `PERM_DEVICE` mapping needs the frame cap's PHYS right.** The plan reasoned
   the console needs no PHYS ("it reads registers through the VA"), so the first cut
   copied the PL011 frame with `RIGHTS_ALL` (READ|WRITE). That made `sys::map(…,
   PERM_DEVICE | PERM_W)` fail (`[init] FAILED: map pl011`): PHYS is the *authority to
   map physical/device memory*, independent of whether the driver ever reads a PA —
   the storaged virtio-mmio map proves it (`DEV_COPY` carries PHYS). **Fix:** copy
   with `RIGHTS_WITH_PHYS` (READ|WRITE|PHYS), matching storaged.
3. **The plan's A→B→C split is wrong: spawning the RX-draining console regresses the
   shell's input.** Once the driver enables PL011 RX and drains the FIFO, the shell's
   still-live `debug_getc` is starved (they share the one UART). `scripts/spawn-test.sh`
   — which pipes commands and waits for the shell to run them — hung. **Fix:** move the
   shell's *input* onto the `stdin` channel now (the input half of C-M9-C's rewiring;
   see the scope note above). With the shell reading from the channel and the console
   the sole RX reader, the race is *gone*, not merely tolerated — `spawn-test` passes.
4. **Bytes typed during boot were lost (a deadlock).** `run-demo.sh` pipes all input
   up front; QEMU buffers it into the PL011 FIFO *before* the console enables RX. The
   level-crossing RX interrupt fires only for bytes arriving *after* the unmask, so a
   pre-filled FIFO never woke the driver — and once full it back-pressured QEMU, so no
   command ever reached the shell. **Fix:** the console drains the FIFO **once at
   startup** (before the serve loop) and acks; the ack-unmask cycle then catches
   everything that follows. A no-op when nothing was typed during boot, so it leaves
   the `spawn-test` path (input fed after the prompt) untouched.

## State after C-M9-B (no racy caveat)

Input is now **correct, not racy**: the console is the sole RX reader and the shell
consumes keystrokes from the channel, so `run-demo.sh` is fully interactive (`cat`
returns what `write` stored, etc.). What remains for **C-M9-C** is purely the *output*
side and the syscall retirement — the shell still emits its prompt/echo/results via
`debug_write`/`debug_putc` (a single TX writer, behaviour unchanged from before this
phase), `DebugGetc` is now unused-by-the-shell but still present, and
`DebugPutc`/`DebugWrite` are still ungated. None of that affects CI.

## Verification

- `cargo verus verify -p kcore` → **389 verified, 0 errors** (unchanged; no `kcore/`
  edit anywhere in the phase).
- `cargo test --manifest-path user/init/Cargo.toml` → 5 passed (updated
  `stdin`/`stdout` assertions + new `console_block_carries_the_pl011_region`).
- `cargo test --manifest-path user/shell/Cargo.toml` → 26 passed (new
  `resolve_stdin_slot` golden/absent assertions).
- `cargo test --manifest-path user/console/Cargo.toml` → 5 passed (the gated build.rs
  and the startup drain leave the host-tested register layer untouched).
- `cd kernel && cargo build` (and `--features m1-test`) link every `user/*` binary
  including `user/console`.
- **`scripts/spawn-test.sh`** (the CI gate that first caught the regression) — **PASS**:
  runloop 100/100 with full slot reclaim, exit-status propagation, the fault and panic
  demos, the time grant, no BSS-LEAK, no unexpected PANIC. Input flows
  stdin → PL011 RX → console IRQ → console → `chan_recv(stdin)` → shell.
- **`scripts/run-demo.sh`** (process-group timeout harness) — green boot **and fully
  interactive**:

  ```
  [init] system up
  [console] serving
  [storaged] store mounted
  [storaged] serving
  Eunomia shell - type help
  eunomia> write docs/smoke hello
  ok
  eunomia> cat docs/smoke
  hello
  eunomia> ls docs
  readme  (53 bytes)
  smoke  (5 bytes)
  eunomia> df
  chunk region: 32136 used / 66019960 free of 66052096 bytes
  ```

  No panic / `Corrupt` / fault; `cat` returns what `write` stored — input crossing the
  console channel end to end.

## Out of scope (carried to C-M9-C)

The shell's *output*/echo move onto `chan_send(stdout)`; the `DebugGetc` decode-arm
deletion (the one verified-surface touch) now that nothing calls it; gating
`DebugPutc`/`DebugWrite` behind a `debug-log` feature; the no-console negative control
as a real gate; and the spec/ledger closeout — all C-M9-C. (C-M9-B already pulled the
input half of the shell rewiring forward; see the scope note at the top.)
