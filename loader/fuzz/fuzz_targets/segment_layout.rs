#![no_main]
//! Page-layout arithmetic on arbitrary `(vaddr, memsz)` (rev1§5, the I-5 site).
//! `spawn::prepare` page-rounds each segment of an untrusted image; B3A pulled
//! that math into `Segment::page_layout` so it is host-fuzzable. This target
//! draws the two words directly from the input (first 16 bytes LE, short inputs
//! zero-padded) so the fuzzer reaches the `vaddr + memsz` within `PAGE-1` of
//! `u64::MAX` overflow edge in two drawn words, instead of having to build a
//! near-`u64::MAX` vaddr through a whole well-formed ELF. The fuzz profile
//! forces overflow-checks + debug-assertions, so any unchecked wrap that slips
//! back in aborts the run — the differential that would have caught I-5.
//!
//! Run: `cargo +nightly fuzz run segment_layout`.
use libfuzzer_sys::fuzz_target;

use loader::elf;

/// Decode 8 LE bytes at `off`, zero-padding when the input is short.
fn word(data: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    if off < data.len() {
        let n = (data.len() - off).min(8);
        buf[..n].copy_from_slice(&data[off..off + n]);
    }
    u64::from_le_bytes(buf)
}

fuzz_target!(|data: &[u8]| {
    let (vaddr, memsz) = (word(data, 0), word(data, 8));

    // Independent oracle: the page-up rounding overflows u64. `page_layout`
    // must refuse exactly these inputs (BadSegment) and lay out all others.
    let overflows = vaddr
        .checked_add(memsz)
        .and_then(|e| e.checked_add(elf::PAGE - 1))
        .is_none();

    let seg = elf::Segment { vaddr, offset: 0, filesz: 0, memsz, flags: 0 };
    match seg.page_layout() {
        Ok(l) => {
            assert!(!overflows, "Ok on an input the oracle says overflows");
            assert!(l.va_start <= vaddr, "va_start above vaddr");
            assert_eq!(l.va_start % elf::PAGE, 0, "va_start not page-aligned");
            assert_eq!(l.va_end % elf::PAGE, 0, "va_end not page-aligned");
            assert!(l.va_end >= l.va_start, "va_end underflowed below va_start");
            // The end never sits below vaddr; a *non-empty* segment ends
            // strictly past it. Only one-directional: an empty segment at an
            // unaligned vaddr still rounds up, so `vaddr < va_end` can hold
            // with `memsz == 0`.
            assert!(vaddr <= l.va_end, "va_end below vaddr");
            if memsz > 0 {
                assert!(vaddr < l.va_end, "non-empty segment does not end past vaddr");
            }
            assert_eq!(l.page_offset, vaddr - l.va_start, "wrong page offset");
            assert!(l.page_offset < elf::PAGE, "page offset not in-page");
            assert_eq!(
                l.pages.checked_mul(elf::PAGE),
                Some(l.va_end - l.va_start),
                "page count not exact",
            );
            if memsz > 0 {
                assert!(l.pages >= 1, "non-empty segment spans zero pages");
            }
        }
        Err(e) => {
            assert_eq!(e, elf::ElfError::BadSegment, "unexpected error variant");
            assert!(overflows, "Err on an input the oracle says is layout-able");
        }
    }
});
