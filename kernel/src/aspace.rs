// SPDX-License-Identifier: 0BSD
//! Kernel-side address-space shell (spec rev2§2.5). The walker itself — table
//! allocation, `map`/`unmap`, the read-only lookup, the descriptor bit
//! assembly, the VA-index arithmetic — lives in [`kcore::aspace`] as safe
//! Rust over the table pool as an indexed slice, where it is Verus-verified.
//! What stays here is the irreducibly architectural half: ASID
//! assignment, the boot kernel-L1 copy, `ttbr0`, and the **one sanctioned
//! int→pointer boundary** — building the `&mut [[u64; 512]]` slice views over
//! the `AspaceObj`'s physical L1/pool addresses so the kcore walker can drive
//! them. The TLBI/DSB maintenance the walker needs is the `KernelStore`
//! `Store` impl (`kernel/src/store.rs`); the kcore walker calls it through the
//! seam.
//!
//! Layout of one process's view:
//!   L1[0]   device GiB        — shared kernel entry, EL1-only
//!   L1[1]   kernel DRAM table — shared kernel entry (incl. the
//!           identity user window used by the idle thread)
//!   L1[2..] process-private   — user mappings (ELF base 0x8000_0000)
//!
//! The kernel is thus mapped in every aspace (exception vectors keep
//! working across TTBR0 switches); user mappings carry AP_EL0 and PXN,
//! kernel entries are EL1-only, so the split is enforced per entry.

pub use kcore::aspace::*;

use crate::store::KernelStore;
use core::ptr;

static mut NEXT_ASID: u16 = 1;

// ── the int→pointer boundary: the L1 table and the table
//    pool are 512-entry u64 tables laid out contiguously after the AspaceObj
//    header (`init`), disjoint from the header itself, so these slice views
//    never alias `*this`'s other fields. This is the one place a PA becomes a
//    pointer; the walker logic they feed is pure kcore. ──────────────────────
unsafe fn l1_view(this: *mut AspaceObj) -> &'static mut [u64; 512] {
    &mut *((*this).l1 as *mut [u64; 512])
}
unsafe fn pool_view(this: *mut AspaceObj) -> &'static mut [[u64; 512]] {
    core::slice::from_raw_parts_mut(
        (*this).pool_base as *mut [u64; 512],
        (*this).pool_pages as usize,
    )
}

/// pre:  `this` points at bytes_for(pool_pages) of 4 KiB-aligned writable
///       memory.
/// post: L1 holds the shared kernel entries; pool empty; fresh ASID.
pub unsafe fn init(this: *mut AspaceObj, pool_pages: u64) {
    let base = this as u64;
    let l1 = base + PAGE;
    ptr::write_bytes(l1 as *mut u8, 0, PAGE as usize);
    // Shared kernel entries from the boot identity map.
    let kernel_l1 = crate::mmu::kernel_l1();
    (l1 as *mut u64).write((kernel_l1 as *const u64).read());
    (l1 as *mut u64)
        .add(1)
        .write((kernel_l1 as *const u64).add(1).read());

    let asid = NEXT_ASID;
    NEXT_ASID = NEXT_ASID.wrapping_add(1);
    if NEXT_ASID == 0 {
        // 8-bit-safe wrap: flush everything once per 64k spawns.
        NEXT_ASID = 1;
        core::arch::asm!("tlbi vmalle1", "dsb sy", "isb");
    }

    this.write(AspaceObj {
        hdr: kcore::cspace::ObjHeader { refs: 1 },
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

/// Map `pages` frames starting at `pa` to `va` with EL0 permissions. Thin
/// shell over [`kcore::aspace::map_in`]: build the slice views, thread the
/// pool high-water mark in/out, let the verified walker do the work.
pub unsafe fn map(
    this: *mut AspaceObj,
    pa: u64,
    va: u64,
    pages: u64,
    perms: u64,
) -> Result<(), MapError> {
    let base = (*this).pool_base;
    let mut used = (*this).pool_used;
    let mut store = KernelStore;
    let r = {
        let l1 = l1_view(this);
        let pool = pool_view(this);
        kcore::aspace::map_in(l1, pool, &mut used, base, pa, va, pages, perms, &mut store)
    };
    (*this).pool_used = used;
    r
}

/// Grow the aspace's intermediate-page-table pool by `add` zeroed tables, carved
/// contiguously at the pool's current end (rev2§2.5 "accepts top-ups"). The
/// caller (the `Sys::AspaceTopUp` handler) places `add` fresh
/// tables physically abutting `pool_base + pool_pages*PAGE`; here we zero them and
/// bump the recorded `pool_pages`, after which `pool_view`/`map` rebuild the larger
/// slice automatically (no map-path change). This shell is plain Rust and *not*
/// verified; soundness — that the extension preserves `pt_wf` and every existing
/// mapping — is the machine-checked **justification** it relies on, the verified
/// [`kcore::aspace::lemma_grow_pool`] (a standalone widening theorem, applied here
/// by trust, not an exec postcondition of this function). Called by the
/// `Sys::AspaceTopUp` handler via [`crate::untyped::aspace_topup`].
pub unsafe fn grow_pool(this: *mut AspaceObj, add: u64) {
    let old_len = (*this).pool_pages;
    let region = (*this).pool_base + old_len * PAGE;
    ptr::write_bytes(region as *mut u8, 0, (add * PAGE) as usize);
    (*this).pool_pages = old_len + add;
}

/// Unmap (frame-cap deletion path). The verified walker clears the leaves and
/// drives the per-page TLBI + trailing barrier through `KernelStore`.
pub unsafe fn unmap(this: *mut AspaceObj, va: u64, pages: u64) {
    let base = (*this).pool_base;
    let asid = (*this).asid;
    let mut store = KernelStore;
    let l1 = l1_view(this);
    let pool = pool_view(this);
    kcore::aspace::unmap_in(l1, pool, base, asid, va, pages, &mut store);
}

/// Is [va, va+len) fully mapped (and writable, if asked)? Used by the syscall
/// layer to validate user pointers before the kernel dereferences them
/// through the process's own translation.
pub unsafe fn range_mapped(this: *mut AspaceObj, va: u64, len: u64, write: bool) -> bool {
    let base = (*this).pool_base;
    let l1: &[u64; 512] = &*((*this).l1 as *const [u64; 512]);
    let pool: &[[u64; 512]] = core::slice::from_raw_parts(
        (*this).pool_base as *const [u64; 512],
        (*this).pool_pages as usize,
    );
    kcore::aspace::range_mapped_in(l1, pool, base, va, len, write)
}

/// pre: refs == 0. The memory (tables included) returns to the donor
/// untyped via revoke; nothing to do but note that mapped frames keep
/// their own cap-side state and are unmapped when their caps die.
pub unsafe fn destroy_aspace(_a: *mut AspaceObj) {}
