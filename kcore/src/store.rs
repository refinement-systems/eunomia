//! The object-store seam.
//!
//! `kcore`'s operations are written against this trait instead of raw pointers:
//! every kernel object and cap slot is reached through an opaque
//! [`ObjId`]/[`SlotId`] handle ([`crate::id`]) that the `Store` resolves. The
//! verified core never dereferences — it only calls these by-handle get/set
//! accessors, so it reads as pure functions over an abstract indexed store:
//!
//!   - **production** (`kernel` crate): the handle wraps the live address; the
//!     accessor is a behaviour-preserving field read/write at the one sanctioned
//!     `unsafe` boundary (replacing the old scattered `(*p).field` derefs);
//!   - **proofs / host tests**: the handle is an array index and the accessors
//!     touch plain arrays — the model Verus and `cargo test` verify.
//!
//! Accessors are **by value** on `Copy` data, so no two `&mut` overlap (the
//! aliasing the old free pointer-mutation sidestepped). A `CapSlot` is `Copy`
//! and is touched **only** through [`Store::slot`]/[`Store::set_slot`] — never as
//! part of a whole-object copy — so a slot has exactly one access path however it
//! is homed (cspace resident, channel ring cap, or TCB binding slot).
//!
//! The trait also folds in the old [`crate::env::Env`] hardware/scheduler seam
//! (`make_runnable`, `aspace_unmap`, the TLB/barrier hooks, the armed-timer
//! head), now keyed on handles: one seam for both object storage and effects.

use crate::cspace::CapSlot;
use crate::id::{ObjId, SlotId};
use crate::thread::{Report, ThreadState};

/// An event binding: the notification a channel/TCB event fires into, and the
/// bits to OR (rev1§3.6). Handle-based (was a `*mut NotifObj`); `None` == unbound.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub notif: Option<ObjId>,
    pub bits: u64,
}

impl Binding {
    pub const UNBOUND: Binding = Binding {
        notif: None,
        bits: 0,
    };
}

/// The abstract kernel-object store the verified core runs against. All methods
/// are total over valid handles; a handle is valid iff the `Store` minted it
/// (production: at object construction; proofs: array bounds).
pub trait Store {
    // ── cap slots — the sole CapSlot access path ──────────────────────────
    fn slot(&self, s: SlotId) -> CapSlot;
    fn set_slot(&mut self, s: SlotId, v: CapSlot);

    // ── object refcounts (uniform via the ObjHeader at every object's head) ─
    fn obj_refs(&self, o: ObjId) -> u32;
    fn set_obj_refs(&mut self, o: ObjId, r: u32);

    // ── cspace residents ──────────────────────────────────────────────────
    fn cspace_num_slots(&self, cs: ObjId) -> u32;
    fn cspace_slot(&self, cs: ObjId, i: u32) -> SlotId;

    // ── channel ───────────────────────────────────────────────────────────
    fn chan_depth(&self, ch: ObjId) -> u32;
    fn chan_end_caps(&self, ch: ObjId, end: usize) -> u32;
    fn set_chan_end_caps(&mut self, ch: ObjId, end: usize, v: u32);
    fn chan_head(&self, ch: ObjId, ring: usize) -> u32;
    fn set_chan_head(&mut self, ch: ObjId, ring: usize, v: u32);
    fn chan_count(&self, ch: ObjId, ring: usize) -> u32;
    fn set_chan_count(&mut self, ch: ObjId, ring: usize, v: u32);
    fn chan_binding(&self, ch: ObjId, end: usize, ev: usize) -> Binding;
    fn set_chan_binding(&mut self, ch: ObjId, end: usize, ev: usize, b: Binding);
    /// The CapSlot handle for ring message `i`'s cap slot `c` (ring ∈ {0,1}).
    fn chan_ring_cap(&self, ch: ObjId, ring: usize, i: u32, c: usize) -> SlotId;
    fn chan_msg_len(&self, ch: ObjId, ring: usize, i: u32) -> u16;
    fn set_chan_msg_len(&mut self, ch: ObjId, ring: usize, i: u32, v: u16);
    /// Copy `data` into message `i`'s payload (truncated to `MSG_PAYLOAD`).
    fn chan_msg_write(&mut self, ch: ObjId, ring: usize, i: u32, data: &[u8]);
    /// Copy message `i`'s payload (`len` bytes) into `buf`.
    fn chan_msg_read(&self, ch: ObjId, ring: usize, i: u32, len: usize, buf: &mut [u8]);

    // ── notification ──────────────────────────────────────────────────────
    fn notif_word(&self, n: ObjId) -> u64;
    fn set_notif_word(&mut self, n: ObjId, v: u64);
    fn notif_wait_head(&self, n: ObjId) -> Option<ObjId>;
    fn set_notif_wait_head(&mut self, n: ObjId, t: Option<ObjId>);
    fn notif_wait_tail(&self, n: ObjId) -> Option<ObjId>;
    fn set_notif_wait_tail(&mut self, n: ObjId, t: Option<ObjId>);

    // ── thread ────────────────────────────────────────────────────────────
    fn tcb_state(&self, t: ObjId) -> ThreadState;
    fn set_tcb_state(&mut self, t: ObjId, s: ThreadState);
    fn tcb_qnext(&self, t: ObjId) -> Option<ObjId>;
    fn set_tcb_qnext(&mut self, t: ObjId, q: Option<ObjId>);
    fn tcb_wait_notif(&self, t: ObjId) -> Option<ObjId>;
    fn set_tcb_wait_notif(&mut self, t: ObjId, n: Option<ObjId>);
    fn tcb_report(&self, t: ObjId) -> Report;
    fn set_tcb_report(&mut self, t: ObjId, r: Report);
    /// The thread's rev1§5.4 run priority — bounded by the spawner's cap ceiling and
    /// written through the verified [`crate::thread::set_priority`].
    fn tcb_priority(&self, t: ObjId) -> u8;
    fn set_tcb_priority(&mut self, t: ObjId, p: u8);
    fn tcb_bind_slot(&self, t: ObjId, which: usize) -> SlotId;
    fn tcb_bind_bits(&self, t: ObjId, which: usize) -> u64;
    fn set_tcb_bind_bits(&mut self, t: ObjId, which: usize, b: u64);
    fn tcb_cspace(&self, t: ObjId) -> Option<ObjId>;
    fn set_tcb_cspace(&mut self, t: ObjId, cs: Option<ObjId>);
    fn tcb_aspace(&self, t: ObjId) -> Option<ObjId>;
    fn set_tcb_aspace(&mut self, t: ObjId, a: Option<ObjId>);
    /// Set the thread's return register (`frame.x[0]`) — the woken word (rev1§3.6).
    fn set_tcb_retval(&mut self, t: ObjId, v: u64);

    // ── timer ─────────────────────────────────────────────────────────────
    fn timer_armed(&self, t: ObjId) -> bool;
    fn set_timer_armed(&mut self, t: ObjId, v: bool);
    fn timer_deadline(&self, t: ObjId) -> u64;
    fn set_timer_deadline(&mut self, t: ObjId, v: u64);
    fn timer_notif(&self, t: ObjId) -> Option<ObjId>;
    fn set_timer_notif(&mut self, t: ObjId, n: Option<ObjId>);
    fn timer_bits(&self, t: ObjId) -> u64;
    fn set_timer_bits(&mut self, t: ObjId, v: u64);
    fn timer_next(&self, t: ObjId) -> Option<ObjId>;
    fn set_timer_next(&mut self, t: ObjId, n: Option<ObjId>);

    // ── hardware / scheduler seam (folded from Env) ───────────────────────
    /// Make a thread Runnable (notification delivery, rev1§3.6). pre: `t` detached
    /// from any wait queue, register state consistent. post: `t` schedulable.
    fn make_runnable(&mut self, t: ObjId);
    /// Remove a Runnable thread from the ready structure (teardown).
    fn unqueue_ready(&mut self, t: ObjId);
    /// Unmap `pages` frames at `va` from aspace `a`, TLB maintenance included.
    fn aspace_unmap(&mut self, a: ObjId, va: u64, pages: u64);
    /// Map `pages` frames at PA `pa` into aspace `a` at `va` with `perms` (B8A, the symmetric
    /// twin of `aspace_unmap`). Page-table machinery only — the verified cap-side record is
    /// [`crate::cspace::map_frame`]. Fallible: the table pool may be exhausted (`NeedMemory`).
    fn aspace_map(&mut self, a: ObjId, pa: u64, va: u64, pages: u64, perms: u64)
        -> Result<(), crate::aspace::MapError>;
    /// Last-reference teardown of an address space.
    fn aspace_destroy(&mut self, a: ObjId);
    fn tlb_invalidate_page(&mut self, asid: u16, va: u64);
    fn barrier_after_map(&mut self);
    fn barrier_after_unmap(&mut self);
    /// The armed-timer list head (a kernel static in production, ghost state in
    /// proofs — the list *logic* lives in [`crate::timer`]).
    fn timer_armed_head(&self) -> Option<ObjId>;
    fn set_timer_armed_head(&mut self, h: Option<ObjId>);
}
