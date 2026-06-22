//! Per-directory prolly trees (Merkle search trees) — spec rev1§4.1, rev1§4.9.
//!
//! A directory is a sorted sequence of entries, split into content-addressed
//! nodes by a content-defined rule, so tree shape is history-independent
//! (canonical): the same logical contents always produce the same root hash
//! regardless of edit order.
//!
//! Format constants (changing any of these is a format migration):
//!   - Entry encoding: deterministic TLV, little-endian, exactly one
//!     encoding per logical entry (rev1§4.9).
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
//! file path-copies only the directories on its path (rev1§4.3), and per-node
//! dedup in the store makes the rebuild emit only changed nodes. Node-level
//! incremental update is an optimization for very large directories,
//! deferred past MVP.
//!
//! Decoders here are strict (they are cargo-fuzz targets, rev1§3.7/rev1§6): every
//! canonicality rule checked on encode is also rejected on decode, and
//! trailing bytes are errors.

use crate::hash::Hash;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use vstd::prelude::*;

/// Files at or below this size live inline in the directory entry (rev1§4.9).
/// The rule is a pure function of content, preserving canonical form.
pub const INLINE_MAX: usize = 512;

/// Advisory-executable bit in the flags word — a type hint with zero
/// security semantics (rev1§4.9).
pub const FLAG_EXECUTABLE: u32 = 1 << 0;

const SPLIT_BITS: u32 = 5;
const SPLIT_MASK: u64 = (1 << SPLIT_BITS) - 1;

// MAX_OPT_BYTES / OPT_TAG_FLAGS / MAX_NODE_ENTRIES live inside the `verus!{}`
// block at the end of this file: a const declared outside the macro is invisible
// to Verus, and the verified codecs (`decode_raw`/`encode_raw`, `decode_node`)
// name them. They erase to the same `pub const` / `const` so external code is
// unchanged.

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
    /// ≤ INLINE_MAX bytes, inlined (rev1§4.9).
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
/// live in one keyspace (hash = address, rev1§4.1).
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
    if ok {
        Ok(())
    } else {
        Err(FormatError::BadName)
    }
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

// `Entry` ↔ `RawEntry`: the trivially-total `Hash`/`EntryKind` (un)wrap that
// keeps `Hash` out of the verified core (the round-trip theorem lives on the
// `[u8; 32]`-carrying `RawEntry`, so it covers all 32 hash bytes).
fn entry_to_raw(e: &Entry) -> RawEntry {
    RawEntry {
        name: e.name.clone(),
        kind: match e.kind {
            EntryKind::File => 0,
            EntryKind::Dir => 1,
        },
        flags: e.flags,
        size: e.size,
        mtime: e.mtime,
        content: match &e.content {
            Content::Inline(bytes) => RawContent::Inline(bytes.clone()),
            Content::ChunkList(h) => RawContent::ChunkList(*h.as_bytes()),
            Content::DirRoot(h) => RawContent::DirRoot(*h.as_bytes()),
        },
    }
}

fn raw_to_entry(raw: RawEntry) -> Entry {
    Entry {
        // `decode_raw` rejects any kind byte other than 0/1.
        kind: if raw.kind == 0 {
            EntryKind::File
        } else {
            EntryKind::Dir
        },
        flags: raw.flags,
        size: raw.size,
        mtime: raw.mtime,
        content: match raw.content {
            RawContent::Inline(bytes) => Content::Inline(bytes),
            RawContent::ChunkList(a) => Content::ChunkList(Hash::from_bytes(a)),
            RawContent::DirRoot(a) => Content::DirRoot(Hash::from_bytes(a)),
        },
        name: raw.name,
    }
}

fn tlv_err(e: TlvErr) -> FormatError {
    match e {
        TlvErr::Truncated => FormatError::BadNode("truncated"),
        TlvErr::BadEntry(why) => FormatError::BadEntry(why),
        TlvErr::BadNode(why) => FormatError::BadNode(why),
    }
}

pub(crate) fn encode_entry(e: &Entry, out: &mut Vec<u8>) {
    encode_raw(&entry_to_raw(e), out);
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

pub(crate) fn decode_entry(r: &mut Reader) -> Result<Entry, FormatError> {
    // The verified core (`decode_raw`) parses one entry's structure + optional
    // section (total ∀ bytes, accepts only canonical encodings); `validate_entry`
    // adds the entry-level well-formedness that only shrinks the accept set.
    let (raw, consumed) = decode_raw(r.buf, r.pos).map_err(tlv_err)?;
    r.pos += consumed;
    let entry = raw_to_entry(raw);
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

/// In-memory logical directory: name → entry, memcmp order (rev1§4.9).
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

/// One stored node, shallowly parsed — the GC mark walk (rev1§4.6) needs the
/// raw child hashes of internal nodes, which `Dir::load` flattens away.
#[derive(Debug)]
pub enum NodeRefs {
    /// Internal node: child node hashes.
    Children(Vec<Hash>),
    /// Leaf node: entries (whose `Content` may reference chunk lists and
    /// child directory roots).
    Entries(Vec<Entry>),
}

pub fn parse_node(bytes: &[u8]) -> Result<NodeRefs, FormatError> {
    // `decode_node` (verified, total ∀ bytes) does the structural parse —
    // level/count/items, the ≤ MAX_NODE_ENTRIES cap, and the whole-buffer-consumed
    // (trailing-bytes) check. Entry-level well-formedness (`validate_entry`) and the
    // `Hash` wrap stay plain Rust (they only shrink the accept set / touch `Hash`).
    match decode_node(bytes).map_err(tlv_err)? {
        (_level, RawNodeBody::Leaf(entries)) => {
            let mut out = Vec::with_capacity(entries.len());
            for raw in entries {
                let entry = raw_to_entry(raw);
                validate_entry(&entry)?;
                out.push(entry);
            }
            Ok(NodeRefs::Entries(out))
        }
        (_level, RawNodeBody::Internal(children)) => {
            let mut out = Vec::with_capacity(children.len());
            for c in children {
                out.push(Hash::from_bytes(c.child));
            }
            Ok(NodeRefs::Children(out))
        }
    }
}

fn load_node(
    store: &impl NodeStore,
    hash: &Hash,
    expected_level: Option<u8>,
    out: &mut Vec<Entry>,
) -> Result<(), FormatError> {
    let bytes = store.get(hash).ok_or(FormatError::MissingNode(*hash))?;
    // `decode_node` (verified) does the structural parse + ≤ MAX cap + trailing
    // check. The cross-node discipline — level matches the parent's expectation,
    // empty only at the directory root, and the separator key equals the first key
    // under each child — needs root-ness/recursion, so it stays here.
    let (level, body) = decode_node(&bytes).map_err(tlv_err)?;
    if let Some(expect) = expected_level {
        if level != expect {
            return Err(FormatError::BadNode("level mismatch"));
        }
    }
    match body {
        RawNodeBody::Leaf(entries) => {
            // Only the root of an empty directory may be an empty node.
            if entries.is_empty() && expected_level.is_some() {
                return Err(FormatError::BadNode("empty non-root node"));
            }
            for raw in entries {
                let entry = raw_to_entry(raw);
                validate_entry(&entry)?;
                out.push(entry);
            }
        }
        RawNodeBody::Internal(children) => {
            // An internal node (level > 0) is never the empty-directory root.
            if children.is_empty() {
                return Err(FormatError::BadNode("empty non-root node"));
            }
            for c in children {
                let first_idx = out.len();
                load_node(store, &Hash::from_bytes(c.child), Some(level - 1), out)?;
                // The separator key must be the first key under the child —
                // one encoding per logical tree.
                if out.get(first_idx).map(|e| e.name.as_slice()) != Some(c.key.as_slice()) {
                    return Err(FormatError::BadNode("separator key mismatch"));
                }
            }
        }
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
/// (equal hashes ⇒ identical subtrees, rev1§4.9). Node-granular pruning inside
/// one directory is a deferred optimization; nested-tree diff recursion
/// belongs to the storage server.
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

// ── Verified TLV core ─────────────────────────────────────────────────────
//
// The directory-entry TLV codec (rev1§4.9), proven in Verus: `decode_raw` is
// **total ∀ bytes** (verifying the body *is* the no-panic theorem) and the
// **canonical-form round-trip is a theorem ∀** — `encode_raw(decode_raw(b)) ==
// b[..k]` (the property `cas/fuzz/.../tlv_entry.rs` samples). `Hash` is kept
// out of the proof surface: the verified core carries `[u8; 32]` so the 32 hash
// bytes round-trip *inside* the proof, and `decode_entry`/`encode_entry` are
// thin `Entry ↔ RawEntry` converters that do the trivially-total
// `Hash::{from_bytes,as_bytes}` wrap. Entry-level well-formedness
// (`validate_entry`) stays plain Rust — it only shrinks the accept set and so
// does not bear on the round-trip; the fuzz oracle covers the full `Entry`
// path.
verus! {

/// Hard cap on optional-TLV bytes per entry (rev1§4.9) — keeps directory nodes
/// directory-shaped regardless of future tags. Inside the macro so the verified
/// `decode_raw` can name it (a const outside `verus!{}` is invisible to Verus).
pub const MAX_OPT_BYTES: usize = 4096;

/// The one optional tag defined by format v0 (the advisory flags word).
const OPT_TAG_FLAGS: u8 = 1;

/// Forced node boundary: at most this many items per directory node (rev1§4.1).
/// Inside the macro so the verified `decode_node` can name it; erases to the same
/// module-level `const` that `build_level` uses.
const MAX_NODE_ENTRIES: usize = 128;

/// The `Hash`-free image of one decoded entry — `[u8; 32]` in place of `Hash`
/// so the round-trip proof never touches the external `Hash` type.
pub struct RawEntry {
    pub name: Vec<u8>,
    pub kind: u8,
    pub flags: u32,
    pub size: u64,
    pub mtime: u64,
    pub content: RawContent,
}

pub enum RawContent {
    Inline(Vec<u8>),
    ChunkList([u8; 32]),
    DirRoot([u8; 32]),
}

/// One internal-node child slot, `Hash`-free: the separator key plus the raw
/// 32-byte child node hash (`[u8; 32]` keeps `Hash` off the proof surface, like
/// `RawContent`). The running `load_node` re-wraps `child` into a `Hash`.
pub struct RawChild {
    pub key: Vec<u8>,
    pub child: [u8; 32],
}

/// The `Hash`-free image of one decoded node body: leaf entries (`level == 0`)
/// or internal child slots (`level > 0`). `decode_node` returns it with the
/// node's level; the running `parse_node`/`load_node` do the thin `Hash` wrap
/// (the entry path also runs `validate_entry`, which only shrinks the accept set).
pub enum RawNodeBody {
    Leaf(Vec<RawEntry>),
    Internal(Vec<RawChild>),
}

/// Why `decode_raw` rejected — mapped 1:1 to `FormatError` by `decode_entry`
/// (an in-block enum because the external `FormatError` cannot be constructed
/// inside `verus!{}`; its `MissingNode(Hash)` variant would drag in `Hash`).
pub enum TlvErr {
    Truncated,
    BadEntry(&'static str),
    BadNode(&'static str),
}

// ── Spec: the canonical byte image of an entry ───────────────────────────

pub open spec fn u16_le(x: u16) -> Seq<u8> {
    seq![x as u8, (x >> 8) as u8]
}

pub open spec fn u32_le(x: u32) -> Seq<u8> {
    seq![x as u8, (x >> 8) as u8, (x >> 16) as u8, (x >> 24) as u8]
}

pub open spec fn u64_le(x: u64) -> Seq<u8> {
    seq![
        x as u8, (x >> 8) as u8, (x >> 16) as u8, (x >> 24) as u8,
        (x >> 32) as u8, (x >> 40) as u8, (x >> 48) as u8, (x >> 56) as u8,
    ]
}

pub open spec fn content_bytes(c: RawContent) -> Seq<u8> {
    match c {
        RawContent::Inline(b) => seq![0u8] + u16_le(b@.len() as u16) + b@,
        RawContent::ChunkList(h) => seq![1u8] + h@,
        RawContent::DirRoot(h) => seq![2u8] + h@,
    }
}

/// The optional section's bytes (the `u16` length prefix + the records). The
/// format is canonical: either absent (`flags == 0`, an empty section) or
/// exactly the one 7-byte flags record.
pub open spec fn opt_bytes(flags: u32) -> Seq<u8> {
    if flags == 0 {
        u16_le(0)
    } else {
        u16_le(7) + seq![1u8] + u16_le(4) + u32_le(flags)
    }
}

pub open spec fn canonical_bytes(e: RawEntry) -> Seq<u8> {
    seq![e.name@.len() as u8] + e.name@ + seq![e.kind] + u64_le(e.size) + u64_le(e.mtime)
        + content_bytes(e.content) + opt_bytes(e.flags)
}

/// The canonical byte image of a leaf node's entry sequence: each entry's
/// `canonical_bytes`, concatenated in order. Back-recursive (peels the last
/// entry) so the decode loop's running concat invariant
/// (`buf[5..pos] == entries_bytes(parsed)`) restores in one `lemma_entries_push`
/// step per pushed entry.
pub open spec fn entries_bytes(es: Seq<RawEntry>) -> Seq<u8>
    decreases es.len(),
{
    if es.len() == 0 {
        Seq::<u8>::empty()
    } else {
        entries_bytes(es.drop_last()) + canonical_bytes(es.last())
    }
}

/// The canonical byte image of a whole **leaf** node: `[level=0][count u32][entries…]`.
/// `decode_node` proves the consumed bytes equal this for every accepted leaf —
/// the node-grain of rev1§4.9 ("exactly one encoding per logical leaf node") and
/// the rev1§6 decode-then-re-encode oracle.
pub open spec fn canonical_leaf_bytes(es: Seq<RawEntry>) -> Seq<u8> {
    seq![0u8] + u32_le(es.len() as u32) + entries_bytes(es)
}

// ── Exec byte readers (explicit index + shift, not
//    `from_le_bytes`/`try_into`), each carrying its `*_le` round-trip ───────

fn read_u16_le(buf: &[u8], off: usize) -> (v: u16)
    requires
        off + 2 <= buf@.len(),
    ensures
        buf@.subrange(off as int, off as int + 2) == u16_le(v),
{
    broadcast use vstd::slice::group_slice_axioms;
    let b0 = buf[off];
    let b1 = buf[off + 1];
    let v: u16 = (b0 as u16) | ((b1 as u16) << 8);
    assert(((b0 as u16) | ((b1 as u16) << 8)) as u8 == b0) by (bit_vector);
    assert((((b0 as u16) | ((b1 as u16) << 8)) >> 8) as u8 == b1) by (bit_vector);
    assert(buf@.subrange(off as int, off as int + 2) =~= u16_le(v));
    v
}

fn read_u32_le(buf: &[u8], off: usize) -> (v: u32)
    requires
        off + 4 <= buf@.len(),
    ensures
        buf@.subrange(off as int, off as int + 4) == u32_le(v),
{
    broadcast use vstd::slice::group_slice_axioms;
    let b0 = buf[off];
    let b1 = buf[off + 1];
    let b2 = buf[off + 2];
    let b3 = buf[off + 3];
    let v: u32 = (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24);
    assert(((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) as u8 == b0)
        by (bit_vector);
    assert(((((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) >> 8) as u8) == b1)
        by (bit_vector);
    assert(((((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) >> 16) as u8) == b2)
        by (bit_vector);
    assert(((((b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24)) >> 24) as u8) == b3)
        by (bit_vector);
    assert(buf@.subrange(off as int, off as int + 4) =~= u32_le(v));
    v
}

fn read_u64_le(buf: &[u8], off: usize) -> (v: u64)
    requires
        off + 8 <= buf@.len(),
    ensures
        buf@.subrange(off as int, off as int + 8) == u64_le(v),
{
    broadcast use vstd::slice::group_slice_axioms;
    let b0 = buf[off];
    let b1 = buf[off + 1];
    let b2 = buf[off + 2];
    let b3 = buf[off + 3];
    let b4 = buf[off + 4];
    let b5 = buf[off + 5];
    let b6 = buf[off + 6];
    let b7 = buf[off + 7];
    let v: u64 = (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
        | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert(v as u8 == b0) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 8) as u8 == b1) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 16) as u8 == b2) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 24) as u8 == b3) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 32) as u8 == b4) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 40) as u8 == b5) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 48) as u8 == b6) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert((v >> 56) as u8 == b7) by (bit_vector)
        requires v == (b0 as u64) | ((b1 as u64) << 8) | ((b2 as u64) << 16) | ((b3 as u64) << 24)
            | ((b4 as u64) << 32) | ((b5 as u64) << 40) | ((b6 as u64) << 48) | ((b7 as u64) << 56);
    assert(buf@.subrange(off as int, off as int + 8) =~= u64_le(v));
    v
}

fn read_arr32(buf: &[u8], off: usize) -> (a: [u8; 32])
    requires
        off + 32 <= buf@.len(),
    ensures
        a@ == buf@.subrange(off as int, off as int + 32),
{
    broadcast use vstd::slice::group_slice_axioms;
    let a: [u8; 32] = [
        buf[off], buf[off + 1], buf[off + 2], buf[off + 3],
        buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7],
        buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11],
        buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15],
        buf[off + 16], buf[off + 17], buf[off + 18], buf[off + 19],
        buf[off + 20], buf[off + 21], buf[off + 22], buf[off + 23],
        buf[off + 24], buf[off + 25], buf[off + 26], buf[off + 27],
        buf[off + 28], buf[off + 29], buf[off + 30], buf[off + 31],
    ];
    assert(a@ =~= buf@.subrange(off as int, off as int + 32));
    a
}

// ── Exec byte writers (push loops with clean `Seq` concat specs; vstd's
//    `extend_from_slice` ensures uses `cloned`, awkward for u8 equality) ────

fn extend_bytes(out: &mut Vec<u8>, src: &[u8])
    ensures
        final(out)@ == old(out)@ + src@,
{
    broadcast use vstd::slice::group_slice_axioms;
    let mut i: usize = 0;
    while i < src.len()
        invariant
            i <= src@.len(),
            out@ == old(out)@ + src@.subrange(0, i as int),
        decreases src@.len() - i,
    {
        out.push(src[i]);
        assert(src@.subrange(0, i as int + 1) =~= src@.subrange(0, i as int).push(src@[i as int]));
        i += 1;
    }
    assert(src@.subrange(0, src@.len() as int) =~= src@);
}

fn copy_range(buf: &[u8], off: usize, n: usize) -> (v: Vec<u8>)
    requires
        off + n <= buf@.len(),
    ensures
        v@ == buf@.subrange(off as int, off as int + n),
{
    broadcast use vstd::slice::group_slice_axioms;
    assert(off + n <= buf.len());     // off + n ≤ buf.len() (usize), so no overflow
    let end = off + n;
    let mut v: Vec<u8> = Vec::new();
    let mut i: usize = off;
    while i < end
        invariant
            off <= i <= end,
            end == off + n,
            end <= buf@.len(),
            v@ == buf@.subrange(off as int, i as int),
        decreases end - i,
    {
        v.push(buf[i]);
        assert(buf@.subrange(off as int, i as int + 1)
            =~= buf@.subrange(off as int, i as int).push(buf@[i as int]));
        i += 1;
    }
    assert(v@ =~= buf@.subrange(off as int, off as int + n));
    v
}

fn push_arr32(out: &mut Vec<u8>, h: &[u8; 32])
    ensures
        final(out)@ == old(out)@ + h@,
{
    broadcast use vstd::array::group_array_axioms;
    let mut i: usize = 0;
    while i < 32
        invariant
            i <= 32,
            h@.len() == 32,
            out@ == old(out)@ + h@.subrange(0, i as int),
        decreases 32 - i,
    {
        out.push(h[i]);
        assert(h@.subrange(0, i as int + 1) =~= h@.subrange(0, i as int).push(h@[i as int]));
        i += 1;
    }
    assert(h@.subrange(0, 32) =~= h@);
}

fn push_u16_le(out: &mut Vec<u8>, x: u16)
    ensures
        final(out)@ == old(out)@ + u16_le(x),
{
    out.push(x as u8);
    out.push((x >> 8) as u8);
    assert(out@ =~= old(out)@ + u16_le(x));
}

fn push_u32_le(out: &mut Vec<u8>, x: u32)
    ensures
        final(out)@ == old(out)@ + u32_le(x),
{
    out.push(x as u8);
    out.push((x >> 8) as u8);
    out.push((x >> 16) as u8);
    out.push((x >> 24) as u8);
    assert(out@ =~= old(out)@ + u32_le(x));
}

fn push_u64_le(out: &mut Vec<u8>, x: u64)
    ensures
        final(out)@ == old(out)@ + u64_le(x),
{
    out.push(x as u8);
    out.push((x >> 8) as u8);
    out.push((x >> 16) as u8);
    out.push((x >> 24) as u8);
    out.push((x >> 32) as u8);
    out.push((x >> 40) as u8);
    out.push((x >> 48) as u8);
    out.push((x >> 56) as u8);
    assert(out@ =~= old(out)@ + u64_le(x));
}

/// Serialize one entry to its canonical TLV, appended to `out`. The exec
/// `Vec`-building encoder, proven to produce exactly `canonical_bytes`.
pub fn encode_raw(e: &RawEntry, out: &mut Vec<u8>)
    ensures
        final(out)@ == old(out)@ + canonical_bytes(*e),
{
    out.push(e.name.len() as u8);
    extend_bytes(out, e.name.as_slice());
    out.push(e.kind);
    push_u64_le(out, e.size);
    push_u64_le(out, e.mtime);
    match &e.content {
        RawContent::Inline(b) => {
            out.push(0u8);
            push_u16_le(out, b.len() as u16);
            extend_bytes(out, b.as_slice());
        }
        RawContent::ChunkList(h) => {
            out.push(1u8);
            push_arr32(out, h);
        }
        RawContent::DirRoot(h) => {
            out.push(2u8);
            push_arr32(out, h);
        }
    }
    if e.flags != 0 {
        push_u16_le(out, 7);
        out.push(OPT_TAG_FLAGS);
        push_u16_le(out, 4);
        push_u32_le(out, e.flags);
    } else {
        push_u16_le(out, 0);
    }
    assert(out@ =~= old(out)@ + canonical_bytes(*e));
}

// ── Decode: total ∀ bytes, and accepts only canonical encodings ───────────

/// `seq.subrange(a, b) + seq.subrange(b, c) == seq.subrange(a, c)`.
proof fn lemma_cat(s: Seq<u8>, a: int, b: int, c: int)
    requires
        0 <= a <= b <= c <= s.len(),
    ensures
        s.subrange(a, b) + s.subrange(b, c) == s.subrange(a, c),
{
    assert(s.subrange(a, b) + s.subrange(b, c) =~= s.subrange(a, c));
}

/// Whether `n` more bytes fit before `end` starting at `pos`, overflow-free.
fn fits(pos: usize, n: usize, end: usize) -> (b: bool)
    ensures
        b <==> pos + n <= end,
{
    n <= end && pos <= end - n
}

/// Parse one entry's `RawEntry` plus the byte count consumed, or reject.
/// **Total ∀** `buf` (verifying the body *is* the no-panic theorem); and on
/// `Ok` the consumed prefix equals the entry's `canonical_bytes` — so the
/// decoder only ever accepts a canonical encoding (the round-trip's hard
/// direction; the opt-section loop accepts at most one record).
pub fn decode_raw(buf: &[u8], start: usize) -> (r: Result<(RawEntry, usize), TlvErr>)
    requires
        start <= buf@.len(),
    ensures
        r matches Ok((e, k)) ==> start + k <= buf@.len()
            && canonical_bytes(e) == buf@.subrange(start as int, start as int + k as int),
{
    broadcast use vstd::slice::group_slice_axioms;
    let len = buf.len();

    // name_len (u8) + name
    if !fits(start, 1, len) {
        return Err(TlvErr::Truncated);
    }
    let name_len = buf[start] as usize;
    if !fits(start + 1, name_len, len) {
        return Err(TlvErr::Truncated);
    }
    let name = copy_range(buf, start + 1, name_len);
    let p_kind = start + 1 + name_len;

    // kind (u8)
    if !fits(p_kind, 1, len) {
        return Err(TlvErr::Truncated);
    }
    let kind = buf[p_kind];
    if kind != 0 && kind != 1 {
        return Err(TlvErr::BadEntry("bad kind"));
    }
    let p_size = p_kind + 1;

    // size, mtime (u64 each)
    if !fits(p_size, 8, len) {
        return Err(TlvErr::Truncated);
    }
    let size = read_u64_le(buf, p_size);
    let p_mtime = p_size + 8;
    if !fits(p_mtime, 8, len) {
        return Err(TlvErr::Truncated);
    }
    let mtime = read_u64_le(buf, p_mtime);
    let p_ctag = p_mtime + 8;

    // content tag (u8) + content
    if !fits(p_ctag, 1, len) {
        return Err(TlvErr::Truncated);
    }
    let ctag = buf[p_ctag];
    // content_bytes (the spec image) begins at the tag byte, so the content
    // segment is buf[p_ctag, p_optlen].
    let ghost gp_content = p_ctag as int;
    let p_content = p_ctag + 1;
    let content: RawContent;
    let p_optlen: usize;
    if ctag == 0 {
        if !fits(p_content, 2, len) {
            return Err(TlvErr::Truncated);
        }
        let ilen_u16 = read_u16_le(buf, p_content);
        let ilen = ilen_u16 as usize;
        let p_inline = p_content + 2;
        if !fits(p_inline, ilen, len) {
            return Err(TlvErr::Truncated);
        }
        let ib = copy_range(buf, p_inline, ilen);
        let end = p_inline + ilen;
        content = RawContent::Inline(ib);
        proof {
            // [0] + u16_le(ilen) + inline-bytes == buf[p_ctag, end]
            lemma_cat(buf@, gp_content, gp_content + 1, gp_content + 3);
            lemma_cat(buf@, gp_content, gp_content + 3, end as int);
        }
        assert(buf@.subrange(gp_content, gp_content + 1) =~= seq![0u8]);
        assert(content_bytes(content) == buf@.subrange(gp_content, end as int));
        p_optlen = end;
    } else if ctag == 1 {
        if !fits(p_content, 32, len) {
            return Err(TlvErr::Truncated);
        }
        let h = read_arr32(buf, p_content);
        let end = p_content + 32;
        content = RawContent::ChunkList(h);
        proof {
            lemma_cat(buf@, gp_content, gp_content + 1, end as int);
        }
        assert(buf@.subrange(gp_content, gp_content + 1) =~= seq![1u8]);
        assert(content_bytes(content) == buf@.subrange(gp_content, end as int));
        p_optlen = end;
    } else if ctag == 2 {
        if !fits(p_content, 32, len) {
            return Err(TlvErr::Truncated);
        }
        let h = read_arr32(buf, p_content);
        let end = p_content + 32;
        content = RawContent::DirRoot(h);
        proof {
            lemma_cat(buf@, gp_content, gp_content + 1, end as int);
        }
        assert(buf@.subrange(gp_content, gp_content + 1) =~= seq![2u8]);
        assert(content_bytes(content) == buf@.subrange(gp_content, end as int));
        p_optlen = end;
    } else {
        return Err(TlvErr::BadEntry("bad content tag"));
    }
    let ghost gp_optlen = p_optlen as int;

    // opt_len (u16) + optional section
    if !fits(p_optlen, 2, len) {
        return Err(TlvErr::Truncated);
    }
    let opt_len_u16 = read_u16_le(buf, p_optlen);
    let opt_len = opt_len_u16 as usize;
    let opt_start = p_optlen + 2;
    if opt_len > MAX_OPT_BYTES {
        return Err(TlvErr::BadEntry("optional section too large"));
    }
    if !fits(opt_start, opt_len, len) {
        return Err(TlvErr::Truncated);
    }
    let opt_end = opt_start + opt_len;
    let ghost g_opt_start = opt_start as int;
    // buf[p_optlen, opt_start] is the u16 length prefix of opt_bytes.
    assert(buf@.subrange(gp_optlen, g_opt_start) == u16_le(opt_len_u16));

    let mut flags: u32 = 0;
    let mut last_tag: i16 = -1;
    let mut p = opt_start;
    while p < opt_end
        invariant
            opt_start <= p <= opt_end,
            opt_end <= len,
            opt_end == opt_start + opt_len,
            len == buf@.len(),
            g_opt_start == opt_start as int,
            last_tag == -1 || last_tag == 1,
            (last_tag == -1) ==> (flags == 0 && p as int == g_opt_start),
            (last_tag == 1) ==> (flags != 0 && p as int == g_opt_start + 7
                && buf@.subrange(g_opt_start, p as int) == seq![1u8] + u16_le(4) + u32_le(flags)),
        decreases opt_end - p,
    {
        let ghost gp = p as int;
        let tag = buf[p];
        assert(tag == buf@[gp]);
        let pt = p + 1;
        if (tag as i16) <= last_tag {
            return Err(TlvErr::BadEntry("optional tags not strictly ascending"));
        }
        if !fits(pt, 2, opt_end) {
            return Err(TlvErr::Truncated);
        }
        let vlen_u16 = read_u16_le(buf, pt);
        let vlen = vlen_u16 as usize;
        let pv = pt + 2;
        if !fits(pv, vlen, opt_end) {
            return Err(TlvErr::Truncated);
        }
        let val_pos = pv;
        let pnext = pv + vlen;
        if tag == OPT_TAG_FLAGS {
            if vlen != 4 {
                return Err(TlvErr::BadEntry("bad flags length"));
            }
            let f = read_u32_le(buf, val_pos);
            if f == 0 {
                return Err(TlvErr::BadEntry("zero flags must be absent"));
            }
            // tag == 1 strictly exceeds last_tag ∈ {-1, 1}, so last_tag == -1:
            // the section's first (and only) record. Positions are relative to
            // gp == g_opt_start: tag@gp, len@gp+1, value@gp+3, pnext == gp+7.
            assert(last_tag == -1);
            assert(gp == g_opt_start);
            assert(pnext as int == g_opt_start + 7);
            assert(g_opt_start + 7 <= buf@.len());   // pnext <= opt_end <= len
            assert(buf@.subrange(g_opt_start, g_opt_start + 1) =~= seq![1u8]);
            assert(buf@.subrange(g_opt_start + 1, g_opt_start + 3) == u16_le(4));
            assert(buf@.subrange(g_opt_start + 3, g_opt_start + 7) == u32_le(f));
            proof {
                lemma_cat(buf@, g_opt_start + 1, g_opt_start + 3, g_opt_start + 7);
                lemma_cat(buf@, g_opt_start, g_opt_start + 1, g_opt_start + 7);
            }
            assert(buf@.subrange(g_opt_start, pnext as int) == seq![1u8] + u16_le(4) + u32_le(f));
            flags = f;
            last_tag = 1;
            p = pnext;
        } else {
            return Err(TlvErr::BadEntry("unknown optional tag"));
        }
    }

    // Loop done: p == opt_end. Either no record (flags == 0, empty section) or
    // exactly the one canonical flags record — so opt_bytes(flags) is exactly
    // the consumed bytes buf[p_optlen, opt_end].
    proof {
        lemma_cat(buf@, gp_optlen, g_opt_start, opt_end as int);
    }
    if last_tag == 1 {
        assert(opt_len_u16 == 7);
        assert(buf@.subrange(g_opt_start, opt_end as int) == seq![1u8] + u16_le(4) + u32_le(flags));
        assert(opt_bytes(flags) == buf@.subrange(gp_optlen, opt_end as int));
    } else {
        assert(flags == 0);
        assert(opt_len_u16 == 0);
        assert(g_opt_start == opt_end as int);
        assert(opt_bytes(flags) == buf@.subrange(gp_optlen, opt_end as int));
    }

    let e = RawEntry { name, kind, flags, size, mtime, content };

    // Assemble: canonical_bytes(e) == buf[start, opt_end].
    assert(seq![e.name@.len() as u8] == buf@.subrange(start as int, start as int + 1));
    assert(e.name@ == buf@.subrange(start as int + 1, p_kind as int));
    assert(seq![e.kind] =~= buf@.subrange(p_kind as int, p_size as int));
    assert(u64_le(e.size) == buf@.subrange(p_size as int, p_mtime as int));
    assert(u64_le(e.mtime) == buf@.subrange(p_mtime as int, gp_content));
    proof {
        lemma_cat(buf@, start as int, start as int + 1, p_kind as int);
        lemma_cat(buf@, start as int, p_kind as int, p_size as int);
        lemma_cat(buf@, start as int, p_size as int, p_mtime as int);
        lemma_cat(buf@, start as int, p_mtime as int, gp_content);
        lemma_cat(buf@, start as int, gp_content, gp_optlen);
        lemma_cat(buf@, start as int, gp_optlen, opt_end as int);
    }
    assert(canonical_bytes(e) == buf@.subrange(start as int, opt_end as int));
    Ok((e, opt_end - start))
}

// ── Node decode/encode: the leaf-grain canonical round-trip ────────────────

/// One unfold step of [`entries_bytes`]: appending an entry appends its
/// `canonical_bytes`. The decode/encode loops cite it to restore their running
/// concat invariant after each pushed entry.
proof fn lemma_entries_push(es: Seq<RawEntry>, e: RawEntry)
    ensures
        entries_bytes(es.push(e)) == entries_bytes(es) + canonical_bytes(e),
{
    assert(es.push(e).drop_last() =~= es);
    assert(es.push(e).last() == e);
}

/// Decode one stored directory node — `[level u8][count u32][items…]` — into its
/// `Hash`-free image, **total ∀ bytes** (verifying the body *is* the no-panic
/// theorem). Leaf items (`level == 0`) are entries via the verified `decode_raw`
/// loop; internal items are `[key_len u8][key][child u8;32]`. The whole buffer
/// must be consumed (a node is one stored object; trailing bytes are rejected).
/// For a **leaf** the consumed bytes equal `canonical_leaf_bytes` — the
/// node-grain canonical round-trip (rev1§4.9/§6). Internal nodes get **totality
/// only**: `parse_node` lowers separator keys into child hashes, so there is no
/// lossless single-node internal re-encoder (B13C's whole-tree oracle covers it).
pub fn decode_node(buf: &[u8]) -> (r: Result<(u8, RawNodeBody), TlvErr>)
    ensures
        r matches Ok((lvl, RawNodeBody::Leaf(es))) ==> lvl == 0 && canonical_leaf_bytes(es@)
            == buf@,
{
    broadcast use vstd::slice::group_slice_axioms;
    let len = buf.len();

    // [level u8][count u32]
    if !fits(0, 1, len) {
        return Err(TlvErr::Truncated);
    }
    let level = buf[0];
    if !fits(1, 4, len) {
        return Err(TlvErr::Truncated);
    }
    let count = read_u32_le(buf, 1);
    if count as usize > MAX_NODE_ENTRIES {
        return Err(TlvErr::BadNode("node too wide"));
    }
    assert(buf@.subrange(0, 1) =~= seq![level]);

    if level == 0 {
        let mut entries: Vec<RawEntry> = Vec::new();
        let mut i: u32 = 0;
        let mut pos: usize = 5;
        assert(buf@.subrange(5, 5) =~= entries_bytes(entries@));
        while i < count
            invariant
                5 <= pos <= len,
                len == buf@.len(),
                i <= count,
                entries@.len() == i,
                buf@.subrange(0, 1) == seq![level],
                buf@.subrange(1, 5) == u32_le(count),
                buf@.subrange(5, pos as int) == entries_bytes(entries@),
            decreases count - i,
        {
            let ghost old_pos = pos;
            let ghost old_entries = entries@;
            match decode_raw(buf, pos) {
                Ok((e, k)) => {
                    // decode_raw: pos + k <= len && canonical_bytes(e) == buf[pos, pos+k]
                    entries.push(e);
                    proof {
                        lemma_entries_push(old_entries, e);
                        lemma_cat(buf@, 5, old_pos as int, (old_pos + k) as int);
                    }
                    pos = pos + k;
                }
                Err(er) => {
                    return Err(er);
                }
            }
            i += 1;
        }
        // count entries parsed; the whole buffer must be consumed.
        if pos != len {
            return Err(TlvErr::BadNode("trailing bytes"));
        }
        proof {
            lemma_cat(buf@, 0, 1, 5);
            lemma_cat(buf@, 0, 5, len as int);
        }
        assert(buf@.subrange(0, 1) =~= seq![0u8]);
        assert(entries@.len() == count);
        assert(entries@.len() as u32 == count);
        assert(buf@ =~= buf@.subrange(0, len as int));
        assert(canonical_leaf_bytes(entries@) =~= buf@);
        Ok((level, RawNodeBody::Leaf(entries)))
    } else {
        let mut children: Vec<RawChild> = Vec::new();
        let mut i: u32 = 0;
        let mut pos: usize = 5;
        while i < count
            invariant
                5 <= pos <= len,
                len == buf@.len(),
                i <= count,
                children@.len() == i,
            decreases count - i,
        {
            if !fits(pos, 1, len) {
                return Err(TlvErr::Truncated);
            }
            let key_len = buf[pos] as usize;
            if !fits(pos + 1, key_len, len) {
                return Err(TlvErr::Truncated);
            }
            let key = copy_range(buf, pos + 1, key_len);
            let hpos = pos + 1 + key_len;
            if !fits(hpos, 32, len) {
                return Err(TlvErr::Truncated);
            }
            let child = read_arr32(buf, hpos);
            children.push(RawChild { key, child });
            pos = hpos + 32;
            i += 1;
        }
        if pos != len {
            return Err(TlvErr::BadNode("trailing bytes"));
        }
        Ok((level, RawNodeBody::Internal(children)))
    }
}

/// Serialize a **leaf** node's entries to their canonical bytes
/// (`[0][count u32][entries…]`), appended to `out`. The encode half of the
/// node-grain round-trip (mirrors `encode_raw` at the node grain): produces
/// exactly `canonical_leaf_bytes`, so `decode_node(encode_node_leaf(es)) == es`
/// and `encode_node_leaf(decode_node(b)) == b` for accepted leaf `b`.
pub fn encode_node_leaf(es: &Vec<RawEntry>, out: &mut Vec<u8>)
    ensures
        final(out)@ == old(out)@ + canonical_leaf_bytes(es@),
{
    out.push(0u8);
    push_u32_le(out, es.len() as u32);
    assert(es@.subrange(0, 0) =~= Seq::<RawEntry>::empty());
    let mut i: usize = 0;
    while i < es.len()
        invariant
            i <= es@.len(),
            out@ == old(out)@ + seq![0u8] + u32_le(es@.len() as u32) + entries_bytes(
                es@.subrange(0, i as int),
            ),
        decreases es@.len() - i,
    {
        let ghost prev = es@.subrange(0, i as int);
        encode_raw(&es[i], out);
        proof {
            lemma_entries_push(prev, es@[i as int]);
            assert(es@.subrange(0, i as int + 1) =~= prev.push(es@[i as int]));
        }
        i += 1;
    }
    assert(es@.subrange(0, es@.len() as int) =~= es@);
    assert(out@ =~= old(out)@ + canonical_leaf_bytes(es@));
}

} // verus!

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
        assert_eq!(
            d.upsert(file_entry(&long, b"x", 0, 0)),
            Err(FormatError::BadName)
        );
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
            dir.upsert(file_entry(name.as_bytes(), &i.to_le_bytes(), 1, 0))
                .unwrap();
        }
        dir.save(&mut store);
        let before = store.len();

        dir.upsert(file_entry(b"file-00500", b"changed", 2, 0))
            .unwrap();
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

    // ── Node-decoder rejection cases (B13A) ─────────────────────────────
    //
    // The node decoder is verified total ∀ bytes; these pin the rejection
    // *messages* the running path returns (the verified totality is the
    // no-panic backstop behind them).

    /// Assemble a leaf node the way `build_level` does, for crafting hostile
    /// parents below.
    fn leaf_node_bytes(entries: &[Entry]) -> Vec<u8> {
        let mut node = vec![0u8];
        node.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            encode_entry(e, &mut node);
        }
        node
    }

    #[test]
    fn node_decoder_rejects_overwide_count() {
        // [level=0][count=200]: count exceeds MAX_NODE_ENTRIES (128).
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&200u32.to_le_bytes());
        // `NodeRefs` is not `PartialEq`, so match the error variant directly.
        assert!(matches!(
            parse_node(&bytes),
            Err(FormatError::BadNode("node too wide"))
        ));
    }

    #[test]
    fn node_decoder_rejects_trailing_bytes() {
        // A valid empty leaf node ([0][0]) plus one trailing byte.
        let mut bytes = vec![0u8];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0xff);
        assert!(matches!(
            parse_node(&bytes),
            Err(FormatError::BadNode("trailing bytes"))
        ));
    }

    #[test]
    fn node_decoder_rejects_truncated_header() {
        // Fewer than the 5-byte [level][count] header.
        assert!(matches!(
            parse_node(&[]),
            Err(FormatError::BadNode("truncated"))
        ));
        assert!(matches!(
            parse_node(&[0u8, 1, 2]),
            Err(FormatError::BadNode("truncated"))
        ));
    }

    #[test]
    fn node_decoder_rejects_level_mismatch() {
        let mut store = MemStore::new();
        let e = file_entry(b"a", b"x", 1, 0);
        let child_hash = store.put(&leaf_node_bytes(&[e.clone()])); // a level-0 leaf
                                                                    // Parent claims level 2, so its leaf child is expected at level 1, not 0.
        let mut parent = vec![2u8];
        parent.extend_from_slice(&1u32.to_le_bytes());
        parent.push(e.name.len() as u8);
        parent.extend_from_slice(&e.name);
        parent.extend_from_slice(child_hash.as_bytes());
        let root = store.put(&parent);
        assert_eq!(
            Dir::load(&store, &root),
            Err(FormatError::BadNode("level mismatch"))
        );
    }

    #[test]
    fn node_decoder_rejects_separator_mismatch() {
        let mut store = MemStore::new();
        let e = file_entry(b"actual", b"x", 1, 0);
        let child_hash = store.put(&leaf_node_bytes(&[e]));
        // Level-1 parent whose separator key does not equal the child's first key.
        let mut parent = vec![1u8];
        parent.extend_from_slice(&1u32.to_le_bytes());
        parent.push(5u8);
        parent.extend_from_slice(b"wrong");
        parent.extend_from_slice(child_hash.as_bytes());
        let root = store.put(&parent);
        assert_eq!(
            Dir::load(&store, &root),
            Err(FormatError::BadNode("separator key mismatch"))
        );
    }

    #[test]
    fn node_decoder_rejects_empty_non_root() {
        let mut store = MemStore::new();
        // An empty leaf node is fine as a directory root, but not as a child.
        let child_hash = store.put(&leaf_node_bytes(&[]));
        let mut parent = vec![1u8];
        parent.extend_from_slice(&1u32.to_le_bytes());
        parent.push(1u8);
        parent.push(b'a');
        parent.extend_from_slice(child_hash.as_bytes());
        let root = store.put(&parent);
        assert_eq!(
            Dir::load(&store, &root),
            Err(FormatError::BadNode("empty non-root node"))
        );
    }

    #[test]
    fn node_leaf_decode_encode_roundtrip() {
        // Build a real (single-leaf) directory, then decode→re-encode its root
        // node: the verified leaf canonical round-trip, on real BLAKE3 bytes.
        let mut store = MemStore::new();
        let mut dir = Dir::new();
        for i in 0..5u32 {
            let name = format!("f-{i}");
            dir.upsert(file_entry(name.as_bytes(), b"x", 1, 0)).unwrap();
        }
        let root = dir.save(&mut store);
        let bytes = store.get(&root).unwrap();
        let (level, body) = match decode_node(&bytes) {
            Ok(x) => x,
            Err(_) => panic!("decode_node rejected a canonical leaf node"),
        };
        assert_eq!(level, 0);
        match body {
            RawNodeBody::Leaf(entries) => {
                let mut out = Vec::new();
                encode_node_leaf(&entries, &mut out);
                assert_eq!(out, bytes, "leaf node decode→encode is not the identity");
            }
            RawNodeBody::Internal(_) => panic!("expected a single leaf root node"),
        }
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
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        /// rev1§4.1: same logical contents ⇒ same root, regardless of edit
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
