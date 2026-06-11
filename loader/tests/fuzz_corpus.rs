//! Replay the committed elf_parse corpus through the ELF parser, re-checking
//! the segment-bounds invariants. Keeps fuzz inputs alive as ordinary tests
//! and lets `cargo miri test` UB-check the parser on each.

use std::fs;
use std::path::PathBuf;

use loader::elf;

fn corpus_files() -> Vec<Vec<u8>> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus/elf_parse");
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
    for data in corpus_files() {
        let Ok(img) = elf::parse(&data) else { continue };
        assert!(img.nsegments <= elf::MAX_SEGMENTS);
        for seg in &img.segments[..img.nsegments] {
            let end = seg.offset.checked_add(seg.filesz).expect("file extent overflow");
            assert!(end <= data.len() as u64);
            assert!(seg.filesz <= seg.memsz);
            assert!(seg.vaddr.checked_add(seg.memsz).is_some());
        }
    }
}
