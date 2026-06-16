//! Capability spaces and the capability derivation tree (spec §2.1–2.3,
//! §3.4).
//!
//! Every kernel object is reached through a `Cap` living in a `CapSlot`.
//! Slots form the CDT: parent/first-child/sibling links threaded through
//! the slots themselves (seL4-style), now expressed as opaque [`SlotId`]
//! handles resolved through the [`Store`] seam. Channel queue slots are
//! ordinary `CapSlot`s owned by the channel, so the revoke walk sees
//! in-flight caps with no special case — the property checked
//! unconditionally by the CapRevocation TLA+ model.
//!
//! Concurrency invariant carried by every function here: the kernel is
//! single-core and non-preemptible (IRQs masked at EL1), so whoever is
//! executing kernel code has exclusive access to all kernel objects. The
//! `Store` impl encapsulates whatever unsafe the production handle
//! resolution needs; the operations below are safe and read as pure
//! functions over an abstract indexed store (plan §3, the arena rewrite).
//!
//! Verification (plan §4.1): these operations are the centerpiece Kani
//! target — the executable re-check of the CapRevocation TLA+ invariants on
//! the real implementation. Each op carries its contract as a pre/post
//! comment; the proof harnesses turn those into `cdt_wf` assertions.

use crate::id::{ObjId, SlotId};
use crate::store::{Binding, Store};
use crate::thread::{Report, ThreadState};
use vstd::prelude::*;

/// Rights bits — monotone under derivation (§2.3): `derive` may only clear
/// bits, never set them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub u8);

// Inside `verus!{}` so the bit consts and `masked` are usable from verified code
// (the §3c `retype_install` rights-inheritance theorem names `READ`/`WRITE`/`PHYS`/
// `ALL`/`THREAD_ALL`; doc/results/28 §1). `masked` carries its bit-level `ensures`
// here rather than via a standalone `assume_specification` — its trivial body is now
// verified, not assumed.
verus! {
impl Rights {
    pub const READ: u8 = 1 << 0; // recv / wait
    pub const WRITE: u8 = 1 << 1; // send / signal
    /// phys-read (§2.5): gates frame_paddr and device mappings. Granted
    /// only on boot-created device/DMA caps — ALL deliberately excludes
    /// it so ordinary derivation chains can never reach a PA.
    pub const PHYS: u8 = 1 << 2;
    /// bind-reports (§2.3): configure a thread's on-exit/on-fault
    /// binding slots (§5.1).
    pub const BIND_REPORTS: u8 = 1 << 3;
    /// read-report (§2.3): read a thread's terminal report record;
    /// later also the debugger's register access (deferred, §8).
    pub const READ_REPORT: u8 = 1 << 4;
    pub const ALL: Rights = Rights(0b11);
    /// The creator's thread cap (§2.3 thread bits; kill is deliberately
    /// not on the list — destruction is resource ancestry, §2.2).
    pub const THREAD_ALL: Rights =
        Rights(Rights::READ | Rights::WRITE | Rights::BIND_REPORTS | Rights::READ_REPORT);

    pub fn has(self, bits: u8) -> bool {
        self.0 & bits == bits
    }

    // The bit-level spec is what makes monotone derivation (and §3c's
    // sub-untyped-never-PHYS theorem) provable.
    pub fn masked(self, mask: u8) -> (out: Rights)
        ensures out.0 == (self.0 & mask),
    {
        Rights(self.0 & mask)
    }
}
} // verus!

/// Common header at the start of every kernel object. `refs` counts every
/// kernel reference that keeps the object alive: cap slots, channel-event
/// bindings, blocked waiters, armed timers. Retained as the production
/// layout struct at every object's head (the `Store` resolves a handle's
/// refcount through it); the verified core touches it only via
/// [`Store::obj_refs`]/[`Store::set_obj_refs`].
#[repr(C)]
pub struct ObjHeader {
    pub refs: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChanEnd {
    A,
    B,
}

/// The object a cap designates. Untyped carries its region inline: untyped
/// caps are never copied (the watermark must have one owner), so the state
/// needs no shared object. Frames carry their mapping inline too — one
/// mapping per cap copy, and deleting the cap unmaps it (§2.5). Object
/// designations are opaque [`ObjId`] handles (was a `*mut Obj`).
#[derive(Clone, Copy)]
pub enum CapKind {
    Empty,
    Untyped {
        base: u64,
        size: u64,
        watermark: u64,
    },
    Frame {
        base: u64,
        pages: u64,
        mapping: Option<(crate::id::ObjId, u64)>,
    },
    Aspace(crate::id::ObjId),
    CSpace(crate::id::ObjId),
    Thread(crate::id::ObjId),
    Channel(crate::id::ObjId, ChanEnd),
    Notification(crate::id::ObjId),
    Timer(crate::id::ObjId),
}

#[derive(Clone, Copy)]
pub struct Cap {
    pub kind: CapKind,
    pub rights: Rights,
}

impl Cap {
    pub const EMPTY: Cap = Cap {
        kind: CapKind::Empty,
        rights: Rights(0),
    };

    pub fn is_empty(&self) -> bool {
        matches!(self.kind, CapKind::Empty)
    }
}

/// A capability slot, CDT links included. Slots live inside cspace objects
/// and inside channel message slots — both are CDT-visible (§3.4). The
/// links are [`SlotId`] handles ([`crate::id`]) that span containers exactly
/// as the old `*mut CapSlot` links did, with no special case in the revoke
/// walk.
#[derive(Clone, Copy)]
pub struct CapSlot {
    pub cap: Cap,
    pub parent: Option<crate::id::SlotId>,
    pub first_child: Option<crate::id::SlotId>,
    pub next_sib: Option<crate::id::SlotId>,
    pub prev_sib: Option<crate::id::SlotId>,
}

impl CapSlot {
    pub const fn empty() -> CapSlot {
        CapSlot {
            cap: Cap::EMPTY,
            parent: None,
            first_child: None,
            next_sib: None,
            prev_sib: None,
        }
    }
}

/// A capability space: header + inline slot array.
///
/// The construction/layout helpers (`bytes_for`, `init`, `slot`) take a
/// caller-supplied `*mut Self` and are retained for the kernel shell that
/// *places* objects in donated untyped memory; they are not part of the
/// Store-based verified core (which addresses cspace residents by handle via
/// [`Store::cspace_slot`]).
#[repr(C)]
pub struct CSpaceObj {
    pub hdr: ObjHeader,
    pub num_slots: u32,
    // CapSlot[num_slots] follows.
}

impl CSpaceObj {
    pub const fn bytes_for(num_slots: u32) -> usize {
        core::mem::size_of::<CSpaceObj>() + num_slots as usize * core::mem::size_of::<CapSlot>()
    }

    /// pre:  self points at a live, initialised cspace object.
    /// post: returns slot i, or null if i is out of range.
    pub unsafe fn slot(this: *mut CSpaceObj, i: u32) -> *mut CapSlot {
        if i >= (*this).num_slots {
            return core::ptr::null_mut();
        }
        let base = this.add(1).cast::<CapSlot>();
        base.add(i as usize)
    }

    /// pre:  memory at `this` is writable, sized via bytes_for(num_slots).
    /// post: every slot is empty with null CDT links; refs = 1 (creator cap).
    pub unsafe fn init(this: *mut CSpaceObj, num_slots: u32) {
        (*this).hdr.refs = 1;
        (*this).num_slots = num_slots;
        for i in 0..num_slots {
            CSpaceObj::slot(this, i).write(CapSlot::empty());
        }
    }
}

// ── Refcount plumbing ───────────────────────────────────────────────────

// `obj_ref` is verified — see the `verus!{}` block at the end of this file.

// `obj_unref`, `unref_cspace`, `destroy_cspace`, and the helper `dec_ref` are verified —
// see the `verus!{}` block at the end of this file (plan §6c, doc/results/43). They moved
// out of this plain-Rust cluster as the teardown members that recurse only through the
// *opaque* `delete`: with `delete`/`destroy_channel`/`destroy_tcb` still `external_body`,
// Verus sees no recursion cycle, so they verify against `delete`'s contract under a plain
// index-countdown loop (no cross-module `decreases` — that is 6d). `unref_aspace` (the
// non-recursive aspace teardown) is likewise in that block (plan §6b, doc/results/42).

// ── CDT structure ───────────────────────────────────────────────────────

// `cdt_insert_child`, `derive`, `slot_move`, and `cdt_unlink` are verified — see
// the `verus!{}` block at the end of this file. `slot_move`'s body proof
// (doc/results/24) shows the move is the identity transposition π=(src dst) and
// lands exactly the renaming. `cdt_unlink`'s body proof (doc/results/25) shows the
// sibling-list *merge* lands exactly `unlinked(m0, slot, last)` (children spliced
// into the parent's list — strictly harder than the transposition): the parent-
// rank acyclicity witness is reused unchanged, the sibling-rank witness is
// rescaled to fit the re-parented child band into the `prev..next` gap, so its
// `external_body` is gone too. Both ops' termination/structure rest on
// sibling-acyclicity (`sib_acyclic`), part of `cspace_wf`.
//
// `delete` and `revoke` are likewise in that block: `delete` still carries an
// assumed teardown-recursion contract (the cross-object destructors are not yet
// in `verus!{}`, plan phases 3–5), and `revoke`'s termination is proven against
// it. `delete`'s contract is host-test-checked against the real body (ArrayStore).

// ── Deductive verification (plan doc/plans/3_verus-rewrite.md §4.1) ───────────
//
// The cspace/CDT operations are verified with Verus against an *abstract* model
// of the `Store` seam: the kernel object store is a finite `Map<SlotId, CapSlot>`
// (the slot arena) plus a `Map<ObjId, nat>` (object refcounts). The generic
// `fn op<S: Store>` operations are proven once for **all** stores; the production
// `KernelStore` (kernel crate, unverified) and any host-test store are trusted to
// satisfy the trait contract — the seam is the TCB boundary (plan §2, §3.2).
//
// `verus!{}` erases to plain Rust in an ordinary build, so the moved operations
// below compile and run exactly as the originals did.
verus! {

broadcast use {vstd::map::group_map_axioms, vstd::set::group_set_axioms};

// The opaque handles and the cap/slot value types are plain Rust (shared with
// the kernel shell); give them Verus type-specs so they can appear in spec
// expressions. `ext_equal` makes `==` mean structural equality in spec code.
// (`allow(dead_code)`: these wrappers are Verus-only scaffolding — after the
// macro erases ghost code in a normal build they are unread tuple structs.)
#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExSlotId(SlotId);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExObjId(ObjId);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExRights(Rights);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExChanEnd(ChanEnd);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExCapKind(CapKind);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExCap(Cap);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExCapSlot(CapSlot);

// An event binding is plain Rust (`crate::store::Binding`); give it a Verus
// type-spec so it can live in the `ChanView.bindings` map and be compared with
// structural `==`. (`allow(dead_code)`: Verus-only scaffolding, erased in a
// normal build — plan doc/plans/3_verus-rewrite_phase3-detail.md §3b.)
#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExBinding(Binding);

// `ThreadState`/`Report` are plain Rust enums (`crate::thread`); give them Verus
// type-specs so they can live in `TcbView` and be compared with structural `==`
// (the phase-4a `tcb_view` analog of `ExChanEnd`, plan §4a). (`allow(dead_code)`:
// Verus-only scaffolding, erased in a normal build.)
#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExThreadState(ThreadState);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExReport(Report);

// ── The channel ghost view (plan §3b) ───────────────────────────────────────
//
// `ChanView` mirrors a `Channel`'s *mutable* state (`channel.rs`) at the
// abstraction the §4.3 proofs reason over — **payload bytes abstracted out**:
// we model message length, cap identity, and order, not the 256 payload bytes.
//
// The load-bearing decision (detail §1.1): a ring message slot is a **real
// `CapSlot` in the single `slot_view` arena** (moved by the already-verified
// `slot_move`). So the cap *contents* live in `slot_view`; `ring_cap` here holds
// only the slot *handles*, which are fixed at channel construction and never
// reassigned (`Store` has a `chan_ring_cap` getter and no setter). `chan_ring_cap`
// is therefore a deterministic projection of this view, and `chan_wf` pins the
// handles to the arena (each in `slot_view`'s domain; window-empty coupling below).
#[verifier::ext_equal]
pub struct ChanView {
    pub depth: nat,
    // Per-end live-endpoint-cap counts (peer-closed, §3.3) and per-ring FIFO
    // cursors. Seqs of length 2 (ring/end ∈ {0,1}).
    pub end_caps: Seq<nat>,
    pub head: Seq<nat>,
    pub count: Seq<nat>,
    // bindings[(end, ev)] — end ∈ {0,1}, ev ∈ {0,1,2} (readable/writable/peer-closed).
    pub bindings: Map<(int, int), Binding>,
    // msg_len[(ring, index)] — the queued payload length (bytes abstracted).
    pub msg_len: Map<(int, int), nat>,
    // ring_cap[(ring, index, cap)] — the CapSlot handle for that ring message's
    // cap slot (cap ∈ {0..4}); the bridge into `slot_view` (the §4.3 coupling).
    pub ring_cap: Map<(int, int, int), SlotId>,
}

// ── The notification / TCB / timer ghost views (plan §4a) ────────────────────
//
// The phase-4 analogs of `ChanView` (§3b, doc 27): each mirrors an object's
// *mutable* state (the `hdr.refs` count is already in `refs_view`). `word`/
// `retval`/`bind_bits`/`bits`/`deadline` are `u64` (not `nat`) so 4b's
// `word | bits` and 4e's `deadline <= now` are expressible directly — the one
// deliberate departure from `ChanView`'s all-`nat` choice, justified by the
// bitwise/comparison semantics those ops need.
#[verifier::ext_equal]
pub struct NotifView {
    pub word: u64,
    pub wait_head: Option<ObjId>,
    pub wait_tail: Option<ObjId>,
}

// The TCB mutable fields the verified ops read/write. `bind_slots` holds the cap
// *slot handles* (length-2 `Seq`) — an immutable projection, since `Store` has a
// `tcb_bind_slot` getter and no setter; the cap *contents* live in `slot_view`,
// exactly as `ChanView.ring_cap` does (§4a, doc 27 §1) — so the TCB binding caps
// stay revoke-visible through the single arena.
#[verifier::ext_equal]
pub struct TcbView {
    pub state: ThreadState,
    pub qnext: Option<ObjId>,
    pub wait_notif: Option<ObjId>,
    pub report: Report,
    pub retval: u64,
    pub cspace: Option<ObjId>,
    pub aspace: Option<ObjId>,
    pub bind_bits: Seq<u64>,     // len 2
    pub bind_slots: Seq<SlotId>, // len 2 — immutable handles into slot_view
}

#[verifier::ext_equal]
pub struct TimerView {
    pub armed: bool,
    pub deadline: u64,
    pub notif: Option<ObjId>,
    pub bits: u64,
    pub next: Option<ObjId>,
}

// Cspace residency (plan §6a) — the slot-handle list a cspace object owns. The
// kernel fixes this at construction (`cspace_num_slots`/`cspace_slot` are getters
// with no setter), so it is an immutable projection exactly like `ChanView
// .ring_cap` / `TcbView.bind_slots`: every mutator frames it unchanged. It is the
// residency `destroy_cspace`'s resident loop (6c) and 6e's revoke-root-survival
// name "the slots `cs` owns" through. `num_slots` and `slots.len()` agree on a
// well-formed cspace (the getter contracts require it).
#[verifier::ext_equal]
pub struct CSpaceView {
    pub num_slots: nat,
    pub slots: Seq<SlotId>,
}

// The abstract `Store` model: a slot arena + a refcount map. The trait stays
// plain Rust (the kernel impls it without ghost members); this `external_trait
// _specification` attaches the contract, and `external_trait_extension` adds the
// ghost views/predicates the trait never declared. Only the methods cspace/CDT
// calls are contracted; the rest of the ~70-method seam is left unconstrained
// (it is exercised by later phases).
#[verifier::external_trait_specification]
#[verifier::external_trait_extension(StoreSpec via StoreSpecImpl)]
pub trait ExStore {
    type ExternalTraitSpecificationFor: Store;

    // The slot arena: handle → slot. Its domain is the set of live slots.
    spec fn slot_view(&self) -> Map<SlotId, CapSlot>;
    // Object refcounts: handle → count.
    spec fn refs_view(&self) -> Map<ObjId, nat>;
    // Cspace residency (plan §6a): handle → the slot-handle list the cspace owns.
    // Immutable (the residency getters have no setter); every mutator frames it
    // unchanged, so `destroy_cspace`'s resident loop (6c) and revoke-root-survival
    // (6e) reason over a residency that stays stable across the teardown ops'
    // internal setter calls (whose bodies land in 6d). The `chan_view` analog,
    // residency edition. (`refcount_sound` does *not* read residency — its terms
    // are over the slot/chan/notif/tcb/timer views — so the sweep is forward-looking
    // for the resident-walk reasoning, not a census dependency.)
    spec fn cspace_view(&self) -> Map<ObjId, CSpaceView>;
    // Channel state: handle → ghost view (plan §3b). The third independent view;
    // the slot/refs setters frame it unchanged and the channel setters frame
    // slot/refs unchanged, so the §4.3 ops can reason about one without the others.
    spec fn chan_view(&self) -> Map<ObjId, ChanView>;
    // Notification / TCB / timer state (plan §4a) — three more independent views.
    // Every setter frames the *other* five views (+ the `timer_head_view` scalar)
    // unchanged, so a §4.4 op reasons about one without re-establishing the rest
    // (the mutual-frame discipline, doc 27 §1, extended to a six-view world).
    spec fn notif_view(&self) -> Map<ObjId, NotifView>;
    spec fn tcb_view(&self) -> Map<ObjId, TcbView>;
    spec fn timer_view(&self) -> Map<ObjId, TimerView>;
    // The armed-timer list head — a `Store`-seam scalar (the kernel static,
    // store.rs:130); the list *logic* is in `crate::timer` (phase 4e).
    spec fn timer_head_view(&self) -> Option<ObjId>;
    // The TLBI effect log (plan §5e): the ordered sequence of `(asid, va)` TLB
    // invalidations issued through this store. The seventh view — pure hardware
    // effect, not object state — so `aspace::unmap_in` can prove "one TLBI per
    // cleared page, in order" as a real postcondition. Only the three hardware-
    // seam methods below touch it; it is left unconstrained across the object
    // setters (no object op interleaves a setter with a TLBI), so adding it is a
    // localized seam change, not a per-setter sweep (plan §5e/§1.4).
    spec fn tlb_log_view(&self) -> Seq<(u16, u64)>;

    fn slot(&self, s: SlotId) -> (r: CapSlot)
        requires self.slot_view().dom().contains(s),
        ensures r == self.slot_view()[s];

    fn set_slot(&mut self, s: SlotId, v: CapSlot)
        requires old(self).slot_view().dom().contains(s),
        ensures
            final(self).slot_view() == old(self).slot_view().insert(s, v),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn obj_refs(&self, o: ObjId) -> (r: u32)
        requires self.refs_view().dom().contains(o),
        ensures r as nat == self.refs_view()[o];

    fn set_obj_refs(&mut self, o: ObjId, r: u32)
        requires old(self).refs_view().dom().contains(o),
        ensures
            final(self).refs_view() == old(self).refs_view().insert(o, r as nat),
            final(self).slot_view() == old(self).slot_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // ── cspace residents (plan §6a) ─────────────────────────────────────────
    //
    // The two residency getters, contracted against `cspace_view` (they are in the
    // plain `Store` trait uncontracted today). Immutable — no setter — so they only
    // read; `destroy_cspace`'s loop (6c) and 6e's revoke-root-survival walk the
    // residents through them. The `slots.len() == num_slots` precondition is the
    // residency well-formedness the kernel maintains by construction.
    fn cspace_num_slots(&self, cs: ObjId) -> (r: u32)
        requires
            self.cspace_view().dom().contains(cs),
            self.cspace_view()[cs].slots.len() == self.cspace_view()[cs].num_slots,
        ensures r as nat == self.cspace_view()[cs].num_slots;

    fn cspace_slot(&self, cs: ObjId, i: u32) -> (r: SlotId)
        requires
            self.cspace_view().dom().contains(cs),
            (i as nat) < self.cspace_view()[cs].num_slots,
            self.cspace_view()[cs].slots.len() == self.cspace_view()[cs].num_slots,
        ensures r == self.cspace_view()[cs].slots[i as int];

    // ── channel accessors (plan §3b) ────────────────────────────────────────
    //
    // Each relates to `chan_view` exactly as `slot`/`set_slot` relate to
    // `slot_view`: getters project a field; setters update one key and frame the
    // *other* two views unchanged. Index bounds (end/ring < 2, ev < 3) mirror the
    // production fixed-array bounds.
    fn chan_depth(&self, ch: ObjId) -> (r: u32)
        requires self.chan_view().dom().contains(ch),
        ensures r as nat == self.chan_view()[ch].depth;

    fn chan_end_caps(&self, ch: ObjId, end: usize) -> (r: u32)
        requires
            self.chan_view().dom().contains(ch),
            end < 2,
        ensures r as nat == self.chan_view()[ch].end_caps[end as int];

    fn set_chan_end_caps(&mut self, ch: ObjId, end: usize, v: u32)
        requires
            old(self).chan_view().dom().contains(ch),
            end < 2,
            old(self).chan_view()[ch].end_caps.len() == 2,
        ensures
            final(self).chan_view() == old(self).chan_view().insert(
                ch,
                ChanView {
                    end_caps: old(self).chan_view()[ch].end_caps.update(end as int, v as nat),
                    ..old(self).chan_view()[ch]
                }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn chan_head(&self, ch: ObjId, ring: usize) -> (r: u32)
        requires
            self.chan_view().dom().contains(ch),
            ring < 2,
        ensures r as nat == self.chan_view()[ch].head[ring as int];

    fn set_chan_head(&mut self, ch: ObjId, ring: usize, v: u32)
        requires
            old(self).chan_view().dom().contains(ch),
            ring < 2,
            old(self).chan_view()[ch].head.len() == 2,
        ensures
            final(self).chan_view() == old(self).chan_view().insert(
                ch,
                ChanView {
                    head: old(self).chan_view()[ch].head.update(ring as int, v as nat),
                    ..old(self).chan_view()[ch]
                }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn chan_count(&self, ch: ObjId, ring: usize) -> (r: u32)
        requires
            self.chan_view().dom().contains(ch),
            ring < 2,
        ensures r as nat == self.chan_view()[ch].count[ring as int];

    fn set_chan_count(&mut self, ch: ObjId, ring: usize, v: u32)
        requires
            old(self).chan_view().dom().contains(ch),
            ring < 2,
            old(self).chan_view()[ch].count.len() == 2,
        ensures
            final(self).chan_view() == old(self).chan_view().insert(
                ch,
                ChanView {
                    count: old(self).chan_view()[ch].count.update(ring as int, v as nat),
                    ..old(self).chan_view()[ch]
                }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn chan_binding(&self, ch: ObjId, end: usize, ev: usize) -> (r: Binding)
        requires
            self.chan_view().dom().contains(ch),
            end < 2,
            ev < 3,
        ensures r == self.chan_view()[ch].bindings[(end as int, ev as int)];

    fn set_chan_binding(&mut self, ch: ObjId, end: usize, ev: usize, b: Binding)
        requires
            old(self).chan_view().dom().contains(ch),
            end < 2,
            ev < 3,
        ensures
            final(self).chan_view() == old(self).chan_view().insert(
                ch,
                ChanView {
                    bindings: old(self).chan_view()[ch].bindings.insert((end as int, ev as int), b),
                    ..old(self).chan_view()[ch]
                }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // The ring cap-slot handle — immutable channel layout, so getter only.
    fn chan_ring_cap(&self, ch: ObjId, ring: usize, i: u32, c: usize) -> (r: SlotId)
        requires
            self.chan_view().dom().contains(ch),
            ring < 2,
            c < 4,
        ensures r == self.chan_view()[ch].ring_cap[(ring as int, i as int, c as int)];

    fn chan_msg_len(&self, ch: ObjId, ring: usize, i: u32) -> (r: u16)
        requires
            self.chan_view().dom().contains(ch),
            ring < 2,
        ensures r as nat == self.chan_view()[ch].msg_len[(ring as int, i as int)];

    fn set_chan_msg_len(&mut self, ch: ObjId, ring: usize, i: u32, v: u16)
        requires
            old(self).chan_view().dom().contains(ch),
            ring < 2,
        ensures
            final(self).chan_view() == old(self).chan_view().insert(
                ch,
                ChanView {
                    msg_len: old(self).chan_view()[ch].msg_len.insert((ring as int, i as int), v as nat),
                    ..old(self).chan_view()[ch]
                }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // Payload is abstracted out, so the write is a frame-only no-op on the
    // abstract state; `chan_msg_read` is `&self` (no obligation, omitted).
    fn chan_msg_write(&mut self, ch: ObjId, ring: usize, i: u32, data: &[u8])
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // `&self` (only `buf` is written), so the store is unchanged automatically; the
    // payload is abstracted out, so no spec on `buf`. Needed in `verus!` since 3d's
    // `recv` calls it (3b omitted it as frame-only).
    fn chan_msg_read(&self, ch: ObjId, ring: usize, i: u32, len: usize, buf: &mut [u8]);

    // ── notification accessors (plan §4a) ───────────────────────────────────
    //
    // Each relates to `notif_view` exactly as `slot`/`set_slot` relate to
    // `slot_view`: getters project a field; setters update one key and frame the
    // *other* five views + the `timer_head_view` scalar unchanged.
    fn notif_word(&self, n: ObjId) -> (r: u64)
        requires self.notif_view().dom().contains(n),
        ensures r == self.notif_view()[n].word;

    fn set_notif_word(&mut self, n: ObjId, v: u64)
        requires old(self).notif_view().dom().contains(n),
        ensures
            final(self).notif_view() == old(self).notif_view().insert(
                n, NotifView { word: v, ..old(self).notif_view()[n] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn notif_wait_head(&self, n: ObjId) -> (r: Option<ObjId>)
        requires self.notif_view().dom().contains(n),
        ensures r == self.notif_view()[n].wait_head;

    fn set_notif_wait_head(&mut self, n: ObjId, t: Option<ObjId>)
        requires old(self).notif_view().dom().contains(n),
        ensures
            final(self).notif_view() == old(self).notif_view().insert(
                n, NotifView { wait_head: t, ..old(self).notif_view()[n] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn notif_wait_tail(&self, n: ObjId) -> (r: Option<ObjId>)
        requires self.notif_view().dom().contains(n),
        ensures r == self.notif_view()[n].wait_tail;

    fn set_notif_wait_tail(&mut self, n: ObjId, t: Option<ObjId>)
        requires old(self).notif_view().dom().contains(n),
        ensures
            final(self).notif_view() == old(self).notif_view().insert(
                n, NotifView { wait_tail: t, ..old(self).notif_view()[n] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // ── thread (TCB) accessors (plan §4a) ───────────────────────────────────
    //
    // Setters update one `tcb_view` field and frame the other five views + the
    // `timer_head_view` scalar unchanged. `tcb_bind_slot` is a getter only — the
    // bind-slot *handles* are immutable channel-layout-style projections (the cap
    // contents live in `slot_view`); `set_tcb_retval` is a setter only (the seam
    // has no `tcb_retval` getter — it writes `frame.x[0]`).
    fn tcb_state(&self, t: ObjId) -> (r: ThreadState)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].state;

    fn set_tcb_state(&mut self, t: ObjId, s: ThreadState)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { state: s, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn tcb_qnext(&self, t: ObjId) -> (r: Option<ObjId>)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].qnext;

    fn set_tcb_qnext(&mut self, t: ObjId, q: Option<ObjId>)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { qnext: q, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn tcb_wait_notif(&self, t: ObjId) -> (r: Option<ObjId>)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].wait_notif;

    fn set_tcb_wait_notif(&mut self, t: ObjId, n: Option<ObjId>)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { wait_notif: n, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn tcb_report(&self, t: ObjId) -> (r: Report)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].report;

    fn set_tcb_report(&mut self, t: ObjId, r: Report)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { report: r, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn tcb_bind_slot(&self, t: ObjId, which: usize) -> (r: SlotId)
        requires
            self.tcb_view().dom().contains(t),
            which < 2,
        ensures r == self.tcb_view()[t].bind_slots[which as int];

    fn tcb_bind_bits(&self, t: ObjId, which: usize) -> (r: u64)
        requires
            self.tcb_view().dom().contains(t),
            which < 2,
        ensures r == self.tcb_view()[t].bind_bits[which as int];

    fn set_tcb_bind_bits(&mut self, t: ObjId, which: usize, b: u64)
        requires
            old(self).tcb_view().dom().contains(t),
            which < 2,
            old(self).tcb_view()[t].bind_bits.len() == 2,
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView {
                    bind_bits: old(self).tcb_view()[t].bind_bits.update(which as int, b),
                    ..old(self).tcb_view()[t]
                }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn tcb_cspace(&self, t: ObjId) -> (r: Option<ObjId>)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].cspace;

    fn set_tcb_cspace(&mut self, t: ObjId, cs: Option<ObjId>)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { cspace: cs, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn tcb_aspace(&self, t: ObjId) -> (r: Option<ObjId>)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].aspace;

    fn set_tcb_aspace(&mut self, t: ObjId, a: Option<ObjId>)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { aspace: a, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn set_tcb_retval(&mut self, t: ObjId, v: u64)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { retval: v, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // ── timer accessors (plan §4a; the armed-list logic is phase 4e) ─────────
    //
    // Setters update one `timer_view` field (or the `timer_head_view` scalar) and
    // frame the other five views unchanged.
    fn timer_armed(&self, t: ObjId) -> (r: bool)
        requires self.timer_view().dom().contains(t),
        ensures r == self.timer_view()[t].armed;

    fn set_timer_armed(&mut self, t: ObjId, v: bool)
        requires old(self).timer_view().dom().contains(t),
        ensures
            final(self).timer_view() == old(self).timer_view().insert(
                t, TimerView { armed: v, ..old(self).timer_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn timer_deadline(&self, t: ObjId) -> (r: u64)
        requires self.timer_view().dom().contains(t),
        ensures r == self.timer_view()[t].deadline;

    fn set_timer_deadline(&mut self, t: ObjId, v: u64)
        requires old(self).timer_view().dom().contains(t),
        ensures
            final(self).timer_view() == old(self).timer_view().insert(
                t, TimerView { deadline: v, ..old(self).timer_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn timer_notif(&self, t: ObjId) -> (r: Option<ObjId>)
        requires self.timer_view().dom().contains(t),
        ensures r == self.timer_view()[t].notif;

    fn set_timer_notif(&mut self, t: ObjId, n: Option<ObjId>)
        requires old(self).timer_view().dom().contains(t),
        ensures
            final(self).timer_view() == old(self).timer_view().insert(
                t, TimerView { notif: n, ..old(self).timer_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn timer_bits(&self, t: ObjId) -> (r: u64)
        requires self.timer_view().dom().contains(t),
        ensures r == self.timer_view()[t].bits;

    fn set_timer_bits(&mut self, t: ObjId, v: u64)
        requires old(self).timer_view().dom().contains(t),
        ensures
            final(self).timer_view() == old(self).timer_view().insert(
                t, TimerView { bits: v, ..old(self).timer_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn timer_next(&self, t: ObjId) -> (r: Option<ObjId>)
        requires self.timer_view().dom().contains(t),
        ensures r == self.timer_view()[t].next;

    fn set_timer_next(&mut self, t: ObjId, n: Option<ObjId>)
        requires old(self).timer_view().dom().contains(t),
        ensures
            final(self).timer_view() == old(self).timer_view().insert(
                t, TimerView { next: n, ..old(self).timer_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn timer_armed_head(&self) -> (r: Option<ObjId>)
        ensures r == self.timer_head_view();

    fn set_timer_armed_head(&mut self, h: Option<ObjId>)
        ensures
            final(self).timer_head_view() == h,
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // ── scheduler seam (plan §4a; §1.3) ─────────────────────────────────────
    //
    // The single assumed scheduler contract phase 4 adds: `signal`'s body proof
    // (4b) needs to know the wake touches only the woken thread's `state`. The
    // ready queue is scheduler state *below* the abstract `tcb_view` (a thread is
    // off every kcore queue once Runnable — `signal` sets `qnext = None` before
    // calling this), so modeling it as "state → Runnable, all else fixed" is
    // faithful; host-test-checked against `ArrayStore`. `unqueue_ready`'s contract
    // waits for 4e.
    fn make_runnable(&mut self, t: ObjId)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { state: ThreadState::Runnable, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // ── aspace hardware seam (plan §1.4; the `aspace::map_in` post-map barrier) ─
    //
    // The barrier carries no object state — it issues a `dsb`/`isb` so the leaf
    // writes are visible before the mapping is used. Modeled as "frames every
    // object view", which is faithful (it touches no kcore object) and is all
    // `map_in` needs to call it in the verified fragment. Because it takes
    // neither page-table slice, Verus already knows it cannot perturb `l1`/`pool`,
    // so `map_in`'s page-table postcondition is independent of this contract.
    // It also frames the TLBI log unchanged (plan §5e) — the log only ever grows
    // via `tlb_invalidate_page`, so the barrier is a pure ordering fence.
    fn barrier_after_map(&mut self)
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).tlb_log_view() == old(self).tlb_log_view();

    // ── unmap hardware seam (plan §5e; the `aspace::unmap_in` TLBI ordering) ──
    //
    // The two effect-log methods `unmap_in` calls. `tlb_invalidate_page` appends
    // exactly one `(asid, va)` entry — that *append* is what makes "one TLBI per
    // cleared page, in ascending order" a postcondition (the loop invariant tracks
    // `tlb_log_view() == old ++ cleared-prefix`). Both frame every object view, so
    // the page-table postcondition and the log postcondition compose cleanly.
    fn tlb_invalidate_page(&mut self, asid: u16, va: u64)
        ensures
            final(self).tlb_log_view() == old(self).tlb_log_view().push((asid, va)),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // The trailing `dsb`/`isb` after the per-page TLBIs — a pure fence, framing
    // every object view *and* the accumulated TLBI log (so the loop's final log
    // state survives the barrier).
    fn barrier_after_unmap(&mut self)
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).tlb_log_view() == old(self).tlb_log_view();

    // ── aspace teardown seam (plan §6a; the cross-object-teardown phase) ──────
    //
    // Two shell-owned page-table ops kcore never sees the body of (the trusted
    // base, plan §2). Assumed, host-checked against `ArrayStore` (the
    // `make_runnable` precedent). `aspace_unmap` is page-table maintenance — no
    // object state — so it frames every object view + `refs_view` + `cspace_view`
    // (the TLBI log it may touch is left unconstrained, like the other hardware
    // effects). `aspace_destroy` is the last-reference teardown `unref_aspace` and
    // `delete`'s frame-unmap branch call once `refs[a] == 0`: it drops `a` from
    // `refs_view` and frames the rest (an aspace is no cspace, so `cspace_view` is
    // untouched).
    fn aspace_unmap(&mut self, a: ObjId, va: u64, pages: u64)
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn aspace_destroy(&mut self, a: ObjId)
        requires
            old(self).refs_view().dom().contains(a),
            old(self).refs_view()[a] == 0,
        ensures
            final(self).refs_view() == old(self).refs_view().remove(a),
            final(self).slot_view() == old(self).slot_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view();
}

// The refcounted object a cap designates (the spec mirror of `Cap::obj`).
pub open spec fn cap_obj(c: Cap) -> Option<ObjId> {
    match c.kind {
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
        CapKind::Aspace(o) => Some(o),
        CapKind::CSpace(o) => Some(o),
        CapKind::Thread(o) => Some(o),
        CapKind::Channel(o, _) => Some(o),
        CapKind::Notification(o) => Some(o),
        CapKind::Timer(o) => Some(o),
    }
}

pub open spec fn is_empty_cap(c: Cap) -> bool {
    c.kind matches CapKind::Empty
}

// The notification a cap names, if it is a notification cap (else `None`). The
// spec projection `thread::report_terminal`/`thread::bind` (plan §4d) use to talk
// about the notification a TCB bind slot holds — narrower than `cap_obj` (which
// returns the object for *any* object cap), because a bind slot only ever holds a
// notification cap and the §4d contracts reason specifically about that case.
pub open spec fn cap_notif(c: Cap) -> Option<ObjId> {
    match c.kind {
        CapKind::Notification(o) => Some(o),
        _ => None,
    }
}

// Exec emptiness check tied to the `is_empty_cap` spec — `Cap::is_empty` is plain
// Rust (outside `verus!`), so verified exec code (channel `recv`, §3d) uses this.
pub fn cap_is_empty(c: Cap) -> (r: bool)
    ensures
        r == is_empty_cap(c),
{
    matches!(c.kind, CapKind::Empty)
}

// The kind a derivation produces from `k`: identical (same object, same channel
// end), except a Frame copy starts unmapped (§2.5, one mapping per cap copy).
// This is the "copy" half of monotone derivation — derivation cannot change the
// designated object or amplify via the kind.
pub open spec fn derived_kind(k: CapKind) -> CapKind {
    match k {
        CapKind::Frame { base, pages, mapping: _ } => CapKind::Frame { base, pages, mapping: None },
        _ => k,
    }
}

// `Rights::masked` now carries its bit-level `ensures` on the verified method
// itself (see the `impl Rights` verus block above) — the standalone
// `assume_specification` it used to need is gone (doc/results/28 §1).

// `CapSlot::empty` is a plain-Rust const fn (shared with the kernel shell); state
// what it builds so `slot_move`'s final clear can be verified — an empty cap with
// all CDT links detached.
pub assume_specification [ CapSlot::empty ]() -> (r: CapSlot)
    ensures
        is_empty_cap(r.cap),
        r.parent is None,
        r.first_child is None,
        r.next_sib is None,
        r.prev_sib is None;

// ── Structural well-formedness of the CDT (the executable `TypeOK`, now total
//    and unbounded). Acyclicity is tracked separately where termination needs
//    it (plan §4.1); this is the structural invariant the op proofs preserve. ──

pub open spec fn link_in_dom(m: Map<SlotId, CapSlot>, o: Option<SlotId>) -> bool {
    match o {
        None => true,
        Some(h) => m.dom().contains(h),
    }
}

// Every CDT link is None or points at a live slot.
pub open spec fn links_in_domain(m: Map<SlotId, CapSlot>) -> bool {
    forall|k: SlotId| #[trigger] m.dom().contains(k) ==> {
        &&& link_in_dom(m, m[k].parent)
        &&& link_in_dom(m, m[k].first_child)
        &&& link_in_dom(m, m[k].next_sib)
        &&& link_in_dom(m, m[k].prev_sib)
    }
}

// The sibling list is doubly consistent in both directions.
pub open spec fn siblings_doubly_consistent(m: Map<SlotId, CapSlot>) -> bool {
    forall|a: SlotId| #[trigger] m.dom().contains(a) ==> {
        &&& (m[a].next_sib matches Some(b) ==> m.dom().contains(b) && m[b].prev_sib == Some(a))
        &&& (m[a].prev_sib matches Some(b) ==> m.dom().contains(b) && m[b].next_sib == Some(a))
    }
}

// Siblings in a next/prev chain share the same parent. Combined with
// `head_is_first_child` and `siblings_doubly_consistent`, this pins a node's
// residents to its `first_child → next_sib` chain — the reachability anchor the
// construction-side acyclicity proofs need (doc/results/21 §9): without it a
// node could name a parent while being absent from that parent's child list.
pub open spec fn siblings_share_parent(m: Map<SlotId, CapSlot>) -> bool {
    forall|a: SlotId| #[trigger] m.dom().contains(a) ==>
        (m[a].next_sib matches Some(b) ==> m[b].parent == m[a].parent)
}

// A node's first child claims it as parent and heads the sibling list.
pub open spec fn first_child_parent_agree(m: Map<SlotId, CapSlot>) -> bool {
    forall|p: SlotId| #[trigger] m.dom().contains(p) ==>
        (m[p].first_child matches Some(c) ==>
            m.dom().contains(c) && m[c].parent == Some(p) && m[c].prev_sib == None)
}

// The converse: a list-head node (parent set, no prev sibling) IS its parent's
// first child. Without this a child could name a parent that has forgotten it —
// an orphan-head the revoke walk would never reach. (Restores parity with the
// pre-rewrite predicate.)
pub open spec fn head_is_first_child(m: Map<SlotId, CapSlot>) -> bool {
    forall|c: SlotId| #[trigger] m.dom().contains(c) ==>
        (m[c].parent matches Some(p) ==> (m[c].prev_sib is None ==>
            m.dom().contains(p) && m[p].first_child == Some(c)))
}

// A node with a parent has that parent set as non-childless: `first_child` is
// `Some`. The "no phantom child" anchor — contrapositive: a childless node
// (`first_child == None`) has no resident naming it parent. That is exactly what
// lets a fresh detached leaf take the lowest acyclicity rank without a
// still-lower phantom child needing to exist (doc/results/21 §9).
pub open spec fn parent_has_first_child(m: Map<SlotId, CapSlot>) -> bool {
    forall|k: SlotId| #[trigger] m.dom().contains(k) ==>
        (m[k].parent matches Some(p) ==> m[p].first_child is Some)
}

// Empty slots are fully detached.
pub open spec fn empty_slots_detached(m: Map<SlotId, CapSlot>) -> bool {
    forall|k: SlotId| #[trigger] m.dom().contains(k) ==> (is_empty_cap(m[k].cap) ==> {
        &&& m[k].parent == None
        &&& m[k].first_child == None
        &&& m[k].next_sib == None
        &&& m[k].prev_sib == None
    })
}

// The **structural** CDT invariant (the structural half of the TLA TypeOK).
// Acyclicity is layered on top as `cspace_wf` below — kept separate so the
// non-recursive ops that don't need termination reason about the cheaper
// predicate.
pub open spec fn cdt_wf(m: Map<SlotId, CapSlot>) -> bool {
    &&& links_in_domain(m)
    &&& siblings_doubly_consistent(m)
    &&& siblings_share_parent(m)
    &&& first_child_parent_agree(m)
    &&& head_is_first_child(m)
    &&& parent_has_first_child(m)
    &&& empty_slots_detached(m)
}

// ── Acyclicity (the basis for revoke's termination) ──────────────────────────
//
// A strict decrease along a link makes that relation well-founded — no cycle can
// return from a smaller rank to a larger one. The ranks are GHOST-only
// (existential witnesses), so they need no home in the abstract `Store`: a proof
// that needs termination chooses a witness. `prank` decreases parent→child, so
// descending to a leaf via `first_child` strictly lowers it.
pub open spec fn valid_prank(m: Map<SlotId, CapSlot>, r: Map<SlotId, nat>) -> bool {
    &&& r.dom() == m.dom()
    &&& forall|k: SlotId| #[trigger] m.dom().contains(k) ==>
            (m[k].parent matches Some(p) ==> m.dom().contains(p) && r[k] < r[p])
}

pub open spec fn acyclic(m: Map<SlotId, CapSlot>) -> bool {
    exists|r: Map<SlotId, nat>| valid_prank(m, r)
}

// Sibling-chain acyclicity — the analog of `acyclic` for the `next_sib` relation,
// the well-founded measure the children-walk loops (`slot_move`/`cdt_unlink`)
// decrease. `cdt_wf` alone does NOT exclude a sibling cycle: a *floating* cycle
// (a ring of `next_sib`/`prev_sib`-consistent nodes, none a `first_child` head,
// all sharing a parent whose `first_child` points elsewhere) satisfies every
// structural clause. So sibling termination needs its own ghost rank, layered
// into `cspace_wf` and preserved by the construction ops (doc/results/22).
pub open spec fn valid_srank(m: Map<SlotId, CapSlot>, s: Map<SlotId, nat>) -> bool {
    &&& s.dom() == m.dom()
    &&& forall|k: SlotId| #[trigger] m.dom().contains(k) ==>
            (m[k].next_sib matches Some(n) ==> m.dom().contains(n) && s[n] < s[k])
}

pub open spec fn sib_acyclic(m: Map<SlotId, CapSlot>) -> bool {
    exists|s: Map<SlotId, nat>| valid_srank(m, s)
}

// The full cspace well-formedness: structure + parent-acyclicity + sibling-
// acyclicity. The invariant the recursive/looping ops require and preserve.
pub open spec fn cspace_wf(m: Map<SlotId, CapSlot>) -> bool {
    cdt_wf(m) && acyclic(m) && sib_acyclic(m)
}

// ── Channel well-formedness (the §4.3 `chan_wf`; plan §3b) ───────────────────
//
// Ring index `i` is in channel `c`'s live window for `ring` iff it is one of the
// `count[ring]` positions starting at `head[ring]` (wrapping mod `depth`) — the
// FIFO window 3d's `send`/`recv` `Seq` model projects through. Stated as the
// existential so the modular arithmetic stays out of the predicate (the doc-25 §2
// discipline: quarantine non-linear `%` into 3d's helpers, not the invariant).
pub open spec fn in_live_window(c: ChanView, ring: int, i: int) -> bool {
    exists|j: int| #![trigger (c.head[ring] + j) % (c.depth as int)]
        0 <= j < c.count[ring] && i == (c.head[ring] + j) % (c.depth as int)
}

// `chan_wf(cv, sv, ch)` — channel `ch` is well-formed. Takes **both** views: the
// detail plan (§3b) lists chan_wf with `(cv, ch)`, but its own clause "ring slots
// outside the live window are empty (their `SlotId` empty in `slot_view`)" needs
// the arena, so the signature is `(cv, sv, ch)` (recorded in doc/results/27 §1.1).
//
// Clauses: depth positive; the Seq fields have length 2; the FIFO cursors are in
// range; `ring_cap`/`msg_len`/`bindings` have their expected domains; every ring
// cap handle lives in the arena; and the **coupling** — a ring cap outside the
// live window is empty in `slot_view`. Per-channel (cross-channel ring-slot
// disjointness and ring-cap injectivity are extra invariants 3d adds when
// `send`/`recv` need them; not part of the shape, so not asserted here).
pub open spec fn chan_wf(cv: Map<ObjId, ChanView>, sv: Map<SlotId, CapSlot>, ch: ObjId) -> bool {
    &&& cv.dom().contains(ch)
    &&& cv[ch].depth > 0
    // `send` forms `(head + count) % depth` in u32; with head < depth and
    // count <= depth this bound keeps the sum within u32 (a channel cannot have
    // 2^31 256-byte message slots — that is 500+ GiB of ring).
    &&& cv[ch].depth <= 0x8000_0000
    &&& cv[ch].end_caps.len() == 2
    &&& cv[ch].head.len() == 2
    &&& cv[ch].count.len() == 2
    &&& forall|r: int| #![trigger cv[ch].count[r]] 0 <= r < 2 ==> cv[ch].count[r] <= cv[ch].depth
    &&& forall|r: int| #![trigger cv[ch].head[r]] 0 <= r < 2 ==> cv[ch].head[r] < cv[ch].depth
    &&& forall|r: int, i: int, c: int|
            (0 <= r < 2 && 0 <= i < cv[ch].depth && 0 <= c < 4)
                ==> #[trigger] cv[ch].ring_cap.dom().contains((r, i, c))
    &&& forall|r: int, i: int, c: int|
            (0 <= r < 2 && 0 <= i < cv[ch].depth && 0 <= c < 4)
                ==> sv.dom().contains(#[trigger] cv[ch].ring_cap[(r, i, c)])
    &&& forall|r: int, i: int, c: int|
            (0 <= r < 2 && 0 <= i < cv[ch].depth && 0 <= c < 4 && !in_live_window(cv[ch], r, i))
                ==> is_empty_cap(sv[#[trigger] cv[ch].ring_cap[(r, i, c)]].cap)
    // Ring-cap injectivity: distinct ring positions map to distinct arena handles
    // (3b deferred this to 3d; doc 27 §3). Load-bearing for `send`/`recv` — it is
    // what lets filling the new tail slot (or emptying the head slot) leave every
    // other in-window message untouched.
    &&& forall|r1: int, i1: int, c1: int, r2: int, i2: int, c2: int|
            #![trigger cv[ch].ring_cap[(r1, i1, c1)], cv[ch].ring_cap[(r2, i2, c2)]]
            (0 <= r1 < 2 && 0 <= i1 < cv[ch].depth && 0 <= c1 < 4
                && 0 <= r2 < 2 && 0 <= i2 < cv[ch].depth && 0 <= c2 < 4
                && cv[ch].ring_cap[(r1, i1, c1)] == cv[ch].ring_cap[(r2, i2, c2)])
                ==> (r1 == r2 && i1 == i2 && c1 == c2)
    &&& forall|r: int, i: int|
            (0 <= r < 2 && 0 <= i < cv[ch].depth) ==> #[trigger] cv[ch].msg_len.dom().contains((r, i))
    &&& forall|e: int, v: int|
            (0 <= e < 2 && 0 <= v < 3) ==> #[trigger] cv[ch].bindings.dom().contains((e, v))
}

// ── The FIFO Seq model (the §4.3 centerpiece; plan §3d) ──────────────────────
//
// A queued message is `(len, caps)` — payload bytes abstracted (doc 27), so its
// observable content is the length and the four cap *contents*, read from the
// arena at the ring handles. `ring_fifo` projects a ring's live window
// `[head, head+count) mod depth` to a `Seq` in FIFO order: `send` appends
// (`Seq::push`), `recv` pops the head (`Seq::drop_first`).
pub open spec fn ring_msg(cv: ChanView, sv: Map<SlotId, CapSlot>, ring: int, idx: int)
    -> (nat, Seq<Cap>) {
    (cv.msg_len[(ring, idx)], Seq::new(4, |c: int| sv[cv.ring_cap[(ring, idx, c)]].cap))
}

pub open spec fn ring_fifo(cv: ChanView, sv: Map<SlotId, CapSlot>, ring: int)
    -> Seq<(nat, Seq<Cap>)> {
    Seq::new(cv.count[ring], |j: int| ring_msg(cv, sv, ring, (cv.head[ring] + j) % (cv.depth as int)))
}

// ── Notification waiter-queue well-formedness + the FIFO Seq model (plan §4a) ─
//
// The waiter queue is a SINGLY-linked intrusive list threaded through the TCBs:
// `NotifView` holds `wait_head`/`wait_tail`, each waiting TCB holds `qnext` (next
// waiter) + `wait_notif` (its notification). Unlike the CDT sibling list it has no
// back-pointer, so the doubly-consistent membership trick does not apply — the clean
// model is an explicit FIFO `Seq` witness (the §3d `ring_fifo` analog). `wait` pushes
// the tail (`Seq::push`, 4b), `signal` pops the head (`Seq::drop_first`, 4b),
// `remove_waiter` splices out one element (4c) — so "wake order = block order" (§4.4)
// is FIFO-ness of `waiter_seq`.

// A generic singly-linked-list acyclicity rank over an abstract successor map — the
// `valid_srank`/`sib_acyclic` analog, shared by the waiter queue (`succ` = `qnext`)
// and, in phase 4e, the armed-timer list (`succ` = `timer_next`): a strict decrease
// along `succ` makes the relation well-founded, so an unlink loop walking `succ`
// terminates. GHOST-only (the rank is an existential witness, no `Store` home). Over
// the `qnext` projection it is implied by `waiter_chain`'s `no_duplicates` (rank =
// position in the chain), so `notif_wf` need not assert it separately; it is the
// decreases mechanism phase 4c/4e instantiate.
pub open spec fn valid_list_rank(succ: Map<ObjId, Option<ObjId>>, r: Map<ObjId, nat>) -> bool {
    &&& r.dom() == succ.dom()
    &&& forall|k: ObjId| #[trigger] succ.dom().contains(k) ==>
            (succ[k] matches Some(nx) ==> succ.dom().contains(nx) && r[nx] < r[k])
}

pub open spec fn list_acyclic(succ: Map<ObjId, Option<ObjId>>) -> bool {
    exists|r: Map<ObjId, nat>| valid_list_rank(succ, r)
}

// `ws` is notification `n`'s waiter chain in FIFO (block) order. Pins the imperative
// `wait_head`/`wait_tail`/`qnext` links to the `Seq`: distinct elements (acyclicity —
// the index IS the rank), the head/tail agree with `ws`'s ends, `qnext` threads each
// element to the next (and the last to `None`), and every charted node names `n` and
// is `BlockedNotif`.
pub open spec fn waiter_chain(
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    n: ObjId,
    ws: Seq<ObjId>,
) -> bool {
    &&& ws.no_duplicates()
    &&& forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() ==> tv.dom().contains(ws[i])
    &&& (ws.len() == 0 ==> nv[n].wait_head is None && nv[n].wait_tail is None)
    &&& (ws.len() > 0 ==> nv[n].wait_head == Some(ws[0])
                       && nv[n].wait_tail == Some(ws[ws.len() - 1]))
    &&& forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() ==>
            tv[ws[i]].qnext == (if i + 1 < ws.len() { Some(ws[i + 1]) } else { None })
    &&& forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() ==>
            tv[ws[i]].wait_notif == Some(n) && tv[ws[i]].state == ThreadState::BlockedNotif
}

// Notification `n` is well-formed: empty-queue head/tail agreement, and a waiter
// chain witness exists. No op PROVES this in 4a — defined for 4b/4c, exercised by
// `notif_wf_exec` (the `chan_wf` discipline, doc 27 §1).
pub open spec fn notif_wf(nv: Map<ObjId, NotifView>, tv: Map<ObjId, TcbView>, n: ObjId) -> bool {
    &&& nv.dom().contains(n)
    &&& (nv[n].wait_head is None <==> nv[n].wait_tail is None)
    &&& exists|ws: Seq<ObjId>| waiter_chain(nv, tv, n, ws)
}

// The FIFO waiter `Seq` — well-defined when `notif_wf` holds (the chain is unique
// given the `qnext` threading). `wait` ⇒ `Seq::push`, `signal` ⇒ `Seq::drop_first`,
// `remove_waiter` ⇒ a splice (4b/4c — where the push/pop/splice lemmas land).
pub open spec fn waiter_seq(nv: Map<ObjId, NotifView>, tv: Map<ObjId, TcbView>, n: ObjId)
    -> Seq<ObjId> {
    choose|ws: Seq<ObjId>| waiter_chain(nv, tv, n, ws)
}

// `waiter_chain` determines `ws` uniquely (the `choose` in `waiter_seq` is therefore
// the FIFO order, not an arbitrary pick): two chains for the same `n` agree pointwise
// (the head fixes element 0, `qnext`-threading fixes each successor) and have equal
// length (a strict prefix's last node would need `qnext == None` by its own chain yet
// `Some(·)` by the longer one). This is what lets 4b/4c state `signal`/`wait`/
// `remove_waiter`'s effect as a `waiter_seq` equality (`drop_first`/`push`/splice) —
// the analog of `ring_fifo` being a deterministic `Seq::new` rather than a `choose`.

// `ws1[k] == ws2[k]` for any in-bounds `k`: heads agree (clause 4), then `qnext`
// threads each step (clause 5).
proof fn lemma_chain_eq_at(
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    n: ObjId,
    ws1: Seq<ObjId>,
    ws2: Seq<ObjId>,
    k: int,
)
    requires
        waiter_chain(nv, tv, n, ws1),
        waiter_chain(nv, tv, n, ws2),
        0 <= k < ws1.len(),
        k < ws2.len(),
    ensures
        ws1[k] == ws2[k],
    decreases k,
{
    if k == 0 {
        assert(nv[n].wait_head == Some(ws1[0]));
        assert(nv[n].wait_head == Some(ws2[0]));
    } else {
        lemma_chain_eq_at(nv, tv, n, ws1, ws2, k - 1);
        assert(tv[ws1[k - 1]].qnext == Some(ws1[k]));
        assert(tv[ws2[k - 1]].qnext == Some(ws2[k]));
    }
}

// No chain is a strict prefix of another: the shorter chain's last node ends the walk
// (`qnext == None`) but the longer chain threads it onward — contradiction.
proof fn lemma_chain_not_strict_prefix(
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    n: ObjId,
    ws1: Seq<ObjId>,
    ws2: Seq<ObjId>,
)
    requires
        waiter_chain(nv, tv, n, ws1),
        waiter_chain(nv, tv, n, ws2),
        ws1.len() < ws2.len(),
    ensures
        false,
{
    if ws1.len() == 0 {
        assert(nv[n].wait_head is None);
        assert(nv[n].wait_head == Some(ws2[0]));
    } else {
        let k: int = ws1.len() as int - 1;
        lemma_chain_eq_at(nv, tv, n, ws1, ws2, k);
        assert(tv[ws1[k]].qnext is None);
        assert(tv[ws2[k]].qnext == Some(ws2[k + 1]));
    }
}

// The uniqueness theorem (the central new lemma of phase 4b).
pub proof fn lemma_waiter_chain_unique(
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    n: ObjId,
    ws1: Seq<ObjId>,
    ws2: Seq<ObjId>,
)
    requires
        waiter_chain(nv, tv, n, ws1),
        waiter_chain(nv, tv, n, ws2),
    ensures
        ws1 == ws2,
{
    if ws1.len() < ws2.len() {
        lemma_chain_not_strict_prefix(nv, tv, n, ws1, ws2);
    }
    if ws2.len() < ws1.len() {
        lemma_chain_not_strict_prefix(nv, tv, n, ws2, ws1);
    }
    assert forall|i: int| 0 <= i < ws1.len() implies ws1[i] == ws2[i] by {
        lemma_chain_eq_at(nv, tv, n, ws1, ws2, i);
    }
    assert(ws1 =~= ws2);
}

// Binding-liveness companion to `chan_wf` (plan §4b, the named-invariant resolution):
// every bound endpoint event names a *live, well-formed* notification. STRUCTURAL only
// (`nv` domain + `notif_wf`) — no `refs` clause — which is exactly what makes it
// preservable across a fire: `signal` preserves `notif_wf` of the notification it
// signals and frames every other notif/TCB, and the enqueue/dequeue `slot_move` frames
// `notif_view`/`tcb_view`. So `fire`/`send`/`recv`/`endpoint_cap_dropped` can carry it
// in both `requires` and `ensures` (the `chan_wf` discipline), and `fire` discharges
// `signal`'s `notif_view`-domain + `notif_wf` preconditions from it. The waiter-release
// `refs[n] > 0` that `signal`'s wake path also needs is NOT here — it is not preservable
// across the `-1` without the refcount census (deferred to the post-phase-5 teardown
// phase, plan §1.4), so it rides as a precondition-only clause on the fire-callers.
pub open spec fn binding_notif_wf(
    cv: Map<ObjId, ChanView>,
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    ch: ObjId,
) -> bool {
    forall|e: int, v: int| #![trigger cv[ch].bindings[(e, v)]]
        (0 <= e < 2 && 0 <= v < 3 && cv[ch].bindings[(e, v)].notif is Some) ==> {
            &&& nv.dom().contains(cv[ch].bindings[(e, v)].notif->Some_0)
            &&& notif_wf(nv, tv, cv[ch].bindings[(e, v)].notif->Some_0)
        }
}

// The per-binding refs side-condition `signal` needs to discharge its wake-release
// `-1`: a queued waiter on binding `(e, v)`'s notification implies that notification has
// `refs > 0`. PRECONDITION-only on the fire-callers (it is the waiter term of the
// refcount census, not preservable across the `-1` without the full census deferred to
// the post-phase-5 teardown phase, plan §1.4) — unlike the structural `binding_notif_wf`.
pub open spec fn binding_refs_ok(
    cv: Map<ObjId, ChanView>,
    nv: Map<ObjId, NotifView>,
    rv: Map<ObjId, nat>,
    ch: ObjId,
    e: int,
    v: int,
) -> bool {
    cv[ch].bindings[(e, v)].notif is Some ==> (
        nv[cv[ch].bindings[(e, v)].notif->Some_0].wait_head is Some ==> (
            rv.dom().contains(cv[ch].bindings[(e, v)].notif->Some_0)
                && rv[cv[ch].bindings[(e, v)].notif->Some_0] > 0))
}

// `notif_wf(m)` survives any edit that leaves `m`'s notification view and all of `m`'s
// waiter TCBs (those with `wait_notif == Some(m)`) untouched — the rest of the store may
// move freely. This is how a fire that signals notification `n != m` preserves `m`'s
// well-formedness: `signal` only perturbs a TCB that was waiting on `n` (its
// `forall k` frame), so `m`'s chain nodes — all naming `m`, never `n` — are unchanged.
pub proof fn lemma_notif_wf_frame(
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    nv2: Map<ObjId, NotifView>,
    tv2: Map<ObjId, TcbView>,
    m: ObjId,
)
    requires
        notif_wf(nv, tv, m),
        nv2.dom().contains(m),
        nv2[m] == nv[m],
        tv2.dom() == tv.dom(),
        forall|k: ObjId| #[trigger] tv[k].wait_notif == Some(m) ==> tv2[k] == tv[k],
    ensures
        notif_wf(nv2, tv2, m),
{
    let ws = choose|ws: Seq<ObjId>| waiter_chain(nv, tv, m, ws);
    assert(waiter_chain(nv, tv, m, ws));
    assert(waiter_chain(nv2, tv2, m, ws)) by {
        assert forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() implies
            tv2.dom().contains(ws[i]) && tv2[ws[i]] == tv[ws[i]] by {
            assert(tv[ws[i]].wait_notif == Some(m));
        }
    }
}

// `signal`'s wake step (plan §4b): popping the head `t == ws0[0]` from a non-empty
// waiter chain yields `ws0.drop_first()` in the post-state, given the head/tail were
// re-pointed past `t` (new head = `t`'s old `qnext`; tail dropped to `None` exactly when
// that is `None`) and only `t`'s TCB moved. Extracted so `signal`'s own body query stays
// under the solver rlimit (the doc 25 §2 decomposition discipline).
pub proof fn lemma_drop_first_chain(
    nv0: Map<ObjId, NotifView>,
    tv0: Map<ObjId, TcbView>,
    nvf: Map<ObjId, NotifView>,
    tvf: Map<ObjId, TcbView>,
    n: ObjId,
    t: ObjId,
    ws0: Seq<ObjId>,
)
    requires
        waiter_chain(nv0, tv0, n, ws0),
        ws0.len() > 0,
        ws0[0] == t,
        nvf[n].wait_head == tv0[t].qnext,
        tv0[t].qnext is None ==> nvf[n].wait_tail is None,
        tv0[t].qnext is Some ==> nvf[n].wait_tail == nv0[n].wait_tail,
        tvf.dom() == tv0.dom(),
        forall|k: ObjId| #![trigger tvf[k]] k != t ==> tvf[k] == tv0[k],
    ensures
        waiter_chain(nvf, tvf, n, ws0.drop_first()),
{
    let dws = ws0.drop_first();
    // `tv0[t].qnext == (if 1 < len { Some(ws0[1]) } else { None })` — ws0 clause 5 at 0.
    assert(tv0[ws0[0]].qnext == (if 1 < ws0.len() { Some(ws0[1]) } else { None }));
    assert(dws.no_duplicates()) by {
        assert forall|i: int, j: int|
            0 <= i < dws.len() && 0 <= j < dws.len() && i != j implies dws[i] != dws[j] by {
            assert(dws[i] == ws0[i + 1]);
            assert(dws[j] == ws0[j + 1]);
        }
    }
    assert forall|i: int| #![trigger dws[i]] 0 <= i < dws.len() implies dws[i] != t by {
        assert(dws[i] == ws0[i + 1]);
    }
    assert forall|i: int| #![trigger dws[i]] 0 <= i < dws.len() implies
        tvf.dom().contains(dws[i])
        && tvf[dws[i]].qnext == (if i + 1 < dws.len() { Some(dws[i + 1]) } else { None })
        && tvf[dws[i]].wait_notif == Some(n)
        && tvf[dws[i]].state == ThreadState::BlockedNotif by {
        assert(dws[i] == ws0[i + 1]);
        assert(tv0[ws0[i + 1]].qnext == (if i + 2 < ws0.len() { Some(ws0[i + 2]) } else { None }));
    }
}

// `remove_waiter`'s splice step (plan §4c): unlinking `t == ws0[k]` from a waiter
// chain yields `ws0.remove(k)` in the post-state, given the imperative link fixups —
// the head re-pointed past `t` when `t` was the head (`k == 0`), the predecessor's
// `qnext` re-threaded past `t` otherwise (`k > 0`), the tail dropped to the
// predecessor when `t` was the tail (`k == len-1`), and `t` itself cleared. The
// mid-list analog of `lemma_drop_first_chain` (which is the `k == 0` head-pop special
// case); singly-linked with no re-parenting, so a plain `Seq::remove`, not the
// rank-rescaled merge `cdt_unlink` needed. Extracted so `remove_waiter`'s own body
// query stays under the solver rlimit (the doc 25 §2 decomposition discipline).
pub proof fn lemma_remove_chain(
    nv0: Map<ObjId, NotifView>,
    tv0: Map<ObjId, TcbView>,
    nvf: Map<ObjId, NotifView>,
    tvf: Map<ObjId, TcbView>,
    n: ObjId,
    t: ObjId,
    ws0: Seq<ObjId>,
    k: int,
)
    requires
        waiter_chain(nv0, tv0, n, ws0),
        0 <= k < ws0.len(),
        ws0[k] == t,
        tvf.dom() == tv0.dom(),
        // `t` cleared (set_tcb_qnext(t, None); set_tcb_wait_notif(t, None)).
        tvf[t].qnext is None,
        tvf[t].wait_notif is None,
        // predecessor re-threaded past `t` (k>0: set_tcb_qnext(ws0[k-1], tv0[t].qnext)),
        // its other fields framed.
        k > 0 ==> tvf[ws0[k - 1]].qnext == tv0[t].qnext,
        k > 0 ==> tvf[ws0[k - 1]].wait_notif == tv0[ws0[k - 1]].wait_notif,
        k > 0 ==> tvf[ws0[k - 1]].state == tv0[ws0[k - 1]].state,
        // every other TCB unchanged.
        forall|j: ObjId| #![trigger tvf[j]]
            j != t && (k == 0 || j != ws0[k - 1]) ==> tvf[j] == tv0[j],
        // head fix: k==0 ⇒ new head is `t`'s old qnext; else unchanged.
        k == 0 ==> nvf[n].wait_head == tv0[t].qnext,
        k > 0 ==> nvf[n].wait_head == nv0[n].wait_head,
        // tail fix: `t` was the tail (k==len-1) ⇒ tail drops to the predecessor; else
        // unchanged.
        k == ws0.len() - 1 ==> nvf[n].wait_tail
            == (if k == 0 { None::<ObjId> } else { Some(ws0[k - 1]) }),
        k < ws0.len() - 1 ==> nvf[n].wait_tail == nv0[n].wait_tail,
    ensures
        waiter_chain(nvf, tvf, n, ws0.remove(k)),
{
    let dws = ws0.remove(k);
    let len = ws0.len() as int;
    ws0.remove_ensures(k);
    // dws.len() == len - 1; dws[i] == ws0[i] for i<k, ws0[i+1] for k<=i<len-1.

    // Clause 1: no_duplicates. Each dws index maps to a distinct ws0 index.
    assert(dws.no_duplicates()) by {
        assert forall|i: int, j: int|
            0 <= i < dws.len() && 0 <= j < dws.len() && i != j implies dws[i] != dws[j] by {
            let ii = if i < k { i } else { i + 1 };
            let jj = if j < k { j } else { j + 1 };
            assert(dws[i] == ws0[ii] && dws[j] == ws0[jj]);
            assert(ii != jj);
        }
    }

    // Clauses 2, 5, 6: per-node domain / qnext-threading / wait_notif+state.
    assert forall|i: int| #![trigger dws[i]] 0 <= i < dws.len() implies
        tvf.dom().contains(dws[i])
        && tvf[dws[i]].qnext == (if i + 1 < dws.len() { Some(dws[i + 1]) } else { None::<ObjId> })
        && tvf[dws[i]].wait_notif == Some(n)
        && tvf[dws[i]].state == ThreadState::BlockedNotif by {
        let ii = if i < k { i } else { i + 1 };
        assert(dws[i] == ws0[ii]);
        // ws0 chain facts at index `ii` (clauses 2/5/6 of the source chain).
        assert(tv0.dom().contains(ws0[ii]));
        assert(tv0[ws0[ii]].qnext == (if ii + 1 < len { Some(ws0[ii + 1]) } else { None::<ObjId> }));
        assert(tv0[ws0[ii]].wait_notif == Some(n) && tv0[ws0[ii]].state == ThreadState::BlockedNotif);
        if k > 0 && i == k - 1 {
            // dws[i] is the predecessor ws0[k-1]; qnext re-threaded to tv0[t].qnext.
            assert(ii == k - 1);
            assert(tv0[t].qnext == (if k + 1 < len { Some(ws0[k + 1]) } else { None::<ObjId> }));
            if i + 1 < dws.len() {
                assert(dws[i + 1] == ws0[k + 1]);   // i+1 == k, k <= k so dws[k] = ws0[k+1]
            } else {
                assert(k + 1 == len);               // i+1 == k == dws.len() == len-1
            }
        } else {
            // tvf[dws[i]] == tv0[dws[i]] (not `t`, not the predecessor).
            assert(tvf[ws0[ii]] == tv0[ws0[ii]]);
            if i + 1 < dws.len() {
                let i1 = if i + 1 < k { i + 1 } else { i + 2 };
                assert(dws[i + 1] == ws0[i1]);      // and i1 == ii + 1
            }
        }
    }

    // Clauses 3, 4: head / tail of `dws`.
    if dws.len() == 0 {
        // len == 1 ⇒ k == 0: head == t's (None) qnext; tail == None (k==len-1==0).
        assert(tv0[ws0[0]].qnext is None);
        assert(nvf[n].wait_head is None);
        assert(nvf[n].wait_tail is None);
    } else {
        if k == 0 {
            assert(dws[0] == ws0[1]);
            assert(tv0[ws0[0]].qnext == Some(ws0[1]));
            assert(nvf[n].wait_head == Some(dws[0]));
        } else {
            assert(dws[0] == ws0[0]);
            assert(nv0[n].wait_head == Some(ws0[0]));
            assert(nvf[n].wait_head == Some(dws[0]));
        }
        let last = dws.len() - 1;
        if k == len - 1 {
            assert(k > 0);                          // dws nonempty ⇒ len>=2 ⇒ k>=1
            assert(last == k - 1);
            assert(dws[last] == ws0[k - 1]);
            assert(nvf[n].wait_tail == Some(dws[last]));
        } else {
            assert(nv0[n].wait_tail == Some(ws0[len - 1]));
            assert(dws[last] == ws0[len - 1]);      // last >= k ⇒ dws[last] = ws0[last+1]
            assert(nvf[n].wait_tail == Some(dws[last]));
        }
    }
}

// ── armed-timer list model (plan §4e) ───────────────────────────────────────
// The armed-timer list is a GLOBAL singly-linked intrusive list: the head is the
// `timer_head_view` scalar (the kernel static, store.rs:130), threaded through each
// `TimerView`'s `next`. Unlike the notification waiter queue it has NO tail pointer
// and is not per-object — it is the one list of every armed timer. The model mirrors
// `waiter_chain`/`notif_wf`/`waiter_seq` (head-only, so lighter — no tail-fixup, no
// per-object key): `arm` prepends the head, `disarm` splices out one element,
// `check_expired` walks it. Acyclicity is `valid_list_rank` over the `next` projection,
// implied by `timer_chain`'s `no_duplicates` (rank = position) exactly as for `qnext`.

// `ts` is the armed-timer list in head-to-tail order. Pins the imperative
// `timer_head_view`/`timer_next` links to the `Seq`: distinct elements (acyclicity —
// the index IS the rank), the head agrees with `ts[0]`, `next` threads each element to
// the next (the last to `None`), and every charted node is `armed` with a bound
// notification (`arm` sets `armed`+`notif` together, `disarm` clears both — so an armed
// timer always names the notification its `+1` ref is held on).
pub open spec fn timer_chain(
    tmv: Map<ObjId, TimerView>,
    head: Option<ObjId>,
    ts: Seq<ObjId>,
) -> bool {
    &&& ts.no_duplicates()
    &&& forall|i: int| #![trigger ts[i]] 0 <= i < ts.len() ==> tmv.dom().contains(ts[i])
    &&& (ts.len() == 0 ==> head is None)
    &&& (ts.len() > 0 ==> head == Some(ts[0]))
    &&& forall|i: int| #![trigger ts[i]] 0 <= i < ts.len() ==>
            tmv[ts[i]].next == (if i + 1 < ts.len() { Some(ts[i + 1]) } else { None })
    &&& forall|i: int| #![trigger ts[i]] 0 <= i < ts.len() ==>
            tmv[ts[i]].armed && tmv[ts[i]].notif is Some
}

// Every armed timer is charted on `ts` (the completeness clause — armed ⇒ on the list).
// This is what makes `disarm`'s walk guaranteed to find an armed `t` (unlike
// `remove_waiter`, which tolerates an absent waiter): `timer_wf` carries it, `arm`/
// `disarm` preserve it, the trusted kernel shell establishes it at boot.
pub open spec fn timer_complete(tmv: Map<ObjId, TimerView>, ts: Seq<ObjId>) -> bool {
    forall|k: ObjId| #[trigger] tmv.dom().contains(k) && tmv[k].armed ==> ts.contains(k)
}

// The armed-timer list is well-formed: a chain witness exists that captures every armed
// timer. The `notif_wf` analog (global rather than per-object; no empty head/tail
// agreement clause since there is no tail pointer).
pub open spec fn timer_wf(tmv: Map<ObjId, TimerView>, head: Option<ObjId>) -> bool {
    exists|ts: Seq<ObjId>| #[trigger] timer_chain(tmv, head, ts) && timer_complete(tmv, ts)
}

// The insertion-order armed `Seq` — well-defined when `timer_wf` holds (the chain is
// unique given the `next` threading, `lemma_timer_chain_unique`). `arm` ⇒ prepend,
// `disarm` ⇒ a splice (`Seq::remove`).
pub open spec fn timer_seq(tmv: Map<ObjId, TimerView>, head: Option<ObjId>) -> Seq<ObjId> {
    choose|ts: Seq<ObjId>| timer_chain(tmv, head, ts) && timer_complete(tmv, ts)
}

// `ts1[k] == ts2[k]` for any in-bounds `k`: heads agree, then `next` threads each step
// (the `lemma_chain_eq_at` analog over the armed list).
proof fn lemma_tchain_eq_at(
    tmv: Map<ObjId, TimerView>,
    head: Option<ObjId>,
    ts1: Seq<ObjId>,
    ts2: Seq<ObjId>,
    k: int,
)
    requires
        timer_chain(tmv, head, ts1),
        timer_chain(tmv, head, ts2),
        0 <= k < ts1.len(),
        k < ts2.len(),
    ensures
        ts1[k] == ts2[k],
    decreases k,
{
    if k == 0 {
        assert(head == Some(ts1[0]));
        assert(head == Some(ts2[0]));
    } else {
        lemma_tchain_eq_at(tmv, head, ts1, ts2, k - 1);
        assert(tmv[ts1[k - 1]].next == Some(ts1[k]));
        assert(tmv[ts2[k - 1]].next == Some(ts2[k]));
    }
}

// No armed chain is a strict prefix of another (the `lemma_chain_not_strict_prefix`
// analog): the shorter chain's last node ends the walk (`next == None`) but the longer
// threads it onward.
proof fn lemma_tchain_not_strict_prefix(
    tmv: Map<ObjId, TimerView>,
    head: Option<ObjId>,
    ts1: Seq<ObjId>,
    ts2: Seq<ObjId>,
)
    requires
        timer_chain(tmv, head, ts1),
        timer_chain(tmv, head, ts2),
        ts1.len() < ts2.len(),
    ensures
        false,
{
    if ts1.len() == 0 {
        assert(head is None);
        assert(head == Some(ts2[0]));
    } else {
        let k: int = ts1.len() as int - 1;
        lemma_tchain_eq_at(tmv, head, ts1, ts2, k);
        assert(tmv[ts1[k]].next is None);
        assert(tmv[ts2[k]].next == Some(ts2[k + 1]));
    }
}

// The armed-chain uniqueness theorem (the `lemma_waiter_chain_unique` analog) — so the
// `choose` in `timer_seq` is the insertion order, letting `disarm` state its effect as a
// `timer_seq` equality rather than mere existence.
pub proof fn lemma_timer_chain_unique(
    tmv: Map<ObjId, TimerView>,
    head: Option<ObjId>,
    ts1: Seq<ObjId>,
    ts2: Seq<ObjId>,
)
    requires
        timer_chain(tmv, head, ts1),
        timer_chain(tmv, head, ts2),
    ensures
        ts1 == ts2,
{
    if ts1.len() < ts2.len() {
        lemma_tchain_not_strict_prefix(tmv, head, ts1, ts2);
    }
    if ts2.len() < ts1.len() {
        lemma_tchain_not_strict_prefix(tmv, head, ts2, ts1);
    }
    assert forall|i: int| 0 <= i < ts1.len() implies ts1[i] == ts2[i] by {
        lemma_tchain_eq_at(tmv, head, ts1, ts2, i);
    }
    assert(ts1 =~= ts2);
}

// `disarm`'s splice step (plan §4e): unlinking `t == ts0[k]` from the armed list yields
// `ts0.remove(k)`. The `lemma_remove_chain` analog minus the tail fixup (no tail pointer)
// — the head re-pointed past `t` when `t` was the head (`k == 0`), the predecessor's
// `next` re-threaded past `t` otherwise (`k > 0`), and `t` itself dropped from the chain
// (its own post-state fields are irrelevant — it is no longer charted).
pub proof fn lemma_timer_remove_chain(
    tmv0: Map<ObjId, TimerView>,
    head0: Option<ObjId>,
    tmvf: Map<ObjId, TimerView>,
    headf: Option<ObjId>,
    t: ObjId,
    ts0: Seq<ObjId>,
    k: int,
)
    requires
        timer_chain(tmv0, head0, ts0),
        tmvf.dom() == tmv0.dom(),
        0 <= k < ts0.len(),
        ts0[k] == t,
        // predecessor re-threaded past `t` (k>0), its armed/notif framed.
        k > 0 ==> tmvf[ts0[k - 1]].next == tmv0[t].next,
        k > 0 ==> tmvf[ts0[k - 1]].armed == tmv0[ts0[k - 1]].armed,
        k > 0 ==> tmvf[ts0[k - 1]].notif == tmv0[ts0[k - 1]].notif,
        // every node other than `t` and the predecessor unchanged.
        forall|j: ObjId| #![trigger tmvf[j]]
            j != t && (k == 0 || j != ts0[k - 1]) ==> tmvf[j] == tmv0[j],
        // head fix: k==0 ⇒ new head is `t`'s old next; else unchanged.
        k == 0 ==> headf == tmv0[t].next,
        k > 0 ==> headf == head0,
    ensures
        timer_chain(tmvf, headf, ts0.remove(k)),
{
    let dts = ts0.remove(k);
    let len = ts0.len() as int;
    ts0.remove_ensures(k);
    // dts.len() == len - 1; dts[i] == ts0[i] for i<k, ts0[i+1] for k<=i<len-1.

    assert(dts.no_duplicates()) by {
        assert forall|i: int, j: int|
            0 <= i < dts.len() && 0 <= j < dts.len() && i != j implies dts[i] != dts[j] by {
            let ii = if i < k { i } else { i + 1 };
            let jj = if j < k { j } else { j + 1 };
            assert(dts[i] == ts0[ii] && dts[j] == ts0[jj]);
            assert(ii != jj);
        }
    }

    // Per-node: domain, next-threading, armed+notif.
    assert forall|i: int| #![trigger dts[i]] 0 <= i < dts.len() implies
        tmvf.dom().contains(dts[i])
        && tmvf[dts[i]].next == (if i + 1 < dts.len() { Some(dts[i + 1]) } else { None::<ObjId> })
        && tmvf[dts[i]].armed && tmvf[dts[i]].notif is Some by {
        let ii = if i < k { i } else { i + 1 };
        assert(dts[i] == ts0[ii]);
        assert(tmv0.dom().contains(ts0[ii]));
        assert(tmv0[ts0[ii]].next == (if ii + 1 < len { Some(ts0[ii + 1]) } else { None::<ObjId> }));
        assert(tmv0[ts0[ii]].armed && tmv0[ts0[ii]].notif is Some);
        if k > 0 && i == k - 1 {
            // dts[i] is the predecessor ts0[k-1]; next re-threaded to tmv0[t].next.
            assert(ii == k - 1);
            assert(tmv0[t].next == (if k + 1 < len { Some(ts0[k + 1]) } else { None::<ObjId> }));
            if i + 1 < dts.len() {
                assert(dts[i + 1] == ts0[k + 1]);
            } else {
                assert(k + 1 == len);
            }
        } else {
            assert(tmvf[ts0[ii]] == tmv0[ts0[ii]]);
            if i + 1 < dts.len() {
                let i1 = if i + 1 < k { i + 1 } else { i + 2 };
                assert(dts[i + 1] == ts0[i1]);
            }
        }
    }

    // Head of `dts`.
    if dts.len() == 0 {
        assert(k == 0 && len == 1);
        assert(tmv0[ts0[0]].next is None);
        assert(headf is None);
    } else {
        if k == 0 {
            assert(dts[0] == ts0[1]);
            assert(tmv0[ts0[0]].next == Some(ts0[1]));
            assert(headf == Some(dts[0]));
        } else {
            assert(dts[0] == ts0[0]);
            assert(head0 == Some(ts0[0]));
            assert(headf == Some(dts[0]));
        }
    }
}

// `[t] ++ ts0` is duplicate-free when `ts0` is and `t ∉ ts0`. Isolated into its own
// query: `Seq::no_duplicates`'s `self[i] != self[j]` is an n² trigger, so leaving it in
// `lemma_timer_push_head_chain`'s body (alongside the threading index terms) exploded the
// rlimit — here the only `Seq`-index terms in scope are `pts`/`ts0`'s.
proof fn lemma_push_head_nodup(ts0: Seq<ObjId>, t: ObjId, pts: Seq<ObjId>)
    requires
        ts0.no_duplicates(),
        !ts0.contains(t),
        pts.len() == ts0.len() + 1,
        pts[0] == t,
        forall|i: int| #![trigger pts[i]] 1 <= i < pts.len() ==> pts[i] == ts0[i - 1],
    ensures
        pts.no_duplicates(),
{
    assert forall|i: int, j: int|
        0 <= i < pts.len() && 0 <= j < pts.len() && i != j implies pts[i] != pts[j] by {
        if i >= 1 && j >= 1 {
            assert(pts[i] == ts0[i - 1] && pts[j] == ts0[j - 1]);
        } else if i == 0 {
            assert(pts[j] == ts0[j - 1] && ts0.contains(ts0[j - 1]));
        } else {
            assert(pts[i] == ts0[i - 1] && ts0.contains(ts0[i - 1]));
        }
    }
}

// `arm`'s prepend step (plan §4e): pushing the freshly-armed `t` onto the head yields
// `pts` (the head-push of `ts0`, i.e. `[t] ++ ts0`). `ts0` is the post-`disarm` chain —
// `t` is not on it (it was just unarmed), and `arm` touches only `t`'s fields and the
// head scalar, so every prior node is intact. The lighter analog of `wait`'s tail-push.
pub proof fn lemma_timer_push_head_chain(
    tmv0: Map<ObjId, TimerView>,
    head0: Option<ObjId>,
    tmvf: Map<ObjId, TimerView>,
    headf: Option<ObjId>,
    t: ObjId,
    ts0: Seq<ObjId>,
    pts: Seq<ObjId>,
)
    requires
        timer_chain(tmv0, head0, ts0),
        !ts0.contains(t),
        tmv0.dom().contains(t),
        // `arm` touches only `t`'s timer fields (each `set_timer_*` inserts at key `t`),
        // so the post-state differs from the post-`disarm` map at `t` alone — a single
        // `insert` rather than a broad `forall` frame (the broad trigger blew the rlimit).
        tmvf == tmv0.insert(t, tmvf[t]),
        tmvf[t].next == head0,
        tmvf[t].armed && tmvf[t].notif is Some,
        headf == Some(t),
        // `pts == [t] ++ ts0`.
        pts.len() == ts0.len() + 1,
        pts[0] == t,
        forall|i: int| #![trigger pts[i]] 1 <= i < pts.len() ==> pts[i] == ts0[i - 1],
    ensures
        timer_chain(tmvf, headf, pts),
{
    lemma_push_head_nodup(ts0, t, pts);
    // Domain.
    assert forall|i: int| #![trigger pts[i]] 0 <= i < pts.len() implies
        tmvf.dom().contains(pts[i]) by {
        if i == 0 { assert(pts[0] == t); } else { assert(pts[i] == ts0[i - 1]); }
    }
    // Armed + bound notification.
    assert forall|i: int| #![trigger pts[i]] 0 <= i < pts.len() implies
        tmvf[pts[i]].armed && tmvf[pts[i]].notif is Some by {
        if i == 0 {
            assert(pts[0] == t);
        } else {
            assert(pts[i] == ts0[i - 1] && ts0[i - 1] != t);
            assert(tmvf[ts0[i - 1]] == tmv0[ts0[i - 1]]);
        }
    }
    // `next`-threading.
    assert forall|i: int| #![trigger pts[i]] 0 <= i < pts.len() implies
        tmvf[pts[i]].next == (if i + 1 < pts.len() { Some(pts[i + 1]) } else { None::<ObjId> }) by {
        if i == 0 {
            assert(tmvf[t].next == head0);
            if 1 < pts.len() {
                assert(pts[1] == ts0[0]);
                assert(head0 == Some(ts0[0]));
            } else {
                assert(ts0.len() == 0 && head0 is None);
            }
        } else {
            assert(pts[i] == ts0[i - 1] && ts0[i - 1] != t);
            assert(tmvf[ts0[i - 1]] == tmv0[ts0[i - 1]]);
            assert(tmv0[ts0[i - 1]].next
                == (if i < ts0.len() { Some(ts0[i]) } else { None::<ObjId> }));
            if i + 1 < pts.len() { assert(pts[i + 1] == ts0[i]); } else { assert(i == ts0.len()); }
        }
    }
}

// An element of a duplicate-free `Seq` other than the one at index `k` survives a
// `remove(k)`. Used to re-establish `timer_complete` after `disarm`'s splice: every
// still-armed timer was charted on `ts0` and is not the removed `t == ts0[k]`, so it
// is still charted on `ts0.remove(k)`.
pub proof fn lemma_seq_remove_keeps(ts0: Seq<ObjId>, k: int, j: ObjId)
    requires
        0 <= k < ts0.len(),
        ts0.no_duplicates(),
        ts0.contains(j),
        j != ts0[k],
    ensures
        ts0.remove(k).contains(j),
{
    let m = ts0.index_of(j);
    ts0.remove_ensures(k);
    if m < k {
        assert(ts0.remove(k)[m] == ts0[m]);
    } else {
        assert(ts0.remove(k)[m - 1] == ts0[m]);
    }
}

// Per-armed-timer signal-precondition supply (plan §4e, the census fragment): an armed
// timer's bound notification is live and well-formed, holds the timer's own ref
// (`refs >= 1`), and — when it has a blocked waiter — the waiter's ref too (`refs >= 2`),
// so after `disarm` releases the timer's `-1` the waiter's survives and `signal`'s
// wake-release precondition (`wait_head is Some ⇒ refs > 0`) still holds. The armed-timer
// analog of `binding_notif_wf` + `binding_refs_ok`; precondition-only — the `refs`
// fractions are not preservable without the full refcount census (the post-phase-5
// teardown phase, plan §1.4) — but `check_expired` preserves it across a fire because the
// armed notifications are pairwise distinct (`timer_notif_injective`).
pub open spec fn timer_signal_ok_at(
    tmv: Map<ObjId, TimerView>,
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    rv: Map<ObjId, nat>,
    c: ObjId,
) -> bool {
    (tmv.dom().contains(c) && tmv[c].armed && tmv[c].notif is Some) ==> {
        let n = tmv[c].notif->Some_0;
        &&& nv.dom().contains(n)
        &&& notif_wf(nv, tv, n)
        &&& rv.dom().contains(n)
        &&& rv[n] >= 1
        &&& (nv[n].wait_head is Some ==> rv[n] >= 2)
    }
}

pub open spec fn timer_signal_ok(
    tmv: Map<ObjId, TimerView>,
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    rv: Map<ObjId, nat>,
) -> bool {
    forall|c: ObjId| #[trigger] tmv.dom().contains(c) ==> timer_signal_ok_at(tmv, nv, tv, rv, c)
}

// Armed timers bind pairwise-distinct notifications — what makes `check_expired`'s sweep
// non-interfering: a `disarm`+`signal` on one armed timer's notification leaves every
// other armed timer's notification (and its refs) untouched, so `timer_signal_ok` and
// `timer_wf` survive across the fire. Realistic at MVP scale (one timer per notification);
// the general (shared-notification) case rides forward to the census phase (plan §1.4).
pub open spec fn timer_notif_injective(tmv: Map<ObjId, TimerView>) -> bool {
    forall|c1: ObjId, c2: ObjId| #![trigger tmv[c1], tmv[c2]]
        (tmv.dom().contains(c1) && tmv.dom().contains(c2)
            && tmv[c1].armed && tmv[c2].armed && tmv[c1].notif == tmv[c2].notif) ==> c1 == c2
}

// `s` is one of channel-view `cv`'s ring cap slots. `send`/`recv` require the
// caller's source/destination slots are NOT ring caps of the channel (the
// kernel naturally supplies cspace residents), so moving them disturbs no other
// queued message. Stated as an existential; its negation is the universal that
// auto-instantiates on a `ring_cap[(r,i,c)]` term.
pub open spec fn is_ring_cap_of(cv: ChanView, s: SlotId) -> bool {
    exists|r: int, i: int, c: int| #![trigger cv.ring_cap[(r, i, c)]]
        0 <= r < 2 && 0 <= i < cv.depth && 0 <= c < 4 && cv.ring_cap[(r, i, c)] == s
}

// Modular helpers (doc 25 §2: quarantine `%` reasoning in tiny lemmas so the big
// send/recv case analyses stay first-order). `lemma_window_index_distinct`: the
// new tail offset `b = count` (< depth) lands on a different ring index than any
// in-window offset `a < count` — the fact that lets a `send` leave every prior
// in-window message untouched. `lemma_mod_shift_head`: after `recv` advances
// `head' = (head+1) % depth`, the after-window offset `j` reads the old index
// `j+1` — the fact behind the `drop_first` pop.
pub proof fn lemma_window_index_distinct(head: int, depth: int, a: int, b: int)
    requires
        depth > 0,
        0 <= a,
        a < b,
        b < depth,
    ensures
        (head + a) % depth != (head + b) % depth,
{
    if (head + a) % depth == (head + b) % depth {
        // ((head+b)%depth - (head+a)%depth) % depth == ((head+b)-(head+a)) % depth;
        // the LHS inner difference is 0 (if-hyp), and 0 % depth == 0, so
        // (b - a) % depth == 0 — but 0 < b - a < depth forces b - a >= depth.
        vstd::arithmetic::div_mod::lemma_sub_mod_noop(head + b, head + a, depth);
        vstd::arithmetic::div_mod::lemma_small_mod(0nat, depth as nat);
        assert((b - a) % depth == 0);
        vstd::arithmetic::div_mod::lemma_mod_is_zero((b - a) as nat, depth as nat);
        assert(false);
    }
}

pub proof fn lemma_mod_shift_head(head: int, depth: int, j: int)
    requires
        depth > 0,
    ensures
        ((head + 1) % depth + j) % depth == (head + 1 + j) % depth,
{
    vstd::arithmetic::div_mod::lemma_add_mod_noop_right(j, head + 1, depth);
}

// A value already in `[0, depth)` is its own residue — used to identify the head
// index `(head + 0) % depth == head` in `recv`'s pop proof.
pub proof fn lemma_self_mod(x: int, depth: int)
    requires
        0 <= x < depth,
    ensures
        x % depth == x,
{
    vstd::arithmetic::div_mod::lemma_small_mod(x as nat, depth as nat);
}

// Two ring messages are equal when their length and their four cap *contents*
// agree — the per-message congruence the FIFO `Seq`-extensionality steps in
// `send`/`recv` lean on (a message unchanged by a move stays put in the queue).
pub proof fn lemma_ring_msg_eq(
    cva: ChanView, sva: Map<SlotId, CapSlot>,
    cvb: ChanView, svb: Map<SlotId, CapSlot>,
    ring: int, idx: int,
)
    requires
        cva.msg_len[(ring, idx)] == cvb.msg_len[(ring, idx)],
        forall|c: int| 0 <= c < 4 ==>
            sva[#[trigger] cva.ring_cap[(ring, idx, c)]].cap == svb[cvb.ring_cap[(ring, idx, c)]].cap,
    ensures
        ring_msg(cva, sva, ring, idx) == ring_msg(cvb, svb, ring, idx),
{
    assert(ring_msg(cva, sva, ring, idx).1 =~= ring_msg(cvb, svb, ring, idx).1);
}

// The count of live (non-empty) slots — the well-founded measure for revoke's
// outer loop: each leaf delete strictly lowers it.
pub open spec fn count_nonempty(m: Map<SlotId, CapSlot>) -> nat {
    m.dom().filter(|k: SlotId| !is_empty_cap(m[k].cap)).len()
}

// ── Refcount census: the stored refcount equals the count of designating slots
//    (cspace residents; channel-queue and TCB-bind homes ride the same arena),
//    plus the non-slot references (bindings/waiters/armed timers) the later
//    phases add. For phase 2 the census is the slot count; the cross-home and
//    non-slot terms land with channel/notification/thread (plan §4.3–§4.4). ──

pub open spec fn slot_refs(m: Map<SlotId, CapSlot>, obj: ObjId) -> nat {
    m.dom().filter(|k: SlotId| cap_obj(m[k].cap) == Some(obj)).len()
}

// Two census lemmas (the spec basis for refcount soundness — the stored
// refcount must move in lockstep with the count of designating slots):

// Link-only edits (same domain, same caps) leave the census untouched.
proof fn lemma_same_caps_same_census(
    m1: Map<SlotId, CapSlot>,
    m2: Map<SlotId, CapSlot>,
    obj: ObjId,
)
    requires
        m1.dom() == m2.dom(),
        forall|k: SlotId| #[trigger] m1.dom().contains(k) ==> m1[k].cap == m2[k].cap,
    ensures
        slot_refs(m1, obj) == slot_refs(m2, obj),
{
    let s1 = m1.dom().filter(|k: SlotId| cap_obj(m1[k].cap) == Some(obj));
    let s2 = m2.dom().filter(|k: SlotId| cap_obj(m2[k].cap) == Some(obj));
    assert forall|k: SlotId| s1.contains(k) <==> s2.contains(k) by {
        if m1.dom().contains(k) {
            assert(m1[k].cap == m2[k].cap);
        }
    }
    assert(s1 =~= s2);
    assert(slot_refs(m1, obj) == s1.len());
    assert(slot_refs(m2, obj) == s2.len());
}

// Re-pointing one slot from designating nothing-of-`obj` to designating `obj`
// raises `obj`'s census by exactly one.
proof fn lemma_designation_bump(
    m: Map<SlotId, CapSlot>,
    k: SlotId,
    v: CapSlot,
    obj: ObjId,
)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_obj(m[k].cap) != Some(obj),
        cap_obj(v.cap) == Some(obj),
    ensures
        slot_refs(m.insert(k, v), obj) == slot_refs(m, obj) + 1,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| cap_obj(m[j].cap) == Some(obj));
    let f2 = m2.dom().filter(|j: SlotId| cap_obj(m2[j].cap) == Some(obj));
    assert(m2.dom() =~= m.dom());
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.insert(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.insert(k));
    assert(!f1.contains(k));
    assert(f1.finite());
    assert(slot_refs(m2, obj) == f2.len());
    assert(slot_refs(m, obj) == f1.len());
}

// ── The full refcount census (plan §6a). `refs[o]` must equal `obj_census(o)` —
//    the recount over *every* reference to `o`: slot designations (`slot_refs`)
//    plus the five non-slot terms phases 3/4/5 landed as per-op deltas, here
//    assembled. Each term is a `slot_refs`-style filter/length (or a `waiter_seq`
//    length). `refcount_sound` is the system invariant the teardown family (6c/6d)
//    and the ref-touching construction ops (6f) preserve. ──

// The aspace a mapped Frame cap holds a (non-cap) reference on — the mapping's
// target. `None` for an unmapped frame or any non-Frame cap. A mapped frame holds
// its aspace ref through this field, *not* via cap designation (`cap_obj` is `None`
// for a Frame, `obj_ref`/`obj_unref`'s Frame arm a no-op), which is exactly why
// `frame_map_refs` is a census term distinct from `slot_refs`.
pub open spec fn cap_frame_aspace(c: Cap) -> Option<ObjId> {
    match c.kind {
        CapKind::Frame { mapping: Some((a, _)), .. } => Some(a),
        _ => None,
    }
}

// Channel bindings naming `o`: the `(ch, end, ev)` triples whose binding's notif is
// `Some(o)` (end ∈ {0,1}, ev ∈ {0,1,2}). A subset of `cv.dom() × {0,1} × {0,1,2}`,
// finite when `cv.dom()` is. The §3.6 binding term (the `binding_refs_ok` companion).
pub open spec fn binding_refs(cv: Map<ObjId, ChanView>, o: ObjId) -> nat {
    Set::new(
        |t: (ObjId, int, int)|
            cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3
                && cv[t.0].bindings[(t.1, t.2)].notif == Some(o),
    ).len()
}

// Blocked waiters on `o`: the length of `o`'s FIFO waiter chain — each blocked TCB
// holds one queued ref (the phase-4 waiter term, plan §4b/§4c).
pub open spec fn waiter_refs(nv: Map<ObjId, NotifView>, tv: Map<ObjId, TcbView>, o: ObjId) -> nat {
    waiter_seq(nv, tv, o).len()
}

// Armed timers naming `o`: each armed timer bound to `o` holds one queued ref while
// armed (the phase-4e armed-timer term, plan §4e).
pub open spec fn armed_timer_refs(tmv: Map<ObjId, TimerView>, o: ObjId) -> nat {
    tmv.dom().filter(|k: ObjId| tmv[k].armed && tmv[k].notif == Some(o)).len()
}

// Frame mappings naming `o`: each mapped frame cap holds one ref on its target
// aspace via the mapping field (the new phase-5-enabled aspace term, plan §6a).
pub open spec fn frame_map_refs(sv: Map<SlotId, CapSlot>, o: ObjId) -> nat {
    sv.dom().filter(|k: SlotId| cap_frame_aspace(sv[k].cap) == Some(o)).len()
}

// Thread holds on `o`: a bound thread holds one ref on its cspace and one on its
// aspace — released by `destroy_tcb`'s `unref_cspace`/`unref_aspace` (plan §6a).
pub open spec fn thread_hold_refs(tv: Map<ObjId, TcbView>, o: ObjId) -> nat {
    tv.dom().filter(|k: ObjId| tv[k].cspace == Some(o)).len()
        + tv.dom().filter(|k: ObjId| tv[k].aspace == Some(o)).len()
}

// The recount: `refs[o]` must equal this over the whole store.
pub open spec fn obj_census<S: Store>(store: &S, o: ObjId) -> nat {
    slot_refs(store.slot_view(), o) + binding_refs(store.chan_view(), o) + waiter_refs(
        store.notif_view(),
        store.tcb_view(),
        o,
    ) + armed_timer_refs(store.timer_view(), o) + frame_map_refs(store.slot_view(), o)
        + thread_hold_refs(store.tcb_view(), o)
}

// Every live object's stored refcount equals its census. The teardown family
// assumes this at entry and re-establishes it at exit (the §4.1 obligation).
pub open spec fn refcount_sound<S: Store>(store: &S) -> bool {
    forall|o: ObjId|
        store.refs_view().dom().contains(o) ==> store.refs_view()[o] == #[trigger] obj_census(
            store,
            o,
        )
}

// Cspace residency well-formedness (plan §6c): `cs` is a known cspace, its residency
// `Seq` agrees with `num_slots` (the getter contracts' precondition), and every resident
// slot handle is live in the arena. `destroy_cspace`'s loop reads `cspace_slot(cs, i)`
// and then `slot(sid)`, so it needs both the getter bounds and the residents-live fact;
// `obj_unref`/`unref_cspace` thread it to that loop. The kernel maintains it by
// construction (residency is fixed when the cspace is carved, §3.2).
pub open spec fn cspace_resident_wf<S: Store>(store: &S, cs: ObjId) -> bool {
    &&& store.cspace_view().dom().contains(cs)
    &&& store.cspace_view()[cs].slots.len() == store.cspace_view()[cs].num_slots
    &&& forall|i: int| 0 <= i < store.cspace_view()[cs].slots.len()
            ==> #[trigger] store.slot_view().dom().contains(store.cspace_view()[cs].slots[i])
}

// ── Cap→object consistency (plan §6d foundation, `doc/results/44`). The teardown
//    *body* proofs (the follow-on §6d PR) cannot run from `cspace_wf` + `refcount_sound`
//    alone: `delete`'s body calls `endpoint_cap_dropped` (Channel branch) and `obj_unref`,
//    both of which demand the *designated object's* well-formedness — `chan_wf`/`notif_wf`/
//    `cspace_resident_wf`/the tcb-bind facts/`timer_wf` — none of which `cspace_wf` carries.
//    Because the teardown recursion deletes *arbitrary-kind* caps (`destroy_cspace` over
//    residents, `revoke` over descendants, `destroy_channel` over ring caps), each caller
//    needs that wf for caps it doesn't statically know — so it must be a *system* invariant
//    over every live cap, not a per-call precondition. This foundation states it; the body
//    PR consumes it. Preservation across teardown rests on `refcount_sound`: a last-ref
//    destroy leaves no cap designating the freed object, so no surviving cap's consistency
//    can depend on it (the refs-coupled clauses below — the Channel `end_caps`/`binding_refs_ok`
//    and the Timer armed-notif-live — are exactly that entanglement). ──

// One cap's designated-object consistency, kind by kind. The clauses mirror `obj_unref`'s
// per-`CapKind` `requires` (so the body proof maps `caps_consistent` to it mechanically),
// plus the structural Channel facts `delete`'s `endpoint_cap_dropped` call needs beyond
// `chan_wf` (the peer-closed end's live end-cap count + `binding_notif_wf`).
//
// **`caps_consistent` is deliberately refs-free** — every clause reads only object views
// (slot/chan/notif/tcb/timer/cspace), never `refs_view`. That is what makes it a clean
// *structural* invariant the `dec_ref` `-1` preserves by framing alone (no census/finiteness
// gymnastics in this foundation). The refs-coupled object-wf facts the body PR also needs
// — `endpoint_cap_dropped`'s `binding_refs_ok` and `obj_unref`'s Timer armed-notif-live —
// are **not** carried here: each is "a reference to `n` ⟹ `refs[n] > 0`", which is exactly
// a `refcount_sound` consequence (the reference makes `census(n) ≥ 1`), so the body PR
// derives them at the call site where `refs` is in scope, rather than threading them
// through teardown (which would couple this invariant to the census and re-introduce the
// nested-`(ch,e,v)` / armed-timer finiteness the recount lemmas were quarantined for).
pub open spec fn cap_consistent<S: Store>(store: &S, c: Cap) -> bool {
    match c.kind {
        CapKind::Channel(o, end) => {
            &&& chan_wf(store.chan_view(), store.slot_view(), o)
            &&& store.chan_view()[o].end_caps[crate::channel::end_idx_spec(end)] > 0
            &&& binding_notif_wf(store.chan_view(), store.notif_view(), store.tcb_view(), o)
        }
        CapKind::CSpace(o) => cspace_resident_wf(store, o),
        CapKind::Thread(o) => {
            &&& store.tcb_view().dom().contains(o)
            &&& store.tcb_view()[o].bind_slots.len() == 2
            &&& store.slot_view().dom().contains(store.tcb_view()[o].bind_slots[0])
            &&& store.slot_view().dom().contains(store.tcb_view()[o].bind_slots[1])
        }
        CapKind::Notification(o) => notif_wf(store.notif_view(), store.tcb_view(), o),
        CapKind::Timer(o) => {
            &&& store.timer_view().dom().contains(o)
            &&& store.timer_view().dom().finite()
            &&& timer_wf(store.timer_view(), store.timer_head_view())
        }
        // Empty / Untyped / Frame / Aspace designate no destructor-bearing object here:
        // `obj_unref` is a no-op (Frame/Untyped/Empty) or the `unref_aspace` leaf (Aspace),
        // neither of which reads an object well-formedness term.
        _ => true,
    }
}

// The system invariant: every live cap's designated object is consistent (and the slot
// arena is finite — the `obj_unref` CSpace/Thread/Timer arms' standing precondition).
pub open spec fn caps_consistent<S: Store>(store: &S) -> bool {
    &&& store.slot_view().dom().finite()
    &&& forall|s: SlotId| #![trigger store.slot_view()[s]]
            store.slot_view().dom().contains(s) && !is_empty_cap(store.slot_view()[s].cap)
            ==> cap_consistent(store, store.slot_view()[s].cap)
}

// ── Per-term recount lemmas (plan §6a). The single-key bump/drop building blocks
//    6b–6f compose: a one-key view edit raises/lowers exactly one census term by
//    one, the others fixed. Each is the `lemma_designation_bump` shape over a
//    different view (`slot_refs` already has its bump above); the drops are its
//    `remove`-mirror, and the thread-hold pair frames the untouched half at `k`.
//    The five single-domain terms (slot, frame-mapping, armed-timer, thread-hold
//    ×2) are settled here. The sixth, `binding_refs`, counts over a *nested*
//    domain (`(ch, end, ev)` triples), so its single-edit recount needs the triple
//    set's finiteness (a subset of `cv.dom() × {0,1} × {0,1,2}`) — the doc-35 §2.6
//    trigger hazard the plan flags (§3). It lands with `destroy_channel`'s binding
//    release, the op that consumes it (6d), per the "count steps single-purpose,
//    where consumed" discipline — recorded, not dropped. ──

// Slot drop: clearing one slot's designation of `obj` lowers `obj`'s slot census.
proof fn lemma_designation_drop(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, obj: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_obj(m[k].cap) == Some(obj),
        cap_obj(v.cap) != Some(obj),
    ensures
        slot_refs(m.insert(k, v), obj) == (slot_refs(m, obj) - 1) as nat,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| cap_obj(m[j].cap) == Some(obj));
    let f2 = m2.dom().filter(|j: SlotId| cap_obj(m2[j].cap) == Some(obj));
    assert(m2.dom() =~= m.dom());
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.remove(k));
    assert(f1.contains(k));
    assert(f1.finite());
}

// Frame-mapping bump: a slot newly designating `o` as its frame's target aspace
// raises `o`'s frame-mapping census.
proof fn lemma_frame_map_bump(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_frame_aspace(m[k].cap) != Some(o),
        cap_frame_aspace(v.cap) == Some(o),
    ensures
        frame_map_refs(m.insert(k, v), o) == frame_map_refs(m, o) + 1,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| cap_frame_aspace(m[j].cap) == Some(o));
    let f2 = m2.dom().filter(|j: SlotId| cap_frame_aspace(m2[j].cap) == Some(o));
    assert(m2.dom() =~= m.dom());
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.insert(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.insert(k));
    assert(!f1.contains(k));
    assert(f1.finite());
}

// Frame-mapping drop: clearing/retargeting a frame's mapping away from `o` lowers
// `o`'s frame-mapping census (the term `delete`'s frame-unmap branch lowers, 6b).
proof fn lemma_frame_map_drop(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_frame_aspace(m[k].cap) == Some(o),
        cap_frame_aspace(v.cap) != Some(o),
    ensures
        frame_map_refs(m.insert(k, v), o) == (frame_map_refs(m, o) - 1) as nat,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| cap_frame_aspace(m[j].cap) == Some(o));
    let f2 = m2.dom().filter(|j: SlotId| cap_frame_aspace(m2[j].cap) == Some(o));
    assert(m2.dom() =~= m.dom());
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.remove(k));
    assert(f1.contains(k));
    assert(f1.finite());
}

// Armed-timer bump: a timer newly armed-and-bound to `o` raises `o`'s armed-timer
// census.
proof fn lemma_armed_timer_bump(m: Map<ObjId, TimerView>, k: ObjId, v: TimerView, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        !(m[k].armed && m[k].notif == Some(o)),
        v.armed && v.notif == Some(o),
    ensures
        armed_timer_refs(m.insert(k, v), o) == armed_timer_refs(m, o) + 1,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: ObjId| m[j].armed && m[j].notif == Some(o));
    let f2 = m2.dom().filter(|j: ObjId| m2[j].armed && m2[j].notif == Some(o));
    assert(m2.dom() =~= m.dom());
    assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.insert(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.insert(k));
    assert(!f1.contains(k));
    assert(f1.finite());
}

// Armed-timer drop: disarming/retargeting a timer away from `o` lowers `o`'s
// armed-timer census (the term `disarm`/`destroy_timer` lowers; teardown, 6d).
proof fn lemma_armed_timer_drop(m: Map<ObjId, TimerView>, k: ObjId, v: TimerView, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        m[k].armed && m[k].notif == Some(o),
        !(v.armed && v.notif == Some(o)),
    ensures
        armed_timer_refs(m.insert(k, v), o) == (armed_timer_refs(m, o) - 1) as nat,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: ObjId| m[j].armed && m[j].notif == Some(o));
    let f2 = m2.dom().filter(|j: ObjId| m2[j].armed && m2[j].notif == Some(o));
    assert(m2.dom() =~= m.dom());
    assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.remove(k));
    assert(f1.contains(k));
    assert(f1.finite());
}

// Armed-timer drop, **disarm-shaped** (plan §6c). `disarm` (`timer.rs`) edits *two*
// keys — it disarms `t` *and* re-points the predecessor's `next` to splice `t` out —
// so the post-state is not a single-key `insert` and `lemma_armed_timer_drop` does not
// apply directly. But `armed_timer_refs` reads only `armed`/`notif`, and those are
// exactly `disarm`'s frame (every `j != t` keeps both; `t` is disarmed), so the census
// delta is still ±1 at `t`'s notification only. This is the lemma `destroy_timer`'s
// `refcount_sound`-preservation (6c) consumes — `pub` so `crate::timer` can name it.
pub proof fn lemma_armed_timer_disarm(
    pre: Map<ObjId, TimerView>,
    post: Map<ObjId, TimerView>,
    t: ObjId,
    o: ObjId,
)
    requires
        pre.dom().finite(),
        post.dom() == pre.dom(),
        pre.dom().contains(t),
        !post[t].armed,
        forall|j: ObjId| #![trigger post[j]]
            j != t ==> post[j].armed == pre[j].armed && post[j].notif == pre[j].notif,
    ensures
        // `+1` form (not `(x-1) as nat`) so the consumer's census arithmetic has no
        // saturation ambiguity: the pre-count is provably ≥ 1 here (`t` is in the set).
        (pre[t].armed && pre[t].notif == Some(o)) ==>
            armed_timer_refs(pre, o) == armed_timer_refs(post, o) + 1,
        !(pre[t].armed && pre[t].notif == Some(o)) ==>
            armed_timer_refs(post, o) == armed_timer_refs(pre, o),
{
    let f1 = pre.dom().filter(|j: ObjId| pre[j].armed && pre[j].notif == Some(o));
    let f2 = post.dom().filter(|j: ObjId| post[j].armed && post[j].notif == Some(o));
    assert(!f2.contains(t));
    if pre[t].armed && pre[t].notif == Some(o) {
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(t).contains(j) by {
            if j != t {
                assert(post[j].armed == pre[j].armed && post[j].notif == pre[j].notif);
            }
        }
        assert(f2 =~= f1.remove(t));
        assert(f1.contains(t));
        assert(f1.finite());
    } else {
        assert(!f1.contains(t));
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
            if j != t {
                assert(post[j].armed == pre[j].armed && post[j].notif == pre[j].notif);
            }
        }
        assert(f2 =~= f1);
    }
}

// Thread-hold bump (cspace edit): a thread newly holding `o` as its cspace raises
// `o`'s thread-hold census (the aspace half is framed unchanged at `k`).
proof fn lemma_thread_hold_cspace_bump(m: Map<ObjId, TcbView>, k: ObjId, v: TcbView, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        m[k].cspace != Some(o),
        v.cspace == Some(o),
        v.aspace == m[k].aspace,
    ensures
        thread_hold_refs(m.insert(k, v), o) == thread_hold_refs(m, o) + 1,
{
    let m2 = m.insert(k, v);
    let c1 = m.dom().filter(|j: ObjId| m[j].cspace == Some(o));
    let c2 = m2.dom().filter(|j: ObjId| m2[j].cspace == Some(o));
    let a1 = m.dom().filter(|j: ObjId| m[j].aspace == Some(o));
    let a2 = m2.dom().filter(|j: ObjId| m2[j].aspace == Some(o));
    assert(m2.dom() =~= m.dom());
    assert(m2[k] == v);
    assert forall|j: ObjId| #![trigger c2.contains(j)] c2.contains(j) <==> c1.insert(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(c2 =~= c1.insert(k));
    assert(!c1.contains(k));
    assert(c1.finite());
    assert forall|j: ObjId| #![trigger a2.contains(j)] a2.contains(j) <==> a1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        } else {
            assert(m2[k].aspace == m[k].aspace);
        }
    }
    assert(a2 =~= a1);
}

// Thread-hold drop (cspace edit): clearing a thread's `o` cspace hold lowers the
// census (the term `destroy_tcb`'s `unref_cspace` releases; teardown, 6d).
proof fn lemma_thread_hold_cspace_drop(m: Map<ObjId, TcbView>, k: ObjId, v: TcbView, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        m[k].cspace == Some(o),
        v.cspace != Some(o),
        v.aspace == m[k].aspace,
    ensures
        thread_hold_refs(m.insert(k, v), o) == (thread_hold_refs(m, o) - 1) as nat,
{
    let m2 = m.insert(k, v);
    let c1 = m.dom().filter(|j: ObjId| m[j].cspace == Some(o));
    let c2 = m2.dom().filter(|j: ObjId| m2[j].cspace == Some(o));
    let a1 = m.dom().filter(|j: ObjId| m[j].aspace == Some(o));
    let a2 = m2.dom().filter(|j: ObjId| m2[j].aspace == Some(o));
    assert(m2.dom() =~= m.dom());
    assert(m2[k] == v);
    assert forall|j: ObjId| #![trigger c2.contains(j)] c2.contains(j) <==> c1.remove(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(c2 =~= c1.remove(k));
    assert(c1.contains(k));
    assert(c1.finite());
    assert forall|j: ObjId| #![trigger a2.contains(j)] a2.contains(j) <==> a1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        } else {
            assert(m2[k].aspace == m[k].aspace);
        }
    }
    assert(a2 =~= a1);
}

// Thread-hold bump/drop (aspace edit): the symmetric pair over the aspace half,
// the cspace half framed unchanged at `k`. `destroy_tcb`'s `unref_aspace` releases
// the drop side (6d); the construction side rides 6f.
proof fn lemma_thread_hold_aspace_bump(m: Map<ObjId, TcbView>, k: ObjId, v: TcbView, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        m[k].aspace != Some(o),
        v.aspace == Some(o),
        v.cspace == m[k].cspace,
    ensures
        thread_hold_refs(m.insert(k, v), o) == thread_hold_refs(m, o) + 1,
{
    let m2 = m.insert(k, v);
    let c1 = m.dom().filter(|j: ObjId| m[j].cspace == Some(o));
    let c2 = m2.dom().filter(|j: ObjId| m2[j].cspace == Some(o));
    let a1 = m.dom().filter(|j: ObjId| m[j].aspace == Some(o));
    let a2 = m2.dom().filter(|j: ObjId| m2[j].aspace == Some(o));
    assert(m2.dom() =~= m.dom());
    assert(m2[k] == v);
    assert forall|j: ObjId| #![trigger a2.contains(j)] a2.contains(j) <==> a1.insert(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(a2 =~= a1.insert(k));
    assert(!a1.contains(k));
    assert(a1.finite());
    assert forall|j: ObjId| #![trigger c2.contains(j)] c2.contains(j) <==> c1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        } else {
            assert(m2[k].cspace == m[k].cspace);
        }
    }
    assert(c2 =~= c1);
}

proof fn lemma_thread_hold_aspace_drop(m: Map<ObjId, TcbView>, k: ObjId, v: TcbView, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        m[k].aspace == Some(o),
        v.aspace != Some(o),
        v.cspace == m[k].cspace,
    ensures
        thread_hold_refs(m.insert(k, v), o) == (thread_hold_refs(m, o) - 1) as nat,
{
    let m2 = m.insert(k, v);
    let c1 = m.dom().filter(|j: ObjId| m[j].cspace == Some(o));
    let c2 = m2.dom().filter(|j: ObjId| m2[j].cspace == Some(o));
    let a1 = m.dom().filter(|j: ObjId| m[j].aspace == Some(o));
    let a2 = m2.dom().filter(|j: ObjId| m2[j].aspace == Some(o));
    assert(m2.dom() =~= m.dom());
    assert(m2[k] == v);
    assert forall|j: ObjId| #![trigger a2.contains(j)] a2.contains(j) <==> a1.remove(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(a2 =~= a1.remove(k));
    assert(a1.contains(k));
    assert(a1.finite());
    assert forall|j: ObjId| #![trigger c2.contains(j)] c2.contains(j) <==> c1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        } else {
            assert(m2[k].cspace == m[k].cspace);
        }
    }
    assert(c2 =~= c1);
}

// ── `delete`'s frame-unmap-branch census lemma (plan §6b, doc/results/42). ──
//
// `delete` clears a deleted cap's slot (`cspace.rs`'s `s.cap = EMPTY; set_slot`)
// then, for a mapped Frame, calls `aspace_unmap` + `unref_aspace`. The census side
// of that branch — landed here, consumed by 6d's `delete` body (the op stays
// `external_body` this sub-phase): clearing a mapped Frame slot lowers exactly the
// target aspace's `frame_map_refs` by one and leaves *every* object's `slot_refs`
// (a Frame designates no object) and every *other* aspace's `frame_map_refs` fixed.
// The matching `-1` is `unref_aspace`'s; the four non-slot census terms ride
// `set_slot`'s view-frame at the call site. Two "unchanged" helpers (the
// `lemma_same_caps_same_census` analog for a single *changed* key whose designation
// of `o` is absent on both sides) plus the proven `lemma_frame_map_drop` compose it.

// A single-slot edit whose old and new caps both designate nothing-of-`obj` leaves
// `obj`'s slot census fixed (no finiteness needed — a pure set-extensionality step).
proof fn lemma_nondesignating_edit_slot_refs(
    m: Map<SlotId, CapSlot>,
    k: SlotId,
    v: CapSlot,
    obj: ObjId,
)
    requires
        m.dom().contains(k),
        cap_obj(m[k].cap) != Some(obj),
        cap_obj(v.cap) != Some(obj),
    ensures
        slot_refs(m.insert(k, v), obj) == slot_refs(m, obj),
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| cap_obj(m[j].cap) == Some(obj));
    let f2 = m2.dom().filter(|j: SlotId| cap_obj(m2[j].cap) == Some(obj));
    assert(m2.dom() =~= m.dom());
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        } else {
            assert(m2[k] == v);
        }
    }
    assert(f2 =~= f1);
}

// The frame-mapping mirror: an edit whose old and new caps both target nothing-of-`o`
// leaves `o`'s frame-mapping census fixed.
proof fn lemma_nontargeting_edit_frame_map(
    m: Map<SlotId, CapSlot>,
    k: SlotId,
    v: CapSlot,
    o: ObjId,
)
    requires
        m.dom().contains(k),
        cap_frame_aspace(m[k].cap) != Some(o),
        cap_frame_aspace(v.cap) != Some(o),
    ensures
        frame_map_refs(m.insert(k, v), o) == frame_map_refs(m, o),
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| cap_frame_aspace(m[j].cap) == Some(o));
    let f2 = m2.dom().filter(|j: SlotId| cap_frame_aspace(m2[j].cap) == Some(o));
    assert(m2.dom() =~= m.dom());
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        } else {
            assert(m2[k] == v);
        }
    }
    assert(f2 =~= f1);
}

// The composite branch lemma 6d's `delete` body consumes: replacing a mapped Frame
// slot `k` (target aspace `asp`) with a non-designating, non-targeting cap `v` (an
// empty cap qualifies — `cap_obj`/`cap_frame_aspace` are both `None` for it) drops
// `frame_map_refs(asp)` by one and fixes every other slot-view census term.
proof fn lemma_frame_clear_census(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, asp: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_frame_aspace(m[k].cap) == Some(asp),
        cap_obj(v.cap) is None,
        cap_frame_aspace(v.cap) is None,
    ensures
        frame_map_refs(m.insert(k, v), asp) == (frame_map_refs(m, asp) - 1) as nat,
        forall|o: ObjId| o != asp ==> #[trigger] frame_map_refs(m.insert(k, v), o) == frame_map_refs(m, o),
        forall|o: ObjId| #[trigger] slot_refs(m.insert(k, v), o) == slot_refs(m, o),
{
    // A Frame designates no object, so the old cap at `k` designates nothing.
    assert(cap_obj(m[k].cap) is None);
    lemma_frame_map_drop(m, k, v, asp);
    assert forall|o: ObjId| o != asp implies #[trigger] frame_map_refs(m.insert(k, v), o)
        == frame_map_refs(m, o) by {
        lemma_nontargeting_edit_frame_map(m, k, v, o);
    }
    assert forall|o: ObjId| #[trigger] slot_refs(m.insert(k, v), o) == slot_refs(m, o) by {
        lemma_nondesignating_edit_slot_refs(m, k, v, o);
    }
}

// ── Construction-side acyclicity preservation (doc/results/21 §9). ──
//
// Re-parenting one **detached, childless** slot `child` under `parent` in an
// acyclic store keeps it acyclic — this is the witness *construction* the §9
// "Key design discovery" identified as the blocker (acyclicity is easy to use,
// hard to re-exhibit after a mutation). The witness: shift every old rank up by
// one and seat `child` at the bottom (rank 0). The shift makes room below
// `parent` even when its old rank was 0; bottom-seating `child` is sound because
// **no slot names `child` as parent** — `child` was childless and
// `parent_has_first_child` forbids a resident of a childless node, so nothing
// needs a rank below 0. This is exactly why Stage 1 strengthened `cdt_wf`.
proof fn lemma_reparent_preserves_acyclic(
    m0: Map<SlotId, CapSlot>,
    m1: Map<SlotId, CapSlot>,
    child: SlotId,
    parent: SlotId,
)
    requires
        acyclic(m0),
        parent_has_first_child(m0),
        m1.dom() == m0.dom(),
        m0.dom().contains(child),
        m0.dom().contains(parent),
        parent != child,
        m0[child].first_child is None,
        m1[child].parent == Some(parent),
        forall|k: SlotId| m0.dom().contains(k) && k != child
            ==> #[trigger] m1[k].parent == m0[k].parent,
    ensures
        acyclic(m1),
{
    let r0 = choose|r: Map<SlotId, nat>| valid_prank(m0, r);
    // No slot names `child` as parent: `child` is childless, and
    // parent_has_first_child(m0) maps a resident's parent to a non-childless
    // node — `child` cannot be one.
    assert forall|k: SlotId| #[trigger] m0.dom().contains(k)
        implies m0[k].parent != Some(child) by {
        if m0[k].parent == Some(child) {
            assert(m0[child].first_child is Some);
        }
    }
    let r1 = Map::<SlotId, nat>::new(
        |k: SlotId| m1.dom().contains(k),
        |k: SlotId| if k == child { 0nat } else { (r0[k] + 1) as nat },
    );
    assert(r1.dom() =~= m1.dom());
    assert forall|k: SlotId| #[trigger] m1.dom().contains(k)
        implies (m1[k].parent matches Some(pp) ==> m1.dom().contains(pp) && r1[k] < r1[pp]) by {
        if let Some(pp) = m1[k].parent {
            if k == child {
                // child.parent == Some(parent); parent != child ⟹ r1[parent] ≥ 1 > 0.
                assert(pp == parent);
            } else {
                // m1[k].parent == m0[k].parent; valid_prank(m0,r0) gives pp live and
                // r0[k] < r0[pp], and pp != child (nothing names child), so the
                // shifted ranks keep the strict drop.
                assert(m1[k].parent == m0[k].parent);
                assert(m0[k].parent == Some(pp));
                assert(pp != child);
            }
        }
    }
    assert(valid_prank(m1, r1));
    assert(acyclic(m1));
}

// Sibling analog of the above for the `cdt_insert_child` shape: `child` becomes a
// new list head whose only `next_sib` edge points at `old_first`, and **no slot
// points `next_sib` at `child`** (it was detached, prev None). The witness seats
// `child` one above its successor; every other rank is untouched (`child` is a
// pure next-source, never a target, so nothing below it constrains its rank).
proof fn lemma_insert_preserves_sib_acyclic(
    m0: Map<SlotId, CapSlot>,
    m1: Map<SlotId, CapSlot>,
    child: SlotId,
)
    requires
        sib_acyclic(m0),
        m1.dom() == m0.dom(),
        m0.dom().contains(child),
        m1[child].next_sib matches Some(n) ==> m0.dom().contains(n) && n != child,
        forall|k: SlotId| m0.dom().contains(k) && k != child
            ==> #[trigger] m1[k].next_sib == m0[k].next_sib,
        forall|k: SlotId| m0.dom().contains(k) ==> #[trigger] m1[k].next_sib != Some(child),
    ensures
        sib_acyclic(m1),
{
    let s0 = choose|s: Map<SlotId, nat>| valid_srank(m0, s);
    let s1 = Map::<SlotId, nat>::new(
        |k: SlotId| m1.dom().contains(k),
        |k: SlotId| if k == child {
            match m1[child].next_sib {
                Some(n) => (s0[n] + 1) as nat,
                None => 0nat,
            }
        } else {
            s0[k]
        },
    );
    assert(s1.dom() =~= m1.dom());
    assert forall|k: SlotId| #[trigger] m1.dom().contains(k)
        implies (m1[k].next_sib matches Some(n) ==> m1.dom().contains(n) && s1[n] < s1[k]) by {
        if let Some(n) = m1[k].next_sib {
            // n != child: nothing names child as a next sibling.
            assert(m1[k].next_sib != Some(child));
            if k == child {
                // s1[child] == s0[n] + 1 > s0[n] == s1[n].
            } else {
                assert(m1[k].next_sib == m0[k].next_sib);
            }
        }
    }
    assert(valid_srank(m1, s1));
    assert(sib_acyclic(m1));
}

// A local edit at one slot `k` that keeps `k`'s four CDT links and never turns a
// non-empty slot empty preserves `cspace_wf`. Every structural clause and both
// acyclicity ranks read only links (identical here) and per-slot emptiness (only
// ever relaxed, empty→non-empty), so the witnesses transfer unchanged. The reuse
// `retype_install` (plan §3c) leans on for its three `set_slot`s — the untyped's
// watermark bump (links + emptiness both fixed) and the two detached dst/dst2
// fills (an empty, hence detached, slot gains a cap with its links still null;
// doc/results/28 §1).
pub(crate) proof fn lemma_local_cap_edit_preserves_cspace_wf(
    m0: Map<SlotId, CapSlot>,
    k: SlotId,
    v: CapSlot,
)
    requires
        cspace_wf(m0),
        m0.dom().contains(k),
        v.parent == m0[k].parent,
        v.first_child == m0[k].first_child,
        v.next_sib == m0[k].next_sib,
        v.prev_sib == m0[k].prev_sib,
        is_empty_cap(v.cap) ==> is_empty_cap(m0[k].cap),
    ensures
        cspace_wf(m0.insert(k, v)),
{
    let m1 = m0.insert(k, v);
    assert(m1.dom() =~= m0.dom());
    // Every slot's four CDT links agree with m0 (k by hypothesis, all others
    // untouched). The structural clauses read only these, so they carry.
    assert forall|j: SlotId| #[trigger] m1.dom().contains(j) implies {
        &&& m1[j].parent == m0[j].parent
        &&& m1[j].first_child == m0[j].first_child
        &&& m1[j].next_sib == m0[j].next_sib
        &&& m1[j].prev_sib == m0[j].prev_sib
    } by {
        if j != k {
            assert(m1[j] == m0[j]);
        }
    }
    // empty_slots_detached: a slot empty in m1 is empty in m0 (k by hypothesis,
    // others unchanged), hence detached there, hence detached here (links agree).
    assert forall|j: SlotId| #[trigger] m1.dom().contains(j) implies (is_empty_cap(m1[j].cap) ==> {
        &&& m1[j].parent == None
        &&& m1[j].first_child == None
        &&& m1[j].next_sib == None
        &&& m1[j].prev_sib == None
    }) by {
        if j != k {
            assert(m1[j] == m0[j]);
        }
    }
    assert(cdt_wf(m1));
    // Ranks reuse m0's witnesses: parent/next links and the domain are identical.
    let r0 = choose|r: Map<SlotId, nat>| valid_prank(m0, r);
    assert(valid_prank(m1, r0));
    let s0 = choose|s: Map<SlotId, nat>| valid_srank(m0, s);
    assert(valid_srank(m1, s0));
    assert(acyclic(m1));
    assert(sib_acyclic(m1));
}

// ── slot_move as an identity transposition (doc/results/23 §A) ──
//
// `slot_move` relabels identity `src` onto the previously-isolated empty slot
// `dst`. Because nothing in a well-formed store references an empty/detached slot
// (proven below), this equals the transposition π = (src dst) applied to the
// whole map: swap the slot *contents* at src/dst and rename every link through π.
// A transposition is an involution and a bijection on slot identities, so it
// preserves every structural clause, and the acyclicity ranks transfer through π.

pub open spec fn swap_id(k: SlotId, src: SlotId, dst: SlotId) -> SlotId {
    if k == src { dst } else if k == dst { src } else { k }
}

pub open spec fn ren(o: Option<SlotId>, src: SlotId, dst: SlotId) -> Option<SlotId> {
    match o {
        None => None,
        Some(h) => Some(swap_id(h, src, dst)),
    }
}

// `m` with identities `src` and `dst` transposed (contents swapped, every link
// renamed through π). `relabeled(relabeled(m)) == m`.
pub open spec fn relabeled(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId) -> Map<SlotId, CapSlot> {
    Map::new(
        |k: SlotId| m.dom().contains(k),
        |k: SlotId| {
            let b = m[swap_id(k, src, dst)];
            CapSlot {
                cap: b.cap,
                parent: ren(b.parent, src, dst),
                first_child: ren(b.first_child, src, dst),
                next_sib: ren(b.next_sib, src, dst),
                prev_sib: ren(b.prev_sib, src, dst),
            }
        },
    )
}

// The transposition proof is factored per-clause: the monolith's combined SMT
// context blows the rlimit, so each clause gets its own solver call. Every helper
// shares the same shape — un-rename a link via the π-involution, apply m's
// corresponding clause, re-rename — over `mf = relabeled(m, src, dst)`.

proof fn lemma_transpose_links(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires links_in_domain(m), m.dom().contains(src), m.dom().contains(dst),
    ensures links_in_domain(relabeled(m, src, dst)),
{
    let mf = relabeled(m, src, dst);
    assert(mf.dom() =~= m.dom());
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies {
        &&& link_in_dom(mf, mf[k].parent)
        &&& link_in_dom(mf, mf[k].first_child)
        &&& link_in_dom(mf, mf[k].next_sib)
        &&& link_in_dom(mf, mf[k].prev_sib)
    } by {
        assert(m.dom().contains(swap_id(k, src, dst)));
    }
}

proof fn lemma_transpose_siblings(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires
        siblings_doubly_consistent(m), siblings_share_parent(m),
        m.dom().contains(src), m.dom().contains(dst),
    ensures
        siblings_doubly_consistent(relabeled(m, src, dst)),
        siblings_share_parent(relabeled(m, src, dst)),
{
    let mf = relabeled(m, src, dst);
    assert(mf.dom() =~= m.dom());
    assert forall|a: SlotId| #[trigger] mf.dom().contains(a) implies {
        &&& (mf[a].next_sib matches Some(b) ==> mf.dom().contains(b) && mf[b].prev_sib == Some(a))
        &&& (mf[a].prev_sib matches Some(b) ==> mf.dom().contains(b) && mf[b].next_sib == Some(a))
        &&& (mf[a].next_sib matches Some(b) ==> mf[b].parent == mf[a].parent)
    } by {
        let ja = swap_id(a, src, dst);
        if let Some(b) = mf[a].next_sib {
            assert(m[ja].next_sib == Some(swap_id(b, src, dst)));
            assert(swap_id(swap_id(b, src, dst), src, dst) == b);
        }
        if let Some(b) = mf[a].prev_sib {
            assert(m[ja].prev_sib == Some(swap_id(b, src, dst)));
            assert(swap_id(swap_id(b, src, dst), src, dst) == b);
        }
    }
}

proof fn lemma_transpose_children(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires
        links_in_domain(m),
        first_child_parent_agree(m), head_is_first_child(m), parent_has_first_child(m),
        m.dom().contains(src), m.dom().contains(dst),
    ensures
        first_child_parent_agree(relabeled(m, src, dst)),
        head_is_first_child(relabeled(m, src, dst)),
        parent_has_first_child(relabeled(m, src, dst)),
{
    let mf = relabeled(m, src, dst);
    assert(mf.dom() =~= m.dom());
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].first_child matches Some(c) ==>
            mf.dom().contains(c) && mf[c].parent == Some(k) && mf[c].prev_sib == None) by {
        let jk = swap_id(k, src, dst);
        if let Some(c) = mf[k].first_child {
            // m[jk].first_child == Some(swap(c)); first_child_parent_agree(m) gives
            // swap(c) live with parent jk, prev None; re-rename back through π.
            assert(m[jk].first_child == Some(swap_id(c, src, dst)));
            assert(m.dom().contains(swap_id(c, src, dst)));
            assert(swap_id(swap_id(c, src, dst), src, dst) == c);
            assert(swap_id(swap_id(k, src, dst), src, dst) == k);
        }
    }
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].parent matches Some(p) ==> (mf[k].prev_sib is None ==>
            mf.dom().contains(p) && mf[p].first_child == Some(k))) by {
        let jk = swap_id(k, src, dst);
        if let Some(p) = mf[k].parent {
            assert(m[jk].parent == Some(swap_id(p, src, dst)));
            assert(m.dom().contains(swap_id(p, src, dst)));
            assert(swap_id(swap_id(p, src, dst), src, dst) == p);
            assert(swap_id(swap_id(k, src, dst), src, dst) == k);
        }
    }
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].parent matches Some(p) ==> mf[p].first_child is Some) by {
        let jk = swap_id(k, src, dst);
        if let Some(p) = mf[k].parent {
            assert(m[jk].parent == Some(swap_id(p, src, dst)));
            assert(m.dom().contains(swap_id(p, src, dst)));
            assert(swap_id(swap_id(p, src, dst), src, dst) == p);
        }
    }
}

proof fn lemma_transpose_empty(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires empty_slots_detached(m), m.dom().contains(src), m.dom().contains(dst),
    ensures empty_slots_detached(relabeled(m, src, dst)),
{
    let mf = relabeled(m, src, dst);
    assert(mf.dom() =~= m.dom());
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (is_empty_cap(mf[k].cap) ==> {
            &&& mf[k].parent == None
            &&& mf[k].first_child == None
            &&& mf[k].next_sib == None
            &&& mf[k].prev_sib == None
        }) by {
        // mf[k].cap == m[swap(k)].cap; if empty, m[swap(k)] is detached, so each
        // link is None and ren(None) == None.
        assert(m.dom().contains(swap_id(k, src, dst)));
    }
}

proof fn lemma_transpose_acyclic(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires acyclic(m), m.dom().contains(src), m.dom().contains(dst),
    ensures acyclic(relabeled(m, src, dst)),
{
    let mf = relabeled(m, src, dst);
    assert(mf.dom() =~= m.dom());
    let r0 = choose|r: Map<SlotId, nat>| valid_prank(m, r);
    let rf = Map::<SlotId, nat>::new(|k: SlotId| mf.dom().contains(k), |k: SlotId| r0[swap_id(k, src, dst)]);
    assert(rf.dom() =~= mf.dom());
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].parent matches Some(p) ==> mf.dom().contains(p) && rf[k] < rf[p]) by {
        let jk = swap_id(k, src, dst);
        if let Some(p) = mf[k].parent {
            assert(m[jk].parent == Some(swap_id(p, src, dst)));
        }
    }
    assert(valid_prank(mf, rf));
}

proof fn lemma_transpose_sib(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires sib_acyclic(m), m.dom().contains(src), m.dom().contains(dst),
    ensures sib_acyclic(relabeled(m, src, dst)),
{
    let mf = relabeled(m, src, dst);
    assert(mf.dom() =~= m.dom());
    let s0 = choose|s: Map<SlotId, nat>| valid_srank(m, s);
    let sf = Map::<SlotId, nat>::new(|k: SlotId| mf.dom().contains(k), |k: SlotId| s0[swap_id(k, src, dst)]);
    assert(sf.dom() =~= mf.dom());
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].next_sib matches Some(n) ==> mf.dom().contains(n) && sf[n] < sf[k]) by {
        let jk = swap_id(k, src, dst);
        if let Some(n) = mf[k].next_sib {
            assert(m[jk].next_sib == Some(swap_id(n, src, dst)));
        }
    }
    assert(valid_srank(mf, sf));
}

// Transposing two slot identities preserves the whole well-formedness — a pure
// renaming. (No emptiness hypothesis: holds for any two live slots; `dst`'s
// emptiness is only what makes the transposition equal `slot_move`'s body —
// established at the call site.)
proof fn lemma_transpose_preserves_cspace_wf(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires
        cspace_wf(m),
        m.dom().finite(),
        m.dom().contains(src),
        m.dom().contains(dst),
    ensures
        cspace_wf(relabeled(m, src, dst)),
        relabeled(m, src, dst).dom() == m.dom(),
        relabeled(m, src, dst).dom().finite(),
{
    assert(relabeled(m, src, dst).dom() =~= m.dom());
    lemma_transpose_links(m, src, dst);
    lemma_transpose_siblings(m, src, dst);
    lemma_transpose_children(m, src, dst);
    lemma_transpose_empty(m, src, dst);
    lemma_transpose_acyclic(m, src, dst);
    lemma_transpose_sib(m, src, dst);
}

// ── Child-chain reachability (doc/results/23 §B): the keystone for the
//    children-walk loops' *completeness* — every child of a node lies on its
//    `first_child → next_sib` chain, so a walk re-parents all of them. ──

// `k` is reachable from `from` by 0+ `next_sib` steps. Well-founded on a sibling
// rank `s` (the walk strictly lowers it), so this terminates.
pub open spec fn next_reach(m: Map<SlotId, CapSlot>, from: SlotId, k: SlotId, s: Map<SlotId, nat>) -> bool
    decreases s[from],
{
    if from == k {
        true
    } else {
        match m[from].next_sib {
            Some(nx) => s[nx] < s[from] && next_reach(m, nx, k, s),
            None => false,
        }
    }
}

// Reachability only ever lowers (weakly) the sibling rank — so a node cannot
// reach a strictly higher-ranked node (used to show the just-processed `cur` is
// not reachable from its successor).
proof fn lemma_next_reach_sr(m: Map<SlotId, CapSlot>, from: SlotId, k: SlotId, s: Map<SlotId, nat>)
    requires next_reach(m, from, k, s),
    ensures s[k] <= s[from],
    decreases s[from],
{
    if from == k {
    } else {
        let nx = m[from].next_sib->0;
        lemma_next_reach_sr(m, nx, k, s);
    }
}

// Append one `next_sib` edge at the tail of a reach-path.
proof fn lemma_next_reach_extend(m: Map<SlotId, CapSlot>, h: SlotId, j: SlotId, k: SlotId, s: Map<SlotId, nat>)
    requires
        next_reach(m, h, j, s),
        m[j].next_sib == Some(k),
        s[k] < s[j],
    ensures
        next_reach(m, h, k, s),
    decreases s[h],
{
    if h == k {
        assert(next_reach(m, h, k, s));
    } else if h == j {
        // next_reach(j, k): j.next == Some(k), s[k] < s[j] == s[h], next_reach(k,k).
        assert(next_reach(m, k, k, s));
        assert(next_reach(m, h, k, s));
    } else {
        // h != j and next_reach(h,j) ⟹ h.next == Some(nx), s[nx] < s[h], next_reach(nx,j).
        assert(m[h].next_sib is Some);
        let nx = m[h].next_sib->0;
        assert(s[nx] < s[h] && next_reach(m, nx, j, s));
        lemma_next_reach_extend(m, nx, j, k, s);
        assert(next_reach(m, nx, k, s));
        assert(next_reach(m, h, k, s));
    }
}

// Every child of `src` is `next_sib`-reachable from `src`'s first child. Recurses
// toward the list head along `prev_sib`; the measure is the count of children
// ranked above `k` (each `prev` step has strictly higher rank, so the count drops).
proof fn lemma_child_on_chain(m: Map<SlotId, CapSlot>, src: SlotId, k: SlotId, s: Map<SlotId, nat>)
    requires
        cdt_wf(m),
        valid_srank(m, s),
        m.dom().finite(),
        m.dom().contains(src),
        m.dom().contains(k),
        m[k].parent == Some(src),
    ensures
        m[src].first_child is Some,
        next_reach(m, m[src].first_child->0, k, s),
    decreases m.dom().filter(|x: SlotId| m[x].parent == Some(src) && s[x] > s[k]).len(),
{
    // k has a parent (src), so by parent_has_first_child src has a first child.
    assert(m[src].first_child is Some);
    let h = m[src].first_child->0;
    if m[k].prev_sib is None {
        // head_is_first_child: a parented, prev-less node IS its parent's first child.
        assert(m[src].first_child == Some(k));
        assert(m[src].first_child->0 == k);
        assert(next_reach(m, k, k, s));
    } else {
        let j = m[k].prev_sib->0;
        // doubly: j.next == Some(k); share_parent: j is also a child of src;
        // valid_srank: s[k] < s[j].
        assert(m.dom().contains(j) && m[j].next_sib == Some(k));
        assert(m[j].parent == Some(src));
        assert(s[k] < s[j]);
        // measure: {child : s > s[j]} ⊊ {child : s > s[k]} (j is in the latter, not
        // the former), so the recursive call's count is strictly smaller.
        let f_k = m.dom().filter(|x: SlotId| m[x].parent == Some(src) && s[x] > s[k]);
        let f_j = m.dom().filter(|x: SlotId| m[x].parent == Some(src) && s[x] > s[j]);
        assert(f_k.contains(j));
        assert(f_j.subset_of(f_k.remove(j))) by {
            assert forall|x: SlotId| f_j.contains(x) implies f_k.remove(j).contains(x) by {}
        }
        assert(f_k.remove(j).len() == f_k.len() - 1);
        assert(f_j.len() <= f_k.remove(j).len()) by {
            vstd::set_lib::lemma_len_subset(f_j, f_k.remove(j));
        }
        lemma_child_on_chain(m, src, j, s);
        lemma_next_reach_extend(m, h, j, k, s);
    }
}

// Replacing a slot's empty cap with another empty cap of the *same links* is
// invisible to `cspace_wf` (which reads only link structure + `is_empty_cap`).
// `slot_move` clears `src` to `CapSlot::empty()` where the transposition leaves
// `m0[dst]` — same (None) links, both empty, possibly different rights bits.
proof fn lemma_replace_empty_cap(mf: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot)
    requires
        cspace_wf(mf),
        mf.dom().finite(),
        mf.dom().contains(k),
        is_empty_cap(mf[k].cap),
        is_empty_cap(v.cap),
        v.parent == mf[k].parent,
        v.first_child == mf[k].first_child,
        v.next_sib == mf[k].next_sib,
        v.prev_sib == mf[k].prev_sib,
    ensures
        cspace_wf(mf.insert(k, v)),
        mf.insert(k, v).dom() == mf.dom(),
{
    let m2 = mf.insert(k, v);
    assert(m2.dom() =~= mf.dom());
    assert forall|j: SlotId| #[trigger] m2.dom().contains(j) implies {
        &&& m2[j].parent == mf[j].parent
        &&& m2[j].first_child == mf[j].first_child
        &&& m2[j].next_sib == mf[j].next_sib
        &&& m2[j].prev_sib == mf[j].prev_sib
        &&& is_empty_cap(m2[j].cap) == is_empty_cap(mf[j].cap)
    } by {}
    assert(cdt_wf(m2));
    let r = choose|r: Map<SlotId, nat>| valid_prank(mf, r);
    assert(valid_prank(m2, r));
    let s = choose|s: Map<SlotId, nat>| valid_srank(mf, s);
    assert(valid_srank(m2, s));
}

// A move's live-slot count is unchanged: the non-empty set loses `src` and gains
// `dst` (every other slot's emptiness is untouched).
proof fn lemma_move_count(m0: Map<SlotId, CapSlot>, mfin: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires
        m0.dom().finite(),
        m0.dom() == mfin.dom(),
        m0.dom().contains(src),
        m0.dom().contains(dst),
        src != dst,
        !is_empty_cap(m0[src].cap),
        is_empty_cap(m0[dst].cap),
        is_empty_cap(mfin[src].cap),
        !is_empty_cap(mfin[dst].cap),
        forall|k: SlotId| m0.dom().contains(k) && k != src && k != dst
            ==> #[trigger] is_empty_cap(mfin[k].cap) == is_empty_cap(m0[k].cap),
    ensures
        count_nonempty(mfin) == count_nonempty(m0),
{
    let ne0 = m0.dom().filter(|k: SlotId| !is_empty_cap(m0[k].cap));
    let nef = mfin.dom().filter(|k: SlotId| !is_empty_cap(mfin[k].cap));
    assert(ne0.contains(src) && !ne0.contains(dst));
    assert(nef =~= ne0.remove(src).insert(dst)) by {
        assert forall|k: SlotId| nef.contains(k) <==> ne0.remove(src).insert(dst).contains(k) by {
            if k != src && k != dst && m0.dom().contains(k) {
                assert(is_empty_cap(mfin[k].cap) == is_empty_cap(m0[k].cap));
            }
        }
    }
    assert(!ne0.remove(src).contains(dst));
    assert(ne0.remove(src).len() == ne0.len() - 1);
    assert(count_nonempty(mfin) == nef.len());
    assert(count_nonempty(m0) == ne0.len());
}

// ── slot_move body-match support (doc/results/24 §A): the classification facts
//    that turn the imperative neighbour-fixups into the transposition's renaming.
//    All follow from `cspace_wf(m0)` + `dst` empty/detached; kept as small
//    lemmas so each SMT call starts with a tiny context (the doc/results/23 §2
//    per-clause discipline). ──

// Nothing in a well-formed store references a detached empty slot `e`. So the
// transposition's `ren(·, src, e)` only ever rewrites `src → e`, never `e → src`
// (the empty `dst` is never a link *target* in `m0`).
proof fn lemma_nothing_points_to_empty(m: Map<SlotId, CapSlot>, e: SlotId)
    requires
        cdt_wf(m),
        m.dom().contains(e),
        is_empty_cap(m[e].cap),
    ensures
        forall|k: SlotId| #[trigger] m.dom().contains(k) ==> {
            &&& m[k].parent != Some(e)
            &&& m[k].first_child != Some(e)
            &&& m[k].next_sib != Some(e)
            &&& m[k].prev_sib != Some(e)
        },
{
    assert(empty_slots_detached(m));
    assert(m[e].parent is None);
    assert(m[e].first_child is None);
    assert(m[e].next_sib is None);
    assert(m[e].prev_sib is None);
    assert(parent_has_first_child(m));
    assert(first_child_parent_agree(m));
    assert(siblings_doubly_consistent(m));
    assert forall|k: SlotId| #[trigger] m.dom().contains(k) implies {
        &&& m[k].parent != Some(e)
        &&& m[k].first_child != Some(e)
        &&& m[k].next_sib != Some(e)
        &&& m[k].prev_sib != Some(e)
    } by {
        // parent==e ⟹ e.first_child is Some (parent_has_first_child); e detached.
        if m[k].parent == Some(e) {
            assert(m[e].first_child is Some);
        }
        // first_child==e ⟹ e.parent==Some(k) (first_child_parent_agree); e detached.
        if m[k].first_child == Some(e) {
            assert(m[e].parent == Some(k));
        }
        // next_sib==e ⟹ e.prev_sib==Some(k) (doubly); e detached.
        if m[k].next_sib == Some(e) {
            assert(m[e].prev_sib == Some(k));
        }
        // prev_sib==e ⟹ e.next_sib==Some(k) (doubly); e detached.
        if m[k].prev_sib == Some(e) {
            assert(m[e].next_sib == Some(k));
        }
    }
}

// `src` never links to itself: a self parent/sibling violates an acyclicity rank,
// and a self first-child violates it via `first_child_parent_agree`.
proof fn lemma_src_no_self_link(m: Map<SlotId, CapSlot>, src: SlotId)
    requires
        cspace_wf(m),
        m.dom().contains(src),
    ensures
        m[src].parent != Some(src),
        m[src].first_child != Some(src),
        m[src].next_sib != Some(src),
        m[src].prev_sib != Some(src),
{
    let r = choose|r: Map<SlotId, nat>| valid_prank(m, r);
    assert(valid_prank(m, r));
    let sr = choose|sr: Map<SlotId, nat>| valid_srank(m, sr);
    assert(valid_srank(m, sr));
    assert(first_child_parent_agree(m));
    assert(siblings_doubly_consistent(m));
    if m[src].parent == Some(src) {
        assert(r[src] < r[src]);
    }
    if m[src].first_child == Some(src) {
        assert(m[src].parent == Some(src));
        assert(r[src] < r[src]);
    }
    if m[src].next_sib == Some(src) {
        assert(sr[src] < sr[src]);
    }
    if m[src].prev_sib == Some(src) {
        assert(m[src].next_sib == Some(src));
        assert(sr[src] < sr[src]);
    }
}

// The transposition value at a **child** of `src`: identical to `m0[x]` except
// its parent is renamed `src → dst`. A child's other three links cannot name
// `src` (first_child ⟹ rank cycle; next/prev_sib ⟹ `siblings_share_parent`
// would force `src.parent == Some(src)`) nor `dst` (lemma_nothing_points_to_empty),
// so the rename is the identity on them.
proof fn lemma_child_relabeled(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId, x: SlotId)
    requires
        cspace_wf(m),
        m.dom().contains(src),
        m.dom().contains(dst),
        m.dom().contains(x),
        src != dst,
        is_empty_cap(m[dst].cap),
        m[x].parent == Some(src),
    ensures
        x != src,
        x != dst,
        relabeled(m, src, dst)[x].cap == m[x].cap,
        relabeled(m, src, dst)[x].parent == Some(dst),
        relabeled(m, src, dst)[x].first_child == m[x].first_child,
        relabeled(m, src, dst)[x].next_sib == m[x].next_sib,
        relabeled(m, src, dst)[x].prev_sib == m[x].prev_sib,
{
    lemma_src_no_self_link(m, src);
    lemma_nothing_points_to_empty(m, dst);
    let r = choose|r: Map<SlotId, nat>| valid_prank(m, r);
    assert(valid_prank(m, r));
    assert(siblings_doubly_consistent(m));
    assert(siblings_share_parent(m));
    // x is a child of src, so x != src (self-parent would break the rank) and
    // x != dst (dst is detached, parent None != Some(src)).
    if x == src {
        assert(r[src] < r[src]);
    }
    assert(x != dst);
    // x's first_child != Some(src): else src.parent == Some(x) and x.parent ==
    // Some(src) is a 2-cycle (r[x] < r[src] < r[x]).
    if m[x].first_child == Some(src) {
        assert(first_child_parent_agree(m));
        assert(m[src].parent == Some(x));
        assert(r[src] < r[x]);
        assert(r[x] < r[src]);
    }
    // x's next_sib/prev_sib != Some(src): doubly-consistency + share_parent would
    // force m[src].parent == m[x].parent == Some(src), impossible by the rank.
    if m[x].next_sib == Some(src) {
        assert(m[src].parent == m[x].parent);
        assert(m[src].parent == Some(src));
        assert(r[src] < r[src]);
    }
    if m[x].prev_sib == Some(src) {
        assert(m[src].next_sib == Some(x));
        assert(m[x].parent == m[src].parent);
        assert(m[src].parent == Some(src));
        assert(r[src] < r[src]);
    }
    // none of x's links name dst (lemma_nothing_points_to_empty).
    assert(m[x].first_child != Some(dst));
    assert(m[x].next_sib != Some(dst));
    assert(m[x].prev_sib != Some(dst));
    // so every `ren` on x's links is the identity except parent (src → dst).
    assert(swap_id(x, src, dst) == x);
    assert(ren(m[x].parent, src, dst) == Some(dst));
    assert(ren(m[x].first_child, src, dst) == m[x].first_child);
    assert(ren(m[x].next_sib, src, dst) == m[x].next_sib);
    assert(ren(m[x].prev_sib, src, dst) == m[x].prev_sib);
}

// One-field updates of a slot — the spec mirrors of the body's `cs.parent = …`
// etc. (kept explicit rather than `..` struct-update to stay portable across
// the Verus spec subset). Used to write the straight-line intermediate maps.
pub open spec fn set_parent(s: CapSlot, p: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: p, first_child: s.first_child, next_sib: s.next_sib, prev_sib: s.prev_sib }
}
pub open spec fn set_first_child(s: CapSlot, f: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: s.parent, first_child: f, next_sib: s.next_sib, prev_sib: s.prev_sib }
}
pub open spec fn set_next_sib(s: CapSlot, n: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: s.parent, first_child: s.first_child, next_sib: n, prev_sib: s.prev_sib }
}
pub open spec fn set_prev_sib(s: CapSlot, p: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: s.parent, first_child: s.first_child, next_sib: s.next_sib, prev_sib: p }
}

// The transposition value at `dst`: exactly `m[src]` (unrenamed). `src`'s links
// avoid both `src` (self-link) and `dst` (detached empty), so the rename is the
// identity on them — which is why the body copying `src`'s links into `dst`
// *verbatim* still lands the transposition. (doc/results/24 §A, fact 2.)
proof fn lemma_dst_relabeled(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId)
    requires
        cspace_wf(m),
        m.dom().contains(src),
        m.dom().contains(dst),
        src != dst,
        is_empty_cap(m[dst].cap),
    ensures
        relabeled(m, src, dst)[dst] == m[src],
{
    lemma_src_no_self_link(m, src);
    lemma_nothing_points_to_empty(m, dst);
    assert(swap_id(dst, src, dst) == src);
    assert(ren(m[src].parent, src, dst) == m[src].parent);
    assert(ren(m[src].first_child, src, dst) == m[src].first_child);
    assert(ren(m[src].next_sib, src, dst) == m[src].next_sib);
    assert(ren(m[src].prev_sib, src, dst) == m[src].prev_sib);
}

// The transposition value at a generic `k ∉ {src, dst}`: `m[k]` with every link
// that named `src` redirected to `dst` (no link names `dst` to begin with —
// lemma_nothing_points_to_empty — so the swap is one-directional). The
// neighbour-fixup case analysis reads each field off this. (doc/results/24 §A.)
proof fn lemma_generic_relabeled(m: Map<SlotId, CapSlot>, src: SlotId, dst: SlotId, k: SlotId)
    requires
        cdt_wf(m),
        m.dom().contains(src),
        m.dom().contains(dst),
        src != dst,
        is_empty_cap(m[dst].cap),
        m.dom().contains(k),
        k != src,
        k != dst,
    ensures
        relabeled(m, src, dst)[k].cap == m[k].cap,
        relabeled(m, src, dst)[k].parent
            == (if m[k].parent == Some(src) { Some(dst) } else { m[k].parent }),
        relabeled(m, src, dst)[k].first_child
            == (if m[k].first_child == Some(src) { Some(dst) } else { m[k].first_child }),
        relabeled(m, src, dst)[k].next_sib
            == (if m[k].next_sib == Some(src) { Some(dst) } else { m[k].next_sib }),
        relabeled(m, src, dst)[k].prev_sib
            == (if m[k].prev_sib == Some(src) { Some(dst) } else { m[k].prev_sib }),
{
    lemma_nothing_points_to_empty(m, dst);
    assert(swap_id(k, src, dst) == k);
    assert(m[k].parent != Some(dst));
    assert(m[k].first_child != Some(dst));
    assert(m[k].next_sib != Some(dst));
    assert(m[k].prev_sib != Some(dst));
    assert(ren(m[k].parent, src, dst)
        == (if m[k].parent == Some(src) { Some(dst) } else { m[k].parent }));
    assert(ren(m[k].first_child, src, dst)
        == (if m[k].first_child == Some(src) { Some(dst) } else { m[k].first_child }));
    assert(ren(m[k].next_sib, src, dst)
        == (if m[k].next_sib == Some(src) { Some(dst) } else { m[k].next_sib }));
    assert(ren(m[k].prev_sib, src, dst)
        == (if m[k].prev_sib == Some(src) { Some(dst) } else { m[k].prev_sib }));
}

// ── cdt_unlink body-match support (doc/results/25): the sibling-list *merge*.
//    Unlike slot_move's transposition, unlink grafts `slot`'s child chain into
//    `slot`'s former sibling position one level up. `unlinked(m, slot, last)` is
//    the closed-form result; `lemma_unlink_preserves_cspace_wf` proves it keeps
//    `cspace_wf`. The structural clauses are factored per-clause (the per-clause
//    SMT discipline of the transpose family); the sibling-acyclicity witness is
//    the crux (a constant additive shift fails — the child band must be rescaled
//    into the `prev..next` gap). ──

// A rank map over a finite domain has a strict upper bound. The sib-acyclicity
// splice witness scales non-children by `b+1` to open a gap wide enough to drop
// the re-parented child band into; `b` is that bound.
proof fn lemma_rank_bounded(dom: Set<SlotId>, s: Map<SlotId, nat>) -> (b: nat)
    requires
        dom.finite(),
        forall|k: SlotId| dom.contains(k) ==> s.dom().contains(k),
    ensures
        forall|k: SlotId| dom.contains(k) ==> s[k] < b,
    decreases dom.len(),
{
    if dom.len() == 0 {
        0
    } else {
        let x = dom.choose();
        assert(dom.contains(x));
        let rest = dom.remove(x);
        assert(rest.len() < dom.len());
        assert forall|k: SlotId| rest.contains(k) implies s.dom().contains(k) by {
            assert(dom.contains(k));
        }
        let b0 = lemma_rank_bounded(rest, s);
        let b: nat = if s[x] < b0 { b0 } else { (s[x] + 1) as nat };
        assert forall|k: SlotId| dom.contains(k) implies s[k] < b by {
            if k != x {
                assert(rest.contains(k));
            }
        }
        b
    }
}

// The result of unlinking `slot`: cap kept everywhere; `slot` fully detached;
// each child re-parented to `slot`'s parent (the grandparent), with the chain
// head's `prev_sib` and the chain tail's `next_sib` rewired to `slot`'s old
// neighbours; the neighbour fixups (`prev.next`/`parent.first_child` → head,
// head.prev → prev, next.prev → tail) applied. `last` is the chain tail (the
// child with `next_sib is None`, or `None` when `slot` is childless) — it appears
// only in `next`'s `prev_sib`, so `next_sib` structure (hence sib-acyclicity) is
// independent of it.
pub open spec fn unlinked(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>) -> Map<SlotId, CapSlot> {
    let p = m[slot].parent;
    let pv = m[slot].prev_sib;
    let nx = m[slot].next_sib;
    let fc = m[slot].first_child;
    let head = if fc is None { nx } else { fc };
    Map::new(
        |k: SlotId| m.dom().contains(k),
        |k: SlotId| if k == slot {
            CapSlot { cap: m[slot].cap, parent: None, first_child: None, next_sib: None, prev_sib: None }
        } else if m[k].parent == Some(slot) {
            CapSlot {
                cap: m[k].cap,
                parent: p,
                first_child: m[k].first_child,
                next_sib: if m[k].next_sib is None { nx } else { m[k].next_sib },
                prev_sib: if m[k].prev_sib is None { pv } else { m[k].prev_sib },
            }
        } else {
            CapSlot {
                cap: m[k].cap,
                parent: m[k].parent,
                first_child: if m[k].first_child == Some(slot) { head } else { m[k].first_child },
                next_sib: if m[k].next_sib == Some(slot) { head } else { m[k].next_sib },
                prev_sib: if m[k].prev_sib == Some(slot) {
                    if fc is None { pv } else { last }
                } else {
                    m[k].prev_sib
                },
            }
        },
    )
}

// The neighbour roles are pairwise distinct and well-placed: `slot` has no self
// link; `prev`/`next` are non-children of `slot` (their parent is `slot`'s
// parent) living in the domain; `prev != next`; the grandparent is a non-child.
proof fn lemma_unlink_roles(m: Map<SlotId, CapSlot>, slot: SlotId)
    requires
        cspace_wf(m),
        m.dom().contains(slot),
    ensures
        m[slot].parent != Some(slot),
        m[slot].first_child != Some(slot),
        m[slot].next_sib != Some(slot),
        m[slot].prev_sib != Some(slot),
        m[slot].prev_sib matches Some(pv) ==> m.dom().contains(pv) && m[pv].parent != Some(slot),
        m[slot].next_sib matches Some(nx) ==> m.dom().contains(nx) && m[nx].parent != Some(slot),
        m[slot].parent matches Some(g) ==> m.dom().contains(g) && m[g].parent != Some(slot),
        (m[slot].prev_sib matches Some(pv) ==> m[slot].next_sib != Some(pv)),
{
    lemma_src_no_self_link(m, slot);
    let r = choose|r: Map<SlotId, nat>| valid_prank(m, r);
    assert(valid_prank(m, r));
    let sr = choose|sr: Map<SlotId, nat>| valid_srank(m, sr);
    assert(valid_srank(m, sr));
    assert(links_in_domain(m));
    assert(siblings_doubly_consistent(m));
    assert(siblings_share_parent(m));
    if let Some(pv) = m[slot].prev_sib {
        // doubly: pv.next == slot; share_parent: pv.parent == slot.parent ≠ Some(slot).
        assert(m[pv].next_sib == Some(slot));
        assert(m[pv].parent == m[slot].parent);
    }
    if let Some(nx) = m[slot].next_sib {
        // share_parent on slot.next: nx.parent == slot.parent ≠ Some(slot).
        assert(m[nx].parent == m[slot].parent);
    }
    if let Some(g) = m[slot].parent {
        // a 2-cycle slot↔g would break the parent rank.
        if m[g].parent == Some(slot) {
            assert(r[slot] < r[g]);
            assert(r[g] < r[slot]);
        }
    }
    // prev == next would give pv.next == slot and slot.next == pv: a srank 2-cycle.
    if let Some(pv) = m[slot].prev_sib {
        if m[slot].next_sib == Some(pv) {
            assert(sr[pv] < sr[slot]);
            assert(sr[slot] < sr[pv]);
        }
    }
}

// Scaling by a positive `w` preserves strict order. (The non-child rank band.)
proof fn lemma_scaled_lt(x: nat, y: nat, w: nat)
    requires
        x < y,
        w > 0,
    ensures
        x * w < y * w,
{
    vstd::arithmetic::mul::lemma_mul_strict_inequality(x as int, y as int, w as int);
}

// The child band `(d+1)*w + e + 1` lands strictly below `(r+1)*w` when the inner
// offset `e+1` is below the band width `w` and `d+1 <= r`. This is the splice's
// "the rescaled child chain fits in the gap below `prev`" inequality.
proof fn lemma_band_below(d: nat, e: nat, r: nat, w: nat)
    requires
        e + 1 < w,
        d + 1 <= r,
        w > 0,
    ensures
        (d + 1) * w + e + 1 < (r + 1) * w,
{
    broadcast use vstd::arithmetic::mul::group_mul_is_commutative_and_distributive;
    // (d+1)*w + (e+1) < (d+1)*w + w == (d+2)*w <= (r+1)*w
    assert((d + 1) * w + w == (d + 2) * w);
    vstd::arithmetic::mul::lemma_mul_inequality((d + 2) as int, (r + 1) as int, w as int);
}

// Sibling-acyclicity survives the splice. The witness *rescales*: non-children
// sit at `(s0[k]+1)*(B+1)` (multiples of the band width `B+1`), and re-parented
// children sit in the band just above `next`'s scaled rank,
// `(D+1)*(B+1) + s0[k] + 1` (D = s0[next] or 0) — `B`'s bound keeps that band
// strictly inside the gap below `prev`. A constant additive shift cannot do this
// (the child chain's rank span can exceed the `prev..next` gap). (doc/results/25.)
proof fn lemma_unlink_sib(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        cdt_wf(m),
        sib_acyclic(m),
        m.dom().finite(),
        m.dom().contains(slot),
        m[slot].parent != Some(slot),
    ensures
        sib_acyclic(unlinked(m, slot, last)),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    let s0 = choose|s: Map<SlotId, nat>| valid_srank(m, s);
    assert(valid_srank(m, s0));
    let bnd = lemma_rank_bounded(m.dom(), s0);
    let nx = m[slot].next_sib;
    let d: nat = match nx {
        Some(n) => s0[n],
        None => 0,
    };
    let bb: nat = (bnd + 1) as nat;
    let sf = Map::<SlotId, nat>::new(
        |k: SlotId| mf.dom().contains(k),
        |k: SlotId| if k != slot && m[k].parent == Some(slot) {
            ((d + 1) * bb + s0[k] + 1) as nat
        } else {
            ((s0[k] + 1) * bb) as nat
        },
    );
    assert(sf.dom() =~= mf.dom());
    assert(links_in_domain(m));
    assert(siblings_doubly_consistent(m));
    assert(siblings_share_parent(m));
    assert(first_child_parent_agree(m));
    // `slot`'s next sibling (if any) is a non-child: share_parent puts its parent
    // at `slot.parent`, which is not `slot` (no self-loop).
    assert(nx is Some ==> m[nx->0].parent == m[slot].parent);
    assert(nx is Some ==> m[nx->0].parent != Some(slot));
    let fc = m[slot].first_child;
    let head = if fc is None { nx } else { fc };

    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].next_sib matches Some(n) ==> mf.dom().contains(n) && sf[n] < sf[k]) by {
        if let Some(n) = mf[k].next_sib {
            if k == slot {
                assert(mf[slot].next_sib is None);
            } else if m[k].parent == Some(slot) {
                // ── k is a child of slot ──
                if let Some(c) = m[k].next_sib {
                    assert(n == c);
                    assert(m[c].parent == Some(slot));   // share_parent
                    assert(m.dom().contains(c));         // links_in_domain
                    assert(s0[c] < s0[k]);               // valid_srank
                    // both children: same `(d+1)*bb` term, ordered by s0.
                } else {
                    // last child: next_sib rewired to nx == Some(n), a non-child.
                    assert(nx == Some(n));
                    assert(m[n].parent != Some(slot));   // roles: slot.next non-child
                    assert(m.dom().contains(n));
                    assert(d == s0[n]);
                    // sf[n] == (s0[n]+1)*bb == (d+1)*bb < (d+1)*bb + s0[k] + 1 == sf[k].
                }
            } else {
                // ── k is a non-child (and != slot) ──
                if m[k].next_sib == Some(slot) {
                    // k == prev; next_sib rewired to head == Some(n).
                    assert(m[slot].prev_sib == Some(k));  // doubly (k.next == slot)
                    assert(s0[slot] < s0[k]);             // valid_srank
                    assert(d < s0[k]) by {
                        if let Some(nn) = nx {
                            assert(s0[nn] < s0[slot]);    // valid_srank slot.next
                        }
                    }
                    if let Some(f) = fc {
                        assert(head == Some(f));
                        assert(n == f);
                        assert(m[f].parent == Some(slot)); // first_child_parent_agree
                        assert(m.dom().contains(f));
                        assert(s0[f] < bnd);               // bound
                        lemma_band_below(d, s0[f], s0[k], bb);
                    } else {
                        // fc None ⟹ head == nx == Some(n), a non-child below prev.
                        assert(nx == Some(n));
                        assert(m[n].parent != Some(slot)); // roles
                        assert(m.dom().contains(n));
                        assert(s0[n] < s0[k]);             // s0[n] < s0[slot] < s0[k]
                        lemma_scaled_lt((s0[n] + 1) as nat, (s0[k] + 1) as nat, bb);
                    }
                } else {
                    // next_sib unchanged: n is a non-child below k.
                    assert(m[k].next_sib == Some(n));
                    assert(m.dom().contains(n));           // links_in_domain
                    assert(s0[n] < s0[k]);                 // valid_srank
                    assert(m[k].parent == m[n].parent);    // share_parent ⟹ n non-child
                    assert(m[n].parent != Some(slot));
                    lemma_scaled_lt((s0[n] + 1) as nat, (s0[k] + 1) as nat, bb);
                }
            }
        }
    }
    assert(valid_srank(mf, sf));
    assert(sib_acyclic(mf));
}

// `last` is `slot`'s child-chain tail (the unique child with `next_sib is None`),
// or `None` when `slot` is childless. The body computes it by walking the chain;
// these clauses are what the wf proof needs of it (it appears only in `next`'s
// new `prev_sib`). Uniqueness (clause 3) is discharged in the body via
// `lemma_child_on_chain` (the chain is linear, so its tail is unique).
pub open spec fn last_wf(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>) -> bool {
    &&& (m[slot].first_child is None <==> last is None)
    &&& (last matches Some(l) ==> m.dom().contains(l) && m[l].parent == Some(slot) && m[l].next_sib is None)
    &&& (forall|x: SlotId| #[trigger] m.dom().contains(x) && m[x].parent == Some(slot) && m[x].next_sib is None
            ==> last == Some(x))
}

// Parent-acyclicity survives with the *same* witness `r0` (the nice asymmetry vs.
// the sibling side): each child moves from `parent=slot` to `parent=grandparent`,
// and `r0[child] < r0[slot] < r0[grandparent]` already holds, so the strict drop
// is preserved; `slot` becomes a root (no constraint). No reseating needed.
proof fn lemma_unlink_acyclic(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        links_in_domain(m),
        acyclic(m),
        m.dom().contains(slot),
    ensures
        acyclic(unlinked(m, slot, last)),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    let r0 = choose|r: Map<SlotId, nat>| valid_prank(m, r);
    assert(valid_prank(m, r0));
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].parent matches Some(pp) ==> mf.dom().contains(pp) && r0[k] < r0[pp]) by {
        if let Some(pp) = mf[k].parent {
            if k == slot {
                assert(mf[slot].parent is None);
            } else if m[k].parent == Some(slot) {
                // child: parent rewired to slot's parent (the grandparent) == pp.
                assert(m[slot].parent == Some(pp));
                assert(r0[k] < r0[slot]);    // k.parent == slot
                assert(r0[slot] < r0[pp]);   // slot.parent == pp
            } else {
                assert(m[k].parent == Some(pp));   // non-child: parent unchanged
            }
        }
    }
    assert(valid_prank(mf, r0));
}

// Empty slots stay detached: a slot's cap is unchanged, and an empty slot was
// already detached in `m` (so it is none of slot/child/neighbour), hence its
// links — all `None` in `m` — are untouched.
proof fn lemma_unlink_empty(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        empty_slots_detached(m),
        m.dom().contains(slot),
    ensures
        empty_slots_detached(unlinked(m, slot, last)),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (is_empty_cap(mf[k].cap) ==> {
            &&& mf[k].parent == None
            &&& mf[k].first_child == None
            &&& mf[k].next_sib == None
            &&& mf[k].prev_sib == None
        }) by {
        if is_empty_cap(mf[k].cap) {
            // cap is framed: mf[k].cap == m[k].cap, so m[k] is empty ⟹ detached.
            assert(m[k].cap == mf[k].cap);
            assert(m[k].parent == None);
            assert(m[k].first_child == None);
            assert(m[k].next_sib == None);
            assert(m[k].prev_sib == None);
            // k detached ⟹ not a child (parent None) and not slot's prev/next/parent
            // target, so every fixup condition is false and links stay None.
        }
    }
}

// Every link in the spliced map lands in the domain: the new targets are `slot`'s
// own neighbours (`parent`/`prev`/`next`/`first_child` — all in `m`'s domain) and
// the chain tail `last` (in domain by `last_wf`); every other field is unchanged.
proof fn lemma_unlink_links(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        links_in_domain(m),
        m.dom().contains(slot),
        last_wf(m, slot, last),
    ensures
        links_in_domain(unlinked(m, slot, last)),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    assert(link_in_dom(m, m[slot].parent));
    assert(link_in_dom(m, m[slot].first_child));
    assert(link_in_dom(m, m[slot].next_sib));
    assert(link_in_dom(m, m[slot].prev_sib));
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies {
        &&& link_in_dom(mf, mf[k].parent)
        &&& link_in_dom(mf, mf[k].first_child)
        &&& link_in_dom(mf, mf[k].next_sib)
        &&& link_in_dom(mf, mf[k].prev_sib)
    } by {
        assert(link_in_dom(m, m[k].parent));
        assert(link_in_dom(m, m[k].first_child));
        assert(link_in_dom(m, m[k].next_sib));
        assert(link_in_dom(m, m[k].prev_sib));
    }
}

// The spliced sibling list is doubly consistent and shares a parent. The merged
// chain is `… prev → first → … → last → next → …`: `prev.next`/`first.prev`
// reconnect, `last.next`/`next.prev` reconnect (the `next.prev → last` rewire is
// why `last` is threaded into `unlinked`), and the re-parented children now share
// the grandparent with the slot-level siblings. Every other edge is `m`'s.
proof fn lemma_unlink_siblings(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        cspace_wf(m),
        m.dom().contains(slot),
        last_wf(m, slot, last),
    ensures
        siblings_doubly_consistent(unlinked(m, slot, last)),
        siblings_share_parent(unlinked(m, slot, last)),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    let p = m[slot].parent;
    let pv = m[slot].prev_sib;
    let nx = m[slot].next_sib;
    let fc = m[slot].first_child;
    let head = if fc is None { nx } else { fc };
    assert(siblings_doubly_consistent(m));
    assert(siblings_share_parent(m));
    assert(first_child_parent_agree(m));
    assert(head_is_first_child(m));
    lemma_unlink_roles(m, slot);

    // ── doubly consistent (both directions per node) ──
    assert forall|a: SlotId| #[trigger] mf.dom().contains(a) implies {
        &&& (mf[a].next_sib matches Some(b) ==> mf.dom().contains(b) && mf[b].prev_sib == Some(a))
        &&& (mf[a].prev_sib matches Some(b) ==> mf.dom().contains(b) && mf[b].next_sib == Some(a))
    } by {
        // next direction
        if let Some(b) = mf[a].next_sib {
            if a == slot {
                assert(mf[slot].next_sib is None);
            } else if m[a].parent == Some(slot) {
                if let Some(c) = m[a].next_sib {
                    assert(b == c);
                    assert(m[c].parent == Some(slot));    // share_parent
                    assert(m[c].prev_sib == Some(a));      // doubly
                    assert(c != slot);
                    assert(mf[c].prev_sib == Some(a));
                } else {
                    assert(nx == Some(b));                  // last child → nx
                    assert(m[b].parent == m[slot].parent);  // share_parent slot.next
                    assert(m[b].prev_sib == Some(slot));    // doubly slot.next
                    assert(b != slot);
                    assert(fc is Some);
                    assert(last == Some(a));                // uniqueness: a is a tail child
                    assert(mf[b].prev_sib == Some(a));
                }
            } else {
                if m[a].next_sib == Some(slot) {
                    assert(m[slot].prev_sib == Some(a));    // doubly: a == prev
                    if let Some(f) = fc {
                        assert(head == Some(f) && b == f);
                        assert(m[f].parent == Some(slot));  // agree
                        assert(m[f].prev_sib is None);       // agree
                        assert(f != slot);
                        assert(pv == Some(a));
                        assert(mf[f].prev_sib == Some(a));
                    } else {
                        assert(nx == Some(b));               // head == nx
                        assert(m[b].prev_sib == Some(slot)); // doubly slot.next
                        assert(m[b].parent != Some(slot));
                        assert(b != slot);
                        assert(pv == Some(a));
                        assert(mf[b].prev_sib == Some(a));
                    }
                } else {
                    assert(m[a].next_sib == Some(b));
                    assert(b != slot);
                    assert(m[b].prev_sib == Some(a));        // doubly
                    assert(m[b].parent == m[a].parent);      // share_parent
                    assert(m[b].parent != Some(slot));
                    assert(mf[b].prev_sib == Some(a));
                }
            }
        }
        // prev direction
        if let Some(b) = mf[a].prev_sib {
            if a == slot {
                assert(mf[slot].prev_sib is None);
            } else if m[a].parent == Some(slot) {
                if let Some(c) = m[a].prev_sib {
                    assert(b == c);
                    assert(m[c].next_sib == Some(a));        // doubly
                    assert(m[c].parent == Some(slot));        // share_parent (c.next==a)
                    assert(c != slot);
                    assert(mf[c].next_sib == Some(a));
                } else {
                    assert(pv == Some(b));                    // first child → pv
                    assert(fc == Some(a));                    // head_is_first_child
                    assert(m[b].next_sib == Some(slot));      // doubly slot.prev
                    assert(b != slot);
                    assert(m[b].parent != Some(slot));
                    assert(head == Some(a));
                    assert(mf[b].next_sib == Some(a));
                }
            } else {
                if m[a].prev_sib == Some(slot) {
                    assert(m[slot].next_sib == Some(a));      // doubly: a == next
                    if fc is Some {
                        assert(last == Some(b));               // a==next.prev rewired to last
                        assert(m[b].parent == Some(slot));     // last_wf
                        assert(m[b].next_sib is None);          // last_wf
                        assert(b != slot);
                        assert(nx == Some(a));
                        assert(mf[b].next_sib == Some(a));
                    } else {
                        assert(pv == Some(b));
                        assert(m[b].next_sib == Some(slot));   // doubly slot.prev
                        assert(b != slot);
                        assert(m[b].parent != Some(slot));
                        assert(head == nx && nx == Some(a));
                        assert(mf[b].next_sib == Some(a));
                    }
                } else {
                    assert(m[a].prev_sib == Some(b));
                    assert(b != slot);
                    assert(m[b].next_sib == Some(a));          // doubly
                    assert(m[a].parent == m[b].parent);        // share_parent (b.next==a)
                    assert(m[b].parent != Some(slot));
                    assert(mf[b].next_sib == Some(a));
                }
            }
        }
    }

    // ── share parent: merged-chain nodes all share the grandparent ──
    assert forall|a: SlotId| #[trigger] mf.dom().contains(a) implies
        (mf[a].next_sib matches Some(b) ==> mf[b].parent == mf[a].parent) by {
        if let Some(b) = mf[a].next_sib {
            if a == slot {
                assert(mf[slot].next_sib is None);
            } else if m[a].parent == Some(slot) {
                // a child → b child (share_parent) or b == next (re-parented to share p).
                if let Some(c) = m[a].next_sib {
                    assert(b == c && m[c].parent == Some(slot) && c != slot);
                    assert(mf[a].parent == p && mf[b].parent == p);
                } else {
                    assert(nx == Some(b) && m[b].parent == m[slot].parent && b != slot);
                    assert(mf[a].parent == p && mf[b].parent == p);
                }
            } else if m[a].next_sib == Some(slot) {
                // a == prev → b == head; both end at the grandparent p.
                assert(m[a].parent == m[slot].parent);  // share_parent a.next==slot
                if let Some(f) = fc {
                    assert(head == Some(f) && b == f && m[f].parent == Some(slot) && f != slot);
                    assert(mf[a].parent == p && mf[b].parent == p);
                } else {
                    assert(nx == Some(b) && m[b].parent == m[slot].parent && b != slot);
                    assert(mf[a].parent == p && mf[b].parent == p);
                }
            } else {
                assert(m[a].next_sib == Some(b) && b != slot);
                assert(m[b].parent == m[a].parent);     // share_parent
                assert(m[b].parent != Some(slot) && m[a].parent != Some(slot));
                assert(mf[a].parent == m[a].parent && mf[b].parent == m[b].parent);
            }
        }
    }
}

// The first-child relations survive: the splice keeps `first_child`/`parent`
// agreement and the head/first-child converse, and no node is left a phantom
// parent. `parent_has_first_child`'s one hard case — a non-child whose only child
// was `slot`, childless — uses `lemma_child_on_chain` to show `slot` had a next
// sibling that becomes the new first child (so the parent is not orphaned).
proof fn lemma_unlink_children(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        cspace_wf(m),
        m.dom().finite(),
        m.dom().contains(slot),
        last_wf(m, slot, last),
    ensures
        first_child_parent_agree(unlinked(m, slot, last)),
        head_is_first_child(unlinked(m, slot, last)),
        parent_has_first_child(unlinked(m, slot, last)),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    let p = m[slot].parent;
    let pv = m[slot].prev_sib;
    let nx = m[slot].next_sib;
    let fc = m[slot].first_child;
    let head = if fc is None { nx } else { fc };
    assert(siblings_doubly_consistent(m));
    assert(siblings_share_parent(m));
    assert(first_child_parent_agree(m));
    assert(head_is_first_child(m));
    assert(parent_has_first_child(m));
    lemma_unlink_roles(m, slot);
    let srk = choose|s: Map<SlotId, nat>| valid_srank(m, s);
    assert(valid_srank(m, srk));

    // ── first_child_parent_agree ──
    assert forall|q: SlotId| #[trigger] mf.dom().contains(q) implies
        (mf[q].first_child matches Some(c) ==>
            mf.dom().contains(c) && mf[c].parent == Some(q) && mf[c].prev_sib == None) by {
        if let Some(c) = mf[q].first_child {
            if q == slot {
                assert(mf[slot].first_child is None);
            } else if m[q].parent == Some(slot) {
                // child q keeps its first_child c; c is a non-child of slot.
                assert(m[q].first_child == Some(c));
                assert(m[c].parent == Some(q) && m[c].prev_sib is None);  // agree(m)
                assert(c != slot && m[c].parent != Some(slot));
            } else if m[q].first_child == Some(slot) {
                // q's first child was slot ⟹ slot.parent == Some(q), slot.prev None.
                assert(m[slot].parent == Some(q));   // agree(m)
                assert(m[slot].prev_sib is None);     // agree(m)
                assert(head == Some(c));
                if let Some(f) = fc {
                    assert(c == f && m[f].parent == Some(slot) && m[f].prev_sib is None);
                    assert(f != slot);
                } else {
                    assert(nx == Some(c));
                    assert(m[c].parent == m[slot].parent && m[c].prev_sib == Some(slot));
                    assert(c != slot && m[c].parent != Some(slot));
                }
            } else {
                // q non-child keeps its first_child c (c != slot).
                assert(m[q].first_child == Some(c) && c != slot);
                assert(m[c].parent == Some(q) && m[c].prev_sib is None);  // agree(m)
                assert(m[c].parent != Some(slot));
            }
        }
    }

    // ── head_is_first_child ──
    assert forall|c: SlotId| #[trigger] mf.dom().contains(c) implies
        (mf[c].parent matches Some(pp) ==> (mf[c].prev_sib is None ==>
            mf.dom().contains(pp) && mf[pp].first_child == Some(c))) by {
        if let Some(pp) = mf[c].parent {
            if mf[c].prev_sib is None {
                if c == slot {
                    assert(mf[slot].parent is None);
                } else if m[c].parent == Some(slot) {
                    // child c with no prev ⟹ slot's first child; pp == grandparent.
                    assert(mf[c].prev_sib == (if m[c].prev_sib is None { pv } else { m[c].prev_sib }));
                    assert(m[c].prev_sib is None && pv is None);
                    assert(fc == Some(c));                  // head_is_first_child(m)
                    assert(m[slot].parent == Some(pp));
                    assert(m[pp].parent != Some(slot));      // pp non-child (roles)
                    assert(m[pp].first_child == Some(slot)); // slot is pp's first child
                    assert(head == Some(c));
                } else if m[c].prev_sib == Some(slot) {
                    // c == next, became the new first child (pv None, fc None).
                    assert(pv is None && fc is None);
                    assert(m[c].parent == m[slot].parent && m[slot].parent == Some(pp));
                    assert(m[pp].parent != Some(slot));
                    assert(m[pp].first_child == Some(slot)); // slot first child of pp
                    assert(nx == Some(c) && head == Some(c));
                } else {
                    // c unchanged head in m.
                    assert(m[c].parent == Some(pp) && m[c].prev_sib is None && c != slot);
                    assert(m[pp].first_child == Some(c));    // head_is_first_child(m)
                    assert(pp != slot);
                    if m[pp].parent == Some(slot) {
                        // pp child of slot keeps first_child.
                    } else {
                        assert(m[pp].first_child != Some(slot));  // == Some(c), c != slot
                    }
                }
            }
        }
    }

    // ── parent_has_first_child ──
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) implies
        (mf[k].parent matches Some(pp) ==> mf[pp].first_child is Some) by {
        if let Some(pp) = mf[k].parent {
            if k == slot {
                assert(mf[slot].parent is None);
            } else if m[k].parent == Some(slot) {
                // child k ⟹ pp == grandparent; slot has children ⟹ head Some.
                assert(m[slot].parent == Some(pp) && fc is Some);
                assert(m[pp].parent != Some(slot));   // roles
                assert(m[pp].first_child is Some);     // parent_has_first_child(m)
            } else {
                // non-child k; pp == m[k].parent, has a first child in m.
                assert(m[k].parent == Some(pp) && pp != slot);
                assert(m[pp].first_child is Some);     // parent_has_first_child(m)
                if m[pp].parent == Some(slot) {
                    // pp child of slot keeps its (Some) first_child.
                } else if m[pp].first_child == Some(slot) {
                    // slot was pp's first child; if slot childless, its next sibling
                    // (which exists, since k is another child of pp) is the new head.
                    assert(m[slot].parent == Some(pp));   // agree(m)
                    if fc is None {
                        lemma_child_on_chain(m, pp, k, srk);
                        // every child of pp is reachable from pp.first_child == slot;
                        // k != slot reachable ⟹ slot.next_sib is Some.
                        assert(m[pp].first_child == Some(slot));
                        assert(next_reach(m, slot, k, srk));
                        assert(k != slot);
                        assert(m[slot].next_sib is Some);  // reach from slot to k≠slot
                        assert(nx is Some && head is Some);
                    }
                }
            }
        }
    }
}

// The splice moves links only, never a cap — so the live-slot count is unchanged.
proof fn lemma_unlink_count(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        m.dom().finite(),
    ensures
        count_nonempty(unlinked(m, slot, last)) == count_nonempty(m),
{
    let mf = unlinked(m, slot, last);
    assert(mf.dom() =~= m.dom());
    assert forall|k: SlotId| #[trigger] m.dom().contains(k) implies mf[k].cap == m[k].cap by {}
    assert(m.dom().filter(|k: SlotId| !is_empty_cap(m[k].cap))
        =~= mf.dom().filter(|k: SlotId| !is_empty_cap(mf[k].cap)));
}

// Unlinking preserves the full well-formedness — composed per-clause (the
// transpose family's per-clause SMT discipline). The parent-rank witness is
// reused unchanged (children move up, the gap shrinks); the sibling-rank witness
// is rescaled (lemma_unlink_sib).
proof fn lemma_unlink_preserves_cspace_wf(m: Map<SlotId, CapSlot>, slot: SlotId, last: Option<SlotId>)
    requires
        cspace_wf(m),
        m.dom().finite(),
        m.dom().contains(slot),
        last_wf(m, slot, last),
    ensures
        cspace_wf(unlinked(m, slot, last)),
        unlinked(m, slot, last).dom() == m.dom(),
        unlinked(m, slot, last).dom().finite(),
{
    assert(unlinked(m, slot, last).dom() =~= m.dom());
    lemma_src_no_self_link(m, slot);   // m[slot].parent != Some(slot) for lemma_unlink_sib
    lemma_unlink_links(m, slot, last);
    lemma_unlink_siblings(m, slot, last);
    lemma_unlink_children(m, slot, last);
    lemma_unlink_empty(m, slot, last);
    lemma_unlink_acyclic(m, slot, last);
    lemma_unlink_sib(m, slot, last);
}

// Two nodes reachable from a common start are comparable along the chain — the
// `next_sib` graph is functional, so the walks cannot branch.
proof fn lemma_reach_comparable(m: Map<SlotId, CapSlot>, a: SlotId, x: SlotId, y: SlotId, s: Map<SlotId, nat>)
    requires
        next_reach(m, a, x, s),
        next_reach(m, a, y, s),
    ensures
        next_reach(m, x, y, s) || next_reach(m, y, x, s),
    decreases s[a],
{
    if a == x {
    } else if a == y {
    } else {
        assert(m[a].next_sib is Some);
        let nn = m[a].next_sib->0;
        assert(next_reach(m, nn, x, s));
        assert(next_reach(m, nn, y, s));
        lemma_reach_comparable(m, nn, x, y, s);
    }
}

// The child chain's tail (the child with no next sibling) is unique: any two such
// are comparable (both reachable from the first child), and a node with no next
// reaches only itself.
proof fn lemma_unique_tail(m: Map<SlotId, CapSlot>, slot: SlotId, x: SlotId, y: SlotId, s: Map<SlotId, nat>)
    requires
        cdt_wf(m),
        valid_srank(m, s),
        m.dom().finite(),
        m.dom().contains(slot),
        m.dom().contains(x),
        m.dom().contains(y),
        m[x].parent == Some(slot),
        m[x].next_sib is None,
        m[y].parent == Some(slot),
        m[y].next_sib is None,
    ensures
        x == y,
{
    lemma_child_on_chain(m, slot, x, s);
    lemma_child_on_chain(m, slot, y, s);
    let h = m[slot].first_child->0;
    lemma_reach_comparable(m, h, x, y, s);
    // next_reach(x, y) with x.next None ⟹ x == y (and symmetric).
}

// ── Verified operations (moved here from plain Rust; bodies are unchanged
//    modulo verus-friendly control flow). ──

/// Bump the refcount of the object a cap designates (no-op for bare caps).
///
/// pre:  if the cap designates an object, that object is live and its refcount
///       is below `u32::MAX` (the overflow guard Verus makes explicit — an
///       unchecked refcount bump is a known kernel vulnerability class).
/// post: that object's refcount is +1, all others unchanged; the slot arena is
///       untouched.
pub fn obj_ref<S: Store>(store: &mut S, cap: Cap)
    requires
        cap_obj(cap) matches Some(o) ==> old(store).refs_view().dom().contains(o)
            && old(store).refs_view()[o] < u32::MAX as nat,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        cap_obj(cap) matches Some(o) ==> final(store).refs_view()
            =~= old(store).refs_view().insert(o, (old(store).refs_view()[o] + 1) as nat),
        cap_obj(cap) is None ==> final(store).refs_view() == old(store).refs_view(),
{
    match cap.kind {
        CapKind::Aspace(o)
        | CapKind::CSpace(o)
        | CapKind::Thread(o)
        | CapKind::Channel(o, _)
        | CapKind::Notification(o)
        | CapKind::Timer(o) => {
            let r = store.obj_refs(o);
            store.set_obj_refs(o, r + 1);
        }
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => {}
    }
}

/// Insert `child` as the first child of `parent` (the CDT link surgery `derive`
/// and `retype` use).
///
/// pre:  the cspace is well-formed; `parent` and `child` are distinct live
///       slots; `child` is detached (all four links null) and non-empty;
///       `parent` is non-empty.
/// post: `child` is `parent`'s first child and the previous children follow it
///       in order (the sibling list is spliced in unchanged); caps and refcounts
///       are untouched; the cspace stays well-formed **and acyclic** (the
///       construction-side acyclicity preservation — `child` is seated as a fresh
///       leaf, so a rank witness is re-exhibited; doc/results/21 §9).
pub fn cdt_insert_child<S: Store>(store: &mut S, parent: SlotId, child: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().contains(parent),
        old(store).slot_view().dom().contains(child),
        parent != child,
        old(store).slot_view()[child].parent is None,
        old(store).slot_view()[child].first_child is None,
        old(store).slot_view()[child].next_sib is None,
        old(store).slot_view()[child].prev_sib is None,
        !is_empty_cap(old(store).slot_view()[child].cap),
        !is_empty_cap(old(store).slot_view()[parent].cap),
    ensures
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).refs_view() == old(store).refs_view(),
        // The CDT surgery is pure `set_slot` (which frames `chan_view`), so the
        // channel ghost view is untouched — the frame `retype_install`'s channel arm
        // (plan §3c) needs to carry `chan_view` across the two inserts it threads
        // between `endpoint_cap_added(A)` and `endpoint_cap_added(B)` (doc 28).
        final(store).chan_view() == old(store).chan_view(),
        forall|k: SlotId| #[trigger] old(store).slot_view().dom().contains(k)
            ==> final(store).slot_view()[k].cap == old(store).slot_view()[k].cap,
        final(store).slot_view()[child].cap == old(store).slot_view()[child].cap,
        final(store).slot_view()[parent].first_child == Some(child),
        // the prior child list is spliced in after `child`, in order: `child`
        // heads it (prev None), the old first child follows, and that old first
        // child now points back at `child`.
        final(store).slot_view()[child].parent == Some(parent),
        final(store).slot_view()[child].prev_sib is None,
        final(store).slot_view()[child].next_sib == old(store).slot_view()[parent].first_child,
        old(store).slot_view()[parent].first_child matches Some(f)
            ==> final(store).slot_view()[f].prev_sib == Some(child),
        // The old first child keeps its parent and cap (only its prev_sib was
        // rewritten) — so a second insert under the same parent (retype's channel
        // arm: dst then dst2) leaves the first-inserted child still parented at
        // `parent`, holding its cap (doc 28). These per-slot frames spare callers the
        // `forall` caps-unchanged instantiation for the parent / old first child.
        final(store).slot_view()[parent].cap == old(store).slot_view()[parent].cap,
        old(store).slot_view()[parent].first_child matches Some(f)
            ==> final(store).slot_view()[f].parent == old(store).slot_view()[f].parent,
        old(store).slot_view()[parent].first_child matches Some(f)
            ==> final(store).slot_view()[f].cap == old(store).slot_view()[f].cap,
        cspace_wf(final(store).slot_view()),
{
    let ghost m0 = old(store).slot_view();
    let old_first = store.slot(parent).first_child;

    let mut c = store.slot(child);
    c.parent = Some(parent);
    c.prev_sib = None;
    c.next_sib = old_first;
    store.set_slot(child, c);

    if let Some(f) = old_first {
        proof {
            // f is a live slot (parent's first child) and distinct from child:
            // child is detached (parent None) but f.parent == Some(parent).
            assert(m0.dom().contains(f));
            assert(m0[f].parent == Some(parent));
        }
        let mut fs = store.slot(f);
        fs.prev_sib = Some(child);
        store.set_slot(f, fs);
    }

    let mut p = store.slot(parent);
    p.first_child = Some(child);
    store.set_slot(parent, p);

    // Acyclicity preservation (parent + sibling): only `child` gained a parent
    // edge and a next_sib edge; every other slot's parent/next_sib is untouched
    // (the body edited prev_sib/first_child elsewhere). The lemmas re-exhibit
    // rank witnesses for the new tree.
    proof {
        let m1 = store.slot_view();
        assert(m1.dom() =~= m0.dom());
        assert forall|k: SlotId| m0.dom().contains(k) && k != child
            implies #[trigger] m1[k].parent == m0[k].parent by {}
        lemma_reparent_preserves_acyclic(m0, m1, child, parent);

        assert forall|k: SlotId| m0.dom().contains(k) && k != child
            implies #[trigger] m1[k].next_sib == m0[k].next_sib by {}
        // child's new next_sib (== parent's old first child) is live and not child
        // itself (else child would name parent as parent — but child is detached).
        assert(m1[child].next_sib matches Some(n) ==> m0.dom().contains(n) && n != child);
        // No slot points next_sib at child: in m0 child had prev None, so doubly-
        // consistency forbids any k.next == child; the insert added none.
        assert forall|k: SlotId| m0.dom().contains(k)
            implies #[trigger] m1[k].next_sib != Some(child) by {
            if k != child {
                assert(m1[k].next_sib == m0[k].next_sib);
            }
        }
        lemma_insert_preserves_sib_acyclic(m0, m1, child);
    }
}

/// Derive a child cap (§2.3): copy with rights intersected — the only
/// derivation; there is no amplification path.
///
/// pre:  the cspace is well-formed; `src`/`dst` are live; if `src` designates an
///       object, that object is live (in the refcount table).
/// post: on `Ok`, `dst` holds a faithful copy of `src`'s cap — same kind and
///       designated object (a fresh Frame copy starts unmapped, §2.5) — with
///       rights ∩ `mask`, so its rights are a **subset** of `src`'s for every
///       `mask` (the load-bearing monotone-derivation theorem, proven ∀ rather
///       than sampled); `dst` is `src`'s first child; the object's refcount and
///       slot census both rise by exactly one; the cspace stays well-formed
///       **and acyclic** (`cspace_wf` — `dst` is seated as a fresh leaf).
///       On `Err` (empty/Untyped src, occupied dst, or a refcount already at
///       `u32::MAX`) the store is unchanged. Refusing at the ceiling makes the
///       refcount bump overflow-free for **all** inputs — no unchecked `+ 1`
///       wrap-to-zero (a UAF class); the production `CapCopy` path inherits this.
pub fn derive<S: Store>(store: &mut S, src: SlotId, dst: SlotId, mask: u8) -> (res: Result<(), ()>)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(src),
        old(store).slot_view().dom().contains(dst),
        cap_obj(old(store).slot_view()[src].cap) matches Some(o) ==>
            old(store).refs_view().dom().contains(o),
    ensures
        res is Ok ==> {
            // faithful copy: dst's kind is src's (same object / channel end),
            // a Frame copy unmapped — derivation cannot change the object or
            // amplify via the kind.
            &&& final(store).slot_view()[dst].cap.kind
                  == derived_kind(old(store).slot_view()[src].cap.kind)
            // monotone derivation: dst's rights are src's rights masked, hence a
            // subset for ALL masks — authority only ever shrinks.
            &&& final(store).slot_view()[dst].cap.rights.0
                  == (old(store).slot_view()[src].cap.rights.0 & mask)
            &&& (final(store).slot_view()[dst].cap.rights.0
                  & old(store).slot_view()[src].cap.rights.0)
                  == final(store).slot_view()[dst].cap.rights.0
            &&& cspace_wf(final(store).slot_view())
            &&& final(store).slot_view()[src].first_child == Some(dst)
            &&& (cap_obj(old(store).slot_view()[src].cap) matches Some(o) ==>
                  final(store).refs_view()
                      =~= old(store).refs_view().insert(o, (old(store).refs_view()[o] + 1) as nat))
            // the stored refcount and the slot census rise by one in lockstep —
            // the per-op delta refcount soundness requires (the full
            // `refs == census` invariant, incl. non-slot refs, is deferred).
            &&& (cap_obj(old(store).slot_view()[src].cap) matches Some(o) ==>
                  slot_refs(final(store).slot_view(), o)
                      == slot_refs(old(store).slot_view(), o) + 1)
            // bare caps (no object): no refcount perturbed.
            &&& (cap_obj(old(store).slot_view()[src].cap) is None ==>
                  final(store).refs_view() == old(store).refs_view())
        },
        res is Err ==> {
            &&& final(store).slot_view() == old(store).slot_view()
            &&& final(store).refs_view() == old(store).refs_view()
        },
{
    let ghost m0 = old(store).slot_view();
    let s = store.slot(src);
    if matches!(s.cap.kind, CapKind::Empty) || matches!(s.cap.kind, CapKind::Untyped { .. }) {
        return Err(());
    }
    if !matches!(store.slot(dst).cap.kind, CapKind::Empty) {
        return Err(());
    }
    // On this path src is non-empty and dst is empty, so src != dst, and (by
    // empty_slots_detached) dst is fully detached.
    assert(src != dst);
    assert(is_empty_cap(m0[dst].cap));
    assert(cap_obj(m0[dst].cap) == Option::<ObjId>::None);
    assert(m0[dst].parent is None && m0[dst].first_child is None
        && m0[dst].next_sib is None && m0[dst].prev_sib is None);

    // One mapping per cap copy (§2.5): a fresh frame copy starts unmapped.
    let kind = match s.cap.kind {
        CapKind::Frame { base, pages, mapping: _ } => CapKind::Frame { base, pages, mapping: None },
        k => k,
    };
    let cap = Cap { kind, rights: s.cap.rights.masked(mask) };
    assert(kind == derived_kind(s.cap.kind));
    assert(cap_obj(cap) == cap_obj(s.cap));
    assert(!is_empty_cap(cap));

    // Refuse rather than wrap: an unchecked refcount bump is a UAF class
    // (doc/results/21). Checking here (before any mutation) discharges obj_ref's
    // overflow precondition without trusting the caller, so the bump below is
    // provably total — and the Err path leaves the store untouched.
    let obj_opt = match cap.kind {
        CapKind::Aspace(o)
        | CapKind::CSpace(o)
        | CapKind::Thread(o)
        | CapKind::Channel(o, _)
        | CapKind::Notification(o)
        | CapKind::Timer(o) => Some(o),
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
    };
    assert(obj_opt == cap_obj(cap));
    if let Some(o) = obj_opt {
        if store.obj_refs(o) == u32::MAX {
            return Err(());
        }
    }

    let mut d = store.slot(dst);
    d.cap = cap;
    store.set_slot(dst, d);
    // Setting an empty, detached slot to a non-empty cap (links still null)
    // preserves well-formedness: dst gains no links and no slot links to it.
    let ghost m1 = store.slot_view();
    assert(m1 =~= m0.insert(dst, d));
    assert(cdt_wf(m1));
    // Acyclicity carries: dst joins as a detached node (no parent edge), so the
    // old rank witness still works — cdt_insert_child needs cspace_wf(m1).
    proof {
        let r0 = choose|r: Map<SlotId, nat>| valid_prank(m0, r);
        assert(valid_prank(m0, r0));
        assert(d.parent is None);
        assert(d.next_sib is None);
        assert(m1.dom() =~= m0.dom());
        assert forall|k: SlotId| #[trigger] m1.dom().contains(k)
            implies (m1[k].parent matches Some(p) ==> m1.dom().contains(p) && r0[k] < r0[p]) by {
            if k != dst {
                assert(m1[k] == m0[k]);
            }
        }
        assert(valid_prank(m1, r0));
        assert(acyclic(m1));
        // Sibling-acyclicity carries the same way: dst joins with no next_sib edge.
        let s0 = choose|s: Map<SlotId, nat>| valid_srank(m0, s);
        assert(valid_srank(m0, s0));
        assert forall|k: SlotId| #[trigger] m1.dom().contains(k)
            implies (m1[k].next_sib matches Some(n) ==> m1.dom().contains(n) && s0[n] < s0[k]) by {
            if k != dst {
                assert(m1[k] == m0[k]);
            }
        }
        assert(valid_srank(m1, s0));
        assert(sib_acyclic(m1));
    }
    // set_slot preserves refcounts, and the overflow check above bounded the
    // designated object's count — so obj_ref's bump cannot wrap.
    assert(cap_obj(cap) matches Some(o) ==> store.refs_view()[o] < u32::MAX as nat);

    obj_ref(store, cap);
    // obj_ref touches only refcounts, so the arena (hence cdt_wf and dst's cap)
    // is exactly m1.
    assert(store.slot_view() =~= m1);
    cdt_insert_child(store, src, dst);

    proof {
        // dst's cap survived obj_ref (slot_view unchanged) and cdt_insert_child
        // (child-cap preserved), so it is still the masked copy.
        assert(m1[dst] == d);
        assert(store.slot_view()[dst].cap == cap);
        assert(store.slot_view()[dst].cap.kind == derived_kind(m0[src].cap.kind));
        let r = m0[src].cap.rights.0;
        // monotone-derivation subset corollary: (r & mask) & r == r & mask.
        assert((r & mask) & r == (r & mask)) by (bit_vector);
        // census delta: dst now designates the object, so its slot census rose
        // by one (set_slot), and the link surgery left every cap unchanged.
        match cap_obj(cap) {
            Some(o) => {
                lemma_designation_bump(m0, dst, d, o);
                lemma_same_caps_same_census(m1, store.slot_view(), o);
            }
            None => {}
        }
    }
    Ok(())
}

/// Unlink `slot` from the CDT, re-parenting its children one level up (§2.3).
///
/// **Verified** (doc/results/25, the full body proof). Unlike `slot_move` (a
/// transposition), this is a sibling-list *merge*: a `first_child→next_sib`
/// children walk re-parents each child to `slot`'s parent, then the child chain
/// is spliced into `slot`'s former sibling position and `slot` is detached. The
/// proof shows the final arena equals `unlinked(m0, slot, last)` (the closed-form
/// merge), then reads `cspace_wf` off `lemma_unlink_preserves_cspace_wf` (the
/// parent-rank witness is reused unchanged; the sibling-rank witness is rescaled
/// to fit the child band into the `prev..next` gap — `lemma_unlink_sib`) and the
/// count off `lemma_unlink_count`. The walk re-parents *every* child
/// (`lemma_child_on_chain` completeness; `next_reach` for per-iteration progress
/// and termination); `last` is the chain tail (`lemma_unique_tail`). The contract
/// is also host-test-checked (test_store). `pub(crate)`: no callers outside `kcore`.
pub(crate) fn cdt_unlink<S: Store>(store: &mut S, slot: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        final(store).refs_view() == old(store).refs_view(),
        // `slot`'s cap is untouched (unlink moves links, not the cap); `slot` ends
        // fully detached. So the live-slot count is unchanged (it is `delete` that
        // empties the cap).
        final(store).slot_view()[slot].cap == old(store).slot_view()[slot].cap,
        final(store).slot_view()[slot].parent is None,
        final(store).slot_view()[slot].first_child is None,
        final(store).slot_view()[slot].next_sib is None,
        final(store).slot_view()[slot].prev_sib is None,
        count_nonempty(final(store).slot_view()) == count_nonempty(old(store).slot_view()),
{
    let ghost m0 = old(store).slot_view();
    let ghost r0 = old(store).refs_view();
    let ghost srk = choose|s: Map<SlotId, nat>| valid_srank(m0, s);
    proof {
        assert(cdt_wf(m0));
        assert(valid_srank(m0, srk));
        assert(parent_has_first_child(m0));
        lemma_src_no_self_link(m0, slot);
    }

    let s = store.slot(slot);
    let parent = s.parent;
    let prev = s.prev_sib;
    let next = s.next_sib;
    let first = s.first_child;

    // The all-children-re-parented arena: the children walk's postcondition.
    let ghost mw = Map::new(
        |k: SlotId| m0.dom().contains(k),
        |k: SlotId| if m0[k].parent == Some(slot) { set_parent(m0[k], parent) } else { m0[k] },
    );

    // ── children walk: re-parent every child to `parent`; record the tail. ──
    proof {
        if let Some(h) = first {
            assert forall|x: SlotId| m0.dom().contains(x) && m0[x].parent == Some(slot)
                implies next_reach(m0, h, x, srk) by {
                lemma_child_on_chain(m0, slot, x, srk);
            }
        }
    }
    let mut last: Option<SlotId> = None;
    let mut c = first;
    while c.is_some()
        invariant
            store.slot_view().dom() == m0.dom(),
            store.slot_view().dom().finite(),
            store.refs_view() == r0,
            cspace_wf(m0),
            valid_srank(m0, srk),
            parent == m0[slot].parent,
            first == m0[slot].first_child,
            m0.dom().contains(slot),
            forall|k: SlotId| #[trigger] m0.dom().contains(k) && m0[k].parent != Some(slot)
                ==> store.slot_view()[k] == m0[k],
            c matches Some(cur) ==> {
                &&& m0.dom().contains(cur)
                &&& m0[cur].parent == Some(slot)
                &&& forall|x: SlotId| #[trigger] m0.dom().contains(x) && m0[x].parent == Some(slot)
                        && next_reach(m0, cur, x, srk) ==> store.slot_view()[x] == m0[x]
                &&& forall|x: SlotId| #[trigger] m0.dom().contains(x) && m0[x].parent == Some(slot)
                        && !next_reach(m0, cur, x, srk) ==> store.slot_view()[x] == set_parent(m0[x], parent)
            },
            c is None ==> forall|x: SlotId| #[trigger] m0.dom().contains(x)
                && m0[x].parent == Some(slot) ==> store.slot_view()[x] == set_parent(m0[x], parent),
            last matches Some(l) ==> m0.dom().contains(l) && m0[l].parent == Some(slot) && m0[l].next_sib == c,
            last is None ==> c == first,
        decreases match c {
            Some(cur) => (srk[cur] + 1) as nat,
            None => 0nat,
        },
    {
        let cur = c.unwrap();
        proof {
            assert(next_reach(m0, cur, cur, srk));
            assert(store.slot_view()[cur] == m0[cur]);
        }
        let mut cs = store.slot(cur);
        cs.parent = parent;
        let nx = cs.next_sib;
        proof {
            assert(cs == set_parent(m0[cur], parent));
            assert(nx == m0[cur].next_sib);
        }
        store.set_slot(cur, cs);
        proof {
            assert(siblings_share_parent(m0));
            assert(links_in_domain(m0));
            match nx {
                Some(nn) => {
                    assert(m0[cur].next_sib == Some(nn));
                    assert(m0[nn].parent == Some(slot));   // share_parent
                    assert(m0.dom().contains(nn));          // links_in_domain
                    assert(srk[nn] < srk[cur]);             // valid_srank
                    if next_reach(m0, nn, cur, srk) {
                        lemma_next_reach_sr(m0, nn, cur, srk);
                    }
                    assert forall|x: SlotId| x != cur
                        implies #[trigger] next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk) by {}
                }
                None => {
                    assert(m0[cur].next_sib is None);
                }
            }
        }
        last = Some(cur);
        c = nx;
    }

    // ── post-walk: the arena is `mw`; `last` is the unique chain tail. ──
    assert(store.slot_view() =~= mw);
    proof {
        assert(m0[slot].first_child is None <==> last is None);
        assert forall|x: SlotId| m0.dom().contains(x) && m0[x].parent == Some(slot) && m0[x].next_sib is None
            implies last == Some(x) by {
            assert(parent_has_first_child(m0));
            assert(m0[slot].first_child is Some);   // x is a child
            assert(last is Some);
            lemma_unique_tail(m0, slot, x, last->0, srk);
        }
        assert(last_wf(m0, slot, last));
        lemma_unlink_roles(m0, slot);
    }

    let head = if first.is_none() { next } else { first };

    // The straight-line splice maps (the spec mirror of the four fixups).
    let ghost ma = match prev {
        Some(pv) => mw.insert(pv, set_next_sib(mw[pv], head)),
        None => match parent {
            Some(pa) => mw.insert(pa, set_first_child(mw[pa], head)),
            None => mw,
        },
    };
    let ghost mb = match head {
        Some(h) => ma.insert(h, set_prev_sib(ma[h], prev)),
        None => ma,
    };
    let ghost mc = match first {
        Some(_) => mb.insert(last->0, set_next_sib(mb[last->0], next)),
        None => mb,
    };
    let ghost md = match first {
        Some(_) => match next {
            Some(nx) => mc.insert(nx, set_prev_sib(mc[nx], last)),
            None => mc,
        },
        None => mc,
    };

    if let Some(pv) = prev {
        proof {
            assert(m0[slot].prev_sib == Some(pv));
            assert(m0.dom().contains(pv));        // links_in_domain
            assert(m0[pv].parent != Some(slot));  // roles: prev is a non-child
        }
        let mut ps = store.slot(pv);
        ps.next_sib = head;
        proof { assert(ps == set_next_sib(mw[pv], head)); }
        store.set_slot(pv, ps);
        proof { assert(store.slot_view() =~= ma); }
    } else if let Some(pa) = parent {
        proof {
            assert(m0[slot].parent == Some(pa));
            assert(m0.dom().contains(pa));
            assert(m0[pa].parent != Some(slot));  // roles: grandparent is a non-child
        }
        let mut pas = store.slot(pa);
        pas.first_child = head;
        proof { assert(pas == set_first_child(mw[pa], head)); }
        store.set_slot(pa, pas);
        proof { assert(store.slot_view() =~= ma); }
    } else {
        proof { assert(store.slot_view() =~= ma); }
    }

    if let Some(h) = head {
        proof { assert(m0.dom().contains(h)); }   // head is first/next, both in dom
        let mut hs = store.slot(h);
        hs.prev_sib = prev;
        proof { assert(hs == set_prev_sib(ma[h], prev)); }
        store.set_slot(h, hs);
        proof { assert(store.slot_view() =~= mb); }
    } else {
        proof { assert(store.slot_view() =~= mb); }
    }

    if first.is_some() {
        proof { assert(last is Some); }   // first Some <==> last Some (last_wf)
        let l = last.unwrap();
        proof {
            assert(m0[l].parent == Some(slot));   // last_wf
            assert(m0.dom().contains(l));
        }
        let mut ls = store.slot(l);
        ls.next_sib = next;
        proof { assert(ls == set_next_sib(mb[l], next)); }
        store.set_slot(l, ls);
        proof { assert(store.slot_view() =~= mc); }
        if let Some(nx) = next {
            proof {
                assert(m0[slot].next_sib == Some(nx));
                assert(m0.dom().contains(nx));
                assert(m0[nx].parent != Some(slot));   // roles: next is a non-child
            }
            let mut ns = store.slot(nx);
            ns.prev_sib = last;
            proof { assert(ns == set_prev_sib(mc[nx], last)); }
            store.set_slot(nx, ns);
            proof { assert(store.slot_view() =~= md); }
        } else {
            proof { assert(store.slot_view() =~= md); }
        }
    } else {
        proof { assert(store.slot_view() =~= md); }
    }

    let mut s = store.slot(slot);
    s.parent = None;
    s.first_child = None;
    s.next_sib = None;
    s.prev_sib = None;
    store.set_slot(slot, s);

    proof {
        let mfin = store.slot_view();
        // `slot` is untouched by the splice (distinct from every fixup target),
        // so its cap rode through; the clear lands the detached empty-links slot.
        assert(md[slot] == m0[slot]);
        assert(mfin =~= md.insert(slot, mfin[slot]));
        // The final arena is exactly the closed-form merge `unlinked`.
        assert(mfin =~= unlinked(m0, slot, last)) by {
            let un = unlinked(m0, slot, last);
            assert forall|k: SlotId| m0.dom().contains(k) implies mfin[k] == un[k] by {
                if k == slot {
                } else if m0[k].parent == Some(slot) {
                    // ── a child: re-parented; first child gains prev=pv, tail next=nx ──
                    assert(m0[k].parent != Some(slot) == false);
                    assert(first is Some && head == first);   // slot has children
                    // k is none of the non-child / slot fixup targets.
                    assert(k != slot);
                    // k == first ⟺ k has no prev; k is the tail ⟺ k has no next.
                    assert((first == Some(k)) <==> (m0[k].prev_sib is None)) by {
                        if m0[k].prev_sib is None {
                            assert(head_is_first_child(m0));
                        }
                        if first == Some(k) {
                            assert(first_child_parent_agree(m0));
                        }
                    }
                    assert((last == Some(k)) <==> (m0[k].next_sib is None));
                } else {
                    // ── a non-child (≠ slot): apply the matching neighbour fixup ──
                    if m0[k].next_sib == Some(slot) {
                        assert(prev == Some(k));               // k == slot's prev
                    } else if m0[k].prev_sib == Some(slot) {
                        assert(next == Some(k));               // k == slot's next
                        assert(m0[slot].next_sib == Some(k));  // doubly
                    } else if m0[k].first_child == Some(slot) {
                        assert(m0[slot].parent == Some(k));    // k == grandparent
                        assert(m0[slot].prev_sib is None);     // slot is k's first child
                        assert(prev is None);
                    } else {
                        // untouched: no link names slot.
                    }
                }
            }
        }
        lemma_unlink_preserves_cspace_wf(m0, slot, last);
        lemma_unlink_count(m0, slot, last);
    }
}

/// Move a cap between slots, preserving its CDT position (§3.4: send and receive
/// move caps; a move is the same cap relocating, not a derivation).
///
/// **Verified** (doc/results/24, the full body proof). The body's whole effect is
/// the identity transposition π=(src dst): because nothing references the isolated
/// empty `dst` (`lemma_nothing_points_to_empty`), copying `src`'s slot onto `dst`
/// verbatim and redirecting `src`'s four neighbour classes (parent / prev / next /
/// children) is exactly the renaming `relabeled(m0, src, dst)`, followed by
/// clearing `src`. The proof shows the final arena equals
/// `relabeled(m0, src, dst).insert(src, CapSlot::empty())`, then reads off
/// `cspace_wf` from `lemma_transpose_preserves_cspace_wf` + `lemma_replace_empty_cap`
/// and the count from `lemma_move_count`. The children-walk re-parents *every*
/// child (`lemma_child_on_chain` completeness; `next_reach` for per-iteration
/// progress and termination). The contract is also host-test-checked (test_store).
pub fn slot_move<S: Store>(store: &mut S, src: SlotId, dst: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(src),
        old(store).slot_view().dom().contains(dst),
        src != dst,
        !is_empty_cap(old(store).slot_view()[src].cap),
        is_empty_cap(old(store).slot_view()[dst].cap),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        final(store).refs_view() == old(store).refs_view(),
        // The channel view is untouched: the body mutates only via `set_slot`,
        // which frames `chan_view` unchanged. 3d's `send`/`recv` rely on this —
        // without it a `slot_move` call havocs every channel cursor (detail §1.1).
        final(store).chan_view() == old(store).chan_view(),
        // Likewise the notification/TCB/timer views (plan §4b): `set_slot` frames all of
        // them, so a queued-cap move preserves `binding_notif_wf` for `send`/`recv`.
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).slot_view()[dst].cap == old(store).slot_view()[src].cap,
        is_empty_cap(final(store).slot_view()[src].cap),
        // The cap-content frame: only `src`/`dst` change cap; the neighbour fixups
        // touch CDT *link* fields, never `.cap`. 3d's `send`/`recv` need this to
        // know a queued-cap move leaves every other ring slot's content alone.
        forall|x: SlotId| old(store).slot_view().dom().contains(x) && x != src && x != dst
            ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap,
        count_nonempty(final(store).slot_view()) == count_nonempty(old(store).slot_view()),
{
    let ghost m0 = old(store).slot_view();
    let ghost r0 = old(store).refs_view();
    let ghost cv0 = old(store).chan_view();
    let ghost nv0 = old(store).notif_view();
    let ghost tv0 = old(store).tcb_view();
    let ghost tmv0 = old(store).timer_view();
    let ghost th0 = old(store).timer_head_view();
    let ghost srk = choose|s: Map<SlotId, nat>| valid_srank(m0, s);
    // The transposition renaming. Every slot but `src` lands on `rl[k]`; `src`
    // ends emptied (cleared below). `rl[dst] == m0[src]` (lemma_dst_relabeled).
    let ghost rl = relabeled(m0, src, dst);
    proof {
        assert(sib_acyclic(m0));
        assert(valid_srank(m0, srk));
        assert(links_in_domain(m0));
        assert(siblings_doubly_consistent(m0));
        assert(siblings_share_parent(m0));
        assert(first_child_parent_agree(m0));
        assert(head_is_first_child(m0));
        assert(empty_slots_detached(m0));
        lemma_src_no_self_link(m0, src);
        lemma_nothing_points_to_empty(m0, dst);
        lemma_dst_relabeled(m0, src, dst);
    }

    let s = store.slot(src);

    let mut d = store.slot(dst);
    d.cap = s.cap;
    d.parent = s.parent;
    d.first_child = s.first_child;
    d.next_sib = s.next_sib;
    d.prev_sib = s.prev_sib;
    proof {
        assert(d == m0[src]);
        assert(rl[dst] == m0[src]);
    }
    store.set_slot(dst, d);
    let ghost m1 = m0.insert(dst, d);
    assert(store.slot_view() =~= m1);

    // The straight-line intermediate maps (the spec mirror of the four fixups).
    let ghost m2 = match m0[src].parent {
        Some(pa) => if m1[pa].first_child == Some(src) {
            m1.insert(pa, set_first_child(m1[pa], Some(dst)))
        } else {
            m1
        },
        None => m1,
    };
    let ghost m3 = match m0[src].prev_sib {
        Some(pv) => m2.insert(pv, set_next_sib(m2[pv], Some(dst))),
        None => m2,
    };
    let ghost m4 = match m0[src].next_sib {
        Some(nx) => m3.insert(nx, set_prev_sib(m3[nx], Some(dst))),
        None => m3,
    };

    // Each fixup proves `store == m_i` *inside* the block — the neighbour handle
    // (pa/pv/nx) is in scope only there, so the map equality can't be reconstructed
    // afterwards.
    if let Some(pa) = d.parent {
        proof {
            assert(m0[src].parent == Some(pa));
            assert(m0.dom().contains(pa));   // links_in_domain
            assert(pa != dst);
        }
        let mut pas = store.slot(pa);
        // Compare via the u64 tag: `SlotId`/`Option` are external types, so the
        // exec `==` operator carries no spec — but `.0 == .0` is native u64 eq.
        let fc_is_src = match pas.first_child {
            Some(c) => c.0 == src.0,
            None => false,
        };
        proof {
            assert(pas.first_child == m1[pa].first_child);
            match m1[pa].first_child {
                Some(c) => {
                    assert(c == src <==> c.0 == src.0);
                    assert((m1[pa].first_child == Some(src)) == (c == src));
                }
                None => {}
            }
            assert(fc_is_src == (m1[pa].first_child == Some(src)));
        }
        if fc_is_src {
            pas.first_child = Some(dst);
            proof { assert(pas == set_first_child(m1[pa], Some(dst))); }
            store.set_slot(pa, pas);
            proof { assert(store.slot_view() =~= m2); }
        } else {
            proof { assert(store.slot_view() =~= m2); }
        }
    } else {
        proof { assert(store.slot_view() =~= m2); }
    }

    if let Some(pv) = d.prev_sib {
        proof {
            assert(m0[src].prev_sib == Some(pv));
            assert(m0.dom().contains(pv));
            assert(pv != dst);
        }
        let mut pvs = store.slot(pv);
        pvs.next_sib = Some(dst);
        proof { assert(pvs == set_next_sib(m2[pv], Some(dst))); }
        store.set_slot(pv, pvs);
        proof { assert(store.slot_view() =~= m3); }
    } else {
        proof { assert(store.slot_view() =~= m3); }
    }

    if let Some(nx) = d.next_sib {
        proof {
            assert(m0[src].next_sib == Some(nx));
            assert(m0.dom().contains(nx));
            assert(nx != dst);
        }
        let mut nxs = store.slot(nx);
        nxs.prev_sib = Some(dst);
        proof { assert(nxs == set_prev_sib(m3[nx], Some(dst))); }
        store.set_slot(nx, nxs);
        proof { assert(store.slot_view() =~= m4); }
    } else {
        proof { assert(store.slot_view() =~= m4); }
    }

    // ── C1/C2/C3: characterize the straight-line map m4 against m0 and tgt. ──
    proof {
        let r = choose|r: Map<SlotId, nat>| valid_prank(m0, r);
        assert(valid_prank(m0, r));

        // C1: src is a non-child untouched by the fixups.
        assert(m0[src].parent != Some(src));
        assert(m0[src].prev_sib != Some(src));
        assert(m0[src].next_sib != Some(src));
        assert(m4[src] == m0[src]);

        // C2: each child of src is untouched (parent flip happens in the loop).
        assert forall|x: SlotId| m0.dom().contains(x) && m0[x].parent == Some(src)
            implies #[trigger] m4[x] == m0[x] by {
            // x != src/dst, and x is none of pa/pv/nx (else src.parent == src etc.).
            if x == src { assert(r[src] < r[src]); }
            assert(x != dst);
            if m0[src].parent == Some(x) { assert(r[src] < r[x]); assert(r[x] < r[src]); }
            if m0[src].prev_sib == Some(x) {
                assert(m0[x].next_sib == Some(src));
                assert(m0[src].parent == m0[x].parent);
            }
            if m0[src].next_sib == Some(x) {
                assert(m0[x].parent == m0[src].parent);
            }
        }

        // C3: each non-child slot other than src lands on its renamed value rl[k].
        assert forall|k: SlotId| m0.dom().contains(k) && k != src && m0[k].parent != Some(src)
            implies #[trigger] m4[k] == rl[k] by {
            if k == dst {
                assert(m0[src].parent != Some(dst));
                assert(m0[src].prev_sib != Some(dst));
                assert(m0[src].next_sib != Some(dst));
                assert(m4[dst] == m1[dst]);
                assert(rl[dst] == m0[src]);
            } else {
                lemma_generic_relabeled(m0, src, dst, k);
                // The fixup conditions match the renaming's via the consistency
                // clauses: k.prev==src ⟺ src.next==k, k.next==src ⟺ src.prev==k,
                // k.first_child==src ⟹ src.parent==k.
                assert(m0[k].prev_sib == Some(src) <==> m0[src].next_sib == Some(k));
                assert(m0[k].next_sib == Some(src) <==> m0[src].prev_sib == Some(k));
                assert(m0[k].first_child == Some(src) ==> m0[src].parent == Some(k));
                assert(m4[k].cap == rl[k].cap);
                assert(m4[k].parent == rl[k].parent);
                assert(m4[k].first_child == rl[k].first_child);
                assert(m4[k].next_sib == rl[k].next_sib);
                assert(m4[k].prev_sib == rl[k].prev_sib);
                assert(m4[k] == rl[k]);
            }
        }
    }

    // ── The children walk: re-parent every child of src to dst. ──
    proof {
        // Completeness: every child is next_sib-reachable from the first child,
        // so the loop's entry "done" branch is vacuous.
        if let Some(h) = m0[src].first_child {
            assert forall|x: SlotId| m0.dom().contains(x) && m0[x].parent == Some(src)
                implies next_reach(m0, h, x, srk) by {
                lemma_child_on_chain(m0, src, x, srk);
            }
        }
    }
    let mut c = d.first_child;
    while c.is_some()
        invariant
            store.slot_view().dom() == m0.dom(),
            store.slot_view().dom().finite(),
            store.refs_view() == r0,
            store.chan_view() == cv0,
            store.notif_view() == nv0,
            store.tcb_view() == tv0,
            store.timer_view() == tmv0,
            store.timer_head_view() == th0,
            cspace_wf(m0),
            valid_srank(m0, srk),
            rl == relabeled(m0, src, dst),
            src != dst,
            is_empty_cap(m0[dst].cap),
            m0.dom().contains(src),
            m0.dom().contains(dst),
            forall|k: SlotId| #[trigger] m0.dom().contains(k) && m0[k].parent != Some(src)
                ==> store.slot_view()[k] == m4[k],
            c matches Some(cur) ==> {
                &&& m0.dom().contains(cur)
                &&& m0[cur].parent == Some(src)
                &&& forall|x: SlotId| #[trigger] m0.dom().contains(x) && m0[x].parent == Some(src)
                        && next_reach(m0, cur, x, srk) ==> store.slot_view()[x] == m0[x]
                &&& forall|x: SlotId| #[trigger] m0.dom().contains(x) && m0[x].parent == Some(src)
                        && !next_reach(m0, cur, x, srk) ==> store.slot_view()[x] == rl[x]
            },
            c is None ==> forall|x: SlotId| #[trigger] m0.dom().contains(x)
                && m0[x].parent == Some(src) ==> store.slot_view()[x] == rl[x],
        decreases match c {
            Some(cur) => (srk[cur] + 1) as nat,
            None => 0nat,
        },
    {
        let cur = c.unwrap();
        proof {
            assert(next_reach(m0, cur, cur, srk));
            assert(store.slot_view()[cur] == m0[cur]);
        }
        let mut cs = store.slot(cur);
        cs.parent = Some(dst);
        let nx = cs.next_sib;
        proof {
            assert(cs == set_parent(m0[cur], Some(dst)));
            lemma_child_relabeled(m0, src, dst, cur);
            assert(rl[cur].cap == m0[cur].cap);
            assert(rl[cur].parent == Some(dst));
            assert(rl[cur].first_child == m0[cur].first_child);
            assert(rl[cur].next_sib == m0[cur].next_sib);
            assert(rl[cur].prev_sib == m0[cur].prev_sib);
            assert(cs == rl[cur]);
            assert(nx == m0[cur].next_sib);
        }
        store.set_slot(cur, cs);
        proof {
            assert(siblings_share_parent(m0));
            assert(links_in_domain(m0));
            match nx {
                Some(nn) => {
                    assert(m0[cur].next_sib == Some(nn));
                    assert(m0[nn].parent == Some(src));   // share_parent
                    assert(m0.dom().contains(nn));         // links_in_domain
                    assert(srk[nn] < srk[cur]);            // valid_srank
                    // cur is not reachable from nn (rank strictly drops along reach).
                    if next_reach(m0, nn, cur, srk) {
                        lemma_next_reach_sr(m0, nn, cur, srk);
                    }
                    // Peel: for x != cur, reach(cur,x) ⟺ reach(nn,x).
                    assert forall|x: SlotId| x != cur
                        implies #[trigger] next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk) by {}
                }
                None => {
                    // reach(cur,x) for x != cur needs cur.next == Some(..); it is None.
                    assert(m0[cur].next_sib is None);
                }
            }
        }
        c = nx;
    }

    proof {
        // Post-loop: children are all at rl, non-children still at m4 (== rl for
        // k != src by C3). So everything but src is already rl.
        assert forall|k: SlotId| #[trigger] m0.dom().contains(k) && k != src
            implies store.slot_view()[k] == rl[k] by {}
    }
    store.set_slot(src, CapSlot::empty());
    proof {
        let mfin = store.slot_view();
        // mfin == rl.insert(src, mfin[src]): k != src is already rl[k] (above) and
        // untouched by the final clear; src holds the freshly-cleared empty slot.
        assert(is_empty_cap(mfin[src].cap));
        assert(mfin[src].parent is None);
        assert(mfin[src].first_child is None);
        assert(mfin[src].next_sib is None);
        assert(mfin[src].prev_sib is None);
        assert forall|k: SlotId| #[trigger] m0.dom().contains(k) && k != src
            implies mfin[k] == rl[k] by {}
        assert(mfin =~= rl.insert(src, mfin[src]));
        // cspace_wf: the transposition preserves it; replacing rl[src] (empty, all
        // links None) with the cleared `src` slot (same shape) keeps it.
        lemma_transpose_preserves_cspace_wf(m0, src, dst);
        assert(m0[dst].parent is None);
        assert(m0[dst].first_child is None);
        assert(m0[dst].next_sib is None);
        assert(m0[dst].prev_sib is None);
        assert(rl[src].cap == m0[dst].cap);
        assert(rl[src].parent is None);
        assert(rl[src].first_child is None);
        assert(rl[src].next_sib is None);
        assert(rl[src].prev_sib is None);
        lemma_replace_empty_cap(rl, src, mfin[src]);
        assert(cspace_wf(rl.insert(src, mfin[src])));
        assert(cspace_wf(mfin));
        // count unchanged (the live set loses src, gains dst).
        assert forall|k: SlotId| m0.dom().contains(k) && k != src && k != dst
            implies #[trigger] is_empty_cap(mfin[k].cap) == is_empty_cap(m0[k].cap) by {
            lemma_generic_relabeled(m0, src, dst, k);
        }
        // The cap-content frame (above): mfin[k] == rl[k] (k != src) and
        // rl[k].cap == m0[k].cap (k != src, k != dst).
        assert forall|x: SlotId| m0.dom().contains(x) && x != src && x != dst
            implies #[trigger] mfin[x].cap == m0[x].cap by {
            lemma_generic_relabeled(m0, src, dst, x);
        }
        lemma_move_count(m0, mfin, src, dst);
        // dst inherits src's cap (rl[dst] == m0[src]).
        assert(mfin[dst] == m0[src]);
    }
}

// ── Cross-object teardown: the refcount plumbing (plan §6c, doc/results/43) ───────
//
// `obj_unref`/`unref_cspace`/`destroy_cspace` and the shared `dec_ref` helper — the
// teardown members that recurse only through the *opaque* `delete`. With
// `delete`/`destroy_channel`/`destroy_tcb` still `external_body`, Verus sees no recursion
// cycle, so these verify against `delete`'s contract with plain index-countdown loops (no
// cross-module `decreases` — that is 6d). The load-bearing invariant is `refcount_sound`:
// it is the underflow gate for every `refs - 1` (§1.3) and, at the zero point, its census
// pins the *structural* emptiness each destructor's `requires` needs (no waiters, no armed
// timers, …) — the §6c headline.

/// Drop one reference to object `o` and restore the census (plan §6c). The shared
/// decrement step `obj_unref`/`unref_cspace` factor out: the caller hands an **off-by-one**
/// state — `refs[o] == census(o) + 1`, sound everywhere else (it already cleared the
/// reference that named `o`) — and the `-1` lands the matching decrement, restoring the
/// full `refcount_sound` invariant. Census-transparent (every object view framed), so the
/// caller can dispatch the at-zero destructor against an unmoved census. The `unref_aspace`
/// proof shape (doc/results/42), minus the aspace-specific last-ref `aspace_destroy`.
fn dec_ref<S: Store>(store: &mut S, o: ObjId)
    requires
        old(store).refs_view().dom().contains(o),
        old(store).refs_view()[o] > 0,
        old(store).refs_view()[o] == obj_census(old(store), o) + 1,
        forall|x: ObjId| x != o && old(store).refs_view().dom().contains(x)
            ==> #[trigger] old(store).refs_view()[x] == obj_census(old(store), x),
        // The cap→object invariant rides through unchanged — it reads only object views, all
        // framed by `set_obj_refs` (plan §6d foundation).
        caps_consistent(old(store)),
    ensures
        refcount_sound(final(store)),
        final(store).refs_view() == old(store).refs_view().insert(
            o, (old(store).refs_view()[o] - 1) as nat),
        final(store).refs_view().dom() == old(store).refs_view().dom(),
        final(store).refs_view()[o] == old(store).refs_view()[o] - 1,
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        caps_consistent(final(store)),
{
    let r = store.obj_refs(o);
    store.set_obj_refs(o, r - 1);
    proof {
        // Every census view is framed by `set_obj_refs`, so the recount is invariant.
        assert forall|x: ObjId| #[trigger] obj_census(final(store), x)
            == obj_census(old(store), x) by {}
        // `o`'s term moved with the `-1`; every other object's refs and census are both
        // untouched, so the off-by-one precondition carries to full soundness.
        assert forall|x: ObjId| final(store).refs_view().dom().contains(x)
            implies #[trigger] final(store).refs_view()[x] == obj_census(final(store), x) by {}
        // `caps_consistent` is refs-free: every per-cap clause reads only the object views
        // `set_obj_refs` left equal to `old`, so each live cap's consistency carries over.
        assert forall|s: SlotId| #![trigger final(store).slot_view()[s]]
            final(store).slot_view().dom().contains(s)
                && !is_empty_cap(final(store).slot_view()[s].cap)
            implies cap_consistent(final(store), final(store).slot_view()[s].cap) by {
            assert(cap_consistent(old(store), old(store).slot_view()[s].cap));
        }
    }
}

/// Tear a cspace down once its last cap is gone (`refs == 0`): delete every cap it still
/// holds (its residents), each through the ordinary CDT cleanup (plan §6c). The loop reads
/// residency through the immutable `cspace_view` and re-reads each slot's emptiness, so a
/// resident already emptied by a sibling's teardown is skipped.
///
/// `delete` is **opaque** here (`external_body`), so there is no visible recursion: the loop
/// `decreases` is the resident-index countdown, and `delete`'s contract re-establishes
/// `cspace_wf`/`refcount_sound`/dom (and frames residency) each iteration. The loop invariant
/// is designed so 6d's visible-`delete` re-verification reuses it unchanged.
///
/// `pub(crate)` so the proof harness can drive the resident loop directly
/// (`check_destroy_cspace`); it has no callers outside this crate.
pub(crate) fn destroy_cspace<S: Store>(store: &mut S, cs: ObjId)
    requires
        old(store).refs_view().dom().contains(cs),
        old(store).refs_view()[cs] == 0,
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        refcount_sound(old(store)),
        caps_consistent(old(store)),
        cspace_resident_wf(old(store), cs),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        count_nonempty(final(store).slot_view()) <= count_nonempty(old(store).slot_view()),
        refcount_sound(final(store)),
        caps_consistent(final(store)),
{
    let n = store.cspace_num_slots(cs);
    let mut i: u32 = 0;
    while i < n
        invariant
            0 <= i <= n,
            n == old(store).cspace_view()[cs].num_slots,
            cspace_wf(store.slot_view()),
            store.slot_view().dom() == old(store).slot_view().dom(),
            store.slot_view().dom().finite(),
            count_nonempty(store.slot_view()) <= count_nonempty(old(store).slot_view()),
            refcount_sound(store),
            // `delete` (assumed `external_body`) preserves the cap→object invariant, so the
            // resident-walk maintains it for the next iteration's `delete` (plan §6d).
            caps_consistent(store),
            // Residency is immutable — `delete` frames `cspace_view`, and dom is preserved,
            // so `cs`'s residents stay live and the getters stay in-bounds across the loop.
            store.cspace_view() == old(store).cspace_view(),
            cspace_resident_wf(store, cs),
        decreases n - i,
    {
        let sid = store.cspace_slot(cs, i);
        if !cap_is_empty(store.slot(sid).cap) {
            delete(store, sid);
        }
        i += 1;
    }
    // Memory returns to the donor untyped only via revoke of the untyped cap; no
    // allocator hands it back early (§3.2).
}

/// Drop the refcount a cap holds on its object; at zero, run the type-specific teardown
/// (plan §6c). The shared decrement (`dec_ref`) carries the off-by-one census; at the zero
/// point `refcount_sound` ⟹ `census(o) == 0`, which discharges each destructor's structural
/// precondition (no waiters for `destroy_notif`; no self-bound armed timer for
/// `destroy_timer`; …). The per-`CapKind` `requires` carry the well-formedness each
/// destructor needs; `delete` (6d) — `obj_unref`'s only kcore caller — establishes them.
///
/// `pub(crate)` so the proof harness can drive the dispatch directly (`check_obj_unref`);
/// `delete` is its only production caller.
pub(crate) fn obj_unref<S: Store>(store: &mut S, cap: Cap)
    requires
        cspace_wf(old(store).slot_view()),
        // Non-designating caps (Empty/Untyped/Frame): a pure no-op, census already sound.
        cap_obj(cap) is None ==> refcount_sound(old(store)),
        // Designating caps: the off-by-one census at `o` (the caller cleared `o`'s
        // designating slot first), sound everywhere else — `dec_ref`'s precondition.
        cap_obj(cap) matches Some(o) ==> {
            &&& old(store).refs_view().dom().contains(o)
            &&& old(store).refs_view()[o] > 0
            &&& old(store).refs_view()[o] == obj_census(old(store), o) + 1
            &&& forall|x: ObjId| x != o && old(store).refs_view().dom().contains(x)
                    ==> #[trigger] old(store).refs_view()[x] == obj_census(old(store), x)
        },
        // Per-kind well-formedness each at-zero destructor's `requires` needs.
        cap.kind matches CapKind::CSpace(o) ==> {
            &&& old(store).slot_view().dom().finite()
            &&& cspace_resident_wf(old(store), o)
        },
        cap.kind matches CapKind::Channel(o, _) ==>
            chan_wf(old(store).chan_view(), old(store).slot_view(), o),
        cap.kind matches CapKind::Thread(o) ==> {
            &&& old(store).slot_view().dom().finite()
            &&& old(store).tcb_view().dom().contains(o)
            &&& old(store).tcb_view()[o].bind_slots.len() == 2
            &&& old(store).slot_view().dom().contains(old(store).tcb_view()[o].bind_slots[0])
            &&& old(store).slot_view().dom().contains(old(store).tcb_view()[o].bind_slots[1])
        },
        cap.kind matches CapKind::Notification(o) ==>
            notif_wf(old(store).notif_view(), old(store).tcb_view(), o),
        cap.kind matches CapKind::Timer(o) ==> {
            &&& old(store).timer_view().dom().contains(o)
            &&& old(store).timer_view().dom().finite()
            &&& timer_wf(old(store).timer_view(), old(store).timer_head_view())
            // `o`'s own armed binding names a live notification (the kernel invariant
            // `disarm`/`destroy_timer` already require). The census rules out `o == n`
            // (an armed self-bound timer would make `census(o) >= 1`, but the zero branch
            // has `census(o) == 0`), so the `-1` on `refs[o]` never touches `refs[n]`.
            &&& (old(store).timer_view()[o].armed ==>
                    (old(store).timer_view()[o].notif matches Some(n) ==>
                        old(store).refs_view().dom().contains(n)
                        && old(store).refs_view()[n] > 0))
        },
        // The system cap→object invariant (plan §6d): needed for the `destroy_channel`/
        // `destroy_tcb` arms (which delete arbitrary caps) and preserved through the `-1`.
        caps_consistent(old(store)),
    ensures
        refcount_sound(final(store)),
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        count_nonempty(final(store).slot_view()) <= count_nonempty(old(store).slot_view()),
        caps_consistent(final(store)),
        // Non-designating caps: the store is untouched (the frame `delete` reads off for a
        // Frame cap — its frame-mapping release rode the frame-unmap branch, not here).
        cap_obj(cap) is None ==> {
            &&& final(store).slot_view() == old(store).slot_view()
            &&& final(store).refs_view() == old(store).refs_view()
            &&& final(store).chan_view() == old(store).chan_view()
            &&& final(store).notif_view() == old(store).notif_view()
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).timer_view() == old(store).timer_view()
            &&& final(store).timer_head_view() == old(store).timer_head_view()
            &&& final(store).cspace_view() == old(store).cspace_view()
        },
{
    match cap.kind {
        CapKind::CSpace(o) => {
            dec_ref(store, o);
            if store.obj_refs(o) == 0 {
                destroy_cspace(store, o);
            }
        }
        CapKind::Thread(o) => {
            dec_ref(store, o);
            if store.obj_refs(o) == 0 {
                crate::thread::destroy_tcb(store, o);
            }
        }
        CapKind::Channel(o, _) => {
            dec_ref(store, o);
            if store.obj_refs(o) == 0 {
                crate::channel::destroy_channel(store, o);
            }
        }
        CapKind::Notification(o) => {
            dec_ref(store, o);
            if store.obj_refs(o) == 0 {
                proof {
                    // census(o) == 0 ⟹ no waiters ⟹ wait_head is None (notif_wf's chain).
                    assert(store.refs_view()[o] == obj_census(store, o));
                    assert(waiter_refs(store.notif_view(), store.tcb_view(), o) == 0);
                    let ws = waiter_seq(store.notif_view(), store.tcb_view(), o);
                    assert(waiter_chain(store.notif_view(), store.tcb_view(), o, ws));
                    assert(ws.len() == 0);
                }
                crate::notification::destroy_notif(store, o);
            }
        }
        CapKind::Timer(o) => {
            dec_ref(store, o);
            if store.obj_refs(o) == 0 {
                proof {
                    // census(o) == 0 ⟹ armed_timer_refs(o) == 0 ⟹ no armed timer is bound
                    // to `o`; in particular `o` is not self-bound, so `destroy_timer`'s
                    // armed-notif-live precondition (`o.notif == Some(n)` ⟹ n live, n ≠ o)
                    // is discharged from the off-by-one precondition's soundness at n ≠ o.
                    assert(store.refs_view()[o] == obj_census(store, o));
                    let armed = store.timer_view().dom().filter(
                        |k: ObjId| store.timer_view()[k].armed && store.timer_view()[k].notif == Some(o));
                    assert(armed_timer_refs(store.timer_view(), o) == 0);
                    assert(armed.finite());
                    assert(armed.len() == 0);
                    assert(!armed.contains(o));
                    // `o` not self-bound (else `o` would be in `armed`).
                    assert(!(store.timer_view()[o].armed && store.timer_view()[o].notif == Some(o)));
                    // dec_ref framed the timer view and dropped only refs[o] (to 0).
                    assert(store.timer_view() == old(store).timer_view());
                    assert(store.refs_view() == old(store).refs_view().insert(o, 0));
                }
                crate::timer::destroy_timer(store, o);
            }
        }
        CapKind::Aspace(o) => {
            // Decrement-then-maybe-`aspace_destroy` — exactly `unref_aspace`'s body, reused.
            unref_aspace(store, o);
        }
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => {}
    }
}

/// Drop one reference to cspace `cs` (a bound thread holds one — released by
/// `destroy_tcb`); at zero, tear it down (plan §6c). `obj_unref`'s CSpace arm in isolation,
/// for the non-cap holder path: the off-by-one decrement (`dec_ref`) then the at-zero
/// `destroy_cspace`.
pub fn unref_cspace<S: Store>(store: &mut S, cs: ObjId)
    requires
        old(store).refs_view().dom().contains(cs),
        old(store).refs_view()[cs] > 0,
        old(store).refs_view()[cs] == obj_census(old(store), cs) + 1,
        forall|x: ObjId| x != cs && old(store).refs_view().dom().contains(x)
            ==> #[trigger] old(store).refs_view()[x] == obj_census(old(store), x),
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        caps_consistent(old(store)),
        cspace_resident_wf(old(store), cs),
    ensures
        refcount_sound(final(store)),
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        count_nonempty(final(store).slot_view()) <= count_nonempty(old(store).slot_view()),
        caps_consistent(final(store)),
{
    dec_ref(store, cs);
    if store.obj_refs(cs) == 0 {
        destroy_cspace(store, cs);
    }
}

/// Drop a non-cap reference to an aspace — mapped frames and bound threads hold
/// these so the aspace can't die under them (plan §6b, doc/results/42). The first
/// teardown op into `verus!{}`: non-recursive (`aspace_destroy` is a seam black box;
/// an aspace owns page tables, not caps), so it closes without the cross-module
/// cluster (6c/6d).
///
/// **Off-by-one census precondition.** The caller (`delete`'s frame-unmap branch;
/// `destroy_tcb`'s aspace release) clears the mapping/hold that named `a` *before*
/// calling, so at entry `a`'s census has already dropped by one while `refs[a]` has
/// not: `refs[a] == census(a) + 1`, sound everywhere else. The `-1` here lands the
/// matching decrement, restoring the full `refcount_sound` invariant. At zero,
/// `aspace_destroy` fires and `a` leaves the live set (the trusted page-table free,
/// plan §2). `refs[a] > 0` is the underflow gate for `obj_refs(a) - 1` (§1.3).
///
/// The proof is light: `obj_census` reads only the seven object views (never
/// `refs_view`), and both `set_obj_refs` and `aspace_destroy` frame those views, so
/// the census is invariant across this op — no per-term recount is needed *inside*
/// `unref_aspace` (that machinery is for the slot-clearing teardown ops, 6d).
pub fn unref_aspace<S: Store>(store: &mut S, a: ObjId)
    requires
        old(store).refs_view().dom().contains(a),
        old(store).refs_view()[a] > 0,
        old(store).refs_view()[a] == obj_census(old(store), a) + 1,
        forall|o: ObjId| o != a && old(store).refs_view().dom().contains(o)
            ==> #[trigger] old(store).refs_view()[o] == obj_census(old(store), o),
        caps_consistent(old(store)),
    ensures
        refcount_sound(final(store)),
        old(store).refs_view()[a] == 1 ==>
            final(store).refs_view() == old(store).refs_view().remove(a),
        old(store).refs_view()[a] > 1 ==>
            final(store).refs_view() == old(store).refs_view().insert(
                a,
                (old(store).refs_view()[a] - 1) as nat,
            ),
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        // Aspaces appear in no `cap_consistent` arm and every object view is framed, so the
        // invariant is preserved (plan §6d).
        caps_consistent(final(store)),
{
    let r = store.obj_refs(a);
    store.set_obj_refs(a, r - 1);
    if store.obj_refs(a) == 0 {
        store.aspace_destroy(a);
        proof {
            // (old.insert(a,0)).remove(a) == old.remove(a) (a was already live).
            assert(final(store).refs_view() =~= old(store).refs_view().remove(a));
        }
    }
    proof {
        // Every census view is framed unchanged, so the recount is invariant.
        assert forall|o: ObjId| #[trigger] obj_census(final(store), o)
            == obj_census(old(store), o) by {}
        // refcount_sound: a's term moved with the `-1`; every other object's refs
        // and census are both untouched, so the precond's soundness carries over.
        assert forall|o: ObjId| final(store).refs_view().dom().contains(o)
            implies #[trigger] final(store).refs_view()[o] == obj_census(final(store), o) by {}
        // caps_consistent: every object view is framed equal to `old`, so each live cap's
        // (refs-free) consistency carries over.
        assert forall|s: SlotId| #![trigger final(store).slot_view()[s]]
            final(store).slot_view().dom().contains(s)
                && !is_empty_cap(final(store).slot_view()[s].cap)
            implies cap_consistent(final(store), final(store).slot_view()[s].cap) by {
            assert(cap_consistent(old(store), old(store).slot_view()[s].cap));
        }
    }
}

/// Delete one cap (children survive, re-parented one level up).
///
/// **Trusted boundary (assumed contract).** The body is the real teardown:
/// `cdt_unlink` + per-end channel `peer_closed` + frame unmap + `obj_unref`,
/// whose last-ref path recurses across objects (`destroy_cspace` → `delete`).
/// That cross-object recursion — the seL4-zombie measure — is the remaining
/// kernel-core proof (it needs the channel/notification/thread destructors
/// ported, plan phases 3–5), so `delete` is `external_body`: Verus trusts this
/// contract and `revoke` (below) is verified against it. The contract states
/// exactly what `revoke`'s termination needs — the live-slot count strictly
/// drops, the domain and well-formedness are preserved — and is the obligation
/// the future body proof must discharge.
///
/// The contract is stated for the **general** (possibly non-leaf) case on
/// purpose: `revoke` only ever passes a leaf, but `destroy_cspace` deletes
/// non-leaf residents, so the body proof must handle `cdt_unlink`'s re-parenting
/// — the harder `cdt_wf`-preservation case.
///
/// **Refcount census (plan §6a).** `delete` now also requires and preserves
/// `refcount_sound` (the §4.1 obligation): the deleted cap lowers exactly its
/// object's `slot_refs`/`frame_map_refs`, matched by the `obj_unref`/`unref_aspace`
/// `-1` (and the per-end `endpoint_cap_dropped` by its `binding_refs` drop). The
/// `requires` is the underflow gate the 6d body proof leans on (`refs[o] - 1`
/// needs `refs[o] ≥ 1`, which the census-at-entry supplies, §1.3); stating it now
/// means the verified callers (`bind`, `revoke`) carry the invariant against the
/// *final* contract, so 6d's body closure adds no caller churn. Assumed here
/// (`external_body`), host-checked against `ArrayStore` (`check_delete`).
#[verifier::external_body]
pub fn delete<S: Store>(store: &mut S, slot: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
        refcount_sound(old(store)),
        // Cap→object consistency (plan §6d foundation): the body's `endpoint_cap_dropped`
        // and `obj_unref` calls need the deleted cap's object well-formed, which only this
        // system invariant supplies. Assumed here (`external_body`), discharged by the body
        // PR; host-checked (`check_delete`). The verified callers (`bind`, `revoke`,
        // `destroy_cspace`) carry it like 6a's `refcount_sound`.
        caps_consistent(old(store)),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        is_empty_cap(final(store).slot_view()[slot].cap),
        count_nonempty(final(store).slot_view()) < count_nonempty(old(store).slot_view()),
        refcount_sound(final(store)),
        caps_consistent(final(store)),
        // Residency is immutable (the kernel fixes it at construction; every internal
        // mutator frames `cspace_view`, swept in 6a) — `delete` re-parents CDT links and
        // clears caps but never reassigns which slots a cspace owns. `destroy_cspace`'s
        // resident loop (6c) reads `cspace_view[cs]` across its `delete` calls, so the
        // frame is load-bearing there; host-checked (`check_delete`), as an assumed
        // `external_body` clause must be.
        final(store).cspace_view() == old(store).cspace_view(),
        // Conditional-on-notification frame (plan §4d): deleting a **notification**
        // cap is robustly clean — `cdt_unlink`/`set_slot` frame every object view,
        // the `Channel`/mapped-`Frame` teardown branches don't fire, and `obj_unref`
        // only drops `refs[n]` (and at zero calls the no-op `destroy_notif`). So the
        // object views and every *other* slot's cap are untouched. This is the
        // additive enabling clause `thread::bind` reads off (the displaced bind cap
        // is always a notification); host-test-checked (`check_delete_notif`), as an
        // assumed `external_body` clause must be. `refs_view` is deliberately left
        // out — the `refs[n] -= 1` rides the host test, not `bind`'s verified contract.
        cap_notif(old(store).slot_view()[slot].cap) is Some ==> {
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).chan_view() == old(store).chan_view()
            &&& final(store).notif_view() == old(store).notif_view()
            &&& final(store).timer_view() == old(store).timer_view()
            &&& final(store).timer_head_view() == old(store).timer_head_view()
            &&& forall|x: SlotId| old(store).slot_view().dom().contains(x) && x != slot
                    ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap
        },
{
    let cap = store.slot(slot).cap;
    debug_assert!(!cap.is_empty());
    cdt_unlink(store, slot);
    let mut s = store.slot(slot);
    s.cap = Cap::EMPTY;
    store.set_slot(slot, s);
    // Channel endpoint liveness is tracked per end for peer-closed (§3.3).
    if let CapKind::Channel(ch, end) = cap.kind {
        crate::channel::endpoint_cap_dropped(store, ch, end);
    }
    // Deleting a mapped frame cap unmaps it — the one revocation story
    // for shared memory (§2.5).
    if let CapKind::Frame { pages, mapping: Some((asp, va)), .. } = cap.kind {
        store.aspace_unmap(asp, va, pages);
        unref_aspace(store, asp);
    }
    obj_unref(store, cap);
}

/// Descend from `start` to a leaf of its subtree (the inner walk of `revoke`).
///
/// **Terminates** (`decreases prank[leaf]`): each step follows `first_child`,
/// and the child's parent rank is strictly below the leaf's (acyclicity) — the
/// headline gain over the old `debug_assert`, here proven unbounded for all tree
/// shapes by *using* the acyclicity witness.
pub fn descend_to_leaf<S: Store>(store: &S, start: SlotId) -> (leaf: SlotId)
    requires
        cdt_wf(store.slot_view()),
        acyclic(store.slot_view()),
        store.slot_view().dom().contains(start),
        !is_empty_cap(store.slot_view()[start].cap),
    ensures
        store.slot_view().dom().contains(leaf),
        store.slot_view()[leaf].first_child is None,
        !is_empty_cap(store.slot_view()[leaf].cap),
{
    let ghost r = choose|r: Map<SlotId, nat>| valid_prank(store.slot_view(), r);
    let mut leaf = start;
    while store.slot(leaf).first_child.is_some()
        invariant
            cdt_wf(store.slot_view()),
            valid_prank(store.slot_view(), r),
            store.slot_view().dom().contains(leaf),
            !is_empty_cap(store.slot_view()[leaf].cap),
        decreases r[leaf],
    {
        let c = store.slot(leaf).first_child.unwrap();
        // c is leaf's first child: c is live, claims leaf as parent (so its rank
        // is strictly lower — the loop measure drops), and is non-empty.
        proof {
            assert(store.slot_view()[c].parent == Some(leaf));
            assert(r[c] < r[leaf]);
        }
        leaf = c;
    }
    leaf
}

/// Revoke: delete every CDT descendant of `slot` — cspace residents and
/// in-flight queue slots alike, unconditionally (§2.2).
///
/// **Terminates** (`decreases count_nonempty`): each iteration descends to a
/// leaf and deletes it, and `delete` strictly lowers the live-slot count. This
/// is the revocation-walk termination the plan calls the headline gain over
/// Kani's `debug_assert` — proven here for all shapes, modulo `delete`'s assumed
/// teardown contract (above).
///
/// **NOT yet proven — `slot`'s cap survives (§2.2).** The postcondition does not
/// assert `slot` stays non-empty. Adding that clause fails against `delete`'s
/// current contract, which frames only the deleted slot's cap: a cross-object
/// teardown (deleting the last cap to a cspace that contains `slot` as a
/// resident) can empty `slot` itself, and the proof would still pass vacuously
/// (an empty slot has `first_child == None`). Closing this needs `delete` to
/// frame *which* slots it may empty (the deleted slot's CDT subtree only) — the
/// reachability strengthening tracked with the looping-op proofs (doc/results/21
/// §9). Until then revoke's root-survival is a documented gap, not a theorem.
///
/// pre:  the cspace is well-formed (and finite); `slot` is live and non-empty.
/// post: `slot` has no children (its subtree is gone); the cspace stays
///       well-formed.
pub fn revoke<S: Store>(store: &mut S, slot: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
        refcount_sound(old(store)),
        caps_consistent(old(store)),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom().contains(slot),
        final(store).slot_view()[slot].first_child is None,
{
    // `refcount_sound` and `caps_consistent` ride the loop as `delete`'s preconditions
    // (§6a/§6d): each `delete` requires them (held by the invariant) and re-establishes them.
    while store.slot(slot).first_child.is_some()
        invariant
            cspace_wf(store.slot_view()),
            store.slot_view().dom().finite(),
            store.slot_view().dom().contains(slot),
            refcount_sound(store),
            caps_consistent(store),
        decreases count_nonempty(store.slot_view()),
    {
        // The first child is live (it names `slot` as parent, so it is not an
        // empty/detached slot) — so we descend from a non-empty node even if
        // `slot` itself were emptied by an earlier delete.
        let first = store.slot(slot).first_child.unwrap();
        proof {
            assert(store.slot_view()[first].parent == Some(slot));
            assert(!is_empty_cap(store.slot_view()[first].cap));
        }
        let leaf = descend_to_leaf(store, first);
        delete(store, leaf);
    }
}

} // verus!
