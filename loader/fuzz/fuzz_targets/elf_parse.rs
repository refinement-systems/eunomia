#![no_main]
//! ELF64 parse on arbitrary bytes (§5). Program images are data in the
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

    assert!(img.nsegments <= elf::MAX_SEGMENTS, "segment count over the cap");
    assert!(core::ptr::eq(img.bytes, data), "image lost its backing slice");

    for seg in &img.segments[..img.nsegments] {
        // The on-disk extent [offset, offset+filesz) must lie inside the
        // input, computed without wrapping (parse promises this; re-verify).
        let end = seg
            .offset
            .checked_add(seg.filesz)
            .expect("segment file extent overflowed u64");
        assert!(end <= data.len() as u64, "segment file extent past end of input");
        assert!(seg.filesz <= seg.memsz, "filesz exceeds memsz");
        // Loading maps memsz bytes at vaddr; that range must not wrap.
        assert!(seg.vaddr.checked_add(seg.memsz).is_some(), "segment vaddr range wraps");
        assert!(seg.memsz > 0, "zero-size segment retained");
    }
});
