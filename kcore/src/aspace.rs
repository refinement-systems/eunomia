//! Address spaces and the page-table walker (spec §2.5).
//!
//! The AArch64 translation-table walk lives here as **safe Rust over the table
//! pool as an indexed slice** (plan §2.4): `map_in`/`unmap_in`/`range_mapped_in`
//! address tables by *pool index*, never by casting a descriptor's physical
//! address to a pointer. The on-hardware descriptor format is byte-identical to
//! the old kernel walker — a table descriptor still stores the table's PA in
//! its output-address field; the walker just converts that PA to a pool index
//! ([`pool_index`]) to follow it, and back ([`pa_of_table`]) to install it.
//!
//! The `kernel` crate keeps a thin shell (`kernel/src/aspace.rs`): it builds
//! the `&mut [[u64; 512]]` slice views from the `AspaceObj`'s PAs (the one
//! sanctioned int→pointer boundary), holds the ASID allocator and the boot
//! kernel-L1 copy, and implements the TLBI/barrier `Env` hooks the walker calls.
//!
//! Mapping state lives in the frame cap, not here (§2.5): one mapping per cap
//! copy, and deleting or revoking the cap unmaps it (via [`unmap_in`] behind
//! [`crate::store::Store::aspace_unmap`]).

use crate::cspace::ObjHeader;
use crate::store::Store;

pub const PAGE: u64 = 4096;
/// Lowest VA a process may map — everything below belongs to the shared
/// kernel entries.
pub const USER_VA_BASE: u64 = 0x8000_0000;
/// 39-bit VA space (T0SZ = 25).
pub const USER_VA_END: u64 = 1 << 39;

pub const PERM_W: u64 = 1 << 0;
pub const PERM_X: u64 = 1 << 1;
/// Device-nGnRnE mapping (MMIO windows). Never executable (enforced by
/// [`pte_encode`]).
pub const PERM_DEVICE: u64 = 1 << 2;

// ── descriptor bits (the on-hardware format; byte-identical to the old
//    kernel walker, plan §2.4) ──────────────────────────────────────────────
// `pub(crate)` so the §4.5 harnesses (`proofs::aspace`) assert against the
// named bits rather than magic numbers; not part of the crate's public API.
pub(crate) const DESC_TABLE: u64 = 0b11;
pub(crate) const DESC_PAGE: u64 = 0b11;
pub(crate) const AF: u64 = 1 << 10; // access flag
pub(crate) const UXN: u64 = 1 << 54; // unprivileged execute-never
pub(crate) const PXN: u64 = 1 << 53; // privileged execute-never (user pages: always set)
pub(crate) const SH_INNER: u64 = 0b11 << 8;
pub(crate) const SH_NONE: u64 = 0b00 << 8;
pub(crate) const AP_EL0_RW: u64 = 0b01 << 6;
pub(crate) const AP_EL0_RO: u64 = 0b11 << 6;
pub(crate) const ATTR_NORMAL: u64 = 0 << 2;
pub(crate) const ATTR_DEVICE: u64 = 1 << 2;
/// Output-address field of a descriptor: bits [47:12].
pub(crate) const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    BadVa,
    AlreadyMapped,
    /// Table pool exhausted — donate a bigger pool (§2.5: one error path).
    NeedMemory,
}

/// The fields are `pub` so the kernel shell can build slice views over the L1
/// table and the table pool from these PAs. Outside that shell — and never in
/// kcore's walker, which works in pool-index space — `l1`/`pool_*` are
/// physical addresses, the int↔ptr territory kcore is built to exclude.
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

// ── pure functions (plan §2.4 / §4.5) ──────────────────────────────────────

/// L1/L2/L3 indices of a VA (39-bit, 4 KiB granule, 512-entry tables).
pub fn l1_index(va: u64) -> usize {
    ((va >> 30) & 0x1FF) as usize
}
pub fn l2_index(va: u64) -> usize {
    ((va >> 21) & 0x1FF) as usize
}
pub fn l3_index(va: u64) -> usize {
    ((va >> 12) & 0x1FF) as usize
}

/// Is `[va, va + pages*PAGE)` a legal user mapping range? (page-aligned, inside
/// `[USER_VA_BASE, USER_VA_END)`). Used by [`map_in`] and proven by
/// `check_va_bounds`.
pub fn va_range_ok(va: u64, pages: u64) -> bool {
    va % PAGE == 0 && va >= USER_VA_BASE && va.saturating_add(pages.saturating_mul(PAGE)) <= USER_VA_END
}

/// PA of pool table `idx` and the inverse, as stored in a table descriptor's
/// output-address field — the byte-identical PA↔pool-index conversion.
fn pa_of_table(pool_base: u64, idx: usize) -> u64 {
    pool_base + (idx as u64) * PAGE
}

/// The pool index a table descriptor points at, or `None` if it addresses
/// outside the pool. Well-formed tables (everything [`map_in`] writes) always
/// yield `Some(idx)` with `idx < pool_len`; the bound keeps the walker total
/// for CBMC (the old pointer walk had no bound — and no provenance either).
fn pool_index(pool_base: u64, pool_len: usize, desc: u64) -> Option<usize> {
    let pa = desc & ADDR_MASK;
    if pa < pool_base {
        return None;
    }
    let idx = ((pa - pool_base) / PAGE) as usize;
    if idx >= pool_len {
        return None;
    }
    Some(idx)
}

/// Build a leaf (L3 page) descriptor. AF and PXN are unconditional (user pages
/// are never EL1-executable); a writable perm grants `AP_EL0_RW`, else RO;
/// **device memory is never executable** — `PERM_X` is ignored when
/// `PERM_DEVICE` is set (spec §2.5; the kernel walker honoured `PERM_X` here,
/// finding AS-1). The output address is masked to bits [47:12].
pub fn pte_encode(pa: u64, perms: u64) -> u64 {
    let ap = if perms & PERM_W != 0 { AP_EL0_RW } else { AP_EL0_RO };
    let device = perms & PERM_DEVICE != 0;
    let xn = if perms & PERM_X != 0 && !device { 0 } else { UXN };
    let (attr, sh) = if device { (ATTR_DEVICE, SH_NONE) } else { (ATTR_NORMAL, SH_INNER) };
    (pa & ADDR_MASK) | DESC_PAGE | AF | sh | attr | ap | xn | PXN
}

/// The output PA of a leaf descriptor (the inverse of [`pte_encode`]'s address
/// field).
pub fn pte_output_pa(pte: u64) -> u64 {
    pte & ADDR_MASK
}

// ── the walker, over the table pool as a slice ──────────────────────────────

/// Grab the next free pool table, zero it, and return its index. The zeroing
/// matches the old `alloc_table`'s `write_bytes(.., 0, PAGE)` so a freshly
/// allocated table starts empty (`check_pool_accounting`).
fn alloc_table(pool: &mut [[u64; 512]], pool_used: &mut u64) -> Result<usize, MapError> {
    if *pool_used as usize >= pool.len() {
        return Err(MapError::NeedMemory);
    }
    let idx = *pool_used as usize;
    *pool_used += 1;
    pool[idx] = [0u64; 512];
    Ok(idx)
}

/// Walk to `va`'s L3 entry, allocating the L2/L3 tables if absent. Returns the
/// `(pool index, entry index)` of the L3 slot. Mirrors the old `l3_slot`.
fn walk_alloc(
    l1: &mut [u64; 512],
    pool: &mut [[u64; 512]],
    pool_used: &mut u64,
    pool_base: u64,
    va: u64,
) -> Result<(usize, usize), MapError> {
    let l1i = l1_index(va);
    if l1[l1i] == 0 {
        let idx = alloc_table(pool, pool_used)?;
        l1[l1i] = pa_of_table(pool_base, idx) | DESC_TABLE;
    }
    let l2_idx = pool_index(pool_base, pool.len(), l1[l1i]).ok_or(MapError::NeedMemory)?;
    let l2i = l2_index(va);
    if pool[l2_idx][l2i] == 0 {
        let idx = alloc_table(pool, pool_used)?;
        pool[l2_idx][l2i] = pa_of_table(pool_base, idx) | DESC_TABLE;
    }
    let l3_idx = pool_index(pool_base, pool.len(), pool[l2_idx][l2i]).ok_or(MapError::NeedMemory)?;
    Ok((l3_idx, l3_index(va)))
}

/// Read-only walk to `va`'s L3 entry. `None` if any intermediate table is
/// absent. Mirrors the old `l3_lookup`. `pub(crate)` so the §4.5 harnesses can
/// read the installed leaf directly.
pub(crate) fn lookup(l1: &[u64; 512], pool: &[[u64; 512]], pool_base: u64, va: u64) -> Option<(usize, usize)> {
    let l1e = l1[l1_index(va)];
    if l1e & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let l2_idx = pool_index(pool_base, pool.len(), l1e)?;
    let l2e = pool[l2_idx][l2_index(va)];
    if l2e & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let l3_idx = pool_index(pool_base, pool.len(), l2e)?;
    Some((l3_idx, l3_index(va)))
}

/// Map `pages` frames at `pa` into `[va, …)`. Two-pass (like the old `map`):
/// pass 1 allocates the tables along the range and rejects any already-mapped
/// page; pass 2 writes the leaves. Because pass 1 walked the whole range, pass
/// 2 allocates nothing and cannot return `NeedMemory` (proven by
/// `check_map_model`). Issues the post-map barrier through `store`.
///
/// pre:  `pool` is the aspace's table pool, `pool_used` its high-water mark,
///       `pool_base` its PA; `l1` the aspace's L1 table.
/// post: PTEs installed or an atomic failure; `*pool_used` only ever grows.
pub fn map_in<S: Store>(
    l1: &mut [u64; 512],
    pool: &mut [[u64; 512]],
    pool_used: &mut u64,
    pool_base: u64,
    pa: u64,
    va: u64,
    pages: u64,
    perms: u64,
    store: &mut S,
) -> Result<(), MapError> {
    if !va_range_ok(va, pages) {
        return Err(MapError::BadVa);
    }
    for i in 0..pages {
        let (l3, e) = walk_alloc(l1, pool, pool_used, pool_base, va + i * PAGE)?;
        if pool[l3][e] != 0 {
            return Err(MapError::AlreadyMapped);
        }
    }
    for i in 0..pages {
        let (l3, e) = walk_alloc(l1, pool, pool_used, pool_base, va + i * PAGE)?;
        pool[l3][e] = pte_encode(pa + i * PAGE, perms);
    }
    store.barrier_after_map();
    Ok(())
}

/// Unmap `pages` frames at `va`, invalidating each cleared page's TLB entry
/// through `store`. Mirrors the old `unmap` (clear + per-page TLBI wherever the
/// L3 table exists, then a single trailing barrier).
pub fn unmap_in<S: Store>(
    l1: &[u64; 512],
    pool: &mut [[u64; 512]],
    pool_base: u64,
    asid: u16,
    va: u64,
    pages: u64,
    store: &mut S,
) {
    for i in 0..pages {
        let page_va = va + i * PAGE;
        if let Some((l3, e)) = lookup(l1, pool, pool_base, page_va) {
            pool[l3][e] = 0;
            store.tlb_invalidate_page(asid, page_va);
        }
    }
    store.barrier_after_unmap();
}

/// Is `[va, va+len)` fully mapped (and writable, if asked)? The predicate the
/// syscall layer trusts before dereferencing user pointers, so it is total
/// over all `(va, len)` including `len == 0` and the `va + len` overflow edge.
pub fn range_mapped_in(
    l1: &[u64; 512],
    pool: &[[u64; 512]],
    pool_base: u64,
    va: u64,
    len: u64,
    write: bool,
) -> bool {
    if len == 0 {
        return va >= USER_VA_BASE && va < USER_VA_END;
    }
    let Some(end) = va.checked_add(len) else {
        return false;
    };
    if va < USER_VA_BASE || end > USER_VA_END {
        return false;
    }
    let mut page = va & !(PAGE - 1);
    while page < end {
        match lookup(l1, pool, pool_base, page) {
            Some((l3, e)) if pool[l3][e] != 0 => {
                // AP[1:0] == 0b01 is EL0 read-write; 0b11 is read-only.
                if write && (pool[l3][e] >> 6) & 0b11 != 0b01 {
                    return false;
                }
            }
            _ => return false,
        }
        page += PAGE;
    }
    true
}
