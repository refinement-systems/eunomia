# C-M9-B — init wiring: real-boot PL011 grant + spawn the console + wire the console↔shell channel

Implements sub-phase **C-M9-B** of `doc/plans/19_cm9-detail.md` (the userspace
PL011 console track, closing audit S-8 / M-9). C-M9-A had landed the `user/console`
driver binary and its host tests (PR #174) but nothing spawned it; C-M9-B brings it
into the running system. **Trusted-shell only — no kcore/verified edit** (the
`DebugGetc` deletion is C-M9-C): `cargo verus verify -p kcore` is **389 verified, 0
errors**, unchanged, and the `external_body`/`assume_specification` tally is
untouched.

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
   the shell holds (rev1§5.1 "same channel under both names"). The shell holds the
   endpoint from C-M9-B but still does terminal I/O over the debug scaffold until
   C-M9-C flips it onto the channel.

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

## Two bugs C-M9-B surfaced (and fixed)

Both were latent because C-M9-A produced the console ELF but never *spawned* it, so
the spawn path was first exercised here.

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

## Intermediate-state caveat (by design)

Once the console binds the PL011 IRQ and drains the RX FIFO, it competes with the
shell's still-live `debug_getc` for the same UART, so interactive *command execution*
is racy in C-M9-B. This is the planned consequence of the strict A→B→C pipeline and
is healed in C-M9-C (the shell moves off `debug_getc` onto the channel). C-M9-B's
acceptance is therefore **green boot only**, not working command echo.

## Verification

- `cargo verus verify -p kcore` → **389 verified, 0 errors** (unchanged; no kcore edit).
- `cargo test --manifest-path user/init/Cargo.toml` → 5 passed (updated
  `stdin`/`stdout` assertions + new `console_block_carries_the_pl011_region`).
- `cargo test --manifest-path user/console/Cargo.toml` → 5 passed (the new gated
  build.rs does not break the host-test link).
- `cd kernel && cargo build` links every `user/*` binary including `user/console`.
- **QEMU smoke** (`scripts/run-demo.sh`, CLAUDE.md process-group timeout harness) —
  green boot:

  ```
  [init] wiring the system
  [init] system up
  [console] serving
  [storaged] virtio-blk up
  [storaged] store mounted
  [storaged] serving

  Eunomia shell - type help
  eunomia>
  ```

  No panic / `Corrupt` / fault. The console serves, storaged mounts, the shell
  prompts. Command echo is the documented racy intermediate state until C-M9-C.

## Out of scope (carried to C-M9-C)

The shell rewiring onto the channel, the `DebugGetc` decode-arm deletion (the one
verified-surface touch), gating `DebugPutc`/`DebugWrite` behind a `debug-log`
feature, the no-console negative control as a real gate, and the interactive QEMU
smoke as the headline acceptance — all C-M9-C.
