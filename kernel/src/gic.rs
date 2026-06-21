//! Minimal GICv3 bring-up for QEMU virt (single core, group-1 only).
//!
//! Enough to take the virtual-timer PPI (INTID 27) at EL1, plus the distributor
//! routing/enable a device SPI needs so the IRQ-handler caps (rev1§1) the
//! userspace drivers hold can deliver (B-IRQ-B; the per-INTID helpers below).

use core::arch::asm;

const GICD_BASE: usize = 0x0800_0000;
const GICR_BASE: usize = 0x080A_0000; // core 0 redistributor

const GICD_CTLR: usize = GICD_BASE;
const GICR_WAKER: usize = GICR_BASE + 0x0014;
const GICR_SGI_BASE: usize = GICR_BASE + 0x1_0000;
const GICR_IGROUPR0: usize = GICR_SGI_BASE + 0x0080;
const GICR_ISENABLER0: usize = GICR_SGI_BASE + 0x0100;

// Distributor banks for device SPIs (INTID >= 32). IGROUPR/ISENABLER/ICENABLER
// are one bit per INTID (32 per word); IPRIORITYR is one byte per INTID; ICFGR
// is two bits per INTID (16 per word); IROUTER is one 64-bit word per INTID.
const GICD_IGROUPR: usize = GICD_BASE + 0x0080;
const GICD_ISENABLER: usize = GICD_BASE + 0x0100;
const GICD_ICENABLER: usize = GICD_BASE + 0x0180;
const GICD_IPRIORITYR: usize = GICD_BASE + 0x0400;
const GICD_ICFGR: usize = GICD_BASE + 0x0C00;
const GICD_IROUTER: usize = GICD_BASE + 0x6000;

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

// ── Device-SPI distributor routing (B-IRQ-B) ──────────────────────────────
//
// `GICD_CTLR` already enables affinity routing (ARE_NS) and group 1 in `init`,
// and `ICC_PMR` is wide open (0xFF), so a device SPI need only be put in group
// 1, given a priority below the mask, set level-triggered, routed to core 0,
// and enabled. `enable`/`disable` are also the per-IRQ mask/unmask the delivery
// path uses (mask-on-deliver / unmask-on-ack, rev1§3.6).

/// Route a device SPI to core 0: group 1, a priority below the mask,
/// level-triggered, affinity 0.0.0.0. The caller enables it with [`enable`].
pub fn set_route(intid: u32) {
    let word = (intid / 32) as usize;
    let bit = intid % 32;
    unsafe {
        // Group 1 (non-secure).
        let g = mmio_r(GICD_IGROUPR + 4 * word);
        mmio_w(GICD_IGROUPR + 4 * word, g | (1 << bit));
        // Priority byte: 0xA0 < ICC_PMR (0xFF), so it passes the mask.
        ((GICD_IPRIORITYR + intid as usize) as *mut u8).write_volatile(0xA0);
        // Level-triggered: clear the edge bit of this INTID's 2-bit ICFGR field.
        let cw = (intid / 16) as usize;
        let shift = (intid % 16) * 2;
        let c = mmio_r(GICD_ICFGR + 4 * cw);
        mmio_w(GICD_ICFGR + 4 * cw, c & !(0b10 << shift));
        // Route to core 0 (affinity 0.0.0.0, IRM=0 → specific PE).
        ((GICD_IROUTER + 8 * intid as usize) as *mut u64).write_volatile(0);
    }
}

/// Enable (unmask) a device SPI at the distributor.
pub fn enable(intid: u32) {
    let word = (intid / 32) as usize;
    unsafe { mmio_w(GICD_ISENABLER + 4 * word, 1 << (intid % 32)) };
}

/// Disable (mask) a device SPI at the distributor — used to stop a
/// level-triggered line re-pending before its driver services it.
pub fn disable(intid: u32) {
    let word = (intid / 32) as usize;
    unsafe { mmio_w(GICD_ICENABLER + 4 * word, 1 << (intid % 32)) };
}
