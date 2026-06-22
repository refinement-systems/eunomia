//! Replay the committed fuzz corpora through their targets, re-checking the
//! invariants. Keeps fuzz inputs alive as ordinary tests and lets `cargo miri
//! test` UB-check them on each. Covers all targets — `elf_parse` (the parser),
//! `segment_layout` (the page-rounding math, the I-5 site), and `startup` (the
//! startup-block codec) — so the one documented Miri command (`--test
//! fuzz_corpus`) replays them all.

use std::fs;
use std::path::PathBuf;

use loader::{elf, startup};

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
            let end = seg
                .offset
                .checked_add(seg.filesz)
                .expect("file extent overflow");
            assert!(end <= data.len() as u64);
            assert!(seg.filesz <= seg.memsz);
            assert!(seg.vaddr.checked_add(seg.memsz).is_some());
            // Parse↔layout agreement: anything parse accepts, prepare lays out.
            let l = seg
                .page_layout()
                .expect("parse accepted an unlayout-able segment");
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
        let seg = elf::Segment {
            vaddr,
            offset: 0,
            filesz: 0,
            memsz,
            flags: 0,
        };
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

#[test]
fn startup() {
    for data in corpus_files("startup") {
        let Some(s) = startup::decode(&data) else {
            continue;
        };
        assert!(s.ngrants <= startup::MAX_GRANTS);
        assert!(s.nargv <= startup::MAX_ARGV);
        assert!(s.nenv <= startup::MAX_ENV);
        // Every borrowed byte-string lies inside the input slice.
        let range = data.as_ptr_range();
        for v in s.argv[..s.nargv].iter().chain(&s.env[..s.nenv]) {
            if !v.is_empty() {
                let r = v.as_ptr_range();
                assert!(r.start >= range.start && r.end <= range.end);
            }
        }
        // A decoded block that fits the budget re-encodes and round-trips.
        let mut buf = [0u8; startup::MAX_BLOCK];
        if let Ok(n) = startup::encode(&s, &mut buf) {
            assert_eq!(startup::decode(&buf[..n]), Some(s));
        }
    }
}
