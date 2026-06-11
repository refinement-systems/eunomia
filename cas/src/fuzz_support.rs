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

use crate::disk::{SB_BODY, SB_MAGIC, SB_SIZE, SB_VERSION, WAL_HEADER, WAL_MAGIC};
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
    let mut off = 0usize;
    while off + WAL_HEADER <= region.len() {
        region[off..off + 4].copy_from_slice(WAL_MAGIC);
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
