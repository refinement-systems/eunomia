//! Content-addressed storage primitives (spec rev2§4).
//!
//! This crate is pure Rust, host-testable (std), no kernel dependency.
//! It is the primary target for Miri, proptest, and the TLA+ commit-
//! protocol model before M2 implementation begins.
//!
//! Modules:
//!   - `chunk`    — FastCDC gear-hash chunker, target 16–64 KiB
//!   - `hash`     — BLAKE3 chunk addressing
//!   - `prolly`   — per-directory prolly trees, deterministic-TLV entries
//!   - `file`     — inline / chunk-list file content storage
//!   - `tree`     — nested-directory path operations (openat-shaped)
//!
//!   - `dev`      — block-device trait; file/mem/crash-injection backends
//!   - `disk`     — on-disk formats: superblocks, WAL, ref table, index
//!   - `overlay`  — per-ref in-memory overlay (interval maps, rev2§4.3–4.4)
//!   - `store`    — the engine: WAL + flush + A/B commit + recovery + GC
//!   - `gc`       — the mark walk (reachability over the tree, rev2§4.6)
#![cfg_attr(not(feature = "std"), no_std)]
// Clippy is not a CI gate: `assign_op_pattern` fires in `verus!{}`
// verified exec code (`x = x + y` is the form Verus reasons about), `BlockDev` is
// a device-size trait where `is_empty` is meaningless, and the flagged type is a
// cohesive store-internal alias. Refactoring for them would be cosmetic churn.
#![allow(
    clippy::assign_op_pattern,
    clippy::len_without_is_empty,
    clippy::type_complexity
)]

extern crate alloc;

pub mod chunk;
pub mod dev;
pub mod disk;
pub mod file;
pub mod gc;
pub mod hash;
pub mod overlay;
pub mod prolly;
pub mod store;
pub mod tlv;
pub mod tree;

/// Fuzz-only buffer mutators (checksum/chain re-sealing). Compiled only
/// under the `fuzzing` feature so the forgery helpers never reach a real
/// build (spec rev2§6: decoders are cargo-fuzz targets on the host).
#[cfg(feature = "fuzzing")]
pub mod fuzz_support;
