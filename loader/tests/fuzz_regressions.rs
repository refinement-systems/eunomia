//! Regression tests for cases surfaced by loader/fuzz. Each pins the
//! hardened behavior so the case cannot silently regress.

use loader::elf;

/// An `e_phoff` near u64::MAX must not make `parse` panic with an arithmetic
/// overflow in `u32le` (`off + 4`) / `phoff + i*phentsize`: elf.rs promises
/// "no panics". The offset math is checked and overflow is reported as
/// `Truncated`.
#[test]
fn elf1_phoff_overflow_rejected() {
    let mut e = vec![0u8; 64];
    e[0..4].copy_from_slice(b"\x7FELF");
    e[4] = 2; // 64-bit
    e[5] = 1; // little-endian
    e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    e[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff
    e[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
    e[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
    assert!(matches!(elf::parse(&e), Err(elf::ElfError::Truncated)));
}

/// I-5 (audit §2.1): a segment whose page-rounded end would exceed `u64::MAX`
/// must be refused, never aborted or wrapped. `vaddr + memsz` within `PAGE-1`
/// of `u64::MAX` overflows the `+ (PAGE-1)` page-up rounding; the old unchecked
/// math aborted (overflow-checks on, dev) or wrapped to a bogus page count
/// (release). B3A made that arithmetic checked (`Segment::page_layout`), and
/// `parse` runs the same predicate so the producer refuses what `prepare`
/// cannot lay out. Pinned two ways: the layout math directly, and a full ELF
/// carrying the witness segment.
#[test]
fn elf2_page_rounding_overflow_refused() {
    // (a) The arithmetic, on the audit's witness: vaddr + memsz == u64::MAX, so
    // the `+ (PAGE-1)` round-up overflows. Clean Err, no panic.
    let seg = elf::Segment {
        vaddr: u64::MAX - 8,
        offset: 0,
        filesz: 8,
        memsz: 8,
        flags: 0,
    };
    assert_eq!(seg.page_layout(), Err(elf::ElfError::BadSegment));

    // (b) A full ELF whose single PT_LOAD carries that vaddr/memsz: `parse`
    // refuses it at the boundary (it used to pass, letting `prepare` abort/wrap).
    // filesz=8 at offset 0x78 keeps the file extent in-bounds and filesz<=memsz,
    // isolating the rounding overflow.
    let mut e = vec![0u8; 0x78 + 8];
    e[0..4].copy_from_slice(b"\x7FELF");
    e[4] = 2; // 64-bit
    e[5] = 1; // little-endian
    e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    e[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e[24..32].copy_from_slice(&0x8000_0000u64.to_le_bytes()); // entry
    e[32..40].copy_from_slice(&0x40u64.to_le_bytes()); // phoff
    e[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
    e[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
    e[0x40..0x44].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    e[0x44..0x48].copy_from_slice(&(elf::PF_R | elf::PF_X).to_le_bytes());
    e[0x48..0x50].copy_from_slice(&0x78u64.to_le_bytes()); // offset
    e[0x50..0x58].copy_from_slice(&(u64::MAX - 8).to_le_bytes()); // vaddr (I-5 witness)
    e[0x60..0x68].copy_from_slice(&8u64.to_le_bytes()); // filesz
    e[0x68..0x70].copy_from_slice(&8u64.to_le_bytes()); // memsz
    assert!(matches!(elf::parse(&e), Err(elf::ElfError::BadSegment)));
}
