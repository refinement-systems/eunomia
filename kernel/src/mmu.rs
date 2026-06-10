// Identity-map MMU setup for AArch64 EL1, QEMU virt machine.
//
// Translation regime: TTBR0, 39-bit VA (T0SZ=25), 4K granule, level-1
// start. Layout:
//
//   L1[0]: PA 0x0000_0000–0x3FFF_FFFF  1 GiB block, device nGnRnE (MMIO)
//   L1[1]: PA 0x4000_0000–0x7FFF_FFFF  → L2 table, 2 MiB blocks (DRAM):
//            - kernel DRAM: normal WB, EL1-only, UXN
//            - user window: normal WB, EL0+EL1 RW, EL0-executable, PXN
//
// The user window hosts the M1 embedded EL0 test program and its stacks.
// Proper per-process address-space objects (created from donated untyped,
// pool-at-creation, §2.5) arrive with M3; until then every thread runs in
// this single identity map.
//
// MAIR_EL1:
//   Attr0 = 0xFF  normal outer/inner write-back, read/write allocate
//   Attr1 = 0x00  device nGnRnE

/// User-accessible identity-mapped window (one 2 MiB L2 block).
pub const USER_BASE: u64 = 0x4800_0000;
pub const USER_SIZE: u64 = 0x0020_0000;

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

static mut L1_TABLE: PageTable = PageTable([0u64; 512]);
static mut L2_DRAM: PageTable = PageTable([0u64; 512]);

const BLOCK: u64 = 0b01;
const TABLE: u64 = 0b11;
const AF: u64 = 1 << 10;
const UXN: u64 = 1 << 54; // EL0 execute-never
const PXN: u64 = 1 << 53; // EL1 execute-never

const ATTR_NORMAL: u64 = 0 << 2; // MAIR Attr0
const ATTR_DEVICE: u64 = 1 << 2; // MAIR Attr1

const SH_INNER: u64 = 0b11 << 8;
const SH_NONE: u64 = 0b00 << 8;

const AP_EL1_RW: u64 = 0b00 << 6;
const AP_EL0_RW: u64 = 0b01 << 6;

const DESC_DEVICE: u64 = UXN | PXN | AF | SH_NONE | ATTR_DEVICE | AP_EL1_RW | BLOCK;
const DESC_KERNEL: u64 = UXN | AF | SH_INNER | ATTR_NORMAL | AP_EL1_RW | BLOCK;
// EL0-executable, EL1-never: the kernel must not fetch from user memory.
const DESC_USER: u64 = PXN | AF | SH_INNER | ATTR_NORMAL | AP_EL0_RW | BLOCK;

pub fn init() {
    unsafe {
        let l2 = &mut *core::ptr::addr_of_mut!(L2_DRAM);
        for (i, slot) in l2.0.iter_mut().enumerate() {
            let pa = 0x4000_0000u64 + (i as u64) * 0x20_0000;
            *slot = if (USER_BASE..USER_BASE + USER_SIZE).contains(&pa) {
                pa | DESC_USER
            } else {
                pa | DESC_KERNEL
            };
        }

        let l1 = &mut *core::ptr::addr_of_mut!(L1_TABLE);
        l1.0[0] = 0x0000_0000 | DESC_DEVICE;
        l1.0[1] = core::ptr::addr_of!(L2_DRAM) as u64 | TABLE;

        let table_pa = core::ptr::addr_of!(L1_TABLE) as u64;

        // MAIR_EL1: Attr0=normal WB, Attr1=device nGnRnE.
        let mair: u64 = 0x00FF;

        // TCR_EL1:
        //   T0SZ   = 25        [5:0]
        //   IRGN0  = 01        [9:8]
        //   ORGN0  = 01        [11:10]
        //   SH0    = 11        [13:12]
        //   TG0    = 00 (4K)   [15:14]
        //   T1SZ   = 25        [21:16]
        //   EPD1   = 1         [23]     disable TTBR1 walks
        //   IRGN1  = 01        [25:24]
        //   ORGN1  = 01        [27:26]
        //   SH1    = 11        [29:28]
        //   TG1    = 10 (4K)   [31:30]
        //   IPS    = 010 (40b) [34:32]
        let tcr: u64 = 25
            | (1 << 8)
            | (1 << 10)
            | (3 << 12)
            | (25 << 16)
            | (1 << 23)
            | (1 << 24)
            | (1 << 26)
            | (3 << 28)
            | (2 << 30)
            | (2u64 << 32);

        core::arch::asm!(
            "msr mair_el1,  {mair}",
            "msr tcr_el1,   {tcr}",
            "msr ttbr0_el1, {ttbr0}",
            "isb",
            mair  = in(reg) mair,
            tcr   = in(reg) tcr,
            ttbr0 = in(reg) table_pa,
        );

        core::arch::asm!("tlbi vmalle1", "dsb sy", "isb");

        // Enable MMU (M) + D-cache (C) + I-cache (I), plus nTWI so the
        // EL0 idle thread's WFI doesn't trap. Leave SCTLR_EL1.SPAN and
        // friends at reset values. WXN stays off: the user window is
        // deliberately RWX until the M3 loader builds real address spaces.
        let mut sctlr: u64;
        core::arch::asm!("mrs {s}, sctlr_el1", s = out(reg) sctlr);
        sctlr |= (1 << 0) | (1 << 2) | (1 << 12) | (1 << 16);
        core::arch::asm!(
            "msr sctlr_el1, {s}",
            "isb",
            s = in(reg) sctlr,
        );
    }
}
