# C1D — rewire shell ↔ child (ST01 → the unified table) + argv; selftest & hello

Phase **C1D** of `doc/plans/17_c1-detail.md` (the detailed decomposition of
parent-plan C1, `doc/plans/0_address_audit_rev0.md:662-679`). C1D is the **last**
of the three rewiring pairs and the **argv headline** (Design Decision 5): the
shell→child bootstrap message moves from the bespoke 13-byte `"ST01"` block
(magic + a one-byte *mode* + the time-page VA) onto the unified `b"EUS1"`
startup-block codec landed by **C1A** (`loader::startup`), carrying a real
**argv** vector instead of a mode byte. `selftest` reads its termination mode
from `argv[1]`; `hello` retires its `b"startup:hello"` magic-string check and
decodes the real block. No Verus/TLA/ledger/spec changes (tally stays **14**).

C1D landed against **C1A only** — it is independent of C1B (init↔storaged, PR
#167, landed) and C1C (init↔shell, in flight): the three pairs are different
per-channel bootstrap messages and migrate atomically and independently. The
shell's *consumer* side of init's `SH01` block is untouched (that is C1C); the
shell keeps decoding `SH01` from init and now *encodes* `EUS1` to its children.

## What landed

- **`user/shell/src/main.rs`** — `build_child_block(out, time_va, argv) ->
  Result<usize, startup::EncodeError>`, a pure, host-testable producer: a `TIME`
  `REGION` grant for the pre-mapped time page (`len = TIME_LEN = 4096`, matching
  init's C1B convention) + the command-line argv, encoded via the shared codec.
  Total in the producer direction (rev1§2.7): over-arena (`> MAX_ARGV`) or
  over-budget (`> MAX_BLOCK`) returns a clean `Err`, never a panic or silent
  truncation. `env` is left empty — carried and round-tripped, unpopulated (DD5).
- **`user/shell/src/runtime.rs`** — `run_once`/`spawn_inner` thread `argv:
  &[&[u8]]` instead of `mode: u8`; the inline 13-byte `"ST01"` build is replaced
  by `build_child_block` + `chan_send(&block[..n])`. New `RunErr::Startup`
  (`b"error: startup block rejected\n"`) maps an encode failure to a clean spawn
  error. `cmd_run` builds argv from the command line (whitespace-split, empties
  dropped; `argv[0]` is the path); `cmd_runloop` passes `argv = [path]` (no mode
  → the child defaults to mode 0, preserving the trivial burn-fix child).
- **`user/selftest/src/main.rs`** — `parse_st01`/`St01` → `parse_startup`/`Boot`
  on `loader::startup::decode`: mode is `argv[1]` (decimal → `u8` via `parse_mode`,
  mirroring the shell's old `parse_u64(..) as u8`), the clock is the `TIME`
  region grant's VA. Total over arbitrary bytes (`decode` → `None` → safe default
  mode 0 / no clock). The mode dispatch (`0xFD`/`0xFE`/`0xFF`/else) is unchanged.
  `+ loader` dep.
- **`user/hello/src/main.rs`** — the `b"startup:hello"` check → `startup::decode(
  &buf[..len]).is_some()`. This *fixes* hello: no producer ever sent
  `startup:hello`, so `run bin/hello` used to print `hello-BAD`; it now decodes
  the shell's real EUS1 block and acks. `+ loader` dep.

## The migration (shell→child block)

| | old `"ST01"` (13 B) | new `b"EUS1"` |
|---|---|---|
| time | `time_va: u64` at `[5..13]` | `TIME` `REGION` grant `{ va, len: 4096, pa: 0 }` |
| mode | `mode: u8` at `[4]` | `argv[1]` (decimal byte-string) |
| path | (not carried) | `argv[0]` (conventional) |
| env | (none) | carried empty (`nenv = 0`), round-tripped |

The selftest mode that selects each termination path now arrives as an
*argument*, not a fixed byte field — every QEMU smoke path is preserved.

## Tests & verification (all green)

- **`cargo test --manifest-path user/shell/Cargo.toml`** — 23 tests (the B15B
  pure-logic suite + 6 new `build_child_block` tests): encode→`loader::startup::
  decode` round-trip of the TIME grant + argv, over-arena refusal (`9 > MAX_ARGV`),
  over-budget refusal (300-byte argv > 256), a negative-control "oracle has teeth"
  assert, and a round-trip proptest (any `u64` VA, any `u8` mode as decimal
  argv[1]), `cfg!(miri){4}else{256}` cases.
- **`cargo test --manifest-path user/selftest/Cargo.toml`** — 7 tests (the B15C
  ST01 suite ported to the EUS1/argv form): full block → `(mode, time_va)`,
  no-mode-arg → mode 0, no-TIME-grant → clockless, malformed (incl. the retired
  `ST01` magic and a bad-magic EUS1) → safe default, `parse_mode` truncation
  parity, totality proptest, round-trip proptest. The codec is shared, so the
  test drives the real `encode` the shell emits — no mirrored hand-parser.
- **`cargo test -p loader`** — **unchanged** (12 startup + elf/layout/fuzz tests);
  C1D touches no loader file.
- **aarch64 cross-build** — `cd kernel && cargo build` links the full `user/*`
  stack (shell/selftest/hello compiling `loader::startup`), no new warnings.
- **Integration smoke** — `scripts/run-demo.sh` under the CLAUDE.md timeout
  harness boots green (storaged mounts and serves: `write`/`cat`/`ls`/`df`
  behave), then the argv-carried mode drives every path:
  - `run bin/selftest 42` → `exited(42)`
  - `run bin/selftest 255` → `faulted(translation, 0xdead0000)` (mode 0xFF)
  - `run bin/selftest 254` → `panicked` (mode 0xFE)
  - `run bin/selftest 253` → `time-ok` → `exited(0)` (mode 0xFD — **the TIME
    region grant rode the EUS1 block**, the time-grant witness)
  - `runloop bin/selftest 50` → `runloop: 50/50 ok, slots 56/56` (argv=[path],
    mode 0, slot recycling intact)
  - `run bin/hello` → `[hello] child alive in its own aspace` → `exited(0)`
  No panic/`Corrupt`.

## Notes

- **argv semantics widened slightly, no smoke regression.** The old `cmd_run`
  used `splitn(2, ' ')`, so `run p a b` parsed the mode from `"a b"` (non-digit →
  0). The new `cmd_run` whitespace-splits the whole tail into argv, so `run p a b`
  → `argv = [p, a, b]` and selftest reads `argv[1] = a`. The smoke only ever
  passes a single mode token, so behaviour there is identical; the new form is the
  faithful argv the plan asked for.
- **`parse_mode` is wrapping** (not the shell's debug-panicking `parse_u64`) — it
  decodes an *untrusted-shaped* argv string in `_start`, so it must be total
  (rev1§2.7). For the `0..=255` modes used it is identical to `parse_u64(..) as u8`.
- **No spec/ledger edits** in C1D. The two §5.1/§8.3 edits the parent plan names
  land with C1C (the "standard names resolve" headline / the final pair). C1D adds
  no new fuzz target — the codec is already fuzzed by C1A; the producer/consumer
  here are exercised by host round-trips against that codec.
- **Out of scope (recorded):** init↔shell `SH01` (C1C), env population, a real
  env source, `stdin`/`stdout` population (C-M9), grandchild storage sessions
  (DD3).
