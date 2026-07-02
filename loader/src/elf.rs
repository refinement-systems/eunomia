// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! Minimal ELF64 little-endian parser for aarch64 executables.
//! Strict (untrusted input — images come from the versioned store):
//! bounds-checked, no panics.
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

// Verus is the deductive-proof tier for the page geometry (`Segment::page_layout`)
// and the ELF decoder (`parse`, reading fixed-width fields via the shared
// `le-bytes` crate's verified little-endian readers), below.
// `vstd::prelude` supplies the `verus!{}` macro + ghost vocabulary; Verus
// requires it imported at the crate root. In an ordinary build the macro erases
// ghost code, so this import is otherwise unused — hence the allow (same as
// freelist/virtio-blk/storage-server).
#[allow(unused_imports)]
use vstd::prelude::*;

// The page-geometry cluster and the ELF decoder are the Verus-verified deductive
// core: `PAGE`, the `Segment`/`PageLayout`/`Image` types, `ElfError`,
// `page_layout`'s total/overflow-safe contract, and `parse`'s total
// bounded-decoder contract all live in the `verus!{}` block so the proofs can
// name them (doc/guidelines/verus.md §6); `parse` reads each fixed-width
// little-endian field through the shared `le-bytes` crate's verified `read_u*_le`
// readers (their `ensures` pin the consumed bytes to the value's `u*_le` split).
// The startup-block codec and the target-only `spawn` stay external plain Rust — after erasure
// these are ordinary items, so `spawn`, the `pub use crate::elf::PAGE`
// re-export, and the callers of `parse` keep working unchanged.
verus! {

/// Page size the loader maps segments at (rev2§5). Canonical home: `spawn`
/// re-exports it (`pub use crate::elf::PAGE`) so the page-layout predicate
/// and its sole consumer agree by construction.
pub const PAGE: u64 = 4096;

/// Low-bits mask for page rounding (`PAGE - 1`). A `u64` const so it reads as
/// `u64` in spec positions — `PAGE - 1` written inline is mathematical-`int`
/// subtraction under Verus and would not type against `&`/the `u64` lemma args.
/// `pub` because `page_layout`'s (public) `ensures` names it.
pub const PAGE_MASK: u64 = PAGE - 1;

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

/// Align-down by an arbitrary mask `m`: clearing `m`'s bits never grows the
/// value, leaves it `m`-aligned, and the cleared remainder is exactly `x & m`
/// (so the two parts partition `x`). All four facts hold for *every* mask, so a
/// single symbolic `by (bit_vector)` discharges them; `page_layout` calls this
/// with `m = PAGE_MASK` and the outer SMT context supplies PAGE's value.
proof fn lemma_align_down(x: u64, m: u64)
    ensures
        (x & !m) <= x,
        (x & !m) & m == 0,
        (x & !m) + (x & m) == x,
        (x & m) <= m,
{
    assert((x & !m) <= x && (x & !m) & m == 0 && (x & !m) + (x & m) == x && (x & m) <= m)
        by (bit_vector);
}

/// Two PAGE-aligned bounds enclose an exact whole number of pages: their span
/// `hi - lo` is a multiple of PAGE, so integer division by PAGE loses nothing
/// (`(hi - lo) / PAGE * PAGE == hi - lo`). PAGE = 2^12, so the low-12-bit mask
/// *is* the modulus (vstd `low_bits_mask_is_mod`); both bounds are then ≡ 0
/// (mod PAGE), their difference is too (`sub_mod_noop`), and the fundamental
/// div-mod identity closes it. Stays in the modular world — no subtraction
/// inside `by (bit_vector)` (where only a contiguous low-bit mask would survive
/// it, doc/guidelines/verus.md §10).
proof fn lemma_pages_exact(lo: u64, hi: u64)
    requires
        lo & PAGE_MASK == 0,
        hi & PAGE_MASK == 0,
        lo <= hi,
    ensures
        (hi - lo) / (PAGE as int) * (PAGE as int) == hi - lo,
{
    vstd::arithmetic::power2::lemma2_to64();
    // The low-12-bit mask is mod 4096; both bounds aligned ⇒ both ≡ 0 (mod PAGE).
    vstd::bits::lemma_u64_low_bits_mask_is_mod(lo, 12);
    vstd::bits::lemma_u64_low_bits_mask_is_mod(hi, 12);
    // Difference of two multiples of PAGE is a multiple of PAGE ⇒ div is exact.
    vstd::arithmetic::div_mod::lemma_sub_mod_noop(hi as int, lo as int, PAGE as int);
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod((hi - lo) as int, PAGE as int);
}

impl Segment {
    /// Page geometry the loader maps this segment into (rev2§5). All
    /// arithmetic is checked: a segment whose page-rounded end would exceed
    /// `u64::MAX` is refused (`BadSegment`), never wrapped or aborted —
    /// `spawn::prepare` runs this on untrusted images (rev2§3.7) and must
    /// refuse-not-crash (rev2§5.3). `parse` runs the same check so the
    /// producer never hands `prepare` a segment it cannot lay out.
    ///
    /// Mechanized total for *all* `(vaddr, memsz)`: `Err(BadSegment)` exactly
    /// when the page-up rounding `vaddr + memsz + (PAGE-1)` overflows `u64`;
    /// otherwise the returned geometry is page-aligned at both ends, encloses
    /// `vaddr` (and, when `memsz > 0`, strictly), the in-page offset is in
    /// `[0, PAGE)`, and `pages` counts the span exactly
    /// (`pages * PAGE == va_end - va_start`). `memsz == 0` yields `pages == 0`
    /// for a page-aligned `vaddr`, else `1` from the round-up — no panic either
    /// way; `parse` drops `memsz == 0` segments, so `prepare` only ever sees
    /// `memsz > 0` (⇒ `pages >= 1`).
    pub fn page_layout(&self) -> (res: Result<PageLayout, ElfError>)
        ensures
            res is Err <==> self.vaddr + self.memsz + PAGE_MASK > u64::MAX,
            res matches Err(e) ==> e == ElfError::BadSegment,
            res matches Ok(l) ==> {
                &&& l.va_start & PAGE_MASK == 0
                &&& l.va_end & PAGE_MASK == 0
                &&& l.va_start <= self.vaddr
                &&& (self.memsz > 0 ==> self.vaddr < l.va_end)
                &&& l.page_offset < PAGE
                &&& l.page_offset == self.vaddr - l.va_start
                &&& l.pages * PAGE == l.va_end - l.va_start
            },
    {
        let va_start = self.vaddr & !PAGE_MASK;  // round down: cannot overflow
        proof {
            lemma_align_down(self.vaddr, PAGE_MASK);
        }
        // `checked_add` refuses both the `vaddr + memsz` wrap and the page-up
        // rounding overflow; either failure is the single `Err(BadSegment)`.
        match self.vaddr.checked_add(self.memsz) {
            None => Err(ElfError::BadSegment),
            Some(s) => match s.checked_add(PAGE_MASK) {
                None => Err(ElfError::BadSegment),
                Some(e) => {
                    let va_end = e & !PAGE_MASK;  // round up to page boundary
                    proof {
                        lemma_align_down(e, PAGE_MASK);
                        // Round-up never drops below the input: va_end == e - (e & m)
                        // >= e - (PAGE-1) == vaddr + memsz, so va_start <= va_end and
                        // the span subtraction below is total (no underflow).
                        assert(va_end >= self.vaddr + self.memsz);
                    }
                    let span = va_end - va_start;
                    proof {
                        // span is an exact number of pages (difference of two aligned bounds).
                        lemma_pages_exact(va_start, va_end);
                    }
                    Ok(
                        PageLayout {
                            va_start,
                            va_end,
                            pages: span / PAGE,
                            page_offset: self.vaddr
                                - va_start,  // in [0, PAGE): cannot underflow
                        },
                    )
                },
            },
        }
    }
}

pub const MAX_SEGMENTS: usize = 8;

/// A parsed, validated ELF image: the entry point, the accepted PT_LOAD
/// segments (`segments[..nsegments]`), and the backing bytes. `parse` is the
/// only constructor and its `ensures` make every returned `Image` satisfy
/// [`well_formed_image`].
#[derive(Debug)]
pub struct Image<'a> {
    pub entry: u64,
    pub segments: [Segment; MAX_SEGMENTS],
    pub nsegments: usize,
    pub bytes: &'a [u8],
}

/// A PT_LOAD segment `parse` will accept: its file extent lies inside the
/// image (`offset + filesz <= len`) and its page geometry does not overflow
/// (equivalently `page_layout()` returns `Ok` — the rev2§5.3 refuse-not-crash
/// boundary mechanized on `page_layout`).
pub open spec fn seg_ok(s: Segment, len: nat) -> bool {
    &&& s.offset + s.filesz <= len
    &&& s.vaddr + s.memsz + PAGE_MASK <= u64::MAX
}

/// What `parse` guarantees of a returned `Image`: between one and
/// `MAX_SEGMENTS` accepted segments, each satisfying [`seg_ok`] against the
/// backing bytes (rev2§5).
pub open spec fn well_formed_image(img: Image) -> bool {
    &&& 1 <= img.nsegments <= MAX_SEGMENTS
    &&& forall|j: int|
        0 <= j < img.nsegments ==> seg_ok(#[trigger] img.segments@[j], img.bytes@.len())
}

/// Parse an ELF64 little-endian aarch64 executable (rev2§5). Strict, untrusted
/// input: images are data in the versioned store, so any writer feeds bytes
/// here. Mechanized **total** — for every `&[u8]`, `parse` returns without
/// panicking or reading out of bounds (rev2§5.3) — and every accepted `Image`
/// satisfies [`well_formed_image`]: a non-empty, bounded set of segments, each
/// in range and layout-safe, so `spawn::prepare` is never handed a segment it
/// cannot lay out.
pub fn parse(bytes: &[u8]) -> (r: Result<Image<'_>, ElfError>)
    ensures
        r matches Ok(img) ==> well_formed_image(img),
{
    broadcast use vstd::slice::group_slice_axioms;

    if bytes.len() < 64 {
        return Err(ElfError::Truncated);
    }
    // ELF magic `\x7FELF`, checked byte-wise (Verus does not spec slice equality).

    if bytes[0] != 0x7f || bytes[1] != 0x45 || bytes[2] != 0x4c || bytes[3] != 0x46 {
        return Err(ElfError::BadMagic);
    }
    // EI_CLASS = 2 (64-bit), EI_DATA = 1 (LE)

    if bytes[4] != 2 || bytes[5] != 1 {
        return Err(ElfError::NotElf64Le);
    }
    let e_type = le_bytes::read_u16_le(bytes, 16);
    if e_type != 2 {
        // ET_EXEC: userspace is statically linked at fixed VAs (rev2§5).
        return Err(ElfError::NotExecutable);
    }
    if le_bytes::read_u16_le(bytes, 18) != 183 {
        // EM_AARCH64
        return Err(ElfError::NotAarch64);
    }
    let entry = le_bytes::read_u64_le(bytes, 24);
    let phoff = le_bytes::read_u64_le(bytes, 32) as usize;
    let phentsize = le_bytes::read_u16_le(bytes, 54) as usize;
    let phnum = le_bytes::read_u16_le(bytes, 56) as usize;
    if phentsize < 56 {
        return Err(ElfError::BadSegment);
    }
    let mut segments = [Segment {
        vaddr: 0,
        offset: 0,
        filesz: 0,
        memsz: 0,
        flags: 0,
    };MAX_SEGMENTS];
    let mut n: usize = 0;
    let mut i: usize = 0;
    while i < phnum
        invariant
            bytes@.len() >= 64,
            phentsize >= 56,
            n <= MAX_SEGMENTS,
            forall|j: int| 0 <= j < n ==> seg_ok(#[trigger] segments@[j], bytes@.len()),
        decreases phnum - i,
    {
        // `phoff`/`phentsize` are untrusted, so `ph` (and the `ph + k` field
        // offsets below) must not wrap. Bounding the whole entry up front keeps
        // the later `ph + k` additions overflow-free (k < phentsize).
        let ph = match i.checked_mul(phentsize) {
            None => return Err(ElfError::Truncated),
            Some(o) => match phoff.checked_add(o) {
                None => return Err(ElfError::Truncated),
                Some(p) => p,
            },
        };
        let ph_end = match ph.checked_add(phentsize) {
            None => return Err(ElfError::Truncated),
            Some(e) => e,
        };
        if ph_end > bytes.len() {
            return Err(ElfError::Truncated);
        }
        proof {
            // ph_end == ph + phentsize <= len and phentsize >= 56, so every field
            // read below (the widest window is ph+40 .. ph+48) is in bounds.
            assert(ph + 56 <= bytes@.len());
        }
        let p_type = le_bytes::read_u32_le(bytes, ph);
        if p_type == 1 {
            // PT_LOAD only.
            if n == MAX_SEGMENTS {
                return Err(ElfError::TooManySegments);
            }
            let seg = Segment {
                flags: le_bytes::read_u32_le(bytes, ph + 4),
                offset: le_bytes::read_u64_le(bytes, ph + 8),
                vaddr: le_bytes::read_u64_le(bytes, ph + 16),
                filesz: le_bytes::read_u64_le(bytes, ph + 32),
                memsz: le_bytes::read_u64_le(bytes, ph + 40),
            };
            let pl = seg.page_layout();
            // Producer/consumer agreement: refuse exactly the segments `prepare`
            // cannot lay out — file extent past the image, `filesz > memsz`, or a
            // page-rounding overflow (the `pl.is_err()` arm).
            match seg.offset.checked_add(seg.filesz) {
                None => return Err(ElfError::BadSegment),
                Some(end) => {
                    if seg.filesz > seg.memsz || end > bytes.len() as u64 || pl.is_err() {
                        return Err(ElfError::BadSegment);
                    }
                    if seg.memsz != 0 {
                        proof {
                            // The accepted segment satisfies `seg_ok`: file extent
                            // in bounds (`end == offset+filesz <= len`) and layout
                            // overflow-free (`pl.is_ok()` ⇒ `page_layout`'s ensures).
                            assert(seg.offset + seg.filesz <= bytes@.len());
                            assert(seg.vaddr + seg.memsz + PAGE_MASK <= u64::MAX);
                            assert(seg_ok(seg, bytes@.len()));
                        }
                        let ghost prev = segments@;
                        let ghost prev_n = n;
                        segments[n] = seg;
                        proof {
                            assert forall|j: int| 0 <= j < prev_n + 1 implies seg_ok(
                                #[trigger] segments@[j],
                                bytes@.len(),
                            ) by {
                                if j < prev_n {
                                    assert(segments@[j] == prev[j]);
                                    assert(seg_ok(prev[j], bytes@.len()));
                                } else {
                                    assert(segments@[j] == seg);
                                }
                            }
                        }
                        n = n + 1;
                    }
                },
            }
        }
        i = i + 1;
    }
    if n == 0 {
        return Err(ElfError::BadSegment);
    }
    Ok(Image { entry, segments, nsegments: n, bytes })
}

} // verus!
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
