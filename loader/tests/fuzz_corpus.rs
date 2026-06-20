//! Replay the committed fuzz corpora through their targets, re-checking the
//! invariants. Keeps fuzz inputs alive as ordinary tests and lets `cargo miri
//! test` UB-check them on each. Covers both targets — `elf_parse` (the parser)
//! and `segment_layout` (the page-rounding math, the I-5 site) — so the one
//! documented Miri command (`--test fuzz_corpus`) replays both.

use std::fs;
use std::path::PathBuf;

use loader::elf;

fn corpus_files(target: &str) -> Vec<Vec<u8>> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus");
    dir.push(target);
    match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| fs::read(e.path()).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn elf_parse() {
    for data in corpus_files("elf_parse") {
        let Ok(img) = elf::parse(&data) else { continue };
        assert!(img.nsegments <= elf::MAX_SEGMENTS);
        for seg in &img.segments[..img.nsegments] {
            let end = seg.offset.checked_add(seg.filesz).expect("file extent overflow");
            assert!(end <= data.len() as u64);
            assert!(seg.filesz <= seg.memsz);
            assert!(seg.vaddr.checked_add(seg.memsz).is_some());
            // Parse↔layout agreement: anything parse accepts, prepare lays out.
            let l = seg.page_layout().expect("parse accepted an unlayout-able segment");
            assert_eq!(l.pages.checked_mul(elf::PAGE), Some(l.va_end - l.va_start));
        }
    }
}

/// Decode 8 LE bytes at `off`, zero-padding when the input is short (the
/// `segment_layout` target's leading-16-bytes convention).
fn word(data: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    if off < data.len() {
        let n = (data.len() - off).min(8);
        buf[..n].copy_from_slice(&data[off..off + n]);
    }
    u64::from_le_bytes(buf)
}

#[test]
fn segment_layout() {
    for data in corpus_files("segment_layout") {
        let (vaddr, memsz) = (word(&data, 0), word(&data, 8));
        let overflows = vaddr
            .checked_add(memsz)
            .and_then(|e| e.checked_add(elf::PAGE - 1))
            .is_none();
        let seg = elf::Segment { vaddr, offset: 0, filesz: 0, memsz, flags: 0 };
        match seg.page_layout() {
            Ok(l) => {
                assert!(!overflows);
                assert!(l.va_start <= vaddr && l.va_start % elf::PAGE == 0);
                assert!(l.va_end % elf::PAGE == 0 && l.va_end >= l.va_start);
                assert!(vaddr <= l.va_end);
                if memsz > 0 {
                    assert!(vaddr < l.va_end);
                }
                assert_eq!(l.page_offset, vaddr - l.va_start);
                assert!(l.page_offset < elf::PAGE);
                assert_eq!(l.pages.checked_mul(elf::PAGE), Some(l.va_end - l.va_start));
                if memsz > 0 {
                    assert!(l.pages >= 1);
                }
            }
            Err(e) => {
                assert_eq!(e, elf::ElfError::BadSegment);
                assert!(overflows);
            }
        }
    }
}
