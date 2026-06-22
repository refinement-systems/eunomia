//! Per-ref in-memory overlay — the memtable (spec rev1§4.3–4.4).
//!
//! Writes land here first, keyed by path with per-file interval maps;
//! reads consult the overlay and fall through to the immutable tree — an
//! LSM read path whose bottom level is the prolly tree. Bounds are
//! denominated in bytes of dirty overlay (rev1§4.4); the store enforces the
//! budget with backpressure-by-flush, never eviction.
//!
//! The overlay keys its per-file interval maps on an ephemeral, server-runtime
//! [`FileId`] (rev1§4.3/§4.9), not on the path: a name-ordered `by_name` index
//! resolves a path to its id, and an `id → name` map (`names`) is what a future
//! rename swaps in O(1) regardless of how much dirty state the file holds. IDs
//! are runtime-only and never touch disk — they are re-derived by replaying the
//! path-keyed WAL on mount. Rename itself and unlink-while-open land in later C2
//! sub-phases; this module is the re-keying that makes them O(1), with behavior
//! otherwise identical to the path-keyed overlay it replaces.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

pub type Path = Vec<Vec<u8>>;

/// Ephemeral, server-runtime file identity (rev1§4.9): assigned per file when it
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
    /// Committed-tree path to read pre-edit bytes from at flush (rev1§4.9 base
    /// origin), fixed at first write and distinct from the current name once a
    /// rename moves the name; `None` for a `fresh` file (no base to read). Set
    /// here but only consulted by flush once rename exists (C2B) — in C2A
    /// `origin` always equals the current name, so flush still reads the base at
    /// the current name and the field is write-only for now.
    #[allow(dead_code)]
    origin: Option<Path>,
}

impl FileOverlay {
    /// Largest offset written (for synthesizing sizes in listings).
    pub fn extent(&self) -> u64 {
        self.writes
            .iter()
            .next_back()
            .map(|(off, data)| off + data.len() as u64)
            .unwrap_or(0)
    }

    /// Offset of the first byte this overlay changed (`None` only if nothing
    /// was written). rev1§4.3 neighborhood re-chunk bounds the re-chunked
    /// region from here.
    pub fn first_write_offset(&self) -> Option<u64> {
        self.writes.keys().next().copied()
    }

    /// Apply the intervals over base content (zero-fill gaps). Borrows `base`
    /// so the caller can keep the pre-edit bytes alive alongside the result
    /// (the rev1§4.3 neighborhood re-chunk diffs new against old).
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
    /// rev1§4.4). Returns the change in dirty bytes.
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
    /// id → current name (rev1§4.9 "ID → current-path map"): the inverse of
    /// `by_name` for every live file. `None` marks an id whose name was unlinked
    /// while still open — reserved for C2C, never produced in C2A.
    names: BTreeMap<FileId, Option<Path>>,
    /// Tombstones: names that read as absent until flush removes them from the
    /// tree (the old `unlinks` set, unchanged in role).
    tombs: BTreeSet<Path>,
    bytes: usize,
}

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty() && self.tombs.is_empty()
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
            // An existing live name keeps its id (and is never `fresh`: a live
            // name cannot also be tombstoned).
            id
        } else {
            // First touch of this name in the window: allocate an id and fix its
            // base origin — `None` for a fresh/resurrected file (no base bytes).
            let id = *next_id;
            *next_id += 1;
            self.by_id.insert(
                id,
                FileOverlay {
                    fresh,
                    origin: if fresh { None } else { Some(path.clone()) },
                    ..FileOverlay::default()
                },
            );
            self.by_name.insert(path.clone(), id);
            self.names.insert(id, Some(path.clone()));
            id
        };
        let fo = self.by_id.get_mut(&id).unwrap();
        fo.mtime = mtime;
        let delta = fo.insert(offset, data.to_vec());
        self.bytes = (self.bytes as isize + delta).max(0) as usize;
    }

    pub fn unlink(&mut self, path: &Path, _mtime: u64) {
        // C2A reaps the id outright; keeping an unlinked-but-open id alive (with
        // a `None` name) is the unlink-while-open semantic deferred to C2C.
        if let Some(id) = self.by_name.remove(path) {
            if let Some(fo) = self.by_id.remove(&id) {
                let drop_bytes: usize = fo.writes.values().map(|v| v.len()).sum();
                self.bytes = self.bytes.saturating_sub(drop_bytes);
            }
            self.names.remove(&id);
        }
        self.tombs.insert(path.clone());
    }

    pub fn state(&self, path: &Path) -> FileState<'_> {
        if self.tombs.contains(path) {
            FileState::Unlinked
        } else if let Some(id) = self.by_name.get(path) {
            FileState::Dirty(self.by_id.get(id).unwrap())
        } else {
            FileState::Clean
        }
    }

    pub fn unlinks(&self) -> impl Iterator<Item = &Path> {
        self.tombs.iter()
    }

    pub fn files(&self) -> impl Iterator<Item = (&Path, &FileOverlay)> {
        // Resolve name → id → interval map. `by_name` order matches the old
        // path-keyed iteration, so flush walks the same names in the same order.
        self.by_name
            .iter()
            .map(move |(name, id)| (name, self.by_id.get(id).unwrap()))
    }

    /// Dirty files directly inside `dir` (for merged listings).
    pub fn files_in_dir<'a>(
        &'a self,
        dir: &'a [Vec<u8>],
    ) -> impl Iterator<Item = (&'a Path, &'a FileOverlay)> + 'a {
        self.by_name
            .iter()
            .filter(move |(p, _)| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
            .map(move |(p, id)| (p, self.by_id.get(id).unwrap()))
    }

    pub fn unlinked_in_dir<'a>(
        &'a self,
        dir: &'a [Vec<u8>],
    ) -> impl Iterator<Item = &'a Path> + 'a {
        self.tombs
            .iter()
            .filter(move |p| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
    }

    /// Cross-check the id indirection (test-only): the indices stay mutually
    /// consistent after every op. `by_name`/`names` are inverses for live files,
    /// `by_id` and `names` cover the same id set, and no name is both live and
    /// tombstoned.
    #[cfg(test)]
    fn check_invariants(&self) {
        for (name, id) in &self.by_name {
            assert!(
                self.by_id.contains_key(id),
                "by_name id {id} absent from by_id"
            );
            assert_eq!(
                self.names.get(id),
                Some(&Some(name.clone())),
                "by_name/names disagree for id {id}"
            );
            assert!(
                !self.tombs.contains(name),
                "name live and tombstoned: {name:?}"
            );
        }
        for (id, maybe) in &self.names {
            assert!(
                self.by_id.contains_key(id),
                "names id {id} absent from by_id"
            );
            if let Some(name) = maybe {
                assert_eq!(
                    self.by_name.get(name),
                    Some(id),
                    "names/by_name disagree for id {id}"
                );
            }
        }
        for id in self.by_id.keys() {
            assert!(
                self.names.contains_key(id),
                "by_id id {id} absent from names"
            );
        }
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

    /// DD3 base origin (consumed by flush in C2B): fixed at first write to the
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

    /// A verbatim copy of the pre-C2A path-keyed overlay: the oracle the re-keyed
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
