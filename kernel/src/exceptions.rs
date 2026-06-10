// Exception vector table and handlers for AArch64 EL1.
//
// The vector table must be aligned to 2 KiB (2^11). The linker script
// places .text.vectors at a 2 KiB boundary; each of the 16 slots is
// 128 bytes (2^7). All EL1 SPx synchronous exceptions land at slot 4
// (offset 0x200), which is where we route the M0 test exception.

use core::arch::global_asm;

global_asm!(
    r#"
    .section ".text.vectors", "ax"
    .global exception_vectors

    /* Each slot is 128 bytes; .align 7 advances to the next boundary. */
exception_vectors:
    /* ---- Current EL, SP0 ---- */
    .align 7
    b   exc_el1sp0_sync
    .align 7
    b   exc_el1sp0_irq
    .align 7
    b   exc_el1sp0_fiq
    .align 7
    b   exc_el1sp0_serror

    /* ---- Current EL, SPx (kernel mode, our normal path) ---- */
    .align 7
    b   exc_el1spx_sync
    .align 7
    b   exc_el1spx_irq
    .align 7
    b   exc_el1spx_fiq
    .align 7
    b   exc_el1spx_serror

    /* ---- Lower EL, AArch64 ---- */
    .align 7
    b   exc_el0_sync
    .align 7
    b   exc_el0_irq
    .align 7
    b   exc_el0_fiq
    .align 7
    b   exc_el0_serror

    /* ---- Lower EL, AArch32 ---- */
    .align 7
    b   exc_el0_aarch32_sync
    .align 7
    b   exc_el0_aarch32_irq
    .align 7
    b   exc_el0_aarch32_fiq
    .align 7
    b   exc_el0_aarch32_serror

    /* ------------------------------------------------------------------ */
    /* Exception entries: read syndrome + address, call Rust handler.      */
    /* None of the M0 handlers return, so we skip full register save.     */
    /* ------------------------------------------------------------------ */

    .macro exc_entry name handler
\name:
    mrs     x0, esr_el1
    mrs     x1, elr_el1
    mrs     x2, far_el1
    bl      \handler
    /* handler is -> !; spin if it somehow returns */
1:  wfe
    b       1b
    .endm

exc_entry exc_el1spx_sync, handle_sync_el1spx
exc_entry exc_el1sp0_sync, handle_unexpected
exc_entry exc_el1sp0_irq,  handle_unexpected
exc_entry exc_el1sp0_fiq,  handle_unexpected
exc_entry exc_el1sp0_serror, handle_unexpected
exc_entry exc_el1spx_irq,  handle_irq_el1spx
exc_entry exc_el1spx_fiq,  handle_unexpected
exc_entry exc_el1spx_serror, handle_unexpected
exc_entry exc_el0_sync,    handle_unexpected
exc_entry exc_el0_irq,     handle_unexpected
exc_entry exc_el0_fiq,     handle_unexpected
exc_entry exc_el0_serror,  handle_unexpected
exc_entry exc_el0_aarch32_sync,   handle_unexpected
exc_entry exc_el0_aarch32_irq,    handle_unexpected
exc_entry exc_el0_aarch32_fiq,    handle_unexpected
exc_entry exc_el0_aarch32_serror, handle_unexpected
    "#
);

extern "C" {
    fn exception_vectors();
}

pub fn init() {
    let vbar = exception_vectors as *const () as usize as u64;
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {v}",
            "isb",
            v = in(reg) vbar,
        );
    }
}

/// ESR_EL1 EC field: bits [31:26].
#[inline]
fn esr_ec(esr: u64) -> u64 {
    (esr >> 26) & 0x3F
}

#[no_mangle]
extern "C" fn handle_sync_el1spx(esr: u64, elr: u64, far: u64) -> ! {
    use core::fmt::Write;
    let mut uart = crate::uart::Uart::new();
    let _ = writeln!(uart, "\n[EXCEPTION] Synchronous EL1 SPx");
    let _ = writeln!(uart, "  ESR_EL1 = {:#018x}  EC={:#04x}", esr, esr_ec(esr));
    let _ = writeln!(uart, "  ELR_EL1 = {:#018x}", elr);
    let _ = writeln!(uart, "  FAR_EL1 = {:#018x}", far);
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
extern "C" fn handle_irq_el1spx(_esr: u64, _elr: u64, _far: u64) -> ! {
    use core::fmt::Write;
    let mut uart = crate::uart::Uart::new();
    let _ = writeln!(uart, "[EXCEPTION] Unexpected IRQ at EL1 — halting");
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
extern "C" fn handle_unexpected(_esr: u64, elr: u64, _far: u64) -> ! {
    use core::fmt::Write;
    let mut uart = crate::uart::Uart::new();
    let _ = writeln!(uart, "[EXCEPTION] Unexpected exception — ELR={:#018x} — halting", elr);
    loop {
        core::hint::spin_loop();
    }
}
