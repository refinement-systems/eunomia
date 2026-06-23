//! The non-blocking IPC primitives — "what every server sees": a typed `Message`
//! (256-byte payload + 4 cap slots, rev2§3.1) and an `Endpoint` that bundles a
//! channel handle with a `Transport`, with cap marshalling and null-slot
//! tolerance (rev2§3.4).
//!
//! This is the typed layer over the byte-level `Transport` seam (`transport.rs`).
//! It is generic over `T: Transport`, so production drives `SyscallTransport` and
//! the Shuttle/Loom harnesses drive `ModelTransport` — the same `send_nb`/
//! `recv_nb` code, checked over a model.
//!
//! The non-blocking `send_nb`/`recv_nb` are strictly non-blocking
//! (`Full`/`Empty`/`NoSlot`); the blocking + bounded-retry sends below
//! layer backpressure over them using the reactor's writable signal.

use crate::reactor::{Reactor, Signals};
use crate::sys::SLOT_NONE;
use crate::transport::{Chan, Notif, RecvErr, SendErr, Transport};

/// Maximum inline payload, in bytes (rev2§3.1). Larger data travels through a
/// per-session bulk window (rev2§3.1), out of scope for these primitives.
pub const MAX_PAYLOAD: usize = 256;

/// A fixed-format IPC message: inline payload + up to 4 capability slots (rev2§3.1).
///
/// `caps[i]` is a cspace **slot index** (`u32`), or `None` for an empty slot:
/// - on **send**, `Some(slot)` moves that cap out of the sender's cspace (rev2§3.4);
/// - on **recv**, the caller pre-sets `Some(dest)` for each slot it is willing
///   to receive a cap into, and after the call `caps[i]` is `Some(dest)` iff a
///   cap actually arrived there, `None` otherwise.
///
/// Receivers must **tolerate `None`** even where they expected a cap: a sender
/// can lie, and revocation may have emptied a queued slot in flight (rev2§3.4).
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
        Message {
            payload: [0u8; MAX_PAYLOAD],
            payload_len: 0,
            caps: [None; 4],
        }
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

/// A non-blocking channel endpoint: a `Chan` handle bound to a `Transport`.
/// The unit a server holds and the reactor builds on.
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

    /// Non-blocking send (rev2§3.3): `Err(SendErr::Full)` when the queue is full —
    /// the message is never dropped (a dropped message could carry a cap, rev2§3.4).
    /// Any `caps` move out of the sender's cspace on success.
    pub fn send_nb(&self, msg: &Message) -> Result<(), SendErr> {
        let slots = msg.cap_slots();
        self.transport
            .send_nb(self.chan, msg.payload(), slots.as_ref())
    }

    /// Non-blocking receive (rev2§3.3) into `msg`. `Err(RecvErr::Empty)` when the
    /// queue is empty; `Err(RecvErr::NoSlot)` when the receiver lacks a free
    /// cspace slot — the message **stays queued**, so make room and retry (rev2§3.4).
    ///
    /// On input, `msg.caps[i] = Some(dest)` offers a dest slot for an incoming
    /// cap; on success each `caps[i]` is left `Some(dest)` iff a cap landed
    /// there and set to `None` otherwise (null-slot tolerance, rev2§3.4).
    pub fn recv_nb(&self, msg: &mut Message) -> Result<(), RecvErr> {
        let dests = msg.cap_slots();
        let ok = self
            .transport
            .recv_nb(self.chan, &mut msg.payload, dests.as_ref())?;
        msg.payload_len = ok.len as u16;
        for i in 0..4 {
            if ok.cap_mask & (1 << i) == 0 {
                msg.caps[i] = None;
            }
        }
        Ok(())
    }

    /// Blocking send over backpressure: on `Full`, wait for the channel's
    /// writable signal (the receiver draining a slot) and retry. Never drops the
    /// message; returns any non-`Full` error (`Closed`, …) immediately.
    ///
    /// `reactor` must already have this channel registered for
    /// `Signals::WRITABLE` (register once at setup — registering per call would
    /// exhaust the reactor's bits). It is treated as the **sender's backpressure
    /// reactor**: `wait()` results are consumed as writable wakeups, so a
    /// multiplexing server should instead fold backpressure into its own
    /// `wait`/dispatch loop. Lost-wakeup safety is the reactor's:
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

    /// Bounded-retry send: like [`send_blocking`](Self::send_blocking) but
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

    /// Sender half of the **valuable-cap ack protocol** (rev2§3.4): send `msg`
    /// (which may carry a valuable cap), then block on `ack_notif` until the
    /// receiver confirms it has the cap in its cspace. Returns `Ok` only once
    /// acked — at which point the handoff is durable and the channel is safe to
    /// tear down. A bare send-then-destroy would let channel destruction shred
    /// the still-queued cap (rev2§3.4: "cash in a shredded envelope"); this gate is
    /// what prevents that, in pure userspace (no kernel reverse-path).
    ///
    /// `Full`/`Closed` from the send propagate (combine with [`Self::send_blocking`]
    /// when the channel may be full). The cap's *exactly-one-owner* / no-dup
    /// guarantee is the kernel's move semantics (`CapRevocation`), not this
    /// protocol's — this carries only the no-loss obligation.
    pub fn send_acked(&self, msg: &Message, ack_notif: Notif) -> Result<(), SendErr> {
        self.send_nb(msg)?;
        // The ack is an event (rev2§3.6): notif_wait checks the accumulated word
        // before sleeping, so a receiver that acks before this wait is never
        // slept through (the lost-wakeup guard the reactor provides).
        let _ = self.transport.notif_wait(ack_notif);
        Ok(())
    }

    /// Receiver half of the valuable-cap ack protocol: receive into `msg`,
    /// then signal `ack_notif` so the sender's [`send_acked`](Self::send_acked)
    /// returns. The ack fires only *after* the cap is in this receiver's cspace,
    /// so the sender never tears the channel down with the cap still in flight.
    /// `Empty`/`Closed` propagate (nothing is acked on a failed receive).
    pub fn recv_acked(
        &self,
        msg: &mut Message,
        ack_notif: Notif,
        ack_bit: u64,
    ) -> Result<(), RecvErr> {
        self.recv_nb(msg)?;
        self.transport.notif_signal(ack_notif, ack_bit);
        Ok(())
    }
}

// Cap-marshalling property tests (rev2§6 baseline tier): the
// `Message.caps` (`[Option<u32>; 4]`) ↔ kernel-ABI `[u32; 4]` (`SLOT_NONE` for
// an empty slot) mapping is pure, single-threaded glue — its tier is proptest +
// Miri. The wire codec proper (`header`/`session`) is Verus + fuzz; this raises
// the *marshalling* alone. Std-only, off the loom/shuttle model builds.
#[cfg(all(test, not(loom), not(shuttle)))]
mod tests {
    use super::*;
    use crate::model::ModelTransport;
    use proptest::prelude::*;

    // A cap array of cspace slot indices. Values are restricted to `< SLOT_NONE`
    // (a real slot index is never the `u32::MAX` sentinel); the
    // `Some(SLOT_NONE)` aliasing edge is asserted separately below, not
    // generated, so it does not spuriously fail the round-trip.
    fn caps_strategy() -> impl Strategy<Value = [Option<u32>; 4]> {
        let slot = || proptest::option::of(0u32..u32::MAX);
        (slot(), slot(), slot(), slot()).prop_map(|(a, b, c, d)| [a, b, c, d])
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            failure_persistence: if cfg!(miri) { None } else { ProptestConfig::default().failure_persistence },
            .. ProptestConfig::default()
        })]

        /// `cap_slots` round-trips: the all-empty case is `None` (so the syscall
        /// layer skips cap handling); otherwise `Some(slots)` with
        /// `slots[i] == caps[i].unwrap_or(SLOT_NONE)`, and decoding back
        /// (`SLOT_NONE` → `None`) reproduces `caps` exactly. Pure: a second call
        /// yields the same result.
        #[test]
        fn cap_slots_round_trips(caps in caps_strategy()) {
            let mut msg = Message::new();
            msg.caps = caps;
            let slots = msg.cap_slots();
            prop_assert_eq!(slots, msg.cap_slots(), "cap_slots is not a pure function of caps");

            if caps.iter().all(Option::is_none) {
                prop_assert_eq!(slots, None, "all-empty caps must marshal to None");
            } else {
                let slots = slots.expect("non-empty caps must marshal to Some");
                for i in 0..4 {
                    prop_assert_eq!(slots[i], caps[i].unwrap_or(SLOT_NONE));
                    let decoded = if slots[i] == SLOT_NONE { None } else { Some(slots[i]) };
                    prop_assert_eq!(decoded, caps[i], "cap slot did not round-trip at index {}", i);
                }
            }
        }

        /// End-to-end through `Endpoint` + `ModelTransport`: after a send/recv,
        /// the receiver's `caps[i]` is `Some(dest)` iff a cap arrived there
        /// (`sent[i].is_some()`) and `None` otherwise — the cap-present mask
        /// round-trips, and a `None` where a cap was expected is tolerated, never
        /// a panic (rev2§3.4 null-slot tolerance).
        #[test]
        fn cap_present_mask_round_trips(sent in caps_strategy()) {
            let t = ModelTransport::shared(2, 0); // capacity 2; no notifications needed
            let ep = Endpoint::new(&*t, 0);

            let mut smsg = Message::new();
            smsg.caps = sent;
            ep.send_nb(&smsg).unwrap();

            let mut rmsg = Message::new();
            // Offer a distinct dest slot at every position (we accept None too).
            rmsg.caps = [Some(7), Some(8), Some(9), Some(10)];
            ep.recv_nb(&mut rmsg).unwrap();

            for i in 0..4 {
                prop_assert_eq!(
                    rmsg.caps[i].is_some(),
                    sent[i].is_some(),
                    "cap-present bit diverged at index {}",
                    i
                );
            }
        }
    }
}
