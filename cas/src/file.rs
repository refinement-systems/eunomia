// SPDX-License-Identifier: 0BSD
//! File content storage (spec rev2§4.1, rev2§4.9): content ≤ INLINE_MAX lives
//! inline in the directory entry; larger content is FastCDC-chunked and
//! referenced through a chunk-list object. The inline rule is a pure
//! function of content, preserving canonical form.
use crate::chunk::{boundaries, ChunkerParams};
use crate::hash::Hash;
use crate::prolly::{Content, Entry, EntryKind, FormatError, NodeStore, TlvErr, INLINE_MAX};
use alloc::vec::Vec;
use vstd::prelude::*;

pub fn store_file(store: &mut impl NodeStore, params: &ChunkerParams, data: &[u8]) -> Content {
    if data.len() <= INLINE_MAX {
        return Content::Inline(data.to_vec());
    }
    let cuts = boundaries(params, data);
    let mut refs: Vec<RawChunkRef> = Vec::with_capacity(cuts.len());
    let mut start = 0;
    for cut in cuts {
        let chunk = &data[start..cut];
        let hash = store.put(chunk);
        refs.push(RawChunkRef {
            hash: *hash.as_bytes(),
            len: chunk.len() as u32,
        });
        start = cut;
    }
    let mut list = Vec::new();
    encode_chunk_list(&refs, &mut list);
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
/// Shared by the read path and the GC mark walk (rev2§4.6).
///
/// The integer/framing half is the verified `decode_chunk_list` (total ∀ bytes,
/// never panics, accepts only a canonical encoding — doc/guidelines/verus.md
/// §8/§9); this thin shell wraps each raw `[u8; 32]` digest back into `Hash`,
/// the one place the crypto type is touched (§9).
pub fn chunk_list_entries(list: &[u8]) -> Result<Vec<(Hash, u32)>, FormatError> {
    let refs = decode_chunk_list(list).map_err(crate::prolly::tlv_err)?;
    Ok(refs
        .into_iter()
        .map(|r| (Hash::from_bytes(r.hash), r.len))
        .collect())
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

/// Re-chunk only the neighborhood an edit disturbed (rev2§4.3 step 3), reusing
/// the untouched prefix/suffix chunks of `old`'s chunk list verbatim. The
/// returned `Content` is byte-for-byte the canonical chunking of `new` — equal
/// to `store_file(store, params, new)` (history-independent canonical form,
/// rev2§4.1) — but only the few chunks the edit touched are hashed and stored,
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
    // up one more chunk (rev2§4.3) and resume the chunker there.
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

    // Encode the spliced chunk list through the same verified layout
    // (`encode_chunk_list`) `store_file` uses — identical bytes.
    let raw_refs: Vec<RawChunkRef> = refs
        .iter()
        .map(|(hash, len)| RawChunkRef {
            hash: *hash.as_bytes(),
            len: *len,
        })
        .collect();
    let mut out = Vec::new();
    encode_chunk_list(&raw_refs, &mut out);
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

// ── Verified chunk-list codec (rev2§4.1/§4.9) ──────────────────────────────
//
// The chunk-list object `[MAGIC][count u32][ count × ([u8;32] hash, u32 len) ]`
// is an on-disk CAS object read by both the file read path and the rev2§4.6 GC
// mark walk, so its decoder is a strict adversarial-input parser. This island
// lifts the integer/framing half into Verus: `decode_chunk_list` is total over
// arbitrary bytes (verifying its body *is* the no-panic theorem) and frames an
// accepted buffer exactly against `chunk_list_bytes`, the one layout spec the
// encoder `encode_chunk_list` also targets — so the plain-Rust shells
// (`chunk_list_entries`/`store_file`/`store_file_neighborhood`) reference a
// single canonical layout. Like `prolly.rs`'s `decode_node`, it works on a
// `Hash`-free image (`[u8; 32]` in place of `Hash`, doc/guidelines/verus.md §9)
// and proves totality + framing only — no injectivity over the digest bytes.
verus! {

/// Chunk-list objects share the content-addressed keyspace with tree nodes; the
/// leading byte keeps the decoders from confusing them (tree nodes start with
/// their level, capped well below this). Inside the macro so the spec can name
/// it (a const outside `verus!{}` is invisible to Verus). `pub(crate)` because
/// the `open` spec body that names it must be at least as visible as the spec.
pub(crate) const CHUNK_LIST_MAGIC: u8 = 0xC1;

/// The `Hash`-free image of one chunk reference — `[u8; 32]` in place of `Hash`
/// so the framing proof never touches the external `Hash` type (§9). The shells
/// do the thin `Hash` wrap/unwrap. `pub(crate)` for the same reason as the const:
/// the `open` specs name it.
pub(crate) struct RawChunkRef {
    pub(crate) hash: [u8; 32],
    pub(crate) len: u32,
}

/// The canonical bytes of one reference: the 32-byte digest then the little-endian
/// chunk length (fixed 36-byte stride). `open` (its body unfolds for the codec
/// proofs), so `pub(crate)` per Verus's open-implies-visible rule.
pub(crate) open spec fn chunk_ref_bytes(r: RawChunkRef) -> Seq<u8> {
    r.hash@ + le_bytes::u32_le(r.len)
}

/// The references concatenated in order. Back-recursive (peels the last ref) so
/// the decode/encode loops restore their running concat invariant in one
/// `lemma_chunk_refs_push` step per pushed ref (mirrors `prolly::entries_bytes`).
pub(crate) open spec fn chunk_refs_bytes(rs: Seq<RawChunkRef>) -> Seq<u8>
    decreases rs.len(),
{
    if rs.len() == 0 {
        Seq::<u8>::empty()
    } else {
        chunk_refs_bytes(rs.drop_last()) + chunk_ref_bytes(rs.last())
    }
}

/// The canonical byte image of a whole chunk-list object:
/// `[MAGIC][count u32][refs…]`. `decode_chunk_list` proves the consumed bytes
/// equal this for every accepted buffer; `encode_chunk_list` produces exactly it.
pub(crate) open spec fn chunk_list_bytes(rs: Seq<RawChunkRef>) -> Seq<u8> {
    seq![CHUNK_LIST_MAGIC] + le_bytes::u32_le(rs.len() as u32) + chunk_refs_bytes(rs)
}

/// One unfold step of [`chunk_refs_bytes`]: appending a ref appends its
/// `chunk_ref_bytes`. The decode/encode loops cite it to restore their running
/// concat invariant after each pushed ref (mirrors `prolly::lemma_entries_push`).
proof fn lemma_chunk_refs_push(rs: Seq<RawChunkRef>, r: RawChunkRef)
    ensures
        chunk_refs_bytes(rs.push(r)) == chunk_refs_bytes(rs) + chunk_ref_bytes(r),
{
    assert(rs.push(r).drop_last() =~= rs);
    assert(rs.push(r).last() == r);
}

/// Decode a stored chunk-list object — `[MAGIC][count u32][refs…]` — into its
/// `Hash`-free image, **total ∀ bytes** (verifying the body *is* the no-panic
/// theorem). The whole buffer must be consumed (a chunk list is one stored
/// object; trailing bytes are rejected). For an accepted buffer the consumed
/// bytes equal `chunk_list_bytes` — the canonical framing (totality + framing
/// only, no injectivity over the digests, as `decode_node` does).
fn decode_chunk_list(buf: &[u8]) -> (r: Result<Vec<RawChunkRef>, TlvErr>)
    ensures
        r matches Ok(rs) ==> chunk_list_bytes(rs@) == buf@,
{
    broadcast use vstd::slice::group_slice_axioms;

    let len = buf.len();

    // [MAGIC][count u32]: one `fits(0, 5, len)` covers both `buf[0]` and the
    // `read_u32_le(buf, 1)` four-byte read.
    if !crate::prolly::fits(0, 5, len) {
        return Err(TlvErr::BadNode("not a chunk list"));
    }
    if buf[0] != CHUNK_LIST_MAGIC {
        return Err(TlvErr::BadNode("not a chunk list"));
    }
    let count = le_bytes::read_u32_le(buf, 1);
    assert(buf@.subrange(0, 1) =~= seq![CHUNK_LIST_MAGIC]);

    let mut refs: Vec<RawChunkRef> = Vec::new();
    let mut i: u32 = 0;
    let mut pos: usize = 5;
    assert(buf@.subrange(5, 5) =~= chunk_refs_bytes(refs@));
    while i < count
        invariant
            5 <= pos <= len,
            len == buf@.len(),
            i <= count,
            refs@.len() == i,
            buf@.subrange(0, 1) == seq![CHUNK_LIST_MAGIC],
            buf@.subrange(1, 5) == le_bytes::u32_le(count),
            buf@.subrange(5, pos as int) == chunk_refs_bytes(refs@),
        decreases count - i,
    {
        if !crate::prolly::fits(pos, 36, len) {
            return Err(TlvErr::BadNode("chunk list length mismatch"));
        }
        // One fixed 36-byte ref, read inline (like `decode_node`'s internal-node
        // loop) so the loop's `fits(pos, 36, len)` — `len` an exec `usize` —
        // bounds the `pos + 32` / `pos + 36` offset arithmetic. The byte-indexed
        // readers keep `from_le_bytes`/`try_into` out of the proof (§8).

        let ghost old_refs = refs@;
        let hash = crate::prolly::read_arr32(buf, pos);
        let clen = le_bytes::read_u32_le(buf, pos + 32);
        let rr = RawChunkRef { hash, len: clen };
        refs.push(rr);
        proof {
            crate::prolly::lemma_cat(buf@, pos as int, pos as int + 32, pos as int + 36);
            assert(chunk_ref_bytes(rr) =~= buf@.subrange(pos as int, pos as int + 36));
            lemma_chunk_refs_push(old_refs, rr);
            crate::prolly::lemma_cat(buf@, 5, pos as int, pos as int + 36);
        }
        pos = pos + 36;
        i += 1;
    }
    // `count` refs parsed; the whole buffer must be consumed.
    if pos != len {
        return Err(TlvErr::BadNode("chunk list length mismatch"));
    }
    proof {
        crate::prolly::lemma_cat(buf@, 0, 1, 5);
        crate::prolly::lemma_cat(buf@, 0, 5, len as int);
    }
    assert(refs@.len() == count);
    assert(refs@.len() as u32 == count);
    assert(buf@ =~= buf@.subrange(0, len as int));
    assert(chunk_list_bytes(refs@) =~= buf@);
    Ok(refs)
}

/// Serialize one reference (`[hash u8;32][len u32]`), appended to `out` — the
/// encode half of one 36-byte stride.
fn encode_chunk_ref(out: &mut Vec<u8>, r: &RawChunkRef)
    ensures
        final(out)@ == old(out)@ + chunk_ref_bytes(*r),
{
    crate::prolly::push_arr32(out, &r.hash);
    crate::prolly::push_u32_le(out, r.len);
    assert(out@ =~= old(out)@ + chunk_ref_bytes(*r));
}

/// Serialize a chunk-list object's references to their canonical bytes
/// (`[MAGIC][count u32][refs…]`), appended to `out`. The encode half of the
/// chunk-list round-trip: produces exactly `chunk_list_bytes`, so it and
/// `decode_chunk_list` reference the one layout spec (mirrors
/// `prolly::encode_node_leaf`).
fn encode_chunk_list(refs: &Vec<RawChunkRef>, out: &mut Vec<u8>)
    ensures
        final(out)@ == old(out)@ + chunk_list_bytes(refs@),
{
    out.push(CHUNK_LIST_MAGIC);
    crate::prolly::push_u32_le(out, refs.len() as u32);
    assert(refs@.subrange(0, 0) =~= Seq::<RawChunkRef>::empty());
    let mut i: usize = 0;
    while i < refs.len()
        invariant
            i <= refs@.len(),
            out@ == old(out)@ + seq![CHUNK_LIST_MAGIC] + le_bytes::u32_le(refs@.len() as u32)
                + chunk_refs_bytes(refs@.subrange(0, i as int)),
        decreases refs@.len() - i,
    {
        let ghost prev = refs@.subrange(0, i as int);
        encode_chunk_ref(out, &refs[i]);
        proof {
            lemma_chunk_refs_push(prev, refs@[i as int]);
            assert(refs@.subrange(0, i as int + 1) =~= prev.push(refs@[i as int]));
        }
        i += 1;
    }
    assert(refs@.subrange(0, refs@.len() as int) =~= refs@);
    assert(out@ =~= old(out)@ + chunk_list_bytes(refs@));
}

} // verus!
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
        fn file_roundtrip(
            // Miri interprets blake3 per chunk; a 4 KiB cap still crosses INLINE_MAX
            // and exercises the chunked path. Native keeps the full 16 KiB range.
            data in proptest::collection::vec(any::<u8>(), 0..if cfg!(miri) { 4096 } else { 16384 }),
        ) {
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

        /// Canonical-form symmetry, the chunker side (rev2§4.1): the cut set is
        /// a pure function of the content, and
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

        /// Neighborhood re-chunk is behavior-preserving (the load-bearing
        /// guard): for an arbitrary edit to a multi-chunk file, the spliced
        /// result equals the canonical whole-file chunking byte-for-byte (same
        /// `Content` ⇒ same chunk-list hash ⇒ history-independent canonical
        /// form, rev2§4.1) and reads back as the new content.
        #[test]
        fn neighborhood_matches_whole_file(
            // Miri interprets blake3 per chunk, so cap the base file (still
            // multi-chunk at 4 KiB); the neighborhood==whole-file invariant is
            // size-independent. Native keeps the full 16 KiB range.
            base in proptest::collection::vec(any::<u8>(), 1024..if cfg!(miri) { 4096 } else { 16384 }),
            edit_off in 0usize..if cfg!(miri) { 4096 } else { 16384 },
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

    /// Strict adversarial decode: `chunk_list_entries` (the verified
    /// `decode_chunk_list` plus the thin `Hash` wrap) round-trips a well-formed
    /// buffer and *rejects* every malformed shape — bad magic, short header,
    /// trailing bytes, a truncated final ref, and a count larger than the bytes
    /// supply — without panicking (the decoder is Verus-total over all inputs).
    #[test]
    fn chunk_list_entries_strictness() {
        // A well-formed one-ref object: [MAGIC][count=1][hash][len=7].
        let hash = Hash::of(b"chunk-0");
        let mut good = Vec::new();
        good.push(CHUNK_LIST_MAGIC);
        good.extend_from_slice(&1u32.to_le_bytes());
        good.extend_from_slice(hash.as_bytes());
        good.extend_from_slice(&7u32.to_le_bytes());

        let entries = chunk_list_entries(&good).expect("well-formed chunk list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], (hash, 7u32));

        // Bad magic byte.
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xFF;
        assert!(chunk_list_entries(&bad_magic).is_err());

        // Short header (< 5 bytes) and the empty buffer.
        assert!(chunk_list_entries(&good[..3]).is_err());
        assert!(chunk_list_entries(b"").is_err());

        // One trailing byte past the single ref.
        let mut trailing = good.clone();
        trailing.push(0x00);
        assert!(chunk_list_entries(&trailing).is_err());

        // Final ref truncated by one byte (35 of 36).
        assert!(chunk_list_entries(&good[..good.len() - 1]).is_err());

        // Count claims two refs but only one is present.
        let mut overcount = good.clone();
        overcount[1..5].copy_from_slice(&2u32.to_le_bytes());
        assert!(chunk_list_entries(&overcount).is_err());
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

    /// Write-amplification: a one-byte edit in a many-chunk file re-hashes
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
        // size — the rev2§4.3 "~2–4 new chunks" behavior, vs `total_chunks` for
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
