//! On-disk formats (spec §4.2): superblocks, WAL records, the ref table.
//! All hand-defined and little-endian — nothing persistent speaks postcard
//! (§3.7). Decoders are strict and reject trailing bytes.
//!
//! Device layout:
//!   [0,      4096)            superblock slot A
//!   [4096,   8192)            superblock slot B
//!   [8192,   8192 + wal_len)  WAL region (wal_len recorded in the SB)
//!   [chunk_off, dev_len)      chunk store: framed, append-only
//!
//! The generation-checksummed A/B superblock flip is the single atomicity
//! mechanism for the entire system (§4.2).

use crate::hash::Hash;
use crate::prolly::{FormatError, Reader};
use std::collections::BTreeMap;

pub const SB_SIZE: usize = 4096;
pub const SB_A_OFF: u64 = 0;
pub const SB_B_OFF: u64 = 4096;
pub const WAL_OFF: u64 = 8192;

const SB_MAGIC: &[u8; 8] = b"EUNOMIA\0";
const SB_VERSION: u32 = 1;
const SB_BODY: usize = 88; // checksummed prefix

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub generation: u64,
    pub ref_table: Hash,
    pub wal_head: u64,     // byte offset within the WAL region
    pub wal_next_seq: u64, // seq of the first record at/after wal_head
    pub wal_len: u64,
    pub chunk_tail: u64, // byte offset within the chunk region
}

impl Superblock {
    pub fn encode(&self) -> [u8; SB_SIZE] {
        let mut buf = [0u8; SB_SIZE];
        buf[0..8].copy_from_slice(SB_MAGIC);
        buf[8..12].copy_from_slice(&SB_VERSION.to_le_bytes());
        buf[16..24].copy_from_slice(&self.generation.to_le_bytes());
        buf[24..56].copy_from_slice(self.ref_table.as_bytes());
        buf[56..64].copy_from_slice(&self.wal_head.to_le_bytes());
        buf[64..72].copy_from_slice(&self.wal_next_seq.to_le_bytes());
        buf[72..80].copy_from_slice(&self.wal_len.to_le_bytes());
        buf[80..88].copy_from_slice(&self.chunk_tail.to_le_bytes());
        let sum = Hash::of(&buf[..SB_BODY]);
        buf[SB_BODY..SB_BODY + 32].copy_from_slice(sum.as_bytes());
        buf
    }

    /// None = torn or never written; recovery discards it (§4.5).
    pub fn decode(buf: &[u8]) -> Option<Superblock> {
        if buf.len() != SB_SIZE || &buf[0..8] != SB_MAGIC {
            return None;
        }
        if u32::from_le_bytes(buf[8..12].try_into().unwrap()) != SB_VERSION {
            return None;
        }
        let sum = Hash::of(&buf[..SB_BODY]);
        if &buf[SB_BODY..SB_BODY + 32] != sum.as_bytes() {
            return None;
        }
        Some(Superblock {
            generation: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
            ref_table: Hash::from_bytes(buf[24..56].try_into().unwrap()),
            wal_head: u64::from_le_bytes(buf[56..64].try_into().unwrap()),
            wal_next_seq: u64::from_le_bytes(buf[64..72].try_into().unwrap()),
            wal_len: u64::from_le_bytes(buf[72..80].try_into().unwrap()),
            chunk_tail: u64::from_le_bytes(buf[80..88].try_into().unwrap()),
        })
    }
}

// ── WAL records ─────────────────────────────────────────────────────────

const WAL_MAGIC: &[u8; 4] = b"WREC";
pub const WAL_HEADER: usize = 4 + 8 + 4 + 32;

/// A logged mutation. Replay must be deterministic, so server-assigned
/// values (mtime) are captured in the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOp {
    Write {
        ref_name: Vec<u8>,
        path: Vec<Vec<u8>>,
        offset: u64,
        mtime: u64,
        data: Vec<u8>,
    },
    Unlink {
        ref_name: Vec<u8>,
        path: Vec<Vec<u8>>,
        mtime: u64,
    },
}

impl WalOp {
    pub fn ref_name(&self) -> &[u8] {
        match self {
            WalOp::Write { ref_name, .. } | WalOp::Unlink { ref_name, .. } => ref_name,
        }
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let put_path = |out: &mut Vec<u8>, path: &[Vec<u8>]| {
            out.push(path.len() as u8);
            for comp in path {
                out.push(comp.len() as u8);
                out.extend_from_slice(comp);
            }
        };
        match self {
            WalOp::Write { ref_name, path, offset, mtime, data } => {
                out.push(1);
                out.push(ref_name.len() as u8);
                out.extend_from_slice(ref_name);
                put_path(&mut out, path);
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&mtime.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(data);
            }
            WalOp::Unlink { ref_name, path, mtime } => {
                out.push(2);
                out.push(ref_name.len() as u8);
                out.extend_from_slice(ref_name);
                put_path(&mut out, path);
                out.extend_from_slice(&mtime.to_le_bytes());
            }
        }
        out
    }

    fn decode_payload(buf: &[u8]) -> Result<WalOp, FormatError> {
        let mut r = Reader { buf, pos: 0 };
        let take_path = |r: &mut Reader| -> Result<Vec<Vec<u8>>, FormatError> {
            let n = r.u8()? as usize;
            let mut path = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.u8()? as usize;
                path.push(r.take(len)?.to_vec());
            }
            Ok(path)
        };
        let op = match r.u8()? {
            1 => {
                let rl = r.u8()? as usize;
                let ref_name = r.take(rl)?.to_vec();
                let path = take_path(&mut r)?;
                let offset = r.u64()?;
                let mtime = r.u64()?;
                let dl = r.u32()? as usize;
                let data = r.take(dl)?.to_vec();
                WalOp::Write { ref_name, path, offset, mtime, data }
            }
            2 => {
                let rl = r.u8()? as usize;
                let ref_name = r.take(rl)?.to_vec();
                let path = take_path(&mut r)?;
                let mtime = r.u64()?;
                WalOp::Unlink { ref_name, path, mtime }
            }
            _ => return Err(FormatError::BadNode("bad wal op tag")),
        };
        if !r.done() {
            return Err(FormatError::BadNode("wal op trailing bytes"));
        }
        Ok(op)
    }

    /// Full record: header (magic, seq, len, checksum-of-payload) + payload.
    pub fn encode_record(&self, seq: u64) -> Vec<u8> {
        let payload = self.encode_payload();
        let mut out = Vec::with_capacity(WAL_HEADER + payload.len());
        out.extend_from_slice(WAL_MAGIC);
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(Hash::of(&payload).as_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Parse one record at the start of `buf`. Returns (seq, op, record
    /// length). None = no valid record here (torn tail or end of log —
    /// either way replay stops, §4.5: such records were never acked).
    pub fn decode_record(buf: &[u8]) -> Option<(u64, WalOp, usize)> {
        if buf.len() < WAL_HEADER || &buf[0..4] != WAL_MAGIC {
            return None;
        }
        let seq = u64::from_le_bytes(buf[4..12].try_into().unwrap());
        let len = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
        if buf.len() < WAL_HEADER + len {
            return None;
        }
        let payload = &buf[WAL_HEADER..WAL_HEADER + len];
        if Hash::of(payload).as_bytes() != &buf[16..48] {
            return None;
        }
        let op = WalOp::decode_payload(payload).ok()?;
        Some((seq, op, WAL_HEADER + len))
    }
}

// ── Ref table (§4.1, §4.7) ──────────────────────────────────────────────

const REFT_MAGIC: &[u8; 4] = b"REFT";

pub const CLASS_KEEP: u8 = 0;
pub const CLASS_AUTO: u8 = 1;
pub const CLASS_EPHEMERAL: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub root: Hash,
    /// Storage-cap revocation generation (§2.2) — not the superblock
    /// generation. Bumping it lazily invalidates all outstanding handles.
    pub generation: u64,
    pub next_snap_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapRow {
    pub id: u64,
    pub root: Hash,
    pub timestamp: u64,
    pub provenance: Vec<u8>,
    pub parent: Option<u64>,
    pub message: Vec<u8>,
    pub class: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefTable {
    pub refs: BTreeMap<Vec<u8>, RefEntry>,
    /// (ref name, snapshot id) → row. Snapshot identity is the per-ref
    /// sequence number, never a hash (§4.7).
    pub snaps: BTreeMap<(Vec<u8>, u64), SnapRow>,
    pub tags: BTreeMap<Vec<u8>, (Vec<u8>, u64)>,
}

impl RefTable {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(REFT_MAGIC);
        out.extend_from_slice(&(self.refs.len() as u32).to_le_bytes());
        for (name, e) in &self.refs {
            out.push(name.len() as u8);
            out.extend_from_slice(name);
            out.extend_from_slice(e.root.as_bytes());
            out.extend_from_slice(&e.generation.to_le_bytes());
            out.extend_from_slice(&e.next_snap_id.to_le_bytes());
        }
        out.extend_from_slice(&(self.snaps.len() as u32).to_le_bytes());
        for ((ref_name, _), s) in &self.snaps {
            out.push(ref_name.len() as u8);
            out.extend_from_slice(ref_name);
            out.extend_from_slice(&s.id.to_le_bytes());
            out.extend_from_slice(s.root.as_bytes());
            out.extend_from_slice(&s.timestamp.to_le_bytes());
            out.push(s.provenance.len() as u8);
            out.extend_from_slice(&s.provenance);
            out.extend_from_slice(&s.parent.unwrap_or(u64::MAX).to_le_bytes());
            out.extend_from_slice(&(s.message.len() as u16).to_le_bytes());
            out.extend_from_slice(&s.message);
            out.push(s.class);
        }
        out.extend_from_slice(&(self.tags.len() as u32).to_le_bytes());
        for (name, (ref_name, snap_id)) in &self.tags {
            out.push(name.len() as u8);
            out.extend_from_slice(name);
            out.push(ref_name.len() as u8);
            out.extend_from_slice(ref_name);
            out.extend_from_slice(&snap_id.to_le_bytes());
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<RefTable, FormatError> {
        let mut r = Reader { buf, pos: 0 };
        if r.take(4)? != REFT_MAGIC {
            return Err(FormatError::BadNode("not a ref table"));
        }
        let mut t = RefTable::default();
        let nrefs = r.u32()?;
        for _ in 0..nrefs {
            let nl = r.u8()? as usize;
            let name = r.take(nl)?.to_vec();
            let root = r.hash()?;
            let generation = r.u64()?;
            let next_snap_id = r.u64()?;
            t.refs.insert(name, RefEntry { root, generation, next_snap_id });
        }
        let nsnaps = r.u32()?;
        for _ in 0..nsnaps {
            let rl = r.u8()? as usize;
            let ref_name = r.take(rl)?.to_vec();
            let id = r.u64()?;
            let root = r.hash()?;
            let timestamp = r.u64()?;
            let pl = r.u8()? as usize;
            let provenance = r.take(pl)?.to_vec();
            let parent_raw = r.u64()?;
            let ml = r.u16()? as usize;
            let message = r.take(ml)?.to_vec();
            let class = r.u8()?;
            if class > CLASS_EPHEMERAL {
                return Err(FormatError::BadNode("bad retention class"));
            }
            t.snaps.insert(
                (ref_name, id),
                SnapRow {
                    id,
                    root,
                    timestamp,
                    provenance,
                    parent: (parent_raw != u64::MAX).then_some(parent_raw),
                    message,
                    class,
                },
            );
        }
        let ntags = r.u32()?;
        for _ in 0..ntags {
            let nl = r.u8()? as usize;
            let name = r.take(nl)?.to_vec();
            let rl = r.u8()? as usize;
            let ref_name = r.take(rl)?.to_vec();
            let snap_id = r.u64()?;
            t.tags.insert(name, (ref_name, snap_id));
        }
        if !r.done() {
            return Err(FormatError::BadNode("ref table trailing bytes"));
        }
        Ok(t)
    }
}

// ── Chunk frames ────────────────────────────────────────────────────────

pub const CHUNK_MAGIC: &[u8; 4] = b"CHNK";
pub const CHUNK_HEADER: usize = 4 + 4 + 8 + 32;

/// Frame a chunk for the append-only store: magic, length, birth
/// generation (§4.2 — the GC epoch hook), content hash, data.
pub fn encode_chunk_frame(data: &[u8], birth_gen: u64, hash: &Hash) -> Vec<u8> {
    let mut out = Vec::with_capacity(CHUNK_HEADER + data.len());
    out.extend_from_slice(CHUNK_MAGIC);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&birth_gen.to_le_bytes());
    out.extend_from_slice(hash.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Parse a frame header. Returns (data length, birth gen, hash).
pub fn decode_chunk_header(buf: &[u8]) -> Option<(usize, u64, Hash)> {
    if buf.len() < CHUNK_HEADER || &buf[0..4] != CHUNK_MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
    let birth = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let hash = Hash::from_bytes(buf[16..48].try_into().unwrap());
    Some((len, birth, hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_roundtrip_and_tearing() {
        let sb = Superblock {
            generation: 7,
            ref_table: Hash::of(b"rt"),
            wal_head: 100,
            wal_next_seq: 42,
            wal_len: 65536,
            chunk_tail: 9999,
        };
        let buf = sb.encode();
        assert_eq!(Superblock::decode(&buf), Some(sb));
        // Any single-byte corruption in the body must invalidate it.
        for i in [0usize, 17, 30, 60, 70, 85, 100] {
            let mut torn = buf;
            torn[i] ^= 0xFF;
            assert_eq!(Superblock::decode(&torn), None, "byte {i}");
        }
    }

    #[test]
    fn wal_record_roundtrip_and_torn_tail() {
        let op = WalOp::Write {
            ref_name: b"main".to_vec(),
            path: vec![b"etc".to_vec(), b"conf".to_vec()],
            offset: 512,
            mtime: 1234,
            data: b"hello".to_vec(),
        };
        let rec = op.encode_record(9);
        let (seq, parsed, len) = WalOp::decode_record(&rec).unwrap();
        assert_eq!((seq, len), (9, rec.len()));
        assert_eq!(parsed, op);
        // Truncated and corrupted records are silently rejected.
        assert!(WalOp::decode_record(&rec[..rec.len() - 1]).is_none());
        let mut bad = rec.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(WalOp::decode_record(&bad).is_none());
    }

    #[test]
    fn ref_table_roundtrip() {
        let mut t = RefTable::default();
        t.refs.insert(
            b"main".to_vec(),
            RefEntry { root: Hash::of(b"r"), generation: 3, next_snap_id: 2 },
        );
        t.snaps.insert(
            (b"main".to_vec(), 1),
            SnapRow {
                id: 1,
                root: Hash::of(b"s"),
                timestamp: 99,
                provenance: b"session=test".to_vec(),
                parent: None,
                message: b"first".to_vec(),
                class: CLASS_KEEP,
            },
        );
        t.tags.insert(b"v1".to_vec(), (b"main".to_vec(), 1));
        let enc = t.encode();
        assert_eq!(RefTable::decode(&enc), Ok(t));
    }
}
