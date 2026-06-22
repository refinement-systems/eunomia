//! File content storage (spec rev1§4.1, rev1§4.9): content ≤ INLINE_MAX lives
//! inline in the directory entry; larger content is FastCDC-chunked and
//! referenced through a chunk-list object. The inline rule is a pure
//! function of content, preserving canonical form.

use crate::chunk::{boundaries, ChunkerParams};
use crate::hash::Hash;
use crate::prolly::{Content, Entry, EntryKind, FormatError, NodeStore, INLINE_MAX};
use alloc::vec;
use alloc::vec::Vec;

/// Chunk-list objects share the content-addressed keyspace with tree nodes;
/// the leading byte keeps the decoders from confusing them (tree nodes
/// start with their level, capped well below this).
const CHUNK_LIST_MAGIC: u8 = 0xC1;

pub fn store_file(store: &mut impl NodeStore, params: &ChunkerParams, data: &[u8]) -> Content {
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
/// Shared by the read path and the GC mark walk (rev1§4.6).
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

/// Re-chunk only the neighborhood an edit disturbed (rev1§4.3 step 3), reusing
/// the untouched prefix/suffix chunks of `old`'s chunk list verbatim. The
/// returned `Content` is byte-for-byte the canonical chunking of `new` — equal
/// to `store_file(store, params, new)` (history-independent canonical form,
/// rev1§4.1) — but only the few chunks the edit touched are hashed and stored,
/// not the whole file (so a 200-byte edit in a 1 GiB file yields ~2–4 new
/// chunks). Falls back to whole-file `store_file` whenever there is nothing to
/// reuse: the new content inlines, or `old` was not itself a chunk list.
///
/// `old_bytes` is the pre-edit content materialized (used only to find the
/// unchanged trailing run); `new` is the post-edit content; `first_dirty` is
/// the offset of the first byte the overlay changed.
///
/// Correctness rests on the chunker restarting its gear fingerprint at each
/// chunk start (`chunk::find_boundary`): a cut depends only on bytes since the
/// previous cut, so resuming the chunker at an old (hence canonical) boundary
/// reproduces the canonical cuts forward, and unchanged prefix/suffix chunks
/// are themselves canonical cuts of `new`.
pub fn store_file_neighborhood(
    store: &mut impl NodeStore,
    params: &ChunkerParams,
    old: &Content,
    old_bytes: &[u8],
    new: &[u8],
    first_dirty: u64,
) -> Content {
    if new.len() <= INLINE_MAX {
        return Content::Inline(new.to_vec());
    }
    // Nothing to reuse unless the old content was itself a readable chunk list.
    let Content::ChunkList(old_hash) = old else {
        return store_file(store, params, new);
    };
    let Some(list) = store.get(old_hash) else {
        return store_file(store, params, new);
    };
    let Ok(old_chunks) = chunk_list_entries(&list) else {
        return store_file(store, params, new);
    };

    // Cumulative old boundaries: `old_cut[k]` is the start of chunk `k`; the
    // last entry equals the old length.
    let mut old_cut = Vec::with_capacity(old_chunks.len() + 1);
    let mut acc = 0u64;
    old_cut.push(0);
    for (_, len) in &old_chunks {
        acc += *len as u64;
        old_cut.push(acc);
    }
    let old_len = acc;
    let new_len = new.len() as u64;
    // Overlay writes never shrink or move bytes, so `new` is at least as long
    // as `old` and `delta >= 0`; computed signed for the index arithmetic.
    let delta = new_len as i64 - old_len as i64;

    // Prefix: keep every chunk before the one holding `first_dirty`, then back
    // up one more chunk (rev1§4.3) and resume the chunker there.
    let containing = old_cut
        .partition_point(|&c| c <= first_dirty)
        .saturating_sub(1);
    let resume_idx = containing.saturating_sub(1);
    let resume = old_cut[resume_idx] as usize;

    // Longest common suffix: every byte past it is unchanged content, shifted
    // by `delta`, so the old chunks tile it once the chunker realigns.
    let common = common_suffix_len(new, old_bytes);
    let s_new = new_len - common as u64;

    let mut refs: Vec<(Hash, u32)> = Vec::new();
    refs.extend_from_slice(&old_chunks[..resume_idx]);

    let mut pos = resume;
    loop {
        // `pos` is an emitted boundary. Once it sits past the last edit and maps
        // onto an old boundary, the rest of the old chunk list tiles the
        // identical tail — splice it in and stop. `pos == new_len` always lands
        // here (`old_len` is a boundary), guaranteeing termination.
        if pos as u64 >= s_new {
            let old_off = pos as i64 - delta;
            if old_off >= 0 {
                if let Ok(j) = old_cut.binary_search(&(old_off as u64)) {
                    refs.extend_from_slice(&old_chunks[j..]);
                    break;
                }
            }
        }
        let cut = pos + crate::chunk::next_cut(params, &new[pos..]);
        let chunk = &new[pos..cut];
        refs.push((store.put(chunk), chunk.len() as u32));
        pos = cut;
    }

    // Encode the spliced chunk list — identical format to `store_file`.
    let mut out = vec![CHUNK_LIST_MAGIC];
    out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    for (hash, len) in &refs {
        out.extend_from_slice(hash.as_bytes());
        out.extend_from_slice(&len.to_le_bytes());
    }
    Content::ChunkList(store.put(&out))
}

/// Length of the longest common suffix of `a` and `b`.
fn common_suffix_len(a: &[u8], b: &[u8]) -> usize {
    let (mut ia, mut ib, mut k) = (a.len(), b.len(), 0);
    while ia > 0 && ib > 0 && a[ia - 1] == b[ib - 1] {
        ia -= 1;
        ib -= 1;
        k += 1;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prolly::MemStore;
    use proptest::prelude::*;

    const TEST_PARAMS: ChunkerParams = ChunkerParams {
        min: 64,
        avg: 256,
        max: 1024,
    };

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
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

        /// Canonical-form symmetry, the chunker side (rev1§4.1; B13 Design
        /// decision 4): the cut set is a pure function of the content, and
        /// `store_file`'s inline-vs-chunk selection is content-determined (the
        /// INLINE_MAX rule) — same content ⇒ same chunks ⇒ same `Content`,
        /// regardless of edit history, mirroring the prolly canonical-form sweep.
        #[test]
        fn chunker_selection_symmetry(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            // `boundaries` is a pure function of the data.
            prop_assert_eq!(
                boundaries(&TEST_PARAMS, &data),
                boundaries(&TEST_PARAMS, &data)
            );

            // `store_file` selection is content-determined: same data ⇒ equal
            // `Content`, and the variant follows the INLINE_MAX rule.
            let mut s1 = MemStore::new();
            let mut s2 = MemStore::new();
            let c1 = store_file(&mut s1, &TEST_PARAMS, &data);
            let c2 = store_file(&mut s2, &TEST_PARAMS, &data);
            prop_assert_eq!(&c1, &c2);
            match &c1 {
                Content::Inline(_) => prop_assert!(data.len() <= INLINE_MAX),
                Content::ChunkList(_) => prop_assert!(data.len() > INLINE_MAX),
                Content::DirRoot(_) => prop_assert!(false, "store_file never yields a DirRoot"),
            }
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

        /// Neighborhood re-chunk is behavior-preserving (M-7, the load-bearing
        /// guard): for an arbitrary edit to a multi-chunk file, the spliced
        /// result equals the canonical whole-file chunking byte-for-byte (same
        /// `Content` ⇒ same chunk-list hash ⇒ history-independent canonical
        /// form, rev1§4.1) and reads back as the new content.
        #[test]
        fn neighborhood_matches_whole_file(
            base in proptest::collection::vec(any::<u8>(), 1024..16384),
            edit_off in 0usize..16384,
            edit in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut store = MemStore::new();
            let old = store_file(&mut store, &TEST_PARAMS, &base);
            let off = edit_off.min(base.len());
            let new = apply_edit(&base, off, &edit);

            let nb = store_file_neighborhood(&mut store, &TEST_PARAMS, &old, &base, &new, off as u64);
            let mut fresh = MemStore::new();
            let whole = store_file(&mut fresh, &TEST_PARAMS, &new);
            prop_assert_eq!(nb.clone(), whole);

            let back = read_file(&store, &nb, new.len() as u64)?;
            prop_assert_eq!(back, new);
        }
    }

    /// Overwrite `edit` into `base` at `off`, extending (zero-filling) if it
    /// runs past the end — the only shapes the overlay produces (writes never
    /// shrink or move bytes).
    fn apply_edit(base: &[u8], off: usize, edit: &[u8]) -> Vec<u8> {
        let mut new = base.to_vec();
        let end = off + edit.len();
        if new.len() < end {
            new.resize(end, 0);
        }
        new[off..end].copy_from_slice(edit);
        new
    }

    /// Deterministic pseudo-random bytes (LCG) — same generator the chunker's
    /// self-synchronization test uses, so realignment behaves the same.
    fn pseudo_random(n: usize, mut state: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    /// A `NodeStore` that counts `put` calls — the number of chunks hashed.
    struct CountingStore {
        inner: MemStore,
        puts: usize,
    }

    impl CountingStore {
        fn new() -> Self {
            Self {
                inner: MemStore::new(),
                puts: 0,
            }
        }
    }

    impl NodeStore for CountingStore {
        fn put(&mut self, bytes: &[u8]) -> Hash {
            self.puts += 1;
            self.inner.put(bytes)
        }
        fn get(&self, hash: &Hash) -> Option<Vec<u8>> {
            self.inner.get(hash)
        }
    }

    /// Write-amplification (M-7): a one-byte edit in a many-chunk file re-hashes
    /// only the disturbed neighborhood — O(edit), not O(file). Deterministic
    /// (fixed seed) so the realignment count is stable. A perf-metric test over
    /// a large file; the splice arithmetic itself is the proptest's Miri
    /// witness, so this is skipped under the interpreted-BLAKE3 Miri run.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn neighborhood_rechunks_only_the_edit() {
        let base = pseudo_random(128 * 1024, 0xC0FFEE);
        let mut store = CountingStore::new();
        let old = store_file(&mut store, &TEST_PARAMS, &base);
        let total_chunks = match &old {
            Content::ChunkList(h) => chunk_list_entries(&store.get(h).unwrap()).unwrap().len(),
            _ => panic!("expected a chunk list for a 128 KiB file"),
        };
        assert!(
            total_chunks > 50,
            "test needs a many-chunk file, got {total_chunks}"
        );

        // Flip a single byte in the middle.
        let off = base.len() / 2;
        let mut new = base.clone();
        new[off] ^= 0xFF;

        store.puts = 0;
        let nb = store_file_neighborhood(&mut store, &TEST_PARAMS, &old, &base, &new, off as u64);
        let nb_puts = store.puts;

        // Same canonical result as a full re-chunk (in a fresh store).
        let mut fresh = MemStore::new();
        let whole = store_file(&mut fresh, &TEST_PARAMS, &new);
        assert_eq!(
            nb, whole,
            "neighborhood re-chunk diverged from canonical form"
        );

        // Only a handful of chunks re-hashed (here 3: two fresh chunks around
        // the edit + the chunk-list object), bounded and independent of file
        // size — the rev1§4.3 "~2–4 new chunks" behavior, vs `total_chunks` for
        // a whole-file re-chunk.
        assert!(
            nb_puts <= 8,
            "neighborhood hashed {nb_puts} chunks; file has {total_chunks}"
        );
        assert!(
            nb_puts < total_chunks,
            "no savings: {nb_puts} vs {total_chunks}"
        );
    }

    /// Prefix reuse alone guarantees strict savings for an interior edit, even
    /// before CDC realignment reuses the suffix: an edit two-or-more chunks in
    /// always hashes fewer chunks than a whole-file re-chunk. Perf-metric test
    /// over a large file; skipped under Miri (see above).
    #[cfg_attr(miri, ignore)]
    #[test]
    fn neighborhood_reuses_prefix() {
        let base = pseudo_random(64 * 1024, 0x5EED);
        let mut store = CountingStore::new();
        let old = store_file(&mut store, &TEST_PARAMS, &base);
        let old_chunks = match &old {
            Content::ChunkList(h) => chunk_list_entries(&store.get(h).unwrap()).unwrap(),
            _ => panic!("expected a chunk list"),
        };
        let total_chunks = old_chunks.len();
        assert!(total_chunks >= 8);

        // Edit inside the middle chunk: the prefix (the chunks before the one
        // we back up to) is reused outright.
        let mid = total_chunks / 2;
        let mut cut = 0u64;
        for (_, len) in &old_chunks[..mid] {
            cut += *len as u64;
        }
        let off = cut as usize + 1;
        let new = apply_edit(&base, off, &[0xAB, 0xCD, 0xEF]);

        store.puts = 0;
        let nb = store_file_neighborhood(&mut store, &TEST_PARAMS, &old, &base, &new, off as u64);
        let nb_puts = store.puts;

        let mut fresh = MemStore::new();
        let whole = store_file(&mut fresh, &TEST_PARAMS, &new);
        let whole_puts = match &whole {
            Content::ChunkList(h) => chunk_list_entries(&fresh.get(h).unwrap()).unwrap().len() + 1,
            _ => panic!(),
        };
        assert_eq!(nb, whole);
        assert!(
            nb_puts < whole_puts,
            "expected prefix-reuse savings: {nb_puts} vs {whole_puts}"
        );
    }
}
