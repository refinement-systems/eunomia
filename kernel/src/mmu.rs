// Minimal identity-map MMU setup for AArch64 EL1, QEMU virt machine.
//
// Translation regime: TTBR0, 39-bit VA (T0SZ=25), 4K granule, level-1
// start. Each level-1 entry covers 1 GiB.
//
//  Entry 0: PA 0x0000_0000–0x3FFF_FFFF  device nGnRnE   (MMIO, UART, GIC)
//  Entry 1: PA 0x4000_0000–0x7FFF_FFFF  normal WB       (DRAM)
//
// MAIR_EL1:
//   Attr0 = 0xFF  normal outer/inner write-back, read/write allocate
//   Attr1 = 0x00  device nGnRnE
//
// TCR_EL1: T0SZ=25, TG0=4K, SH0=inner-shareable, IRGN/ORGN=write-back WA,
//          EPD1=1 (disable TTBR1 walk), IPS=40-bit PA.

// Level-1 table: 512 entries × 8 bytes = 4 KiB, must be 4 KiB-aligned.
// Placing it in a #[repr(align(4096))] wrapper satisfies that.
#[repr(C, align(4096))]
struct L1Table([u64; 512]);

static mut L1_TABLE: L1Table = L1Table([0u64; 512]);

// Block descriptor bits.
const BLOCK: u64 = 0b01; // valid block at level 1
const AF: u64 = 1 << 10; // access flag (must be set or access fault fires)
const UXN: u64 = 1 << 54; // EL0 execute-never

// AttrIdx field is bits [4:2].
const ATTR_NORMAL: u64 = 0 << 2; // MAIR Attr0 = normal WB
const ATTR_DEVICE: u64 = 1 << 2; // MAIR Attr1 = device nGnRnE

// SH field is bits [9:8].
const SH_INNER: u64 = 0b11 << 8; // inner-shareable (normal memory)
const SH_NONE: u64 = 0b00 << 8; // non-shareable (device memory)

// AP field bits [7:6]: 00 = EL1 R/W, EL0 no access (what we want).
// Leave AP=00 (zero); no constant needed.

const DESC_DEVICE: u64 = UXN | AF | SH_NONE | ATTR_DEVICE | BLOCK;
const DESC_NORMAL: u64 = UXN | AF | SH_INNER | ATTR_NORMAL | BLOCK;

pub fn init() {
    unsafe {
        // Entry 0: device memory — PA 0x0000_0000.
        L1_TABLE.0[0] = 0x0000_0000 | DESC_DEVICE;
        // Entry 1: normal memory — PA 0x4000_0000 (bit 30 set).
        L1_TABLE.0[1] = 0x4000_0000 | DESC_NORMAL;

        let table_pa = core::ptr::addr_of!(L1_TABLE.0) as u64;

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

        // Flush stale TLB entries before enabling the MMU.
        core::arch::asm!("tlbi vmalle1", "dsb sy", "isb");

        // Enable MMU (M=1) + D-cache (C=1) + I-cache (I=1) in SCTLR_EL1.
        let mut sctlr: u64;
        core::arch::asm!("mrs {s}, sctlr_el1", s = out(reg) sctlr);
        sctlr |= (1 << 0) | (1 << 2) | (1 << 12);
        core::arch::asm!(
            "msr sctlr_el1, {s}",
            "isb",
            s = in(reg) sctlr,
        );
    }
}
