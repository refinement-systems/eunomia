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
use vstd::prelude::*;
// `StoreSpec` (the `external_trait_extension`) must be in scope to resolve the
// `slot_view`/`chan_view`/`refs_view` views the §3b contracts quantify over, and
// `ChanView` names the channel ghost view in those contracts; both appear only in
// `requires`/`ensures`, which erase in a normal build — hence unused there (the
// doc/results/26 §2.3 idiom).
#[allow(unused_imports)]
use crate::cspace::{ChanView, StoreSpec};

verus! {

pub const MSG_PAYLOAD: usize = 256;
pub const MSG_CAPS: usize = 4;

pub const EV_READABLE: usize = 0;
pub const EV_WRITABLE: usize = 1;
pub const EV_PEER_CLOSED: usize = 2;

} // verus!

#[repr(C)]
pub struct MsgSlot {
    pub len: u16,
    pub payload: [u8; MSG_PAYLOAD],
    pub caps: [CapSlot; MSG_CAPS],
}

#[repr(C)]
pub struct Channel {
    pub hdr: ObjHeader,
    pub depth: u32,
    /// Live endpoint caps per end, for peer-closed (§3.3).
    pub end_caps: [u32; 2],
    pub head: [u32; 2],
    pub count: [u32; 2],
    /// bindings[end][event] — events observed by that end's holder.
    pub bindings: [[Binding; 3]; 2],
    // MsgSlot[2 * depth] follows: ring 0 then ring 1.
}

verus! {

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChanError {
    Full,
    Empty,
    NoCapSlot,
    PeerClosed,
}

} // verus!

verus! {

/// Ghost mirror of [`end_idx`]: A → 0, B → 1. Lets the §3b contracts name the
/// `end_caps`/ring index a `ChanEnd` selects.
pub open spec fn end_idx_spec(e: ChanEnd) -> int {
    match e {
        ChanEnd::A => 0,
        ChanEnd::B => 1,
    }
}

fn end_idx(e: ChanEnd) -> (r: usize)
    ensures
        r < 2,
        r as int == end_idx_spec(e),
{
    match e {
        ChanEnd::A => 0,
        ChanEnd::B => 1,
    }
}

} // verus!

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

    pub unsafe fn slot(this: *mut Channel, ring: usize, i: u32) -> *mut MsgSlot {
        let base = this.add(1).cast::<MsgSlot>();
        base.add(ring * (*this).depth as usize + i as usize)
    }
}

verus! {

/// Account a newly installed endpoint cap (retype's channel arm, §2.5; §3.3
/// peer-closed accounting).
///
/// Verified (plan §3b): bumps `end_caps[end]` by one, leaving `slot_view`/
/// `refs_view` and every other channel field untouched. The `requires` bound on
/// the count discharges the `+ 1` (no `u32` wrap); the caller (3c's
/// `retype_install`) supplies it from the freshly carved channel's zero counts.
pub fn endpoint_cap_added<S: Store>(store: &mut S, ch: ObjId, end: ChanEnd)
    requires
        old(store).chan_view().dom().contains(ch),
        old(store).chan_view()[ch].end_caps.len() == 2,
        old(store).chan_view()[ch].end_caps[end_idx_spec(end)] < u32::MAX as nat,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view().insert(
            ch,
            ChanView {
                end_caps: old(store).chan_view()[ch].end_caps.update(
                    end_idx_spec(end),
                    (old(store).chan_view()[ch].end_caps[end_idx_spec(end)] + 1) as nat),
                ..old(store).chan_view()[ch]
            }),
{
    let e = end_idx(end);
    store.set_chan_end_caps(ch, e, store.chan_end_caps(ch, e) + 1);
}

/// Called on every endpoint-cap deletion; the last cap of an end raises
/// the other end's peer-closed event (§3.3, session cleanup §2.4).
///
/// Verified (plan §3e): decrements `end_caps[end]`, then — only when that count
/// reaches zero — fires the *other* end's peer-closed event through the verified
/// `fire` (3b). The `requires` bound (`> 0`) discharges the `- 1` (no `u32`
/// wrap). The `slot_view`/`chan_view` frames hold on every path (`fire` keeps
/// both); the `refs_view` frame is **conditional** — the non-firing branch
/// leaves it untouched (the only mutation, `set_chan_end_caps`, frames it), but
/// the firing branch delegates to `signal`, which is permitted to perturb
/// `refs_view` (a waiter's queued ref), so nothing is asserted there.
pub fn endpoint_cap_dropped<S: Store>(store: &mut S, ch: ObjId, end: ChanEnd)
    requires
        old(store).chan_view().dom().contains(ch),
        old(store).chan_view()[ch].end_caps.len() == 2,
        old(store).chan_view()[ch].end_caps[end_idx_spec(end)] > 0,
        cspace::binding_notif_wf(old(store).chan_view(), old(store).notif_view(),
            old(store).tcb_view(), ch),
        cspace::binding_refs_ok(old(store).chan_view(), old(store).notif_view(),
            old(store).refs_view(), ch, 1 - end_idx_spec(end), EV_PEER_CLOSED as int),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view().insert(
            ch,
            ChanView {
                end_caps: old(store).chan_view()[ch].end_caps.update(
                    end_idx_spec(end),
                    (old(store).chan_view()[ch].end_caps[end_idx_spec(end)] - 1) as nat),
                ..old(store).chan_view()[ch]
            }),
        old(store).chan_view()[ch].end_caps[end_idx_spec(end)] != 1
            ==> final(store).refs_view() == old(store).refs_view(),
        cspace::binding_notif_wf(final(store).chan_view(), final(store).notif_view(),
            final(store).tcb_view(), ch),
{
    let e = end_idx(end);
    store.set_chan_end_caps(ch, e, store.chan_end_caps(ch, e) - 1);
    // `set_chan_end_caps` left the bindings (and notif/TCB views) untouched, so the
    // binding invariant + the fired binding's refs side-condition carry to the fire.
    assert(store.chan_view()[ch].bindings == old(store).chan_view()[ch].bindings);
    if store.chan_end_caps(ch, e) == 0 {
        fire(store, ch, 1 - e, EV_PEER_CLOSED);
    }
}

/// Raise an endpoint's event into its bound notification, if bound (§3.6).
///
/// Verified (plan §3b frame; §4b signal discharge): reads a binding (a getter) and
/// conditionally calls `signal` (now a *proven* body, doc/results/32). `signal`'s new
/// preconditions — the bound notification is live + `notif_wf`, and a queued waiter
/// implies `refs > 0` — are discharged from `cspace::binding_notif_wf` (the named
/// binding-liveness invariant) and the per-call refs clause. `slot_view`/`chan_view`
/// stay unchanged (the §3d frame `send`/`recv` need); `binding_notif_wf` is *preserved*
/// (signal preserves the fired notification's `notif_wf` and, via
/// `cspace::lemma_notif_wf_frame`, leaves every other bound notification's intact).
fn fire<S: Store>(store: &mut S, ch: ObjId, end: usize, event: usize)
    requires
        old(store).chan_view().dom().contains(ch),
        end < 2,
        event < 3,
        cspace::binding_notif_wf(old(store).chan_view(), old(store).notif_view(),
            old(store).tcb_view(), ch),
        cspace::binding_refs_ok(old(store).chan_view(), old(store).notif_view(),
            old(store).refs_view(), ch, end as int, event as int),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        cspace::binding_notif_wf(final(store).chan_view(), final(store).notif_view(),
            final(store).tcb_view(), ch),
{
    let b = store.chan_binding(ch, end, event);
    if let Some(n) = b.notif {
        // `n` is `(end, event)`'s bound notification; `binding_notif_wf(old)` makes it
        // live + `notif_wf`, discharging `signal`'s structural preconditions.
        assert(old(store).chan_view()[ch].bindings[(end as int, event as int)].notif is Some);
        notification::signal(store, n, b.bits);
        proof {
            let cvf = store.chan_view();
            let nvf = store.notif_view();
            let tvf = store.tcb_view();
            assert(nvf.dom() == old(store).notif_view().dom());
            assert forall|e: int, v: int|
                (0 <= e < 2 && 0 <= v < 3
                    && #[trigger] cvf[ch].bindings[(e, v)].notif is Some) implies {
                    let m = cvf[ch].bindings[(e, v)].notif->Some_0;
                    nvf.dom().contains(m) && cspace::notif_wf(nvf, tvf, m)
                } by {
                let m = cvf[ch].bindings[(e, v)].notif->Some_0;
                // `cvf == old.cv` (signal frames chan_view), so the old invariant covers
                // this binding; the fired notification `n` is reproven by signal, every
                // other by the frame lemma (signal touched no waiter of `m != n`).
                assert(old(store).chan_view()[ch].bindings[(e, v)].notif is Some);
                if m != n {
                    cspace::lemma_notif_wf_frame(old(store).notif_view(),
                        old(store).tcb_view(), nvf, tvf, m);
                }
            }
        }
    }
}

/// The `refs_view` after `bind` releases `old_notif`'s ref and then adds
/// `new_notif`'s — the decrement-before-increment order the body performs, so a
/// rebind to the *same* notification (`old_notif == new_notif`) is provably
/// net-zero (the second `insert` reads the already-decremented count). The first
/// installment of `refcount_sound`'s binding term; the full census lands
/// phases 4–5.
pub open spec fn bind_refs_post(
    r0: Map<ObjId, nat>,
    old_notif: Option<ObjId>,
    new_notif: Option<ObjId>,
) -> Map<ObjId, nat> {
    let r1 = match old_notif {
        Some(no) => r0.insert(no, (r0[no] - 1) as nat),
        None => r0,
    };
    match new_notif {
        Some(nn) => r1.insert(nn, (r1[nn] + 1) as nat),
        None => r1,
    }
}

/// Configure an endpoint's event binding (holder-configured, §3.6).
/// Replacing a binding releases the old notification's ref and adds the new
/// one's (§3.6 binding-refcount discipline).
///
/// Verified (plan §3e): installs `Binding { notif, bits }` at `(end, event)`,
/// leaving `slot_view` and every other channel field untouched; the `refs_view`
/// delta is `bind_refs_post`. The `requires` refcount bounds discharge the
/// `- 1` (old notif's ref, `> 0`) and `+ 1` (new notif's ref, `< u32::MAX`).
pub fn bind<S: Store>(
    store: &mut S,
    ch: ObjId,
    end: ChanEnd,
    event: usize,
    notif: Option<ObjId>,
    bits: u64,
)
    requires
        old(store).chan_view().dom().contains(ch),
        event < 3,
        old(store).chan_view()[ch].bindings[(end_idx_spec(end), event as int)].notif is Some
            ==> old(store).refs_view().dom().contains(
                    old(store).chan_view()[ch].bindings[(end_idx_spec(end), event as int)].notif->Some_0)
                && old(store).refs_view()[
                    old(store).chan_view()[ch].bindings[(end_idx_spec(end), event as int)].notif->Some_0] > 0,
        notif is Some ==> old(store).refs_view().dom().contains(notif->Some_0)
            && old(store).refs_view()[notif->Some_0] < u32::MAX as nat,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view().insert(
            ch,
            ChanView {
                bindings: old(store).chan_view()[ch].bindings.insert(
                    (end_idx_spec(end), event as int),
                    Binding { notif, bits }),
                ..old(store).chan_view()[ch]
            }),
        final(store).refs_view() == bind_refs_post(
            old(store).refs_view(),
            old(store).chan_view()[ch].bindings[(end_idx_spec(end), event as int)].notif,
            notif),
{
    let e = end_idx(end);
    let old_b = store.chan_binding(ch, e, event);
    if let Some(n) = old_b.notif {
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
/// Verified (plan §3d): on `Ok` the message is enqueued FIFO at the tail —
/// `ring_fifo` of the sending ring grows by `Seq::push`, the other ring is
/// untouched — the supplied caps move out of the sender's slots (move totality,
/// via the verified `slot_move`), and `chan_wf` is preserved; the readable event
/// is then fired (`fire`, framing slot/chan). On `Full`/`PeerClosed` the store
/// is unchanged. The caps precondition is what the kernel naturally supplies:
/// each source slot is a live, non-empty cspace resident, disjoint from the
/// channel's own ring caps and pairwise distinct.
pub fn send<S: Store>(
    store: &mut S,
    ch: ObjId,
    end: ChanEnd,
    data: &[u8],
    caps: &[Option<SlotId>; MSG_CAPS],
) -> (res: Result<(), ChanError>)
    requires
        cspace::chan_wf(old(store).chan_view(), old(store).slot_view(), ch),
        cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        data.len() <= MSG_PAYLOAD,
        forall|c: int| #![trigger caps@[c]]
            0 <= c < 4 && caps@[c] is Some ==> (
                old(store).slot_view().dom().contains(caps@[c]->Some_0)
                && !cspace::is_empty_cap(old(store).slot_view()[caps@[c]->Some_0].cap)
                && !cspace::is_ring_cap_of(old(store).chan_view()[ch], caps@[c]->Some_0)),
        forall|c1: int, c2: int| #![trigger caps@[c1], caps@[c2]]
            0 <= c1 < 4 && 0 <= c2 < 4 && c1 != c2
                && caps@[c1] is Some && caps@[c2] is Some
                ==> caps@[c1]->Some_0 != caps@[c2]->Some_0,
        cspace::binding_notif_wf(old(store).chan_view(), old(store).notif_view(),
            old(store).tcb_view(), ch),
        cspace::binding_refs_ok(old(store).chan_view(), old(store).notif_view(),
            old(store).refs_view(), ch, 1 - end_idx_spec(end), EV_READABLE as int),
    ensures
        cspace::binding_notif_wf(final(store).chan_view(), final(store).notif_view(),
            final(store).tcb_view(), ch),
        res is Err ==> (
            final(store).slot_view() == old(store).slot_view()
            && final(store).chan_view() == old(store).chan_view()
            && final(store).refs_view() == old(store).refs_view()),
        res is Ok ==> (
            cspace::chan_wf(final(store).chan_view(), final(store).slot_view(), ch)
            && cspace::cspace_wf(final(store).slot_view())
            && final(store).slot_view().dom() == old(store).slot_view().dom()
            && final(store).slot_view().dom().finite()
            && final(store).chan_view()[ch].depth == old(store).chan_view()[ch].depth
            && final(store).chan_view()[ch].head == old(store).chan_view()[ch].head
            && final(store).chan_view()[ch].count[end_idx_spec(end)]
                   == old(store).chan_view()[ch].count[end_idx_spec(end)] + 1
            && cspace::ring_fifo(final(store).chan_view()[ch], final(store).slot_view(), end_idx_spec(end))
                   == cspace::ring_fifo(old(store).chan_view()[ch], old(store).slot_view(), end_idx_spec(end)).push(
                       cspace::ring_msg(final(store).chan_view()[ch], final(store).slot_view(), end_idx_spec(end),
                           (old(store).chan_view()[ch].head[end_idx_spec(end)] as int
                               + old(store).chan_view()[ch].count[end_idx_spec(end)] as int)
                               % (old(store).chan_view()[ch].depth as int)))
            && cspace::ring_fifo(final(store).chan_view()[ch], final(store).slot_view(), 1 - end_idx_spec(end))
                   == cspace::ring_fifo(old(store).chan_view()[ch], old(store).slot_view(), 1 - end_idx_spec(end))
            && forall|c: int| 0 <= c < 4 && caps@[c] is Some
                   ==> cspace::is_empty_cap(final(store).slot_view()[caps@[c]->Some_0].cap)),
{
    let ghost sv0 = old(store).slot_view();
    let ghost cv0 = old(store).chan_view();
    let ghost r0 = old(store).refs_view();
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();

    let e = end_idx(end);
    if store.chan_end_caps(ch, 1 - e) == 0 {
        return Err(ChanError::PeerClosed);
    }
    let ring = e; // end A sends on ring 0, B on ring 1
    let depth = store.chan_depth(ch);
    if store.chan_count(ch, ring) == depth {
        return Err(ChanError::Full);
    }
    // N < D after the Full guard (chan_wf: count <= depth, and != here).
    let ghost rr = ring as int;
    let ghost hh = cv0[ch].head[rr] as int;
    let ghost nn = cv0[ch].count[rr] as int;
    let ghost dd = cv0[ch].depth as int;
    let i = (store.chan_head(ch, ring) + store.chan_count(ch, ring)) % depth;
    assert(i as int == (hh + nn) % dd);
    let ghost ii = i as int;

    store.set_chan_msg_len(ch, ring, i, data.len() as u16);
    store.chan_msg_write(ch, ring, i, data);
    let ghost cv1 = store.chan_view();
    assert(cv1[ch].ring_cap == cv0[ch].ring_cap);
    assert(cv1[ch].head == cv0[ch].head);
    assert(cv1[ch].count == cv0[ch].count);
    assert(cv1[ch].depth == cv0[ch].depth);
    assert(store.slot_view() == sv0);
    assert(cv1.dom().contains(ch));
    proof {
        // ii (the new tail) is out of the OLD window: every old-window offset
        // j < nn lands on a different index (lemma_window_index_distinct).
        assert(0 <= ii < dd);
        assert(!cspace::in_live_window(cv0[ch], rr, ii)) by {
            assert forall|j: int| #![trigger (cv0[ch].head[rr] + j) % (cv0[ch].depth as int)]
                0 <= j < nn
                implies (cv0[ch].head[rr] + j) % (cv0[ch].depth as int) != ii by {
                cspace::lemma_window_index_distinct(hh, dd, j, nn);
            }
        }
    }

    // ── The cap-move loop: move each supplied cap into its ring slot. ──
    let mut c: usize = 0;
    while c < MSG_CAPS
        invariant
            0 <= c <= 4,
            ring < 2,
            rr == ring as int,
            ii == i as int,
            dd == depth as int,
            store.chan_view() == cv1,
            cv1.dom().contains(ch),
            cv1[ch].ring_cap == cv0[ch].ring_cap,
            cv1[ch].head == cv0[ch].head,
            cv1[ch].count == cv0[ch].count,
            cv1[ch].depth == cv0[ch].depth,
            store.refs_view() == r0,
            store.notif_view() == nv0,
            store.tcb_view() == tv0,
            cspace::cspace_wf(store.slot_view()),
            store.slot_view().dom() == sv0.dom(),
            store.slot_view().dom().finite(),
            cv0[ch].depth > 0,
            dd == cv0[ch].depth as int,
            0 <= ii < dd,
            0 <= ii < cv0[ch].depth,
            !cspace::in_live_window(cv0[ch], rr, ii),
            cspace::chan_wf(cv0, sv0, ch),
            // precondition A (each source slot is live, non-empty, ring-disjoint)
            // and C (sources pairwise distinct), carried in sv0/cv0 terms so the
            // loop body can instantiate them (they are immutable, so preserved).
            forall|cc: int| #![trigger caps@[cc]]
                (0 <= cc < 4 && caps@[cc] is Some) ==> (
                    sv0.dom().contains(caps@[cc]->Some_0)
                    && !cspace::is_empty_cap(sv0[caps@[cc]->Some_0].cap)
                    && !cspace::is_ring_cap_of(cv0[ch], caps@[cc]->Some_0)),
            forall|c1: int, c2: int| #![trigger caps@[c1], caps@[c2]]
                (0 <= c1 < 4 && 0 <= c2 < 4 && c1 != c2
                    && caps@[c1] is Some && caps@[c2] is Some)
                    ==> caps@[c1]->Some_0 != caps@[c2]->Some_0,
            // dsts not yet processed (cc >= c) still empty:
            forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, ii, cc)]]
                (c <= cc < 4) ==> cspace::is_empty_cap(store.slot_view()[cv0[ch].ring_cap[(rr, ii, cc)]].cap),
            // dsts processed (cc < c) filled (Some) or empty (None):
            forall|cc: int| #![trigger caps@[cc], cv0[ch].ring_cap[(rr, ii, cc)]]
                (0 <= cc < c && caps@[cc] is Some)
                ==> store.slot_view()[cv0[ch].ring_cap[(rr, ii, cc)]].cap == sv0[caps@[cc]->Some_0].cap,
            forall|cc: int| #![trigger caps@[cc], cv0[ch].ring_cap[(rr, ii, cc)]]
                (0 <= cc < c && caps@[cc] is None)
                ==> cspace::is_empty_cap(store.slot_view()[cv0[ch].ring_cap[(rr, ii, cc)]].cap),
            // unprocessed srcs (cc >= c) unchanged; processed srcs emptied:
            forall|cc: int| #![trigger caps@[cc]]
                (c <= cc < 4 && caps@[cc] is Some)
                ==> store.slot_view()[caps@[cc]->Some_0].cap == sv0[caps@[cc]->Some_0].cap,
            forall|cc: int| #![trigger caps@[cc]]
                (0 <= cc < c && caps@[cc] is Some)
                ==> cspace::is_empty_cap(store.slot_view()[caps@[cc]->Some_0].cap),
            // every ring slot NOT at (ring, ii) unchanged:
            forall|r2: int, idx2: int, c2: int| #![trigger cv0[ch].ring_cap[(r2, idx2, c2)]]
                (0 <= r2 < 2 && 0 <= idx2 < cv0[ch].depth && 0 <= c2 < 4 && (r2 != rr || idx2 != ii))
                ==> store.slot_view()[cv0[ch].ring_cap[(r2, idx2, c2)]].cap
                        == sv0[cv0[ch].ring_cap[(r2, idx2, c2)]].cap,
        decreases 4 - c,
    {
        let src_opt = caps[c];
        if let Some(src) = src_opt {
            let dst = store.chan_ring_cap(ch, ring, i, c);
            assert(caps@[c as int] is Some);
            assert(src == caps@[c as int]->Some_0);
            assert(dst == cv0[ch].ring_cap[(rr, ii, c as int)]);
            proof {
                // src is a live, non-empty, ring-disjoint slot (precondition A @ c);
                // dst empty (cc>=c clause @ cc=c); src != dst (B, on the ring_cap term).
                assert(0 <= c < 4 && caps@[c as int] is Some);
                assert(sv0.dom().contains(src)
                    && !cspace::is_empty_cap(sv0[src].cap)
                    && !cspace::is_ring_cap_of(cv0[ch], src));
                assert(sv0.dom().contains(cv0[ch].ring_cap[(rr, ii, c as int)]));
                assert(store.slot_view()[src].cap == sv0[src].cap);
                assert(!cspace::is_empty_cap(store.slot_view()[src].cap));
                assert(cspace::is_empty_cap(store.slot_view()[dst].cap));
                assert(store.slot_view().dom().contains(src));
                assert(store.slot_view().dom().contains(dst));
                assert(src != dst) by {
                    if src == dst {
                        assert(cv0[ch].ring_cap[(rr, ii, c as int)] == src);
                        assert(cspace::is_ring_cap_of(cv0[ch], src));
                    }
                }
            }
            cspace::slot_move(store, src, dst);
            proof {
                let ghost sv2 = store.slot_view();
                assert(sv2[dst].cap == sv0[src].cap);
                assert(cspace::is_empty_cap(sv2[src].cap));
                // (D1) every ring cap of ch differs from src (precondition B).
                assert forall|r3: int, i3: int, c3: int| #![trigger cv0[ch].ring_cap[(r3, i3, c3)]]
                    (0 <= r3 < 2 && 0 <= i3 < cv0[ch].depth && 0 <= c3 < 4)
                    implies cv0[ch].ring_cap[(r3, i3, c3)] != src by {
                    if cv0[ch].ring_cap[(r3, i3, c3)] == src {
                        assert(cspace::is_ring_cap_of(cv0[ch], src));
                    }
                }
                // Re-establish each frame clause for c+1 (injectivity gives x != dst at
                // a different ring index; D1 gives ring caps != src; C/A give the
                // sender-cap disequalities; slot_move's cap-frame does the rest).
                assert forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, ii, cc)]]
                    (c + 1 <= cc < 4) implies cspace::is_empty_cap(sv2[cv0[ch].ring_cap[(rr, ii, cc)]].cap) by {
                    assert(cv0[ch].ring_cap[(rr, ii, cc)] != dst);
                }
                assert forall|cc: int| #![trigger caps@[cc], cv0[ch].ring_cap[(rr, ii, cc)]]
                    (0 <= cc < c + 1 && caps@[cc] is Some)
                    implies sv2[cv0[ch].ring_cap[(rr, ii, cc)]].cap == sv0[caps@[cc]->Some_0].cap by {
                    if cc < c {
                        assert(cv0[ch].ring_cap[(rr, ii, cc)] != dst);
                    } else {
                        assert(cv0[ch].ring_cap[(rr, ii, cc)] == dst);
                    }
                }
                assert forall|cc: int| #![trigger caps@[cc], cv0[ch].ring_cap[(rr, ii, cc)]]
                    (0 <= cc < c + 1 && caps@[cc] is None)
                    implies cspace::is_empty_cap(sv2[cv0[ch].ring_cap[(rr, ii, cc)]].cap) by {
                    assert(cv0[ch].ring_cap[(rr, ii, cc)] != dst);
                }
                assert forall|cc: int| #![trigger caps@[cc]]
                    (c + 1 <= cc < 4 && caps@[cc] is Some)
                    implies sv2[caps@[cc]->Some_0].cap == sv0[caps@[cc]->Some_0].cap by {
                    if caps@[cc]->Some_0 == dst {
                        assert(cspace::is_ring_cap_of(cv0[ch], caps@[cc]->Some_0));
                    }
                }
                assert forall|cc: int| #![trigger caps@[cc]]
                    (0 <= cc < c + 1 && caps@[cc] is Some)
                    implies cspace::is_empty_cap(sv2[caps@[cc]->Some_0].cap) by {
                    if cc < c {
                        if caps@[cc]->Some_0 == dst {
                            assert(cspace::is_ring_cap_of(cv0[ch], caps@[cc]->Some_0));
                        }
                    } else {
                        assert(caps@[cc]->Some_0 == src);
                    }
                }
                assert forall|r2: int, idx2: int, c2: int| #![trigger cv0[ch].ring_cap[(r2, idx2, c2)]]
                    (0 <= r2 < 2 && 0 <= idx2 < cv0[ch].depth && 0 <= c2 < 4 && (r2 != rr || idx2 != ii))
                    implies sv2[cv0[ch].ring_cap[(r2, idx2, c2)]].cap
                        == sv0[cv0[ch].ring_cap[(r2, idx2, c2)]].cap by {
                    assert(cv0[ch].ring_cap[(r2, idx2, c2)] != dst);
                    assert(cv0[ch].ring_cap[(r2, idx2, c2)] != src);
                }
            }
        } else {
            // None: store unchanged; the cc==c dst (empty, old cc>=c clause @ cc=c)
            // joins the cc<c+1 None-empty class; every other clause shifts trivially.
            assert(cspace::is_empty_cap(store.slot_view()[cv0[ch].ring_cap[(rr, ii, c as int)]].cap));
        }
        c += 1;
    }

    store.set_chan_count(ch, ring, store.chan_count(ch, ring) + 1);
    let ghost cv2 = store.chan_view();
    // The enqueue framed the notif/TCB/refs views and the channel's bindings, so the
    // binding invariant + the fired binding's refs side-condition carry to the fire.
    assert(store.notif_view() == old(store).notif_view());
    assert(store.tcb_view() == old(store).tcb_view());
    assert(store.refs_view() == old(store).refs_view());
    assert(store.chan_view()[ch].bindings == cv0[ch].bindings);
    fire(store, ch, 1 - e, EV_READABLE);

    proof {
        let svf = store.slot_view();
        let cvf = store.chan_view();
        assert(cvf == cv2);
        assert(cvf[ch].count[rr] == nn + 1);
        assert(cvf[ch].head == cv0[ch].head);
        assert(cvf[ch].depth == cv0[ch].depth);
        assert(cvf[ch].ring_cap == cv0[ch].ring_cap);
        assert(nn < dd);

        // ii is the nn-th window position of the *new* window, hence in it.
        assert(cspace::in_live_window(cvf[ch], rr, ii)) by {
            assert(ii == (cvf[ch].head[rr] as int + nn) % (cvf[ch].depth as int));
            assert(0 <= nn < cvf[ch].count[rr]);
        }

        // chan_wf(cvf, svf, ch). The windowing coupling is the only nontrivial
        // clause: an out-of-(new)window ring slot is out-of-old-window too (the
        // window only grew by ii) and not at (rr,ii), so the frame keeps it at its
        // sv0 value, which was empty.
        assert(cspace::chan_wf(cvf, svf, ch)) by {
            assert forall|r2: int, idx2: int, c2: int|
                (0 <= r2 < 2 && 0 <= idx2 < cvf[ch].depth && 0 <= c2 < 4
                    && !cspace::in_live_window(cvf[ch], r2, idx2))
                implies cspace::is_empty_cap(svf[#[trigger] cvf[ch].ring_cap[(r2, idx2, c2)]].cap) by {
                // (r2,idx2) != (rr,ii): ii is in-window, idx2 is not.
                assert(r2 != rr || idx2 != ii);
                // out-of-new ⟹ out-of-old: the old window's witness j (< nn) also
                // witnesses the new window (< nn+1), so old-window ⊆ new-window.
                if cspace::in_live_window(cv0[ch], r2, idx2) {
                    let j = choose|j: int| #![trigger (cv0[ch].head[r2] + j) % (cv0[ch].depth as int)]
                        0 <= j < cv0[ch].count[r2] && idx2 == (cv0[ch].head[r2] + j) % (cv0[ch].depth as int);
                    assert(0 <= j < cvf[ch].count[r2]
                        && idx2 == (cvf[ch].head[r2] + j) % (cvf[ch].depth as int));
                }
                assert(!cspace::in_live_window(cv0[ch], r2, idx2));
            }
        }

        // FIFO append on the sending ring: ring_fifo grows by Seq::push.
        let new_msg = cspace::ring_msg(cvf[ch], svf, rr, ii);
        assert(cspace::ring_fifo(cvf[ch], svf, rr) =~= cspace::ring_fifo(cv0[ch], sv0, rr).push(new_msg)) by {
            assert(cspace::ring_fifo(cvf[ch], svf, rr).len() == nn + 1);
            assert(cspace::ring_fifo(cv0[ch], sv0, rr).push(new_msg).len() == nn + 1);
            assert forall|j: int| 0 <= j < nn + 1
                implies cspace::ring_fifo(cvf[ch], svf, rr)[j]
                    == cspace::ring_fifo(cv0[ch], sv0, rr).push(new_msg)[j] by {
                if j < nn {
                    // in-window message j unchanged: its index (hh+j)%dd != ii, so
                    // its msg_len and ring caps are framed to sv0.
                    cspace::lemma_window_index_distinct(hh, dd, j, nn);
                    assert((cvf[ch].head[rr] + j) % (cvf[ch].depth as int) == (hh + j) % dd);
                    cspace::lemma_ring_msg_eq(cvf[ch], svf, cv0[ch], sv0, rr, (hh + j) % dd);
                } else {
                    assert((cvf[ch].head[rr] + j) % (cvf[ch].depth as int) == ii);
                }
            }
        }

        // The other ring is untouched: its cursors and slots are unchanged.
        assert(cspace::ring_fifo(cvf[ch], svf, 1 - rr) =~= cspace::ring_fifo(cv0[ch], sv0, 1 - rr)) by {
            assert(cspace::ring_fifo(cvf[ch], svf, 1 - rr).len()
                == cspace::ring_fifo(cv0[ch], sv0, 1 - rr).len());
            assert forall|j: int| #![trigger cspace::ring_fifo(cvf[ch], svf, 1 - rr)[j]]
                0 <= j < cv0[ch].count[1 - rr]
                implies cspace::ring_fifo(cvf[ch], svf, 1 - rr)[j]
                    == cspace::ring_fifo(cv0[ch], sv0, 1 - rr)[j] by {
                assert((cvf[ch].head[1 - rr] + j) % (cvf[ch].depth as int)
                    == (cv0[ch].head[1 - rr] + j) % (cv0[ch].depth as int));
                cspace::lemma_ring_msg_eq(cvf[ch], svf, cv0[ch], sv0, 1 - rr,
                    (cv0[ch].head[1 - rr] + j) % (cv0[ch].depth as int));
            }
        }
    }
    Ok(())
}

} // verus!

verus! {

/// Receive into `buf`, installing caps into `dests`. If any arriving cap
/// has no free destination the receive fails and the message stays queued
/// (§3.3) — receive-side exhaustion is the receiver's own problem.
/// Revocation may have emptied queued slots in flight; receivers see those
/// as absent caps (§3.4 null slots).
///
/// Verified (plan §3d): two-pass atomicity — pass 1 is read-only, so `Empty`/
/// `NoCapSlot` leave the store (and the queued message) unchanged; pass 2 moves
/// the head message's caps into `dests` and dequeues, so `ring_fifo` of the
/// receiving ring loses its head (`Seq::drop_first`), the other ring is
/// untouched, and `chan_wf` is preserved. A ring cap emptied in flight by
/// revocation is delivered as absent (null-slot tolerance) — never a panic, by
/// the guarded unwrap. `dests` are live, empty, ring-disjoint, pairwise-distinct
/// cspace residents (what the kernel supplies).
pub fn recv<S: Store>(
    store: &mut S,
    ch: ObjId,
    end: ChanEnd,
    buf: &mut [u8; MSG_PAYLOAD],
    dests: &[Option<SlotId>; MSG_CAPS],
) -> (res: Result<(usize, u8), ChanError>)
    requires
        cspace::chan_wf(old(store).chan_view(), old(store).slot_view(), ch),
        cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        forall|c: int| #![trigger dests@[c]]
            0 <= c < 4 && dests@[c] is Some ==> (
                old(store).slot_view().dom().contains(dests@[c]->Some_0)
                && cspace::is_empty_cap(old(store).slot_view()[dests@[c]->Some_0].cap)
                && !cspace::is_ring_cap_of(old(store).chan_view()[ch], dests@[c]->Some_0)),
        forall|c1: int, c2: int| #![trigger dests@[c1], dests@[c2]]
            0 <= c1 < 4 && 0 <= c2 < 4 && c1 != c2
                && dests@[c1] is Some && dests@[c2] is Some
                ==> dests@[c1]->Some_0 != dests@[c2]->Some_0,
        cspace::binding_notif_wf(old(store).chan_view(), old(store).notif_view(),
            old(store).tcb_view(), ch),
        cspace::binding_refs_ok(old(store).chan_view(), old(store).notif_view(),
            old(store).refs_view(), ch, 1 - end_idx_spec(end), EV_WRITABLE as int),
    ensures
        cspace::binding_notif_wf(final(store).chan_view(), final(store).notif_view(),
            final(store).tcb_view(), ch),
        res is Err ==> (
            final(store).slot_view() == old(store).slot_view()
            && final(store).chan_view() == old(store).chan_view()
            && final(store).refs_view() == old(store).refs_view()),
        res is Ok ==> (
            cspace::chan_wf(final(store).chan_view(), final(store).slot_view(), ch)
            && cspace::cspace_wf(final(store).slot_view())
            && final(store).slot_view().dom() == old(store).slot_view().dom()
            && final(store).slot_view().dom().finite()
            && final(store).chan_view()[ch].depth == old(store).chan_view()[ch].depth
            && final(store).chan_view()[ch].count[1 - end_idx_spec(end)]
                   == old(store).chan_view()[ch].count[1 - end_idx_spec(end)] - 1
            && cspace::ring_fifo(final(store).chan_view()[ch], final(store).slot_view(), 1 - end_idx_spec(end))
                   == cspace::ring_fifo(old(store).chan_view()[ch], old(store).slot_view(), 1 - end_idx_spec(end)).drop_first()
            && cspace::ring_fifo(final(store).chan_view()[ch], final(store).slot_view(), end_idx_spec(end))
                   == cspace::ring_fifo(old(store).chan_view()[ch], old(store).slot_view(), end_idx_spec(end))
            && res->Ok_0.0 as nat == old(store).chan_view()[ch].msg_len[
                   (1 - end_idx_spec(end), old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int)]),
{
    let ghost sv0 = old(store).slot_view();
    let ghost cv0 = old(store).chan_view();
    let ghost r0 = old(store).refs_view();
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();

    let e = end_idx(end);
    let ring = 1 - e;
    if store.chan_count(ch, ring) == 0 {
        return Err(ChanError::Empty);
    }
    let head = store.chan_head(ch, ring);
    let ghost rr = ring as int;
    let ghost hh = head as int;
    let ghost nn = cv0[ch].count[rr] as int;
    let ghost dd = cv0[ch].depth as int;
    assert(hh == cv0[ch].head[rr]);
    assert(nn >= 1);
    assert(0 <= hh < dd);

    // ── Pass 1 (read-only): every non-empty arriving cap must have a free dest. ──
    let mut c: usize = 0;
    while c < MSG_CAPS
        invariant
            0 <= c <= 4,
            ring < 2,
            rr == ring as int,
            hh == head as int,
            store.slot_view() == sv0,
            store.chan_view() == cv0,
            store.refs_view() == r0,
            store.notif_view() == nv0,
            store.tcb_view() == tv0,
            // Pass 1 is read-only, so the binding invariant rides through unchanged — it
            // is what each `NoCapSlot` early-return needs to re-establish its postcondition.
            cspace::binding_notif_wf(store.chan_view(), store.notif_view(), store.tcb_view(), ch),
            cspace::chan_wf(cv0, sv0, ch),
            0 <= hh < cv0[ch].depth,
            forall|cc: int| #![trigger dests@[cc]]
                (0 <= cc < 4 && dests@[cc] is Some)
                ==> sv0.dom().contains(dests@[cc]->Some_0),
            forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                (0 <= cc < c && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))
                ==> (dests@[cc] is Some
                    && cspace::is_empty_cap(sv0[dests@[cc]->Some_0].cap)),
        decreases 4 - c,
    {
        let src = store.chan_ring_cap(ch, ring, head, c);
        assert(src == cv0[ch].ring_cap[(rr, hh, c as int)]);
        if !cspace::cap_is_empty(store.slot(src).cap) {
            match dests[c] {
                None => return Err(ChanError::NoCapSlot),
                Some(d) => {
                    assert(d == dests@[c as int]->Some_0);
                    if !cspace::cap_is_empty(store.slot(d).cap) {
                        return Err(ChanError::NoCapSlot);
                    }
                }
            }
        }
        c += 1;
    }

    // ── Pass 2: move each non-empty arriving cap into its dest, dequeue. ──
    let mut mask = 0u8;
    let mut c2: usize = 0;
    while c2 < MSG_CAPS
        invariant
            0 <= c2 <= 4,
            ring < 2,
            rr == ring as int,
            hh == head as int,
            dd == cv0[ch].depth as int,
            store.chan_view() == cv0,
            store.refs_view() == r0,
            store.notif_view() == nv0,
            store.tcb_view() == tv0,
            cspace::cspace_wf(store.slot_view()),
            store.slot_view().dom() == sv0.dom(),
            store.slot_view().dom().finite(),
            cv0[ch].depth > 0,
            0 <= hh < cv0[ch].depth,
            cspace::chan_wf(cv0, sv0, ch),
            // pass-1 result, carried in:
            forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                (0 <= cc < 4 && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))
                ==> (dests@[cc] is Some
                    && cspace::is_empty_cap(sv0[dests@[cc]->Some_0].cap)),
            // dests precondition (live, empty, ring-disjoint, distinct), in sv0/cv0:
            forall|cc: int| #![trigger dests@[cc]]
                (0 <= cc < 4 && dests@[cc] is Some) ==> (
                    sv0.dom().contains(dests@[cc]->Some_0)
                    && cspace::is_empty_cap(sv0[dests@[cc]->Some_0].cap)
                    && !cspace::is_ring_cap_of(cv0[ch], dests@[cc]->Some_0)),
            forall|d1: int, d2: int| #![trigger dests@[d1], dests@[d2]]
                (0 <= d1 < 4 && 0 <= d2 < 4 && d1 != d2
                    && dests@[d1] is Some && dests@[d2] is Some)
                    ==> dests@[d1]->Some_0 != dests@[d2]->Some_0,
            // processed head caps (cc < c2) emptied; unprocessed unchanged:
            forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                (0 <= cc < c2) ==> cspace::is_empty_cap(store.slot_view()[cv0[ch].ring_cap[(rr, hh, cc)]].cap),
            forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                (c2 <= cc < 4) ==> store.slot_view()[cv0[ch].ring_cap[(rr, hh, cc)]].cap
                        == sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap,
            // unprocessed dests (cc >= c2) unchanged (still empty):
            forall|cc: int| #![trigger dests@[cc]]
                (c2 <= cc < 4 && dests@[cc] is Some)
                ==> store.slot_view()[dests@[cc]->Some_0].cap == sv0[dests@[cc]->Some_0].cap,
            // every ring slot NOT at (rr, hh) unchanged:
            forall|r2: int, idx2: int, c3: int| #![trigger cv0[ch].ring_cap[(r2, idx2, c3)]]
                (0 <= r2 < 2 && 0 <= idx2 < cv0[ch].depth && 0 <= c3 < 4 && (r2 != rr || idx2 != hh))
                ==> store.slot_view()[cv0[ch].ring_cap[(r2, idx2, c3)]].cap
                        == sv0[cv0[ch].ring_cap[(r2, idx2, c3)]].cap,
        decreases 4 - c2,
    {
        let src = store.chan_ring_cap(ch, ring, head, c2);
        assert(src == cv0[ch].ring_cap[(rr, hh, c2 as int)]);
        if !cspace::cap_is_empty(store.slot(src).cap) {
            assert(!cspace::is_empty_cap(sv0[src].cap));
            assert(dests@[c2 as int] is Some
                && cspace::is_empty_cap(sv0[dests@[c2 as int]->Some_0].cap));
            let d = dests[c2].unwrap();
            assert(d == dests@[c2 as int]->Some_0);
            proof {
                // src non-empty now (unprocessed-head clause @ cc=c2); dst d empty
                // (unprocessed-dest clause @ cc=c2); src != d (d not a ring cap, B).
                assert(store.slot_view()[src].cap == sv0[src].cap);
                assert(store.slot_view()[d].cap == sv0[d].cap);
                assert(sv0.dom().contains(d));
                assert(sv0.dom().contains(cv0[ch].ring_cap[(rr, hh, c2 as int)]));
                assert(src != d) by {
                    if src == d {
                        assert(cspace::is_ring_cap_of(cv0[ch], d));
                    }
                }
            }
            cspace::slot_move(store, src, d);
            proof {
                let ghost sv2 = store.slot_view();
                // (D1) every ring cap of ch differs from d (precondition B on d).
                assert forall|r3: int, i3: int, c4: int| #![trigger cv0[ch].ring_cap[(r3, i3, c4)]]
                    (0 <= r3 < 2 && 0 <= i3 < cv0[ch].depth && 0 <= c4 < 4)
                    implies cv0[ch].ring_cap[(r3, i3, c4)] != d by {
                    if cv0[ch].ring_cap[(r3, i3, c4)] == d {
                        assert(cspace::is_ring_cap_of(cv0[ch], d));
                    }
                }
                // Re-establish the frame for c2+1.
                assert forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                    (0 <= cc < c2 + 1) implies cspace::is_empty_cap(sv2[cv0[ch].ring_cap[(rr, hh, cc)]].cap) by {
                    if cc < c2 {
                        assert(cv0[ch].ring_cap[(rr, hh, cc)] != src);
                        assert(cv0[ch].ring_cap[(rr, hh, cc)] != d);
                    } else {
                        assert(cv0[ch].ring_cap[(rr, hh, cc)] == src);
                    }
                }
                assert forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                    (c2 + 1 <= cc < 4) implies sv2[cv0[ch].ring_cap[(rr, hh, cc)]].cap
                        == sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap by {
                    assert(cv0[ch].ring_cap[(rr, hh, cc)] != src);
                    assert(cv0[ch].ring_cap[(rr, hh, cc)] != d);
                }
                assert forall|cc: int| #![trigger dests@[cc]]
                    (c2 + 1 <= cc < 4 && dests@[cc] is Some)
                    implies sv2[dests@[cc]->Some_0].cap == sv0[dests@[cc]->Some_0].cap by {
                    assert(dests@[cc]->Some_0 != d);
                    if dests@[cc]->Some_0 == src {
                        assert(cspace::is_ring_cap_of(cv0[ch], dests@[cc]->Some_0));
                    }
                }
                assert forall|r2: int, idx2: int, c3: int| #![trigger cv0[ch].ring_cap[(r2, idx2, c3)]]
                    (0 <= r2 < 2 && 0 <= idx2 < cv0[ch].depth && 0 <= c3 < 4 && (r2 != rr || idx2 != hh))
                    implies sv2[cv0[ch].ring_cap[(r2, idx2, c3)]].cap
                        == sv0[cv0[ch].ring_cap[(r2, idx2, c3)]].cap by {
                    assert(cv0[ch].ring_cap[(r2, idx2, c3)] != src);
                    assert(cv0[ch].ring_cap[(r2, idx2, c3)] != d);
                }
            }
            mask |= 1 << c2;
        } else {
            // null cap (revoked in flight): skip; head cap cc=c2 already empty.
            assert(cspace::is_empty_cap(store.slot_view()[cv0[ch].ring_cap[(rr, hh, c2 as int)]].cap));
        }
        c2 += 1;
    }

    let len = store.chan_msg_len(ch, ring, head);
    assert(len as nat == cv0[ch].msg_len[(rr, hh)]);
    store.chan_msg_read(ch, ring, head, len as usize, buf);
    store.set_chan_msg_len(ch, ring, head, 0);
    let depth = store.chan_depth(ch);
    store.set_chan_head(ch, ring, (head + 1) % depth);
    let ghost cv_h = store.chan_view();
    store.set_chan_count(ch, ring, store.chan_count(ch, ring) - 1);
    let ghost cv2 = store.chan_view();
    // The dequeue framed the notif/TCB/refs views and the channel's bindings, so the
    // binding invariant + the fired binding's refs side-condition carry to the fire.
    assert(store.notif_view() == old(store).notif_view());
    assert(store.tcb_view() == old(store).tcb_view());
    assert(store.refs_view() == old(store).refs_view());
    assert(store.chan_view()[ch].bindings == cv0[ch].bindings);
    fire(store, ch, 1 - e, EV_WRITABLE);

    proof {
        let svf = store.slot_view();
        let cvf = store.chan_view();
        assert(cvf == cv2);
        assert(cvf[ch].count[rr] == nn - 1);
        assert(cvf[ch].head[rr] == (hh + 1) % dd);
        assert(cvf[ch].depth == cv0[ch].depth);
        assert(cvf[ch].ring_cap == cv0[ch].ring_cap);
        assert(cvf[ch].msg_len == cv0[ch].msg_len.insert((rr, hh), 0));

        // chan_wf(cvf, svf, ch): out-of-(new)window ring slots are empty. The new
        // window is the old minus the head index hh; the head slot is now empty
        // (all its caps moved out / already empty), and every other out-of-window
        // slot was out-of-old-window and is unchanged.
        assert(cspace::chan_wf(cvf, svf, ch)) by {
            assert forall|r2: int, idx2: int, c3: int|
                (0 <= r2 < 2 && 0 <= idx2 < cvf[ch].depth && 0 <= c3 < 4
                    && !cspace::in_live_window(cvf[ch], r2, idx2))
                implies cspace::is_empty_cap(svf[#[trigger] cvf[ch].ring_cap[(r2, idx2, c3)]].cap) by {
                if r2 == rr && idx2 == hh {
                    // head slot: every cap emptied in pass 2 (cc < 4).
                } else {
                    // out-of-new ⟹ out-of-old (new window = old minus head hh).
                    if cspace::in_live_window(cv0[ch], r2, idx2) {
                        let j = choose|j: int| #![trigger (cv0[ch].head[r2] + j) % (cv0[ch].depth as int)]
                            0 <= j < cv0[ch].count[r2] && idx2 == (cv0[ch].head[r2] + j) % (cv0[ch].depth as int);
                        if r2 == rr {
                            // idx2 != hh == head, so the witness j is not 0; shift to j-1.
                            assert(cv0[ch].head[r2] == hh);
                            assert(j >= 1) by {
                                if j == 0 {
                                    cspace::lemma_self_mod(hh, dd);
                                    assert(idx2 == hh);
                                }
                            }
                            cspace::lemma_mod_shift_head(cv0[ch].head[r2] as int, dd, j - 1);
                            assert(0 <= j - 1 < cvf[ch].count[r2]);
                            assert(idx2 == (cvf[ch].head[r2] + (j - 1)) % (cvf[ch].depth as int));
                        } else {
                            // other ring: head/count unchanged, witness j stands.
                            assert(0 <= j < cvf[ch].count[r2]);
                            assert(idx2 == (cvf[ch].head[r2] + j) % (cvf[ch].depth as int));
                        }
                    }
                    assert(!cspace::in_live_window(cv0[ch], r2, idx2));
                }
            }
        }

        // FIFO pop on the receiving ring: ring_fifo loses its head (drop_first).
        assert(cspace::ring_fifo(cvf[ch], svf, rr) =~= cspace::ring_fifo(cv0[ch], sv0, rr).drop_first()) by {
            assert(cspace::ring_fifo(cvf[ch], svf, rr).len() == nn - 1);
            assert(cspace::ring_fifo(cv0[ch], sv0, rr).drop_first().len() == nn - 1);
            assert forall|j: int| 0 <= j < nn - 1
                implies cspace::ring_fifo(cvf[ch], svf, rr)[j]
                    == cspace::ring_fifo(cv0[ch], sv0, rr).drop_first()[j] by {
                // after-index ((hh+1)%dd + j)%dd == (hh + (j+1))%dd (old position j+1),
                // which is not the head hh (lemma_window_index_distinct(hh,dd,0,j+1)).
                cspace::lemma_mod_shift_head(hh, dd, j);
                assert(cvf[ch].head[rr] == (hh + 1) % dd);
                assert((cvf[ch].head[rr] + j) % (cvf[ch].depth as int) == (hh + (j + 1)) % dd);
                // idx = (hh+(j+1))%dd is a non-head window position, so its msg_len
                // and ring caps survived the dequeue.
                cspace::lemma_window_index_distinct(hh, dd, 0, j + 1);
                cspace::lemma_self_mod(hh, dd);
                assert((hh + (j + 1)) % dd != hh);
                assert(cvf[ch].msg_len[(rr, (hh + (j + 1)) % dd)]
                    == cv0[ch].msg_len[(rr, (hh + (j + 1)) % dd)]);
                cspace::lemma_ring_msg_eq(cvf[ch], svf, cv0[ch], sv0, rr, (hh + (j + 1)) % dd);
            }
        }

        // The other ring is untouched.
        assert(cspace::ring_fifo(cvf[ch], svf, 1 - rr) =~= cspace::ring_fifo(cv0[ch], sv0, 1 - rr)) by {
            assert(cspace::ring_fifo(cvf[ch], svf, 1 - rr).len()
                == cspace::ring_fifo(cv0[ch], sv0, 1 - rr).len());
            assert forall|j: int| #![trigger cspace::ring_fifo(cvf[ch], svf, 1 - rr)[j]]
                0 <= j < cv0[ch].count[1 - rr]
                implies cspace::ring_fifo(cvf[ch], svf, 1 - rr)[j]
                    == cspace::ring_fifo(cv0[ch], sv0, 1 - rr)[j] by {
                assert((cvf[ch].head[1 - rr] + j) % (cvf[ch].depth as int)
                    == (cv0[ch].head[1 - rr] + j) % (cv0[ch].depth as int));
                cspace::lemma_ring_msg_eq(cvf[ch], svf, cv0[ch], sv0, 1 - rr,
                    (cv0[ch].head[1 - rr] + j) % (cv0[ch].depth as int));
            }
        }
    }
    Ok((len as usize, mask))
}

} // verus!

verus! {

/// Tear a channel down once its last endpoint cap is gone (`refs == 0`): delete
/// every queued cap with ordinary CDT cleanup — cashing a shredded envelope
/// (§3.4) — and release every event binding's notification ref.
///
/// **Assumed, host-test-checked (plan §3e — the declared scope-out, §1.3).** The
/// body recurses through the still-`external_body` `cspace::delete` (the
/// cross-object teardown) and releases binding refs whose soundness needs the
/// full `refcount_sound` census — both of which land in phases 4–5. So like
/// `delete` and `signal`, it carries an `external_body` contract checked against
/// its real body in `test_store.rs` (`check_destroy_channel`), not a Verus body
/// proof. The contract states the robustly-true, checkable core — `cspace_wf`
/// preserved, the arena unchanged in extent, and **every ring-cap slot emptied**.
///
/// **Refcount census (plan §6a).** The contract now also requires and preserves
/// `refcount_sound` and states the `count_nonempty` non-increase 6d's measure
/// needs. The per-binding ref release that had "no clean closed form here" is the
/// census's job: each `-1` is matched by the corresponding `binding_refs` drop
/// (6d closes the body proof). Stated now (still `external_body`, host-checked via
/// `check_destroy_channel`) so `obj_unref` (6c) verifies against the final contract.
#[verifier::external_body]
pub fn destroy_channel<S: Store>(store: &mut S, ch: ObjId)
    requires
        cspace::cspace_wf(old(store).slot_view()),
        cspace::chan_wf(old(store).chan_view(), old(store).slot_view(), ch),
        cspace::refcount_sound(old(store)),
        // Cap→object consistency (plan §6d foundation): the body deletes ring caps of
        // arbitrary kind, so it needs each one's object well-formed. Assumed here
        // (`external_body`), discharged by the body PR; host-checked (`check_destroy_channel`).
        cspace::caps_consistent(old(store)),
    ensures
        cspace::cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        cspace::count_nonempty(final(store).slot_view())
            <= cspace::count_nonempty(old(store).slot_view()),
        cspace::refcount_sound(final(store)),
        cspace::caps_consistent(final(store)),
        forall|r: int, i: int, c: int|
            (0 <= r < 2 && 0 <= i < old(store).chan_view()[ch].depth && 0 <= c < 4)
                ==> cspace::is_empty_cap(
                    final(store).slot_view()[
                        #[trigger] old(store).chan_view()[ch].ring_cap[(r, i, c)]].cap),
{
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

} // verus!
