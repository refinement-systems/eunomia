# C-M9-A findings — the userspace PL011 console driver (`user/console`)

Phase **C-M9-A** (`doc/plans/19_cm9-detail.md`), the foundation of the C-M9 console track:
the net-new `user/console` binary and its host-tested PL011 register layer, built and
proptest/Miri-tested **in isolation** before any init wiring (C-M9-B) or shell rewiring
(C-M9-C). Branch `cm9a-console-driver`, based on `origin/main` @ `cc3a0bd`.

C-M9-A is a **pure addition**: the driver is built and linked into the aarch64 image set but
**not spawned** at boot, so existing boot behaviour is byte-identical. No kcore / Verus / TLA /
spec / ledger edit lands in A (those are C-M9-C). Design decisions 1, 2, 3 are resolved here.

## What landed

1. **`user/console` — the new driver binary** (`user/console/Cargo.toml`, `src/main.rs`,
   `src/pl011.rs`). Its own mini-workspace, the storaged template shape: `_start` recvs the
   startup block on the bootstrap channel, decodes the `b"EUS1"` block, extracts the PL011 MMIO
   VA from a `REGION` grant (`NAME_PL011_MMIO`), `irq_bind`s the PL011 IRQ cap to a wake
   notification, enables RX interrupts, and runs a one-`reactor.wait()` serve loop that
   forwards RX→shell (`chan_send` + `irq_ack`) and shell→TX (`chan_recv` + polled `write_tx`).

2. **The host-tested register layer** (`user/console/src/pl011.rs`, Design decision 3). A
   `Pl011` MMIO seam (`read`/`write`), a real volatile `MmioWindow`, and pure functions:
   `enable_rx_interrupts` (clear `UARTICR`, set `UARTIMSC` `RXIM|RTIM` — the kernel UART never
   wrote these; it was poll-only), `drain_rx` (read `DR` while `!FR.RXFE`, bounded by the
   buffer), `write_tx` (spin `FR.TXFF`, write `DR`). Five host tests over a fake PL011 with an
   injectable RX FIFO + TX capture + TXFF countdown — incl. a negative-control proving the
   RX-order oracle has teeth.

3. **`NAME_PL011_MMIO = 18`** (`loader/src/startup.rs`), beside `NAME_VIRTIO_MMIO`/`NAME_DMA`.
   Pure data — `encode`/`decode` are generic over the name byte; no codec or test change.

4. **Build wiring** (`kernel/build.rs`): a `user/console` rerun entry, a
   `build_user(.., "console", "console", &[])` call, and a `CONSOLE_ELF_PATH` env var passed to
   the init build (consumed by init only in C-M9-B; an unread env var in A — boot-neutral).

## Deviations from the literal plan (`doc/plans/19`), both to match codebase convention

- **`src/pl011.rs` module, not `src/lib.rs`.** No user binary uses a lib/main split; the
  host-testable layer lives in a module of the binary crate (`mod pl011;`) with its own
  `#[cfg(test)] mod tests`, reachable because the crate builds as a std harness under
  `cfg(test)` (the storaged pattern). No lib target is needed.
- **No heap (no `urt`).** The driver allocates nothing: `ipc` is alloc-free without the `wire`
  feature, and `loader` (default-features-off) decodes into fixed arenas. Deps are `ipc` +
  `loader` only — the `user/hello` precedent. (storaged carries `urt` only because
  `cas`/`DmaPool` need `alloc`.)

## Design notes worth keeping

- **Reactor bit ordering (collision-free by construction).** `register_bound(RX_BIT=1<<0,
  IRQ_KEY)` is called **before** `register(SHELL_CHAN, READABLE, CHAN_KEY)`: the bound claim
  takes bit 0, and `register` then auto-allocates the lowest *clear* bit (bit 1). The same
  `RX_BIT` is `irq_bind`'s `bits` argument — the kernel signals it, the reactor dispatches it
  to `IRQ_KEY`. `irq_bind` runs **before** enabling the line so an early keystroke can't reach
  an unbound INTID.
- **`Pl011::read(&mut self)`.** Read takes `&mut self` so the fake pops its RX FIFO / advances
  its TXFF model with no interior-mutability ceremony; a `&mut` volatile read is standard and
  Miri-clean. `_start` already holds `let mut regs`.
- **The fake's TXFF countdown.** `with_txff_full_for(k)` makes `read(FR)` report `TXFF` set for
  `k` polls then clear — so the spin-on-full TX test makes progress and cannot hang.
- **`_start` is QEMU-only.** The IRQ bind, reactor wait, and real MMIO are not host-reachable;
  they are the C-M9-B/C integration gate. In A the glue need only compile and link.

## Verification

- `cargo test --manifest-path user/console/Cargo.toml` → **5 passed, 0 failed**. Flipping the
  negative control's `assert_ne!`→`assert_eq!` (the deliberately-wrong oracle) **fails** as
  designed (`left [97,98,99,100]` ≠ reversed `right [100,99,98,97]`), then reverted.
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --manifest-path user/console/Cargo.toml`
  → **5 passed**, UB-clean (the user mini-workspace runs under Miri; `cfg!(miri)` trims proptest
  to 4 cases; no test touches `MmioWindow` or a real syscall).
- `cd kernel && cargo build` → green; the new ELF
  `target/user/aarch64-unknown-none-softfloat/release/console` is produced (statically-linked
  ARM aarch64). The pre-existing kcore unused-import warnings are unchanged.
- `cargo verus verify -p kcore` → **389 verified, 0 errors** (unchanged — no kcore edit in A).
- `cargo test -p loader` → all startup-codec tests pass (the additive const broke nothing).
- `cargo fmt -- --check` clean for both the `user/console` mini-workspace and the root.
- **QEMU boot smoke unaffected** (`scripts/run-demo.sh` under the CLAUDE.md timeout harness):
  `[storaged] store mounted` → `serving`, the shell runs `write`/`sync`/`cat`/`ls`/`df`
  (`cat docs/smoke` → `hello`), no panic/`Corrupt`. The shell still uses the debug-UART path
  (correct until C-M9-C); the console is built but not spawned.

## What C-M9-A does **not** do (deferred, not a gap)

- **C-M9-B**: un-gate the real-boot PL011 grant into init's free cspace slots
  (`kernel/src/main.rs`), spawn the console, create + wire the console↔shell channel, populate
  `stdin`/`stdout`.
- **C-M9-C**: shell onto the channel, remove `DebugGetc`, gate `DebugPutc`/`DebugWrite` behind
  `debug-log`, spec/ledger closeout. The interactive QEMU smoke (real keystroke delivery) is the
  B/C gate.
