//! The PL011 register layer over an injectable MMIO seam. The pure functions —
//! RX-FIFO drain, polled TX write, RX
//! interrupt enable — are host-tested against a fake (rev2§6 Baseline tier);
//! the real [`MmioWindow`] does volatile MMIO on the granted device VA and is
//! exercised only in QEMU.
//!
//! Register offsets are the PL011 layout the kernel pokes for polled
//! I/O (`kernel/src/uart.rs`: `DR`/`FR`) plus the interrupt registers the
//! kernel leaves alone — the kernel UART is pure-polling, so the userspace
//! driver owns RX interrupt enablement on its own MMIO frame (note 3).

/// Data register (read pops the RX FIFO; write pushes the TX FIFO).
pub const DR: usize = 0x00;
/// Flag register (read-only): `TXFF`/`RXFE` below.
pub const FR: usize = 0x18;
/// Interrupt mask set/clear: a 1 bit unmasks that interrupt.
pub const UARTIMSC: usize = 0x38;
/// Interrupt clear: writing 1s clears the corresponding pending interrupts.
pub const UARTICR: usize = 0x44;

/// `FR` bit: TX FIFO full (spin before writing `DR`).
pub const FR_TXFF: u32 = 1 << 5;
/// `FR` bit: RX FIFO empty (stop draining `DR`).
pub const FR_RXFE: u32 = 1 << 4;
/// `UARTIMSC` bit: RX interrupt — fires when the FIFO crosses its trigger level.
pub const RXIM: u32 = 1 << 4;
/// `UARTIMSC` bit: RX-timeout interrupt — fires for a partial FIFO that stops
/// filling, so a lone keystroke still delivers.
pub const RTIM: u32 = 1 << 6;
/// Write to `UARTICR` to clear every interrupt source (all 11 PL011 bits).
pub const ICR_ALL: u32 = 0x7FF;

/// The injectable MMIO seam: 32-bit reads/writes at a register offset. `read`
/// takes `&mut self` so the test fake can pop its RX FIFO / advance its TXFF
/// model without interior-mutability ceremony; a `&mut` volatile read is
/// standard and Miri-clean. The real impl ([`MmioWindow`]) hits the granted
/// device window; the test fake (`FakePl011`) is an in-memory model.
pub trait Pl011 {
    fn read(&mut self, off: usize) -> u32;
    fn write(&mut self, off: usize, val: u32);
}

/// The real MMIO window over the granted PL011 VA (the storaged `MmioWindow`
/// precedent). QEMU-only — never instantiated by the host tests, which drive
/// the pure functions through `FakePl011`.
pub struct MmioWindow {
    base: usize,
}

impl MmioWindow {
    pub fn new(base: usize) -> Self {
        MmioWindow { base }
    }
}

impl Pl011 for MmioWindow {
    fn read(&mut self, off: usize) -> u32 {
        // Safety: `base` is the VA init mapped the PL011 frame at before start,
        // and `off` is one of the fixed register offsets above (in-frame).
        unsafe { ((self.base + off) as *const u32).read_volatile() }
    }

    fn write(&mut self, off: usize, val: u32) {
        // Safety: as `read`.
        unsafe { ((self.base + off) as *mut u32).write_volatile(val) }
    }
}

/// Enable RX + RX-timeout interrupts: clear any stale pending interrupt
/// (`UARTICR`), then unmask `RXIM | RTIM` (`UARTIMSC`). Bind the IRQ cap to the
/// wake notification *before* calling this so
/// an early keystroke cannot reach a not-yet-bound INTID.
pub fn enable_rx_interrupts<R: Pl011>(regs: &mut R) {
    regs.write(UARTICR, ICR_ALL);
    regs.write(UARTIMSC, RXIM | RTIM);
}

/// Drain the RX FIFO into `out`: read `DR` while `!FR.RXFE`, stopping at empty
/// or when `out` is full. Returns the count read (`<= out.len()`). Never reads
/// past empty and never overruns `out`; a still-non-empty FIFO (buffer smaller
/// than the FIFO) is re-drained on the next delivery — the line stays asserted
/// until the FIFO empties.
pub fn drain_rx<R: Pl011>(regs: &mut R, out: &mut [u8]) -> usize {
    let mut n = 0;
    while n < out.len() && (regs.read(FR) & FR_RXFE) == 0 {
        out[n] = regs.read(DR) as u8;
        n += 1;
    }
    n
}

/// Write every byte of `bytes` to `DR`, spinning on `!FR.TXFF` before each
/// write — the `uart::putc` discipline, no TX interrupt (a console's TX is
/// low-volume, so a brief full-FIFO spin is fine).
pub fn write_tx<R: Pl011>(regs: &mut R, bytes: &[u8]) {
    for &b in bytes {
        while (regs.read(FR) & FR_TXFF) != 0 {}
        regs.write(DR, b as u32);
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for the PL011 register layer (rev2§6 Baseline tier).
    //! The pure functions run against `FakePl011`, an in-memory model with an
    //! injectable RX FIFO, a TX capture, and a TXFF countdown (so a spin-on-full
    //! test makes progress and cannot hang). The real `MmioWindow` is never
    //! touched here — its volatile MMIO is the QEMU path.
    use super::*;
    use proptest::prelude::*;
    use std::collections::VecDeque;

    /// In-memory PL011: a register array, an injectable RX FIFO, a TX capture,
    /// and a model of `TXFF` clearing after a chosen number of `FR` polls.
    struct FakePl011 {
        regs: [u32; 0x44 / 4 + 1],
        rx: VecDeque<u8>,
        tx: Vec<u8>,
        /// While `> 0`, a read of `FR` reports `TXFF` set and decrements; at `0`
        /// the FIFO reports not-full. Models a TX FIFO draining over time.
        txff_remaining: usize,
    }

    impl FakePl011 {
        fn new() -> Self {
            FakePl011 {
                regs: [0; 0x44 / 4 + 1],
                rx: VecDeque::new(),
                tx: Vec::new(),
                txff_remaining: 0,
            }
        }

        fn with_rx(bytes: &[u8]) -> Self {
            let mut f = Self::new();
            f.rx = bytes.iter().copied().collect();
            f
        }

        fn with_txff_full_for(cycles: usize) -> Self {
            let mut f = Self::new();
            f.txff_remaining = cycles;
            f
        }

        /// Read a register the code wrote (no side effects) — for assertions.
        fn peek(&self, off: usize) -> u32 {
            self.regs[off / 4]
        }
    }

    impl Pl011 for FakePl011 {
        fn read(&mut self, off: usize) -> u32 {
            match off {
                FR => {
                    let mut fr = 0;
                    if self.rx.is_empty() {
                        fr |= FR_RXFE;
                    }
                    if self.txff_remaining > 0 {
                        fr |= FR_TXFF;
                        self.txff_remaining -= 1;
                    }
                    fr
                }
                DR => self.rx.pop_front().unwrap_or(0) as u32,
                _ => self.regs[off / 4],
            }
        }

        fn write(&mut self, off: usize, val: u32) {
            match off {
                DR => self.tx.push(val as u8),
                _ => self.regs[off / 4] = val,
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            failure_persistence: if cfg!(miri) { None } else { ProptestConfig::default().failure_persistence },
            .. ProptestConfig::default()
        })]

        /// Arbitrary RX byte sequences round-trip through `drain_rx` in FIFO
        /// order, bounded by the output buffer: the first `min(len, cap)` bytes
        /// come out in order, and the drain stops at empty (`RXFE`).
        #[test]
        fn rx_round_trips_in_order(
            bytes in proptest::collection::vec(any::<u8>(), 0..128),
            cap in 1usize..64,
        ) {
            let mut fake = FakePl011::with_rx(&bytes);
            let mut out = vec![0u8; cap];
            let n = drain_rx(&mut fake, &mut out);
            let want = bytes.len().min(cap);
            prop_assert_eq!(n, want);
            prop_assert_eq!(&out[..n], &bytes[..want]);
        }

        /// Arbitrary output sequences reach the TX capture in order: `write_tx`
        /// writes each byte to `DR`, and the fake captures them in `tx`.
        #[test]
        fn tx_reaches_capture_in_order(
            bytes in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut fake = FakePl011::new();
            write_tx(&mut fake, &bytes);
            prop_assert_eq!(&fake.tx, &bytes);
        }

        /// Full-FIFO spin-then-drain: with `TXFF` asserted for `full_cycles`
        /// polls, `write_tx` spins then completes — it must not hang and must
        /// still deliver every byte in order.
        #[test]
        fn tx_spins_on_full_then_drains(
            bytes in proptest::collection::vec(any::<u8>(), 1..32),
            full_cycles in 0usize..8,
        ) {
            let mut fake = FakePl011::with_txff_full_for(full_cycles);
            write_tx(&mut fake, &bytes);
            prop_assert_eq!(&fake.tx, &bytes);
        }
    }

    /// `enable_rx_interrupts` clears stale pending (`UARTICR`) then unmasks
    /// `RXIM | RTIM` (`UARTIMSC`).
    #[test]
    fn enable_rx_sets_imsc_and_clears_icr() {
        let mut fake = FakePl011::new();
        enable_rx_interrupts(&mut fake);
        assert_eq!(fake.peek(UARTICR), ICR_ALL);
        assert_eq!(fake.peek(UARTIMSC), RXIM | RTIM);
    }

    /// Negative control — proves the order oracle in `rx_round_trips_in_order`
    /// has teeth. The real drain is FIFO order, so it must NOT equal the
    /// *reversed* expectation: a buggy LIFO drain would make `assert_ne!` fire.
    /// (Flip the `assert_ne!` to `assert_eq!` to watch the oracle reject the
    /// wrong order — the deliberately-broken oracle then fails.)
    #[test]
    fn rx_order_oracle_has_teeth() {
        let bytes = [b'a', b'b', b'c', b'd'];
        let mut fake = FakePl011::with_rx(&bytes);
        let mut out = [0u8; 4];
        let n = drain_rx(&mut fake, &mut out);
        let mut reversed = bytes;
        reversed.reverse();
        assert_ne!(&out[..n], &reversed[..]);
        assert_eq!(&out[..n], &bytes[..]);
    }
}
