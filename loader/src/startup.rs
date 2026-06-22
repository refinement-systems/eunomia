//! The startup block (rev1§5.1): the first message on a child's bootstrap
//! channel, carrying *argv*, *env*, and a **named-grant table**. One versioned,
//! self-describing format (`b"EUS1"`) supersedes the three hand-rolled
//! fixed-layout blocks (`SD02` init→storaged, `SH01` init→shell, `ST01`
//! shell→child).
//!
//! Strict, like the sibling `elf` decoder: the block is **untrusted-shaped
//! input** consumed in `_start` before anything else exists, so `decode` is
//! **total over arbitrary bytes** — a malformed block is refused (`None`),
//! never a panic / out-of-bounds read / unbounded allocation (rev1§2.7
//! refuse-not-crash). The producer side is total the other way: `encode`
//! refuses an over-budget block with a clean `Err`, never a panic or a silent
//! truncation. `no_std`/`core`-only (no `alloc`): argv/env decode as borrowed
//! slices into the message buffer, and the grant table lives in a fixed-size
//! arena, so a `no_std` `_start` reads a block without touching a heap.
//!
//! ## Wire layout
//!
//! ```text
//! Startup block  (≤ MAX_BLOCK = 256 bytes, one bootstrap-channel message)
//!   Header (7 bytes):
//!     [0..4]  magic   : [u8;4] = b"EUS1"
//!     [4]     ngrants : u8
//!     [5]     nargv   : u8
//!     [6]     nenv    : u8
//!   Grants (ngrants entries; each tagged, so its size is a function of kind):
//!     name : u8     well-known name id (NAME_*)
//!     kind : u8     KIND_CAP_SLOT | KIND_STORAGE_HANDLE | KIND_REGION
//!       KIND_CAP_SLOT       : slot:u32                  (entry = 6 bytes)
//!       KIND_STORAGE_HANDLE : handle:u32                (entry = 6 bytes)
//!       KIND_REGION         : va:u64, len:u64, pa:u64   (entry = 26 bytes)
//!   Argv (nargv entries):  each  len:u16, then len bytes
//!   Env  (nenv  entries):  each  len:u16, then len bytes
//! ```
//!
//! The grant kinds: `CAP_SLOT` and `STORAGE_HANDLE` are the spec's literal two
//! (kernel caps resolve to cspace slots, storage grants resolve to handle
//! numbers). `REGION` generalizes the one grant the system already delivers —
//! `time` is a pre-mapped VA in every block today, and the old `SD02` MMIO/DMA
//! fields are the same shape (VA + len, plus a device PA for DMA). A region
//! carries **no new authority**: the parent maps the page before start exactly
//! as today; only the VA travels.

/// Magic for the unified startup block, version 1. Supersedes the bespoke
/// `SD02`/`SH01`/`ST01` fixed layouts.
pub const MAGIC: [u8; 4] = *b"EUS1";

/// Hard size budget for one block: the kernel's `MSG_PAYLOAD`
/// (`kcore::channel::MSG_PAYLOAD = 256`). A block that would exceed this is
/// refused by `encode`, since it could not be delivered in one message.
pub const MAX_BLOCK: usize = 256;

// Well-known name ids (rev1§5.1 standard names + bring-up device names). A
// small `u8` enum so a `no_std` `_start` resolves a name with an integer match,
// not string handling. `name = 0` is reserved as a future string-name escape so
// the eventual stable public ABI (rev1§8.3) can widen to byte-string names
// without a format break; v1 uses ids only.
/// Reserved string-name escape (unused in v1; see module docs).
pub const NAME_STRING: u8 = 0;
/// The process's storage root (rev1§5.1).
pub const NAME_ROOT: u8 = 1;
/// Standard input — deliberately split from `stdout` (rev1§5.1). Reserved in
/// C1; populated by C-M9 (the console).
pub const NAME_STDIN: u8 = 2;
/// Standard output — deliberately split from `stdin`. Reserved in C1.
pub const NAME_STDOUT: u8 = 3;
/// A writable scratch subtree (rev1§5.1). Reserved unless carvable.
pub const NAME_TMP: u8 = 4;
/// The process's storage session channel (rev1§5.1).
pub const NAME_STORAGE: u8 = 5;
/// The monotonic time page (rev1§2.6). The one named grant delivered today.
pub const NAME_TIME: u8 = 6;
/// The virtio MMIO transport window (bring-up; storaged).
pub const NAME_VIRTIO_MMIO: u8 = 16;
/// The DMA pool region (bring-up; storaged).
pub const NAME_DMA: u8 = 17;

/// Grant kind: a kernel cap, named by the cspace slot it was installed into.
pub const KIND_CAP_SLOT: u8 = 1;
/// Grant kind: a storage grant, named by its handle number on the session.
pub const KIND_STORAGE_HANDLE: u8 = 2;
/// Grant kind: a pre-mapped region (VA, length, and an optional device PA).
pub const KIND_REGION: u8 = 3;

/// Maximum grant-table entries a block may carry. The fixed arena `decode`
/// fills and the bound it validates `ngrants` against. Comfortably above the
/// real blocks (storaged's is 3; the shell's is ≤ 4).
pub const MAX_GRANTS: usize = 8;
/// Maximum argv byte-strings a block may carry.
pub const MAX_ARGV: usize = 8;
/// Maximum env byte-strings a block may carry.
pub const MAX_ENV: usize = 8;

/// What a named grant resolves to. `CapSlot`/`StorageHandle` are the spec's two
/// kinds; `Region` is the additive pre-mapped-region kind (carries no new
/// authority — only a VA the parent already mapped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// A kernel cap, installed at this cspace slot index before start.
    CapSlot(u32),
    /// A storage grant, this handle number on the process's session channel.
    StorageHandle(u32),
    /// A pre-mapped region: virtual address, length, and device physical
    /// address (`pa == 0` unless it is a DMA region read through a phys-cap).
    Region { va: u64, len: u64, pa: u64 },
}

/// One named-grant-table entry: a well-known `name` id and what it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub name: u8,
    pub kind: GrantKind,
}

impl Grant {
    /// Arena placeholder (filler for the unused tail of `Startup::grants`).
    const PLACEHOLDER: Grant = Grant {
        name: NAME_STRING,
        kind: GrantKind::CapSlot(0),
    };
}

/// A decoded (or to-be-encoded) startup block. The grant table and the
/// argv/env vectors live in fixed-size arenas with explicit counts; only the
/// first `n*` entries of each are meaningful. argv/env entries borrow into the
/// backing byte buffer (the message, for `decode`).
#[derive(Debug, Clone)]
pub struct Startup<'a> {
    pub grants: [Grant; MAX_GRANTS],
    pub ngrants: usize,
    pub argv: [&'a [u8]; MAX_ARGV],
    pub nargv: usize,
    pub env: [&'a [u8]; MAX_ENV],
    pub nenv: usize,
}

/// Equality compares only the meaningful prefixes (`[..n*]`) of each arena, so
/// two blocks built different ways — e.g. a producer-built one and the result
/// of decoding its encoding — compare equal regardless of arena filler.
impl PartialEq for Startup<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.ngrants == other.ngrants
            && self.nargv == other.nargv
            && self.nenv == other.nenv
            && self.grants[..self.ngrants] == other.grants[..other.ngrants]
            && self.argv[..self.nargv] == other.argv[..other.nargv]
            && self.env[..self.nenv] == other.env[..other.nenv]
    }
}

impl Eq for Startup<'_> {}

impl Default for Startup<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why `encode` (or a builder) refused a block. The producer maps either to a
/// clean boot/spawn failure — never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// More entries than the fixed arena holds (`MAX_GRANTS`/`MAX_ARGV`/
    /// `MAX_ENV`), or a single byte-string longer than `u16::MAX`.
    TooManyEntries,
    /// The serialized block would exceed the output buffer or `MAX_BLOCK`.
    Overflow,
}

impl<'a> Startup<'a> {
    /// An empty block (no grants, no argv, no env), ready to push onto.
    pub const fn new() -> Self {
        Startup {
            grants: [Grant::PLACEHOLDER; MAX_GRANTS],
            ngrants: 0,
            argv: [&[]; MAX_ARGV],
            nargv: 0,
            env: [&[]; MAX_ENV],
            nenv: 0,
        }
    }

    /// Append a grant. `Err(TooManyEntries)` past `MAX_GRANTS`.
    pub fn push_grant(&mut self, g: Grant) -> Result<(), EncodeError> {
        if self.ngrants >= MAX_GRANTS {
            return Err(EncodeError::TooManyEntries);
        }
        self.grants[self.ngrants] = g;
        self.ngrants += 1;
        Ok(())
    }

    /// Append an argv byte-string (borrowed). `Err(TooManyEntries)` past
    /// `MAX_ARGV`.
    pub fn push_argv(&mut self, s: &'a [u8]) -> Result<(), EncodeError> {
        if self.nargv >= MAX_ARGV {
            return Err(EncodeError::TooManyEntries);
        }
        self.argv[self.nargv] = s;
        self.nargv += 1;
        Ok(())
    }

    /// Append an env byte-string (borrowed). `Err(TooManyEntries)` past
    /// `MAX_ENV`.
    pub fn push_env(&mut self, s: &'a [u8]) -> Result<(), EncodeError> {
        if self.nenv >= MAX_ENV {
            return Err(EncodeError::TooManyEntries);
        }
        self.env[self.nenv] = s;
        self.nenv += 1;
        Ok(())
    }

    /// The resolved value of the first grant named `name`, if present.
    pub fn grant(&self, name: u8) -> Option<GrantKind> {
        self.grants[..self.ngrants]
            .iter()
            .find(|g| g.name == name)
            .map(|g| g.kind)
    }
}

/// A bounds-checked cursor over the input. Every read is `get`-checked with
/// `checked_add` (mirroring `elf::u16le`/`u32le`/`u64le`), so decode is total:
/// any read past the end yields `None`, never a panic or an out-of-bounds
/// access.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|s| u16::from_le_bytes([s[0], s[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
    }
}

/// Decode a startup block. Total over arbitrary bytes (rev1§2.7): validates the
/// magic, then each count against its arena cap, then bounds-checks every grant
/// body / argv / env length against the remaining slice before reading. Any
/// shortfall, unknown grant `kind`, bad magic, or over-cap count returns `None`
/// — never a panic, an out-of-bounds read, or an unbounded allocation. Trailing
/// bytes after the last field are tolerated (the `elf`/`parse_config`
/// precedent). Returned argv/env slices borrow into `buf`.
pub fn decode(buf: &[u8]) -> Option<Startup<'_>> {
    let mut r = Reader { buf, pos: 0 };
    if r.take(4)? != MAGIC {
        return None;
    }
    let ngrants = r.u8()? as usize;
    let nargv = r.u8()? as usize;
    let nenv = r.u8()? as usize;
    if ngrants > MAX_GRANTS || nargv > MAX_ARGV || nenv > MAX_ENV {
        return None;
    }

    let mut s = Startup::new();
    for _ in 0..ngrants {
        let name = r.u8()?;
        let kind = match r.u8()? {
            KIND_CAP_SLOT => GrantKind::CapSlot(r.u32()?),
            KIND_STORAGE_HANDLE => GrantKind::StorageHandle(r.u32()?),
            KIND_REGION => GrantKind::Region {
                va: r.u64()?,
                len: r.u64()?,
                pa: r.u64()?,
            },
            _ => return None,
        };
        // `_ < ngrants <= MAX_GRANTS`, so the push cannot exceed the arena.
        s.push_grant(Grant { name, kind }).ok()?;
    }
    for _ in 0..nargv {
        let len = r.u16()? as usize;
        s.push_argv(r.take(len)?).ok()?;
    }
    for _ in 0..nenv {
        let len = r.u16()? as usize;
        s.push_env(r.take(len)?).ok()?;
    }
    Some(s)
}

/// A bounds-checked writer over the output buffer. Every write is `get_mut`-
/// checked, so `encode` is total: a write past the end yields
/// `Err(Overflow)`, never a panic or a silent truncation.
struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(EncodeError::Overflow)?;
        let dst = self
            .buf
            .get_mut(self.pos..end)
            .ok_or(EncodeError::Overflow)?;
        dst.copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), EncodeError> {
        self.put(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<(), EncodeError> {
        self.put(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> Result<(), EncodeError> {
        self.put(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> Result<(), EncodeError> {
        self.put(&v.to_le_bytes())
    }
}

/// Serialize `s` into `out`, returning the number of bytes written. Total: an
/// over-cap count (`TooManyEntries`) or a block that would exceed `out` or
/// `MAX_BLOCK` (`Overflow`) returns a clean `Err`, never a panic or a silent
/// truncation — the producer maps it to a boot/spawn failure. `out` is normally
/// the producer's `[u8; MAX_BLOCK]`.
pub fn encode(s: &Startup, out: &mut [u8]) -> Result<usize, EncodeError> {
    if s.ngrants > MAX_GRANTS || s.nargv > MAX_ARGV || s.nenv > MAX_ENV {
        return Err(EncodeError::TooManyEntries);
    }
    // Counts are bounded by the arena caps (<= 8), so the `as u8` casts below
    // cannot truncate.
    let mut w = Writer { buf: out, pos: 0 };
    w.put(&MAGIC)?;
    w.u8(s.ngrants as u8)?;
    w.u8(s.nargv as u8)?;
    w.u8(s.nenv as u8)?;
    for g in &s.grants[..s.ngrants] {
        w.u8(g.name)?;
        match g.kind {
            GrantKind::CapSlot(slot) => {
                w.u8(KIND_CAP_SLOT)?;
                w.u32(slot)?;
            }
            GrantKind::StorageHandle(handle) => {
                w.u8(KIND_STORAGE_HANDLE)?;
                w.u32(handle)?;
            }
            GrantKind::Region { va, len, pa } => {
                w.u8(KIND_REGION)?;
                w.u64(va)?;
                w.u64(len)?;
                w.u64(pa)?;
            }
        }
    }
    for v in s.argv[..s.nargv].iter().chain(&s.env[..s.nenv]) {
        let len = u16::try_from(v.len()).map_err(|_| EncodeError::TooManyEntries)?;
        w.u16(len)?;
        w.put(v)?;
    }
    // Enforce the message budget even when `out` is larger than one message.
    if w.pos > MAX_BLOCK {
        return Err(EncodeError::Overflow);
    }
    Ok(w.pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A representative block exercising all three grant kinds plus a
    /// multi-element argv and an env entry.
    fn sample<'a>() -> Startup<'a> {
        let mut s = Startup::new();
        s.push_grant(Grant {
            name: NAME_TIME,
            kind: GrantKind::Region {
                va: 0xA300_0000,
                len: 4096,
                pa: 0,
            },
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_STORAGE,
            kind: GrantKind::CapSlot(1),
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_ROOT,
            kind: GrantKind::StorageHandle(0),
        })
        .unwrap();
        s.push_argv(b"selftest").unwrap();
        s.push_argv(b"254").unwrap();
        s.push_env(b"K=V").unwrap();
        s
    }

    #[test]
    fn golden_layout() {
        // encode → the exact bytes at the pinned offsets.
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let got = &buf[..n];

        // Header.
        assert_eq!(&got[0..4], b"EUS1");
        assert_eq!(got[4], 3); // ngrants
        assert_eq!(got[5], 2); // nargv
        assert_eq!(got[6], 1); // nenv

        // Grant 0: TIME region (name, kind, va, len, pa) = 26 bytes at offset 7.
        assert_eq!(got[7], NAME_TIME);
        assert_eq!(got[8], KIND_REGION);
        assert_eq!(&got[9..17], &0xA300_0000u64.to_le_bytes());
        assert_eq!(&got[17..25], &4096u64.to_le_bytes());
        assert_eq!(&got[25..33], &0u64.to_le_bytes());

        // Grant 1: STORAGE cap-slot = 6 bytes at offset 33.
        assert_eq!(got[33], NAME_STORAGE);
        assert_eq!(got[34], KIND_CAP_SLOT);
        assert_eq!(&got[35..39], &1u32.to_le_bytes());

        // Grant 2: ROOT storage-handle = 6 bytes at offset 39.
        assert_eq!(got[39], NAME_ROOT);
        assert_eq!(got[40], KIND_STORAGE_HANDLE);
        assert_eq!(&got[41..45], &0u32.to_le_bytes());

        // Argv[0] = "selftest" (len:u16 + bytes) at offset 45.
        assert_eq!(&got[45..47], &8u16.to_le_bytes());
        assert_eq!(&got[47..55], b"selftest");
        // Argv[1] = "254" at offset 55.
        assert_eq!(&got[55..57], &3u16.to_le_bytes());
        assert_eq!(&got[57..60], b"254");
        // Env[0] = "K=V" at offset 60.
        assert_eq!(&got[60..62], &3u16.to_le_bytes());
        assert_eq!(&got[62..65], b"K=V");
        assert_eq!(n, 65);

        // …and decode of those exact bytes yields the entries back.
        let d = decode(got).unwrap();
        assert_eq!(d.ngrants, 3);
        assert_eq!(d.grant(NAME_TIME), Some(s.grants[0].kind));
        assert_eq!(d.grant(NAME_STORAGE), Some(GrantKind::CapSlot(1)));
        assert_eq!(d.grant(NAME_ROOT), Some(GrantKind::StorageHandle(0)));
        assert_eq!(&d.argv[..d.nargv], &[b"selftest".as_slice(), b"254"]);
        assert_eq!(&d.env[..d.nenv], &[b"K=V".as_slice()]);
    }

    #[test]
    fn decode_tolerates_trailing_bytes() {
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        // The recv buffer is always [u8; 256]; a decode of the whole padded
        // buffer (trailing zeros past the block) must still succeed.
        assert_eq!(decode(&buf), Some(s));
    }

    #[test]
    fn rejects_malformed() {
        // Empty and a wrong magic.
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(b"EUS0\x00\x00\x00"), None);
        // Header truncated (magic but no counts).
        assert_eq!(decode(b"EUS1"), None);

        // A well-formed prefix whose single grant body is truncated.
        // ngrants=1, nargv=0, nenv=0, then a REGION grant with only 3 of its
        // 24 body bytes present.
        let mut t = Vec::new();
        t.extend_from_slice(&MAGIC);
        t.extend_from_slice(&[1, 0, 0]); // ngrants, nargv, nenv
        t.push(NAME_TIME);
        t.push(KIND_REGION);
        t.extend_from_slice(&[0u8; 3]); // far short of 24
        assert_eq!(decode(&t), None);

        // An unknown grant kind byte.
        let mut k = Vec::new();
        k.extend_from_slice(&MAGIC);
        k.extend_from_slice(&[1, 0, 0]);
        k.push(NAME_TIME);
        k.push(99); // not a KIND_*
        k.extend_from_slice(&[0u8; 24]);
        assert_eq!(decode(&k), None);

        // A count over the arena cap is refused before any body read.
        let mut c = Vec::new();
        c.extend_from_slice(&MAGIC);
        c.extend_from_slice(&[(MAX_GRANTS as u8) + 1, 0, 0]);
        assert_eq!(decode(&c), None);

        // An argv length that runs past the buffer.
        let mut a = Vec::new();
        a.extend_from_slice(&MAGIC);
        a.extend_from_slice(&[0, 1, 0]); // one argv
        a.extend_from_slice(&255u16.to_le_bytes()); // declares 255 bytes…
        a.extend_from_slice(b"short"); // …but only 5 follow
        assert_eq!(decode(&a), None);
    }

    #[test]
    fn encode_refuses_over_budget() {
        // An argv vector whose bytes blow past the 256-byte budget: encode must
        // return Err(Overflow), never panic or truncate. Each entry is 200
        // bytes; two of them exceed MAX_BLOCK.
        let big = [b'x'; 200];
        let mut s = Startup::new();
        s.push_argv(&big).unwrap();
        s.push_argv(&big).unwrap();
        let mut buf = [0u8; MAX_BLOCK];
        assert_eq!(encode(&s, &mut buf), Err(EncodeError::Overflow));

        // A too-small output buffer also refuses cleanly.
        let mut tiny = [0u8; 4];
        assert_eq!(encode(&sample(), &mut tiny), Err(EncodeError::Overflow));
    }

    #[test]
    fn builders_refuse_past_the_arena() {
        let mut s = Startup::new();
        for _ in 0..MAX_GRANTS {
            s.push_grant(Grant {
                name: NAME_TIME,
                kind: GrantKind::CapSlot(0),
            })
            .unwrap();
        }
        assert_eq!(
            s.push_grant(Grant {
                name: NAME_TIME,
                kind: GrantKind::CapSlot(0),
            }),
            Err(EncodeError::TooManyEntries)
        );
    }

    /// Negative control (anti-theater): the round-trip equality oracle must
    /// have teeth — perturbing one field of the decoded block makes it compare
    /// *un*equal, so the round-trip proptest below is not vacuously true.
    #[test]
    fn round_trip_oracle_has_teeth() {
        let s = sample();
        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let got = decode(&buf[..n]).unwrap();
        assert_eq!(got, s); // the real round-trip holds…

        // …and a single perturbed grant name is distinguished.
        let mut wrong = s.clone();
        wrong.grants[0].name ^= 1;
        assert_ne!(got, wrong);
        // As is a perturbed argv element and a changed count.
        let mut wrong_argv = s.clone();
        wrong_argv.argv[0] = b"SELFTEST";
        assert_ne!(got, wrong_argv);
        let mut wrong_n = s.clone();
        wrong_n.nargv = 1;
        assert_ne!(got, wrong_n);
    }

    // Bounded strategies so an encoded block stays within the 256-byte budget:
    // grants/argv/env counts within their arenas, short byte-strings, and a
    // grant kind drawn from all three.
    fn grant_kind() -> impl Strategy<Value = GrantKind> {
        prop_oneof![
            any::<u32>().prop_map(GrantKind::CapSlot),
            any::<u32>().prop_map(GrantKind::StorageHandle),
            (any::<u64>(), any::<u64>(), any::<u64>())
                .prop_map(|(va, len, pa)| GrantKind::Region { va, len, pa }),
        ]
    }

    fn grant() -> impl Strategy<Value = Grant> {
        (any::<u8>(), grant_kind()).prop_map(|(name, kind)| Grant { name, kind })
    }

    fn byte_strings(max: usize) -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(prop::collection::vec(any::<u8>(), 0..12), 0..=max)
    }

    proptest! {
        // Miri runs the interpreter; a handful of cases cover the same logic,
        // native keeps the full sweep (the cas/file.rs, layout_props idiom).
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// encode → decode round-trips every block within the arena caps.
        #[test]
        fn round_trips(
            grants in prop::collection::vec(grant(), 0..=MAX_GRANTS),
            argv in byte_strings(MAX_ARGV),
            env in byte_strings(MAX_ENV),
        ) {
            let mut s = Startup::new();
            for g in &grants {
                s.push_grant(*g).unwrap();
            }
            for a in &argv {
                s.push_argv(a).unwrap();
            }
            for e in &env {
                s.push_env(e).unwrap();
            }
            let mut buf = [0u8; MAX_BLOCK];
            // Within-arena counts and short byte-strings, but a full table of
            // region grants can still exceed 256 bytes. encode is total: it
            // either fits (and round-trips) or refuses cleanly — and since the
            // counts are within the arena and no string exceeds u16, the only
            // legal refusal is the budget (`Overflow`), never `TooManyEntries`.
            match encode(&s, &mut buf) {
                Ok(n) => prop_assert_eq!(decode(&buf[..n]), Some(s)),
                Err(e) => prop_assert_eq!(e, EncodeError::Overflow),
            }
        }

        /// Total over arbitrary bytes: `decode` never panics — the rev1§2.7
        /// refuse-not-crash floor (the shape of B15C's `parse_config_is_total`).
        #[test]
        fn decode_is_total(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            if let Some(s) = decode(&bytes) {
                // Whatever it returns obeys the arena bounds and every borrowed
                // slice lies inside the input.
                prop_assert!(s.ngrants <= MAX_GRANTS);
                prop_assert!(s.nargv <= MAX_ARGV);
                prop_assert!(s.nenv <= MAX_ENV);
                let range = bytes.as_ptr_range();
                for v in s.argv[..s.nargv].iter().chain(&s.env[..s.nenv]) {
                    if !v.is_empty() {
                        let r = v.as_ptr_range();
                        prop_assert!(r.start >= range.start && r.end <= range.end);
                    }
                }
            }
        }
    }
}
