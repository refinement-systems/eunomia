//! On-disk formats (spec §4.2): superblocks, WAL records, the ref table,
//! and the chunk index. All hand-defined and little-endian — nothing
//! persistent speaks postcard (§3.7). Decoders are strict and reject
//! trailing bytes.
//!
//! Device layout:
//!   [0,      4096)            superblock slot A
//!   [4096,   8192)            superblock slot B
//!   [8192,   8192 + wal_len)  WAL region (wal_len recorded in the SB)
//!   [chunk_off, dev_len)      chunk store: framed chunks + index objects
//!
//! Format v2 (M5): the superblock references a durable index object — the
//! hash → (offset, length, birth generation) map plus the free-extent
//! list (§4.2 items 3 and 4) — written as an ordinary self-verifying
//! frame. v1 rebuilt the index by scanning an append-only region; a scan
//! cannot represent holes, and GC exists to make holes.
//!
//! Format v3 (time page, §2.6): snapshot timestamps and file mtimes are
//! UTC nanoseconds since the Unix epoch. The layout did not change — a
//! tick field and a nanosecond field are structurally identical — which
//! is exactly why the version had to: pre-v3 images (whose on-OS rows
//! hold raw CNTVCT ticks) are refused with a version error and re-created
//! with mkfs, never silently misread as dates in 1970.
//!
//! The generation-checksummed A/B superblock flip is the single atomicity
//! mechanism for the entire system (§4.2).

use crate::hash::Hash;
use crate::prolly::{FormatError, Reader};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use vstd::prelude::*;

pub const SB_A_OFF: u64 = 0;
pub const SB_B_OFF: u64 = 4096;

pub(crate) const SB_MAGIC: &[u8; 8] = b"EUNOMIA\0";
/// Checksummed prefix length (pub: the corpus generator forges
/// old-version slots and must re-seal them).
pub const SB_BODY: usize = 96;

// SB_SIZE / WAL_OFF / SB_VERSION live inside the `verus!{}` block below
// (a const declared outside the macro is invisible to Verus); they erase to
// ordinary `pub const`s, so external references are unchanged. CHUNK_HEADER is
// likewise inside the block (it is named by the geometry spec).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub generation: u64,
    pub ref_table: Hash,
    pub wal_head: u64,     // byte offset within the WAL region
    pub wal_next_seq: u64, // seq of the first record at/after wal_head
    pub wal_len: u64,
    pub chunk_tail: u64, // byte offset within the chunk region
    /// Frame offset of the index object within the chunk region.
    pub index_off: u64,
}

impl Superblock {
    pub fn encode(&self) -> [u8; SB_SIZE] {
        let mut buf = [0u8; SB_SIZE];
        buf[0..8].copy_from_slice(SB_MAGIC);
        buf[8..12].copy_from_slice(&SB_VERSION.to_le_bytes());
        buf[16..24].copy_from_slice(&self.generation.to_le_bytes());
        buf[24..56].copy_from_slice(self.ref_table.as_bytes());
        buf[56..64].copy_from_slice(&self.wal_head.to_le_bytes());
        buf[64..72].copy_from_slice(&self.wal_next_seq.to_le_bytes());
        buf[72..80].copy_from_slice(&self.wal_len.to_le_bytes());
        buf[80..88].copy_from_slice(&self.chunk_tail.to_le_bytes());
        buf[88..96].copy_from_slice(&self.index_off.to_le_bytes());
        let sum = Hash::of(&buf[..SB_BODY]);
        buf[SB_BODY..SB_BODY + 32].copy_from_slice(sum.as_bytes());
        buf
    }

    /// Geometry chokepoint (fuzzing findings, MNT-1). The body checksum is
    /// integrity, not authenticity: it proves the slot was written whole,
    /// not that this code wrote it, so every offset/length field is
    /// untrusted until checked here against the device length — the one
    /// ground truth mount has that no field in the block can vouch for.
    /// Checked arithmetic throughout: a wrapping sum that passes a bound is
    /// the same failure shape as OVL-1/ELF-1. After this returns Ok,
    /// downstream code may trust that the WAL region, the committed chunk
    /// region, and the index frame header all lie within the device.
    ///
    /// Thin delegator to the Verus-verified [`validate_geometry_fields`]
    /// (plan §7f): the totality + region-within-device invariant is proven ∀
    /// there, replacing the bounded `check_superblock_geometry` Kani harness.
    pub fn validate_geometry(&self, dev_len: u64) -> Result<(), &'static str> {
        validate_geometry_fields(
            self.wal_head,
            self.wal_len,
            self.chunk_tail,
            self.index_off,
            dev_len,
        )
    }

    /// None = unusable for any reason; recovery discards it (§4.5).
    pub fn decode(buf: &[u8]) -> Option<Superblock> {
        Self::decode_checked(buf).ok()
    }

    /// Like `decode`, but distinguishes *why* a slot is unusable: a torn
    /// slot is recovery's business (discard, try the other slot), while an
    /// intact slot from another format version must surface as a version
    /// error — tick-era (pre-v3) timestamp fields are structurally
    /// identical to nanosecond fields, so misreading is silent (§2.6).
    ///
    /// Thin assembly wrapper over the Verus-verified [`decode_checked_fields`]
    /// (plan §7f): that function proves the parse is **total** ∀ buffer bytes
    /// (no panic — every fixed-offset read is in bounds, the blake3 checksum is
    /// the assumed-total seam), replacing the bounded `check_superblock_decode_total`
    /// Kani harness. The remaining step here — wrapping the raw `[u8; 32]` into a
    /// `Hash` and building the `Superblock` — is trivially total (`Hash::from_bytes`
    /// and the struct literal never panic), so it stays plain Rust.
    pub fn decode_checked(buf: &[u8]) -> Result<Superblock, SbError> {
        let f = decode_checked_fields(buf)?;
        Ok(Superblock {
            generation: f.generation,
            ref_table: Hash::from_bytes(f.ref_table),
            wal_head: f.wal_head,
            wal_next_seq: f.wal_next_seq,
            wal_len: f.wal_len,
            chunk_tail: f.chunk_tail,
            index_off: f.index_off,
        })
    }
}

verus! {

/// Superblock slot size (4 KiB). Inside the macro so the verified parsers and
/// their preconditions can name it.
pub const SB_SIZE: usize = 4096;
/// First byte of the WAL region (after the two 4 KiB superblock slots).
pub const WAL_OFF: u64 = 8192;
/// On-disk format version (format v3, §2.6). Inside the macro for the version
/// gate in `decode_checked_fields`.
pub(crate) const SB_VERSION: u32 = 3;
/// Chunk frame header size (magic + len + birth gen + hash). Inside the macro
/// because the geometry spec bounds the index frame by it.
pub const CHUNK_HEADER: usize = 4 + 4 + 8 + 32;

/// WAL record header size (magic + seq + payload len + payload checksum). Inside
/// the macro (since 8c) so `store.rs`'s verified `decode_frame`/`replay_bound`
/// can name its concrete value — a `const` declared outside `verus!{}` is opaque
/// to it (the 7f rule). Erases to the same `pub const`, so external refs (the
/// plain-Rust `encode_record`/`decode_record` below) are unchanged.
pub const WAL_HEADER: usize = 4 + 8 + 4 + 32;

/// Geometry predicate (the ghost model of [`validate_geometry_fields`]): every
/// committed region lies within the device, each field checked against the one
/// ground truth `dev_len` (no field vouches for another). Stated over `int` so
/// the equivalence is overflow-exact: the exec's `checked_add` rejections are
/// precisely the cases where a clause would wrap past `u64::MAX >= dev_len`.
pub open spec fn geometry_ok(
    wal_head: u64,
    wal_len: u64,
    chunk_tail: u64,
    index_off: u64,
    dev_len: u64,
) -> bool {
    &&& (WAL_OFF as int + wal_len as int <= dev_len as int)
    &&& wal_head <= wal_len
    &&& (WAL_OFF as int + wal_len as int + chunk_tail as int <= dev_len as int)
    &&& (index_off as int + CHUNK_HEADER as int <= chunk_tail as int)
}

/// The §4.5 mount geometry chokepoint, verified ∀ (plan §7f). Total over all
/// field values and `dev_len` (it is all `checked_add`); accepts iff
/// [`geometry_ok`]; and on `Ok` the committed chunk region provably fits the
/// device. Supersedes the bounded `check_superblock_geometry` Kani harness.
pub fn validate_geometry_fields(
    wal_head: u64,
    wal_len: u64,
    chunk_tail: u64,
    index_off: u64,
    dev_len: u64,
) -> (r: Result<(), &'static str>)
    ensures
        (r is Ok) <==> geometry_ok(wal_head, wal_len, chunk_tail, index_off, dev_len),
        r is Ok ==> (WAL_OFF as int + wal_len as int + chunk_tail as int <= dev_len as int),
        r is Ok ==> wal_head <= wal_len,
{
    let chunk_off = match WAL_OFF.checked_add(wal_len) {
        Some(o) => o,
        None => return Err("wal region exceeds device"),
    };
    if chunk_off > dev_len {
        return Err("wal region exceeds device");
    }
    if wal_head > wal_len {
        return Err("wal head beyond wal region");
    }
    let committed_end = match chunk_off.checked_add(chunk_tail) {
        Some(e) => e,
        None => return Err("committed chunk region exceeds device"),
    };
    if committed_end > dev_len {
        return Err("committed chunk region exceeds device");
    }
    let index_end = match index_off.checked_add(CHUNK_HEADER as u64) {
        Some(e) => e,
        None => return Err("index frame outside committed region"),
    };
    if index_end > chunk_tail {
        return Err("index frame outside committed region");
    }
    Ok(())
}

/// The integer/byte fields of a parsed superblock — the Verus-native (`Hash`-free)
/// result of [`decode_checked_fields`]. `Superblock::decode_checked` wraps
/// `ref_table` into a `Hash` afterwards.
pub struct RawSuperblock {
    pub generation: u64,
    pub ref_table: [u8; 32],
    pub wal_head: u64,
    pub wal_next_seq: u64,
    pub wal_len: u64,
    pub chunk_tail: u64,
    pub index_off: u64,
}

/// Why a superblock slot was rejected (see `Superblock::decode_checked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SbError {
    /// Torn, unwritten, or not a superblock at all.
    Invalid,
    /// Intact (magic and checksum verify) but written by another format
    /// version — refuse, never reinterpret (§2.6).
    WrongVersion(u32),
}

/// The 8-byte magic check (`SB_MAGIC == b"EUNOMIA\0"`), spelled per-byte so Verus
/// reasons over `buf[i]` rather than the unspecced slice `==`.
fn magic_ok(buf: &[u8]) -> bool
    requires
        buf@.len() == SB_SIZE,
{
    broadcast use vstd::slice::group_slice_axioms;
    // SB_MAGIC == b"EUNOMIA\0" == [0x45, 0x55, 0x4E, 0x4F, 0x4D, 0x49, 0x41, 0x00].
    buf[0] == 0x45u8
        && buf[1] == 0x55u8
        && buf[2] == 0x4Eu8
        && buf[3] == 0x4Fu8
        && buf[4] == 0x4Du8
        && buf[5] == 0x49u8
        && buf[6] == 0x41u8
        && buf[7] == 0x00u8
}

/// Little-endian `u32` from four bytes at `off`, by explicit indexing + shifts
/// (not `from_le_bytes`/`try_into`, which Verus does not spec — the 7a recipe).
/// `pub(crate)` since 8c so `store.rs`'s `decode_frame` reuses it.
pub(crate) fn read_u32_le(buf: &[u8], off: usize) -> u32
    requires
        off + 4 <= buf@.len(),
{
    broadcast use vstd::slice::group_slice_axioms;
    (buf[off] as u32)
        | ((buf[off + 1] as u32) << 8)
        | ((buf[off + 2] as u32) << 16)
        | ((buf[off + 3] as u32) << 24)
}

/// Little-endian `u64` from eight bytes at `off` (see [`read_u32_le`]).
/// `pub(crate)` since 8c so `store.rs`'s `decode_frame` reuses it.
pub(crate) fn read_u64_le(buf: &[u8], off: usize) -> u64
    requires
        off + 8 <= buf@.len(),
{
    broadcast use vstd::slice::group_slice_axioms;
    (buf[off] as u64)
        | ((buf[off + 1] as u64) << 8)
        | ((buf[off + 2] as u64) << 16)
        | ((buf[off + 3] as u64) << 24)
        | ((buf[off + 4] as u64) << 32)
        | ((buf[off + 5] as u64) << 40)
        | ((buf[off + 6] as u64) << 48)
        | ((buf[off + 7] as u64) << 56)
}

/// The 32 hash bytes at `off`, as an array literal (no `try_into().unwrap()`).
fn read_arr32(buf: &[u8], off: usize) -> [u8; 32]
    requires
        off + 32 <= buf@.len(),
{
    broadcast use vstd::slice::group_slice_axioms;
    [
        buf[off], buf[off + 1], buf[off + 2], buf[off + 3],
        buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7],
        buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11],
        buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15],
        buf[off + 16], buf[off + 17], buf[off + 18], buf[off + 19],
        buf[off + 20], buf[off + 21], buf[off + 22], buf[off + 23],
        buf[off + 24], buf[off + 25], buf[off + 26], buf[off + 27],
        buf[off + 28], buf[off + 29], buf[off + 30], buf[off + 31],
    ]
}

/// The body-checksum gate: `blake3(buf[..SB_BODY]) == buf[SB_BODY..SB_BODY+32]`.
/// `external_body` because blake3 is interpreted hashing — out of verification
/// scope (Kani stubbed it with `-Z stubbing` for the same reason). Assumed
/// **total**: it inspects the buffer and returns a bool, never panics. Totality
/// needs no collision-freedom (a deterministic total function suffices); a
/// round-trip proof would instead axiomatize injectivity here. The
/// `buf@.len() == SB_SIZE` precondition keeps the internal slicing in bounds.
#[verifier::external_body]
fn checksum_ok(buf: &[u8]) -> bool
    requires
        buf@.len() == SB_SIZE,
{
    let sum = Hash::of(&buf[..SB_BODY]);
    &buf[SB_BODY..SB_BODY + 32] == sum.as_bytes()
}

/// Parse the integer/byte fields of a superblock slot, or reject. **Total ∀**
/// buffer bytes — verifying this *is* the totality theorem (Verus proves every
/// fixed-offset read in bounds and no arithmetic overflow for all inputs), the
/// unbounded form of `check_superblock_decode_total`. The `Hash`/`Superblock`
/// assembly is the caller's trivially-total job (`Superblock::decode_checked`).
pub fn decode_checked_fields(buf: &[u8]) -> (r: Result<RawSuperblock, SbError>) {
    broadcast use vstd::slice::group_slice_axioms;
    if buf.len() != SB_SIZE {
        return Err(SbError::Invalid);
    }
    if !magic_ok(buf) {
        return Err(SbError::Invalid);
    }
    if !checksum_ok(buf) {
        return Err(SbError::Invalid);
    }
    // After the checksum: a torn old-format slot is just torn. (Every version
    // so far shares this layout and checksum extent; a future layout change
    // must move this check ahead of the checksum.)
    let version = read_u32_le(buf, 8);
    if version != SB_VERSION {
        return Err(SbError::WrongVersion(version));
    }
    Ok(RawSuperblock {
        generation: read_u64_le(buf, 16),
        ref_table: read_arr32(buf, 24),
        wal_head: read_u64_le(buf, 56),
        wal_next_seq: read_u64_le(buf, 64),
        wal_len: read_u64_le(buf, 72),
        chunk_tail: read_u64_le(buf, 80),
        index_off: read_u64_le(buf, 88),
    })
}

} // verus!

// ── Chunk index object (§4.2 items 3–4, durable since format v2) ───────

const INDEX_MAGIC: &[u8; 4] = b"CIDX";

/// One indexed object: `off` is the *data* offset within the chunk region
/// (the frame header sits CHUNK_HEADER bytes before it). `birth` is the
/// superblock generation the object was appended under — the GC epoch
/// hook (§4.2, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub off: u64,
    pub len: u32,
    pub birth: u64,
}

/// Exact payload size for given table sizes (`pad` excluded). The store
/// sizes the frame's extent with this *before* carving the extent out of
/// the free list it is about to serialize; the explicit trailing pad
/// then absorbs the (bounded) estimation slack, so extents fit exactly
/// and nothing leaks.
pub fn index_payload_len(entries: usize, free_extents: usize) -> usize {
    4 + 8 + entries * 52 + 8 + free_extents * 16 + 4
}

/// Serialize the index map + free-extent list. Both maps iterate in key
/// order, so the encoding is deterministic.
pub fn encode_index(
    entries: &BTreeMap<Hash, IndexEntry>,
    free: &BTreeMap<u64, u64>,
    pad: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(index_payload_len(entries.len(), free.len()) + pad);
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (hash, e) in entries {
        out.extend_from_slice(hash.as_bytes());
        out.extend_from_slice(&e.off.to_le_bytes());
        out.extend_from_slice(&e.len.to_le_bytes());
        out.extend_from_slice(&e.birth.to_le_bytes());
    }
    out.extend_from_slice(&(free.len() as u64).to_le_bytes());
    for (&off, &len) in free {
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(&(pad as u32).to_le_bytes());
    out.resize(out.len() + pad, 0);
    debug_assert_eq!(out.len(), index_payload_len(entries.len(), free.len()) + pad);
    out
}

#[allow(clippy::type_complexity)]
pub fn decode_index(
    buf: &[u8],
) -> Result<(BTreeMap<Hash, IndexEntry>, BTreeMap<u64, u64>), FormatError> {
    let mut r = Reader { buf, pos: 0 };
    if r.take(4)? != INDEX_MAGIC {
        return Err(FormatError::BadNode("not a chunk index"));
    }
    let n = r.u64()?;
    let mut entries = BTreeMap::new();
    for _ in 0..n {
        let hash = r.hash()?;
        let off = r.u64()?;
        let len = r.u32()?;
        let birth = r.u64()?;
        entries.insert(hash, IndexEntry { off, len, birth });
    }
    let nf = r.u64()?;
    let mut free = BTreeMap::new();
    for _ in 0..nf {
        let off = r.u64()?;
        let len = r.u64()?;
        free.insert(off, len);
    }
    let pad = r.u32()? as usize;
    if r.take(pad)?.iter().any(|&b| b != 0) {
        return Err(FormatError::BadNode("nonzero index padding"));
    }
    if !r.done() {
        return Err(FormatError::BadNode("index trailing bytes"));
    }
    Ok((entries, free))
}

// ── WAL records ─────────────────────────────────────────────────────────

pub(crate) const WAL_MAGIC: &[u8; 4] = b"WREC";
// WAL_HEADER is declared inside the `verus!{}` block above (8c: the verified
// `decode_frame`/`replay_bound` in store.rs name its concrete value); it erases
// to the same `pub const WAL_HEADER: usize = 48`.

/// A logged mutation. Replay must be deterministic, so server-assigned
/// values (mtime) are captured in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOp {
    Write {
        ref_name: Vec<u8>,
        path: Vec<Vec<u8>>,
        offset: u64,
        mtime: u64,
        data: Vec<u8>,
    },
    Unlink {
        ref_name: Vec<u8>,
        path: Vec<Vec<u8>>,
        mtime: u64,
    },
}

impl WalOp {
    pub fn ref_name(&self) -> &[u8] {
        match self {
            WalOp::Write { ref_name, .. } | WalOp::Unlink { ref_name, .. } => ref_name,
        }
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let put_path = |out: &mut Vec<u8>, path: &[Vec<u8>]| {
            out.push(path.len() as u8);
            for comp in path {
                out.push(comp.len() as u8);
                out.extend_from_slice(comp);
            }
        };
        match self {
            WalOp::Write { ref_name, path, offset, mtime, data } => {
                out.push(1);
                out.push(ref_name.len() as u8);
                out.extend_from_slice(ref_name);
                put_path(&mut out, path);
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&mtime.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(data);
            }
            WalOp::Unlink { ref_name, path, mtime } => {
                out.push(2);
                out.push(ref_name.len() as u8);
                out.extend_from_slice(ref_name);
                put_path(&mut out, path);
                out.extend_from_slice(&mtime.to_le_bytes());
            }
        }
        out
    }

    fn decode_payload(buf: &[u8]) -> Result<WalOp, FormatError> {
        let mut r = Reader { buf, pos: 0 };
        let take_path = |r: &mut Reader| -> Result<Vec<Vec<u8>>, FormatError> {
            let n = r.u8()? as usize;
            let mut path = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.u8()? as usize;
                path.push(r.take(len)?.to_vec());
            }
            Ok(path)
        };
        let op = match r.u8()? {
            1 => {
                let rl = r.u8()? as usize;
                let ref_name = r.take(rl)?.to_vec();
                let path = take_path(&mut r)?;
                let offset = r.u64()?;
                let mtime = r.u64()?;
                let dl = r.u32()? as usize;
                let data = r.take(dl)?.to_vec();
                WalOp::Write { ref_name, path, offset, mtime, data }
            }
            2 => {
                let rl = r.u8()? as usize;
                let ref_name = r.take(rl)?.to_vec();
                let path = take_path(&mut r)?;
                let mtime = r.u64()?;
                WalOp::Unlink { ref_name, path, mtime }
            }
            _ => return Err(FormatError::BadNode("bad wal op tag")),
        };
        if !r.done() {
            return Err(FormatError::BadNode("wal op trailing bytes"));
        }
        Ok(op)
    }

    /// Full record: header (magic, seq, len, checksum-of-payload) + payload.
    pub fn encode_record(&self, seq: u64) -> Vec<u8> {
        let payload = self.encode_payload();
        let mut out = Vec::with_capacity(WAL_HEADER + payload.len());
        out.extend_from_slice(WAL_MAGIC);
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(Hash::of(&payload).as_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Parse one record at the start of `buf`. Returns (seq, op, record
    /// length). None = no valid record here (torn tail or end of log —
    /// either way replay stops, §4.5: such records were never acked).
    pub fn decode_record(buf: &[u8]) -> Option<(u64, WalOp, usize)> {
        if buf.len() < WAL_HEADER || &buf[0..4] != WAL_MAGIC {
            return None;
        }
        let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap());
        let len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        if buf.len() < WAL_HEADER + len {
            return None;
        }
        let payload = &buf[WAL_HEADER..WAL_HEADER + len];
        if Hash::of(payload).as_bytes() != &buf[16..48] {
            return None;
        }
        let op = WalOp::decode_payload(payload).ok()?;
        Some((seq, op, WAL_HEADER + len))
    }
}

// ── Ref table (§4.1, §4.7) ──────────────────────────────────────────────

const REFT_MAGIC: &[u8; 4] = b"REFT";

pub const CLASS_KEEP: u8 = 0;
pub const CLASS_AUTO: u8 = 1;
pub const CLASS_EPHEMERAL: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub root: Hash,
    /// Storage-cap revocation generation (§2.2) — not the superblock
    /// generation. Bumping it lazily invalidates all outstanding handles.
    pub generation: u64,
    pub next_snap_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapRow {
    pub id: u64,
    pub root: Hash,
    /// UTC nanoseconds since the Unix epoch (format v3; the spec-level
    /// representation is signed 64-bit, §2.6 — positive in practice, so
    /// the u64 carries identical bytes). Server-assigned, strictly
    /// increasing per ref (§4.7).
    pub timestamp: u64,
    pub provenance: Vec<u8>,
    pub parent: Option<u64>,
    pub message: Vec<u8>,
    pub class: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefTable {
    pub refs: BTreeMap<Vec<u8>, RefEntry>,
    /// (ref name, snapshot id) → row. Snapshot identity is the per-ref
    /// sequence number, never a hash (§4.7).
    pub snaps: BTreeMap<(Vec<u8>, u64), SnapRow>,
    pub tags: BTreeMap<Vec<u8>, (Vec<u8>, u64)>,
}

impl RefTable {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(REFT_MAGIC);
        out.extend_from_slice(&(self.refs.len() as u32).to_le_bytes());
        for (name, e) in &self.refs {
            out.push(name.len() as u8);
            out.extend_from_slice(name);
            out.extend_from_slice(e.root.as_bytes());
            out.extend_from_slice(&e.generation.to_le_bytes());
            out.extend_from_slice(&e.next_snap_id.to_le_bytes());
        }
        out.extend_from_slice(&(self.snaps.len() as u32).to_le_bytes());
        for ((ref_name, _), s) in &self.snaps {
            out.push(ref_name.len() as u8);
            out.extend_from_slice(ref_name);
            out.extend_from_slice(&s.id.to_le_bytes());
            out.extend_from_slice(s.root.as_bytes());
            out.extend_from_slice(&s.timestamp.to_le_bytes());
            out.push(s.provenance.len() as u8);
            out.extend_from_slice(&s.provenance);
            out.extend_from_slice(&s.parent.unwrap_or(u64::MAX).to_le_bytes());
            out.extend_from_slice(&(s.message.len() as u16).to_le_bytes());
            out.extend_from_slice(&s.message);
            out.push(s.class);
        }
        out.extend_from_slice(&(self.tags.len() as u32).to_le_bytes());
        for (name, (ref_name, snap_id)) in &self.tags {
            out.push(name.len() as u8);
            out.extend_from_slice(name);
            out.push(ref_name.len() as u8);
            out.extend_from_slice(ref_name);
            out.extend_from_slice(&snap_id.to_le_bytes());
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<RefTable, FormatError> {
        let mut r = Reader { buf, pos: 0 };
        if r.take(4)? != REFT_MAGIC {
            return Err(FormatError::BadNode("not a ref table"));
        }
        let mut t = RefTable::default();
        let nrefs = r.u32()?;
        for _ in 0..nrefs {
            let nl = r.u8()? as usize;
            let name = r.take(nl)?.to_vec();
            let root = r.hash()?;
            let generation = r.u64()?;
            let next_snap_id = r.u64()?;
            t.refs.insert(name, RefEntry { root, generation, next_snap_id });
        }
        let nsnaps = r.u32()?;
        for _ in 0..nsnaps {
            let rl = r.u8()? as usize;
            let ref_name = r.take(rl)?.to_vec();
            let id = r.u64()?;
            let root = r.hash()?;
            let timestamp = r.u64()?;
            let pl = r.u8()? as usize;
            let provenance = r.take(pl)?.to_vec();
            let parent_raw = r.u64()?;
            let ml = r.u16()? as usize;
            let message = r.take(ml)?.to_vec();
            let class = r.u8()?;
            if class > CLASS_EPHEMERAL {
                return Err(FormatError::BadNode("bad retention class"));
            }
            t.snaps.insert(
                (ref_name, id),
                SnapRow {
                    id,
                    root,
                    timestamp,
                    provenance,
                    parent: (parent_raw != u64::MAX).then_some(parent_raw),
                    message,
                    class,
                },
            );
        }
        let ntags = r.u32()?;
        for _ in 0..ntags {
            let nl = r.u8()? as usize;
            let name = r.take(nl)?.to_vec();
            let rl = r.u8()? as usize;
            let ref_name = r.take(rl)?.to_vec();
            let snap_id = r.u64()?;
            t.tags.insert(name, (ref_name, snap_id));
        }
        if !r.done() {
            return Err(FormatError::BadNode("ref table trailing bytes"));
        }
        Ok(t)
    }
}

// ── Chunk frames ────────────────────────────────────────────────────────

pub const CHUNK_MAGIC: &[u8; 4] = b"CHNK";
// CHUNK_HEADER is declared inside the `verus!{}` block (the geometry spec names
// it); it erases to the same `pub const CHUNK_HEADER: usize = 48`.

/// Frame a chunk for the append-only store: magic, length, birth
/// generation (§4.2 — the GC epoch hook), content hash, data.
pub fn encode_chunk_frame(data: &[u8], birth_gen: u64, hash: &Hash) -> Vec<u8> {
    let mut out = Vec::with_capacity(CHUNK_HEADER + data.len());
    out.extend_from_slice(CHUNK_MAGIC);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&birth_gen.to_le_bytes());
    out.extend_from_slice(hash.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Parse a frame header. Returns (data length, birth gen, hash).
pub fn decode_chunk_header(buf: &[u8]) -> Option<(usize, u64, Hash)> {
    if buf.len() < CHUNK_HEADER || &buf[0..4] != CHUNK_MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let birth = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let hash = Hash::from_bytes(buf[16..48].try_into().unwrap());
    Some((len, birth, hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_roundtrip_and_tearing() {
        let sb = Superblock {
            generation: 7,
            ref_table: Hash::of(b"rt"),
            wal_head: 100,
            wal_next_seq: 42,
            wal_len: 65536,
            chunk_tail: 9999,
            index_off: 4242,
        };
        let buf = sb.encode();
        assert_eq!(Superblock::decode(&buf), Some(sb));
        // Any single-byte corruption in the body must invalidate it.
        for i in [0usize, 17, 30, 60, 70, 85, 92, 110] {
            let mut torn = buf;
            torn[i] ^= 0xFF;
            assert_eq!(Superblock::decode(&torn), None, "byte {i}");
        }
    }

    #[test]
    fn index_roundtrip_and_strictness() {
        let mut entries = BTreeMap::new();
        entries.insert(Hash::of(b"a"), IndexEntry { off: 48, len: 100, birth: 1 });
        entries.insert(Hash::of(b"b"), IndexEntry { off: 196, len: 7, birth: 3 });
        let mut free = BTreeMap::new();
        free.insert(300u64, 64u64);
        for pad in [0usize, 17] {
            let enc = encode_index(&entries, &free, pad);
            assert_eq!(enc.len(), index_payload_len(entries.len(), free.len()) + pad);
            assert_eq!(decode_index(&enc), Ok((entries.clone(), free.clone())));

            assert!(decode_index(&enc[..enc.len() - 1]).is_err());
            let mut extra = enc.clone();
            extra.push(0);
            assert!(decode_index(&extra).is_err());
        }
        // Pad bytes must be zero — one encoding per logical index.
        let mut enc = encode_index(&entries, &free, 8);
        let n = enc.len();
        enc[n - 3] = 1;
        assert!(decode_index(&enc).is_err());
        assert!(decode_index(b"XIDX").is_err());
    }

    #[test]
    fn wal_record_roundtrip_and_torn_tail() {
        let op = WalOp::Write {
            ref_name: b"main".to_vec(),
            path: vec![b"etc".to_vec(), b"conf".to_vec()],
            offset: 512,
            mtime: 1234,
            data: b"hello".to_vec(),
        };
        let rec = op.encode_record(9);
        let (seq, parsed, len) = WalOp::decode_record(&rec).unwrap();
        assert_eq!((seq, len), (9, rec.len()));
        assert_eq!(parsed, op);
        // Truncated and corrupted records are silently rejected.
        assert!(WalOp::decode_record(&rec[..rec.len() - 1]).is_none());
        let mut bad = rec.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(WalOp::decode_record(&bad).is_none());
    }

    #[test]
    fn ref_table_roundtrip() {
        let mut t = RefTable::default();
        t.refs.insert(
            b"main".to_vec(),
            RefEntry { root: Hash::of(b"r"), generation: 3, next_snap_id: 2 },
        );
        t.snaps.insert(
            (b"main".to_vec(), 1),
            SnapRow {
                id: 1,
                root: Hash::of(b"s"),
                timestamp: 99,
                provenance: b"session=test".to_vec(),
                parent: None,
                message: b"first".to_vec(),
                class: CLASS_KEEP,
            },
        );
        t.tags.insert(b"v1".to_vec(), (b"main".to_vec(), 1));
        let enc = t.encode();
        assert_eq!(RefTable::decode(&enc), Ok(t));
    }
}
