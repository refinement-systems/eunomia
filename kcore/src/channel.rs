//! Asynchronous IPC channels (spec rev1§3.1-3.4, rev1§3.6).
//!
//! A channel is two endpoints (A, B) over two fixed-depth rings of message
//! slots — ring 0 carries A→B, ring 1 carries B→A. A message slot is a
//! 256-byte inline payload plus 4 real `CapSlot`s: queued caps are
//! CDT-visible and owned by the channel, so revocation sees through queues
//! (rev1§3.4) with no special case.
//!
//! Queue memory comes from the untyped donated at retype; capacity is the
//! creator-chosen depth (rev1§3.2). Send is non-blocking and returns FULL;
//! messages are never dropped. Each endpoint carries fixed binding slots
//! (on-readable / on-writable / on-peer-closed → notification, bits);
//! event delivery never allocates (rev1§3.6).
//!
//! The channel is addressed by an opaque
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
// `slot_view`/`chan_view`/`refs_view` views the contracts quantify over, and
// `ChanView` names the channel ghost view in those contracts; both appear only in
// `requires`/`ensures`, which erase in a normal build — hence unused there.
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
    /// Live endpoint caps per end, for peer-closed (rev1§3.3).
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

/// Ghost mirror of [`end_idx`]: A → 0, B → 1. Lets the contracts name the
/// `end_caps`/ring index a `ChanEnd` selects.
pub open spec fn end_idx_spec(e: ChanEnd) -> int {
    match e {
        ChanEnd::A => 0,
        ChanEnd::B => 1,
    }
}

/// Bit `c` of a `recv` install mask: set iff `recv` moved a non-empty arriving cap
/// into `dests[c]`. Named (not inline `(m >> c) & 1`) so the `recv` ensures and the
/// pass-2 invariant share one canonical trigger (`mask_bit(mask, c)`) and the bit-vector
/// lemmas below have a single shape to discharge.
pub open spec fn mask_bit(m: u8, c: int) -> bool {
    (m >> (c as u64)) & 1u8 == 1u8
}

/// A zero install mask has every bit clear — the loop-entry base case for `recv`'s mask
/// invariant. One bit_vector step (bit reasoning isolated to a lemma).
proof fn lemma_mask_zero(cc: u64)
    requires
        cc < 8,
    ensures
        !mask_bit(0u8, cc as int),
{
    assert((0u8 >> cc) & 1u8 == 0u8) by (bit_vector) requires cc < 8;
}

/// OR-ing in bit `c2` sets exactly that bit of an install mask and leaves the others — the
/// step `recv`'s pass-2 mask invariant needs across `mask |= 1 << c2`. `c2 < 8` covers the
/// `u8` shift; `recv` only ever uses `c2 < 4`.
proof fn lemma_mask_set_bit(m: u8, c2: u64)
    requires
        c2 < 8,
    ensures
        mask_bit(m | (1u8 << c2), c2 as int),
        forall|cc: int| #![trigger mask_bit(m | (1u8 << c2), cc)]
            0 <= cc < 8 && cc != c2 as int
            ==> (mask_bit(m | (1u8 << c2), cc) <==> mask_bit(m, cc)),
{
    assert(((m | (1u8 << c2)) >> c2) & 1u8 == 1u8) by (bit_vector) requires c2 < 8;
    assert forall|cc: int| #![trigger mask_bit(m | (1u8 << c2), cc)]
        0 <= cc < 8 && cc != c2 as int implies
        (mask_bit(m | (1u8 << c2), cc) <==> mask_bit(m, cc)) by {
        let ccu = cc as u64;
        assert(((m | (1u8 << c2)) >> ccu) & 1u8 == (m >> ccu) & 1u8) by (bit_vector)
            requires ccu < 8, c2 < 8, ccu != c2;
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

/// Account a newly installed endpoint cap (retype's channel arm, rev1§2.5; rev1§3.3
/// peer-closed accounting).
///
/// Bumps `end_caps[end]` by one, leaving `slot_view`/
/// `refs_view` and every other channel field untouched. The `requires` bound on
/// the count discharges the `+ 1` (no `u32` wrap); the caller (`retype_install`)
/// supplies it from the freshly carved channel's zero counts.
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
        // `refcount_sound` as a system invariant: `end_caps` is no census term and the
        // bindings are untouched, so refs and census are both unchanged ⇒ a sound census carries.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
{
    let e = end_idx(end);
    store.set_chan_end_caps(ch, e, store.chan_end_caps(ch, e) + 1);
    proof {
        // census-neutral: only `end_caps[ch]` moved (not a census term), the bindings frame.
        if cspace::refcount_sound(old(store)) {
            assert(store.chan_view().dom() == old(store).chan_view().dom());
            assert forall|c: ObjId| #[trigger] old(store).chan_view().dom().contains(c)
                implies store.chan_view()[c].bindings == old(store).chan_view()[c].bindings by {
                if c != ch {
                    assert(store.chan_view()[c] == old(store).chan_view()[c]);
                }
            }
            assert forall|x: ObjId| #[trigger] cspace::obj_census(store, x)
                == cspace::obj_census(old(store), x) by {
                cspace::lemma_binding_refs_frame(old(store).chan_view(), store.chan_view(), x);
            }
            cspace::lemma_refcount_sound_from_census_eq(old(store), store);
        }
    }
}

/// Called on every endpoint-cap deletion; the last cap of an end raises
/// the other end's peer-closed event (rev1§3.3, session cleanup rev1§2.4).
///
/// Decrements `end_caps[end]`, then — only when that count
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
        // The cap→object invariant + the rev1§3.3 endpoint census, both off by one at `(ch, end)`:
        // `delete` cleared the deleted cap's slot before this call, so
        // `end_caps[ch][end]` over-counts the arena by one. The decrement here restores
        // `end_caps_sound` and re-establishes `caps_consistent` (no sibling stranded — a live
        // sibling makes the count ≥ 1, so the over-count is ≥ 2). `delete`-supplied.
        cspace::caps_consistent(old(store)),
        cspace::end_caps_off_by_one(old(store), ch, end_idx_spec(end)),
        // B8C: the peer-closed `fire` faithfully enqueues the woken thread, so carry the
        // ready-queue invariants (`delete` supplies them; `fire` re-establishes them on the wake).
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        // Residency is framed: `set_chan_end_caps` and `fire` both frame `cspace_view`, so
        // `delete`'s Channel branch carries it to `obj_unref`.
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete(final(store).ready_view(), final(store).tcb_view()),
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
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
        // The refcount census moves in lockstep: the only state
        // change before a possible fire is `set_chan_end_caps`, and `end_caps` is **not** a
        // census term — `binding_refs` reads only the (unchanged) bindings, the other five
        // terms read framed views — so refs *and* census are unchanged across the decrement,
        // and `fire` freezes the delta across the peer-closed fire. Unconditional and
        // `requires`-free — `delete`'s Channel branch consumes it in the off-by-one window.
        cspace::census_delta_frozen(old(store), final(store)),
        // `refcount_sound` as a system invariant: the frozen delta bridges it.
        // Conditional + `requires`-free — `delete`'s Channel branch runs this in the off-by-one
        // window where `refcount_sound` is *false*, so it consumes the frozen delta directly,
        // never this clause; a census-sound caller gets a census-sound result.
        cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store)),
        // …and a census off by one at any `z` survives — exactly the shape `delete` carries
        // across the peer-closed fire (its deleted channel cap's slot was just cleared).
        forall|z: ObjId| cspace::census_off_by_one(old(store), z)
            ==> #[trigger] cspace::census_off_by_one(final(store), z),
        // …and refs-domain completeness survives (`set_chan_end_caps` is census/dom-neutral,
        // the fire only drops a census term and keeps the domain). `delete` carries it to `obj_unref`.
        cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(final(store)),
        // The channel skeleton rides through the `end_caps`-only update (`fire` frames
        // `chan_view`): `delete`'s Channel branch composes it to `obj_unref`.
        cspace::chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        // Dead, queue-detached TCBs are frozen: `set_chan_end_caps`
        // frames `tcb`/`refs` whole, and the peer-closed `fire` is signal-shaped (its own
        // `dead_tcb_frozen`). `delete`'s Channel branch reads it off.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // "Dead stays dead": `set_chan_end_caps` frames `refs` whole and the peer-closed
        // `fire` carries `refs_death_persist`, so a dead object stays dead. `delete`'s Channel
        // branch composes it for the provenance frame.
        cspace::refs_death_persist(old(store), final(store)),
        // The TCB domain + every immutable `bind_slots` ride through: `set_chan_end_caps`
        // frames `tcb`, `fire` keeps both — the `home_views_frozen` stability `delete`'s Channel
        // branch threads for the provenance frame.
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        forall|k: ObjId| #[trigger] final(store).tcb_view()[k].bind_slots
            == old(store).tcb_view()[k].bind_slots,
{
    let e = end_idx(end);
    store.set_chan_end_caps(ch, e, store.chan_end_caps(ch, e) - 1);
    let ghost st_mid = *store;
    // `set_chan_end_caps` left the bindings (and notif/TCB views) untouched, so the
    // binding invariant + the fired binding's refs side-condition carry to the fire.
    assert(store.chan_view()[ch].bindings == old(store).chan_view()[ch].bindings);
    // The census is unchanged across the `end_caps` decrement (`end_caps` is no census
    // term): `binding_refs` is framed (bindings unchanged), the other five read framed views;
    // `set_chan_end_caps` also frames `refs`. So both refs and census equal `old` here.
    proof {
        assert(store.chan_view().dom() == old(store).chan_view().dom());
        assert forall|c: ObjId| old(store).chan_view().dom().contains(c) implies
            #[trigger] store.chan_view()[c].bindings == old(store).chan_view()[c].bindings by {
            if c != ch {
                assert(store.chan_view()[c] == old(store).chan_view()[c]);
            }
        }
        assert forall|x: ObjId| #[trigger] cspace::obj_census(store, x)
            == cspace::obj_census(old(store), x) by {
            cspace::lemma_binding_refs_frame(old(store).chan_view(), store.chan_view(), x);
        }
        // end_caps_sound after the decrement: the off-by-one at `(ch, e)` is landed (the
        // decrement), every other `(ch2, e2)` was already sound (off-by-one offset 0), and
        // `set_chan_end_caps` frames `slot_view` so `end_cap_count` is unchanged.
        assert(store.slot_view() == old(store).slot_view());
        assert forall|ch2: ObjId, e2: int|
            store.chan_view().dom().contains(ch2) && store.chan_view()[ch2].end_caps.len() == 2
                && 0 <= e2 < 2 implies #[trigger] store.chan_view()[ch2].end_caps[e2]
                == cspace::end_cap_count(store.slot_view(), ch2, e2) by {
            assert(cspace::end_cap_count(store.slot_view(), ch2, e2)
                == cspace::end_cap_count(old(store).slot_view(), ch2, e2));
        }
        // caps_consistent after the decrement: the only changed term is `end_caps[ch][e]`,
        // which `end_caps_sound` keeps `> 0` for any live `Channel(ch, e)` sibling; every
        // other cap reads framed views (chan `bindings`/depth, notif/tcb/timer/cspace/slot).
        assert forall|s: SlotId| #![trigger store.slot_view()[s]]
            store.slot_view().dom().contains(s) && !cspace::is_empty_cap(store.slot_view()[s].cap)
            implies cspace::cap_consistent(store, store.slot_view()[s].cap) by {
            let c = store.slot_view()[s].cap;
            assert(c == old(store).slot_view()[s].cap);
            assert(cspace::cap_consistent(old(store), c));
            // A live Channel cap makes its endpoint count >= 1, so `end_caps_sound` (above)
            // keeps `end_caps[..] > 0` — the only `cap_consistent` clause reading `end_caps`.
            if let Some((ch2, e2idx)) = cspace::cap_chan_end(c) {
                cspace::lemma_end_cap_count_positive(store.slot_view(), s, ch2, e2idx);
            }
        }
    }
    proof {
        // B8C: `set_chan_end_caps` frames `ready_view` + `tcb_view`, so the ready pair carries
        // to feed `fire`'s requires (bound branch) and the no-fire ensures (else branch).
        cspace::lemma_ready_inv_frame(old(store), store);
    }
    if store.chan_end_caps(ch, e) == 0 {
        fire(store, ch, 1 - e, EV_PEER_CLOSED);
    } else {
        // No fire ⇒ the store is unchanged since `st_mid`, so it is trivially dead-tcb-frozen.
        proof {
            assert(cspace::dead_tcb_frozen(&st_mid, store));
            // …and trivially death-preserving (`refs` unchanged since `st_mid`).
            cspace::lemma_refs_death_persist_from_refs_eq(&st_mid, store);
        }
    }
    // caps_consistent + end_caps_sound at exit: established after the decrement above; in the
    // fired branch `fire` carries `caps_consistent` (its conditional ensures) and frames
    // chan/slot so `end_caps_sound` rides through; in the unfired branch the store is unchanged.
    proof {
        assert(cspace::end_caps_sound(store));
        assert(cspace::caps_consistent(store));
    }
    // census_delta_frozen(old, final): the `set_chan_end_caps` step left refs *and* census
    // equal to `old` (above), and `fire` froze the delta across the peer-closed fire — so
    // the net delta from `old` is exactly `fire`'s frozen delta. A census off-by-one then
    // survives by `lemma_off_by_one_frozen` applied to that frozen delta.
    proof {
        assert(cspace::census_delta_frozen(old(store), store));
        // refcount_sound (conditional): the frozen delta bridges it.
        if cspace::refcount_sound(old(store)) {
            cspace::lemma_refcount_sound_from_frozen(old(store), store);
        }
        assert forall|z: ObjId| cspace::census_off_by_one(old(store), z) implies
            #[trigger] cspace::census_off_by_one(store, z) by {
            cspace::lemma_off_by_one_frozen(old(store), store, z);
        }
        // census_dom_complete: `set_chan_end_caps` is census/dom-neutral and `fire` carries it.
        assert(cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(store));
        // The skeleton: `chan_view` ends an `end_caps`-only update of `ch` (`..old[ch]` keeps
        // `ring_cap`/`depth`); `fire` framed `chan_view`.
        let v = store.chan_view()[ch];
        assert(store.chan_view() =~= old(store).chan_view().insert(ch, v));
        cspace::lemma_chan_field_update_struct_frame(old(store).chan_view(), ch, v);
        // dead_tcb_frozen: `set_chan_end_caps` froze `tcb` + `refs` (so `old → st_mid` is frozen);
        // the fire (or no-op) carries `st_mid → final`.
        assert(st_mid.tcb_view() == old(store).tcb_view());
        assert(st_mid.refs_view() == old(store).refs_view());
        assert(cspace::dead_tcb_frozen(old(store), &st_mid)) by {
            assert forall|k: ObjId| #[trigger] st_mid.tcb_view()[k] == old(store).tcb_view()[k]
                || old(store).tcb_view()[k].wait_notif == Some(ch) by {}
            cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), &st_mid, ch);
        }
        cspace::lemma_dead_tcb_frozen_trans(old(store), &st_mid, store);
        // TCB domain + `bind_slots` ride through (`set_chan_end_caps` froze `tcb`; `fire`
        // keeps both, or the unfired branch leaves the store at `st_mid`).
        assert forall|k: ObjId| #[trigger] store.tcb_view()[k].bind_slots
            == old(store).tcb_view()[k].bind_slots by {}
        // "Dead stays dead": `set_chan_end_caps` froze `refs` (`old → st_mid` death-persist refl);
        // the fire (or no-op) carries `st_mid → final` death-persist; compose.
        cspace::lemma_refs_death_persist_from_refs_eq(old(store), &st_mid);
        cspace::lemma_refs_death_persist_trans(old(store), &st_mid, store);
    }
}

/// Raise an endpoint's event into its bound notification, if bound (rev1§3.6).
///
/// Reads a binding (a getter) and
/// conditionally calls `signal` (a proven body). `signal`'s
/// preconditions — the bound notification is live + `notif_wf`, and a queued waiter
/// implies `refs > 0` — are discharged from `cspace::binding_notif_wf` (the named
/// binding-liveness invariant) and the per-call refs clause. `slot_view`/`chan_view`
/// stay unchanged (the frame `send`/`recv` need); `binding_notif_wf` is *preserved*
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
        // B8C: the fire carries the ready-queue invariants across the bound `signal`
        // (the teardown/IPC callers supply them; `signal` re-establishes them on the wake).
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete(final(store).ready_view(), final(store).tcb_view()),
        final(store).chan_view() == old(store).chan_view(),
        // Residency is framed across the fire — `signal` frames `cspace_view`, the unbound
        // branch is a no-op; the teardown chain reads it off to `obj_unref`.
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        cspace::binding_notif_wf(final(store).chan_view(), final(store).notif_view(),
            final(store).tcb_view(), ch),
        // The cap→object invariant survives the fire: `signal` keeps every
        // notification well-formed (the fired one by its own `ensures`, the rest by
        // `lemma_notif_wf_frame`) and every TCB's `bind_slots`, so `lemma_caps_consistent_frame`
        // applies. **Conditional** (no new `requires`) so `send`/`recv` keep no obligation;
        // `endpoint_cap_dropped`/`delete` supply the hypothesis.
        cspace::caps_consistent(old(store)) ==> cspace::caps_consistent(final(store)),
        // The refcount census moves in lockstep across the fire: `fire`
        // reads a binding then either does nothing or calls `signal` (whose own
        // `census_delta_frozen` applies, its `old` being this `old` — no mutation precedes
        // it). Unconditional and `requires`-free, so `send`/`recv` (the construction-op
        // callers) keep no census obligation; `endpoint_cap_dropped` consumes it.
        cspace::census_delta_frozen(old(store), final(store)),
        // …and a census off by one at any `z` survives (the frozen delta applied to that
        // shape) — `endpoint_cap_dropped`/`delete` read this off the chain.
        forall|z: ObjId| cspace::census_off_by_one(old(store), z)
            ==> #[trigger] cspace::census_off_by_one(final(store), z),
        // …and refs-domain completeness survives (`signal`'s own conditional, or the unbound
        // no-op) — the teardown chain carries it to `obj_unref`.
        cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(final(store)),
        // Dead, queue-detached TCBs are frozen across the fire: the
        // bound branch rides `signal`'s own `dead_tcb_frozen` (its `old` is this `old`, nothing
        // mutated before it); the unbound branch is a no-op. `endpoint_cap_dropped`/`delete`
        // carry it up the teardown chain.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // "Dead stays dead" across the fire: the bound branch rides `signal`'s own
        // `refs_death_persist`; the unbound branch is a no-op. `endpoint_cap_dropped`/`delete`
        // carry it up the teardown chain.
        cspace::refs_death_persist(old(store), final(store)),
        // The TCB domain + every immutable `bind_slots` ride the fire: `signal` keeps
        // both (the bound branch) and the unbound branch is a no-op — the `home_views_frozen`
        // stability `endpoint_cap_dropped`/`delete` thread for the provenance frame.
        final(store).tcb_view().dom() == old(store).tcb_view().dom(),
        forall|k: ObjId| #[trigger] final(store).tcb_view()[k].bind_slots
            == old(store).tcb_view()[k].bind_slots,
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
                    // B8C: signal's contrapositive frame freezes every in-domain waiter of
                    // `m != n`. If such a `k` (`wait_notif == Some(m)`) had changed, signal's
                    // frame says it was an `n`-waiter (`Some(n) != Some(m)`) or Runnable (⇒
                    // `wait_notif None` by `ready_complete`, contradicting `Some(m)`) — neither
                    // holds, so `k` is fixed. (Phantom out-of-domain keys are dom-guarded out.)
                    assert forall|k: ObjId| #[trigger] old(store).tcb_view()[k].wait_notif == Some(m)
                        && old(store).tcb_view().dom().contains(k)
                        implies tvf[k] == old(store).tcb_view()[k] by {
                        if tvf[k] != old(store).tcb_view()[k] {
                            assert(old(store).tcb_view()[k].state
                                != crate::thread::ThreadState::Runnable);
                        }
                    }
                    cspace::lemma_notif_wf_frame(old(store).notif_view(),
                        old(store).tcb_view(), nvf, tvf, m);
                }
            }
            // caps_consistent preservation across the signal (the bound branch): every
            // notification stays wf and every TCB's bind_slots are fixed, so the frame applies.
            if cspace::caps_consistent(old(store)) {
                assert forall|k: ObjId| #[trigger] tvf[k].bind_slots
                    == old(store).tcb_view()[k].bind_slots by {}
                // The bound cspace of every TCB is framed across the wake — `signal` moves only
                // the woken head's queue/wait/retval fields, never any cspace (the strengthened
                // `cap_consistent(Thread)` clause).
                assert forall|k: ObjId| #[trigger] tvf[k].cspace
                    == old(store).tcb_view()[k].cspace by {}
                // The only changed TCB is the woken head — `signal` sets it `Runnable`, so it is
                // not blocked in the post-state (waiter-coherence frame;
                // a changed-and-still-blocked thread would have to be blocked on `n`).
                assert forall|k: ObjId| #[trigger] tvf[k] != old(store).tcb_view()[k]
                    && tvf[k].state == crate::thread::ThreadState::BlockedNotif
                    implies (tvf[k].wait_notif matches Some(wn) ==> wn == n) by {}
                // Other notifications' waiters (`wait_notif` Some, `!= n`) are frozen — each is
                // `BlockedNotif` (non-Runnable by `ready_complete`), so signal's contrapositive
                // frame (changed ⇒ `n`-waiter or Runnable) leaves it unchanged.
                assert forall|k: ObjId| #[trigger] old(store).tcb_view()[k].wait_notif is Some
                    && old(store).tcb_view()[k].wait_notif != Some(n)
                    && old(store).tcb_view().dom().contains(k)
                    implies tvf[k] == old(store).tcb_view()[k] by {
                    if tvf[k] != old(store).tcb_view()[k] {
                        assert(old(store).tcb_view()[k].state
                            != crate::thread::ThreadState::Runnable);
                    }
                }
                cspace::lemma_caps_consistent_frame(old(store), store, n);
            }
        }
    }
    // The fire freezes the census delta: in the bound branch `signal`'s own
    // `census_delta_frozen` applies (nothing mutated before it, so its `old` is this `old`);
    // in the unbound branch the store is untouched (a trivially frozen delta). A census
    // off-by-one then survives by `lemma_off_by_one_frozen` applied to that frozen delta.
    proof {
        assert(cspace::census_delta_frozen(old(store), store));
        assert forall|z: ObjId| cspace::census_off_by_one(old(store), z) implies
            #[trigger] cspace::census_off_by_one(store, z) by {
            cspace::lemma_off_by_one_frozen(old(store), store, z);
        }
        // census_dom_complete: the bound branch rides `signal`'s own conditional (its `old` is
        // this `old`); the unbound branch leaves the store untouched.
        assert(cspace::census_dom_complete(old(store)) ==> cspace::census_dom_complete(store));
        // TCB domain + `bind_slots` ride the fire (signal's own ensures in the bound branch;
        // store untouched in the unbound one).
        assert forall|k: ObjId| #[trigger] store.tcb_view()[k].bind_slots
            == old(store).tcb_view()[k].bind_slots by {}
        // Death persists — the bound branch rides `signal`'s `refs_death_persist` (its
        // `old` is this `old`, nothing preceded it); the unbound branch frames `refs` whole.
        if b.notif is None {
            cspace::lemma_refs_death_persist_from_refs_eq(old(store), store);
            // B8C: the unbound branch is a no-op, so the ready invariants ride the equal-views
            // frame. (The bound branch rides `signal`'s own `ready_wf`/`ready_complete` ensures.)
            cspace::lemma_ready_inv_frame(old(store), store);
        }
    }
}

/// The `refs_view` after `bind` releases `old_notif`'s ref and then adds
/// `new_notif`'s — the decrement-before-increment order the body performs, so a
/// rebind to the *same* notification (`old_notif == new_notif`) is provably
/// net-zero (the second `insert` reads the already-decremented count). The
/// binding term of `refcount_sound`'s census.
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

/// The per-object delta of `bind_refs_post`, additive form: `refs[x]` drops at the
/// old notif and rises at the new one, matching `lemma_binding_replace`'s binding-census delta
/// term-for-term — the lockstep `channel::bind` reads off to preserve `refcount_sound`. The
/// `old > 0` guard is the same `nat`-underflow gate the body's `- 1` already requires.
proof fn lemma_bind_refs_post_at(
    r0: Map<ObjId, nat>,
    old_notif: Option<ObjId>,
    new_notif: Option<ObjId>,
    x: ObjId,
)
    requires
        old_notif matches Some(no) ==> r0.dom().contains(no) && r0[no] > 0,
        new_notif matches Some(nn) ==> r0.dom().contains(nn),
    ensures
        bind_refs_post(r0, old_notif, new_notif)[x] + (if old_notif == Some(x) {
            1nat
        } else {
            0nat
        }) == r0[x] + (if new_notif == Some(x) { 1nat } else { 0nat }),
{
    let r1 = match old_notif {
        Some(no) => r0.insert(no, (r0[no] - 1) as nat),
        None => r0,
    };
    if let Some(no) = old_notif {
        assert(r0[no] > 0);
        assert(r1[x] == if x == no { (r0[no] - 1) as nat } else { r0[x] });
    } else {
        assert(r1[x] == r0[x]);
    }
    if let Some(nn) = new_notif {
        assert(bind_refs_post(r0, old_notif, new_notif)[x] == if x == nn {
            (r1[nn] + 1) as nat
        } else {
            r1[x]
        });
    } else {
        assert(bind_refs_post(r0, old_notif, new_notif)[x] == r1[x]);
    }
}

/// Configure an endpoint's event binding (holder-configured, rev1§3.6).
/// Replacing a binding releases the old notification's ref and adds the new
/// one's (rev1§3.6 binding-refcount discipline).
///
/// Installs `Binding { notif, bits }` at `(end, event)`,
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
        // `refcount_sound` as a system invariant: the binding-census delta
        // (`lemma_binding_replace`) matches the `bind_refs_post` refs delta
        // (`lemma_bind_refs_post_at`) term-for-term — `refs` and the census move in lockstep at
        // the old and new notifications. Conditional + `requires`-free; the finiteness antecedent
        // is the `binding_refs` `len` well-definedness `caps_consistent` carries (it is no extra
        // burden on a well-formed store).
        (old(store).chan_view().dom().finite() && cspace::refcount_sound(old(store)))
            ==> cspace::refcount_sound(final(store)),
{
    let e = end_idx(end);
    let ghost old_notif =
        old(store).chan_view()[ch].bindings[(end_idx_spec(end), event as int)].notif;
    let old_b = store.chan_binding(ch, e, event);
    if let Some(n) = old_b.notif {
        store.set_obj_refs(n, store.obj_refs(n) - 1);
    }
    if let Some(n) = notif {
        store.set_obj_refs(n, store.obj_refs(n) + 1);
    }
    store.set_chan_binding(ch, e, event, Binding { notif, bits });
    proof {
        if old(store).chan_view().dom().finite() && cspace::refcount_sound(old(store)) {
            let cv0 = old(store).chan_view();
            // The five non-binding census terms frame: the slot/notif/tcb/timer views are
            // untouched (the setters frame them), so only `binding_refs` moves.
            assert(store.slot_view() == old(store).slot_view());
            assert(store.notif_view() == old(store).notif_view());
            assert(store.tcb_view() == old(store).tcb_view());
            assert(store.timer_view() == old(store).timer_view());
            assert(store.chan_view() == cv0.insert(
                ch,
                ChanView {
                    bindings: cv0[ch].bindings.insert(
                        (end_idx_spec(end), event as int),
                        Binding { notif, bits }),
                    ..cv0[ch]
                }));
            assert forall|x: ObjId| store.refs_view().dom().contains(x) implies
                #[trigger] store.refs_view()[x] == cspace::obj_census(store, x) by {
                cspace::lemma_binding_replace(cv0, ch, end_idx_spec(end), event as int,
                    Binding { notif, bits }, x);
                lemma_bind_refs_post_at(old(store).refs_view(), old_notif, notif, x);
                // refcount_sound(old) at x; the binding delta == the refs delta closes refs == census.
                assert(old(store).refs_view()[x] == cspace::obj_census(old(store), x));
            }
        }
    }
}

/// Send: copy the payload into the ring and move caps from the sender's
/// slots into the message's CDT-visible slots (rev1§3.4 move semantics).
///
/// On `Ok` the message is enqueued FIFO at the tail —
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
        // B8C: the readable `fire` faithfully enqueues a woken receiver, so `send` carries the
        // ready-queue invariants in. (The trusted syscall shell supplies them; `send` consumes
        // them only to discharge `fire`'s requires — no kcore caller needs them in `ensures`.)
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
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
    let ghost rrv0 = old(store).ready_view();

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
            // B8C: the ring/cap loop touches no thread state; the setters frame `ready_view`,
            // so it stays pinned to entry — lets `lemma_ready_inv_frame` carry the pair to `fire`.
            store.ready_view() == rrv0,
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
    proof {
        // B8C: the ring enqueue framed `ready_view` + `tcb_view`, so the ready pair carries
        // unchanged to feed `fire`'s requires.
        cspace::lemma_ready_inv_frame(old(store), store);
    }
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
/// (rev1§3.3) — receive-side exhaustion is the receiver's own problem.
/// Revocation may have emptied queued slots in flight; receivers see those
/// as absent caps (rev1§3.4 null slots).
///
/// Two-pass atomicity — pass 1 is read-only, so `Empty`/
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
        // B8C: the writable `fire` faithfully enqueues a woken sender, so `recv` carries the
        // ready-queue invariants in (trusted shell supplies them; consumed only for `fire`).
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
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
                   (1 - end_idx_spec(end), old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int)]
            // The receive-half of move semantics, mirroring `send`'s source export.
            // (B) Every non-empty arriving cap landed in the dest the caller named — so a
            // verified caller can conclude where the cap went, not merely that it left the
            // queue. (`dests@[c] is Some` here is forced by the pass-1 free-slot check.)
            && (forall|c: int| #![trigger dests@[c]]
                   0 <= c < 4 && !cspace::is_empty_cap(
                       old(store).slot_view()[old(store).chan_view()[ch].ring_cap[(
                           1 - end_idx_spec(end),
                           old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int,
                           c)]].cap)
                   ==> (dests@[c] is Some
                        && final(store).slot_view()[dests@[c]->Some_0].cap
                           == old(store).slot_view()[old(store).chan_view()[ch].ring_cap[(
                               1 - end_idx_spec(end),
                               old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int,
                               c)]].cap))
            // (C) The dequeued head's ring slots are all empty afterward — the queue-slot
            // owner relinquished the cap (moved-out caps cleared, null caps already empty).
            && (forall|c: int| #![trigger old(store).chan_view()[ch].ring_cap[(
                       1 - end_idx_spec(end),
                       old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int, c)]]
                   0 <= c < 4 ==> cspace::is_empty_cap(
                       final(store).slot_view()[old(store).chan_view()[ch].ring_cap[(
                           1 - end_idx_spec(end),
                           old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int,
                           c)]].cap))
            // (A) The returned install mask names exactly the filled dests: bit c set iff
            // arriving cap c was non-empty (hence moved into dests@[c] by (B)). A caller
            // can decode which slots it now owns directly from the mask.
            && (forall|c: int| #![trigger mask_bit(res->Ok_0.1, c)]
                   0 <= c < 4 ==> (mask_bit(res->Ok_0.1, c)
                       <==> !cspace::is_empty_cap(
                           old(store).slot_view()[old(store).chan_view()[ch].ring_cap[(
                               1 - end_idx_spec(end),
                               old(store).chan_view()[ch].head[1 - end_idx_spec(end)] as int,
                               c)]].cap)))),
{
    let ghost sv0 = old(store).slot_view();
    let ghost cv0 = old(store).chan_view();
    let ghost r0 = old(store).refs_view();
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost rrv0 = old(store).ready_view();

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
            // B8C: the ring/cap loop touches no thread state; the setters frame `ready_view`,
            // so it stays pinned to entry — lets `lemma_ready_inv_frame` carry the pair to `fire`.
            store.ready_view() == rrv0,
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
    proof {
        // (2b) base case: the empty mask has every bit clear (RHS uniformly false at c2==0).
        assert forall|cc: int| 0 <= cc < 4 implies !mask_bit(mask, cc) by {
            lemma_mask_zero(cc as u64);
        }
    }
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
            // B8C: the ring/cap loop touches no thread state; the setters frame `ready_view`,
            // so it stays pinned to entry — lets `lemma_ready_inv_frame` carry the pair to `fire`.
            store.ready_view() == rrv0,
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
            // (2a) processed dests (cc < c2) with a non-empty arriving cap now HOLD
            // it — the receive-half installation we will export from the `ensures`:
            forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                (0 <= cc < c2
                    && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))
                ==> (dests@[cc] is Some
                    && store.slot_view()[dests@[cc]->Some_0].cap
                        == sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap),
            // (2b) the install mask names exactly the processed-and-filled indices:
            // bit cc set iff (cc already processed AND arriving cap cc was non-empty):
            forall|cc: int| #![trigger mask_bit(mask, cc)]
                0 <= cc < 4 ==> (mask_bit(mask, cc)
                    <==> (cc < c2
                        && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))),
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
                // (2a) processed dests through c2 hold their arriving cap. The cc==c2
                // case is this very `slot_move` (dst d holds src's cap); cc < c2 survive
                // because d (dests distinct) and src (a dest is not a ring cap) are not them.
                assert forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                    (0 <= cc < c2 + 1
                        && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))
                    implies (dests@[cc] is Some
                        && sv2[dests@[cc]->Some_0].cap
                            == sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap) by {
                    if cc < c2 {
                        assert(dests@[cc]->Some_0 != d);
                        if dests@[cc]->Some_0 == src {
                            assert(cspace::is_ring_cap_of(cv0[ch], dests@[cc]->Some_0));
                        }
                    } else {
                        assert(cv0[ch].ring_cap[(rr, hh, cc)] == src);
                        assert(dests@[cc]->Some_0 == d);
                    }
                }
            }
            let ghost m_pre = mask;
            let c2u: u64 = c2 as u64;
            mask = mask | (1u8 << c2u);
            proof {
                // (2b) re-establish the mask invariant for c2+1: the lemma sets bit
                // c2 and frames the rest; the arriving cap at c2 is non-empty in this branch.
                lemma_mask_set_bit(m_pre, c2u);
                assert forall|cc: int| 0 <= cc < 4 implies (mask_bit(mask, cc)
                    <==> (cc < c2 + 1
                        && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))) by {
                    if cc == c2 as int {
                        assert(!cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap));
                    }
                }
            }
        } else {
            // null cap (revoked in flight): skip; head cap cc=c2 already empty.
            assert(cspace::is_empty_cap(store.slot_view()[cv0[ch].ring_cap[(rr, hh, c2 as int)]].cap));
            proof {
                // arriving cap c2 is empty (this branch), so the cc==c2 obligation is vacuous;
                // nothing moved this iteration, so cc < c2 ride through unchanged.
                assert(cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, c2 as int)]].cap));
                assert forall|cc: int| #![trigger cv0[ch].ring_cap[(rr, hh, cc)]]
                    (0 <= cc < c2 + 1
                        && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))
                    implies (dests@[cc] is Some
                        && store.slot_view()[dests@[cc]->Some_0].cap
                            == sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap) by {
                    assert(cc < c2);
                }
                // (2b) mask unchanged; arriving c2 empty ⟹ bit c2 stays 0.
                assert forall|cc: int| 0 <= cc < 4 implies (mask_bit(mask, cc)
                    <==> (cc < c2 + 1
                        && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, cc)]].cap))) by {}
            }
        }
        c2 += 1;
    }

    let ghost sv_loop = store.slot_view();
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
    proof {
        // B8C: the ring dequeue framed `ready_view` + `tcb_view`, so the ready pair carries
        // unchanged to feed `fire`'s requires.
        cspace::lemma_ready_inv_frame(old(store), store);
    }
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

        // ── export the receive-half of move semantics ──
        // The dequeue ops + fire frame slot_view, so the pass-2 installation rides to
        // `final`; the c2==4 loop invariants then yield ensures (B) and (C) directly.
        assert(svf == sv_loop);
        // (C) every dequeued-head ring slot is empty (loop invariant "processed emptied").
        assert forall|c: int| #![trigger cv0[ch].ring_cap[(rr, hh, c)]]
            0 <= c < 4 implies cspace::is_empty_cap(svf[cv0[ch].ring_cap[(rr, hh, c)]].cap) by {}
        // (B) each non-empty arriving cap landed in the named dest ((2a) at c2==4).
        assert forall|c: int| #![trigger dests@[c]]
            (0 <= c < 4 && !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, c)]].cap))
            implies (dests@[c] is Some
                && svf[dests@[c]->Some_0].cap == sv0[cv0[ch].ring_cap[(rr, hh, c)]].cap) by {}
        // (A) mask exactness ((2b) at c2==4: cc < 4 always holds, so RHS == non-emptiness).
        assert forall|c: int| #![trigger mask_bit(mask, c)]
            0 <= c < 4 implies (mask_bit(mask, c)
                <==> !cspace::is_empty_cap(sv0[cv0[ch].ring_cap[(rr, hh, c)]].cap)) by {}
    }
    Ok((len as usize, mask))
}

} // verus!

verus! {

/// Release one event binding's notification reference: drop `refs[n]` and **clear the
/// binding** (`notif: None`) so `binding_refs(n)` falls in lockstep — the census's
/// answer to the "no clean closed form" `destroy_channel` would otherwise face.
/// Quarantined from `destroy_channel`'s loop so its census
/// recount is one context-light SMT query; non-recursive (no `delete`), so not an SCC
/// member.
fn release_binding<S: Store>(store: &mut S, ch: ObjId, end: usize, ev: usize)
    requires
        cspace::refcount_sound(old(store)),
        cspace::caps_consistent(old(store)),
        cspace::end_caps_sound(old(store)),
        cspace::census_dom_complete(old(store)),
        old(store).chan_view().dom().contains(ch),
        end < 2,
        ev < 3,
    ensures
        cspace::refcount_sound(final(store)),
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
        cspace::census_dom_complete(final(store)),
        final(store).slot_view() == old(store).slot_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        // B8C: `release_binding` touches only chan bindings + `refs[n]`; the setters frame
        // `tcb_view` + `ready_view`, so `destroy_channel`'s binding-release loop carries the
        // ready pair across it (via `lemma_ready_inv_frame`).
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).ready_view() == old(store).ready_view(),
        final(store).chan_view().dom() == old(store).chan_view().dom(),
        cspace::chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        // Dead, queue-detached TCBs are frozen: `release_binding`
        // frames `tcb` whole and drops `refs` only at the binding's notification (which had a
        // binding ref, so `refs > 0`). `destroy_channel`'s binding-release loop reads it off.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // The home maps are framed: `release_binding` frames `tcb` whole and the
        // bindings-only chan edit keeps the skeleton — `destroy_channel` threads it.
        cspace::home_views_frozen(old(store), final(store)),
        // "Dead stays dead": the bound case drops only `refs[n]` (positive — a binding
        // held a ref), keeping the domain; the unbound case frames `refs` whole. `destroy_channel`
        // composes it across the binding-release loop.
        cspace::refs_death_persist(old(store), final(store)),
{
    let b = store.chan_binding(ch, end, ev);
    if let Some(n) = b.notif {
        let ghost s_b = *store;
        proof {
            // The binding names `n`, so `binding_refs(n) >= 1`; refs-domain completeness +
            // `refcount_sound` make `refs[n] == census(n) >= 1` — the underflow gate.
            assert(s_b.chan_view()[ch].bindings[(end as int, ev as int)].notif == Some(n));
            cspace::lemma_binding_refs_pos(s_b.chan_view(), ch, end as int, ev as int, n);
            assert(cspace::obj_census(&s_b, n) >= 1);
            cspace::lemma_in_refs_from_census(&s_b, n);
        }
        let r = store.obj_refs(n);
        store.set_obj_refs(n, r - 1);
        store.set_chan_binding(ch, end, ev, Binding { notif: None, bits: b.bits });
        proof {
            let cvf = store.chan_view();
            let cv_b = s_b.chan_view();
            let nb = Binding { notif: None, bits: b.bits };
            // Framing: the two setters touch only `refs` (at `n`) and `ch`'s bindings.
            assert(store.slot_view() == s_b.slot_view());
            assert(store.notif_view() == s_b.notif_view());
            assert(store.tcb_view() == s_b.tcb_view());
            assert(store.timer_view() == s_b.timer_view());
            assert(store.cspace_view() == s_b.cspace_view());
            assert(store.refs_view() =~= s_b.refs_view().insert(n, (r - 1) as nat));
            assert(cvf =~= cv_b.insert(
                ch, ChanView { bindings: cv_b[ch].bindings.insert((end as int, ev as int), nb), ..cv_b[ch] }));
            // The cleared binding lowers `binding_refs(n)` by one and frames every other object.
            cspace::lemma_binding_drop(cv_b, ch, end as int, ev as int, nb, n);
            // Census drops by one at `n` only (additive — no `nat` underflow); the other five
            // terms read the framed views.
            assert forall|x: ObjId| #[trigger] cspace::obj_census(&s_b, x)
                == cspace::obj_census(store, x) + (if x == n { 1nat } else { 0nat }) by {}
            // refcount_sound: `n`'s term moved with the `-1`; every other object's refs and
            // census are both untouched.
            assert forall|x: ObjId| store.refs_view().dom().contains(x)
                implies #[trigger] store.refs_view()[x] == cspace::obj_census(store, x) by {
                assert(s_b.refs_view()[x] == cspace::obj_census(&s_b, x));
            }
            // census_dom_complete: every census only dropped; domain unchanged.
            assert forall|x: ObjId| #[trigger] cspace::obj_census(store, x) >= 1
                implies store.refs_view().dom().contains(x) by {
                assert(cspace::obj_census(&s_b, x) >= 1);
            }
            // caps_consistent: slot view unchanged; the only chan edit cleared `ch`'s binding
            // to `None`, which keeps `binding_notif_wf(ch)` (vacuous antecedent) and leaves
            // `chan_wf`/`end_caps` (the other `cap_consistent` clauses) framed.
            assert forall|s: SlotId| #![trigger store.slot_view()[s]]
                store.slot_view().dom().contains(s) && !cspace::is_empty_cap(store.slot_view()[s].cap)
                implies cspace::cap_consistent(store, store.slot_view()[s].cap) by {
                let c = store.slot_view()[s].cap;
                assert(c == s_b.slot_view()[s].cap);
                assert(cspace::cap_consistent(&s_b, c));
                if let cspace::CapKind::Channel(ch2, _) = c.kind {
                    if ch2 == ch {
                        // `chan_view[ch]` changed only in one binding's value: `chan_wf` (reads
                        // dom/ring_cap/depth/msg_len) and `end_caps` carry, `binding_notif_wf`
                        // survives the clear-to-`None` (vacuous antecedent).
                        assert(cvf[ch].ring_cap == cv_b[ch].ring_cap);
                        assert(cvf[ch].depth == cv_b[ch].depth);
                        assert(cvf[ch].msg_len == cv_b[ch].msg_len);
                        assert(cvf[ch].end_caps == cv_b[ch].end_caps);
                        assert(cvf[ch].head == cv_b[ch].head);
                        assert(cvf[ch].count == cv_b[ch].count);
                        assert(cvf[ch].bindings.dom() =~= cv_b[ch].bindings.dom());
                        // `chan_wf` lift via the dedicated frame lemma — proving it inline blew
                        // the trigger context after the `cap_consistent` strengthening widened it;
                        // the lemma isolates a clean context.
                        // `release_binding` touches no slot, so `sv0 == sv1`.
                        cspace::lemma_chan_wf_frame(cv_b, store.chan_view(), store.slot_view(),
                            store.slot_view(), ch);
                        assert(cspace::binding_notif_wf(
                            store.chan_view(), store.notif_view(), store.tcb_view(), ch)) by {
                            assert forall|e2: int, v2: int| #![trigger cvf[ch].bindings[(e2, v2)]]
                                (0 <= e2 < 2 && 0 <= v2 < 3 && cvf[ch].bindings[(e2, v2)].notif is Some)
                                implies {
                                    &&& store.notif_view().dom().contains(cvf[ch].bindings[(e2, v2)].notif->Some_0)
                                    &&& cspace::notif_wf(store.notif_view(), store.tcb_view(),
                                            cvf[ch].bindings[(e2, v2)].notif->Some_0)
                                } by {
                                if (e2, v2) != (end as int, ev as int) {
                                    assert(cvf[ch].bindings[(e2, v2)] == cv_b[ch].bindings[(e2, v2)]);
                                }
                            }
                        }
                    }
                }
            }
            // end_caps_sound: `end_caps` and the slot arena are both unchanged.
            assert forall|ch2: ObjId, e2: int|
                store.chan_view().dom().contains(ch2) && store.chan_view()[ch2].end_caps.len() == 2
                    && 0 <= e2 < 2 implies #[trigger] store.chan_view()[ch2].end_caps[e2]
                    == cspace::end_cap_count(store.slot_view(), ch2, e2) by {
                assert(store.chan_view()[ch2].end_caps == s_b.chan_view()[ch2].end_caps);
            }
            // The skeleton rides through the bindings-only update.
            cspace::lemma_chan_field_update_struct_frame(cv_b, ch, cvf[ch]);
            // dead_tcb_frozen: `tcb` framed whole (the two setters touch only `refs[n]` + `ch`'s
            // binding), and `refs` dropped only at `n` (which held a binding ref, so `refs > 0`).
            // `s_b` is captured before any mutation, so it equals the function entry state.
            assert(s_b.refs_view() == old(store).refs_view());
            assert(s_b.tcb_view() == old(store).tcb_view());
            assert(old(store).refs_view()[n] > 0);
            assert(store.tcb_view() == old(store).tcb_view());
            assert forall|x: ObjId|
                old(store).refs_view().dom().contains(x) && old(store).refs_view()[x] == 0
                implies #[trigger] store.refs_view()[x] == 0 by { assert(x != n); }
            assert forall|k: ObjId| #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k]
                || old(store).tcb_view()[k].wait_notif == Some(n) by {}
            assert(store.refs_view().dom() =~= old(store).refs_view().dom());
            assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
            cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, n);
            // "Dead stays dead": the bound case drops only `refs[n]` (positive), keeping the domain.
            assert(store.refs_view()
                == old(store).refs_view().insert(n, (old(store).refs_view()[n] - 1) as nat));
            cspace::lemma_refs_death_persist_dec_ref(old(store), store, n);
        }
    } else {
        // No bound notification ⇒ the store is unchanged, so it is trivially dead-tcb-frozen.
        proof {
            assert forall|k: ObjId| #[trigger] store.tcb_view()[k] == old(store).tcb_view()[k]
                || old(store).tcb_view()[k].wait_notif == Some(ch) by {}
            assert(store.refs_view().dom() =~= old(store).refs_view().dom());
            assert(store.tcb_view().dom() =~= old(store).tcb_view().dom());
            cspace::lemma_dead_tcb_frozen_signal_shaped(old(store), store, ch);
            // "Dead stays dead": the store is unchanged, so death is trivially preserved.
            cspace::lemma_refs_death_persist_from_refs_eq(old(store), store);
        }
    }
    // The home maps are framed — `tcb` is framed whole in both branches, `cspace_view` too,
    // and the chan edit (if any) is the skeleton-preserving binding clear.
    proof {
        assert(store.tcb_view() == old(store).tcb_view());
        assert forall|k: ObjId| #[trigger] store.tcb_view()[k].bind_slots
            == old(store).tcb_view()[k].bind_slots by {}
    }
}

/// Tear a channel down once its last endpoint cap is gone (`refs == 0`): delete
/// every queued cap with ordinary CDT cleanup — cashing a shredded envelope
/// (rev1§3.4) — and release every event binding's notification ref.
///
/// The teardown verifies against the full contract, closing the channel arm of the
/// cross-object SCC `obj_unref → destroy_channel → delete → obj_unref` under the
/// shared lexicographic `decreases (count_nonempty(slot_view), height)` with
/// `destroy_channel` at height 3. The ring-cap delete loop reads `old.ring_cap[ch]`
/// across the recursive `delete`s via `chan_struct_frame` (the channel skeleton is
/// immutable), and the per-binding release matches each `refs -= 1` with a
/// `binding_refs` drop — by **clearing the binding** (`set_chan_binding(.., None)`,
/// `lemma_binding_drop`), the "no clean closed form" a queued-binding census faces.
pub fn destroy_channel<S: Store>(store: &mut S, ch: ObjId)
    requires
        cspace::cspace_wf(old(store).slot_view()),
        cspace::chan_wf(old(store).chan_view(), old(store).slot_view(), ch),
        cspace::refcount_sound(old(store)),
        // Cap→object consistency: the body deletes ring caps of
        // arbitrary kind, so it needs each one's object well-formed.
        cspace::caps_consistent(old(store)),
        // The rev1§3.3 endpoint-cap census: ring caps may be
        // channel caps, so the body's `delete`s thread it.
        cspace::end_caps_sound(old(store)),
        // Refs-domain completeness: the body's `delete`s thread it.
        cspace::census_dom_complete(old(store)),
        // "Dead stays dead": `ch` is dead — its last endpoint cap is gone, so it is out of `refs.dom` or
        // sits there at `refs == 0` (`obj_unref` calls this at `refs[ch] == 0`). `ch` homes its ring
        // caps, so it is the death witness for each ring slot the teardown empties.
        cspace::dead_obj(old(store), ch),
        // B8C: ring-cap `delete`s can fire / tear down threads, touching the ready queue.
        cspace::ready_wf(old(store).ready_view(), old(store).tcb_view()),
        cspace::ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        cspace::cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        cspace::count_nonempty(final(store).slot_view())
            <= cspace::count_nonempty(old(store).slot_view()),
        cspace::refcount_sound(final(store)),
        cspace::caps_consistent(final(store)),
        cspace::end_caps_sound(final(store)),
        cspace::census_dom_complete(final(store)),
        cspace::ready_wf(final(store).ready_view(), final(store).tcb_view()),
        cspace::ready_complete(final(store).ready_view(), final(store).tcb_view()),
        cspace::only_empties(old(store).slot_view(), final(store).slot_view()),
        // Residency is immutable: the ring-cap `delete`s and `set_obj_refs` all frame
        // `cspace_view`, so `obj_unref`'s Channel arm carries it.
        final(store).cspace_view() == old(store).cspace_view(),
        // (No `irq_view` frame: a ring cap may be an `Irq` cap, whose `delete` runs
        // `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
        // The channel skeleton (`ring_cap`/`depth`/dom) is immutable: the body deletes ring
        // caps (slots, not the layout) and clears bindings, never re-homing a channel.
        // `obj_unref`'s Channel arm reads it off.
        cspace::chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        forall|r: int, i: int, c: int|
            (0 <= r < 2 && 0 <= i < old(store).chan_view()[ch].depth && 0 <= c < 4)
                ==> cspace::is_empty_cap(
                    final(store).slot_view()[
                        #[trigger] old(store).chan_view()[ch].ring_cap[(r, i, c)]].cap),
        // Dead, queue-detached TCBs are frozen across the teardown:
        // each ring-cap `delete` and binding `release_binding` carries `dead_tcb_frozen`, threaded
        // through the loops by `lemma_dead_tcb_frozen_trans`. `obj_unref`'s Channel arm reads it.
        cspace::dead_tcb_frozen(old(store), final(store)),
        // The home maps are framed: residency immutable, channel skeleton fixed, TCB
        // domain + every `bind_slots` preserved across the ring-cap deletes + binding releases.
        cspace::home_views_frozen(old(store), final(store)),
        // Provenance: this destructor empties only its ring caps (each homed in `ch`) and
        // their recursive closure, so every un-homed slot keeps its cap. `obj_unref` reads it off.
        cspace::unhomed_frozen_free(old(store), final(store)),
        // Dual provenance: every emptied slot was a home handle of a dead object. A ring cap
        // emptied by a `delete` is homed by `ch` (dead throughout: it entered dead and stays so);
        // the recursive closure each `delete` clears carries its own witness. `obj_unref` reads it.
        cspace::emptied_via_dead_home_free(old(store), final(store)),
        // "Dead stays dead" across the ring-cap deletes + binding releases (each decrements only).
        cspace::refs_death_persist(old(store), final(store)),
    // SCC measure: height 3 — above `delete` (0), below `obj_unref` (4); its
    // ring-cap `delete`s are count-flat on the first iteration, so the height drops.
    decreases cspace::count_nonempty(old(store).slot_view()), 3int
{
    let ghost rc = old(store).chan_view()[ch].ring_cap;
    let ghost depth0 = old(store).chan_view()[ch].depth;
    let depth = store.chan_depth(ch);
    // The ring-cap slots are live (chan_wf, about the immutable old state).
    assert forall|r: int, i: int, c: int|
        (0 <= r < 2 && 0 <= i < depth0 && 0 <= c < 4)
            implies #[trigger] old(store).slot_view().dom().contains(rc[(r, i, c)]) by {}
    for ring in 0..2usize
        invariant
            depth as nat == depth0,
            cspace::cspace_wf(store.slot_view()),
            store.slot_view().dom() == old(store).slot_view().dom(),
            store.slot_view().dom().finite(),
            cspace::count_nonempty(store.slot_view())
                <= cspace::count_nonempty(old(store).slot_view()),
            cspace::refcount_sound(store),
            cspace::caps_consistent(store),
            cspace::end_caps_sound(store),
            cspace::census_dom_complete(store),
            cspace::only_empties(old(store).slot_view(), store.slot_view()),
            store.cspace_view() == old(store).cspace_view(),
            // (No `irq_view` invariant: a ring cap may be an `Irq` cap, whose `delete` runs
            // `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
            cspace::chan_struct_frame(old(store).chan_view(), store.chan_view()),
            cspace::dead_tcb_frozen(old(store), store),
            cspace::home_views_frozen(old(store), store),
            cspace::unhomed_frozen_free(old(store), store),
            // Dual provenance composes across the ring-cap deletes.
            cspace::emptied_via_dead_home_free(old(store), store),
            cspace::refs_death_persist(old(store), store),
            // B8C: the ready pair carries across the ring loop — each `delete` ensures it.
            cspace::ready_wf(store.ready_view(), store.tcb_view()),
            cspace::ready_complete(store.ready_view(), store.tcb_view()),
            // `ch` is dead throughout (it entered dead, death is monotone-preserved) — the witness
            // for each ring cap being emptied.
            cspace::dead_obj(store, ch),
            store.chan_view().dom().contains(ch),
            store.chan_view()[ch].ring_cap == rc,
            forall|r: int, i: int, c: int|
                (0 <= r < 2 && 0 <= i < depth0 && 0 <= c < 4)
                    ==> #[trigger] old(store).slot_view().dom().contains(rc[(r, i, c)]),
            // Completed rings are fully empty.
            forall|r: int, i: int, c: int|
                (0 <= r < ring && 0 <= i < depth0 && 0 <= c < 4)
                    ==> cspace::is_empty_cap(#[trigger] store.slot_view()[rc[(r, i, c)]].cap),
    {
        for i in 0..depth
            invariant
                depth as nat == depth0,
                0 <= ring < 2,
                cspace::cspace_wf(store.slot_view()),
                store.slot_view().dom() == old(store).slot_view().dom(),
                store.slot_view().dom().finite(),
                cspace::count_nonempty(store.slot_view())
                    <= cspace::count_nonempty(old(store).slot_view()),
                cspace::refcount_sound(store),
                cspace::caps_consistent(store),
                cspace::end_caps_sound(store),
                cspace::census_dom_complete(store),
                cspace::only_empties(old(store).slot_view(), store.slot_view()),
                store.cspace_view() == old(store).cspace_view(),
                // (No `irq_view` invariant: a ring cap may be an `Irq` cap, whose `delete`
                // runs `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
                cspace::chan_struct_frame(old(store).chan_view(), store.chan_view()),
                cspace::dead_tcb_frozen(old(store), store),
                cspace::home_views_frozen(old(store), store),
                cspace::unhomed_frozen_free(old(store), store),
                // Dual provenance composes across the ring-cap deletes.
                cspace::emptied_via_dead_home_free(old(store), store),
                cspace::refs_death_persist(old(store), store),
                cspace::ready_wf(store.ready_view(), store.tcb_view()),
                cspace::ready_complete(store.ready_view(), store.tcb_view()),
                cspace::dead_obj(store, ch),
                store.chan_view().dom().contains(ch),
                store.chan_view()[ch].ring_cap == rc,
                forall|r: int, ii: int, c: int|
                    (0 <= r < 2 && 0 <= ii < depth0 && 0 <= c < 4)
                        ==> #[trigger] old(store).slot_view().dom().contains(rc[(r, ii, c)]),
                forall|r: int, ii: int, c: int|
                    (0 <= r < ring && 0 <= ii < depth0 && 0 <= c < 4)
                        ==> cspace::is_empty_cap(#[trigger] store.slot_view()[rc[(r, ii, c)]].cap),
                // Completed rows in the current ring are empty.
                forall|ii: int, c: int|
                    (0 <= ii < i && 0 <= c < 4)
                        ==> cspace::is_empty_cap(
                            #[trigger] store.slot_view()[rc[(ring as int, ii, c)]].cap),
        {
            for c in 0..MSG_CAPS
                invariant
                    depth as nat == depth0,
                    0 <= ring < 2,
                    0 <= i < depth,
                    cspace::cspace_wf(store.slot_view()),
                    store.slot_view().dom() == old(store).slot_view().dom(),
                    store.slot_view().dom().finite(),
                    cspace::count_nonempty(store.slot_view())
                        <= cspace::count_nonempty(old(store).slot_view()),
                    cspace::refcount_sound(store),
                    cspace::caps_consistent(store),
                    cspace::end_caps_sound(store),
                    cspace::census_dom_complete(store),
                    cspace::only_empties(old(store).slot_view(), store.slot_view()),
                    store.cspace_view() == old(store).cspace_view(),
                    // (No `irq_view` invariant: a ring cap may be an `Irq` cap, whose `delete`
                    // runs `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
                    cspace::chan_struct_frame(old(store).chan_view(), store.chan_view()),
                    cspace::dead_tcb_frozen(old(store), store),
                    cspace::home_views_frozen(old(store), store),
                    cspace::unhomed_frozen_free(old(store), store),
                    // Dual provenance composes across the ring-cap deletes.
                    cspace::emptied_via_dead_home_free(old(store), store),
                    cspace::refs_death_persist(old(store), store),
                    cspace::ready_wf(store.ready_view(), store.tcb_view()),
                    cspace::ready_complete(store.ready_view(), store.tcb_view()),
                    cspace::dead_obj(store, ch),
                    store.chan_view().dom().contains(ch),
                    store.chan_view()[ch].ring_cap == rc,
                    forall|r: int, ii: int, cc: int|
                        (0 <= r < 2 && 0 <= ii < depth0 && 0 <= cc < 4)
                            ==> #[trigger] old(store).slot_view().dom().contains(rc[(r, ii, cc)]),
                    forall|r: int, ii: int, cc: int|
                        (0 <= r < ring && 0 <= ii < depth0 && 0 <= cc < 4)
                            ==> cspace::is_empty_cap(#[trigger] store.slot_view()[rc[(r, ii, cc)]].cap),
                    forall|ii: int, cc: int|
                        (0 <= ii < i && 0 <= cc < 4)
                            ==> cspace::is_empty_cap(
                                #[trigger] store.slot_view()[rc[(ring as int, ii, cc)]].cap),
                    // Completed positions in the current row are empty.
                    forall|cc: int|
                        (0 <= cc < c)
                            ==> cspace::is_empty_cap(
                                #[trigger] store.slot_view()[rc[(ring as int, i as int, cc)]].cap),
            {
                let cs = store.chan_ring_cap(ch, ring, i, c);
                assert(cs == rc[(ring as int, i as int, c as int)]);
                assert(old(store).slot_view().dom().contains(cs));
                let ghost sv_before = store.slot_view();
                let ghost cv_before = store.chan_view();
                let ghost st_before = *store;
                if !cspace::cap_is_empty(store.slot(cs).cap) {
                    cspace::delete(store, cs);
                    proof {
                        cspace::lemma_only_empties_trans(
                            old(store).slot_view(), sv_before, store.slot_view());
                        cspace::lemma_chan_struct_frame_trans(
                            old(store).chan_view(), cv_before, store.chan_view());
                        cspace::lemma_dead_tcb_frozen_trans(old(store), &st_before, store);
                        // `cs` is a ring cap of `ch` (homed), so `delete`'s target-aware frame
                        // is already target-free — composing the provenance frame across the loop.
                        assert(cspace::homed_in_chan(&st_before, cs)) by {
                            // `cs` is `ch`'s ring cap at `(ring, i, c)` — the getter pinned the
                            // value, and the loop pins `ch`'s ring_cap map to `rc`.
                            assert(st_before.chan_view().dom().contains(ch));
                            assert(st_before.chan_view()[ch].ring_cap == rc);
                            assert(st_before.chan_view()[ch].ring_cap[(ring as int, i as int,
                                c as int)] == cs);
                        }
                        cspace::lemma_unhomed_frozen_free_from_homed(&st_before, store, cs);
                        cspace::lemma_unhomed_frozen_free_trans(old(store), &st_before, store);
                        cspace::lemma_home_views_frozen_trans(old(store), &st_before, store);
                        // Dual provenance: `ch` homes `cs` (ring cap at `(ring, i, c)`) at `st_before`, and
                        // `ch` is dead there (loop invariant) and stays dead (`delete`'s
                        // `refs_death_persist`), so the directly-deleted `cs` carries the death
                        // witness `ch`. Lift `delete`'s target-aware frame to the free frame, compose.
                        assert(cspace::homes_in_chan(&st_before, ch, cs)) by {
                            assert(st_before.chan_view().dom().contains(ch));
                            assert(st_before.chan_view()[ch].ring_cap[(ring as int, i as int,
                                c as int)] == cs);
                        }
                        assert(cspace::homes(&st_before, ch, cs));
                        assert(cspace::dead_obj(&st_before, ch));  // loop invariant
                        assert(cspace::dead_obj(store, ch));        // `delete`'s `refs_death_persist`
                        cspace::lemma_emptied_via_dead_home_free_from_homed(
                            &st_before, store, cs, ch);
                        cspace::lemma_emptied_via_dead_home_free_trans(old(store), &st_before, store);
                        cspace::lemma_refs_death_persist_trans(old(store), &st_before, store);
                    }
                }
                // Re-establish the empty prefix: prior empties stay empty (`only_empties` from
                // this step), the just-handled position `cs` is now empty (guard or `delete`).
                assert forall|r: int, ii: int, cc: int|
                    (0 <= r < ring && 0 <= ii < depth0 && 0 <= cc < 4)
                        implies cspace::is_empty_cap(#[trigger] store.slot_view()[rc[(r, ii, cc)]].cap)
                    by {}
                assert forall|ii: int, cc: int|
                    (0 <= ii < i && 0 <= cc < 4)
                        implies cspace::is_empty_cap(
                            #[trigger] store.slot_view()[rc[(ring as int, ii, cc)]].cap)
                    by {}
                assert forall|cc: int|
                    (0 <= cc < c + 1)
                        implies cspace::is_empty_cap(
                            #[trigger] store.slot_view()[rc[(ring as int, i as int, cc)]].cap)
                    by {
                        if cc == c as int {
                            assert(rc[(ring as int, i as int, cc)] == cs);
                        }
                    }
            }
        }
    }
    // Release every event binding's notification ref — clearing the binding so the
    // `binding_refs` census drops in lockstep with the `refs -= 1`.
    for end in 0..2usize
        invariant
            depth as nat == depth0,
            cspace::cspace_wf(store.slot_view()),
            store.slot_view().dom() == old(store).slot_view().dom(),
            store.slot_view().dom().finite(),
            cspace::count_nonempty(store.slot_view())
                <= cspace::count_nonempty(old(store).slot_view()),
            cspace::refcount_sound(store),
            cspace::caps_consistent(store),
            cspace::end_caps_sound(store),
            cspace::census_dom_complete(store),
            cspace::only_empties(old(store).slot_view(), store.slot_view()),
            store.cspace_view() == old(store).cspace_view(),
            // (No `irq_view` invariant: a ring cap may be an `Irq` cap, whose `delete` runs
            // `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
            cspace::chan_struct_frame(old(store).chan_view(), store.chan_view()),
            cspace::dead_tcb_frozen(old(store), store),
            cspace::home_views_frozen(old(store), store),
            cspace::unhomed_frozen_free(old(store), store),
            cspace::emptied_via_dead_home_free(old(store), store),
            cspace::refs_death_persist(old(store), store),
            // B8C: the ready pair carries across the binding-release loop — `release_binding`
            // frames `ready_view` + `tcb_view`, threaded by `lemma_ready_inv_frame`.
            cspace::ready_wf(store.ready_view(), store.tcb_view()),
            cspace::ready_complete(store.ready_view(), store.tcb_view()),
            store.chan_view().dom().contains(ch),
            forall|r: int, i: int, c: int|
                (0 <= r < 2 && 0 <= i < depth0 && 0 <= c < 4)
                    ==> cspace::is_empty_cap(#[trigger] store.slot_view()[rc[(r, i, c)]].cap),
    {
        for ev in 0..3usize
            invariant
                depth as nat == depth0,
                0 <= end < 2,
                cspace::cspace_wf(store.slot_view()),
                store.slot_view().dom() == old(store).slot_view().dom(),
                store.slot_view().dom().finite(),
                cspace::count_nonempty(store.slot_view())
                    <= cspace::count_nonempty(old(store).slot_view()),
                cspace::refcount_sound(store),
                cspace::caps_consistent(store),
                cspace::end_caps_sound(store),
                cspace::census_dom_complete(store),
                cspace::only_empties(old(store).slot_view(), store.slot_view()),
                store.cspace_view() == old(store).cspace_view(),
                // (No `irq_view` invariant: a ring cap may be an `Irq` cap, whose `delete`
                // runs `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
                cspace::chan_struct_frame(old(store).chan_view(), store.chan_view()),
                cspace::dead_tcb_frozen(old(store), store),
                cspace::home_views_frozen(old(store), store),
                cspace::unhomed_frozen_free(old(store), store),
                cspace::emptied_via_dead_home_free(old(store), store),
                cspace::refs_death_persist(old(store), store),
                cspace::ready_wf(store.ready_view(), store.tcb_view()),
                cspace::ready_complete(store.ready_view(), store.tcb_view()),
                store.chan_view().dom().contains(ch),
                forall|r: int, i: int, c: int|
                    (0 <= r < 2 && 0 <= i < depth0 && 0 <= c < 4)
                        ==> cspace::is_empty_cap(#[trigger] store.slot_view()[rc[(r, i, c)]].cap),
        {
            let ghost cv_before = store.chan_view();
            let ghost st_before = *store;
            release_binding(store, ch, end, ev);
            proof {
                // B8C: `release_binding` frames `ready_view` + `tcb_view`; carry the pair.
                cspace::lemma_ready_inv_frame(&st_before, store);
                cspace::lemma_chan_struct_frame_trans(
                    old(store).chan_view(), cv_before, store.chan_view());
                cspace::lemma_dead_tcb_frozen_trans(old(store), &st_before, store);
                // `release_binding` frames `slot_view` (only bindings/refs move), so no slot
                // is emptied — the free + home frames compose across the binding-release loop.
                cspace::lemma_unhomed_frozen_free_from_slot_eq(&st_before, store);
                cspace::lemma_unhomed_frozen_free_trans(old(store), &st_before, store);
                cspace::lemma_home_views_frozen_trans(old(store), &st_before, store);
                // Dual provenance: `release_binding` frames `slot_view` (free refl) and exports
                // `refs_death_persist`; compose across the binding-release loop.
                cspace::lemma_emptied_via_dead_home_free_from_slot_eq(&st_before, store);
                cspace::lemma_emptied_via_dead_home_free_trans(old(store), &st_before, store);
                cspace::lemma_refs_death_persist_trans(old(store), &st_before, store);
            }
        }
    }
    // The ring-cap-empty ensures (over `old.ring_cap`) is exactly the carried invariant.
    assert forall|r: int, i: int, c: int|
        (0 <= r < 2 && 0 <= i < old(store).chan_view()[ch].depth && 0 <= c < 4)
            implies cspace::is_empty_cap(
                store.slot_view()[#[trigger] old(store).chan_view()[ch].ring_cap[(r, i, c)]].cap)
        by {}
}

} // verus!
