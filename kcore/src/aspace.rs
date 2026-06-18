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
// `StoreSpec` (the `external_trait_extension`) must be in scope so `unmap_in` can
// name the `tlb_log_view` ghost view on the generic `S: Store`; it erases in a
// normal build, so it is otherwise unused here (the doc/results/26 §2.3 idiom).
#[allow(unused_imports)]
use crate::cspace::StoreSpec;
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

verus! {

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    BadVa,
    AlreadyMapped,
    /// Table pool exhausted — donate a bigger pool (§2.5: one error path).
    NeedMemory,
}

} // verus!

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
    /// Object footprint: the page-padded header (so the L1 is page-aligned),
    /// the L1 table, and the pool pages. Retype aligns the whole object to 4 KiB.
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
/// Spec mirror of [`pte_encode`] — the leaf-PTE bit pattern as a `spec fn`, so the
/// `map_in` postcondition can say "the installed leaf is exactly `pte_encode(pa,
/// perms)`". `pub closed` so the cross-crate `pub map_in` may name it without
/// leaking the `pub(crate)` descriptor-bit consts (the doc-38 §2 idiom).
pub closed spec fn spec_pte_encode(pa: u64, perms: u64) -> u64 {
    let ap = if perms & PERM_W != 0 { AP_EL0_RW } else { AP_EL0_RO };
    let device = perms & PERM_DEVICE != 0;
    let xn = if perms & PERM_X != 0 && !device { 0u64 } else { UXN };
    let sh = if device { SH_NONE } else { SH_INNER };
    let attr = if device { ATTR_DEVICE } else { ATTR_NORMAL };
    (pa & ADDR_MASK) | DESC_PAGE | AF | sh | attr | ap | xn | PXN
}

pub(crate) fn pte_encode(pa: u64, perms: u64) -> (pte: u64)
    ensures
        pte == spec_pte_encode(pa, perms),
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

/// The leaf **slot** `(l3 table, entry)` a walk of `va` lands on, or `None` — the
/// structural mirror of [`pt_lookup`] (which returns the *value* `pool[l3][e]`).
/// `map_in`'s leaf write needs the slot, not just the value: it writes `pool[l3][e]`
/// and reasons that `va` (and only pages sharing that slot) now read it.
pub closed spec fn pt_leaf_slot(l1: Seq<u64>, pool: Seq<[u64; 512]>, pool_base: u64, va: u64) -> Option<(nat, nat)> {
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
                        Some(l3_idx) => Some((l3_idx, spec_l3_index(va) as nat)),
                    }
                }
            }
        }
    }
}

/// `va`'s L3 table is a leaf table for *some* `pt_wf` witness — the predicate
/// `walk_alloc` hands `map_in` so its leaf write preserves `pt_wf` (a frame PTE
/// must land in a leaf table, never an inner one — the `DESC_PAGE == DESC_TABLE`
/// hazard). Existential so it composes with the existential [`pt_wf`].
pub closed spec fn pt_wf_leaf(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pool_used: nat,
    pool_len: nat,
    l3: nat,
) -> bool {
    exists|leaves: Set<nat>|
        pt_wf_leveled(l1, pool, pool_base, pool_used, pool_len, leaves) && leaves.contains(l3)
}

/// `pt_leaf_slot` and `pt_lookup` agree: if the walk lands on slot `(l3, e)` then
/// the looked-up value is exactly `pool[l3][e]` (both unfold the same walk).
pub proof fn lemma_leaf_slot_lookup(l1: Seq<u64>, pool: Seq<[u64; 512]>, pool_base: u64, va: u64)
    ensures
        match pt_leaf_slot(l1, pool, pool_base, va) {
            Some((l3, e)) => pt_lookup(l1, pool, pool_base, va) == Some(pool[l3 as int][e as int]),
            None => pt_lookup(l1, pool, pool_base, va) is None,
        },
{
}

/// Table-pool well-formedness — the `chan_wf`/`notif_wf`/`timer_wf` analog for the
/// page table, **refined in 5d** (doc 39) to carry the L2/L3 level structure
/// `map_in` needs. The 5c definition quantified closure over *all* used tables,
/// which is unsatisfiable once a real mapping is installed: a leaf (L3) PTE has
/// `DESC_PAGE == DESC_TABLE == 0b11` (`aspace.rs`), so its frame-PA address field
/// would be (wrongly) required to resolve into the pool. The level *partition*
/// fixes this — closure/no-aliasing apply to the intermediate (L2) tables only;
/// leaf (L3) tables hold frame PTEs and are unconstrained. The partition is
/// existentially quantified so the public signature stays
/// `(l1, pool, pool_base, pool_used, pool_len)` (no ghost arg leaks to the kernel
/// shell, which calls `map_in` with no `leaves`); [`pt_wf_leveled`] is the witness.
pub closed spec fn pt_wf(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pool_used: nat,
    pool_len: nat,
) -> bool {
    exists|leaves: Set<nat>| pt_wf_leveled(l1, pool, pool_base, pool_used, pool_len, leaves)
}

/// The leveled core of [`pt_wf`]. `leaves` are the L3 (leaf) pool tables; the rest
/// of `[0, pool_used)` are the L2 (inner) tables. Clauses:
///   (a) accounting — pool length, the high-water mark, `leaves ⊆ [0, pool_used)`;
///   (b1) L1 descriptors resolve to an **inner** table `< pool_used`;
///   (b2) descriptors **inside an inner table** resolve to a **leaf** table
///        `< pool_used` (leaf tables' entries are unconstrained — frame PTEs);
///   (c1)/(c2) the parent→child index map is injective (the page table is a tree,
///        not a DAG — the load-bearing locality invariant for the leaf-write frame).
/// Cross-level no-aliasing (an L1 target vs an L2 target) is free: L1 targets are
/// inner, L2 targets are leaf, so they are disjoint by the partition.
pub closed spec fn pt_wf_leveled(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pool_used: nat,
    pool_len: nat,
    leaves: Set<nat>,
) -> bool {
    // (a) accounting.
    &&& l1.len() == 512
    &&& pool.len() == pool_len
    &&& pool_used <= pool_len
    &&& (forall|x: nat| leaves.contains(x) ==> x < pool_used)
    // (b1) L1 → inner (present, in-range, not a leaf).
    &&& (forall|i: int| #![trigger l1[i]]
            0 <= i < 512 && l1[i] & DESC_TABLE == DESC_TABLE ==> {
                &&& pool_index_spec(pool_base, pool_len, l1[i]) is Some
                &&& pool_index_spec(pool_base, pool_len, l1[i]).unwrap() < pool_used
                &&& !leaves.contains(pool_index_spec(pool_base, pool_len, l1[i]).unwrap())
            })
    // (b2) inner → leaf.
    &&& (forall|t: int, e: int| #![trigger pool[t][e]]
            0 <= t < pool_used && !leaves.contains(t as nat) && 0 <= e < 512
                && pool[t][e] & DESC_TABLE == DESC_TABLE ==> {
                &&& pool_index_spec(pool_base, pool_len, pool[t][e]) is Some
                &&& pool_index_spec(pool_base, pool_len, pool[t][e]).unwrap() < pool_used
                &&& leaves.contains(pool_index_spec(pool_base, pool_len, pool[t][e]).unwrap())
            })
    // (c1) L1 injective.
    &&& (forall|i: int, j: int| #![trigger l1[i], l1[j]]
            0 <= i < 512 && 0 <= j < 512 && i != j
                && l1[i] & DESC_TABLE == DESC_TABLE && l1[j] & DESC_TABLE == DESC_TABLE
                ==> pool_index_spec(pool_base, pool_len, l1[i])
                        != pool_index_spec(pool_base, pool_len, l1[j]))
    // (c2) inner-table descriptors injective across all inner tables.
    &&& (forall|t1: int, e1: int, t2: int, e2: int| #![trigger pool[t1][e1], pool[t2][e2]]
            0 <= t1 < pool_used && !leaves.contains(t1 as nat) && 0 <= e1 < 512
                && 0 <= t2 < pool_used && !leaves.contains(t2 as nat) && 0 <= e2 < 512
                && !(t1 == t2 && e1 == e2)
                && pool[t1][e1] & DESC_TABLE == DESC_TABLE
                && pool[t2][e2] & DESC_TABLE == DESC_TABLE
                ==> pool_index_spec(pool_base, pool_len, pool[t1][e1])
                        != pool_index_spec(pool_base, pool_len, pool[t2][e2]))
}

/// The pool's address geometry the descriptor round-trip needs: `pool_base`
/// page-aligned, and the whole pool inside the 48-bit output-address field (so
/// `pool_base + idx*PAGE` never overflows and never collides with the descriptor
/// control bits). Both hold for the kernel shell — the table pool is page-aligned
/// untyped memory (`AspaceObj::bytes_for`, retype aligns to 4 KiB). A `requires`
/// on the map ops; `0x1_0000_…` is `2^48`.
pub open spec fn pool_geom_ok(pool_base: u64, pool_len: nat) -> bool {
    &&& pool_base & PAGE_MASK == 0
    &&& (pool_base as int) + (pool_len as int) * (PAGE as int) <= 0x1_0000_0000_0000
}

/// PA of pool table `idx`, as stored in a table descriptor's output-address field
/// — the byte-identical PA↔pool-index conversion (the inverse is [`pool_index`]).
/// The geometry `requires` keeps `pool_base + idx*PAGE` overflow-free.
fn pa_of_table(pool_base: u64, pool_len: usize, idx: usize) -> (r: u64)
    requires
        pool_geom_ok(pool_base, pool_len as nat),
        idx < pool_len,
    ensures
        r as int == pool_base as int + (idx as int) * (PAGE as int),
{
    let _ = pool_len; // spec-only (geometry `requires`/the proof `assert`); erased build sees it unused
    assert((idx as int) * (PAGE as int) <= (pool_len as int) * (PAGE as int)) by (nonlinear_arith)
        requires idx < pool_len;
    pool_base + (idx as u64) * PAGE
}

/// The descriptor round-trip: a table descriptor built from `pa_of_table(idx)`
/// resolves back to exactly `idx` (and reads as a present table descriptor). The
/// arithmetic linchpin of `walk_alloc`'s closure reasoning — the low 0b11 tag is
/// disjoint from `ADDR_MASK[47:12]`, and a page-aligned in-range PA round-trips
/// through `(.. & ADDR_MASK) - pool_base) / PAGE`. The hard mask/division steps
/// are isolated into `bit_vector`/`nonlinear_arith` one-liners (the doc-25/37 §2
/// discipline).
proof fn lemma_desc_roundtrip(pool_base: u64, pool_len: nat, idx: nat, pa: u64)
    requires
        pool_geom_ok(pool_base, pool_len),
        idx < pool_len,
        pa as int == pool_base as int + (idx as int) * (PAGE as int),
    ensures
        (pa | DESC_TABLE) & DESC_TABLE == DESC_TABLE,
        pool_index_spec(pool_base, pool_len, pa | DESC_TABLE) == Some(idx),
{
    assert(PAGE == 4096 && PAGE_MASK == 4095 && DESC_TABLE == 3) by (compute);
    assert(ADDR_MASK == 0xFFFF_FFFF_F000) by (compute);
    // `pa = pool_base + idx*PAGE` is page-aligned (base aligned + a multiple of
    // PAGE), in range (< pool_base + pool_len*PAGE ≤ 2^48), hence < 2^48.
    let ghost bound: int = 0x1_0000_0000_0000;
    assert(pool_base as int % 4096 == 0) by (bit_vector)
        requires pool_base & PAGE_MASK == 0, PAGE_MASK == 4095;
    assert(pa as int % 4096 == 0) by (nonlinear_arith)
        requires pa as int == pool_base as int + (idx as int) * 4096, pool_base as int % 4096 == 0;
    assert(pa & PAGE_MASK == 0) by (bit_vector)
        requires pa as int % 4096 == 0, PAGE_MASK == 4095;
    assert((idx as int) * 4096 <= (pool_len as int) * 4096) by (nonlinear_arith)
        requires pool_len > idx;
    assert(bound > pa as int);
    assert(pa < 0x1_0000_0000_0000u64);
    // The control tag does not touch the output-address field, so it round-trips.
    assert((pa | DESC_TABLE) & DESC_TABLE == DESC_TABLE) by (bit_vector)
        requires DESC_TABLE == 3;
    assert((pa | DESC_TABLE) & ADDR_MASK == pa) by (bit_vector)
        requires
            pa & PAGE_MASK == 0,
            pa < 0x1_0000_0000_0000u64,
            PAGE_MASK == 4095,
            DESC_TABLE == 3,
            ADDR_MASK == 0xFFFF_FFFF_F000;
    // Now `pool_index_spec` divides the (recovered) offset by PAGE → idx.
    assert(((pa as int) - (pool_base as int)) == (idx as int) * (PAGE as int));
    assert(((idx as int) * (PAGE as int)) / (PAGE as int) == idx as int) by (nonlinear_arith);
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

verus! {

/// Grab the next free pool table, zero it, and return its index. The zeroing
/// matches the old `alloc_table`'s `write_bytes(.., 0, PAGE)` so a freshly
/// allocated table starts empty (`check_pool_accounting`).
///
/// Purely structural — no `pt_wf`/`pt_lookup` here; [`walk_alloc`] combines this
/// with `pt_wf` to conclude the fresh table perturbs no lookup. `Ok` carries: the
/// fresh index is the old high-water mark, it is in bounds, `pool_used` advanced
/// by one (still `<= len`), the new table is all-zero, and every other table is
/// untouched. `Err` is `NeedMemory` exactly at exhaustion, leaving state intact.
fn alloc_table(pool: &mut [[u64; 512]], pool_used: &mut u64) -> (r: Result<usize, MapError>)
    ensures
        final(pool).len() == old(pool).len(),
        match r {
            Ok(idx) => {
                &&& idx as int == *old(pool_used) as int
                &&& idx < final(pool).len()
                &&& *final(pool_used) == *old(pool_used) + 1
                &&& *final(pool_used) <= final(pool).len()
                &&& (forall|e: int| 0 <= e < 512 ==> final(pool)@[idx as int][e] == 0)
                &&& (forall|t: int| 0 <= t < final(pool).len() && t != idx as int
                        ==> final(pool)@[t] == old(pool)@[t])
            }
            Err(e) => {
                &&& e == MapError::NeedMemory
                &&& *final(pool_used) == *old(pool_used)
                &&& final(pool)@ == old(pool)@
                &&& *old(pool_used) >= old(pool).len()
            }
        },
{
    broadcast use {vstd::slice::group_slice_axioms, vstd::array::group_array_axioms};
    // Compare in `u64` before the `as usize` cast so the cast is lossless without
    // pinning `usize`'s width (the doc-38 §2 discipline; `pool.len() as u64` is
    // exact since `usize <= u64`).
    if *pool_used >= pool.len() as u64 {
        return Err(MapError::NeedMemory);
    }
    let idx = *pool_used as usize;
    *pool_used = *pool_used + 1;
    pool[idx] = [0u64; 512];
    assert(forall|e: int| 0 <= e < 512 ==> pool@[idx as int][e] == 0);
    Ok(idx)
}

} // verus!

verus! {

/// The three table indices of any VA are `< 512` (the spec mirror of the exec
/// `l1_index`/`l2_index`/`l3_index` `ensures`, usable from `proof`/`spec` context).
proof fn lemma_va_indices(w: u64)
    ensures
        spec_l1_index(w) < 512,
        spec_l2_index(w) < 512,
        spec_l3_index(w) < 512,
{
    assert(((w >> 30) & 0x1FF) < 512) by (bit_vector);
    assert(((w >> 21) & 0x1FF) < 512) by (bit_vector);
    assert(((w >> 12) & 0x1FF) < 512) by (bit_vector);
}

/// `0` is not a table descriptor (a zeroed pool entry) and `desc | DESC_TABLE` is
/// — the two descriptor-tag facts the link proofs lean on, isolated to bit_vector.
proof fn lemma_desc_tag(desc: u64)
    ensures
        0u64 & DESC_TABLE != DESC_TABLE,
        (desc | DESC_TABLE) & DESC_TABLE == DESC_TABLE,
{
    assert(DESC_TABLE == 3) by (compute);
    assert(0u64 & 3 != 3) by (bit_vector);
    assert((desc | 3) & 3 == 3) by (bit_vector);
}

/// Linking a **fresh, zeroed** table (index `pu`, the old high-water mark) into a
/// previously-empty **L1** slot preserves `pt_wf` and **changes no `pt_lookup`** —
/// the new L2 table is empty, so a walk that now enters it immediately dead-ends.
/// The fresh index `pu` becomes a new inner (L2) table; `leaves` is unchanged.
/// Freshness (`pu` is distinct from every present descriptor's target, all `< pu`
/// by the pre-link closure) is what re-establishes the injectivity clauses.
proof fn lemma_link_l1(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    i0: int,
    desc: u64,
)
    requires
        pt_wf(l1, pool, pool_base, pu, pl),
        pu < pl,
        0 <= i0 < 512,
        l1[i0] & DESC_TABLE != DESC_TABLE,
        desc & DESC_TABLE == DESC_TABLE,
        pool_index_spec(pool_base, pl, desc) == Some(pu),
        pooln.len() == pl,
        forall|e: int| 0 <= e < 512 ==> pooln[pu as int][e] == 0,
        forall|t: int| 0 <= t < pl && t != pu ==> pooln[t] == pool[t],
    ensures
        pt_wf(l1.update(i0, desc), pooln, pool_base, (pu + 1) as nat, pl),
        forall|w: u64| #![trigger pt_lookup(l1.update(i0, desc), pooln, pool_base, w)]
            pt_lookup(l1.update(i0, desc), pooln, pool_base, w)
                == pt_lookup(l1, pool, pool_base, w),
{
    let leaves = choose|lv: Set<nat>| pt_wf_leveled(l1, pool, pool_base, pu, pl, lv);
    let l1n = l1.update(i0, desc);
    let pun = (pu + 1) as nat;
    // Every present descriptor's target is `< pu` (closure), so `pu` is fresh.
    lemma_desc_tag(0);
    assert(!leaves.contains(pu));  // leaves ⊆ [0, pu)
    assert(pt_wf_leveled(l1n, pooln, pool_base, pun, pl, leaves)) by {
        assert forall|i: int| #![trigger l1n[i]]
            0 <= i < 512 && l1n[i] & DESC_TABLE == DESC_TABLE implies {
                &&& pool_index_spec(pool_base, pl, l1n[i]) is Some
                &&& pool_index_spec(pool_base, pl, l1n[i]).unwrap() < pun
                &&& !leaves.contains(pool_index_spec(pool_base, pl, l1n[i]).unwrap())
            } by {
            if i == i0 {
                assert(l1n[i] == desc);
            } else {
                assert(l1n[i] == l1[i]);  // old (b1) fires on l1[i]
            }
        }
        assert forall|t: int, e: int| #![trigger pooln[t][e]]
            0 <= t < pun && !leaves.contains(t as nat) && 0 <= e < 512
                && pooln[t][e] & DESC_TABLE == DESC_TABLE implies {
                &&& pool_index_spec(pool_base, pl, pooln[t][e]) is Some
                &&& pool_index_spec(pool_base, pl, pooln[t][e]).unwrap() < pun
                &&& leaves.contains(pool_index_spec(pool_base, pl, pooln[t][e]).unwrap())
            } by {
            if t == pu {
                assert(pooln[t][e] == 0);  // fresh table — entry is 0, antecedent false
            } else {
                assert(t < pu);
                assert(pooln[t] == pool[t]);
                assert(pooln[t][e] == pool[t][e]);  // old (b2) fires on pool[t][e]
            }
        }
        assert forall|i: int, j: int| #![trigger l1n[i], l1n[j]]
            0 <= i < 512 && 0 <= j < 512 && i != j
                && l1n[i] & DESC_TABLE == DESC_TABLE && l1n[j] & DESC_TABLE == DESC_TABLE
            implies pool_index_spec(pool_base, pl, l1n[i]) != pool_index_spec(pool_base, pl, l1n[j]) by {
            if i == i0 {
                assert(l1n[i] == desc && l1n[j] == l1[j]);  // l1[j] target < pu = Some(pu)'s idx
            } else if j == i0 {
                assert(l1n[j] == desc && l1n[i] == l1[i]);
            } else {
                assert(l1n[i] == l1[i] && l1n[j] == l1[j]);  // old (c1)
            }
        }
        assert forall|t1: int, e1: int, t2: int, e2: int| #![trigger pooln[t1][e1], pooln[t2][e2]]
            0 <= t1 < pun && !leaves.contains(t1 as nat) && 0 <= e1 < 512
                && 0 <= t2 < pun && !leaves.contains(t2 as nat) && 0 <= e2 < 512
                && !(t1 == t2 && e1 == e2)
                && pooln[t1][e1] & DESC_TABLE == DESC_TABLE
                && pooln[t2][e2] & DESC_TABLE == DESC_TABLE
            implies pool_index_spec(pool_base, pl, pooln[t1][e1])
                        != pool_index_spec(pool_base, pl, pooln[t2][e2]) by {
            if t1 == pu {
                assert(pooln[t1][e1] == 0);  // antecedent false
            } else if t2 == pu {
                assert(pooln[t2][e2] == 0);  // antecedent false
            } else {
                assert(t1 < pu && t2 < pu);
                assert(pooln[t1] == pool[t1] && pooln[t2] == pool[t2]);
                assert(pooln[t1][e1] == pool[t1][e1] && pooln[t2][e2] == pool[t2][e2]);  // old (c2)
            }
        }
    }
    // pt_lookup is unchanged for every `w` (the new L2 table is empty).
    assert forall|w: u64| #![trigger pt_lookup(l1n, pooln, pool_base, w)]
        pt_lookup(l1n, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w) by {
        lemma_link_l1_lookup(l1, pool, pooln, pool_base, pu, pl, i0, desc, leaves, w);
    }
}

/// Per-`w` core of [`lemma_link_l1`]'s `pt_lookup` frame: case-splits on whether
/// `w`'s L1 index is the linked slot. If it is, both walks dead-end (`None`); if
/// not, the walk reads only old tables (`< pu`, by closure), all unchanged.
proof fn lemma_link_l1_lookup(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    i0: int,
    desc: u64,
    leaves: Set<nat>,
    w: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        pu < pl,
        0 <= i0 < 512,
        l1[i0] & DESC_TABLE != DESC_TABLE,
        pool_index_spec(pool_base, pl, desc) == Some(pu),
        pooln.len() == pl,
        forall|e: int| 0 <= e < 512 ==> pooln[pu as int][e] == 0,
        forall|t: int| 0 <= t < pl && t != pu ==> pooln[t] == pool[t],
    ensures
        pt_lookup(l1.update(i0, desc), pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    lemma_va_indices(w);
    lemma_desc_tag(0);
    assert(pool.len() == pl && pooln.len() == pl);  // align both walks' pool_index_spec
    let l1n = l1.update(i0, desc);
    let i1 = spec_l1_index(w) as int;
    if i1 == i0 {
        // After: enter the fresh empty L2 table at `pu`; its entries are 0, so the
        // L2 read is not a table descriptor → None. Before: l1[i0] absent → None.
        assert(l1n[i1] == desc);
        assert(pooln[pu as int][spec_l2_index(w) as int] == 0);
        assert(pt_lookup(l1, pool, pool_base, w) is None);
        assert(pt_lookup(l1n, pooln, pool_base, w) is None);
    } else {
        assert(l1n[i1] == l1[i1]);
        if l1[i1] & DESC_TABLE == DESC_TABLE {
            let l2i = pool_index_spec(pool_base, pl, l1[i1]).unwrap();
            assert(l2i < pu);                 // (b1) closure
            assert(pooln[l2i as int] == pool[l2i as int]);
            let l2e = pool[l2i as int][spec_l2_index(w) as int];
            if l2e & DESC_TABLE == DESC_TABLE {
                let l3i = pool_index_spec(pool_base, pl, l2e).unwrap();
                assert(l2i < pu && !leaves.contains(l2i));   // l1 target is inner
                assert(l3i < pu);             // (b2) closure
                assert(pooln[l3i as int] == pool[l3i as int]);
            }
        }
        // every read on `w`'s walk matched, so the two walks coincide.
        assert(pt_lookup(l1n, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w));
    }
}

/// Linking a **fresh, zeroed** leaf table (index `pu`) into a previously-empty
/// slot `(t1, e1)` of an **inner** table `t1` preserves `pt_wf` (with `pu` joining
/// `leaves`) and **frames every nonzero leaf**: a page that was mapped stays
/// mapped to the same value. The walk into the new leaf table only ever surfaces a
/// fresh `0` (an unmapped page), so no nonzero PTE is perturbed. Takes the witness
/// `leaves` explicitly because the caller (`walk_alloc`) must already know `t1` is
/// inner (an L1 target, by `(b1)`).
proof fn lemma_link_l2(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    t1: int,
    e1: int,
    desc: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        pu < pl,
        0 <= t1 < pu,
        !leaves.contains(t1 as nat),
        0 <= e1 < 512,
        pool[t1][e1] & DESC_TABLE != DESC_TABLE,
        desc & DESC_TABLE == DESC_TABLE,
        pool_index_spec(pool_base, pl, desc) == Some(pu),
        pooln.len() == pl,
        forall|e: int| 0 <= e < 512 ==> pooln[pu as int][e] == 0,
        pooln[t1][e1] == desc,
        forall|e: int| 0 <= e < 512 && e != e1 ==> pooln[t1][e] == pool[t1][e],
        forall|t: int| 0 <= t < pl && t != pu && t != t1 ==> pooln[t] == pool[t],
    ensures
        pt_wf(l1, pooln, pool_base, (pu + 1) as nat, pl),
        forall|w: u64| #![trigger pt_lookup(l1, pooln, pool_base, w)]
            pt_lookup(l1, pool, pool_base, w) is Some
                ==> pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    lemma_desc_tag(0);
    let leavesn = leaves.insert(pu);
    let pun = (pu + 1) as nat;
    assert(!leaves.contains(pu));  // leaves ⊆ [0, pu)
    assert(pt_wf_leveled(l1, pooln, pool_base, pun, pl, leavesn)) by {
        assert forall|i: int| #![trigger l1[i]]
            0 <= i < 512 && l1[i] & DESC_TABLE == DESC_TABLE implies {
                &&& pool_index_spec(pool_base, pl, l1[i]) is Some
                &&& pool_index_spec(pool_base, pl, l1[i]).unwrap() < pun
                &&& !leavesn.contains(pool_index_spec(pool_base, pl, l1[i]).unwrap())
            } by {
            // L1 target is an old inner index `< pu`, so it is `!= pu` and stays inner.
            assert(pool_index_spec(pool_base, pl, l1[i]).unwrap() < pu);
        }
        assert forall|t: int, e: int| #![trigger pooln[t][e]]
            0 <= t < pun && !leavesn.contains(t as nat) && 0 <= e < 512
                && pooln[t][e] & DESC_TABLE == DESC_TABLE implies {
                &&& pool_index_spec(pool_base, pl, pooln[t][e]) is Some
                &&& pool_index_spec(pool_base, pl, pooln[t][e]).unwrap() < pun
                &&& leavesn.contains(pool_index_spec(pool_base, pl, pooln[t][e]).unwrap())
            } by {
            // `t` is an old inner table (`pu` is in `leavesn`, excluded).
            assert(t < pu && !leaves.contains(t as nat));
            if t == t1 && e == e1 {
                assert(pooln[t][e] == desc);  // → pu ∈ leavesn
            } else if t == t1 {
                assert(pooln[t][e] == pool[t][e]);  // unchanged entry; old (b2)
            } else {
                assert(pooln[t] == pool[t]);  // old (b2)
                assert(pooln[t][e] == pool[t][e]);
            }
        }
        assert forall|i: int, j: int| #![trigger l1[i], l1[j]]
            0 <= i < 512 && 0 <= j < 512 && i != j
                && l1[i] & DESC_TABLE == DESC_TABLE && l1[j] & DESC_TABLE == DESC_TABLE
            implies pool_index_spec(pool_base, pl, l1[i]) != pool_index_spec(pool_base, pl, l1[j]) by {
            // l1 unchanged → old (c1).
        }
        assert forall|ta: int, ea: int, tb: int, eb: int| #![trigger pooln[ta][ea], pooln[tb][eb]]
            0 <= ta < pun && !leavesn.contains(ta as nat) && 0 <= ea < 512
                && 0 <= tb < pun && !leavesn.contains(tb as nat) && 0 <= eb < 512
                && !(ta == tb && ea == eb)
                && pooln[ta][ea] & DESC_TABLE == DESC_TABLE
                && pooln[tb][eb] & DESC_TABLE == DESC_TABLE
            implies pool_index_spec(pool_base, pl, pooln[ta][ea])
                        != pool_index_spec(pool_base, pl, pooln[tb][eb]) by {
            assert(ta < pu && tb < pu && !leaves.contains(ta as nat) && !leaves.contains(tb as nat));
            let is_new = |t: int, e: int| t == t1 && e == e1;
            if is_new(ta, ea) && is_new(tb, eb) {
                // same slot — excluded by ta==tb && ea==eb
            } else if is_new(ta, ea) {
                // pooln[ta][ea] == desc → pu; the other resolves to an old leaf < pu.
                assert(pooln[ta][ea] == desc);
                assert(pooln[tb][eb] == pool[tb][eb]);
                assert(pool_index_spec(pool_base, pl, pool[tb][eb]).unwrap() < pu);  // old (b2)
            } else if is_new(tb, eb) {
                assert(pooln[tb][eb] == desc);
                assert(pooln[ta][ea] == pool[ta][ea]);
                assert(pool_index_spec(pool_base, pl, pool[ta][ea]).unwrap() < pu);
            } else {
                assert(pooln[ta][ea] == pool[ta][ea] && pooln[tb][eb] == pool[tb][eb]);  // old (c2)
            }
        }
    }
    assert forall|w: u64| #![trigger pt_lookup(l1, pooln, pool_base, w)]
        pt_lookup(l1, pool, pool_base, w) is Some
            implies pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w) by {
        lemma_link_l2_lookup(l1, pool, pooln, pool_base, pu, pl, leaves, t1, e1, desc, w);
    }
}

/// Per-`w` core of [`lemma_link_l2`]'s frame: a **present** `w` cannot have its L2
/// step land on the just-written slot `(t1, e1)` — that slot was empty, but a
/// present `w` needs a descriptor there — so its whole walk reads only old tables,
/// unchanged. (Holds for any present page, mapped or `Some(0)`.)
proof fn lemma_link_l2_lookup(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    t1: int,
    e1: int,
    desc: u64,
    w: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        pu < pl,
        0 <= t1 < pu,
        !leaves.contains(t1 as nat),
        0 <= e1 < 512,
        pool[t1][e1] & DESC_TABLE != DESC_TABLE,
        pool_index_spec(pool_base, pl, desc) == Some(pu),
        pooln.len() == pl,
        pooln[t1][e1] == desc,
        forall|e: int| 0 <= e < 512 && e != e1 ==> pooln[t1][e] == pool[t1][e],
        forall|t: int| 0 <= t < pl && t != pu && t != t1 ==> pooln[t] == pool[t],
        pt_lookup(l1, pool, pool_base, w) is Some,
    ensures
        pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    lemma_va_indices(w);
    assert(pool.len() == pl && pooln.len() == pl);
    let i1 = spec_l1_index(w) as int;
    // `w` is mapped, so the full walk is present: L1 desc → inner l2i → L2 desc → leaf l3i.
    assert(l1[i1] & DESC_TABLE == DESC_TABLE);
    let l2i = pool_index_spec(pool_base, pl, l1[i1]).unwrap();
    assert(l2i < pu && !leaves.contains(l2i));    // (b1): l1 target is inner
    let l2e = pool[l2i as int][spec_l2_index(w) as int];
    assert(l2e & DESC_TABLE == DESC_TABLE);       // present (else lookup were None)
    // If the walk used `t1`, its L2 entry index cannot be `e1` (that slot was empty),
    // so the written slot is untouched on `w`'s path.
    if l2i == t1 {
        assert(spec_l2_index(w) as int != e1);    // else `l2e` would be the empty slot
        assert(pooln[l2i as int][spec_l2_index(w) as int] == pool[l2i as int][spec_l2_index(w) as int]);
    } else {
        assert(pooln[l2i as int] == pool[l2i as int]);
    }
    let l3i = pool_index_spec(pool_base, pl, l2e).unwrap();
    assert(l3i < pu && leaves.contains(l3i));     // (b2): inner target is a leaf
    assert(l3i != pu && l3i != t1);               // leaf ≠ the fresh/inner indices
    assert(pooln[l3i as int] == pool[l3i as int]);
}

/// Writing a leaf PTE `pte` into the leaf slot `(l3, e)` that `va` resolves to
/// preserves `pt_wf` (the slot is in a **leaf** table `l3 ∈ leaves`, excluded from
/// the closure/no-aliasing clauses, so the frame PTE cannot violate them), makes
/// `va` map to `pte`, and **frames every page whose looked-up value differs from
/// the slot's old value** — a *value*-based locality argument that needs no
/// no-aliasing: a page reading a value `!= pool[l3][e]` must read a different slot,
/// hence is untouched. `map_in` writes into slots that pass 1 left `0`, so this
/// preserves every nonzero (mapped) page.
proof fn lemma_leaf_write(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    l3: nat,
    e: nat,
    va: u64,
    pte: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        leaves.contains(l3),
        e < 512,
        pt_leaf_slot(l1, pool, pool_base, va) == Some((l3, e)),
        pooln.len() == pl,
        pooln[l3 as int][e as int] == pte,
        forall|j: int| 0 <= j < 512 && j != e ==> pooln[l3 as int][j] == pool[l3 as int][j],
        forall|t: int| 0 <= t < pl && t != l3 ==> pooln[t] == pool[t],
    ensures
        pt_wf(l1, pooln, pool_base, pu, pl),
        pt_lookup(l1, pooln, pool_base, va) == Some(pte),
        // slot-based frame: a page resolving to a *different* leaf slot is unchanged
        // (what `map_in` uses, via `lemma_distinct_pages_slots`, for the other pages).
        forall|w: u64| #![trigger pt_lookup(l1, pooln, pool_base, w)]
            (pt_leaf_slot(l1, pool, pool_base, w) is Some
                && pt_leaf_slot(l1, pool, pool_base, w) != Some((l3, e)))
                ==> pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    // (1) pt_wf preserved with the same `leaves` — `l3 ∈ leaves` excludes it from
    // (b2)/(c2), so writing a frame PTE there breaks nothing.
    assert(pt_wf_leveled(l1, pooln, pool_base, pu, pl, leaves)) by {
        assert forall|t: int, e2: int| #![trigger pooln[t][e2]]
            0 <= t < pu && !leaves.contains(t as nat) && 0 <= e2 < 512
                && pooln[t][e2] & DESC_TABLE == DESC_TABLE implies {
                &&& pool_index_spec(pool_base, pl, pooln[t][e2]) is Some
                &&& pool_index_spec(pool_base, pl, pooln[t][e2]).unwrap() < pu
                &&& leaves.contains(pool_index_spec(pool_base, pl, pooln[t][e2]).unwrap())
            } by {
            assert(t != l3);  // t inner, l3 ∈ leaves
            assert(pooln[t] == pool[t]);
            assert(pooln[t][e2] == pool[t][e2]);  // old (b2)
        }
        assert forall|t1: int, e1: int, t2: int, e2: int| #![trigger pooln[t1][e1], pooln[t2][e2]]
            0 <= t1 < pu && !leaves.contains(t1 as nat) && 0 <= e1 < 512
                && 0 <= t2 < pu && !leaves.contains(t2 as nat) && 0 <= e2 < 512
                && !(t1 == t2 && e1 == e2)
                && pooln[t1][e1] & DESC_TABLE == DESC_TABLE
                && pooln[t2][e2] & DESC_TABLE == DESC_TABLE
            implies pool_index_spec(pool_base, pl, pooln[t1][e1])
                        != pool_index_spec(pool_base, pl, pooln[t2][e2]) by {
            assert(t1 != l3 && t2 != l3);
            assert(pooln[t1] == pool[t1] && pooln[t2] == pool[t2]);  // old (c2)
        }
    }
    // (2) va maps to pte, and (3) the slot-based frame, per `w`.
    lemma_leaf_slot_lookup(l1, pooln, pool_base, va);
    assert forall|w: u64| #![trigger pt_lookup(l1, pooln, pool_base, w)]
        (pt_leaf_slot(l1, pool, pool_base, w) is Some
            && pt_leaf_slot(l1, pool, pool_base, w) != Some((l3, e)))
            implies pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w) by {
        lemma_leaf_write_frame(l1, pool, pooln, pool_base, pu, pl, leaves, l3, e, w);
    }
    // va's walk lands on (l3, e); after the write the leaf reads `pte`.
    assert(pt_leaf_slot(l1, pooln, pool_base, va) == Some((l3, e))) by {
        lemma_leaf_write_slot(l1, pool, pooln, pool_base, pu, pl, leaves, l3, e, va);
    }
    lemma_leaf_slot_lookup(l1, pooln, pool_base, va);
}

/// Per-`w` slot frame of [`lemma_leaf_write`]: a page resolving to a leaf slot
/// other than the written `(l3, e)` reads only old tables (its walk goes through
/// `l1` + inner tables, all `!= l3`) and a leaf entry `!= (l3, e)`, all unchanged.
proof fn lemma_leaf_write_frame(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    l3: nat,
    e: nat,
    w: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        leaves.contains(l3),
        e < 512,
        pooln.len() == pl,
        forall|j: int| 0 <= j < 512 && j != e ==> pooln[l3 as int][j] == pool[l3 as int][j],
        forall|t: int| 0 <= t < pl && t != l3 ==> pooln[t] == pool[t],
    ensures
        (pt_leaf_slot(l1, pool, pool_base, w) is Some
            && pt_leaf_slot(l1, pool, pool_base, w) != Some((l3, e)))
            ==> pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    lemma_va_indices(w);
    assert(pool.len() == pl && pooln.len() == pl);
    if pt_leaf_slot(l1, pool, pool_base, w) is Some
        && pt_leaf_slot(l1, pool, pool_base, w) != Some((l3, e)) {
        let l1e = l1[spec_l1_index(w) as int];
        let l2_idx = pool_index_spec(pool_base, pl, l1e).unwrap();
        assert(l2_idx < pu && !leaves.contains(l2_idx));   // l1 target inner
        assert(l2_idx != l3 && pooln[l2_idx as int] == pool[l2_idx as int]);
        let l2e = pool[l2_idx as int][spec_l2_index(w) as int];
        let l3w = pool_index_spec(pool_base, pl, l2e).unwrap();
        assert(l3w < pu && leaves.contains(l3w));          // L3 table is a leaf
        // w's slot is (l3w, l3_index(w)) != (l3, e); the read at it is unchanged.
        if l3w == l3 {
            assert(spec_l3_index(w) as nat != e);
        }
        assert(pooln[l3w as int][spec_l3_index(w) as int] == pool[l3w as int][spec_l3_index(w) as int]);
    }
}

/// The leaf write does not move `va`'s walk: its L1/L2 path reads only `l1` and
/// **inner** tables (all `!= l3`, since `l3 ∈ leaves`), unchanged by the write.
proof fn lemma_leaf_write_slot(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    l3: nat,
    e: nat,
    va: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        leaves.contains(l3),
        pt_leaf_slot(l1, pool, pool_base, va) == Some((l3, e)),
        pooln.len() == pl,
        forall|t: int| 0 <= t < pl && t != l3 ==> pooln[t] == pool[t],
    ensures
        pt_leaf_slot(l1, pooln, pool_base, va) == Some((l3, e)),
{
    lemma_va_indices(va);
    assert(pool.len() == pl && pooln.len() == pl);
    let l1e = l1[spec_l1_index(va) as int];
    let l2_idx = pool_index_spec(pool_base, pl, l1e).unwrap();
    assert(l2_idx < pu && !leaves.contains(l2_idx));   // l1 target inner
    assert(l2_idx != l3);
    assert(pooln[l2_idx as int] == pool[l2_idx as int]);
}

/// **The page table is a tree, not a DAG (the chief 5d theorem).** Two distinct
/// page-aligned user VAs resolve to **distinct** leaf slots. The proof runs the
/// no-aliasing clauses backwards: equal leaf slots force (via (c2)) the same L2
/// table + entry, then (via (c1)) the same L1 entry — i.e. equal L1/L2/L3 indices,
/// which for aligned in-range VAs means equal VAs (`bit_vector`), contradiction.
/// This is what makes `map_in`'s per-page leaf writes non-interfering.
proof fn lemma_distinct_pages_slots(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    va1: u64,
    va2: u64,
)
    requires
        pt_wf(l1, pool, pool_base, pu, pl),
        va1 % PAGE == 0,
        va2 % PAGE == 0,
        USER_VA_BASE <= va1 < USER_VA_END,
        USER_VA_BASE <= va2 < USER_VA_END,
        va1 != va2,
        pt_leaf_slot(l1, pool, pool_base, va1) is Some,
        pt_leaf_slot(l1, pool, pool_base, va2) is Some,
    ensures
        pt_leaf_slot(l1, pool, pool_base, va1) != pt_leaf_slot(l1, pool, pool_base, va2),
{
    lemma_va_indices(va1);
    lemma_va_indices(va2);
    assert(USER_VA_END == 0x80_0000_0000) by (compute);
    assert(PAGE == 4096 && PAGE_MASK == 4095) by (compute);
    let leaves = choose|lv: Set<nat>| pt_wf_leveled(l1, pool, pool_base, pu, pl, lv);
    if pt_leaf_slot(l1, pool, pool_base, va1) == pt_leaf_slot(l1, pool, pool_base, va2) {
        // Equal slots ⇒ same l3 table and same l3_index.
        let l3 = pt_leaf_slot(l1, pool, pool_base, va1).unwrap().0;
        assert(spec_l3_index(va1) == spec_l3_index(va2));
        // va1/va2's L2 tables and entries.
        let l2a = pool_index_spec(pool_base, pl, l1[spec_l1_index(va1) as int]).unwrap();
        let l2b = pool_index_spec(pool_base, pl, l1[spec_l1_index(va2) as int]).unwrap();
        assert(l2a < pu && !leaves.contains(l2a));   // (b1) inner
        assert(l2b < pu && !leaves.contains(l2b));
        // Both L2 descriptors resolve to `l3`; (c2) ⇒ same (table, entry).
        assert(pool_index_spec(pool_base, pl, pool[l2a as int][spec_l2_index(va1) as int]) == Some(l3));
        assert(pool_index_spec(pool_base, pl, pool[l2b as int][spec_l2_index(va2) as int]) == Some(l3));
        assert(l2a == l2b && spec_l2_index(va1) == spec_l2_index(va2));
        // Both L1 entries resolve to `l2a`; (c1) ⇒ same L1 index.
        assert(spec_l1_index(va1) == spec_l1_index(va2));
        // Bridge the `usize` index equalities to the underlying `u64` bit-fields,
        // and `% PAGE == 0` to the `& PAGE_MASK == 0` alignment form.
        assert(((va1 >> 30) & 0x1FF) < 512 && ((va2 >> 30) & 0x1FF) < 512
            && ((va1 >> 21) & 0x1FF) < 512 && ((va2 >> 21) & 0x1FF) < 512
            && ((va1 >> 12) & 0x1FF) < 512 && ((va2 >> 12) & 0x1FF) < 512) by (bit_vector);
        assert(spec_l1_index(va1) == spec_l1_index(va2));
        assert(spec_l2_index(va1) == spec_l2_index(va2));
        assert(spec_l3_index(va1) == spec_l3_index(va2));
        assert((va1 >> 30) & 0x1FF == (va2 >> 30) & 0x1FF);
        assert((va1 >> 21) & 0x1FF == (va2 >> 21) & 0x1FF);
        assert((va1 >> 12) & 0x1FF == (va2 >> 12) & 0x1FF);
        assert(va1 & 4095 == 0) by (bit_vector) requires va1 % 4096 == 0;
        assert(va2 & 4095 == 0) by (bit_vector) requires va2 % 4096 == 0;
        // Equal index triple + aligned + in range ⇒ va1 == va2.
        assert(va1 == va2) by (bit_vector)
            requires
                (va1 >> 30) & 0x1FF == (va2 >> 30) & 0x1FF,
                (va1 >> 21) & 0x1FF == (va2 >> 21) & 0x1FF,
                (va1 >> 12) & 0x1FF == (va2 >> 12) & 0x1FF,
                va1 & 4095 == 0,
                va2 & 4095 == 0,
                va1 < 0x80_0000_0000u64,
                va2 < 0x80_0000_0000u64;
        assert(false);
    }
}

} // verus!

verus! {

/// Walk to `va`'s L3 entry, allocating the L2/L3 tables if absent. Returns the
/// `(pool index, entry index)` of the L3 slot. The presence tests are on the
/// **descriptor tag** (`& DESC_TABLE`), matching [`lookup`]/[`pt_lookup`] — for a
/// well-formed table (entries are `0` or table descriptors) this is identical to
/// the old `== 0`, and it makes the walker total: a non-table-descriptor is
/// treated as absent and re-allocated rather than chased (doc 39).
///
/// Verified against the `pt_wf` tree model: preserves `pt_wf`, grows `pool_used`
/// monotonically, **clobbers no nonzero leaf** (the no-overwrite frame), and on
/// `Ok` returns the in-bounds leaf slot `(l3, l3_index(va))` that `pt_lookup(va)`
/// now resolves to; if `va`'s tables were already present it allocates nothing and
/// leaves `l1`/`pool`/`pool_used` byte-identical (the two-pass enabler).
fn walk_alloc(
    l1: &mut [u64; 512],
    pool: &mut [[u64; 512]],
    pool_used: &mut u64,
    pool_base: u64,
    va: u64,
) -> (r: Result<(usize, usize), MapError>)
    requires
        pool_geom_ok(pool_base, old(pool).len() as nat),
        pt_wf(old(l1)@, old(pool)@, pool_base, *old(pool_used) as nat, old(pool).len() as nat),
        USER_VA_BASE <= va < USER_VA_END,
    ensures
        final(pool).len() == old(pool).len(),
        pt_wf(final(l1)@, final(pool)@, pool_base, *final(pool_used) as nat, final(pool).len() as nat),
        *final(pool_used) >= *old(pool_used),
        forall|w: u64| #![trigger pt_lookup(final(l1)@, final(pool)@, pool_base, w)]
            pt_lookup(old(l1)@, old(pool)@, pool_base, w) is Some
                ==> pt_lookup(final(l1)@, final(pool)@, pool_base, w)
                        == pt_lookup(old(l1)@, old(pool)@, pool_base, w),
        // tables already present ⇒ no allocation ⇒ success (the pass-2 enabler).
        pt_lookup(old(l1)@, old(pool)@, pool_base, va) is Some ==> r is Ok,
        match r {
            Ok((l3, e)) => {
                &&& l3 < final(pool).len()
                &&& e < 512
                &&& e == spec_l3_index(va)
                &&& pt_lookup(final(l1)@, final(pool)@, pool_base, va)
                        == Some(final(pool)@[l3 as int][e as int])
                &&& pt_leaf_slot(final(l1)@, final(pool)@, pool_base, va) == Some((l3 as nat, e as nat))
                &&& pt_wf_leaf(final(l1)@, final(pool)@, pool_base, *final(pool_used) as nat,
                        final(pool).len() as nat, l3 as nat)
                &&& (pt_lookup(old(l1)@, old(pool)@, pool_base, va) is Some
                        ==> *final(pool_used) == *old(pool_used)
                            && final(l1)@ == old(l1)@ && final(pool)@ == old(pool)@)
            }
            Err(e) => e == MapError::NeedMemory,
        },
{
    let ghost pl = pool.len() as nat;
    let ghost l1_0 = l1@;
    let ghost pool_0 = pool@;
    let ghost pu_0 = *pool_used;
    proof {
        lemma_va_indices(va);
        lemma_desc_tag(0);
    }
    let plen = pool.len();
    let l1i = l1_index(va);

    // ── L1 level: ensure `l1[l1i]` is a present table descriptor ──
    if l1[l1i] & DESC_TABLE != DESC_TABLE {
        assert(pt_lookup(l1_0, pool_0, pool_base, va) is None);  // l1 entry absent
        let idx = alloc_table(pool, pool_used)?;
        let ghost pool_1 = pool@;
        let pa = pa_of_table(pool_base, plen, idx);
        let desc = pa | DESC_TABLE;
        proof {
            lemma_desc_roundtrip(pool_base, pl, idx as nat, pa);
            lemma_link_l1(l1_0, pool_0, pool_1, pool_base, pu_0 as nat, pl, l1i as int, desc);
        }
        l1[l1i] = desc;
    }
    // Post-L1: `l1[l1i]` present, `pt_wf` holds, every lookup preserved from entry.
    assert(l1@[l1i as int] & DESC_TABLE == DESC_TABLE);
    assert(pt_wf(l1@, pool@, pool_base, *pool_used as nat, pl));
    assert(*pool_used >= pu_0);
    assert(pool@.len() == pl);
    assert forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
        pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w) by {};
    let ghost l1_a = l1@;
    let ghost pool_a = pool@;
    let ghost pu_a = *pool_used;

    // The L1 target is an inner table (by closure (b1)); name the witness so the
    // L2 link knows it.
    let ghost leaves_a = choose|lv: Set<nat>| pt_wf_leveled(l1_a, pool_a, pool_base, pu_a as nat, pl, lv);
    assert(pool_index_spec(pool_base, pl, l1_a[l1i as int]) is Some
        && pool_index_spec(pool_base, pl, l1_a[l1i as int]).unwrap() < pu_a as nat
        && !leaves_a.contains(pool_index_spec(pool_base, pl, l1_a[l1i as int]).unwrap()));
    let l2_idx = match pool_index(pool_base, plen, l1[l1i]) {
        Some(i) => i,
        None => { assert(false); return Err(MapError::NeedMemory); }
    };
    let l2i = l2_index(va);
    assert(l2_idx < pu_a && !leaves_a.contains(l2_idx as nat));

    // ── L2 level: ensure `pool[l2_idx][l2i]` is a present table descriptor ──
    if pool[l2_idx][l2i] & DESC_TABLE != DESC_TABLE {
        assert(pt_lookup(l1_a, pool_a, pool_base, va) is None);  // L2 entry absent
        let idx = alloc_table(pool, pool_used)?;
        let ghost pool_b = pool@;
        let pa = pa_of_table(pool_base, plen, idx);
        let desc = pa | DESC_TABLE;
        pool[l2_idx][l2i] = desc;
        proof {
            lemma_desc_roundtrip(pool_base, pl, idx as nat, pa);
            lemma_link_l2(l1_a, pool_a, pool@, pool_base, pu_a as nat, pl, leaves_a,
                l2_idx as int, l2i as int, desc);
        }
    }
    // Post-L2: `pool[l2_idx][l2i]` present, `pt_wf` holds, nonzero leaves preserved.
    assert(l1@[l1i as int] & DESC_TABLE == DESC_TABLE);
    assert(pool@[l2_idx as int][l2i as int] & DESC_TABLE == DESC_TABLE);
    assert(pt_wf(l1@, pool@, pool_base, *pool_used as nat, pl));
    assert forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
        pt_lookup(l1_0, pool_0, pool_base, w) is Some
            implies pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w) by {};

    let l3_idx = match pool_index(pool_base, plen, pool[l2_idx][l2i]) {
        Some(i) => i,
        None => { assert(false); return Err(MapError::NeedMemory); }
    };
    let e = l3_index(va);
    proof {
        lemma_walk_alloc_resolves(l1@, pool@, pool_base, *pool_used as nat, pl,
            l1i as nat, l2_idx as nat, l2i as nat, l3_idx as nat, va);
    }
    Ok((l3_idx, e))
}

/// `pt_lookup(va)` resolves to the leaf slot `walk_alloc` returns: given the L1 and
/// L2 entries are present and resolve (through the closure) to `l2_idx`/`l3_idx`,
/// the lookup yields `Some(pool[l3_idx][l3_index(va)])`.
proof fn lemma_walk_alloc_resolves(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    l1i: nat,
    l2_idx: nat,
    l2i: nat,
    l3_idx: nat,
    va: u64,
)
    requires
        pt_wf(l1, pool, pool_base, pu, pl),
        l1i == spec_l1_index(va),
        l2i == spec_l2_index(va),
        l1[l1i as int] & DESC_TABLE == DESC_TABLE,
        pool_index_spec(pool_base, pl, l1[l1i as int]) == Some(l2_idx),
        pool[l2_idx as int][l2i as int] & DESC_TABLE == DESC_TABLE,
        pool_index_spec(pool_base, pl, pool[l2_idx as int][l2i as int]) == Some(l3_idx),
        pool.len() == pl,
    ensures
        pt_lookup(l1, pool, pool_base, va) == Some(pool[l3_idx as int][spec_l3_index(va) as int]),
        pt_leaf_slot(l1, pool, pool_base, va) == Some((l3_idx, spec_l3_index(va) as nat)),
        pt_wf_leaf(l1, pool, pool_base, pu, pl, l3_idx),
{
    lemma_va_indices(va);
    assert(pool.len() == pl);
    assert(pool_index_spec(pool_base, pool.len(), l1[l1i as int]) == Some(l2_idx));
    assert(pool_index_spec(pool_base, pool.len(), pool[l2_idx as int][l2i as int]) == Some(l3_idx));
    // l3_idx is a leaf: the inner L1 target l2_idx's present descriptor resolves
    // (by closure (b2)) into `leaves`.
    let leaves = choose|lv: Set<nat>| pt_wf_leveled(l1, pool, pool_base, pu, pl, lv);
    assert(l1[l1i as int] & DESC_TABLE == DESC_TABLE);              // (b1) trigger
    assert(pool_index_spec(pool_base, pl, l1[l1i as int]).unwrap() < pu);  // (b1) closure
    assert(l2_idx < pu && !leaves.contains(l2_idx));               // l1 target inner
    assert(pool[l2_idx as int][l2i as int] & DESC_TABLE == DESC_TABLE);  // (b2) trigger
    assert(leaves.contains(l3_idx));    // (b2): inner table's target is a leaf
    assert(pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves) && leaves.contains(l3_idx));
}

} // verus!

verus! {

/// Read-only walk to `va`'s L3 entry. `None` if any intermediate table is
/// absent. Mirrors the old `l3_lookup`. `pub(crate)` so the §4.5 harnesses can
/// read the installed leaf directly.
///
/// Verified equal to the model [`pt_lookup`]: a `Some((l3, e))` result names the
/// in-bounds leaf slot (`l3 < pool.len()`, `e < 512`) whose value is exactly
/// `pt_lookup`'s leaf PTE; `None` matches `pt_lookup` being `None`. The bounds
/// are what let [`range_mapped_in`] index `pool[l3][e]` safely. The slot is also
/// returned *structurally* as [`pt_leaf_slot`] (`== Some((l3, e))`) — `unmap_in`
/// (5e) needs the slot, not just the value, to hand the leaf-clear frame lemma.
/// The two `?` are spelled as explicit `match`/early-return so the control flow
/// stays in the verified fragment (the 5a convention).
pub(crate) fn lookup(l1: &[u64; 512], pool: &[[u64; 512]], pool_base: u64, va: u64) -> (r: Option<(usize, usize)>)
    ensures
        match r {
            Some((l3, e)) => l3 < pool.len() && e < 512
                && pt_lookup(l1@, pool@, pool_base, va) == Some(pool@[l3 as int][e as int])
                && pt_leaf_slot(l1@, pool@, pool_base, va) == Some((l3 as nat, e as nat)),
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

verus! {

/// The `i`-th page VA/PA of a `map_in` range — `base + i*PAGE` as a `u64` (exact
/// under the no-overflow guarantees: `va_range_ok` for `va`, the `pa` `requires`
/// for `pa`).
pub closed spec fn pg(base: u64, i: int) -> u64 {
    (base + i * (PAGE as int)) as u64
}

/// Map `pages` frames at `pa` into `[va, …)`. Two-pass (like the old `map`):
/// pass 1 allocates the tables along the range and rejects any already-mapped
/// page; pass 2 writes the leaves. Because pass 1 walked the whole range, pass
/// 2 allocates nothing and cannot return `NeedMemory` (the two-pass theorem,
/// `walk_alloc`'s present⇒Ok). Issues the post-map barrier through `store`.
///
/// Verified against the `pt_wf` tree model (doc 39): adds **exactly** the
/// requested pages (`pt_lookup` == `spec_pte_encode`) or fails atomically
/// (`BadVa`/`AlreadyMapped`/`NeedMemory` with no leaf written); preserves `pt_wf`;
/// grows `pool_used` monotonically; and **clobbers no nonzero (mapped) page** (the
/// no-overwrite frame, via the distinct-leaf-slot theorem).
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
) -> (r: Result<(), MapError>)
    requires
        pool_geom_ok(pool_base, old(pool).len() as nat),
        pt_wf(old(l1)@, old(pool)@, pool_base, *old(pool_used) as nat, old(pool).len() as nat),
        (pa as int) + (pages as int) * (PAGE as int) <= u64::MAX as int,
    ensures
        final(pool).len() == old(pool).len(),
        pt_wf(final(l1)@, final(pool)@, pool_base, *final(pool_used) as nat, final(pool).len() as nat),
        *final(pool_used) >= *old(pool_used),
        // no-overwrite frame: every page mapped (nonzero) before is preserved.
        forall|w: u64| #![trigger pt_lookup(final(l1)@, final(pool)@, pool_base, w)]
            (pt_lookup(old(l1)@, old(pool)@, pool_base, w) is Some
                && pt_lookup(old(l1)@, old(pool)@, pool_base, w).unwrap() != 0)
                ==> pt_lookup(final(l1)@, final(pool)@, pool_base, w)
                        == pt_lookup(old(l1)@, old(pool)@, pool_base, w),
        match r {
            Ok(()) => forall|i: int| #![trigger pg(va, i)] 0 <= i < pages ==>
                pt_lookup(final(l1)@, final(pool)@, pool_base, pg(va, i))
                    == Some(spec_pte_encode(pg(pa, i), perms)),
            Err(e) => e == MapError::BadVa || e == MapError::AlreadyMapped || e == MapError::NeedMemory,
        },
{
    let ghost l1_0 = l1@;
    let ghost pool_0 = pool@;
    let ghost pu_0 = *pool_used;
    let ghost pl = pool.len() as nat;
    proof { assert(USER_VA_END == 0x80_0000_0000) by (compute); }
    if !va_range_ok(va, pages) {
        return Err(MapError::BadVa);
    }
    // ── Pass 1: walk-allocate the tables, reject any already-mapped page ──
    let mut i: u64 = 0;
    while i < pages
        invariant
            i <= pages,
            pool.len() == pl,
            pool.len() == old(pool).len(),
            l1_0 == old(l1)@,
            pool_0 == old(pool)@,
            *pool_used >= *old(pool_used),
            pool_geom_ok(pool_base, pl),
            pt_wf(l1@, pool@, pool_base, *pool_used as nat, pl),
            *pool_used >= pu_0,
            va % PAGE == 0,
            va >= USER_VA_BASE,
            (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
            forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
                pt_lookup(l1_0, pool_0, pool_base, w) is Some
                    ==> pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w),
            forall|j: int| #![trigger pg(va, j)] 0 <= j < i ==>
                pt_lookup(l1@, pool@, pool_base, pg(va, j)) == Some(0u64),
        decreases pages - i,
    {
        proof { lemma_pg_in_range(va, pages, i as int); }
        let page = va + i * PAGE;
        let ghost before_l1 = l1@;
        let ghost before_pool = pool@;
        let res = walk_alloc(l1, pool, pool_used, pool_base, page);
        // The frame composes S0→(loop head)→(post-walk_alloc): walk_alloc preserves
        // every present page, so the early-exit error returns still frame S0's
        // nonzero pages.
        proof {
            assert forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
                (pt_lookup(l1_0, pool_0, pool_base, w) is Some
                    && pt_lookup(l1_0, pool_0, pool_base, w).unwrap() != 0) implies
                pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w) by {
                assert(pt_lookup(before_l1, before_pool, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w));
            }
        }
        let (l3, e) = match res {
            Ok(x) => x,
            Err(er) => return Err(er),
        };
        if pool[l3][e] != 0 {
            return Err(MapError::AlreadyMapped);
        }
        assert(pt_lookup(l1@, pool@, pool_base, page) == Some(0u64));
        assert(page == pg(va, i as int));
        assert forall|j: int| #![trigger pg(va, j)] 0 <= j < i + 1 implies
            pt_lookup(l1@, pool@, pool_base, pg(va, j)) == Some(0u64) by {
            if j < i as int {
                assert(pt_lookup(before_l1, before_pool, pool_base, pg(va, j)) == Some(0u64));
            } else {
                assert(pg(va, j) == page);
            }
        }
        i = i + 1;
    }
    // ── Pass 2: write the leaves; pass 1 guarantees no allocation is needed ──
    let mut k: u64 = 0;
    while k < pages
        invariant
            k <= pages,
            pool.len() == pl,
            pool.len() == old(pool).len(),
            l1_0 == old(l1)@,
            pool_0 == old(pool)@,
            *pool_used >= *old(pool_used),
            pool_geom_ok(pool_base, pl),
            pt_wf(l1@, pool@, pool_base, *pool_used as nat, pl),
            *pool_used >= pu_0,
            va % PAGE == 0,
            va >= USER_VA_BASE,
            (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
            (pa as int) + (pages as int) * (PAGE as int) <= u64::MAX as int,
            forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
                (pt_lookup(l1_0, pool_0, pool_base, w) is Some
                    && pt_lookup(l1_0, pool_0, pool_base, w).unwrap() != 0)
                    ==> pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w),
            forall|j: int| #![trigger pg(va, j)] 0 <= j < k ==>
                pt_lookup(l1@, pool@, pool_base, pg(va, j)) == Some(spec_pte_encode(pg(pa, j), perms)),
            forall|j: int| #![trigger pg(va, j)] k <= j < pages ==>
                pt_lookup(l1@, pool@, pool_base, pg(va, j)) == Some(0u64),
        decreases pages - k,
    {
        proof { lemma_pg_in_range(va, pages, k as int); }
        let page = va + k * PAGE;
        assert(page == pg(va, k as int));
        assert(pt_lookup(l1@, pool@, pool_base, page) == Some(0u64));   // present (pass 1) → alloc-free
        let res = walk_alloc(l1, pool, pool_used, pool_base, page);
        let (l3, e) = match res {
            Ok(x) => x,
            Err(er) => { assert(false); return Err(er); }
        };
        // alloc-free: tables present ⇒ l1/pool/pool_used unchanged.
        assert(pool@[l3 as int][e as int] == 0u64);
        let ghost before = pool@;
        let ghost puv = *pool_used as nat;
        let pte = pte_encode(pa + k * PAGE, perms);
        proof { lemma_pg_pa(pa, pages, k as int); }
        assert(pte == spec_pte_encode(pg(pa, k as int), perms));
        let ghost leaves = choose|lv: Set<nat>|
            pt_wf_leveled(l1@, before, pool_base, puv, pl, lv) && lv.contains(l3 as nat);
        pool[l3][e] = pte;
        proof {
            lemma_leaf_write(l1@, before, pool@, pool_base, puv, pl, leaves,
                l3 as nat, e as nat, page, pte);
            lemma_map_in_step(l1@, before, pool@, pool_base, puv, pl, l1_0, pool_0,
                va, pa, pages, perms, k as int, l3 as nat, e as nat);
        }
        k = k + 1;
    }
    proof {
        assert(pool.len() == pl);
        assert(*pool_used >= pu_0);
        assert(pt_wf(l1@, pool@, pool_base, *pool_used as nat, pl));
    }
    store.barrier_after_map();
    proof {
        assert(pool.len() == pl);
        assert(*pool_used >= pu_0);
        assert forall|i: int| #![trigger pg(va, i)] 0 <= i < pages implies
            pt_lookup(l1@, pool@, pool_base, pg(va, i)) == Some(spec_pte_encode(pg(pa, i), perms)) by {};
    }
    Ok(())
}

/// `va + i*PAGE` (for `0 <= i < pages` under `va_range_ok`) is a page-aligned user
/// VA, overflow-free, equal to `pg(va, i)` — the per-page bound `map_in`'s loops
/// hand `walk_alloc`.
proof fn lemma_pg_in_range(va: u64, pages: u64, i: int)
    requires
        va % PAGE == 0,
        va >= USER_VA_BASE,
        (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
        0 <= i < pages,
    ensures
        pg(va, i) as int == (va as int) + i * (PAGE as int),
        USER_VA_BASE <= pg(va, i) < USER_VA_END,
        pg(va, i) % PAGE == 0,
{
    assert(USER_VA_END == 0x80_0000_0000) by (compute);
    assert(i * (PAGE as int) < (pages as int) * (PAGE as int)) by (nonlinear_arith)
        requires 0 <= i, i < pages, PAGE > 0;
    assert(i * (PAGE as int) >= 0) by (nonlinear_arith) requires i >= 0, PAGE > 0;
    // `va + i*PAGE < USER_VA_END < 2^64`, so the `as u64` in `pg` is exact.
    assert((va as int) + i * (PAGE as int) < USER_VA_END as int);
    assert(pg(va, i) as int == (va as int) + i * (PAGE as int));
    assert((i * (PAGE as int)) % (PAGE as int) == 0) by (nonlinear_arith) requires PAGE > 0;
    assert(pg(va, i) % PAGE == 0) by (nonlinear_arith)
        requires
            pg(va, i) as int == (va as int) + i * (PAGE as int),
            (va as int) % (PAGE as int) == 0,
            (i * (PAGE as int)) % (PAGE as int) == 0,
            PAGE > 0;
}

/// `pa + i*PAGE` is overflow-free and equals `pg(pa, i)` (under the `pa` bound).
proof fn lemma_pg_pa(pa: u64, pages: u64, i: int)
    requires
        (pa as int) + (pages as int) * (PAGE as int) <= u64::MAX as int,
        0 <= i < pages,
    ensures
        pa + (i as u64) * PAGE == pg(pa, i),
        (pa as int) + i * (PAGE as int) <= u64::MAX as int,
{
    assert(i * (PAGE as int) <= (pages as int) * (PAGE as int)) by (nonlinear_arith)
        requires 0 <= i, i < pages, PAGE > 0;
    assert(i * (PAGE as int) >= 0) by (nonlinear_arith) requires i >= 0, PAGE > 0;
}

/// Pass-2 step `k`: after the leaf write at page `k`, the already-written pages
/// (`j < k`), the page just written (`j == k`), and the unwritten pages (`j > k`)
/// all hold their intended values, and every nonzero pre-existing page is framed —
/// each via the distinct-slot/leaf-write frame.
proof fn lemma_map_in_step(
    l1: Seq<u64>,
    before: Seq<[u64; 512]>,
    after: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    l1_0: Seq<u64>,
    pool_0: Seq<[u64; 512]>,
    va: u64,
    pa: u64,
    pages: u64,
    perms: u64,
    k: int,
    l3: nat,
    e: nat,
)
    requires
        va % PAGE == 0,
        va >= USER_VA_BASE,
        (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
        0 <= k < pages,
        pt_wf(l1, before, pool_base, pu, pl),
        before.len() == pl,
        pt_leaf_slot(l1, before, pool_base, pg(va, k)) == Some((l3, e)),
        // the leaf-write frame facts (from lemma_leaf_write on this step):
        pt_wf(l1, after, pool_base, pu, pl),
        pt_lookup(l1, after, pool_base, pg(va, k)) == Some(spec_pte_encode(pg(pa, k), perms)),
        forall|w: u64| #![trigger pt_lookup(l1, after, pool_base, w)]
            (pt_leaf_slot(l1, before, pool_base, w) is Some
                && pt_leaf_slot(l1, before, pool_base, w) != Some((l3, e)))
                ==> pt_lookup(l1, after, pool_base, w) == pt_lookup(l1, before, pool_base, w),
        // pre-step invariants:
        forall|j: int| #![trigger pg(va, j)] 0 <= j < k ==>
            pt_lookup(l1, before, pool_base, pg(va, j)) == Some(spec_pte_encode(pg(pa, j), perms)),
        forall|j: int| #![trigger pg(va, j)] k <= j < pages ==>
            pt_lookup(l1, before, pool_base, pg(va, j)) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1, before, pool_base, w)]
            (pt_lookup(l1_0, pool_0, pool_base, w) is Some
                && pt_lookup(l1_0, pool_0, pool_base, w).unwrap() != 0)
                ==> pt_lookup(l1, before, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w),
    ensures
        forall|j: int| #![trigger pg(va, j)] 0 <= j < k + 1 ==>
            pt_lookup(l1, after, pool_base, pg(va, j)) == Some(spec_pte_encode(pg(pa, j), perms)),
        forall|j: int| #![trigger pg(va, j)] k + 1 <= j < pages ==>
            pt_lookup(l1, after, pool_base, pg(va, j)) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1, after, pool_base, w)]
            (pt_lookup(l1_0, pool_0, pool_base, w) is Some
                && pt_lookup(l1_0, pool_0, pool_base, w).unwrap() != 0)
                ==> pt_lookup(l1, after, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w),
{
    // The written page's slot value before the write was 0, so anything with a
    // nonzero looked-up value reads a *different* slot (used for the `w` frame).
    lemma_leaf_slot_lookup(l1, before, pool_base, pg(va, k));
    // (1) written + unwritten in-range pages: a different in-range page has a
    // distinct slot (the tree theorem), so the write does not touch it.
    assert forall|j: int| #![trigger pg(va, j)] 0 <= j < pages && j != k implies
        pt_lookup(l1, after, pool_base, pg(va, j)) == pt_lookup(l1, before, pool_base, pg(va, j)) by {
        lemma_pg_in_range(va, pages, j);
        lemma_pg_in_range(va, pages, k);
        lemma_pg_distinct(va, pages, j, k);
        assert(pt_leaf_slot(l1, before, pool_base, pg(va, j)) is Some);
        lemma_distinct_pages_slots(l1, before, pool_base, pu, pl, pg(va, j), pg(va, k));
    }
    // (2) nonzero pre-existing pages: their value != 0 == the written slot's old
    // value, so their slot != (l3, e), so they are framed.
    assert forall|w: u64| #![trigger pt_lookup(l1, after, pool_base, w)]
        (pt_lookup(l1_0, pool_0, pool_base, w) is Some
            && pt_lookup(l1_0, pool_0, pool_base, w).unwrap() != 0) implies
        pt_lookup(l1, after, pool_base, w) == pt_lookup(l1_0, pool_0, pool_base, w) by {
        lemma_leaf_slot_lookup(l1, before, pool_base, w);
        assert(pt_lookup(l1, before, pool_base, w).unwrap() != 0);  // == S0's nonzero value
    }
}

/// Distinct in-range page offsets give distinct page VAs (a `bit_vector` corollary
/// used to invoke [`lemma_distinct_pages_slots`] in pass 2).
proof fn lemma_pg_distinct(va: u64, pages: u64, j: int, k: int)
    requires
        va % PAGE == 0,
        va >= USER_VA_BASE,
        (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
        0 <= j < pages,
        0 <= k < pages,
        j != k,
    ensures
        pg(va, j) != pg(va, k),
{
    lemma_pg_in_range(va, pages, j);
    lemma_pg_in_range(va, pages, k);
    assert((va as int) + j * (PAGE as int) != (va as int) + k * (PAGE as int)) by (nonlinear_arith)
        requires j != k, PAGE > 0;
}

} // verus!

verus! {

/// The TLBI log `unmap_in` issues over `[va, va+n*PAGE)`: one `(asid, pg(va,j))`
/// entry per **present** page (`pt_lookup` of the *original* table is `Some`), in
/// ascending `j` order — the §4.5 "one TLBI per cleared page, in order" as a
/// closed-form spec (the `expected_tlb_log` of the detail plan). Built over the
/// original tables: a clear sets a leaf to `0` (still `Some`), so a page's
/// presence is invariant across the unmap, and the runtime branch (`lookup` of the
/// *current* table) agrees with this original-table predicate (bridged by the
/// frame invariant). `pub closed` so `unmap_in`'s public ensures may name it
/// without leaking the walk (the `pg`/`spec_pte_encode` idiom).
pub closed spec fn unmap_log(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    asid: u16,
    va: u64,
    n: nat,
) -> Seq<(u16, u64)>
    decreases n,
{
    if n == 0 {
        Seq::empty()
    } else {
        let prev = unmap_log(l1, pool, pool_base, asid, va, (n - 1) as nat);
        if pt_lookup(l1, pool, pool_base, pg(va, (n - 1) as int)) is Some {
            prev.push((asid, pg(va, (n - 1) as int)))
        } else {
            prev
        }
    }
}

/// One-step unfold of [`unmap_log`]: clearing `n = i+1` pages appends page `i`'s
/// TLBI iff page `i` was present. The recursive `closed` fn needs the explicit
/// successor reveal (fuel) to unfold a symbolic `(i+1) as nat`.
proof fn lemma_unmap_log_step(l1: Seq<u64>, pool: Seq<[u64; 512]>, pool_base: u64, asid: u16, va: u64, i: int)
    requires 0 <= i,
    ensures
        unmap_log(l1, pool, pool_base, asid, va, (i + 1) as nat)
            == (if pt_lookup(l1, pool, pool_base, pg(va, i)) is Some {
                    unmap_log(l1, pool, pool_base, asid, va, i as nat).push((asid, pg(va, i)))
                } else {
                    unmap_log(l1, pool, pool_base, asid, va, i as nat)
                }),
{
    reveal_with_fuel(unmap_log, 2);
    assert((i + 1) as nat > 0);
    assert(((i + 1) as nat - 1) as nat == i as nat);
    assert(pg(va, ((i + 1) as nat - 1) as int) == pg(va, i));
}

/// A present leaf slot lives in a **leaf** table: if `va` resolves to slot
/// `(l3, e)` then `l3 ∈ leaves` (and `l3 < pool_used`). The same closure walk as
/// [`lemma_walk_alloc_resolves`]' tail, but starting from `pt_leaf_slot` (what
/// `lookup` hands `unmap_in`), so the leaf-clear can invoke [`lemma_leaf_write`].
proof fn lemma_present_leaf_in_leaves(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    va: u64,
    l3: nat,
    e: nat,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        pool.len() == pl,
        pt_leaf_slot(l1, pool, pool_base, va) == Some((l3, e)),
    ensures
        leaves.contains(l3),
        l3 < pu,
{
    lemma_va_indices(va);
    let l1e = l1[spec_l1_index(va) as int];
    assert(l1e & DESC_TABLE == DESC_TABLE);            // else pt_leaf_slot is None
    assert(pool_index_spec(pool_base, pl, l1e) is Some
        && pool_index_spec(pool_base, pl, l1e).unwrap() < pu
        && !leaves.contains(pool_index_spec(pool_base, pl, l1e).unwrap()));  // (b1)
    let l2_idx = pool_index_spec(pool_base, pl, l1e).unwrap();
    let l2e = pool[l2_idx as int][spec_l2_index(va) as int];
    assert(l2e & DESC_TABLE == DESC_TABLE);            // else pt_leaf_slot is None
    assert(pool_index_spec(pool_base, pl, l2e) == Some(l3));  // from pt_leaf_slot == Some((l3,e))
    assert(pool_index_spec(pool_base, pl, l2e) is Some
        && pool_index_spec(pool_base, pl, l2e).unwrap() < pu
        && leaves.contains(pool_index_spec(pool_base, pl, l2e).unwrap()));  // (b2)
}

/// An **absent** page's lookup is unchanged by a leaf-table clear: a `None` walk
/// dead-ends at `l1` or an **inner** table (all `!= l3`, since `l3 ∈ leaves`), so
/// zeroing a leaf entry leaves it `None`. The `None`-companion of
/// [`lemma_leaf_write`]'s frame (which only covers present pages), needed because
/// `unmap_in`'s frame ranges over pages that may be unmapped.
proof fn lemma_leaf_clear_none(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    l3: nat,
    w: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        leaves.contains(l3),
        pool.len() == pl,
        pooln.len() == pl,
        forall|t: int| 0 <= t < pl && t != l3 ==> pooln[t] == pool[t],
        pt_leaf_slot(l1, pool, pool_base, w) is None,
    ensures
        pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    lemma_va_indices(w);
    let l1e = l1[spec_l1_index(w) as int];
    if l1e & DESC_TABLE == DESC_TABLE {
        assert(pool_index_spec(pool_base, pl, l1e) is Some
            && pool_index_spec(pool_base, pl, l1e).unwrap() < pu
            && !leaves.contains(pool_index_spec(pool_base, pl, l1e).unwrap()));  // (b1)
        let l2_idx = pool_index_spec(pool_base, pl, l1e).unwrap();
        assert(l2_idx != l3);                          // inner != leaf
        assert(pooln[l2_idx as int] == pool[l2_idx as int]);
        let l2e = pool[l2_idx as int][spec_l2_index(w) as int];
        // `pt_leaf_slot(w)` is None, so `l2e` cannot be a present table descriptor
        // (else (b2) would resolve it to a leaf, making `pt_leaf_slot` Some).
        assert(l2e & DESC_TABLE != DESC_TABLE) by {
            if l2e & DESC_TABLE == DESC_TABLE {
                assert(pool_index_spec(pool_base, pl, l2e) is Some
                    && leaves.contains(pool_index_spec(pool_base, pl, l2e).unwrap()));  // (b2)
                assert(pt_leaf_slot(l1, pool, pool_base, w) is Some);
                assert(false);
            }
        }
    }
    // Both walks dead-end identically (only the leaf table `l3` changed; the dead-
    // end reads `l1` + inner tables, none of which is `l3`).
    assert(pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w));
}

/// Clearing the leaf slot `(l3, e)` that `va` resolves to (writing `0`): preserves
/// `pt_wf`, makes `va` read `Some(0)` (unmapped), and **frames every page whose
/// leaf slot differs from `(l3, e)`** — present or absent. [`lemma_leaf_write`]
/// (with `pte == 0`) gives the present-page frame; [`lemma_leaf_clear_none`] adds
/// the absent-page case, so the unified frame covers all of `unmap_in`'s range.
proof fn lemma_leaf_clear(
    l1: Seq<u64>,
    pool: Seq<[u64; 512]>,
    pooln: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    leaves: Set<nat>,
    l3: nat,
    e: nat,
    va: u64,
)
    requires
        pt_wf_leveled(l1, pool, pool_base, pu, pl, leaves),
        leaves.contains(l3),
        e < 512,
        pool.len() == pl,
        pt_leaf_slot(l1, pool, pool_base, va) == Some((l3, e)),
        pooln.len() == pl,
        pooln[l3 as int][e as int] == 0,
        forall|j: int| 0 <= j < 512 && j != e ==> pooln[l3 as int][j] == pool[l3 as int][j],
        forall|t: int| 0 <= t < pl && t != l3 ==> pooln[t] == pool[t],
    ensures
        pt_wf(l1, pooln, pool_base, pu, pl),
        pt_lookup(l1, pooln, pool_base, va) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1, pooln, pool_base, w)]
            pt_leaf_slot(l1, pool, pool_base, w) != Some((l3, e))
                ==> pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w),
{
    lemma_leaf_write(l1, pool, pooln, pool_base, pu, pl, leaves, l3, e, va, 0u64);
    assert forall|w: u64| #![trigger pt_lookup(l1, pooln, pool_base, w)]
        pt_leaf_slot(l1, pool, pool_base, w) != Some((l3, e))
            implies pt_lookup(l1, pooln, pool_base, w) == pt_lookup(l1, pool, pool_base, w) by {
        if pt_leaf_slot(l1, pool, pool_base, w) is Some {
            // present-but-other-slot — lemma_leaf_write's frame applies.
        } else {
            lemma_leaf_clear_none(l1, pool, pooln, pool_base, pu, pl, leaves, l3, w);
        }
    }
}

/// Per-step advance of `unmap_in`'s "range-unmapped" (A) + "outside-range framed"
/// (C) invariants for a **cleared** page `i` — the `unmap` analog of
/// [`lemma_map_in_step`]. Distinct in-range/outside-range pages have distinct leaf
/// slots (the tree theorem [`lemma_distinct_pages_slots`]), so clearing page `i`'s
/// slot leaves every other tracked page untouched.
proof fn lemma_unmap_in_step(
    l1: Seq<u64>,
    before: Seq<[u64; 512]>,
    after: Seq<[u64; 512]>,
    pool_base: u64,
    pu: nat,
    pl: nat,
    pool_0: Seq<[u64; 512]>,
    va: u64,
    pages: u64,
    i: int,
    l3: nat,
    e: nat,
)
    requires
        va % PAGE == 0,
        va >= USER_VA_BASE,
        (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
        0 <= i < pages,
        pt_wf(l1, before, pool_base, pu, pl),
        before.len() == pl,
        pt_leaf_slot(l1, before, pool_base, pg(va, i)) == Some((l3, e)),
        pt_lookup(l1, after, pool_base, pg(va, i)) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1, after, pool_base, w)]
            pt_leaf_slot(l1, before, pool_base, w) != Some((l3, e))
                ==> pt_lookup(l1, after, pool_base, w) == pt_lookup(l1, before, pool_base, w),
        forall|j: int| #![trigger pg(va, j)] 0 <= j < i ==>
            pt_lookup(l1, before, pool_base, pg(va, j)) is None
                || pt_lookup(l1, before, pool_base, pg(va, j)) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1, before, pool_base, w)]
            (USER_VA_BASE <= w < USER_VA_END && w % PAGE == 0
                && ((w as int) < (va as int) || (w as int) >= (va as int) + i * (PAGE as int)))
                ==> pt_lookup(l1, before, pool_base, w) == pt_lookup(l1, pool_0, pool_base, w),
    ensures
        forall|j: int| #![trigger pg(va, j)] 0 <= j < i + 1 ==>
            pt_lookup(l1, after, pool_base, pg(va, j)) is None
                || pt_lookup(l1, after, pool_base, pg(va, j)) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1, after, pool_base, w)]
            (USER_VA_BASE <= w < USER_VA_END && w % PAGE == 0
                && ((w as int) < (va as int) || (w as int) >= (va as int) + (i + 1) * (PAGE as int)))
                ==> pt_lookup(l1, after, pool_base, w) == pt_lookup(l1, pool_0, pool_base, w),
{
    lemma_pg_in_range(va, pages, i);
    assert((i + 1) * (PAGE as int) == i * (PAGE as int) + (PAGE as int)) by (nonlinear_arith);
    assert(i * (PAGE as int) >= 0) by (nonlinear_arith) requires i >= 0, PAGE > 0;
    // (A) advance: every page `j ≤ i` is unmapped after the clear.
    assert forall|j: int| #![trigger pg(va, j)] 0 <= j < i + 1 implies
        (pt_lookup(l1, after, pool_base, pg(va, j)) is None
            || pt_lookup(l1, after, pool_base, pg(va, j)) == Some(0u64)) by {
        if j < i {
            lemma_pg_in_range(va, pages, j);
            lemma_pg_distinct(va, pages, j, i);
            if pt_leaf_slot(l1, before, pool_base, pg(va, j)) is Some {
                lemma_distinct_pages_slots(l1, before, pool_base, pu, pl, pg(va, j), pg(va, i));
            }
            // pg(va,j)'s slot != (l3,e) ⟹ framed; A_i carries.
        }
        // j == i: pt_lookup(after, pg(va,i)) == Some(0).
    }
    // (C) advance: every aligned user page outside [va, va+(i+1)*PAGE) is framed.
    assert forall|w: u64| #![trigger pt_lookup(l1, after, pool_base, w)]
        (USER_VA_BASE <= w < USER_VA_END && w % PAGE == 0
            && ((w as int) < (va as int) || (w as int) >= (va as int) + (i + 1) * (PAGE as int)))
        implies pt_lookup(l1, after, pool_base, w) == pt_lookup(l1, pool_0, pool_base, w) by {
        assert(w != pg(va, i));   // pg(va,i) == va + i*PAGE, strictly inside the gap
        if pt_leaf_slot(l1, before, pool_base, w) is Some {
            lemma_distinct_pages_slots(l1, before, pool_base, pu, pl, w, pg(va, i));
        }
        // w's slot != (l3,e) ⟹ framed to `before`; w also outside [va,va+i*PAGE) ⟹ C_i.
    }
}

/// Unmap `pages` frames at `va`, invalidating each cleared page's TLB entry
/// through `store`. Mirrors the old `unmap` (clear + per-page TLBI wherever the
/// L3 table exists, then a single trailing barrier).
///
/// Verified against the `pt_wf` tree model + the TLBI effect-log (doc 40): on
/// return every page in `[va, va+pages·PAGE)` is unmapped (`pt_lookup` is `None`
/// or `Some(0)`), every aligned user page **outside** the range keeps its mapping
/// (the frame), `pt_wf` is preserved (clearing a leaf keeps the tree — no table is
/// freed), and `store`'s TLBI log grows by **exactly one `(asid, va+i·PAGE)` per
/// cleared page, in ascending order** ([`unmap_log`]) followed by the trailing
/// barrier — the §4.5 "one TLBI per cleared page, in order" as a postcondition.
pub fn unmap_in<S: Store>(
    l1: &[u64; 512],
    pool: &mut [[u64; 512]],
    pool_base: u64,
    asid: u16,
    va: u64,
    pages: u64,
    store: &mut S,
)
    requires
        pool_geom_ok(pool_base, old(pool).len() as nat),
        exists|pu: nat| pt_wf(l1@, old(pool)@, pool_base, pu, old(pool).len() as nat),
        va % PAGE == 0,
        va >= USER_VA_BASE,
        (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
    ensures
        final(pool).len() == old(pool).len(),
        exists|pu: nat| pt_wf(l1@, final(pool)@, pool_base, pu, final(pool).len() as nat),
        forall|i: int| #![trigger pg(va, i)] 0 <= i < pages ==>
            pt_lookup(l1@, final(pool)@, pool_base, pg(va, i)) is None
                || pt_lookup(l1@, final(pool)@, pool_base, pg(va, i)) == Some(0u64),
        forall|w: u64| #![trigger pt_lookup(l1@, final(pool)@, pool_base, w)]
            (USER_VA_BASE <= w < USER_VA_END && w % PAGE == 0
                && ((w as int) < (va as int) || (w as int) >= (va as int) + (pages as int) * (PAGE as int)))
                ==> pt_lookup(l1@, final(pool)@, pool_base, w) == pt_lookup(l1@, old(pool)@, pool_base, w),
        final(store).tlb_log_view()
            == old(store).tlb_log_view() + unmap_log(l1@, old(pool)@, pool_base, asid, va, pages as nat),
{
    broadcast use {vstd::slice::group_slice_axioms, vstd::array::group_array_axioms};
    let ghost pl = pool.len() as nat;
    let ghost pool_0 = pool@;
    let ghost pu = choose|pu: nat| pt_wf(l1@, pool_0, pool_base, pu, pl);
    proof { assert(USER_VA_END == 0x80_0000_0000) by (compute); }
    let mut i: u64 = 0;
    while i < pages
        invariant
            i <= pages,
            pool.len() == pl,
            pool.len() == old(pool).len(),
            pool_0 == old(pool)@,
            pool_geom_ok(pool_base, pl),
            pt_wf(l1@, pool@, pool_base, pu, pl),
            va % PAGE == 0,
            va >= USER_VA_BASE,
            (va as int) + (pages as int) * (PAGE as int) <= USER_VA_END as int,
            USER_VA_END == 0x80_0000_0000,
            // (A) every already-processed page is unmapped.
            forall|j: int| #![trigger pg(va, j)] 0 <= j < i ==>
                pt_lookup(l1@, pool@, pool_base, pg(va, j)) is None
                    || pt_lookup(l1@, pool@, pool_base, pg(va, j)) == Some(0u64),
            // (C) every aligned user page outside [va, va+i*PAGE) keeps its mapping.
            forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
                (USER_VA_BASE <= w < USER_VA_END && w % PAGE == 0
                    && ((w as int) < (va as int) || (w as int) >= (va as int) + (i as int) * (PAGE as int)))
                    ==> pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1@, pool_0, pool_base, w),
            // (E) the TLBI log so far = one entry per cleared page, in order.
            store.tlb_log_view()
                == old(store).tlb_log_view() + unmap_log(l1@, pool_0, pool_base, asid, va, i as nat),
        decreases pages - i,
    {
        proof { lemma_pg_in_range(va, pages, i as int); }
        let page = va + i * PAGE;
        assert(page == pg(va, i as int));
        let ghost before = pool@;
        proof { lemma_unmap_log_step(l1@, pool_0, pool_base, asid, va, i as int); }
        // C_i at `page` (page == va+i*PAGE is outside [va, va+i*PAGE)) bridges the
        // current-table presence test to the original-table `unmap_log` predicate.
        assert((page as int) >= (va as int) + (i as int) * (PAGE as int));
        assert(pt_lookup(l1@, before, pool_base, page) == pt_lookup(l1@, pool_0, pool_base, page));
        match lookup(l1, pool, pool_base, page) {
            Some((l3, e)) => {
                let ghost leaves = choose|lv: Set<nat>| pt_wf_leveled(l1@, before, pool_base, pu, pl, lv);
                proof { lemma_present_leaf_in_leaves(l1@, before, pool_base, pu, pl, leaves, page, l3 as nat, e as nat); }
                pool[l3][e] = 0;
                proof {
                    lemma_leaf_clear(l1@, before, pool@, pool_base, pu, pl, leaves, l3 as nat, e as nat, page);
                    lemma_unmap_in_step(l1@, before, pool@, pool_base, pu, pl, pool_0, va, pages, i as int,
                        l3 as nat, e as nat);
                }
                store.tlb_invalidate_page(asid, page);
                proof {
                    // (E) advance: present ⟹ unmap_log gained (asid, page); push distributes over +.
                    assert(pt_lookup(l1@, pool_0, pool_base, pg(va, i as int)) is Some);
                    assert(store.tlb_log_view()
                        =~= old(store).tlb_log_view() + unmap_log(l1@, pool_0, pool_base, asid, va, (i + 1) as nat));
                }
            }
            None => {
                // Page absent: no L3 entry to clear, no TLBI. Pool + log unchanged.
                proof {
                    assert(pool@ == before);
                    // (A) j == i: the page is unmapped (None); (C) shrinks; (E) no push.
                    assert(pt_lookup(l1@, pool@, pool_base, pg(va, i as int)) is None);
                    assert(pt_lookup(l1@, pool_0, pool_base, pg(va, i as int)) is None);
                    assert(store.tlb_log_view()
                        =~= old(store).tlb_log_view() + unmap_log(l1@, pool_0, pool_base, asid, va, (i + 1) as nat));
                    assert forall|w: u64| #![trigger pt_lookup(l1@, pool@, pool_base, w)]
                        (USER_VA_BASE <= w < USER_VA_END && w % PAGE == 0
                            && ((w as int) < (va as int) || (w as int) >= (va as int) + ((i + 1) as int) * (PAGE as int)))
                        implies pt_lookup(l1@, pool@, pool_base, w) == pt_lookup(l1@, pool_0, pool_base, w) by {
                        assert((i + 1) as int * (PAGE as int) >= (i as int) * (PAGE as int)) by (nonlinear_arith)
                            requires PAGE > 0, i >= 0;
                    }
                    assert forall|j: int| #![trigger pg(va, j)] 0 <= j < (i + 1) as int implies
                        (pt_lookup(l1@, pool@, pool_base, pg(va, j)) is None
                            || pt_lookup(l1@, pool@, pool_base, pg(va, j)) == Some(0u64)) by {
                        if j == i as int { assert(pg(va, j) == page); }
                    }
                }
            }
        }
        i = i + 1;
    }
    store.barrier_after_unmap();
    proof {
        // The barrier frames both the page tables (it takes neither slice) and the
        // accumulated TLBI log, so the loop-exit invariants are the postconditions.
        assert(pt_wf(l1@, pool@, pool_base, pu, pl));
    }
}

} // verus!

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
        l1[l1_index(va)] = pa_of_table(pool_base, 2, 0) | DESC_TABLE;
        pool[0][l2_index(va)] = pa_of_table(pool_base, 2, 1) | DESC_TABLE;
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
