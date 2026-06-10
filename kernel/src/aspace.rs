//! Address-space objects (spec §2.5): a per-process translation table
//! tree created from donated untyped, pool-at-creation — intermediate
//! tables are drawn from the aspace's own pool, NEED_MEMORY when it runs
//! dry, and teardown returns the pool with the object.
//!
//! Layout of one process's view:
//!   L1[0]   device GiB        — shared kernel entry, EL1-only
//!   L1[1]   kernel DRAM table — shared kernel entry (incl. the legacy
//!           identity user window used by the idle thread)
//!   L1[2..] process-private   — user mappings (ELF base 0x8000_0000)
//!
//! The kernel is thus mapped in every aspace (exception vectors keep
//! working across TTBR0 switches); user mappings carry AP_EL0 and PXN,
//! kernel entries are EL1-only, so the split is enforced per entry.
//!
//! Mapping state lives in the frame cap, not here (§2.5): one mapping
//! per cap copy, and deleting or revoking the cap unmaps it. That single
//! rule gives shared memory the same revocation story as everything
//! else.

use crate::cspace::ObjHeader;
use core::ptr;

pub const PAGE: u64 = 4096;
/// Lowest VA a process may map — everything below belongs to the shared
/// kernel entries.
pub const USER_VA_BASE: u64 = 0x8000_0000;
/// 39-bit VA space (T0SZ = 25).
pub const USER_VA_END: u64 = 1 << 39;

pub const PERM_W: u64 = 1 << 0;
pub const PERM_X: u64 = 1 << 1;

const DESC_TABLE: u64 = 0b11;
const DESC_PAGE: u64 = 0b11;
const AF: u64 = 1 << 10;
const UXN: u64 = 1 << 54;
const PXN: u64 = 1 << 53;
const SH_INNER: u64 = 0b11 << 8;
const AP_EL0_RW: u64 = 0b01 << 6;
const AP_EL0_RO: u64 = 0b11 << 6;
const ATTR_NORMAL: u64 = 0 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    BadVa,
    AlreadyMapped,
    /// Table pool exhausted — donate a bigger pool (§2.5: one error path).
    NeedMemory,
}

#[repr(C)]
pub struct AspaceObj {
    pub hdr: ObjHeader,
    pub asid: u16,
    l1: u64,        // PA of the 4 KiB L1 table
    pool_base: u64, // table pool (pool-at-creation)
    pool_pages: u64,
    pool_used: u64,
}

static mut NEXT_ASID: u16 = 1;

impl AspaceObj {
    /// Object footprint: header (padded to a page so the L1 is
    /// page-aligned) + L1 table + pool pages. Retype aligns the whole
    /// object to 4 KiB.
    pub const fn bytes_for(pool_pages: u64) -> usize {
        (PAGE + PAGE + pool_pages * PAGE) as usize
    }

    /// pre:  `this` points at bytes_for(pool_pages) of 4 KiB-aligned
    ///       writable memory.
    /// post: L1 holds the shared kernel entries; pool empty; fresh ASID.
    pub unsafe fn init(this: *mut AspaceObj, pool_pages: u64) {
        let base = this as u64;
        let l1 = base + PAGE;
        ptr::write_bytes(l1 as *mut u8, 0, PAGE as usize);
        // Shared kernel entries from the boot identity map.
        let kernel_l1 = crate::mmu::kernel_l1();
        (l1 as *mut u64).write((kernel_l1 as *const u64).read());
        (l1 as *mut u64).add(1).write((kernel_l1 as *const u64).add(1).read());

        let asid = NEXT_ASID;
        NEXT_ASID = NEXT_ASID.wrapping_add(1);
        if NEXT_ASID == 0 {
            // 8-bit-safe wrap: flush everything once per 64k spawns.
            NEXT_ASID = 1;
            core::arch::asm!("tlbi vmalle1", "dsb sy", "isb");
        }

        this.write(AspaceObj {
            hdr: ObjHeader { refs: 1 },
            asid,
            l1,
            pool_base: base + 2 * PAGE,
            pool_pages,
            pool_used: 0,
        });
    }

    pub unsafe fn ttbr0(this: *mut AspaceObj) -> u64 {
        (*this).l1 | ((*this).asid as u64) << 48
    }

    unsafe fn alloc_table(this: *mut AspaceObj) -> Result<u64, MapError> {
        if (*this).pool_used == (*this).pool_pages {
            return Err(MapError::NeedMemory);
        }
        let pa = (*this).pool_base + (*this).pool_used * PAGE;
        (*this).pool_used += 1;
        ptr::write_bytes(pa as *mut u8, 0, PAGE as usize);
        Ok(pa)
    }

    /// Walk (allocating intermediate tables) to the L3 slot for `va`.
    unsafe fn l3_slot(this: *mut AspaceObj, va: u64) -> Result<*mut u64, MapError> {
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        let l1e = ((*this).l1 as *mut u64).add(l1_idx as usize);
        if *l1e == 0 {
            *l1e = Self::alloc_table(this)? | DESC_TABLE;
        }
        let l2 = (*l1e & 0x0000_FFFF_FFFF_F000) as *mut u64;
        let l2e = l2.add(l2_idx as usize);
        if *l2e == 0 {
            *l2e = Self::alloc_table(this)? | DESC_TABLE;
        }
        let l3 = (*l2e & 0x0000_FFFF_FFFF_F000) as *mut u64;
        Ok(l3.add(l3_idx as usize))
    }

    /// Map `pages` frames starting at `pa` to `va` with EL0 permissions.
    ///
    /// pre:  va page-aligned, in [USER_VA_BASE, USER_VA_END).
    /// post: PTEs installed; no TLB shootdown needed (they were invalid).
    pub unsafe fn map(
        this: *mut AspaceObj,
        pa: u64,
        va: u64,
        pages: u64,
        perms: u64,
    ) -> Result<(), MapError> {
        if va % PAGE != 0
            || va < USER_VA_BASE
            || va.saturating_add(pages * PAGE) > USER_VA_END
        {
            return Err(MapError::BadVa);
        }
        let ap = if perms & PERM_W != 0 { AP_EL0_RW } else { AP_EL0_RO };
        let xn = if perms & PERM_X != 0 { 0 } else { UXN };
        let attrs = DESC_PAGE | AF | SH_INNER | ATTR_NORMAL | ap | xn | PXN;
        // First pass: nothing may already be mapped (no silent remap).
        for i in 0..pages {
            let slot = Self::l3_slot(this, va + i * PAGE)?;
            if *slot != 0 {
                return Err(MapError::AlreadyMapped);
            }
        }
        for i in 0..pages {
            let slot = Self::l3_slot(this, va + i * PAGE)?;
            *slot = (pa + i * PAGE) | attrs;
        }
        core::arch::asm!("dsb ishst");
        Ok(())
    }

    /// Read-only walk: the L3 slot for `va` if every level exists.
    unsafe fn l3_lookup(this: *mut AspaceObj, va: u64) -> Option<*mut u64> {
        let l1e = ((*this).l1 as *mut u64).add(((va >> 30) & 0x1FF) as usize);
        if *l1e & DESC_TABLE != DESC_TABLE {
            return None;
        }
        let l2 = (*l1e & 0x0000_FFFF_FFFF_F000) as *mut u64;
        let l2e = l2.add(((va >> 21) & 0x1FF) as usize);
        if *l2e & DESC_TABLE != DESC_TABLE {
            return None;
        }
        let l3 = (*l2e & 0x0000_FFFF_FFFF_F000) as *mut u64;
        Some(l3.add(((va >> 12) & 0x1FF) as usize))
    }

    /// Unmap (frame-cap deletion path). Invalidates by ASID+VA.
    pub unsafe fn unmap(this: *mut AspaceObj, va: u64, pages: u64) {
        for i in 0..pages {
            let page_va = va + i * PAGE;
            if let Some(slot) = Self::l3_lookup(this, page_va) {
                *slot = 0;
                // TLBI VAE1: [63:48] ASID, [43:0] VA[55:12].
                let arg = (((*this).asid as u64) << 48) | ((page_va >> 12) & 0xFFF_FFFF_FFFF);
                core::arch::asm!("tlbi vae1, {v}", v = in(reg) arg);
            }
        }
        core::arch::asm!("dsb ish", "isb");
    }

    /// Is [va, va+len) fully mapped (and writable, if asked)? Used by the
    /// syscall layer to validate user pointers before the kernel
    /// dereferences them through the process's own translation.
    pub unsafe fn range_mapped(this: *mut AspaceObj, va: u64, len: u64, write: bool) -> bool {
        if len == 0 {
            return va >= USER_VA_BASE && va < USER_VA_END;
        }
        let Some(end) = va.checked_add(len) else { return false };
        if va < USER_VA_BASE || end > USER_VA_END {
            return false;
        }
        let mut page = va & !(PAGE - 1);
        while page < end {
            match Self::l3_lookup(this, page) {
                Some(slot) if *slot != 0 => {
                    if write && (*slot >> 6) & 0b11 != 0b01 {
                        return false;
                    }
                }
                _ => return false,
            }
            page += PAGE;
        }
        true
    }
}

/// pre: refs == 0. The memory (tables included) returns to the donor
/// untyped via revoke; nothing to do but note that mapped frames keep
/// their own cap-side state and were unmapped when their caps died.
pub unsafe fn destroy_aspace(_a: *mut AspaceObj) {}
