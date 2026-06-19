//! Minimal GICv3 bring-up for QEMU virt (single core, group-1 only).
//!
//! Just enough to take the virtual-timer PPI (INTID 27) at EL1. Userspace
//! IRQ-handler caps (rev0§1) are introduced by the userspace drivers.

use core::arch::asm;

const GICD_BASE: usize = 0x0800_0000;
const GICR_BASE: usize = 0x080A_0000; // core 0 redistributor

const GICD_CTLR: usize = GICD_BASE;
const GICR_WAKER: usize = GICR_BASE + 0x0014;
const GICR_SGI_BASE: usize = GICR_BASE + 0x1_0000;
const GICR_IGROUPR0: usize = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: usize = GICR_SGI_BASE + 0x0100;

pub const INTID_VTIMER: u32 = 27;

unsafe fn mmio_r(addr: usize) -> u32 {
    (addr as *const u32).read_volatile()
}

unsafe fn mmio_w(addr: usize, v: u32) {
    (addr as *mut u32).write_volatile(v)
}

pub fn init() {
    unsafe {
        // Distributor: affinity routing + group-1 enable.
        mmio_w(GICD_CTLR, (1 << 4) | (1 << 1));

        // Wake this core's redistributor.
        let waker = mmio_r(GICR_WAKER) & !(1 << 1); // clear ProcessorSleep
        mmio_w(GICR_WAKER, waker);
        while mmio_r(GICR_WAKER) & (1 << 2) != 0 {} // ChildrenAsleep

        // PPIs to group 1, enable the virtual timer PPI.
        mmio_w(GICR_IGROUPR0, 0xFFFF_FFFF);
        mmio_w(GICR_ISENABLER0, 1 << INTID_VTIMER);

        // CPU interface: system-register access, open priority mask,
        // enable group 1.
        asm!("msr icc_sre_el1, {v}", v = in(reg) 1u64);
        asm!("isb");
        asm!("msr icc_pmr_el1, {v}", v = in(reg) 0xFFu64);
        asm!("msr icc_igrpen1_el1, {v}", v = in(reg) 1u64);
        asm!("isb");
    }
}

/// Acknowledge the pending interrupt; returns its INTID.
pub fn ack() -> u32 {
    let v: u64;
    unsafe { asm!("mrs {v}, icc_iar1_el1", v = out(reg) v) };
    v as u32
}

pub fn eoi(intid: u32) {
    unsafe { asm!("msr icc_eoir1_el1, {v}", v = in(reg) intid as u64) };
}
