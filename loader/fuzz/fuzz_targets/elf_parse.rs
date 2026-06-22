#![no_main]
//! ELF64 parse on arbitrary bytes (rev1§5). Program images are data in the
//! versioned store, so any holder of write access to a path feeds bytes to
//! this parser — it is untrusted input. Property set: never panic; every
//! segment range the parser reports is bounds-checked against the input
//! slice; nothing the parser hands back can index out of `bytes`; the
//! segment count is bounded. The parser uses a fixed-size segment array,
//! so there is no length-field-driven allocation to bound — this re-checks
//! the invariants `parse` claims, defending against a future refactor that
//! forgets one.
use libfuzzer_sys::fuzz_target;

use loader::elf;

fuzz_target!(|data: &[u8]| {
    let Ok(img) = elf::parse(data) else { return };

    assert!(
        img.nsegments <= elf::MAX_SEGMENTS,
        "segment count over the cap"
    );
    assert!(
        core::ptr::eq(img.bytes, data),
        "image lost its backing slice"
    );

    for seg in &img.segments[..img.nsegments] {
        // The on-disk extent [offset, offset+filesz) must lie inside the
        // input, computed without wrapping (parse promises this; re-verify).
        let end = seg
            .offset
            .checked_add(seg.filesz)
            .expect("segment file extent overflowed u64");
        assert!(
            end <= data.len() as u64,
            "segment file extent past end of input"
        );
        assert!(seg.filesz <= seg.memsz, "filesz exceeds memsz");
        // Loading maps memsz bytes at vaddr; that range must not wrap.
        assert!(
            seg.vaddr.checked_add(seg.memsz).is_some(),
            "segment vaddr range wraps"
        );
        assert!(seg.memsz > 0, "zero-size segment retained");

        // Parse↔layout agreement (the B3A producer/consumer tightening): every
        // segment `parse` accepts, `prepare` must be able to page-lay-out. A
        // future loosening of `parse` that re-permits an unlayout-able segment
        // fails here. (`memsz > 0` always — `parse` drops zero-size segments,
        // so the round-up end is strictly past vaddr and pages >= 1.)
        let l = seg
            .page_layout()
            .expect("parse accepted a segment prepare cannot lay out");
        assert!(
            l.va_start <= seg.vaddr && l.va_start % elf::PAGE == 0,
            "bad va_start"
        );
        assert!(
            l.va_end % elf::PAGE == 0 && l.va_end > seg.vaddr,
            "bad va_end"
        );
        assert_eq!(l.page_offset, seg.vaddr - l.va_start, "wrong page offset");
        assert!(l.page_offset < elf::PAGE, "page offset not in-page");
        assert_eq!(
            l.pages.checked_mul(elf::PAGE),
            Some(l.va_end - l.va_start),
            "page count not exact",
        );
        assert!(l.pages >= 1, "non-empty segment spans zero pages");
    }
});
