//! The non-blocking IPC primitives (plan `doc/plans/2_ipc.md` §4.1) — "what
//! every server sees": a typed `Message` (256-byte payload + 4 cap slots, §3.1)
//! and an `Endpoint` that bundles a channel handle with a `Transport`, with cap
//! marshalling and null-slot tolerance (§3.4).
//!
//! This is the typed layer over the byte-level `Transport` seam (`transport.rs`).
//! It is generic over `T: Transport`, so production drives `SyscallTransport` and
//! the Shuttle/Loom harnesses drive `ModelTransport` — the same `send_nb`/
//! `recv_nb` code, checked over a model.
//!
//! The non-blocking `send_nb`/`recv_nb` are strictly non-blocking
//! (`Full`/`Empty`/`NoSlot`); the blocking + bounded-retry sends below (§4.3)
//! layer backpressure over them using the reactor's writable signal.

use crate::reactor::{Reactor, Signals};
use crate::sys::SLOT_NONE;
use crate::transport::{Chan, RecvErr, SendErr, Transport};

/// Maximum inline payload, in bytes (§3.1). Larger data travels through a
/// per-session bulk window (§3.1), out of scope for these primitives.
pub const MAX_PAYLOAD: usize = 256;

/// A fixed-format IPC message: inline payload + up to 4 capability slots (§3.1).
///
/// `caps[i]` is a cspace **slot index** (`u32`), or `None` for an empty slot:
/// - on **send**, `Some(slot)` moves that cap out of the sender's cspace (§3.4);
/// - on **recv**, the caller pre-sets `Some(dest)` for each slot it is willing
///   to receive a cap into, and after the call `caps[i]` is `Some(dest)` iff a
///   cap actually arrived there, `None` otherwise.
///
/// Receivers must **tolerate `None`** even where they expected a cap: a sender
/// can lie, and revocation may have emptied a queued slot in flight (§3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    pub payload: [u8; MAX_PAYLOAD],
    pub payload_len: u16,
    pub caps: [Option<u32>; 4],
}

impl Default for Message {
    fn default() -> Self {
        Message::new()
    }
}

impl Message {
    /// An empty message (zero-length payload, no caps).
    pub const fn new() -> Message {
        Message { payload: [0u8; MAX_PAYLOAD], payload_len: 0, caps: [None; 4] }
    }

    /// A payload-only message. `data` must fit in `MAX_PAYLOAD`.
    pub fn bytes(data: &[u8]) -> Message {
        let mut m = Message::new();
        let n = data.len();
        debug_assert!(n <= MAX_PAYLOAD, "payload exceeds MAX_PAYLOAD");
        m.payload[..n].copy_from_slice(data);
        m.payload_len = n as u16;
        m
    }

    /// The populated payload prefix.
    pub fn payload(&self) -> &[u8] {
        &self.payload[..self.payload_len as usize]
    }

    /// Marshal `caps` to the kernel's `[u32; 4]` slot array (`SLOT_NONE` for an
    /// empty slot), or `None` when every slot is empty (so the syscall layer can
    /// skip cap handling entirely).
    fn cap_slots(&self) -> Option<[u32; 4]> {
        if self.caps.iter().all(Option::is_none) {
            return None;
        }
        let mut slots = [SLOT_NONE; 4];
        for (i, c) in self.caps.iter().enumerate() {
            if let Some(s) = c {
                slots[i] = *s;
            }
        }
        Some(slots)
    }
}

/// A non-blocking channel endpoint: a `Chan` handle bound to a `Transport`
/// (§4.1). The unit a server holds and the reactor (§4.2) will build on.
#[derive(Debug, Clone, Copy)]
pub struct Endpoint<'t, T: Transport> {
    transport: &'t T,
    chan: Chan,
}

impl<'t, T: Transport> Endpoint<'t, T> {
    pub fn new(transport: &'t T, chan: Chan) -> Endpoint<'t, T> {
        Endpoint { transport, chan }
    }

    /// The channel handle this endpoint wraps.
    pub fn chan(&self) -> Chan {
        self.chan
    }

    /// Non-blocking send (§3.3): `Err(SendErr::Full)` when the queue is full —
    /// the message is never dropped (a dropped message could carry a cap, §3.4).
    /// Any `caps` move out of the sender's cspace on success.
    pub fn send_nb(&self, msg: &Message) -> Result<(), SendErr> {
        let slots = msg.cap_slots();
        self.transport.send_nb(self.chan, msg.payload(), slots.as_ref())
    }

    /// Non-blocking receive (§3.3) into `msg`. `Err(RecvErr::Empty)` when the
    /// queue is empty; `Err(RecvErr::NoSlot)` when the receiver lacks a free
    /// cspace slot — the message **stays queued**, so make room and retry (§3.4).
    ///
    /// On input, `msg.caps[i] = Some(dest)` offers a dest slot for an incoming
    /// cap; on success each `caps[i]` is left `Some(dest)` iff a cap landed
    /// there and set to `None` otherwise (null-slot tolerance, §3.4).
    pub fn recv_nb(&self, msg: &mut Message) -> Result<(), RecvErr> {
        let dests = msg.cap_slots();
        let ok = self.transport.recv_nb(self.chan, &mut msg.payload, dests.as_ref())?;
        msg.payload_len = ok.len as u16;
        for i in 0..4 {
            if ok.cap_mask & (1 << i) == 0 {
                msg.caps[i] = None;
            }
        }
        Ok(())
    }

    /// Blocking send over backpressure (§4.3): on `Full`, wait for the channel's
    /// writable signal (the receiver draining a slot) and retry. Never drops the
    /// message; returns any non-`Full` error (`Closed`, …) immediately.
    ///
    /// `reactor` must already have this channel registered for
    /// `Signals::WRITABLE` (register once at setup — registering per call would
    /// exhaust the reactor's bits). It is treated as the **sender's backpressure
    /// reactor**: `wait()` results are consumed as writable wakeups, so a
    /// multiplexing server should instead fold backpressure into its own
    /// `wait`/dispatch loop. Lost-wakeup safety is the reactor's (harness #1):
    /// `notif_wait` checks the accumulated word before sleeping, so a drain that
    /// races the wait is never slept through.
    pub fn send_blocking<'r>(
        &self,
        reactor: &mut Reactor<'r, T>,
        msg: &Message,
    ) -> Result<(), SendErr> {
        debug_assert!(Signals::WRITABLE.writable());
        loop {
            match self.send_nb(msg) {
                Ok(()) => return Ok(()),
                Err(SendErr::Full) => {
                    let _ = reactor.wait();
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Bounded-retry send (§4.3): like [`send_blocking`](Self::send_blocking) but
    /// waits for writability at most `max_waits` times before giving up with
    /// `Err(SendErr::Full)`. Still never drops — `Full` means "not sent".
    pub fn send_retry<'r>(
        &self,
        reactor: &mut Reactor<'r, T>,
        msg: &Message,
        max_waits: u32,
    ) -> Result<(), SendErr> {
        let mut waits = 0;
        loop {
            match self.send_nb(msg) {
                Ok(()) => return Ok(()),
                Err(SendErr::Full) => {
                    if waits == max_waits {
                        return Err(SendErr::Full);
                    }
                    waits += 1;
                    let _ = reactor.wait();
                }
                Err(e) => return Err(e),
            }
        }
    }
}
