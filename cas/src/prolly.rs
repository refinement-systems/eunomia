//! Per-directory prolly trees (Merkle search trees) — spec §4.1, §4.9.
//!
//! A directory is a sorted sequence of entries, split into content-addressed
//! nodes by a content-defined rule, so tree shape is history-independent
//! (canonical): the same logical contents always produce the same root hash
//! regardless of edit order.
//!
//! Format constants (changing any of these is a format migration):
//!   - Entry encoding: deterministic TLV, little-endian, exactly one
//!     encoding per logical entry (§4.9).
//!   - Split rule: an item is a node boundary iff the low SPLIT_BITS bits
//!     of BLAKE3(item bytes) are zero (average fanout 2^SPLIT_BITS), with
//!     a forced boundary every MAX_NODE_ENTRIES items. The split decision
//!     is a pure per-item function, so an edit perturbs only the node
//!     holding the edited entry (plus the spine above it) — per-item
//!     hashing self-synchronizes immediately, unlike a rolling window.
//!   - Node encoding: [level u8][count u32][items…]; leaf items are
//!     entries, internal items are (first key of child, child root hash).
//!
//! Incremental node-level surgery is deliberately absent: `Dir::save`
//! rebuilds one directory's node tree from its full entry list. Editing a
//! file path-copies only the directories on its path (§4.3), and per-node
//! dedup in the store makes the rebuild emit only changed nodes. Node-level
//! incremental update is an optimization for very large directories,
//! deferred past MVP.
//!
//! Decoders here are strict (they are cargo-fuzz targets, §3.7/§6): every
//! canonicality rule checked on encode is also rejected on decode, and
//! trailing bytes are errors.

use crate::hash::Hash;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

/// Files at or below this size live inline in the directory entry (§4.9).
/// The rule is a pure function of content, preserving canonical form.
pub const INLINE_MAX: usize = 512;

/// Hard cap on optional-TLV bytes per entry (§4.9) — keeps directory nodes
/// directory-shaped regardless of future tags.
pub const MAX_OPT_BYTES: usize = 4096;

/// Advisory-executable bit in the flags word — a type hint with zero
/// security semantics (§4.9).
pub const FLAG_EXECUTABLE: u32 = 1 << 0;

const SPLIT_BITS: u32 = 5;
const SPLIT_MASK: u64 = (1 << SPLIT_BITS) - 1;
const MAX_NODE_ENTRIES: usize = 128;

const OPT_TAG_FLAGS: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub kind: EntryKind,
    pub flags: u32,
    pub size: u64,
    pub mtime: u64,
    pub content: Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// ≤ INLINE_MAX bytes, inlined (§4.9).
    Inline(Vec<u8>),
    /// Chunk-list object hash (file > INLINE_MAX bytes).
    ChunkList(Hash),
    /// Child directory root hash.
    DirRoot(Hash),
}

#[derive(Debug, PartialEq, Eq)]
pub enum FormatError {
    BadName,
    BadEntry(&'static str),
    BadNode(&'static str),
    MissingNode(Hash),
    NotSorted,
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FormatError::BadName => write!(f, "invalid entry name"),
            FormatError::BadEntry(why) => write!(f, "invalid entry: {why}"),
            FormatError::BadNode(why) => write!(f, "invalid node: {why}"),
            FormatError::MissingNode(h) => write!(f, "missing object {h:?}"),
            FormatError::NotSorted => write!(f, "entries not strictly sorted"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FormatError {}

/// Content-addressed object store: chunks, tree nodes, and chunk lists all
/// live in one keyspace (hash = address, §4.1).
pub trait NodeStore {
    fn put(&mut self, bytes: &[u8]) -> Hash;
    fn get(&self, hash: &Hash) -> Option<Vec<u8>>;
}

/// In-memory store for tests and host-side tooling.
#[derive(Default)]
pub struct MemStore {
    objects: BTreeMap<Hash, Vec<u8>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

impl NodeStore for MemStore {
    fn put(&mut self, bytes: &[u8]) -> Hash {
        let hash = Hash::of(bytes);
        self.objects.entry(hash).or_insert_with(|| bytes.to_vec());
        hash
    }

    fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
        self.objects.get(hash).cloned()
    }
}

// ── Entry encoding ──────────────────────────────────────────────────────

pub fn validate_name(name: &[u8]) -> Result<(), FormatError> {
    let ok = (1..=255).contains(&name.len())
        && !name.iter().any(|&b| b == 0 || b == 0x2F)
        && name != b"."
        && name != b"..";
    if ok { Ok(()) } else { Err(FormatError::BadName) }
}

fn validate_entry(e: &Entry) -> Result<(), FormatError> {
    validate_name(&e.name)?;
    match (&e.kind, &e.content) {
        (EntryKind::File, Content::Inline(bytes)) => {
            if bytes.len() > INLINE_MAX {
                return Err(FormatError::BadEntry("inline content too large"));
            }
            if e.size != bytes.len() as u64 {
                return Err(FormatError::BadEntry("inline size mismatch"));
            }
        }
        (EntryKind::File, Content::ChunkList(_)) => {
            // Small content must be inline — one encoding per logical entry.
            if e.size <= INLINE_MAX as u64 {
                return Err(FormatError::BadEntry("small file must be inline"));
            }
        }
        (EntryKind::Dir, Content::DirRoot(_)) => {
            if e.size != 0 {
                return Err(FormatError::BadEntry("dir size must be 0"));
            }
        }
        _ => return Err(FormatError::BadEntry("kind/content mismatch")),
    }
    Ok(())
}

fn encode_entry(e: &Entry, out: &mut Vec<u8>) {
    out.push(e.name.len() as u8);
    out.extend_from_slice(&e.name);
    out.push(match e.kind {
        EntryKind::File => 0,
        EntryKind::Dir => 1,
    });
    out.extend_from_slice(&e.size.to_le_bytes());
    out.extend_from_slice(&e.mtime.to_le_bytes());
    match &e.content {
        Content::Inline(bytes) => {
            out.push(0);
            out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Content::ChunkList(h) => {
            out.push(1);
            out.extend_from_slice(h.as_bytes());
        }
        Content::DirRoot(h) => {
            out.push(2);
            out.extend_from_slice(h.as_bytes());
        }
    }
    // Optional TLV section: absent fields contribute zero bytes (§4.9).
    let mut opt = Vec::new();
    if e.flags != 0 {
        opt.push(OPT_TAG_FLAGS);
        opt.extend_from_slice(&4u16.to_le_bytes());
        opt.extend_from_slice(&e.flags.to_le_bytes());
    }
    debug_assert!(opt.len() <= MAX_OPT_BYTES);
    out.extend_from_slice(&(opt.len() as u16).to_le_bytes());
    out.extend_from_slice(&opt);
}

pub(crate) struct Reader<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(FormatError::BadNode("truncated"))?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn hash(&mut self) -> Result<Hash, FormatError> {
        Ok(Hash::from_bytes(self.take(32)?.try_into().unwrap()))
    }

    pub(crate) fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

fn decode_entry(r: &mut Reader) -> Result<Entry, FormatError> {
    let name_len = r.u8()? as usize;
    let name = r.take(name_len)?.to_vec();
    let kind = match r.u8()? {
        0 => EntryKind::File,
        1 => EntryKind::Dir,
        _ => return Err(FormatError::BadEntry("bad kind")),
    };
    let size = r.u64()?;
    let mtime = r.u64()?;
    let content = match r.u8()? {
        0 => {
            let len = r.u16()? as usize;
            Content::Inline(r.take(len)?.to_vec())
        }
        1 => Content::ChunkList(r.hash()?),
        2 => Content::DirRoot(r.hash()?),
        _ => return Err(FormatError::BadEntry("bad content tag")),
    };
    let opt_len = r.u16()? as usize;
    if opt_len > MAX_OPT_BYTES {
        return Err(FormatError::BadEntry("optional section too large"));
    }
    let mut opt = Reader { buf: r.take(opt_len)?, pos: 0 };
    let mut flags = 0u32;
    let mut last_tag: i16 = -1;
    while !opt.done() {
        let tag = opt.u8()?;
        if i16::from(tag) <= last_tag {
            return Err(FormatError::BadEntry("optional tags not strictly ascending"));
        }
        last_tag = i16::from(tag);
        let len = opt.u16()? as usize;
        let value = opt.take(len)?;
        match tag {
            OPT_TAG_FLAGS => {
                if len != 4 {
                    return Err(FormatError::BadEntry("bad flags length"));
                }
                flags = u32::from_le_bytes(value.try_into().unwrap());
                if flags == 0 {
                    // Zero flags must be encoded as absence — canonical form.
                    return Err(FormatError::BadEntry("zero flags must be absent"));
                }
            }
            // Format version 0 defines exactly one optional tag. Accepting
            // and dropping unknown tags would silently rewrite newer-format
            // entries, so reject instead.
            _ => return Err(FormatError::BadEntry("unknown optional tag")),
        }
    }
    let entry = Entry { name, kind, flags, size, mtime, content };
    validate_entry(&entry)?;
    Ok(entry)
}

// ── Node building ───────────────────────────────────────────────────────

fn is_boundary(item_bytes: &[u8]) -> bool {
    let h = Hash::of(item_bytes);
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) & SPLIT_MASK == 0
}

fn encode_internal_item(key: &[u8], child: &Hash, out: &mut Vec<u8>) {
    out.push(key.len() as u8);
    out.extend_from_slice(key);
    out.extend_from_slice(child.as_bytes());
}

/// Split one level's items into nodes and store them.
/// `items` is (first key under item, encoded item bytes).
fn build_level(
    store: &mut impl NodeStore,
    level: u8,
    items: &[(Vec<u8>, Vec<u8>)],
) -> Vec<(Vec<u8>, Hash)> {
    let mut out = Vec::new();
    let mut node_start = 0;
    let mut count_in_node = 0;
    for (i, (_, bytes)) in items.iter().enumerate() {
        count_in_node += 1;
        if is_boundary(bytes) || count_in_node == MAX_NODE_ENTRIES || i + 1 == items.len() {
            let node_items = &items[node_start..=i];
            let mut node = vec![level];
            node.extend_from_slice(&(node_items.len() as u32).to_le_bytes());
            for (_, b) in node_items {
                node.extend_from_slice(b);
            }
            let hash = store.put(&node);
            out.push((node_items[0].0.clone(), hash));
            node_start = i + 1;
            count_in_node = 0;
        }
    }
    out
}

// ── Directory ───────────────────────────────────────────────────────────

/// In-memory logical directory: name → entry, memcmp order (§4.9).
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    entries: BTreeMap<Vec<u8>, Entry>,
}

impl Dir {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, entry: Entry) -> Result<(), FormatError> {
        validate_entry(&entry)?;
        self.entries.insert(entry.name.clone(), entry);
        Ok(())
    }

    pub fn remove(&mut self, name: &[u8]) -> Option<Entry> {
        self.entries.remove(name)
    }

    pub fn get(&self, name: &[u8]) -> Option<&Entry> {
        self.entries.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Build the canonical node tree and return the root hash.
    pub fn save(&self, store: &mut impl NodeStore) -> Hash {
        if self.entries.is_empty() {
            let node = [&[0u8][..], &0u32.to_le_bytes()].concat();
            return store.put(&node);
        }
        let items: Vec<(Vec<u8>, Vec<u8>)> = self
            .entries
            .values()
            .map(|e| {
                let mut bytes = Vec::new();
                encode_entry(e, &mut bytes);
                (e.name.clone(), bytes)
            })
            .collect();
        let mut level = 0u8;
        let mut nodes = build_level(store, level, &items);
        while nodes.len() > 1 {
            level = level.checked_add(1).expect("tree deeper than 255 levels");
            let items: Vec<(Vec<u8>, Vec<u8>)> = nodes
                .iter()
                .map(|(key, hash)| {
                    let mut bytes = Vec::new();
                    encode_internal_item(key, hash, &mut bytes);
                    (key.clone(), bytes)
                })
                .collect();
            nodes = build_level(store, level, &items);
        }
        nodes[0].1
    }

    /// Load and validate a directory from its root hash.
    pub fn load(store: &impl NodeStore, root: &Hash) -> Result<Dir, FormatError> {
        let mut entries_vec: Vec<Entry> = Vec::new();
        load_node(store, root, None, &mut entries_vec)?;
        let mut entries = BTreeMap::new();
        let mut prev: Option<&[u8]> = None;
        for e in &entries_vec {
            if let Some(p) = prev {
                if p >= e.name.as_slice() {
                    return Err(FormatError::NotSorted);
                }
            }
            prev = Some(e.name.as_slice());
        }
        for e in entries_vec {
            entries.insert(e.name.clone(), e);
        }
        Ok(Dir { entries })
    }
}

fn load_node(
    store: &impl NodeStore,
    hash: &Hash,
    expected_level: Option<u8>,
    out: &mut Vec<Entry>,
) -> Result<(), FormatError> {
    let bytes = store.get(hash).ok_or(FormatError::MissingNode(*hash))?;
    let mut r = Reader { buf: &bytes, pos: 0 };
    let level = r.u8()?;
    if let Some(expect) = expected_level {
        if level != expect {
            return Err(FormatError::BadNode("level mismatch"));
        }
    }
    let count = r.u32()? as usize;
    if count > MAX_NODE_ENTRIES {
        return Err(FormatError::BadNode("node too wide"));
    }
    if count == 0 && !(level == 0 && expected_level.is_none()) {
        // Only the root of an empty directory may be an empty node.
        return Err(FormatError::BadNode("empty non-root node"));
    }
    if level == 0 {
        for _ in 0..count {
            out.push(decode_entry(&mut r)?);
        }
    } else {
        for _ in 0..count {
            let key_len = r.u8()? as usize;
            let key = r.take(key_len)?.to_vec();
            let child = r.hash()?;
            let first_idx = out.len();
            load_node(store, &child, Some(level - 1), out)?;
            // The separator key must be the first key under the child —
            // one encoding per logical tree.
            if out.get(first_idx).map(|e| e.name.as_slice()) != Some(key.as_slice()) {
                return Err(FormatError::BadNode("separator key mismatch"));
            }
        }
    }
    if !r.done() {
        return Err(FormatError::BadNode("trailing bytes"));
    }
    Ok(())
}

// ── Diff ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

/// Entry-level diff between two directory roots. Equal roots short-circuit
/// (equal hashes ⇒ identical subtrees, §4.9). Node-granular pruning inside
/// one directory is a deferred optimization; nested-tree diff recursion
/// belongs to the storage server (M2).
pub fn diff(
    store: &impl NodeStore,
    a: &Hash,
    b: &Hash,
) -> Result<Vec<(Vec<u8>, DiffKind)>, FormatError> {
    if a == b {
        return Ok(Vec::new());
    }
    let da = Dir::load(store, a)?;
    let db = Dir::load(store, b)?;
    let mut out = Vec::new();
    for (name, ea) in &da.entries {
        match db.entries.get(name) {
            None => out.push((name.clone(), DiffKind::Removed)),
            Some(eb) if ea != eb => out.push((name.clone(), DiffKind::Modified)),
            Some(_) => {}
        }
    }
    for name in db.entries.keys() {
        if !da.entries.contains_key(name) {
            out.push((name.clone(), DiffKind::Added));
        }
    }
    out.sort();
    Ok(out)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn fake_hash(seed: u8) -> Hash {
        Hash::of(&[seed])
    }

    fn file_entry(name: &[u8], content: &[u8], mtime: u64, flags: u32) -> Entry {
        if content.len() <= INLINE_MAX {
            Entry {
                name: name.to_vec(),
                kind: EntryKind::File,
                flags,
                size: content.len() as u64,
                mtime,
                content: Content::Inline(content.to_vec()),
            }
        } else {
            Entry {
                name: name.to_vec(),
                kind: EntryKind::File,
                flags,
                size: content.len() as u64,
                mtime,
                content: Content::ChunkList(Hash::of(content)),
            }
        }
    }

    fn dir_entry(name: &[u8], child: Hash, mtime: u64) -> Entry {
        Entry {
            name: name.to_vec(),
            kind: EntryKind::Dir,
            flags: 0,
            size: 0,
            mtime,
            content: Content::DirRoot(child),
        }
    }

    #[test]
    fn empty_dir_roundtrip() {
        let mut store = MemStore::new();
        let root = Dir::new().save(&mut store);
        let loaded = Dir::load(&store, &root).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.save(&mut store), root);
    }

    #[test]
    fn rejects_bad_names() {
        let mut d = Dir::new();
        for bad in [&b""[..], b".", b"..", b"a/b", b"a\0b"] {
            let e = file_entry(bad, b"x", 0, 0);
            assert_eq!(d.upsert(e), Err(FormatError::BadName), "{bad:?}");
        }
        let long = vec![b'a'; 256];
        assert_eq!(d.upsert(file_entry(&long, b"x", 0, 0)), Err(FormatError::BadName));
    }

    #[test]
    fn rejects_non_canonical_entries() {
        let mut d = Dir::new();
        // Small file pretending to be chunked.
        let mut e = file_entry(b"f", b"small", 0, 0);
        e.content = Content::ChunkList(fake_hash(1));
        assert!(d.upsert(e).is_err());
        // Dir with nonzero size.
        let mut e = dir_entry(b"d", fake_hash(2), 0);
        e.size = 7;
        assert!(d.upsert(e).is_err());
        // Inline size lying about content length.
        let mut e = file_entry(b"g", b"abc", 0, 0);
        e.size = 4;
        assert!(d.upsert(e).is_err());
    }

    #[test]
    fn structural_sharing_on_small_edit() {
        let mut store = MemStore::new();
        let mut dir = Dir::new();
        for i in 0..1000u32 {
            let name = format!("file-{i:05}");
            dir.upsert(file_entry(name.as_bytes(), &i.to_le_bytes(), 1, 0)).unwrap();
        }
        dir.save(&mut store);
        let before = store.len();

        dir.upsert(file_entry(b"file-00500", b"changed", 2, 0)).unwrap();
        dir.save(&mut store);
        let new_nodes = store.len() - before;
        // A one-entry edit rewrites the leaf holding it (the split decision
        // is per-item, so neighbors are untouched) plus the spine above.
        assert!(new_nodes <= 8, "edit rewrote {new_nodes} nodes");
    }

    #[test]
    fn diff_reports_changes() {
        let mut store = MemStore::new();
        let mut d1 = Dir::new();
        d1.upsert(file_entry(b"keep", b"same", 1, 0)).unwrap();
        d1.upsert(file_entry(b"gone", b"bye", 1, 0)).unwrap();
        d1.upsert(file_entry(b"edit", b"v1", 1, 0)).unwrap();
        let r1 = d1.save(&mut store);

        let mut d2 = Dir::new();
        d2.upsert(file_entry(b"keep", b"same", 1, 0)).unwrap();
        d2.upsert(file_entry(b"edit", b"v2", 2, 0)).unwrap();
        d2.upsert(file_entry(b"new", b"hi", 2, 0)).unwrap();
        let r2 = d2.save(&mut store);

        assert_eq!(
            diff(&store, &r1, &r2).unwrap(),
            vec![
                (b"edit".to_vec(), DiffKind::Modified),
                (b"gone".to_vec(), DiffKind::Removed),
                (b"new".to_vec(), DiffKind::Added),
            ]
        );
        assert!(diff(&store, &r1, &r1).unwrap().is_empty());
    }

    // ── Proptest strategies ─────────────────────────────────────────────

    fn arb_name() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(
            proptest::sample::select(b"abcdefgxyz-_.0123456789".to_vec()),
            1..16,
        )
        .prop_filter("reserved", |n| validate_name(n).is_ok())
    }

    fn arb_entry() -> impl Strategy<Value = Entry> {
        (
            arb_name(),
            proptest::collection::vec(any::<u8>(), 0..64),
            any::<u64>(),
            prop_oneof![Just(0u32), Just(FLAG_EXECUTABLE)],
            any::<bool>(),
            any::<u8>(),
            1u64..1_000_000,
        )
            .prop_map(|(name, content, mtime, flags, is_dir, seed, big_size)| {
                if is_dir {
                    Entry {
                        name,
                        kind: EntryKind::Dir,
                        flags,
                        size: 0,
                        mtime,
                        content: Content::DirRoot(fake_hash(seed)),
                    }
                } else if seed % 3 == 0 {
                    Entry {
                        name,
                        kind: EntryKind::File,
                        flags,
                        size: INLINE_MAX as u64 + big_size,
                        mtime,
                        content: Content::ChunkList(fake_hash(seed)),
                    }
                } else {
                    Entry {
                        name: name.clone(),
                        kind: EntryKind::File,
                        flags,
                        size: content.len() as u64,
                        mtime,
                        content: Content::Inline(content),
                    }
                }
            })
    }

    fn arb_entries(max: usize) -> impl Strategy<Value = Vec<Entry>> {
        proptest::collection::vec(arb_entry(), 0..max).prop_map(|es| {
            // Same-name entries collapse; keep the last, like upsert would.
            let mut m = BTreeMap::new();
            for e in es {
                m.insert(e.name.clone(), e);
            }
            m.into_values().collect()
        })
    }

    proptest! {
        /// §4.1: same logical contents ⇒ same root, regardless of edit
        /// order and regardless of churn (inserts later removed).
        #[test]
        fn canonical_form(
            entries in arb_entries(64),
            churn in arb_entries(16),
            order_a in any::<u64>(),
            order_b in any::<u64>(),
        ) {
            let shuffle = |seed: u64| {
                let mut v = entries.clone();
                let mut s = seed;
                for i in (1..v.len()).rev() {
                    s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    v.swap(i, (s >> 33) as usize % (i + 1));
                }
                v
            };
            let final_names: std::collections::HashSet<_> =
                entries.iter().map(|e| e.name.clone()).collect();

            let mut store = MemStore::new();

            let mut d1 = Dir::new();
            for e in shuffle(order_a) {
                d1.upsert(e).unwrap();
            }
            let r1 = d1.save(&mut store);

            // Second build: interleave churn entries, then remove them.
            let mut d2 = Dir::new();
            for e in &churn {
                d2.upsert(e.clone()).unwrap();
            }
            for e in shuffle(order_b) {
                d2.upsert(e).unwrap();
            }
            for e in &churn {
                if !final_names.contains(&e.name) {
                    d2.remove(&e.name);
                }
            }
            let r2 = d2.save(&mut store);

            prop_assert_eq!(r1, r2);
        }

        /// Round-trip: save → load = identity, and re-save reproduces the
        /// identical root (serialize/deserialize is the identity).
        #[test]
        fn roundtrip(entries in arb_entries(64)) {
            let mut store = MemStore::new();
            let mut dir = Dir::new();
            for e in entries.clone() {
                dir.upsert(e).unwrap();
            }
            let root = dir.save(&mut store);
            let loaded = Dir::load(&store, &root).unwrap();
            prop_assert_eq!(&loaded, &dir);
            prop_assert_eq!(loaded.save(&mut store), root);
        }

        /// Diff against a modified copy reports exactly the touched names.
        #[test]
        fn diff_matches_logical_changes(
            entries in arb_entries(48),
            extra in arb_entry(),
        ) {
            prop_assume!(!entries.iter().any(|e| e.name == extra.name));
            let mut store = MemStore::new();
            let mut d1 = Dir::new();
            for e in entries.clone() {
                d1.upsert(e).unwrap();
            }
            let r1 = d1.save(&mut store);
            let mut d2 = d1.clone();
            d2.upsert(extra.clone()).unwrap();
            let r2 = d2.save(&mut store);
            prop_assert_eq!(
                diff(&store, &r1, &r2).unwrap(),
                vec![(extra.name.clone(), DiffKind::Added)]
            );
        }

        /// Decoder is total: arbitrary bytes never panic, only error.
        #[test]
        fn decoder_rejects_garbage(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let mut store = MemStore::new();
            let hash = store.put(&bytes);
            let _ = Dir::load(&store, &hash);
        }
    }
}
