// Exception vector table and handlers for AArch64 EL1.
//
// Two regimes:
//  - Exceptions from EL0 (syscalls, IRQs, faults) save a full TrapFrame
//    onto the kernel stack, run a Rust handler that may rewrite the frame
//    (context switch), then restore from the frame and eret. The kernel
//    runs with IRQs masked (non-preemptible), so kernel entries always run
//    to completion and SP_EL1 is back at the stack top by the next entry.
//  - Exceptions from EL1 are kernel bugs: dump ESR/ELR/FAR and halt.
//
// TrapFrame layout (thread.rs): x0..x30 at 8*i, sp_el0 at 248, elr at
// 256, spsr at 264; 272 bytes total, 16-aligned.

use crate::thread::TrapFrame;
use core::arch::global_asm;

global_asm!(
    r#"
    .section ".text.vectors", "ax"
    .global exception_vectors

    .macro el0_entry handler
    sub     sp, sp, #272
    stp     x0, x1,   [sp]
    stp     x2, x3,   [sp, #16]
    stp     x4, x5,   [sp, #32]
    stp     x6, x7,   [sp, #48]
    stp     x8, x9,   [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30,      [sp, #240]
    mrs     x0, sp_el0
    mrs     x1, elr_el1
    stp     x0, x1,   [sp, #248]
    mrs     x0, spsr_el1
    str     x0,       [sp, #264]
    mov     x0, sp
    bl      \handler
    b       el0_restore
    .endm

    .macro el1_fatal handler
    mrs     x0, esr_el1
    mrs     x1, elr_el1
    mrs     x2, far_el1
    bl      \handler
1:  wfe
    b       1b
    .endm

exception_vectors:
    /* ---- Current EL, SP0 (never used: SPSel=1) ---- */
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal

    /* ---- Current EL, SPx (kernel mode) ---- */
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal

    /* ---- Lower EL, AArch64 (userspace) ---- */
    .align 7
    el0_entry handle_el0_sync
    .align 7
    el0_entry handle_el0_irq
    .align 7
    el0_entry handle_el0_fiq
    .align 7
    el0_entry handle_el0_serror

    /* ---- Lower EL, AArch32 (unsupported) ---- */
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal
    .align 7
    el1_fatal handle_el1_fatal

el0_restore:
    ldp     x0, x1,   [sp, #248]
    msr     sp_el0, x0
    msr     elr_el1, x1
    ldr     x0,       [sp, #264]
    msr     spsr_el1, x0
    ldp     x0, x1,   [sp]
    ldp     x2, x3,   [sp, #16]
    ldp     x4, x5,   [sp, #32]
    ldp     x6, x7,   [sp, #48]
    ldp     x8, x9,   [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30,      [sp, #240]
    add     sp, sp, #272
    eret
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

fn read_esr() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {v}, esr_el1", v = out(reg) v) };
    v
}

fn read_far() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {v}, far_el1", v = out(reg) v) };
    v
}

#[no_mangle]
extern "C" fn handle_el1_fatal(esr: u64, elr: u64, far: u64) -> ! {
    use core::fmt::Write;
    let mut uart = crate::uart::Uart::new();
    let _ = writeln!(uart, "\n[KERNEL FATAL] Exception at EL1");
    let _ = writeln!(uart, "  ESR_EL1 = {:#018x}  EC={:#04x}", esr, esr_ec(esr));
    let _ = writeln!(uart, "  ELR_EL1 = {:#018x}", elr);
    let _ = writeln!(uart, "  FAR_EL1 = {:#018x}", far);
    loop {
        core::hint::spin_loop();
    }
}

#[no_mangle]
extern "C" fn handle_el0_sync(frame: *mut TrapFrame) {
    let esr = read_esr();
    match esr_ec(esr) {
        // SVC from AArch64: ELR already points past the svc instruction.
        0x15 => unsafe {
            if let Some(ret) = crate::syscall::dispatch(frame) {
                (*frame).x[0] = ret as u64;
            }
            crate::thread::maybe_switch(frame, false);
        },
        // Anything else from EL0 is a fault: suspend, never destroy (rev2§5.3).
        _ => unsafe {
            use core::fmt::Write;
            let mut uart = crate::uart::Uart::new();
            let t = crate::thread::current();
            let far = read_far();
            let _ = writeln!(
                uart,
                "\n[FAULT] thread {:p}: ESR={:#x} EC={:#04x} ELR={:#x} FAR={:#x}",
                t,
                esr,
                esr_ec(esr),
                (*frame).elr,
                far
            );
            (*t).state = crate::thread::ThreadState::Faulted;
            // The registers are already saved by this entry; the record
            // is the rest of the report (rev2§5.1, rev2§5.3).
            crate::thread::report_terminal(t, crate::thread::Report::Faulted { cause: esr, far });
            crate::thread::maybe_switch(frame, false);
        },
    }
}

#[no_mangle]
extern "C" fn handle_el0_irq(frame: *mut TrapFrame) {
    let intid = crate::gic::ack();
    if intid == crate::gic::INTID_VTIMER {
        crate::timer::rearm_tick();
        unsafe {
            crate::timer::check_expired(crate::timer::counter());
        }
        crate::gic::eoi(intid);
        unsafe {
            crate::thread::maybe_switch(frame, true);
        }
    } else {
        // Device SPI: a bound INTID signals its notification and the
        // line is masked (inside `deliver`) before we EOI; an unbound INTID is
        // EOI'd and dropped (no receiver). `woke` hints the reschedule.
        let woke = unsafe { crate::irq::deliver(intid) };
        crate::gic::eoi(intid);
        unsafe {
            crate::thread::maybe_switch(frame, woke);
        }
    }
}

#[no_mangle]
extern "C" fn handle_el0_fiq(frame: *mut TrapFrame) {
    let _ = frame;
    handle_el1_fatal(read_esr(), unsafe { (*frame).elr }, read_far());
}

#[no_mangle]
extern "C" fn handle_el0_serror(frame: *mut TrapFrame) {
    handle_el1_fatal(read_esr(), unsafe { (*frame).elr }, read_far());
}
