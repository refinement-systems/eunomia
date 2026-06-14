//! The IPC reactor (plan `doc/plans/2_ipc.md` §4.2; spec §3.6) — the
//! lost-wakeup core. An epoll-shaped `register(source, signals, key)` /
//! `wait() -> (key, signals)` API over a notification word's **bit-groups**.
//! It **owns the "bind, poll once, then wait" discipline**, so no server reaches
//! for a notification bit, and the §3.6 wait-set kernel object is a future O(1)
//! drop-in behind this same API.
//!
//! Lost-wakeup safety has two halves, both modeled by `tla/ipc_reactor` and
//! re-checked on this code by harness #1 (model.rs, Shuttle + Loom):
//!   1. `register` binds the source's events to a bit and then **self-signals**
//!      that bit — the "poll once". It forces the first `wait()` to surface the
//!      source, so a message queued *before* the bind (whose edge signal went
//!      nowhere) is still polled. Without it, the send-before-bind interleaving
//!      deadlocks (the negative control).
//!   2. `wait()` blocks via `Transport::notif_wait`, whose word-check-before-
//!      block (`kcore::notification`'s `wait`) never sleeps through a signal that
//!      already arrived.
//!
//! Generic over `Transport`: production drives `SyscallTransport`, the harnesses
//! drive `ModelTransport`. Single-threaded per process (`wait` takes `&mut`), so
//! the reactor itself holds no locks (§2 of the plan).

use core::ops::BitOr;

use crate::transport::{Chan, Event, Notif, Transport};

/// The events a source can be registered for / reported ready on (§3.3, §3.6).
/// A set of bits; combine with `|`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signals(u8);

impl Signals {
    pub const READABLE: Signals = Signals(1);
    pub const WRITABLE: Signals = Signals(2);
    pub const PEER_CLOSED: Signals = Signals(4);

    pub const fn readable(self) -> bool {
        self.0 & Self::READABLE.0 != 0
    }
    pub const fn writable(self) -> bool {
        self.0 & Self::WRITABLE.0 != 0
    }
    pub const fn peer_closed(self) -> bool {
        self.0 & Self::PEER_CLOSED.0 != 0
    }
}

impl BitOr for Signals {
    type Output = Signals;
    fn bitor(self, rhs: Signals) -> Signals {
        Signals(self.0 | rhs.0)
    }
}

/// An opaque, server-chosen token naming a registered source. The reactor
/// returns it from `wait` so the server dispatches without ever seeing a bit.
pub type Key = usize;

/// `register` failure: the 64-bit word is exhausted, or a `bind` syscall failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterErr {
    /// No free bit — at most 64 sources for the MVP (bit-groups come with scale).
    Full,
    /// A `Transport::bind` returned a kernel error.
    Bind(i64),
}

/// Width of the notification word — the MVP per-thread source limit (§3.6).
const WORD_BITS: usize = 64;

#[derive(Debug, Clone, Copy)]
struct Reg {
    key: Key,
    signals: Signals,
}

/// The reactor (§4.2): waits on one notification multiplexing many sources.
pub struct Reactor<'t, T: Transport> {
    transport: &'t T,
    notif: Notif,
    slots: [Option<Reg>; WORD_BITS],
    next_bit: u32,
    /// Set bits observed by the last `notif_wait` but not yet returned — drained
    /// across `wait` calls so several ready sources surface without re-signaling.
    pending: u64,
}

impl<'t, T: Transport> Reactor<'t, T> {
    /// A reactor over `notif`, the notification all its sources bind into.
    pub fn new(transport: &'t T, notif: Notif) -> Reactor<'t, T> {
        Reactor { transport, notif, slots: [None; WORD_BITS], next_bit: 0, pending: 0 }
    }

    /// Register `source` for `signals`, dispatched as `key`. Binds each requested
    /// event to a freshly-allocated bit, then **self-signals** that bit so the
    /// first `wait` polls the source (the "poll once", catching a pre-bind
    /// message). Idempotent re-registration is not supported — each call consumes
    /// a bit.
    pub fn register(&mut self, source: Chan, signals: Signals, key: Key) -> Result<(), RegisterErr> {
        let bit = self.next_bit as usize;
        if bit >= WORD_BITS {
            return Err(RegisterErr::Full);
        }
        let mask = 1u64 << bit;

        if signals.readable() {
            self.bind(source, Event::Readable, mask)?;
        }
        if signals.writable() {
            self.bind(source, Event::Writable, mask)?;
        }
        if signals.peer_closed() {
            self.bind(source, Event::PeerClosed, mask)?;
        }

        self.slots[bit] = Some(Reg { key, signals });
        self.next_bit += 1;
        // Poll once: surface this source on the first wait, so a message already
        // queued before the bind is not slept through.
        self.transport.notif_signal(self.notif, mask);
        Ok(())
    }

    fn bind(&self, source: Chan, ev: Event, mask: u64) -> Result<(), RegisterErr> {
        self.transport.bind(source, ev, self.notif, mask).map_err(RegisterErr::Bind)
    }

    /// Block until a registered source is ready, returning its `(key, signals)`.
    /// The returned `signals` are the source's *registered* set (a level-drain
    /// hint, not a precise per-event readiness); the caller polls (`recv_nb` for
    /// readable, `send_nb` for writable) and re-`wait`s on a spurious wakeup.
    pub fn wait(&mut self) -> (Key, Signals) {
        loop {
            if self.pending == 0 {
                // notif_wait returns the accumulated word (non-zero) and clears
                // it, blocking only while it is zero — the lost-wakeup guard.
                self.pending = self.transport.notif_wait(self.notif);
            }
            let bit = self.pending.trailing_zeros() as usize;
            self.pending &= !(1u64 << bit);
            if let Some(reg) = self.slots[bit] {
                return (reg.key, reg.signals);
            }
            // A set bit with no registration: ignore and keep draining/waiting.
        }
    }
}
