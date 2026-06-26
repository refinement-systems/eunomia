//! Per-ref in-memory overlay — the memtable (spec rev2§4.3–4.4).
//!
//! Writes land here first, keyed by path with per-file interval maps;
//! reads consult the overlay and fall through to the immutable tree — an
//! LSM read path whose bottom level is the prolly tree. Bounds are
//! denominated in bytes of dirty overlay (rev2§4.4); the store enforces the
//! budget with backpressure-by-flush, never eviction.
//!
//! The overlay keys its per-file interval maps on an ephemeral, server-runtime
//! [`FileId`] (rev2§4.3/§4.9), not on the path: a name-ordered `by_name` index
//! resolves a path to its id, and an `id → name` map (`names`) is what a
//! [`rename`](Overlay::rename) swaps in O(1) regardless of how much dirty state
//! the file holds. IDs are runtime-only and never touch disk — they are
//! re-derived by replaying the path-keyed WAL on mount. A file rename moves the
//! name only (the interval map never moves); a directory rename is recorded in
//! `dir_renames` and applied at flush as a tree detach/reattach (rev2§4.9).
//!
//! An id can also be held *open* across an unlink (the rev2§4.9 open handle):
//! `open` refcounts the live handles per id; unlinking an *open* id orphans it
//! (`names[id] = None`) instead of reaping it, so the handle keeps working
//! against the overlay, and at flush an orphaned id (resolving to no name) is
//! discarded — "which is what unlink means here."

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

pub type Path = Vec<Vec<u8>>;

/// Ephemeral, server-runtime file identity (rev2§4.9): assigned per file when it
/// first enters the overlay, opaque, store-global, and **never persisted** — it
/// is re-derived by replaying the path-keyed WAL on mount. The interval maps key
/// on it so a rename is an O(1) name-pointer swap, not a re-key of dirty state.
pub type FileId = u64;

#[derive(Debug, Default, Clone)]
pub struct FileOverlay {
    /// Sorted, non-overlapping intervals: offset → bytes.
    writes: BTreeMap<u64, Vec<u8>>,
    /// Base content is ignored: the file was (re)created inside this
    /// overlay window (e.g. write after unlink).
    pub fresh: bool,
    pub mtime: u64,
    /// Committed-tree path to read pre-edit bytes from at flush (rev2§4.9 base
    /// origin), fixed at first write and distinct from the current name once a
    /// rename moves the name; `None` for a `fresh` file (no base to read). Read
    /// by `Store::read`/`flush_ref`: a renamed dirty file applies its
    /// interval map over the committed bytes at the *original* name.
    origin: Option<Path>,
}

impl FileOverlay {
    /// The committed-tree path to read pre-edit bytes from (rev2§4.9 base
    /// origin). `None` for a `fresh` file. Differs from the current name after a
    /// rename; the store reads the base here and writes at the current name.
    pub fn origin(&self) -> Option<&Path> {
        self.origin.as_ref()
    }

    /// Largest offset written (for synthesizing sizes in listings).
    pub fn extent(&self) -> u64 {
        self.writes
            .iter()
            .next_back()
            .map(|(off, data)| off + data.len() as u64)
            .unwrap_or(0)
    }

    /// Offset of the first byte this overlay changed (`None` only if nothing
    /// was written). rev2§4.3 neighborhood re-chunk bounds the re-chunked
    /// region from here.
    pub fn first_write_offset(&self) -> Option<u64> {
        self.writes.keys().next().copied()
    }

    /// Apply the intervals over base content (zero-fill gaps). Borrows `base`
    /// so the caller can keep the pre-edit bytes alive alongside the result
    /// (the rev2§4.3 neighborhood re-chunk diffs new against old).
    pub fn apply(&self, base: &[u8]) -> Vec<u8> {
        let mut content = if self.fresh {
            Vec::new()
        } else {
            base.to_vec()
        };
        for (off, data) in &self.writes {
            let off = *off as usize;
            let end = off + data.len();
            if content.len() < end {
                content.resize(end, 0);
            }
            content[off..end].copy_from_slice(data);
        }
        content
    }

    /// Insert an interval, trimming whatever it overlaps (last-write-wins,
    /// rev2§4.4). Returns the change in dirty bytes.
    fn insert(&mut self, off: u64, data: Vec<u8>) -> isize {
        let end = off + data.len() as u64;
        let mut delta = data.len() as isize;
        // Overlapping intervals start before `end` and finish after `off`.
        let overlapping: Vec<u64> = self
            .writes
            .range(..end)
            .rev()
            .take_while(|(k, v)| **k + v.len() as u64 > off)
            .map(|(k, _)| *k)
            .collect();
        for k in overlapping {
            let v = self.writes.remove(&k).unwrap();
            delta -= v.len() as isize;
            if k < off {
                let keep = (off - k) as usize;
                delta += keep as isize;
                self.writes.insert(k, v[..keep].to_vec());
            }
            if k + v.len() as u64 > end {
                let cut = (end - k) as usize;
                delta += (v.len() - cut) as isize;
                self.writes.insert(end, v[cut..].to_vec());
            }
        }
        self.writes.insert(off, data);
        delta
    }
}

#[derive(Debug, Clone)]
pub enum FileState<'a> {
    /// No overlay opinion — fall through to the tree.
    Clean,
    /// Pending writes over the (possibly absent) base.
    Dirty(&'a FileOverlay),
    /// Unlinked in this window.
    Unlinked,
}

#[derive(Debug, Default)]
pub struct Overlay {
    /// The per-file interval maps, re-keyed on the ephemeral [`FileId`].
    by_id: BTreeMap<FileId, FileOverlay>,
    /// Live name → id. The name-ordered index: it resolves a path to its id and
    /// range-scans for directory listings, replacing the old direct path-keying.
    by_name: BTreeMap<Path, FileId>,
    /// id → current name (rev2§4.9 "ID → current-path map"): the inverse of
    /// `by_name` for every live file. `None` marks an id whose name was unlinked
    /// while still open (rev2§4.9 unlink-while-open — the orphaned open handle).
    names: BTreeMap<FileId, Option<Path>>,
    /// Tombstones: names that read as absent until flush removes them from the
    /// tree (the old `unlinks` set, unchanged in role). A file rename adds its
    /// source name here.
    tombs: BTreeSet<Path>,
    /// Open-handle refcount per id (rev2§4.9). `open[id] > 0` means a live handle
    /// holds the id, which changes two things: unlinking the id *orphans* it
    /// (`names[id] = None`, data kept) rather than reaping it, and the handle's
    /// id↔name binding is carried across a flush so it keeps working. Empty until
    /// an id is `open`ed; ephemeral, like the ids themselves (never persisted, no
    /// open/close WAL record — a crash leaves no open handles, see [`Self::carry_open`]).
    open: BTreeMap<FileId, u32>,
    /// Pending directory moves `from → to` (rev2§4.9 detach/reattach). A
    /// directory has no dirty bytes to re-key, so its move is recorded here and
    /// executed at flush as a `tree::remove`+`tree::put` of its `DirRoot` entry.
    /// `Store::rename` drains dirty descendants (flush-first) before
    /// recording the move, so no file overlay hides under `from`.
    dir_renames: BTreeMap<Path, Path>,
    bytes: usize,
}

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty() && self.tombs.is_empty() && self.dir_renames.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn write(
        &mut self,
        path: &Path,
        offset: u64,
        data: &[u8],
        mtime: u64,
        next_id: &mut FileId,
    ) {
        // A write resurrects an unlinked path as a fresh file.
        let fresh = self.tombs.remove(path);
        let id = if let Some(&id) = self.by_name.get(path) {
            // An existing live name keeps its id. It may be a dirty file or an
            // opened-but-unwritten handle (a `by_name` entry with no `by_id`
            // yet); either way the id is reused.
            id
        } else {
            // First touch of this name in the window: allocate an id and bind it.
            let id = *next_id;
            *next_id += 1;
            self.by_name.insert(path.clone(), id);
            self.names.insert(id, Some(path.clone()));
            id
        };
        // Materialize the interval map on first write to the id, fixing its base
        // origin — `None` for a fresh/resurrected file (no base bytes). A live
        // dirty name can never be tombstoned, so `fresh` is only true here when
        // the id had no `by_id` entry yet (a first, opened-handle, or resurrecting
        // write); for an already-dirty id `or_insert_with` does not run.
        let fo = self.by_id.entry(id).or_insert_with(|| FileOverlay {
            fresh,
            origin: if fresh { None } else { Some(path.clone()) },
            ..FileOverlay::default()
        });
        fo.mtime = mtime;
        let delta = fo.insert(offset, data.to_vec());
        self.bytes = (self.bytes as isize + delta).max(0) as usize;
    }

    pub fn unlink(&mut self, path: &Path, _mtime: u64) {
        if let Some(id) = self.by_name.remove(path) {
            if self.is_open(id) {
                // rev2§4.9 unlink-while-open: the id is held by a live handle, so
                // keep its dirty data and mark it nameless (`None`). The handle
                // keeps working against the overlay; at flush the now-orphaned id
                // resolves to no name and `files()` skips it, so the data is
                // discarded. `bytes` is unchanged — the data is still held.
                self.names.insert(id, None);
            } else {
                // No open handle: reap the id outright.
                if let Some(fo) = self.by_id.remove(&id) {
                    let drop_bytes: usize = fo.writes.values().map(|v| v.len()).sum();
                    self.bytes = self.bytes.saturating_sub(drop_bytes);
                }
                self.names.remove(&id);
            }
        }
        self.tombs.insert(path.clone());
    }

    /// Whether a live handle holds `id` (rev2§4.9). Drives the orphan-vs-reap
    /// choice in [`Self::unlink`] and the carry in [`Self::carry_open`].
    fn is_open(&self, id: FileId) -> bool {
        self.open.get(&id).copied().unwrap_or(0) > 0
    }

    /// Open a handle on `path`, returning its ephemeral [`FileId`] (rev2§4.9
    /// "assigned per open file"). Resolves an existing binding — a dirty file or
    /// an already-open handle — or allocates a fresh id bound to the name with no
    /// `by_id` entry yet (an open file holds no dirty data until first written).
    /// Idempotent on the id: repeated opens of one name share it and bump the
    /// refcount. Does not touch `tombs`: opening a tombstoned name reserves the id
    /// but leaves the name reading-as-absent until a write resurrects it.
    pub fn open(&mut self, path: &Path, next_id: &mut FileId) -> FileId {
        let id = if let Some(&id) = self.by_name.get(path) {
            id
        } else {
            let id = *next_id;
            *next_id += 1;
            self.by_name.insert(path.clone(), id);
            self.names.insert(id, Some(path.clone()));
            id
        };
        *self.open.entry(id).or_insert(0) += 1;
        id
    }

    /// Close one handle on `id`, returning whether it was the last (so the store
    /// can drop its id→ref entry). On the last close, reap by state (rev2§4.9):
    /// an orphaned id (`names[id] == None`) discards its dirty data; an opened-
    /// but-never-written name drops its binding (it represented nothing — back to
    /// `Clean`); a dirty, still-named id is kept (it flushes normally, just no
    /// longer pinned open).
    pub fn close(&mut self, id: FileId) -> bool {
        match self.open.get_mut(&id) {
            Some(c) if *c > 1 => {
                *c -= 1;
                false
            }
            Some(_) => {
                self.open.remove(&id);
                match self.names.get(&id) {
                    Some(None) => {
                        // Orphaned (unlinked-while-open): discard the held data.
                        if let Some(fo) = self.by_id.remove(&id) {
                            let drop: usize = fo.writes.values().map(|v| v.len()).sum();
                            self.bytes = self.bytes.saturating_sub(drop);
                        }
                        self.names.remove(&id);
                    }
                    Some(Some(name)) if !self.by_id.contains_key(&id) => {
                        // Opened, never written, still named: nothing dirty — drop
                        // the reservation so the name reverts to the tree.
                        let name = name.clone();
                        self.by_name.remove(&name);
                        self.names.remove(&id);
                    }
                    _ => {} // dirty and still named: keep; it flushes normally.
                }
                true
            }
            None => true,
        }
    }

    /// The current name an open id resolves to (rev2§4.9 "ID → current-path
    /// map"): `Some(path)` while named, `None` once unlinked-while-open. The store
    /// routes `write_id`/`read_id` on this — a named handle through the durable
    /// path-addressed write, an orphaned one through [`Self::write_orphan`].
    pub fn name_of(&self, id: FileId) -> Option<Path> {
        self.names.get(&id).cloned().flatten()
    }

    /// Write to an *orphaned* (nameless) open id (rev2§4.9): the handle keeps
    /// working against the overlay after its name was unlinked. The data has no
    /// path, so it is never WAL-logged and is discarded at flush — purely
    /// ephemeral. Returns nothing; `bytes` tracks it until close/flush drops it.
    pub fn write_orphan(&mut self, id: FileId, offset: u64, data: &[u8], mtime: u64) {
        let fo = self.by_id.entry(id).or_insert_with(|| FileOverlay {
            fresh: true, // orphaned: no base path to read, apply over empty.
            origin: None,
            ..FileOverlay::default()
        });
        fo.mtime = mtime;
        let delta = fo.insert(offset, data.to_vec());
        self.bytes = (self.bytes as isize + delta).max(0) as usize;
    }

    /// Read an *orphaned* open id's content against an empty base (rev2§4.9): the
    /// name is gone, so there is no tree base to fall through to. Empty if the
    /// handle was orphaned before any write.
    pub fn read_orphan(&self, id: FileId) -> Vec<u8> {
        self.by_id
            .get(&id)
            .map(|fo| fo.apply(&[]))
            .unwrap_or_default()
    }

    /// Consume the overlay at flush, returning a fresh one that carries only the
    /// open handles forward (rev2§4.9 "the open handle keeps working") — their
    /// refcounts and id↔name bindings, with **no** dirty data (`by_id`/`tombs`
    /// empty, `bytes == 0`): the named data was just committed to the tree and the
    /// orphaned data discarded. `None` when nothing is open, so the store removes
    /// the overlay entirely. Re-deriving the bindings
    /// here is why an open handle survives the auto-flushes that WAL/byte pressure
    /// can trigger mid-write.
    pub fn carry_open(self) -> Option<Overlay> {
        if self.open.is_empty() {
            return None;
        }
        let mut next = Overlay::default();
        for (&id, &refs) in &self.open {
            let name = self.names.get(&id).cloned().flatten();
            if let Some(p) = &name {
                next.by_name.insert(p.clone(), id);
            }
            next.names.insert(id, name);
            next.open.insert(id, refs);
        }
        Some(next)
    }

    pub fn state(&self, path: &Path) -> FileState<'_> {
        if self.tombs.contains(path) {
            FileState::Unlinked
        } else if let Some(id) = self.by_name.get(path) {
            // An opened-but-unwritten name has a `by_name` entry but no `by_id`:
            // no overlay opinion yet, so fall through to the tree.
            match self.by_id.get(id) {
                Some(fo) => FileState::Dirty(fo),
                None => FileState::Clean,
            }
        } else {
            FileState::Clean
        }
    }

    /// Whether `path` is a live (dirty) file in the overlay — the fast check
    /// that lets a rename take the O(1) name-swap path (vs. opening a clean
    /// committed file or recording a directory move).
    pub fn contains_file(&self, path: &Path) -> bool {
        self.by_name.contains_key(path)
    }

    pub fn unlinks(&self) -> impl Iterator<Item = &Path> {
        self.tombs.iter()
    }

    pub fn files(&self) -> impl Iterator<Item = (&Path, &FileOverlay)> {
        // Resolve name → id → interval map. `by_name` order matches the old
        // path-keyed iteration, so flush walks the same names in the same order.
        // An opened-but-unwritten name (a `by_name` entry with no `by_id`)
        // holds no dirty data and is skipped — flush has nothing to write for it.
        self.by_name
            .iter()
            .filter_map(move |(name, id)| self.by_id.get(id).map(|fo| (name, fo)))
    }

    /// Dirty files directly inside `dir` (for merged listings).
    pub fn files_in_dir<'a>(
        &'a self,
        dir: &'a [Vec<u8>],
    ) -> impl Iterator<Item = (&'a Path, &'a FileOverlay)> + 'a {
        self.by_name
            .iter()
            .filter(move |(p, _)| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
            .filter_map(move |(p, id)| self.by_id.get(id).map(|fo| (p, fo)))
    }

    pub fn unlinked_in_dir<'a>(
        &'a self,
        dir: &'a [Vec<u8>],
    ) -> impl Iterator<Item = &'a Path> + 'a {
        self.tombs
            .iter()
            .filter(move |p| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
    }

    /// Pending directory moves (`from`, `to`), applied at flush.
    pub fn dir_renames(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.dir_renames.iter()
    }

    /// Bring a clean, committed *file* into the overlay so a rename can move it
    /// (rev2§4.9). It carries no dirty writes; its `origin` is its current name,
    /// so flush reads the unchanged committed bytes there and writes them at the
    /// renamed name. The caller (`Store::rename`) has verified `name` is a file
    /// absent from the overlay.
    pub fn open_for_rename(&mut self, name: &Path, next_id: &mut FileId) {
        let id = *next_id;
        *next_id += 1;
        self.by_id.insert(
            id,
            FileOverlay {
                fresh: false,
                origin: Some(name.clone()),
                ..FileOverlay::default()
            },
        );
        self.by_name.insert(name.clone(), id);
        self.names.insert(id, Some(name.clone()));
    }

    /// Move a live overlay *file* `from → to` in O(1): the id keeps its interval
    /// map (it never moves), only the name pointers swap (rev2§4.9). The source
    /// name is tombstoned (reads absent, flush removes its committed entry); the
    /// `origin` is untouched, so flush still reads the pre-edit base at the
    /// original name. A pre-existing destination is overwritten last-write-wins
    /// (rev2§4.4). The caller guarantees `from` is live in `by_name`.
    pub fn rename(&mut self, from: &Path, to: &Path, mtime: u64) {
        let id = *self.by_name.get(from).expect("rename source must be live");
        // Destination last-write-wins: the target name becomes live again; reap
        // any id currently parked there.
        self.tombs.remove(to);
        if let Some(victim) = self.by_name.remove(to) {
            if let Some(fo) = self.by_id.remove(&victim) {
                let drop_bytes: usize = fo.writes.values().map(|v| v.len()).sum();
                self.bytes = self.bytes.saturating_sub(drop_bytes);
            }
            self.names.remove(&victim);
        }
        // The O(1) swap: move the name, never the interval map.
        self.by_name.remove(from);
        self.by_name.insert(to.clone(), id);
        self.names.insert(id, Some(to.clone()));
        self.tombs.insert(from.clone());
        self.by_id.get_mut(&id).unwrap().mtime = mtime;
    }

    /// Record a directory move `from → to` for flush to apply as a tree
    /// detach/reattach (rev2§4.9). The caller has drained dirty descendants.
    pub fn rename_dir(&mut self, from: &Path, to: &Path) {
        self.dir_renames.insert(from.clone(), to.clone());
    }

    /// Cross-check the id indirection (test-only): the indices stay mutually
    /// consistent after every op. `by_name`/`names` are inverses for live files;
    /// every id is either dirty (`by_id`) or open (or both); a nameless id is an
    /// unlinked-while-open orphan and must be held open; a *dirty* live name
    /// is never tombstoned (an opened-but-unwritten one may be); and the byte
    /// census is recomputable from state — `bytes` equals the total length of
    /// every live interval map, so a clamp cannot silently absorb a miscount.
    #[cfg(test)]
    pub(crate) fn check_invariants(&self) {
        for (name, id) in &self.by_name {
            assert!(
                self.by_id.contains_key(id) || self.is_open(*id),
                "by_name id {id} neither dirty nor open"
            );
            assert_eq!(
                self.names.get(id),
                Some(&Some(name.clone())),
                "by_name/names disagree for id {id}"
            );
            // A dirty live name can never be tombstoned; an opened-but-unwritten
            // name may be (opening a tombstoned name reserves the id without
            // resurrecting it — a write would clear the tomb and set `fresh`).
            if self.by_id.contains_key(id) {
                assert!(
                    !self.tombs.contains(name),
                    "dirty name live and tombstoned: {name:?}"
                );
            }
        }
        for (id, maybe) in &self.names {
            assert!(
                self.by_id.contains_key(id) || self.is_open(*id),
                "names id {id} neither dirty nor open"
            );
            match maybe {
                Some(name) => assert_eq!(
                    self.by_name.get(name),
                    Some(id),
                    "names/by_name disagree for id {id}"
                ),
                None => assert!(self.is_open(*id), "nameless (orphan) id {id} not open"),
            }
        }
        for id in self.by_id.keys() {
            assert!(
                self.names.contains_key(id),
                "by_id id {id} absent from names"
            );
        }
        for (id, refs) in &self.open {
            assert!(*refs > 0, "open id {id} has zero refcount");
            assert!(
                self.names.contains_key(id),
                "open id {id} absent from names"
            );
        }
        // The byte census is recomputable from state: `bytes` (rev2§4.4) must
        // equal the total length of every live interval map. It is delta-accounted
        // through `(bytes + delta).max(0)` writes and `saturating_sub` reaps, whose
        // clamps would otherwise silently absorb a miscount — this independent
        // recomputation catches any such drift.
        let census: usize = self
            .by_id
            .values()
            .map(|fo| fo.writes.values().map(|v| v.len()).sum::<usize>())
            .sum();
        assert_eq!(
            self.bytes, census,
            "byte census disagrees with by_id writes"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn path(s: &str) -> Path {
        vec![s.as_bytes().to_vec()]
    }

    #[test]
    fn overlapping_writes_last_wins() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let p = path("f");
        o.write(&p, 0, b"aaaaaaaa", 1, &mut next);
        o.write(&p, 2, b"bb", 2, &mut next);
        o.write(&p, 6, b"cccc", 3, &mut next);
        let FileState::Dirty(fo) = o.state(&p) else {
            panic!()
        };
        assert_eq!(fo.apply(&[]), b"aabbaacccc".to_vec());
        assert_eq!(o.bytes(), 10);
        o.check_invariants();
        // Three writes to one name share a single id (no re-key per write).
        assert_eq!(next, 1);
    }

    #[test]
    fn unlink_then_write_is_fresh() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let p = path("f");
        o.unlink(&p, 1);
        assert!(matches!(o.state(&p), FileState::Unlinked));
        o.write(&p, 1, b"x", 2, &mut next);
        let FileState::Dirty(fo) = o.state(&p) else {
            panic!()
        };
        assert!(fo.fresh);
        assert_eq!(fo.apply(b"old content"), vec![0, b'x']);
        o.check_invariants();
    }

    /// Renaming-free re-key still allocates a fresh id when a name is unlinked
    /// and then rewritten (the old id was reaped at unlink).
    #[test]
    fn resurrect_after_unlink_reallocates_id() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let p = path("f");
        o.write(&p, 0, b"hello", 1, &mut next);
        assert_eq!(next, 1);
        o.unlink(&p, 2);
        o.write(&p, 0, b"x", 3, &mut next);
        // A second id was minted for the resurrected file.
        assert_eq!(next, 2);
        o.check_invariants();
    }

    /// Base origin (consumed by flush): fixed at first write to the
    /// name for a file with a committed base, `None` for a fresh/resurrected one.
    #[test]
    fn origin_fixed_at_first_write() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let p = path("f");
        o.write(&p, 0, b"hi", 1, &mut next);
        let FileState::Dirty(fo) = o.state(&p) else {
            panic!()
        };
        assert_eq!(fo.origin, Some(p.clone()));

        // A resurrected (write-after-unlink) file carries no base origin.
        let q = path("g");
        o.unlink(&q, 2);
        o.write(&q, 0, b"x", 3, &mut next);
        let FileState::Dirty(fo) = o.state(&q) else {
            panic!()
        };
        assert_eq!(fo.origin, None);
    }

    /// Negative control: the model-equivalence proptest's oracle has teeth. A
    /// reference that forgot resurrect-as-`fresh` would apply the new write over
    /// the stale base; the real overlay must disagree with that wrong prediction.
    #[test]
    fn negative_control_resurrect_fresh() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let p = path("f");
        o.unlink(&p, 1);
        o.write(&p, 1, b"x", 2, &mut next);
        let FileState::Dirty(fo) = o.state(&p) else {
            panic!()
        };
        // Real overlay: fresh, base ignored → [0, 'x'].
        assert_eq!(fo.apply(b"OLD"), vec![0, b'x']);
        // A fresh-forgetting oracle would splice 'x' into "OLD" → "OxD".
        let mut wrong = b"OLD".to_vec();
        wrong[1] = b'x';
        assert_ne!(fo.apply(b"OLD"), wrong);
    }

    /// A file rename moves the name only: the id, its interval map, and its
    /// `origin` are untouched, the source reads absent, the destination reads
    /// the moved dirty content (rev2§4.9).
    #[test]
    fn rename_moves_name_keeps_id_and_origin() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let (a, b) = (path("a"), path("b"));
        o.write(&a, 0, b"hello", 1, &mut next);
        assert_eq!(next, 1); // one id minted
        o.rename(&a, &b, 5);
        assert_eq!(next, 1); // rename mints no id
                             // Source reads absent (tombstoned); destination carries the same bytes.
        assert!(matches!(o.state(&a), FileState::Unlinked));
        let FileState::Dirty(fo) = o.state(&b) else {
            panic!("renamed file must be dirty at the new name")
        };
        assert_eq!(fo.apply(&[]), b"hello".to_vec());
        // `origin` stays the *original* name — flush reads the base there.
        assert_eq!(fo.origin(), Some(&a));
        assert_eq!(fo.mtime, 5);
        o.check_invariants();
    }

    /// O(1) witness: renaming a file with a large dirty interval map does
    /// not move or rebuild the interval map — only the name pointers swap. We
    /// witness this structurally: the per-id `writes` map is byte-identical
    /// before and after the rename.
    #[test]
    fn rename_does_not_move_the_interval_map() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let (a, b) = (path("a"), path("b"));
        // A fat, fragmented interval map: 500 disjoint 4-byte intervals.
        for i in 0..500u64 {
            o.write(&a, i * 8, b"abcd", i + 1, &mut next);
        }
        let FileState::Dirty(before) = o.state(&a) else {
            panic!()
        };
        let writes_before = before.writes.clone();
        let bytes_before = o.bytes();
        assert_eq!(writes_before.len(), 500);

        o.rename(&a, &b, 9999);

        let FileState::Dirty(after) = o.state(&b) else {
            panic!()
        };
        // Same interval map object content — the rename touched no dirty state.
        assert_eq!(after.writes, writes_before);
        assert_eq!(o.bytes(), bytes_before);
        o.check_invariants();
    }

    /// Renaming onto an existing destination is last-write-wins: the old target
    /// id is reaped (its dirty bytes released) and the source takes its place.
    #[test]
    fn rename_onto_existing_is_last_write_wins() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let (a, b) = (path("a"), path("b"));
        o.write(&a, 0, b"AAAA", 1, &mut next);
        o.write(&b, 0, b"BBBBBBBB", 2, &mut next);
        assert_eq!(o.bytes(), 12);
        o.rename(&a, &b, 3);
        let FileState::Dirty(fo) = o.state(&b) else {
            panic!()
        };
        // `b` now holds `a`'s content; `b`'s old 8 bytes were released.
        assert_eq!(fo.apply(&[]), b"AAAA".to_vec());
        assert_eq!(o.bytes(), 4);
        assert!(matches!(o.state(&a), FileState::Unlinked));
        o.check_invariants();
    }

    /// Renaming back to the original name (`a→b→a`) restores `a` as a live file
    /// and leaves only `b` tombstoned (the indices stay consistent).
    #[test]
    fn rename_round_trip_restores_source() {
        let mut o = Overlay::default();
        let mut next = 0u64;
        let (a, b) = (path("a"), path("b"));
        o.write(&a, 0, b"x", 1, &mut next);
        o.rename(&a, &b, 2);
        o.rename(&b, &a, 3);
        let FileState::Dirty(fo) = o.state(&a) else {
            panic!()
        };
        assert_eq!(fo.apply(&[]), b"x".to_vec());
        assert!(matches!(o.state(&b), FileState::Unlinked));
        o.check_invariants();
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// The interval map agrees with a naive byte-array model.
        #[test]
        fn interval_map_matches_model(
            ops in proptest::collection::vec(
                (0u64..128, proptest::collection::vec(any::<u8>(), 1..32)),
                1..40,
            ),
        ) {
            let mut fo = FileOverlay::default();
            let mut model: Vec<u8> = Vec::new();
            let mut written = vec![false; 256];
            for (off, data) in &ops {
                fo.insert(*off, data.clone());
                let end = *off as usize + data.len();
                if model.len() < end {
                    model.resize(end, 0);
                }
                model[*off as usize..end].copy_from_slice(data);
                for w in &mut written[*off as usize..end] {
                    *w = true;
                }
            }
            // Untouched gap bytes are zero in both.
            prop_assert_eq!(fo.apply(&[]), model);
            // Intervals stay non-overlapping and sorted.
            let mut prev_end = 0u64;
            for (off, data) in &fo.writes {
                prop_assert!(*off >= prev_end);
                prev_end = off + data.len() as u64;
            }
        }
    }

    // ── Behaviour-preserving re-key: reference model + equivalence proptest ──

    const NAMES: usize = 4;

    /// A small fixed name set: two at the root and two under `d/`, so the
    /// equivalence check exercises both root and subdirectory listings.
    fn name_set() -> [Path; NAMES] {
        [
            vec![b"a".to_vec()],
            vec![b"b".to_vec()],
            vec![b"d".to_vec(), b"x".to_vec()],
            vec![b"d".to_vec(), b"y".to_vec()],
        ]
    }

    /// A verbatim copy of the path-keyed overlay: the oracle the re-keyed
    /// `Overlay` must match operation-for-operation. (`FileOverlay::insert` and
    /// its `writes` field are reachable from this child module.)
    #[derive(Default)]
    struct RefOverlay {
        files: BTreeMap<Path, FileOverlay>,
        unlinks: BTreeSet<Path>,
        bytes: usize,
    }

    impl RefOverlay {
        fn write(&mut self, path: &Path, offset: u64, data: &[u8], mtime: u64) {
            let fresh = self.unlinks.remove(path);
            let fo = self.files.entry(path.clone()).or_default();
            if fresh {
                fo.fresh = true;
            }
            fo.mtime = mtime;
            let delta = fo.insert(offset, data.to_vec());
            self.bytes = (self.bytes as isize + delta).max(0) as usize;
        }

        fn unlink(&mut self, path: &Path) {
            if let Some(fo) = self.files.remove(path) {
                let drop_bytes: usize = fo.writes.values().map(|v| v.len()).sum();
                self.bytes = self.bytes.saturating_sub(drop_bytes);
            }
            self.unlinks.insert(path.clone());
        }

        /// Path-keyed rename oracle: move the per-file interval map by path
        /// (the naive O(dirty) cost the id indirection avoids), last-write-wins
        /// over the destination, tombstone the source. Caller guarantees `from`
        /// is a live file.
        fn rename(&mut self, from: &Path, to: &Path, mtime: u64) {
            let mut fo = self
                .files
                .remove(from)
                .expect("ref rename source must be live");
            if let Some(victim) = self.files.remove(to) {
                let drop_bytes: usize = victim.writes.values().map(|v| v.len()).sum();
                self.bytes = self.bytes.saturating_sub(drop_bytes);
            }
            self.unlinks.remove(to);
            fo.mtime = mtime;
            self.files.insert(to.clone(), fo);
            self.unlinks.insert(from.clone());
        }

        fn state(&self, path: &Path) -> FileState<'_> {
            if self.unlinks.contains(path) {
                FileState::Unlinked
            } else if let Some(fo) = self.files.get(path) {
                FileState::Dirty(fo)
            } else {
                FileState::Clean
            }
        }

        fn is_empty(&self) -> bool {
            self.files.is_empty() && self.unlinks.is_empty()
        }
    }

    /// Collapse a `FileState` to a comparable snapshot so a real and a reference
    /// state can be `prop_assert_eq!`'d directly.
    fn snap(s: FileState<'_>) -> (u8, Vec<u8>, bool, u64, u64) {
        match s {
            FileState::Clean => (0, Vec::new(), false, 0, 0),
            FileState::Unlinked => (1, Vec::new(), false, 0, 0),
            FileState::Dirty(fo) => (2, fo.apply(&[]), fo.fresh, fo.mtime, fo.extent()),
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Write(usize, u64, Vec<u8>),
        Unlink(usize),
        Rename(usize, usize),
    }

    fn overlay_op() -> impl Strategy<Value = Op> {
        prop_oneof![
            (
                0usize..NAMES,
                0u64..64,
                proptest::collection::vec(any::<u8>(), 1..16)
            )
                .prop_map(|(i, off, data)| Op::Write(i, off, data)),
            (0usize..NAMES).prop_map(Op::Unlink),
            (0usize..NAMES, 0usize..NAMES).prop_map(|(i, j)| Op::Rename(i, j)),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// The re-keyed overlay reproduces the path-keyed semantics exactly, and
        /// the id indices stay internally consistent after every op.
        #[test]
        fn overlay_matches_path_model(ops in proptest::collection::vec(overlay_op(), 1..60)) {
            let names = name_set();
            let mut real = Overlay::default();
            let mut refm = RefOverlay::default();
            let mut next_id = 0u64;
            let mut mtime = 1u64;
            for op in &ops {
                match op {
                    Op::Write(i, off, data) => {
                        real.write(&names[*i], *off, data, mtime, &mut next_id);
                        refm.write(&names[*i], *off, data, mtime);
                    }
                    Op::Unlink(i) => {
                        real.unlink(&names[*i], mtime);
                        refm.unlink(&names[*i]);
                    }
                    Op::Rename(i, j) => {
                        // The overlay-level swap requires a live source; a clean
                        // committed file is opened by the store, not here. Skip
                        // self-renames and dead sources so both models step in
                        // lockstep (the store-level proptest covers the rest).
                        if i != j && real.contains_file(&names[*i]) {
                            real.rename(&names[*i], &names[*j], mtime);
                            refm.rename(&names[*i], &names[*j], mtime);
                        }
                    }
                }
                mtime += 1;
                real.check_invariants();
                prop_assert_eq!(real.bytes(), refm.bytes);
                prop_assert_eq!(real.is_empty(), refm.is_empty());
                for p in &names {
                    prop_assert_eq!(snap(real.state(p)), snap(refm.state(p)));
                }
                // Listings agree for the root and the one subdirectory.
                for dir in [&[][..], &[b"d".to_vec()][..]] {
                    let mut rf: Vec<(Path, bool, u64)> = real
                        .files_in_dir(dir)
                        .map(|(p, fo)| (p.clone(), fo.fresh, fo.extent()))
                        .collect();
                    let mut mf: Vec<(Path, bool, u64)> = refm
                        .files
                        .iter()
                        .filter(|(p, _)| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
                        .map(|(p, fo)| (p.clone(), fo.fresh, fo.extent()))
                        .collect();
                    rf.sort();
                    mf.sort();
                    prop_assert_eq!(rf, mf);

                    let mut ru: Vec<Path> = real.unlinked_in_dir(dir).cloned().collect();
                    let mut mu: Vec<Path> = refm
                        .unlinks
                        .iter()
                        .filter(|p| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
                        .cloned()
                        .collect();
                    ru.sort();
                    mu.sort();
                    prop_assert_eq!(ru, mu);
                }
            }
        }
    }
}
