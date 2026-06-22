#![no_main]
//! Two oracles over the directory node format (rev1§4.1, rev1§4.9):
//!
//! 1. **Shallow node decoder.** `parse_node` is the decoder the GC mark walk
//!    (rev1§4.6) runs on raw stored bytes — *below* the fetch-time hash check,
//!    so it must be total on hostile input. The node hash gate lives above it,
//!    so this harness feeds arbitrary bytes directly. For leaf nodes we also
//!    apply the canonical oracle: a level-0 node is `[0][count u32][entry…]`,
//!    every part deterministic, so re-encoding the parsed entries under the
//!    same header must reproduce the input byte for byte. Internal nodes drop
//!    their separator keys into child hashes during parse (the GC walk doesn't
//!    need them), so they get the totality check only — there is no lossless
//!    single-node re-encoder for them.
//!
//! 2. **Whole-tree round-trip (B13C).** The same bytes are carved into
//!    directory entries and built into a tree; `Dir::save → Dir::load →
//!    Dir::save` must reproduce the identical root over the whole, possibly
//!    multi-level tree. This reaches the lossless **internal-node** level the
//!    single-node leaf oracle above cannot — the separator-key discipline and
//!    the spine get their canonical-round-trip guard here (the rev1§6
//!    decode-then-re-encode oracle at the whole-tree grain).
use libfuzzer_sys::fuzz_target;

use cas::prolly::{parse_node, Content, Dir, Entry, EntryKind, MemStore, NodeRefs};

/// Valid name bytes (`validate_name` still rejects "." / ".." / over-long /
/// empty, which we simply skip on `upsert`).
const NAME_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-_.";

fuzz_target!(|data: &[u8]| {
    // (1) Shallow node-decoder oracle on the raw bytes: totality + leaf
    // canonical re-encode.
    if let Ok(NodeRefs::Entries(entries)) = parse_node(data) {
        let mut re = alloc_node_header(entries.len());
        for e in &entries {
            re.extend_from_slice(&cas::tlv::encode(e));
        }
        assert_eq!(re, data, "leaf node decoder accepted non-canonical bytes");
    }

    // (2) Whole-tree round-trip oracle: carve the bytes into entries, build the
    // tree, and check save → load → save is the identity.
    let dir = dir_from_bytes(data);
    let mut store = MemStore::new();
    let root = dir.save(&mut store);
    let loaded = Dir::load(&store, &root).expect("save then load must round-trip");
    assert_eq!(loaded, dir, "save → load is not the identity");
    assert_eq!(
        loaded.save(&mut store),
        root,
        "save → load → save changed the root"
    );
});

fn alloc_node_header(count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + count);
    out.push(0u8); // level 0
    out.extend_from_slice(&(count as u32).to_le_bytes());
    out
}

/// Carve fuzz bytes into a directory. Each record is a name-length byte
/// (1..=12) then that many bytes mapped into `NAME_ALPHABET`, then a
/// content-length byte (0..=7) then that many raw content bytes. Entries are
/// capped at 400 so a single iteration stays fast yet can still cross the
/// 128-entry node cap and climb the spine. Invalid names (e.g. "." / "..") are
/// skipped on `upsert`; duplicate names overwrite, exactly like the real path.
fn dir_from_bytes(data: &[u8]) -> Dir {
    let mut dir = Dir::new();
    let mut i = 0usize;
    while i < data.len() && dir.len() < 400 {
        let nlen = (data[i] as usize % 12) + 1;
        i += 1;
        let mut name = Vec::with_capacity(nlen);
        for _ in 0..nlen {
            let Some(&b) = data.get(i) else { break };
            name.push(NAME_ALPHABET[b as usize % NAME_ALPHABET.len()]);
            i += 1;
        }
        let clen = data.get(i).map_or(0, |&b| b as usize % 8);
        i += 1;
        let mut content = Vec::with_capacity(clen);
        for _ in 0..clen {
            let Some(&b) = data.get(i) else { break };
            content.push(b);
            i += 1;
        }
        let entry = Entry {
            name,
            kind: EntryKind::File,
            flags: 0,
            size: content.len() as u64,
            mtime: 0,
            content: Content::Inline(content),
        };
        let _ = dir.upsert(entry);
    }
    dir
}
