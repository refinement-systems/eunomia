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
use vstd::prelude::*;

verus! {

// The geometry/permission/descriptor consts live inside `verus!{}` so the §4.5
// `pte_encode`/`va_range_ok` contracts can name them (the `channel::MSG_PAYLOAD`
// idiom — a const must be in a `verus!{}` block to be spec-visible; it erases to
// a byte-identical `pub`/`pub(crate) const`, so the kernel's glob re-export and
// the aarch64 build are unchanged).

pub const PAGE: u64 = 4096;
/// `PAGE - 1` as a `u64` — page-offset / alignment mask. A named const so the
/// page-align-down `va & !PAGE_MASK` and the alignment test `p & PAGE_MASK == 0`
/// stay `u64` bitwise ops in spec position (a spec `PAGE - 1` is `int`, on which
/// `!`/`&` are undefined).
pub const PAGE_MASK: u64 = PAGE - 1;
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
// `pub(crate)` so the in-module `tests` assert against the named bits rather
// than magic numbers; not part of the crate's public API.
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

// vstd specs `saturating_add`/`saturating_sub` but not `saturating_mul`
// (`std_specs/num.rs`); `va_range_ok` needs it. Trust the standard saturating
// semantics (the `untyped.rs` `checked_next_multiple_of` precedent).
pub assume_specification[ u64::saturating_mul ](x: u64, y: u64) -> u64
    returns (if x * y > u64::MAX { u64::MAX } else { (x * y) as u64 });

} // verus!

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

// ── pure functions (plan §2.4 / §4.5), verified (plan
//    doc/plans/3_verus-rewrite_phase5-detail.md §5b) ──────────────────────────

verus! {

/// L1/L2/L3 indices of a VA (39-bit, 4 KiB granule, 512-entry tables). The
/// `spec_*` mirrors make the indices spec-visible (`when_used_as_spec`) so the
/// [`lemma_user_va_l1_index`] corollary — and 5c's page-table spec walk — can
/// name them.
pub open spec fn spec_l1_index(va: u64) -> usize {
    ((va >> 30) & 0x1FF) as usize
}
#[verifier::when_used_as_spec(spec_l1_index)]
pub fn l1_index(va: u64) -> (r: usize)
    ensures r == spec_l1_index(va), r < 512,
{
    assert(((va >> 30) & 0x1FF) < 512) by (bit_vector);
    ((va >> 30) & 0x1FF) as usize
}
pub open spec fn spec_l2_index(va: u64) -> usize {
    ((va >> 21) & 0x1FF) as usize
}
#[verifier::when_used_as_spec(spec_l2_index)]
pub fn l2_index(va: u64) -> (r: usize)
    ensures r == spec_l2_index(va), r < 512,
{
    assert(((va >> 21) & 0x1FF) < 512) by (bit_vector);
    ((va >> 21) & 0x1FF) as usize
}
pub open spec fn spec_l3_index(va: u64) -> usize {
    ((va >> 12) & 0x1FF) as usize
}
#[verifier::when_used_as_spec(spec_l3_index)]
pub fn l3_index(va: u64) -> (r: usize)
    ensures r == spec_l3_index(va), r < 512,
{
    assert(((va >> 12) & 0x1FF) < 512) by (bit_vector);
    ((va >> 12) & 0x1FF) as usize
}

/// Is `[va, va + pages*PAGE)` a legal user mapping range? (page-aligned, inside
/// `[USER_VA_BASE, USER_VA_END)`). Used by [`map_in`].
///
/// Verified **total** and fully functional: the result is exactly the integer
/// predicate `va % PAGE == 0 ∧ va ≥ USER_VA_BASE ∧ va + pages·PAGE ≤
/// USER_VA_END`. The saturating arithmetic equals the int condition because
/// `USER_VA_END = 2³⁹ ≪ 2⁶⁴`, so any saturation forces the range out of bounds.
pub fn va_range_ok(va: u64, pages: u64) -> (ok: bool)
    ensures
        ok == (va % PAGE == 0 && va >= USER_VA_BASE
            && (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int),
{
    assert(USER_VA_END == 0x80_0000_0000) by (compute);
    va % PAGE == 0 && va >= USER_VA_BASE && va.saturating_add(pages.saturating_mul(PAGE)) <= USER_VA_END
}

/// Build a leaf (L3 page) descriptor. AF and PXN are unconditional (user pages
/// are never EL1-executable); a writable perm grants `AP_EL0_RW`, else RO;
/// **device memory is never executable** — `PERM_X` is ignored when
/// `PERM_DEVICE` is set (spec §2.5; the kernel walker honoured `PERM_X` here,
/// finding AS-1). The output address is masked to bits [47:12].
///
/// Verified — the §2.5/§4.5 **isolation theorem**, ∀ `(pa, perms)`: AF + PXN
/// always set; `AP` grants EL0 write iff `PERM_W`; device is non-executable
/// (`UXN`) + `SH_NONE` + `ATTR_DEVICE` even when `PERM_X` is set (the AS-1 fix);
/// a non-device non-`X` page is `UXN`; the address field round-trips. The
/// security corollary — no `perms` yields an EL1-writable or EL0-kernel-
/// executable page — is the conjunction of PXN-always + the `AP`/`UXN` clauses.
///
/// `pub(crate)`: the contract names the crate-internal descriptor bits, and the
/// public aspace surface is `map_in`/`unmap_in`/`range_mapped_in` (which call
/// this), not the leaf encoder directly.
pub(crate) fn pte_encode(pa: u64, perms: u64) -> (pte: u64)
    ensures
        pte & AF == AF,
        pte & PXN == PXN,
        pte & ADDR_MASK == pa & ADDR_MASK,
        perms & PERM_W != 0 ==> (pte >> 6) & 0b11 == 0b01,
        perms & PERM_W == 0 ==> (pte >> 6) & 0b11 == 0b11,
        perms & PERM_DEVICE != 0 ==> pte & UXN == UXN,
        perms & PERM_DEVICE != 0 ==> pte & SH_INNER == 0,
        perms & PERM_DEVICE != 0 ==> pte & ATTR_DEVICE == ATTR_DEVICE,
        (perms & PERM_DEVICE == 0 && perms & PERM_X == 0) ==> pte & UXN == UXN,
{
    let ap = if perms & PERM_W != 0 { AP_EL0_RW } else { AP_EL0_RO };
    let device = perms & PERM_DEVICE != 0;
    let xn = if perms & PERM_X != 0 && !device { 0 } else { UXN };
    let (attr, sh) = if device { (ATTR_DEVICE, SH_NONE) } else { (ATTR_NORMAL, SH_INNER) };
    let pte = (pa & ADDR_MASK) | DESC_PAGE | AF | sh | attr | ap | xn | PXN;
    proof {
        lemma_pte_bits(pa, ap, sh, attr, xn, pte);
    }
    pte
}

/// The output PA of a leaf descriptor (the inverse of [`pte_encode`]'s address
/// field). Composed with `pte_encode`'s address-field `ensures` it round-trips:
/// `pte_output_pa(pte_encode(pa, perms)) == pa & ADDR_MASK` (host-tested).
/// `pub(crate)` for the same reason as [`pte_encode`] (names `ADDR_MASK`).
// The decoder half of the encode/decode pair: exercised by the round-trip host
// test (`cfg(test)`) and available to the walker; no non-test caller yet.
#[allow(dead_code)]
pub(crate) fn pte_output_pa(pte: u64) -> (r: u64)
    ensures r == pte & ADDR_MASK,
{
    pte & ADDR_MASK
}

/// The PTE field-extraction facts, isolated into one `bit_vector` step (the
/// `untyped.rs` §2.5 discipline). The descriptor-bit masks are pairwise
/// disjoint, so each field of `pte` is independent of the others; the value
/// constraints on `ap`/`sh`/`attr`/`xn` (the `pte_encode` if-arms) plus the
/// const literals (fixed via `compute`) make the bit-blast a tautology.
proof fn lemma_pte_bits(pa: u64, ap: u64, sh: u64, attr: u64, xn: u64, pte: u64)
    requires
        pte == (pa & ADDR_MASK) | DESC_PAGE | AF | sh | attr | ap | xn | PXN,
        ap == AP_EL0_RW || ap == AP_EL0_RO,
        sh == SH_NONE || sh == SH_INNER,
        attr == ATTR_NORMAL || attr == ATTR_DEVICE,
        xn == 0 || xn == UXN,
    ensures
        pte & AF == AF,
        pte & PXN == PXN,
        pte & ADDR_MASK == pa & ADDR_MASK,
        ap == AP_EL0_RW ==> (pte >> 6) & 0b11 == 0b01,
        ap == AP_EL0_RO ==> (pte >> 6) & 0b11 == 0b11,
        xn == UXN ==> pte & UXN == UXN,
        sh == SH_NONE ==> pte & SH_INNER == 0,
        attr == ATTR_DEVICE ==> pte & ATTR_DEVICE == ATTR_DEVICE,
{
    // Pin every named const to its literal so the bit-vector solver reasons over
    // concrete masks (the bound vars `ap`/`sh`/`attr`/`xn` stay symbolic but are
    // constrained to two values each).
    assert(AF == 0x400) by (compute);
    assert(PXN == 0x20_0000_0000_0000) by (compute);
    assert(ADDR_MASK == 0xFFFF_FFFF_F000) by (compute);
    assert(AP_EL0_RW == 0x40) by (compute);
    assert(AP_EL0_RO == 0xC0) by (compute);
    assert(SH_NONE == 0) by (compute);
    assert(SH_INNER == 0x300) by (compute);
    assert(ATTR_NORMAL == 0) by (compute);
    assert(ATTR_DEVICE == 0x4) by (compute);
    assert(UXN == 0x40_0000_0000_0000) by (compute);
    assert(DESC_PAGE == 0x3) by (compute);
    assert(
        pte & AF == AF
        && pte & PXN == PXN
        && pte & ADDR_MASK == pa & ADDR_MASK
        && (ap == AP_EL0_RW ==> (pte >> 6) & 0b11 == 0b01)
        && (ap == AP_EL0_RO ==> (pte >> 6) & 0b11 == 0b11)
        && (xn == UXN ==> pte & UXN == UXN)
        && (sh == SH_NONE ==> pte & SH_INNER == 0)
        && (attr == ATTR_DEVICE ==> pte & ATTR_DEVICE == ATTR_DEVICE)
    ) by (bit_vector)
        requires
            pte == (pa & ADDR_MASK) | DESC_PAGE | AF | sh | attr | ap | xn | PXN,
            ap == AP_EL0_RW || ap == AP_EL0_RO,
            sh == SH_NONE || sh == SH_INNER,
            attr == ATTR_NORMAL || attr == ATTR_DEVICE,
            xn == 0 || xn == UXN,
            AF == 0x400,
            PXN == 0x20_0000_0000_0000,
            ADDR_MASK == 0xFFFF_FFFF_F000,
            AP_EL0_RW == 0x40,
            AP_EL0_RO == 0xC0,
            SH_NONE == 0,
            SH_INNER == 0x300,
            ATTR_NORMAL == 0,
            ATTR_DEVICE == 0x4,
            UXN == 0x40_0000_0000_0000,
            DESC_PAGE == 0x3;
}

/// A user mapping never touches the two shared kernel L1 entries (indices 0/1):
/// every page VA in `[USER_VA_BASE, USER_VA_END)` has `l1_index ≥ 2` (the §4.5
/// theorem, consumed by 5d's `walk_alloc`). Stated over the half-open
/// mapped-page range, so the `pages == 0` edge (`va` can equal `USER_VA_END`) is
/// excluded by construction.
pub proof fn lemma_user_va_l1_index(va: u64)
    requires USER_VA_BASE <= va < USER_VA_END,
    ensures l1_index(va) >= 2,
{
    assert(USER_VA_END == 0x80_0000_0000) by (compute);
    assert(((va >> 30) & 0x1FF) >= 2 && ((va >> 30) & 0x1FF) < 0x200) by (bit_vector)
        requires 0x8000_0000u64 <= va, va < 0x80_0000_0000u64;
    assert(((va >> 30) & 0x1FF) as usize >= 2);
}

} // verus!

/// PA of pool table `idx` and the inverse, as stored in a table descriptor's
/// output-address field — the byte-identical PA↔pool-index conversion.
fn pa_of_table(pool_base: u64, idx: usize) -> u64 {
    pool_base + (idx as u64) * PAGE
}

// ── the page-table partial-map model (plan
//    doc/plans/3_verus-rewrite_phase5-detail.md §1.2/§5c) ──────────────────────
//
// The first kcore Verus reasoning over concrete Rust slices: the L1 table is a
// `&[u64; 512]` (view `Seq<u64>`) and the table pool a `&[[u64; 512]]` (view
// `Seq<[u64; 512]>`). The model is defined over the **natural** slice view
// `Seq<[u64; 512]>` (i.e. `pool@`), not `Seq<Seq<u64>>` — `pool@[i][j]` already
// resolves to the array `spec_index`, matching the exec read `pool[i][j]`
// directly, so there is no `deep_view` conversion to thread (doc 38 §2).

verus! {

broadcast use {vstd::slice::group_slice_axioms, vstd::array::group_array_axioms};

/// The pool index a table descriptor points at (the spec mirror of
/// [`pool_index`]): `None` if the descriptor's output address is below the pool
/// or `pool_len` tables or more in. The model's addressing primitive.
pub closed spec fn pool_index_spec(pool_base: u64, pool_len: nat, desc: u64) -> Option<nat> {
    let pa = (desc & ADDR_MASK) as int;
    let base = pool_base as int;
    if pa < base {
        None
    } else if ((pa - base) / (PAGE as int)) as nat >= pool_len {
        None
    } else {
        Some(((pa - base) / (PAGE as int)) as nat)
    }
}

/// The leaf PTE that a read-only walk of `va` resolves to, or `None` if any
/// level is absent (the spec analog of [`lookup`] + the `pool[l3][e]` read). The
/// §4.5 ghost `Map<va_page, pte>` in pointwise form (the per-node idiom, doc 27
/// §3). Returns the leaf *value* (which [`range_mapped_in`] tests for `!= 0` and
/// writability), so `pt_lookup(va) == Some(0)` means "tables present, page empty".
pub closed spec fn pt_lookup(l1: Seq<u64>, pool: Seq<[u64; 512]>, pool_base: u64, va: u64) -> Option<u64> {
    let l1e = l1[spec_l1_index(va) as int];
    if l1e & DESC_TABLE != DESC_TABLE {
        None
    } else {
        match pool_index_spec(pool_base, pool.len(), l1e) {
            None => None,
            Some(l2_idx) => {
                let l2e = pool[l2_idx as int][spec_l2_index(va) as int];
                if l2e & DESC_TABLE != DESC_TABLE {
                    None
                } else {
                    match pool_index_spec(pool_base, pool.len(), l2e) {
                        None => None,
                        Some(l3_idx) => Some(pool[l3_idx as int][spec_l3_index(va) as int]),
                    }
                }
            }
        }
    }
}

/// Is the page at `va` present and (if `write`) writable? The per-page predicate
/// [`range_mapped_in`]'s `forall` ranges over (the 5b `(pte >> 6) & 0b11 == 0b01`
/// writability bridge).
pub closed spec fn page_ok(l1: Seq<u64>, pool: Seq<[u64; 512]>, pool_base: u64, page: u64, write: bool) -> bool {
    match pt_lookup(l1, pool, pool_base, page) {
        Some(pte) => pte != 0 && (write ==> (pte >> 6) & 0b11 == 0b01),
        None => false,
    }
}

/// Table-pool well-formedness — the `chan_wf`/`notif_wf`/`timer_wf` analog for
/// the page table. **Designed here (5c), consumed by 5d/5e** — no 5c op
/// establishes or preserves it (the read-only [`range_mapped_in`] does not need
/// it), so it is a definition only in this sub-phase; 5d validates/refines it
/// against `map_in` (the "add the clause when the op needs it" discipline, doc
/// 27 §3). Three aspects (detail §1.2):
///   (a) accounting — pool length and the used-tables high-water mark;
///   (b) closure — every present table descriptor (in `l1` and in used tables)
///       resolves to a pool index `< pool_used`;
///   (c) tree-shape / no-aliasing — distinct present table descriptors point to
///       distinct pool indices (the page table is a tree, not a DAG; the
///       load-bearing locality invariant for 5d's leaf-write frame lemma).
pub closed spec fn pt_wf(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pool_used: nat,
    pool_len: nat,
) -> bool {
    // (a) accounting.
    &&& l1.len() == 512
    &&& pool.len() == pool_len
    &&& pool_used <= pool_len
    // (b) closure: every present L1 table descriptor lands inside the used pool.
    &&& (forall|i: int| #![trigger l1[i]]
            0 <= i < 512 && l1[i] & DESC_TABLE == DESC_TABLE
                ==> pool_index_resolves(pool_base, pool_len, pool_used, l1[i]))
    // (b) closure: ditto for descriptors held in any used table (the L2 level).
    &&& (forall|t: int, e: int| #![trigger pool[t][e]]
            0 <= t < pool_used && 0 <= e < 512 && pool[t][e] & DESC_TABLE == DESC_TABLE
                ==> pool_index_resolves(pool_base, pool_len, pool_used, pool[t][e]))
    // (c) tree-shape / no-aliasing: distinct present L1 descriptors → distinct
    //     pool indices (extended to the inner levels in 5d as `map_in` pays for it).
    &&& (forall|i: int, j: int|
            0 <= i < 512 && 0 <= j < 512 && i != j
                && l1[i] & DESC_TABLE == DESC_TABLE && l1[j] & DESC_TABLE == DESC_TABLE
                ==> pool_index_spec(pool_base, pool_len, l1[i]) != pool_index_spec(pool_base, pool_len, l1[j]))
}

/// `desc` resolves (as a table descriptor) to a pool index strictly inside the
/// used region — the closure obligation `pt_wf` repeats per level.
pub closed spec fn pool_index_resolves(pool_base: u64, pool_len: nat, pool_used: nat, desc: u64) -> bool {
    match pool_index_spec(pool_base, pool_len, desc) {
        Some(idx) => idx < pool_used,
        None => false,
    }
}

/// The pool index a table descriptor points at, or `None` if it addresses
/// outside the pool. Well-formed tables (everything [`map_in`] writes) always
/// yield `Some(idx)` with `idx < pool_len`; the bound keeps the walker total
/// (the old pointer walk had no bound — and no provenance either).
///
/// Verified equal to [`pool_index_spec`]. The comparison is done in `u64`
/// **before** the `as usize` cast (`off < pool_len as u64 <= usize::MAX`), so the
/// cast is provably lossless without pinning `usize`'s width (doc 38 §2).
fn pool_index(pool_base: u64, pool_len: usize, desc: u64) -> (r: Option<usize>)
    ensures
        match r {
            Some(idx) => idx < pool_len
                && pool_index_spec(pool_base, pool_len as nat, desc) == Some(idx as nat),
            None => pool_index_spec(pool_base, pool_len as nat, desc) is None,
        },
{
    let pa = desc & ADDR_MASK;
    if pa < pool_base {
        return None;
    }
    let off = (pa - pool_base) / PAGE;
    if off >= pool_len as u64 {
        return None;
    }
    Some(off as usize)
}

} // verus!

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

verus! {

/// Read-only walk to `va`'s L3 entry. `None` if any intermediate table is
/// absent. Mirrors the old `l3_lookup`. `pub(crate)` so the §4.5 harnesses can
/// read the installed leaf directly.
///
/// Verified equal to the model [`pt_lookup`]: a `Some((l3, e))` result names the
/// in-bounds leaf slot (`l3 < pool.len()`, `e < 512`) whose value is exactly
/// `pt_lookup`'s leaf PTE; `None` matches `pt_lookup` being `None`. The bounds
/// are what let [`range_mapped_in`] index `pool[l3][e]` safely. The two `?` are
/// spelled as explicit `match`/early-return so the control flow stays in the
/// verified fragment (the 5a convention).
pub(crate) fn lookup(l1: &[u64; 512], pool: &[[u64; 512]], pool_base: u64, va: u64) -> (r: Option<(usize, usize)>)
    ensures
        match r {
            Some((l3, e)) => l3 < pool.len() && e < 512
                && pt_lookup(l1@, pool@, pool_base, va) == Some(pool@[l3 as int][e as int]),
            None => pt_lookup(l1@, pool@, pool_base, va) is None,
        },
{
    broadcast use {vstd::slice::group_slice_axioms, vstd::array::group_array_axioms};
    let l1e = l1[l1_index(va)];
    if l1e & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let l2_idx = match pool_index(pool_base, pool.len(), l1e) {
        Some(i) => i,
        None => return None,
    };
    let l2e = pool[l2_idx][l2_index(va)];
    if l2e & DESC_TABLE != DESC_TABLE {
        return None;
    }
    let l3_idx = match pool_index(pool_base, pool.len(), l2e) {
        Some(i) => i,
        None => return None,
    };
    Some((l3_idx, l3_index(va)))
}

} // verus!

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

verus! {

/// Is `[va, va+len)` fully mapped (and writable, if asked)? The predicate the
/// syscall layer trusts before dereferencing user pointers, so it is total
/// over all `(va, len)` including `len == 0` and the `va + len` overflow edge.
///
/// Verified to **full functional equivalence** with the page-table model: for an
/// in-range request the result is exactly "every page in `[va, va+len)` is
/// present (and, if `write`, writable)" expressed via [`page_ok`]/[`pt_lookup`];
/// `len == 0` reduces to the bare `[USER_VA_BASE, USER_VA_END)` membership of
/// `va`; and any overflow or out-of-range request is rejected (`!r`). The loop
/// computes the `forall` — the invariant carries "every aligned page below the
/// cursor is `page_ok`", the early `return false` witnesses the `forall` failing.
pub fn range_mapped_in(
    l1: &[u64; 512],
    pool: &[[u64; 512]],
    pool_base: u64,
    va: u64,
    len: u64,
    write: bool,
) -> (r: bool)
    ensures
        len == 0 ==> r == (USER_VA_BASE <= va && va < USER_VA_END),
        (len != 0 && !(va >= USER_VA_BASE && (va as int) + (len as int) <= USER_VA_END as int)) ==> !r,
        (len != 0 && va >= USER_VA_BASE && (va as int) + (len as int) <= USER_VA_END as int) ==> (r <==> forall|p: u64|
            #![trigger page_ok(l1@, pool@, pool_base, p, write)]
            (va & !PAGE_MASK) <= p && (p as int) < (va as int) + (len as int) && (p & PAGE_MASK) == 0
                ==> page_ok(l1@, pool@, pool_base, p, write)),
{
    broadcast use {vstd::slice::group_slice_axioms, vstd::array::group_array_axioms};
    assert(USER_VA_END == 0x80_0000_0000) by (compute);
    assert(PAGE == 4096 && PAGE_MASK == 4095) by (compute);
    if len == 0 {
        return va >= USER_VA_BASE && va < USER_VA_END;
    }
    let end = match va.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    if va < USER_VA_BASE || end > USER_VA_END {
        return false;
    }
    let start = va & !PAGE_MASK;
    assert(start <= va && (start & PAGE_MASK) == 0) by (bit_vector)
        requires start == va & !PAGE_MASK;
    let mut page = start;
    while page < end
        invariant
            len != 0,
            start == va & !PAGE_MASK,
            PAGE == 4096,
            PAGE_MASK == 4095,
            (end as int) == (va as int) + (len as int),
            end <= USER_VA_END,
            USER_VA_END == 0x80_0000_0000,
            va >= USER_VA_BASE,
            start <= page,
            (page & PAGE_MASK) == 0,
            forall|p: u64| #![trigger page_ok(l1@, pool@, pool_base, p, write)]
                start <= p && p < page && (p & PAGE_MASK) == 0
                    ==> page_ok(l1@, pool@, pool_base, p, write),
        // `page` steps by `PAGE` and may overshoot `end` on the last iteration,
        // so clamp the measure to stay non-negative (well-founded).
        decreases if page < end { (end - page) as int } else { 0int },
    {
        let res = lookup(l1, pool, pool_base, page);
        match res {
            Some((l3, e)) => {
                if pool[l3][e] != 0 {
                    // AP[1:0] == 0b01 is EL0 read-write; 0b11 is read-only.
                    if write && (pool[l3][e] >> 6) & 0b11 != 0b01 {
                        assert(!page_ok(l1@, pool@, pool_base, page, write));
                        return false;
                    }
                    assert(page_ok(l1@, pool@, pool_base, page, write));
                } else {
                    assert(!page_ok(l1@, pool@, pool_base, page, write));
                    return false;
                }
            }
            None => {
                assert(!page_ok(l1@, pool@, pool_base, page, write));
                return false;
            }
        }
        let ghost prev = page;
        page = page + PAGE;
        assert(page == prev + 4096);
        assert((page & PAGE_MASK) == 0) by (bit_vector)
            requires page == prev + 4096, (prev & PAGE_MASK) == 0, PAGE_MASK == 4095;
        assert forall|p: u64| #![trigger page_ok(l1@, pool@, pool_base, p, write)]
            start <= p && p < page && (p & PAGE_MASK) == 0
                implies page_ok(l1@, pool@, pool_base, p, write) by {
            if !(p < prev) {
                // The only aligned page in [prev, prev + PAGE) is prev itself.
                assert(p == prev) by (bit_vector)
                    requires
                        prev <= p,
                        p < page,
                        page == prev + 4096,
                        PAGE_MASK == 4095,
                        (p & PAGE_MASK) == 0,
                        (prev & PAGE_MASK) == 0;
            }
        }
    }
    assert forall|p: u64| #![trigger page_ok(l1@, pool@, pool_base, p, write)]
        (va & !PAGE_MASK) <= p && (p as int) < (va as int) + (len as int) && (p & PAGE_MASK) == 0
            implies page_ok(l1@, pool@, pool_base, p, write) by {
        // p < va+len == end <= page (loop exit), so the loop invariant applies.
    }
    true
}

} // verus!

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pte_encode_writable_vs_ro() {
        let pa = 0x4800_0000;
        let rw = pte_encode(pa, PERM_W);
        assert_eq!((rw >> 6) & 0b11, 0b01, "PERM_W => AP_EL0_RW");
        assert_eq!(rw & AF, AF);
        assert_eq!(rw & PXN, PXN);
        assert_eq!(rw & ADDR_MASK, pa & ADDR_MASK);
        let ro = pte_encode(pa, 0);
        assert_eq!((ro >> 6) & 0b11, 0b11, "no PERM_W => AP_EL0_RO");
    }

    #[test]
    fn pte_encode_device_never_executable() {
        // AS-1 regression: PERM_X is ignored when PERM_DEVICE is set.
        let pte = pte_encode(0x0900_0000, PERM_DEVICE | PERM_X | PERM_W);
        assert_eq!(pte & UXN, UXN, "device memory must be execute-never");
        assert_eq!(pte & SH_INNER, 0, "device memory is SH_NONE");
        assert_eq!(pte & ATTR_DEVICE, ATTR_DEVICE);
    }

    #[test]
    fn pte_encode_normal_exec_vs_nx() {
        // Non-device, executable: UXN clear (EL0 may execute).
        assert_eq!(pte_encode(0x4800_0000, PERM_X) & UXN, 0);
        // Non-device, non-executable: UXN set.
        assert_eq!(pte_encode(0x4800_0000, 0) & UXN, UXN);
    }

    #[test]
    fn pte_output_pa_roundtrip() {
        let pa = 0x4800_1000;
        for &perms in &[0u64, PERM_W, PERM_X, PERM_DEVICE, PERM_DEVICE | PERM_X] {
            assert_eq!(pte_output_pa(pte_encode(pa, perms)), pa & ADDR_MASK);
        }
    }

    #[test]
    fn va_range_ok_boundaries() {
        assert!(va_range_ok(USER_VA_BASE, 1));
        assert!(va_range_ok(USER_VA_BASE, 0)); // empty range at base
        assert!(va_range_ok(USER_VA_END - PAGE, 1)); // last page fits exactly
        assert!(!va_range_ok(USER_VA_BASE - PAGE, 1)); // below base
        assert!(!va_range_ok(USER_VA_BASE + 1, 1)); // unaligned
        assert!(!va_range_ok(USER_VA_END - PAGE, 2)); // runs past the top
        assert!(!va_range_ok(USER_VA_BASE, u64::MAX)); // saturating overflow edge
    }

    #[test]
    fn user_va_never_touches_kernel_l1() {
        assert_eq!(l1_index(USER_VA_BASE), 2);
        assert!(l1_index(USER_VA_END - PAGE) >= 2);
    }

    // ── range_mapped_in: the first executable check of the verified walker
    //    against the page-table model (the §5c host tests) ─────────────────────

    /// Build an L1 table + a two-table pool (one L2, one L3) mapping `npages`
    /// consecutive pages from `va` to `pte_for(i)`. `va` and `npages` must stay
    /// within one L3 table (≤ 512 pages, no L2/L3-index carry) — enough for the
    /// `range_mapped_in` checks. A zero `pte_for(i)` leaves that page a hole.
    fn build_table(
        va: u64,
        npages: usize,
        pte_for: impl Fn(usize) -> u64,
    ) -> ([u64; 512], Vec<[u64; 512]>, u64) {
        let pool_base = 0x4900_0000u64;
        let mut l1 = [0u64; 512];
        let mut pool = vec![[0u64; 512]; 2]; // pool[0] = L2, pool[1] = L3
        l1[l1_index(va)] = pa_of_table(pool_base, 0) | DESC_TABLE;
        pool[0][l2_index(va)] = pa_of_table(pool_base, 1) | DESC_TABLE;
        for i in 0..npages {
            let page = va + (i as u64) * PAGE;
            pool[1][l3_index(page)] = pte_for(i);
        }
        (l1, pool, pool_base)
    }

    #[test]
    fn range_mapped_fully_mapped_rw() {
        let va = USER_VA_BASE;
        let (l1, pool, base) = build_table(va, 4, |i| pte_encode(0x4800_0000 + (i as u64) * PAGE, PERM_W));
        // Writable mapping: present for both read and write queries.
        assert!(range_mapped_in(&l1, &pool, base, va, 4 * PAGE, false));
        assert!(range_mapped_in(&l1, &pool, base, va, 4 * PAGE, true));
        // A sub-range, and an unaligned start that rounds down into page 0.
        assert!(range_mapped_in(&l1, &pool, base, va, PAGE, true));
        assert!(range_mapped_in(&l1, &pool, base, va + 0x100, 1, true));
    }

    #[test]
    fn range_mapped_readonly_rejects_write() {
        let va = USER_VA_BASE;
        let (l1, pool, base) = build_table(va, 4, |i| pte_encode(0x4800_0000 + (i as u64) * PAGE, 0));
        // Read-only mapping: present for reads, rejected for writes.
        assert!(range_mapped_in(&l1, &pool, base, va, 4 * PAGE, false));
        assert!(!range_mapped_in(&l1, &pool, base, va, 4 * PAGE, true));
    }

    #[test]
    fn range_mapped_hole_rejected() {
        let va = USER_VA_BASE;
        // Page 2 is a hole (pte == 0); the rest are RW.
        let (l1, pool, base) = build_table(va, 4, |i| {
            if i == 2 { 0 } else { pte_encode(0x4800_0000 + (i as u64) * PAGE, PERM_W) }
        });
        // A range covering the hole is rejected; ranges that avoid it pass.
        assert!(!range_mapped_in(&l1, &pool, base, va, 4 * PAGE, false));
        assert!(range_mapped_in(&l1, &pool, base, va, 2 * PAGE, false));
        assert!(!range_mapped_in(&l1, &pool, base, va + 2 * PAGE, PAGE, false));
    }

    #[test]
    fn range_mapped_missing_l3_table_rejected() {
        // L1 present but no L2 table installed: the walk dead-ends → not mapped.
        let pool: Vec<[u64; 512]> = vec![[0u64; 512]; 2];
        let l1 = [0u64; 512];
        assert!(!range_mapped_in(&l1, &pool, 0x4900_0000, USER_VA_BASE, PAGE, false));
    }

    #[test]
    fn range_mapped_len_zero_and_bounds() {
        let (l1, pool, base) = build_table(USER_VA_BASE, 1, |_| pte_encode(0x4800_0000, PERM_W));
        // len == 0 reduces to bare [USER_VA_BASE, USER_VA_END) membership of va.
        assert!(range_mapped_in(&l1, &pool, base, USER_VA_BASE, 0, false));
        assert!(!range_mapped_in(&l1, &pool, base, USER_VA_BASE - PAGE, 0, false));
        assert!(!range_mapped_in(&l1, &pool, base, USER_VA_END, 0, false));
        // Below base / past the top are rejected regardless of the tables.
        assert!(!range_mapped_in(&l1, &pool, base, USER_VA_BASE - PAGE, PAGE, false));
        assert!(!range_mapped_in(&l1, &pool, base, USER_VA_END - PAGE, 2 * PAGE, false));
    }

    #[test]
    fn range_mapped_overflow_rejected() {
        let (l1, pool, base) = build_table(USER_VA_BASE, 1, |_| pte_encode(0x4800_0000, PERM_W));
        // va + len overflows u64 → checked_add is None → rejected, no panic.
        assert!(!range_mapped_in(&l1, &pool, base, u64::MAX - 100, 200, false));
    }
}
