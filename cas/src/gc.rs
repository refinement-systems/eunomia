//! GC mark phase (spec rev0§4.6): the reachability walk from a tree root.
//!
//! Mark state is an exact in-memory hash set — the MVP bet that mark time
//! ≪ server uptime. The walk reads only directory nodes and chunk-list
//! objects; file chunk *data* is never fetched (its hashes come from the
//! chunk list), which is what keeps the mark phase gentle. Already-marked
//! subtrees are pruned, so structural sharing makes the walk cheap across
//! snapshot families.
//!
//! Sweep policy (which entries to condemn, the birth-generation epoch
//! check, extent accounting) lives in `store::Store::gc` — this module is
//! pure tree traversal.

use crate::file::chunk_list_entries;
use crate::hash::Hash;
use crate::prolly::{parse_node, Content, FormatError, NodeRefs, NodeStore};
use alloc::collections::BTreeSet;

/// Insert `root` and every object reachable from it (directory nodes,
/// chunk-list objects, content chunks) into `live`.
pub fn mark(
    store: &impl NodeStore,
    root: &Hash,
    live: &mut BTreeSet<Hash>,
) -> Result<(), FormatError> {
    if !live.insert(*root) {
        return Ok(()); // shared subtree, already walked
    }
    let bytes = store.get(root).ok_or(FormatError::MissingNode(*root))?;
    match parse_node(&bytes)? {
        NodeRefs::Children(children) => {
            for child in children {
                mark(store, &child, live)?;
            }
        }
        NodeRefs::Entries(entries) => {
            for e in entries {
                match e.content {
                    Content::Inline(_) => {}
                    Content::ChunkList(h) => {
                        if live.insert(h) {
                            let list =
                                store.get(&h).ok_or(FormatError::MissingNode(h))?;
                            for (chunk, _) in chunk_list_entries(&list)? {
                                live.insert(chunk);
                            }
                        }
                    }
                    Content::DirRoot(h) => mark(store, &h, live)?,
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkerParams;
    use crate::file::{make_file_entry, read_file};
    use crate::prolly::{Dir, MemStore};
    use crate::tree;

    const TEST_PARAMS: ChunkerParams = ChunkerParams { min: 64, avg: 256, max: 1024 };

    /// Serves only marked objects — reads through it succeed exactly when
    /// the mark set is sufficient.
    struct LiveOnly<'a> {
        inner: &'a MemStore,
        live: &'a BTreeSet<Hash>,
    }

    impl NodeStore for LiveOnly<'_> {
        fn put(&mut self, _bytes: &[u8]) -> Hash {
            unreachable!("read-only")
        }

        fn get(&self, hash: &Hash) -> Option<alloc::vec::Vec<u8>> {
            self.live.contains(hash).then(|| self.inner.get(hash)).flatten()
        }
    }

    #[test]
    fn mark_set_is_sufficient_to_read_everything() {
        let mut store = MemStore::new();
        let empty_root = Dir::new().save(&mut store);
        // Inline file, chunked file, nested directory.
        let small = make_file_entry(&mut store, &TEST_PARAMS, b"small", b"tiny", 1, 0);
        let big_data: Vec<u8> = (0..40_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let big = make_file_entry(&mut store, &TEST_PARAMS, b"big", &big_data, 1, 0);
        let root = tree::put(&mut store, &empty_root, &[], small, 1).unwrap();
        let root = tree::put(&mut store, &root, &[b"deep", b"er"], big, 1).unwrap();

        let mut live = BTreeSet::new();
        mark(&store, &root, &mut live).unwrap();

        // The incremental build left superseded roots behind; the walk
        // must not drag them in...
        assert!(live.len() < store.len());
        // ...while everything reachable stays readable through the mark
        // set alone.
        let filtered = LiveOnly { inner: &store, live: &live };
        let e = tree::lookup(&filtered, &root, &[b"small"]).unwrap().unwrap();
        assert_eq!(read_file(&filtered, &e.content, e.size).unwrap(), b"tiny");
        let e = tree::lookup(&filtered, &root, &[b"deep", b"er", b"big"]).unwrap().unwrap();
        assert_eq!(read_file(&filtered, &e.content, e.size).unwrap(), big_data);
    }
}
