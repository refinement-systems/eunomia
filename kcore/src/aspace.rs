//! Address-space object *data* (spec §2.5). The page-table walker — table
//! allocation, `map`/`unmap`, the read-only lookup, ASID assignment, and all
//! the TLBI/DSB and int→PA→ptr work — stays in the `kernel` crate
//! (`kernel/src/aspace.rs`) until the phase-5 rewrite (plan §2.4); kcore
//! holds only the struct, the public constants, the error type, and the pure
//! size function, so the rest of the object machinery (frame mappings in
//! caps, the refcount census) can name an aspace without depending on the
//! walker.
//!
//! Mapping state lives in the frame cap, not here (§2.5): one mapping per
//! cap copy, and deleting or revoking the cap unmaps it (via
//! [`crate::env::Env::aspace_unmap`]).

use crate::cspace::ObjHeader;

pub const PAGE: u64 = 4096;
/// Lowest VA a process may map — everything below belongs to the shared
/// kernel entries.
pub const USER_VA_BASE: u64 = 0x8000_0000;
/// 39-bit VA space (T0SZ = 25).
pub const USER_VA_END: u64 = 1 << 39;

pub const PERM_W: u64 = 1 << 0;
pub const PERM_X: u64 = 1 << 1;
/// Device-nGnRnE mapping (MMIO windows). Never executable.
pub const PERM_DEVICE: u64 = 1 << 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    BadVa,
    AlreadyMapped,
    /// Table pool exhausted — donate a bigger pool (§2.5: one error path).
    NeedMemory,
}

/// The fields are `pub` so the kernel walker (a different crate until phase
/// 5) can drive them. Outside that walker — and never in kcore — they are
/// not to be touched: `l1`/`pool_*` are physical addresses, exactly the
/// int↔ptr territory kcore is built to exclude.
#[repr(C)]
pub struct AspaceObj {
    pub hdr: ObjHeader,
    pub asid: u16,
    pub l1: u64,        // PA of the 4 KiB L1 table
    pub pool_base: u64, // table pool (pool-at-creation)
    pub pool_pages: u64,
    pub pool_used: u64,
}

impl AspaceObj {
    /// Object footprint: header (padded to a page so the L1 is page-aligned)
    /// + L1 table + pool pages. Retype aligns the whole object to 4 KiB.
    /// Pure — moves with the struct so both crates and the harnesses agree
    /// on the size.
    pub const fn bytes_for(pool_pages: u64) -> usize {
        (PAGE + PAGE + pool_pages * PAGE) as usize
    }
}
