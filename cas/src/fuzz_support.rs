//! Fuzz-only mutators (compiled only under the `fuzzing` feature).
//!
//! A coverage-guided mutator cannot forge a BLAKE3 checksum, so a fuzzer
//! pointed at a checksummed structure explores the rejection branch
//! forever — coverage plateaus at "checksum mismatch" and the
//! field-validation logic *behind* the gate is never reached. These
//! helpers re-seal a mutated buffer so the fuzzer's mutations land on the
//! decoded fields instead of bouncing off the integrity check. They are
//! deliberately not part of the normal build: re-sealing arbitrary bytes
//! is exactly the forgery the real system must reject.

use crate::disk::{
    decode_index, encode_index, index_payload_len, Superblock, CHUNK_HEADER, CHUNK_MAGIC, SB_A_OFF,
    SB_BODY, SB_B_OFF, SB_MAGIC, SB_SIZE, SB_VERSION, WAL_HEADER, WAL_MAGIC, WAL_OFF,
};
use crate::hash::Hash;

/// Re-seal a 4 KiB superblock slot so `Superblock::decode` accepts it.
///
/// `decode` gates in order on magic, version, then the body checksum, so a
/// fixup that touched only the checksum field would be a no-op — the magic
/// gate still rejects. This stamps all three, leaving the fuzzer free to
/// drive the *body fields* (generation, wal_len, offsets, …) into mount's
/// recovery logic. The slot tail past the checksum is ignored by `decode`
/// and left as-is.
pub fn fixup_superblock_checksum(block: &mut [u8]) {
    if block.len() < SB_SIZE {
        return;
    }
    block[0..8].copy_from_slice(SB_MAGIC);
    block[8..12].copy_from_slice(&SB_VERSION.to_le_bytes());
    let sum = Hash::of(&block[..SB_BODY]);
    block[SB_BODY..SB_BODY + 32].copy_from_slice(sum.as_bytes());
}

/// Re-seal `region` as a valid chain of WAL records so the replay scanner
/// reaches record *bodies* instead of stopping at the first checksum
/// mismatch. For each record slot it stamps the magic, clamps the
/// attacker-supplied length to what remains (no over-read), and recomputes
/// the payload checksum, then advances. Record sequence numbers are left
/// untouched — `WalOp::decode_record` does not check them; the seq-chain
/// check lives one layer up in `Store::mount`.
pub fn fixup_wal_chain(region: &mut [u8]) {
    fixup_wal_chain_seq(region, None);
}

/// `fixup_wal_chain`, optionally also stamping record sequence numbers
/// continuously from `seq` — mount's replay (unlike `decode_record`) stops
/// at the first seq discontinuity, so a reseal that left attacker seqs in
/// place would rarely get more than one record applied.
pub fn fixup_wal_chain_seq(region: &mut [u8], mut seq: Option<u64>) {
    let mut off = 0usize;
    while off + WAL_HEADER <= region.len() {
        region[off..off + 4].copy_from_slice(WAL_MAGIC);
        if let Some(s) = seq.as_mut() {
            region[off + 4..off + 12].copy_from_slice(&s.to_le_bytes());
            *s = s.wrapping_add(1);
        }
        let max_payload = region.len() - off - WAL_HEADER;
        let mut len = u32::from_le_bytes(region[off + 12..off + 16].try_into().unwrap()) as usize;
        if len > max_payload {
            len = max_payload;
            region[off + 12..off + 16].copy_from_slice(&(len as u32).to_le_bytes());
        }
        let body = off + WAL_HEADER;
        let sum = Hash::of(&region[body..body + len]);
        region[off + 16..off + 48].copy_from_slice(sum.as_bytes());
        off += WAL_HEADER + len;
    }
}

/// Re-seal a whole device image the way a disk-writing adversary would:
/// recompute every checksum and content hash `Store::mount` verifies, but
/// never invent, repair, or clamp a geometry field, and never seal a claim
/// the image cannot physically back (a frame whose claimed payload runs
/// past the end of the image stays broken, exactly as on a real undersized
/// device). Layers, in mount order:
///
///   1. both superblock slots (magic, version, body checksum);
///   2. the winning slot's index frame: magic and payload hash — and, when
///      the payload decodes and maps the superblock's ref-table hash to an
///      extent the image holds, the entry is re-keyed to the extent's true
///      content hash so `read_object`'s verification opens too, cascading
///      the new key back through the index payload and the superblock;
///   3. the WAL chain from the committed head, restamped seq-continuous
///      from `wal_next_seq` so replay walks past the first record.
///
/// Tree nodes below the ref table are left alone: each reseal layer exists
/// to expose the *validation* logic behind a gate, and the node decoders
/// are fuzzed on raw bytes by their own targets.
pub fn reseal_image(img: &mut [u8]) {
    let len = img.len() as u64;
    for slot in [SB_A_OFF as usize, SB_B_OFF as usize] {
        if let Some(block) = img.get_mut(slot..slot + SB_SIZE) {
            fixup_superblock_checksum(block);
        }
    }
    let decode_slot =
        |img: &[u8], off: usize| img.get(off..off + SB_SIZE).and_then(Superblock::decode);
    let (mut sb, slot) = match (
        decode_slot(img, SB_A_OFF as usize),
        decode_slot(img, SB_B_OFF as usize),
    ) {
        (Some(a), Some(b)) if a.generation >= b.generation => (a, SB_A_OFF as usize),
        (Some(_), Some(b)) => (b, SB_B_OFF as usize),
        (Some(a), None) => (a, SB_A_OFF as usize),
        (None, Some(b)) => (b, SB_B_OFF as usize),
        (None, None) => return,
    };
    let Some(chunk_off) = WAL_OFF.checked_add(sb.wal_len) else {
        return;
    };

    let in_image = |start: u64, l: u64| start.checked_add(l).is_some_and(|end| end <= len);
    if let Some(hstart) = chunk_off
        .checked_add(sb.index_off)
        .filter(|&h| in_image(h, CHUNK_HEADER as u64))
    {
        let h = hstart as usize;
        img[h..h + 4].copy_from_slice(CHUNK_MAGIC);
        let ilen = u32::from_le_bytes(img[h + 4..h + 8].try_into().unwrap());
        let p = h + CHUNK_HEADER;
        if in_image(hstart + CHUNK_HEADER as u64, ilen as u64) {
            let n = ilen as usize;
            if let Ok((mut entries, free)) = decode_index(&img[p..p + n]) {
                let extent = entries.get(&sb.ref_table).copied().filter(|e| {
                    chunk_off
                        .checked_add(e.off)
                        .is_some_and(|d| in_image(d, e.len as u64))
                });
                if let Some(e) = extent {
                    let d = (chunk_off + e.off) as usize;
                    let true_hash = Hash::of(&img[d..d + e.len as usize]);
                    let body = index_payload_len(entries.len(), free.len());
                    if true_hash != sb.ref_table && !entries.contains_key(&true_hash) && n >= body {
                        entries.remove(&sb.ref_table);
                        entries.insert(true_hash, e);
                        sb.ref_table = true_hash;
                        // Same counts and pad, so the re-encoding fills the
                        // frame's extent exactly.
                        img[p..p + n].copy_from_slice(&encode_index(&entries, &free, n - body));
                        img[slot..slot + SB_SIZE].copy_from_slice(&sb.encode());
                    }
                }
            }
            let sum = Hash::of(&img[p..p + n]);
            img[h + 16..h + 48].copy_from_slice(sum.as_bytes());
        }
    }

    let wal_end = chunk_off.min(len) as usize;
    if let Some(head) = WAL_OFF
        .checked_add(sb.wal_head)
        .filter(|&o| o < wal_end as u64)
    {
        fixup_wal_chain_seq(&mut img[head as usize..wal_end], Some(sb.wal_next_seq));
    }
}
