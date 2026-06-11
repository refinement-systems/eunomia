//! File content storage (spec §4.1, §4.9): content ≤ INLINE_MAX lives
//! inline in the directory entry; larger content is FastCDC-chunked and
//! referenced through a chunk-list object. The inline rule is a pure
//! function of content, preserving canonical form.

use crate::chunk::{boundaries, ChunkerParams};
use alloc::vec;
use alloc::vec::Vec;
use crate::hash::Hash;
use crate::prolly::{Content, Entry, EntryKind, FormatError, NodeStore, INLINE_MAX};

/// Chunk-list objects share the content-addressed keyspace with tree nodes;
/// the leading byte keeps the decoders from confusing them (tree nodes
/// start with their level, capped well below this).
const CHUNK_LIST_MAGIC: u8 = 0xC1;

pub fn store_file(
    store: &mut impl NodeStore,
    params: &ChunkerParams,
    data: &[u8],
) -> Content {
    if data.len() <= INLINE_MAX {
        return Content::Inline(data.to_vec());
    }
    let mut list = vec![CHUNK_LIST_MAGIC];
    let cuts = boundaries(params, data);
    list.extend_from_slice(&(cuts.len() as u32).to_le_bytes());
    let mut start = 0;
    for cut in cuts {
        let chunk = &data[start..cut];
        let hash = store.put(chunk);
        list.extend_from_slice(hash.as_bytes());
        list.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        start = cut;
    }
    Content::ChunkList(store.put(&list))
}

pub fn read_file(
    store: &impl NodeStore,
    content: &Content,
    size: u64,
) -> Result<Vec<u8>, FormatError> {
    match content {
        Content::Inline(bytes) => {
            if bytes.len() as u64 != size {
                return Err(FormatError::BadEntry("inline size mismatch"));
            }
            Ok(bytes.clone())
        }
        Content::ChunkList(hash) => {
            let list = store.get(hash).ok_or(FormatError::MissingNode(*hash))?;
            let mut out = Vec::new();
            for (chunk_hash, chunk_len) in chunk_list_entries(&list)? {
                let chunk = store
                    .get(&chunk_hash)
                    .ok_or(FormatError::MissingNode(chunk_hash))?;
                if chunk.len() != chunk_len as usize {
                    return Err(FormatError::BadNode("chunk length mismatch"));
                }
                out.extend_from_slice(&chunk);
            }
            if out.len() as u64 != size {
                return Err(FormatError::BadEntry("file size mismatch"));
            }
            Ok(out)
        }
        Content::DirRoot(_) => Err(FormatError::BadEntry("not a file")),
    }
}

/// Parse a chunk-list object into (chunk hash, chunk length) pairs.
/// Shared by the read path and the GC mark walk (§4.6).
pub fn chunk_list_entries(list: &[u8]) -> Result<Vec<(Hash, u32)>, FormatError> {
    if list.len() < 5 || list[0] != CHUNK_LIST_MAGIC {
        return Err(FormatError::BadNode("not a chunk list"));
    }
    let count = u32::from_le_bytes(list[1..5].try_into().unwrap()) as usize;
    if list.len() != 5 + count * 36 {
        return Err(FormatError::BadNode("chunk list length mismatch"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let off = 5 + i * 36;
        let chunk_hash = Hash::from_bytes(list[off..off + 32].try_into().unwrap());
        let chunk_len = u32::from_le_bytes(list[off + 32..off + 36].try_into().unwrap());
        out.push((chunk_hash, chunk_len));
    }
    Ok(out)
}

/// Build a complete, validated file entry from raw content.
pub fn make_file_entry(
    store: &mut impl NodeStore,
    params: &ChunkerParams,
    name: &[u8],
    data: &[u8],
    mtime: u64,
    flags: u32,
) -> Entry {
    Entry {
        name: name.to_vec(),
        kind: EntryKind::File,
        flags,
        size: data.len() as u64,
        mtime,
        content: store_file(store, params, data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prolly::MemStore;
    use proptest::prelude::*;

    const TEST_PARAMS: ChunkerParams = ChunkerParams { min: 64, avg: 256, max: 1024 };

    proptest! {
        #[test]
        fn file_roundtrip(data in proptest::collection::vec(any::<u8>(), 0..16384)) {
            let mut store = MemStore::new();
            let content = store_file(&mut store, &TEST_PARAMS, &data);
            match &content {
                Content::Inline(_) => prop_assert!(data.len() <= INLINE_MAX),
                Content::ChunkList(_) => prop_assert!(data.len() > INLINE_MAX),
                Content::DirRoot(_) => prop_assert!(false),
            }
            let back = read_file(&store, &content, data.len() as u64)?;
            prop_assert_eq!(back, data);
        }

        /// Chunk-level dedup: storing the same content twice adds nothing.
        #[test]
        fn dedup_identical_content(data in proptest::collection::vec(any::<u8>(), 513..8192)) {
            let mut store = MemStore::new();
            let c1 = store_file(&mut store, &TEST_PARAMS, &data);
            let objects = store.len();
            let c2 = store_file(&mut store, &TEST_PARAMS, &data);
            prop_assert_eq!(c1, c2);
            prop_assert_eq!(store.len(), objects);
        }
    }
}
