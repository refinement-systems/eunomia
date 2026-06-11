//! Per-ref in-memory overlay — the memtable (spec §4.3–4.4).
//!
//! Writes land here first, keyed by path with per-file interval maps;
//! reads consult the overlay and fall through to the immutable tree — an
//! LSM read path whose bottom level is the prolly tree. Bounds are
//! denominated in bytes of dirty overlay (§4.4); the store enforces the
//! budget with backpressure-by-flush, never eviction.
//!
//! Paths key the overlay directly: rename support (and with it the
//! ephemeral file-id indirection of §4.9) is deferred until a rename
//! operation exists — M2 debt, recorded.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

pub type Path = Vec<Vec<u8>>;

#[derive(Debug, Default, Clone)]
pub struct FileOverlay {
    /// Sorted, non-overlapping intervals: offset → bytes.
    writes: BTreeMap<u64, Vec<u8>>,
    /// Base content is ignored: the file was (re)created inside this
    /// overlay window (e.g. write after unlink).
    pub fresh: bool,
    pub mtime: u64,
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

    /// Apply the intervals over base content (zero-fill gaps).
    pub fn apply(&self, base: Vec<u8>) -> Vec<u8> {
        let mut content = if self.fresh { Vec::new() } else { base };
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
    /// §4.4). Returns the change in dirty bytes.
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
    files: BTreeMap<Path, FileOverlay>,
    unlinks: BTreeSet<Path>,
    bytes: usize,
}

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.unlinks.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn write(&mut self, path: &Path, offset: u64, data: &[u8], mtime: u64) {
        // A write resurrects an unlinked path as a fresh file.
        let fresh = self.unlinks.remove(path);
        let fo = self.files.entry(path.clone()).or_default();
        if fresh {
            fo.fresh = true;
        }
        fo.mtime = mtime;
        let delta = fo.insert(offset, data.to_vec());
        self.bytes = (self.bytes as isize + delta).max(0) as usize;
    }

    pub fn unlink(&mut self, path: &Path, _mtime: u64) {
        if let Some(fo) = self.files.remove(path) {
            let drop_bytes: usize = fo.writes.values().map(|v| v.len()).sum();
            self.bytes = self.bytes.saturating_sub(drop_bytes);
        }
        self.unlinks.insert(path.clone());
    }

    pub fn state(&self, path: &Path) -> FileState<'_> {
        if self.unlinks.contains(path) {
            FileState::Unlinked
        } else if let Some(fo) = self.files.get(path) {
            FileState::Dirty(fo)
        } else {
            FileState::Clean
        }
    }

    pub fn unlinks(&self) -> impl Iterator<Item = &Path> {
        self.unlinks.iter()
    }

    pub fn files(&self) -> impl Iterator<Item = (&Path, &FileOverlay)> {
        self.files.iter()
    }

    /// Dirty files directly inside `dir` (for merged listings).
    pub fn files_in_dir<'a>(
        &'a self,
        dir: &'a [Vec<u8>],
    ) -> impl Iterator<Item = (&'a Path, &'a FileOverlay)> + 'a {
        self.files.iter().filter(move |(p, _)| {
            p.len() == dir.len() + 1 && p[..dir.len()] == *dir
        })
    }

    pub fn unlinked_in_dir<'a>(
        &'a self,
        dir: &'a [Vec<u8>],
    ) -> impl Iterator<Item = &'a Path> + 'a {
        self.unlinks
            .iter()
            .filter(move |p| p.len() == dir.len() + 1 && p[..dir.len()] == *dir)
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
        let p = path("f");
        o.write(&p, 0, b"aaaaaaaa", 1);
        o.write(&p, 2, b"bb", 2);
        o.write(&p, 6, b"cccc", 3);
        let FileState::Dirty(fo) = o.state(&p) else { panic!() };
        assert_eq!(fo.apply(Vec::new()), b"aabbaacccc".to_vec());
        assert_eq!(o.bytes(), 10);
    }

    #[test]
    fn unlink_then_write_is_fresh() {
        let mut o = Overlay::default();
        let p = path("f");
        o.unlink(&p, 1);
        assert!(matches!(o.state(&p), FileState::Unlinked));
        o.write(&p, 1, b"x", 2);
        let FileState::Dirty(fo) = o.state(&p) else { panic!() };
        assert!(fo.fresh);
        assert_eq!(fo.apply(b"old content".to_vec()), vec![0, b'x']);
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
            prop_assert_eq!(fo.apply(Vec::new()), model);
            // Intervals stay non-overlapping and sorted.
            let mut prev_end = 0u64;
            for (off, data) in &fo.writes {
                prop_assert!(*off >= prev_end);
                prev_end = off + data.len() as u64;
            }
        }
    }
}
