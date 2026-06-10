//! Nested per-directory prolly trees (Merkle search trees) — spec §4.1, §4.9.
//!
//! Key properties to preserve:
//!   - History-independent (canonical): same logical contents → same root hash
//!     regardless of edit order. Proptest target: same_content_same_root.
//!   - Structural sharing across snapshots (equal subtree hashes → skip diff).
//!   - Directory moves O(depth); subtree diff O(changes × depth).
//!
//! Entry encoding: deterministic TLV, sorted by tag (spec §4.9).
//! Entry schema:
//!   name → (type: file|dir, flags, size, mtime, content: inline|chunk-list|dir-root)

use crate::hash::Hash;

/// A directory entry as stored in the tree.
#[derive(Debug)]
pub struct Entry {
    pub name: Vec<u8>,
    pub kind: EntryKind,
    pub flags: u32,
    pub size: u64,
    pub mtime: u64,
    pub content: Content,
}

#[derive(Debug)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug)]
pub enum Content {
    /// ≤ 512 bytes, inlined (spec §4.9).
    Inline(Vec<u8>),
    /// chunk-list hash (file ≥ 512 bytes).
    ChunkList(Hash),
    /// Child directory root hash.
    DirRoot(Hash),
}

/// An in-memory node of the prolly tree.
pub struct Node {
    pub entries: Vec<Entry>,
}

impl Node {
    pub fn new() -> Self {
        Node { entries: Vec::new() }
    }

    /// Insert or update an entry; maintains sorted order by name (memcmp).
    pub fn upsert(&mut self, _entry: Entry) {
        todo!("M2: insert into sorted entries, maintain prolly split invariant")
    }

    /// Remove an entry by name.
    pub fn remove(&mut self, _name: &[u8]) -> Option<Entry> {
        todo!("M2: remove and rebalance")
    }

    /// Compute the canonical hash of this node.
    pub fn hash(&self) -> Hash {
        todo!("M2: deterministic TLV serialise then BLAKE3")
    }
}
