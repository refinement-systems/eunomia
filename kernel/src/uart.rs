// PL011 UART driver for QEMU virt (base 0x0900_0000).
// QEMU pre-initialises the UART, so we only need to poll FR.TXFF
// before writing to DR.

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
