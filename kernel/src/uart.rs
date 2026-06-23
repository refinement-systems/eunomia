// PL011 UART — the kernel-internal diagnostic path only (rev1§7).
//
// As of C-M9 the user-facing console is the userspace `user/console` driver,
// which owns the PL011 RX line; the kernel no longer reads the UART (the
// ambient `debug_getc` input syscall is retired). What remains here is
// write-only: QEMU pre-initialises the UART, so we only poll FR.TXFF before
// writing to DR. The sole callers are the kernel's own panic/fault/boot
// reporting (`main.rs`, `exceptions.rs`) — never EL0.

const UART_BASE: usize = 0x0900_0000;

const DR: usize = 0x00; // Data register
const FR: usize = 0x18; // Flag register
const FR_TXFF: u32 = 1 << 5; // TX FIFO full

pub struct Uart;

impl Uart {
    pub const fn new() -> Self {
        Uart
    }

    fn putc(&mut self, byte: u8) {
        let fr = (UART_BASE + FR) as *const u32;
        let dr = (UART_BASE + DR) as *mut u32;
        unsafe {
            while fr.read_volatile() & FR_TXFF != 0 {}
            dr.write_volatile(byte as u32);
        }
    }
}

impl core::fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.putc(b'\r');
            }
            self.putc(byte);
        }
        Ok(())
    }
}
