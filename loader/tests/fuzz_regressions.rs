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
