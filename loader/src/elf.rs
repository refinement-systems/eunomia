//! Minimal ELF64 little-endian parser for aarch64 executables.
//! Strict (untrusted input — images come from the versioned store):
//! bounds-checked, no panics.

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

/// Page size the loader maps segments at (rev2§5). Canonical home: `spawn`
/// re-exports it (`pub use crate::elf::PAGE`) so the page-layout predicate
/// and its sole consumer agree by construction.
pub const PAGE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
}

/// Page geometry a segment maps into: the page-aligned VA span, the page
/// count, and the in-page offset of `vaddr` where the file bytes are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLayout {
    pub va_start: u64,
    pub va_end: u64,
    pub pages: u64,
    pub page_offset: u64,
}

impl Segment {
    /// Page geometry the loader maps this segment into (rev2§5). All
    /// arithmetic is checked: a segment whose page-rounded end would exceed
    /// `u64::MAX` is refused (`BadSegment`), never wrapped or aborted —
    /// `spawn::prepare` runs this on untrusted images (rev2§3.7) and must
    /// refuse-not-crash (rev2§5.3). `parse` runs the same check so the
    /// producer never hands `prepare` a segment it cannot lay out.
    ///
    /// Total for *all* `(vaddr, memsz)` including `memsz == 0` (yields
    /// `pages == 0` for a page-aligned `vaddr`, else `1` from the round-up — no
    /// panic either way); `parse` drops `memsz == 0` segments, so `prepare`
    /// only ever sees `memsz > 0` (⇒ `pages >= 1`).
    pub fn page_layout(&self) -> Result<PageLayout, ElfError> {
        let va_start = self.vaddr & !(PAGE - 1); // round down: cannot overflow
        let va_end = self
            .vaddr
            .checked_add(self.memsz) // catches the vaddr+memsz wrap
            .and_then(|e| e.checked_add(PAGE - 1)) // the page-rounding overflow point
            .map(|e| e & !(PAGE - 1)) // round up to page boundary
            .ok_or(ElfError::BadSegment)?;
        // va_end >= va_start (round-up of vaddr+memsz vs round-down of vaddr),
        // so the subtraction cannot underflow once the checked add succeeds;
        // checked_sub is belt-and-suspenders, keeping the fn total under any
        // future caller.
        let span = va_end.checked_sub(va_start).ok_or(ElfError::BadSegment)?;
        Ok(PageLayout {
            va_start,
            va_end,
            pages: span / PAGE,
            page_offset: self.vaddr - va_start, // in [0, PAGE): cannot underflow
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    Truncated,
    BadMagic,
    NotElf64Le,
    NotAarch64,
    NotExecutable,
    TooManySegments,
    BadSegment,
}

pub const MAX_SEGMENTS: usize = 8;

#[derive(Debug)]
pub struct Image<'a> {
    pub entry: u64,
    pub segments: [Segment; MAX_SEGMENTS],
    pub nsegments: usize,
    pub bytes: &'a [u8],
}

// `off` comes from untrusted header fields: the end offset needs checked
// math, not just the slice bounds check.
fn u16le(b: &[u8], off: usize) -> Result<u16, ElfError> {
    off.checked_add(2)
        .and_then(|end| b.get(off..end))
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or(ElfError::Truncated)
}

fn u32le(b: &[u8], off: usize) -> Result<u32, ElfError> {
    off.checked_add(4)
        .and_then(|end| b.get(off..end))
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(ElfError::Truncated)
}

fn u64le(b: &[u8], off: usize) -> Result<u64, ElfError> {
    off.checked_add(8)
        .and_then(|end| b.get(off..end))
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or(ElfError::Truncated)
}

pub fn parse(bytes: &[u8]) -> Result<Image<'_>, ElfError> {
    if bytes.len() < 64 {
        return Err(ElfError::Truncated);
    }
    if &bytes[0..4] != b"\x7FELF" {
        return Err(ElfError::BadMagic);
    }
    // EI_CLASS = 2 (64-bit), EI_DATA = 1 (LE)
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err(ElfError::NotElf64Le);
    }
    let e_type = u16le(bytes, 16)?;
    if e_type != 2 {
        // ET_EXEC: userspace is statically linked at fixed VAs (rev2§5).
        return Err(ElfError::NotExecutable);
    }
    if u16le(bytes, 18)? != 183 {
        // EM_AARCH64
        return Err(ElfError::NotAarch64);
    }
    let entry = u64le(bytes, 24)?;
    let phoff = u64le(bytes, 32)? as usize;
    let phentsize = u16le(bytes, 54)? as usize;
    let phnum = u16le(bytes, 56)? as usize;
    if phentsize < 56 {
        return Err(ElfError::BadSegment);
    }

    let mut segments = [Segment {
        vaddr: 0,
        offset: 0,
        filesz: 0,
        memsz: 0,
        flags: 0,
    }; MAX_SEGMENTS];
    let mut n = 0;
    for i in 0..phnum {
        // Checked: `e_phoff` is untrusted, so `ph` (and the `ph + k` field
        // offsets below) must not wrap. Bounding the whole entry up front
        // keeps the later `ph + k` additions overflow-free (k < phentsize).
        let ph = i
            .checked_mul(phentsize)
            .and_then(|o| phoff.checked_add(o))
            .ok_or(ElfError::Truncated)?;
        let ph_end = ph.checked_add(phentsize).ok_or(ElfError::Truncated)?;
        if ph_end > bytes.len() {
            return Err(ElfError::Truncated);
        }
        let p_type = u32le(bytes, ph)?;
        if p_type != 1 {
            continue; // PT_LOAD only
        }
        if n == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }
        let seg = Segment {
            flags: u32le(bytes, ph + 4)?,
            offset: u64le(bytes, ph + 8)?,
            vaddr: u64le(bytes, ph + 16)?,
            filesz: u64le(bytes, ph + 32)?,
            memsz: u64le(bytes, ph + 40)?,
        };
        if seg.filesz > seg.memsz
            || seg
                .offset
                .checked_add(seg.filesz)
                .is_none_or(|end| end > bytes.len() as u64)
            // Producer/consumer agreement: refuse exactly the segments
            // `prepare` cannot lay out (catches the vaddr+memsz wrap and
            // rejects the page-rounding overflow).
            || seg.page_layout().is_err()
        {
            return Err(ElfError::BadSegment);
        }
        if seg.memsz == 0 {
            continue;
        }
        segments[n] = seg;
        n += 1;
    }
    if n == 0 {
        return Err(ElfError::BadSegment);
    }
    Ok(Image {
        entry,
        segments,
        nsegments: n,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built minimal ELF: one PT_LOAD with 8 file bytes + 8 bss.
    fn tiny_elf() -> Vec<u8> {
        let mut e = vec![0u8; 0x78 + 8];
        e[0..4].copy_from_slice(b"\x7FELF");
        e[4] = 2; // 64-bit
        e[5] = 1; // LE
        e[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        e[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
        e[24..32].copy_from_slice(&0x8000_0000u64.to_le_bytes()); // entry
        e[32..40].copy_from_slice(&0x40u64.to_le_bytes()); // phoff
        e[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
        e[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
                                                        // phdr at 0x40
        e[0x40..0x44].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        e[0x44..0x48].copy_from_slice(&(PF_R | PF_X).to_le_bytes());
        e[0x48..0x50].copy_from_slice(&0x78u64.to_le_bytes()); // offset
        e[0x50..0x58].copy_from_slice(&0x8000_0000u64.to_le_bytes()); // vaddr
        e[0x60..0x68].copy_from_slice(&8u64.to_le_bytes()); // filesz
        e[0x68..0x70].copy_from_slice(&16u64.to_le_bytes()); // memsz
        e[0x78..0x80].copy_from_slice(b"codecode");
        e
    }

    #[test]
    fn parses_minimal_image() {
        let bytes = tiny_elf();
        let img = parse(&bytes).unwrap();
        assert_eq!(img.entry, 0x8000_0000);
        assert_eq!(img.nsegments, 1);
        let s = img.segments[0];
        assert_eq!((s.vaddr, s.filesz, s.memsz), (0x8000_0000, 8, 16));
        assert_eq!(s.flags, PF_R | PF_X);
    }

    #[test]
    fn rejects_malformed() {
        assert!(matches!(parse(b"not an elf"), Err(ElfError::Truncated)));
        let mut bad = tiny_elf();
        bad[0] = 0;
        assert!(matches!(parse(&bad), Err(ElfError::BadMagic)));
        let mut trunc = tiny_elf();
        trunc[0x60..0x68].copy_from_slice(&10_000u64.to_le_bytes()); // filesz beyond file
        assert!(matches!(parse(&trunc), Err(ElfError::BadSegment)));
        // vaddr + memsz within PAGE-1 of u64::MAX. The page-up rounding would
        // overflow, so `parse` must refuse it. memsz=8, filesz=8 keeps
        // filesz<=memsz and the file extent in-bounds, isolating the rounding
        // overflow.
        let mut i5 = tiny_elf();
        i5[0x50..0x58].copy_from_slice(&(u64::MAX - 8).to_le_bytes()); // vaddr
        i5[0x60..0x68].copy_from_slice(&8u64.to_le_bytes()); // filesz
        i5[0x68..0x70].copy_from_slice(&8u64.to_le_bytes()); // memsz
        assert!(matches!(parse(&i5), Err(ElfError::BadSegment)));
    }

    fn seg(vaddr: u64, memsz: u64) -> Segment {
        Segment {
            vaddr,
            offset: 0,
            filesz: 0,
            memsz,
            flags: 0,
        }
    }

    #[test]
    fn page_layout_normal() {
        let l = seg(0x8000_0123, 0x2000).page_layout().unwrap();
        assert_eq!(l.va_start, 0x8000_0000);
        assert_eq!(l.va_end, 0x8000_3000);
        assert_eq!(l.pages, 3);
        assert_eq!(l.page_offset, 0x123);
        // Universal invariants.
        assert!(l.va_start <= 0x8000_0123);
        assert!(0x8000_0123 < l.va_end);
        assert_eq!(l.pages * PAGE, l.va_end - l.va_start);
        assert!(l.page_offset < PAGE);
    }

    #[test]
    fn page_layout_overflow_boundary_refused() {
        // Witness vaddr + memsz == u64::MAX, so the `+ PAGE-1` page-up rounding
        // overflows. Unchecked, the math would abort (overflow-checks on, dev)
        // or wrap (release); `page_layout` returns a clean Err with no panic.
        assert_eq!(
            seg(u64::MAX - 8, 8).page_layout(),
            Err(ElfError::BadSegment)
        );
        // The exact boundary: the largest vaddr+memsz that still rounds up
        // without overflow is u64::MAX - (PAGE-1).
        assert!(seg(u64::MAX - (PAGE - 1), 0).page_layout().is_ok());
        assert_eq!(
            seg(u64::MAX - (PAGE - 1) + 1, 0).page_layout(),
            Err(ElfError::BadSegment)
        );
    }
}
