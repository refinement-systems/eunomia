// SPDX-License-Identifier: 0BSD
//! Regression tests for cases surfaced by loader/fuzz. Each pins the
//! hardened behavior so the case cannot silently regress.

use loader::{elf, startup};

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

/// A segment whose page-rounded end would exceed `u64::MAX` must be refused,
/// never aborted or wrapped. `vaddr + memsz` within `PAGE-1` of `u64::MAX`
/// overflows the `+ (PAGE-1)` page-up rounding; unchecked, that math would
/// abort (overflow-checks on, dev) or wrap to a bogus page count (release).
/// The arithmetic is checked (`Segment::page_layout`), and `parse` runs the
/// same predicate so the producer refuses what `prepare` cannot lay out.
/// Pinned two ways: the layout math directly, and a full ELF carrying the
/// witness segment.
#[test]
fn elf2_page_rounding_overflow_refused() {
    // (a) The arithmetic, on the witness vaddr + memsz == u64::MAX, so the
    // `+ (PAGE-1)` round-up overflows. Clean Err, no panic.
    let seg = elf::Segment {
        vaddr: u64::MAX - 8,
        offset: 0,
        filesz: 8,
        memsz: 8,
        flags: 0,
    };
    assert_eq!(seg.page_layout(), Err(elf::ElfError::BadSegment));

    // (b) A full ELF whose single PT_LOAD carries that vaddr/memsz: `parse`
    // refuses it at the boundary, before `prepare` can abort/wrap. filesz=8 at
    // offset 0x78 keeps the file extent in-bounds and filesz<=memsz, isolating
    // the rounding overflow.
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
    e[0x50..0x58].copy_from_slice(&(u64::MAX - 8).to_le_bytes()); // vaddr (overflow witness)
    e[0x60..0x68].copy_from_slice(&8u64.to_le_bytes()); // filesz
    e[0x68..0x70].copy_from_slice(&8u64.to_le_bytes()); // memsz
    assert!(matches!(elf::parse(&e), Err(elf::ElfError::BadSegment)));
}

/// startup-1: a startup block whose declared counts/lengths far exceed the
/// bytes that follow must `decode` to `None`, never panic, over-read, or drive
/// an unbounded allocation (rev2§2.7 — the block is untrusted-shaped input read
/// in `_start`). The analog of `elf1`: a count from untrusted input validated
/// against ground truth before use. Pinned three ways — an over-cap grant
/// count, a within-cap grant count with no bodies, and an argv length past the
/// buffer.
#[test]
fn startup1_oversized_counts_refused() {
    // Header declaring 255 grants, with no grant bodies following. The count is
    // both over the arena cap and unbacked; decode refuses without reading a
    // single body.
    let mut a = startup::MAGIC.to_vec();
    a.extend_from_slice(&[255, 0, 0]); // ngrants=255, nargv=0, nenv=0
    assert_eq!(startup::decode(&a), None);

    // A within-cap grant count (1) but the single grant's body is absent: the
    // length-from-input is validated against the remaining slice before the
    // read, so this refuses rather than over-reading.
    let mut b = startup::MAGIC.to_vec();
    b.extend_from_slice(&[1, 0, 0]);
    b.push(startup::NAME_TIME);
    b.push(startup::KIND_REGION); // promises 24 body bytes…
                                  // …none follow.
    assert_eq!(startup::decode(&b), None);

    // One argv string declaring u16::MAX bytes, only a few present: the length
    // is bounds-checked against the remaining slice, so no unbounded read/alloc.
    let mut c = startup::MAGIC.to_vec();
    c.extend_from_slice(&[0, 1, 0]); // one argv
    c.extend_from_slice(&u16::MAX.to_le_bytes());
    c.extend_from_slice(b"short");
    assert_eq!(startup::decode(&c), None);
}

/// startup-2: the `KIND_SEED` inline-bytes grant. A declared seed
/// grant whose 32-byte body is cut short must `decode` to `None` (the
/// `KIND_REGION` truncation discipline extended to the new kind); the well-formed
/// counterpart must decode with the four words intact. The pair pins both the
/// refusal and the acceptance of the new arm.
#[test]
fn startup2_truncated_seed_refused() {
    // A SEED grant declared, but only 20 of its 32 body bytes present.
    let mut t = startup::MAGIC.to_vec();
    t.extend_from_slice(&[1, 0, 0]); // one grant, no argv/env
    t.push(startup::NAME_RANDOM_SEED);
    t.push(startup::KIND_SEED); // promises 32 body bytes…
                                // …only 20 follow.
    t.extend_from_slice(&[0u8; 20]);
    assert_eq!(startup::decode(&t), None);

    // The well-formed counterpart decodes with the seed words intact.
    let mut ok = startup::MAGIC.to_vec();
    ok.extend_from_slice(&[1, 0, 0]);
    ok.push(startup::NAME_RANDOM_SEED);
    ok.push(startup::KIND_SEED);
    for w in [1u64, 2, 3, 4] {
        ok.extend_from_slice(&w.to_le_bytes());
    }
    let s = startup::decode(&ok).expect("well-formed seed block decodes");
    assert_eq!(
        s.grant(startup::NAME_RANDOM_SEED),
        Some(startup::GrantKind::Seed([1, 2, 3, 4]))
    );
}
