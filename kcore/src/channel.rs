//! Asynchronous IPC channels (spec §3.1–3.4, §3.6).
//!
//! A channel is two endpoints (A, B) over two fixed-depth rings of message
//! slots — ring 0 carries A→B, ring 1 carries B→A. A message slot is a
//! 256-byte inline payload plus 4 real `CapSlot`s: queued caps are
//! CDT-visible and owned by the channel, so revocation sees through queues
//! (§3.4) with no special case.
//!
//! Queue memory comes from the untyped donated at retype; capacity is the
//! creator-chosen depth (§3.2). Send is non-blocking and returns FULL;
//! messages are never dropped. Each endpoint carries fixed binding slots
//! (on-readable / on-writable / on-peer-closed → notification, bits);
//! event delivery never allocates (§3.6).
//!
//! Arena rewrite (plan §3): the channel is addressed by an opaque
//! [`ObjId`](crate::id::ObjId) and all of its state is reached through the
//! [`Store`] seam — ring caps are [`SlotId`](crate::id::SlotId) handles, event
//! bindings are [`crate::store::Binding`]s. The construction/layout helpers
//! (`bytes_for`/`init`/`slot`) remain pointer-based: the kernel shell uses them
//! to *place* an object before any handle exists.

use crate::cspace::{self, CapSlot, ChanEnd, ObjHeader};
use crate::id::{ObjId, SlotId};
use crate::notification;
use crate::store::{Binding, Store};

pub const MSG_PAYLOAD: usize = 256;
pub const MSG_CAPS: usize = 4;

pub const EV_READABLE: usize = 0;
pub const EV_WRITABLE: usize = 1;
pub const EV_PEER_CLOSED: usize = 2;

#[repr(C)]
pub struct MsgSlot {
    pub len: u16,
    pub payload: [u8; MSG_PAYLOAD],
    pub caps: [CapSlot; MSG_CAPS],
}

#[repr(C)]
pub struct Channel {
    pub hdr: ObjHeader,
    pub(crate) depth: u32,
    /// Live endpoint caps per end, for peer-closed (§3.3).
    pub(crate) end_caps: [u32; 2],
    pub(crate) head: [u32; 2],
    pub(crate) count: [u32; 2],
    /// bindings[end][event] — events observed by that end's holder.
    pub(crate) bindings: [[Binding; 3]; 2],
    // MsgSlot[2 * depth] follows: ring 0 then ring 1.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChanError {
    Full,
    Empty,
    NoCapSlot,
    PeerClosed,
}

fn end_idx(e: ChanEnd) -> usize {
    match e {
        ChanEnd::A => 0,
        ChanEnd::B => 1,
    }
}

impl Channel {
    pub const fn bytes_for(depth: u32) -> usize {
        core::mem::size_of::<Channel>() + 2 * depth as usize * core::mem::size_of::<MsgSlot>()
    }

    /// pre:  memory at `this` writable, sized via bytes_for(depth).
    /// post: empty rings, all cap slots empty, unbound events, refs = 1
    ///       (endpoint A's cap; retype adds another for endpoint B).
    pub unsafe fn init(this: *mut Channel, depth: u32) {
        this.write(Channel {
            hdr: ObjHeader { refs: 1 },
            depth,
            end_caps: [0, 0],
            head: [0, 0],
            count: [0, 0],
            bindings: [[Binding::UNBOUND; 3]; 2],
        });
        for ring in 0..2 {
            for i in 0..depth {
                let s = Channel::slot(this, ring, i);
                (*s).len = 0;
                for c in 0..MSG_CAPS {
                    (*s).caps[c] = CapSlot::empty();
                }
            }
        }
    }

    pub(crate) unsafe fn slot(this: *mut Channel, ring: usize, i: u32) -> *mut MsgSlot {
        let base = this.add(1).cast::<MsgSlot>();
        base.add(ring * (*this).depth as usize + i as usize)
    }
}

pub fn endpoint_cap_added<S: Store>(store: &mut S, ch: ObjId, end: ChanEnd) {
    let e = end_idx(end);
    store.set_chan_end_caps(ch, e, store.chan_end_caps(ch, e) + 1);
}

/// Called on every endpoint-cap deletion; the last cap of an end raises
/// the other end's peer-closed event (§3.3, session cleanup §2.4).
pub fn endpoint_cap_dropped<S: Store>(store: &mut S, ch: ObjId, end: ChanEnd) {
    let e = end_idx(end);
    store.set_chan_end_caps(ch, e, store.chan_end_caps(ch, e) - 1);
    if store.chan_end_caps(ch, e) == 0 {
        fire(store, ch, 1 - e, EV_PEER_CLOSED);
    }
}

fn fire<S: Store>(store: &mut S, ch: ObjId, end: usize, event: usize) {
    let b = store.chan_binding(ch, end, event);
    if let Some(n) = b.notif {
        notification::signal(store, n, b.bits);
    }
}

/// Configure an endpoint's event binding (holder-configured, §3.6).
/// Replacing a binding releases the old notification's ref.
pub fn bind<S: Store>(
    store: &mut S,
    ch: ObjId,
    end: ChanEnd,
    event: usize,
    notif: Option<ObjId>,
    bits: u64,
) {
    let e = end_idx(end);
    let old = store.chan_binding(ch, e, event);
    if let Some(n) = old.notif {
        store.set_obj_refs(n, store.obj_refs(n) - 1);
    }
    if let Some(n) = notif {
        store.set_obj_refs(n, store.obj_refs(n) + 1);
    }
    store.set_chan_binding(ch, e, event, Binding { notif, bits });
}

/// Send: copy the payload into the ring and move caps from the sender's
/// slots into the message's CDT-visible slots (§3.4 move semantics).
///
/// pre:  data.len() ≤ MSG_PAYLOAD; each caps[i] is None or a non-empty
///       slot owned by the sender.
/// post: message queued FIFO; sender's cap slots empty; receiver's
///       readable event fired.
pub fn send<S: Store>(
    store: &mut S,
    ch: ObjId,
    end: ChanEnd,
    data: &[u8],
    caps: &[Option<SlotId>; MSG_CAPS],
) -> Result<(), ChanError> {
    let e = end_idx(end);
    if store.chan_end_caps(ch, 1 - e) == 0 {
        return Err(ChanError::PeerClosed);
    }
    let ring = e; // end A sends on ring 0, B on ring 1
    let depth = store.chan_depth(ch);
    if store.chan_count(ch, ring) == depth {
        return Err(ChanError::Full);
    }
    let i = (store.chan_head(ch, ring) + store.chan_count(ch, ring)) % depth;
    store.set_chan_msg_len(ch, ring, i, data.len() as u16);
    store.chan_msg_write(ch, ring, i, data);
    for (c, &src) in caps.iter().enumerate() {
        if let Some(src) = src {
            let dst = store.chan_ring_cap(ch, ring, i, c);
            cspace::slot_move(store, src, dst);
        }
    }
    store.set_chan_count(ch, ring, store.chan_count(ch, ring) + 1);
    fire(store, ch, 1 - e, EV_READABLE);
    Ok(())
}

/// Receive into `buf`, installing caps into `dests`. If any arriving cap
/// has no free destination the receive fails and the message stays queued
/// (§3.3) — receive-side exhaustion is the receiver's own problem.
/// Revocation may have emptied queued slots in flight; receivers see those
/// as absent caps (§3.4 null slots).
///
/// post on success: returns (len, cap-present mask); message dequeued;
///       sender's writable event fired.
pub fn recv<S: Store>(
    store: &mut S,
    ch: ObjId,
    end: ChanEnd,
    buf: &mut [u8; MSG_PAYLOAD],
    dests: &[Option<SlotId>; MSG_CAPS],
) -> Result<(usize, u8), ChanError> {
    let e = end_idx(end);
    let ring = 1 - e;
    if store.chan_count(ch, ring) == 0 {
        return Err(ChanError::Empty);
    }
    let head = store.chan_head(ch, ring);
    for c in 0..MSG_CAPS {
        let src = store.chan_ring_cap(ch, ring, head, c);
        if !store.slot(src).cap.is_empty() {
            match dests[c] {
                None => return Err(ChanError::NoCapSlot),
                Some(d) => {
                    if !store.slot(d).cap.is_empty() {
                        return Err(ChanError::NoCapSlot);
                    }
                }
            }
        }
    }
    let mut mask = 0u8;
    for c in 0..MSG_CAPS {
        let src = store.chan_ring_cap(ch, ring, head, c);
        if !store.slot(src).cap.is_empty() {
            // Checked above: dests[c] is Some and empty.
            let d = dests[c].unwrap();
            cspace::slot_move(store, src, d);
            mask |= 1 << c;
        }
    }
    let len = store.chan_msg_len(ch, ring, head) as usize;
    store.chan_msg_read(ch, ring, head, len, buf);
    store.set_chan_msg_len(ch, ring, head, 0);
    let depth = store.chan_depth(ch);
    store.set_chan_head(ch, ring, (head + 1) % depth);
    store.set_chan_count(ch, ring, store.chan_count(ch, ring) - 1);
    fire(store, ch, 1 - e, EV_WRITABLE);
    Ok((len, mask))
}

/// pre:  refs == 0 (both ends' caps all deleted).
/// post: queued caps destroyed with ordinary CDT cleanup — cash in a
///       shredded envelope (§3.4); bindings released.
pub fn destroy_channel<S: Store>(store: &mut S, ch: ObjId) {
    let depth = store.chan_depth(ch);
    for ring in 0..2 {
        for i in 0..depth {
            for c in 0..MSG_CAPS {
                let cs = store.chan_ring_cap(ch, ring, i, c);
                if !store.slot(cs).cap.is_empty() {
                    cspace::delete(store, cs);
                }
            }
        }
    }
    for end in 0..2 {
        for ev in 0..3 {
            let b = store.chan_binding(ch, end, ev);
            if let Some(n) = b.notif {
                store.set_obj_refs(n, store.obj_refs(n) - 1);
            }
        }
    }
}
