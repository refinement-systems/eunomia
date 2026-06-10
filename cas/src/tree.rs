//! Nested directory operations (spec §4.9): every operation is
//! openat-shaped — relative to an explicitly named root, taking component
//! lists. `/` is shell presentation, not a format concept. There is no
//! global root anywhere in this module's API.
//!
//! Mutations path-copy: each touched directory is rebuilt (its unchanged
//! nodes dedup away in the store) and its parent re-pointed, up to a new
//! root hash. Untouched siblings keep their subtree hashes — structural
//! sharing and O(depth) cost per edit.

use crate::hash::Hash;
use crate::prolly::{Content, Dir, Entry, EntryKind, FormatError, NodeStore};

/// Resolve a path to an entry. An empty path names no entry (the root
/// directory is not an entry).
pub fn lookup(
    store: &impl NodeStore,
    root: &Hash,
    path: &[&[u8]],
) -> Result<Option<Entry>, FormatError> {
    let Some((last, parents)) = path.split_last() else {
        return Ok(None);
    };
    let mut cur = *root;
    for comp in parents {
        let dir = Dir::load(store, &cur)?;
        match dir.get(comp) {
            Some(Entry { content: Content::DirRoot(h), .. }) => cur = *h,
            Some(_) => return Err(FormatError::BadEntry("not a directory")),
            None => return Ok(None),
        }
    }
    Ok(Dir::load(store, &cur)?.get(last).cloned())
}

/// Insert or replace `entry` in the directory at `dir_path`, creating
/// missing intermediate directories (with `mkdir_mtime`). Returns the new
/// root hash.
pub fn put(
    store: &mut impl NodeStore,
    root: &Hash,
    dir_path: &[&[u8]],
    entry: Entry,
    mkdir_mtime: u64,
) -> Result<Hash, FormatError> {
    let mut dir = Dir::load(store, root)?;
    let Some((first, rest)) = dir_path.split_first() else {
        dir.upsert(entry)?;
        return Ok(dir.save(store));
    };
    let child_root = match dir.get(first) {
        Some(Entry { content: Content::DirRoot(h), .. }) => *h,
        Some(_) => return Err(FormatError::BadEntry("not a directory")),
        None => Dir::new().save(store),
    };
    let new_child_root = put(store, &child_root, rest, entry, mkdir_mtime)?;
    let child_entry = match dir.get(first) {
        Some(e) => Entry { content: Content::DirRoot(new_child_root), ..e.clone() },
        None => Entry {
            name: first.to_vec(),
            kind: EntryKind::Dir,
            flags: 0,
            size: 0,
            mtime: mkdir_mtime,
            content: Content::DirRoot(new_child_root),
        },
    };
    dir.upsert(child_entry)?;
    Ok(dir.save(store))
}

/// Remove the entry at `path`. Returns the new root hash and the removed
/// entry; a missing path is a no-op returning the old root.
pub fn remove(
    store: &mut impl NodeStore,
    root: &Hash,
    path: &[&[u8]],
) -> Result<(Hash, Option<Entry>), FormatError> {
    let Some((first, rest)) = path.split_first() else {
        return Ok((*root, None));
    };
    let mut dir = Dir::load(store, root)?;
    if rest.is_empty() {
        let removed = dir.remove(first);
        let new_root = if removed.is_some() { dir.save(store) } else { *root };
        return Ok((new_root, removed));
    }
    match dir.get(first) {
        Some(Entry { content: Content::DirRoot(h), .. }) => {
            let child_root = *h;
            let (new_child_root, removed) = remove(store, &child_root, rest)?;
            if removed.is_none() {
                return Ok((*root, None));
            }
            let child_entry = dir.get(first).cloned().map(|e| Entry {
                content: Content::DirRoot(new_child_root),
                ..e
            });
            dir.upsert(child_entry.expect("child entry exists"))?;
            Ok((dir.save(store), removed))
        }
        Some(_) => Err(FormatError::BadEntry("not a directory")),
        None => Ok((*root, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkerParams;
    use crate::file::make_file_entry;
    use crate::prolly::MemStore;
    use proptest::prelude::*;

    const TEST_PARAMS: ChunkerParams = ChunkerParams { min: 64, avg: 256, max: 1024 };

    fn empty_root(store: &mut MemStore) -> Hash {
        Dir::new().save(store)
    }

    #[test]
    fn put_lookup_remove_nested() {
        let mut store = MemStore::new();
        let root = empty_root(&mut store);
        let entry = make_file_entry(&mut store, &TEST_PARAMS, b"config", b"hello", 7, 0);
        let root = put(&mut store, &root, &[b"etc", b"app"], entry.clone(), 7).unwrap();

        let found = lookup(&store, &root, &[b"etc", b"app", b"config"]).unwrap();
        assert_eq!(found, Some(entry));
        assert_eq!(lookup(&store, &root, &[b"etc", b"missing"]).unwrap(), None);
        assert_eq!(lookup(&store, &root, &[b"nope", b"x", b"y"]).unwrap(), None);

        let (root, removed) = remove(&mut store, &root, &[b"etc", b"app", b"config"]).unwrap();
        assert!(removed.is_some());
        assert_eq!(lookup(&store, &root, &[b"etc", b"app", b"config"]).unwrap(), None);
        // Parent dirs survive removal of their last entry (no auto-prune).
        assert!(lookup(&store, &root, &[b"etc", b"app"]).unwrap().is_some());
    }

    #[test]
    fn sibling_subtrees_share_on_edit() {
        let mut store = MemStore::new();
        let root = empty_root(&mut store);
        let f1 = make_file_entry(&mut store, &TEST_PARAMS, b"x", b"one", 1, 0);
        let f2 = make_file_entry(&mut store, &TEST_PARAMS, b"y", b"two", 1, 0);
        let root = put(&mut store, &root, &[b"a"], f1, 1).unwrap();
        let root = put(&mut store, &root, &[b"b"], f2, 1).unwrap();
        let b_before = lookup(&store, &root, &[b"b"]).unwrap().unwrap();

        let f1b = make_file_entry(&mut store, &TEST_PARAMS, b"x", b"changed", 2, 0);
        let root2 = put(&mut store, &root, &[b"a"], f1b, 2).unwrap();
        assert_ne!(root, root2);
        // The untouched sibling's subtree hash is byte-identical.
        let b_after = lookup(&store, &root2, &[b"b"]).unwrap().unwrap();
        assert_eq!(b_before, b_after);
    }

    proptest! {
        /// Canonical form across the nested structure: building the same
        /// set of files in any order yields the same root hash.
        #[test]
        fn nested_build_order_independent(
            files in proptest::collection::btree_map(
                (
                    proptest::collection::vec(
                        proptest::sample::select(vec![b"d1".to_vec(), b"d2".to_vec(), b"d3".to_vec()]),
                        0..3,
                    ),
                    proptest::sample::select(vec![b"f1".to_vec(), b"f2".to_vec(), b"f3".to_vec()]),
                ),
                proptest::collection::vec(any::<u8>(), 0..1024),
                1..12,
            ),
            seed in any::<u64>(),
        ) {
            let files: Vec<_> = files.into_iter().collect();
            let shuffled = {
                let mut v = files.clone();
                let mut s = seed;
                for i in (1..v.len()).rev() {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    v.swap(i, (s >> 33) as usize % (i + 1));
                }
                v
            };

            let build = |order: &[((Vec<Vec<u8>>, Vec<u8>), Vec<u8>)]| {
                let mut store = MemStore::new();
                let mut root = empty_root(&mut store);
                for ((dirs, name), content) in order {
                    let entry = make_file_entry(&mut store, &TEST_PARAMS, name, content, 1, 0);
                    let dir_path: Vec<&[u8]> = dirs.iter().map(|d| d.as_slice()).collect();
                    root = put(&mut store, &root, &dir_path, entry, 1).unwrap();
                }
                root
            };

            prop_assert_eq!(build(&files), build(&shuffled));
        }
    }
}
