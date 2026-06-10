#![no_std]
#![no_main]

mod boot;
mod exceptions;
mod mmu;
mod uart;

use core::fmt::Write;

#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    // Install exception vectors before anything else.
    exceptions::init();

    let mut out = uart::Uart::new();

    writeln!(out, "").unwrap();
    writeln!(out, "Eunomia OS — M0 boot").unwrap();

    writeln!(out, "Setting up MMU (identity map)...").unwrap();
    mmu::init();
    writeln!(out, "MMU enabled.").unwrap();

    // Demonstrate M0 exit criterion: trigger a synchronous exception and
    // report it. EC=0x00 from UDF = "Unknown reason" class.
    writeln!(out, "Triggering UDF exception to exercise exception handler...").unwrap();
    unsafe {
        core::arch::asm!("udf #0x4E");  // 0x4E = 'N' for Eunomia
    }

    // Not reached — exception handler halts.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn on_panic(info: &core::panic::PanicInfo) -> ! {
    let mut out = uart::Uart::new();
    let _ = writeln!(out, "\nKERNEL PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
