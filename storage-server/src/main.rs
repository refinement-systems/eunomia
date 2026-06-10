//! Storage server — userspace process holding the virtio-blk cap (spec §4).
//!
//! Runs on macOS (for host testing) or on the OS (M2+).
//! Owns the session/handle table; all storage caps are handles in this table.
//!
//! M2 work items:
//!   - Session management: open/close sessions, per-session handle tables
//!   - Handle operations: read, write, open_child, close, snapshot, sync
//!   - Claim-ticket minting and redemption (spec §2.4)
//!   - Memtable + flush + commit (spec §4.3–4.4) backed by cas crate
//!   - Crash recovery (spec §4.5)
//!   - WAL: durability before flush (spec §4.3 step 2)
//!
//! The TLA+ CommitProtocol model must be checked BEFORE M2 implementation.

fn main() {
    todo!("M2: storage server main loop")
}
