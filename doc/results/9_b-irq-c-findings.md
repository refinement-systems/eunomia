# B-IRQ-C findings — boot grant + the end-to-end device-IRQ integration test + ledger closeout

Implementation notes from B-IRQ-C (`doc/plans/11_birq-detail.md`): the conformance closeout that
grants init the PL011 device caps (rev1§1: init "holds all device resources … MMIO frames, IRQ
caps"), proves the device-IRQ→notification path end to end in QEMU, adds the revoke/teardown
accounting test, and finalizes the ledger. Builds on B-IRQ-A's verified kcore object (PR #144) and
B-IRQ-B's GIC/delivery/syscall shell (PR #145).

**Results:** `cargo verus verify -p kcore` **389/0 (unchanged)**; `cargo test -p kcore` **109 green**
(108 → 109, the new `delete_irq_cap_releases_notif_ref`); `cargo build` (real-boot kernel, with the
user binaries) + `cargo build --features m1-test` + `cargo build -p ipc` clean; the M1 smoke
(`scripts/m1-test.sh`) reaches **`1234567M1 PASS`** — the new marker `7` is a bound PL011 IRQ
signalling its notification through the real GIC + exception path, an ack, and a second delivery.
B-IRQ-C adds **no verified items** and **no ledger numeric edits** (the boot grant is trusted shell;
the accounting test is `#[cfg(test)]`).

---

## 1. The interrupt trigger is forced by the harness — synthetic `GICD_ISPENDR`, not a real keystroke

The plan's B-IRQ-C acceptance imagined "a real PL011 RX interrupt (keystroke)." The actual automated
regression makes that unreachable, for two independent reasons discovered during implementation:

- **No stdin.** `scripts/m1-test.sh` boots QEMU with `< /dev/null` and watches the serial log for a
  verdict — there is no input stream to type into.
- **No aspace, no GIC access from EL0.** The embedded exit-criterion test (`kernel/src/user.rs`) runs
  in the identity window; on the m1-test path `setup_init` returns a **null aspace** (slot 5 is
  skipped), so the EL0 thread cannot `Map` the PL011 MMIO frame to program `UARTIMSC.RXIM`, and it
  has no authority over the GIC distributor either.

So the only in-scope automated trigger is to assert the line **from EL1**. `irq::bind`/`irq::ack`
software-pend INTID 33 via `GICD_ISPENDR` (offset `0x200`) under `#[cfg(feature = "m1-test")]`. This
is not a shortcut around the mechanism: the pend drives the **entire real path** — GIC distributor
routing/enable → the real `handle_el0_irq` exception entry → the device branch → `irq::deliver` →
the **verified** `notification::signal` → mask-on-deliver → `IrqAck` unmask → re-fire. The only thing
it does *not* exercise is the PL011 peripheral asserting its own RX line, which is QEMU device
behaviour, not B-IRQ code (and is the userspace driver's concern, C-M9). A real-device check is
documented as a one-off below (§5).

A useful GIC fact that makes the kick deterministic: for a **level-sensitive** SPI whose input line
is deasserted (no real device driving it), one `ISPENDR` write yields **exactly one** delivery — the
pending state clears on activation and there is no asserted line to re-pend it. So two kicks (one on
bind, one on ack) give two clean deliveries, no storms.

## 2. The boot grant is two high cspace slots, beside the existing device frames

init's device-MMIO frames are boot-static slots in `kernel_main` (slot 3 virtio, slot 4 PL031 RTC),
not retyped from untyped. B-IRQ-C adds the PL011's two resources the same way (Design decision 3):

- **Slot 23** — the PL011 MMIO frame (`CapKind::Frame { base: 0x0900_0000, pages: 1 }`, `R|W|PHYS`).
- **Slot 24** — the PL011 IRQ-handler cap (`CapKind::Irq(irq::pl011_objid())`, `R|W`).

**Why slots 23/24 and not 6/7 beside the other device frames:** the m1-test exerciser (`user.rs`)
*retypes into slots 6..=22*, so the next free indices above its range avoid a collision. The grant is
written **unconditionally** in `kernel_main` (shared by both boot paths), so the real-boot init also
holds them — correct per rev1§1, harmless (unused) until C-M9 delegates them to the console driver.
The real-boot `setup_init` ignores `_root` (writes no cspace slots itself), so there is no layout
conflict. `irq::pl011_objid()` is the one new public surface on the kernel shell: it wraps the
private `IRQ_TABLE[PL011]` address into the `ObjId` the `Store::irq_*` accessors resolve back through
— the same handle `irq::bind` forms, so bind/ack/teardown all name the same object.

The IRQ-cap rights are `R|W`, but note the `IrqBind`/`IrqAck` handlers gate on the cap **kind**, not
its rights (`syscall.rs`); the rights matter only for delegation/attenuation when C-M9 hands an
attenuated copy to the driver.

## 3. The EL0 segment is the timer segment's twin — bind, observe, ack, observe again

The new `user_main` segment (after marker `6`) mirrors the timer exercise: retype a *fresh*
notification `N_IRQ` (so its bits don't alias N1's many uses), `irq_bind(PL011_IRQ, N_IRQ, BIT_IRQ)`,
`wait_for(N_IRQ, BIT_IRQ)` (delivery 1), `irq_ack(PL011_IRQ)`, `wait_for` again (delivery 2),
`putc('7')`. The second wait is the load-bearing one: it only returns because `irq_ack` re-enabled
the line that `deliver` had masked — the witness that the mask-on-deliver / unmask-on-ack cycle
works. The flow is lost-wakeup-safe either way: if the EL1 kick lands before the wait it is a
poll-once, if after, a genuine block-then-wake (the timer segment already proved block-then-wake for
notifications, so the IRQ segment need not re-prove the scheduler interaction).

The two new `user.rs` syscall wrappers are plain (opcode 25 `irq_bind`, 26 `irq_ack`); they don't hit
the `irq` module/field-name clash B-IRQ-B's handlers had (finding 8.6) because they are free
functions, not handlers destructuring a `Sys::IrqBind { irq, .. }`.

**Gotcha (cost me a confusing run): the shared target path.** `cargo build` (default real-boot) and
`cargo build --features m1-test` write the *same* `target/.../debug/kernel`. Running a default build
between an m1-test build and the smoke booted the wrong binary (`123456M1 PASS`, no `7`). The fix is
just to let `scripts/m1-test.sh` do its own `--features m1-test` build immediately before boot (it
does); don't interleave a default build. Not a code issue — a build-cache footgun worth noting.

## 4. The accounting test drives the real `delete` → `obj_unref` → `destroy_irq`

`destroy_irq_unbinds` (B-IRQ-A) already proved the *object op* in isolation. B-IRQ-C's
`delete_irq_cap_releases_notif_ref` (`kcore/src/test_store.rs`) drives it through the **cap-deletion
dispatch** the rev1§2.2 revoke path actually takes: a fixture with the notification's own cap (slot
0) plus a *bound* `IrqObj` named by its lone `Irq` cap (slot 1); `census(notif) == refs == 2`
(cap + binding) at entry. Deleting slot 1 takes the IRQ object's refcount to 0 → `obj_unref`'s Irq
arm → `destroy_irq` → unbind → the notification's binding ref is released. Asserted: the generic
`check_delete` contract (cspace_wf, count drop, `refcount_sound` preserved), then the IRQ-specific
close — the object is unbound, `obj_census(notif)` drops from 2 to 1 (binding term gone), `refs[notif]`
drops to its own cap, and the notification's own cap **survives** the IRQ object's destruction. Needed
one new fixture helper, `irq_cap(o)` (the `notif_cap` twin).

## 5. Manual real-device verification (one-off, NOT wired into CI)

The committed regression proves the whole kernel mechanism synthetically (§1). To additionally confirm
that a *real* PL011 RX interrupt lands on INTID 33 (the boot grant's INTID choice, end to end with the
QEMU device), use this throwaway procedure — it is deliberately **not** committed, because the PL011
register programming it needs is C-M9 (console-driver) scope:

1. **Enable PL011 RX interrupts from EL1** (the kernel already owns the UART for debug output). In
   `kernel/src/uart.rs`, after QEMU's pre-init, set `UARTIMSC` (`0x38`) bit 4 (`RXIM`) and clear
   `UARTLCR_H` (`0x2C`) bit 4 (`FEN`) so RX raises an interrupt on **each** byte (with FIFOs enabled,
   a single keystroke sits below the default 1/8 trigger and only the receive-timeout interrupt would
   fire). Call it from `kernel_main` after `irq::init()`.
2. **Suppress the synthetic kick** for the run (comment the `#[cfg(feature = "m1-test")]
   gic::set_pending` in `irq::bind`) so delivery 1 must come from the keystroke; keep the `irq::ack`
   kick (or feed a second byte) for delivery 2.
3. **Boot interactively, feeding a byte:** drop the `< /dev/null` and use a plain serial, e.g.
   `printf 'x' | qemu-system-aarch64 -machine virt,gic-version=3 -cpu cortex-a72 -m 256M -nographic
   -nic none -serial stdio -kernel target/aarch64-unknown-none-softfloat/debug/kernel` (or run it
   fully interactive and type a key). The EL0 thread blocks in `wait_for(N_IRQ, …)` until the byte
   asserts PL011 RX → SPI 33 → `deliver` → signal → wake → marker `7`.

This validates only that QEMU's PL011 raises SPI 33 (a well-known QEMU-virt fact: PL011 RX is SPI 1 →
INTID 33); everything upstream of the device is already covered by the committed synthetic test.

**Result (ran it):** with the throwaway patch above (`UARTIMSC |= RXIM|RTIM`, bind-kick suppressed) and
a byte fed ~3 s after boot via `( sleep 3; printf 'x'; sleep 6 ) | qemu … -serial stdio`, the run
reached **`1234567M1 PASS`** — a real keystroke woke the bound EL0 thread through the device path.
A **negative control** (same kernel, `< /dev/null`, no byte) blocked at **`123456`** and never reached
`7`, confirming the keystroke is *necessary* (with the bind-kick suppressed, delivery 1 has no other
source). Delivery 2 came from the surviving ack-kick. The patch was reverted — only the synthetic path
is committed.

## 6. Ledger: nothing to change

B-IRQ-A/B left `doc/guidelines/verus_trusted-base.md` in its final B-IRQ state. Confirmed unchanged:
the verified-surface scope paragraph names the **IRQ-handler object** (`irq_bind`/`irq_unbind`/
`destroy_irq` + `irq_binding_refs`, B-IRQ-A); the kcore baseline reads **389 verified, 0 errors**; the
trusted-base tally is **13** (7 `external_body` + 6 `assume_specification`) — boot-static `IrqObj`
adds no `ExIrqObj` opaque-size seam (Design decision 3). No `[verifying]` flip, no §6.1 spec edit
(honesty note 4): rev1§1/§3.6 already bless the IRQ object, and delivery reuses the already-verified
`signal`. B-IRQ is complete.
