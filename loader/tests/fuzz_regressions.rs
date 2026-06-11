//! Regression reproducers for findings surfaced by loader/fuzz.
//!
//! These document *currently unfixed* findings (fixing is out of scope for
//! the fuzzing work). Each is written `#[should_panic]` so it passes today
//! by asserting the bug still bites, and fails — loudly, demanding an
//! update — the moment the parser is hardened. See doc/results/1_fuzzing-findings.md.

use loader::elf;

/// FINDING ELF-1 (unfixed): `e_phoff` near u64::MAX makes `parse` panic with
/// an arithmetic overflow in `u32le` (`off + 4`) / `phoff + i*phentsize`,
/// despite elf.rs documenting "no panics". Found by the `elf_parse` target.
/// When fixed, `parse` should return `Err(ElfError::Truncated)`; flip this
/// test to assert that.
#[test]
#[should_panic(expected = "overflow")]
fn elf1_phoff_overflow_panics() {
    let mut e = vec![0u8; 64];
    e[0..4].copy_from_slice(b"\x7FELF");
    e[4] = 2; // 64-bit
    e[5] = 1; // little-endian
    e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    e[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // e_phoff
    e[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
    e[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
    let _ = elf::parse(&e);
}
