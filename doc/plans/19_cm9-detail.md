# Plan — Part C-M9 detail: the userspace console UART driver (a new `user/console` binary holding the PL011 **IRQ-handler cap** (B-IRQ) **and** MMIO frame cap, delivering RX keystrokes and accepting TX bytes over **one bidirectional channel** that init grants the shell under the C1-reserved `stdin`/`stdout` standard names (rev1§5.1) — the "console cap" of rev1§7; then moving the shell off `sys::debug_getc`/`debug_putc`/`debug_write` onto that channel, landing the real-boot PL011 grant B-IRQ-C deferred (un-gating `kernel/src/main.rs`'s `#[cfg(feature = "m1-test")]` boot grant into real init's cspace), and retiring the EL0 debug-UART syscalls for the user-facing path — closing audit **S-8** and resolving **M-9 [high]**. The driver is userspace tooling at the rev1§6 Baseline tier: **no Verus, no TLA, no new trusted seam** (ledger tally unchanged); the one verified-surface touch is deleting the `DebugGetc` decode arm, which re-establishes `kcore::sysabi::decode` totality at ≥ 389/0.)

Detailed, separately-implementable decomposition of **Phase C-M9** from
`doc/plans/0_address_audit_rev0.md` (parent-plan C-M9 at `:681-712`). C-M9 is the **Wave-5**
deliverable that the whole console track has been building toward: it is the **driver-and-shell
rewiring on top of** the kernel IRQ object **B-IRQ** delivered and the named-grant table **C1**
delivered. It depends on **both** prerequisites, and both have landed and are green
(`git log`: B-IRQ-A/B/C merged in PRs #144/#145/#146; C1A–C1D merged in #166/#167/#168/#169) — so
C-M9 is now **unblocked**. The parent plan's framing holds exactly: "the heavy kernel lifting is in
B-IRQ; this phase is the driver + shell rewiring on top of it" (`:711-712`).

The framing that shapes the whole phase: C-M9 is the moment the system's **user-facing I/O stops
being ambient**. Today every interactive byte crosses the EL0 **debug-UART syscalls** — an
ambient, capability-free path the spec sanctions only as a **time-boxed bring-up scaffold**
(rev1§7 `:431-433`, rev1§2.7 `:135`) whose **exit condition is precisely the two prerequisites now
in place**: "the device-interrupt-to-notification path a receive side needs (§3.6) and the named-
grant table that delivers the console cap under standard names (§5.1)." C-M9 builds the userspace
PL011 driver that owns those caps, makes the "console cap" a channel to it, and wires that channel
to the shell under `stdin`/`stdout` — converting the standing ambient-authority hole (audit S-8)
into a capability-gated, revocable channel. After C-M9 the shell does **all** terminal I/O over a
delegated channel cap, no EL0 user-facing path touches the kernel UART, and the kernel UART is
demoted to the **kernel-internal** diagnostic path rev1§7 still sanctions ("kept, if at all, only
for kernel-internal panic reporting").

**Honesty notes up front (read first).** C-M9 is **not** behaviour-preserving, and unlike C1 it
touches one corner of the verified surface. Five notes:

1. **C-M9 changes runtime behaviour AND makes one verified-decoder edit — but adds no verified
   item, no trusted seam, no model.** The driver, the channel protocol, the init wiring, and the
   shell rewiring are all userspace + trusted-shell; the **one** verified-surface touch is
   **deleting the `DebugGetc` arm** (opcode 20) from `kcore::sysabi::decode` (`sysabi.rs:183`) so
   the ambient input syscall ceases to exist — `decode` then returns `UnknownCall` for `nr == 20`,
   and its totality `ensures` re-establishes with one fewer arm (the B10B/B-IRQ-B decoder posture
   in reverse). `cargo verus verify -p kcore` stays **≥ 389/0** (a deleted total arm cannot lower
   the count below the existing floor; record the exact number). The `external_body`/
   `assume_specification` tally (ledger) is **unchanged** — C-M9 adds no seam. Everything else
   verified (kcore object core, CAS, IPC, dma-pool, the TLA models) is held **by not touching it**.
   The driver's own correctness is **Baseline tier** (Miri + proptest on its host-testable logic) +
   the **QEMU interactive boot smoke** as the real integration gate (like C1, the format-on-the-
   boot-path is a *real* gate, not a regression check).

2. **C-M9 lands the real-boot PL011 grant that B-IRQ-C deliberately deferred.** B-IRQ-C proved the
   device-IRQ→notification mechanism end to end, but only on the **m1-test** init: the real-boot
   grant is gated `#[cfg(feature = "m1-test")]` (`kernel/src/main.rs:144-167`) because real init's
   64-slot cspace is hand-packed and the chosen slots 23/24 collide with storaged's spawn scratch
   (`SD_SPAWN_BASE = 20`), and because **there was no consumer for the caps until the console driver
   existed** (`main.rs:129-143`, verbatim: "the real-boot grant + the init-cspace restructuring it
   needs land with C-M9"). C-M9 is that consumer: it un-gates the boot grant into free real-init
   slots (Design decision 4) and delegates the caps to the driver. This is a **kernel-shell
   (trusted) change**, not a verified one — the boot grant writes cspace slots exactly as the
   existing device-frame grants do (`main.rs:106-127`).

3. **The driver is interrupt-driven RX + polled TX — it must enable the PL011 RX interrupt the
   kernel never touched.** Today `kernel/src/uart.rs` is **pure polling**: `getc` reads `FR.RXFE`
   and the kernel never writes `UARTIMSC` (the interrupt-mask register, offset `0x38`), so the line
   never fires. The userspace driver must (a) enable RX interrupts in the PL011 (`UARTIMSC.RXIM` +
   the RX-timeout bit), (b) `IrqBind` the PL011 IRQ cap (INTID 33) to a notification (B-IRQ), and
   (c) run the **mask-on-deliver / `IrqAck`-unmask** cycle B-IRQ-B built (`kernel/src/irq.rs:77-114`).
   TX stays **poll-on-`FR.TXFF`** — no TX interrupt at MVP (the driver writes output bytes as the
   shell sends them; a full TX FIFO spins on `TXFF`, the existing `uart::putc` discipline).

4. **The debug-syscall carve-out closes for the user-facing path, not for every boot diagnostic —
   recorded, not silently dropped.** The audit S-8 hole and the rev1§7 carve-out are about the
   **user-facing console** (the shell). C-M9 closes that fully: the shell uses the channel, and the
   ambient **input** syscall (`DebugGetc`) is **removed** (note 1). But the EL0 **output** syscalls
   (`DebugPutc`/`DebugWrite`) carry pre-console **server boot diagnostics** (`[storaged] store
   mounted`, `[init] system up`) that happen *before any console exists* and have nowhere else to
   go at boot. C-M9 **re-scopes** those two from "user-facing scaffold" to a **disclosed,
   build-gated kernel-diagnostic path** (the rev1§7 "kept, if at all, only for kernel-internal"
   clause), used by no user-facing code. Fully routing server diagnostics through the console (and
   removing all three syscalls) is the stricter end-state — recorded out of scope (Design decision 5).

5. **The `cargo fmt` workspace-split trap applies, and there is a new mini-workspace.** `user/console`
   is a **new** `user/*` mini-workspace; it and every other `user/*` file C-M9 touches
   (`user/shell`, `user/init`) format via their **own** manifests
   (`cargo fmt --manifest-path user/console/Cargo.toml`, etc.); `kcore`/`kernel`/`ipc` format via
   the root (CLAUDE.md "Formatting"). Editing `user/console` and running only the root `cargo fmt`
   leaves it untouched.

**Closes (from the parent plan / audit).** Parent plan C-M9 `:681-712`; audit
`doc/results/0_audit_rev0.md` M-9 [high] and S-8:

- **M-9 [high]** — the user-facing console is the ambient debug-UART path; the spec's userspace UART
  driver does not exist. C-M9 builds it (parent plan `:681`, `:699-707`).
- **S-8** — the kernel debug-UART syscalls are an undisclosed standing ambient-authority hole; Phase
  A3 scoped them as a sanctioned, time-boxed M1 scaffold (rev1§7), and C-M9 **retires the user-facing
  path** that motivated the carve-out (parent plan `:682`, `:706-707`).

---

## Spec target — Part A is blessed; C-M9 makes closeout edits on landing, no normative change

Every citation is `rev1§` against the already-blessed text. Like B-IRQ and C1, C-M9 is a
**conformance** phase: rev1§7 already blesses the userspace UART driver as **the** console and the
debug syscalls as a scaffold with a stated exit condition; C-M9 makes that true and **satisfies the
exit condition**. The only spec touches are closeout notes when C-M9 lands (the carve-out's
time-box has now expired); C-M9 makes **no normative spec edit**.

- **rev1§7 — the console** (`spec_rev1.md:431-433`). The normative target, already written as the
  design:
  > *"The user-facing console is a userspace UART driver holding the PL011 IRQ and MMIO caps; a
  > 'console cap' is a channel to that driver, and a shell does all terminal I/O over it, wired under
  > `stdin`/`stdout` (§5.1). This is the sanctioned path … the kernel separately retains a minimal
  > debug-print path to the UART … a deliberate early-bring-up scaffold for the kernel's own
  > diagnostics … The carve-out is time-boxed: once the userspace console driver lands, the debug
  > syscalls are gated off for EL0 — kept, if at all, only for kernel-internal panic reporting —
  > closing the ambient-authority hole."*
  C-M9 makes the first sentence true and triggers the last. **Edit on landing:** a forward note that
  the userspace console driver is implemented as of C-M9; the debug `getc` syscall is removed and the
  `putc`/`write` syscalls are demoted to the build-gated kernel-diagnostic path (Design decision 5);
  the kernel UART (`uart.rs`) is now the kernel-internal panic/fault/boot path only.
- **rev1§5.1 — the console under standard names** (`spec_rev1.md:363`): *"`stdin` and `stdout`
  (deliberately split … an interactive console is the same channel granted under both names)."*
  C1 reserved `NAME_STDIN = 2` / `NAME_STDOUT = 3` (`loader/src/startup.rs:64,66`) and init omits
  them (`user/init/src/main.rs:144-145`); C-M9 **populates** them — init grants the console
  channel's shell-endpoint cap and emits `STDIN → CapSlot(n)` and `STDOUT → CapSlot(n)` with **the
  same `n`** (one bidirectional channel, both names — Design decision 1). This is a **pure
  population step in the C1 format — no format change** (the whole point of C1 reserving the names).
  No text change.
- **rev1§3.6 — IRQ → notification delivery** (`spec_rev1.md:188`): *"IRQ handlers bind identically
  (seL4 precedent) … the lost-wakeup discipline (bind, poll once, then wait) lives in the IPC
  crate."* The driver binds the PL011 IRQ cap to a notification via `IrqBind` (B-IRQ) and
  multiplexes it with the shell channel through the IPC reactor (Design decision 2). No text change;
  C-M9 is the **first non-timer consumer** of the device-IRQ path B-IRQ built.
- **rev1§2.7 — the syscall boundary** (`spec_rev1.md:135`): *"The debug-print scaffold of §7
  occupies opcodes in this space as a disclosed, temporary exception to the capability model of §2 …
  retained only until the userspace console driver replaces it."* C-M9 satisfies "replaced." **Edit
  on landing:** the `DebugGetc` opcode (20) is removed (note 1) and the decode totality
  `ensures` re-established with one fewer arm; the carve-out note's "retained only until … replaced"
  is now historically discharged for input. No discipline change — `decode` stays total.
- **rev1§1 / rev1§2.2 — the IRQ cap is a derivable, revocable device resource**
  (`spec_rev1.md:26,32,48`): init holds the PL011 IRQ + MMIO caps and **delegates** attenuated
  copies to the driver; revoking the grant tears the binding down (B-IRQ's `destroy_irq` releases
  the bound notif's ref). C-M9 exercises this delegation for the first time on the real-boot init
  (note 2). No text change.

---

## What is actually true today — both prerequisites landed; the shell still speaks ambient debug-UART

The inventory that shapes the phase. Two prerequisites are **complete and green**; the console
itself is **entirely unbuilt**, and the shell's I/O is still the scaffold.

### B-IRQ delivered the kernel IRQ object, its delivery path, and its syscalls (verified, 389/0)

- **The cap + the verified object.** `CapKind::Irq(ObjId)` (`kcore/src/cspace.rs:122`) designates an
  `IrqObj { hdr, intid, notif, bits, bound, masked }` (`kcore/src/irq.rs:29-36`); the verified ops
  `irq_bind` (`:196`), `irq_unbind` (`:81`), `destroy_irq` (`:302`) maintain the `irq_binding_refs`
  census term (the timer's twin). `cargo verus verify -p kcore` is **389/0**.
- **The kernel shell.** `kernel/src/irq.rs`: `PL011_INTID = 33` (`:18`, SPI 1 / RX on QEMU virt),
  a boot-static `IRQ_TABLE` (`:30`, `N_SPI = 1` — PL011 only), `pl011_objid()` (`:47-49`),
  `irq_for_intid` (`:52-54`), `init()` routes+enables device SPIs at the GIC (`:58-66`),
  `deliver(intid)` looks up the `IrqObj`, **masks the line** (`gic::disable`) and calls the verified
  `notification::signal` on the bound notif, returning a `woke` hint (`:77-90`); `bind`/`ack`
  wrappers (`:94-114`) over the verified ops.
- **The syscalls + libcalls.** `Sys::IrqBind { irq, notif, bits }` = opcode 25, `Sys::IrqAck { irq }`
  = opcode 26 (`kcore/src/sysabi.rs:69-70`, decode `:199-200`, the `nr >= 27 ==> UnknownCall` bound
  `:116`); handlers `kernel/src/syscall.rs:539-560` (IrqBind: resolve the IRQ cap + the notif cap,
  type-check `CapKind::Irq` / `CapKind::Notification` with `WRITE`, call `irq::bind`) and `:563-573`
  (IrqAck: resolve the IRQ cap, `irq::ack`); userspace `ipc::sys::irq_bind` (`ipc/src/sys.rs:225-227`)
  / `irq_ack` (`:231-233`).
- **The GIC + exception path.** `gic::set_route`/`enable`/`disable` (`kernel/src/gic.rs:85-115`);
  `handle_el0_irq`'s device branch routes a bound INTID through `crate::irq::deliver` then EOIs
  (`kernel/src/exceptions.rs:220-228`).
- **The one piece deferred to C-M9.** The real-boot init **does not yet hold the PL011 caps** — the
  boot grant (slot 23 = PL011 MMIO frame `0x0900_0000` READ|WRITE|PHYS, slot 24 =
  `CapKind::Irq(pl011_objid())` READ|WRITE) is `#[cfg(feature = "m1-test")]` only
  (`kernel/src/main.rs:144-167`), with the deferral rationale spelled out at `:129-143`. The
  m1-test path synthesizes the interrupt with `gic::set_pending` (`kernel/src/irq.rs` under
  `m1-test`); the **real RX interrupt is unwired** (note 3).

### C1 delivered the named-grant table and reserved `stdin`/`stdout` (unpopulated)

- **The format.** `loader::startup` (`loader/src/startup.rs`): the `Startup` model (`:128-135`),
  name ids `NAME_STDIN = 2` (`:64`), `NAME_STDOUT = 3` (`:66`), `NAME_TIME = 6`, `NAME_STORAGE = 5`,
  `NAME_ROOT = 1`, plus device names `NAME_VIRTIO_MMIO = 16` / `NAME_DMA = 17` (`:74-76`); kinds
  `KIND_CAP_SLOT = 1` / `KIND_STORAGE_HANDLE = 2` / `KIND_REGION = 3` (`:79-83`); `decode` (`:267`) /
  `encode` (`:351`); the `Startup::grant(name)` lookup (`:215-222`); `MAX_BLOCK = 256` (`:51`).
- **`stdin`/`stdout` reserved, unpopulated.** init's `build_shell_block` (`user/init/src/main.rs:148-168`)
  emits `TIME`(region), `STORAGE`(CapSlot 1), `ROOT`(StorageHandle 0) and **omits** STDIN/STDOUT/TMP
  (`:144-145`); the test asserts `grant(NAME_STDIN) == None` (`:451-476`). **C-M9 populates them.**
- **The shell resolves names from the table.** `resolve_storage_slot`/`resolve_root_handle`/
  `resolve_time_va` (`user/shell/src/main.rs:200-221`); the `_start` decode + resolve
  (`user/shell/src/runtime.rs:681-702`) sets `STORE_SLOT`/`ROOT_HANDLE` atomics (`:82-83`).
  **C-M9 adds `resolve_stdin_slot`/`resolve_stdout_slot` in the same shape.**

### The shell's terminal I/O is still the ambient debug-UART scaffold

- **Input.** The REPL polls `sys::debug_getc()` (`user/shell/src/runtime.rs:721`), `yield`s on
  `ERR_EMPTY`, echoes printable bytes with `sys::debug_putc(b)` (`:742`).
- **Output.** `out(s)` wraps `sys::debug_write(s)` (`:95-96`), used for the prompt and every command's
  output.
- **The syscalls.** `DebugPutc` = opcode 0, `DebugWrite` = opcode 1 (`kcore/src/sysabi.rs:131-132`),
  `DebugGetc` = opcode 20 (`:183`); handlers `kernel/src/syscall.rs:196-213` (putc/write, via
  `uart::Uart`) and `:726-731` (getc, via the polling `uart::getc`); libcalls
  `ipc/src/sys.rs:128-134,322-324`.
- **The kernel UART.** `kernel/src/uart.rs`: base `0x0900_0000` (`:5`), `DR` `0x00` (`:7`),
  `FR` `0x18` (`:8`, `TXFF` bit 5 `:9`, `RXFE` bit 4 `:28`); `putc` poll-`TXFF` (`:18-25`),
  `getc` poll-`RXFE` non-blocking (`:31-41`), `Write` impl `\n→\r\n` (`:43-53`). **No RX interrupt
  is ever enabled** (`UARTIMSC` is never written — note 3).
- **What legitimately stays kernel-internal.** Direct `uart::Uart` writes that are **not** syscalls:
  the panic handler (`kernel/src/main.rs:323-330`), the EL1 fatal handler
  (`kernel/src/exceptions.rs:160-171`), the EL0 fault reporter (`:185-199`), and the boot log
  (`kernel/src/main.rs:54-296`). These are the rev1§7 "kernel-internal panic reporting" path and
  are **out of scope** for C-M9 (Design decision 5).
- **The pre-console server diagnostics that complicate "no EL0 debug syscalls".** `[init] …`
  (`user/init/src/main.rs:172-393`, e.g. `[init] system up` `:386`), `[storaged] …`
  (`user/storaged/src/main.rs:115-309`, e.g. `[storaged] store mounted` `:220`, `serving` `:232`,
  `virtio-blk up` `:213`), `[selftest] …` (`user/selftest/src/main.rs:113-179`), `[hello] …`
  (`user/hello/src/main.rs:18,45`), `[urt] BUG …` (`urt/src/spawn.rs:118`) all use `sys::debug_write`
  and run **before the console exists** (Design decision 5).

### The userspace driver + IPC machinery C-M9 builds on (the storaged template)

- **The driver shape.** storaged (`user/storaged/src/main.rs`): `_start` (`:168`) recvs its startup
  block on the bootstrap channel, decodes the EUS1 block, extracts `REGION` grants by name (init has
  **pre-mapped** each region at its VA — `:153-163`), inits the device, builds an `Endpoint` +
  `Reactor`, `register`s the session channel readable (`:234-249`), and runs a serve loop draining
  messages on each `reactor.wait()` (`:251-302`).
- **The reactor.** `ipc/src/reactor.rs`: `register(source, signals, key)` allocates a notif bit,
  **binds** the channel event to it, and **self-signals** (poll-once, lost-wakeup safe) (`:196-220`);
  **`register_bound(mask, key)`** registers an **externally-bound** source (no bind, no self-signal)
  — **the exact hook for an IRQ-delivered notification** (`:238-254`); `wait()` blocks on
  `notif_wait` and returns `(key, signals)` (`:266-280`). `notif_wait` = opcode 12 / `notif_signal`
  = opcode 11 (`ipc/src/sys.rs:209-215`).
- **The request/reply pattern (shell ↔ storaged).** `request(&Request)`
  (`user/shell/src/runtime.rs:117-135`): encode → `chan_send` (retry on `ERR_FULL` with `yield`) →
  `chan_recv` → decode. The console channel is **simpler** — a byte pipe, not a CAS protocol (Design
  decision 1).
- **Channel creation + endpoint distribution.** init creates a channel with two endpoints via
  `retype(UNTYPED, OBJ_CHANNEL, order, slot_a, slot_b)` (`user/init/src/main.rs:227-229`, the
  storaged session channel `SESSION_A = 10` / `SESSION_B = 11`), then `cap_install`s each endpoint
  into the relevant child's cspace (`:320,377`). **The console↔shell channel is created the same
  way.**
- **Spawning a driver.** init's storaged spawn (`user/init/src/main.rs:244-327`): `spawn::prepare`
  (`:244`), map MMIO/DMA into the child's aspace (`:258,269`), build + send the startup block
  (`:283-303`), `cap_install` the boot/session/notif caps (`:316-326`), `spawn::start` at a priority
  (`:327`). **The console driver is spawned the same way, plus the PL011 frame map + IRQ-cap install.**
- **The build.** `kernel/build.rs` (`:42-78`) builds each `user/*` binary and passes the storaged/
  shell ELF paths to init as env vars. **A new `user/console` joins this list + an `init` env var.**
- **init's cspace map (the slot-pressure constraint, note 2 / Design decision 4).**
  `user/init/src/main.rs:34-57`: kernel-bestowed `UNTYPED=0`, thread `1`, `UNTYPED2=2`,
  `DEVICE_FRAME=3`, `PL031_FRAME=4`, `SELF_ASPACE=5`; init's allocations `6..=18`
  (`SD_BOOT_A=6 … TIME_SH_CHILD=18`); spawn scratch `SD_SPAWN_BASE=20`, `SH_SPAWN_BASE=40`. **Slot 19
  and 28..=39 are free** — the candidates for the PL011 caps (Design decision 4).

---

## Primary files (current line numbers)

- **The new driver.** `user/console/src/main.rs` + `user/console/src/lib.rs` (**new**, the storaged
  mini-workspace shape) — the `_start`, the PL011 register layer (a host-testable `lib`), the IRQ
  bind + reactor multiplex, the byte-pipe serve loop. `user/console/Cargo.toml` (**new**, deps `ipc`,
  `urt`, `loader` default-features-off — the storaged manifest shape).
- `kernel/build.rs` — the user-binary build loop (`:42-78`): add `user/console` to the rerun list,
  a `build_user(root, &user_target, "console", "console", &[])` call, and a `CONSOLE_ELF_PATH` env
  var into the `init` build (the `STORAGED_ELF_PATH`/`SHELL_ELF_PATH` precedent).
- `kernel/src/main.rs` — **un-gate the boot grant** (`:144-167`): write the PL011 MMIO frame + IRQ
  cap into **real init** at free slots (Design decision 4), removing the `#[cfg(feature = "m1-test")]`
  gate and updating the deferral comment (`:129-143`) to "granted as of C-M9." The kernel UART
  (`uart.rs`) and the kernel-internal log sites (`:54-296,323-330`) are **unchanged**.
- `loader/src/startup.rs` — add a device name id for the PL011 MMIO region (e.g.
  `NAME_PL011_MMIO = 18`, beside `NAME_VIRTIO_MMIO = 16` `:74`) so the console's MMIO VA travels as a
  named `REGION` grant; STDIN/STDOUT (`:64,66`) are already reserved (C1). No codec change — a new
  name id is data.
- `user/init/src/main.rs` — the spawn orchestration: add `CONSOLE_FRAME`/`CONSOLE_IRQ` slot consts
  (the free slots, Design decision 4) and `CONSOLE_*` channel/boot consts; create the console↔shell
  channel (`retype(OBJ_CHANNEL …)`, the `:227-229` pattern); spawn the console **before** the shell
  (`:244-327` storaged pattern + map PL011 MMIO + `cap_install` the IRQ cap + the console RX
  endpoint); add `STDIN`/`STDOUT` → `CapSlot(shell-console-endpoint)` to `build_shell_block`
  (`:148-168`, the populate step); install the shell's console endpoint into its cspace.
- `user/shell/src/runtime.rs` — replace the REPL's `sys::debug_getc` (`:721`) / `sys::debug_putc`
  (`:742`) / `out → sys::debug_write` (`:95-96`) with console-channel I/O resolved from the table;
  the `_start` decode (`:681-702`) gains `stdin`/`stdout` resolution.
- `user/shell/src/main.rs` — add `resolve_stdin_slot`/`resolve_stdout_slot` (the `:200-221`
  resolver shape) as host-tested pure helpers.
- `kcore/src/sysabi.rs` — **delete the `DebugGetc` arm** (`:183`, opcode 20) and its decode test;
  re-establish `decode`'s totality `ensures` (note 1, the **one** verified-surface edit).
- `kernel/src/syscall.rs` — delete the `DebugGetc` handler (`:726-731`); **feature-gate** the
  `DebugPutc`/`DebugWrite` handlers (`:196-213`) behind a `debug-log` cargo feature (Design decision
  5). `ipc/src/sys.rs` — delete `debug_getc` (`:322-324`); gate `debug_putc`/`debug_write`
  (`:128-134`) likewise.
- `doc/spec/spec_rev1.md` — the closeout notes on landing (§7 `:431-433`, §2.7 `:135`); **no
  normative change**.
- `doc/guidelines/verus_trusted-base.md` — the ledger: tally **unchanged** (no new seam); record the
  console driver's host tests as a Baselines row (the C1/B15 precedent); note the `DebugGetc` arm
  removed and `kcore` re-verified at its new total (≥ 389/0).
- `scripts/run-demo.sh` — the QEMU integration gate, now driving **interactive** console I/O (note 1
  / Design decision 6): boot green (`[storaged] store mounted` → `serving`), then the **shell prompt,
  echo, and commands cross the console channel**.

---

## Verification tier & baseline (applies to all sub-phases)

C-M9's driver and rewiring are rev1§6 **Baseline** tier (Miri + proptest on host-testable logic) +
the **QEMU interactive boot smoke** as the real gate; the **one** verified-surface touch is the
`DebugGetc` decode-arm deletion (note 1). Five notes so nothing is over- or under-claimed:

- **No new Verus, no TLA, no new seam — but one verified-decoder deletion.** The driver is userspace
  (outside the verified surface, like `storaged`/`loader::elf`); the channel protocol, init wiring,
  and shell rewiring are trusted shell. The **only** verified edit is removing `DebugGetc` from
  `kcore::sysabi::decode`, which **cannot** lower the count below 389 (a deleted total arm) — but it
  **must** re-establish `decode`'s totality `ensures` with one fewer arm. The
  `external_body`/`assume_specification` tally is **unchanged** (no seam added or removed). Every
  other Verus/TLA gate is held **by not touching it**.
- **The driver's host-testable core is the Baseline deliverable.** The PL011 register layer (a
  `no_std` `lib` with an injectable MMIO trait — the storaged `Mmio`/`MmioWindow` precedent,
  `user/storaged/src/main.rs:44-56`), the byte-pipe framing/echo policy, and any RX ring/line buffer
  get **proptest + Miri** on the host (driven against a fake in-memory PL011, the way storaged's
  device logic is host-tested). The IRQ wiring and the actual hardware path are **not** host-testable
  — they are the QEMU integration gate.
- **The QEMU smoke becomes interactive (a real gate, not a regression check).** Like C1, C-M9 is on
  the boot path and changes runtime behaviour — a half-wired console is an unusable system. The
  CLAUDE.md timeout-harness pattern drives scripted commands on stdin; C-M9's acceptance is that
  those bytes now flow **through the console channel** (driver → shell and shell → driver), the shell
  echoes and runs them, and **no panic/`Corrupt`** appears. A **negative control** (the project's
  anti-theater habit): with the console driver **not** spawned, the shell's stdin/stdout names are
  unresolved and the shell must fail-cleanly to a no-console state (or the boot must visibly stall at
  "console not up"), **not** silently fall back to the now-removed `debug_getc`.
- **kcore stays ≥ 389/0 and the boot still boots.** `cargo verus verify -p kcore` re-verifies at ≥ 389
  (record the exact number after the `DebugGetc` deletion); the aarch64 cross-build links **every**
  `user/*` binary including the new `user/console`; `scripts/run-demo.sh` boots green and is now
  interactively usable.
- **The `cargo fmt` workspace-split trap (note 5).** `user/console` (new), `user/shell`, `user/init`
  format via their own manifests; `kcore`/`kernel`/`ipc`/`loader` via the root.

**Baseline to re-establish at end of C-M9:**

- `cargo verus verify -p kcore` **≥ 389/0** (the `DebugGetc` arm removed; `decode` totality
  re-established — record the exact total). `external_body`/`assume_specification` tally **unchanged**.
- `cargo test --manifest-path user/console/Cargo.toml` green (the PL011-layer + byte-pipe host
  tests, including the fake-PL011 RX/TX round-trips and the echo policy).
- `cargo test --manifest-path user/shell/Cargo.toml` green (the `resolve_stdin_slot`/
  `resolve_stdout_slot` helper tests + the existing B15B/C1C logic tests).
- The aarch64 cross-build links every `user/*` binary; **`scripts/run-demo.sh` boots green and is
  interactive** under the CLAUDE.md timeout-harness: `[storaged] store mounted` → `serving`, then the
  shell prompt, echo, and `date`/`ls`/`cat`/`write`/`df`/`run` **all cross the console channel**, no
  panic/`Corrupt`; the no-console negative control fails cleanly.
- The verified-surface gates (cas/ipc/dma-pool/freelist/urt Verus counts, the three TLA models, the
  fuzz corpora + Miri replay) **unchanged** — C-M9 touches none of them.

---

## Design decision 1 — the console transport: one bidirectional channel granted under both `stdin`/`stdout`, carrying raw byte payloads *(resolve in C-M9-A)*

The spec is explicit: "an interactive console is the same channel granted under both names"
(rev1§5.1 `:363`). The question is the channel's *shape* and *payload*.

- **Adopted — one bidirectional channel between the shell and the driver; the shell's endpoint is
  granted under **both** `stdin` and `stdout`; payloads are raw byte buffers (output: shell→driver
  bytes to write; input: driver→shell bytes read).** Concretely:
  1. **One channel, two endpoints.** init `retype(UNTYPED, OBJ_CHANNEL, …, console_end, shell_end)`
     (the storaged-session pattern, `user/init/src/main.rs:227-229`). The **driver** holds
     `console_end`; the **shell** holds `shell_end`. Each endpoint is bidirectional (the storaged
     session proves it: the shell sends `Request` and receives `Response` on one endpoint), so a
     single channel carries both directions: shell→driver output bytes and driver→shell input bytes.
  2. **Both names → the one shell endpoint.** init emits `STDIN → CapSlot(shell_console_slot)` **and**
     `STDOUT → CapSlot(shell_console_slot)` with the **same** slot (rev1§5.1's "same channel granted
     under both names"). The "deliberately split" property (a pipeline wiring one process's `stdout`
     to another's `stdin`) is a **format** capability C1 already delivers (two independent name slots);
     C-M9's interactive console points both at one channel — the spec's exact described case.
  3. **Raw byte payloads, no postcard.** A console is a **byte stream**, not a structured protocol:
     the message payload **is** the bytes (output `chan_send(console, &out_bytes)`, input
     `chan_recv` yields the keystroke bytes). No `serde`/postcard codec, no `Request`/`Response` enum
     — there is nothing to decode beyond "these are bytes." (rev1§3.7's "decoders treat payloads as
     untrusted" has no bite where there is no decode; the message boundary is the only framing, and
     a console driver treats any byte sequence as valid input.) This keeps the driver and the shell
     trivially small and avoids dragging the IPC wire layer onto a byte pipe.
  4. **Echo + line discipline stay in the shell (driver is a raw pipe).** The shell keeps its
     existing REPL structure (read a byte, echo it, accumulate a line, dispatch on `\r`) — only the
     *transport* changes: `debug_getc` → `chan_recv(stdin)`, `debug_putc`/`out` →
     `chan_send(stdout)`. The driver does **no** line editing — it forwards RX bytes to the shell and
     writes shell output bytes to the UART. This is the minimal change (the shell's line logic is
     already host-tested, B15B) and the cleanest division (driver = hardware, shell = policy).
  - **Decisive reasons:** (a) it is the spec's literal model ("same channel … both names"); (b) raw
    bytes make the driver and the rewiring minimal — no new wire types, no codec; (c) keeping echo/
    line discipline in the shell preserves its host-tested logic and means the driver never needs to
    understand terminal semantics.
- **Rejected — a typed console protocol (`Request::Write(bytes)` / `Request::Read` over the IPC
  wire).** More "in keeping" with rev1§3.7's postcard discipline, but a console has no structured
  request space — it is a byte pipe — so the enum is ceremony with no payload diversity, and it drags
  the `ipc::wire` codec (and a fuzz target) onto something that does not decode. Reserve typed
  protocols for servers with real request spaces (storage); the console is not one.
- **Rejected — two separate channels (one for `stdin`, one for `stdout`).** Contradicts rev1§5.1's
  "same channel granted under both names," doubles the endpoints and the init wiring, and buys
  nothing — the one channel is already bidirectional. (The format's name-split exists for *pipelines*,
  which point the two names at *different* channels; the interactive console is the unified case.)
- **Rejected — the message's 4 cap slots / shared memory for the byte stream.** A console is low-
  bandwidth (keystrokes, line output); the 256-byte message payload is ample, and caps/shared memory
  are the wrong tool for a byte trickle.

**Recommendation: one bidirectional channel; the shell's endpoint granted under both `stdin` and
`stdout`; raw byte-buffer payloads; echo/line discipline stays in the shell, the driver is a raw
pipe. Resolve in C-M9-A (the protocol) and wire in C-M9-B/C.**

---

## Design decision 2 — RX delivery & the driver's event multiplex: `IrqBind` the PL011 line + `register_bound` it alongside the shell channel in one reactor *(resolve in C-M9-A)*

The driver must wake on **two** independent sources — a hardware RX interrupt (keystroke available)
and a readable shell channel (output bytes to write) — without polling either. B-IRQ's delivery
primitive and the IPC reactor compose exactly for this.

- **Adopted — the driver binds the PL011 IRQ cap to a notification via `IrqBind`, registers that
  notification with the reactor as an externally-bound source (`register_bound`), registers the shell
  channel readable (`register`), and dispatches both in one `reactor.wait()` loop; on an RX event it
  drains the UART RX FIFO and forwards the bytes to the shell, then `IrqAck`s to unmask.**
  Concretely:
  1. **Bind the IRQ first (B-IRQ).** `ipc::sys::irq_bind(irq_cap_slot, notif_slot, IRQ_BITS)` (opcode
     25, `ipc/src/sys.rs:225-227`) binds the PL011 IRQ cap (INTID 33) to the driver's wake
     notification with a chosen bit. When the line fires, `handle_el0_irq` → `irq::deliver` masks the
     line and `notification::signal`s `(notif, IRQ_BITS)` (B-IRQ, `kernel/src/irq.rs:77-90`). **Bind
     before enabling the line** (step 2) so an early keystroke cannot reach an *unbound* INTID and be
     EOI-and-dropped (`irq::deliver` returns "no receiver" for an unbound line — benign, but a lost
     char); ordering the bind first closes even that window.
  2. **Then enable PL011 RX interrupts (note 3).** The driver writes the PL011's `UARTIMSC` (offset
     `0x38`) to set `RXIM` (RX interrupt) **and** `RTIM` (RX-timeout, so a partial FIFO still
     delivers), after clearing any stale `UARTICR`. The kernel's `uart.rs` never did this (it was
     poll-only), so the driver owns RX interrupt enablement on its MMIO frame.
  3. **Multiplex in one reactor.** The reactor's wake notification is the **same** notif: the driver
     `register_bound(IRQ_BITS, IRQ_KEY)` (`ipc/src/reactor.rs:238-254` — **no** bind/self-signal,
     because the kernel binds the IRQ source) and `register(shell_chan, Signals::READABLE, CHAN_KEY)`
     (`:196-220` — the IPC crate binds + self-signals the channel). One `reactor.wait()`
     (`:266-280`) returns `(IRQ_KEY, …)` on a keystroke or `(CHAN_KEY, …)` on shell output — the
     reactor is built for exactly this (it is how storaged would multiplex if it had a second source).
  4. **The RX path (mask-on-deliver / `IrqAck`-unmask).** On `IRQ_KEY`: drain the RX FIFO — read
     `DR` while `!FR.RXFE` (`uart.rs:28,31-41` logic, in userspace now) — and `chan_send` the bytes
     to the shell (Design decision 1); then `ipc::sys::irq_ack(irq_cap_slot)` (opcode 26) unmasks the
     line (B-IRQ's `irq::ack` → `gic::enable`, `kernel/src/irq.rs:106-114`). The mask-on-deliver /
     ack-unmask cycle (B-IRQ Design decision 2) prevents a level-triggered storm while the driver
     services the FIFO in EL0.
  5. **The TX path (polled).** On `CHAN_KEY`: drain the shell channel (`chan_recv`) and write each
     byte to `DR` after spinning on `!FR.TXFF` (`uart::putc` logic, `uart.rs:18-25`). No TX
     interrupt — a console's TX is low-volume and a brief `TXFF` spin is acceptable at MVP (note 3).
  - **Decisive reasons:** (a) it reuses B-IRQ's **verified** delivery primitive and the IPC reactor's
    **existing** multiplex — the driver writes no new kernel or event-loop machinery; (b) the
    `register_bound` hook exists **precisely** for externally-bound sources like IRQs (the reactor's
    author anticipated this); (c) one wait loop, two keys is the storaged serve-loop shape with a
    second source — a known-good pattern.
- **Rejected — poll the UART from userspace (keep `debug_getc`-style polling, no IRQ).** This is the
  status quo's flaw: polling a console wastes CPU and cannot block, and it is exactly the ambient path
  C-M9 retires. The whole point of B-IRQ was to make RX interrupt-driven; using it is the deliverable.
- **Rejected — two threads (one blocking on the IRQ notif, one on the channel).** Doubles the
  driver's threads and needs cross-thread buffering; the reactor multiplexes both sources on **one**
  thread with no shared state. A single-threaded reactor driver is simpler and matches storaged.
- **Rejected — TX interrupts (`TXIM` + a TX ring).** Correct for a high-throughput UART, but a
  console's output is bursty-small; poll-on-`TXFF` is the existing `uart::putc` discipline and avoids
  a TX ring + a second IRQ binding. Recorded as the upgrade if console TX ever bottlenecks.

**Recommendation: `IrqBind` the PL011 line to the reactor's wake notification, **then** enable RX
interrupts (`UARTIMSC`); `register_bound` the IRQ alongside the `register`ed shell channel; one wait
loop drains RX→shell (then `IrqAck`) and shell→TX (poll-`TXFF`). Resolve in C-M9-A.**

---

## Design decision 3 — the driver's host-testable core: a PL011 register `lib` over an injectable MMIO trait *(resolve in C-M9-A)*

The driver is the only net-new binary, and rev1§6's Baseline tier wants its logic host-tested. But
the IRQ path and real MMIO are not host-reachable. The split decides what gets proptest+Miri and what
is QEMU-only.

- **Adopted — factor the driver into a `no_std`, host-buildable `console` lib (the PL011 register
  layer + the byte-pipe/echo logic) over an injectable MMIO trait, plus a thin `main.rs` `_start`
  that supplies the real MMIO window + the syscalls.** Concretely:
  1. **The MMIO seam.** A `trait Pl011 { fn read(&self, off) -> u32; fn write(&mut self, off, v); }`
     with a real `MmioWindow` impl (volatile reads/writes against the granted VA — the storaged
     `Mmio`/`MmioWindow` precedent, `user/storaged/src/main.rs:44-56`) and a **fake** in-memory impl
     for host tests (a register array + an injectable RX FIFO).
  2. **The host-tested logic.** Over the fake: RX drain (read `DR` while `!RXFE`, stop at empty),
     TX write (spin `TXFF`, write `DR`), `UARTIMSC`/`UARTICR` setup, and the byte-pipe framing
     (output bytes → TX, RX bytes → a `chan_send` buffer). Proptest: arbitrary RX byte sequences
     round-trip to the shell-bound buffer; arbitrary output sequences reach `DR` in order; a full
     fake-TX FIFO is handled (spin then drain); Miri clean (the volatile/pointer arithmetic in the
     real impl is exercised by `cargo test` against the fake, the dma-pool/storaged posture).
  3. **The QEMU-only part.** `IrqBind`/`IrqAck`, the reactor wait, and the real interrupt are **not**
     host-testable — they are the interactive QEMU smoke (Design decision 6). The `_start` glue is
     thin and trusted (the storaged `_start` posture).
  - **Decisive reasons:** (a) it brings the driver's real logic (FIFO drain, TX, register setup) to
    the Baseline bar the spec wants, matching how storaged's device logic is host-tested; (b) the
    injectable-MMIO trait is the established pattern in this tree; (c) it cleanly fences the
    QEMU-only IRQ path so the host tests are deterministic.
- **Rejected — no host tests, QEMU-only.** Leaves the FIFO-drain and TX logic (real off-by-one and
  ordering hazards) covered only by an integration smoke — below the rev1§6 Baseline bar every other
  userspace logic component meets (B15's whole point).
- **Rejected — Verus on the driver.** The driver is userspace tooling outside the verified surface
  (like `storaged`/`loader::elf`); Verus is not the routed tier (rev1§6). No mechanized proof.

**Recommendation: a `no_std` `console` lib (PL011 register layer + byte-pipe logic) over an
injectable MMIO trait, proptest+Miri on the host against a fake PL011; the IRQ/reactor path is the
QEMU smoke. Resolve in C-M9-A.**

---

## Design decision 4 — where the PL011 caps live in real init's cspace (resolve the B-IRQ-C deferral) *(the load-bearing sign-off — resolve before C-M9-B)*

B-IRQ-C deferred the real-boot PL011 grant because init's 64-slot cspace is hand-packed and the
m1-test slots 23/24 collide with storaged's spawn scratch (`SD_SPAWN_BASE = 20`), and because there
was no consumer (`kernel/src/main.rs:129-143`). C-M9 is the consumer; it must place the caps.

- **Adopted — the kernel writes the PL011 MMIO frame + IRQ cap into **two free real-init slots**
  (un-gating the boot grant from `m1-test`), and init delegates them to the console driver at spawn,
  freeing the slots for later reuse.** Concretely:
  1. **The free slots (and the fragility to avoid).** Each `spawn::prepare(image, untyped, base,
     child_cspace_slots)` consumes init-side slots `base` (aspace), `base+1` (tcb), `base+2` (cspace),
     `base+3+i` per ELF segment, and `base+3+nsegments` (stack) — `loader/src/spawn.rs:53-90`. So the
     scratch extent is **segment-count-dependent**: storaged at `SD_SPAWN_BASE = 20` reaches into the
     **mid/high 20s** (the m1-test comment confirms 23/24 are "storaged's first segment frames"), and
     the shell at `SH_SPAWN_BASE = 40` into the **mid/high 40s**. The grant needs **two** slots that
     are clear of **both** ranges *regardless of segment count*, so the genuinely-safe windows are
     **slot 19** (the lone free slot between init's allocation block `6..=18` and `SD_SPAWN_BASE = 20`)
     and the **top of the 64-slot table** (above the shell's scratch — `40 + 3 + nsegments + 1` cannot
     plausibly reach the low 60s). The clean choice is a **contiguous pair at the top**, e.g.
     `CONSOLE_FRAME = 62`, `CONSOLE_IRQ = 63` (clear of every `prepare` base range *by construction*,
     so no segment-count audit is needed). (Do **not** pick the `28..=39` mid-window naively — its low
     end is occupied by storaged's scratch for any storaged ELF with ≥ 6 segments; slot 19 + a
     high slot is the segment-count-robust alternative if a contiguous top pair is undesirable.)
  2. **The kernel boot grant (un-gated).** `kernel/src/main.rs:144-167`: drop the
     `#[cfg(feature = "m1-test")]`, change the slot indices from `23/24` to the chosen pair
     (`62/63`), and update the comment (`:129-143`) to "granted as of C-M9 — delegated to the console
     driver; m1-test retains its own exerciser grant." The cap contents are unchanged (Frame
     `0x0900_0000` READ|WRITE|PHYS; `CapKind::Irq(pl011_objid())` READ|WRITE). The m1-test path keeps
     its slots `23/24` under its own `cfg` (the exerciser's cspace is free there).
  3. **Delegation frees the slots.** init `cap_install`s the PL011 MMIO frame (after mapping it into
     the driver's aspace) and the IRQ cap into the **console driver's** cspace at spawn (the storaged
     `cap_install` pattern, `user/init/src/main.rs:316-326`); the init-side slots `62/63` are then
     spent and reusable. Because the console is spawned **before** the shell (Design decision 6), the
     grant slots do not collide with the shell's spawn scratch.
  - **Decisive reasons:** (a) it reuses the **device-MMIO-frame boot-grant precedent** (slots 3/4,
    `main.rs:106-127`) and B-IRQ's adopted **boot-static** provenance (Design decision 3 there) — no
    new mechanism, no retype, no new seam; (b) two contiguous named slots clear of the spawn ranges
    resolve the exact collision B-IRQ-C flagged; (c) delegation-then-reuse keeps init's cspace within
    its 64 slots.
- **Rejected — keep the grant m1-test-only and have the driver retype its own IRQ object.** Contradicts
  B-IRQ Design decision 3 (boot-static, not retyped) and rev1§1 ("init holds … IRQ caps"); it would
  add the `ExIrqObj` seam + a uniqueness invariant B-IRQ deliberately avoided. The caps are init's to
  delegate, not the driver's to mint.
- **Rejected — restructure init's whole cspace layout (renumber the allocation/spawn blocks).** More
  churn than the collision needs; two free slots already exist. Reserve a renumber for if the device-
  cap set grows (a follow-on adding virtio/RTC IRQ caps).

**Recommendation: un-gate the kernel boot grant into two free contiguous real-init slots
(`CONSOLE_FRAME`/`CONSOLE_IRQ`, e.g. a contiguous top pair 62/63 clear of every spawn range by
construction), init delegates both to the driver at spawn (freeing the
slots). This is the load-bearing sign-off — it resolves the exact deferral B-IRQ-C recorded. Confirm
before C-M9-B.**

---

## Design decision 5 — the debug-syscall retirement: remove the EL0 **input** syscall; re-scope the **output** syscalls to a build-gated kernel-diagnostic path *(resolve in C-M9-C; flag for sign-off)*

The parent plan's acceptance is "no EL0 path uses the kernel debug-UART syscalls" (`:709-710`), and
the work item is "gate/remove the EL0 debug-UART syscalls (closing S-8 for the user-facing path)"
(`:706-707`). rev1§7 wants them "gated off for EL0 — kept, if at all, only for kernel-internal panic
reporting." The complication: the EL0 **output** syscalls also carry **pre-console server boot
diagnostics** (`[storaged] store mounted`, `[init] system up`) that run **before any console exists**
(note 4).

- **Adopted — remove the ambient **input** syscall outright; re-scope the two **output** syscalls to
  a disclosed, build-gated kernel-diagnostic path used only by pre-console server logging; the shell
  (the user-facing console) uses none of them.** Concretely:
  1. **Remove `DebugGetc` (the actual ambient hole).** Input is the dangerous ambient authority — it
     **is** the console (whoever calls `debug_getc` reads the keyboard). Once the driver owns PL011
     RX, no EL0 thread may read it ambiently. So delete the `DebugGetc` decode arm
     (`kcore/src/sysabi.rs:183`, the **one** verified edit, note 1), the handler
     (`kernel/src/syscall.rs:726-731`), and the libcall (`ipc/src/sys.rs:322-324`). The shell's only
     `debug_getc` site (`user/shell/src/runtime.rs:721`) moves to the channel (C-M9-C), so nothing
     calls it.
  2. **Re-scope `DebugPutc`/`DebugWrite` to a `debug-log` build feature.** These carry boot
     diagnostics that **predate** the console (init wires the system and spawns servers before the
     console serves; a failure before the console is up must still be visible). Keep the handlers
     (`kernel/src/syscall.rs:196-213`) and libcalls (`ipc/src/sys.rs:128-134`) but gate them behind a
     `debug-log` cargo feature (default-on for dev images, off for a "production" build), explicitly
     re-labelled in code + rev1§7 as the **kernel-diagnostic** path, not user-facing authority. The
     **shell** uses neither (its `out`/echo move to the channel); the **servers** (init/storaged/
     selftest/urt) keep them as diagnostics under the feature.
  3. **The shell is fully off the debug syscalls.** After C-M9-C the shell's REPL does **all** I/O
     over the console channel — `debug_getc` → `chan_recv(stdin)`, `debug_putc`/`out` →
     `chan_send(stdout)`. The user-facing ambient hole (S-8) is closed: the shell, the one
     user-facing path the spec names, no longer touches the kernel UART.
  - **Decisive reasons:** (a) it closes the **user-facing** hole the audit and rev1§7 actually name
    (the shell's interactive I/O + ambient input), which is S-8's substance; (b) it keeps early-boot
    failures debuggable (a server panicking before the console exists still prints), which fully
    routing through a not-yet-existent console would silence — a real regression; (c) it matches
    rev1§7's "kept, if at all, only for kernel-internal" by re-labelling the output path as kernel-
    diagnostic, not user authority.
- **Rejected — route ALL server diagnostics through the console and remove all three syscalls.** The
  strict end-state, but the console driver must then be the **very first** thing init spawns, and
  init's own pre-spawn log + any failure before the console serves would have **nowhere to go** —
  early-boot diagnostics go silent, a debuggability regression for no MVP gain. Recorded as the
  follow-on (route boot logs through the console once a boot-time log buffer exists). The honest
  split: C-M9 closes the user-facing path; full server-log routing is later work.
- **Rejected — leave all three EL0 syscalls in place (gate nothing).** Leaves the ambient **input**
  hole (S-8) open — the very thing C-M9 exists to close. Removing input is non-negotiable.

**Recommendation: remove `DebugGetc` (the ambient input hole, one verified-decoder edit); gate
`DebugPutc`/`DebugWrite` behind a `debug-log` feature as the disclosed kernel-diagnostic path; the
shell uses the channel for everything. Flag the input-removal-vs-strict-full-removal scope as the
sign-off (this plan recommends the split). Resolve in C-M9-C.**

---

## Design decision 6 — spawn ordering & boot bring-up: console before shell; the no-console negative control *(resolve in C-M9-B)*

The shell's `stdin`/`stdout` must point at a **live** console (its first prompt needs somewhere to
go), and init's own boot log + storaged's diagnostics happen before the console serves. The ordering
and the bring-up gap must be decided.

- **Adopted — init spawns the console driver **before** the shell, wires the console↔shell channel,
  then spawns the shell with `stdin`/`stdout` populated; pre-console diagnostics use the build-gated
  kernel-diagnostic path (Design decision 5).** Concretely:
  1. **Order.** init: wire the system → create the console↔shell channel (`retype(OBJ_CHANNEL …)`) →
     **spawn the console** (map its PL011 MMIO, `cap_install` the IRQ cap + the console RX endpoint,
     send its startup block, start it) → spawn storaged (unchanged) → **spawn the shell** with
     `STDIN`/`STDOUT` → the shell's console endpoint (the C1 populate step). Console-before-shell is
     **required**; console-vs-storaged order is flexible (no dependency between them).
  2. **The bring-up gap.** Between boot and "console serving," init's `[init] …` and storaged's
     `[storaged] …` lines use the `debug-log` kernel-diagnostic path (Design decision 5) — they are
     not user-facing console I/O, and they predate the console. The shell, spawned last, finds a live
     console and never uses the debug path.
  3. **The no-console negative control (anti-theater).** If the console driver is **not** spawned (or
     fails), the shell's `stdin`/`stdout` resolve to nothing; the shell must **fail-cleanly** to a
     visible "console not available" state (via the kernel-diagnostic path) — **not** silently fall
     back to the removed `debug_getc` (it is gone) and **not** hang invisibly. The QEMU smoke runs
     this control: with the console spawn disabled, the boot must visibly report no console, proving
     the shell genuinely depends on the channel.
  - **Decisive reasons:** (a) console-before-shell is forced by the data flow (the shell's prompt
    needs a live sink); (b) the build-gated diagnostic path (Design decision 5) covers exactly the
    pre-console window, so there is no silent-boot gap; (c) the negative control proves the rewiring
    is real (the shell uses the channel, not a hidden fallback).
- **Rejected — spawn the shell first and let it block until the console appears.** Inverts the
  dependency and risks a lost first prompt / a race on channel readiness; spawning the producer
  (console) before the consumer (shell) is the storaged-session discipline (init wires the server
  before the client).
- **Rejected — a kernel boot-log ring the console drains on startup (replay pre-console logs to the
  screen).** A nice-to-have (the user sees the full boot log once the console comes up), but it is a
  new kernel buffer + a drain protocol — out of scope for MVP. Recorded as the follow-on that would
  let Design decision 5 remove the output syscalls entirely.

**Recommendation: console before shell; pre-console diagnostics on the build-gated kernel path; a
no-console negative control in the QEMU smoke. Resolve in C-M9-B.**

---

## Sub-phase C-M9-A — the userspace PL011 console driver *(must-do; the foundation; the new binary + its host-tested logic)*

The net-new `user/console` binary and its host-testable core — built and unit-/proptest-/Miri-tested
**before** any init wiring or shell rewiring, so the driver is proven in isolation. It can be
exercised against the m1-test grant or a fake harness; integration is C-M9-B/C.

- **Touches:**
  - `user/console/Cargo.toml`, `user/console/src/main.rs`, `user/console/src/lib.rs` — **new**: the
    `Pl011` MMIO trait + real `MmioWindow` impl + fake (Design decision 3); the PL011 register layer
    (`UARTIMSC`/`UARTICR` setup, RX drain, TX write); the byte-pipe + echo-free forwarding (Design
    decision 1); the `_start` glue (recv startup block, map MMIO from the named `REGION` grant,
    `IrqBind`, build the reactor with `register`(channel) + `register_bound`(IRQ), serve loop —
    Design decision 2). The storaged `_start`/`Mmio`/serve-loop is the template
    (`user/storaged/src/main.rs:44-56,168-302`).
  - `kernel/build.rs` — add `user/console` to the build (`:42-78`): the rerun entry, the
    `build_user(..., "console", "console", &[])` call, and the `CONSOLE_ELF_PATH` env var into the
    `init` build.
  - `loader/src/startup.rs` — add `NAME_PL011_MMIO = 18` beside the device names (`:74-76`) so the
    driver's MMIO VA travels as a named `REGION` (a data-only addition, no codec change).
- **Depends on:** Part A blessed; B-IRQ + C1 landed (both are). Design decisions 1, 2, 3 signed off.
  **No** intra-C-M9 dependency (C-M9-B/C consume this binary).
- **Work:**
  1. Scaffold the mini-workspace (the storaged manifest + `_start` shape); wire it into
     `kernel/build.rs` so the aarch64 build produces and links the ELF.
  2. The `Pl011` trait + real/fake impls; the register layer (RX FIFO drain, TX poll-write, IMSC/ICR
     setup).
  3. The reactor multiplex (Design decision 2): `IrqBind` the PL011 cap, `register_bound` the IRQ
     bit, `register` the shell channel, one `wait()` loop dispatching RX→`chan_send(shell)`+`IrqAck`
     and channel→TX.
  4. Host tests (Design decision 3): proptest RX byte sequences → shell-bound buffer; output
     sequences → `DR` order; full-FIFO handling; Miri-clean pointer/volatile arithmetic. A **negative
     control**: a deliberately wrong expected RX order fails the oracle.
- **Acceptance:**
  - `cargo test --manifest-path user/console/Cargo.toml` green (PL011-layer + byte-pipe host tests);
    the broken-oracle control fails.
  - The aarch64 cross-build links `user/console` (the new ELF is produced and embedded); the existing
    boot is **unaffected** (the driver is not yet spawned — C-M9-A is a pure addition).
  - `cargo verus verify -p kcore` unchanged (no kcore edit yet); the ledger gains a Baselines row for
    the console host tests.
- **Effort/Risk:** M / medium. The substance is the register layer + the reactor multiplex against a
  fake; the new binary's build wiring is mechanical (the storaged precedent).

---

## Sub-phase C-M9-B — init wiring: real-boot PL011 grant + spawn the console + create & wire the console↔shell channel *(must-do; resolves the B-IRQ-C deferral; populates `stdin`/`stdout`)*

Brings the driver into the running system: un-gate the kernel boot grant into real init, create the
console↔shell channel, spawn the console (delegating the PL011 caps), and populate the C1-reserved
`stdin`/`stdout` names in the shell's startup block. After C-M9-B the console driver runs and the
shell **holds** the console channel (but still uses the debug scaffold — C-M9-C flips it).

- **Touches:**
  - `kernel/src/main.rs` — un-gate the boot grant (`:144-167`): drop `#[cfg(feature = "m1-test")]`,
    move the PL011 MMIO frame + IRQ cap to the free slots (Design decision 4, e.g. 62/63), update the
    deferral comment (`:129-143`) to "granted as of C-M9." The m1-test exerciser keeps its own
    `cfg`-gated slots.
  - `user/init/src/main.rs` — `CONSOLE_FRAME`/`CONSOLE_IRQ` consts (the free slots) + `CONSOLE_*`
    channel/boot consts; create the console↔shell channel (`retype(OBJ_CHANNEL …)`, the `:227-229`
    pattern); spawn the console **before** the shell (Design decision 6) — `spawn::prepare`, map the
    PL011 MMIO into the driver's aspace, `cap_install` the IRQ cap + the console RX endpoint, build +
    send its startup block (a `REGION` for the MMIO VA via `NAME_PL011_MMIO`, the IRQ-cap slot, the
    RX-endpoint slot), `spawn::start` (the storaged spawn `:244-327` template); add `STDIN`/`STDOUT`
    → `CapSlot(shell-console-endpoint)` to `build_shell_block` (`:148-168`) and `cap_install` that
    endpoint into the shell's cspace.
- **Depends on:** C-M9-A (the driver binary). Design decision 4 signed off (the cspace slots).
- **Work:**
  1. Un-gate + relocate the kernel boot grant (Design decision 4); confirm the aarch64 boot still
     comes up with init now holding the PL011 caps.
  2. init creates the console↔shell channel and spawns the console (map MMIO, install IRQ cap + RX
     endpoint, send block, start) **before** the shell; delegate-then-reuse keeps init within 64 slots.
  3. init populates `STDIN`/`STDOUT` in the shell's block (the C1 reserve→populate step) and installs
     the shell's console endpoint.
  4. Verify in QEMU that the console driver reaches "serving" (a `[console] serving` diagnostic via
     the build-gated path) and that the shell's block now carries `stdin`/`stdout` (the shell does not
     yet *use* them — C-M9-C).
- **Acceptance:**
  - `scripts/run-demo.sh` boots green: the console driver spawns and serves, storaged still mounts
    (`store mounted` → `serving`), the shell starts; `cargo verus verify -p kcore` unchanged (the
    boot grant is trusted shell, no kcore edit yet).
  - init holds and delegates the PL011 MMIO frame + IRQ cap (the B-IRQ-C deferral resolved); the
    shell's startup block carries `STDIN`/`STDOUT` → the console endpoint (asserted in init's host
    test, the `:451-476` shape now expecting the two grants).
  - The aarch64 build links every `user/*` binary including `user/console`.
- **Effort/Risk:** M / medium. The judgment is the cspace-slot placement (Design decision 4) and the
  spawn ordering; the spawn/map/install mechanics are the storaged precedent.

---

## Sub-phase C-M9-C — shell rewiring + debug-syscall retirement + closeout *(must-do; the headline "all console I/O over the channel"; closes S-8 / M-9)*

The headline: the shell does **all** terminal I/O over the console channel, the EL0 ambient input
syscall is removed, the output syscalls are re-scoped, the kernel UART is demoted to kernel-internal,
and the spec/ledger close out. Depends on C-M9-B (the channel must exist and be granted).

- **Touches:**
  - `user/shell/src/main.rs` — add `resolve_stdin_slot`/`resolve_stdout_slot` (the `:200-221`
    resolver shape) as host-tested pure helpers.
  - `user/shell/src/runtime.rs` — resolve `stdin`/`stdout` in `_start` (`:681-702`, beside
    storage/root/time); replace the REPL input `sys::debug_getc()` (`:721`) with `chan_recv(stdin)`
    (blocking/`yield` on empty, the `request` recv shape `:117-135`), the echo `sys::debug_putc(b)`
    (`:742`) and `out → sys::debug_write` (`:95-96`) with `chan_send(stdout)`. Echo/line discipline
    stays in the shell (Design decision 1).
  - `kcore/src/sysabi.rs` — delete the `DebugGetc` arm (`:183`) + its decode test; re-establish
    `decode` totality (note 1, the one verified edit).
  - `kernel/src/syscall.rs` — delete the `DebugGetc` handler (`:726-731`); `#[cfg(feature =
    "debug-log")]`-gate the `DebugPutc`/`DebugWrite` handlers (`:196-213`) (Design decision 5).
    `ipc/src/sys.rs` — delete `debug_getc` (`:322-324`); gate `debug_putc`/`debug_write`
    (`:128-134`). The server diagnostics (init/storaged/selftest/urt) build under `debug-log`.
  - `kernel/src/uart.rs` — **no functional change**, but re-comment as the kernel-internal diagnostic
    path (panic/fault/boot only); the kernel-internal sites (`main.rs:323-330`,
    `exceptions.rs:160-199`, `main.rs:54-296`) are untouched.
  - `doc/spec/spec_rev1.md` — the §7 / §2.7 closeout notes (no normative change);
    `doc/guidelines/verus_trusted-base.md` — record the `DebugGetc` arm removed, kcore re-verified at
    its new total (≥ 389/0), tally unchanged.
- **Depends on:** C-M9-B (the channel is granted). Design decision 5 signed off.
- **Work:**
  1. shell: resolve `stdin`/`stdout`; move input to `chan_recv(stdin)`, output + echo to
     `chan_send(stdout)`; keep the line-editing/echo logic (host-tested) — only the transport changes.
  2. Remove `DebugGetc` (decoder + handler + libcall); re-establish `decode` totality; gate the two
     output syscalls behind `debug-log`; build the servers under that feature.
  3. The no-console negative control (Design decision 6): with the console spawn disabled, the shell
     fails-cleanly (no silent `debug_getc` fallback — it is gone).
  4. Spec/ledger closeout; re-run `cargo verus verify -p kcore` and the interactive QEMU smoke.
- **Acceptance:**
  - The shell does **all** console I/O over the channel; **no EL0 user-facing path** uses the kernel
    debug-UART syscalls (`DebugGetc` is gone; the shell calls neither output syscall).
  - `scripts/run-demo.sh` is **interactive**: scripted stdin reaches the shell **through the console
    channel** (driver → shell), the shell echoes and runs `date`/`ls`/`cat`/`write`/`df`/`run`
    (output → console channel → UART), no panic/`Corrupt`; the no-console control fails cleanly.
  - `cargo verus verify -p kcore` **≥ 389/0** (the `DebugGetc` arm removed, totality re-established);
    `cargo test --manifest-path user/shell/Cargo.toml` green (the resolver helpers + existing logic);
    the ledger reflects the removed arm and the unchanged tally; rev1§7/§2.7 carry the closeout notes.
- **Effort/Risk:** M / medium. The care is in the shell's input loop (blocking on the channel vs the
  old poll-and-yield) and getting the `DebugGetc` deletion to re-verify; the syscall gating is
  mechanical.

---

## Execution order

```
C-M9-A  the user/console driver + its host-tested PL011/byte-pipe core   [foundation; new binary; pure addition]
          │
          ▼
C-M9-B  un-gate the real-boot PL011 grant + spawn the console + wire the console↔shell channel
          │     [init/kernel-shell; resolves the B-IRQ-C deferral; populates stdin/stdout]
          ▼
C-M9-C  shell onto the channel + remove DebugGetc + gate DebugPutc/Write + spec/ledger closeout
                [the headline; closes S-8 / M-9; the one verified-decoder edit]
```

- **C-M9-A is the prerequisite** (the binary must exist before init can spawn it). **C-M9-B depends
  on C-M9-A** (the ELF) and resolves the B-IRQ-C cspace deferral (Design decision 4). **C-M9-C
  depends on C-M9-B** (the channel must be granted before the shell can use it). Unlike C1's
  independent pairs, C-M9 is a **strict pipeline** — each sub-phase enables the next — because the
  console is a single new data path threaded end to end (driver → init wiring → shell). Mirrors
  B-IRQ's A(core)/B(wiring)/C(grant+closeout) decomposition.
- The **QEMU interactive smoke** is the gate after C-M9-B (console serves) and the **headline
  acceptance** after C-M9-C (the shell is interactive over the channel). Re-run `scripts/run-demo.sh`
  after C-M9-B and C-M9-C (the console is on the boot path — a half-wired console is an unusable
  system, exactly the C1 landing discipline).
- **The two sign-off gates are Design decisions 4 and 5** — the cspace slots for the PL011 grant (the
  B-IRQ-C deferral) and the debug-syscall retirement scope (input-removal vs strict full-removal).
  Confirm Design decision 4 before C-M9-B and Design decision 5 before C-M9-C.

## Out of scope for C-M9 (recorded so it is not mistaken for a gap)

- **Routing ALL server boot diagnostics through the console + removing the output syscalls.** C-M9
  closes the **user-facing** path (the shell) and removes the ambient **input** syscall; it
  **re-scopes** `DebugPutc`/`DebugWrite` to a build-gated kernel-diagnostic path for pre-console
  server logging (Design decision 5), because those run before any console exists. Fully routing boot
  logs through the console (and removing all three syscalls) needs a kernel boot-log buffer the
  console drains on startup (Design decision 6's rejected branch) — a follow-on, recorded.
- **The kernel-internal UART path (panic/fault/boot).** The direct `uart::Uart` writes
  (`kernel/src/main.rs:323-330`, `exceptions.rs:160-199`, `main.rs:54-296`) are the rev1§7-sanctioned
  "kernel-internal panic reporting" path and are **kept unchanged** — C-M9 demotes the *EL0* UART
  access, not the kernel's own.
- **Grandchildren's console (the shell's children).** The shell's `run`-spawned children
  (selftest/hello) get **no** console channel — exactly as they get no storage session (C1 Design
  decision 3). Their diagnostics stay on the build-gated kernel path; a child's own `stdin`/`stdout`
  (a pipeline) needs the shell to grant a channel under those names, which the C1 format supports but
  C-M9 does not wire (the interactive shell is the one console consumer at MVP).
- **TX interrupts / a TX ring.** The driver's TX is poll-on-`TXFF` (Design decision 2); a `TXIM`-driven
  TX ring is the upgrade if console output ever bottlenecks — recorded, not built.
- **Line discipline in the driver (canonical mode, history, completion).** The driver is a **raw byte
  pipe** (Design decision 1); echo, line editing, and the prompt stay in the shell (its host-tested
  logic). A terminal line-discipline layer (cooked mode, `^C`/`^D`, history) is shell/console policy
  for later, not C-M9's mechanism.
- **A typed console wire protocol / the IPC postcard codec on the byte stream.** The console carries
  raw bytes (Design decision 1); it is not a structured-request server, so it gets no `Request`/
  `Response` enum and no fuzz target (there is nothing to decode). Reserve typed protocols for servers
  with real request spaces (storage).
- **Retiring the virtio-blk poll (B2/I-4) via the now-built device-IRQ path.** B-IRQ made device-IRQ
  drivers possible and C-M9 builds the first one (the console), but converting the block driver's
  used-ring spin to interrupt-driven completion is a B2/driver follow-on, not C-M9's mechanism
  (parent plan B-IRQ "bonus, not a dependency").
- **Multiple consoles / a second UART / framebuffer console.** C-M9 delivers the one PL011 console the
  rev1§7 design names; additional consoles or a graphical console are out of scope (the IRQ_TABLE is
  sized for the platform's device SPIs, B-IRQ — adding a line is a boot-grant addition, not new
  mechanism).
- **Verus/TLA on the driver or the channel protocol.** The console is Baseline-tier userspace tooling
  (like `loader::elf`/`storaged`); C-M9 adds no mechanized proof and no model. The one verified-surface
  touch is the `DebugGetc` decode-arm **deletion** (note 1), which re-verifies kcore at ≥ 389/0 and
  changes no seam — the tally stays as B-IRQ left it.
