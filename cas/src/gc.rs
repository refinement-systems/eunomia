//! GC mark phase (spec rev1§4.6): the reachability walk from a tree root.
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
//!
//! Bound (rev1§4.8 "detect on read, never fault"): the walk is an explicit
//! heap work-stack of nodes-to-parse, not native recursion, so native stack
//! depth is **O(1)** regardless of tree depth/width. Nodes are marked
//! *on push*, so each distinct reachable parse-node is pushed at most once
//! and total work is bounded by the distinct reachable set — a deep or wide
//! tree completes (its legitimate cost) instead of overflowing the stack.
//! Content-addressing forbids true cycles (a node's hash depends on its
//! children's), so depth/width are the only adversarial axes, and the
//! work-stack + mark-on-push dedup absorb both. A malformed node yields a
//! clean `FormatError` (`parse_node`/`chunk_list_entries` are total), never
//! a fault. The mark walk is a host cargo-fuzz target (`gc_mark`, rev1§6);
//! its driver `check_recipe` lives here so the target and its corpus-replay
//! test share one sufficiency oracle.

use crate::chunk::ChunkerParams;
use crate::file::{chunk_list_entries, make_file_entry, read_file};
use crate::hash::Hash;
use crate::prolly::{
    parse_node, Content, Dir, Entry, EntryKind, FormatError, MemStore, NodeRefs, NodeStore,
    INLINE_MAX,
};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

/// Insert `root` and every object reachable from it (directory nodes,
/// chunk-list objects, content chunks) into `live`.
///
/// An explicit heap work-stack (not native recursion): native stack depth is
/// O(1) and total work is bounded by the distinct reachable set (mark-on-push
/// ⟹ each parse-node pushed once). See the module doc for the structural bound.
pub fn mark(
    store: &impl NodeStore,
    root: &Hash,
    live: &mut BTreeSet<Hash>,
) -> Result<(), FormatError> {
    let mut stack: Vec<Hash> = Vec::new();
    if live.insert(*root) {
        stack.push(*root);
    }
    while let Some(h) = stack.pop() {
        let bytes = store.get(&h).ok_or(FormatError::MissingNode(h))?;
        match parse_node(&bytes)? {
            NodeRefs::Children(children) => {
                for c in children {
                    if live.insert(c) {
                        stack.push(c); // internal node: parse later
                    }
                }
            }
            NodeRefs::Entries(entries) => {
                for e in entries {
                    match e.content {
                        Content::Inline(_) => {}
                        Content::ChunkList(ch) => {
                            if live.insert(ch) {
                                let list =
                                    store.get(&ch).ok_or(FormatError::MissingNode(ch))?;
                                for (chunk, _) in chunk_list_entries(&list)? {
                                    live.insert(chunk); // chunk leaves: mark, never parse
                                }
                            }
                        }
                        Content::DirRoot(dr) => {
                            if live.insert(dr) {
                                stack.push(dr); // nested directory root: parse later
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ── GC fuzz/test driver (rev1§6: the mark walk is a host cargo-fuzz target) ──
//
// `check_recipe` is the single oracle driven by both the `gc_mark` cargo-fuzz
// target (`cas/fuzz`, built with the `fuzzing` feature) and its corpus-replay
// test (`cas/tests/fuzz_corpus.rs`, built against the default-feature lib and
// Miri-replayed). Integration tests link the library compiled normally — they
// see neither `#[cfg(test)]` nor `fuzzing`-gated items — so the shared driver
// is ungated `pub`, matching the host-test posture of `mark`/`parse_node`/
// `MemStore`. The strengthened sufficiency proptest (B6C) reuses the same
// `LiveOnly`/`walk_collect` helpers via the child `tests` module.

/// Chunker params for recipe-built chunked files (small, fuzz-friendly).
const RECIPE_PARAMS: ChunkerParams = ChunkerParams {
    min: 64,
    avg: 256,
    max: 1024,
};

/// Read one recipe byte (saturating to 0 past the end), always advancing.
fn next_byte(data: &[u8], i: &mut usize) -> u8 {
    let b = data.get(*i).copied().unwrap_or(0);
    *i += 1;
    b
}

/// A unique, always-valid entry name (printable ASCII; never 0/'/'/"."/"..").
fn name_for(k: usize) -> Vec<u8> {
    let mut name = vec![b'e'];
    let mut v = k;
    loop {
        name.push(b'0' + (v % 10) as u8);
        v /= 10;
        if v == 0 {
            break;
        }
    }
    name
}

/// Build a `MemStore` from `data` interpreted as a node-spec recipe, returning
/// the store and the last-built node (the GC root to mark), or `None` for an
/// empty recipe.
///
/// The recipe builds **well-formed** nodes via the real encoders (malformed
/// single nodes are covered by the `tree_node` target); structural hostility —
/// deep chains, wide fanout, shared subtrees, dangling references — comes from
/// how commands reference earlier-built nodes. `data` is a stream of 1-byte
/// commands (`op = byte % 6`); each builds one directory node and pushes its
/// hash onto `built`, with later references taken modulo `built.len()` (always
/// a DAG; content-addressing guarantees acyclicity). Total over all inputs —
/// every length/index read is saturating/modular, so it never panics.
pub(crate) fn build_recipe(data: &[u8]) -> (MemStore, Option<Hash>) {
    let mut store = MemStore::new();
    let mut built: Vec<Hash> = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let op = data[i];
        i += 1;
        match op % 6 {
            // Inline-file leaf.
            0 => {
                let n = (next_byte(data, &mut i) as usize) % 8;
                let mut content = Vec::new();
                for _ in 0..n {
                    content.push(next_byte(data, &mut i));
                }
                let mut dir = Dir::new();
                let _ = dir.upsert(Entry {
                    name: b"f".to_vec(),
                    kind: EntryKind::File,
                    flags: 0,
                    size: content.len() as u64,
                    mtime: 1,
                    content: Content::Inline(content),
                });
                built.push(dir.save(&mut store));
            }
            // Dir-root leaf (chain link): points at the last-built node, so a
            // run of these builds a deep `DirRoot` chain.
            1 => {
                let child = built
                    .last()
                    .copied()
                    .unwrap_or_else(|| Dir::new().save(&mut store));
                let mut dir = Dir::new();
                let _ = dir.upsert(Entry {
                    name: b"d".to_vec(),
                    kind: EntryKind::Dir,
                    flags: 0,
                    size: 0,
                    mtime: 1,
                    content: Content::DirRoot(child),
                });
                built.push(dir.save(&mut store));
            }
            // Wide node: several dir-root entries → fanout + shared subtrees.
            2 => {
                let fanout = (next_byte(data, &mut i) as usize) % 16 + 1;
                let mut dir = Dir::new();
                for k in 0..fanout {
                    let child = if built.is_empty() {
                        Dir::new().save(&mut store)
                    } else {
                        built[next_byte(data, &mut i) as usize % built.len()]
                    };
                    let _ = dir.upsert(Entry {
                        name: name_for(k),
                        kind: EntryKind::Dir,
                        flags: 0,
                        size: 0,
                        mtime: 1,
                        content: Content::DirRoot(child),
                    });
                }
                built.push(dir.save(&mut store));
            }
            // Chunked-file leaf: content > INLINE_MAX forces a chunk list.
            3 => {
                let len = INLINE_MAX + 1 + (next_byte(data, &mut i) as usize % 64);
                let blob: Vec<u8> = (0..len).map(|x| (x as u8) ^ op).collect();
                let entry = make_file_entry(&mut store, &RECIPE_PARAMS, b"big", &blob, 1, 0);
                let mut dir = Dir::new();
                let _ = dir.upsert(entry);
                built.push(dir.save(&mut store));
            }
            // Dangling reference: a `DirRoot` at a hash never stored → `mark`
            // must refuse with `MissingNode`, not fault.
            4 => {
                let sentinel = Hash::of(&[0xDE, 0xAD, op, built.len() as u8]);
                let mut dir = Dir::new();
                let _ = dir.upsert(Entry {
                    name: b"x".to_vec(),
                    kind: EntryKind::Dir,
                    flags: 0,
                    size: 0,
                    mtime: 1,
                    content: Content::DirRoot(sentinel),
                });
                built.push(dir.save(&mut store));
            }
            // Mixed leaf: an inline entry plus a dir-root entry — a leaf node
            // with entries of differing content.
            _ => {
                let child = built
                    .last()
                    .copied()
                    .unwrap_or_else(|| Dir::new().save(&mut store));
                let mut dir = Dir::new();
                let _ = dir.upsert(Entry {
                    name: b"a".to_vec(),
                    kind: EntryKind::File,
                    flags: 0,
                    size: 1,
                    mtime: 1,
                    content: Content::Inline(vec![next_byte(data, &mut i)]),
                });
                let _ = dir.upsert(Entry {
                    name: b"b".to_vec(),
                    kind: EntryKind::Dir,
                    flags: 0,
                    size: 0,
                    mtime: 1,
                    content: Content::DirRoot(child),
                });
                built.push(dir.save(&mut store));
            }
        }
        if built.len() >= 1 << 16 {
            break; // bound the store for the fuzzer
        }
    }
    (store, built.last().copied())
}

/// Serves only marked objects — reads through it succeed exactly when the mark
/// set is sufficient. The sufficiency oracle for the `gc_mark` fuzz target, the
/// corpus replay, and (B6C) the strengthened proptest.
pub(crate) struct LiveOnly<'a> {
    pub(crate) inner: &'a MemStore,
    pub(crate) live: &'a BTreeSet<Hash>,
}

impl NodeStore for LiveOnly<'_> {
    fn put(&mut self, _bytes: &[u8]) -> Hash {
        unreachable!("read-only")
    }

    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.live
            .contains(hash)
            .then(|| self.inner.get(hash))
            .flatten()
    }
}

/// Read every reachable entry, returning a deterministic map of path → file
/// content (or the read error). Iterative across nested directories, so the
/// oracle itself cannot overflow on a deep tree. Comparing this over the full
/// store against the same over a `LiveOnly` view is the sufficiency oracle:
/// equal ⟹ the mark set serves every reachable read identically.
pub(crate) fn walk_collect(
    store: &impl NodeStore,
    root: &Hash,
) -> Result<BTreeMap<Vec<Vec<u8>>, Result<Vec<u8>, FormatError>>, FormatError> {
    let mut out: BTreeMap<Vec<Vec<u8>>, Result<Vec<u8>, FormatError>> = BTreeMap::new();
    let mut stack: Vec<(Hash, Vec<Vec<u8>>)> = vec![(*root, Vec::new())];
    while let Some((dir_hash, prefix)) = stack.pop() {
        let dir = Dir::load(store, &dir_hash)?;
        for e in dir.iter() {
            let mut path = prefix.clone();
            path.push(e.name.clone());
            match &e.content {
                Content::DirRoot(h) => stack.push((*h, path)),
                _ => {
                    out.insert(path, read_file(store, &e.content, e.size));
                }
            }
        }
    }
    Ok(out)
}

/// Run the GC mark walk over a recipe-built store and check it (rev1§4.6/§6):
/// the walk must never panic or overflow; on `Err` it refused cleanly (a
/// dangling reference → `MissingNode`); on `Ok` the mark set must be
/// **sufficient** — every reachable read through the mark set alone matches
/// reading through the full store. Returns `Err` describing a sufficiency
/// violation so callers (`unwrap`) surface it as a crash; never asserts itself.
pub fn check_recipe(data: &[u8]) -> Result<(), &'static str> {
    let (store, root) = build_recipe(data);
    let Some(root) = root else {
        return Ok(()); // empty recipe: nothing to mark
    };
    let mut live = BTreeSet::new();
    if mark(&store, &root, &mut live).is_err() {
        return Ok(()); // refuse-not-fault: a clean FormatError
    }
    let full = walk_collect(&store, &root);
    let view = LiveOnly {
        inner: &store,
        live: &live,
    };
    if full == walk_collect(&view, &root) {
        Ok(())
    } else {
        Err("mark set insufficient: read-through-LiveOnly diverged from the full store")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree;

    const TEST_PARAMS: ChunkerParams = ChunkerParams {
        min: 64,
        avg: 256,
        max: 1024,
    };

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
        let filtered = LiveOnly {
            inner: &store,
            live: &live,
        };
        let e = tree::lookup(&filtered, &root, &[b"small"])
            .unwrap()
            .unwrap();
        assert_eq!(read_file(&filtered, &e.content, e.size).unwrap(), b"tiny");
        let e = tree::lookup(&filtered, &root, &[b"deep", b"er", b"big"])
            .unwrap()
            .unwrap();
        assert_eq!(read_file(&filtered, &e.content, e.size).unwrap(), big_data);
    }

    /// A `DirRoot` chain deep enough that the pre-B6B native recursion
    /// overflowed the stack; the heap work-stack makes native depth O(1), so
    /// it completes (rev1§4.8 refuse/complete, never fault). Miri runs a small
    /// depth (the path, not the scale, is what it UB-checks).
    #[test]
    fn deep_dir_root_chain_does_not_overflow() {
        let mut store = MemStore::new();
        let depth: usize = if cfg!(miri) { 64 } else { 100_000 };
        let mut root = Dir::new().save(&mut store);
        for _ in 0..depth {
            let mut d = Dir::new();
            d.upsert(Entry {
                name: b"c".to_vec(),
                kind: EntryKind::Dir,
                flags: 0,
                size: 0,
                mtime: 1,
                content: Content::DirRoot(root),
            })
            .unwrap();
            root = d.save(&mut store);
        }
        let mut live = BTreeSet::new();
        mark(&store, &root, &mut live).unwrap();
        // Every distinct dir node is marked: the empty leaf + `depth` wrappers.
        assert_eq!(live.len(), depth + 1);
    }

    /// The recipe driver accepts arbitrary bytes without panicking, and a
    /// dangling reference is refused (not faulted) while a valid chain is
    /// marked sufficiently.
    #[test]
    fn check_recipe_handles_recipes() {
        check_recipe(&[]).unwrap();
        check_recipe(&[1u8; 300]).unwrap(); // deep chain → Ok, sufficient
        check_recipe(&[0, 4]).unwrap(); // dangling ref → refused cleanly
        // A byte sweep must never panic or report insufficiency.
        for b in 0u8..=255 {
            check_recipe(&[b, b, b, b, b]).unwrap();
        }
    }
}
