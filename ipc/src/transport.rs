//! The transport seam (plan `doc/plans/2_ipc.md` §3.1): the kernel IPC surface
//! behind a trait, so the reactor (later phases) is generic over it.
//!
//! - `SyscallTransport` is the production impl over `crate::sys` — zero-sized,
//!   a thin shim over the real `chan_*`/`notif_*` syscalls.
//! - `ModelTransport` (host/test only, `crate::model`) is a deterministic
//!   in-memory kernel so Shuttle/Loom can schedule the communicating processes
//!   over it (the IPC analogue of `kcore`'s `Env`/`Hal` seam).
//!
//! Move semantics, FIFO delivery, and the accumulate-and-clear notification
//! word are all the *kernel's* behavior (§3.3, §3.4, §3.6); this trait only
//! names the surface so the reactor's discipline can be checked over a model.

use crate::sys;

/// A channel endpoint handle (a cspace slot index).
pub type Chan = u32;
/// A notification cap handle (a cspace slot index).
pub type Notif = u32;
/// A timer cap handle (a cspace slot index).
pub type Timer = u32;

/// The channel events a binding can target (§3.3, §3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Readable,
    Writable,
    PeerClosed,
}

impl Event {
    /// The raw `EV_*` selector for the `chan_bind` syscall (`sys.rs`).
    pub fn raw(self) -> u64 {
        match self {
            Event::Readable => sys::EV_READABLE,
            Event::Writable => sys::EV_WRITABLE,
            Event::PeerClosed => sys::EV_PEER_CLOSED,
        }
    }
}

/// `send_nb` failure (§3.3): `Full` is backpressure (retry when writable), not
/// a drop. `Closed` is peer-closed. A message is never silently lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendErr {
    Full,
    Closed,
    Other(i64),
}

/// `recv_nb` failure (§3.3): `Empty` (wait for readable), `NoSlot` (receiver
/// has no free cspace slot — the message stays queued, make room and retry,
/// §3.4), `Closed` (peer-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvErr {
    Empty,
    NoSlot,
    Closed,
    Other(i64),
}

/// A successful receive: payload length and the cap-present mask (§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvOk {
    pub len: usize,
    pub cap_mask: u64,
}

/// The kernel IPC surface the reactor needs, 1:1 with `sys.rs` / §3.3, §3.6.
pub trait Transport {
    /// Non-blocking send (§3.3): `Full` when the queue is full, never a drop.
    fn send_nb(&self, ch: Chan, data: &[u8], caps: Option<&[u32; 4]>) -> Result<(), SendErr>;

    /// Non-blocking receive (§3.3): caps land in `dests`; `Empty`/`NoSlot` per
    /// `RecvErr`. `buf` must hold a full inline payload (256 bytes, §3.1).
    fn recv_nb(&self, ch: Chan, buf: &mut [u8], dests: Option<&[u32; 4]>) -> Result<RecvOk, RecvErr>;

    /// Bind a channel event to `(notif, bits)` (§3.6). Persistent: a later
    /// event ORs `bits` into the notification word.
    fn bind(&self, ch: Chan, ev: Event, notif: Notif, bits: u64) -> Result<(), i64>;

    /// OR `bits` into a notification word, waking a waiter or accumulating (§3.6).
    fn notif_signal(&self, n: Notif, bits: u64);

    /// Consume the accumulated word if non-zero, else block (§3.6). Returns the
    /// word (which is cleared). This is the "wait" half of the lost-wakeup
    /// discipline — it checks the word before sleeping.
    fn notif_wait(&self, n: Notif) -> u64;

    /// Arm a timer to signal `(notif, bits)` after `delta` ticks (§3.6, §4.4).
    fn timer_arm(&self, t: Timer, n: Notif, bits: u64, delta: u64);
}

/// Production transport: a zero-sized shim over the real kernel syscalls
/// (`crate::sys`). On the aarch64 target these are real `svc #0`s; on the host
/// the `sys` stubs are `unreachable!`, so this type compiles but is never used
/// there (tests drive `ModelTransport` instead).
#[derive(Debug, Clone, Copy, Default)]
pub struct SyscallTransport;

impl Transport for SyscallTransport {
    fn send_nb(&self, ch: Chan, data: &[u8], caps: Option<&[u32; 4]>) -> Result<(), SendErr> {
        let r = sys::chan_send(ch, data, caps);
        if r >= 0 {
            Ok(())
        } else if r == sys::ERR_FULL {
            Err(SendErr::Full)
        } else if r == sys::ERR_CLOSED {
            Err(SendErr::Closed)
        } else {
            Err(SendErr::Other(r))
        }
    }

    fn recv_nb(&self, ch: Chan, buf: &mut [u8], dests: Option<&[u32; 4]>) -> Result<RecvOk, RecvErr> {
        let (r, mask) = sys::chan_recv(ch, buf.as_mut_ptr(), dests);
        if r >= 0 {
            Ok(RecvOk { len: r as usize, cap_mask: mask })
        } else if r == sys::ERR_EMPTY {
            Err(RecvErr::Empty)
        } else if r == sys::ERR_NOSLOT {
            Err(RecvErr::NoSlot)
        } else if r == sys::ERR_CLOSED {
            Err(RecvErr::Closed)
        } else {
            Err(RecvErr::Other(r))
        }
    }

    fn bind(&self, ch: Chan, ev: Event, notif: Notif, bits: u64) -> Result<(), i64> {
        let r = sys::chan_bind(ch, ev.raw(), notif, bits);
        if r >= 0 {
            Ok(())
        } else {
            Err(r)
        }
    }

    fn notif_signal(&self, n: Notif, bits: u64) {
        sys::notif_signal(n, bits);
    }

    fn notif_wait(&self, n: Notif) -> u64 {
        sys::notif_wait(n) as u64
    }

    fn timer_arm(&self, t: Timer, n: Notif, bits: u64, delta: u64) {
        sys::timer_arm(t, n, bits, delta);
    }
}
