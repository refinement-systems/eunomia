//! The storage engine (spec §4.3–4.7): memtable + WAL + flush + the A/B
//! superblock commit, with crash recovery. This is the code the
//! CommitProtocol TLA+ model models; the crash-injection proptest at the
//! bottom checks the model's headline invariant against the real bytes:
//! after any crash, every acknowledged write is recoverable from durable
//! state alone.
//!
//! Commit is always: fsync chunks (barrier 1) → write new superblock to
//! the older slot → fsync (barrier 2). Nothing is freed on the write
//! path, ever; reclamation is GC's job exclusively (M5).
//!
//! MVP simplifications, recorded:
//!   - Flush rebuilds whole dirty files instead of re-chunking only the
//!     affected neighborhood (§4.3 step 3) — a perf optimization with no
//!     semantic difference; owed when file sizes warrant it.
//!   - The WAL is linear, not circular: when full, everything is flushed
//!     and committed and the log resets to offset 0 (head can only ever
//!     advance past flushed records, so the §4.4 invariant holds; the
//!     flush-the-pinner scheduler arrives with real multi-ref traffic).
//!   - Oversized writes (record > WAL region) bypass the WAL and commit
//!     synchronously before acknowledging — same durability contract.
//!   - The chunk index is rebuilt at mount by scanning the framed chunk
//!     region (§4.2 allows index format changes as migrations; a durable
//!     index is one).

use crate::chunk::ChunkerParams;
use crate::dev::BlockDev;
use crate::disk::{
    self, RefEntry, RefTable, SnapRow, Superblock, WalOp, SB_A_OFF, SB_B_OFF, SB_SIZE, WAL_OFF,
};
use crate::file::{make_file_entry, read_file};
use crate::hash::Hash;
use crate::overlay::{FileState, Overlay, Path};
use crate::prolly::{Content, Dir, Entry, EntryKind, FormatError, NodeStore};
use crate::tree;
use std::collections::{HashMap, VecDeque};
use std::io;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Format(FormatError),
    NoSuperblock,
    NoSuchRef,
    NoSuchSnapshot,
    NotAFile,
    Corrupt(&'static str),
    NoSpace,
}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
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
            StoreError::NoSuchRef => write!(f, "no such ref"),
            StoreError::NoSuchSnapshot => write!(f, "no such snapshot"),
            StoreError::NotAFile => write!(f, "not a file"),
            StoreError::Corrupt(w) => write!(f, "corrupt store: {w}"),
            StoreError::NoSpace => write!(f, "chunk region full"),
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    pub wal_len: u64,
    pub chunker: ChunkerParams,
    /// Global dirty-overlay budget (§4.4); exceeding it forces sync.
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
    tail: u64,
    birth_gen: u64,
    index: HashMap<Hash, (u64, u32)>, // hash → (data offset in region, len)
    io_error: Option<io::Error>,
}

impl<D: BlockDev> ChunkStore<D> {
    fn region_len(&self) -> u64 {
        self.dev.len() - self.chunk_off
    }

    fn read_object(&self, hash: &Hash) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(&(off, len)) = self.index.get(hash) else {
            return Ok(None);
        };
        let mut buf = vec![0u8; len as usize];
        self.dev.read(self.chunk_off + off, &mut buf)?;
        // Every layer self-verifies (§4.8).
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
            return hash; // dedup (§4.3); GC resurrection check arrives in M5
        }
        let frame = disk::encode_chunk_frame(bytes, self.birth_gen, &hash);
        if self.tail + frame.len() as u64 > self.region_len() {
            self.io_error = Some(io::Error::other("chunk region full"));
            return hash;
        }
        if let Err(e) = self.dev.write(self.chunk_off + self.tail, &frame) {
            self.io_error = Some(e);
            return hash;
        }
        self.index
            .insert(hash, (self.tail + disk::CHUNK_HEADER as u64, bytes.len() as u32));
        self.tail += frame.len() as u64;
        hash
    }

    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.read_object(hash).ok().flatten()
    }
}

// ── The engine ──────────────────────────────────────────────────────────

struct RecMeta {
    seq: u64,
    off: u64,
    ref_name: Vec<u8>,
    flushed: bool,
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
    overlays: HashMap<Vec<u8>, Overlay>,
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
            index: HashMap::new(),
            io_error: None,
        };
        let table = RefTable::default();
        let rt_hash = chunks.put(&table.encode());
        chunks.io_error.take().map_or(Ok(()), Err)?;
        chunks.dev.flush()?; // barrier 1

        let sb = Superblock {
            generation: 1,
            ref_table: rt_hash,
            wal_head: 0,
            wal_next_seq: 1,
            wal_len: opts.wal_len,
            chunk_tail: chunks.tail,
        };
        chunks.dev.write(SB_A_OFF, &sb.encode())?;
        chunks.dev.flush()?; // barrier 2

        Ok(Store {
            chunks,
            opts,
            sb,
            sb_in_b: false,
            table,
            overlays: HashMap::new(),
            wal_tail: 0,
            wal_seq: 1,
            wal_records: VecDeque::new(),
        })
    }

    /// Mount = crash recovery (§4.5): both paths are the same code. Read
    /// both slots, discard invalid, take the higher generation; rebuild
    /// the chunk index by scan; replay the WAL tail into overlays.
    pub fn mount(dev: D, opts: StoreOptions) -> Result<Store<D>, StoreError> {
        let mut buf_a = vec![0u8; SB_SIZE];
        let mut buf_b = vec![0u8; SB_SIZE];
        dev.read(SB_A_OFF, &mut buf_a)?;
        dev.read(SB_B_OFF, &mut buf_b)?;
        let (sb, sb_in_b) = match (Superblock::decode(&buf_a), Superblock::decode(&buf_b)) {
            (Some(a), Some(b)) => {
                if a.generation >= b.generation {
                    (a, false)
                } else {
                    (b, true)
                }
            }
            (Some(a), None) => (a, false),
            (None, Some(b)) => (b, true),
            (None, None) => return Err(StoreError::NoSuperblock),
        };

        let chunk_off = WAL_OFF + sb.wal_len;
        let mut chunks = ChunkStore {
            dev,
            chunk_off,
            tail: sb.chunk_tail,
            birth_gen: sb.generation + 1,
            index: HashMap::new(),
            io_error: None,
        };
        // Index rebuild: the committed region is durable and framed; a
        // malformed frame inside it is real corruption, not a torn tail.
        let mut pos = 0u64;
        let mut header = [0u8; disk::CHUNK_HEADER];
        while pos < sb.chunk_tail {
            chunks.dev.read(chunk_off + pos, &mut header)?;
            let Some((len, _birth, hash)) = disk::decode_chunk_header(&header) else {
                return Err(StoreError::Corrupt("bad chunk frame in committed region"));
            };
            chunks
                .index
                .insert(hash, (pos + disk::CHUNK_HEADER as u64, len as u32));
            pos += (disk::CHUNK_HEADER + len) as u64;
        }
        if pos != sb.chunk_tail {
            return Err(StoreError::Corrupt("chunk region overruns committed tail"));
        }

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
            overlays: HashMap::new(),
            wal_tail: sb.wal_head,
            wal_seq: sb.wal_next_seq,
            wal_records: VecDeque::new(),
        };

        // WAL replay: contiguous, checksummed, seq-continuous records from
        // the committed head. Anything else is an unacked torn tail.
        let mut wal = vec![0u8; sb.wal_len as usize];
        store.chunks.dev.read(WAL_OFF, &mut wal)?;
        let mut off = sb.wal_head;
        let mut seq = sb.wal_next_seq;
        while let Some((rseq, op, rlen)) = WalOp::decode_record(&wal[off as usize..]) {
            if rseq != seq {
                break;
            }
            store.apply_to_overlay(&op);
            store.wal_records.push_back(RecMeta {
                seq,
                off,
                ref_name: op.ref_name().to_vec(),
                flushed: false,
            });
            off += rlen as u64;
            seq += 1;
        }
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

    pub fn snapshots(&self, ref_name: &[u8]) -> impl Iterator<Item = &SnapRow> {
        let key = ref_name.to_vec();
        self.table
            .snaps
            .range((key.clone(), 0)..(key, u64::MAX))
            .map(|(_, row)| row)
    }

    /// Snapshot the ref (forces a flush — a snapshot must name a tree
    /// hash, §4.4). `now` comes from the caller (server-assigned, §4.7);
    /// timestamps are clamped non-decreasing per ref (§2.6).
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
            timestamp: now.max(last.map(|(_, t)| t).unwrap_or(0)),
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
    /// ref-table edit (§4.6).
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

    // ── Write path (§4.3) ───────────────────────────────────────────

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
    /// implies durability (§4.3 step 2).
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

        // Size pressure (§4.4), collapsed to the simplest correct policy:
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

    // ── Read path (overlay first, tree below — §4.3) ────────────────

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

    /// Mass revocation of a ref's storage handles (§2.2): O(1) — bump the
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

    /// Merged directory listing: committed tree + dirty overlay (§4.4
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

    // ── Flush and commit (§4.3 steps 3–4) ───────────────────────────

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

    /// The single atomicity mechanism (§4.2): barrier 1, superblock to the
    /// older slot, barrier 2. The WAL head advances past the contiguous
    /// prefix of flushed records (§4.3 step 4).
    pub fn commit(&mut self) -> Result<(), StoreError> {
        let rt_hash = self.chunks.put(&self.table.encode());
        self.check_io()?;
        self.chunks.dev.flush()?; // barrier 1: no SB may reference non-durable chunks

        while let Some(rec) = self.wal_records.front() {
            if rec.flushed {
                self.wal_records.pop_front();
            } else {
                break;
            }
        }
        let (wal_head, wal_next_seq) = match self.wal_records.front() {
            Some(rec) => (rec.off, rec.seq),
            None => {
                // Empty log: reclaim the region. Stale bytes beyond the
                // head are rejected on replay by the seq check.
                self.wal_tail = 0;
                (0, self.wal_seq)
            }
        };

        let new_sb = Superblock {
            generation: self.sb.generation + 1,
            ref_table: rt_hash,
            wal_head,
            wal_next_seq,
            wal_len: self.opts.wal_len,
            chunk_tail: self.chunks.tail,
        };
        // Always alternate; never overwrite the current latest commit.
        let target = if self.sb_in_b { SB_A_OFF } else { SB_B_OFF };
        self.chunks.dev.write(target, &new_sb.encode())?;
        self.chunks.dev.flush()?; // barrier 2: only now is the commit real
        self.sb = new_sb;
        self.sb_in_b = !self.sb_in_b;
        self.chunks.birth_gen = self.sb.generation + 1;
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

        // Snapshot identity is the per-ref sequence number (§4.7).
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

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        /// The CommitProtocol headline invariant against real bytes: after
        /// a crash at an arbitrary point (power cut mid-operation, torn
        /// unflushed writes), every acknowledged mutation is recoverable.
        /// At most the single in-flight unacked mutation is ambiguous.
        #[test]
        fn crash_recovery_preserves_acked_state(
            ops in proptest::collection::vec(
                (0u8..12, 0u64..400, proptest::collection::vec(any::<u8>(), 1..96), any::<bool>()),
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
}
