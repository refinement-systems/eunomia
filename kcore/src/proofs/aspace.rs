//! Address-space / page-table-walker harnesses (plan §4.5), over the
//! slice-indexed walker the §2.4 rewrite moved into [`crate::aspace`]. The
//! pure-function harnesses (`check_pte_encode`, `check_va_bounds`) run fully
//! nondet; the walker harnesses build a fresh L1 + a tiny table pool on the
//! stack and pin VAs to a small window so the L1/L2/L3 indices stay concrete
//! (the §4.3 cost lesson — `[u64; 512]` tables are large, so symbolic indexing
//! is the cost driver). The VA-coverage obligation is carried by the pure
//! `check_va_bounds`, leaving the walker harnesses free to use concrete VAs.

#![cfg(kani)]

use super::ghost::{GhostEnv, GhostEvent};
use crate::aspace::{
    self, MapError, ADDR_MASK, AF, ATTR_DEVICE, ATTR_NORMAL, PAGE, PERM_DEVICE, PERM_W, PERM_X,
    PXN, UXN, USER_VA_BASE, USER_VA_END,
};

/// Pool tables for the walker harnesses: a mapping needs one L2 + one L3, so 3
/// gives headroom (and `check_pool_accounting` shrinks it to force exhaustion).
const POOL: usize = 3;
/// A page-aligned synthetic pool base PA (the descriptors store
/// `pool_base + idx*PAGE`, which `pool_index` inverts).
const POOL_BASE: u64 = 0x4000_0000;
/// A page-aligned synthetic frame PA to map.
const FRAME_PA: u64 = 0x5000_0000;
/// A synthetic ASID for the unmap TLBI witness.
const ASID: u16 = 7;

/// `check_pte_encode` (plan §4.5, finding AS-1): the leaf-descriptor encoding.
/// AF + PXN unconditional; valid page descriptor; address round-trips; W⇒RW
/// ¬W⇒RO; **device ⇒ never executable + SH_NONE + device attr** (the AS-1 fix
/// — the kernel walker honoured PERM_X for device pages); normal memory is
/// executable iff PERM_X.
#[kani::proof]
fn check_pte_encode() {
    let pa: u64 = kani::any();
    let perms: u64 = kani::any();
    let pte = aspace::pte_encode(pa, perms);

    assert!(pte & AF != 0); // access flag always set
    assert!(pte & PXN != 0); // user pages never EL1-executable
    assert!(pte & 0b11 == 0b11); // valid page descriptor
    assert!(aspace::pte_output_pa(pte) == pa & ADDR_MASK); // address bits round-trip

    let ap = (pte >> 6) & 0b11;
    if perms & PERM_W != 0 {
        assert!(ap == 0b01); // AP_EL0_RW
    } else {
        assert!(ap == 0b11); // AP_EL0_RO
    }

    if perms & PERM_DEVICE != 0 {
        assert!(pte & UXN != 0); // AS-1: device is never executable
        assert!((pte >> 8) & 0b11 == 0b00); // SH_NONE
        assert!((pte >> 2) & 0b111 == ATTR_DEVICE >> 2); // device attr index
    } else {
        if perms & PERM_X != 0 {
            assert!(pte & UXN == 0); // normal + X ⇒ executable
        } else {
            assert!(pte & UXN != 0);
        }
        assert!((pte >> 8) & 0b11 == 0b11); // SH_INNER
        assert!((pte >> 2) & 0b111 == ATTR_NORMAL >> 2); // normal attr index
    }
}

/// `check_va_bounds` (plan §4.5): `va_range_ok` is exactly the page-aligned,
/// in-`[USER_VA_BASE, USER_VA_END)` predicate, and **every** page of an
/// accepted range lands in L1 entries `[2, 511]` — never the two shared kernel
/// entries (L1[0] device, L1[1] kernel DRAM). Fully nondet.
#[kani::proof]
fn check_va_bounds() {
    let va: u64 = kani::any();
    let pages: u64 = kani::any();

    let ok = aspace::va_range_ok(va, pages);
    let expect = va % PAGE == 0
        && va >= USER_VA_BASE
        && va.saturating_add(pages.saturating_mul(PAGE)) <= USER_VA_END;
    assert!(ok == expect);

    if ok {
        // Any page in the accepted range (nondet i) is a private user entry.
        let i: u64 = kani::any();
        kani::assume(i < pages);
        let page = va + i * PAGE; // no overflow: va + pages*PAGE <= USER_VA_END
        let l1 = aspace::l1_index(page);
        assert!(l1 >= 2 && l1 < 512);
    }
}

/// `check_map_model` (plan §4.5): map adds exactly the requested page or fails
/// **atomically**. Nondet pool size exercises both the success and the
/// `NeedMemory` paths; the two-pass design means a failure writes no leaf (the
/// page stays unmapped), and a success that got past pass 1 cannot then run out
/// of pool in pass 2.
#[kani::proof]
#[kani::unwind(4)]
fn check_map_model() {
    let mut l1 = [0u64; 512];
    let mut pool = [[0u64; 512]; POOL];
    let mut used = 0u64;
    let mut env = GhostEnv::new();
    let perms: u64 = kani::any();
    // Nondet pool size in {1,2,3}: 1 is too small (L2+L3 needed) ⇒ NeedMemory.
    let np: usize = kani::any();
    kani::assume(np >= 1 && np <= POOL);
    let va = USER_VA_BASE;

    let r = unsafe {
        aspace::map_in(&mut l1, &mut pool[..np], &mut used, POOL_BASE, FRAME_PA, va, 1, perms, &mut env)
    };

    assert!(used as usize <= np); // pool high-water never exceeds capacity
    let mapped = aspace::range_mapped_in(&l1, &pool[..np], POOL_BASE, va, PAGE, false);
    // Guard the `np` assume (rec. #3): both the successful map and the
    // pool-exhaustion (`NeedMemory`) outcomes must be reachable.
    // (use `==`, not `matches!`: the latter lowers to a `match` whose dead arm
    // CBMC instruments as a spurious UNREACHABLE cover.)
    kani::cover!(r.is_ok());
    kani::cover!(r == Err(MapError::NeedMemory));
    match r {
        Ok(()) => assert!(mapped), // exactly the requested page is mapped
        Err(MapError::NeedMemory) => assert!(!mapped), // atomic: nothing installed
        Err(_) => assert!(false, "only NeedMemory is possible for a valid VA"),
    }
}

/// `check_map_no_silent_remap` (plan §4.5): mapping a VA that is already mapped
/// returns `AlreadyMapped` and leaves the existing leaf untouched (no silent
/// overwrite).
#[kani::proof]
#[kani::unwind(4)]
fn check_map_no_silent_remap() {
    let mut l1 = [0u64; 512];
    let mut pool = [[0u64; 512]; POOL];
    let mut used = 0u64;
    let mut env = GhostEnv::new();
    let va = USER_VA_BASE;

    unsafe {
        assert!(aspace::map_in(&mut l1, &mut pool, &mut used, POOL_BASE, FRAME_PA, va, 1, PERM_W, &mut env).is_ok());
    }
    // Record the installed leaf.
    let (l3, e) = aspace::lookup(&l1, &pool, POOL_BASE, va).unwrap();
    let leaf = pool[l3][e];
    assert!(leaf != 0);

    // Re-map the same page with different perms ⇒ AlreadyMapped, leaf unchanged.
    let r = unsafe {
        aspace::map_in(&mut l1, &mut pool, &mut used, POOL_BASE, 0x6000_0000, va, 1, 0, &mut env)
    };
    assert!(r == Err(MapError::AlreadyMapped));
    assert!(pool[l3][e] == leaf);
}

/// `check_unmap_exact` (plan §4.5): unmap clears exactly the mapped pages, and
/// the ghost `Env` records one TLB invalidation per cleared page plus the
/// single trailing unmap barrier (§2.2 rule 3).
#[kani::proof]
#[kani::unwind(4)]
fn check_unmap_exact() {
    let mut l1 = [0u64; 512];
    let mut pool = [[0u64; 512]; POOL];
    let mut used = 0u64;
    let mut env = GhostEnv::new();
    let va = USER_VA_BASE; // two adjacent pages share one L3 table

    unsafe {
        assert!(aspace::map_in(&mut l1, &mut pool, &mut used, POOL_BASE, FRAME_PA, va, 2, PERM_W, &mut env).is_ok());
        assert!(aspace::range_mapped_in(&l1, &pool, POOL_BASE, va, 2 * PAGE, false));

        aspace::unmap_in(&l1, &mut pool, POOL_BASE, ASID, va, 2, &mut env);
    }

    // Both pages cleared; nothing left mapped.
    assert!(!aspace::range_mapped_in(&l1, &pool, POOL_BASE, va, PAGE, false));
    assert!(!aspace::range_mapped_in(&l1, &pool, POOL_BASE, va + PAGE, PAGE, false));
    // Exactly one TLBI per cleared page + one unmap barrier.
    assert!(env.count(GhostEvent::TlbInvalidate(ASID, va)) == 1);
    assert!(env.count(GhostEvent::TlbInvalidate(ASID, va + PAGE)) == 1);
    assert!(env.count(GhostEvent::BarrierUnmap) == 1);
}

/// `check_range_mapped` (plan §4.5): `range_mapped_in` agrees with a ghost
/// "mapped set" model — including the `len == 0` and `va + len` overflow edges
/// — for a single mapped page and nondet queries in a small window. This is
/// the predicate the syscall layer trusts before dereferencing user pointers.
#[kani::proof]
#[kani::unwind(6)]
fn check_range_mapped() {
    let mut l1 = [0u64; 512];
    let mut pool = [[0u64; 512]; POOL];
    let mut used = 0u64;
    let mut env = GhostEnv::new();
    let base = USER_VA_BASE;
    let writable: bool = kani::any();
    let perms = if writable { PERM_W } else { 0 };
    unsafe {
        assert!(aspace::map_in(&mut l1, &mut pool, &mut used, POOL_BASE, FRAME_PA, base, 1, perms, &mut env).is_ok());
    }

    // Query [va, va+len): va in {base-PAGE, base, base+PAGE}; len in {0..3}*PAGE.
    let q: u64 = kani::any();
    kani::assume(q < 3);
    let va = base - PAGE + q * PAGE;
    let np: u64 = kani::any();
    kani::assume(np <= 3);
    let len = np * PAGE;
    let want_write: bool = kani::any();

    let got = aspace::range_mapped_in(&l1, &pool, POOL_BASE, va, len, want_write);

    // Ghost model: the mapped set is exactly [base, base+PAGE), writable iff
    // `writable`. range_mapped is true iff every queried page is in the set
    // (and writable, if asked); len==0 only checks the user-range bound.
    let expect = if len == 0 {
        va >= USER_VA_BASE && va < USER_VA_END
    } else {
        let end = va + len; // bounded: va,len small, no overflow
        let in_set = va >= base && end <= base + PAGE;
        in_set && (!want_write || writable)
    };
    assert!(got == expect);

    // Guard the query-window `assume`s (rec. #3): the in-set (mapped),
    // out-of-set (unmapped), zero-length, and write-granted cases must all be
    // reachable — else `got == expect` could hold vacuously on one side.
    kani::cover!(got);
    kani::cover!(!got);
    kani::cover!(len == 0);
    kani::cover!(want_write && got);
}

/// `check_pool_accounting` (plan §4.5): `pool_used` never exceeds the pool
/// size; `NeedMemory` fires exactly at exhaustion; and a freshly allocated
/// table is zeroed (no stale entries leak from prior pool contents).
#[kani::proof]
#[kani::unwind(4)]
fn check_pool_accounting() {
    // Zeroing: seed the would-be L3 table with a stale sentinel; after map it
    // must be cleared everywhere except the entry the leaf occupies.
    let mut l1 = [0u64; 512];
    let mut pool = [[0u64; 512]; POOL];
    pool[1][7] = 0xDEAD_BEEF; // L3 lands at pool index 1 (L2 takes index 0)
    let mut used = 0u64;
    let mut env = GhostEnv::new();
    let va = USER_VA_BASE; // l3_index == 0, so [7] is a non-leaf slot

    unsafe {
        assert!(aspace::map_in(&mut l1, &mut pool, &mut used, POOL_BASE, FRAME_PA, va, 1, PERM_W, &mut env).is_ok());
    }
    assert!(used as usize <= POOL);
    assert!(used == 2); // one L2 + one L3
    assert!(pool[1][7] == 0); // alloc_table zeroed the stale entry

    // Exhaustion: a pool of one table cannot hold both the L2 and the L3.
    let mut l1b = [0u64; 512];
    let mut poolb = [[0u64; 512]; 1];
    let mut usedb = 0u64;
    let r = unsafe {
        aspace::map_in(&mut l1b, &mut poolb, &mut usedb, POOL_BASE, FRAME_PA, va, 1, PERM_W, &mut env)
    };
    assert!(r == Err(MapError::NeedMemory));
    assert!(usedb as usize <= 1); // never overshoots capacity
}
