//! The storage engine (rev0§4.3-4.7): memtable + WAL + flush + the A/B
//! superblock commit, with crash recovery and GC. This is the code the
//! CommitProtocol TLA+ model models; the crash-injection proptest at the
//! bottom checks the model's headline invariant against the real bytes:
//! after any crash, every acknowledged write is recoverable from durable
//! state alone.
//!
//! Commit is always: fsync chunks (barrier 1) → write new superblock to
//! the older slot → fsync (barrier 2). Nothing is freed on the write
//! path, ever; reclamation is GC's job exclusively (rev0§4.6): `gc` marks
//! from the committed root set and sweeps by *removing index entries* —
//! a pure metadata edit that commits through the ordinary superblock
//! flip, so a crash anywhere inside GC recovers the previous commit with
//! nothing lost. Freed extents become allocatable only after the sweep
//! commit lands (the same rule that forbids overwriting the latest
//! superblock); until then the durable index still lists the condemned
//! chunks, and reusing their extents early would let a dedup hit on the
//! old index resurrect overwritten bytes after a crash.
//!
//! MVP simplifications, recorded:
//!   - Flush rebuilds whole dirty files instead of re-chunking only the
//!     affected neighborhood (rev0§4.3 step 3) — a perf optimization with no
//!     semantic difference; owed when file sizes warrant it.
//!   - The WAL is linear, not circular: when full, everything is flushed
//!     and committed and the log resets to offset 0 (head can only ever
//!     advance past flushed records, so the rev0§4.4 invariant holds; the
//!     flush-the-pinner scheduler arrives with real multi-ref traffic).
//!   - Oversized writes (record > WAL region) bypass the WAL and commit
//!     synchronously before acknowledging — same durability contract.
//!   - The allocator is first-fit over a flat extent list and the tail
//!     high-water mark never retracts (freed space is reusable, but the
//!     region never visibly shrinks). Fine at MVP scale.

use crate::chunk::ChunkerParams;
use crate::dev::{BlockDev, DevError};
use crate::disk::{
    self, read_u32_le, read_u64_le, IndexEntry, RefEntry, RefTable, SnapRow, Superblock, WalOp,
    CHUNK_HEADER, SB_A_OFF, SB_B_OFF, SB_SIZE, WAL_HEADER, WAL_OFF,
};
use crate::file::{make_file_entry, read_file};
use crate::gc;
use crate::hash::Hash;
use crate::overlay::{FileState, Overlay, Path};
use crate::prolly::{Content, Dir, Entry, EntryKind, FormatError, NodeStore};
use crate::tree;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec;
use alloc::vec::Vec;
use vstd::prelude::*;

#[derive(Debug)]
pub enum StoreError {
    Io(DevError),
    Format(FormatError),
    NoSuperblock,
    /// An intact superblock from another format version. Old images are
    /// re-created with mkfs, never migrated or reinterpreted (rev0§2.6).
    UnsupportedVersion(u32),
    NoSuchRef,
    NoSuchSnapshot,
    NotAFile,
    Corrupt(&'static str),
    NoSpace,
    /// The snapshot is a tag target; tags are keep-strength pins (rev0§4.7).
    Pinned,
    /// Write extent overflows u64 or exceeds the chunk region capacity.
    WriteOutOfRange,
}

impl From<DevError> for StoreError {
    fn from(e: DevError) -> Self {
        StoreError::Io(e)
    }
}

impl From<FormatError> for StoreError {
    fn from(e: FormatError) -> Self {
        StoreError::Format(e)
    }
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "io: {e}"),
            StoreError::Format(e) => write!(f, "format: {e}"),
            StoreError::NoSuperblock => write!(f, "no valid superblock"),
            StoreError::UnsupportedVersion(v) => {
                write!(f, "unsupported format version {v} (re-create the image with mkfs)")
            }
            StoreError::NoSuchRef => write!(f, "no such ref"),
            StoreError::NoSuchSnapshot => write!(f, "no such snapshot"),
            StoreError::NotAFile => write!(f, "not a file"),
            StoreError::Corrupt(w) => write!(f, "corrupt store: {w}"),
            StoreError::NoSpace => write!(f, "chunk region full"),
            StoreError::Pinned => write!(f, "snapshot pinned by a tag"),
            StoreError::WriteOutOfRange => write!(f, "write extent out of range"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for StoreError {}

#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    pub wal_len: u64,
    pub chunker: ChunkerParams,
    /// Global dirty-overlay budget (rev0§4.4); exceeding it forces sync.
    pub overlay_budget: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        StoreOptions {
            wal_len: 16 * 1024 * 1024,
            chunker: ChunkerParams::DEFAULT,
            overlay_budget: 8 * 1024 * 1024,
        }
    }
}

// ── Chunk store ─────────────────────────────────────────────────────────

struct ChunkStore<D: BlockDev> {
    dev: D,
    chunk_off: u64,
    /// High-water mark: everything at/after `tail` has never been
    /// allocated. Space below `tail` is governed by `index` and `free`.
    tail: u64,
    birth_gen: u64,
    index: BTreeMap<Hash, IndexEntry>,
    /// Committed-free extents (frame offset → byte length), allocatable.
    free: BTreeMap<u64, u64>,
    /// Extents freed by uncommitted state (GC sweep, superseded index
    /// frames). They join `free` only after the next superblock flip:
    /// the current durable commit still references them.
    pending_free: Vec<(u64, u64)>,
    /// Extent of the index frame the *current durable* superblock points
    /// at; freed (via `pending_free`) when the next commit supersedes it.
    index_extent: (u64, u64),
    io_error: Option<DevError>,
}

impl<D: BlockDev> ChunkStore<D> {
    fn region_len(&self) -> u64 {
        self.dev.len() - self.chunk_off
    }

    /// First-fit from committed-free extents, else bump the tail.
    fn alloc(&mut self, need: u64) -> Option<u64> {
        let found = self
            .free
            .iter()
            .find(|(_, &len)| len >= need)
            .map(|(&off, &len)| (off, len));
        if let Some((off, len)) = found {
            self.free.remove(&off);
            if len > need {
                self.free.insert(off + need, len - need);
            }
            return Some(off);
        }
        if self.tail + need <= self.region_len() {
            let off = self.tail;
            self.tail += need;
            return Some(off);
        }
        None
    }

    /// `free` ∪ `pending_free`, adjacent extents merged — the free list
    /// as the next commit will record it.
    fn merged_free(&self) -> BTreeMap<u64, u64> {
        let mut all: Vec<(u64, u64)> = self.free.iter().map(|(&o, &l)| (o, l)).collect();
        all.extend_from_slice(&self.pending_free);
        all.sort_unstable();
        let mut merged: BTreeMap<u64, u64> = BTreeMap::new();
        let mut cur: Option<(u64, u64)> = None;
        for (off, len) in all {
            match cur {
                Some((coff, clen)) if coff + clen == off => cur = Some((coff, clen + len)),
                Some((coff, clen)) => {
                    merged.insert(coff, clen);
                    cur = Some((off, len));
                }
                None => cur = Some((off, len)),
            }
        }
        if let Some((coff, clen)) = cur {
            merged.insert(coff, clen);
        }
        merged
    }

    fn free_bytes(&self) -> u64 {
        (self.region_len() - self.tail)
            + self.free.values().sum::<u64>()
            + self.pending_free.iter().map(|&(_, l)| l).sum::<u64>()
    }

    /// Serialize the index + free list and write the frame into space
    /// that is free in *both* the current durable commit and the new one:
    /// a committed-free extent (then carved out of the list the frame
    /// itself records) or the tail. Never a `pending_free` extent — the
    /// durable commit, including the index frame it points at, must stay
    /// fully intact until barrier 2.
    ///
    /// Sizing knot: the frame records the free list, but carving the
    /// frame's own extent reshapes that list. Resolved with an upper
    /// bound — merging only ever coalesces extents and the carve splits
    /// at most one in two — and explicit padding to make the frame fill
    /// its extent exactly.
    ///
    /// Returns the frame's extent and the free list as recorded in it
    /// (live after the flip); the caller commits via the superblock.
    fn write_index_frame(&mut self) -> Result<((u64, u64), BTreeMap<u64, u64>), StoreError> {
        let bound_extents = self.free.len() + self.pending_free.len() + 1;
        let need =
            (CHUNK_HEADER + disk::index_payload_len(self.index.len(), bound_extents)) as u64;
        let chosen = self
            .free
            .iter()
            .find(|(_, &len)| len >= need)
            .map(|(&off, &len)| (off, len));
        let off = match chosen {
            Some((off, len)) => {
                self.free.remove(&off);
                if len > need {
                    self.free.insert(off + need, len - need);
                }
                off
            }
            None => {
                if self.tail + need > self.region_len() {
                    return Err(StoreError::NoSpace);
                }
                let off = self.tail;
                self.tail += need;
                off
            }
        };
        let new_free = self.merged_free();
        let body = disk::index_payload_len(self.index.len(), new_free.len());
        let pad = need as usize - CHUNK_HEADER - body;
        let payload = disk::encode_index(&self.index, &new_free, pad);
        let hash = Hash::of(&payload);
        let frame = disk::encode_chunk_frame(&payload, self.birth_gen, &hash);
        debug_assert_eq!(frame.len() as u64, need);
        self.dev.write(self.chunk_off + off, &frame)?;
        Ok(((off, need), new_free))
    }

    fn read_object(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(&IndexEntry { off, len, .. }) = self.index.get(hash) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len as usize];
        self.dev.read(self.chunk_off + off, &mut buf)?;
        // Every layer self-verifies (rev0§4.8).
        if Hash::of(&buf) != *hash {
            return Err(StoreError::Corrupt("chunk hash mismatch"));
        }
        Ok(Some(buf))
    }
}

/// NodeStore is infallible by signature; the chunk store records I/O
/// errors out of band and the engine surfaces them after each operation.
impl<D: BlockDev> NodeStore for ChunkStore<D> {
    fn put(&mut self, bytes: &[u8]) -> Hash {
        let hash = Hash::of(bytes);
        if self.index.contains_key(&hash) {
            // Dedup (rev0§4.3). The rev0§4.6 resurrection hazard (an index hit on
            // a chunk the marker has condemned) cannot arise: GC here is
            // synchronous, and the sweep removes condemned entries before
            // any subsequent put, so a re-put of condemned content is an
            // index miss and rewrites the chunk.
            return hash;
        }
        let frame = disk::encode_chunk_frame(bytes, self.birth_gen, &hash);
        let Some(off) = self.alloc(frame.len() as u64) else {
            self.io_error = Some(DevError::Io("chunk region full"));
            return hash;
        };
        if let Err(e) = self.dev.write(self.chunk_off + off, &frame) {
            self.io_error = Some(e);
            return hash;
        }
        self.index.insert(
            hash,
            IndexEntry {
                off: off + CHUNK_HEADER as u64,
                len: bytes.len() as u32,
                birth: self.birth_gen,
            },
        );
        hash
    }

    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.read_object(hash).ok().flatten()
    }
}

// ── The engine ──────────────────────────────────────────────────────────

// ── The recovery decision core (rev0§4.8) ─────────────────────────────────
//
// The pure recovery/commit *decisions* extracted from `mount`/`commit` and
// proven faithful to the CommitProtocol ∀ inputs — Verus closing the model-to-
// code gap that TLA+ (design) and the crash-injection proptest (sampled bytes)
// leave open. Additive: TLA+ stays the design gate, the proptest the
// differential seam. The surrounding I/O (the BlockDev reads/writes, the two
// fsync barriers, the chunk store, the prolly tree) stays plain Rust outside
// the proof surface — a Hash-free verified core with thin plain-Rust callers.
// `Survivor`/`Slot` are in-block enums because an external enum can't be
// *constructed* inside `verus!{}`; `mount`/`commit` map them back to the
// existing control flow.
verus! {

/// Which superblock slot won recovery — the verified form of the `match decoded`
/// arms in `Store::mount` (rev0§4.5), one variant per arm of the original control
/// flow.
pub enum Survivor {
    SlotA,
    SlotB,
    Neither,
}

/// Pick the live superblock slot: the valid slot of higher generation (the TLA+
/// `LiveSlot` / `OlderIsA`). **Total** ∀ `(gen, valid)`. Faithful to mount's
/// `a.generation >= b.generation` tie-break (slot A wins a tie). Under distinct
/// generations — every honest commit bumps `generation` and writes the *other*
/// slot (the TLA+ `GenerationsDistinct`), so two valid slots never share one —
/// the `>=` is a strict `>`, making the choice deterministic.
pub fn pick_survivor(gen_a: u64, valid_a: bool, gen_b: u64, valid_b: bool) -> (r: Survivor)
    ensures
        (!valid_a && !valid_b) ==> r is Neither,
        (valid_a && !valid_b) ==> r is SlotA,
        (!valid_a && valid_b) ==> r is SlotB,
        (valid_a && valid_b) ==> ((r is SlotA) <==> gen_a >= gen_b),
        // A chosen slot is always a valid one — what justifies mount's `unwrap`.
        (r is SlotA) ==> valid_a,
        (r is SlotB) ==> valid_b,
{
    if valid_a && valid_b {
        if gen_a >= gen_b {
            Survivor::SlotA
        } else {
            Survivor::SlotB
        }
    } else if valid_a {
        Survivor::SlotA
    } else if valid_b {
        Survivor::SlotB
    } else {
        Survivor::Neither
    }
}

/// Which superblock slot the next commit writes (the A/B alternation, rev0§4.2).
pub enum Slot {
    A,
    B,
}

/// The currently-live slot (ghost model of `Store::sb_in_b`): B iff `sb_in_b`.
pub open spec fn live_slot(sb_in_b: bool) -> Slot {
    if sb_in_b {
        Slot::B
    } else {
        Slot::A
    }
}

/// Which slot the next commit writes: always the **non-live** slot, so a crash
/// mid-write damages only the slot being written and the last committed slot
/// survives — the code witness of the TLA+ `Crash` three-outcome safety
/// (`AtLeastOneValidSlot` preserved by construction). **Total**.
pub fn commit_target(sb_in_b: bool) -> (r: Slot)
    ensures
        (r is A) <==> sb_in_b,
        r != live_slot(sb_in_b),
{
    if sb_in_b {
        Slot::A
    } else {
        Slot::B
    }
}

/// One WAL record's commit-relevant metadata: its sequence number, its byte
/// offset in the WAL region, and whether its effects have been flushed into
/// immutable tree (so the commit head may advance past it). `ref_name` is the
/// owning ref (matched in `flush_ref`); the head-advance never reasons about
/// it. Lives in the `verus!{}` block so `advance_head` can name its
/// fields — it erases to a plain struct, so the plain-Rust `wal_records`
/// machinery is unchanged.
struct RecMeta {
    seq: u64,
    off: u64,
    ref_name: Vec<u8>,
    flushed: bool,
}

/// The head-advance decision: pop `n_flushed` records off the WAL queue front,
/// then the new superblock `(wal_head, wal_next_seq)` are `head`/`next_seq`.
struct HeadAdvance {
    n_flushed: usize,
    head: u64,
    next_seq: u64,
}

/// Advance the commit head past the contiguous flushed prefix of the WAL record
/// queue (the TLA+ `CommitPrepare.newHead` — "longest contiguous prefix of
/// records whose effects are flushed"). The new head/seq is read off the first
/// non-flushed record, or the linear-WAL reset sentinel `(0, wal_seq)` when the
/// log drains (rev0§4.4). **Total** ∀ records, **terminating**. Pure sequence
/// reasoning — the prefix-scan kcore already did for the channel FIFO head.
///
/// Out of scope here (a Store-level invariant, not a property of this pure
/// function): cross-commit head monotonicity (`new head >= old head`) rests on
/// WAL offsets strictly increasing by construction in `log_then_apply`, which
/// the plain-Rust call site can't hand Verus as a precondition. The per-piece
/// contract below is precondition-free and justifies the extraction; the
/// monotone fact is left to the composition where the invariant is in scope
/// (per-piece contracts before the composed theorem).
fn advance_head(records: &[RecMeta], wal_seq: u64) -> (r: HeadAdvance)
    ensures
        r.n_flushed <= records@.len(),
        // Everything popped is flushed (the contiguous flushed prefix).
        forall|j: int| #![trigger records@[j]] 0 <= j < r.n_flushed ==> records@[j].flushed,
        // The head record, if any, is the first non-flushed one.
        r.n_flushed < records@.len() ==> !records@[r.n_flushed as int].flushed,
        // The head/seq is read off that first non-flushed record...
        r.n_flushed < records@.len() ==>
            r.head == records@[r.n_flushed as int].off
            && r.next_seq == records@[r.n_flushed as int].seq,
        // ...or the linear-WAL reset sentinel when all flushed.
        r.n_flushed == records@.len() ==> r.head == 0 && r.next_seq == wal_seq,
{
    broadcast use vstd::slice::group_slice_axioms;
    let mut i: usize = 0;
    while i < records.len() && records[i].flushed
        invariant
            i <= records@.len(),
            forall|j: int| #![trigger records@[j]] 0 <= j < i ==> records@[j].flushed,
        decreases records@.len() - i,
    {
        i += 1;
    }
    if i < records.len() {
        HeadAdvance { n_flushed: i, head: records[i].off, next_seq: records[i].seq }
    } else {
        HeadAdvance { n_flushed: i, head: 0, next_seq: wal_seq }
    }
}

// ── Replay bound (rev0§4.8) ───────────────────────────────────────────────
//
// The recovery-path dual of `advance_head`: from the committed head, how much of
// the WAL is a valid recoverable run. `Store::mount` (rev0§4.5) reads contiguous,
// checksummed, seq-continuous records until the first torn or seq-discontinuous
// one (an unacked tail). The *bound* of that walk — the count of records and the
// resulting `(wal_tail, wal_seq)` — lives in a verified parser core, leaving the
// overlay apply + the content-level extent gate as the plain-Rust applier
// (`mount` re-walks the span): a verified parser, a plain-Rust applier.
//
// The framing parse (`decode_frame`) is **verified** — its in-bounds guarantee
// is what makes the walk terminate and stay in range, the unbounded form of an
// `off += rlen` in-bounds argument. The blake3 checksum and the `WalOp` payload
// decode stay the **content seam** (`wal_content_ok`, `external_body`) — the
// same boundary drawn for the superblock.

/// One WAL record's framing: its sequence number and total on-disk length. The
/// `Hash`-free verified analogue of `WalOp::decode_record`'s header parse.
struct RecFrame {
    seq: u64,
    rlen: usize,
}

/// Spec mirror of [`decode_frame`]: the framing parse of the WAL record at `off`
/// as a ghost `Option<(seq, rlen)>`, the unbounded handle the recovery-core spec
/// reasons through. `decode_frame`'s `ensures` proves the exec parse agrees with
/// it ∀ bytes; `run_len` (and the gap-freedom composition) build on it. `None` = no
/// in-bounds well-framed record here (short buffer, bad magic, or a length that
/// runs past the buffer — including the 32-bit `usize` overflow of `WAL_HEADER +
/// len`, which `decode_frame`'s `checked_add` rejects: such an `rlen` exceeds any
/// real `wal.len() <= usize::MAX`, so this clause already excludes it).
spec fn frame_at(wal: Seq<u8>, off: int) -> Option<(u64, nat)> {
    if off < 0 || wal.len() < off + WAL_HEADER {
        None
    } else if !(wal[off] == 0x57u8 && wal[off + 1] == 0x52u8 && wal[off + 2] == 0x45u8
        && wal[off + 3] == 0x43u8)
    {
        None
    } else {
        // `disk::spec_*_le` by path (not `use`d): a `spec fn` import would dangle
        // in the macro-erased plain build, but this reference erases with `frame_at`.
        let len = disk::spec_u32_le(wal, off + 12) as nat;
        if off + WAL_HEADER + len <= wal.len() {
            Some((disk::spec_u64_le(wal, off + 4), (WAL_HEADER + len) as nat))
        } else {
            None
        }
    }
}

/// Parse the fixed WAL record header at `wal[off..]` (magic, seq, payload len)
/// and bounds-check the whole record against `wal`. **Total ∀** bytes; the
/// `Some` arm carries the in-bounds + nonzero-length guarantee that makes the
/// replay walk terminate and stay in range. Mirrors the framing arms of
/// `WalOp::decode_record` (`disk.rs`): magic + `len` + `WAL_HEADER + len` in
/// bounds. Indexes `wal[off + k]` (the `disk.rs` byte-reader recipe) rather than
/// range-slicing, so the proof stays first-order. The second `ensures` ties the
/// exec parse to the ghost [`frame_at`] ∀ bytes, so `run_len` and the
/// composition can reason over the framing without re-deriving the byte reads.
fn decode_frame(wal: &[u8], off: usize) -> (r: Option<RecFrame>)
    requires
        off <= wal@.len(),
    ensures
        r matches Some(f) ==> WAL_HEADER <= f.rlen && off + f.rlen <= wal@.len(),
        match r {
            Some(f) => frame_at(wal@, off as int) == Some((f.seq, f.rlen as nat)),
            None => frame_at(wal@, off as int) is None,
        },
{
    broadcast use vstd::slice::group_slice_axioms;
    if wal.len() - off < WAL_HEADER {
        return None;
    }
    // WAL_MAGIC == b"WREC" == [0x57, 0x52, 0x45, 0x43]; per-byte so Verus reasons
    // over `wal[i]` rather than the unspecced slice `==` (the `magic_ok` recipe).
    if !(wal[off] == 0x57u8 && wal[off + 1] == 0x52u8 && wal[off + 2] == 0x45u8
        && wal[off + 3] == 0x43u8)
    {
        return None;
    }
    let seq = read_u64_le(wal, off + 4);
    let len = read_u32_le(wal, off + 12) as usize;
    match WAL_HEADER.checked_add(len) {
        Some(rlen) => {
            if rlen <= wal.len() - off {
                Some(RecFrame { seq, rlen })
            } else {
                None
            }
        }
        None => {
            // 32-bit `usize` overflow of `WAL_HEADER + len`: an `rlen` that big
            // exceeds any real `wal.len() <= usize::MAX`, so `frame_at`'s
            // in-bounds clause already rejects it (the two agree, ∀ arch).
            assert(wal@.len() <= usize::MAX);
            None
        }
    }
}

/// The content-layer acceptance `decode_record` makes after framing: the blake3
/// payload checksum AND that the payload decodes to a `WalOp`. `external_body`
/// because both are out of verification scope — blake3 is interpreted hashing
/// (the same seam as `checksum_ok`) and the `WalOp` payload is `Vec`-building
/// content (TLA+'s abstracted record value, not the replay-bound decision). Assumed
/// **total**: it inspects the exact-`rlen` record slice and returns a bool. The
/// `content_ok_spec` ghost lets the maximal-run characterization name the seam
/// (the standard trusted-fn-with-uninterpreted-spec idiom). The
/// `off + rlen <= len` precondition (from `decode_frame`) keeps the slice in bounds.
#[verifier::external_body]
fn wal_content_ok(wal: &[u8], off: usize, rlen: usize) -> (r: bool)
    requires
        off + rlen <= wal@.len(),
    ensures
        r == content_ok_spec(wal@.subrange(off as int, (off + rlen) as int)),
{
    WalOp::decode_record(&wal[off..off + rlen]).is_some()
}

/// Ghost model of [`wal_content_ok`] — uninterpreted (blake3 + the `WalOp`
/// payload decode are seams), so the maximal-run spec can reference "this record
/// is content-valid" without looking inside the hash or the content decode.
uninterp spec fn content_ok_spec(rec: Seq<u8>) -> bool;

/// The length of the maximal contiguous recoverable run from `off` at sequence
/// `seq`: each record must frame ([`frame_at`]), continue the sequence, and pass
/// the content seam (`content_ok_spec`); the run ends at the first that does not.
/// The record at `seq == u64::MAX` is *counted* but ends the run (the sequence
/// can't advance) — matching `replay_bound`/`mount`'s rev0§4.4 seq-exhaustion gate
/// (the `mnt1_forged_wal_seq_max_rejected` corner). This is the closed form
/// `replay_bound.count` is proven equal to, and the quantity the gap-freedom
/// composition reasons about. **Terminating**
/// (`decreases wal.len() - off`; each accepted record's `rlen >= WAL_HEADER > 0`,
/// from `frame_at`'s `Some` arm, strictly shrinks the remaining buffer).
spec fn run_len(wal: Seq<u8>, off: int, seq: u64) -> nat
    decreases wal.len() - off,
{
    if off < 0 || wal.len() <= off {
        0
    } else {
        match frame_at(wal, off) {
            None => 0,
            Some((s, rlen)) => {
                if s != seq {
                    0
                } else if !content_ok_spec(wal.subrange(off, off + rlen)) {
                    0
                } else if seq == u64::MAX {
                    1
                } else {
                    (1 + run_len(wal, off + rlen, (seq + 1) as u64)) as nat
                }
            }
        }
    }
}

/// How much of the WAL is a valid recoverable run from the committed head: the
/// number of records to replay (`count`) and the byte offset just past them
/// (`end_off`). `mount` re-walks `count` records itself (recomputing `wal_seq`
/// via its `checked_add` forgery gate), so the span need not carry `wal_seq`.
struct ReplaySpan {
    count: usize,
    end_off: u64,
}

/// Walk the WAL from `wal_head`, accepting contiguous records that frame cleanly
/// (`decode_frame`), pass the content seam (`wal_content_ok`), and continue the
/// sequence (`seq` match), stopping at the first torn or seq-discontinuous one —
/// the TLA+ `Recover` action.
///
/// Proven here: **totality** (the `end_off <= wal.len()` in-bounds postcondition,
/// ∀ bytes — the unbounded form of `mount`'s `off += rlen` in-bounds argument),
/// **termination** (`decreases wal.len() - off`; each accepted record advances
/// `off` by `rlen >= WAL_HEADER > 0`), and the tight **maximal-run
/// equality** `count == run_len(wal@, wal_head, wal_next_seq)`: the walk accepts
/// *exactly* the maximal contiguous seq-run (the closed-form characterization,
/// the quantity the gap-freedom composition reasons about). A record at `seq ==
/// u64::MAX` is still *counted* (so `mount`'s re-walk applies it and its
/// `checked_add` fires the rev0§4.4 seq-exhaustion forgery gate, the
/// `mnt1_forged_wal_seq_max_rejected` behaviour); the loop just can't advance past
/// it, so it stops there — `run_len` counts it the same way.
///
/// The proof is the accumulator invariant `count + run_len(wal@, off, seq) ==
/// run_len(wal@, wal_head, wal_next_seq)`: each accepted record unfolds `run_len`
/// once (`1 + run_len` at the next offset/seq), and each stop point leaves
/// `run_len(wal@, off, seq) == 0` — so `count` equals the total at every exit.
/// The extent gate stays the plain-Rust applier's job (it needs the decoded
/// `Write` content); `run_len` models the per-record acceptance through the
/// `content_ok_spec` seam, not the extent check.
fn replay_bound(wal: &[u8], wal_head: u64, wal_next_seq: u64) -> (r: ReplaySpan)
    requires
        wal_head <= wal@.len(),
    ensures
        r.end_off <= wal@.len(),
        r.count == run_len(wal@, wal_head as int, wal_next_seq),
{
    broadcast use vstd::slice::group_slice_axioms;
    let ghost total = run_len(wal@, wal_head as int, wal_next_seq);
    // Materializes `wal@.len() <= usize::MAX` (a real slice length fits usize), so
    // `wal_head <= wal.len()` makes the cast below value-preserving.
    let wlen = wal.len();
    let mut off: usize = wal_head as usize;
    let mut seq: u64 = wal_next_seq;
    let mut count: usize = 0;
    assert(off as int == wal_head as int);
    loop
        // The accumulator: what's counted plus what `run_len` still sees from the
        // current cursor equals the total run. Holds at every iteration top, but
        // *not* at the seq-exhaustion break (`count` is bumped past a record whose
        // `run_len` tail is not yet zero), so it is `invariant_except_break`; the
        // loop `ensures` states what does hold at every exit.
        invariant_except_break
            count + run_len(wal@, off as int, seq) == total,
        invariant
            wlen == wal@.len(),
            off <= wal@.len(),
            count <= off,
        ensures
            off <= wal@.len(),
            count == total,
        decreases wal@.len() - off,
    {
        if off >= wlen {
            // off == wal.len() (with off <= len), so the run from here is empty.
            assert(wal@.len() <= off as int);
            assert(run_len(wal@, off as int, seq) == 0);
            break;
        }
        let frame = match decode_frame(wal, off) {
            Some(f) => f,
            None => {
                // decode_frame: None ==> frame_at None ==> run_len here is 0.
                assert(run_len(wal@, off as int, seq) == 0);
                break;
            }
        };
        // decode_frame: frame_at(wal@, off) == Some((frame.seq, frame.rlen)).
        if frame.seq != seq {
            assert(run_len(wal@, off as int, seq) == 0);
            break;
        }
        if !wal_content_ok(wal, off, frame.rlen) {
            // wal_content_ok: !r ==> !content_ok_spec(subrange) ==> run_len is 0.
            assert(run_len(wal@, off as int, seq) == 0);
            break;
        }
        // Record accepted. Unfold run_len once at this (off, seq): the two
        // consequences carry the accumulator across the step (non-MAX) or pin
        // count == total at the seq-exhaustion stop (MAX). Captured here (pre-bump)
        // so the facts persist for the old (off, seq) after the mutations below.
        assert(seq == u64::MAX ==> run_len(wal@, off as int, seq) == 1);
        assert(seq != u64::MAX ==> run_len(wal@, off as int, seq)
            == 1 + run_len(wal@, (off + frame.rlen) as int, (seq + 1) as u64));
        // Accept this record (so `mount` replays it); count before the seq-exhaustion
        // stop so a planted record at `seq == u64::MAX` is replayed and trips
        // `mount`'s `checked_add` gate, not silently dropped (rev0§4.4 forgery gate).
        count = count + 1;
        off = off + frame.rlen;
        // An honest seq counter never nears u64::MAX (2^64 records). At the boundary
        // the loop can't advance the sequence, so it stops — having already counted
        // the record above. The accumulator at this iteration's top gave count_top +
        // 1 == total (run_len of a counted MAX record is 1), so count == total here.
        if seq == u64::MAX {
            assert(count == total);
            break;
        }
        seq = seq + 1;
    }
    ReplaySpan { count, end_off: off as u64 }
}

} // verus!

// ── The gap-freedom composition (rev0§4.8) ────────────────────────────────
//
// `advance_head` (write path) and `replay_bound` (recovery path) operate on
// different views — a `&[RecMeta]` queue vs. the raw WAL bytes. The composition
// theorem relates them through `laid_out`, the linking invariant that the byte
// region from a record's `off` *is* the record the queue describes (frames at
// `off` with that `seq`, content-valid, contiguous, seq-continuous). Under it the
// **gap-freedom lemma** holds: `advance_head`'s head sits at the first non-flushed
// record, and replay from that head covers the whole suffix — so every
// acked-but-uncommitted (unflushed) record is replayed. This is the code-level
// shadow of the TLA+ `AckedWritesRecoverable` (WAL-replay half); the
// content-coverage half — "flushed ⇒ effects already in the committed root" —
// stays the `CommitProtocol` design gate, deliberately out of scope here.
//
// `laid_out` is a documented invariant, not enforced at one site Verus sees
// (mount *builds* `records` by replaying; commit *consumes* the live queue), so
// `lemma_gap_freedom` takes `advance_head`/`replay_bound`'s `ensures` as
// hypotheses — they hold exactly when commit/mount call the two in sequence. This
// is the rev0§4.8 "per-piece contracts compose into the theorem" shape: the
// pieces are proven; the lemma joins them.

verus! {

/// The linking invariant for the record suffix from index `k`: the WAL byte
/// region at each record's `off` frames as exactly that record (`frame_at` Some
/// with the record's `seq`), is content-valid (`content_ok_spec`), has a sequence
/// below `u64::MAX` (an honest log never wraps the 64-bit counter), and is laid
/// out contiguously and seq-continuously into the next record. Recursive over the
/// suffix so the coverage induction is structural.
spec fn laid_out(wal: Seq<u8>, records: Seq<RecMeta>, k: int) -> bool
    decreases records.len() - k,
{
    if k < 0 || records.len() <= k {
        true
    } else {
        match frame_at(wal, records[k].off as int) {
            None => false,
            Some((s, rlen)) => {
                &&& s == records[k].seq
                &&& records[k].seq < u64::MAX
                &&& content_ok_spec(wal.subrange(records[k].off as int, records[k].off as int + rlen))
                &&& (k + 1 < records.len() ==> records[k + 1].off as int == records[k].off as int + rlen)
                &&& (k + 1 < records.len() ==> records[k + 1].seq == (records[k].seq + 1) as u64)
                &&& laid_out(wal, records, k + 1)
            }
        }
    }
}

/// `laid_out` from `k` carries to any later index `m` (the suffix of a laid-out
/// suffix is laid out — each unfold exposes `laid_out` at the next index).
proof fn lemma_laid_out_mono(wal: Seq<u8>, records: Seq<RecMeta>, k: int, m: int)
    requires
        0 <= k <= m <= records.len(),
        laid_out(wal, records, k),
    ensures
        laid_out(wal, records, m),
    decreases m - k,
{
    if k < m {
        lemma_laid_out_mono(wal, records, k + 1, m);
    }
}

/// **Coverage**: from a laid-out record `k`, `run_len` accepts at least the whole
/// remaining suffix — `>= records.len() - k`. Induction on the suffix: each record
/// frames, matches its sequence, passes the content seam, and has `seq < u64::MAX`
/// (so `run_len` takes its `1 + …` step rather than the seq-exhaustion stop),
/// chaining contiguously into the next via `laid_out`. The bound is a lower bound
/// (not equality) because the WAL may hold further valid records past the queue —
/// replay covering *more* never drops an acked record, which is all gap-freedom needs.
proof fn lemma_run_len_covers(wal: Seq<u8>, records: Seq<RecMeta>, k: int)
    requires
        0 <= k < records.len(),
        laid_out(wal, records, k),
    ensures
        run_len(wal, records[k].off as int, records[k].seq) >= records.len() - k,
    decreases records.len() - k,
{
    let off = records[k].off as int;
    let seq = records[k].seq;
    // Unfold laid_out(.,k): frame_at Some, sequence match, content ok, seq < MAX.
    let rlen = match frame_at(wal, off) {
        Some(p) => p.1,
        None => 0nat,
    };
    assert(frame_at(wal, off) == Some((seq, rlen)));
    assert(content_ok_spec(wal.subrange(off, off + rlen)));
    assert(seq < u64::MAX);
    // Unfold run_len once: the accepted record contributes 1 + the tail.
    assert(run_len(wal, off, seq) == 1 + run_len(wal, off + rlen, (seq + 1) as u64));
    if k + 1 < records.len() {
        // Contiguous + seq-continuous into record k+1, which is itself laid out.
        assert(records[k + 1].off as int == off + rlen);
        assert(records[k + 1].seq == (seq + 1) as u64);
        lemma_run_len_covers(wal, records, k + 1);
    }
}

/// The gap-freedom theorem: composing `advance_head` (`n_flushed`/`head`/`next_seq`)
/// with `replay_bound` (`count`). The four `requires` after `laid_out` are exactly
/// the two functions' `ensures` — so this fires whenever commit/mount call them in
/// sequence over a laid-out queue. Conclusion: **every unflushed record lies in the
/// replayed span** `[n_flushed, n_flushed + count)`. With `advance_head` placing
/// `head` at the first non-flushed record (everything below it flushed) and
/// `replay_bound.count == run_len >= len - n_flushed` (coverage), no
/// acked-but-uncommitted write is left behind — the code-level shadow of
/// `AckedWritesRecoverable`'s WAL-replay half.
proof fn lemma_gap_freedom(
    wal: Seq<u8>,
    records: Seq<RecMeta>,
    n_flushed: int,
    head: u64,
    next_seq: u64,
    count: int,
)
    requires
        laid_out(wal, records, 0),
        // advance_head's ensures (the flushed-prefix structure):
        0 <= n_flushed <= records.len(),
        forall|j: int| 0 <= j < n_flushed ==> (#[trigger] records[j]).flushed,
        n_flushed < records.len() ==> head == records[n_flushed].off
            && next_seq == records[n_flushed].seq,
        // replay_bound's ensures (Part 1, the maximal-run equality):
        n_flushed < records.len() ==> count == run_len(wal, head as int, next_seq),
    ensures
        forall|i: int|
            (0 <= i < records.len() && !(#[trigger] records[i]).flushed) ==> (n_flushed <= i
                && i < n_flushed + count),
{
    if n_flushed < records.len() {
        lemma_laid_out_mono(wal, records, 0, n_flushed);
        lemma_run_len_covers(wal, records, n_flushed);
        // count == run_len(head, next_seq) >= records.len() - n_flushed, and an
        // unflushed record's index is >= n_flushed (else flushed) and < len.
        assert forall|i: int| #![trigger records[i]] (0 <= i < records.len()
            && !records[i].flushed) implies (n_flushed <= i && i < n_flushed + count) by {
            if i < n_flushed {
                assert(records[i].flushed);
            }
        }
    }
    // n_flushed == records.len(): all records flushed, so no unflushed record
    // exists — the conclusion is vacuous (any such i would be flushed by the hyp).
}

} // verus!

pub struct Store<D: BlockDev> {
    chunks: ChunkStore<D>,
    opts: StoreOptions,
    /// Last committed superblock and the slot it lives in (A = false).
    sb: Superblock,
    sb_in_b: bool,
    /// Working ref table: committed state + flushed-but-uncommitted roots
    /// and staged row edits. Serialized at commit.
    table: RefTable,
    overlays: BTreeMap<Vec<u8>, Overlay>,
    wal_tail: u64,
    wal_seq: u64,
    wal_records: VecDeque<RecMeta>,
}

impl<D: BlockDev> Store<D> {
    // ── Lifecycle ───────────────────────────────────────────────────

    pub fn format(mut dev: D, opts: StoreOptions) -> Result<Store<D>, StoreError> {
        let chunk_off = WAL_OFF + opts.wal_len;
        assert!(dev.len() > chunk_off + 4096, "device too small");
        // Invalidate both slots first so a re-format can't leave a stale
        // valid superblock pointing into the new chunk region.
        dev.write(SB_A_OFF, &[0u8; SB_SIZE])?;
        dev.write(SB_B_OFF, &[0u8; SB_SIZE])?;
        dev.flush()?;

        let mut chunks = ChunkStore {
            dev,
            chunk_off,
            tail: 0,
            birth_gen: 1,
            index: BTreeMap::new(),
            free: BTreeMap::new(),
            pending_free: Vec::new(),
            index_extent: (0, 0),
            io_error: None,
        };
        let table = RefTable::default();
        let rt_hash = chunks.put(&table.encode());
        chunks.io_error.take().map_or(Ok(()), Err)?;
        let (index_extent, free) = chunks.write_index_frame()?;
        chunks.free = free;
        chunks.index_extent = index_extent;
        chunks.dev.flush()?; // barrier 1

        let sb = Superblock {
            generation: 1,
            ref_table: rt_hash,
            wal_head: 0,
            wal_next_seq: 1,
            wal_len: opts.wal_len,
            chunk_tail: chunks.tail,
            index_off: index_extent.0,
        };
        chunks.dev.write(SB_A_OFF, &sb.encode())?;
        chunks.dev.flush()?; // barrier 2

        Ok(Store {
            chunks,
            opts,
            sb,
            sb_in_b: false,
            table,
            overlays: BTreeMap::new(),
            wal_tail: 0,
            wal_seq: 1,
            wal_records: VecDeque::new(),
        })
    }

    /// Mount = crash recovery (rev0§4.5): both paths are the same code. Read
    /// both slots, discard invalid, take the higher generation; load the
    /// durable index it points at; replay the WAL tail into overlays.
    pub fn mount(dev: D, opts: StoreOptions) -> Result<Store<D>, StoreError> {
        let mut buf_a = vec![0u8; SB_SIZE];
        let mut buf_b = vec![0u8; SB_SIZE];
        dev.read(SB_A_OFF, &mut buf_a)?;
        dev.read(SB_B_OFF, &mut buf_b)?;
        let (ra, rb) = (Superblock::decode_checked(&buf_a), Superblock::decode_checked(&buf_b));
        // The survivor decision is the Verus-verified `pick_survivor`:
        // the valid slot of higher generation, the TLA+ `LiveSlot`. The version-
        // error distinction below stays plain Rust — it only shapes the refusal,
        // not the choice.
        let valid_a = ra.is_ok();
        let valid_b = rb.is_ok();
        let gen_a = ra.as_ref().map(|s| s.generation).unwrap_or(0);
        let gen_b = rb.as_ref().map(|s| s.generation).unwrap_or(0);
        let (sb, sb_in_b) = match pick_survivor(gen_a, valid_a, gen_b, valid_b) {
            // `pick_survivor` ensures `SlotA ==> valid_a` / `SlotB ==> valid_b`,
            // so these unwraps cannot panic.
            Survivor::SlotA => (ra.unwrap(), false),
            Survivor::SlotB => (rb.unwrap(), true),
            // No usable slot. An intact other-version slot is a refusal,
            // not a recovery case: tick-era timestamp fields are byte-
            // compatible with nanoseconds, so falling through to
            // "no superblock" (or worse, mounting) would misread, and the
            // rev0§2.6 stance — pre-v3 images are re-created with mkfs — is
            // only real if the user is told that's what happened.
            Survivor::Neither => {
                use crate::disk::SbError;
                return Err(match (ra, rb) {
                    (Err(SbError::WrongVersion(v)), _) | (_, Err(SbError::WrongVersion(v))) => {
                        StoreError::UnsupportedVersion(v)
                    }
                    _ => StoreError::NoSuperblock,
                });
            }
        };
        // Geometry chokepoint: the checksum above is integrity, not
        // authenticity — a torn write can't pass it, but anything that can
        // place bytes on the device can re-seal it. Validate every
        // offset/length field against the device length before the first
        // use; honest code never writes out-of-device geometry, so a
        // violation is corruption or forgery, never a slot to silently
        // fall back from.
        sb.validate_geometry(dev.len()).map_err(StoreError::Corrupt)?;
        // Geometry is not the only forgeable scalar: `generation` feeds
        // `birth_gen = generation + 1` here and `generation + 1` at every
        // commit. u64::MAX is 2^64 commits past honest — reject it rather
        // than overflow the derive (found by `mount_reseal`).
        let birth_gen = sb
            .generation
            .checked_add(1)
            .ok_or(StoreError::Corrupt("superblock generation exhausted"))?;

        let chunk_off = WAL_OFF + sb.wal_len;
        let mut chunks = ChunkStore {
            dev,
            chunk_off,
            tail: sb.chunk_tail,
            birth_gen,
            index: BTreeMap::new(),
            free: BTreeMap::new(),
            pending_free: Vec::new(),
            index_extent: (0, 0),
            io_error: None,
        };
        // The durable index (format v2): a self-verifying frame the
        // superblock points at. It was covered by barrier 1 of the commit
        // that won recovery, so a bad frame here is real corruption.
        let mut header = [0u8; CHUNK_HEADER];
        chunks.dev.read(chunk_off + sb.index_off, &mut header)?;
        let Some((ilen, _, ihash)) = disk::decode_chunk_header(&header) else {
            return Err(StoreError::Corrupt("bad index frame"));
        };
        // `chunk_tail` is geometry-validated above, so this gate bounds
        // `ilen` (and with it the allocation below) by the real device
        // length — checked, because index_off + frame_len wrapping past an
        // honest tail is exactly the extent-overrun shape.
        let frame_len = (CHUNK_HEADER + ilen) as u64;
        if sb
            .index_off
            .checked_add(frame_len)
            .is_none_or(|end| end > sb.chunk_tail)
        {
            return Err(StoreError::Corrupt("index frame overruns committed tail"));
        }
        let mut payload = vec![0u8; ilen];
        chunks
            .dev
            .read(chunk_off + sb.index_off + CHUNK_HEADER as u64, &mut payload)?;
        if Hash::of(&payload) != ihash {
            return Err(StoreError::Corrupt("index frame hash mismatch"));
        }
        let (index, free) = disk::decode_index(&payload)?;
        for e in index.values() {
            let end = e.off.checked_add(e.len as u64);
            if e.off < CHUNK_HEADER as u64 || end.is_none_or(|end| end > sb.chunk_tail) {
                return Err(StoreError::Corrupt("index entry out of bounds"));
            }
        }
        for (&off, &len) in &free {
            if off.checked_add(len).is_none_or(|end| end > sb.chunk_tail) {
                return Err(StoreError::Corrupt("free extent out of bounds"));
            }
        }
        chunks.index = index;
        chunks.free = free;
        chunks.index_extent = (sb.index_off, frame_len);

        let rt_bytes = chunks
            .read_object(&sb.ref_table)?
            .ok_or(StoreError::Corrupt("ref table missing"))?;
        let table = RefTable::decode(&rt_bytes)?;

        let mut store = Store {
            chunks,
            opts: StoreOptions { wal_len: sb.wal_len, ..opts },
            sb: sb.clone(),
            sb_in_b,
            table,
            overlays: BTreeMap::new(),
            wal_tail: sb.wal_head,
            wal_seq: sb.wal_next_seq,
            wal_records: VecDeque::new(),
        };

        // WAL replay: contiguous, checksummed, seq-continuous records from the
        // committed head; anything else is an unacked torn tail. The *bound* of
        // this walk — how many records replay, and the resulting
        // `(wal_tail, wal_seq)` — is the Verus-verified `replay_bound`:
        // total + terminating ∀ bytes, so the `off += rlen` in-bounds
        // reasoning is a theorem, not a code comment. The applier below
        // re-walks exactly that many records to decode + apply each; the content
        // layer (the `WalOp` decode) and the extent gate stay plain Rust.
        let mut wal = vec![0u8; sb.wal_len as usize];
        store.chunks.dev.read(WAL_OFF, &mut wal)?;
        let span = replay_bound(&wal, sb.wal_head, sb.wal_next_seq);
        let mut off = sb.wal_head;
        let mut seq = sb.wal_next_seq;
        for _ in 0..span.count {
            // `replay_bound` proved each of these `span.count` records frames,
            // checksums, and continues the sequence — so `decode_record` returns
            // `Some` here (the call is total because the verified core's contract
            // says so).
            let (_rseq, op, rlen) = WalOp::decode_record(&wal[off as usize..])
                .expect("replay_bound accepted this record");
            // Mirror of the pre-WAL extent gate in Store::write: no image
            // this code produced contains such a record — write rejects the
            // extent before logging — and a torn write can't fake one (the
            // record checksum covers the whole payload). So this is forgery
            // or corruption, not an unacked tail: reject loudly.
            if let WalOp::Write { offset, data, .. } = &op {
                if offset
                    .checked_add(data.len() as u64)
                    .is_none_or(|end| end > store.chunks.region_len())
                {
                    return Err(StoreError::Corrupt("wal record extent out of range"));
                }
            }
            store.apply_to_overlay(&op);
            store.wal_records.push_back(RecMeta {
                seq,
                off,
                ref_name: op.ref_name().to_vec(),
                flushed: false,
            });
            off += rlen as u64;
            // An honest seq counter never nears u64::MAX (2^64 records); a
            // re-sealed `wal_next_seq` there overflows the increment. `replay_bound`
            // stops at that boundary, so within `span.count` this never fires — it
            // stays as the loud rejection the original walk gave (rev0§4.4 forgery gate).
            seq = seq
                .checked_add(1)
                .ok_or(StoreError::Corrupt("wal sequence exhausted"))?;
        }
        // The applier walked exactly to `replay_bound`'s verified bound (both
        // advance by the same per-record `rlen`); cross-check it in debug builds.
        debug_assert_eq!(off, span.end_off);
        store.wal_tail = off;
        store.wal_seq = seq;
        Ok(store)
    }

    fn check_io(&mut self) -> Result<(), StoreError> {
        match self.chunks.io_error.take() {
            Some(e) => Err(StoreError::Io(e)),
            None => Ok(()),
        }
    }

    // ── Refs and snapshots ──────────────────────────────────────────

    pub fn create_ref(&mut self, name: &[u8]) -> Result<(), StoreError> {
        let empty_root = Dir::new().save(&mut self.chunks);
        self.check_io()?;
        self.table.refs.insert(
            name.to_vec(),
            RefEntry { root: empty_root, generation: 0, next_snap_id: 1 },
        );
        self.commit()
    }

    pub fn refs(&self) -> impl Iterator<Item = (&Vec<u8>, &RefEntry)> {
        self.table.refs.iter()
    }

    /// Committed superblock generation (advances on every commit).
    pub fn generation(&self) -> u64 {
        self.sb.generation
    }

    pub fn snapshots(&self, ref_name: &[u8]) -> impl Iterator<Item = &SnapRow> {
        let key = ref_name.to_vec();
        self.table
            .snaps
            .range((key.clone(), 0)..(key, u64::MAX))
            .map(|(_, row)| row)
    }

    /// Snapshot the ref (forces a flush — a snapshot must name a tree
    /// hash, rev0§4.4). `now` is UTC nanoseconds from the caller
    /// (server-assigned, rev0§4.7) and is clamped per-ref strictly monotone:
    /// `ts = max(now, predecessor_ts + 1)`. A host clock regressing
    /// between boots can therefore never make a child snapshot "older"
    /// than its parent — the clamp protects exactly what retention needs
    /// (per-ref strict order) and nothing it can't (a wildly wrong RTC
    /// still skews absolute ages, rev0§2.6/rev0§4.7).
    pub fn snapshot(
        &mut self,
        ref_name: &[u8],
        provenance: &[u8],
        message: &[u8],
        class: u8,
        now: u64,
    ) -> Result<u64, StoreError> {
        self.flush_ref(ref_name)?;
        let entry = self.table.refs.get(ref_name).ok_or(StoreError::NoSuchRef)?.clone();
        let id = entry.next_snap_id;
        let last = self.snapshots(ref_name).last().map(|r| (r.id, r.timestamp));
        let row = SnapRow {
            id,
            root: entry.root,
            timestamp: now.max(last.map(|(_, t)| t.saturating_add(1)).unwrap_or(0)),
            provenance: provenance.to_vec(),
            parent: last.map(|(id, _)| id),
            message: message.to_vec(),
            class,
        };
        self.table.snaps.insert((ref_name.to_vec(), id), row);
        self.table.refs.get_mut(ref_name).unwrap().next_snap_id = id + 1;
        self.commit()?;
        Ok(id)
    }

    /// Roll the ref head back to a snapshot. Pending overlay writes are
    /// flushed first (into the abandoned pre-rollback root) so the WAL
    /// stays coherent; the rollback then commits the snapshot's root as
    /// the new head. History rewriting at the storage layer is just a
    /// ref-table edit (rev0§4.6).
    pub fn rollback(&mut self, ref_name: &[u8], snap_id: u64) -> Result<(), StoreError> {
        let root = self
            .table
            .snaps
            .get(&(ref_name.to_vec(), snap_id))
            .ok_or(StoreError::NoSuchSnapshot)?
            .root;
        self.flush_ref(ref_name)?;
        self.table.refs.get_mut(ref_name).ok_or(StoreError::NoSuchRef)?.root = root;
        self.commit()
    }

    pub fn tag(&mut self, name: &[u8], ref_name: &[u8], snap_id: u64) -> Result<(), StoreError> {
        if !self.table.snaps.contains_key(&(ref_name.to_vec(), snap_id)) {
            return Err(StoreError::NoSuchSnapshot);
        }
        self.table.tags.insert(name.to_vec(), (ref_name.to_vec(), snap_id));
        self.commit()
    }

    // ── Write path (rev0§4.3) ───────────────────────────────────────────

    /// A mutation must be rejected *before* it reaches the WAL if it can
    /// never flush (writing onto a directory, or under a file): an acked
    /// record that cannot apply would poison every future replay.
    fn validate_mutation_path(&self, ref_name: &[u8], path: &Path) -> Result<(), StoreError> {
        let entry = self.table.refs.get(ref_name).ok_or(StoreError::NoSuchRef)?;
        if path.is_empty() {
            return Err(StoreError::Format(FormatError::BadName));
        }
        for comp in path {
            crate::prolly::validate_name(comp)?;
        }
        let overlay = self.overlays.get(ref_name);
        for i in 1..=path.len() {
            let prefix: Path = path[..i].to_vec();
            let is_final = i == path.len();
            // The overlay only ever holds files; an unlinked path is absent.
            if let Some(o) = overlay {
                match o.state(&prefix) {
                    FileState::Dirty(_) if !is_final => return Err(StoreError::NotAFile),
                    FileState::Dirty(_) | FileState::Unlinked => continue,
                    FileState::Clean => {}
                }
            }
            let comps: Vec<&[u8]> = prefix.iter().map(|c| c.as_slice()).collect();
            match tree::lookup(&self.chunks, &entry.root, &comps)? {
                Some(Entry { kind: EntryKind::Dir, .. }) if is_final => {
                    return Err(StoreError::NotAFile)
                }
                Some(Entry { kind: EntryKind::File, .. }) if !is_final => {
                    return Err(StoreError::NotAFile)
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn write(
        &mut self,
        ref_name: &[u8],
        path: &Path,
        offset: u64,
        data: &[u8],
        mtime: u64,
    ) -> Result<(), StoreError> {
        self.validate_mutation_path(ref_name, path)?;
        // Same pre-WAL rule as validate_mutation_path: an acked record that
        // cannot apply would poison every future replay. A u64-overflowing
        // extent wraps the overlay's interval math, and an extent beyond the
        // chunk region can never flush (dedup aside) — and would force apply()
        // to materialize the whole extent in memory.
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(StoreError::WriteOutOfRange)?;
        if end > self.chunks.region_len() {
            return Err(StoreError::WriteOutOfRange);
        }
        let op = WalOp::Write {
            ref_name: ref_name.to_vec(),
            path: path.clone(),
            offset,
            mtime,
            data: data.to_vec(),
        };
        self.log_then_apply(op)
    }

    pub fn unlink(
        &mut self,
        ref_name: &[u8],
        path: &Path,
        mtime: u64,
    ) -> Result<(), StoreError> {
        self.validate_mutation_path(ref_name, path)?;
        let op = WalOp::Unlink { ref_name: ref_name.to_vec(), path: path.clone(), mtime };
        self.log_then_apply(op)
    }

    /// Hand the device back (for crash-injection tests).
    pub fn into_dev(self) -> D {
        self.chunks.dev
    }

    pub fn dev_mut(&mut self) -> &mut D {
        &mut self.chunks.dev
    }

    /// WAL append + fsync before the overlay sees the write — the ack
    /// implies durability (rev0§4.3 step 2).
    fn log_then_apply(&mut self, op: WalOp) -> Result<(), StoreError> {
        let rec = op.encode_record(self.wal_seq);
        if rec.len() as u64 > self.opts.wal_len {
            // Oversized: bypass the WAL, commit synchronously before ack.
            let r = op.ref_name().to_vec();
            self.apply_to_overlay(&op);
            self.flush_ref(&r)?;
            return self.commit();
        }
        if self.wal_tail + rec.len() as u64 > self.opts.wal_len {
            // WAL full: flush everything, commit (covers all records),
            // log resets to offset 0.
            self.sync_all()?;
            debug_assert_eq!(self.wal_tail, 0);
        }
        self.chunks.dev.write(WAL_OFF + self.wal_tail, &rec)?;
        self.chunks.dev.flush()?;
        self.wal_records.push_back(RecMeta {
            seq: self.wal_seq,
            off: self.wal_tail,
            ref_name: op.ref_name().to_vec(),
            flushed: false,
        });
        self.wal_tail += rec.len() as u64;
        self.wal_seq += 1;
        self.apply_to_overlay(&op);

        // Size pressure (rev0§4.4), collapsed to the simplest correct policy:
        // blow the global budget → sync everything.
        let total: usize = self.overlays.values().map(|o| o.bytes()).sum();
        if total > self.opts.overlay_budget {
            self.sync_all()?;
        }
        Ok(())
    }

    fn apply_to_overlay(&mut self, op: &WalOp) {
        let overlay = self.overlays.entry(op.ref_name().to_vec()).or_default();
        match op {
            WalOp::Write { path, offset, mtime, data, .. } => {
                overlay.write(path, *offset, data, *mtime);
            }
            WalOp::Unlink { path, mtime, .. } => {
                overlay.unlink(path, *mtime);
            }
        }
    }

    // ── Read path (overlay first, tree below — rev0§4.3) ────────────────

    pub fn read(
        &self,
        ref_name: &[u8],
        path: &Path,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let entry = self.table.refs.get(ref_name).ok_or(StoreError::NoSuchRef)?;
        let state = self
            .overlays
            .get(ref_name)
            .map(|o| o.state(path))
            .unwrap_or(FileState::Clean);
        match state {
            FileState::Unlinked => Ok(None),
            FileState::Clean => self.read_from_tree(&entry.root, path),
            FileState::Dirty(fo) => {
                let base = if fo.fresh {
                    Vec::new()
                } else {
                    self.read_from_tree(&entry.root, path)?.unwrap_or_default()
                };
                Ok(Some(fo.apply(base)))
            }
        }
    }

    /// Read a file out of a committed/flushed tree root (also used for
    /// snapshot reads, where no overlay applies).
    pub fn read_at_root(
        &self,
        root: &Hash,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_from_tree(root, path)
    }

    /// Mass revocation of a ref's storage handles (rev0§2.2): O(1) — bump the
    /// generation; every handle recorded at an older generation goes
    /// stale lazily, on next use. Persists through the normal commit.
    pub fn bump_generation(&mut self, ref_name: &[u8]) -> Result<(), StoreError> {
        self.table
            .refs
            .get_mut(ref_name)
            .ok_or(StoreError::NoSuchRef)?
            .generation += 1;
        self.commit()
    }

    pub fn lookup_at_root(
        &self,
        root: &Hash,
        comps: &[&[u8]],
    ) -> Result<Option<Entry>, StoreError> {
        Ok(tree::lookup(&self.chunks, root, comps)?)
    }

    pub fn list_dir_node(
        &self,
        node: &Hash,
    ) -> Result<Vec<(Vec<u8>, EntryKind, u64)>, StoreError> {
        let dir = Dir::load(&self.chunks, node)?;
        Ok(dir.iter().map(|e| (e.name.clone(), e.kind, e.size)).collect())
    }

    pub fn snapshot_root(&self, ref_name: &[u8], snap_id: u64) -> Result<Hash, StoreError> {
        Ok(self
            .table
            .snaps
            .get(&(ref_name.to_vec(), snap_id))
            .ok_or(StoreError::NoSuchSnapshot)?
            .root)
    }

    fn read_from_tree(&self, root: &Hash, path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        let comps: Vec<&[u8]> = path.iter().map(|c| c.as_slice()).collect();
        match tree::lookup(&self.chunks, root, &comps)? {
            None => Ok(None),
            Some(Entry { kind: EntryKind::Dir, .. }) => Err(StoreError::NotAFile),
            Some(e) => Ok(Some(read_file(&self.chunks, &e.content, e.size)?)),
        }
    }

    /// Merged directory listing: committed tree + dirty overlay (rev0§4.4
    /// read path applies to listings too).
    pub fn list(
        &self,
        ref_name: &[u8],
        dir_path: &Path,
    ) -> Result<Vec<(Vec<u8>, EntryKind, u64)>, StoreError> {
        let entry = self.table.refs.get(ref_name).ok_or(StoreError::NoSuchRef)?;
        let comps: Vec<&[u8]> = dir_path.iter().map(|c| c.as_slice()).collect();
        let dir_root = if comps.is_empty() {
            Some(entry.root)
        } else {
            match tree::lookup(&self.chunks, &entry.root, &comps)? {
                Some(Entry { content: Content::DirRoot(h), .. }) => Some(h),
                Some(_) => return Err(StoreError::NotAFile),
                // Directory only exists in the overlay (or not at all).
                None => None,
            }
        };
        let mut out: Vec<(Vec<u8>, EntryKind, u64)> = Vec::new();
        if let Some(root) = dir_root {
            let dir = Dir::load(&self.chunks, &root)?;
            for e in dir.iter() {
                out.push((e.name.clone(), e.kind, e.size));
            }
        }
        if let Some(overlay) = self.overlays.get(ref_name) {
            for p in overlay.unlinked_in_dir(dir_path) {
                let name = p.last().unwrap();
                out.retain(|(n, _, _)| n != name);
            }
            for (p, fo) in overlay.files_in_dir(dir_path) {
                let name = p.last().unwrap().clone();
                let size = if fo.fresh {
                    fo.extent()
                } else {
                    out.iter()
                        .find(|(n, _, _)| *n == name)
                        .map(|(_, _, s)| (*s).max(fo.extent()))
                        .unwrap_or(fo.extent())
                };
                out.retain(|(n, _, _)| *n != name);
                out.push((name, EntryKind::File, size));
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn overlay_bytes(&self) -> usize {
        self.overlays.values().map(|o| o.bytes()).sum()
    }

    // ── Flush and commit (rev0§4.3 steps 3–4) ───────────────────────────

    /// Turn one ref's overlay into immutable tree (path-copy to a new
    /// root). Nothing on disk references the result until commit.
    pub fn flush_ref(&mut self, ref_name: &[u8]) -> Result<(), StoreError> {
        let Some(overlay) = self.overlays.remove(ref_name) else {
            return Ok(());
        };
        if overlay.is_empty() {
            return Ok(());
        }
        let mut root = self.table.refs.get(ref_name).ok_or(StoreError::NoSuchRef)?.root;

        for path in overlay.unlinks() {
            let comps: Vec<&[u8]> = path.iter().map(|c| c.as_slice()).collect();
            let (new_root, _) = tree::remove(&mut self.chunks, &root, &comps)?;
            self.check_io()?;
            root = new_root;
        }
        for (path, fo) in overlay.files() {
            let comps: Vec<&[u8]> = path.iter().map(|c| c.as_slice()).collect();
            let (dir, name) = comps.split_at(comps.len() - 1);
            let old = tree::lookup(&self.chunks, &root, &comps)?;
            let base = match (&old, fo.fresh) {
                (Some(Entry { kind: EntryKind::Dir, .. }), _) => {
                    return Err(StoreError::NotAFile)
                }
                (Some(e), false) => read_file(&self.chunks, &e.content, e.size)?,
                _ => Vec::new(),
            };
            let content = fo.apply(base);
            let flags = old.map(|e| e.flags).unwrap_or(0);
            let mut entry =
                make_file_entry(&mut self.chunks, &self.opts.chunker, name[0], &content, fo.mtime, flags);
            self.check_io()?;
            entry.mtime = fo.mtime;
            root = tree::put(&mut self.chunks, &root, dir, entry, fo.mtime)?;
            self.check_io()?;
        }

        self.table.refs.get_mut(ref_name).unwrap().root = root;
        for rec in &mut self.wal_records {
            if rec.ref_name.as_slice() == ref_name {
                rec.flushed = true;
            }
        }
        Ok(())
    }

    /// The single atomicity mechanism (rev0§4.2): barrier 1, superblock to the
    /// older slot, barrier 2. The WAL head advances past the contiguous
    /// prefix of flushed records (rev0§4.3 step 4).
    pub fn commit(&mut self) -> Result<(), StoreError> {
        let rt_hash = self.chunks.put(&self.table.encode());
        self.check_io()?;
        // The index frame this commit supersedes becomes free once the
        // flip lands; record it in the new frame's free list now. (On a
        // failed commit it may be pushed again next time — merged_free
        // dedups identical extents.)
        let old_index_extent = self.chunks.index_extent;
        self.chunks.pending_free.push(old_index_extent);
        let (new_index_extent, new_free) = self.chunks.write_index_frame()?;
        self.chunks.dev.flush()?; // barrier 1: no SB may reference non-durable chunks

        // Pop the contiguous flushed prefix; the new head/seq is the first
        // non-flushed record (or the linear-WAL reset when all flushed). The
        // decision is the Verus-verified `advance_head`: everything
        // popped is flushed, the head record (if any) is not — the TLA+
        // `CommitPrepare.newHead`.
        let wal_seq = self.wal_seq;
        let adv = advance_head(self.wal_records.make_contiguous(), wal_seq);
        for _ in 0..adv.n_flushed {
            self.wal_records.pop_front();
        }
        let (wal_head, wal_next_seq) = (adv.head, adv.next_seq);
        if self.wal_records.is_empty() {
            // Empty log: reclaim the region. Stale bytes beyond the head are
            // rejected on replay by the seq check.
            self.wal_tail = 0;
        }

        let new_sb = Superblock {
            generation: self.sb.generation + 1,
            ref_table: rt_hash,
            wal_head,
            wal_next_seq,
            wal_len: self.opts.wal_len,
            chunk_tail: self.chunks.tail,
            index_off: new_index_extent.0,
        };
        // Always alternate; never overwrite the current latest commit. The
        // target is the Verus-verified `commit_target`: the non-live
        // slot, so a torn write here damages only the slot being written.
        let target = match commit_target(self.sb_in_b) {
            Slot::A => SB_A_OFF,
            Slot::B => SB_B_OFF,
        };
        self.chunks.dev.write(target, &new_sb.encode())?;
        self.chunks.dev.flush()?; // barrier 2: only now is the commit real
        self.sb = new_sb;
        self.sb_in_b = !self.sb_in_b;
        self.chunks.birth_gen = self.sb.generation + 1;
        // The flip landed: extents freed by this commit (GC sweep, the
        // superseded index frame) are now committed-free and reusable.
        self.chunks.free = new_free;
        self.chunks.pending_free.clear();
        self.chunks.index_extent = new_index_extent;
        Ok(())
    }

    pub fn sync_ref(&mut self, ref_name: &[u8]) -> Result<(), StoreError> {
        self.flush_ref(ref_name)?;
        self.commit()
    }

    pub fn sync_all(&mut self) -> Result<(), StoreError> {
        let refs: Vec<Vec<u8>> = self.overlays.keys().cloned().collect();
        for r in refs {
            self.flush_ref(&r)?;
        }
        self.commit()
    }

    // ── GC and history rewriting (rev0§4.6-4.7) ─────────────────────

    /// Mark-and-sweep GC. Marks from the committed root set (every ref
    /// head and snapshot root; tags name snapshot IDs, so their targets
    /// are already covered), sweeps by removing index entries whose birth
    /// generation predates the epoch, and commits the new index + free
    /// list through the ordinary superblock flip. The sweep is pure
    /// metadata until that flip: a crash anywhere inside GC recovers the
    /// previous commit, losing reclamation work but never data.
    pub fn gc(&mut self) -> Result<GcStats, StoreError> {
        // Flush + commit first: the committed ref table then equals the
        // working table and the WAL is empty, so the committed root set
        // is the complete root set.
        self.sync_all()?;
        // Chunks born at/after the epoch are live by fiat (rev0§4.6). In this
        // synchronous cycle none can appear between mark and sweep, but
        // the check is the stated contract, not an optimization.
        let epoch = self.chunks.birth_gen;
        let mut live: BTreeSet<Hash> = BTreeSet::new();
        live.insert(self.sb.ref_table);
        let mut roots: Vec<Hash> = self.table.refs.values().map(|r| r.root).collect();
        roots.extend(self.table.snaps.values().map(|s| s.root));
        for root in roots {
            gc::mark(&self.chunks, &root, &mut live)?;
        }

        let condemned: Vec<(Hash, IndexEntry)> = self
            .chunks
            .index
            .iter()
            .filter(|(h, e)| !live.contains(h) && e.birth < epoch)
            .map(|(h, e)| (*h, *e))
            .collect();
        let mut freed_bytes = 0u64;
        for (hash, e) in &condemned {
            self.chunks.index.remove(hash);
            let frame_len = e.len as u64 + CHUNK_HEADER as u64;
            self.chunks
                .pending_free
                .push((e.off - CHUNK_HEADER as u64, frame_len));
            freed_bytes += frame_len;
        }
        self.commit()?;
        Ok(GcStats {
            live_objects: self.chunks.index.len() as u64,
            freed_objects: condemned.len() as u64,
            freed_bytes,
        })
    }

    /// Chunk-region space accounting — what the watermark trigger and a
    /// `df` builtin read.
    pub fn space(&self) -> SpaceInfo {
        let total = self.chunks.region_len();
        let free = self.chunks.free_bytes();
        SpaceInfo { total, used: total - free, free }
    }

    /// History rewriting (rev0§4.6): drop one snapshot row, re-pointing
    /// children's advisory parent past it (rev0§4.7). Tag targets are
    /// keep-strength pins and refuse deletion. The newly unreachable
    /// mass is reclaimed by the next GC, not here — this op is O(small).
    pub fn delete_snapshot(&mut self, ref_name: &[u8], snap_id: u64) -> Result<(), StoreError> {
        let key = (ref_name.to_vec(), snap_id);
        let row = self.table.snaps.get(&key).ok_or(StoreError::NoSuchSnapshot)?;
        if self
            .table
            .tags
            .values()
            .any(|(r, s)| r.as_slice() == ref_name && *s == snap_id)
        {
            return Err(StoreError::Pinned);
        }
        let new_parent = row.parent;
        self.table.snaps.remove(&key);
        let range = (ref_name.to_vec(), 0)..(ref_name.to_vec(), u64::MAX);
        for (_, r) in self.table.snaps.range_mut(range) {
            if r.parent == Some(snap_id) {
                r.parent = new_parent;
            }
        }
        self.commit()
    }

    /// Retention-class edit (rev0§4.7): the "mark survivors `keep`, run the
    /// policy" flow is this plus `delete_snapshot`, policy in userspace.
    pub fn set_snapshot_class(
        &mut self,
        ref_name: &[u8],
        snap_id: u64,
        class: u8,
    ) -> Result<(), StoreError> {
        if class > disk::CLASS_EPHEMERAL {
            return Err(StoreError::Format(FormatError::BadEntry("bad retention class")));
        }
        self.table
            .snaps
            .get_mut(&(ref_name.to_vec(), snap_id))
            .ok_or(StoreError::NoSuchSnapshot)?
            .class = class;
        self.commit()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcStats {
    pub live_objects: u64,
    pub freed_objects: u64,
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::{CrashDev, MemDev};
    use proptest::prelude::*;

    fn test_opts() -> StoreOptions {
        StoreOptions {
            wal_len: 8 * 1024,
            chunker: ChunkerParams { min: 64, avg: 256, max: 1024 },
            overlay_budget: 32 * 1024,
        }
    }

    fn p(parts: &[&str]) -> Path {
        parts.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn write_read_sync_remount() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["etc", "conf"]), 0, b"hello", 1).unwrap();
        store.write(b"main", &p(&["etc", "conf"]), 5, b" world", 2).unwrap();
        assert_eq!(store.read(b"main", &p(&["etc", "conf"])).unwrap().unwrap(), b"hello world");

        store.sync_ref(b"main").unwrap();
        assert_eq!(store.read(b"main", &p(&["etc", "conf"])).unwrap().unwrap(), b"hello world");

        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(store2.read(b"main", &p(&["etc", "conf"])).unwrap().unwrap(), b"hello world");
        let ls = store2.list(b"main", &p(&["etc"])).unwrap();
        assert_eq!(ls, vec![(b"conf".to_vec(), EntryKind::File, 11)]);
    }

    #[test]
    fn acked_write_survives_crash_without_sync() {
        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["a"]), 0, b"acked data", 1).unwrap();
        // No sync: the tree never saw this write — only the fsynced WAL has it.
        let mut dev = store.into_dev();
        dev.crash(0xDEAD);
        let store2 = Store::mount(dev, test_opts()).unwrap();
        assert_eq!(store2.read(b"main", &p(&["a"])).unwrap().unwrap(), b"acked data");
    }

    #[test]
    fn unlink_and_resurrect() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"version one", 1).unwrap();
        store.sync_ref(b"main").unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap(), None);
        store.write(b"main", &p(&["f"]), 2, b"x", 3).unwrap();
        // Fresh file after unlink: old content must not bleed through.
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap().unwrap(), vec![0, 0, b'x']);
        store.sync_ref(b"main").unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap().unwrap(), vec![0, 0, b'x']);
    }

    #[test]
    fn snapshot_rollback() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["doc"]), 0, b"original", 10).unwrap();
        let snap = store.snapshot(b"main", b"session=test", b"before edit", disk::CLASS_KEEP, 100).unwrap();
        store.write(b"main", &p(&["doc"]), 0, b"MODIFIED", 11).unwrap();
        store.sync_ref(b"main").unwrap();
        assert_eq!(store.read(b"main", &p(&["doc"])).unwrap().unwrap(), b"MODIFIED");

        // Snapshot reads see the old root.
        let root = store.snapshot_root(b"main", snap).unwrap();
        assert_eq!(store.read_at_root(&root, &p(&["doc"])).unwrap().unwrap(), b"original");

        store.rollback(b"main", snap).unwrap();
        assert_eq!(store.read(b"main", &p(&["doc"])).unwrap().unwrap(), b"original");

        // Snapshot identity is the per-ref sequence number (rev0§4.7).
        let rows: Vec<u64> = store.snapshots(b"main").map(|r| r.id).collect();
        assert_eq!(rows, vec![1]);
    }

    #[test]
    fn wal_full_forces_commit_and_resets() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        // Each record is ~100 bytes; 8 KiB WAL forces several resets.
        for i in 0..200u32 {
            let path = p(&[&format!("f{}", i % 7)]);
            store.write(b"main", &path, (i as u64) * 16, &i.to_le_bytes(), i as u64).unwrap();
        }
        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        for i in 193..200u32 {
            let path = p(&[&format!("f{}", i % 7)]);
            let content = store2.read(b"main", &path).unwrap().unwrap();
            let off = (i as u64 * 16) as usize;
            assert_eq!(&content[off..off + 4], &i.to_le_bytes());
        }
    }

    #[test]
    fn torn_superblock_recovers_older_commit() {
        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["a"]), 0, b"committed", 1).unwrap();
        store.sync_ref(b"main").unwrap();

        // Second commit: cut power during it, at every possible point.
        for fail_at in 0..12u64 {
            let mut store = Store::mount(
                {
                    let mut d = CrashDev::new(1 << 20);
                    let src = store_snapshot_bytes(&store);
                    d.write(0, &src).unwrap();
                    d.flush().unwrap();
                    d
                },
                test_opts(),
            )
            .unwrap();
            store.write(b"main", &p(&["a"]), 0, b"NEWERDATA", 2).unwrap();
            store.dev_mut().set_fail_after(fail_at);
            let _ = store.sync_ref(b"main");
            let mut dev = store.into_dev();
            dev.clear_fail();
            dev.crash(fail_at.wrapping_mul(0x9E3779B9));
            let recovered = Store::mount(dev, test_opts()).unwrap();
            // The acked write must be there — committed or via WAL replay.
            assert_eq!(
                recovered.read(b"main", &p(&["a"])).unwrap().unwrap(),
                b"NEWERDATA",
                "fail_at={fail_at}"
            );
        }
    }

    /// Serialize a CrashDev-backed store's durable state (test helper).
    fn store_snapshot_bytes(store: &Store<CrashDev>) -> Vec<u8> {
        let dev_len = store.chunks.dev.len() as usize;
        let mut buf = vec![0u8; dev_len];
        store.chunks.dev.read(0, &mut buf).unwrap();
        buf
    }

    #[test]
    fn gc_reclaims_superseded_roots_and_reuses_space() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();

        // Churn: each iteration supersedes the previous root and file
        // chunks; without reclamation `used` grows without bound, with it
        // the footprint stays flat (freed extents get reused).
        let mut used_after_first = 0;
        for i in 0..10u8 {
            let data: Vec<u8> = (0..20_000).map(|j| (j as u8).wrapping_add(i)).collect();
            store.write(b"main", &p(&["churn"]), 0, &data, i as u64).unwrap();
            store.sync_ref(b"main").unwrap();
            let stats = store.gc().unwrap();
            if i == 0 {
                used_after_first = store.space().used;
                assert!(stats.live_objects > 0);
            } else {
                assert!(stats.freed_objects > 0, "iteration {i} freed nothing");
            }
        }
        assert!(
            store.space().used < used_after_first * 3,
            "space not reused: used {} vs first-iteration {}",
            store.space().used,
            used_after_first
        );

        // The store still works and survives a remount with its free list.
        let expect: Vec<u8> = (0..20_000).map(|j| (j as u8).wrapping_add(9)).collect();
        assert_eq!(store.read(b"main", &p(&["churn"])).unwrap().unwrap(), expect);
        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(store2.read(b"main", &p(&["churn"])).unwrap().unwrap(), expect);
    }

    #[test]
    fn snapshots_pin_data_until_deleted() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let old: Vec<u8> = (0..30_000).map(|j| (j % 251) as u8).collect();
        store.write(b"main", &p(&["data"]), 0, &old, 1).unwrap();
        let snap = store.snapshot(b"main", b"t", b"v1", disk::CLASS_AUTO, 10).unwrap();
        let new: Vec<u8> = (0..30_000).map(|j| (j % 13) as u8).collect();
        store.write(b"main", &p(&["data"]), 0, &new, 2).unwrap();
        store.sync_ref(b"main").unwrap();

        // The snapshot pins the old root: GC must keep it readable.
        store.gc().unwrap();
        let root = store.snapshot_root(b"main", snap).unwrap();
        assert_eq!(store.read_at_root(&root, &p(&["data"])).unwrap().unwrap(), old);

        // Dropping the snapshot is a ref-table edit; the next GC reclaims
        // the now-unreachable mass (rev0§4.6 "history rewriting").
        let used_before = store.space().used;
        store.delete_snapshot(b"main", snap).unwrap();
        let stats = store.gc().unwrap();
        assert!(stats.freed_objects > 0);
        assert!(store.space().used < used_before);
        assert!(matches!(
            store.snapshot_root(b"main", snap),
            Err(StoreError::NoSuchSnapshot)
        ));
        assert_eq!(store.read(b"main", &p(&["data"])).unwrap().unwrap(), new);
    }

    #[test]
    fn canonical_roots_shared_across_snapshots_survive_partial_delete() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, &[7u8; 5000], 1).unwrap();
        // Two snapshots of unchanged content share one root (rev0§4.7: same
        // root for different events is normal under canonical trees).
        let s1 = store.snapshot(b"main", b"t", b"a", disk::CLASS_AUTO, 10).unwrap();
        let s2 = store.snapshot(b"main", b"t", b"b", disk::CLASS_AUTO, 11).unwrap();
        store.write(b"main", &p(&["f"]), 0, &[9u8; 5000], 2).unwrap();
        store.sync_ref(b"main").unwrap();

        store.delete_snapshot(b"main", s1).unwrap();
        store.gc().unwrap();
        // s2 still pins the shared root.
        let root = store.snapshot_root(b"main", s2).unwrap();
        assert_eq!(store.read_at_root(&root, &p(&["f"])).unwrap().unwrap(), [7u8; 5000]);

        store.delete_snapshot(b"main", s2).unwrap();
        let stats = store.gc().unwrap();
        assert!(stats.freed_objects > 0);
    }

    #[test]
    fn delete_snapshot_repoints_parents_and_respects_tag_pins() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"1", 1).unwrap();
        let s1 = store.snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 10).unwrap();
        store.write(b"main", &p(&["f"]), 0, b"2", 2).unwrap();
        let s2 = store.snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 20).unwrap();
        store.write(b"main", &p(&["f"]), 0, b"3", 3).unwrap();
        let s3 = store.snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 30).unwrap();

        // Prune the middle: the child re-points to the grandparent (rev0§4.7).
        store.delete_snapshot(b"main", s2).unwrap();
        let rows: Vec<(u64, Option<u64>)> =
            store.snapshots(b"main").map(|r| (r.id, r.parent)).collect();
        assert_eq!(rows, vec![(s1, None), (s3, Some(s1))]);

        // Tags are keep-strength pins.
        store.tag(b"release", b"main", s1).unwrap();
        assert!(matches!(
            store.delete_snapshot(b"main", s1),
            Err(StoreError::Pinned)
        ));

        // Retention class is an editable row field (rev0§4.7).
        store.set_snapshot_class(b"main", s3, disk::CLASS_KEEP).unwrap();
        assert_eq!(
            store.snapshots(b"main").find(|r| r.id == s3).unwrap().class,
            disk::CLASS_KEEP
        );
    }

    #[test]
    fn crash_mid_gc_loses_no_data() {
        // Base state: a snapshot pinning old content, current head content,
        // and a deleted file whose chunks are reclaimable garbage.
        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["keepme"]), 0, b"pinned by snap", 1).unwrap();
        let snap = store.snapshot(b"main", b"t", b"", disk::CLASS_KEEP, 10).unwrap();
        store.write(b"main", &p(&["keepme"]), 0, b"current state!", 2).unwrap();
        store.write(b"main", &p(&["junk"]), 0, &[0xAB; 3000], 3).unwrap();
        store.sync_all().unwrap();
        store.unlink(b"main", &p(&["junk"]), 4).unwrap();
        store.sync_all().unwrap();
        let base = store_snapshot_bytes(&store);

        // Cut power at every point inside the GC cycle (both commits,
        // the sweep, the index writes). Whatever survives must mount and
        // serve every piece of live data.
        for fail_at in 0..24u64 {
            let mut dev = CrashDev::new(1 << 20);
            dev.write(0, &base).unwrap();
            dev.flush().unwrap();
            let mut store = Store::mount(dev, test_opts()).unwrap();
            store.dev_mut().set_fail_after(fail_at);
            let _ = store.gc();
            let mut dev = store.into_dev();
            dev.clear_fail();
            dev.crash(fail_at.wrapping_mul(0x9E3779B97F4A7C15));

            let mut rec = Store::mount(dev, test_opts()).unwrap();
            let check = |s: &Store<CrashDev>| {
                assert_eq!(
                    s.read(b"main", &p(&["keepme"])).unwrap().unwrap(),
                    b"current state!",
                    "fail_at={fail_at}"
                );
                let root = s.snapshot_root(b"main", snap).unwrap();
                assert_eq!(
                    s.read_at_root(&root, &p(&["keepme"])).unwrap().unwrap(),
                    b"pinned by snap",
                    "fail_at={fail_at}"
                );
                assert_eq!(s.read(b"main", &p(&["junk"])).unwrap(), None);
            };
            check(&rec);
            // A clean GC after recovery converges and leaves data intact.
            rec.gc().unwrap();
            check(&rec);
        }
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 64 },
            ..ProptestConfig::default()
        })]
        /// The CommitProtocol headline invariant against real bytes: after
        /// a crash at an arbitrary point (power cut mid-operation, torn
        /// unflushed writes), every acknowledged mutation is recoverable.
        /// At most the single in-flight unacked mutation is ambiguous.
        /// Selectors 12–15 mix in maintenance ops (sync, snapshot,
        /// snapshot deletion, GC) so the crash point can land anywhere in
        /// the GC cycle too — none of them may change logical state.
        #[test]
        fn crash_recovery_preserves_acked_state(
            ops in proptest::collection::vec(
                (0u8..16, 0u64..400, proptest::collection::vec(any::<u8>(), 1..96), any::<bool>()),
                1..50,
            ),
            fail_at in 4u64..600,
            crash_seed in any::<u64>(),
        ) {
            let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
            store.create_ref(b"main").unwrap();
            store.dev_mut().set_fail_after(fail_at);

            // Acked logical state; the failing op (if any) is the one
            // ambiguous mutation.
            let mut model: std::collections::HashMap<Path, Option<Vec<u8>>> =
                std::collections::HashMap::new();
            let mut inflight: Option<(Path, Option<Vec<u8>>)> = None;

            let dirs = ["d1", "d2"];
            for (sel, off, data, is_unlink) in &ops {
                if *sel >= 12 {
                    let r = match sel {
                        12 => store.sync_all(),
                        13 => store
                            .snapshot(b"main", b"prop", b"", disk::CLASS_AUTO, *off)
                            .map(|_| ()),
                        14 => {
                            let oldest = store.snapshots(b"main").next().map(|r| r.id);
                            match oldest {
                                Some(id) => store.delete_snapshot(b"main", id),
                                None => Ok(()),
                            }
                        }
                        _ => store.gc().map(|_| ()),
                    };
                    // Maintenance never changes logical content, so a
                    // power cut inside one is unambiguous for the model.
                    if r.is_err() {
                        break;
                    }
                    continue;
                }
                let path = if sel % 2 == 0 {
                    p(&[&format!("f{}", sel % 4)])
                } else {
                    p(&[dirs[(sel % 2) as usize], &format!("f{}", sel % 4)])
                };
                if *is_unlink {
                    let next = None;
                    let r = store.unlink(b"main", &path, 1);
                    if r.is_ok() {
                        model.insert(path, next);
                    } else {
                        inflight = Some((path, next));
                        break;
                    }
                } else {
                    let mut content = model
                        .get(&path)
                        .cloned()
                        .flatten()
                        .unwrap_or_default();
                    let end = *off as usize + data.len();
                    if content.len() < end {
                        content.resize(end, 0);
                    }
                    content[*off as usize..end].copy_from_slice(data);
                    let next = Some(content);
                    let r = store.write(b"main", &path, *off, data, 1);
                    if r.is_ok() {
                        model.insert(path, next);
                    } else {
                        inflight = Some((path, next));
                        break;
                    }
                }
            }

            let mut dev = store.into_dev();
            dev.clear_fail();
            dev.crash(crash_seed);
            let recovered = Store::mount(dev, test_opts()).unwrap();

            for (path, expect) in &model {
                let got = recovered.read(b"main", path).unwrap();
                let matches_model = got == *expect;
                let matches_inflight = inflight
                    .as_ref()
                    .is_some_and(|(ip, iv)| ip == path && got == *iv);
                prop_assert!(
                    matches_model || matches_inflight,
                    "path {:?}: got {:?}, want {:?} (inflight: {:?})",
                    path, got, expect, inflight
                );
            }
        }
    }

    /// rev0§2.6: pre-v3 images are re-created with mkfs, and that stance is
    /// only mechanical if an intact old-version image is refused with a
    /// *version* error — tick and nanosecond timestamp fields are
    /// structurally identical, so nothing else stands between an old
    /// image and being misread as dates in 1970.
    #[test]
    fn old_format_version_is_refused_with_a_version_error() {
        use crate::disk::{SB_A_OFF, SB_B_OFF};

        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"data", 1).unwrap();
        store.snapshot(b"main", b"t", b"image", disk::CLASS_KEEP, 100).unwrap();
        let dev = store.into_dev();
        let mut img = vec![0u8; dev.len() as usize];
        dev.read(0, &mut img).unwrap();
        // Re-stamp both slots as format v2 with valid checksums: the
        // dangerous artifact is intact and plausible, not torn.
        for off in [SB_A_OFF as usize, SB_B_OFF as usize] {
            img[off + 8..off + 12].copy_from_slice(&2u32.to_le_bytes());
            let sum = Hash::of(&img[off..off + disk::SB_BODY]);
            img[off + disk::SB_BODY..off + disk::SB_BODY + 32].copy_from_slice(sum.as_bytes());
        }
        let err = Store::mount(MemDev::from_bytes(img), test_opts())
            .err()
            .expect("a v2 image must not mount");
        assert!(matches!(err, StoreError::UnsupportedVersion(2)), "got {err:?}");
    }

    /// rev0§4.7: `ts = max(now, predecessor_ts + 1)` — an RTC that regressed
    /// between boots can never disorder a ref's snapshot log.
    #[test]
    fn snapshot_timestamps_are_strictly_monotone_per_ref() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"x", 1).unwrap();
        let a = store.snapshot(b"main", b"t", b"first", disk::CLASS_KEEP, 1000).unwrap();
        // The clock went backwards; order must survive anyway.
        let b = store.snapshot(b"main", b"t", b"second", disk::CLASS_KEEP, 500).unwrap();
        // Same instant as the first: still strictly after its parent.
        let c = store.snapshot(b"main", b"t", b"third", disk::CLASS_KEEP, 1000).unwrap();
        let rows: Vec<(u64, u64)> =
            store.snapshots(b"main").map(|r| (r.id, r.timestamp)).collect();
        assert_eq!(rows, vec![(a, 1000), (b, 1001), (c, 1002)]);
    }
}
