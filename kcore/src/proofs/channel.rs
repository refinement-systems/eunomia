//! Channel harnesses (plan §4.3): the two-ring FIFO, move-on-send,
//! receive atomicity and null-tolerance, peer-closed firing, binding
//! refcounts, and the two TSpec teardown mirrors (`ReclaimedReleased`,
//! `ChannelFireSafe`).
//!
//! State is a `ChannelPool` (header + the trailing `MsgSlot` ring array, the
//! exact layout `Channel::slot` addresses) with caps placed in real ring /
//! cspace slots, so the CDT machinery the channel ops call (`slot_move`,
//! `delete`) runs unchanged. The two harnesses that write the ring payload in
//! a branching loop (`check_ring_fifo`, `check_send_move`) use a *bare*
//! `ChannelPool` plus stack objects; the rest use a full [`World`] so they can
//! reach cspace/TCB slots and the refcount census. (Payload writes inside the
//! large `World` allocation were a CBMC cost blowup — see the findings doc.)
//! The ring invariant [`chan_wf`] is asserted where an op could break it.
//!
//! TLA correspondence: `check_ring_fifo` re-checks the message-ordering core
//! of the §3.1–3.4 channel against an independent ghost queue;
//! `check_destroy_channel` / `check_teardown_fire_safe` are the
//! implementation mirrors of the CapRevocation `TSpec` `ReclaimedReleased`
//! and `ChannelFireSafe` properties (plan §3, DN-2) — the latter is the M1
//! EL0 step-6 scenario rendered as a proof.

#![cfg(kani)]

use super::bounds::CHAN_DEPTH;
use super::ghost::GhostEnv;
use super::wf::chan_wf;
use super::world::{empty_notif, ChannelPool, World};
use crate::channel::{
    self, ChanError, Channel, EV_PEER_CLOSED, EV_READABLE, MSG_CAPS, MSG_PAYLOAD,
};
use crate::cspace::{self, Cap, CapKind, CapSlot, ChanEnd, Rights};
use core::ptr;

/// Op-sequence length for the FIFO transition harness. K = 4 over a depth-2
/// ring exercises fill → drain → wrap-around (the modular `head`/`count`
/// arithmetic only wraps after `> depth` sends), and stays inside the CI
/// budget; raise alongside `CHAN_DEPTH` when the budget grows (plan §3).
const K: usize = 4;

fn notif_cap(n: *mut crate::notification::NotifObj) -> Cap {
    Cap { kind: CapKind::Notification(n), rights: Rights::ALL }
}

/// `check_ring_fifo` (plan §4.3): send/recv against a ghost FIFO of payload
/// tags. For any op sequence at depth 2, the kernel ring delivers payloads in
/// send order, `Full`/`Empty` exactly track the count, indices stay in
/// bounds, and `chan_wf` holds after every step. One direction (A→B, ring 0)
/// — the two rings are independent, so a single direction is the FIFO unit.
#[kani::proof]
#[kani::unwind(6)]
fn check_ring_fifo() {
    // A standalone ChannelPool (header + ring array) rather than the full
    // World: this harness only touches the channel, and modeling the World's
    // TCBs / cspaces / trap frames across K branching steps blew the budget
    // (~11 min). The pool is the exact layout Channel::slot addresses.
    let mut pool = ChannelPool::new();
    let mut env = GhostEnv::new();
    unsafe {
        let ch = ptr::addr_of_mut!(pool.ch);
        (*ch).end_caps = [1, 1]; // both ends open: no peer-closed in this harness
        let depth = CHAN_DEPTH as usize;

        // Ghost queue of payload tags mirroring ring 0.
        let mut model = [0u8; CHAN_DEPTH as usize];
        let mut mhead = 0usize;
        let mut mcount = 0usize;

        for _ in 0..K {
            let do_send: bool = kani::any();
            if do_send {
                let tag: u8 = kani::any();
                let buf = [tag];
                let caps = [ptr::null_mut(); MSG_CAPS];
                let r = channel::send(ch, ChanEnd::A, &buf, &caps, &mut env);
                if mcount < depth {
                    assert!(r.is_ok());
                    model[(mhead + mcount) % depth] = tag;
                    mcount += 1;
                } else {
                    assert!(r == Err(ChanError::Full));
                }
            } else {
                let mut rb = [0u8; MSG_PAYLOAD];
                let dests = [ptr::null_mut(); MSG_CAPS];
                let r = channel::recv(ch, ChanEnd::B, &mut rb, &dests, &mut env);
                if mcount > 0 {
                    assert!(r.is_ok());
                    let (len, mask) = r.unwrap();
                    assert!(len == 1 && mask == 0); // one tag byte, no caps
                    assert!(rb[0] == model[mhead]); // FIFO order
                    mhead = (mhead + 1) % depth;
                    mcount -= 1;
                } else {
                    assert!(r == Err(ChanError::Empty));
                }
            }
            assert!(chan_wf(ch));
        }
    }
}

/// `check_send_move` (plan §4.3): a cap leaves the sender's slot exactly when
/// send succeeds; on `Full` or `PeerClosed` the sender's slots are untouched.
#[kani::proof]
#[kani::unwind(5)]
fn check_send_move() {
    // Minimal state (ChannelPool + a standalone sender slot + notification),
    // not the full World: send's payload copy into a World-embedded ring blew
    // the budget (~9.5 min) the same way check_ring_fifo did (see findings).
    let mut pool = ChannelPool::new();
    let mut env = GhostEnv::new();
    let mut src_slot = CapSlot::empty();
    let mut nobj = empty_notif();
    unsafe {
        let ch = ptr::addr_of_mut!(pool.ch);
        let n = ptr::addr_of_mut!(nobj);
        let src = ptr::addr_of_mut!(src_slot);
        (*src).cap = notif_cap(n);
        (*n).hdr.refs = 1;

        (*ch).end_caps = [1, 1];
        (*ch).head = [0, 0];
        (*ch).count = [0, 0];
        // scenario: 0 = ok, 1 = ring full, 2 = peer closed.
        let scen: u8 = kani::any();
        kani::assume(scen < 3);
        if scen == 1 {
            (*ch).count[0] = (*ch).depth;
        }
        if scen == 2 {
            (*ch).end_caps[1] = 0;
        }

        let caps = [src, ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
        let buf = [0xABu8];
        let r = channel::send(ch, ChanEnd::A, &buf, &caps, &mut env);

        match scen {
            0 => {
                assert!(r.is_ok());
                assert!((*src).cap.is_empty()); // moved out
                let q = ptr::addr_of_mut!((*Channel::slot(ch, 0, 0)).caps[0]);
                assert!(matches!((*q).cap.kind, CapKind::Notification(p) if p == n));
                assert!((*ch).count[0] == 1);
                assert!((*n).hdr.refs == 1); // a move, not a copy
            }
            1 => {
                assert!(r == Err(ChanError::Full));
                assert!(!(*src).cap.is_empty()); // untouched
            }
            _ => {
                assert!(r == Err(ChanError::PeerClosed));
                assert!(!(*src).cap.is_empty()); // untouched
            }
        }
    }
}

/// `check_recv_atomic` (plan §4.3, §3.3): a `NoCapSlot` failure leaves the
/// message **fully queued** — no partial cap installation, payload intact,
/// count unchanged — so the receiver can retry. recv validates *all*
/// destinations before moving *any*, so the first unplaceable cap aborts
/// before the move loop.
#[kani::proof]
#[kani::unwind(5)]
fn check_recv_atomic() {
    let mut w = World::new();
    unsafe {
        let ch = w.channel();
        let n = w.notif(0);
        (*ch).end_caps = [1, 1];
        (*ch).head[0] = 0;
        (*ch).count[0] = 1;
        // One message on ring 0 carrying two caps.
        let q0 = w.ring_cap(0, 0, 0);
        let q1 = w.ring_cap(0, 0, 1);
        (*q0).cap = notif_cap(n);
        (*q1).cap = notif_cap(n);
        (*n).hdr.refs = 2;
        let slot = Channel::slot(ch, 0, 0);
        (*slot).len = 1;

        // cap0 has a free destination, cap1's destination is null → NoCapSlot
        // is raised on cap1, and cap0 must NOT have been moved.
        let d0 = w.cspace_slot(0, 0);
        let dests = [d0, ptr::null_mut(), ptr::null_mut(), ptr::null_mut()];
        let mut rb = [0u8; MSG_PAYLOAD];
        let r = channel::recv(ch, ChanEnd::B, &mut rb, &dests, &mut w.env);

        assert!(r == Err(ChanError::NoCapSlot));
        assert!(!(*q0).cap.is_empty()); // both caps still queued
        assert!(!(*q1).cap.is_empty());
        assert!((*d0).cap.is_empty()); // nothing installed
        assert!((*ch).count[0] == 1 && (*ch).head[0] == 0); // not dequeued
        assert!((*n).hdr.refs == 2); // no refcount churn
        assert!((*slot).len == 1); // payload intact
    }
}

/// `check_recv_null_tolerant` (plan §4.3, §3.4): a queued slot emptied by
/// revocation in flight is delivered as an *absent* cap (its mask bit clear),
/// never a panic. Present caps around the hole are delivered normally.
#[kani::proof]
#[kani::unwind(5)]
fn check_recv_null_tolerant() {
    let mut w = World::new();
    unsafe {
        let ch = w.channel();
        let n = w.notif(0);
        (*ch).end_caps = [1, 1];
        (*ch).head[0] = 0;
        (*ch).count[0] = 1;
        // caps[0] present, caps[1] empty (revoked mid-flight), caps[2] present.
        let q0 = w.ring_cap(0, 0, 0);
        let q2 = w.ring_cap(0, 0, 2);
        (*q0).cap = notif_cap(n);
        (*q2).cap = notif_cap(n);
        (*n).hdr.refs = 2;
        let slot = Channel::slot(ch, 0, 0);
        (*slot).len = 0;

        let d0 = w.cspace_slot(0, 0);
        let d1 = w.cspace_slot(0, 1);
        let d2 = w.cspace_slot(0, 2);
        let d3 = w.cspace_slot(0, 3);
        let dests = [d0, d1, d2, d3];
        let mut rb = [0u8; MSG_PAYLOAD];
        let r = channel::recv(ch, ChanEnd::B, &mut rb, &dests, &mut w.env);

        assert!(r.is_ok());
        let (len, mask) = r.unwrap();
        assert!(len == 0);
        assert!(mask == 0b101); // bits 0 and 2 set, bit 1 (revoked) clear
        assert!(!(*d0).cap.is_empty());
        assert!((*d1).cap.is_empty()); // nothing for the revoked slot
        assert!(!(*d2).cap.is_empty());
        assert!((*ch).count[0] == 0); // dequeued
    }
}

/// `check_peer_closed` (plan §4.3, §3.3): dropping an end's last cap fires the
/// *other* end's peer-closed binding into a live notification, and a send into
/// the now-closed peer errors. Nondet over which end closes.
#[kani::proof]
#[kani::unwind(5)]
fn check_peer_closed() {
    let mut w = World::new();
    unsafe {
        let ch = w.channel();
        let n = w.notif(0);
        (*ch).end_caps = [1, 1];

        let drop_b: bool = kani::any();
        let bits: u64 = 0b1000;
        // Bind the peer-closed event of the end that *observes* the close
        // (the end opposite the one whose cap drops).
        let observer = if drop_b { ChanEnd::A } else { ChanEnd::B };
        channel::bind(ch, observer, EV_PEER_CLOSED, n, bits);
        let refs_after_bind = (*n).hdr.refs;

        let closing = if drop_b { ChanEnd::B } else { ChanEnd::A };
        channel::endpoint_cap_dropped(ch, closing, &mut w.env);

        // The observer's binding fired into the still-live n (no waiter ⇒ the
        // word accumulates the bits); the binding keeps its ref.
        assert!((*n).word == bits);
        assert!((*n).hdr.refs == refs_after_bind);

        // The surviving end's send into the closed peer errors.
        let caps = [ptr::null_mut(); MSG_CAPS];
        let buf = [0u8];
        let r = channel::send(ch, observer, &buf, &caps, &mut w.env);
        assert!(r == Err(ChanError::PeerClosed));
    }
}

/// `check_bind_refcounts` (plan §4.3, §3.6): bind/rebind/unbind keep the
/// bound notifications' refcounts exact — rebinding releases the old
/// notification's ref before taking the new one's.
#[kani::proof]
fn check_bind_refcounts() {
    let mut w = World::new();
    unsafe {
        let ch = w.channel();
        let n0 = w.notif(0);
        let n1 = w.notif(1);
        let r0: u32 = kani::any();
        let r1: u32 = kani::any();
        kani::assume(r0 < 1 << 20 && r1 < 1 << 20); // keep the +1 off the overflow edge
        (*n0).hdr.refs = r0;
        (*n1).hdr.refs = r1;

        channel::bind(ch, ChanEnd::A, EV_READABLE, n0, 0b1);
        assert!((*n0).hdr.refs == r0 + 1);

        // Rebind the same slot to n1: n0 released, n1 taken.
        channel::bind(ch, ChanEnd::A, EV_READABLE, n1, 0b10);
        assert!((*n0).hdr.refs == r0);
        assert!((*n1).hdr.refs == r1 + 1);

        // Unbind: n1 released; the slot is null.
        channel::bind(ch, ChanEnd::A, EV_READABLE, ptr::null_mut(), 0);
        assert!((*n1).hdr.refs == r1);
        assert!((*ch).bindings[0][EV_READABLE].notif.is_null());
    }
}

/// `check_destroy_channel` (plan §4.3, TSpec `ReclaimedReleased`): tearing a
/// channel down deletes every queued cap (their objects unref'd) and releases
/// every binding's notification ref — no leaked refcount, no orphaned cap.
/// Queued caps are notifications, whose teardown is loop/recursion-free, so
/// the whole-channel destroy stays tractable under CBMC (finding DN-4).
#[kani::proof]
#[kani::unwind(6)]
fn check_destroy_channel() {
    let mut w = World::new();
    unsafe {
        let ch = w.channel();
        let n0 = w.notif(0); // two queued caps designate this
        let n1 = w.notif(1); // a peer-closed binding holds a ref to this
        let q0 = w.ring_cap(0, 0, 0);
        let q1 = w.ring_cap(1, 0, 0);
        (*q0).cap = notif_cap(n0);
        (*q1).cap = notif_cap(n0);
        (*n0).hdr.refs = 2;
        channel::bind(ch, ChanEnd::A, EV_PEER_CLOSED, n1, 0b1);
        let n1_before = (*n1).hdr.refs;

        channel::destroy_channel(ch, &mut w.env);

        assert!((*q0).cap.is_empty()); // queued caps deleted
        assert!((*q1).cap.is_empty());
        assert!((*n0).hdr.refs == 0); // both refs released → object destroyed
        assert!((*n1).hdr.refs == n1_before - 1); // binding ref released
    }
}

/// `check_teardown_fire_safe` (plan §4.3, TSpec `ChannelFireSafe`): the M1
/// EL0 step-6 scenario as a proof. A channel's two endpoint caps both bind
/// their peer-closed event to a *separately funded* notification; deleting the
/// caps in either order fires each surviving peer's binding into a notification
/// that is still live at fire time (the binding holds the ref), and the
/// notification outlives the channel (its funding cap remains). `delete`
/// fires `endpoint_cap_dropped` (peer-closed) before `obj_unref`, so the
/// final teardown still signals a live object.
#[kani::proof]
#[kani::unwind(6)]
fn check_teardown_fire_safe() {
    let mut w = World::new();
    unsafe {
        let ch = w.channel();
        let n = w.notif(0);
        // n separately funded: one cap to it lives outside the channel.
        let funding = w.cspace_slot(1, 0);
        (*funding).cap = notif_cap(n);
        (*n).hdr.refs = 1;

        let bits_a: u64 = 0b01;
        let bits_b: u64 = 0b10;
        channel::bind(ch, ChanEnd::A, EV_PEER_CLOSED, n, bits_a); // n: 1→2
        channel::bind(ch, ChanEnd::B, EV_PEER_CLOSED, n, bits_b); // n: 2→3

        let ca = w.cspace_slot(0, 0);
        let cb = w.cspace_slot(0, 1);
        (*ca).cap = Cap { kind: CapKind::Channel(ch, ChanEnd::A), rights: Rights::ALL };
        (*cb).cap = Cap { kind: CapKind::Channel(ch, ChanEnd::B), rights: Rights::ALL };
        (*ch).end_caps = [1, 1];
        (*ch).hdr.refs = 2;

        let a_first: bool = kani::any();
        if a_first {
            cspace::delete(ca, &mut w.env);
            cspace::delete(cb, &mut w.env);
        } else {
            cspace::delete(cb, &mut w.env);
            cspace::delete(ca, &mut w.env);
        }

        // Both peer-closed bindings fired into the live n (both bits present);
        // the channel is destroyed but n outlived it via its funding cap.
        assert!((*n).word == (bits_a | bits_b));
        assert!((*ch).hdr.refs == 0);
        assert!((*n).hdr.refs == 1);
        assert!(!(*funding).cap.is_empty());
    }
}
