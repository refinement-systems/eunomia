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
//! Two kinds of source register here. [`Reactor::register`] takes a **channel**
//! and is **level-triggered**: it `bind`s the channel's events and self-signals
//! a poll-once so a message queued before the bind still surfaces.
//! [`Reactor::register_bound`] takes an **externally-bound, edge-triggered**
//! source — a thread on-exit/on-fault binding (`thread_bind`, §5.1), a timer, an
//! IRQ — already wired to a caller-chosen bit; it neither binds nor self-signals
//! (a poll-once would fabricate a one-shot event), so lost-wakeup safety there
//! rests on the caller binding before the source can fire plus `wait`'s
//! word-check. The shell's spawn/reap loop is the first `register_bound`
//! consumer; storaged is the first `register` consumer.
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

/// `register`/`register_bound` failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterErr {
    /// No free bit — at most 64 sources for the MVP (bit-groups come with scale).
    Full,
    /// A `Transport::bind` returned a kernel error.
    Bind(i64),
    /// A `register_bound` requested a bit already allocated to another source.
    Taken,
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
    /// Allocated bits. A source owns a bit for life (there is no deregister), so
    /// the lowest clear bit is the next free one — `register` allocates that way,
    /// and `register_bound` claims caller-chosen bits, both recorded here.
    used: u64,
    /// Set bits observed by the last `notif_wait` but not yet returned — drained
    /// across `wait` calls so several ready sources surface without re-signaling.
    pending: u64,
}

impl<'t, T: Transport> Reactor<'t, T> {
    /// A reactor over `notif`, the notification all its sources bind into.
    pub fn new(transport: &'t T, notif: Notif) -> Reactor<'t, T> {
        Reactor { transport, notif, slots: [None; WORD_BITS], used: 0, pending: 0 }
    }

    /// The lowest free bit, marking it allocated; `None` when the 64-bit word is
    /// exhausted (`RegisterErr::Full`).
    fn alloc_bit(&mut self) -> Option<usize> {
        let bit = (!self.used).trailing_zeros() as usize;
        if bit >= WORD_BITS {
            return None;
        }
        self.used |= 1u64 << bit;
        Some(bit)
    }

    /// Register `source` for `signals`, dispatched as `key`. Binds each requested
    /// event to a freshly-allocated bit, then **self-signals** that bit so the
    /// first `wait` polls the source (the "poll once", catching a pre-bind
    /// message). Idempotent re-registration is not supported — each call consumes
    /// a bit.
    pub fn register(&mut self, source: Chan, signals: Signals, key: Key) -> Result<(), RegisterErr> {
        let bit = self.alloc_bit().ok_or(RegisterErr::Full)?;
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
        // Poll once: surface this source on the first wait, so a message already
        // queued before the bind is not slept through.
        self.transport.notif_signal(self.notif, mask);
        Ok(())
    }

    /// Register a source whose events are bound to `mask` **outside** the reactor
    /// and are **edge-triggered**: a thread on-exit/on-fault binding (a
    /// `thread_bind` into the TCB, §5.1), a timer (armed via `sys::timer_arm`),
    /// or an IRQ — anything the kernel signals into this notification at a bit
    /// the caller controls. Each set bit in `mask` dispatches to `key`.
    ///
    /// Unlike [`register`], this does **no** `bind` (it is not a channel event)
    /// and does **no** poll-once self-signal: an edge-triggered source fires
    /// exactly once when the event actually happens, so a fabricated poll-once
    /// would deliver a spurious wakeup (e.g. report a thread dead before it is).
    /// The reactor therefore owns only the bit→key dispatch and the
    /// word-check-before-block half of the lost-wakeup discipline; **the caller
    /// must bind the source before it can fire** (e.g. `SpawnRec::arm` before
    /// `start`), so a `wait` cannot sleep through a signal that already arrived.
    ///
    /// `Err(Taken)` if any requested bit is already allocated.
    pub fn register_bound(&mut self, mask: u64, key: Key) -> Result<(), RegisterErr> {
        if mask & self.used != 0 {
            return Err(RegisterErr::Taken);
        }
        self.used |= mask;
        let mut bits = mask;
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            // Bound sources carry no channel signals; the key alone names them.
            self.slots[bit] = Some(Reg { key, signals: Signals(0) });
        }
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
