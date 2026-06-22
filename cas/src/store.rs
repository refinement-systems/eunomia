//! The storage engine (rev1§4.3-4.7): memtable + WAL + flush + the A/B
//! superblock commit, with crash recovery and GC. This is the code the
//! CommitProtocol TLA+ model models; the crash-injection proptest at the
//! bottom checks the model's headline invariant against the real bytes:
//! after any crash, every acknowledged write is recoverable from durable
//! state alone.
//!
//! Commit is always: fsync chunks (barrier 1) → write new superblock to
//! the older slot → fsync (barrier 2). Nothing is freed on the write
//! path, ever; reclamation is GC's job exclusively (rev1§4.6): `gc` marks
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
//!   - Oversized writes (record > WAL region) bypass the WAL and commit
//!     synchronously before acknowledging — same durability contract.
//!   - The allocator is first-fit over a flat extent list and the tail
//!     high-water mark never retracts (freed space is reusable, but the
//!     region never visibly shrinks). Fine at MVP scale.
//!   - GC is synchronous (rev1§8.3 defers concurrency to Phase C4). The
//!     rev1§4.6 step-3 dedup-resurrection check (`ChunkStore::put`'s
//!     `condemned` consultation) and the birth-generation live-by-fiat filter
//!     (`gc`'s `birth < epoch` clause) are installed and structurally correct
//!     but inert under the synchronous cycle — no flush interleaves a sweep,
//!     so no chunk is condemned-then-resurrected and none is born mid-cycle.
//!     Both become load-bearing when C4 makes GC concurrent.

use crate::chunk::ChunkerParams;
use crate::dev::{BlockDev, DevError};
use crate::disk::{
    self, read_u32_le, read_u64_le, IndexEntry, RefEntry, RefTable, SnapRow, Superblock, WalOp,
    CHUNK_HEADER, SB_A_OFF, SB_B_OFF, SB_SIZE, WAL_HEADER, WAL_OFF,
};
use crate::file::{read_file, store_file, store_file_neighborhood};
use crate::gc;
use crate::hash::Hash;
use crate::overlay::{FileId, FileState, Overlay, Path};
use crate::prolly::{Content, Dir, Entry, EntryKind, FormatError, NodeStore};
use crate::tree;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec;
use alloc::vec::Vec;
use vstd::prelude::*;

/// Minimal chunk region a freshly-`format`ted device must hold beyond the WAL,
/// for the initial ref-table object and durable index frame (rev1§4.5 geometry
/// floor). A device that does not clear this is refused, not panicked; a write
/// that nonetheless overruns the device surfaces as a `DevError`, also no panic.
const MIN_CHUNK_REGION: u64 = 4096;

#[derive(Debug)]
pub enum StoreError {
    Io(DevError),
    Format(FormatError),
    NoSuperblock,
    /// An intact superblock from another format version. Old images are
    /// re-created with mkfs, never migrated or reinterpreted (rev1§2.6).
    UnsupportedVersion(u32),
    NoSuchRef,
    NoSuchSnapshot,
    /// An id-addressed op (`write_id`/`read_id`/`close`) named a [`FileId`] that
    /// is not an open handle (rev1§4.9 / C2C): never opened, already closed, or
    /// gone after a crash (handles do not survive a remount).
    NoSuchHandle,
    NotAFile,
    Corrupt(&'static str),
    NoSpace,
    /// The snapshot is a tag target; tags are keep-strength pins (rev1§4.7).
    Pinned,
    /// A guarded ref-table batch's `expected_version` no longer matches the
    /// ref's current edit version (rev1§4.7): a concurrent mutation advanced it
    /// between the caller's enumerate and its `apply_batch`. Carries the
    /// current version so the caller can re-read and retry; the batch made no
    /// mutation and did not commit.
    VersionMismatch {
        current: u64,
    },
    /// Write extent overflows u64 or exceeds the chunk region capacity.
    WriteOutOfRange,
    /// `format` was handed a device too small to hold the requested geometry —
    /// the two superblock slots, the WAL region, and a minimal chunk region
    /// (rev1§4.5: `format` is total over device *geometry*, refusing an
    /// undersized or un-layoutable device with an error, never a panic).
    DeviceTooSmall,
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
                write!(
                    f,
                    "unsupported format version {v} (re-create the image with mkfs)"
                )
            }
            StoreError::NoSuchRef => write!(f, "no such ref"),
            StoreError::NoSuchSnapshot => write!(f, "no such snapshot"),
            StoreError::NoSuchHandle => write!(f, "no such open handle"),
            StoreError::NotAFile => write!(f, "not a file"),
            StoreError::Corrupt(w) => write!(f, "corrupt store: {w}"),
            StoreError::NoSpace => write!(f, "chunk region full"),
            StoreError::Pinned => write!(f, "snapshot pinned by a tag"),
            StoreError::VersionMismatch { current } => {
                write!(f, "ref edit version mismatch (current {current})")
            }
            StoreError::WriteOutOfRange => write!(f, "write extent out of range"),
            StoreError::DeviceTooSmall => {
                write!(f, "device too small for the requested geometry")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for StoreError {}

/// One edit in a guarded ref-table batch (rev1§4.7). The vocabulary is
/// row + tag surgery — exactly the mutations a retention daemon issues ("mark
/// survivors `keep`, then run the policy") — and deliberately excludes data
/// writes (rev1§4.4 stays last-write-wins) and head moves (`Rollback` is a
/// flush-bearing operation of its own; a concurrent head move still advances
/// the edit version, so it invalidates a batch, but it is not itself a
/// batchable edit). Snapshot ids and tag names are scoped to the one ref the
/// batch targets. Serialized over the wire by the storage server; carried into
/// `Store::apply_batch`, the single authority that validates and applies them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
pub enum RefEdit {
    /// Drop a snapshot row (rev1§4.6): fails `Pinned` if a tag points at it,
    /// `NoSuchSnapshot` if absent; survivors re-parent to the grandparent.
    DeleteSnapshot { id: u64 },
    /// Edit a snapshot's retention class (the "mark survivors `keep`" flow).
    SetClass { id: u64, class: u8 },
    /// Re-point a snapshot's parent (history surgery, rev1§4.6).
    SetParent { id: u64, parent: Option<u64> },
    /// Replace a snapshot's commit message.
    SetMessage { id: u64, message: Vec<u8> },
    /// Pin a snapshot under a tag name (rev1§4.7); the tag maps to the snapshot
    /// id, so it survives metadata edits.
    CreateTag { name: Vec<u8>, snap_id: u64 },
    /// Remove a tag, unpinning its snapshot.
    DeleteTag { name: Vec<u8> },
}

#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    pub wal_len: u64,
    pub chunker: ChunkerParams,
    /// Global dirty-overlay byte budget (rev1§4.4 high watermark): the sum of
    /// every per-ref overlay may not exceed it. Crossing it forces a flush —
    /// the "global budget exists because memory is finite" half of the policy.
    pub global_budget: usize,
    /// Per-ref soft byte bound (rev1§4.4 containment): a single ref's overlay
    /// flushes once it exceeds this size, so one ref cannot consume the whole
    /// `global_budget` (the per-ref soft quota under the global budget). The
    /// recommended default (8 MiB) is shipped by B12F; this struct only carries
    /// it as a tunable number — the mechanism, not the figure, is mandatory.
    pub per_ref_budget: usize,
    /// Per-ref operation-count secondary bound (rev1§4.4): a ref also flushes
    /// once it has accumulated this many unflushed mutating ops, so a metadata
    /// storm whose dirty *bytes* stay small cannot hide under the byte budget.
    pub op_count_bound: u64,
    /// Size-pressure low watermark (rev1§4.4): flushing the biggest offenders
    /// starts here, below the high watermark (`global_budget`), so writers
    /// rarely hit the high one. Consumed by B12B; carried here as the substrate
    /// (stubbed equal to `global_budget` until then).
    pub size_low_watermark: usize,
    /// WAL-usage watermark that triggers flush-the-pinner (rev1§4.4, the
    /// recommended 50% of `wal_len`). Consumed by B12C's circular ring; carried
    /// here as the substrate (stubbed equal to `wal_len` until then).
    pub wal_watermark: u64,
    /// Staleness bound in nanoseconds (rev1§4.4 timer trigger): a quietly dirty
    /// ref eventually becomes committed tree once its oldest-dirty age exceeds
    /// this. Consumed by B12D; carried here as the substrate (stubbed to no
    /// staleness flush until then).
    pub staleness_ns: u64,
}

impl Default for StoreOptions {
    fn default() -> Self {
        // The rev1§4.4 recommended defaults (S-9, spec line 266): the *triggers
        // and bounds* are mandatory mechanisms (shipped by B12A–B12D); these
        // *numbers* are the recommended figures a store may tune. The storage
        // server (`storaged`) mounts with this `default()`, so it *is* "the
        // storage server's shipped configuration" the spec says matches the
        // table. (`mount` overrides only `wal_len` from the on-disk superblock;
        // the memory budgets come from these opts.)
        let global_budget = 128 * 1024 * 1024; // global high watermark
        let wal_len = 64 * 1024 * 1024;
        StoreOptions {
            wal_len,
            chunker: ChunkerParams::DEFAULT,
            global_budget,
            per_ref_budget: 8 * 1024 * 1024, // per-ref soft bound (containment)
            // No spec number for the op-count secondary bound: a few-thousand-op
            // ceiling that catches a metadata storm whose dirty *bytes* stay well
            // under `per_ref_budget`. Tunable like the rest.
            op_count_bound: 8192,
            // Flush-the-biggest-offenders starts here, below the high watermark
            // (`global_budget`), so writers rarely hit it — B12B's 3/4 fraction.
            size_low_watermark: global_budget / 4 * 3,
            wal_watermark: wal_len / 2, // flush-the-pinner at 50% (M-5)
            staleness_ns: 30 * 1_000_000_000, // 30 s staleness bound (M-6 timer)
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
    /// rev1§4.6 step 3 / §8.3: the *exact deletion-candidate list* for the
    /// in-flight GC sweep. In-memory and transient — empty at every commit
    /// boundary, populated only inside a `gc()` sweep window, never
    /// serialized. While non-empty, `put` treats an index hit on one of these
    /// hashes as a miss and rewrites the chunk (the single GC/mutator
    /// interaction point, rev1§4.6). Inert under synchronous GC (no put
    /// interleaves a sweep); load-bearing once C4 makes GC concurrent.
    condemned: BTreeSet<Hash>,
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
        let need = (CHUNK_HEADER + disk::index_payload_len(self.index.len(), bound_extents)) as u64;
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
        // Every layer self-verifies (rev1§4.8).
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
        // Dedup (rev1§4.3): a *live* index hit returns the existing object.
        // rev1§4.6 step 3 (dedup-resurrection): an index hit on a chunk the
        // in-flight sweep has condemned is treated as a miss and rewritten
        // below, so all GC/mutator interaction is confined to this one point.
        // The `is_empty` short-circuit keeps the hot path free of the set
        // lookup whenever no sweep is in flight. Inert under synchronous GC;
        // load-bearing once C4 makes GC concurrent.
        if self.index.contains_key(&hash)
            && (self.condemned.is_empty() || !self.condemned.contains(&hash))
        {
            return hash;
        }
        // A true miss, or a resurrected condemned hit: cancel its condemnation
        // and rewrite under the same hash at the current `birth_gen` (>= the
        // GC epoch, so the rewrite is never re-condemned this cycle). The old
        // condemned extent is still freed by the sweep (via `pending_free`),
        // and the index no longer points at it after this replace, so there is
        // no double-reference and no early reuse (rev1§4.2 deferred-reuse law).
        self.condemned.remove(&hash);
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

// ── The recovery decision core (rev1§4.8) ─────────────────────────────────
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
/// arms in `Store::mount` (rev1§4.5), one variant per arm of the original control
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

/// Which superblock slot the next commit writes (the A/B alternation, rev1§4.2).
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
/// log drains (rev1§4.4). **Total** ∀ records, **terminating**. Pure sequence
/// reasoning — the prefix-scan kcore already did for the channel FIFO head.
///
/// Out of scope here (a Store-level concern, not a property of this pure
/// function): nothing this function does assumes WAL offsets are monotonic.
/// Under the B12C circular ring they are *not* — `wal_tail` wraps, so a later
/// record can sit at a lower physical offset than an earlier one, and the head
/// this returns can move "backwards" in raw-offset terms when it crosses the
/// wrap. That is sound because this function only **selects** a record's `off`
/// (it copies `records[i].off`); it never adds, compares, or orders offsets.
/// The per-piece contract below is precondition-free and justifies the
/// extraction over the rotated/linearized view `recover_records` rebuilds.
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

// ── Recovery walk (rev1§4.8) ──────────────────────────────────────────────
//
// The recovery-path dual of `advance_head`: from the committed head, how much of
// the WAL is a valid recoverable run. `Store::mount` (rev1§4.5) reads contiguous,
// checksummed, seq-continuous records until the first torn or seq-discontinuous
// one (an unacked tail). The *decision* of that walk — the records, the resulting
// `(wal_tail, wal_seq)`, and (B7C) the proof the run is `laid_out` — lives in the
// verified `recover_records` core (below, in the gap-freedom block), leaving the
// overlay apply + the content-level extent gate as the plain-Rust applier: a
// verified parser, a plain-Rust applier.
//
// The framing parse (`decode_frame`) is **verified** — its in-bounds guarantee
// is what makes the walk terminate and stay in range, the unbounded form of an
// `off += rlen` in-bounds argument. The blake3 checksum stays the **content seam**
// (`wal_checksum_ok`, `external_body`) — the same boundary drawn for the
// superblock; the `WalOp` structural decode is verified (`wal_struct_ok`, B7B).

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

// ── The record content seam, split (rev1§3.7/§6.1(e), T-5) ─────────────────
//
// `decode_record`'s post-framing acceptance folds three things into one
// `is_some()`: framing (already verified by `decode_frame`/`frame_at`), the
// **blake3 record checksum** (interpreted hashing — irreducibly trusted, the
// same seam class as `checksum_ok`), and the **structural payload decode**
// (`WalOp::decode_payload` — a bounded, total tag-dispatch + length-prefixed
// walk). B7B pulls the structural half into the verified surface so blake3 is
// the *only* uninterpreted part of the record seam:
//   content_ok_spec(rec) == wal_payload_struct_ok_spec(rec)   [verified]
//                        && checksum_ok_spec(rec)             [trusted: blake3]
// The spec `s_*` mirror of `decode_payload` is interpreted; `wal_struct_ok`
// proves the exec walk equals it ∀ bytes (the `decode_frame`/`frame_at`
// shape). `content_ok_spec` keeps its name/meaning, so `run_len`/`laid_out`/
// `recover_records` and the gap-freedom lemmas are unchanged.

/// One bounded read from `pay` at `pos`: `Some(pos + n)` iff `n` bytes remain,
/// else `None` — the spec mirror of `Reader::take` (`prolly.rs`). Total.
spec fn s_take(pay: Seq<u8>, pos: int, n: int) -> Option<int> {
    if 0 <= pos && 0 <= n && pos + n <= pay.len() {
        Some(pos + n)
    } else {
        None
    }
}

/// A length-prefixed path of `count` components from `pos`: each component is a
/// `u8` length byte followed by that many bytes (the `take_path` closure in
/// `WalOp::decode_payload`). `Some(end)` iff all `count` fit; `None` on the
/// first truncation. Terminating (`decreases count`).
spec fn s_path(pay: Seq<u8>, pos: int, count: int) -> Option<int>
    decreases count,
{
    if count <= 0 {
        Some(pos)
    } else {
        match s_take(pay, pos, 1) {
            None => None,
            Some(p1) => match s_take(pay, p1, pay[pos] as int) {
                None => None,
                Some(p2) => s_path(pay, p2, count - 1),
            },
        }
    }
}

/// The payload region structurally decodes and is *exactly* consumed — the
/// interpreted mirror of `WalOp::decode_payload`: a tag byte (1 = Write,
/// 2 = Unlink), then for Write `ref_name`/`path`/`offset`/`mtime`/`data`
/// (the data length a `u32`), for Unlink `ref_name`/`path`/`mtime`, with the
/// final cursor at the end (`Reader::done`). Verified-equal to the exec walk by
/// [`wal_struct_ok`]; no longer trusted.
spec fn s_payload_ok(pay: Seq<u8>) -> bool {
    match s_take(pay, 0, 1) {
        None => false,
        Some(p_tag) => {
            let tag = pay[0];
            if tag == 1u8 {
                // Write: rl·ref_name, path, offset u64, mtime u64, dl u32·data.
                match s_take(pay, p_tag, 1) {
                    None => false,
                    Some(p_rl) => match s_take(pay, p_rl, pay[p_tag] as int) {
                        None => false,
                        Some(p_ref) => match s_take(pay, p_ref, 1) {
                            None => false,
                            Some(p_pc) => match s_path(pay, p_pc, pay[p_ref] as int) {
                                None => false,
                                Some(p_path) => match s_take(pay, p_path, 8) {
                                    None => false,
                                    Some(p_off) => match s_take(pay, p_off, 8) {
                                        None => false,
                                        Some(p_mt) => match s_take(pay, p_mt, 4) {
                                            None => false,
                                            Some(p_dl) => match s_take(
                                                pay,
                                                p_dl,
                                                disk::spec_u32_le(pay, p_mt) as int,
                                            ) {
                                                None => false,
                                                Some(p_data) => p_data == pay.len(),
                                            },
                                        },
                                    },
                                },
                            },
                        },
                    },
                }
            } else if tag == 2u8 {
                // Unlink: rl·ref_name, path, mtime u64.
                match s_take(pay, p_tag, 1) {
                    None => false,
                    Some(p_rl) => match s_take(pay, p_rl, pay[p_tag] as int) {
                        None => false,
                        Some(p_ref) => match s_take(pay, p_ref, 1) {
                            None => false,
                            Some(p_pc) => match s_path(pay, p_pc, pay[p_ref] as int) {
                                None => false,
                                Some(p_path) => match s_take(pay, p_path, 8) {
                                    None => false,
                                    Some(p_mt) => p_mt == pay.len(),
                                },
                            },
                        },
                    },
                }
            } else {
                false
            }
        }
    }
}

/// The record's payload (everything past the 48-byte header) structurally
/// decodes — the **verified** half of [`content_ok_spec`]. The payload is the
/// tail `rec[WAL_HEADER..]` because `decode_record` slices `buf[WAL_HEADER..
/// WAL_HEADER + len]` over a record whose `rlen == WAL_HEADER + len`.
spec fn wal_payload_struct_ok_spec(rec: Seq<u8>) -> bool {
    &&& rec.len() >= WAL_HEADER
    &&& s_payload_ok(rec.subrange(WAL_HEADER as int, rec.len() as int))
}

/// The blake3 record-checksum gate, uninterpreted — interpreted hashing out of
/// SMT scope (the same seam class as `checksum_ok`, `disk.rs`). [`wal_checksum_ok`]
/// is its `external_body` exec twin. After B7B's structural split this is the
/// **only** uninterpreted part of the record seam (rev1§6.1(e)).
uninterp spec fn checksum_ok_spec(rec: Seq<u8>) -> bool;

/// "This record is content-valid": the structural decode (verified) **and** the
/// blake3 checksum (trusted). Opaque so `run_len`/`laid_out`/`recover_records` and
/// the gap-freedom lemmas keep treating it as a black box exactly as before the
/// split — only [`wal_content_ok`] reveals the conjunction.
#[verifier::opaque]
spec fn content_ok_spec(rec: Seq<u8>) -> bool {
    wal_payload_struct_ok_spec(rec) && checksum_ok_spec(rec)
}

/// Exec twin of [`s_take`]: advance `pos` by `n` if `n` bytes remain. Bounds-
/// checked via `len - pos` (never `pos + n`), so no `usize` overflow — total ∀.
fn e_take(pay: &[u8], pos: usize, n: usize) -> (r: Option<usize>)
    requires
        pos <= pay@.len(),
    ensures
        match r {
            Some(p) => s_take(pay@, pos as int, n as int) == Some(p as int) && p <= pay@.len(),
            None => s_take(pay@, pos as int, n as int) is None,
        },
{
    if pay.len() - pos < n {
        None
    } else {
        Some(pos + n)
    }
}

/// Exec twin of [`s_path`]: walk `count` length-prefixed components from `pos`,
/// proven equal to the spec walk ∀ bytes. Recursive (mirroring `s_path`) so the
/// equality unfolds in lockstep; `decreases count`.
fn e_path(pay: &[u8], pos: usize, count: usize) -> (r: Option<usize>)
    requires
        pos <= pay@.len(),
    ensures
        match r {
            Some(p) => s_path(pay@, pos as int, count as int) == Some(p as int) && p <= pay@.len(),
            None => s_path(pay@, pos as int, count as int) is None,
        },
    decreases count,
{
    broadcast use vstd::slice::group_slice_axioms;
    if count == 0 {
        Some(pos)
    } else {
        match e_take(pay, pos, 1) {
            None => None,
            Some(p1) => {
                let clen = pay[pos] as usize;
                match e_take(pay, p1, clen) {
                    None => None,
                    Some(p2) => e_path(pay, p2, count - 1),
                }
            }
        }
    }
}

/// Exec twin of [`s_payload_ok`]: the structural payload walk as a verified
/// `bool`, proven equal to the spec ∀ bytes (the totality + correctness theorem
/// for the structural half of the record seam, rev1§3.7). Mirrors
/// `WalOp::decode_payload` byte-for-byte but returns acceptance instead of
/// building the `Vec`s (that stays the plain-Rust applier's job, rev1§6.1(e)).
fn e_payload_ok(pay: &[u8]) -> (r: bool)
    ensures
        r == s_payload_ok(pay@),
{
    broadcast use vstd::slice::group_slice_axioms;
    let p_tag = match e_take(pay, 0, 1) {
        None => return false,
        Some(p) => p,
    };
    let tag = pay[0];
    if tag == 1u8 {
        let p_rl = match e_take(pay, p_tag, 1) {
            None => return false,
            Some(p) => p,
        };
        let rl = pay[p_tag] as usize;
        let p_ref = match e_take(pay, p_rl, rl) {
            None => return false,
            Some(p) => p,
        };
        let p_pc = match e_take(pay, p_ref, 1) {
            None => return false,
            Some(p) => p,
        };
        let pc = pay[p_ref] as usize;
        let p_path = match e_path(pay, p_pc, pc) {
            None => return false,
            Some(p) => p,
        };
        let p_off = match e_take(pay, p_path, 8) {
            None => return false,
            Some(p) => p,
        };
        let p_mt = match e_take(pay, p_off, 8) {
            None => return false,
            Some(p) => p,
        };
        let p_dl = match e_take(pay, p_mt, 4) {
            None => return false,
            Some(p) => p,
        };
        let dl = read_u32_le(pay, p_mt) as usize;
        let p_data = match e_take(pay, p_dl, dl) {
            None => return false,
            Some(p) => p,
        };
        p_data == pay.len()
    } else if tag == 2u8 {
        let p_rl = match e_take(pay, p_tag, 1) {
            None => return false,
            Some(p) => p,
        };
        let rl = pay[p_tag] as usize;
        let p_ref = match e_take(pay, p_rl, rl) {
            None => return false,
            Some(p) => p,
        };
        let p_pc = match e_take(pay, p_ref, 1) {
            None => return false,
            Some(p) => p,
        };
        let pc = pay[p_ref] as usize;
        let p_path = match e_path(pay, p_pc, pc) {
            None => return false,
            Some(p) => p,
        };
        let p_mt = match e_take(pay, p_path, 8) {
            None => return false,
            Some(p) => p,
        };
        p_mt == pay.len()
    } else {
        false
    }
}

/// The **verified** structural half of the record seam: does the record's
/// payload region structurally decode? Proven equal to [`wal_payload_struct_ok_spec`]
/// ∀ bytes. The payload is the tail past the 48-byte header (`decode_record`'s
/// `buf[WAL_HEADER..WAL_HEADER + len]` over an `rlen == WAL_HEADER + len`
/// record). `off + rlen <= len` (from `decode_frame`) keeps the slice in bounds.
fn wal_struct_ok(wal: &[u8], off: usize, rlen: usize) -> (r: bool)
    requires
        off + rlen <= wal@.len(),
    ensures
        r == wal_payload_struct_ok_spec(wal@.subrange(off as int, (off + rlen) as int)),
{
    broadcast use vstd::slice::group_slice_axioms;
    // Materialize `wal@.len() <= usize::MAX` so the `off + rlen` slice bound
    // below is overflow-free (the recovery-walk recipe).
    let _glen = wal.len();
    let ghost rec = wal@.subrange(off as int, (off + rlen) as int);
    assert(rec.len() == rlen);
    if rlen < WAL_HEADER {
        return false;
    }
    let pay = &wal[off + WAL_HEADER..off + rlen];
    assert(pay@ =~= rec.subrange(WAL_HEADER as int, rlen as int));
    e_payload_ok(pay)
}

/// The **trusted** half of the record seam: the blake3 record checksum, the lone
/// uninterpreted part after B7B's split. `external_body` because blake3 is
/// interpreted hashing — out of SMT scope, the same boundary as `checksum_ok`
/// (`disk.rs`). Total: inspects the exact-`rlen` record and returns a bool;
/// recomputes the canonical `record_checksum` (`disk.rs`) over `seq‖len‖payload`
/// and compares the stored 32 bytes. `checksum_ok_spec` is its uninterpreted
/// twin. `off + rlen <= len` (from `decode_frame`) keeps the slicing in bounds.
#[verifier::external_body]
fn wal_checksum_ok(wal: &[u8], off: usize, rlen: usize) -> (r: bool)
    requires
        off + rlen <= wal@.len(),
    ensures
        r == checksum_ok_spec(wal@.subrange(off as int, (off + rlen) as int)),
{
    let rec = &wal[off..off + rlen];
    if rec.len() < WAL_HEADER {
        return false;
    }
    let seq = u64::from_le_bytes(rec[4..12].try_into().unwrap());
    let len = u32::from_le_bytes(rec[12..16].try_into().unwrap());
    let payload_len = len as usize;
    if rec.len() - WAL_HEADER < payload_len {
        return false;
    }
    let payload = &rec[WAL_HEADER..WAL_HEADER + payload_len];
    disk::record_checksum(seq, len, payload).as_bytes() == &rec[16..48]
}

/// The content-layer acceptance `decode_record` makes after framing, now a
/// **verified composition** (no longer one `external_body` box): the structural
/// decode (verified, [`wal_struct_ok`]) **and** the blake3 checksum (trusted,
/// [`wal_checksum_ok`]). Equal to [`content_ok_spec`], so `run_len`/`recover_records`
/// reason over the maximal run exactly as before — only the *internal* trust
/// boundary shrank to blake3 (rev1§6.1(e), T-5).
fn wal_content_ok(wal: &[u8], off: usize, rlen: usize) -> (r: bool)
    requires
        off + rlen <= wal@.len(),
    ensures
        r == content_ok_spec(wal@.subrange(off as int, (off + rlen) as int)),
{
    reveal(content_ok_spec);
    wal_struct_ok(wal, off, rlen) && wal_checksum_ok(wal, off, rlen)
}

/// The length of the maximal contiguous recoverable run from `off` at sequence
/// `seq`: each record must frame ([`frame_at`]), continue the sequence, and pass
/// the content seam (`content_ok_spec`); the run ends at the first that does not.
/// The record at `seq == u64::MAX` is *counted* but ends the run (the sequence
/// can't advance) — matching `recover_records`/`mount`'s rev1§4.4 seq-exhaustion
/// gate (the `mnt1_forged_wal_seq_max_rejected` corner; `recover_records` flags it
/// as `forged_max` rather than fold it into the laid-out skeleton). This is the
/// closed form `recover_records`'s `records.len() + forged_max` is proven equal
/// to, and the quantity the gap-freedom composition reasons about. **Terminating**
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

// The recovery walk that *bounds* and *materializes* the run — totality, the
// maximal-run equality `count == run_len`, and the proven `laid_out` skeleton —
// is `recover_records`, in the gap-freedom composition block below (B7C folded
// the former `replay_bound` into it so the recovery decision and its gap-freedom
// guarantee are proven at one site, with no dead proof left behind).

} // verus!

// ── The gap-freedom composition (rev1§4.8) ────────────────────────────────
//
// `advance_head` (write path) and the recovery walk operate on different views —
// a `&[RecMeta]` queue vs. the raw WAL bytes. The composition theorem relates
// them through `laid_out`, the linking invariant that the byte region from a
// record's `off` *is* the record the queue describes (frames at `off` with that
// `seq`, content-valid, contiguous, seq-continuous). Under it the **gap-freedom
// lemma** holds: `advance_head`'s head sits at the first non-flushed record, and
// replay from that head covers the whole suffix — so every acked-but-uncommitted
// (unflushed) record is replayed. This is the code-level shadow of the TLA+
// `AckedWritesRecoverable` (WAL-replay half); the content-coverage half —
// "flushed ⇒ effects already in the committed root" — stays the `CommitProtocol`
// design gate, deliberately out of scope here.
//
// B7C discharges `laid_out` rather than assuming it: `recover_records` *rebuilds*
// the run from the committed head and **proves `laid_out(wal@, records@, 0)`**
// from the per-record framing/content/sequence facts its walk establishes, then
// fires `lemma_gap_freedom` (and `lemma_run_len_covers` / `lemma_laid_out_mono`)
// on that run — so the whole composition is live, its premise proven, not
// documented. `mount` drives its applier off the verified skeleton, so the
// running recovery sequencing is the proved one. What stays trusted is only the
// *lifetime* join — that the live `wal_records` queue keeps matching the bytes as
// write/flush/commit mutate it — the §6.1(c)/(e) Store seam; the full
// replay-equality invariant remains the `CommitProtocol` model's (rev1§6.1(e)).

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
/// with the recovery walk's `count` (`run_len`). The four `requires` after
/// `laid_out` are exactly those functions' `ensures` — so this fires whenever
/// commit/mount call them in sequence over a laid-out queue (discharged by
/// `recover_records`). Conclusion: **every unflushed record lies in the replayed
/// span** `[n_flushed, n_flushed + count)`. With `advance_head` placing `head` at
/// the first non-flushed record (everything below it flushed) and
/// `count == run_len >= len - n_flushed` (coverage), no acked-but-uncommitted
/// write is left behind — the code-level shadow of `AckedWritesRecoverable`'s
/// WAL-replay half.
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
        // recover_records' ensures (Part 1, the maximal-run equality):
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

// ── B7C: discharge `laid_out`; fire the gap-freedom theorem on the running
// recovery decision (T-2 + the mount/commit glue) ──────────────────────────
//
// `laid_out` used to be a *documented* invariant: `lemma_gap_freedom` took it on
// faith and fired nowhere. B7C makes the recovery walk *produce* it.
// `recover_records` rebuilds the maximal seq-continuous, content-valid run from
// the committed head and **proves `laid_out(wal@, records@, 0)`** from the
// per-record framing/content/sequence facts the walk already establishes — so the
// linking invariant is discharged at the one site the recovery decision is made.
// It then fires `lemma_gap_freedom` (and its supports `lemma_run_len_covers` /
// `lemma_laid_out_mono`) on that rebuilt run, making the whole composition live.
// `mount` consumes the verified skeleton to drive its applier, so the running
// recovery sequencing *is* the proved one (the §4.2 glue contract).
//
// What stays trusted is the join across the Store's *lifetime* — that the live
// in-memory `wal_records` queue keeps matching the on-device bytes as
// write/flush/commit mutate it — the same trusted-Store seam §6.1(c)/(e) already
// names; the full replay-equality invariant remains the `CommitProtocol` model's
// (rev1§6.1(e)). The discharge ties the recovery *decision* to the code; it does
// not pull the device I/O or the `WalOp` applier into the verified core.

/// The framing length of the record at `off` (0 if none) — names the `rlen`
/// [`frame_at`] returns so [`recover_records`] can state its cursor invariant.
spec fn frame_rlen(wal: Seq<u8>, off: int) -> nat {
    match frame_at(wal, off) {
        Some((_, rlen)) => rlen,
        None => 0,
    }
}

/// One unfolded level of [`laid_out`]: the record at `k` frames as itself,
/// continues the sequence, passes the content seam, has `seq < u64::MAX`, and
/// (if not last) chains contiguously into `k + 1`. The `forall` over this is the
/// loop-friendly form [`recover_records`] maintains; [`lemma_forall_laid_out`]
/// folds it back into the recursive `laid_out`.
spec fn rec_ok(wal: Seq<u8>, records: Seq<RecMeta>, k: int) -> bool {
    match frame_at(wal, records[k].off as int) {
        None => false,
        Some((s, rlen)) => {
            &&& s == records[k].seq
            &&& records[k].seq < u64::MAX
            &&& content_ok_spec(wal.subrange(records[k].off as int, records[k].off as int + rlen))
            &&& (k + 1 < records.len() ==> records[k + 1].off as int == records[k].off as int + rlen)
            &&& (k + 1 < records.len() ==> records[k + 1].seq == (records[k].seq + 1) as u64)
        }
    }
}

/// The forall-form implies the recursive form: if every record from `k` is
/// `rec_ok`, the suffix is `laid_out`. Straight induction down the suffix.
proof fn lemma_forall_laid_out(wal: Seq<u8>, records: Seq<RecMeta>, k: int)
    requires
        0 <= k <= records.len(),
        forall|j: int| k <= j < records.len() ==> rec_ok(wal, records, j),
    ensures
        laid_out(wal, records, k),
    decreases records.len() - k,
{
    if k < records.len() {
        assert(rec_ok(wal, records, k));
        lemma_forall_laid_out(wal, records, k + 1);
    }
}

/// The recovery span plus the rebuilt record skeleton. `records` is the maximal
/// seq-continuous, content-valid run from the head with `seq < u64::MAX` — the
/// records `mount` replays — and is *proven* `laid_out`. `forged_max` flags an
/// otherwise-valid record at `seq == u64::MAX` right after the run: the rev1§4.4
/// seq-exhaustion forgery `mount` rejects (the run can't extend past it).
/// `end_off`/`next_seq` are the post-run cursor for the new WAL tail/seq.
struct Recovered {
    records: Vec<RecMeta>,
    end_off: u64,
    next_seq: u64,
    forged_max: bool,
}

/// Rebuild the recoverable run from `wal_head` and **prove it is laid out** —
/// discharging what `lemma_gap_freedom` used to assume. The verified recovery
/// core (B7C folded the former `replay_bound` bound into it): it walks the WAL
/// (framing via `decode_frame`, the content seam via `wal_content_ok`, seq
/// continuity), proves the **maximal-run equality** (`run_len`), *and*
/// materializes the records so the linking invariant `laid_out` is established
/// where the recovery decision is made. Then it fires the gap-freedom theorem on
/// the rebuilt run (the in-code shadow of `AckedWritesRecoverable`'s WAL-replay
/// half). `mount` drives its applier off the returned skeleton.
fn recover_records(wal: &[u8], wal_head: u64, wal_next_seq: u64) -> (r: Recovered)
    requires
        wal_head <= wal@.len(),
    ensures
        // The maximal-run equality `replay_bound` used to prove, now keyed to the
        // rebuilt records: the run accounts for exactly `run_len` (the `forged_max`
        // record, if any, is the one counted past the laid-out skeleton).
        run_len(wal@, wal_head as int, wal_next_seq)
            == r.records@.len() + (if r.forged_max { 1nat } else { 0nat }),
        r.end_off <= wal@.len(),
        // The rebuilt run is laid out (the discharged `lemma_gap_freedom` premise),
        // all unflushed (just replayed), and starts at the committed head.
        laid_out(wal@, r.records@, 0),
        forall|k: int| 0 <= k < r.records@.len() ==> !(#[trigger] r.records@[k]).flushed,
        r.records@.len() > 0 ==> r.records@[0].off == wal_head && r.records@[0].seq == wal_next_seq,
{
    broadcast use vstd::slice::group_slice_axioms;
    let ghost total = run_len(wal@, wal_head as int, wal_next_seq);
    let wlen = wal.len();
    let mut records: Vec<RecMeta> = Vec::new();
    let mut off: usize = wal_head as usize;
    let mut seq: u64 = wal_next_seq;
    let mut forged_max = false;
    assert(off as int == wal_head as int);
    loop
        invariant_except_break
            records@.len() + run_len(wal@, off as int, seq) == total,
            !forged_max,
        invariant
            wlen == wal@.len(),
            off <= wal@.len(),
            forall|k: int| 0 <= k < records@.len() ==> rec_ok(wal@, records@, k),
            forall|k: int| 0 <= k < records@.len() ==> !(#[trigger] records@[k]).flushed,
            records@.len() == 0 ==> (off as int == wal_head as int && seq == wal_next_seq),
            records@.len() > 0 ==> records@[0].off == wal_head && records@[0].seq == wal_next_seq,
            records@.len() > 0 ==> {
                &&& off as int == records@[records@.len() - 1].off as int
                        + frame_rlen(wal@, records@[records@.len() - 1].off as int)
                &&& seq as int == records@[records@.len() - 1].seq as int + 1
            },
        ensures
            off <= wal@.len(),
            total == records@.len() + (if forged_max { 1nat } else { 0nat }),
            forall|k: int| 0 <= k < records@.len() ==> rec_ok(wal@, records@, k),
            forall|k: int| 0 <= k < records@.len() ==> !(#[trigger] records@[k]).flushed,
            records@.len() > 0 ==> records@[0].off == wal_head && records@[0].seq == wal_next_seq,
        decreases wal@.len() - off,
    {
        if off >= wlen {
            // off == wal.len(): the run from here is empty.
            assert(run_len(wal@, off as int, seq) == 0);
            break;
        }
        let frame = match decode_frame(wal, off) {
            Some(f) => f,
            None => {
                assert(run_len(wal@, off as int, seq) == 0);
                break;
            }
        };
        if frame.seq != seq {
            assert(run_len(wal@, off as int, seq) == 0);
            break;
        }
        if !wal_content_ok(wal, off, frame.rlen) {
            assert(run_len(wal@, off as int, seq) == 0);
            break;
        }
        if seq == u64::MAX {
            // An otherwise-valid record at the seq ceiling: the run can't extend
            // (no seq + 1), so stop and flag the forgery for `mount` to reject.
            // `run_len` *counts* it (1), so `total == records.len() + 1`.
            assert(run_len(wal@, off as int, seq) == 1);
            forged_max = true;
            break;
        }
        // Accepted (seq < MAX): unfold `run_len` once so the accumulator carries
        // across the step (the maximal-run accumulator recipe).
        assert(run_len(wal@, off as int, seq)
            == 1 + run_len(wal@, (off + frame.rlen) as int, (seq + 1) as u64));
        let ghost prev = records@;
        records.push(RecMeta { seq, off: off as u64, ref_name: Vec::new(), flushed: false });
        proof {
            let r = records@;
            let n = prev.len() as int;
            assert(r.len() == n + 1);
            assert(r[n].off == off as u64 && r[n].seq == seq && !r[n].flushed);
            // decode_frame tied the exec frame to frame_at; wal_content_ok tied the
            // accept to content_ok_spec — the facts the new last record needs.
            assert(frame_at(wal@, off as int) == Some((frame.seq, frame.rlen as nat)));
            assert(content_ok_spec(wal@.subrange(off as int, off as int + frame.rlen as nat)));
            assert forall|k: int| 0 <= k < r.len() implies rec_ok(wal@, r, k) by {
                if k < n {
                    // Unchanged records keep their framing/content; only the previous
                    // last record (k == n - 1) gains a contiguity clause, discharged
                    // by the cursor invariant (off / seq sit just past record n - 1).
                    assert(r[k] == prev[k]);
                    assert(rec_ok(wal@, prev, k));
                    if k + 1 == r.len() - 1 {
                        assert(r[k + 1] == r[n]);
                        assert(off as int == prev[k].off as int + frame_rlen(wal@, prev[k].off as int));
                    } else if k + 1 < n {
                        assert(r[k + 1] == prev[k + 1]);
                    }
                } else {
                    // k == n: the freshly pushed record frames and is content-valid;
                    // its contiguity clause is vacuous (it is last).
                    assert(r[k] == r[n]);
                }
            }
        }
        off = off + frame.rlen;
        seq = seq + 1;
    }
    // The rebuilt run is laid out, and the gap-freedom theorem fires on it: every
    // record sits within the replay count (here the whole run, all unflushed) —
    // lemma_gap_freedom + lemma_run_len_covers + lemma_laid_out_mono are now live,
    // their `laid_out` premise discharged above.
    proof {
        lemma_forall_laid_out(wal@, records@, 0);
        if records@.len() > 0 {
            let count = run_len(wal@, wal_head as int, wal_next_seq) as int;
            lemma_gap_freedom(wal@, records@, 0, wal_head, wal_next_seq, count);
        }
    }
    Recovered { records, end_off: off as u64, next_seq: seq, forged_max }
}

} // verus!

/// Per-ref flush-scheduler accounting (rev1§4.4), riding alongside `overlays`.
/// Every field is *derived* from the ref's currently-unflushed records, so it
/// is reconstructed at mount during WAL replay and never persisted — B12 is
/// format-stable (no `SB_VERSION` bump, no corpus regen). Reset when the ref
/// flushes (its overlay becomes immutable tree). The dirty *byte* count is not
/// duplicated here — it already lives in `Overlay::bytes()`.
#[derive(Debug, Default)]
struct RefAcct {
    /// Mutating ops applied to this ref since its last flush — the op-count
    /// secondary bound (rev1§4.4), so a metadata storm with tiny bytes flushes.
    op_count: u64,
    /// Physical WAL offset of this ref's oldest unflushed record (rev1§4.4). Set
    /// by the first op after a flush, untouched after; `None` while clean. For
    /// the tail-pinner this equals the live `wal_head`. B12C does **not** pick
    /// the pinner by sorting on this field — that is wrong across the ring wrap
    /// (a newer ref can sit at a lower offset than the tail-pinner). The runtime
    /// pinner is the front of `wal_records` (the record at `wal_head`); this
    /// field stays as the per-ref position datum the tests assert against.
    oldest_wal_pos: Option<u64>,
    /// UTC-nanos timestamp at which this ref first became dirty: the staleness
    /// trigger's age key (rev1§4.4). `None` while clean. Consumed by B12D.
    oldest_dirty_ns: Option<u64>,
}

pub struct Store<D: BlockDev> {
    chunks: ChunkStore<D>,
    opts: StoreOptions,
    /// Last committed superblock and the slot it lives in (A = false).
    sb: Superblock,
    sb_in_b: bool,
    /// Working ref table: committed state + flushed-but-uncommitted roots
    /// and staged row edits. Serialized at commit.
    table: RefTable,
    /// Refs whose entry-set changed since the last commit. Drained at the top
    /// of `commit()`, advancing each named ref's rev1§4.7 edit version exactly
    /// once — so N staged edits on one ref (a guarded batch) tick the version
    /// once, and a no-op commit ticks nothing.
    dirty_refs: BTreeSet<Vec<u8>>,
    overlays: BTreeMap<Vec<u8>, Overlay>,
    /// Per-ref flush-scheduler accounting (rev1§4.4), keyed like `overlays`.
    /// Derived runtime state, reconstructed on WAL replay (B12 is format-stable).
    acct: BTreeMap<Vec<u8>, RefAcct>,
    wal_tail: u64,
    wal_seq: u64,
    wal_records: VecDeque<RecMeta>,
    /// Monotonic allocator for ephemeral overlay file ids (rev1§4.9). Store-
    /// global, never persisted, and re-derived deterministically by WAL replay,
    /// which re-runs the same op stream through `apply_to_overlay`.
    next_file_id: FileId,
    /// Open file handles (rev1§4.9 / C2C): `FileId → ref_name`, locating which
    /// ref's overlay an id-addressed `write_id`/`read_id`/`close` acts on. The
    /// id↔name binding lives in that overlay's `names`; this only routes by ref.
    /// Like the ids, it is ephemeral — never persisted, empty after a crash (no
    /// open/close WAL record), so replay restores no handles (Design decision 2).
    open_files: BTreeMap<FileId, Vec<u8>>,
}

impl<D: BlockDev> Store<D> {
    // ── Lifecycle ───────────────────────────────────────────────────

    pub fn format(mut dev: D, opts: StoreOptions) -> Result<Store<D>, StoreError> {
        // rev1§4.5: `format` is total over device *geometry* — validate the
        // requested layout against the device *before writing anything*, and
        // refuse an undersized or un-layoutable device with an error, never a
        // panic. (Mount is total over device *contents*; this is its geometry
        // twin.) `checked_add` keeps the check total even for a hostile
        // `wal_len` near `u64::MAX`, which a bare add would wrap into a false
        // pass. The threshold is the old `assert!`'s: room for the two
        // superblock slots (folded into `WAL_OFF`), the WAL region, and a
        // minimal chunk region for the initial ref-table + index frame.
        let chunk_off = WAL_OFF
            .checked_add(opts.wal_len)
            .ok_or(StoreError::DeviceTooSmall)?;
        let min_dev = chunk_off
            .checked_add(MIN_CHUNK_REGION)
            .ok_or(StoreError::DeviceTooSmall)?;
        if dev.len() <= min_dev {
            return Err(StoreError::DeviceTooSmall);
        }
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
            condemned: BTreeSet::new(),
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
            dirty_refs: BTreeSet::new(),
            overlays: BTreeMap::new(),
            acct: BTreeMap::new(),
            wal_tail: 0,
            wal_seq: 1,
            wal_records: VecDeque::new(),
            next_file_id: 0,
            open_files: BTreeMap::new(),
        })
    }

    /// Mount = crash recovery (rev1§4.5): both paths are the same code. Read
    /// both slots, discard invalid, take the higher generation; load the
    /// durable index it points at; replay the WAL tail into overlays.
    pub fn mount(dev: D, opts: StoreOptions) -> Result<Store<D>, StoreError> {
        let mut buf_a = vec![0u8; SB_SIZE];
        let mut buf_b = vec![0u8; SB_SIZE];
        dev.read(SB_A_OFF, &mut buf_a)?;
        dev.read(SB_B_OFF, &mut buf_b)?;
        let (ra, rb) = (
            Superblock::decode_checked(&buf_a),
            Superblock::decode_checked(&buf_b),
        );
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
            // rev1§2.6 stance — pre-v3 images are re-created with mkfs — is
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
        sb.validate_geometry(dev.len())
            .map_err(StoreError::Corrupt)?;
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
            condemned: BTreeSet::new(),
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
            opts: StoreOptions {
                wal_len: sb.wal_len,
                ..opts
            },
            sb: sb.clone(),
            sb_in_b,
            table,
            dirty_refs: BTreeSet::new(),
            overlays: BTreeMap::new(),
            acct: BTreeMap::new(),
            wal_tail: sb.wal_head,
            wal_seq: sb.wal_next_seq,
            wal_records: VecDeque::new(),
            next_file_id: 0,
            open_files: BTreeMap::new(),
        };

        // WAL replay: contiguous, checksummed, seq-continuous records from the
        // committed head; anything else is an unacked torn tail. The recovery
        // *decision* — which records replay, the resulting `(wal_tail, wal_seq)`,
        // and the rebuilt `RecMeta` skeleton — is the Verus-verified
        // `recover_records`: total + terminating ∀ bytes, and it **proves the
        // rebuilt run is `laid_out`** (B7C/T-2), discharging the `lemma_gap_freedom`
        // premise on exactly this run. So the sequencing here is the proved one;
        // the applier below decodes + applies each record over that verified
        // skeleton — the content layer (`WalOp` decode) and the extent gate stay
        // plain Rust (rev1§6.1(e)).
        //
        // Circular WAL (B12C, rev1§4.4, M-5): the live window runs from the
        // committed `wal_head` around the ring to the tail and may straddle the
        // buffer end. Rotate the buffer so the head sits at offset 0, linearizing
        // the ring into the contiguous run `recover_records` walks — so the
        // verified decision core is reused **unchanged** (no new Verus; the cas
        // gate holds). `validate_geometry` above guarantees `wal_head <= wal_len`,
        // so `rotate_left` cannot panic and mount stays total over hostile
        // contents. The offsets the walk returns are *rotated*; map them back to
        // physical ring positions with `(wal_head + rotated) mod wal_len` for the
        // live `wal_records`/accounting/tail (the persisted head is already
        // physical). The applier decodes from the *rotated* buffer at the rotated
        // offset, so a straddling record reads contiguously.
        let mut wal = vec![0u8; sb.wal_len as usize];
        store.chunks.dev.read(WAL_OFF, &mut wal)?;
        debug_assert!(sb.wal_head as usize <= wal.len());
        wal.rotate_left(sb.wal_head as usize);
        let recovered = recover_records(&wal, 0, sb.wal_next_seq);
        // `wal_len == 0` (degenerate empty WAL) admits no records, so the mod is
        // never reached for a real offset; guard it so the call is total.
        let to_phys = |rotated: u64| -> u64 {
            if sb.wal_len == 0 {
                0
            } else {
                (sb.wal_head + rotated) % sb.wal_len
            }
        };
        for rec in recovered.records.iter() {
            // `recover_records` proved each rebuilt record frames, checksums, and
            // continues the sequence — so `decode_record` returns `Some` here (the
            // call is total because the verified core's contract says so). `rec.off`
            // is a rotated offset into the rotated buffer, where the record is
            // contiguous even if it physically straddled the wrap.
            let (_rseq, op, _rlen) = WalOp::decode_record(&wal[rec.off as usize..])
                .expect("recover_records accepted this record");
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
            // Reconstruct per-ref flush accounting from the replayed records
            // (rev1§4.4): nothing of it is persisted, so the live state is
            // recomputed here exactly as the write path built it (B12 is
            // format-stable). The accounted position is the record's *physical*
            // ring offset, so `oldest_wal_pos` matches the live write path.
            let phys = to_phys(rec.off);
            store.account_op(&op, phys);
            store.wal_records.push_back(RecMeta {
                seq: rec.seq,
                off: phys,
                ref_name: op.ref_name().to_vec(),
                flushed: false,
            });
        }
        // An otherwise-valid record at the seq ceiling immediately past the run is
        // the rev1§4.4 seq-exhaustion forgery (an honest counter never nears
        // u64::MAX). `recover_records` flags it rather than fold it into the
        // laid-out run; reject it loudly, as the original re-walk's `checked_add` did.
        if recovered.forged_max {
            return Err(StoreError::Corrupt("wal sequence exhausted"));
        }
        // Physical tail: just past the last live record on the ring. When the run
        // fills the whole buffer (`end_off == wal_len`) this maps to `wal_head`,
        // the exactly-full ring `wal_usage` reports as full (not empty).
        store.wal_tail = to_phys(recovered.end_off);
        store.wal_seq = recovered.next_seq;
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
            RefEntry {
                root: empty_root,
                generation: 0,
                next_snap_id: 1,
                // A fresh ref starts at edit version 0; create_ref does not mark
                // it dirty, so its first committed entry-set mutation ticks it
                // to 1 (rev1§4.7).
                edit_version: 0,
            },
        );
        self.commit()
    }

    pub fn refs(&self) -> impl Iterator<Item = (&Vec<u8>, &RefEntry)> {
        self.table.refs.iter()
    }

    /// The ref's current rev1§4.7 edit version (the value a guarded batch's
    /// `expected_version` is compared against), or `None` if the ref is
    /// unknown. Reads the working table, so an enumerate and a follow-up
    /// `expected_version` taken back-to-back see one consistent value.
    pub fn edit_version(&self, ref_name: &[u8]) -> Option<u64> {
        self.table.refs.get(ref_name).map(|e| e.edit_version)
    }

    /// Mark a ref's entry-set as mutated this commit cycle (rev1§4.7). The
    /// version is advanced once in `commit()`, no matter how many times this
    /// is called for the same ref before the flip.
    fn touch_ref(&mut self, name: &[u8]) {
        self.dirty_refs.insert(name.to_vec());
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
    /// hash, rev1§4.4). `now` is UTC nanoseconds from the caller
    /// (server-assigned, rev1§4.7) and is clamped per-ref strictly monotone:
    /// `ts = max(now, predecessor_ts + 1)`. A host clock regressing
    /// between boots can therefore never make a child snapshot "older"
    /// than its parent — the clamp protects exactly what retention needs
    /// (per-ref strict order) and nothing it can't (a wildly wrong RTC
    /// still skews absolute ages, rev1§2.6/rev1§4.7).
    pub fn snapshot(
        &mut self,
        ref_name: &[u8],
        provenance: &[u8],
        message: &[u8],
        class: u8,
        now: u64,
    ) -> Result<u64, StoreError> {
        self.flush_ref(ref_name)?;
        let entry = self
            .table
            .refs
            .get(ref_name)
            .ok_or(StoreError::NoSuchRef)?
            .clone();
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
        self.touch_ref(ref_name); // new snapshot row + next_snap_id (rev1§4.7)
        self.commit()?;
        Ok(id)
    }

    /// Roll the ref head back to a snapshot. Pending overlay writes are
    /// flushed first (into the abandoned pre-rollback root) so the WAL
    /// stays coherent; the rollback then commits the snapshot's root as
    /// the new head. History rewriting at the storage layer is just a
    /// ref-table edit (rev1§4.6).
    pub fn rollback(&mut self, ref_name: &[u8], snap_id: u64) -> Result<(), StoreError> {
        let root = self
            .table
            .snaps
            .get(&(ref_name.to_vec(), snap_id))
            .ok_or(StoreError::NoSuchSnapshot)?
            .root;
        self.flush_ref(ref_name)?;
        self.table
            .refs
            .get_mut(ref_name)
            .ok_or(StoreError::NoSuchRef)?
            .root = root;
        self.touch_ref(ref_name); // head move (rev1§4.7)
        self.commit()
    }

    /// Pin a snapshot under a tag name (rev1§4.7): the tag maps to the
    /// snapshot *id*, so it survives metadata edits, and it is a
    /// `keep`-strength pin — `delete_snapshot` of a tagged snapshot fails
    /// `Pinned`. A tag is an entry-set mutation, so it advances the ref's
    /// edit version (rev1§4.7) like a row edit or a head move.
    pub fn tag(&mut self, name: &[u8], ref_name: &[u8], snap_id: u64) -> Result<(), StoreError> {
        if !self.table.snaps.contains_key(&(ref_name.to_vec(), snap_id)) {
            return Err(StoreError::NoSuchSnapshot);
        }
        self.table
            .tags
            .insert(name.to_vec(), (ref_name.to_vec(), snap_id));
        self.touch_ref(ref_name); // tag added (rev1§4.7 entry-set mutation)
        self.commit()
    }

    /// Remove a tag, unpinning its snapshot (rev1§4.7). Ref-scoped: only a
    /// tag that currently pins a snapshot on `ref_name` is removed, so a
    /// handle on one ref cannot reach across to another ref's tags (the same
    /// confinement `RefEdit::DeleteTag` keeps inside a guarded batch).
    /// Idempotent: removing an absent — or foreign-ref — tag is a no-op that
    /// neither commits nor advances the edit version.
    pub fn untag(&mut self, ref_name: &[u8], name: &[u8]) -> Result<(), StoreError> {
        match self.table.tags.get(name) {
            Some((r, _)) if r.as_slice() == ref_name => {
                self.table.tags.remove(name);
                self.touch_ref(ref_name); // tag removed (rev1§4.7 entry-set mutation)
                self.commit()
            }
            _ => Ok(()),
        }
    }

    /// Enumerate every tag as `(name, ref_name, snap_id)` (rev1§4.7). Reads
    /// the working table; the wire `ListTags` handler scopes the view to the
    /// caller's ref.
    pub fn tags(&self) -> impl Iterator<Item = (&[u8], &[u8], u64)> + '_ {
        self.table
            .tags
            .iter()
            .map(|(name, (r, id))| (name.as_slice(), r.as_slice(), *id))
    }

    // ── Write path (rev1§4.3) ───────────────────────────────────────────

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
                Some(Entry {
                    kind: EntryKind::Dir,
                    ..
                }) if is_final => return Err(StoreError::NotAFile),
                Some(Entry {
                    kind: EntryKind::File,
                    ..
                }) if !is_final => return Err(StoreError::NotAFile),
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

    pub fn unlink(&mut self, ref_name: &[u8], path: &Path, mtime: u64) -> Result<(), StoreError> {
        self.validate_mutation_path(ref_name, path)?;
        let op = WalOp::Unlink {
            ref_name: ref_name.to_vec(),
            path: path.clone(),
            mtime,
        };
        self.log_then_apply(op)
    }

    // ── Id-addressed surface: open handles (rev1§4.9, Design decision 2) ──
    //
    // These realize the rev1§4.9 *open handle* at the `Store` API. The file-id
    // layer stays server-internal (no wire op — that is C2D); the rename/unlink
    // interleaving proptest drives this surface to hold a file open across an
    // unlink. A *named* handle routes its writes through the durable path-
    // addressed `write`, sharing one WAL/replay path; only an *orphaned*
    // (unlinked-while-open) handle's writes are ephemeral.

    /// Open a handle on `(ref_name, path)`, returning its ephemeral [`FileId`]
    /// (rev1§4.9). The id resolves any existing binding (a dirty file or another
    /// open handle on the same name) or is freshly allocated; the handle then
    /// follows the name across unlinks (and renames, C2B). Errors if the ref is
    /// unknown. Opening does not create the file — until written, the name still
    /// reads from the tree (or as absent if tombstoned).
    pub fn open(&mut self, ref_name: &[u8], path: &Path) -> Result<FileId, StoreError> {
        if !self.table.refs.contains_key(ref_name) {
            return Err(StoreError::NoSuchRef);
        }
        // Disjoint field borrows: the id allocator and the overlay map are
        // separate fields (the `apply_to_overlay` pattern).
        let next_id = &mut self.next_file_id;
        let id = self
            .overlays
            .entry(ref_name.to_vec())
            .or_default()
            .open(path, next_id);
        self.open_files.insert(id, ref_name.to_vec());
        Ok(id)
    }

    /// Write through an open handle (rev1§4.9). A *named* handle delegates to the
    /// durable path-addressed [`Self::write`] (the same id, resolved via the
    /// overlay's `by_name`), so the write is WAL-logged and replays. An
    /// *orphaned* handle (its name was unlinked while open) writes ephemerally to
    /// the overlay: the data has no path, is never logged, and is discarded at
    /// flush — exactly rev1§4.9's "the open handle keeps working against the
    /// overlay." Errors if `id` is not an open handle.
    pub fn write_id(
        &mut self,
        id: FileId,
        offset: u64,
        data: &[u8],
        mtime: u64,
    ) -> Result<(), StoreError> {
        let ref_name = self
            .open_files
            .get(&id)
            .ok_or(StoreError::NoSuchHandle)?
            .clone();
        let name = self.overlays.get(&ref_name).and_then(|o| o.name_of(id));
        match name {
            Some(path) => self.write(&ref_name, &path, offset, data, mtime),
            None => {
                // Orphaned: keep `write`'s extent-overflow guard (the data never
                // reaches the chunk region, so the region-capacity check is moot).
                offset
                    .checked_add(data.len() as u64)
                    .ok_or(StoreError::WriteOutOfRange)?;
                self.overlays
                    .entry(ref_name)
                    .or_default()
                    .write_orphan(id, offset, data, mtime);
                Ok(())
            }
        }
    }

    /// Read through an open handle (rev1§4.9). A named handle reads like its path
    /// (overlay over tree); an orphaned handle reads its overlay data against an
    /// empty base (its tree path is gone). Errors if `id` is not an open handle.
    pub fn read_id(&self, id: FileId) -> Result<Option<Vec<u8>>, StoreError> {
        let ref_name = self.open_files.get(&id).ok_or(StoreError::NoSuchHandle)?;
        match self.overlays.get(ref_name).and_then(|o| o.name_of(id)) {
            Some(path) => self.read(ref_name, &path),
            None => Ok(Some(
                self.overlays
                    .get(ref_name)
                    .map(|o| o.read_orphan(id))
                    .unwrap_or_default(),
            )),
        }
    }

    /// Close an open handle (rev1§4.9). On the last close the overlay reaps by
    /// state: an orphaned id's data is discarded, an opened-but-never-written
    /// name reverts to the tree, a dirty named id is kept to flush normally.
    /// Errors if `id` is not an open handle.
    pub fn close(&mut self, id: FileId) -> Result<(), StoreError> {
        let ref_name = self
            .open_files
            .get(&id)
            .ok_or(StoreError::NoSuchHandle)?
            .clone();
        let fully = self
            .overlays
            .get_mut(&ref_name)
            .map(|o| o.close(id))
            .unwrap_or(true);
        if fully {
            self.open_files.remove(&id);
        }
        Ok(())
    }

    /// Hand the device back (for crash-injection tests).
    pub fn into_dev(self) -> D {
        self.chunks.dev
    }

    pub fn dev_mut(&mut self) -> &mut D {
        &mut self.chunks.dev
    }

    /// Live WAL usage in bytes (rev1§4.4): the span the unflushed records occupy
    /// on the circular ring, `[wal_head, wal_tail)` modulo `wal_len`. `0` when the
    /// log is empty; `wal_len` when it is exactly full (the tail has wrapped back
    /// onto the head — the only non-empty state where the raw difference is `0`,
    /// so it must read as full, not empty). `wal_head` is `self.sb.wal_head` (it
    /// only ever moves at commit).
    fn wal_usage(&self) -> u64 {
        if self.wal_records.is_empty() {
            return 0;
        }
        // No underflow: `wal_tail + wal_len >= wal_head` since `wal_head <= wal_len`.
        let raw = (self.wal_tail + self.opts.wal_len - self.sb.wal_head) % self.opts.wal_len;
        if raw == 0 {
            self.opts.wal_len
        } else {
            raw
        }
    }

    /// Append a WAL record's bytes at ring offset `off`, splitting the write
    /// across the buffer end when the record wraps (rev1§4.4 circular ring), then
    /// a **single** `flush()` after both halves. The single fsync is load-bearing
    /// for crash-safety: a flush *between* the halves would make one half durable
    /// while the other is not, and that half-record was never acked — a later
    /// same-offset write could then partially overlay a torn, ack-less record.
    /// Keep it one fsync. A header that itself straddles the wrap is handled (the
    /// first segment is then shorter than `WAL_HEADER`); replay reassembles it
    /// after the mount-time rotation.
    fn wal_write(&mut self, off: u64, rec: &[u8]) -> Result<(), StoreError> {
        let wal_len = self.opts.wal_len;
        // Not oversized (caller's bypass), so `rec.len() <= wal_len` and
        // `off + rec.len() < 2*wal_len` — no overflow.
        if off + rec.len() as u64 <= wal_len {
            self.chunks.dev.write(WAL_OFF + off, rec)?;
        } else {
            let first = (wal_len - off) as usize;
            self.chunks.dev.write(WAL_OFF + off, &rec[..first])?;
            self.chunks.dev.write(WAL_OFF, &rec[first..])?;
        }
        self.chunks.dev.flush()?;
        Ok(())
    }

    /// WAL append + fsync before the overlay sees the write — the ack
    /// implies durability (rev1§4.3 step 2).
    fn log_then_apply(&mut self, op: WalOp) -> Result<(), StoreError> {
        let rec = op.encode_record(self.wal_seq);
        let reclen = rec.len() as u64;
        if reclen > self.opts.wal_len {
            // Oversized: bypass the WAL, commit synchronously before ack.
            let r = op.ref_name().to_vec();
            self.apply_to_overlay(&op);
            self.flush_ref(&r)?;
            return self.commit();
        }
        // WAL pressure (rev1§4.4 trigger 2, M-5): make room for this record on the
        // ring and, if usage crosses the watermark, flush the tail-pinning ref so
        // `wal_head` advances past the freed prefix. This may `commit()` (which
        // can move `wal_tail`), so `rec_off` MUST be read *after* it returns.
        self.relieve_wal_pressure(reclen)?;
        // Read the tail only now: relief's `commit` may have moved it. Relief
        // guarantees the record fits the free gap, so the ring write stays within
        // `[wal_tail, wal_head)` and never overruns the live window.
        debug_assert!(self.wal_usage() + reclen <= self.opts.wal_len);
        let rec_off = self.wal_tail;
        self.wal_write(rec_off, &rec)?;
        self.wal_records.push_back(RecMeta {
            seq: self.wal_seq,
            off: rec_off,
            ref_name: op.ref_name().to_vec(),
            flushed: false,
        });
        // Ring advance: the tail wraps mod `wal_len` (the record may have
        // straddled the buffer end). `rec_off + reclen < 2*wal_len`, no overflow.
        self.wal_tail = (rec_off + reclen) % self.opts.wal_len;
        self.wal_seq += 1;
        let r = op.ref_name().to_vec();
        self.apply_to_overlay(&op);
        self.account_op(&op, rec_off);

        // Per-ref soft bound (rev1§4.4 containment, M-3/M-6): a ref that
        // crosses its byte quota or its op-count secondary bound self-flushes,
        // so one hot ref cannot consume the whole global budget and a metadata
        // storm with tiny bytes still flushes. Backpressure is a synchronous
        // blocking flush (no eviction, no `FULL` return): the write proceeds
        // once the overlay has become tree. The flush + commit rides the
        // existing partial-head-advance (`advance_head` pops the contiguous
        // flushed prefix); B12C's ring reclaims the rest of the WAL span.
        let over_bytes = self.overlays.get(&r).map_or(0, |o| o.bytes()) > self.opts.per_ref_budget;
        let over_ops = self.acct.get(&r).map_or(0, |a| a.op_count) > self.opts.op_count_bound;
        if over_bytes || over_ops {
            self.flush_ref(&r)?;
            self.commit()?;
        }

        // Size pressure (rev1§4.4, M-4): when total dirty overlay bytes cross
        // the *low* watermark, flush the biggest offenders — not flush-everything
        // at one hard threshold. Small refs stay dirty; the high watermark
        // (`global_budget`) is the backpressure point, rarely reached because
        // flushing starts at the low one.
        self.relieve_size_pressure()?;

        // Staleness (rev1§4.4 trigger 4, M-6 timer half): lowest priority, after
        // WAL/size pressure relieved. The incoming op's mtime is the clock — a
        // write whose timestamp leaves an *older* quiet ref past the bound flushes
        // that ref, so the write path itself bounds staleness without a timer.
        // Disabled by default (staleness_ns == u64::MAX); B12F ships the 30 s figure.
        self.relieve_staleness(op.mtime())?;
        Ok(())
    }

    /// Size-pressure flush (rev1§4.4 trigger 3, M-4): when total dirty overlay
    /// bytes cross the *low* watermark, flush the **biggest offenders** — sort
    /// the dirty refs by overlay bytes descending and `flush_ref` them until the
    /// total is back at or below the low watermark (or only one ref remains) —
    /// *not* `sync_all`, so the smallest refs stay dirty. The *high* watermark is
    /// `global_budget`, the backpressure point; because flushing starts at the
    /// low one, writers rarely reach it.
    ///
    /// Backpressure is the synchronous flush itself (Design decision 4): the
    /// flush runs inline and the write proceeds once pressure is relieved — no
    /// eviction (overlay leaves memory only by becoming tree) and no `FULL`
    /// return (a refusal a single-threaded server can never need — the flush
    /// always relieves; the async `FULL` reply is recorded future work). In the
    /// synchronous model this keeps `total <= size_low_watermark < global_budget`,
    /// so the high watermark is never crossed by normal traffic and needs no
    /// separate enforcement here.
    ///
    /// The flush + commit rides `commit`'s partial-head-advance (the verified
    /// `advance_head` pops the contiguous flushed prefix), exactly as B12A's
    /// per-ref soft-bound flush does; B12C's ring reclaims the rest of the span.
    fn relieve_size_pressure(&mut self) -> Result<(), StoreError> {
        let total = |s: &Self| -> usize { s.overlays.values().map(|o| o.bytes()).sum() };
        if total(self) <= self.opts.size_low_watermark {
            return Ok(());
        }
        // Biggest offenders first; ties broken by ref name for determinism. The
        // snapshot is consistent — this is the only flusher (single-threaded).
        let mut by_size: Vec<(usize, Vec<u8>)> = self
            .overlays
            .iter()
            .map(|(name, o)| (o.bytes(), name.clone()))
            .collect();
        by_size.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut flushed_any = false;
        for (_, name) in by_size {
            // Leave at least one ref dirty: size pressure flushes the biggest
            // offenders, never everything (that is the WAL-full / explicit-sync
            // path). A single ref over the low watermark is contained instead by
            // the per-ref soft bound (rev1§4.4 M-3), not by size pressure.
            if self.overlays.len() <= 1 || total(self) <= self.opts.size_low_watermark {
                break;
            }
            self.flush_ref(&name)?;
            flushed_any = true;
        }
        // One commit folds in every flush this round (rev1§4.2 atomicity).
        if flushed_any {
            self.commit()?;
        }
        Ok(())
    }

    /// WAL-pressure flush (rev1§4.4 trigger 2, M-5): keep the circular WAL below
    /// its watermark and make room for the next `incoming`-byte record by flushing
    /// the ref **pinning the tail** — the ref whose oldest unflushed record sits
    /// at `wal_head`, i.e. the front of `wal_records`. Flushing it lets `commit`'s
    /// Verus-verified `advance_head` move `wal_head` past the now-flushed
    /// contiguous prefix, reclaiming exactly that span of the ring. (The old
    /// linear WAL could only reclaim by resetting to 0 when *everything* flushed —
    /// the M-5 gap: flush-the-pinner was meaningless without space reclaim.)
    ///
    /// Repeats until comfortable: `usage + incoming` fits and `usage` is below the
    /// watermark. Each iteration flushes the front ref (which always has an overlay
    /// — `advance_head` leaves a non-flushed record at the front, and a non-flushed
    /// record implies a live overlay), so `commit` pops at least that front record,
    /// `usage` strictly shrinks, and the loop terminates. Under interleaved refs it
    /// may flush more than the lone pinner, which is fine. When one ref pins the
    /// whole ring, flushing it empties the WAL and `commit` resets the tail to 0 —
    /// the normative "a full WAL flushes everything and resets" edge case (rev1§4.4).
    ///
    /// Picking the victim by `front()` is mandatory: sorting refs by their raw
    /// `oldest_wal_pos` is wrong across the ring wrap (a newer ref can sit at a
    /// lower physical offset than the tail-pinner). Backpressure is the synchronous
    /// flush itself (Design decision 4): no eviction, no `FULL` return.
    fn relieve_wal_pressure(&mut self, incoming: u64) -> Result<(), StoreError> {
        loop {
            let usage = self.wal_usage();
            // `usage <= wal_len` and `incoming <= wal_len` (oversized bypassed), so
            // `usage + incoming` cannot overflow.
            let fits = usage + incoming <= self.opts.wal_len;
            let comfortable = usage < self.opts.wal_watermark;
            if fits && comfortable {
                break;
            }
            // An empty ring can't be relieved further; `incoming <= wal_len` always
            // fits an empty ring, so this only breaks once there is nothing to flush.
            let Some(pinner) = self.wal_records.front().map(|r| r.ref_name.clone()) else {
                break;
            };
            self.flush_ref(&pinner)?;
            self.commit()?;
        }
        Ok(())
    }

    /// Staleness flush (rev1§4.4 trigger 4, M-6 timer half): a quietly dirty ref
    /// whose *oldest-dirty* age exceeds `staleness_ns` becomes committed tree, so
    /// no dirty byte sits unflushed indefinitely — "a quietly dirty ref
    /// eventually becomes committed tree." It keys on the ref's oldest unflushed
    /// op (`RefAcct::oldest_dirty_ns`, set once per dirty epoch and reset on
    /// flush), so it bounds the *maximum* staleness of any dirty byte, not the
    /// time since the last touch.
    ///
    /// Lowest priority of the four triggers (Design decision 5): it fires only
    /// after WAL/size/per-ref pressure had their say, opportunistically at the
    /// points the single-threaded server already runs — the write path
    /// (`log_then_apply`, with the incoming op's mtime as the clock) and the
    /// storage server's reactor idle (`flush_stale`) — so no background thread or
    /// armed kernel timer is needed. `now` is the caller-injected UTC-nanos clock
    /// (the same source as op mtimes; there is no internal store clock), so tests
    /// drive it deterministically. `saturating_sub` makes a non-monotone clock a
    /// no-op rather than a spurious flush.
    ///
    /// Selective like size pressure (not flush-everything): only overdue refs
    /// flush; fresher dirty refs stay in overlay. Backpressure is the synchronous
    /// flush itself (Design decision 4) — `flush_ref` turns the overlay into tree
    /// and a single `commit` folds in the whole sweep, riding the verified
    /// `advance_head` exactly as the other relievers do.
    fn relieve_staleness(&mut self, now: u64) -> Result<(), StoreError> {
        // Disabled stub (the default until B12F ships the 30 s figure): skip the
        // scan entirely. `staleness_ns == u64::MAX` can never be exceeded anyway,
        // but the early return keeps the hot write path free of the acct walk.
        if self.opts.staleness_ns == u64::MAX {
            return Ok(());
        }
        // Collect overdue refs first (can't flush while borrowing `acct`).
        let stale: Vec<Vec<u8>> = self
            .acct
            .iter()
            .filter(|(_, a)| {
                a.oldest_dirty_ns
                    .map_or(false, |t| now.saturating_sub(t) > self.opts.staleness_ns)
            })
            .map(|(name, _)| name.clone())
            .collect();
        let mut flushed_any = false;
        for name in stale {
            self.flush_ref(&name)?;
            flushed_any = true;
        }
        // One commit folds in every staleness flush this sweep (rev1§4.2 atomicity).
        if flushed_any {
            self.commit()?;
        }
        Ok(())
    }

    /// The storage server's opportunistic staleness sweep (rev1§4.4 trigger 4):
    /// flush every ref dirty past the `staleness_ns` bound as of `now`. Called at
    /// request boundaries and at reactor idle (Design decision 5), the points a
    /// single-threaded request-driven server already runs — so a quietly dirty
    /// ref eventually becomes committed tree even with no further writes. A no-op
    /// at the default-stubbed `staleness_ns == u64::MAX` until B12F ships 30 s.
    pub fn flush_stale(&mut self, now: u64) -> Result<(), StoreError> {
        self.relieve_staleness(now)
    }

    /// Update a ref's flush-scheduler accounting after one mutating op lands in
    /// its overlay (rev1§4.4). `wal_pos` is the op's byte offset in the WAL.
    /// Pure derived state: called on the live write path and again, identically,
    /// during mount WAL replay, so a remounted store recomputes it (B12 is
    /// format-stable). Reset in `flush_ref` when the ref's overlay becomes tree.
    fn account_op(&mut self, op: &WalOp, wal_pos: u64) {
        let a = self.acct.entry(op.ref_name().to_vec()).or_default();
        a.op_count += 1;
        // The first op after a flush pins the oldest position/timestamp; later
        // ops leave them — "oldest" never moves forward until the ref flushes.
        a.oldest_wal_pos.get_or_insert(wal_pos);
        a.oldest_dirty_ns.get_or_insert(op.mtime());
    }

    fn apply_to_overlay(&mut self, op: &WalOp) {
        // Disjoint field borrows: the id allocator and the overlay map are
        // separate fields, so write can mint a fresh id while holding the overlay.
        let next_id = &mut self.next_file_id;
        let overlay = self.overlays.entry(op.ref_name().to_vec()).or_default();
        match op {
            WalOp::Write {
                path,
                offset,
                mtime,
                data,
                ..
            } => {
                overlay.write(path, *offset, data, *mtime, next_id);
            }
            WalOp::Unlink { path, mtime, .. } => {
                overlay.unlink(path, *mtime);
            }
        }
    }

    // ── Read path (overlay first, tree below — rev1§4.3) ────────────────

    pub fn read(&self, ref_name: &[u8], path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
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
                Ok(Some(fo.apply(&base)))
            }
        }
    }

    /// Read a file out of a committed/flushed tree root (also used for
    /// snapshot reads, where no overlay applies).
    pub fn read_at_root(&self, root: &Hash, path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        self.read_from_tree(root, path)
    }

    /// Mass revocation of a ref's storage handles (rev1§2.2): O(1) — bump the
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

    pub fn list_dir_node(&self, node: &Hash) -> Result<Vec<(Vec<u8>, EntryKind, u64)>, StoreError> {
        let dir = Dir::load(&self.chunks, node)?;
        Ok(dir
            .iter()
            .map(|e| (e.name.clone(), e.kind, e.size))
            .collect())
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
            Some(Entry {
                kind: EntryKind::Dir,
                ..
            }) => Err(StoreError::NotAFile),
            Some(e) => Ok(Some(read_file(&self.chunks, &e.content, e.size)?)),
        }
    }

    /// Merged directory listing: committed tree + dirty overlay (rev1§4.4
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
                Some(Entry {
                    content: Content::DirRoot(h),
                    ..
                }) => Some(h),
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

    // ── Flush and commit (rev1§4.3 steps 3–4) ───────────────────────────

    /// Turn one ref's overlay into immutable tree (path-copy to a new
    /// root). Nothing on disk references the result until commit.
    pub fn flush_ref(&mut self, ref_name: &[u8]) -> Result<(), StoreError> {
        // The overlay becomes immutable tree, so the ref is no longer dirty:
        // drop its flush-scheduler accounting (rev1§4.4). No-op when clean —
        // a clean ref never has an accounting entry.
        self.acct.remove(ref_name);
        let Some(overlay) = self.overlays.remove(ref_name) else {
            return Ok(());
        };
        if overlay.is_empty() {
            // No dirty content to commit — but any open handles must survive the
            // flush (rev1§4.9), so re-seed their bindings before returning.
            if let Some(carry) = overlay.carry_open() {
                self.overlays.insert(ref_name.to_vec(), carry);
            }
            return Ok(());
        }
        let mut root = self
            .table
            .refs
            .get(ref_name)
            .ok_or(StoreError::NoSuchRef)?
            .root;

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
            // Reuse the old chunk list only when there is a real base to diff
            // against — not a fresh create / unlink-then-write, and not a
            // directory. `reuse` carries the old content (its chunk list) and
            // the materialized old bytes (for the neighborhood suffix diff).
            let reuse = match (&old, fo.fresh) {
                (
                    Some(Entry {
                        kind: EntryKind::Dir,
                        ..
                    }),
                    _,
                ) => return Err(StoreError::NotAFile),
                (Some(e), false) => Some((
                    e.content.clone(),
                    read_file(&self.chunks, &e.content, e.size)?,
                )),
                _ => None,
            };
            let flags = old.as_ref().map(|e| e.flags).unwrap_or(0);
            let content = fo.apply(reuse.as_ref().map(|(_, b)| b.as_slice()).unwrap_or(&[]));
            // rev1§4.3 step 3: re-chunk only the edited neighborhood when an old
            // chunk list is available, hashing O(edit) chunks instead of the
            // whole file; the result is the same canonical chunking (rev1§4.1).
            let entry_content = match &reuse {
                Some((old_content, base)) => store_file_neighborhood(
                    &mut self.chunks,
                    &self.opts.chunker,
                    old_content,
                    base,
                    &content,
                    fo.first_write_offset().unwrap_or(0),
                ),
                None => store_file(&mut self.chunks, &self.opts.chunker, &content),
            };
            self.check_io()?;
            let entry = Entry {
                name: name[0].to_vec(),
                kind: EntryKind::File,
                flags,
                size: content.len() as u64,
                mtime: fo.mtime,
                content: entry_content,
            };
            root = tree::put(&mut self.chunks, &root, dir, entry, fo.mtime)?;
            self.check_io()?;
        }

        self.table.refs.get_mut(ref_name).unwrap().root = root;
        // A flush that re-points the head is an entry-set mutation (rev1§4.7):
        // a concurrent writer's commit between a retention daemon's read and
        // its guarded batch advances the version, invalidating the batch (I-2).
        self.touch_ref(ref_name);
        for rec in &mut self.wal_records {
            if rec.ref_name.as_slice() == ref_name {
                rec.flushed = true;
            }
        }
        // The named data is now tree and any orphaned (unlinked-while-open) data
        // was dropped above (absent from `files()`); the open handles keep working
        // across the flush (rev1§4.9), so re-seed their id↔name bindings onto a
        // fresh, data-free overlay. `None` ⇒ nothing open ⇒ the ref's overlay is
        // simply gone, the C2A behavior.
        if let Some(carry) = overlay.carry_open() {
            self.overlays.insert(ref_name.to_vec(), carry);
        }
        Ok(())
    }

    /// The single atomicity mechanism (rev1§4.2): barrier 1, superblock to the
    /// older slot, barrier 2. The WAL head advances past the contiguous
    /// prefix of flushed records (rev1§4.3 step 4).
    pub fn commit(&mut self) -> Result<(), StoreError> {
        // rev1§4.7: advance the edit version of every ref whose entry-set
        // changed since the last commit — once per ref, regardless of how many
        // edits (head moves, rows, tags) were staged. Drained before the table
        // is serialized so the bumped values are what lands on disk. A ref that
        // was dirtied then dropped from the table (none today) is silently
        // skipped by the `get_mut`.
        for name in core::mem::take(&mut self.dirty_refs) {
            if let Some(e) = self.table.refs.get_mut(&name) {
                e.edit_version += 1;
            }
        }
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
        // decision is the Verus-verified `advance_head`: everything popped is
        // flushed, the head record (if any) is not — the TLA+
        // `CommitPrepare.newHead`. This is the write-path half of the gap-freedom
        // composition (B7C): `advance_head` here + the recovery walk
        // (`recover_records`) compose in `lemma_gap_freedom` to guarantee no
        // acked-but-unflushed record is left behind the advanced head — leaving
        // the bytes from `wal_head` recoverable, which the next `mount` replays.
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

    // ── GC and history rewriting (rev1§4.6-4.7) ─────────────────────

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
        // The birth-generation "live by fiat" filter (the `e.birth < epoch`
        // clause below) and the put-side resurrection check (`ChunkStore::put`)
        // are the two halves of the rev1§4.6 single GC/mutator interaction
        // point. Both are installed and structurally correct here, and become
        // load-bearing once C4 (rev1§8.3) makes GC concurrent: a chunk written
        // after the epoch then has `birth >= epoch` (never condemned), and a
        // dedup hit on a condemned chunk rewrites rather than resurrects. Under
        // today's synchronous cycle both are inert — `sync_all` above pins
        // `epoch == birth_gen`, so every existing chunk has `birth < epoch` and
        // none can be born between mark and sweep. Kept, not optimized away,
        // because the contract is stated and C4 needs both halves in place.
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
        // Open the resurrection-check window (rev1§4.6 step 3): for the
        // duration of the sweep, a dedup hit on one of these condemned hashes
        // rewrites the chunk instead of resurrecting the about-to-be-deleted
        // index entry. Assigning (rather than extending) also discards any set
        // left over by a prior cycle whose commit failed mid-flight.
        self.chunks.condemned = condemned.iter().map(|(h, _)| *h).collect();
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
        // Close the window: the sweep is durable, so no chunk is condemned
        // until the next cycle reopens it.
        self.chunks.condemned.clear();
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
        SpaceInfo {
            total,
            used: total - free,
            free,
        }
    }

    /// History rewriting (rev1§4.6): drop one snapshot row, re-pointing
    /// children's advisory parent past it (rev1§4.7). Tag targets are
    /// keep-strength pins and refuse deletion. The newly unreachable
    /// mass is reclaimed by the next GC, not here — this op is O(small).
    pub fn delete_snapshot(&mut self, ref_name: &[u8], snap_id: u64) -> Result<(), StoreError> {
        let key = (ref_name.to_vec(), snap_id);
        let row = self
            .table
            .snaps
            .get(&key)
            .ok_or(StoreError::NoSuchSnapshot)?;
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
        self.touch_ref(ref_name); // snapshot row removed + re-parented (rev1§4.7)
        self.commit()
    }

    /// Retention-class edit (rev1§4.7): the "mark survivors `keep`, run the
    /// policy" flow is this plus `delete_snapshot`, policy in userspace.
    pub fn set_snapshot_class(
        &mut self,
        ref_name: &[u8],
        snap_id: u64,
        class: u8,
    ) -> Result<(), StoreError> {
        if class > disk::CLASS_EPHEMERAL {
            return Err(StoreError::Format(FormatError::BadEntry(
                "bad retention class",
            )));
        }
        self.table
            .snaps
            .get_mut(&(ref_name.to_vec(), snap_id))
            .ok_or(StoreError::NoSuchSnapshot)?
            .class = class;
        self.touch_ref(ref_name); // retention-class edit on a snapshot row (rev1§4.7)
        self.commit()
    }

    /// Guarded ref-table batch (rev1§4.7) — the I-2/S-1 read-then-act fix.
    /// Apply `edits` to `ref_name` all-or-nothing within one commit, but only
    /// if the ref's edit version still equals `expected_version`:
    ///
    /// * **Version mismatch** (a concurrent mutation advanced the ref between
    ///   the caller's enumerate and now): nothing is mutated or committed;
    ///   `Err(VersionMismatch { current })` carries the value to re-read against.
    /// * **Invalid edit** (missing snapshot, pinned deletion, bad class, …):
    ///   the whole batch is rejected with the edit's `StoreError` and no commit
    ///   — the staged clone is discarded, so the live table is untouched.
    /// * **Success**: every edit lands in the one superblock flip (rev1§4.2,
    ///   the system's sole atomicity mechanism — no new machinery), the ref's
    ///   edit version advances exactly once (the §4.7 dirty-set rule, regardless
    ///   of edit count), and the new version is returned.
    ///
    /// Staging on a clone makes all-or-nothing structural rather than a matter
    /// of unwinding partial work, and lets each edit validate against the
    /// batch's own intermediate state — so e.g. a `CreateTag` earlier in the
    /// batch pins a snapshot a later `DeleteSnapshot` then correctly cannot
    /// remove.
    pub fn apply_batch(
        &mut self,
        ref_name: &[u8],
        expected_version: u64,
        edits: &[RefEdit],
    ) -> Result<u64, StoreError> {
        let current = self
            .table
            .refs
            .get(ref_name)
            .ok_or(StoreError::NoSuchRef)?
            .edit_version;
        if current != expected_version {
            return Err(StoreError::VersionMismatch { current });
        }
        let mut staged = self.table.clone();
        for edit in edits {
            apply_ref_edit(&mut staged, ref_name, edit)?;
        }
        self.table = staged;
        self.touch_ref(ref_name); // one dirty mark → exactly one bump in commit()
        self.commit()?;
        Ok(self.table.refs.get(ref_name).unwrap().edit_version)
    }
}

/// Apply one `RefEdit` to a ref table, validating as it mutates. Operates on
/// the staged clone `apply_batch` owns, so any `Err` leaves the live table
/// untouched (the clone is discarded) and all checks compose against the
/// batch's intermediate state. Tag/snapshot ids are scoped to `ref_name`.
fn apply_ref_edit(table: &mut RefTable, ref_name: &[u8], edit: &RefEdit) -> Result<(), StoreError> {
    match edit {
        RefEdit::DeleteSnapshot { id } => {
            let key = (ref_name.to_vec(), *id);
            let row = table.snaps.get(&key).ok_or(StoreError::NoSuchSnapshot)?;
            if table
                .tags
                .values()
                .any(|(r, s)| r.as_slice() == ref_name && *s == *id)
            {
                return Err(StoreError::Pinned);
            }
            let new_parent = row.parent;
            table.snaps.remove(&key);
            let range = (ref_name.to_vec(), 0)..(ref_name.to_vec(), u64::MAX);
            for (_, r) in table.snaps.range_mut(range) {
                if r.parent == Some(*id) {
                    r.parent = new_parent;
                }
            }
        }
        RefEdit::SetClass { id, class } => {
            if *class > disk::CLASS_EPHEMERAL {
                return Err(StoreError::Format(FormatError::BadEntry(
                    "bad retention class",
                )));
            }
            table
                .snaps
                .get_mut(&(ref_name.to_vec(), *id))
                .ok_or(StoreError::NoSuchSnapshot)?
                .class = *class;
        }
        RefEdit::SetParent { id, parent } => {
            // Referential integrity only: a `Some(p)` parent must name an
            // existing snapshot on this ref (no dangling parent). Cycle/policy
            // questions belong to the retention daemon, not the store.
            if let Some(p) = parent {
                if !table.snaps.contains_key(&(ref_name.to_vec(), *p)) {
                    return Err(StoreError::NoSuchSnapshot);
                }
            }
            table
                .snaps
                .get_mut(&(ref_name.to_vec(), *id))
                .ok_or(StoreError::NoSuchSnapshot)?
                .parent = *parent;
        }
        RefEdit::SetMessage { id, message } => {
            table
                .snaps
                .get_mut(&(ref_name.to_vec(), *id))
                .ok_or(StoreError::NoSuchSnapshot)?
                .message = message.clone();
        }
        RefEdit::CreateTag { name, snap_id } => {
            if !table.snaps.contains_key(&(ref_name.to_vec(), *snap_id)) {
                return Err(StoreError::NoSuchSnapshot);
            }
            table
                .tags
                .insert(name.clone(), (ref_name.to_vec(), *snap_id));
        }
        RefEdit::DeleteTag { name } => {
            // Ref-scoped: only remove a tag that currently pins a snapshot on
            // this batch's ref, so a batch can't reach across refs.
            if table
                .tags
                .get(name)
                .is_some_and(|(r, _)| r.as_slice() == ref_name)
            {
                table.tags.remove(name);
            }
        }
    }
    Ok(())
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
            chunker: ChunkerParams {
                min: 64,
                avg: 256,
                max: 1024,
            },
            global_budget: 32 * 1024,
            // Keep the low watermark consistent with this fixture's tightened
            // global_budget (rev1§4.4 invariant: size_low_watermark <= the high
            // watermark). Default would leave it at 96 MiB, above this 32 KiB
            // high watermark; pinning it equal preserves the pre-B12 size
            // trigger threshold, now realized as flush-the-biggest-offenders.
            size_low_watermark: 32 * 1024,
            // B12F ships triggering op-count and staleness *defaults* (8192 ops,
            // 30 s); pin them off here so this shared fixture keeps preserving
            // the pre-B12 flush behavior. The wal watermark inherits the default
            // (>> this fixture's 8 KiB wal_len), so flush-the-pinner stays inert
            // too. Tests that exercise a specific bound pass their own tight opts
            // (`crash_opts`, `wal_opts`, `stale_opts`, and the inline builders).
            op_count_bound: u64::MAX,
            staleness_ns: u64::MAX,
            ..StoreOptions::default()
        }
    }

    /// `test_opts` with a tight per-ref op-count bound so the B12A per-ref
    /// soft-bound auto-flush (rev1§4.4) fires mid-stream — used by the
    /// crash-injection proptest to re-witness all-acked-survives across the new
    /// selective-flush path (flush_ref + commit inside a write).
    fn crash_opts() -> StoreOptions {
        StoreOptions {
            op_count_bound: 4,
            ..test_opts()
        }
    }

    fn p(parts: &[&str]) -> Path {
        parts.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    /// Wrap a raw payload in a minimal WAL record frame (magic+seq+len+cksum+
    /// payload). `wal_struct_ok` reads only the structure of the tail past the
    /// 48-byte header, so the header fields here need not be self-consistent —
    /// the structural predicate never inspects them.
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut rec = Vec::with_capacity(WAL_HEADER + payload.len());
        rec.extend_from_slice(b"WREC");
        rec.extend_from_slice(&0u64.to_le_bytes()); // seq
        rec.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // len
        rec.extend_from_slice(&[0u8; 32]); // checksum placeholder
        rec.extend_from_slice(payload);
        rec
    }

    // ── B12F: refuse-not-panic format contract (rev1§4.5, S-10) ──

    /// rev1§4.5: `format` is total over device *geometry* — an undersized or
    /// un-layoutable device is refused with `StoreError::DeviceTooSmall`, never
    /// a panic (`mkfs`'s clean `ExitCode::FAILURE` path depends on this). With
    /// the 8 KiB-WAL fixture the geometry floor is `WAL_OFF (8 KiB) + wal_len
    /// (8 KiB) + MIN_CHUNK_REGION (4 KiB) = 20 KiB`.
    #[test]
    fn format_refuses_undersized_device_without_panic() {
        let floor = WAL_OFF + test_opts().wal_len + MIN_CHUNK_REGION;
        // At or below the floor: refused cleanly, no panic. `.map(|_| ())` drops
        // the (non-Debug) `Store` from the Ok arm so the mismatch is printable.
        for too_small in [0u64, 8192, floor - 1, floor] {
            match Store::format(MemDev::new(too_small as usize), test_opts()).map(|_| ()) {
                Err(StoreError::DeviceTooSmall) => {}
                other => panic!("len {too_small}: expected DeviceTooSmall, got {other:?}"),
            }
        }
        // One byte over the floor formats cleanly.
        assert!(Store::format(MemDev::new((floor + 1) as usize), test_opts()).is_ok());
        // A hostile `wal_len` near u64::MAX overflows the geometry add — the
        // `checked_add` refuses rather than wrapping into a false pass; still no
        // panic.
        let huge = StoreOptions {
            wal_len: u64::MAX,
            ..test_opts()
        };
        match Store::format(MemDev::new(1 << 20), huge).map(|_| ()) {
            Err(StoreError::DeviceTooSmall) => {}
            other => panic!("u64::MAX wal_len: expected DeviceTooSmall, got {other:?}"),
        }
    }

    // ── B12A: per-ref accounting + per-ref soft bound (rev1§4.4, M-3/M-6) ──
    //
    // `mod tests` is a child module of `store`, so these read the private
    // `Store::overlays` / `Store::acct` directly — no accessors needed.

    /// rev1§4.4 M-3: a ref written far past its per-ref soft bound self-flushes
    /// (backpressure, not eviction — the overlay becomes committed tree), so it
    /// cannot consume the whole global budget; a quiet ref under its bound stays
    /// dirty. The flushed data is materialized to tree (read-backable), not lost.
    #[test]
    fn per_ref_soft_bound_flushes_hot_ref_keeps_quiet_ref_dirty() {
        const PER_REF: usize = 4096;
        const GLOBAL: usize = 1 << 20;
        let opts = StoreOptions {
            wal_len: 1 << 20,
            global_budget: GLOBAL,
            per_ref_budget: PER_REF,
            ..test_opts()
        };
        let mut store = Store::format(MemDev::new(4 << 20), opts).unwrap();
        store.create_ref(b"hot").unwrap();
        store.create_ref(b"quiet").unwrap();

        // One small write to the quiet ref, well under the soft bound.
        store.write(b"quiet", &p(&["q"]), 0, &[7u8; 64], 1).unwrap();

        // Drive the hot ref far past PER_REF with distinct 512-byte files.
        let chunk = [0xABu8; 512];
        let mut hot_total = 0usize;
        for i in 0..20 {
            store
                .write(b"hot", &p(&[&format!("f{i}")]), 0, &chunk, (i + 2) as u64)
                .unwrap();
            hot_total += chunk.len();
            // M-3 invariant: the soft-bound flush fires inside the write that
            // would cross the bound, so the overlay never exceeds it.
            let hot_bytes = store
                .overlays
                .get(b"hot".as_slice())
                .map_or(0, |o| o.bytes());
            assert!(
                hot_bytes <= PER_REF,
                "hot overlay {hot_bytes} exceeded per_ref_budget {PER_REF}"
            );
        }

        // The hot ref flushed (its live overlay holds far less than everything
        // written to it) and that data reached the tree (read-backable).
        let hot_bytes = store
            .overlays
            .get(b"hot".as_slice())
            .map_or(0, |o| o.bytes());
        assert!(hot_bytes < hot_total, "hot ref never flushed");
        assert_eq!(
            store.read(b"hot", &p(&["f0"])).unwrap(),
            Some(chunk.to_vec())
        );
        assert_eq!(
            store.read(b"hot", &p(&["f19"])).unwrap(),
            Some(chunk.to_vec())
        );

        // The quiet ref stayed dirty — no eviction swept it (it is under its bound).
        assert!(
            !store.overlays.get(b"quiet".as_slice()).unwrap().is_empty(),
            "quiet ref was flushed despite staying under its soft bound"
        );
        assert!(store.acct.contains_key(b"quiet".as_slice()));
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.4 M-3 invariant under arbitrary multi-ref write sequences: the
        /// per-ref soft-bound flush keeps every ref's overlay at or below the
        /// soft bound after every op, so no single ref can ever reach the global
        /// budget — containment holds regardless of interleaving.
        #[test]
        fn per_ref_overlay_never_exceeds_soft_bound(
            ops in proptest::collection::vec((0usize..3, 1usize..400), 1..80),
        ) {
            const PER_REF: usize = 4096;
            const GLOBAL: usize = 1 << 20;
            let opts = StoreOptions {
                wal_len: 4 << 20,
                global_budget: GLOBAL,
                per_ref_budget: PER_REF,
                ..test_opts()
            };
            let mut store = Store::format(MemDev::new(8 << 20), opts).unwrap();
            let refs: [&[u8]; 3] = [b"r0", b"r1", b"r2"];
            for r in &refs {
                store.create_ref(r).unwrap();
            }
            let max_ops = if cfg!(miri) { 16 } else { ops.len() };
            for (i, (ri, len)) in ops.iter().take(max_ops).enumerate() {
                let data = vec![0x5Au8; *len];
                store.write(refs[*ri], &p(&[&format!("f{i}")]), 0, &data, (i + 1) as u64).unwrap();
                for r in &refs {
                    let b = store.overlays.get(*r).map_or(0, |o| o.bytes());
                    prop_assert!(b <= PER_REF, "ref {:?} overlay {} > per_ref_budget {}", r, b, PER_REF);
                    prop_assert!(b < GLOBAL);
                }
            }
        }
    }

    /// rev1§4.4 M-6 (op-count half): a metadata storm — many tiny ops whose
    /// dirty bytes stay far under the per-ref byte bound — still flushes once it
    /// crosses the op-count secondary bound. The byte-only bound would miss it.
    #[test]
    fn op_count_bound_flushes_a_metadata_storm_under_the_byte_bound() {
        let opts = StoreOptions {
            wal_len: 64 * 1024,
            global_budget: 1 << 20,
            per_ref_budget: 1 << 20, // loose: bytes never trigger
            op_count_bound: 8,
            ..test_opts()
        };
        let mut store = Store::format(MemDev::new(2 << 20), opts).unwrap();
        store.create_ref(b"main").unwrap();

        // Eight 1-byte writes: op_count reaches 8 (== bound, not yet over).
        for i in 0..8u64 {
            store
                .write(b"main", &p(&[&format!("f{i}")]), 0, &[1u8], i + 1)
                .unwrap();
        }
        assert_eq!(
            store.acct.get(b"main".as_slice()).map(|a| a.op_count),
            Some(8)
        );
        let bytes = store
            .overlays
            .get(b"main".as_slice())
            .map_or(0, |o| o.bytes());
        assert_eq!(bytes, 8);
        assert!(bytes < (1 << 20), "byte bound would already have fired");

        // The ninth op crosses op_count_bound (9 > 8) → the ref flushes.
        store.write(b"main", &p(&["f8"]), 0, &[1u8], 9).unwrap();
        assert!(
            store.overlays.get(b"main".as_slice()).is_none(),
            "op-count storm did not flush the ref"
        );
        assert!(
            store.acct.get(b"main".as_slice()).is_none(),
            "accounting not reset on flush"
        );
        // All nine tiny files reached the tree.
        assert_eq!(store.read(b"main", &p(&["f0"])).unwrap(), Some(vec![1u8]));
        assert_eq!(store.read(b"main", &p(&["f8"])).unwrap(), Some(vec![1u8]));
    }

    /// rev1§4.4 / Design decision 1: all per-ref accounting is derived runtime
    /// state, reconstructed at mount from WAL replay — B12 is format-stable. A
    /// remount must recompute bytes / op-count / oldest-WAL-position /
    /// oldest-dirty-timestamp identical to the pre-remount live state.
    #[test]
    fn per_ref_accounting_is_reconstructed_on_remount() {
        // Loose bounds + ample WAL so nothing flushes; the dirty records sit in
        // the WAL to be replayed.
        let opts = StoreOptions {
            wal_len: 1 << 20,
            global_budget: 1 << 20,
            per_ref_budget: 1 << 20,
            op_count_bound: u64::MAX,
            ..test_opts()
        };
        let mut store = Store::format(MemDev::new(4 << 20), opts).unwrap();
        store.create_ref(b"a").unwrap();
        store.create_ref(b"b").unwrap();
        // Interleave writes across refs at distinct mtimes.
        store.write(b"a", &p(&["a0"]), 0, &[1u8; 100], 10).unwrap();
        store.write(b"b", &p(&["b0"]), 0, &[2u8; 50], 20).unwrap();
        store.write(b"a", &p(&["a1"]), 0, &[3u8; 70], 30).unwrap();
        store.write(b"b", &p(&["b1"]), 0, &[4u8; 40], 40).unwrap();

        let snap = |s: &Store<MemDev>| -> Vec<(Vec<u8>, usize, u64, Option<u64>, Option<u64>)> {
            s.acct
                .iter()
                .map(|(name, a)| {
                    (
                        name.clone(),
                        s.overlays.get(name).map_or(0, |o| o.bytes()),
                        a.op_count,
                        a.oldest_wal_pos,
                        a.oldest_dirty_ns,
                    )
                })
                .collect()
        };
        let before = snap(&store);
        // Sanity on the live state (encoding-independent): bytes are additive
        // over distinct files, op-count is one per write, the oldest position is
        // the ref's first record (ref "a" pins WAL offset 0; "b" sits after it),
        // and the oldest-dirty timestamp is the first op's mtime.
        assert_eq!(before.len(), 2);
        assert_eq!(before[0].0, b"a".to_vec());
        assert_eq!(
            (before[0].1, before[0].2, before[0].3, before[0].4),
            (170, 2, Some(0), Some(10))
        );
        assert_eq!(before[1].0, b"b".to_vec());
        assert_eq!((before[1].1, before[1].2, before[1].4), (90, 2, Some(20)));
        assert!(
            before[1].3.unwrap() > 0,
            "ref b's oldest record should sit after a0"
        );

        let dev = store.into_dev();
        let recovered = Store::mount(dev, opts).unwrap();
        assert_eq!(
            snap(&recovered),
            before,
            "replay did not reconstruct accounting"
        );
    }

    // ── B12B: size-pressure low/high watermarks + flush-the-biggest-offenders ──
    //         (rev1§4.4 trigger 3, M-4)

    /// rev1§4.4 M-4 headline: crossing the size low watermark flushes the
    /// *biggest offenders*, not everything. Three refs of unequal size cross the
    /// low watermark; the largest flushes (overlay → committed tree, read-backable)
    /// while the smaller two stay dirty — versus the pre-B12 `sync_all` that
    /// emptied all of them. Size pressure is the only trigger here (ample WAL,
    /// loose per-ref/op-count bounds).
    #[test]
    fn size_pressure_flushes_biggest_offenders_keeps_small_dirty() {
        const LOW: usize = 4096;
        let opts = StoreOptions {
            wal_len: 1 << 20,        // ample: WAL pressure never interferes
            global_budget: 1 << 20,  // high watermark well above LOW
            per_ref_budget: 1 << 20, // loose: the per-ref bound never fires
            op_count_bound: u64::MAX,
            size_low_watermark: LOW,
            ..test_opts()
        };
        let mut store = Store::format(MemDev::new(4 << 20), opts).unwrap();
        for r in [b"big".as_slice(), b"mid", b"small"] {
            store.create_ref(r).unwrap();
        }
        // Below the low watermark so far: small (200) + mid (2000) = 2200 < LOW.
        store
            .write(b"small", &p(&["s"]), 0, &[1u8; 200], 1)
            .unwrap();
        store.write(b"mid", &p(&["m"]), 0, &[2u8; 2000], 2).unwrap();
        // The big write pushes the total (8200) over LOW → flush the biggest.
        store.write(b"big", &p(&["b"]), 0, &[3u8; 6000], 3).unwrap();

        // The biggest offender flushed; the two smaller refs stayed dirty.
        assert!(
            store
                .overlays
                .get(b"big".as_slice())
                .map_or(true, |o| o.is_empty()),
            "biggest ref was not flushed by size pressure"
        );
        assert!(
            !store.overlays.get(b"mid".as_slice()).unwrap().is_empty(),
            "mid ref was swept though it was not the biggest offender"
        );
        assert!(
            !store.overlays.get(b"small".as_slice()).unwrap().is_empty(),
            "small ref was swept though it was the smallest"
        );
        // Total dirty is back at or below the low watermark.
        let total: usize = store.overlays.values().map(|o| o.bytes()).sum();
        assert!(
            total <= LOW,
            "size pressure left total {total} above low watermark {LOW}"
        );
        // The flushed data is durable committed tree (read-backable).
        assert_eq!(
            store.read(b"big", &p(&["b"])).unwrap(),
            Some(vec![3u8; 6000])
        );
    }

    /// rev1§4.4 M-4: because flushing starts at the low watermark, steady traffic
    /// keeps total dirty bytes around the low watermark and never reaches the high
    /// watermark (`global_budget`) — the writer "rarely hits FULL at the high one".
    #[test]
    fn low_watermark_shields_high_watermark_under_steady_writes() {
        const LOW: usize = 8 * 1024;
        const HIGH: usize = 16 * 1024;
        let opts = StoreOptions {
            wal_len: 1 << 20,
            global_budget: HIGH,
            per_ref_budget: 1 << 20, // loose: size pressure is the only flusher
            op_count_bound: u64::MAX,
            size_low_watermark: LOW,
            ..test_opts()
        };
        let mut store = Store::format(MemDev::new(8 << 20), opts).unwrap();
        let refs: [&[u8]; 4] = [b"r0", b"r1", b"r2", b"r3"];
        for r in &refs {
            store.create_ref(r).unwrap();
        }
        // Steady ~1 KiB round-robin writes: after each, the low-watermark flush
        // has already run, so total stays at or below LOW, far under HIGH.
        for i in 0..64u64 {
            let r = refs[(i % 4) as usize];
            store
                .write(r, &p(&[&format!("f{i}")]), 0, &[0x7Eu8; 1024], i + 1)
                .unwrap();
            let total: usize = store.overlays.values().map(|o| o.bytes()).sum();
            assert!(
                total < HIGH,
                "total {total} reached the high watermark {HIGH}"
            );
            assert!(
                total <= LOW,
                "total {total} above low watermark {LOW} after the flush"
            );
        }
        // Flushing demonstrably happened: 64 KiB was written, but the live dirty
        // set is bounded by the low watermark.
        let total: usize = store.overlays.values().map(|o| o.bytes()).sum();
        assert!(total <= LOW);
    }

    // ── B12C: circular WAL ring + flush-the-pinner (rev1§4.4 trigger 2, M-5) ──

    /// Options that isolate the WAL-pressure trigger: a small ring with the given
    /// watermark, every *other* flush trigger disabled (huge byte budgets, no
    /// op-count / staleness / per-ref bound). So only the circular-WAL scheduler
    /// flushes and the tests observe it in isolation.
    fn wal_opts(wal_len: u64, wal_watermark: u64) -> StoreOptions {
        StoreOptions {
            wal_len,
            wal_watermark,
            global_budget: 1 << 30,
            size_low_watermark: 1 << 30,
            per_ref_budget: 1 << 30,
            op_count_bound: u64::MAX,
            ..test_opts()
        }
    }

    /// rev1§4.4 M-5 headline: WAL pressure flushes the ref **pinning the tail**
    /// (the oldest record, at `wal_head`), not everything. A pinner sits at the
    /// head while a newer ref fills the ring; crossing the watermark flushes the
    /// pinner — its overlay becomes committed tree and `wal_head` advances past
    /// it, reclaiming its WAL span — while the newer ref stays dirty (versus the
    /// pre-B12 flush-everything-and-reset).
    #[test]
    fn wal_pressure_flushes_pinner_keeps_newer_dirty() {
        let wal_len = 8 * 1024u64;
        let mut store =
            Store::format(MemDev::new(1 << 20), wal_opts(wal_len, wal_len / 2)).unwrap();
        store.create_ref(b"pin").unwrap();
        store.create_ref(b"new").unwrap();

        // The pinner's single (larger) record is the oldest in the WAL: it sits
        // at offset 0 == wal_head.
        store.write(b"pin", &p(&["p"]), 0, &[1u8; 512], 1).unwrap();
        assert_eq!(
            store.acct.get(b"pin".as_slice()).unwrap().oldest_wal_pos,
            Some(0)
        );
        let head_before = store.sb.wal_head;

        // Hammer 'new' until the scheduler flushes the pinner (the front of the
        // queue). Smaller 'new' records, so flushing the larger pinner drops usage
        // well below the watermark and the relief loop stops before touching 'new'.
        let mut i = 0u64;
        while store.acct.contains_key(b"pin".as_slice()) {
            store
                .write(b"new", &p(&[&format!("n{i}")]), 0, &[2u8; 64], i + 2)
                .unwrap();
            i += 1;
            assert!(i < 100_000, "pinner never flushed");
        }
        // The pinner flushed; the newer ref stayed dirty (M-5, not flush-everything).
        assert!(
            store
                .overlays
                .get(b"pin".as_slice())
                .map_or(true, |o| o.is_empty()),
            "pinner overlay should be flushed to committed tree"
        );
        assert!(
            store
                .overlays
                .get(b"new".as_slice())
                .map_or(false, |o| !o.is_empty()),
            "newer ref should still be dirty after flush-the-pinner"
        );
        // The head advanced past the flushed pinner — its WAL span was reclaimed.
        assert_ne!(
            store.sb.wal_head, head_before,
            "wal_head did not advance past the flushed pinner"
        );
        // The flushed data is durable committed tree (read-backable).
        assert_eq!(
            store.read(b"pin", &p(&["p"])).unwrap(),
            Some(vec![1u8; 512])
        );
    }

    /// rev1§4.4 M-5 across a ring **wrap**: the victim is the front of the WAL
    /// queue (the record at `wal_head`), *not* the ref with the smallest
    /// `oldest_wal_pos` — those differ once the ring has wrapped (a newer ref sits
    /// at a lower physical offset than the pinner). Flushing the front advances
    /// the head (progress); flushing the min-offset ref would not. Also exercises
    /// a straddling record and recovery across the wrap on remount.
    #[test]
    fn ring_wrap_front_pinner_reclaim_and_remount() {
        // Watermark == wal_len, so the auto-scheduler fires only on a genuine
        // won't-fit; we drive flushes explicitly to control head/tail precisely.
        // Single-char refs + fixed-length paths + fixed data ⇒ uniform record size.
        // Miri interprets blake3 per write, so use a small ring there — it still
        // wraps and exercises the front-pinner reclaim/remount scenario within a
        // handful of records (rsz stays < wal_len/4); native keeps the 16 KiB ring.
        let wal_len: u64 = if cfg!(miri) { 2 * 1024 } else { 16 * 1024 };
        let opts = wal_opts(wal_len, wal_len);
        let mut store = Store::format(MemDev::new(1 << 20), opts).unwrap();
        for r in [b"a".as_slice(), b"p", b"w", b"n"] {
            store.create_ref(r).unwrap();
        }
        let data = [0u8; 200];
        let mut seq = 1u64;
        let mut write = |s: &mut Store<MemDev>, r: &[u8]| {
            s.write(r, &p(&[&format!("{seq:05}")]), 0, &data, seq)
                .unwrap();
            let used = format!("{seq:05}");
            seq += 1;
            used
        };

        // Measure the uniform record size.
        write(&mut store, b"a");
        let rsz = store.wal_tail;
        assert!(rsz > 0 && rsz < wal_len / 4);

        // 'a' block to ~half the ring.
        while store.wal_tail + rsz <= wal_len / 2 {
            write(&mut store, b"a");
        }
        let pin_off = store.wal_tail; // the pinner record will sit here (mid-ring)
        write(&mut store, b"p"); // the future tail-pinner
                                 // 'a' block again, up to near the end.
        while store.wal_tail + rsz <= wal_len {
            write(&mut store, b"a");
        }

        // Flush 'a': its records before the pinner are the contiguous flushed
        // prefix, so the head advances to the pinner; the 'a' records after the
        // pinner stay flushed-but-unpopped.
        store.flush_ref(b"a").unwrap();
        store.commit().unwrap();
        assert_eq!(
            store.sb.wal_head, pin_off,
            "head should advance past the 'a' prefix to the pinner"
        );

        // A 'w' record straddles the buffer end into the freed low region, moving
        // the tail to a low offset; then 'n' lands at that low offset.
        let tail_before_wrap = store.wal_tail;
        assert!(
            tail_before_wrap + rsz > wal_len,
            "the 'w' write should straddle the wrap"
        );
        let w_path = write(&mut store, b"w");
        assert!(
            store.wal_tail < tail_before_wrap,
            "tail should have wrapped to a low offset"
        );
        let n_path = write(&mut store, b"n");
        let n_off = store
            .acct
            .get(b"n".as_slice())
            .unwrap()
            .oldest_wal_pos
            .unwrap();
        assert!(
            n_off < pin_off,
            "the newer ref ({n_off}) should sit below the pinner ({pin_off}) after the wrap"
        );

        // Victim selection: the front of the queue is the pinner 'p', even though
        // 'n' has the smallest oldest_wal_pos. Min-by-oldest_wal_pos would be wrong.
        assert_eq!(store.wal_records.front().unwrap().ref_name, b"p".to_vec());
        let min_ref = store
            .acct
            .iter()
            .min_by_key(|(_, a)| a.oldest_wal_pos.unwrap())
            .unwrap()
            .0
            .clone();
        assert_eq!(
            min_ref,
            b"n".to_vec(),
            "sanity: a min-offset victim policy would (wrongly) pick 'n'"
        );

        // Flushing the front pinner advances the head; flushing 'n' would not.
        let head_before = store.sb.wal_head;
        store.flush_ref(b"p").unwrap();
        store.commit().unwrap();
        assert_ne!(
            store.sb.wal_head, head_before,
            "flushing the front pinner must advance wal_head"
        );
        assert!(!store.acct.contains_key(b"p".as_slice()), "pinner flushed");
        assert!(
            store.acct.contains_key(b"n".as_slice()),
            "newer ref still dirty"
        );

        // Remount: rotation linearizes the ring and reassembles the straddling
        // 'w' record; every live and flushed value reads back.
        let dev = store.into_dev();
        let recovered = Store::mount(dev, opts).unwrap();
        assert_eq!(
            recovered.read(b"w", &p(&[&w_path])).unwrap(),
            Some(vec![0u8; 200]),
            "straddling record lost across remount"
        );
        assert_eq!(
            recovered.read(b"n", &p(&[&n_path])).unwrap(),
            Some(vec![0u8; 200])
        );
        // A flushed-'a' record (committed tree before the wrap) survives too.
        assert_eq!(
            recovered.read(b"a", &p(&["00001"])).unwrap(),
            Some(vec![0u8; 200])
        );
    }

    /// rev1§4.4 edge case: an exactly-full ring. With `wal_len` a multiple of the
    /// record size, the tail wraps back exactly onto the head — `wal_usage` must
    /// report `wal_len` (full), not `0` (empty). The next write then relieves; one
    /// ref pinning the whole ring degenerates to flush-everything-and-reset.
    #[test]
    fn ring_exactly_full_reports_full_then_relieves() {
        // Measure the record size on a scratch store, then size the ring to an
        // exact multiple of it.
        let mut scratch = Store::format(MemDev::new(1 << 20), wal_opts(1 << 16, 1 << 16)).unwrap();
        scratch.create_ref(b"a").unwrap();
        scratch
            .write(b"a", &p(&["00001"]), 0, &[0u8; 200], 1)
            .unwrap();
        let rsz = scratch.wal_tail;

        let wal_len = 4 * rsz;
        let opts = wal_opts(wal_len, wal_len);
        let mut store = Store::format(MemDev::new(1 << 20), opts).unwrap();
        store.create_ref(b"a").unwrap();
        // Four records fill the ring exactly: tail wraps to 0 == head.
        for s in 1..=4u64 {
            store
                .write(b"a", &p(&[&format!("{s:05}")]), 0, &[0u8; 200], s)
                .unwrap();
        }
        assert_eq!(store.wal_tail, 0, "tail should wrap exactly onto the head");
        assert!(!store.wal_records.is_empty());
        assert_eq!(
            store.wal_usage(),
            wal_len,
            "an exactly-full ring must report full, not empty"
        );

        // The fifth write can't fit: relief flushes the lone pinner — i.e.
        // everything — and the ring resets, so the write proceeds.
        store
            .write(b"a", &p(&["00005"]), 0, &[1u8; 200], 5)
            .unwrap();
        // The earlier records flushed to committed tree (read-backable).
        assert_eq!(
            store.read(b"a", &p(&["00001"])).unwrap(),
            Some(vec![0u8; 200])
        );
        assert_eq!(
            store.read(b"a", &p(&["00005"])).unwrap(),
            Some(vec![1u8; 200])
        );
    }

    /// rev1§4.4 edge case: a record whose **header** (not just payload) straddles
    /// the wrap. The write must split mid-header, and the mount-time rotation must
    /// reassemble it so `decode_frame` reads a contiguous header.
    #[test]
    fn wal_record_header_straddles_wrap() {
        // Measure the record size, then size the ring so a record boundary lands
        // 30 bytes (< WAL_HEADER = 48) before the buffer end.
        let mut scratch = Store::format(MemDev::new(1 << 20), wal_opts(1 << 16, 1 << 16)).unwrap();
        scratch.create_ref(b"a").unwrap();
        scratch
            .write(b"a", &p(&["00001"]), 0, &[0u8; 200], 1)
            .unwrap();
        let rsz = scratch.wal_tail;
        assert!(rsz > WAL_HEADER as u64 + 30);

        let wal_len = 5 * rsz + 30;
        let opts = wal_opts(wal_len, wal_len);
        let mut store = Store::format(MemDev::new(1 << 20), opts).unwrap();
        store.create_ref(b"a").unwrap();
        store.create_ref(b"b").unwrap();
        store.create_ref(b"c").unwrap();

        let mut seq = 1u64;
        let mut write = |s: &mut Store<MemDev>, r: &[u8]| {
            let path = format!("{seq:05}");
            s.write(r, &p(&[&path]), 0, &[7u8; 200], seq).unwrap();
            seq += 1;
            path
        };

        // One 'a' record, then 'b' records until the tail is exactly 30 bytes
        // before the end (a record boundary at wal_len - 30).
        write(&mut store, b"a");
        while store.wal_tail + rsz <= wal_len {
            write(&mut store, b"b");
        }
        assert_eq!(
            store.wal_tail,
            wal_len - 30,
            "tail should sit 30 bytes before the end"
        );

        // Flush 'a' to free the low region so the next write doesn't trip the
        // won't-fit relief (it would flush everything and reset to 0).
        store.flush_ref(b"a").unwrap();
        store.commit().unwrap();

        // 'c' starts at wal_len - 30: its 48-byte header straddles the wrap.
        let c_path = write(&mut store, b"c");
        assert!(
            store.wal_tail < wal_len - 30,
            "the 'c' write should have wrapped"
        );

        let dev = store.into_dev();
        let recovered = Store::mount(dev, opts).unwrap();
        assert_eq!(
            recovered.read(b"c", &p(&[&c_path])).unwrap(),
            Some(vec![7u8; 200]),
            "record with a header straddling the wrap lost across remount"
        );
    }

    /// rev1§4.4 normative edge case: an oversized record (larger than the whole
    /// WAL region) bypasses the log and commits synchronously before ack — even
    /// while the ring already holds live records. The bypass must not corrupt the
    /// ring's tail; the prior live record and the oversized write both survive.
    #[test]
    fn oversized_write_while_ring_nonempty() {
        let wal_len = 4 * 1024u64;
        let opts = wal_opts(wal_len, wal_len);
        let mut store = Store::format(MemDev::new(4 << 20), opts).unwrap();
        store.create_ref(b"a").unwrap();
        store.create_ref(b"b").unwrap();

        // A normal live record on the ring.
        store.write(b"a", &p(&["small"]), 0, &[1u8; 64], 1).unwrap();
        assert!(!store.wal_records.is_empty());

        // An oversized write (record > wal_len) to a different ref: bypass.
        let big = vec![2u8; wal_len as usize + 1024];
        store.write(b"b", &p(&["big"]), 0, &big, 2).unwrap();

        // Both read back; the ring tail stayed in range.
        assert_eq!(
            store.read(b"a", &p(&["small"])).unwrap(),
            Some(vec![1u8; 64])
        );
        assert_eq!(store.read(b"b", &p(&["big"])).unwrap(), Some(big.clone()));
        assert!(store.wal_tail < wal_len);

        // And it all survives a remount across the bypass.
        let dev = store.into_dev();
        let recovered = Store::mount(dev, opts).unwrap();
        assert_eq!(
            recovered.read(b"a", &p(&["small"])).unwrap(),
            Some(vec![1u8; 64])
        );
        assert_eq!(recovered.read(b"b", &p(&["big"])).unwrap(), Some(big));
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.4 M-5 ring-arithmetic guard: across arbitrary multi-ref write
        /// streams on a small ring (so it wraps repeatedly and flush-the-pinner
        /// fires), the WAL invariants hold after every op — `wal_head` equals the
        /// front record's offset, `wal_usage` equals the independent sum of live
        /// record spans, and usage never exceeds `wal_len` (relief always makes
        /// room). A min-offset victim policy (the wrap bug) would stall relief and
        /// blow the usage bound.
        #[test]
        fn wal_ring_invariants_hold_across_random_wraps(
            ops in proptest::collection::vec((0usize..3, 1usize..400), 1..200),
        ) {
            let wal_len = 4096u64;
            let opts = wal_opts(wal_len, wal_len / 2);
            // Native: an ample chunk region, since frequent flushing without GC
            // accumulates dead chunks (stop cleanly if it still fills — chunk
            // capacity is not what this test is about, the WAL ring is). Miri
            // interprets blake3 and tracks every device byte, so there it uses a
            // small device and a short op stream — the wrap still happens within a
            // handful of records, and the deterministic ring tests carry the rest
            // of the Miri UB coverage.
            let dev_bytes = if cfg!(miri) { 2 << 20 } else { 64 << 20 };
            let max_ops = if cfg!(miri) { 16 } else { ops.len() };
            let mut store = Store::format(MemDev::new(dev_bytes), opts).unwrap();
            let refs: [&[u8]; 3] = [b"r0", b"r1", b"r2"];
            for r in &refs {
                store.create_ref(r).unwrap();
            }
            for (i, (ri, len)) in ops.iter().take(max_ops).enumerate() {
                let data = vec![0x33u8; *len];
                if store
                    .write(refs[*ri], &p(&[&format!("{i:05}")]), 0, &data, (i + 1) as u64)
                    .is_err()
                {
                    break; // chunk region full (no GC here) — WAL invariants already checked
                }

                // Invariant 1: the head tracks the front of the queue.
                if let Some(front) = store.wal_records.front() {
                    prop_assert_eq!(store.sb.wal_head, front.off);
                }
                // Invariant 2: usage equals the independent sum of live spans
                // (each record tiles [its off, the next off) around the ring).
                let offs: Vec<u64> = store.wal_records.iter().map(|r| r.off).collect();
                if offs.is_empty() {
                    prop_assert_eq!(store.wal_usage(), 0);
                } else {
                    let mut sum = 0u64;
                    for w in 0..offs.len() {
                        let next = if w + 1 < offs.len() { offs[w + 1] } else { store.wal_tail };
                        sum += (next + wal_len - offs[w]) % wal_len;
                    }
                    let sum = if sum == 0 { wal_len } else { sum };
                    prop_assert_eq!(store.wal_usage(), sum);
                }
                // Invariant 3: relief keeps usage within the ring.
                prop_assert!(store.wal_usage() <= wal_len);
            }
        }
    }

    proptest! {
        // Crash-injection: the storage-layer convention (64 native / 4 Miri).
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 64 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.4 M-5 under crash injection: the circular-WAL flush-the-pinner
        /// path — selective flush, partial head advance, **split writes across the
        /// wrap** — preserves all-acked-survives. A small ring (not record-aligned)
        /// with a 50% watermark makes writes wrap and straddle and the scheduler
        /// flush mid-stream; the crash point can land between the two halves of a
        /// split write. Every acked write must still recover from durable state.
        #[test]
        fn crash_recovery_survives_wal_wrap(
            ops in proptest::collection::vec(
                (0u8..3, 0u8..6, 0u64..200, proptest::collection::vec(any::<u8>(), 1..200)),
                1..60,
            ),
            fail_at in 4u64..600,
            crash_seed in any::<u64>(),
        ) {
            let wal_len = 3000u64; // small, deliberately not record-aligned
            let opts = wal_opts(wal_len, wal_len / 2);
            let refs: [&[u8]; 3] = [b"r0", b"r1", b"r2"];
            let mut store = Store::format(CrashDev::new(1 << 20), opts).unwrap();
            for r in &refs {
                store.create_ref(r).unwrap();
            }
            store.dev_mut().set_fail_after(fail_at);

            let mut model: std::collections::HashMap<(usize, Path), Vec<u8>> =
                std::collections::HashMap::new();
            let mut inflight: Option<((usize, Path), Vec<u8>)> = None;

            // Miri interprets blake3, so cap the op stream there (still wraps and
            // flushes within a handful of records); native runs the full stream.
            let max_ops = if cfg!(miri) { 12 } else { ops.len() };
            for (rsel, psel, off, data) in ops.iter().take(max_ops) {
                let ri = *rsel as usize;
                let path = p(&[&format!("f{psel}")]);
                let mut content = model.get(&(ri, path.clone())).cloned().unwrap_or_default();
                let end = *off as usize + data.len();
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[*off as usize..end].copy_from_slice(data);
                let r = store.write(refs[ri], &path, *off, data, 1);
                if r.is_ok() {
                    model.insert((ri, path), content);
                } else {
                    inflight = Some(((ri, path), content));
                    break;
                }
            }

            let mut dev = store.into_dev();
            dev.clear_fail();
            dev.crash(crash_seed);
            let recovered = Store::mount(dev, opts).unwrap();

            for ((ri, path), expect) in &model {
                let got = recovered.read(refs[*ri], path).unwrap();
                let matches_model = got.as_deref() == Some(expect.as_slice());
                let matches_inflight = inflight.as_ref().is_some_and(|((iri, ip), iv)| {
                    iri == ri && ip == path && got.as_deref() == Some(iv.as_slice())
                });
                prop_assert!(
                    matches_model || matches_inflight,
                    "ref {} path {:?}: got {:?}, want {:?} (inflight {:?})",
                    ri,
                    path,
                    got,
                    expect,
                    inflight
                );
            }
        }
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.4 M-4 containment under arbitrary multi-ref write interleavings:
        /// the per-ref soft bound caps each ref, and size pressure caps the total,
        /// so dirty bytes stay at or below the low watermark and the high watermark
        /// (`global_budget`) is never reached — except the documented one-ref guard
        /// (size pressure never empties the store), which the assertion allows for.
        #[test]
        fn size_pressure_holds_total_below_high_watermark(
            ops in proptest::collection::vec((0usize..4, 1usize..600), 1..120),
        ) {
            const LOW: usize = 8 * 1024;
            const HIGH: usize = 16 * 1024;
            let opts = StoreOptions {
                wal_len: 1 << 20,
                global_budget: HIGH,
                // Per-ref bound at the low watermark contains a single hot ref, so
                // the one-ref guard never strands a ref above LOW (matches the spec
                // relationship per_ref_budget <= global_budget).
                per_ref_budget: LOW,
                op_count_bound: u64::MAX,
                size_low_watermark: LOW,
                ..test_opts()
            };
            let mut store = Store::format(MemDev::new(8 << 20), opts).unwrap();
            let refs: [&[u8]; 4] = [b"r0", b"r1", b"r2", b"r3"];
            for r in &refs {
                store.create_ref(r).unwrap();
            }
            let max_ops = if cfg!(miri) { 16 } else { ops.len() };
            for (i, (ri, len)) in ops.iter().take(max_ops).enumerate() {
                let data = vec![0x5Au8; *len];
                store
                    .write(refs[*ri], &p(&[&format!("f{i}")]), 0, &data, (i + 1) as u64)
                    .unwrap();
                let total: usize = store.overlays.values().map(|o| o.bytes()).sum();
                prop_assert!(total < HIGH, "total {} reached the high watermark {}", total, HIGH);
                prop_assert!(
                    total <= LOW || store.overlays.len() <= 1,
                    "total {} above low watermark with {} dirty refs",
                    total,
                    store.overlays.len()
                );
            }
        }
    }

    proptest! {
        // Crash-injection: the storage-layer convention (64 native / 4 Miri).
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 64 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.4 M-4 under crash injection: the size-pressure flush-the-biggest-
        /// offenders path (`flush_ref` + `commit`, partial head advance) preserves
        /// all-acked-survives. Multi-ref writes of unequal size cross a tight low
        /// watermark mid-stream, so the crash point can land inside a *partial*
        /// size-pressure flush (some refs flushed, some still dirty); every acked
        /// write must still recover from durable state.
        ///
        /// A dedicated multi-ref test, not an extension of the single-ref
        /// `crash_recovery_preserves_acked_state`: the "one ref remains" guard means
        /// size pressure never flushes a lone ref, so only a multi-ref workload
        /// witnesses "some flushed, some not". Size pressure is the flusher here
        /// (ample WAL, loose per-ref/op-count bounds).
        #[test]
        fn crash_recovery_survives_size_pressure_flush(
            ops in proptest::collection::vec(
                (0u8..3, 0u8..4, 0u64..200, proptest::collection::vec(any::<u8>(), 1..200)),
                1..40,
            ),
            fail_at in 4u64..600,
            crash_seed in any::<u64>(),
        ) {
            let opts = StoreOptions {
                wal_len: 64 * 1024,      // ample for the small op stream; fits the
                                         // 1 MiB device and isolates the size path
                                         // from the WAL-full trigger
                global_budget: 16 * 1024,
                per_ref_budget: 1 << 20, // loose: size pressure (not per-ref) flushes
                op_count_bound: u64::MAX,
                size_low_watermark: 4 * 1024,
                ..test_opts()
            };
            let refs: [&[u8]; 3] = [b"r0", b"r1", b"r2"];
            let mut store = Store::format(CrashDev::new(1 << 20), opts).unwrap();
            for r in &refs {
                store.create_ref(r).unwrap();
            }
            store.dev_mut().set_fail_after(fail_at);

            // Acked logical state keyed by (ref index, path); the failing write
            // (if any) is the one ambiguous mutation.
            let mut model: std::collections::HashMap<(usize, Path), Vec<u8>> =
                std::collections::HashMap::new();
            let mut inflight: Option<((usize, Path), Vec<u8>)> = None;

            for (rsel, psel, off, data) in &ops {
                let ri = *rsel as usize;
                let path = p(&[&format!("f{psel}")]);
                let mut content = model.get(&(ri, path.clone())).cloned().unwrap_or_default();
                let end = *off as usize + data.len();
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[*off as usize..end].copy_from_slice(data);
                let r = store.write(refs[ri], &path, *off, data, 1);
                if r.is_ok() {
                    model.insert((ri, path), content);
                } else {
                    inflight = Some(((ri, path), content));
                    break;
                }
            }

            let mut dev = store.into_dev();
            dev.clear_fail();
            dev.crash(crash_seed);
            let recovered = Store::mount(dev, opts).unwrap();

            for ((ri, path), expect) in &model {
                let got = recovered.read(refs[*ri], path).unwrap();
                let matches_model = got.as_deref() == Some(expect.as_slice());
                let matches_inflight = inflight.as_ref().is_some_and(|((iri, ip), iv)| {
                    iri == ri && ip == path && got.as_deref() == Some(iv.as_slice())
                });
                prop_assert!(
                    matches_model || matches_inflight,
                    "ref {} path {:?}: got {:?}, want {:?} (inflight {:?})",
                    ri,
                    path,
                    got,
                    expect,
                    inflight
                );
            }
        }
    }

    /// B7B/T-5: the structural decode split out of `wal_content_ok` and verified
    /// like the other on-disk decoders. This test gives the verified predicate
    /// *teeth* (verus.md §11): it must accept well-formed records and reject
    /// structurally-malformed ones, and the blake3 half must stay independent.
    #[test]
    fn wal_struct_ok_has_teeth() {
        // A real Write record's structure is accepted, and (with its real
        // checksum) so is the full content predicate.
        let write = WalOp::Write {
            ref_name: b"root".to_vec(),
            path: vec![b"dir".to_vec(), b"file".to_vec()],
            offset: 42,
            mtime: 7,
            data: b"hello world".to_vec(),
        };
        let rec = write.encode_record(1);
        assert!(wal_struct_ok(&rec, 0, rec.len()));
        assert!(wal_content_ok(&rec, 0, rec.len()));

        // A complete Unlink record is accepted too (the other tag).
        let unlink = WalOp::Unlink {
            ref_name: b"root".to_vec(),
            path: vec![b"x".to_vec()],
            mtime: 3,
        };
        let urec = unlink.encode_record(2);
        assert!(wal_struct_ok(&urec, 0, urec.len()));

        // The blake3 half still bites: corrupt one checksum byte and the
        // structure still decodes, but content acceptance fails.
        let mut bad_cksum = rec.clone();
        bad_cksum[16] ^= 0xFF;
        assert!(wal_struct_ok(&bad_cksum, 0, bad_cksum.len()));
        assert!(!wal_content_ok(&bad_cksum, 0, bad_cksum.len()));

        // Teeth — structurally-malformed payloads must be rejected:
        // (a) unknown op tag.
        let bad_tag = framed(&[99]);
        assert!(!wal_struct_ok(&bad_tag, 0, bad_tag.len()));
        // (b) trailing bytes after a complete Unlink op (the `done()` check).
        //     payload = tag(2) · rl(0) · path-count(0) · mtime(8 zero bytes).
        let mut unlink_payload = vec![2u8, 0u8, 0u8];
        unlink_payload.extend_from_slice(&0u64.to_le_bytes());
        let mut trailing = unlink_payload.clone();
        trailing.push(0xFF);
        let with_trailing = framed(&trailing);
        assert!(!wal_struct_ok(&with_trailing, 0, with_trailing.len()));
        // the same payload without the trailing byte is accepted.
        let exact = framed(&unlink_payload);
        assert!(wal_struct_ok(&exact, 0, exact.len()));
        // (c) a path component length that runs past the buffer.
        //     tag(2) · rl(0) · path-count(1) · comp-len(5) · <no comp bytes>.
        let truncated = framed(&[2, 0, 1, 5]);
        assert!(!wal_struct_ok(&truncated, 0, truncated.len()));
        // (d) empty payload (no tag byte at all).
        let empty = framed(&[]);
        assert!(!wal_struct_ok(&empty, 0, empty.len()));
        // (e) a record shorter than the header decodes nothing.
        assert!(!wal_struct_ok(&[0u8; 4], 0, 4));
    }

    /// B7C/T-2: the recovery core `recover_records` rebuilds the laid-out run and
    /// flags the seq-exhaustion forgery — teeth for the discharge that makes
    /// `lemma_gap_freedom` live (verus.md §11). The `laid_out` ensures is checked
    /// by Verus; here we pin the *runtime* behaviour the proof rides on: the right
    /// records, the right cursor, and the `forged_max` boundary.
    #[test]
    fn recover_records_rebuilds_run_and_flags_forgery() {
        let r0 = WalOp::Write {
            ref_name: b"main".to_vec(),
            path: p(&["a"]),
            offset: 0,
            mtime: 1,
            data: b"hi".to_vec(),
        }
        .encode_record(0);
        let r1 = WalOp::Unlink {
            ref_name: b"main".to_vec(),
            path: p(&["a"]),
            mtime: 2,
        }
        .encode_record(1);

        // Two well-formed, seq-continuous records, then a zeroed (torn) tail.
        let mut wal = Vec::new();
        wal.extend_from_slice(&r0);
        wal.extend_from_slice(&r1);
        wal.resize(wal.len() + 64, 0);
        let rec = recover_records(&wal, 0, 0);
        assert_eq!(rec.records.len(), 2);
        assert!(!rec.forged_max);
        assert_eq!((rec.records[0].off, rec.records[0].seq), (0, 0));
        assert_eq!(
            (rec.records[1].off, rec.records[1].seq),
            (r0.len() as u64, 1)
        );
        assert_eq!(rec.end_off, (r0.len() + r1.len()) as u64);
        assert_eq!(rec.next_seq, 2);
        assert!(rec.records.iter().all(|m| !m.flushed));

        // A seq-discontinuous record is an unacked tail: the run stops at r0.
        let r1_gap = WalOp::Unlink {
            ref_name: b"main".to_vec(),
            path: p(&["a"]),
            mtime: 2,
        }
        .encode_record(5);
        let mut wal_gap = Vec::new();
        wal_gap.extend_from_slice(&r0);
        wal_gap.extend_from_slice(&r1_gap);
        wal_gap.resize(wal_gap.len() + 64, 0);
        let rec_gap = recover_records(&wal_gap, 0, 0);
        assert_eq!(rec_gap.records.len(), 1);
        assert!(!rec_gap.forged_max);

        // A valid record at the seq ceiling is the rev1§4.4 forgery: flagged, not
        // folded into the laid-out run (mount rejects it loudly).
        let r_max = WalOp::Write {
            ref_name: b"main".to_vec(),
            path: p(&["a"]),
            offset: 0,
            mtime: 1,
            data: b"x".to_vec(),
        }
        .encode_record(u64::MAX);
        let mut wal_max = Vec::new();
        wal_max.extend_from_slice(&r_max);
        wal_max.resize(wal_max.len() + 64, 0);
        let rec_max = recover_records(&wal_max, 0, u64::MAX);
        assert_eq!(rec_max.records.len(), 0);
        assert!(rec_max.forged_max);
    }

    #[test]
    fn write_read_sync_remount() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["etc", "conf"]), 0, b"hello", 1)
            .unwrap();
        store
            .write(b"main", &p(&["etc", "conf"]), 5, b" world", 2)
            .unwrap();
        assert_eq!(
            store.read(b"main", &p(&["etc", "conf"])).unwrap().unwrap(),
            b"hello world"
        );

        store.sync_ref(b"main").unwrap();
        assert_eq!(
            store.read(b"main", &p(&["etc", "conf"])).unwrap().unwrap(),
            b"hello world"
        );

        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(
            store2.read(b"main", &p(&["etc", "conf"])).unwrap().unwrap(),
            b"hello world"
        );
        let ls = store2.list(b"main", &p(&["etc"])).unwrap();
        assert_eq!(ls, vec![(b"conf".to_vec(), EntryKind::File, 11)]);
    }

    #[test]
    fn acked_write_survives_crash_without_sync() {
        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["a"]), 0, b"acked data", 1)
            .unwrap();
        // No sync: the tree never saw this write — only the fsynced WAL has it.
        let mut dev = store.into_dev();
        dev.crash(0xDEAD);
        let store2 = Store::mount(dev, test_opts()).unwrap();
        assert_eq!(
            store2.read(b"main", &p(&["a"])).unwrap().unwrap(),
            b"acked data"
        );
    }

    #[test]
    fn unlink_and_resurrect() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["f"]), 0, b"version one", 1)
            .unwrap();
        store.sync_ref(b"main").unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap(), None);
        store.write(b"main", &p(&["f"]), 2, b"x", 3).unwrap();
        // Fresh file after unlink: old content must not bleed through.
        assert_eq!(
            store.read(b"main", &p(&["f"])).unwrap().unwrap(),
            vec![0, 0, b'x']
        );
        store.sync_ref(b"main").unwrap();
        assert_eq!(
            store.read(b"main", &p(&["f"])).unwrap().unwrap(),
            vec![0, 0, b'x']
        );
    }

    // ── C2C: unlink-while-open + open handles (rev1§4.9, Design decision 2) ──

    /// rev1§4.9: "the open handle keeps working against the overlay" after its
    /// name is unlinked — the path reads as absent, but the handle still sees and
    /// can extend the data.
    #[test]
    fn open_handle_survives_unlink() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"held bytes", 1).unwrap();
        // Handle and path agree while named.
        assert_eq!(
            store.read(b"main", &p(&["f"])).unwrap().unwrap(),
            b"held bytes"
        );
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"held bytes");
        // Unlink the name: the path goes absent, the handle keeps working.
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap(), None);
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"held bytes");
        // A further write through the orphaned handle still lands (ephemerally).
        store.write_id(h, 10, b"!", 3).unwrap();
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"held bytes!");
        store.close(h).unwrap();
    }

    /// rev1§4.9: "if at flush time the ID resolves to no path, the data is
    /// discarded." The orphaned handle's data never reaches the tree.
    #[test]
    fn unlinked_open_data_discarded_at_flush() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"ephemeral", 1).unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        // Flush: the orphaned id resolves to no name → discarded, not committed.
        store.flush_ref(b"main").unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap(), None);
        // The handle keeps working post-flush, but its data is gone (discarded).
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"");
        store.close(h).unwrap();
        // Nothing was ever committed to the tree at `f`.
        store.sync_ref(b"main").unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap(), None);
    }

    /// rev1§4.9: closing the last handle on an orphaned id discards its dirty data
    /// at once — the overlay reclaims the bytes and the id is forgotten.
    #[test]
    fn close_reaps_orphaned_handle() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"orphan bytes", 1).unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        assert!(store.overlays.get(b"main".as_slice()).unwrap().bytes() > 0);
        store.close(h).unwrap();
        // The orphan's data is gone, and the handle is no longer known.
        assert_eq!(store.overlays.get(b"main".as_slice()).unwrap().bytes(), 0);
        assert!(store.open_files.is_empty());
        assert!(matches!(
            store.write_id(h, 0, b"x", 3),
            Err(StoreError::NoSuchHandle)
        ));
    }

    /// rev1§4.9: an open handle keeps working *across* a flush (which auto-flushes
    /// can trigger mid-write). A named handle's data commits; a later write
    /// through it re-materializes over the now-committed base.
    #[test]
    fn named_handle_survives_flush() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"abc", 1).unwrap();
        store.flush_ref(b"main").unwrap();
        // Committed to the tree, readable by path and by handle.
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap().unwrap(), b"abc");
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"abc");
        // A post-flush write through the surviving handle appends over the now-
        // committed base (re-materialized lazily from the tree).
        store.write_id(h, 3, b"def", 2).unwrap();
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"abcdef");
        store.sync_ref(b"main").unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap().unwrap(), b"abcdef");
        store.close(h).unwrap();
    }

    /// Opening a name and closing it without writing leaves no trace: no dirty
    /// state is introduced and the name still reads from the tree.
    #[test]
    fn open_unwritten_close_is_inert() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["f"]), 0, b"committed", 1)
            .unwrap();
        store.sync_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        assert_eq!(store.read_id(h).unwrap().unwrap(), b"committed");
        store.close(h).unwrap();
        assert_eq!(
            store.read(b"main", &p(&["f"])).unwrap().unwrap(),
            b"committed"
        );
        assert!(store.open_files.is_empty());
    }

    /// rev1§4.9 / work item 4: an open-then-unlinked id has no client across a
    /// crash (ids are ephemeral). The acked `Write`+`Unlink` records replay to the
    /// same path-visible state — `f` absent — with no special replay logic, and
    /// the orphaned handle's later ephemeral writes (never WAL-logged) are gone.
    #[test]
    fn unlink_while_open_survives_crash() {
        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"acked then unlinked", 1).unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        // A further ephemeral write through the orphaned handle (never logged).
        store.write_id(h, 100, b"ephemeral", 3).unwrap();
        assert_eq!(store.read(b"main", &p(&["f"])).unwrap(), None);
        let mut dev = store.into_dev();
        dev.crash(0xC2C);
        let store2 = Store::mount(dev, test_opts()).unwrap();
        // Replay of Write{f}+Unlink{f} reproduces the absence; no handle survives.
        assert_eq!(store2.read(b"main", &p(&["f"])).unwrap(), None);
    }

    // ── C2C negative controls (anti-theater): the interleaving oracle has teeth ──

    /// A model that *kept* an orphaned id's data at flush would predict the path
    /// still readable; the real store discards it, so the two must disagree.
    #[test]
    fn negative_control_flush_keeps_orphan() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"data", 1).unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        store.flush_ref(b"main").unwrap();
        let got = store.read_id(h).unwrap().unwrap();
        assert_eq!(got, b""); // correct: discarded
        assert_ne!(got, b"data".to_vec()); // a keep-orphan oracle would diverge
    }

    /// A model that *reaped* (dropped the data) on unlink-while-open would predict
    /// an empty handle read; the real handle keeps working, so they must disagree.
    #[test]
    fn negative_control_unlink_reaps_open() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let h = store.open(b"main", &p(&["f"])).unwrap();
        store.write_id(h, 0, b"held", 1).unwrap();
        store.unlink(b"main", &p(&["f"]), 2).unwrap();
        let got = store.read_id(h).unwrap().unwrap();
        assert_eq!(got, b"held"); // correct: handle keeps working
        assert_ne!(got, Vec::<u8>::new()); // a reap-on-unlink oracle would diverge
    }

    // ── C2C interleaving proptest: real Store vs a path-keyed handle model ──
    //
    // The reference model is a path-keyed naive store with explicit open-handle
    // tracking — the semantics the id indirection optimizes. Files are whole byte
    // vectors applied fresh each read (no interval maps); the committed tree is a
    // plain map (no chunk store). A non-fresh file's base is read lazily at read
    // time from the committed map — so an orphaned id reads against an empty base,
    // exactly as the real overlay does. `rename` joins the op set when C2B lands;
    // C2C exercises open/write/write_id/unlink/close/flush.

    #[derive(Default)]
    struct OpenModel {
        next_id: u64,
        committed: BTreeMap<Path, Vec<u8>>,
        by_name: BTreeMap<Path, u64>,
        name_of: BTreeMap<u64, Option<Path>>,
        files: BTreeMap<u64, ModelFile>,
        tombs: BTreeSet<Path>,
        open: BTreeMap<u64, u32>,
    }

    #[derive(Default)]
    struct ModelFile {
        writes: Vec<(u64, Vec<u8>)>,
        fresh: bool,
    }

    impl OpenModel {
        fn splice(content: &mut Vec<u8>, offset: u64, data: &[u8]) {
            let end = offset as usize + data.len();
            if content.len() < end {
                content.resize(end, 0);
            }
            content[offset as usize..end].copy_from_slice(data);
        }

        /// Apply an id's writes over its base — committed bytes for a non-fresh
        /// *named* id, empty for a fresh or *orphaned* (nameless) id.
        fn applied(&self, id: u64) -> Vec<u8> {
            let f = &self.files[&id];
            let mut content = match (f.fresh, self.name_of.get(&id)) {
                (false, Some(Some(path))) => self.committed.get(path).cloned().unwrap_or_default(),
                _ => Vec::new(),
            };
            for (off, data) in &f.writes {
                Self::splice(&mut content, *off, data);
            }
            content
        }

        fn bind(&mut self, path: &Path) -> u64 {
            if let Some(&id) = self.by_name.get(path) {
                id
            } else {
                let id = self.next_id;
                self.next_id += 1;
                self.by_name.insert(path.clone(), id);
                self.name_of.insert(id, Some(path.clone()));
                id
            }
        }

        fn write(&mut self, path: &Path, offset: u64, data: &[u8]) {
            let fresh = self.tombs.remove(path);
            let id = self.bind(path);
            let f = self.files.entry(id).or_insert(ModelFile {
                writes: Vec::new(),
                fresh,
            });
            f.writes.push((offset, data.to_vec()));
        }

        fn open(&mut self, path: &Path) -> u64 {
            let id = self.bind(path);
            *self.open.entry(id).or_insert(0) += 1;
            id
        }

        fn is_open(&self, id: u64) -> bool {
            self.open.get(&id).copied().unwrap_or(0) > 0
        }

        fn write_id(&mut self, id: u64, offset: u64, data: &[u8]) {
            match self.name_of.get(&id).cloned().flatten() {
                Some(path) => self.write(&path, offset, data),
                None => {
                    // Orphaned: empty base, ephemeral.
                    let f = self.files.entry(id).or_default();
                    f.writes.push((offset, data.to_vec()));
                }
            }
        }

        fn unlink(&mut self, path: &Path) {
            if let Some(id) = self.by_name.remove(path) {
                if self.is_open(id) {
                    self.name_of.insert(id, None);
                } else {
                    self.files.remove(&id);
                    self.name_of.remove(&id);
                }
            }
            self.tombs.insert(path.clone());
        }

        fn close(&mut self, id: u64) {
            match self.open.get_mut(&id) {
                Some(c) if *c > 1 => *c -= 1,
                Some(_) => {
                    self.open.remove(&id);
                    match self.name_of.get(&id) {
                        Some(None) => {
                            self.files.remove(&id);
                            self.name_of.remove(&id);
                        }
                        Some(Some(name)) if !self.files.contains_key(&id) => {
                            let name = name.clone();
                            self.by_name.remove(&name);
                            self.name_of.remove(&id);
                        }
                        _ => {}
                    }
                }
                None => {}
            }
        }

        fn flush(&mut self) {
            let named_dirty: Vec<(u64, Path)> = self
                .name_of
                .iter()
                .filter_map(|(id, n)| match n {
                    Some(p) if self.files.contains_key(id) => Some((*id, p.clone())),
                    _ => None,
                })
                .collect();
            for (id, path) in named_dirty {
                let c = self.applied(id);
                self.committed.insert(path, c);
            }
            for t in self.tombs.clone() {
                self.committed.remove(&t);
            }
            // Carry only open handles forward (rev1§4.9); non-open ids vanish.
            let keep: BTreeSet<u64> = self.open.keys().copied().collect();
            self.by_name.retain(|_, id| keep.contains(id));
            self.name_of.retain(|id, _| keep.contains(id));
            self.files.clear();
            self.tombs.clear();
        }

        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            if self.tombs.contains(path) {
                None
            } else if let Some(&id) = self.by_name.get(path) {
                if self.files.contains_key(&id) {
                    Some(self.applied(id))
                } else {
                    self.committed.get(path).cloned() // open-unwritten → tree
                }
            } else {
                self.committed.get(path).cloned()
            }
        }

        fn read_id(&self, id: u64) -> Option<Vec<u8>> {
            match self.name_of.get(&id) {
                Some(Some(path)) => self.read(&path.clone()),
                Some(None) => Some(
                    self.files
                        .get(&id)
                        .map(|_| self.applied(id))
                        .unwrap_or_default(),
                ),
                None => None,
            }
        }
    }

    #[derive(Clone, Debug)]
    enum OpenOp {
        Write(usize, u64, Vec<u8>),
        Unlink(usize),
        Open(usize),
        WriteId(usize, u64, Vec<u8>),
        Close(usize),
        Flush,
    }

    /// Budgets pinned high so no auto-flush (WAL/byte/op-count backpressure) fires
    /// — flushes happen only at explicit `Flush` ops, which the model mirrors. The
    /// open-handle semantics are still proven across flushes (the `Flush` op).
    fn no_autoflush_opts() -> StoreOptions {
        StoreOptions {
            // 64 KiB WAL holds ~50 tiny records without wrapping (so no WAL-
            // pressure flush); the default watermark is far above it.
            wal_len: 64 * 1024,
            global_budget: 1 << 30,
            per_ref_budget: 1 << 30,
            size_low_watermark: 1 << 30,
            op_count_bound: u64::MAX,
            staleness_ns: u64::MAX,
            ..test_opts()
        }
    }

    fn open_op() -> impl Strategy<Value = OpenOp> {
        const NPATHS: usize = 3;
        prop_oneof![
            (
                0usize..NPATHS,
                0u64..64,
                proptest::collection::vec(any::<u8>(), 1..16)
            )
                .prop_map(|(i, o, d)| OpenOp::Write(i, o, d)),
            (0usize..NPATHS).prop_map(OpenOp::Unlink),
            (0usize..NPATHS).prop_map(OpenOp::Open),
            (
                0usize..8,
                0u64..64,
                proptest::collection::vec(any::<u8>(), 1..16)
            )
                .prop_map(|(h, o, d)| OpenOp::WriteId(h, o, d)),
            (0usize..8).prop_map(OpenOp::Close),
            Just(OpenOp::Flush),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// The real id-addressed `Store` matches the path-keyed handle model
        /// op-for-op: path reads, handle reads, and post-flush durable state all
        /// agree, and the overlay's id indices stay internally consistent.
        #[test]
        fn rename_unlink_interleaving(
            // Cap the op stream under Miri (interpreted blake3 chunks on every
            // flush/sync — native-scale streams would take hours, CLAUDE.md).
            ops in proptest::collection::vec(open_op(), 1..if cfg!(miri) { 12 } else { 50 }),
        ) {
            // A small fixed name set: two at the root, one under a subdirectory.
            let names = [p(&["a"]), p(&["b"]), p(&["d", "x"])];
            let mut store = Store::format(MemDev::new(1 << 20), no_autoflush_opts()).unwrap();
            store.create_ref(b"main").unwrap();
            let mut model = OpenModel::default();
            // Live handles: (real FileId, model id). Ids are not assumed equal —
            // only the observable reads through them are compared.
            let mut handles: Vec<(FileId, u64)> = Vec::new();

            for op in &ops {
                match op {
                    OpenOp::Write(i, off, data) => {
                        store.write(b"main", &names[*i], *off, data, 1).unwrap();
                        model.write(&names[*i], *off, data);
                    }
                    OpenOp::Unlink(i) => {
                        store.unlink(b"main", &names[*i], 1).unwrap();
                        model.unlink(&names[*i]);
                    }
                    OpenOp::Open(i) => {
                        let h = store.open(b"main", &names[*i]).unwrap();
                        let mh = model.open(&names[*i]);
                        handles.push((h, mh));
                    }
                    OpenOp::WriteId(hi, off, data) => {
                        if handles.is_empty() {
                            continue;
                        }
                        let (h, mh) = handles[*hi % handles.len()];
                        store.write_id(h, *off, data, 1).unwrap();
                        model.write_id(mh, *off, data);
                    }
                    OpenOp::Close(hi) => {
                        if handles.is_empty() {
                            continue;
                        }
                        let (h, mh) = handles.remove(*hi % handles.len());
                        store.close(h).unwrap();
                        model.close(mh);
                    }
                    OpenOp::Flush => {
                        store.flush_ref(b"main").unwrap();
                        model.flush();
                    }
                }
                // The overlay's id indices stay internally consistent.
                if let Some(o) = store.overlays.get(b"main".as_slice()) {
                    o.check_invariants();
                }
                // Path reads and handle reads agree with the model.
                for nm in &names {
                    prop_assert_eq!(store.read(b"main", nm).unwrap(), model.read(nm));
                }
                for (h, mh) in &handles {
                    prop_assert_eq!(store.read_id(*h).unwrap(), model.read_id(*mh));
                }
            }
            // Durable cross-check: sync to the tree and re-read every path.
            store.sync_ref(b"main").unwrap();
            model.flush();
            for nm in &names {
                prop_assert_eq!(store.read(b"main", nm).unwrap(), model.read(nm));
            }
        }
    }

    #[test]
    fn snapshot_rollback() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["doc"]), 0, b"original", 10)
            .unwrap();
        let snap = store
            .snapshot(
                b"main",
                b"session=test",
                b"before edit",
                disk::CLASS_KEEP,
                100,
            )
            .unwrap();
        store
            .write(b"main", &p(&["doc"]), 0, b"MODIFIED", 11)
            .unwrap();
        store.sync_ref(b"main").unwrap();
        assert_eq!(
            store.read(b"main", &p(&["doc"])).unwrap().unwrap(),
            b"MODIFIED"
        );

        // Snapshot reads see the old root.
        let root = store.snapshot_root(b"main", snap).unwrap();
        assert_eq!(
            store.read_at_root(&root, &p(&["doc"])).unwrap().unwrap(),
            b"original"
        );

        store.rollback(b"main", snap).unwrap();
        assert_eq!(
            store.read(b"main", &p(&["doc"])).unwrap().unwrap(),
            b"original"
        );

        // Snapshot identity is the per-ref sequence number (rev1§4.7).
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
            store
                .write(b"main", &path, (i as u64) * 16, &i.to_le_bytes(), i as u64)
                .unwrap();
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
        store
            .write(b"main", &p(&["a"]), 0, b"committed", 1)
            .unwrap();
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
            store
                .write(b"main", &p(&["a"]), 0, b"NEWERDATA", 2)
                .unwrap();
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
    fn resurrection_rewrites_condemned_chunk() {
        // The I-3 witness (rev1§4.6 step 3): a dedup lookup that hits a chunk
        // the in-flight sweep has condemned must rewrite under the same hash,
        // not resurrect the about-to-be-deleted index entry. Pre-B6A this
        // dedup'd straight onto the condemned entry — the resurrection bug.
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        let content = b"resurrect me";

        let h = store.chunks.put(content);
        assert!(store.chunks.io_error.is_none());
        let e1 = *store.chunks.index.get(&h).unwrap();

        // Simulate a sweep that marked the live set *without* this chunk.
        store.chunks.condemned.insert(h);
        let h2 = store.chunks.put(content);
        assert!(store.chunks.io_error.is_none());
        assert_eq!(h2, h, "same content hashes the same");

        let e2 = *store.chunks.index.get(&h).unwrap();
        assert_ne!(
            e2.off, e1.off,
            "condemned hit must rewrite to a fresh extent"
        );
        assert_eq!(
            e2.birth, store.chunks.birth_gen,
            "the rewrite carries the current birth_gen (>= epoch)"
        );
        assert!(
            !store.chunks.condemned.contains(&h),
            "resurrection cancels the chunk's condemnation"
        );
        assert_eq!(
            store.chunks.get(&h),
            Some(content.to_vec()),
            "the rewritten chunk reads back correctly"
        );
    }

    #[test]
    fn live_dedup_unaffected_outside_sweep() {
        // With no sweep in flight (`condemned` empty), a re-put dedups: same
        // hash, same index entry, no fresh extent — the rev1§4.3 fast path and
        // the `is_empty` short-circuit are preserved.
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        let content = b"dedup me";

        let h = store.chunks.put(content);
        assert!(store.chunks.io_error.is_none());
        assert!(store.chunks.condemned.is_empty());
        let e1 = *store.chunks.index.get(&h).unwrap();
        let tail = store.chunks.tail;

        let h2 = store.chunks.put(content);
        assert_eq!(h2, h);
        let e2 = *store.chunks.index.get(&h).unwrap();
        assert_eq!(e2, e1, "live dedup leaves the index entry untouched");
        assert_eq!(
            store.chunks.tail, tail,
            "live dedup allocates no new extent"
        );
    }

    #[test]
    fn gc_closes_condemned_window() {
        // After a synchronous gc() returns, the resurrection-check window is
        // closed: no chunk remains condemned until the next cycle reopens it.
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["f"]), 0, &[7u8; 5000], 1)
            .unwrap();
        store.sync_ref(b"main").unwrap();
        // Supersede the file so its old chunks become condemnable garbage.
        store
            .write(b"main", &p(&["f"]), 0, &[9u8; 5000], 2)
            .unwrap();
        store.sync_ref(b"main").unwrap();

        let stats = store.gc().unwrap();
        assert!(stats.freed_objects > 0, "expected reclaimable garbage");
        assert!(
            store.chunks.condemned.is_empty(),
            "gc must close the resurrection-check window"
        );
    }

    #[test]
    fn gc_reclaims_superseded_roots_and_reuses_space() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();

        // Churn: each iteration supersedes the previous root and file
        // chunks; without reclamation `used` grows without bound, with it
        // the footprint stays flat (freed extents get reused).
        let mut used_after_first = 0;
        // Miri interprets blake3 over every chunk, so churn fewer/smaller writes
        // there — the supersede→reclaim→reuse invariant holds at any scale.
        let iters: u8 = if cfg!(miri) { 4 } else { 10 };
        let n: usize = if cfg!(miri) { 4_000 } else { 20_000 };
        for i in 0..iters {
            let data: Vec<u8> = (0..n).map(|j| (j as u8).wrapping_add(i)).collect();
            store
                .write(b"main", &p(&["churn"]), 0, &data, i as u64)
                .unwrap();
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
        let expect: Vec<u8> = (0..n).map(|j| (j as u8).wrapping_add(iters - 1)).collect();
        assert_eq!(
            store.read(b"main", &p(&["churn"])).unwrap().unwrap(),
            expect
        );
        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(
            store2.read(b"main", &p(&["churn"])).unwrap().unwrap(),
            expect
        );
    }

    #[test]
    fn snapshots_pin_data_until_deleted() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let old: Vec<u8> = (0..30_000).map(|j| (j % 251) as u8).collect();
        store.write(b"main", &p(&["data"]), 0, &old, 1).unwrap();
        let snap = store
            .snapshot(b"main", b"t", b"v1", disk::CLASS_AUTO, 10)
            .unwrap();
        let new: Vec<u8> = (0..30_000).map(|j| (j % 13) as u8).collect();
        store.write(b"main", &p(&["data"]), 0, &new, 2).unwrap();
        store.sync_ref(b"main").unwrap();

        // The snapshot pins the old root: GC must keep it readable.
        store.gc().unwrap();
        let root = store.snapshot_root(b"main", snap).unwrap();
        assert_eq!(
            store.read_at_root(&root, &p(&["data"])).unwrap().unwrap(),
            old
        );

        // Dropping the snapshot is a ref-table edit; the next GC reclaims
        // the now-unreachable mass (rev1§4.6 "history rewriting").
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
        store
            .write(b"main", &p(&["f"]), 0, &[7u8; 5000], 1)
            .unwrap();
        // Two snapshots of unchanged content share one root (rev1§4.7: same
        // root for different events is normal under canonical trees).
        let s1 = store
            .snapshot(b"main", b"t", b"a", disk::CLASS_AUTO, 10)
            .unwrap();
        let s2 = store
            .snapshot(b"main", b"t", b"b", disk::CLASS_AUTO, 11)
            .unwrap();
        store
            .write(b"main", &p(&["f"]), 0, &[9u8; 5000], 2)
            .unwrap();
        store.sync_ref(b"main").unwrap();

        store.delete_snapshot(b"main", s1).unwrap();
        store.gc().unwrap();
        // s2 still pins the shared root.
        let root = store.snapshot_root(b"main", s2).unwrap();
        assert_eq!(
            store.read_at_root(&root, &p(&["f"])).unwrap().unwrap(),
            [7u8; 5000]
        );

        store.delete_snapshot(b"main", s2).unwrap();
        let stats = store.gc().unwrap();
        assert!(stats.freed_objects > 0);
    }

    #[test]
    fn delete_snapshot_repoints_parents_and_respects_tag_pins() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"1", 1).unwrap();
        let s1 = store
            .snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 10)
            .unwrap();
        store.write(b"main", &p(&["f"]), 0, b"2", 2).unwrap();
        let s2 = store
            .snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 20)
            .unwrap();
        store.write(b"main", &p(&["f"]), 0, b"3", 3).unwrap();
        let s3 = store
            .snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 30)
            .unwrap();

        // Prune the middle: the child re-points to the grandparent (rev1§4.7).
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

        // Retention class is an editable row field (rev1§4.7).
        store
            .set_snapshot_class(b"main", s3, disk::CLASS_KEEP)
            .unwrap();
        assert_eq!(
            store.snapshots(b"main").find(|r| r.id == s3).unwrap().class,
            disk::CLASS_KEEP
        );
    }

    /// rev1§4.7 "Tags" (M-8): `tag`/`untag`/`tags` round-trip; a tag is an
    /// entry-set mutation (advances the edit version) that pins the snapshot,
    /// persists across mount, and survives a metadata edit (it names the id);
    /// `untag` is ref-scoped and idempotent.
    #[test]
    fn tag_untag_list_pin_and_advance_edit_version() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let snap = store
            .snapshot(b"main", b"t", b"first", disk::CLASS_AUTO, 10)
            .unwrap();
        let v0 = store.edit_version(b"main").unwrap();

        // Create: pins the snapshot, advances the edit version, lists.
        store.tag(b"release", b"main", snap).unwrap();
        assert_eq!(store.edit_version(b"main"), Some(v0 + 1), "tag advances");
        assert_eq!(
            store.tags().collect::<Vec<_>>(),
            vec![(b"release".as_slice(), b"main".as_slice(), snap)]
        );
        assert!(matches!(
            store.delete_snapshot(b"main", snap),
            Err(StoreError::Pinned)
        ));

        // A metadata edit leaves the tag in place (it names the id, not a hash).
        store
            .set_snapshot_class(b"main", snap, disk::CLASS_KEEP)
            .unwrap();
        assert_eq!(
            store.tags().collect::<Vec<_>>(),
            vec![(b"release".as_slice(), b"main".as_slice(), snap)]
        );

        // Tagging a missing snapshot fails without touching the table.
        assert!(matches!(
            store.tag(b"bad", b"main", snap + 999),
            Err(StoreError::NoSuchSnapshot)
        ));

        // Persisted: the tag and the bumped version rode the commit onto disk.
        let mut store = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(
            store.tags().collect::<Vec<_>>(),
            vec![(b"release".as_slice(), b"main".as_slice(), snap)]
        );
        let v_tagged = store.edit_version(b"main").unwrap();

        // Untag of a name that does not pin this ref is a no-op: no commit, so
        // the edit version is unchanged.
        store.untag(b"main", b"nope").unwrap();
        assert_eq!(store.edit_version(b"main"), Some(v_tagged), "no-op untag");

        // Real untag: unpins, advances the version, and the snapshot now deletes.
        store.untag(b"main", b"release").unwrap();
        assert_eq!(
            store.edit_version(b"main"),
            Some(v_tagged + 1),
            "untag advances"
        );
        assert_eq!(store.tags().count(), 0);
        store.delete_snapshot(b"main", snap).unwrap();
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.7 invariant under random tag/untag/delete interleavings: a
        /// pinned snapshot is never deleted, and a tag never strands on a
        /// snapshot that is gone — the model mirrors which ids are tagged and
        /// which still exist, and the store must agree at every step.
        #[test]
        fn tag_pins_hold_under_random_interleavings(
            ops in proptest::collection::vec((0u8..3, 0u8..4, 0u8..3), 1..40),
        ) {
            let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
            store.create_ref(b"main").unwrap();
            // Four snapshots to tag/delete against.
            let mut live: Vec<u64> = Vec::new();
            for i in 0..4u64 {
                store.write(b"main", &p(&["f"]), 0, &[i as u8], i + 1).unwrap();
                live.push(
                    store
                        .snapshot(b"main", b"t", b"", disk::CLASS_AUTO, 10 + i)
                        .unwrap(),
                );
            }
            // model: tag name -> snapshot id it pins (only names "0".."2").
            let mut tags: std::collections::BTreeMap<u8, u64> =
                std::collections::BTreeMap::new();

            for (op, snap_sel, tag_sel) in ops {
                let id = 1 + snap_sel as u64; // snapshot ids are 1..=4
                let tag_name = [b'0' + tag_sel];
                match op {
                    // Create a tag (if the snapshot still exists).
                    0 => {
                        match store.tag(&tag_name, b"main", id) {
                            Ok(()) => {
                                prop_assert!(live.contains(&id));
                                tags.insert(tag_sel, id);
                            }
                            Err(StoreError::NoSuchSnapshot) => {
                                prop_assert!(!live.contains(&id));
                            }
                            Err(e) => prop_assert!(false, "unexpected tag error: {e:?}"),
                        }
                    }
                    // Remove a tag (ref-scoped, idempotent).
                    1 => {
                        store.untag(b"main", &tag_name).unwrap();
                        tags.remove(&tag_sel);
                    }
                    // Delete a snapshot: must fail iff some tag still pins it.
                    _ => {
                        let pinned = tags.values().any(|t| *t == id);
                        match store.delete_snapshot(b"main", id) {
                            Ok(()) => {
                                prop_assert!(!pinned, "deleted a pinned snapshot");
                                prop_assert!(live.contains(&id));
                                live.retain(|x| *x != id);
                            }
                            Err(StoreError::Pinned) => prop_assert!(pinned),
                            Err(StoreError::NoSuchSnapshot) => {
                                prop_assert!(!live.contains(&id))
                            }
                            Err(e) => prop_assert!(false, "unexpected delete error: {e:?}"),
                        }
                    }
                }
                // Invariant: every tag points at a still-live snapshot.
                let pinned_ids: Vec<u64> = store.tags().map(|(_, _, id)| id).collect();
                for pinned_id in pinned_ids {
                    prop_assert!(
                        live.contains(&pinned_id),
                        "tag stranded on a deleted snapshot"
                    );
                }
            }
        }
    }

    #[test]
    fn crash_mid_gc_loses_no_data() {
        // Base state: a snapshot pinning old content, current head content,
        // and a deleted file whose chunks are reclaimable garbage.
        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &p(&["keepme"]), 0, b"pinned by snap", 1)
            .unwrap();
        let snap = store
            .snapshot(b"main", b"t", b"", disk::CLASS_KEEP, 10)
            .unwrap();
        store
            .write(b"main", &p(&["keepme"]), 0, b"current state!", 2)
            .unwrap();
        store
            .write(b"main", &p(&["junk"]), 0, &[0xAB; 3000], 3)
            .unwrap();
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
        /// Selectors 12–17 mix in maintenance ops (sync, snapshot,
        /// snapshot deletion, GC, and rev1§4.7 guarded ref-table batches) so
        /// the crash point can land anywhere in the commit too — none of them
        /// may change logical (file) state, and a batch is all-or-nothing.
        #[test]
        fn crash_recovery_preserves_acked_state(
            ops in proptest::collection::vec(
                (0u8..18, 0u64..400, proptest::collection::vec(any::<u8>(), 1..96), any::<bool>()),
                1..50,
            ),
            fail_at in 4u64..600,
            crash_seed in any::<u64>(),
        ) {
            // B12A: tight op-count bound so the per-ref soft-bound auto-flush
            // (flush_ref + commit) fires mid-stream; the crash point can then
            // land inside a selective flush, re-witnessing all-acked-survives.
            let mut store = Store::format(CrashDev::new(1 << 20), crash_opts()).unwrap();
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
                        15 => store.gc().map(|_| ()),
                        // 16/17: a rev1§4.7 guarded batch at the ref's current
                        // version (so it never version-mismatches here). Edits
                        // are row/tag surgery — content-neutral like the others
                        // — and ride the one commit all-or-nothing across the
                        // crash point. 16 edits metadata; 17 deletes a row.
                        _ => {
                            let oldest = store.snapshots(b"main").next().map(|r| r.id);
                            match oldest {
                                Some(id) => {
                                    let v = store.edit_version(b"main").unwrap_or(0);
                                    let edits = if *sel == 16 {
                                        vec![
                                            RefEdit::SetClass {
                                                id,
                                                class: disk::CLASS_KEEP,
                                            },
                                            RefEdit::SetMessage {
                                                id,
                                                message: b"batched".to_vec(),
                                            },
                                        ]
                                    } else {
                                        vec![RefEdit::DeleteSnapshot { id }]
                                    };
                                    store.apply_batch(b"main", v, &edits).map(|_| ())
                                }
                                None => Ok(()),
                            }
                        }
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
            let recovered = Store::mount(dev, crash_opts()).unwrap();

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

    /// Regression for the WAL stale-record graft (rev1§4.5): the record
    /// checksum once covered only the payload, not the sequence number. The WAL
    /// is a reused linear region whose stale bytes are rejected on replay by the
    /// seq check, but a torn write at a reused offset could overwrite *just* the
    /// seq field of a stale, already-superseded record — stamping a fresh seq
    /// onto an old body that still matched the old (payload-only) checksum — so
    /// replay resurrected the superseded write.
    ///
    /// This is the deterministic minimization of a
    /// `crash_recovery_preserves_acked_state` failure (the proptest seed is also
    /// checked in under `proptest-regressions/`). The op sequence writes then
    /// snapshots `d2/f3`, unlinks it, syncs, then GCs — leaving the old "write
    /// d2/f3" record's bytes lingering at WAL offset 0. The crash then tears the
    /// next op's record (an unlink) at that same offset such that its fresh seq
    /// lands but the body reverts to the stale "write d2/f3". Pre-fix the grafted
    /// record passed the checksum and `d2/f3` was resurrected; with the seq bound
    /// into the checksum it fails integrity and replay correctly stops.
    #[test]
    fn crash_does_not_graft_seq_onto_stale_wal_record() {
        // (selector, offset, data, is_unlink); selectors ≥12 are maintenance.
        let ops: &[(u8, u64, &[u8], bool)] = &[
            (12, 0, &[0], false),
            (0, 0, &[0], false),
            (4, 0, &[0], false),
            (7, 0, &[0], false),
            (13, 0, &[0], false),
            (0, 0, &[0], false),
            (0, 0, &[0], false),
            (3, 0, &[0], true),
            (12, 0, &[0], false),
            (4, 0, &[0], false),
            (4, 0, &[0], false),
            (0, 0, &[0], false),
            (0, 0, &[0], false),
            (13, 0, &[0], false),
            (7, 0, &[0], false),
            (9, 0, &[0], false),
            (6, 0, &[0], true),
            (11, 0, &[0], true),
            (4, 0, &[0], true),
            (12, 0, &[0], false),
            (15, 0, &[0], false),
            (8, 0, &[0], true),
        ];
        let fail_at = 70u64;
        let crash_seed = 14532340274190305123u64;
        let dirs = ["d1", "d2"];

        let mut store = Store::format(CrashDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.dev_mut().set_fail_after(fail_at);

        let mut model: std::collections::HashMap<Path, Option<Vec<u8>>> =
            std::collections::HashMap::new();
        let mut inflight: Option<(Path, Option<Vec<u8>>)> = None;
        for (sel, off, data, is_unlink) in ops {
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
                if store.unlink(b"main", &path, 1).is_ok() {
                    model.insert(path, None);
                } else {
                    inflight = Some((path, None));
                    break;
                }
            } else {
                let mut content = model.get(&path).cloned().flatten().unwrap_or_default();
                let end = *off as usize + data.len();
                if content.len() < end {
                    content.resize(end, 0);
                }
                content[*off as usize..end].copy_from_slice(data);
                if store.write(b"main", &path, *off, data, 1).is_ok() {
                    model.insert(path, Some(content));
                } else {
                    inflight = Some((path, None));
                    break;
                }
            }
        }

        let mut dev = store.into_dev();
        dev.clear_fail();
        dev.crash(crash_seed);
        let recovered = Store::mount(dev, test_opts()).unwrap();

        // The headline: d2/f3 was acked-deleted; it must not be resurrected.
        let d2f3 = p(&["d2", "f3"]);
        assert_eq!(model.get(&d2f3), Some(&None), "model expects d2/f3 deleted");
        assert_eq!(
            recovered.read(b"main", &d2f3).unwrap(),
            None,
            "acked unlink of d2/f3 must survive recovery (no stale-WAL graft)"
        );
        // And the full invariant holds for every acked path.
        for (path, expect) in &model {
            let got = recovered.read(b"main", path).unwrap();
            let ok = got == *expect
                || inflight
                    .as_ref()
                    .is_some_and(|(ip, iv)| ip == path && got == *iv);
            assert!(
                ok,
                "path {path:?}: got {got:?}, want {expect:?} (inflight {inflight:?})"
            );
        }
    }

    /// rev1§2.6: pre-v3 images are re-created with mkfs, and that stance is
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
        store
            .snapshot(b"main", b"t", b"image", disk::CLASS_KEEP, 100)
            .unwrap();
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
        assert!(
            matches!(err, StoreError::UnsupportedVersion(2)),
            "got {err:?}"
        );
    }

    /// B5A bumped the format v3 → v4 (each `RefEntry` now carries a fixed-width
    /// `edit_version`, rev1§4.7). A v3 ref record is one u64 shorter, so a v3
    /// reader handed a v4 record — or vice versa — would desync; the version
    /// gate must refuse the superseded format cleanly, never mis-decode.
    #[test]
    fn format_v3_image_is_refused_with_a_version_error() {
        use crate::disk::{SB_A_OFF, SB_B_OFF};

        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"data", 1).unwrap();
        store
            .snapshot(b"main", b"t", b"image", disk::CLASS_KEEP, 100)
            .unwrap();
        let dev = store.into_dev();
        let mut img = vec![0u8; dev.len() as usize];
        dev.read(0, &mut img).unwrap();
        // Re-stamp both slots as the immediately-preceding format v3, intact
        // and plausibly checksummed — the dangerous artifact, not a torn slot.
        for off in [SB_A_OFF as usize, SB_B_OFF as usize] {
            img[off + 8..off + 12].copy_from_slice(&3u32.to_le_bytes());
            let sum = Hash::of(&img[off..off + disk::SB_BODY]);
            img[off + disk::SB_BODY..off + disk::SB_BODY + 32].copy_from_slice(sum.as_bytes());
        }
        let err = Store::mount(MemDev::from_bytes(img), test_opts())
            .err()
            .expect("a v3 image must not mount");
        assert!(
            matches!(err, StoreError::UnsupportedVersion(3)),
            "got {err:?}"
        );
    }

    /// rev1§4.7: the edit version advances once per committed entry-set
    /// mutation (snapshot row, head move, row surgery) and persists through the
    /// commit — the value a later guarded batch's `expected_version` compares
    /// against (B5B).
    #[test]
    fn edit_version_advances_per_committed_mutation_and_persists() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        // A fresh ref starts at 0 (create_ref does not mark it dirty).
        assert_eq!(store.edit_version(b"main"), Some(0));

        let snap = store
            .snapshot(b"main", b"t", b"first", disk::CLASS_KEEP, 100)
            .unwrap();
        assert_eq!(store.edit_version(b"main"), Some(1), "snapshot row");

        store.rollback(b"main", snap).unwrap();
        assert_eq!(store.edit_version(b"main"), Some(2), "head move");

        store.delete_snapshot(b"main", snap).unwrap();
        assert_eq!(store.edit_version(b"main"), Some(3), "row surgery");

        store.write(b"main", &p(&["f"]), 0, b"data", 1).unwrap();
        store.sync_ref(b"main").unwrap();
        assert_eq!(
            store.edit_version(b"main"),
            Some(4),
            "write+flush head move"
        );

        // Persisted: the bumped value rode the commit onto disk.
        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(store2.edit_version(b"main"), Some(4));
    }

    /// rev1§4.7/§2.2: the edit version and the revocation generation are
    /// orthogonal counters in the same `RefEntry`. A revoke advances generation
    /// and leaves the edit version untouched; an entry-set mutation advances the
    /// edit version and leaves generation untouched. (Load-bearing: a retention
    /// batch must not be rejected because an unrelated handle was revoked.)
    #[test]
    fn edit_version_orthogonal_to_revocation_generation() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        let gen = |s: &Store<MemDev>| {
            s.refs()
                .find(|(n, _)| n.as_slice() == b"main")
                .unwrap()
                .1
                .generation
        };
        assert_eq!((store.edit_version(b"main"), gen(&store)), (Some(0), 0));

        store.bump_generation(b"main").unwrap();
        assert_eq!(
            (store.edit_version(b"main"), gen(&store)),
            (Some(0), 1),
            "revoke moves generation only"
        );

        store
            .snapshot(b"main", b"t", b"x", disk::CLASS_KEEP, 10)
            .unwrap();
        assert_eq!(
            (store.edit_version(b"main"), gen(&store)),
            (Some(1), 1),
            "snapshot moves edit version only"
        );
    }

    /// A commit with nothing dirty (a pure flush that moved no head) does not
    /// advance the edit version — a flush that does not mutate the entry-set is
    /// not a §4.7 mutation, so an outstanding `expected_version` stays valid.
    #[test]
    fn no_op_sync_does_not_advance_edit_version() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.sync_ref(b"main").unwrap(); // nothing dirty
        assert_eq!(store.edit_version(b"main"), Some(0));

        store.write(b"main", &p(&["f"]), 0, b"data", 1).unwrap();
        store.sync_ref(b"main").unwrap(); // real head move
        assert_eq!(store.edit_version(b"main"), Some(1));

        store.sync_all().unwrap(); // nothing dirty again
        store.sync_ref(b"main").unwrap();
        assert_eq!(store.edit_version(b"main"), Some(1));
    }

    /// Design decision 2 / rev1§4.7: the bump is once per commit per dirtied
    /// ref, not once per edit — so the guarded batch (B5B), which stages several
    /// edits on one ref in one commit, ticks the version exactly once. The
    /// dirty-set collapses repeated touches before the single commit.
    #[test]
    fn one_commit_bumps_a_dirtied_ref_exactly_once() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        // Stage several entry-set edits' worth of dirtying, then commit once.
        store.touch_ref(b"main");
        store.touch_ref(b"main");
        store.touch_ref(b"main");
        store.commit().unwrap();
        assert_eq!(store.edit_version(b"main"), Some(1));
    }

    /// rev1§4.7 / I-2: the guarded batch is conditional on the ref's edit
    /// version. A stale `expected_version` is refused carrying the current
    /// version, with no mutation and no commit; re-reading and retrying at the
    /// current version applies the batch and ticks the version once.
    #[test]
    fn apply_batch_version_mismatch_rejects_without_mutation() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"1", 1).unwrap();
        let s1 = store
            .snapshot(b"main", b"t", b"a", disk::CLASS_AUTO, 10)
            .unwrap();
        let v = store.edit_version(b"main").unwrap();

        let edits = [RefEdit::SetMessage {
            id: s1,
            message: b"renamed".to_vec(),
        }];
        // Stale version → VersionMismatch carrying the current version.
        assert!(matches!(
            store.apply_batch(b"main", v - 1, &edits),
            Err(StoreError::VersionMismatch { current }) if current == v
        ));
        // No mutation, no commit: message and version are unchanged.
        assert_eq!(
            store
                .snapshots(b"main")
                .find(|r| r.id == s1)
                .unwrap()
                .message,
            b"a"
        );
        assert_eq!(store.edit_version(b"main"), Some(v));

        // Matching version applies the batch and ticks once.
        let new_v = store.apply_batch(b"main", v, &edits).unwrap();
        assert_eq!(new_v, v + 1);
        assert_eq!(store.edit_version(b"main"), Some(v + 1));
        assert_eq!(
            store
                .snapshots(b"main")
                .find(|r| r.id == s1)
                .unwrap()
                .message,
            b"renamed"
        );
    }

    /// rev1§4.7: a guarded batch is all-or-nothing. An invalid edit anywhere in
    /// the batch (deleting a pinned snapshot, or a nonexistent id) rejects the
    /// whole batch with that edit's error — none of the earlier valid edits
    /// land, and the version does not advance (no commit).
    #[test]
    fn apply_batch_all_or_nothing_on_invalid_edit() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"1", 1).unwrap();
        let s1 = store
            .snapshot(b"main", b"t", b"a", disk::CLASS_AUTO, 10)
            .unwrap();
        store.write(b"main", &p(&["f"]), 0, b"2", 2).unwrap();
        let s2 = store
            .snapshot(b"main", b"t", b"b", disk::CLASS_AUTO, 20)
            .unwrap();
        store.tag(b"rel", b"main", s1).unwrap(); // pin s1
        let v = store.edit_version(b"main").unwrap();
        let class =
            |s: &Store<MemDev>, id: u64| s.snapshots(b"main").find(|r| r.id == id).unwrap().class;

        // Two valid edits then a pinned delete → Pinned, nothing applied.
        let edits = [
            RefEdit::SetClass {
                id: s1,
                class: disk::CLASS_KEEP,
            },
            RefEdit::SetMessage {
                id: s2,
                message: b"keep".to_vec(),
            },
            RefEdit::DeleteSnapshot { id: s1 },
        ];
        assert!(matches!(
            store.apply_batch(b"main", v, &edits),
            Err(StoreError::Pinned)
        ));
        assert_eq!(class(&store, s1), disk::CLASS_AUTO);
        assert_eq!(
            store
                .snapshots(b"main")
                .find(|r| r.id == s2)
                .unwrap()
                .message,
            b"b"
        );
        assert_eq!(store.edit_version(b"main"), Some(v));
        assert!(store.snapshots(b"main").any(|r| r.id == s1));

        // A nonexistent id → NoSuchSnapshot, also all-or-nothing.
        let edits = [
            RefEdit::SetClass {
                id: s2,
                class: disk::CLASS_KEEP,
            },
            RefEdit::DeleteSnapshot { id: 999 },
        ];
        assert!(matches!(
            store.apply_batch(b"main", v, &edits),
            Err(StoreError::NoSuchSnapshot)
        ));
        assert_eq!(class(&store, s2), disk::CLASS_AUTO);
        assert_eq!(store.edit_version(b"main"), Some(v));
    }

    /// rev1§4.7: a multi-edit batch commits once (one version bump) and every
    /// edit lands — including a tag create that then pins its snapshot — and
    /// the result survives a remount.
    #[test]
    fn apply_batch_bumps_once_applies_all_and_persists() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"1", 1).unwrap();
        let s1 = store
            .snapshot(b"main", b"t", b"a", disk::CLASS_AUTO, 10)
            .unwrap();
        store.write(b"main", &p(&["f"]), 0, b"2", 2).unwrap();
        let s2 = store
            .snapshot(b"main", b"t", b"b", disk::CLASS_AUTO, 20)
            .unwrap();
        let v = store.edit_version(b"main").unwrap();

        let edits = [
            RefEdit::SetClass {
                id: s1,
                class: disk::CLASS_KEEP,
            },
            RefEdit::SetParent {
                id: s2,
                parent: None,
            },
            RefEdit::CreateTag {
                name: b"rel".to_vec(),
                snap_id: s2,
            },
        ];
        let new_v = store.apply_batch(b"main", v, &edits).unwrap();
        assert_eq!(new_v, v + 1, "one bump for the whole batch");
        assert_eq!(
            store.snapshots(b"main").find(|r| r.id == s1).unwrap().class,
            disk::CLASS_KEEP
        );
        assert_eq!(
            store
                .snapshots(b"main")
                .find(|r| r.id == s2)
                .unwrap()
                .parent,
            None
        );
        // The batch's CreateTag now pins s2.
        assert!(matches!(
            store.delete_snapshot(b"main", s2),
            Err(StoreError::Pinned)
        ));

        // Persisted across remount.
        let store2 = Store::mount(store.into_dev(), test_opts()).unwrap();
        assert_eq!(store2.edit_version(b"main"), Some(v + 1));
        assert_eq!(
            store2
                .snapshots(b"main")
                .find(|r| r.id == s1)
                .unwrap()
                .class,
            disk::CLASS_KEEP
        );
    }

    /// rev1§4.7: `ts = max(now, predecessor_ts + 1)` — an RTC that regressed
    /// between boots can never disorder a ref's snapshot log.
    #[test]
    fn snapshot_timestamps_are_strictly_monotone_per_ref() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store.write(b"main", &p(&["f"]), 0, b"x", 1).unwrap();
        let a = store
            .snapshot(b"main", b"t", b"first", disk::CLASS_KEEP, 1000)
            .unwrap();
        // The clock went backwards; order must survive anyway.
        let b = store
            .snapshot(b"main", b"t", b"second", disk::CLASS_KEEP, 500)
            .unwrap();
        // Same instant as the first: still strictly after its parent.
        let c = store
            .snapshot(b"main", b"t", b"third", disk::CLASS_KEEP, 1000)
            .unwrap();
        let rows: Vec<(u64, u64)> = store
            .snapshots(b"main")
            .map(|r| (r.id, r.timestamp))
            .collect();
        assert_eq!(rows, vec![(a, 1000), (b, 1001), (c, 1002)]);
    }

    // ── B12D: staleness-timer trigger (rev1§4.4 trigger 4, M-6 timer half) ──
    //
    // Time is the caller-injected `now`/`mtime` (UTC-nanos, the same source op
    // mtimes carry) — there is no internal store clock — so these drive it with
    // plain integers; no wall-clock sleeps, deterministic and Miri-safe.

    /// Options that isolate the staleness trigger: every byte/op/WAL bound
    /// disabled (huge budgets, watermark == wal_len so the ring never proactively
    /// flushes), only `staleness_ns` finite — so the only flush the tests observe
    /// is the staleness sweep.
    fn stale_opts(staleness_ns: u64) -> StoreOptions {
        StoreOptions {
            staleness_ns,
            wal_len: 64 * 1024,
            wal_watermark: 64 * 1024,
            global_budget: 1 << 30,
            size_low_watermark: 1 << 30,
            per_ref_budget: 1 << 30,
            op_count_bound: u64::MAX,
            ..test_opts()
        }
    }

    /// rev1§4.4 trigger 4 (M-6 timer half): the explicit staleness sweep
    /// (`flush_stale`, the reactor-idle / request-boundary entry point) flushes a
    /// ref whose oldest-dirty age exceeds `staleness_ns` to committed tree, while
    /// a ref dirtied within the bound stays dirty — selective, not flush-everything.
    #[test]
    fn staleness_flushes_quietly_dirty_ref_keeps_fresh_ref_dirty() {
        let staleness = 1000u64;
        let mut store = Store::format(MemDev::new(1 << 20), stale_opts(staleness)).unwrap();
        store.create_ref(b"old").unwrap();
        store.create_ref(b"fresh").unwrap();

        // 'old' first dirtied at t=100.
        store.write(b"old", &p(&["o"]), 0, &[1u8; 64], 100).unwrap();
        // 'fresh' first dirtied at t=1050. The in-write staleness scan runs with
        // now=1050: 'old' age = 950 < 1000, not yet stale → both stay dirty.
        store
            .write(b"fresh", &p(&["f"]), 0, &[2u8; 64], 1050)
            .unwrap();
        assert!(
            !store.overlays.get(b"old".as_slice()).unwrap().is_empty(),
            "'old' should still be dirty before it goes stale"
        );
        assert!(
            !store.overlays.get(b"fresh".as_slice()).unwrap().is_empty(),
            "'fresh' should be dirty"
        );

        // Simulated reactor-idle sweep at t=1200: 'old' age = 1100 > 1000 → flush;
        // 'fresh' age = 150 < 1000 → stays dirty.
        store.flush_stale(1200).unwrap();
        assert!(
            store
                .overlays
                .get(b"old".as_slice())
                .map_or(true, |o| o.is_empty()),
            "stale 'old' should have been flushed to committed tree"
        );
        assert!(
            store.acct.get(b"old".as_slice()).is_none(),
            "flushed ref's accounting should be cleared"
        );
        assert!(
            store
                .overlays
                .get(b"fresh".as_slice())
                .map_or(false, |o| !o.is_empty()),
            "'fresh' (within the staleness bound) should stay dirty"
        );
        // The flushed data is durable committed tree (read-backable).
        assert_eq!(store.read(b"old", &p(&["o"])).unwrap(), Some(vec![1u8; 64]));
    }

    /// The staleness trigger also fires opportunistically on the write path
    /// (rev1§4.4: a check at the request entry, lowest priority): a write whose
    /// timestamp leaves an *older* quiet ref past the bound flushes that quiet
    /// ref, while the just-written ref stays dirty.
    #[test]
    fn staleness_fires_on_next_write_request() {
        let staleness = 1000u64;
        let mut store = Store::format(MemDev::new(1 << 20), stale_opts(staleness)).unwrap();
        store.create_ref(b"quiet").unwrap();
        store.create_ref(b"active").unwrap();

        // 'quiet' first dirtied at t=100.
        store
            .write(b"quiet", &p(&["q"]), 0, &[1u8; 64], 100)
            .unwrap();
        // A later write to 'active' at t=1200 makes 'quiet' (age 1100 > 1000)
        // stale; the in-write staleness scan flushes it. 'active' was just
        // dirtied (age 0), so it stays.
        store
            .write(b"active", &p(&["a"]), 0, &[2u8; 64], 1200)
            .unwrap();
        assert!(
            store
                .overlays
                .get(b"quiet".as_slice())
                .map_or(true, |o| o.is_empty()),
            "stale 'quiet' should flush on the next write request"
        );
        assert!(
            store
                .overlays
                .get(b"active".as_slice())
                .map_or(false, |o| !o.is_empty()),
            "just-written 'active' should stay dirty"
        );
        assert_eq!(
            store.read(b"quiet", &p(&["q"])).unwrap(),
            Some(vec![1u8; 64])
        );
    }

    /// With the default-stubbed `staleness_ns == u64::MAX` (what `test_opts` and
    /// every shipped/B12A-C fixture inherit until B12F), the staleness sweep never
    /// fires — a dirty ref stays dirty no matter how far the clock advances. Guards
    /// that B12D installs the mechanism without changing default behavior.
    #[test]
    fn staleness_disabled_by_default_never_flushes() {
        let mut store = Store::format(MemDev::new(1 << 20), test_opts()).unwrap();
        store.create_ref(b"r").unwrap();
        store.write(b"r", &p(&["x"]), 0, &[1u8; 64], 1).unwrap();
        // The clock jumps to the far end; with the bound disabled nothing flushes.
        store.flush_stale(u64::MAX).unwrap();
        assert!(
            !store.overlays.get(b"r".as_slice()).unwrap().is_empty(),
            "no staleness flush when the bound is the disabled stub"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// After a staleness sweep at any clock value, no ref left dirty is overdue
        /// (the sweep is total over the dirty set) and refs within the bound stay
        /// dirty (selective). Random multi-ref streams under a monotone-increasing
        /// clock, then `flush_stale(now)`.
        #[test]
        fn staleness_sweep_leaves_no_overdue_ref(
            ops in proptest::collection::vec((0usize..4, 1u64..600), 1..40),
            sweep_at in 0u64..2000,
        ) {
            let staleness = 1000u64;
            let mut store =
                Store::format(MemDev::new(16 << 20), stale_opts(staleness)).unwrap();
            for r in 0..4u8 {
                store.create_ref(&[b'a' + r]).unwrap();
            }
            let mut clock = 0u64;
            let max_ops = if cfg!(miri) { 16 } else { ops.len() };
            for (ref_idx, dt) in ops.into_iter().take(max_ops) {
                clock += dt;
                let name = [b'a' + ref_idx as u8];
                // Heavy flush-without-GC can fill the chunk region (the limit is
                // chunk capacity, not staleness — B12C finding 5); a NoSpace ends
                // the run early and the invariant still holds on the state reached.
                if store.write(&name, &p(&["f"]), 0, &[1u8; 32], clock).is_err() {
                    break;
                }
            }
            // Sweep at a clock value at or after the last write (the clock only
            // moves forward). If the sweep itself NoSpaces, skip the assertion —
            // an incomplete flush can leave an overdue ref legitimately.
            let now = clock.saturating_add(sweep_at);
            if store.flush_stale(now).is_ok() {
                for (name, a) in &store.acct {
                    if let Some(t) = a.oldest_dirty_ns {
                        prop_assert!(
                            now.saturating_sub(t) <= staleness,
                            "ref {:?} left dirty while overdue (age {})",
                            name,
                            now.saturating_sub(t)
                        );
                    }
                }
            }
        }
    }
}
