//! console — the userspace PL011 UART console driver on Eunomia (spec rev2§7).
//! It holds the PL011 IRQ-handler cap and the MMIO frame cap, delivers
//! RX keystrokes to the shell and writes the shell's output bytes to the UART,
//! all over one bidirectional channel granted to the shell under the
//! `stdin`/`stdout` standard names (rev2§5.1) — the "console cap" of rev2§7.
//!
//! World (built by init; fixed here so the driver and init agree):
//! slot 0 = bootstrap channel whose first message is the unified startup block
//! (`b"EUS1"`, the rev2§5.1 named-grant table); slot 1 = the shell's console
//! channel (bidirectional — RX bytes out to the shell, TX bytes in from it);
//! slot 2 = the wake notification the reactor waits on;
//! slot 3 = the PL011 IRQ-handler cap. The PL011 MMIO window is pre-mapped by
//! init and arrives in the block as a `REGION` grant (`NAME_PL011_MMIO`).
//!
//! The driver is interrupt-driven on RX (the kernel masks the line on delivery;
//! the driver drains the FIFO in EL0 then `IrqAck`s to unmask) and polled on TX.
//! The register layer + byte forwarding live in
//! [`pl011`], host-tested against a fake; this `_start` glue is QEMU-only.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the crate builds as a host harness for the `pl011` register
// tests: std and the default test `main` take over, the bare-metal items below
// are gated out, and the `_start`-only helpers are dead — allow the noise.
#![cfg_attr(test, allow(dead_code, unused_imports))]

mod pl011;

use ipc::{sys, Reactor, Signals, SyscallTransport};
use loader::startup;
use pl011::{drain_rx, enable_rx_interrupts, write_tx, MmioWindow};

const BOOT_CHAN: u32 = 0;
const SHELL_CHAN: u32 = 1;
const WAKE_NOTIF: u32 = 2;
const IRQ_CAP: u32 = 3;

/// Reactor dispatch keys — opaque tokens, never notification bits (rev2§3.6:
/// the reactor hides the bit shape, returning a key from `wait`).
const IRQ_KEY: ipc::Key = 0;
const CHAN_KEY: ipc::Key = 1;

/// The notification bit the kernel signals on a PL011 RX interrupt. Claimed via
/// `register_bound` *before* `register(SHELL_CHAN)` auto-allocates the lowest
/// clear bit, so the two never collide: the bound claim takes bit 0 and
/// `register` then takes bit 1. The same bit is `irq_bind`'s `bits` argument —
/// the kernel signals it, the reactor dispatches it to `IRQ_KEY`.
const RX_BIT: u64 = 1 << 0;

#[cfg(not(test))]
#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    // 1. Receive the startup block on the bootstrap channel and decode it
    //    (refuse-not-crash: a malformed block is a boot failure, not a panic).
    let mut buf = [0u8; 256];
    let len = recv_blocking(BOOT_CHAN, &mut buf);
    let Some(s) = startup::decode(&buf[..len]) else {
        fail(b"bad startup block");
    };
    // 2. The PL011 MMIO base is the VA init pre-mapped the frame at (it travels
    //    as a REGION grant, not the hardcoded 0x0900_0000 — like storaged's
    //    virtio-mmio window).
    let Some((mmio_va, _len, _pa)) = startup::region(&s, startup::NAME_PL011_MMIO) else {
        fail(b"no pl011 mmio grant");
    };
    let mut regs = MmioWindow::new(mmio_va as usize);

    // 3. Bind the IRQ cap to the wake notification *first* — before enabling the
    //    line — so an early keystroke cannot reach a still-unbound INTID and be
    //    EOI-and-dropped.
    if sys::irq_bind(IRQ_CAP, WAKE_NOTIF, RX_BIT) < 0 {
        fail(b"irq_bind failed");
    }
    // 4. Then enable PL011 RX + RX-timeout interrupts: the driver owns RX
    //    interrupt enablement (the kernel UART is poll-only).
    enable_rx_interrupts(&mut regs);

    // 5. Multiplex the IRQ notification and the shell channel in one reactor.
    //    register_bound (the IRQ, externally bound by the kernel) claims bit 0;
    //    register (the channel, bound + self-signalled by the IPC crate) then
    //    auto-allocates bit 1 — collision-free by ordering.
    let transport = SyscallTransport;
    let mut reactor = Reactor::new(&transport, WAKE_NOTIF);
    if reactor.register_bound(RX_BIT, IRQ_KEY).is_err() {
        fail(b"reactor register_bound");
    }
    if reactor
        .register(SHELL_CHAN, Signals::READABLE, CHAN_KEY)
        .is_err()
    {
        fail(b"reactor register");
    }

    sys::debug_write(b"[console] serving\n");

    let mut rx = [0u8; 32];
    let mut out = [0u8; 256];

    // Drain keystrokes that arrived during boot, before RX interrupts were
    // enabled: QEMU buffers them into the FIFO, but the level-crossing RX
    // interrupt fires only for bytes arriving *after* the unmask — a pre-filled
    // FIFO would never wake us, and once full it back-pressures the input
    // (the shell would then see no keystrokes at all). Drain once up front, then
    // ack so the line is unmasked; the ack-unmask cycle in the loop catches
    // everything that follows. A no-op when nothing was typed during boot.
    let n0 = drain_rx(&mut regs, &mut rx);
    if n0 > 0 {
        send_bytes(SHELL_CHAN, &rx[..n0]);
    }
    sys::irq_ack(IRQ_CAP);

    // 6. Serve loop: one wait(), two keys. RX → drain the FIFO, forward to the
    //    shell, then unmask (mask-on-deliver / ack-unmask). TX → drain the shell
    //    channel, write each message's bytes to the UART (polled on TXFF).
    loop {
        let (key, _signals) = reactor.wait();
        match key {
            IRQ_KEY => {
                let n = drain_rx(&mut regs, &mut rx);
                if n > 0 {
                    send_bytes(SHELL_CHAN, &rx[..n]);
                }
                sys::irq_ack(IRQ_CAP);
            }
            CHAN_KEY => loop {
                let (rlen, _caps) = sys::chan_recv(SHELL_CHAN, out.as_mut_ptr(), None);
                if rlen < 0 {
                    break; // ERR_EMPTY: the ring is drained, re-wait.
                }
                write_tx(&mut regs, &out[..rlen as usize]);
            },
            _ => {}
        }
    }
}

/// Block until a message lands on `chan`, yielding while the ring is empty.
fn recv_blocking(chan: u32, buf: &mut [u8; 256]) -> usize {
    loop {
        let (len, _) = sys::chan_recv(chan, buf.as_mut_ptr(), None);
        if len >= 0 {
            return len as usize;
        }
        sys::yield_now();
    }
}

/// Forward RX bytes to the shell, retrying on backpressure. A console is
/// low-bandwidth, so `Full` (the shell hasn't drained its input ring yet)
/// resolves promptly on a yield-retry.
fn send_bytes(chan: u32, bytes: &[u8]) {
    loop {
        let r = sys::chan_send(chan, bytes, None);
        if r != sys::ERR_FULL {
            return;
        }
        sys::yield_now();
    }
}

fn fail(msg: &[u8]) -> ! {
    sys::debug_write(b"[console] FATAL: ");
    sys::debug_write(msg);
    sys::debug_write(b"\n");
    sys::exit()
}

#[cfg(not(test))]
#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[console] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}
