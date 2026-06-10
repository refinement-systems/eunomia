//! Content-addressed storage primitives (spec §4).
//!
//! This crate is pure Rust, host-testable (std), no kernel dependency.
//! It is the primary target for Miri, proptest, and the TLA+ commit-
//! protocol model before M2 implementation begins.
//!
//! Modules (M2 work items):
//!   - `chunk`    — FastCDC gear-hash chunker, target 16–64 KiB
//!   - `hash`     — BLAKE3 chunk addressing
//!   - `prolly`   — nested per-directory prolly trees (Merkle search trees)
//!   - `memtable` — per-ref in-memory overlay (interval map, §4.3–4.4)
//!   - `wal`      — write-ahead log (§4.3 step 2)
//!   - `commit`   — A/B superblock flip + fsync barriers (§4.3 step 4)
//!   - `gc`       — mark-and-sweep from live roots (§4.6)
//!   - `snapshot` — snapshot log rows (§4.7)
//!
//! Key proptest invariants to cover before M2:
//!   - same logical content → same prolly tree root regardless of edit order
//!   - round-trip: serialize then deserialize tree = identity
//!   - after crash (any point in flush/commit), recovered state = committed
//!     roots + WAL replay (matches TLA+ CommitProtocol invariant)

pub mod chunk;
pub mod hash;
pub mod prolly;
