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

use crate::cspace::{self, CapSlot, ChanEnd, ObjHeader};
use crate::env::Env;
use crate::notification::{self, NotifObj};
use core::ptr;

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
#[derive(Clone, Copy)]
pub struct Binding {
    pub notif: *mut NotifObj,
    pub bits: u64,
}

const UNBOUND: Binding = Binding { notif: ptr::null_mut(), bits: 0 };

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
            bindings: [[UNBOUND; 3]; 2],
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

pub unsafe fn endpoint_cap_added(ch: *mut Channel, end: ChanEnd) {
    (*ch).end_caps[end_idx(end)] += 1;
}

/// Called on every endpoint-cap deletion; the last cap of an end raises
/// the other end's peer-closed event (§3.3, session cleanup §2.4).
pub unsafe fn endpoint_cap_dropped<E: Env>(ch: *mut Channel, end: ChanEnd, env: &mut E) {
    let e = end_idx(end);
    (*ch).end_caps[e] -= 1;
    if (*ch).end_caps[e] == 0 {
        fire(ch, 1 - e, EV_PEER_CLOSED, env);
    }
}

unsafe fn fire<E: Env>(ch: *mut Channel, end: usize, event: usize, env: &mut E) {
    let b = (*ch).bindings[end][event];
    if !b.notif.is_null() {
        notification::signal(b.notif, b.bits, env);
    }
}

/// Configure an endpoint's event binding (holder-configured, §3.6).
/// Replacing a binding releases the old notification's ref.
pub unsafe fn bind(
    ch: *mut Channel,
    end: ChanEnd,
    event: usize,
    notif: *mut NotifObj,
    bits: u64,
) {
    let slot = &mut (*ch).bindings[end_idx(end)][event];
    if !slot.notif.is_null() {
        (*slot.notif).hdr.refs -= 1;
    }
    if !notif.is_null() {
        (*notif).hdr.refs += 1;
    }
    *slot = Binding { notif, bits };
}

/// Send: copy the payload into the ring and move caps from the sender's
/// slots into the message's CDT-visible slots (§3.4 move semantics).
///
/// pre:  data.len() ≤ MSG_PAYLOAD; each caps[i] is null or a non-empty
///       slot owned by the sender.
/// post: message queued FIFO; sender's cap slots empty; receiver's
///       readable event fired.
pub unsafe fn send<E: Env>(
    ch: *mut Channel,
    end: ChanEnd,
    data: &[u8],
    caps: &[*mut CapSlot; MSG_CAPS],
    env: &mut E,
) -> Result<(), ChanError> {
    let e = end_idx(end);
    if (*ch).end_caps[1 - e] == 0 {
        return Err(ChanError::PeerClosed);
    }
    let ring = e; // end A sends on ring 0, B on ring 1
    if (*ch).count[ring] == (*ch).depth {
        return Err(ChanError::Full);
    }
    let i = ((*ch).head[ring] + (*ch).count[ring]) % (*ch).depth;
    let slot = Channel::slot(ch, ring, i);
    (*slot).len = data.len() as u16;
    core::ptr::copy_nonoverlapping(
        data.as_ptr(),
        core::ptr::addr_of_mut!((*slot).payload).cast::<u8>(),
        data.len(),
    );
    for (c, &src) in caps.iter().enumerate() {
        if !src.is_null() {
            cspace::slot_move(src, core::ptr::addr_of_mut!((*slot).caps[c]));
        }
    }
    (*ch).count[ring] += 1;
    fire(ch, 1 - e, EV_READABLE, env);
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
pub unsafe fn recv<E: Env>(
    ch: *mut Channel,
    end: ChanEnd,
    buf: &mut [u8; MSG_PAYLOAD],
    dests: &[*mut CapSlot; MSG_CAPS],
    env: &mut E,
) -> Result<(usize, u8), ChanError> {
    let e = end_idx(end);
    let ring = 1 - e;
    if (*ch).count[ring] == 0 {
        return Err(ChanError::Empty);
    }
    let slot = Channel::slot(ch, ring, (*ch).head[ring]);
    for c in 0..MSG_CAPS {
        if !(*slot).caps[c].cap.is_empty() {
            let d = dests[c];
            if d.is_null() || !(*d).cap.is_empty() {
                return Err(ChanError::NoCapSlot);
            }
        }
    }
    let mut mask = 0u8;
    for c in 0..MSG_CAPS {
        if !(*slot).caps[c].cap.is_empty() {
            cspace::slot_move(core::ptr::addr_of_mut!((*slot).caps[c]), dests[c]);
            mask |= 1 << c;
        }
    }
    let len = (*slot).len as usize;
    core::ptr::copy_nonoverlapping(
        core::ptr::addr_of!((*slot).payload).cast::<u8>(),
        buf.as_mut_ptr(),
        len,
    );
    (*slot).len = 0;
    (*ch).head[ring] = ((*ch).head[ring] + 1) % (*ch).depth;
    (*ch).count[ring] -= 1;
    fire(ch, 1 - e, EV_WRITABLE, env);
    Ok((len, mask))
}

/// pre:  refs == 0 (both ends' caps all deleted).
/// post: queued caps destroyed with ordinary CDT cleanup — cash in a
///       shredded envelope (§3.4); bindings released.
pub unsafe fn destroy_channel<E: Env>(ch: *mut Channel, env: &mut E) {
    for ring in 0..2 {
        let depth = (*ch).depth;
        for i in 0..depth {
            let slot = Channel::slot(ch, ring, i);
            for c in 0..MSG_CAPS {
                let cs = core::ptr::addr_of_mut!((*slot).caps[c]);
                if !(*cs).cap.is_empty() {
                    cspace::delete(cs, env);
                }
            }
        }
    }
    for end in 0..2 {
        for ev in 0..3 {
            let b = (*ch).bindings[end][ev];
            if !b.notif.is_null() {
                (*b.notif).hdr.refs -= 1;
            }
        }
    }
}
