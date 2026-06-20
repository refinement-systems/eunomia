//! Property tests for `Segment::page_layout` — the page-rounding arithmetic
//! B3A extracted out of the target-only `spawn::prepare` (the I-5 site). rev1§6
//! routes loader's layout math to the host "Miri + proptest" baseline; this is
//! that always-on gate (cargo-fuzz, in loader/fuzz, adds adversarial depth but
//! is not part of `cargo test`). The invariant set below is the same oracle the
//! `segment_layout` / `elf_parse` fuzz targets assert.

use loader::elf::{ElfError, Segment, PAGE};
use proptest::prelude::*;

fn seg(vaddr: u64, memsz: u64) -> Segment {
    Segment {
        vaddr,
        offset: 0,
        filesz: 0,
        memsz,
        flags: 0,
    }
}

/// `u64` strategy that samples the full range but biases toward the I-5
/// overflow boundary (near `u64::MAX`) and page-edge values, so the boundary is
/// hit by construction rather than by luck.
fn word() -> impl Strategy<Value = u64> {
    prop_oneof![
        4 => any::<u64>(),
        1 => (0u64..=PAGE).prop_map(|k| u64::MAX - k),                  // near u64::MAX
        1 => any::<u64>().prop_map(|v| v & !(PAGE - 1)),                 // page-aligned
        1 => any::<u64>().prop_map(|v| (v & !(PAGE - 1)).wrapping_add(1)), // aligned + 1
        1 => any::<u64>().prop_map(|v| (v & !(PAGE - 1)).wrapping_sub(1)), // aligned - 1
    ]
}

proptest! {
    // Miri: a handful of cases cover the same arithmetic; native keeps the full
    // sweep (mirrors cas/src/file.rs, storage-server rights_lattice).
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// `page_layout` is total and its geometry internally consistent on every
    /// `(vaddr, memsz)`; the only legal error is `BadSegment`, and it occurs
    /// exactly at the page-rounding overflow boundary (rev1§5/§3.7: untrusted
    /// images must refuse-not-crash; the boundary is the I-5 site B3A fixed).
    #[test]
    fn page_layout_invariants(vaddr in word(), memsz in word()) {
        // Independent oracle for the refusal condition, written with checked_*
        // so it is itself total: the page-up rounding `vaddr + memsz + (PAGE-1)`
        // overflows u64. This is the *exact* condition `page_layout` must refuse
        // on — reverting its `checked_add` to a plain `+` would make the Ok
        // branch wrap and fail the geometry asserts below.
        let overflows = vaddr
            .checked_add(memsz)
            .and_then(|e| e.checked_add(PAGE - 1))
            .is_none();
        match seg(vaddr, memsz).page_layout() {
            Ok(l) => {
                prop_assert!(!overflows, "Ok on an input the oracle says overflows");
                // Round down: start is page-aligned and at-or-below vaddr.
                prop_assert!(l.va_start <= vaddr);
                prop_assert_eq!(l.va_start % PAGE, 0);
                // Round up: end is page-aligned and never underflowed below start.
                prop_assert_eq!(l.va_end % PAGE, 0);
                prop_assert!(l.va_end >= l.va_start);
                // The end never sits below vaddr; a *non-empty* segment ends
                // strictly past it. (Only one-directional: an empty segment at
                // an unaligned vaddr still rounds up to the next page, so
                // `vaddr < va_end` can hold with `memsz == 0`.)
                prop_assert!(vaddr <= l.va_end);
                if memsz > 0 {
                    prop_assert!(vaddr < l.va_end);
                }
                // In-page write offset of vaddr.
                prop_assert_eq!(l.page_offset, vaddr - l.va_start);
                prop_assert!(l.page_offset < PAGE);
                // Page count is exact and did not overflow.
                prop_assert_eq!(l.pages.checked_mul(PAGE), Some(l.va_end - l.va_start));
                if memsz > 0 {
                    prop_assert!(l.pages >= 1);
                }
            }
            Err(e) => {
                prop_assert_eq!(e, ElfError::BadSegment);
                prop_assert!(overflows, "Err on an input the oracle says is layout-able");
            }
        }
    }
}

/// Oracle sanity (negative control): prove the property above guards a *real*
/// wrap, not a tautology. On the audit's I-5 witness the pre-B3A unchecked
/// formula `vaddr + memsz + (PAGE-1)` overflows u64 and, rounded down, lands
/// *below* va_start — the bogus page count the fix refuses. (Same negative-
/// control posture as storage-server rights_lattice's independent `&` oracle.)
#[test]
fn old_unchecked_formula_would_wrap_on_i5_witness() {
    let (vaddr, memsz) = (u64::MAX - 8, 8u64);
    let va_start = vaddr & !(PAGE - 1);
    // The old round-up, reproduced with wrapping to model what an unchecked `+`
    // does in release (and what overflow-checks would abort on in dev).
    let old_va_end = vaddr.wrapping_add(memsz).wrapping_add(PAGE - 1) & !(PAGE - 1);
    assert!(
        old_va_end < va_start,
        "expected the unchecked round-up to wrap below va_start"
    );
    // The checked path refuses the same input cleanly, with no panic.
    assert_eq!(seg(vaddr, memsz).page_layout(), Err(ElfError::BadSegment));
}
