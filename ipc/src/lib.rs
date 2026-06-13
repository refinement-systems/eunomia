//! Userspace IPC crate — shared by every server (spec §3.5, §3.7).
//!
//! Responsibilities (M1+):
//!   - Async send/recv over kernel channels
//!   - FULL backpressure and retry
//!   - Valuable-cap ack protocol
//!   - Postcard message (de)serialisation (module-private)
//!   - Lost-wakeup discipline around the notification object
//!
//! The reactor API is epoll-shaped — `register(source, signals, key)` —
//! implemented over notification bit-groups for M1 and upgraded to the
//! kernel wait-set object when that lands (spec §3.6).

#![cfg_attr(not(feature = "std"), no_std)]

pub mod header;
pub mod sys;

/// Kani harnesses (plan §4.7), compiled only under `cargo kani`.
#[cfg(kani)]
mod proofs;

// ── Milestone M1 work items ──────────────────────────────────────────────────

/// Opaque handle to a kernel channel endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel(u64);

/// Fixed message format: 256-byte inline payload + 4 cap slots (spec §3.1).
#[derive(Debug)]
pub struct Message {
    pub payload: [u8; 256],
    pub payload_len: u16,
    pub caps: [Option<u64>; 4],
}

/// Non-blocking send; returns `Err(Full)` when the queue is full (spec §3.3).
pub fn send(_ch: Channel, _msg: &Message) -> Result<(), SendError> {
    todo!("M1: implement kernel send syscall wrapper")
}

/// Non-blocking receive.
pub fn recv(_ch: Channel, _msg: &mut Message) -> Result<(), RecvError> {
    todo!("M1: implement kernel recv syscall wrapper")
}

#[derive(Debug)]
pub enum SendError {
    Full,
}

#[derive(Debug)]
pub enum RecvError {
    Empty,
    NoCspaceSlot,
}
