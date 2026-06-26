//! Capability spaces and the capability derivation tree (rev2§2.1–2.3,
//! rev2§3.4).
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
//! functions over an abstract indexed store.
//!
//! Verification: these operations are the centerpiece Verus target — the
//! deductive re-check of the CapRevocation TLA+ invariants on the real
//! implementation. Each op carries its contract as a pre/post comment; the
//! proofs turn those into `cdt_wf` assertions.

use crate::id::{ObjId, SlotId};
use crate::store::{Binding, Store};
#[allow(unused_imports)] // NUM_PRIOS: referenced only in spec/proof code
use crate::sysabi::NUM_PRIOS;
use crate::thread::{Report, ThreadState};
use vstd::prelude::*;

/// Rights bits — monotone under derivation (rev2§2.3): `derive` may only clear
/// bits, never set them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub u8);

// Inside `verus!{}` so the bit consts and `masked` are usable from verified code
// (the `retype_install` rights-inheritance theorem names `READ`/`WRITE`/`PHYS`/
// `ALL`/`THREAD_ALL`). `masked` carries its bit-level `ensures` here rather than
// via a standalone `assume_specification` — its trivial body is verified, not
// assumed.
verus! {
impl Rights {
    pub const READ: u8 = 1 << 0; // recv / wait
    pub const WRITE: u8 = 1 << 1; // send / signal
    /// phys-read (rev2§2.5): gates frame_paddr and device mappings. Granted
    /// only on boot-created device/DMA caps — ALL deliberately excludes
    /// it so ordinary derivation chains can never reach a PA.
    pub const PHYS: u8 = 1 << 2;
    /// bind-reports (rev2§2.3): configure a thread's on-exit/on-fault
    /// binding slots (rev2§5.1).
    pub const BIND_REPORTS: u8 = 1 << 3;
    /// read-report (rev2§2.3): read a thread's terminal report record;
    /// later also the debugger's register access (deferred, rev2§8).
    pub const READ_REPORT: u8 = 1 << 4;
    pub const ALL: Rights = Rights(0b11);
    /// The creator's thread cap (rev2§2.3 thread bits; kill is deliberately
    /// not on the list — destruction is resource ancestry, rev2§2.2).
    pub const THREAD_ALL: Rights =
        Rights(Rights::READ | Rights::WRITE | Rights::BIND_REPORTS | Rights::READ_REPORT);

    pub fn has(self, bits: u8) -> bool {
        self.0 & bits == bits
    }

    // The bit-level spec is what makes monotone derivation (and the
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
/// mapping per cap copy, and deleting the cap unmaps it (rev2§2.5). Object
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
    // The `u8` is the rev2§5.4 maximum-controlled-priority ceiling — a value on the
    // cap (rev2§2.3), attenuated monotonically by `derive` like rights. spawn gates a
    // thread's priority on it (`prio <= max_prio`).
    Thread(crate::id::ObjId, u8),
    Channel(crate::id::ObjId, ChanEnd),
    Notification(crate::id::ObjId),
    Timer(crate::id::ObjId),
    // The rev2§1 IRQ-handler cap: a plain designating handle to an `IrqObj`, the
    // timer's twin. The `intid` rides the object, not the discriminant, so this is uniform
    // with `Notification`/`Timer` — `derived_kind`'s `_ => k` fallthrough makes it a faithful
    // designating copy with no new arm, and `cap_max_prio` returns `None` (it carries no ceiling).
    Irq(crate::id::ObjId),
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
/// and inside channel message slots — both are CDT-visible (rev2§3.4). The
/// links are [`SlotId`] handles ([`crate::id`]) that span containers, with no
/// special case in the revoke walk.
#[derive(Clone, Copy)]
pub struct CapSlot {
    pub cap: Cap,
    pub parent: Option<crate::id::SlotId>,
    pub first_child: Option<crate::id::SlotId>,
    pub next_sib: Option<crate::id::SlotId>,
    pub prev_sib: Option<crate::id::SlotId>,
    /// Revoke-in-progress marker. Set on the *root* of an in-flight,
    /// preemptible `revoke_step` walk and cleared when the subtree is empty;
    /// `derive` refuses any derivation whose source's ancestor chain reaches a
    /// `revoking` root, so the multi-call walk terminates under concurrent
    /// derivation (rev2§2.2 "preemptible and restartable"). It is *not* a CDT
    /// link nor part of the cap: no structural (`cdt_wf`/`acyclic`) or census
    /// (`count_nonempty`/`refcount_sound`) predicate reads it — they key off
    /// `.cap` and the four links — so flipping it frames every invariant
    /// (`lemma_set_revoking_frames`).
    pub revoking: bool,
}

impl CapSlot {
    pub const fn empty() -> CapSlot {
        CapSlot {
            cap: Cap::EMPTY,
            parent: None,
            first_child: None,
            next_sib: None,
            prev_sib: None,
            revoking: false,
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

    /// pre: self points at a live, initialised cspace object.
    /// post: returns slot i, or null if i is out of range.
    pub unsafe fn slot(this: *mut CSpaceObj, i: u32) -> *mut CapSlot {
        if i >= (*this).num_slots {
            return core::ptr::null_mut();
        }
        let base = this.add(1).cast::<CapSlot>();
        base.add(i as usize)
    }

    /// pre: memory at `this` is writable, sized via bytes_for(num_slots).
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

// `obj_unref`, `unref_cspace`, `destroy_cspace`, the helper `dec_ref`, `delete`,
// `destroy_channel`, and `destroy_tcb` are verified — see the `verus!{}` block at
// the end of this file. With `delete`'s body proven, the cspace teardown cycle
// `delete → obj_unref → destroy_cspace → delete` is closed in Verus under the shared
// lexicographic `decreases (count_nonempty(slot_view), height)`. `obj_unref`'s
// Channel/Thread arms recurse through `destroy_channel`/`destroy_tcb`, whose proven
// bodies carry the census invariants. `unref_aspace` (the non-recursive aspace
// teardown) is likewise in that block.

// ── CDT structure ───────────────────────────────────────────────────────

// `cdt_insert_child`, `derive`, `slot_move`, and `cdt_unlink` are verified — see
// the `verus!{}` block at the end of this file. `slot_move`'s body proof shows the
// move is the identity transposition π=(src dst) and lands exactly the renaming.
// `cdt_unlink`'s body proof shows the sibling-list *merge* lands exactly
// `unlinked(m0, slot, last)` (children spliced into the parent's list — strictly
// harder than the transposition): the parent-rank acyclicity witness is reused
// unchanged, the sibling-rank witness is rescaled to fit the re-parented child band
// into the `prev..next` gap. Both ops' termination/structure rest on
// sibling-acyclicity (`sib_acyclic`), part of `cspace_wf`.
//
// `delete` and `revoke` are likewise in that block, with proven bodies: the cross-
// object destructors they invoke are themselves verified, and `revoke`'s termination
// is proven against `delete`.

// ── Deductive verification ───────────────────────────────────────────────────
//
// The cspace/CDT operations are verified with Verus against an *abstract* model
// of the `Store` seam: the kernel object store is a finite `Map<SlotId, CapSlot>`
// (the slot arena) plus a `Map<ObjId, nat>` (object refcounts). The generic
// `fn op<S: Store>` operations are proven once for **all** stores; the production
// `KernelStore` (kernel crate, unverified) and any host-test store are trusted to
// satisfy the trait contract — the seam is the TCB boundary.
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
// normal build.)
#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExBinding(Binding);

// `ThreadState`/`Report` are plain Rust enums (`crate::thread`); give them Verus
// type-specs so they can live in `TcbView` and be compared with structural `==`
// (the `tcb_view` analog of `ExChanEnd`). (`allow(dead_code)`:
// Verus-only scaffolding, erased in a normal build.)
#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExThreadState(ThreadState);

#[verifier::external_type_specification]
#[verifier::ext_equal]
#[allow(dead_code)]
pub struct ExReport(Report);

// ── The channel ghost view ───────────────────────────────────────
//
// `ChanView` mirrors a `Channel`'s *mutable* state (`channel.rs`) at the
// abstraction the channel proofs reason over — **payload bytes abstracted out**:
// we model message length, cap identity, and order, not the 256 payload bytes.
//
// The load-bearing decision: a ring message slot is a **real
// `CapSlot` in the single `slot_view` arena** (moved by the already-verified
// `slot_move`). So the cap *contents* live in `slot_view`; `ring_cap` here holds
// only the slot *handles*, which are fixed at channel construction and never
// reassigned (`Store` has a `chan_ring_cap` getter and no setter). `chan_ring_cap`
// is therefore a deterministic projection of this view, and `chan_wf` pins the
// handles to the arena (each in `slot_view`'s domain; window-empty coupling below).
#[verifier::ext_equal]
pub struct ChanView {
    pub depth: nat,
    // Per-end live-endpoint-cap counts (peer-closed, rev2§3.3) and per-ring FIFO
    // cursors. Seqs of length 2 (ring/end ∈ {0,1}).
    pub end_caps: Seq<nat>,
    pub head: Seq<nat>,
    pub count: Seq<nat>,
    // bindings[(end, ev)] — end ∈ {0,1}, ev ∈ {0,1,2} (readable/writable/peer-closed).
    pub bindings: Map<(int, int), Binding>,
    // msg_len[(ring, index)] — the queued payload length (bytes abstracted).
    pub msg_len: Map<(int, int), nat>,
    // ring_cap[(ring, index, cap)] — the CapSlot handle for that ring message's
    // cap slot (cap ∈ {0..4}); the bridge into `slot_view` (the channel-view/slot-arena coupling).
    pub ring_cap: Map<(int, int, int), SlotId>,
}

// ── The notification / TCB / timer ghost views ────────────────────
//
// The analogs of `ChanView`: each mirrors an object's
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
// exactly as `ChanView.ring_cap` does — so the TCB binding caps
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
    // rev2§5.4 run priority. Bounded by the spawner's cap ceiling (`cap_max_prio`) and
    // written only through the verified `thread::set_priority`: surfacing it in the
    // view is what lets `set_priority` carry a machine-checked
    // `priority == prio (≤ ceiling)`.
    pub priority: u8,
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

// The IRQ-handler object (rev2§1, rev2§3.6): the timer's **census twin**, minus the
// armed list. An IRQ cap binds a (notification, bits) pair exactly as a timer does
// (`bound` for `armed`, `notif`/`bits` shared), and a hardware interrupt signals that
// notification — but delivery is by direct INTID→object lookup, not by sweeping
// a chain, so there is **no `next` field** and no armed-list analog. `intid` rides the
// object (the cap is a plain designating handle, uniform with `Notification`/`Timer`);
// `masked` is the line-state bit the GIC shell toggles on deliver/ack — bind/
// unbind/teardown never touch it. The binding holds a ref on `notif` (the
// `irq_binding_refs` census term), so revoking the notif cap cannot free it under a bound
// IRQ — the exact hazard `armed_timer_refs` guards for timers.
#[verifier::ext_equal]
pub struct IrqView {
    pub intid: u32,
    pub notif: Option<ObjId>,
    pub bits: u64,
    pub bound: bool,
    pub masked: bool,
}

// The 32-level ready queue (rev2§5.4): one intrusive `Tcb.qnext` list per priority
// level + a `u32` presence bitmap. Per-level head/tail (the waiter-queue shape, ×32)
// plus the `timer_head_view`-style global-scalar bitmap. A thread is on the
// `heads[level]`..`tails[level]` chain iff it is `Runnable` at `priority == level`
// (the per-element covenant in `ready_chain`); `bitmap` bit `level` is set iff that
// level's chain is non-empty (the coherence invariant in `ready_wf`). The link is the
// same `Tcb.qnext` the waiter queue threads — disambiguated by state (`Runnable` here,
// `BlockedNotif` there) — so a thread is on at most one of the two.
#[verifier::ext_equal]
pub struct ReadyView {
    pub heads: Map<int, Option<ObjId>>, // level (0..NUM_PRIOS) → list head
    pub tails: Map<int, Option<ObjId>>, // level (0..NUM_PRIOS) → list tail
    pub bitmap: u32,                     // bit `level` set ⇔ level's chain non-empty
}

// Cspace residency — the slot-handle list a cspace object owns. The
// kernel fixes this at construction (`cspace_num_slots`/`cspace_slot` are getters
// with no setter), so it is an immutable projection exactly like `ChanView
//.ring_cap` / `TcbView.bind_slots`: every mutator frames it unchanged. It is the
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
    // Cspace residency: handle → the slot-handle list the cspace owns.
    // Immutable (the residency getters have no setter); every mutator frames it
    // unchanged, so `destroy_cspace`'s resident loop (6c) and revoke-root-survival
    // (6e) reason over a residency that stays stable across the teardown ops'
    // internal setter calls (whose bodies land in 6d). The `chan_view` analog,
    // residency edition. (`refcount_sound` does *not* read residency — its terms
    // are over the slot/chan/notif/tcb/timer views — so the sweep is forward-looking
    // for the resident-walk reasoning, not a census dependency.)
    spec fn cspace_view(&self) -> Map<ObjId, CSpaceView>;
    // Channel state: handle → ghost view. The third independent view;
    // the slot/refs setters frame it unchanged and the channel setters frame
    // slot/refs unchanged, so the channel ops can reason about one without the others.
    spec fn chan_view(&self) -> Map<ObjId, ChanView>;
    // Notification / TCB / timer state — three more independent views.
    // Every setter frames the *other* five views (+ the `timer_head_view` scalar)
    // unchanged, so a notification op reasons about one without re-establishing the rest
    // (the mutual-frame discipline, extended to a six-view world).
    spec fn notif_view(&self) -> Map<ObjId, NotifView>;
    spec fn tcb_view(&self) -> Map<ObjId, TcbView>;
    spec fn timer_view(&self) -> Map<ObjId, TimerView>;
    // The armed-timer list head — a `Store`-seam scalar (the kernel static,
    // store.rs:130); the list *logic* is in `crate::timer`.
    spec fn timer_head_view(&self) -> Option<ObjId>;
    // The IRQ-handler arena: handle → ghost view, the `timer_view` twin minus the
    // armed-list scalar (delivery is by direct INTID lookup, not a chain). Framed unchanged
    // by every object setter exactly as `timer_view` is; changed only by the verified
    // `crate::irq` bind/unbind/destroy ops. The `irq_binding_refs` census term reads it.
    spec fn irq_view(&self) -> Map<ObjId, IrqView>;
    // The 32-level ready queue (per-level head/tail + presence bitmap) — `Store`-seam
    // state (the `READY`/`READY_BITMAP` kernel statics); the list *logic* is the
    // verified `crate::ready` ops. Framed unchanged by every object setter
    // exactly as `timer_head_view` is; changed only by the enqueue/dequeue/unqueue ops.
    spec fn ready_view(&self) -> ReadyView;
    // The TLBI effect log: the ordered sequence of `(asid, va)` TLB
    // invalidations issued through this store. The seventh view — pure hardware
    // effect, not object state — so `aspace::unmap_in` can prove "one TLBI per
    // cleared page, in order" as a real postcondition. Only the three hardware-
    // seam methods below touch it; it is left unconstrained across the object
    // setters (no object op interleaves a setter with a TLBI), so adding it is a
    // localized seam change, not a per-setter sweep.
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // ── cspace residents ─────────────────────────────────────────
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

    // ── channel accessors ────────────────────────────────────────
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // `&self` (only `buf` is written), so the store is unchanged automatically; the
    // payload is abstracted out, so no spec on `buf`. Needed in `verus!` since 3d's
    // `recv` calls it (3b omitted it as frame-only).
    fn chan_msg_read(&self, ch: ObjId, ring: usize, i: u32, len: usize, buf: &mut [u8]);

    // ── notification accessors ───────────────────────────────────
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // ── thread (TCB) accessors ───────────────────────────────────
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    fn tcb_priority(&self, t: ObjId) -> (r: u8)
        requires self.tcb_view().dom().contains(t),
        ensures r == self.tcb_view()[t].priority;

    fn set_tcb_priority(&mut self, t: ObjId, p: u8)
        requires old(self).tcb_view().dom().contains(t),
        ensures
            final(self).tcb_view() == old(self).tcb_view().insert(
                t, TcbView { priority: p, ..old(self).tcb_view()[t] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // ── timer accessors (the armed-list logic) ──────────────────────────────
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // ── IRQ-handler object ──────────────────────────────────────────────────
    //
    // The timer accessors' twin, minus the armed-list (`next`/head) seam: by-handle
    // getters/setters over `irq_view`, each setter `insert`ing one IRQ and framing every
    // other view. `intid` is boot-static (a getter only — no setter). The verified
    // `crate::irq` ops run against these; production derefs an `IrqObj` (kernel/src/store.rs),
    // host tests use the array backing (`test_store`).
    fn irq_intid(&self, i: ObjId) -> (r: u32)
        requires self.irq_view().dom().contains(i),
        ensures r == self.irq_view()[i].intid;

    fn irq_notif(&self, i: ObjId) -> (r: Option<ObjId>)
        requires self.irq_view().dom().contains(i),
        ensures r == self.irq_view()[i].notif;

    fn set_irq_notif(&mut self, i: ObjId, n: Option<ObjId>)
        requires old(self).irq_view().dom().contains(i),
        ensures
            final(self).irq_view() == old(self).irq_view().insert(
                i, IrqView { notif: n, ..old(self).irq_view()[i] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn irq_bits(&self, i: ObjId) -> (r: u64)
        requires self.irq_view().dom().contains(i),
        ensures r == self.irq_view()[i].bits;

    fn set_irq_bits(&mut self, i: ObjId, v: u64)
        requires old(self).irq_view().dom().contains(i),
        ensures
            final(self).irq_view() == old(self).irq_view().insert(
                i, IrqView { bits: v, ..old(self).irq_view()[i] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn irq_bound(&self, i: ObjId) -> (r: bool)
        requires self.irq_view().dom().contains(i),
        ensures r == self.irq_view()[i].bound;

    fn set_irq_bound(&mut self, i: ObjId, v: bool)
        requires old(self).irq_view().dom().contains(i),
        ensures
            final(self).irq_view() == old(self).irq_view().insert(
                i, IrqView { bound: v, ..old(self).irq_view()[i] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view();

    fn irq_masked(&self, i: ObjId) -> (r: bool)
        requires self.irq_view().dom().contains(i),
        ensures r == self.irq_view()[i].masked;

    fn set_irq_masked(&mut self, i: ObjId, v: bool)
        requires old(self).irq_view().dom().contains(i),
        ensures
            final(self).irq_view() == old(self).irq_view().insert(
                i, IrqView { masked: v, ..old(self).irq_view()[i] }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view();

    // ── ready queue ────────────────────────────────────────────────────────
    //
    // The 32-level ready queue's per-level head/tail + presence bitmap, the
    // `timer_armed_head` precedent generalized: getters project `ready_view`, setters
    // update one level (or the scalar bitmap) and frame every other view. The list
    // *logic* (enqueue/dequeue/unqueue/top) is the verified `crate::ready` ops; these are
    // the by-handle accessors they run against (kernel statics in production, the array
    // backing in `test_store`). `level < NUM_PRIOS` mirrors the production fixed-array bound.
    fn ready_head(&self, level: usize) -> (r: Option<ObjId>)
        requires level < crate::sysabi::NUM_PRIOS,
        ensures r == self.ready_view().heads[level as int];

    fn set_ready_head(&mut self, level: usize, h: Option<ObjId>)
        requires level < crate::sysabi::NUM_PRIOS,
        ensures
            final(self).ready_view() == (ReadyView {
                heads: old(self).ready_view().heads.insert(level as int, h),
                ..old(self).ready_view()
            }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    fn ready_tail(&self, level: usize) -> (r: Option<ObjId>)
        requires level < crate::sysabi::NUM_PRIOS,
        ensures r == self.ready_view().tails[level as int];

    fn set_ready_tail(&mut self, level: usize, t: Option<ObjId>)
        requires level < crate::sysabi::NUM_PRIOS,
        ensures
            final(self).ready_view() == (ReadyView {
                tails: old(self).ready_view().tails.insert(level as int, t),
                ..old(self).ready_view()
            }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    fn ready_bitmap(&self) -> (r: u32)
        ensures r == self.ready_view().bitmap;

    fn set_ready_bitmap(&mut self, b: u32)
        ensures
            final(self).ready_view() == (ReadyView { bitmap: b, ..old(self).ready_view() }),
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // ── scheduler seam (faithful ready-queue ops) ──────────────────────────
    //
    // `make_runnable` is the seam lift of the verified `ready::ready_enqueue` (ready.rs): it
    // appends `t` to the tail of `t`'s priority level (writing the old level-tail's `qnext`)
    // and sets the presence bit. The contract mirrors `ready_enqueue`'s ensures term-for-term so
    // the verified op discharges it (the KernelStore realization routes through it;
    // ArrayStore is host-checked via `signal_frame`). `signal` supplies the `priority < NUM_PRIOS`
    // precondition from the strengthened `waiter_chain` (via `notif_wf`) and `wait_notif is None`
    // by clearing it before the call.
    fn make_runnable(&mut self, t: ObjId)
        requires
            old(self).tcb_view().dom().contains(t),
            (old(self).tcb_view()[t].priority as int) < NUM_PRIOS,
            old(self).tcb_view()[t].state != ThreadState::Runnable,
            old(self).tcb_view()[t].wait_notif is None,
            ready_wf(old(self).ready_view(), old(self).tcb_view()),
            ready_complete(old(self).ready_view(), old(self).tcb_view()),
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view(),
            final(self).tcb_view().dom() == old(self).tcb_view().dom(),
            ready_wf(final(self).ready_view(), final(self).tcb_view()),
            ready_complete(final(self).ready_view(), final(self).tcb_view()),
            // tcb_view changes only at `t` (now Runnable, tail, qnext None) and the old
            // level-tail (qnext now points at `t`); `t`'s other fields are framed.
            final(self).tcb_view()[t] == (TcbView {
                state: ThreadState::Runnable,
                qnext: None,
                ..old(self).tcb_view()[t]
            }),
            forall|x: ObjId| #![trigger final(self).tcb_view()[x]]
                x != t
                && old(self).ready_view().tails[old(self).tcb_view()[t].priority as int] != Some(x)
                ==> final(self).tcb_view()[x] == old(self).tcb_view()[x],
            // global frame: only `state` (on the woken `t`, specified above) and `qnext` (on `t`
            // + the old tail) change. Every thread *but* `t` changes **only its `qnext`** — its
            // `state`/`wait_notif`/`cspace`/`aspace`/`bind_*`/`priority`/`retval` are preserved.
            // So the old ready-tail stays Runnable with `wait_notif None`; `signal`'s caps/
            // waiter-coherence proofs read that off this (a Runnable tail is never `BlockedNotif`).
            forall|x: ObjId| #![trigger final(self).tcb_view()[x]]
                x != t ==> final(self).tcb_view()[x] == (TcbView {
                    qnext: final(self).tcb_view()[x].qnext,
                    ..old(self).tcb_view()[x]
                }),
            ready_seq(final(self).ready_view(), final(self).tcb_view(),
                old(self).tcb_view()[t].priority as int)
                == ready_seq(old(self).ready_view(), old(self).tcb_view(),
                    old(self).tcb_view()[t].priority as int).push(t);

    // `unqueue_ready` — the seam lift of the verified `ready::ready_unqueue`: the
    // arbitrary-position splice walk. Removes a Runnable `t` from its level's chain
    // (re-threading its predecessor's `qnext`, clearing `t`'s), leaving `t` transiently
    // Runnable-and-off-chain — `destroy_tcb` (its sole caller) halts `t` immediately after,
    // promoting `ready_complete_except(t)` back to `ready_complete`. Mirrors `ready_unqueue`'s
    // ensures. `destroy_tcb` supplies `priority < NUM_PRIOS` from `ready_complete` (`t` Runnable).
    // Host-checked via `check_destroy_tcb`.
    fn unqueue_ready(&mut self, t: ObjId)
        requires
            old(self).tcb_view().dom().contains(t),
            old(self).tcb_view()[t].state == ThreadState::Runnable,
            (old(self).tcb_view()[t].priority as int) < NUM_PRIOS,
            ready_wf(old(self).ready_view(), old(self).tcb_view()),
            ready_complete(old(self).ready_view(), old(self).tcb_view()),
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view(),
            final(self).tcb_view().dom() == old(self).tcb_view().dom(),
            ready_wf(final(self).ready_view(), final(self).tcb_view()),
            ready_complete_except(final(self).ready_view(), final(self).tcb_view(), t),
            ({
                let level = old(self).tcb_view()[t].priority as int;
                let rs0 = ready_seq(old(self).ready_view(), old(self).tcb_view(), level);
                &&& ready_seq(final(self).ready_view(), final(self).tcb_view(), level)
                        == rs0.remove(rs0.index_of(t))
                &&& final(self).tcb_view()[t].qnext is None
                &&& final(self).tcb_view()[t].state == old(self).tcb_view()[t].state
                &&& final(self).tcb_view()[t].priority == old(self).tcb_view()[t].priority
                &&& final(self).tcb_view()[t].wait_notif == old(self).tcb_view()[t].wait_notif
                &&& final(self).tcb_view()[t].cspace == old(self).tcb_view()[t].cspace
                &&& final(self).tcb_view()[t].aspace == old(self).tcb_view()[t].aspace
                &&& final(self).tcb_view()[t].bind_slots == old(self).tcb_view()[t].bind_slots
                // `t`'s report survives the splice (only `qnext` is written) —
                // `destroy_tcb` reads it off to preserve the halted subject's report (rev2§5.1).
                &&& final(self).tcb_view()[t].report == old(self).tcb_view()[t].report
                // signal-shaped frame: only level's chain nodes (t + predecessor) moved, each
                // Runnable (off every waiter chain), and each preserves the home fields
                // `destroy_tcb`'s census/caps reasoning needs — `wait_notif` (waiter census),
                // `cspace`/`aspace` (`thread_hold_refs`), `bind_slots` (`caps_consistent`).
                // Extends the part-2 frame with `cspace`/`aspace`.
                &&& forall|x: ObjId| #![trigger final(self).tcb_view()[x]]
                        final(self).tcb_view()[x] != old(self).tcb_view()[x]
                        ==> old(self).tcb_view()[x].state == ThreadState::Runnable
                            && final(self).tcb_view()[x].state == old(self).tcb_view()[x].state
                            && old(self).tcb_view()[x].priority as int == level
                            && final(self).tcb_view()[x].wait_notif
                                == old(self).tcb_view()[x].wait_notif
                            && final(self).tcb_view()[x].cspace == old(self).tcb_view()[x].cspace
                            && final(self).tcb_view()[x].aspace == old(self).tcb_view()[x].aspace
                            && final(self).tcb_view()[x].bind_slots
                                == old(self).tcb_view()[x].bind_slots
            });

    // ── aspace hardware seam (the `aspace::map_in` post-map barrier) ──────────
    //
    // The barrier carries no object state — it issues a `dsb`/`isb` so the leaf
    // writes are visible before the mapping is used. Modeled as "frames every
    // object view", which is faithful (it touches no kcore object) and is all
    // `map_in` needs to call it in the verified fragment. Because it takes
    // neither page-table slice, Verus already knows it cannot perturb `l1`/`pool`,
    // so `map_in`'s page-table postcondition is independent of this contract.
    // It also frames the TLBI log unchanged — the log only ever grows
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view(),
            final(self).tlb_log_view() == old(self).tlb_log_view();

    // ── unmap hardware seam (the `aspace::unmap_in` TLBI ordering) ────────────
    //
    // The two effect-log methods `unmap_in` calls. `tlb_invalidate_page` appends
    // exactly one `(asid, va)` entry — that *append* is what makes "one TLBI per
    // cleared page, in ascending order" a postcondition (the loop invariant tracks
    // `tlb_log_view== old ++ cleared-prefix`). Both frame every object view, so
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view(),
            final(self).tlb_log_view() == old(self).tlb_log_view();

    // ── aspace teardown seam (the cross-object-teardown seam) ──────
    //
    // Two shell-owned page-table ops kcore never sees the body of (the trusted
    // base). Assumed, host-checked against `ArrayStore` (the
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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

    // The map-time twin of `aspace_unmap` (rev2§6.1(c)). Like the unmap, it is page-table
    // maintenance — no object state — so it frames every object view + `refs_view` +
    // `cspace_view` in **both** result arms (the TLBI log it may touch is left unconstrained,
    // like the other hardware effects). It is *fallible* (the unmap is not): the table pool may
    // be exhausted. `map_frame` consumes it exactly as `delete`'s frame branch consumes
    // `aspace_unmap` — page-table side here, the verified cap-side record + `ref_aspace` there.
    fn aspace_map(&mut self, a: ObjId, pa: u64, va: u64, pages: u64, perms: u64)
        -> (res: Result<(), crate::aspace::MapError>)
        ensures
            final(self).slot_view() == old(self).slot_view(),
            final(self).refs_view() == old(self).refs_view(),
            final(self).chan_view() == old(self).chan_view(),
            final(self).notif_view() == old(self).notif_view(),
            final(self).tcb_view() == old(self).tcb_view(),
            final(self).timer_view() == old(self).timer_view(),
            final(self).timer_head_view() == old(self).timer_head_view(),
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();

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
            final(self).ready_view() == old(self).ready_view(),
            final(self).cspace_view() == old(self).cspace_view(),
            final(self).irq_view() == old(self).irq_view();
}

// The refcounted object a cap designates (the spec mirror of `Cap::obj`).
pub open spec fn cap_obj(c: Cap) -> Option<ObjId> {
    match c.kind {
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
        CapKind::Aspace(o) => Some(o),
        CapKind::CSpace(o) => Some(o),
        CapKind::Thread(o, _) => Some(o),
        CapKind::Channel(o, _) => Some(o),
        CapKind::Notification(o) => Some(o),
        CapKind::Timer(o) => Some(o),
        CapKind::Irq(o) => Some(o),
    }
}

// True iff `c` is a thread cap designating `t`, *ignoring* the priority ceiling —
// the field-shape-stable test for "is this a thread cap for `t`?". The
// dead-object lemmas (`lemma_no_live_thread_cap_from_dead` et al.) reason about
// that question independent of the ceiling `Thread` carries.
//
// `closed` (cross-platform headroom): the body unfolds inside `cspace`
// (where the two dead-object lemmas prove/consume it), but stays opaque to
// `thread::destroy_tcb`, which only *carries* this predicate from one lemma's
// `ensures` into the next's `requires` (a syntactic match) and never needs its
// body. Measured: with `open`, `destroy_tcb` flakes the rlimit at 8; with
// `closed` it passes at 8 — hiding the unneeded unfold there is what restores the
// cross-platform headroom (resource counting varies Linux↔macOS; see the
// `spinoff_prover` note on `destroy_tcb`).
pub closed spec fn is_thread_cap_for(c: Cap, t: ObjId) -> bool {
    c.kind matches CapKind::Thread(o, _) && o == t
}

// The rev2§5.4 maximum-controlled-priority ceiling a cap carries, if it is a thread
// cap (else `None`). Priority attenuates monotonically through `derive` exactly
// like rights (rev2§2.3) — see `derive`'s ceiling `ensures`.
pub open spec fn cap_max_prio(c: Cap) -> Option<u8> {
    match c.kind {
        CapKind::Thread(_, mp) => Some(mp),
        _ => None,
    }
}

pub open spec fn is_empty_cap(c: Cap) -> bool {
    c.kind matches CapKind::Empty
}

// The (channel, end-index) a cap designates, if it is a channel cap (else `None`).
// Narrower than `cap_obj` (which drops the end): the rev2§3.3 per-endpoint census
// `end_cap_count` filters on *which end* a `Channel(o, end)` cap names, since
// `end_caps[end]` is tracked per end for peer-closed firing.
pub open spec fn cap_chan_end(c: Cap) -> Option<(ObjId, int)> {
    match c.kind {
        CapKind::Channel(o, end) => Some((o, crate::channel::end_idx_spec(end))),
        _ => None,
    }
}

// The notification a cap names, if it is a notification cap (else `None`). The
// spec projection `thread::report_terminal`/`thread::bind` use to talk
// about the notification a TCB bind slot holds — narrower than `cap_obj` (which
// returns the object for *any* object cap), because a bind slot only ever holds a
// notification cap and the bind/report contracts reason specifically about that case.
pub open spec fn cap_notif(c: Cap) -> Option<ObjId> {
    match c.kind {
        CapKind::Notification(o) => Some(o),
        _ => None,
    }
}

// Exec emptiness check tied to the `is_empty_cap` spec — `Cap::is_empty` is plain
// Rust (outside `verus!`), so verified exec code (channel `recv`) uses this.
pub fn cap_is_empty(c: Cap) -> (r: bool)
    ensures
        r == is_empty_cap(c),
{
    matches!(c.kind, CapKind::Empty)
}

// The kind a derivation produces from `k` under a requested priority ceiling:
// identical (same object, same channel end), except a Frame copy starts unmapped
// (rev2§2.5, one mapping per cap copy) and a Thread copy's rev2§5.4 ceiling is reduced to
// `min(parent, prio_ceiling)`. This is the "copy" half of monotone derivation —
// derivation cannot change the designated object or amplify via the kind, and the
// priority ceiling can only shrink (rev2§2.3). `prio_ceiling = 0xFF` (the `cap_copy`
// no-reduction sentinel; priorities are `< NUM_PRIOS = 32`) preserves the parent
// ceiling exactly; a lower value is the rev2§2.3 supervision grant.
pub open spec fn derived_kind(k: CapKind, prio_ceiling: u8) -> CapKind {
    match k {
        CapKind::Frame { base, pages, mapping: _ } => CapKind::Frame { base, pages, mapping: None },
        CapKind::Thread(o, mp) => CapKind::Thread(o, if mp <= prio_ceiling { mp } else { prio_ceiling }),
        _ => k,
    }
}

// `Rights::masked` carries its bit-level `ensures` on the verified method itself
// (see the `impl Rights` verus block above), so it needs no standalone
// `assume_specification`.

// `CapSlot::empty` is a plain-Rust const fn (shared with the kernel shell); state
// what it builds so `slot_move`'s final clear can be verified — an empty cap with
// all CDT links detached.
pub assume_specification [ CapSlot::empty ]() -> (r: CapSlot)
    ensures
        is_empty_cap(r.cap),
        r.parent is None,
        r.first_child is None,
        r.next_sib is None,
        r.prev_sib is None,
        !r.revoking;

// ── Structural well-formedness of the CDT (the executable `TypeOK`, now total
// and unbounded). Acyclicity is tracked separately where termination needs
// it; this is the structural invariant the op proofs preserve. ──

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
// construction-side acyclicity proofs need: without it a
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
// still-lower phantom child needing to exist.
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

// ── CDT descendant reachability (the rev2§3.4 "sees through queues" obligation) ────
//
// `revoke` deletes the *whole subtree* of its target, in-flight queue caps included
// (rev2§3.4: queue slots are ordinary `CapSlot`s carrying the parent edge, so a cap queued
// in a message is a genuine CDT descendant — `slot_move`, what `send` uses, inherits the
// edge into the ring slot). The structural predicates above pin only the *direct* child
// relation; these add the transitive closure so `revoke` can export "the subtree is gone"
// as a named obligation rather than leaving it structurally implied.

// `path` is a child→parent walk in `m`: every entry is live and each entry's `parent`
// points at the next. Length ≥ 1.
pub open spec fn is_parent_path(m: Map<SlotId, CapSlot>, path: Seq<SlotId>) -> bool {
    &&& path.len() >= 1
    &&& (forall|i: int| 0 <= i < path.len() ==> m.dom().contains(#[trigger] path[i]))
    &&& (forall|i: int| 0 <= i < path.len() - 1 ==>
            m[#[trigger] path[i]].parent == Some(path[i + 1]))
}

// `d` is a strict CDT descendant of `anc`: a non-trivial parent-walk leads from `d` up to
// `anc`. A queued in-flight cap derived under `anc` satisfies this (its ring slot's parent
// edge was inherited from the source by `slot_move`).
pub open spec fn is_descendant(m: Map<SlotId, CapSlot>, d: SlotId, anc: SlotId) -> bool {
    exists|path: Seq<SlotId>| #[trigger] is_parent_path(m, path)
        && path.len() >= 2 && path[0] == d && path.last() == anc
}

// No live slot (resident *or* in-flight ring cap) is a CDT descendant of `anc` — the
// "subtree is empty" fact `revoke` exports.
pub open spec fn no_live_descendant(m: Map<SlotId, CapSlot>, anc: SlotId) -> bool {
    forall|d: SlotId| #[trigger] m.dom().contains(d) ==> !is_descendant(m, d, anc)
}

// A childless node has no descendants at all. Non-inductive: any descendant chain's
// topmost step names `anc` as parent, so `parent_has_first_child` forces `anc` to have a
// first child — contradicting `first_child is None`. This is the lemma `revoke` reads off
// at loop exit to turn its `first_child is None` post into the transitive `no_live_descendant`.
pub proof fn lemma_childless_no_descendant(m: Map<SlotId, CapSlot>, anc: SlotId)
    requires
        parent_has_first_child(m),
        m.dom().contains(anc),
        m[anc].first_child is None,
    ensures
        no_live_descendant(m, anc),
{
    assert forall|d: SlotId| m.dom().contains(d) implies !is_descendant(m, d, anc) by {
        if is_descendant(m, d, anc) {
            let path = choose|path: Seq<SlotId>| #[trigger] is_parent_path(m, path)
                && path.len() >= 2 && path[0] == d && path.last() == anc;
            let n = path.len();
            // The node just below `anc` on the path: its parent edge points at `anc`.
            let c = path[n - 2];
            assert(path[n - 1] == path.last());
            assert(m[c].parent == Some(path[n - 1]));  // is_parent_path link clause @ i = n-2
            assert(m.dom().contains(c));               // is_parent_path liveness clause @ i = n-2
            assert(m[anc].first_child is Some);        // parent_has_first_child @ k = c
            assert(false);
        }
    }
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
// into `cspace_wf` and preserved by the construction ops.
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

// ── Channel well-formedness (`chan_wf`) ──────────────────────────────────
//
// Ring index `i` is in channel `c`'s live window for `ring` iff it is one of the
// `count[ring]` positions starting at `head[ring]` (wrapping mod `depth`) — the
// FIFO window 3d's `send`/`recv` `Seq` model projects through. Stated as the
// existential so the modular arithmetic stays out of the predicate (the
// discipline: quarantine non-linear `%` into 3d's helpers, not the invariant).
pub open spec fn in_live_window(c: ChanView, ring: int, i: int) -> bool {
    exists|j: int| #![trigger (c.head[ring] + j) % (c.depth as int)]
        0 <= j < c.count[ring] && i == (c.head[ring] + j) % (c.depth as int)
}

// `chan_wf(cv, sv, ch)` — channel `ch` is well-formed. Takes **both** views: the
// An earlier `chan_wf` signature was `(cv, ch)`, but the clause "ring slots
// outside the live window are empty (their `SlotId` empty in `slot_view`)" needs
// the arena, so the signature is `(cv, sv, ch)`.
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
    // Load-bearing for `send`/`recv` — it is
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

// `chan_wf` reads only `ch`'s `depth`/`end_caps.len()`/`head`/`count`/`ring_cap`/`msg_len`/
// `bindings.dom()` (never a binding *value* nor an `end_caps` *count*) and the slot arena's
// dom + the *emptiness* of `ch`'s ring slots. So `chan_wf` is carried by any edit that fixes
// those `ChanView` fields and only **empties** slots (or leaves them) — exactly the window
// `delete`'s Channel branch lives in: `delete_prepare` clears the deleted cap's slot, then
// `endpoint_cap_dropped` decrements `end_caps[end]` (a count, not the `.len()`). Quarantined
// into a deterministic lemma: proving the lift inline relied on auto-derivation
// that flakes CI's Z3 once the strengthened `cap_consistent(Thread)` widened the context
// (the final-thread teardown trigger-perturbation hazard).
pub proof fn lemma_chan_wf_frame(
    cv0: Map<ObjId, ChanView>,
    cv1: Map<ObjId, ChanView>,
    sv0: Map<SlotId, CapSlot>,
    sv1: Map<SlotId, CapSlot>,
    ch: ObjId,
)
    requires
        chan_wf(cv0, sv0, ch),
        cv1.dom().contains(ch),
        cv1[ch].depth == cv0[ch].depth,
        cv1[ch].end_caps.len() == cv0[ch].end_caps.len(),
        cv1[ch].head == cv0[ch].head,
        cv1[ch].count == cv0[ch].count,
        cv1[ch].ring_cap == cv0[ch].ring_cap,
        cv1[ch].msg_len == cv0[ch].msg_len,
        cv1[ch].bindings.dom() == cv0[ch].bindings.dom(),
        sv1.dom() == sv0.dom(),
        // Each live slot is either unchanged or emptied — `chan_wf`'s only slot requirement is
        // that out-of-window ring slots (always in dom) are empty, which emptying preserves.
        forall|s: SlotId| #[trigger] sv1.dom().contains(s)
            ==> (sv1[s].cap == sv0[s].cap || is_empty_cap(sv1[s].cap)),
    ensures
        chan_wf(cv1, sv1, ch),
{
    // `in_live_window` reads only `head`/`count`/`depth`, all framed, so the window is identical.
    assert forall|r: int, i: int| #[trigger] in_live_window(cv1[ch], r, i)
        == in_live_window(cv0[ch], r, i) by {}
}

// `chan_wf` carried across an edit that fixes the `ChanView` and each slot's *emptiness*
// (not necessarily its cap content). The only slot facts `chan_wf` reads are the arena
// domain and the emptiness of out-of-window ring slots, so an edit that keeps every slot's
// emptiness — e.g. a frame cap's `mapping: None → Some` (a Frame stays non-empty) — carries
// it. The `lemma_chan_wf_frame` companion for `map_frame`, which records (not empties).
pub proof fn lemma_chan_wf_emptiness_frame(
    cv0: Map<ObjId, ChanView>,
    cv1: Map<ObjId, ChanView>,
    sv0: Map<SlotId, CapSlot>,
    sv1: Map<SlotId, CapSlot>,
    ch: ObjId,
)
    requires
        chan_wf(cv0, sv0, ch),
        cv1.dom().contains(ch),
        cv1[ch].depth == cv0[ch].depth,
        cv1[ch].end_caps.len() == cv0[ch].end_caps.len(),
        cv1[ch].head == cv0[ch].head,
        cv1[ch].count == cv0[ch].count,
        cv1[ch].ring_cap == cv0[ch].ring_cap,
        cv1[ch].msg_len == cv0[ch].msg_len,
        cv1[ch].bindings.dom() == cv0[ch].bindings.dom(),
        sv1.dom() == sv0.dom(),
        forall|s: SlotId| #[trigger] sv1.dom().contains(s)
            ==> is_empty_cap(sv1[s].cap) == is_empty_cap(sv0[s].cap),
    ensures
        chan_wf(cv1, sv1, ch),
{
    // `in_live_window` reads only `head`/`count`/`depth`, all framed, so the window is identical;
    // the out-of-window ring slots stay empty because every slot's emptiness is fixed.
    assert forall|r: int, i: int| #[trigger] in_live_window(cv1[ch], r, i)
        == in_live_window(cv0[ch], r, i) by {}
}

// ── The FIFO Seq model (the channel centerpiece) ─────────────────────────
//
// A queued message is `(len, caps)` — payload bytes abstracted, so its
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

// ── Notification waiter-queue well-formedness + the FIFO Seq model ─
//
// The waiter queue is a SINGLY-linked intrusive list threaded through the TCBs:
// `NotifView` holds `wait_head`/`wait_tail`, each waiting TCB holds `qnext` (next
// waiter) + `wait_notif` (its notification). Unlike the CDT sibling list it has no
// back-pointer, so the doubly-consistent membership trick does not apply — the clean
// model is an explicit FIFO `Seq` witness (the `ring_fifo` analog). `wait` pushes
// the tail (`Seq::push`, 4b), `signal` pops the head (`Seq::drop_first`, 4b),
// `remove_waiter` splices out one element — so "wake order = block order"
// is FIFO-ness of `waiter_seq`.

// A generic singly-linked-list acyclicity rank over an abstract successor map — the
// `valid_srank`/`sib_acyclic` analog, shared by the waiter queue (`succ` = `qnext`)
// and the armed-timer list (`succ` = `timer_next`): a strict decrease
// along `succ` makes the relation well-founded, so an unlink loop walking `succ`
// terminates. GHOST-only (the rank is an existential witness, no `Store` home). Over
// the `qnext` projection it is implied by `waiter_chain`'s `no_duplicates` (rank =
// position in the chain), so `notif_wf` need not assert it separately; it is the
// decreases mechanism instantiated here.
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
    // A queued waiter's priority is a valid ready-queue level. This rides `notif_wf`
    // (already threaded everywhere), so `signal` can discharge the woken head's
    // `priority < NUM_PRIOS` precondition for the faithful `make_runnable`/`ready_enqueue`
    // without a separate global `prio_bounded` invariant. Only `wait` (the sole appender)
    // must establish it for the blocking thread — a leaf precondition the kernel supplies.
    &&& forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() ==>
            (tv[ws[i]].priority as int) < NUM_PRIOS
}

// Notification `n` is well-formed: empty-queue head/tail agreement, and a waiter
// chain witness exists. No op PROVES this in 4a — defined for 4b/4c, exercised by
// `notif_wf_exec` (the `chan_wf` discipline).
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

// The uniqueness theorem (the central lemma).
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

// Binding-liveness companion to `chan_wf`:
// every bound endpoint event names a *live, well-formed* notification. STRUCTURAL only
// (`nv` domain + `notif_wf`) — no `refs` clause — which is exactly what makes it
// preservable across a fire: `signal` preserves `notif_wf` of the notification it
// signals and frames every other notif/TCB, and the enqueue/dequeue `slot_move` frames
// `notif_view`/`tcb_view`. So `fire`/`send`/`recv`/`endpoint_cap_dropped` can carry it
// in both `requires` and `ensures` (the `chan_wf` discipline), and `fire` discharges
// `signal`'s `notif_view`-domain + `notif_wf` preconditions from it. The waiter-release
// `refs[n] > 0` that `signal`'s wake path also needs is NOT here — it is not preservable
// across the `-1` without the refcount census (it belongs to the teardown phase),
// so it rides as a precondition-only clause on the fire-callers.
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
// the teardown phase) — unlike the structural `binding_notif_wf`.
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
        // Dom-guarded — the lemma only consumes the in-domain waiters of `m` (the chain
        // `m`'s head/tail thread, all in `tv.dom()`), so a caller need not reason about phantom
        // out-of-domain keys. A weakening of the precondition; every existing caller, which
        // supplied the un-guarded `forall`, satisfies it unchanged.
        forall|k: ObjId| #[trigger] tv[k].wait_notif == Some(m) && tv.dom().contains(k)
            ==> tv2[k] == tv[k],
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

// `signal`'s wake step: popping the head `t == ws0[0]` from a non-empty
// waiter chain yields `ws0.drop_first` in the post-state, given the head/tail were
// re-pointed past `t` (new head = `t`'s old `qnext`; tail dropped to `None` exactly when
// that is `None`) and only `t`'s TCB moved. Extracted so `signal`'s own body query stays
// under the solver rlimit (the decomposition discipline).
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
        // Only `n`'s waiters need to be unchanged (the chain `ws0.drop_first` is all
        // waiters of `n`). Weakened from `k != t ==> unchanged` so `signal`'s faithful enqueue —
        // which also re-threads the Runnable old ready-tail `p` (`wait_notif None`, not a
        // waiter of `n`) — can still discharge it.
        forall|k: ObjId| #![trigger tvf[k]]
            k != t && tv0[k].wait_notif == Some(n) ==> tvf[k] == tv0[k],
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
        // `dws[i]` is a waiter of `n` (covenant) and `!= t`, so the weakened frame freezes it.
        assert(tv0[ws0[i + 1]].wait_notif == Some(n));
        assert(tvf[dws[i]] == tv0[dws[i]]);
        assert(tv0[ws0[i + 1]].qnext == (if i + 2 < ws0.len() { Some(ws0[i + 2]) } else { None }));
    }
}

// A waiter chain `ws` for `o` transports verbatim across a frame that changes only `o`'s
// own notif view (held equal) and a *set* of TCBs none of which is on `o`'s chain: every
// chain node names `o` (clause 6), but every changed TCB has `wait_notif != Some(o)` in
// the source state, so no chain node is changed — and the chain holds in the target. The
// directional building block of `lemma_waiter_refs_frame`, set-shaped so it covers both
// `signal`'s single-node wake and `remove_waiter`'s two-node (head + predecessor) splice.
pub proof fn lemma_chain_frame_set(
    anv: Map<ObjId, NotifView>,
    atv: Map<ObjId, TcbView>,
    bnv: Map<ObjId, NotifView>,
    btv: Map<ObjId, TcbView>,
    o: ObjId,
    ws: Seq<ObjId>,
)
    requires
        waiter_chain(anv, atv, o, ws),
        bnv[o] == anv[o],
        btv.dom() == atv.dom(),
        forall|k: ObjId| #[trigger] btv[k] != atv[k] ==> atv[k].wait_notif != Some(o),
    ensures
        waiter_chain(bnv, btv, o, ws),
{
    assert forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() implies
        btv[ws[i]] == atv[ws[i]] by {
        assert(atv[ws[i]].wait_notif == Some(o));
        if btv[ws[i]] != atv[ws[i]] {
            assert(false);
        }
    }
}

// `waiter_refs(o)` is unchanged by an edit that holds `o`'s notif view fixed and changes
// only TCBs that are *not* on `o`'s chain (every changed TCB has `wait_notif != Some(o)`
// in both states). For `o != n`, both `signal`'s wake (it dequeues a waiter on `n`) and
// `remove_waiter`'s splice (it touches the removed head and its predecessor, both waiters
// on `n`) meet this: the chain set is preserved (transport both ways via
// `lemma_chain_frame_set`), so existence agrees and (by uniqueness) the chosen seq agrees;
// the robust `waiter_refs` keeps the no-chain case at 0 in both. The frame the wake/splice
// `refcount_sound` preservation rests on.
pub proof fn lemma_waiter_refs_frame(
    nv0: Map<ObjId, NotifView>,
    tv0: Map<ObjId, TcbView>,
    nvf: Map<ObjId, NotifView>,
    tvf: Map<ObjId, TcbView>,
    n: ObjId,
    o: ObjId,
)
    requires
        o != n,
        nvf[o] == nv0[o],
        tvf.dom() == tv0.dom(),
        forall|k: ObjId| #[trigger] tvf[k] != tv0[k]
            ==> tv0[k].wait_notif != Some(o) && tvf[k].wait_notif != Some(o),
    ensures
        waiter_refs(nvf, tvf, o) == waiter_refs(nv0, tv0, o),
{
    if exists|ws: Seq<ObjId>| waiter_chain(nv0, tv0, o, ws) {
        let a = waiter_seq(nv0, tv0, o);
        assert(waiter_chain(nv0, tv0, o, a));
        lemma_chain_frame_set(nv0, tv0, nvf, tvf, o, a);
        let b = waiter_seq(nvf, tvf, o);
        assert(waiter_chain(nvf, tvf, o, b));
        lemma_chain_frame_set(nvf, tvf, nv0, tv0, o, b);
        lemma_waiter_chain_unique(nv0, tv0, o, a, b);
    } else {
        // No chain for `o` in old ⇒ none in new either (a new chain would transport back).
        assert(!exists|ws: Seq<ObjId>| waiter_chain(nvf, tvf, o, ws)) by {
            if exists|ws: Seq<ObjId>| waiter_chain(nvf, tvf, o, ws) {
                let wf = choose|ws: Seq<ObjId>| waiter_chain(nvf, tvf, o, ws);
                lemma_chain_frame_set(nvf, tvf, nv0, tv0, o, wf);
            }
        }
    }
}

// `waiter_refs(o)` is unchanged by an edit that changes only `o`'s OWN notif view (and
// leaves it equal) with the TCB view fixed — the `signal` accumulate path's word-only
// edit (and any `nv[n].word` bump): `waiter_chain` reads `nv` only at the queried object
// `o` (the head/tail clauses), so `nvf[o] == nv0[o]` makes the chain predicate identical,
// hence the existence and the chosen seq agree. The accumulate-path companion of
// `lemma_waiter_refs_frame`.
pub proof fn lemma_waiter_refs_frame_nv(
    nv0: Map<ObjId, NotifView>,
    nvf: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    o: ObjId,
)
    requires
        nvf[o] == nv0[o],
    ensures
        waiter_refs(nvf, tv, o) == waiter_refs(nv0, tv, o),
{
    assert forall|ws: Seq<ObjId>| #[trigger] waiter_chain(nv0, tv, o, ws)
        == waiter_chain(nvf, tv, o, ws) by {}
    if exists|ws: Seq<ObjId>| waiter_chain(nv0, tv, o, ws) {
        let a = waiter_seq(nv0, tv, o);
        let b = waiter_seq(nvf, tv, o);
        assert(waiter_chain(nv0, tv, o, a));
        assert(waiter_chain(nvf, tv, o, b));
        assert(waiter_chain(nv0, tv, o, b));
        lemma_waiter_chain_unique(nv0, tv, o, a, b);
    } else {
        assert(!exists|ws: Seq<ObjId>| waiter_chain(nvf, tv, o, ws)) by {
            if exists|ws: Seq<ObjId>| waiter_chain(nvf, tv, o, ws) {
                let w = choose|ws: Seq<ObjId>| waiter_chain(nvf, tv, o, ws);
                assert(waiter_chain(nv0, tv, o, w));
            }
        }
    }
}

// `waiter_chain` (clause 6) forces every chain node to be **both** `BlockedNotif` *and*
// `wait_notif == Some(o)`. So a thread that is provably **off every chain** — `wait_notif is
// None` OR `state != BlockedNotif` — is in no `o`'s chain, and editing it cannot perturb any
// `waiter_chain`. This is the frame `destroy_tcb` needs that `lemma_waiter_refs_frame` (keyed
// on `wait_notif != Some(o)`) cannot give: a **Runnable thread with a stale `wait_notif`**
// (`unqueue_ready` leaves `wait_notif` untouched) violates that key yet is off-chain by state;
// a thread `remove_waiter` just spliced out is off-chain by its cleared `wait_notif`. The
// off-chain witness on the *changed* threads, set-shaped to cover both the chain in `atv` and
// the back-transport — the `lemma_chain_frame_set` analog keyed on off-chain-ness.
proof fn lemma_chain_frame_offchain(
    anv: Map<ObjId, NotifView>,
    atv: Map<ObjId, TcbView>,
    bnv: Map<ObjId, NotifView>,
    btv: Map<ObjId, TcbView>,
    o: ObjId,
    ws: Seq<ObjId>,
)
    requires
        waiter_chain(anv, atv, o, ws),
        bnv[o] == anv[o],
        btv.dom() == atv.dom(),
        forall|k: ObjId| #[trigger] btv[k] != atv[k]
            ==> (atv[k].wait_notif is None || atv[k].state != ThreadState::BlockedNotif),
    ensures
        waiter_chain(bnv, btv, o, ws),
{
    assert forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() implies
        btv[ws[i]] == atv[ws[i]] by {
        // `ws[i]` is on `o`'s chain (clause 6), so it is `BlockedNotif` *and* names `o` —
        // hence not in the changed set (which is off-chain on both counts).
        assert(atv[ws[i]].wait_notif == Some(o));
        assert(atv[ws[i]].state == ThreadState::BlockedNotif);
        if btv[ws[i]] != atv[ws[i]] {
            assert(false);
        }
    }
}

// `waiter_refs(o)` is unchanged by an edit that holds the notification view fixed and changes
// only threads that are **off every chain in both states** — exactly `destroy_tcb`'s
// `set_tcb_qnext`/`set_tcb_state` step (the woken/unqueued/spliced thread moves
// off-chain → off-chain). Transport the `tv0` chain forward and the `tvf` chain back through
// `lemma_chain_frame_offchain`, then `lemma_waiter_chain_unique` equates the chosen seqs; the
// no-chain case stays at 0 in both (a `tvf` chain would transport back to a `tv0` chain).
pub proof fn lemma_waiter_refs_frame_offchain(
    nv: Map<ObjId, NotifView>,
    tv0: Map<ObjId, TcbView>,
    tvf: Map<ObjId, TcbView>,
    o: ObjId,
)
    requires
        tvf.dom() == tv0.dom(),
        forall|k: ObjId| #[trigger] tvf[k] != tv0[k] ==> {
            &&& (tv0[k].wait_notif is None || tv0[k].state != ThreadState::BlockedNotif)
            &&& (tvf[k].wait_notif is None || tvf[k].state != ThreadState::BlockedNotif)
        },
    ensures
        waiter_refs(nv, tvf, o) == waiter_refs(nv, tv0, o),
{
    if exists|ws: Seq<ObjId>| waiter_chain(nv, tv0, o, ws) {
        let a = waiter_seq(nv, tv0, o);
        assert(waiter_chain(nv, tv0, o, a));
        lemma_chain_frame_offchain(nv, tv0, nv, tvf, o, a);
        let b = waiter_seq(nv, tvf, o);
        assert(waiter_chain(nv, tvf, o, b));
        lemma_chain_frame_offchain(nv, tvf, nv, tv0, o, b);
        lemma_waiter_chain_unique(nv, tv0, o, a, b);
    } else {
        assert(!exists|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws)) by {
            if exists|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws) {
                let wf = choose|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws);
                lemma_chain_frame_offchain(nv, tvf, nv, tv0, o, wf);
            }
        }
    }
}

// `waiter_refs(o)` is unchanged by an edit that changes only thread `t`, when `t` lies on
// `o`'s chain in NEITHER state. This is what `remove_waiter`'s *absent* path needs that the
// state/wait predicate cannot give: `destroy_tcb` clears a stale `wait_notif == Some(o)` on a
// thread that was provably never queued (`!waiter_seq(o).contains(t)`), and must frame
// `waiter_refs(o)` across that single-thread edit.
pub proof fn lemma_waiter_refs_frame_dequeued(
    nv: Map<ObjId, NotifView>,
    tv0: Map<ObjId, TcbView>,
    tvf: Map<ObjId, TcbView>,
    t: ObjId,
    o: ObjId,
)
    requires
        tvf.dom() == tv0.dom(),
        forall|k: ObjId| k != t ==> #[trigger] tvf[k] == tv0[k],
        forall|ws: Seq<ObjId>| waiter_chain(nv, tv0, o, ws) ==> !ws.contains(t),
        forall|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws) ==> !ws.contains(t),
    ensures
        waiter_refs(nv, tvf, o) == waiter_refs(nv, tv0, o),
{
    if exists|ws: Seq<ObjId>| waiter_chain(nv, tv0, o, ws) {
        let a = waiter_seq(nv, tv0, o);
        assert(waiter_chain(nv, tv0, o, a));
        assert(!a.contains(t));
        assert(waiter_chain(nv, tvf, o, a)) by {
            assert forall|i: int| #![trigger a[i]] 0 <= i < a.len() implies
                tvf[a[i]] == tv0[a[i]] by {
                assert(a.contains(a[i]));
            }
        }
        let b = waiter_seq(nv, tvf, o);
        assert(waiter_chain(nv, tvf, o, b));
        lemma_waiter_chain_unique(nv, tvf, o, a, b);
    } else {
        assert(!exists|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws)) by {
            if exists|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws) {
                let w = choose|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws);
                assert(!w.contains(t));
                assert(waiter_chain(nv, tv0, o, w)) by {
                    assert forall|i: int| #![trigger w[i]] 0 <= i < w.len() implies
                        tv0[w[i]] == tvf[w[i]] by {
                        assert(w.contains(w[i]));
                    }
                }
            }
        }
    }
}

// `waiter_refs(o)` is unchanged by a TCB edit that preserves every thread's *chain* fields —
// `qnext`, `wait_notif`, and `state`. `waiter_chain` reads only those (plus the domain and the
// notification view), so the chain predicate is identical in both states; hence the existence
// and the unique chosen sequence agree. This is the frame `thread::bind` (D-F2) needs for its
// `set_tcb_bind_bits` step, which writes only `bind_bits` — a field no census term reads, and on
// a thread that may itself be on a chain (so the off-chain frames don't apply).
pub proof fn lemma_waiter_refs_frame_fields(
    nv: Map<ObjId, NotifView>,
    tv0: Map<ObjId, TcbView>,
    tvf: Map<ObjId, TcbView>,
    o: ObjId,
)
    requires
        tvf.dom() == tv0.dom(),
        forall|k: ObjId| #[trigger] tvf[k].qnext == tv0[k].qnext,
        forall|k: ObjId| #[trigger] tvf[k].wait_notif == tv0[k].wait_notif,
        forall|k: ObjId| #[trigger] tvf[k].state == tv0[k].state,
        // `waiter_chain` carries a priority bound, so its frame transfer needs
        // priority preserved too (the callers — `bind_bits`, etc. — never write priority).
        forall|k: ObjId| #[trigger] tvf[k].priority == tv0[k].priority,
    ensures
        waiter_refs(nv, tvf, o) == waiter_refs(nv, tv0, o),
{
    if exists|ws: Seq<ObjId>| waiter_chain(nv, tv0, o, ws) {
        let a = waiter_seq(nv, tv0, o);
        assert(waiter_chain(nv, tv0, o, a));
        assert(waiter_chain(nv, tvf, o, a));
        let b = waiter_seq(nv, tvf, o);
        assert(waiter_chain(nv, tvf, o, b));
        lemma_waiter_chain_unique(nv, tvf, o, a, b);
    } else {
        assert(!exists|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws)) by {
            if exists|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws) {
                let w = choose|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws);
                assert(waiter_chain(nv, tv0, o, w));
            }
        }
    }
}

// `caps_consistent` is preserved by an edit that holds the slot/chan/timer/notif/cspace views
// fixed and changes only threads that are **off every chain in both states** (keeping each TCB's
// `bind_slots`/`cspace`). This is `destroy_tcb`'s `set_tcb_qnext`/`set_tcb_state` step (the
// halted thread moves off-chain → off-chain): no notification's chain — hence `notif_wf` —
// moves, and the strengthened Thread clause carries (a thread still BlockedNotif-on-`wn` in `s1`
// was unchanged, so its `notif_wf(wn)` is the one from `s0`). The notif-frozen, off-chain
// analog of `lemma_caps_consistent_frame`.
pub proof fn lemma_caps_consistent_frame_thread_offchain<S: Store>(s0: &S, s1: &S)
    requires
        caps_consistent(s0),
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.timer_view() == s0.timer_view(),
        s1.timer_head_view() == s0.timer_head_view(),
        s1.cspace_view() == s0.cspace_view(),
        s1.irq_view() == s0.irq_view(),
        s1.notif_view() == s0.notif_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].bind_slots == s0.tcb_view()[k].bind_slots,
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].cspace == s0.tcb_view()[k].cspace,
        forall|k: ObjId| #[trigger] s1.tcb_view()[k] != s0.tcb_view()[k] ==> {
            &&& (s0.tcb_view()[k].wait_notif is None
                    || s0.tcb_view()[k].state != ThreadState::BlockedNotif)
            &&& (s1.tcb_view()[k].wait_notif is None
                    || s1.tcb_view()[k].state != ThreadState::BlockedNotif)
        },
    ensures
        caps_consistent(s1),
{
    // `notif_wf` carries for every notification: its chain nodes are on-chain (BlockedNotif and
    // naming it), so they are *unchanged* (the off-chain hypothesis), so the chain is preserved.
    assert forall|m: ObjId| #[trigger] s0.notif_view().dom().contains(m)
        && notif_wf(s0.notif_view(), s0.tcb_view(), m) implies
        notif_wf(s1.notif_view(), s1.tcb_view(), m) by {
        let ws = waiter_seq(s0.notif_view(), s0.tcb_view(), m);
        assert(waiter_chain(s0.notif_view(), s0.tcb_view(), m, ws));
        lemma_chain_frame_offchain(s0.notif_view(), s0.tcb_view(), s1.notif_view(),
            s1.tcb_view(), m, ws);
    }
    assert forall|s: SlotId| #![trigger s1.slot_view()[s]]
        s1.slot_view().dom().contains(s) && !is_empty_cap(s1.slot_view()[s].cap)
        implies cap_consistent(s1, s1.slot_view()[s].cap) by {
        let c = s1.slot_view()[s].cap;
        assert(c == s0.slot_view()[s].cap);
        assert(cap_consistent(s0, c));
        match c.kind {
            CapKind::Notification(m) => {
                assert(s0.notif_view().dom().contains(m));
                assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
            }
            CapKind::Thread(m, _) => {
                if let Some(cs) = s1.tcb_view()[m].cspace {
                    assert(s0.tcb_view()[m].cspace == Some(cs));
                    assert(cspace_resident_wf(s0, cs));
                    assert(cspace_resident_wf(s1, cs));
                }
                if s1.tcb_view()[m].state == ThreadState::BlockedNotif {
                    if let Some(wn) = s1.tcb_view()[m].wait_notif {
                        // A changed thread is off-chain in `s1`, so `m` (BlockedNotif-on-`wn`)
                        // is unchanged; `notif_wf(s0, wn)` (from `cap_consistent(s0)`) carries.
                        assert(s1.tcb_view()[m] == s0.tcb_view()[m]);
                        assert(s0.notif_view().dom().contains(wn));
                        assert(notif_wf(s0.notif_view(), s0.tcb_view(), wn));
                    }
                }
            }
            CapKind::Channel(co, _) => {
                assert forall|e: int, v: int|
                    (0 <= e < 2 && 0 <= v < 3
                        && #[trigger] s1.chan_view()[co].bindings[(e, v)].notif is Some) implies {
                        let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                        s1.notif_view().dom().contains(m)
                            && notif_wf(s1.notif_view(), s1.tcb_view(), m)
                    } by {
                    let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                    assert(s0.chan_view()[co].bindings[(e, v)].notif == Some(m));
                    assert(s0.notif_view().dom().contains(m));
                    assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
                }
            }
            _ => {}
        }
    }
}

// `caps_consistent` is preserved by clearing a single thread `t`'s wait link — the
// membership analog of `lemma_caps_consistent_frame_thread_offchain` for `destroy_tcb`'s
// absent-`remove_waiter` case: `t` is BlockedNotif with a *stale* `wait_notif` (provably on no
// chain), so the predicate cannot see it off-chain, but membership can. The edit clears `t`'s
// wait link (leaving it off-chain by predicate in `s1`), holds every other view fixed, and `t`
// is on no chain in either state — so no notification's `notif_wf` moves.
pub proof fn lemma_caps_consistent_frame_thread_dequeued<S: Store>(s0: &S, s1: &S, t: ObjId)
    requires
        caps_consistent(s0),
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.timer_view() == s0.timer_view(),
        s1.timer_head_view() == s0.timer_head_view(),
        s1.cspace_view() == s0.cspace_view(),
        s1.irq_view() == s0.irq_view(),
        s1.notif_view() == s0.notif_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|k: ObjId| k != t ==> #[trigger] s1.tcb_view()[k] == s0.tcb_view()[k],
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].bind_slots == s0.tcb_view()[k].bind_slots,
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].cspace == s0.tcb_view()[k].cspace,
        // `t` after the edit is off-chain by predicate (its `wait_notif` was cleared), so its
        // own Thread-cap coherence clause is vacuous.
        s1.tcb_view()[t].wait_notif is None
            || s1.tcb_view()[t].state != ThreadState::BlockedNotif,
        // `t` is a node of no waiter chain in either state.
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s0.notif_view(), s0.tcb_view(), o, ws) ==> !ws.contains(t),
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s1.notif_view(), s1.tcb_view(), o, ws) ==> !ws.contains(t),
    ensures
        caps_consistent(s1),
{
    // `notif_wf(m)` carries: `m`'s chain nodes name `m` and `t` is on no chain, so every node
    // differs from `t`, hence is unchanged — the chain is preserved.
    assert forall|m: ObjId| #[trigger] s0.notif_view().dom().contains(m)
        && notif_wf(s0.notif_view(), s0.tcb_view(), m) implies
        notif_wf(s1.notif_view(), s1.tcb_view(), m) by {
        let ws = waiter_seq(s0.notif_view(), s0.tcb_view(), m);
        assert(waiter_chain(s0.notif_view(), s0.tcb_view(), m, ws));
        assert(!ws.contains(t));
        assert(waiter_chain(s1.notif_view(), s1.tcb_view(), m, ws)) by {
            assert forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() implies
                s1.tcb_view()[ws[i]] == s0.tcb_view()[ws[i]] by {
                assert(ws.contains(ws[i]));
            }
        }
    }
    assert forall|s: SlotId| #![trigger s1.slot_view()[s]]
        s1.slot_view().dom().contains(s) && !is_empty_cap(s1.slot_view()[s].cap)
        implies cap_consistent(s1, s1.slot_view()[s].cap) by {
        let c = s1.slot_view()[s].cap;
        assert(c == s0.slot_view()[s].cap);
        assert(cap_consistent(s0, c));
        match c.kind {
            CapKind::Notification(m) => {
                assert(s0.notif_view().dom().contains(m));
                assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
            }
            CapKind::Thread(m, _) => {
                if let Some(cs) = s1.tcb_view()[m].cspace {
                    assert(s0.tcb_view()[m].cspace == Some(cs));
                    assert(cspace_resident_wf(s0, cs));
                    assert(cspace_resident_wf(s1, cs));
                }
                if s1.tcb_view()[m].state == ThreadState::BlockedNotif {
                    if let Some(wn) = s1.tcb_view()[m].wait_notif {
                        // `m == t` is excluded (`t` is off-chain by predicate in `s1`), so `m`
                        // is unchanged and `notif_wf(s0, wn)` carries.
                        assert(m != t);
                        assert(s1.tcb_view()[m] == s0.tcb_view()[m]);
                        assert(s0.notif_view().dom().contains(wn));
                        assert(notif_wf(s0.notif_view(), s0.tcb_view(), wn));
                    }
                }
            }
            CapKind::Channel(co, _) => {
                assert forall|e: int, v: int|
                    (0 <= e < 2 && 0 <= v < 3
                        && #[trigger] s1.chan_view()[co].bindings[(e, v)].notif is Some) implies {
                        let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                        s1.notif_view().dom().contains(m)
                            && notif_wf(s1.notif_view(), s1.tcb_view(), m)
                    } by {
                    let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                    assert(s0.chan_view()[co].bindings[(e, v)].notif == Some(m));
                    assert(s0.notif_view().dom().contains(m));
                    assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
                }
            }
            _ => {}
        }
    }
}

// Halting + hold-clearing the single dead TCB `t` preserves `caps_consistent`
// (`destroy_tcb`'s halt + clear-before-unref steps). `t` is designated by
// no live cap (`refs[t] == 0`), so the only `cap_consistent` clauses reading `t`'s fields — a
// `Thread(t)` cap's `cspace_resident_wf`/waiter-coherence — are never instantiated; every other
// clause reads a framed field, and `t` lies on no waiter chain, so its edits break no
// notification's `notif_wf`. Unlike `_dequeued` this allows `t`'s `cspace`/`aspace`/`state`/
// `wait_notif`/`qnext` to change freely (the hold-clear), traded for the no-`Thread(t)`-cap fact.
pub proof fn lemma_caps_consistent_frame_thread_halt_clear<S: Store>(s0: &S, s1: &S, t: ObjId)
    requires
        caps_consistent(s0),
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.timer_view() == s0.timer_view(),
        s1.timer_head_view() == s0.timer_head_view(),
        s1.cspace_view() == s0.cspace_view(),
        s1.irq_view() == s0.irq_view(),
        s1.notif_view() == s0.notif_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|k: ObjId| k != t ==> #[trigger] s1.tcb_view()[k] == s0.tcb_view()[k],
        // `t` is designated by no live cap (sourced from `refs[t] == 0` at the call site).
        forall|s: SlotId| #[trigger] s0.slot_view().dom().contains(s)
            && !is_empty_cap(s0.slot_view()[s].cap)
            ==> !is_thread_cap_for(s0.slot_view()[s].cap, t),
        // `t` is a node of no waiter chain in either state.
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s0.notif_view(), s0.tcb_view(), o, ws) ==> !ws.contains(t),
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s1.notif_view(), s1.tcb_view(), o, ws) ==> !ws.contains(t),
    ensures
        caps_consistent(s1),
{
    // `notif_wf(m)` carries: `m`'s chain nodes name `m` and `t` is on no chain, so every node
    // differs from `t`, hence is unchanged — the chain is preserved (same as `_dequeued`).
    assert forall|m: ObjId| #[trigger] s0.notif_view().dom().contains(m)
        && notif_wf(s0.notif_view(), s0.tcb_view(), m) implies
        notif_wf(s1.notif_view(), s1.tcb_view(), m) by {
        let ws = waiter_seq(s0.notif_view(), s0.tcb_view(), m);
        assert(waiter_chain(s0.notif_view(), s0.tcb_view(), m, ws));
        assert(!ws.contains(t));
        assert(waiter_chain(s1.notif_view(), s1.tcb_view(), m, ws)) by {
            assert forall|i: int| #![trigger ws[i]] 0 <= i < ws.len() implies
                s1.tcb_view()[ws[i]] == s0.tcb_view()[ws[i]] by {
                assert(ws.contains(ws[i]));
            }
        }
    }
    assert forall|s: SlotId| #![trigger s1.slot_view()[s]]
        s1.slot_view().dom().contains(s) && !is_empty_cap(s1.slot_view()[s].cap)
        implies cap_consistent(s1, s1.slot_view()[s].cap) by {
        let c = s1.slot_view()[s].cap;
        assert(c == s0.slot_view()[s].cap);
        assert(cap_consistent(s0, c));
        // No live cap is `Thread(t)`.
        assert(!is_thread_cap_for(s0.slot_view()[s].cap, t));
        match c.kind {
            CapKind::Notification(m) => {
                assert(s0.notif_view().dom().contains(m));
                assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
            }
            CapKind::Thread(m, _) => {
                // `c` is a live `Thread(m)` cap, and no live cap is `Thread(t)`, so `m != t`;
                // hence `tcb[m]` is unchanged and its coherence clauses carry.
                assert(m != t);
                assert(s1.tcb_view()[m] == s0.tcb_view()[m]);
                if let Some(cs) = s1.tcb_view()[m].cspace {
                    assert(s0.tcb_view()[m].cspace == Some(cs));
                    assert(cspace_resident_wf(s0, cs));
                    assert(cspace_resident_wf(s1, cs));
                }
                if s1.tcb_view()[m].state == ThreadState::BlockedNotif {
                    if let Some(wn) = s1.tcb_view()[m].wait_notif {
                        assert(s0.notif_view().dom().contains(wn));
                        assert(notif_wf(s0.notif_view(), s0.tcb_view(), wn));
                    }
                }
            }
            CapKind::Channel(co, _) => {
                assert forall|e: int, v: int|
                    (0 <= e < 2 && 0 <= v < 3
                        && #[trigger] s1.chan_view()[co].bindings[(e, v)].notif is Some) implies {
                        let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                        s1.notif_view().dom().contains(m)
                            && notif_wf(s1.notif_view(), s1.tcb_view(), m)
                    } by {
                    let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                    assert(s0.chan_view()[co].bindings[(e, v)].notif == Some(m));
                    assert(s0.notif_view().dom().contains(m));
                    assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
                }
            }
            _ => {}
        }
    }
}

// A thread that is not a blocked waiter of any live chain lies on no waiter chain
// (`destroy_tcb`'s post-detach state). A `waiter_chain` node is
// `BlockedNotif` and names its notification, so `t` can be a node only of its own
// `wait_notif`'s chain; if `t` is not `BlockedNotif`, or `wait_notif is None`, or it is
// `notif_wf`-absent from that one chain, it is on no chain at all.
pub proof fn lemma_thread_off_all_chains<S: Store>(s: &S, t: ObjId)
    requires
        s.tcb_view()[t].state != ThreadState::BlockedNotif
        || s.tcb_view()[t].wait_notif is None
        || (s.tcb_view()[t].wait_notif matches Some(wn)
            && notif_wf(s.notif_view(), s.tcb_view(), wn)
            && !waiter_seq(s.notif_view(), s.tcb_view(), wn).contains(t)),
    ensures
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s.notif_view(), s.tcb_view(), o, ws) ==> !ws.contains(t),
{
    assert forall|o: ObjId, ws: Seq<ObjId>|
        waiter_chain(s.notif_view(), s.tcb_view(), o, ws) implies !ws.contains(t) by {
        if ws.contains(t) {
            let i = ws.index_of(t);
            assert(0 <= i < ws.len() && ws[i] == t);
            // A chain node names `o` and is `BlockedNotif`, so the first two disjuncts are out.
            assert(s.tcb_view()[t].wait_notif == Some(o)
                && s.tcb_view()[t].state == ThreadState::BlockedNotif);
            // The third disjunct then has `wn == o`, and `t` is absent from `o`'s *unique* chain —
            // but `ws` is that chain (uniqueness), contradicting `ws.contains(t)`.
            let wsq = waiter_seq(s.notif_view(), s.tcb_view(), o);
            assert(waiter_chain(s.notif_view(), s.tcb_view(), o, wsq));
            lemma_waiter_chain_unique(s.notif_view(), s.tcb_view(), o, wsq, ws);
        }
    }
}

// `remove_waiter`'s splice step: unlinking `t == ws0[k]` from a waiter
// chain yields `ws0.remove(k)` in the post-state, given the imperative link fixups —
// the head re-pointed past `t` when `t` was the head (`k == 0`), the predecessor's
// `qnext` re-threaded past `t` otherwise (`k > 0`), the tail dropped to the
// predecessor when `t` was the tail (`k == len-1`), and `t` itself cleared. The
// mid-list analog of `lemma_drop_first_chain` (which is the `k == 0` head-pop special
// case); singly-linked with no re-parenting, so a plain `Seq::remove`, not the
// rank-rescaled merge `cdt_unlink` needed. Extracted so `remove_waiter`'s own body
// query stays under the solver rlimit (the decomposition discipline).
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
        // `waiter_chain`'s priority covenant — the re-threaded predecessor keeps its
        // priority (the splice writes only its `qnext`), so the result chain stays bounded.
        k > 0 ==> tvf[ws0[k - 1]].priority == tv0[ws0[k - 1]].priority,
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

// ── ready-queue list model ───────────────────────────────────────────────
// The 32-level ready queue is, per level, a head+tail intrusive list over `Tcb.qnext`
// — the waiter-queue shape (`waiter_chain`/`waiter_seq`/`lemma_remove_chain`) with the
// notification `n` replaced by a priority `level`, `wait_head`/`wait_tail` by
// `ready_view().heads`/`tails` at `level`, and the per-element covenant
// `wait_notif == Some(n) && BlockedNotif` by `state == Runnable && priority == level`.
// Globally it adds (separate from `ready_wf`): `ready_complete` — the timer-list
// completeness discipline, every Runnable thread is charted on its level's chain, which
// makes `ready_unqueue`/`ready_dequeue` find their target — and a `u32` presence bitmap
// (`ready_bitmap_coherent`) whose bit `level` is set iff that level's chain is non-empty
// (what `top_ready` bit-scans). The link is the same `Tcb.qnext`, disambiguated by
// state — a thread is on the ready chain (Runnable) or a waiter chain (BlockedNotif),
// never both.

// `rs` is level `level`'s ready list in head-to-tail (FIFO) order. The `waiter_chain`
// analog: distinct (acyclic — index IS the rank), head/tail agree with the ends, `qnext`
// threads each to the next (last to `None`), every charted node is `Runnable` at
// `priority == level`.
pub open spec fn ready_chain(
    rv: ReadyView,
    tv: Map<ObjId, TcbView>,
    level: int,
    rs: Seq<ObjId>,
) -> bool {
    &&& rs.no_duplicates()
    &&& forall|i: int| #![trigger rs[i]] 0 <= i < rs.len() ==> tv.dom().contains(rs[i])
    &&& (rs.len() == 0 ==> rv.heads[level] is None && rv.tails[level] is None)
    &&& (rs.len() > 0 ==> rv.heads[level] == Some(rs[0])
                       && rv.tails[level] == Some(rs[rs.len() - 1]))
    &&& forall|i: int| #![trigger rs[i]] 0 <= i < rs.len() ==>
            tv[rs[i]].qnext == (if i + 1 < rs.len() { Some(rs[i + 1]) } else { None })
    &&& forall|i: int| #![trigger rs[i]] 0 <= i < rs.len() ==>
            tv[rs[i]].state == ThreadState::Runnable && tv[rs[i]].priority as int == level
}

// The FIFO ready `Seq` at `level` — well-defined when a chain exists (unique by the
// `qnext` threading, `lemma_ready_chain_unique`). `ready_enqueue` ⇒ `Seq::push`,
// `ready_dequeue` ⇒ `Seq::drop_first`, `ready_unqueue` ⇒ a splice (`Seq::remove`).
pub open spec fn ready_seq(rv: ReadyView, tv: Map<ObjId, TcbView>, level: int) -> Seq<ObjId> {
    choose|rs: Seq<ObjId>| ready_chain(rv, tv, level, rs)
}

// `rs1[k] == rs2[k]` for any in-bounds `k` — heads agree, then `qnext` threads each step
// (`lemma_chain_eq_at`, per level).
proof fn lemma_ready_chain_eq_at(
    rv: ReadyView,
    tv: Map<ObjId, TcbView>,
    level: int,
    rs1: Seq<ObjId>,
    rs2: Seq<ObjId>,
    k: int,
)
    requires
        ready_chain(rv, tv, level, rs1),
        ready_chain(rv, tv, level, rs2),
        0 <= k < rs1.len(),
        k < rs2.len(),
    ensures
        rs1[k] == rs2[k],
    decreases k,
{
    if k == 0 {
        assert(rv.heads[level] == Some(rs1[0]));
        assert(rv.heads[level] == Some(rs2[0]));
    } else {
        lemma_ready_chain_eq_at(rv, tv, level, rs1, rs2, k - 1);
        assert(tv[rs1[k - 1]].qnext == Some(rs1[k]));
        assert(tv[rs2[k - 1]].qnext == Some(rs2[k]));
    }
}

// No ready chain is a strict prefix of another (`lemma_chain_not_strict_prefix` per level).
proof fn lemma_ready_chain_not_strict_prefix(
    rv: ReadyView,
    tv: Map<ObjId, TcbView>,
    level: int,
    rs1: Seq<ObjId>,
    rs2: Seq<ObjId>,
)
    requires
        ready_chain(rv, tv, level, rs1),
        ready_chain(rv, tv, level, rs2),
        rs1.len() < rs2.len(),
    ensures
        false,
{
    if rs1.len() == 0 {
        assert(rv.heads[level] is None);
        assert(rv.heads[level] == Some(rs2[0]));
    } else {
        let k: int = rs1.len() as int - 1;
        lemma_ready_chain_eq_at(rv, tv, level, rs1, rs2, k);
        assert(tv[rs1[k]].qnext is None);
        assert(tv[rs2[k]].qnext == Some(rs2[k + 1]));
    }
}

// `ready_chain` determines `rs` uniquely (the `choose` in `ready_seq` is the FIFO order).
pub proof fn lemma_ready_chain_unique(
    rv: ReadyView,
    tv: Map<ObjId, TcbView>,
    level: int,
    rs1: Seq<ObjId>,
    rs2: Seq<ObjId>,
)
    requires
        ready_chain(rv, tv, level, rs1),
        ready_chain(rv, tv, level, rs2),
    ensures
        rs1 == rs2,
{
    if rs1.len() < rs2.len() {
        lemma_ready_chain_not_strict_prefix(rv, tv, level, rs1, rs2);
    }
    if rs2.len() < rs1.len() {
        lemma_ready_chain_not_strict_prefix(rv, tv, level, rs2, rs1);
    }
    assert forall|i: int| 0 <= i < rs1.len() implies rs1[i] == rs2[i] by {
        lemma_ready_chain_eq_at(rv, tv, level, rs1, rs2, i);
    }
    assert(rs1 =~= rs2);
}

// The splice lemma (`lemma_remove_chain` per level): removing `t == rs0[k]` from level
// `level`'s chain — predecessor re-threaded past `t`, head/tail fixed — yields the chain
// over `rs0.remove(k)`. Unlike the waiter splice, `t` itself is *not* cleared (the
// scheduler leaves a Runnable thread's `qnext`/state alone; `destroy_tcb` halts it
// afterwards), so `t` is merely excluded from the "every other TCB unchanged" clause.
pub proof fn lemma_ready_remove_chain(
    rv0: ReadyView,
    tv0: Map<ObjId, TcbView>,
    rvf: ReadyView,
    tvf: Map<ObjId, TcbView>,
    level: int,
    t: ObjId,
    rs0: Seq<ObjId>,
    k: int,
)
    requires
        ready_chain(rv0, tv0, level, rs0),
        0 <= k < rs0.len(),
        rs0[k] == t,
        tvf.dom() == tv0.dom(),
        // predecessor re-threaded past `t` (k>0: set_tcb_qnext(rs0[k-1], tv0[t].qnext)),
        // its covenant fields framed.
        k > 0 ==> tvf[rs0[k - 1]].qnext == tv0[t].qnext,
        k > 0 ==> tvf[rs0[k - 1]].state == tv0[rs0[k - 1]].state,
        k > 0 ==> tvf[rs0[k - 1]].priority == tv0[rs0[k - 1]].priority,
        // every other TCB unchanged (`t` excepted — the scheduler leaves it alone).
        forall|j: ObjId| #![trigger tvf[j]]
            j != t && (k == 0 || j != rs0[k - 1]) ==> tvf[j] == tv0[j],
        // head fix: k==0 ⇒ new head is `t`'s old qnext; else unchanged.
        k == 0 ==> rvf.heads[level] == tv0[t].qnext,
        k > 0 ==> rvf.heads[level] == rv0.heads[level],
        // tail fix: `t` was the tail (k==len-1) ⇒ tail drops to the predecessor; else
        // unchanged.
        k == rs0.len() - 1 ==> rvf.tails[level]
            == (if k == 0 { None::<ObjId> } else { Some(rs0[k - 1]) }),
        k < rs0.len() - 1 ==> rvf.tails[level] == rv0.tails[level],
    ensures
        ready_chain(rvf, tvf, level, rs0.remove(k)),
{
    let drs = rs0.remove(k);
    let len = rs0.len() as int;
    rs0.remove_ensures(k);

    // Clause 1: no_duplicates.
    assert(drs.no_duplicates()) by {
        assert forall|i: int, j: int|
            0 <= i < drs.len() && 0 <= j < drs.len() && i != j implies drs[i] != drs[j] by {
            let ii = if i < k { i } else { i + 1 };
            let jj = if j < k { j } else { j + 1 };
            assert(drs[i] == rs0[ii] && drs[j] == rs0[jj]);
            assert(ii != jj);
        }
    }

    // Clauses 2, 5, 6: per-node domain / qnext-threading / state+priority covenant.
    assert forall|i: int| #![trigger drs[i]] 0 <= i < drs.len() implies
        tvf.dom().contains(drs[i])
        && tvf[drs[i]].qnext == (if i + 1 < drs.len() { Some(drs[i + 1]) } else { None::<ObjId> })
        && tvf[drs[i]].state == ThreadState::Runnable
        && tvf[drs[i]].priority as int == level by {
        let ii = if i < k { i } else { i + 1 };
        assert(drs[i] == rs0[ii]);
        assert(tv0.dom().contains(rs0[ii]));
        assert(tv0[rs0[ii]].qnext == (if ii + 1 < len { Some(rs0[ii + 1]) } else { None::<ObjId> }));
        assert(tv0[rs0[ii]].state == ThreadState::Runnable && tv0[rs0[ii]].priority as int == level);
        if k > 0 && i == k - 1 {
            assert(ii == k - 1);
            assert(tv0[t].qnext == (if k + 1 < len { Some(rs0[k + 1]) } else { None::<ObjId> }));
            if i + 1 < drs.len() {
                assert(drs[i + 1] == rs0[k + 1]);
            } else {
                assert(k + 1 == len);
            }
        } else {
            assert(tvf[rs0[ii]] == tv0[rs0[ii]]);
            if i + 1 < drs.len() {
                let i1 = if i + 1 < k { i + 1 } else { i + 2 };
                assert(drs[i + 1] == rs0[i1]);
            }
        }
    }

    // Clauses 3, 4: head / tail of `drs`.
    if drs.len() == 0 {
        assert(tv0[rs0[0]].qnext is None);
        assert(rvf.heads[level] is None);
        assert(rvf.tails[level] is None);
    } else {
        if k == 0 {
            assert(drs[0] == rs0[1]);
            assert(tv0[rs0[0]].qnext == Some(rs0[1]));
            assert(rvf.heads[level] == Some(drs[0]));
        } else {
            assert(drs[0] == rs0[0]);
            assert(rv0.heads[level] == Some(rs0[0]));
            assert(rvf.heads[level] == Some(drs[0]));
        }
        let last = drs.len() - 1;
        if k == len - 1 {
            assert(k > 0);
            assert(last == k - 1);
            assert(drs[last] == rs0[k - 1]);
            assert(rvf.tails[level] == Some(drs[last]));
        } else {
            assert(rv0.tails[level] == Some(rs0[len - 1]));
            assert(drs[last] == rs0[len - 1]);
            assert(rvf.tails[level] == Some(drs[last]));
        }
    }
}

// `ready_chain` reads only `heads[level]`, `tails[level]`, and `tv[rs[i]]` — so a change
// that frames those (e.g. an op at a *different* level, or a touch to threads off `rs`)
// preserves the chain. The per-level frame the ops use to carry the 31 untouched levels.
pub proof fn lemma_ready_chain_frame(
    rv0: ReadyView,
    tv0: Map<ObjId, TcbView>,
    rvf: ReadyView,
    tvf: Map<ObjId, TcbView>,
    level: int,
    rs: Seq<ObjId>,
)
    requires
        ready_chain(rv0, tv0, level, rs),
        rvf.heads[level] == rv0.heads[level],
        rvf.tails[level] == rv0.tails[level],
        tvf.dom() == tv0.dom(),
        forall|i: int| #![trigger tvf[rs[i]]] 0 <= i < rs.len() ==> tvf[rs[i]] == tv0[rs[i]],
    ensures
        ready_chain(rvf, tvf, level, rs),
{
}

// The *field-based* chain frame — `ready_chain` reads only each member's `qnext`/`state`/
// `priority` (plus `rv`'s heads/tails and `tv`'s domain), so an edit preserving exactly those
// three fields at the chain members carries the chain even if it rewrites other fields (e.g.
// `report_terminal`'s `report`, `bind`'s `cspace`/`aspace`/`bind_*`).
pub proof fn lemma_ready_chain_frame_fields(
    rv0: ReadyView,
    tv0: Map<ObjId, TcbView>,
    rvf: ReadyView,
    tvf: Map<ObjId, TcbView>,
    level: int,
    rs: Seq<ObjId>,
)
    requires
        ready_chain(rv0, tv0, level, rs),
        rvf.heads[level] == rv0.heads[level],
        rvf.tails[level] == rv0.tails[level],
        tvf.dom() == tv0.dom(),
        forall|i: int| #![trigger tvf[rs[i]]] 0 <= i < rs.len()
            ==> tvf.dom().contains(rs[i])
                && tvf[rs[i]].qnext == tv0[rs[i]].qnext
                && tvf[rs[i]].state == tv0[rs[i]].state
                && tvf[rs[i]].priority == tv0[rs[i]].priority,
    ensures
        ready_chain(rvf, tvf, level, rs),
{
}

// When the chain at `level` is preserved (`lemma_ready_chain_frame`'s conclusion), so is
// `ready_seq` at `level` (uniqueness). Carries the per-level `ready_seq` equality the ops'
// `ready_complete` re-establishment needs for the 31 untouched levels.
pub proof fn lemma_ready_seq_frame(
    rv0: ReadyView,
    tv0: Map<ObjId, TcbView>,
    rvf: ReadyView,
    tvf: Map<ObjId, TcbView>,
    level: int,
)
    requires
        rvf.heads[level] == rv0.heads[level],
        rvf.tails[level] == rv0.tails[level],
        tvf.dom() == tv0.dom(),
        exists|rs: Seq<ObjId>| ready_chain(rv0, tv0, level, rs),
        forall|i: int| #![trigger tv0[ready_seq(rv0, tv0, level)[i]]]
            0 <= i < ready_seq(rv0, tv0, level).len()
            ==> tvf[ready_seq(rv0, tv0, level)[i]] == tv0[ready_seq(rv0, tv0, level)[i]],
    ensures
        ready_chain(rvf, tvf, level, ready_seq(rv0, tv0, level)),
        ready_seq(rvf, tvf, level) == ready_seq(rv0, tv0, level),
{
    let rs = ready_seq(rv0, tv0, level);
    assert(ready_chain(rv0, tv0, level, rs));
    lemma_ready_chain_frame(rv0, tv0, rvf, tvf, level, rs);
    lemma_ready_chain_unique(rvf, tvf, level, ready_seq(rvf, tvf, level), rs);
}

// Every Runnable thread is charted on its priority level's chain — the timer-list
// completeness discipline (`timer_complete`), which makes `ready_unqueue`/`ready_dequeue`
// find their target. Kept *separate* from `ready_wf`: `ready_unqueue` transiently leaves
// `t` Runnable-and-off-chain (until `destroy_tcb` halts it), so it preserves only the
// `except t` form.
pub open spec fn ready_complete(rv: ReadyView, tv: Map<ObjId, TcbView>) -> bool {
    forall|t: ObjId| #[trigger] tv.dom().contains(t) && tv[t].state == ThreadState::Runnable
        ==> (tv[t].priority as int) < NUM_PRIOS
            && ready_seq(rv, tv, tv[t].priority as int).contains(t)
            // A Runnable thread is not waiting on any notification. Folded in (rather
            // than threaded as a global `runnable_not_waiting` invariant) so `signal`'s census
            // frame can discharge `wait_notif != Some(o)` for the *old level-tail* `p` that a
            // faithful `make_runnable` re-threads — `p` is Runnable, hence charted here. The
            // sole appender (`make_runnable`/`ready_enqueue`) carries the matching leaf
            // precondition; `signal` supplies it by clearing `wait_notif` before the enqueue.
            && tv[t].wait_notif is None
}

// `ready_complete` with one Runnable thread `t` excepted — the liveness `ready_unqueue`
// preserves. The splice leaves `t` transiently Runnable-and-off-chain, so completeness
// holds for every *other* Runnable thread; `destroy_tcb` closes the gap by halting
// `t`. Kept a separate predicate (not a `ready_wf` conjunct) per the off-chain-`t` carve-out.
pub open spec fn ready_complete_except(rv: ReadyView, tv: Map<ObjId, TcbView>, t: ObjId) -> bool {
    forall|x: ObjId| #[trigger] tv.dom().contains(x) && tv[x].state == ThreadState::Runnable
        && x != t
        ==> (tv[x].priority as int) < NUM_PRIOS
            && ready_seq(rv, tv, tv[x].priority as int).contains(x)
            && tv[x].wait_notif is None
}

// The presence bitmap is coherent: bit `level` set ⇔ level `level`'s chain is non-empty.
// Links the `u32` map `top_ready` bit-scans to the per-level chains.
pub open spec fn ready_bitmap_coherent(rv: ReadyView, tv: Map<ObjId, TcbView>) -> bool {
    forall|level: int| #![trigger ready_seq(rv, tv, level)] 0 <= level < NUM_PRIOS as int ==>
        ((rv.bitmap & (1u32 << (level as u32))) != 0 <==> ready_seq(rv, tv, level).len() > 0)
}

// The ready queue is well-formed: per-level head/tail domain + empty-agreement, a chain
// witness per level, and bitmap coherence. (`ready_complete` is the separate liveness
// half — see its note.)
pub open spec fn ready_wf(rv: ReadyView, tv: Map<ObjId, TcbView>) -> bool {
    &&& rv.heads.dom() == Set::new(|i: int| 0 <= i < NUM_PRIOS as int)
    &&& rv.tails.dom() == Set::new(|i: int| 0 <= i < NUM_PRIOS as int)
    &&& forall|level: int| #![trigger rv.heads[level]] 0 <= level < NUM_PRIOS as int ==>
            (rv.heads[level] is None <==> rv.tails[level] is None)
    // The chain witness *is* `ready_seq` — stated directly (rather than `exists rs`) so the
    // conjunct has a trigger anchor (`ready_seq`) in its body and is re-provable without a
    // witness-surfacing by-block. Equivalent: `ready_chain(.., ready_seq(..))` ⟺ a chain exists.
    &&& forall|level: int| #![trigger ready_seq(rv, tv, level)] 0 <= level < NUM_PRIOS as int ==>
            ready_chain(rv, tv, level, ready_seq(rv, tv, level))
    &&& ready_bitmap_coherent(rv, tv)
}

// ── armed-timer list model ───────────────────────────────────────
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

// `disarm`'s splice step: unlinking `t == ts0[k]` from the armed list yields
// `ts0.remove(k)`. The `lemma_remove_chain` analog minus the tail fixup (no tail pointer)
// — the head re-pointed past `t` when `t` was the head (`k == 0`), the predecessor's
// `next` re-threaded past `t` otherwise (`k > 0`), and `t` itself dropped from the chain
// (its own post-state fields are irrelevant — it is no longer charted).
// `Seq::remove(k)` of a duplicate-free seq is duplicate-free. Generic and isolated in its
// own query — the `no_duplicates` `self[i] != self[j]` is an n² trigger (an n² trigger hazard),
// so proving it with only `Seq` in context (rather than inside the timer/waiter-chain proofs,
// whose `Map`/view definitions add instantiation pressure) keeps those proofs well under the
// rlimit across platforms (Z3's resource counting varies Linux↔macOS, so a borderline proof
// flakes in CI; this is the headroom fix).
pub proof fn lemma_seq_remove_no_dup<A>(s: Seq<A>, k: int)
    requires
        s.no_duplicates(),
        0 <= k < s.len(),
    ensures
        s.remove(k).no_duplicates(),
{
    let r = s.remove(k);
    s.remove_ensures(k);
    assert forall|i: int, j: int|
        0 <= i < r.len() && 0 <= j < r.len() && i != j implies r[i] != r[j] by {
        let ii = if i < k { i } else { i + 1 };
        let jj = if j < k { j } else { j + 1 };
        assert(r[i] == s[ii] && r[j] == s[jj]);
        assert(ii != jj);
    }
}

// Removing the unique occurrence of an element from a `no_duplicates` sequence
// leaves a sequence that no longer contains it — `destroy_tcb` uses this to prove the unqueued
// `t` is off its (post-splice) level chain, hence (with `ready_wf`) off every ready chain.
pub proof fn lemma_seq_remove_drops<A>(s: Seq<A>, k: int)
    requires
        s.no_duplicates(),
        0 <= k < s.len(),
    ensures
        !s.remove(k).contains(s[k]),
{
    let r = s.remove(k);
    s.remove_ensures(k);
    assert forall|j: int| 0 <= j < r.len() implies r[j] != s[k] by {
        let jj = if j < k { j } else { j + 1 };
        assert(r[j] == s[jj]);
        assert(jj != k);   // `no_duplicates` ⇒ `s[jj] != s[k]`
    }
}

// `spinoff_prover`: the canonical victim of the `cap_consistent`-strengthening batch
// contamination (offloaded to a `no_duplicates` lemma).
// the final-thread teardown strengthens `cap_consistent` *further* (two clauses), so its prior headroom
// is at elevated risk on a different CI Z3 seed; isolate it alongside its `push_head` sibling.
#[verifier::spinoff_prover]
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

    // `timer_chain` gives `ts0.no_duplicates`; the splice preserves it (offloaded query).
    assert(ts0.no_duplicates());
    lemma_seq_remove_no_dup(ts0, k);

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

// `arm`'s prepend step: pushing the freshly-armed `t` onto the head yields
// `pts` (the head-push of `ts0`, i.e. `[t] ++ ts0`). `ts0` is the post-`disarm` chain —
// `t` is not on it (it was just unarmed), and `arm` touches only `t`'s fields and the
// head scalar, so every prior node is intact. The lighter analog of `wait`'s tail-push.
//
// **`spinoff_prover` (cross-platform headroom).** This borderline
// `Seq`/chain proof flakes the rlimit on CI's Linux Z3 (resource counting varies
// Linux↔macOS) when `cap_consistent` is strengthened: Verus batches a
// module's goals in a shared SMT context, so the new clauses' axioms shift Z3's resource
// accounting for *unrelated* functions — strengthening `cap_consistent` can
// destabilize an unrelated timer proof's rlimit. Its
// own `no_duplicates` is offloaded (`lemma_push_head_nodup`), so the remaining
// headroom comes from isolating it into a dedicated Z3 instance, immune to the batch
// contamination — the standard Verus flakiness fix.
#[verifier::spinoff_prover]
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

// Per-armed-timer signal-precondition supply (the census fragment): an armed
// timer's bound notification is live and well-formed, holds the timer's own ref
// (`refs >= 1`), and — when it has a blocked waiter — the waiter's ref too (`refs >= 2`),
// so after `disarm` releases the timer's `-1` the waiter's survives and `signal`'s
// wake-release precondition (`wait_head is Some ⇒ refs > 0`) still holds. The armed-timer
// analog of `binding_notif_wf` + `binding_refs_ok`. `check_expired` preserves it across a
// fire by reconstructing the per-notification `refs` fractions from the full refcount census
// (`refcount_sound`): a notification shared by a second armed timer keeps `refs == census >=
// armed_timer_refs (+ waiter_refs)`, covering the general shared-notification case (D-E2).
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

// `s` is one of channel-view `cv`'s ring cap slots. `send`/`recv` require the
// caller's source/destination slots are NOT ring caps of the channel (the
// kernel naturally supplies cspace residents), so moving them disturbs no other
// queued message. Stated as an existential; its negation is the universal that
// auto-instantiates on a `ring_cap[(r,i,c)]` term.
pub open spec fn is_ring_cap_of(cv: ChanView, s: SlotId) -> bool {
    exists|r: int, i: int, c: int| #![trigger cv.ring_cap[(r, i, c)]]
        0 <= r < 2 && 0 <= i < cv.depth && 0 <= c < 4 && cv.ring_cap[(r, i, c)] == s
}

// Modular helpers (quarantine `%` reasoning in tiny lemmas so the big
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

// Teardown only ever *empties* slots, it never fills an empty one: `cdt_unlink`
// moves links not caps, `set_slot` only clears, and the recursive destructors only delete.
// This is the frame `delete`'s own `is_empty_cap(final[slot])` ensures rests on (`obj_unref`
// must leave the just-cleared slot empty) and that `destroy_channel`'s ring-cap loop carries
// to conclude every ring slot ends empty. Refs-free and slot-local, so it composes
// transitively across the teardown recursion.
pub open spec fn only_empties(sv0: Map<SlotId, CapSlot>, sv1: Map<SlotId, CapSlot>) -> bool {
    forall|s: SlotId|
        sv0.dom().contains(s) && is_empty_cap(sv0[s].cap) ==> is_empty_cap(#[trigger] sv1[s].cap)
}

// `only_empties` composes along the teardown chain (each `delete` empties some more).
pub proof fn lemma_only_empties_trans(
    a: Map<SlotId, CapSlot>,
    b: Map<SlotId, CapSlot>,
    c: Map<SlotId, CapSlot>,
)
    requires
        only_empties(a, b),
        only_empties(b, c),
        a.dom() == b.dom(),
    ensures
        only_empties(a, c),
{
    assert forall|s: SlotId| a.dom().contains(s) && is_empty_cap(a[s].cap) implies is_empty_cap(
        #[trigger] c[s].cap,
    ) by {
        assert(is_empty_cap(b[s].cap));
    }
}

// ── Refcount census: the stored refcount equals the count of designating slots
// (cspace residents; channel-queue and TCB-bind homes ride the same arena),
// plus the non-slot references (bindings/waiters/armed timers) the later
// phases add. The census is the slot count; the cross-home and
// non-slot terms land with channel/notification/thread. ──

pub open spec fn slot_refs(m: Map<SlotId, CapSlot>, obj: ObjId) -> nat {
    m.dom().filter(|k: SlotId| cap_obj(m[k].cap) == Some(obj)).len()
}

// The rev2§3.3 per-endpoint cap census: the number of live `Channel(ch, e)` caps in
// the slot arena. The kernel keeps `chan_view[ch].end_caps[e]` exactly equal to
// this (maintained by `endpoint_cap_added`/`_dropped`), so peer-closed fires when
// the *last* endpoint cap is gone. `end_caps_sound` (below) makes that equality a
// theorem — the invariant `delete`'s body needs to re-prove `caps_consistent`'s
// `end_caps[end] > 0` for any sibling channel cap after dropping one.
pub open spec fn end_cap_count(m: Map<SlotId, CapSlot>, ch: ObjId, e: int) -> nat {
    m.dom().filter(|k: SlotId| cap_chan_end(m[k].cap) == Some((ch, e))).len()
}

// A witnessing slot makes the endpoint count positive — `endpoint_cap_dropped` uses it to
// show a surviving sibling `Channel(ch, e)` cap keeps `end_caps[ch][e] > 0` after the
// decrement (via `end_caps_sound`). The non-empty-finite-set-has-positive-len fact.
pub proof fn lemma_end_cap_count_positive(m: Map<SlotId, CapSlot>, s: SlotId, ch: ObjId, e: int)
    requires
        m.dom().finite(),
        m.dom().contains(s),
        cap_chan_end(m[s].cap) == Some((ch, e)),
    ensures
        end_cap_count(m, ch, e) >= 1,
{
    let f = m.dom().filter(|k: SlotId| cap_chan_end(m[k].cap) == Some((ch, e)));
    assert(f.contains(s));
    assert(f.finite());
    if f.len() == 0 {
        assert(f =~= Set::empty());
    }
}

// A designating slot witnesses a positive slot census — `delete` uses it (with
// `lemma_in_refs_from_census`) to place the deleted cap's object in the refs domain.
pub proof fn lemma_slot_refs_positive(m: Map<SlotId, CapSlot>, s: SlotId, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(s),
        cap_obj(m[s].cap) == Some(o),
    ensures
        slot_refs(m, o) >= 1,
{
    let f = m.dom().filter(|k: SlotId| cap_obj(m[k].cap) == Some(o));
    assert(f.contains(s));
    assert(f.finite());
    if f.len() == 0 {
        assert(f =~= Set::empty());
    }
}

// No live cap designates a dead object: `refs[o] == 0` forces
// `obj_census(o) == 0` (refcount_sound), so `slot_refs(o) == 0` — no live cap has `cap_obj ==
// Some(o)`. `destroy_tcb` reads the `Thread(t)` instance off to discharge
// `lemma_caps_consistent_frame_thread_halt_clear`'s "no live `Thread(t)` cap" precondition (its
// subject `t` is dead, `refs[t] == 0`, by the time `obj_unref` calls the destructor).
pub proof fn lemma_no_live_thread_cap_from_dead<S: Store>(s: &S, t: ObjId)
    requires
        refcount_sound(s),
        s.slot_view().dom().finite(),
        s.refs_view().dom().contains(t),
        s.refs_view()[t] == 0,
    ensures
        forall|sl: SlotId| #[trigger] s.slot_view().dom().contains(sl)
            && !is_empty_cap(s.slot_view()[sl].cap)
            ==> !is_thread_cap_for(s.slot_view()[sl].cap, t),
{
    assert(s.refs_view()[t] == obj_census(s, t));
    assert forall|sl: SlotId| #[trigger] s.slot_view().dom().contains(sl)
        && !is_empty_cap(s.slot_view()[sl].cap)
        implies !is_thread_cap_for(s.slot_view()[sl].cap, t) by {
        if is_thread_cap_for(s.slot_view()[sl].cap, t) {
            assert(cap_obj(s.slot_view()[sl].cap) == Some(t));
            lemma_slot_refs_positive(s.slot_view(), sl, t);
        }
    }
}

// A mapping slot witnesses a positive frame-map census — the aspace analog of the above.
pub proof fn lemma_frame_map_positive(m: Map<SlotId, CapSlot>, s: SlotId, o: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(s),
        cap_frame_aspace(m[s].cap) == Some(o),
    ensures
        frame_map_refs(m, o) >= 1,
{
    let f = m.dom().filter(|k: SlotId| cap_frame_aspace(m[k].cap) == Some(o));
    assert(f.contains(s));
    assert(f.finite());
    if f.len() == 0 {
        assert(f =~= Set::empty());
    }
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

// Same-dom, same-caps arenas have the same `frame_map_refs` (the `lemma_same_caps_same_census`
// companion over `cap_frame_aspace`). `delete`/`cdt_unlink` link-only edits preserve caps, so
// the frame-mapping census carries.
proof fn lemma_same_caps_same_frame_map(m1: Map<SlotId, CapSlot>, m2: Map<SlotId, CapSlot>, o: ObjId)
    requires
        m1.dom() == m2.dom(),
        forall|k: SlotId| #[trigger] m1.dom().contains(k) ==> m1[k].cap == m2[k].cap,
    ensures
        frame_map_refs(m1, o) == frame_map_refs(m2, o),
{
    let s1 = m1.dom().filter(|k: SlotId| cap_frame_aspace(m1[k].cap) == Some(o));
    let s2 = m2.dom().filter(|k: SlotId| cap_frame_aspace(m2[k].cap) == Some(o));
    assert forall|k: SlotId| s1.contains(k) <==> s2.contains(k) by {
        if m1.dom().contains(k) {
            assert(m1[k].cap == m2[k].cap);
        }
    }
    assert(s1 =~= s2);
}

// Moving a cap from slot `a` (emptied) to a previously-empty slot `b` preserves both
// slot-derived census terms at every object: the designating-slot count is fixed because one
// slot loses the cap exactly as another gains it (the filter set merely swaps `a`↔`b`). This
// is the census frame `slot_move` exports operationally (`refs_view` fixed) but never as an
// `ensures`; `thread::bind` (D-F2) consumes it to carry `refcount_sound` across its
// notification-cap `slot_move`.
pub proof fn lemma_cap_move_census(
    pre: Map<SlotId, CapSlot>,
    post: Map<SlotId, CapSlot>,
    a: SlotId,
    b: SlotId,
    o: ObjId,
)
    requires
        pre.dom() == post.dom(),
        pre.dom().finite(),
        a != b,
        pre.dom().contains(a),
        pre.dom().contains(b),
        is_empty_cap(pre[b].cap),
        is_empty_cap(post[a].cap),
        post[b].cap == pre[a].cap,
        forall|x: SlotId| #[trigger] pre.dom().contains(x) && x != a && x != b
            ==> post[x].cap == pre[x].cap,
    ensures
        slot_refs(post, o) == slot_refs(pre, o),
        frame_map_refs(post, o) == frame_map_refs(pre, o),
{
    // An empty cap designates / maps nothing, so `a` (post) and `b` (pre) are in neither filter.
    let s1 = pre.dom().filter(|k: SlotId| cap_obj(pre[k].cap) == Some(o));
    let s2 = post.dom().filter(|k: SlotId| cap_obj(post[k].cap) == Some(o));
    assert(s1.finite());
    if cap_obj(pre[a].cap) == Some(o) {
        assert forall|k: SlotId| #![trigger s2.contains(k)]
            s2.contains(k) <==> s1.remove(a).insert(b).contains(k) by {
            if pre.dom().contains(k) && k != a && k != b { assert(post[k].cap == pre[k].cap); }
        }
        assert(s2 =~= s1.remove(a).insert(b));
        assert(s1.contains(a));
        assert(!s1.contains(b));
        assert(!s1.remove(a).contains(b));
    } else {
        assert forall|k: SlotId| #![trigger s2.contains(k)] s2.contains(k) <==> s1.contains(k) by {
            if pre.dom().contains(k) && k != a && k != b { assert(post[k].cap == pre[k].cap); }
        }
        assert(s2 =~= s1);
    }
    let g1 = pre.dom().filter(|k: SlotId| cap_frame_aspace(pre[k].cap) == Some(o));
    let g2 = post.dom().filter(|k: SlotId| cap_frame_aspace(post[k].cap) == Some(o));
    assert(g1.finite());
    if cap_frame_aspace(pre[a].cap) == Some(o) {
        assert forall|k: SlotId| #![trigger g2.contains(k)]
            g2.contains(k) <==> g1.remove(a).insert(b).contains(k) by {
            if pre.dom().contains(k) && k != a && k != b { assert(post[k].cap == pre[k].cap); }
        }
        assert(g2 =~= g1.remove(a).insert(b));
        assert(g1.contains(a));
        assert(!g1.contains(b));
        assert(!g1.remove(a).contains(b));
    } else {
        assert forall|k: SlotId| #![trigger g2.contains(k)] g2.contains(k) <==> g1.contains(k) by {
            if pre.dom().contains(k) && k != a && k != b { assert(post[k].cap == pre[k].cap); }
        }
        assert(g2 =~= g1);
    }
}

// Same-dom, same-caps arenas have the same `end_cap_count` (the `cap_chan_end` companion).
proof fn lemma_same_caps_same_end_cap(
    m1: Map<SlotId, CapSlot>,
    m2: Map<SlotId, CapSlot>,
    ch: ObjId,
    e: int,
)
    requires
        m1.dom() == m2.dom(),
        forall|k: SlotId| #[trigger] m1.dom().contains(k) ==> m1[k].cap == m2[k].cap,
    ensures
        end_cap_count(m1, ch, e) == end_cap_count(m2, ch, e),
{
    let s1 = m1.dom().filter(|k: SlotId| cap_chan_end(m1[k].cap) == Some((ch, e)));
    let s2 = m2.dom().filter(|k: SlotId| cap_chan_end(m2[k].cap) == Some((ch, e)));
    assert forall|k: SlotId| s1.contains(k) <==> s2.contains(k) by {
        if m1.dom().contains(k) {
            assert(m1[k].cap == m2[k].cap);
        }
    }
    assert(s1 =~= s2);
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

// ── The full refcount census. `refs[o]` must equal `obj_census(o)` —
// the recount over *every* reference to `o`: slot designations (`slot_refs`)
// plus the five non-slot terms phases 3/4/5 landed as per-op deltas, here
// assembled. Each term is a `slot_refs`-style filter/length (or a `waiter_seq`
// length). `refcount_sound` is the system invariant the teardown family (6c/6d)
// and the ref-touching construction ops (6f) preserve. ──

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
// finite when `cv.dom()` is. The rev2§3.6 binding term (the `binding_refs_ok` companion).
pub open spec fn binding_refs(cv: Map<ObjId, ChanView>, o: ObjId) -> nat {
    Set::new(
        |t: (ObjId, int, int)|
            cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3
                && cv[t.0].bindings[(t.1, t.2)].notif == Some(o),
    ).len()
}

// `binding_refs(o)` reads `cv` only through each channel's `bindings` map, so any edit
// that leaves every live channel's `bindings` unchanged (whatever else it touches —
// `end_caps`, `head`, `count`, …) frames the term. The frame `endpoint_cap_dropped`'s
// `refcount_sound` preservation needs: its `set_chan_end_caps` moves only `end_caps`.
pub proof fn lemma_binding_refs_frame(cv0: Map<ObjId, ChanView>, cvf: Map<ObjId, ChanView>, o: ObjId)
    requires
        cvf.dom() == cv0.dom(),
        forall|c: ObjId| #[trigger] cv0.dom().contains(c) ==> cvf[c].bindings == cv0[c].bindings,
    ensures
        binding_refs(cvf, o) == binding_refs(cv0, o),
{
    assert(Set::new(
        |t: (ObjId, int, int)|
            cvf.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3
                && cvf[t.0].bindings[(t.1, t.2)].notif == Some(o),
    ) =~= Set::new(
        |t: (ObjId, int, int)|
            cv0.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3
                && cv0[t.0].bindings[(t.1, t.2)].notif == Some(o),
    ));
}

// The immutable structural skeleton of every channel — `ring_cap` (the arena handles
// of each ring slot) and `depth` are fixed at channel construction and changed by *no*
// op (teardown clears bindings and `end_caps`, `send`/`recv` move head/count, but the
// layout never moves). `chan_struct_frame` is the teardown-wide frame that says exactly
// this survives: it is what lets `destroy_channel`'s ring-cap loop read
// `old.chan_view[ch].ring_cap` across the recursive `delete`s (the channel `ch` is not
// re-homed, its slot handles do not move) and conclude every *old* ring slot ends empty.
// Threaded through the SCC members (`delete`/`obj_unref`/`destroy_cspace`/`unref_cspace`/
// `destroy_channel`/`destroy_tcb`); the non-recursive callees (`endpoint_cap_dropped`,
// `set_chan_binding`, `destroy_notif`/`destroy_timer`'s full `chan_view` frame) supply it
// for free.
pub open spec fn chan_struct_frame(cv0: Map<ObjId, ChanView>, cvf: Map<ObjId, ChanView>) -> bool {
    &&& cvf.dom() == cv0.dom()
    &&& forall|ch: ObjId| #[trigger] cv0.dom().contains(ch)
            ==> cvf[ch].ring_cap == cv0[ch].ring_cap && cvf[ch].depth == cv0[ch].depth
}

// `chan_struct_frame` is reflexive and transitive — the loop/recursion composition the
// teardown bodies need (each `delete` preserves the skeleton; the whole loop does too).
pub proof fn lemma_chan_struct_frame_trans(
    cv0: Map<ObjId, ChanView>,
    cv1: Map<ObjId, ChanView>,
    cv2: Map<ObjId, ChanView>,
)
    requires
        chan_struct_frame(cv0, cv1),
        chan_struct_frame(cv1, cv2),
    ensures
        chan_struct_frame(cv0, cv2),
{
}

// Updating a single channel `ch` to a view that keeps its `ring_cap`/`depth` preserves the
// skeleton. The exact shape `endpoint_cap_dropped` (`end_caps`-only, `..old[ch]`) and
// `set_chan_binding` (`bindings`-only, `..old[ch]`) land, so `delete`/`destroy_channel` read
// `chan_struct_frame` off their ensures with this one lemma.
pub proof fn lemma_chan_field_update_struct_frame(cv: Map<ObjId, ChanView>, ch: ObjId, v: ChanView)
    requires
        cv.dom().contains(ch),
        v.ring_cap == cv[ch].ring_cap,
        v.depth == cv[ch].depth,
    ensures
        chan_struct_frame(cv, cv.insert(ch, v)),
{
    assert(cv.insert(ch, v).dom() =~= cv.dom());
}

// Blocked waiters on `o`: the length of `o`'s FIFO waiter chain — each blocked TCB
// holds one queued ref (the waiter term). **Robust** when no
// waiter chain exists (`o` is not a well-formed notification): then the term is 0, not
// the `choose`-garbage `waiter_seq` would otherwise yield. This matches the exec mirror
// (`waiter_count_exec` returns 0 for a non-notification `o`) and is what lets `signal`
// frame `waiter_refs(x)` for every `x != n` it does not signal: the wake perturbs only
// `n`'s chain and the woken head `t`, so for `x != n` the chain predicate is unchanged
// (`lemma_waiter_refs_frame`) — and an `x` with no chain stays at 0 in both states,
// which the bare `waiter_seq(x).len()` could not guarantee across the edit. Where a
// chain provably exists (every `notif_wf` notification), the `if` reduces to the plain
// length, so the notification contracts and the `obj_unref` Notification arm are unaffected.
pub open spec fn waiter_refs(nv: Map<ObjId, NotifView>, tv: Map<ObjId, TcbView>, o: ObjId) -> nat {
    if exists|ws: Seq<ObjId>| waiter_chain(nv, tv, o, ws) {
        waiter_seq(nv, tv, o).len()
    } else {
        0
    }
}

// A non-empty wait queue means at least one queued waiter ref — `delete` uses it to
// discharge `endpoint_cap_dropped`'s `binding_refs_ok` from the census (a bound notification
// with `wait_head is Some` has `refs >= census >= waiter_refs >= 1`).
pub proof fn lemma_waiter_refs_pos_from_head(
    nv: Map<ObjId, NotifView>,
    tv: Map<ObjId, TcbView>,
    o: ObjId,
)
    requires
        notif_wf(nv, tv, o),
        nv[o].wait_head is Some,
    ensures
        waiter_refs(nv, tv, o) >= 1,
{
    let ws = waiter_seq(nv, tv, o);
    assert(waiter_chain(nv, tv, o, ws));
    // `wait_head is Some` excludes the empty chain (which forces `wait_head is None`).
    assert(ws.len() >= 1);
}

// Armed timers naming `o`: each armed timer bound to `o` holds one queued ref while
// armed (the armed-timer term).
pub open spec fn armed_timer_refs(tmv: Map<ObjId, TimerView>, o: ObjId) -> nat {
    tmv.dom().filter(|k: ObjId| tmv[k].armed && tmv[k].notif == Some(o)).len()
}

// An armed timer bound to `o` witnesses a positive armed-timer count — `delete`'s Timer
// branch uses it to discharge `obj_unref`'s armed-notif-live precondition from the census.
pub proof fn lemma_armed_timer_refs_pos(tmv: Map<ObjId, TimerView>, t: ObjId, o: ObjId)
    requires
        tmv.dom().finite(),
        tmv.dom().contains(t),
        tmv[t].armed,
        tmv[t].notif == Some(o),
    ensures
        armed_timer_refs(tmv, o) >= 1,
{
    let f = tmv.dom().filter(|k: ObjId| tmv[k].armed && tmv[k].notif == Some(o));
    assert(f.contains(t));
    assert(f.finite());
    if f.len() == 0 {
        assert(f =~= Set::empty());
    }
}

// IRQ-object well-formedness: a bound IRQ names a notification — the per-object
// invariant the `irq_binding_refs` term and `destroy_irq`'s release rest on (the
// `armed ⇒ notif is Some` timer fact, minus the chain). No list, so no head/`timer_wf`
// existential — a pure pointwise predicate.
pub open spec fn irq_wf(irqv: Map<ObjId, IrqView>) -> bool {
    forall|k: ObjId| #[trigger] irqv.dom().contains(k) ==> (irqv[k].bound ==> irqv[k].notif is Some)
}

// IRQ bindings naming `o`: each bound IRQ holds one ref on its notification `o` (the
// irq-binding term — the `armed_timer_refs` twin, filtering on `bound && notif == Some(o)`).
pub open spec fn irq_binding_refs(irqv: Map<ObjId, IrqView>, o: ObjId) -> nat {
    irqv.dom().filter(|k: ObjId| irqv[k].bound && irqv[k].notif == Some(o)).len()
}

// A bound IRQ naming `o` witnesses a positive irq-binding count — the `delete`/`obj_unref`
// Irq branch uses it to discharge the bound-notif-live precondition from the census
// (the `lemma_armed_timer_refs_pos` twin).
pub proof fn lemma_irq_binding_refs_pos(irqv: Map<ObjId, IrqView>, i: ObjId, o: ObjId)
    requires
        irqv.dom().finite(),
        irqv.dom().contains(i),
        irqv[i].bound,
        irqv[i].notif == Some(o),
    ensures
        irq_binding_refs(irqv, o) >= 1,
{
    let f = irqv.dom().filter(|k: ObjId| irqv[k].bound && irqv[k].notif == Some(o));
    assert(f.contains(i));
    assert(f.finite());
    if f.len() == 0 {
        assert(f =~= Set::empty());
    }
}

// A reference making `census(n) >= 1` forces `refs[n] > 0` under a census off by one at any
// `z`: `refs[n] == census(n) + (1 if n == z else 0) >= 1`. `delete` uses it to discharge the
// refs-coupled preconditions of `endpoint_cap_dropped` (`binding_refs_ok`) and `obj_unref`
// (the Timer armed-notif-live) from the census in the off-by-one window.
pub proof fn lemma_refs_pos_from_off_by_one<S: Store>(store: &S, z: ObjId, n: ObjId)
    requires
        census_off_by_one(store, z),
        store.refs_view().dom().contains(n),
        obj_census(store, n) >= 1,
    ensures
        store.refs_view()[n] > 0,
{
    if n != z {
        assert(store.refs_view()[n] == obj_census(store, n));
    }
}

// Frame mappings naming `o`: each mapped frame cap holds one ref on its target
// aspace via the mapping field (the aspace term).
pub open spec fn frame_map_refs(sv: Map<SlotId, CapSlot>, o: ObjId) -> nat {
    sv.dom().filter(|k: SlotId| cap_frame_aspace(sv[k].cap) == Some(o)).len()
}

// Thread holds on `o`: a bound thread holds one ref on its cspace and one on its
// aspace — released by `destroy_tcb`'s `unref_cspace`/`unref_aspace`.
pub open spec fn thread_hold_refs(tv: Map<ObjId, TcbView>, o: ObjId) -> nat {
    tv.dom().filter(|k: ObjId| tv[k].cspace == Some(o)).len()
        + tv.dom().filter(|k: ObjId| tv[k].aspace == Some(o)).len()
}

// `thread_hold_refs(o)` depends only on every TCB's `cspace`/`aspace` fields, so any edit
// that leaves those two fields untouched (whatever else it changes — state, qnext,
// wait_notif, retval) frames the term. The frame `signal`/`remove_waiter`'s
// `refcount_sound` preservation needs: they move a TCB's queue/wait fields but never its
// cspace/aspace.
pub proof fn lemma_thread_hold_frame(tv0: Map<ObjId, TcbView>, tvf: Map<ObjId, TcbView>, o: ObjId)
    requires
        tvf.dom() == tv0.dom(),
        forall|k: ObjId| #[trigger] tvf[k].cspace == tv0[k].cspace,
        forall|k: ObjId| #[trigger] tvf[k].aspace == tv0[k].aspace,
    ensures
        thread_hold_refs(tvf, o) == thread_hold_refs(tv0, o),
{
    assert(tv0.dom().filter(|k: ObjId| tv0[k].cspace == Some(o))
        =~= tvf.dom().filter(|k: ObjId| tvf[k].cspace == Some(o)));
    assert(tv0.dom().filter(|k: ObjId| tv0[k].aspace == Some(o))
        =~= tvf.dom().filter(|k: ObjId| tvf[k].aspace == Some(o)));
}

// The recount: `refs[o]` must equal this over the whole store.
pub open spec fn obj_census<S: Store>(store: &S, o: ObjId) -> nat {
    slot_refs(store.slot_view(), o) + binding_refs(store.chan_view(), o) + waiter_refs(
        store.notif_view(),
        store.tcb_view(),
        o,
    ) + armed_timer_refs(store.timer_view(), o) + irq_binding_refs(store.irq_view(), o)
        + frame_map_refs(store.slot_view(), o) + thread_hold_refs(store.tcb_view(), o)
}

// Every live object's stored refcount equals its census. The teardown family
// assumes this at entry and re-establishes it at exit (the verification obligation).
pub open spec fn refcount_sound<S: Store>(store: &S) -> bool {
    forall|o: ObjId|
        store.refs_view().dom().contains(o) ==> store.refs_view()[o] == #[trigger] obj_census(
            store,
            o,
        )
}

// The refs domain *covers* every referenced object: anything with a positive census (a
// designating slot, a binding, a waiter, an armed timer, a frame mapping, or a thread hold)
// is in `refs_view.dom()`. `refcount_sound` only constrains objects *already* in the domain;
// this is the missing coverage `delete`'s body needs — its deleted cap's object `o` has
// `slot_refs(o) >= 1`, so `o ∈ refs.dom()`, which `obj_unref`/`unref_aspace` require and which
// `census_off_by_one(·, o)` itself presupposes. Preserved by teardown: an object only leaves
// `refs.dom()` when its `refs` (hence, by `refcount_sound`, its census) is already zero.
pub open spec fn census_dom_complete<S: Store>(store: &S) -> bool {
    forall|o: ObjId| #[trigger] obj_census(store, o) >= 1 ==> store.refs_view().dom().contains(o)
}

// A positive census witness is in the refs domain — the `census_dom_complete` consequence
// `delete` reads off to place its deleted cap's object (and the notifications its branches
// reference) into `refs.dom()` from the census.
pub proof fn lemma_in_refs_from_census<S: Store>(store: &S, o: ObjId)
    requires
        census_dom_complete(store),
        obj_census(store, o) >= 1,
    ensures
        store.refs_view().dom().contains(o),
{
}

// `refs[x] - census(x)` is unchanged for every object across an edit — `refs` and the
// census move in lockstep (additive form, so no `nat` underflow). The contract the fire/wait
// ops (`signal`/`fire`/`endpoint_cap_dropped`/`remove_waiter`) carry **unconditionally** (a
// wake/splice drops one waiter's queued `refs` and its `waiter_seq` length together; an
// `end_caps` decrement touches no census term). It is what `delete`'s body needs that the
// conditional `refcount_sound(old) ==> refcount_sound(final)` could not give: `delete` runs
// `endpoint_cap_dropped` in the window after clearing the deleted slot — where the census is
// off by one at the channel object, so `refcount_sound` is *false* — and must still carry
// the off-by-one across the peer-closed fire (`lemma_off_by_one_frozen`).
pub open spec fn census_delta_frozen<S: Store>(s0: &S, s1: &S) -> bool {
    &&& s1.refs_view().dom() == s0.refs_view().dom()
    // Trigger on `obj_census(s1, x)` — the *final* census — so the quantifier instantiates
    // only where someone reasons about the census (the teardown ops), not in census-agnostic
    // callers that merely carry the ensures (e.g. `check_expired`'s `signal`-in-a-loop, which
    // would otherwise blow the rlimit instantiating `obj_census` per object per iteration).
    &&& forall|x: ObjId| s0.refs_view().dom().contains(x)
            ==> s1.refs_view()[x] + obj_census(s0, x) == s0.refs_view()[x] + #[trigger] obj_census(s1, x)
}

// The census is sound everywhere **except** off by one at `z` (`refs[z] == census(z) + 1`):
// the `obj_unref` precondition shape after `delete` clears the deleted cap's slot but before
// `dec_ref` restores the count. `delete`'s Channel branch carries it across the peer-closed
// fire — the fire/wait ops state `census_off_by_one`-preservation as an `ensures` so it
// applies to the call automatically (the trigger keeps census-agnostic callers free of it).
pub open spec fn census_off_by_one<S: Store>(store: &S, z: ObjId) -> bool {
    &&& store.refs_view().dom().contains(z)
    &&& store.refs_view()[z] == obj_census(store, z) + 1
    &&& forall|x: ObjId| x != z && store.refs_view().dom().contains(x)
            ==> store.refs_view()[x] == obj_census(store, x)
}

// A teardown-stable frame for "dead, queue-detached" TCBs (the `tcb_view` frame the
// cross-object recursion needs). An object `x` with
// `refs[x] == 0` whose TCB entry is off every waiter queue (`wait_notif is None`) is untouched
// by any teardown op: `refs[x] == 0` ⟹ no cap designates it (so no `dec_ref`/`destroy_*`
// targets it, hence no `set_tcb_*` runs on it), and `wait_notif is None` ⟹ it sits on no
// notification's waiter chain (so no `signal`/`remove_waiter` wakes it — `signal`'s frame keys
// exactly on `wait_notif != Some(n)`). **Self-composing:** the antecedent is itself preserved
// (a frozen `tcb[x]` keeps `wait_notif is None`, and `refs[x]` stays `0` in-domain), so the
// frame threads through the cross-module cluster and `destroy_cspace`'s resident loop without an
// external refs-monotonicity lemma. The five non-`destroy_tcb` cluster members carry this base
// form; `destroy_tcb` carries it with its own subject excepted (it *does* rewrite `tcb[t]`,
// then re-qualifies `t` — halted with `wait_notif` cleared — to read `t`'s own postconditions
// off the recursive `unref_cspace`/`delete`).
// The per-object kernel of `dead_tcb_frozen` (a clean predicate so the system quantifier triggers
// on `dead_tcb_frozen_at(s0, s1, x)`, not the fragile `s1.tcb_view[x]` map index).
pub open spec fn dead_tcb_frozen_at<S: Store>(s0: &S, s1: &S, x: ObjId) -> bool {
    // A *Runnable* thread is not "dead" for freezing purposes — the faithful
    // `make_runnable`/`ready_unqueue` re-thread a Runnable node's `qnext` (the old ready-tail /
    // the spliced predecessor), which may be refs-0 yet legitimately on the ready queue (ready
    // membership carries no refcount). The teardown consumers only ever freeze Halted/Blocked
    // dead threads (`destroy_tcb` halts its subject before recursing), so excluding Runnable
    // threads here is sound and weakens the obligation precisely where the scheduler edits land.
    (s0.tcb_view().dom().contains(x) && s0.refs_view().dom().contains(x)
        && s0.refs_view()[x] == 0 && s0.tcb_view()[x].wait_notif is None
        && s0.tcb_view()[x].state != ThreadState::Runnable)
        ==> (s1.tcb_view().dom().contains(x) && s1.refs_view().dom().contains(x)
            && s1.refs_view()[x] == 0 && s1.tcb_view()[x] == s0.tcb_view()[x])
}

pub open spec fn dead_tcb_frozen<S: Store>(s0: &S, s1: &S) -> bool {
    forall|x: ObjId| #[trigger] dead_tcb_frozen_at(s0, s1, x)
}

// `dead_tcb_frozen` composes (the antecedent is self-preserving): the building block
// `destroy_cspace`'s resident loop and `delete`/`destroy_tcb`'s sequential teardown steps use to
// thread the frame across each sub-call.
pub proof fn lemma_dead_tcb_frozen_trans<S: Store>(s0: &S, s1: &S, s2: &S)
    requires
        dead_tcb_frozen(s0, s1),
        dead_tcb_frozen(s1, s2),
    ensures
        dead_tcb_frozen(s0, s2),
{
    assert forall|x: ObjId| #[trigger] dead_tcb_frozen_at(s0, s2, x) by {
        // Instantiate both frames at `x` (clean predicate triggers).
        assert(dead_tcb_frozen_at(s0, s1, x));
        assert(dead_tcb_frozen_at(s1, s2, x));
        // Chain: if `x` is dead+detached in s0, frame 1 makes it dead+detached in s1 (with
        // `tcb[x] == s0.tcb[x]`, so still `wait_notif is None`), then frame 2 reaches s2.
        if s0.tcb_view().dom().contains(x) && s0.refs_view().dom().contains(x)
            && s0.refs_view()[x] == 0 && s0.tcb_view()[x].wait_notif is None
            && s0.tcb_view()[x].state != ThreadState::Runnable {
            // The antecedent matches the weakened `dead_tcb_frozen_at` (Runnable
            // excluded), so frame 1 fires and pins `s1.tcb[x] == s0.tcb[x]`; that carries
            // `wait_notif is None` *and* `state != Runnable` into s1, firing frame 2 to s2.
            assert(s1.tcb_view()[x] == s0.tcb_view()[x]);
            assert(s1.tcb_view()[x].wait_notif is None);
            assert(s1.tcb_view()[x].state != ThreadState::Runnable);
        }
    }
}

// Derive `dead_tcb_frozen` from a **signal-shaped** edit: only TCBs
// waiting on `n` move (the woken/spliced waiter), and `refs` keeps already-dead objects dead. A
// dead (`refs 0`), detached (`wait_notif is None`) `x` is not waiting on `n`, so its TCB is
// frozen, and it stays dead. `signal`/`fire`/`endpoint_cap_dropped`/`remove_waiter` feed this
// with their fired/spliced `n`; `release_binding` feeds it with `n` arbitrary (no TCB moves, so
// the left disjunct holds for every `k`). `delete`/`destroy_channel` read the result off.
pub proof fn lemma_dead_tcb_frozen_signal_shaped<S: Store>(s0: &S, s1: &S, n: ObjId)
    requires
        s1.refs_view().dom() == s0.refs_view().dom(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|x: ObjId| s0.refs_view().dom().contains(x) && s0.refs_view()[x] == 0
            ==> #[trigger] s1.refs_view()[x] == 0,
        // A changed TCB is either an `n`-waiter (the woken/spliced head) OR a Runnable
        // node (the enqueue's re-threaded old ready-tail). A *dead* `x` (refs 0, detached,
        // non-Runnable per the weakened `dead_tcb_frozen_at`) is neither, so it is frozen.
        forall|k: ObjId| #[trigger] s1.tcb_view()[k] == s0.tcb_view()[k]
            || s0.tcb_view()[k].wait_notif == Some(n)
            || s0.tcb_view()[k].state == ThreadState::Runnable,
    ensures
        dead_tcb_frozen(s0, s1),
{
    assert forall|x: ObjId| #[trigger] dead_tcb_frozen_at(s0, s1, x) by {
        if s0.tcb_view().dom().contains(x) && s0.refs_view().dom().contains(x)
            && s0.refs_view()[x] == 0 && s0.tcb_view()[x].wait_notif is None
            && s0.tcb_view()[x].state != ThreadState::Runnable {
            // Dead `x`: `wait_notif None != Some(n)` and not Runnable, so the right two
            // disjuncts fail and the left (`tcb` frozen) holds; refs keeps it dead-in-domain.
            assert(s1.tcb_view()[x] == s0.tcb_view()[x]
                || s0.tcb_view()[x].wait_notif == Some(n)
                || s0.tcb_view()[x].state == ThreadState::Runnable);
        }
    }
}

// `dec_ref(o)` (with `refs[o] > 0`) is dead-tcb-frozen: it frames `tcb` whole and drops only
// `refs[o]` (a positive object, so never a dead one). `obj_unref`'s arms read it off after the
// `dec_ref` before composing with the at-zero destructor.
pub proof fn lemma_dead_tcb_frozen_dec_ref<S: Store>(s0: &S, s1: &S, o: ObjId)
    requires
        s0.refs_view().dom().contains(o),
        s0.refs_view()[o] > 0,
        s1.tcb_view() == s0.tcb_view(),
        s1.refs_view() == s0.refs_view().insert(o, (s0.refs_view()[o] - 1) as nat),
    ensures
        dead_tcb_frozen(s0, s1),
{
    // `insert` at the in-domain `o` keeps the refs domain (set extensionality).
    assert(s1.refs_view().dom() =~= s0.refs_view().dom());
    assert forall|x: ObjId| s0.refs_view().dom().contains(x) && s0.refs_view()[x] == 0
        implies #[trigger] s1.refs_view()[x] == 0 by {
        assert(x != o);
    }
    assert forall|k: ObjId| #[trigger] s1.tcb_view()[k] == s0.tcb_view()[k]
        || s0.tcb_view()[k].wait_notif == Some(o) by {}
    lemma_dead_tcb_frozen_signal_shaped(s0, s1, o);
}

// `dead_tcb_frozen` when `refs`/`tcb` are framed whole — a no-op step or a destructor whose
// every view is framed (`destroy_notif`). The reflexive base of the composition.
pub proof fn lemma_dead_tcb_frozen_refl<S: Store>(s0: &S, s1: &S)
    requires
        s1.refs_view() == s0.refs_view(),
        s1.tcb_view() == s0.tcb_view(),
    ensures
        dead_tcb_frozen(s0, s1),
{
    assert forall|x: ObjId| #[trigger] dead_tcb_frozen_at(s0, s1, x) by {}
}

// ── The except-`t` frame for `destroy_tcb`'s body. ──
//
// `destroy_tcb` rewrites its own halted subject `t` (halts it, clears its holds), so the *full*
// `dead_tcb_frozen` cannot hold across its body — but the frame `obj_unref`'s Thread arm needs
// is the **except-`t`** one (`forall x != t. dead_tcb_frozen_at(old, final, x)`, `t` excepted
// because `obj_unref` re-establishes `t`'s own facts separately). These three helpers build it:
// `_to_except` weakens a recursive call's full frame to except-`t`; `_except_single_t` supplies
// the except-`t` frame of `t`'s own halt/clear edits (which touch only `tcb[t]`); `_except_trans`
// composes two except-`t` segments. The body threads a running `forall x != t.
// dead_tcb_frozen_at(old, store, x)` invariant across its segments with these.

pub proof fn lemma_dead_tcb_frozen_to_except<S: Store>(s0: &S, s1: &S, t: ObjId)
    requires
        dead_tcb_frozen(s0, s1),
    ensures
        forall|x: ObjId| x != t ==> #[trigger] dead_tcb_frozen_at(s0, s1, x),
{
    assert forall|x: ObjId| x != t implies #[trigger] dead_tcb_frozen_at(s0, s1, x) by {
        assert(dead_tcb_frozen_at(s0, s1, x));
    }
}

pub proof fn lemma_dead_tcb_frozen_except_single_t<S: Store>(s0: &S, s1: &S, t: ObjId)
    requires
        s1.refs_view() == s0.refs_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|x: ObjId| x != t ==> #[trigger] s1.tcb_view()[x] == s0.tcb_view()[x],
    ensures
        forall|x: ObjId| x != t ==> #[trigger] dead_tcb_frozen_at(s0, s1, x),
{
    assert forall|x: ObjId| x != t implies #[trigger] dead_tcb_frozen_at(s0, s1, x) by {}
}

pub proof fn lemma_dead_tcb_frozen_except_trans<S: Store>(s0: &S, s1: &S, s2: &S, t: ObjId)
    requires
        forall|x: ObjId| x != t ==> #[trigger] dead_tcb_frozen_at(s0, s1, x),
        forall|x: ObjId| x != t ==> #[trigger] dead_tcb_frozen_at(s1, s2, x),
    ensures
        forall|x: ObjId| x != t ==> #[trigger] dead_tcb_frozen_at(s0, s2, x),
{
    assert forall|x: ObjId| x != t implies #[trigger] dead_tcb_frozen_at(s0, s2, x) by {
        assert(dead_tcb_frozen_at(s0, s1, x));
        assert(dead_tcb_frozen_at(s1, s2, x));
    }
}

// ── Home frame: slot "home" residency + the structured-emptying provenance frame. ──────────────
//
// A teardown op clears a slot's cap only for (a) the directly-deleted target or (b) a slot that
// is an internal **home handle** of some object it tore down: a `cspace` resident, a channel
// `ring_cap`, or a TCB `bind_slot`. `destroy_cspace` empties its residents, `destroy_channel`
// its ring caps, `destroy_tcb` its bind slots; everything else they empty is the recursive
// closure of those, themselves home handles. So an **un-homed** slot other than the deleted
// target keeps its exact cap across the whole teardown — the provenance frame `revoke` reads
// off to prove its root survives (a root that is no object's home handle can be emptied only by
// a *direct* `delete`, which `revoke` never does to its root — it deletes descendants). The
// three home maps are immutable across teardown (`cspace_view` framed equal, `ring_cap` via
// `chan_struct_frame`, `bind_slots` has no setter), so `is_homed` is stable; `home_views_frozen`
// packages exactly that stability for the composition lemmas.

pub open spec fn homed_in_cspace<S: Store>(s: &S, x: SlotId) -> bool {
    exists|cs: ObjId, i: int|
        #![trigger s.cspace_view()[cs].slots[i]]
        s.cspace_view().dom().contains(cs) && 0 <= i < s.cspace_view()[cs].slots.len()
            && s.cspace_view()[cs].slots[i] == x
}

pub open spec fn homed_in_chan<S: Store>(s: &S, x: SlotId) -> bool {
    exists|ch: ObjId, k: (int, int, int)|
        #![trigger s.chan_view()[ch].ring_cap[k]]
        s.chan_view().dom().contains(ch) && s.chan_view()[ch].ring_cap[k] == x
}

pub open spec fn homed_in_tcb<S: Store>(s: &S, x: SlotId) -> bool {
    exists|t: ObjId, j: int|
        #![trigger s.tcb_view()[t].bind_slots[j]]
        s.tcb_view().dom().contains(t) && 0 <= j < s.tcb_view()[t].bind_slots.len()
            && s.tcb_view()[t].bind_slots[j] == x
}

// `x` is some object's internal home handle (a cspace resident / channel ring cap / TCB bind
// slot). The teardown clears a non-target slot only if it is homed.
pub open spec fn is_homed<S: Store>(s: &S, x: SlotId) -> bool {
    homed_in_cspace(s, x) || homed_in_chan(s, x) || homed_in_tcb(s, x)
}

// Every un-homed slot other than `target` keeps its exact cap — the Home-frame provenance for an
// op with a single directly-deleted `target` (`delete`).
pub open spec fn unhomed_frozen<S: Store>(s0: &S, s1: &S, target: SlotId) -> bool {
    forall|x: SlotId|
        s1.slot_view().dom().contains(x) && x != target && !is_homed(s0, x)
            ==> #[trigger] s1.slot_view()[x].cap == s0.slot_view()[x].cap
}

// The target-free form (`obj_unref` and the destructors empty only homed slots — every slot they
// clear is one of their residents/ring-caps/bind-slots or the recursive closure of those).
pub open spec fn unhomed_frozen_free<S: Store>(s0: &S, s1: &S) -> bool {
    forall|x: SlotId|
        s1.slot_view().dom().contains(x) && !is_homed(s0, x)
            ==> #[trigger] s1.slot_view()[x].cap == s0.slot_view()[x].cap
}

// ── Death-provenance: the **provenance** frame — *which* homing object died when a homed slot is emptied.
//
// `unhomed_frozen` is the contrapositive-floor: un-homed slots are never emptied. Its dual, here,
// is the *positive* witness for a **homed** slot: when a non-target slot `x` *is* emptied by the
// teardown, some object `o` that **homes** `x` was **destroyed** (its `refs` reached zero — the
// object that, having lost its last cap, ran the destructor that cleared its home handle `x`).
// `revoke` reads this off to prove its root survives when *all* of the root's homing objects keep
// a live external reference: a homing object never reaches `refs == 0`, so by contraposition the
// root is never emptied (the resident-with-external-reference case).
//
// **Death model** (codebase reality — *not* dom-removal): a cspace/channel/TCB
// destructor leaves its object in `refs.dom()` with `refs == 0` (only `aspace_destroy` removes from
// the domain, and an aspace homes nothing). So `dead_obj` is the *disjunction* — out of the
// domain, or in it with zero refs — which (a) covers both seams and (b) is **monotone** across the
// whole teardown cluster (no teardown op ever re-refs or re-adds a dead object), the property
// `refs_death_persist` packages for the composition lemmas.

// `o` homes `x` *as a resident of cspace `o`* — the object-indexed `homed_in_cspace` (the witness
// `o` is fixed, not existentially chosen).
pub open spec fn homes_in_cspace<S: Store>(s: &S, o: ObjId, x: SlotId) -> bool {
    s.cspace_view().dom().contains(o) && exists|i: int|
        #![trigger s.cspace_view()[o].slots[i]]
        0 <= i < s.cspace_view()[o].slots.len() && s.cspace_view()[o].slots[i] == x
}

// `o` homes `x` *as a ring cap of channel `o`* — the object-indexed `homed_in_chan`.
pub open spec fn homes_in_chan<S: Store>(s: &S, o: ObjId, x: SlotId) -> bool {
    s.chan_view().dom().contains(o) && exists|k: (int, int, int)|
        #![trigger s.chan_view()[o].ring_cap[k]]
        s.chan_view()[o].ring_cap[k] == x
}

// `o` homes `x` *as a bind slot of TCB `o`* — the object-indexed `homed_in_tcb`.
pub open spec fn homes_in_tcb<S: Store>(s: &S, o: ObjId, x: SlotId) -> bool {
    s.tcb_view().dom().contains(o) && exists|j: int|
        #![trigger s.tcb_view()[o].bind_slots[j]]
        0 <= j < s.tcb_view()[o].bind_slots.len() && s.tcb_view()[o].bind_slots[j] == x
}

// `o` homes `x`: `x` is one of `o`'s internal home handles (a cspace resident, a channel ring cap,
// or a TCB bind slot). The existential-over-`o` of this is exactly `is_homed`.
pub open spec fn homes<S: Store>(s: &S, o: ObjId, x: SlotId) -> bool {
    homes_in_cspace(s, o, x) || homes_in_chan(s, o, x) || homes_in_tcb(s, o, x)
}

// `is_homed(s, x) <==> exists|o| homes(s, o, x)` — the object-indexed and existential forms agree
// disjunct-by-disjunct (each `homed_in_*` is the existential-over-`o` of `homes_in_*`).
pub proof fn lemma_is_homed_iff_homes<S: Store>(s: &S, x: SlotId)
    ensures
        is_homed(s, x) <==> exists|o: ObjId| homes(s, o, x),
{
    // (⟹) each `homed_in_*` witness `(obj, idx)` gives a homing object `obj`.
    if is_homed(s, x) {
        if homed_in_cspace(s, x) {
            let (cs, i) = choose|cs: ObjId, i: int|
                #![trigger s.cspace_view()[cs].slots[i]]
                s.cspace_view().dom().contains(cs) && 0 <= i < s.cspace_view()[cs].slots.len()
                    && s.cspace_view()[cs].slots[i] == x;
            assert(homes_in_cspace(s, cs, x));
            assert(homes(s, cs, x));
        } else if homed_in_chan(s, x) {
            let (ch, k) = choose|ch: ObjId, k: (int, int, int)|
                #![trigger s.chan_view()[ch].ring_cap[k]]
                s.chan_view().dom().contains(ch) && s.chan_view()[ch].ring_cap[k] == x;
            assert(homes_in_chan(s, ch, x));
            assert(homes(s, ch, x));
        } else {
            let (t, j) = choose|t: ObjId, j: int|
                #![trigger s.tcb_view()[t].bind_slots[j]]
                s.tcb_view().dom().contains(t) && 0 <= j < s.tcb_view()[t].bind_slots.len()
                    && s.tcb_view()[t].bind_slots[j] == x;
            assert(homes_in_tcb(s, t, x));
            assert(homes(s, t, x));
        }
    }
    // (⟸) a homing object `o` witnesses the matching `homed_in_*`.
    if exists|o: ObjId| homes(s, o, x) {
        let o = choose|o: ObjId| homes(s, o, x);
        if homes_in_cspace(s, o, x) {
            let i = choose|i: int|
                #![trigger s.cspace_view()[o].slots[i]]
                0 <= i < s.cspace_view()[o].slots.len() && s.cspace_view()[o].slots[i] == x;
            assert(homed_in_cspace(s, x));
        } else if homes_in_chan(s, o, x) {
            let k = choose|k: (int, int, int)|
                #![trigger s.chan_view()[o].ring_cap[k]]
                s.chan_view()[o].ring_cap[k] == x;
            assert(homed_in_chan(s, x));
        } else {
            let j = choose|j: int|
                #![trigger s.tcb_view()[o].bind_slots[j]]
                0 <= j < s.tcb_view()[o].bind_slots.len() && s.tcb_view()[o].bind_slots[j] == x;
            assert(homed_in_tcb(s, x));
        }
    }
}

// `homes` is stable across a home-frame edit (the object-indexed analog of `lemma_is_homed_stable`)
// — `cspace_view` equal, `ring_cap`/dom equal (`chan_struct_frame`), `bind_slots`/dom equal.
pub proof fn lemma_homes_stable<S: Store>(s0: &S, s1: &S, o: ObjId, x: SlotId)
    requires
        home_views_frozen(s0, s1),
    ensures
        homes(s0, o, x) == homes(s1, o, x),
{
    // Channel disjunct: dom equal and `ring_cap` equal per channel, so a witness transfers.
    if homes_in_chan(s0, o, x) {
        let k = choose|k: (int, int, int)|
            #![trigger s0.chan_view()[o].ring_cap[k]] s0.chan_view()[o].ring_cap[k] == x;
        assert(s1.chan_view()[o].ring_cap == s0.chan_view()[o].ring_cap);
        assert(s1.chan_view()[o].ring_cap[k] == x);
    }
    if homes_in_chan(s1, o, x) {
        let k = choose|k: (int, int, int)|
            #![trigger s1.chan_view()[o].ring_cap[k]] s1.chan_view()[o].ring_cap[k] == x;
        assert(s1.chan_view()[o].ring_cap == s0.chan_view()[o].ring_cap);
        assert(s0.chan_view()[o].ring_cap[k] == x);
    }
    // TCB disjunct: dom equal and `bind_slots` equal per TCB.
    if homes_in_tcb(s0, o, x) {
        let j = choose|j: int|
            #![trigger s0.tcb_view()[o].bind_slots[j]]
            0 <= j < s0.tcb_view()[o].bind_slots.len() && s0.tcb_view()[o].bind_slots[j] == x;
        assert(s1.tcb_view()[o].bind_slots == s0.tcb_view()[o].bind_slots);
        assert(s1.tcb_view()[o].bind_slots[j] == x);
    }
    if homes_in_tcb(s1, o, x) {
        let j = choose|j: int|
            #![trigger s1.tcb_view()[o].bind_slots[j]]
            0 <= j < s1.tcb_view()[o].bind_slots.len() && s1.tcb_view()[o].bind_slots[j] == x;
        assert(s1.tcb_view()[o].bind_slots == s0.tcb_view()[o].bind_slots);
        assert(s0.tcb_view()[o].bind_slots[j] == x);
    }
}

// An object is **dead** iff its last cap is gone: it left `refs.dom()` (`aspace_destroy`) *or* it
// sits in `refs.dom()` at `refs == 0` (every other destructor leaves its object so). Either way no
// live cap designates it.
pub open spec fn dead_obj<S: Store>(s: &S, o: ObjId) -> bool {
    !s.refs_view().dom().contains(o) || s.refs_view()[o] == 0
}

// "Dead stays dead": a teardown op never re-refs or re-adds a dead object. Monotone across the
// whole cluster (the ops only decrement / remove / leave-equal `refs`), so a death witnessed at an
// inner step persists to the outer final state — the refs-monotone fact `revoke`'s composition
// needs.
pub open spec fn refs_death_persist<S: Store>(s0: &S, s1: &S) -> bool {
    forall|o: ObjId| #[trigger] dead_obj(s1, o) || !dead_obj(s0, o)
}

// `refs_death_persist` is reflexive on a refs-framing step (no `refs` change ⟹ death is preserved).
pub proof fn lemma_refs_death_persist_from_refs_eq<S: Store>(s0: &S, s1: &S)
    requires
        s1.refs_view() == s0.refs_view(),
    ensures
        refs_death_persist(s0, s1),
{
}

// `refs_death_persist` composes (death at `a` survives to `b`, then to `c`).
pub proof fn lemma_refs_death_persist_trans<S: Store>(a: &S, b: &S, c: &S)
    requires
        refs_death_persist(a, b),
        refs_death_persist(b, c),
    ensures
        refs_death_persist(a, c),
{
    assert forall|o: ObjId| dead_obj(a, o) implies #[trigger] dead_obj(c, o) by {
        assert(dead_obj(b, o));
    }
}

// `refs_death_persist` across a `dec_ref(o)` step: `refs.dom()` is preserved and only `refs[o]`
// drops (by one), so any object dead at `s0` (`refs[y] == 0` ⟹ `y != o`, since `dec_ref` requires
// `refs[o] > 0`) stays dead.
pub proof fn lemma_refs_death_persist_dec_ref<S: Store>(s0: &S, s1: &S, o: ObjId)
    requires
        s1.refs_view() == s0.refs_view().insert(o, (s0.refs_view()[o] - 1) as nat),
        s1.refs_view().dom() == s0.refs_view().dom(),
        s0.refs_view().dom().contains(o),
        s0.refs_view()[o] > 0,
    ensures
        refs_death_persist(s0, s1),
{
    assert forall|y: ObjId| dead_obj(s0, y) implies #[trigger] dead_obj(s1, y) by {
        if s0.refs_view().dom().contains(y) && s0.refs_view()[y] == 0 {
            assert(y != o);
            assert(s1.refs_view()[y] == s0.refs_view()[y]);
        }
    }
}

// The **target-free** provenance frame: every non-target slot the teardown empties was a home
// handle of some object that died. (`delete`'s recursive destructors export this — every slot they
// clear is a resident / ring cap / bind slot of an object whose `refs` reached zero.)
pub open spec fn emptied_via_dead_home_free<S: Store>(s0: &S, s1: &S) -> bool {
    forall|x: SlotId|
        s1.slot_view().dom().contains(x) && !is_empty_cap(s0.slot_view()[x].cap)
            && is_empty_cap(#[trigger] s1.slot_view()[x].cap)
            ==> exists|o: ObjId| homes(s0, o, x) && dead_obj(s1, o)
}

// The **target-aware** provenance frame (`delete` exports this): the directly-deleted `target` is
// exempt (it is emptied by the direct CDT clear, not a home-handle teardown); every *other* emptied
// slot was a home handle of a dead object.
pub open spec fn emptied_via_dead_home<S: Store>(s0: &S, s1: &S, target: SlotId) -> bool {
    forall|x: SlotId|
        s1.slot_view().dom().contains(x) && x != target && !is_empty_cap(s0.slot_view()[x].cap)
            && is_empty_cap(#[trigger] s1.slot_view()[x].cap)
            ==> exists|o: ObjId| homes(s0, o, x) && dead_obj(s1, o)
}

// A slot-view-framing step (no cap moves) trivially satisfies the free frame (no slot is emptied,
// so the antecedent is empty).
pub proof fn lemma_emptied_via_dead_home_free_from_slot_eq<S: Store>(s0: &S, s1: &S)
    requires
        s1.slot_view() == s0.slot_view(),
    ensures
        emptied_via_dead_home_free(s0, s1),
{
    assert forall|x: SlotId|
        s1.slot_view().dom().contains(x) && !is_empty_cap(s0.slot_view()[x].cap)
            && is_empty_cap(#[trigger] s1.slot_view()[x].cap)
        implies exists|o: ObjId| homes(s0, o, x) && dead_obj(s1, o) by {
        assert(s1.slot_view()[x].cap == s0.slot_view()[x].cap);
    }
}

// When the directly-deleted `target` is itself a home handle of a dead object, the target-aware
// frame lifts to the free frame: the now-empty `target` gets its own death witness (the object
// whose destruction cleared it), and every other emptied slot already had one. The destructors use
// this to lift each resident / ring / bind `delete` to the free frame.
pub proof fn lemma_emptied_via_dead_home_free_from_homed<S: Store>(
    s0: &S, s1: &S, target: SlotId, o_w: ObjId)
    requires
        emptied_via_dead_home(s0, s1, target),
        homes(s0, o_w, target),
        dead_obj(s1, o_w),
    ensures
        emptied_via_dead_home_free(s0, s1),
{
    assert forall|x: SlotId|
        s1.slot_view().dom().contains(x) && !is_empty_cap(s0.slot_view()[x].cap)
            && is_empty_cap(#[trigger] s1.slot_view()[x].cap)
        implies exists|o: ObjId| homes(s0, o, x) && dead_obj(s1, o) by {
        if x == target {
            assert(homes(s0, o_w, target) && dead_obj(s1, o_w));
        }
    }
}

// `emptied_via_dead_home_free` composes across a sub-call. Requires (a) the home maps framed
// (`homes` stable, so a witness at `b` re-homes at `a`), (b) the slot domain preserved, and (c)
// refs-death-persistence `b → c` (so a death witnessed at `b` survives to `c`). The death-provenance analog
// of `lemma_unhomed_frozen_free_trans`.
pub proof fn lemma_emptied_via_dead_home_free_trans<S: Store>(a: &S, b: &S, c: &S)
    requires
        emptied_via_dead_home_free(a, b),
        emptied_via_dead_home_free(b, c),
        home_views_frozen(a, b),
        refs_death_persist(b, c),
        a.slot_view().dom() == b.slot_view().dom(),
        b.slot_view().dom() == c.slot_view().dom(),
    ensures
        emptied_via_dead_home_free(a, c),
{
    assert forall|x: SlotId|
        c.slot_view().dom().contains(x) && !is_empty_cap(a.slot_view()[x].cap)
            && is_empty_cap(#[trigger] c.slot_view()[x].cap)
        implies exists|o: ObjId| homes(a, o, x) && dead_obj(c, o) by {
        if is_empty_cap(b.slot_view()[x].cap) {
            // Emptied already by `a → b`: its witness `o` homes `x` at `a`, dead at `b`, and the
            // death persists to `c`.
            assert(b.slot_view().dom().contains(x));
            let o = choose|o: ObjId| homes(a, o, x) && dead_obj(b, o);
            assert(dead_obj(c, o));
            assert(homes(a, o, x));
        } else {
            // Emptied by `b → c`: its witness `o` homes `x` at `b`; `homes` is stable, so `o`
            // homes `x` at `a` too. `dead_obj(c, o)` comes straight from the `b → c` frame.
            assert(c.slot_view().dom().contains(x));
            let o = choose|o: ObjId| homes(b, o, x) && dead_obj(c, o);
            lemma_homes_stable(a, b, o, x);
            assert(homes(a, o, x));
        }
    }
}

// Compose `delete`'s target-aware frame with a following target-free step — the death-provenance analog of
// `lemma_unhomed_frozen_compose`.
pub proof fn lemma_emptied_via_dead_home_compose<S: Store>(a: &S, b: &S, c: &S, target: SlotId)
    requires
        emptied_via_dead_home(a, b, target),
        emptied_via_dead_home_free(b, c),
        home_views_frozen(a, b),
        refs_death_persist(b, c),
        a.slot_view().dom() == b.slot_view().dom(),
        b.slot_view().dom() == c.slot_view().dom(),
    ensures
        emptied_via_dead_home(a, c, target),
{
    assert forall|x: SlotId|
        c.slot_view().dom().contains(x) && x != target && !is_empty_cap(a.slot_view()[x].cap)
            && is_empty_cap(#[trigger] c.slot_view()[x].cap)
        implies exists|o: ObjId| homes(a, o, x) && dead_obj(c, o) by {
        if is_empty_cap(b.slot_view()[x].cap) {
            assert(b.slot_view().dom().contains(x));
            let o = choose|o: ObjId| homes(a, o, x) && dead_obj(b, o);
            assert(dead_obj(c, o));
            assert(homes(a, o, x));
        } else {
            assert(c.slot_view().dom().contains(x));
            let o = choose|o: ObjId| homes(b, o, x) && dead_obj(c, o);
            lemma_homes_stable(a, b, o, x);
            assert(homes(a, o, x));
        }
    }
}

// The three home maps are framed — the stability `is_homed` (hence the home frame) composes on.
// `cspace_view` equal + `ring_cap`/depth equal (`chan_struct_frame`) + the TCB domain and every
// TCB's immutable `bind_slots` fixed. Every teardown member ensures this (the leaves frame the
// views whole; the recursive members compose it).
pub open spec fn home_views_frozen<S: Store>(s0: &S, s1: &S) -> bool {
    // Note: this deliberately frames only the *home/residency* maps (cspace + chan skeleton +
    // tcb dom/bind_slots), NOT the object views — it is established *across* destructors that
    // change object state (incl. `destroy_irq`, which mutates `irq_view`), so no `irq_view`
    // conjunct belongs here (the `timer_view`/`notif_view` precedent).
    &&& s1.cspace_view() == s0.cspace_view()
    &&& chan_struct_frame(s0.chan_view(), s1.chan_view())
    &&& s1.tcb_view().dom() == s0.tcb_view().dom()
    &&& forall|t: ObjId| #[trigger] s1.tcb_view()[t].bind_slots == s0.tcb_view()[t].bind_slots
}

pub proof fn lemma_home_views_frozen_refl<S: Store>(s0: &S, s1: &S)
    requires
        s1.cspace_view() == s0.cspace_view(),
        s1.chan_view() == s0.chan_view(),
        s1.tcb_view() == s0.tcb_view(),
    ensures
        home_views_frozen(s0, s1),
{
    assert(chan_struct_frame(s0.chan_view(), s1.chan_view()));
}

pub proof fn lemma_home_views_frozen_trans<S: Store>(s0: &S, s1: &S, s2: &S)
    requires
        home_views_frozen(s0, s1),
        home_views_frozen(s1, s2),
    ensures
        home_views_frozen(s0, s2),
{
    lemma_chan_struct_frame_trans(s0.chan_view(), s1.chan_view(), s2.chan_view());
}

// `is_homed` is stable across a home-frame edit. `cspace_view` is equal, so the cspace
// disjunct is identical; the channel/TCB disjuncts transfer their witnesses through the per-key
// `ring_cap`/`bind_slots` equalities.
pub proof fn lemma_is_homed_stable<S: Store>(s0: &S, s1: &S, x: SlotId)
    requires
        home_views_frozen(s0, s1),
    ensures
        is_homed(s0, x) == is_homed(s1, x),
{
    // Channel: dom equal (`chan_struct_frame`) and `ring_cap` equal per channel, so a witness
    // `(ch, k)` for one side witnesses the other.
    if homed_in_chan(s0, x) {
        let (ch, k) = choose|ch: ObjId, k: (int, int, int)|
            #![trigger s0.chan_view()[ch].ring_cap[k]]
            s0.chan_view().dom().contains(ch) && s0.chan_view()[ch].ring_cap[k] == x;
        assert(s1.chan_view()[ch].ring_cap == s0.chan_view()[ch].ring_cap);
        assert(s1.chan_view()[ch].ring_cap[k] == x);
    }
    if homed_in_chan(s1, x) {
        let (ch, k) = choose|ch: ObjId, k: (int, int, int)|
            #![trigger s1.chan_view()[ch].ring_cap[k]]
            s1.chan_view().dom().contains(ch) && s1.chan_view()[ch].ring_cap[k] == x;
        assert(s1.chan_view()[ch].ring_cap == s0.chan_view()[ch].ring_cap);
        assert(s0.chan_view()[ch].ring_cap[k] == x);
    }
    // TCB: dom equal and `bind_slots` equal per TCB.
    if homed_in_tcb(s0, x) {
        let (t, j) = choose|t: ObjId, j: int|
            #![trigger s0.tcb_view()[t].bind_slots[j]]
            s0.tcb_view().dom().contains(t) && 0 <= j < s0.tcb_view()[t].bind_slots.len()
                && s0.tcb_view()[t].bind_slots[j] == x;
        assert(s1.tcb_view()[t].bind_slots == s0.tcb_view()[t].bind_slots);
        assert(s1.tcb_view()[t].bind_slots[j] == x);
    }
    if homed_in_tcb(s1, x) {
        let (t, j) = choose|t: ObjId, j: int|
            #![trigger s1.tcb_view()[t].bind_slots[j]]
            s1.tcb_view().dom().contains(t) && 0 <= j < s1.tcb_view()[t].bind_slots.len()
                && s1.tcb_view()[t].bind_slots[j] == x;
        assert(s1.tcb_view()[t].bind_slots == s0.tcb_view()[t].bind_slots);
        assert(s0.tcb_view()[t].bind_slots[j] == x);
    }
}

// A slot-view-framing step (no cap moves) trivially freezes every un-homed slot.
pub proof fn lemma_unhomed_frozen_free_from_slot_eq<S: Store>(s0: &S, s1: &S)
    requires
        s1.slot_view() == s0.slot_view(),
    ensures
        unhomed_frozen_free(s0, s1),
{
}

// When the directly-deleted target is itself homed (a resident / ring cap / bind slot), the
// target-aware frame is already target-free: every un-homed slot differs from the homed target.
// The destructors use this to lift each resident/ring/bind `delete` to the free frame.
pub proof fn lemma_unhomed_frozen_free_from_homed<S: Store>(s0: &S, s1: &S, target: SlotId)
    requires
        unhomed_frozen(s0, s1, target),
        is_homed(s0, target),
    ensures
        unhomed_frozen_free(s0, s1),
{
    assert forall|x: SlotId| s1.slot_view().dom().contains(x) && !is_homed(s0, x) implies
        #[trigger] s1.slot_view()[x].cap == s0.slot_view()[x].cap by {
        assert(x != target);
    }
}

// `unhomed_frozen_free` composes across a sub-call (homes immutable, slot dom preserved) — the
// home-frame analog of `lemma_only_empties_trans`.
pub proof fn lemma_unhomed_frozen_free_trans<S: Store>(a: &S, b: &S, c: &S)
    requires
        unhomed_frozen_free(a, b),
        unhomed_frozen_free(b, c),
        home_views_frozen(a, b),
        a.slot_view().dom() == b.slot_view().dom(),
        b.slot_view().dom() == c.slot_view().dom(),
    ensures
        unhomed_frozen_free(a, c),
{
    assert forall|x: SlotId| c.slot_view().dom().contains(x) && !is_homed(a, x) implies
        #[trigger] c.slot_view()[x].cap == a.slot_view()[x].cap by {
        lemma_is_homed_stable(a, b, x);
    }
}

// Compose `delete`'s target-aware frame with a following target-free step.
pub proof fn lemma_unhomed_frozen_compose<S: Store>(a: &S, b: &S, c: &S, target: SlotId)
    requires
        unhomed_frozen(a, b, target),
        unhomed_frozen_free(b, c),
        home_views_frozen(a, b),
        a.slot_view().dom() == b.slot_view().dom(),
        b.slot_view().dom() == c.slot_view().dom(),
    ensures
        unhomed_frozen(a, c, target),
{
    assert forall|x: SlotId|
        c.slot_view().dom().contains(x) && x != target && !is_homed(a, x) implies
        #[trigger] c.slot_view()[x].cap == a.slot_view()[x].cap by {
        lemma_is_homed_stable(a, b, x);
    }
}

// A frozen delta turns `refcount_sound` at the start into `refcount_sound` at the end —
// the form `destroy_tcb` consumes for its `remove_waiter` call (where `refcount_sound` holds).
pub proof fn lemma_refcount_sound_from_frozen<S: Store>(s0: &S, s1: &S)
    requires
        census_delta_frozen(s0, s1),
        refcount_sound(s0),
    ensures
        refcount_sound(s1),
{
    assert forall|x: ObjId| s1.refs_view().dom().contains(x) implies #[trigger] s1.refs_view()[x]
        == obj_census(s1, x) by {
        assert(s0.refs_view().dom().contains(x));
        assert(s0.refs_view()[x] == obj_census(s0, x));
    }
}

// The four teardown system invariants ride an edit that frames every object view + `refs`
// (a no-op step, or one whose only effect is scheduler-side like `unqueue_ready`). Plan
// the final-thread teardown `destroy_tcb`'s detach branches use it where the store is unchanged.
pub proof fn lemma_sysinv_frame_equal_views<S: Store>(s0: &S, s1: &S)
    requires
        refcount_sound(s0),
        caps_consistent(s0),
        end_caps_sound(s0),
        census_dom_complete(s0),
        s1.slot_view() == s0.slot_view(),
        s1.refs_view() == s0.refs_view(),
        s1.chan_view() == s0.chan_view(),
        s1.notif_view() == s0.notif_view(),
        s1.tcb_view() == s0.tcb_view(),
        s1.timer_view() == s0.timer_view(),
        s1.timer_head_view() == s0.timer_head_view(),
        s1.cspace_view() == s0.cspace_view(),
        s1.irq_view() == s0.irq_view(),
    ensures
        refcount_sound(s1),
        caps_consistent(s1),
        end_caps_sound(s1),
        census_dom_complete(s1),
{
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) == obj_census(s0, o) by {}
    lemma_refcount_sound_from_census_eq(s0, s1);
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) >= 1
        implies s1.refs_view().dom().contains(o) by {
        lemma_in_refs_from_census(s0, o);
    }
    // Every object view is equal to `s0`'s, so each live cap's (view-only) consistency carries
    // (the `unref_aspace`-body pattern — a per-slot forall avoids the bare-`caps_consistent`
    // existential/`choose` triggers).
    assert forall|s: SlotId| #![trigger s1.slot_view()[s]]
        s1.slot_view().dom().contains(s) && !is_empty_cap(s1.slot_view()[s].cap)
        implies cap_consistent(s1, s1.slot_view()[s].cap) by {
        assert(cap_consistent(s0, s0.slot_view()[s].cap));
    }
    assert(end_caps_sound(s1));
}

// The ready-queue invariants (`ready_wf` + `ready_complete`) ride an edit that frames both
// `ready_view` and `tcb_view` — the `lemma_sysinv_frame_equal_views` analogue for the ready pair.
// The cascade carriers (`delete`/`obj_unref`/`destroy_cspace`/`unref_cspace`/`destroy_channel`/
// `revoke`/`bind`, and the `endpoint_cap_dropped`/`signal` non-wake segments) discharge their
// ready obligation with one call to this across each object-only step; the `signal` wake and the
// `destroy_tcb` detach use the seam ops' own ensures instead. The predicates are pure functions of
// (`ready_view`, `tcb_view`), both held equal, so the body is empty.
pub proof fn lemma_ready_inv_frame<S: Store>(s0: &S, s1: &S)
    requires
        ready_wf(s0.ready_view(), s0.tcb_view()),
        ready_complete(s0.ready_view(), s0.tcb_view()),
        s1.ready_view() == s0.ready_view(),
        s1.tcb_view() == s0.tcb_view(),
    ensures
        ready_wf(s1.ready_view(), s1.tcb_view()),
        ready_complete(s1.ready_view(), s1.tcb_view()),
{
}

// The generalised ready frame — the invariants ride an edit that frames `ready_view` and
// changes `tcb_view` only at threads that are non-Runnable in BOTH states. A blocked/halted
// thread is on no ready chain (every chain member is Runnable by the `ready_chain` covenant), so
// the per-level chains — hence `ready_wf` — and the Runnable set with its `wait_notif`/charting —
// hence `ready_complete` — are untouched. Used by `signal`'s pre-enqueue fixups (which retarget
// only the still-`BlockedNotif` woken head), `remove_waiter` (a waiter-chain splice over
// `BlockedNotif` nodes), and `destroy_tcb`'s blocked/halt branches.
#[verifier::spinoff_prover]
#[verifier::rlimit(60)]
pub proof fn lemma_ready_inv_frame_offchain<S: Store>(s0: &S, s1: &S)
    requires
        ready_wf(s0.ready_view(), s0.tcb_view()),
        ready_complete(s0.ready_view(), s0.tcb_view()),
        s1.ready_view() == s0.ready_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|x: ObjId| #[trigger] s1.tcb_view()[x] != s0.tcb_view()[x]
            ==> s0.tcb_view()[x].state != ThreadState::Runnable
                && s1.tcb_view()[x].state != ThreadState::Runnable,
    ensures
        ready_wf(s1.ready_view(), s1.tcb_view()),
        ready_complete(s1.ready_view(), s1.tcb_view()),
{
    let rv = s0.ready_view();
    let tv0 = s0.tcb_view();
    let tv1 = s1.tcb_view();
    // Each level's chain is unchanged: its members are Runnable, hence not among the changed
    // (non-Runnable) threads, so framed; `lemma_ready_seq_frame` transfers the chain + seq.
    assert forall|level: int| #![trigger ready_seq(rv, tv1, level)] 0 <= level < NUM_PRIOS as int
        implies ready_seq(rv, tv1, level) == ready_seq(rv, tv0, level)
            && ready_chain(rv, tv1, level, ready_seq(rv, tv1, level)) by {
        let rs = ready_seq(rv, tv0, level);
        assert(ready_chain(rv, tv0, level, rs));
        assert forall|i: int| 0 <= i < rs.len() implies #[trigger] tv1[rs[i]] == tv0[rs[i]] by {
            assert(tv0[rs[i]].state == ThreadState::Runnable);
        }
        lemma_ready_seq_frame(rv, tv0, rv, tv1, level);
    }
    // ready_wf: domains + empty-agreement depend only on `rv`; the per-level chains carry
    // (above); bitmap coherence carries since each `ready_seq` is unchanged.
    assert(ready_wf(rv, tv1));
    // ready_complete: a Runnable `x` in `tv1` is unchanged (a changed thread is non-Runnable),
    // so it kept its level/`wait_notif` and its (unchanged) chain still charts it.
    assert forall|x: ObjId| #[trigger] tv1.dom().contains(x) && tv1[x].state == ThreadState::Runnable
        implies (tv1[x].priority as int) < NUM_PRIOS
            && ready_seq(rv, tv1, tv1[x].priority as int).contains(x)
            && tv1[x].wait_notif is None by {
        assert(tv1[x] == tv0[x]);
        let px = tv0[x].priority as int;
        assert(ready_seq(rv, tv1, px) == ready_seq(rv, tv0, px));
    }
}

// The *field-based* ready frame — the invariants ride an edit that frames `ready_view` and
// preserves the four ready-relevant fields (`state`/`priority`/`qnext`/`wait_notif`) of EVERY
// thread, even if it rewrites other fields. Used by `report_terminal` (sets `report`) and `bind`
// (sets `cspace`/`aspace`/`bind_*`) — neither touches a field `ready_wf`/`ready_complete` reads.
#[verifier::spinoff_prover]
#[verifier::rlimit(60)]
pub proof fn lemma_ready_inv_frame_fields<S: Store>(s0: &S, s1: &S)
    requires
        ready_wf(s0.ready_view(), s0.tcb_view()),
        ready_complete(s0.ready_view(), s0.tcb_view()),
        s1.ready_view() == s0.ready_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|x: ObjId| #[trigger] s1.tcb_view()[x].state == s0.tcb_view()[x].state
            && s1.tcb_view()[x].priority == s0.tcb_view()[x].priority
            && s1.tcb_view()[x].qnext == s0.tcb_view()[x].qnext
            && s1.tcb_view()[x].wait_notif == s0.tcb_view()[x].wait_notif,
    ensures
        ready_wf(s1.ready_view(), s1.tcb_view()),
        ready_complete(s1.ready_view(), s1.tcb_view()),
{
    let rv = s0.ready_view();
    let tv0 = s0.tcb_view();
    let tv1 = s1.tcb_view();
    // Each level's chain carries: the field-based chain frame needs only qnext/state/priority,
    // all preserved; uniqueness then transfers `ready_seq`.
    assert forall|level: int| #![trigger ready_seq(rv, tv1, level)] 0 <= level < NUM_PRIOS as int
        implies ready_seq(rv, tv1, level) == ready_seq(rv, tv0, level)
            && ready_chain(rv, tv1, level, ready_seq(rv, tv1, level)) by {
        let rs = ready_seq(rv, tv0, level);
        assert(ready_chain(rv, tv0, level, rs));
        lemma_ready_chain_frame_fields(rv, tv0, rv, tv1, level, rs);
        lemma_ready_chain_unique(rv, tv1, level, ready_seq(rv, tv1, level), rs);
    }
    assert(ready_wf(rv, tv1));
    // ready_complete: each Runnable `x` (same state/priority/wait_notif as in `tv0`) is still
    // Runnable in `tv0` and charted by its (unchanged) level chain.
    assert forall|x: ObjId| #[trigger] tv1.dom().contains(x) && tv1[x].state == ThreadState::Runnable
        implies (tv1[x].priority as int) < NUM_PRIOS
            && ready_seq(rv, tv1, tv1[x].priority as int).contains(x)
            && tv1[x].wait_notif is None by {
        assert(tv0[x].state == ThreadState::Runnable);
        let px = tv0[x].priority as int;
        assert(tv1[x].priority == tv0[x].priority);
        assert(ready_seq(rv, tv1, px) == ready_seq(rv, tv0, px));
    }
}

// A thread is off **every** ready chain when it is either non-Runnable or off its
// own priority level's chain — `ready_wf`'s per-level covenant (`state == Runnable && priority ==
// level`) forces any chain member to be a Runnable thread sitting at exactly that level, so a
// non-Runnable `t`, or a `t` absent from its own level's chain, cannot appear on any level's chain.
// `destroy_tcb` uses this at the post-detach snapshot: the Runnable branch's `unqueue_ready` left
// `t` off its level (the splice), the other branches have `t` non-Runnable.
pub proof fn lemma_thread_off_all_ready_chains<S: Store>(s: &S, t: ObjId)
    requires
        ready_wf(s.ready_view(), s.tcb_view()),
        s.tcb_view()[t].state != ThreadState::Runnable
            || !ready_seq(s.ready_view(), s.tcb_view(),
                    s.tcb_view()[t].priority as int).contains(t),
    ensures
        forall|level: int| 0 <= level < NUM_PRIOS as int
            ==> !(#[trigger] ready_seq(s.ready_view(), s.tcb_view(), level)).contains(t),
{
    let rv = s.ready_view();
    let tv = s.tcb_view();
    assert forall|level: int| 0 <= level < NUM_PRIOS as int
        implies !(#[trigger] ready_seq(rv, tv, level)).contains(t) by {
        let rs = ready_seq(rv, tv, level);
        if rs.contains(t) {
            assert(ready_chain(rv, tv, level, rs));
            let i = rs.index_of(t);
            assert(0 <= i < rs.len() && rs[i] == t);
            // `ready_chain`'s covenant: a chain member is Runnable at exactly `level`.
            assert(tv[t].state == ThreadState::Runnable);
            assert(tv[t].priority as int == level);
            assert(ready_seq(rv, tv, tv[t].priority as int).contains(t));
            assert(false);
        }
    }
}

// The `destroy_tcb` halt promotes `ready_complete_except(t)` back to full
// `ready_complete`. The detach left `t` off every ready chain (the splice removed it; a
// non-Runnable `t` was never on one), and the halt is the only tcb edit (`t` → Halted, `ready_view`
// framed). So every Runnable `x != t` is unchanged (still charted), and `t` is no longer Runnable —
// closing the `_except` gap; `ready_wf` rides too (the chains, all `t`-free, are unchanged).
pub proof fn lemma_ready_complete_halt_promote<S: Store>(s0: &S, s1: &S, t: ObjId)
    requires
        ready_wf(s0.ready_view(), s0.tcb_view()),
        ready_complete_except(s0.ready_view(), s0.tcb_view(), t),
        s1.ready_view() == s0.ready_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|x: ObjId| x != t ==> #[trigger] s1.tcb_view()[x] == s0.tcb_view()[x],
        s1.tcb_view()[t].state != ThreadState::Runnable,
        forall|level: int| 0 <= level < NUM_PRIOS as int
            ==> !(#[trigger] ready_seq(s0.ready_view(), s0.tcb_view(), level)).contains(t),
    ensures
        ready_wf(s1.ready_view(), s1.tcb_view()),
        ready_complete(s1.ready_view(), s1.tcb_view()),
{
    let rv = s0.ready_view();
    let tv0 = s0.tcb_view();
    let tv1 = s1.tcb_view();
    // Each level's chain is `t`-free, and `t` is the only changed thread, so every member is
    // unchanged → the chain + seq transfer (exactly the `_offchain` argument, keyed on `t`-absence).
    assert forall|level: int| #![trigger ready_seq(rv, tv1, level)] 0 <= level < NUM_PRIOS as int
        implies ready_seq(rv, tv1, level) == ready_seq(rv, tv0, level)
            && ready_chain(rv, tv1, level, ready_seq(rv, tv1, level)) by {
        let rs = ready_seq(rv, tv0, level);
        assert(ready_chain(rv, tv0, level, rs));
        assert forall|i: int| 0 <= i < rs.len() implies #[trigger] tv1[rs[i]] == tv0[rs[i]] by {
            assert(rs.contains(rs[i]));
            assert(rs[i] != t);
        }
        lemma_ready_seq_frame(rv, tv0, rv, tv1, level);
    }
    assert(ready_wf(rv, tv1));
    // ready_complete: a Runnable `x` in `tv1` has `x != t` (t is non-Runnable in tv1), so `x` is
    // unchanged and Runnable in tv0 → charted by `ready_complete_except`; its chain is unchanged.
    assert forall|x: ObjId| #[trigger] tv1.dom().contains(x) && tv1[x].state == ThreadState::Runnable
        implies (tv1[x].priority as int) < NUM_PRIOS
            && ready_seq(rv, tv1, tv1[x].priority as int).contains(x)
            && tv1[x].wait_notif is None by {
        assert(x != t);
        assert(tv1[x] == tv0[x]);
        let px = tv0[x].priority as int;
        assert(ready_seq(rv, tv1, px) == ready_seq(rv, tv0, px));
    }
}

// `refcount_sound` rides an edit that holds `refs` fixed and every object's census fixed — the
// `destroy_tcb` halt step (`lemma_census_frame_thread_halt` supplies the census equality, the
// `set_tcb_*` setters frame `refs`). Plan the final-thread teardown
pub proof fn lemma_refcount_sound_from_census_eq<S: Store>(s0: &S, s1: &S)
    requires
        refcount_sound(s0),
        s1.refs_view() == s0.refs_view(),
        forall|o: ObjId| #[trigger] obj_census(s1, o) == obj_census(s0, o),
    ensures
        refcount_sound(s1),
{
    assert forall|o: ObjId| s1.refs_view().dom().contains(o) implies #[trigger] s1.refs_view()[o]
        == obj_census(s1, o) by {
        assert(s0.refs_view()[o] == obj_census(s0, o));
    }
}

// A frozen delta carries the census **off-by-one at `z`** (the `obj_unref` precondition shape)
// from start to end — `delete`'s Channel branch consumes it across `endpoint_cap_dropped`.
pub proof fn lemma_off_by_one_frozen<S: Store>(s0: &S, s1: &S, z: ObjId)
    requires
        census_delta_frozen(s0, s1),
        s0.refs_view().dom().contains(z),
        s0.refs_view()[z] == obj_census(s0, z) + 1,
        forall|x: ObjId| x != z && s0.refs_view().dom().contains(x)
            ==> #[trigger] s0.refs_view()[x] == obj_census(s0, x),
    ensures
        s1.refs_view().dom().contains(z),
        s1.refs_view()[z] == obj_census(s1, z) + 1,
        forall|x: ObjId| x != z && s1.refs_view().dom().contains(x)
            ==> #[trigger] s1.refs_view()[x] == obj_census(s1, x),
{
    assert(s1.refs_view()[z] + obj_census(s0, z) == s0.refs_view()[z] + obj_census(s1, z));
    assert forall|x: ObjId| x != z && s1.refs_view().dom().contains(x) implies
        #[trigger] s1.refs_view()[x] == obj_census(s1, x) by {
        assert(s0.refs_view().dom().contains(x));
        assert(s1.refs_view()[x] + obj_census(s0, x) == s0.refs_view()[x] + obj_census(s1, x));
    }
}

// Cspace residency well-formedness: `cs` is a known cspace, its residency
// `Seq` agrees with `num_slots` (the getter contracts' precondition), and every resident
// slot handle is live in the arena. `destroy_cspace`'s loop reads `cspace_slot(cs, i)`
// and then `slot(sid)`, so it needs both the getter bounds and the residents-live fact;
// `obj_unref`/`unref_cspace` thread it to that loop. The kernel maintains it by
// construction (residency is fixed when the cspace is carved).
pub open spec fn cspace_resident_wf<S: Store>(store: &S, cs: ObjId) -> bool {
    &&& store.cspace_view().dom().contains(cs)
    &&& store.cspace_view()[cs].slots.len() == store.cspace_view()[cs].num_slots
    &&& forall|i: int| 0 <= i < store.cspace_view()[cs].slots.len()
            ==> #[trigger] store.slot_view().dom().contains(store.cspace_view()[cs].slots[i])
}

// ── Cap→object consistency. The teardown
// *body* proofs (the follow-on teardown) cannot run from `cspace_wf` + `refcount_sound`
// alone: `delete`'s body calls `endpoint_cap_dropped` (Channel branch) and `obj_unref`,
// both of which demand the *designated object's* well-formedness — `chan_wf`/`notif_wf`/
// `cspace_resident_wf`/the tcb-bind facts/`timer_wf` — none of which `cspace_wf` carries.
// Because the teardown recursion deletes *arbitrary-kind* caps (`destroy_cspace` over
// residents, `revoke` over descendants, `destroy_channel` over ring caps), each caller
// needs that wf for caps it doesn't statically know — so it must be a *system* invariant
// over every live cap, not a per-call precondition. This foundation states it; the body
// PR consumes it. Preservation across teardown rests on `refcount_sound`: a last-ref
// destroy leaves no cap designating the freed object, so no surviving cap's consistency
// can depend on it (the refs-coupled clauses below — the Channel `end_caps`/`binding_refs_ok`
// and the Timer armed-notif-live — are exactly that entanglement). ──

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
        CapKind::Thread(o, _) => {
            &&& store.tcb_view().dom().contains(o)
            &&& store.tcb_view()[o].bind_slots.len() == 2
            &&& store.slot_view().dom().contains(store.tcb_view()[o].bind_slots[0])
            &&& store.slot_view().dom().contains(store.tcb_view()[o].bind_slots[1])
            // The bound cspace is resident-wf (the final-thread teardown — the fifth
            // system invariant). `destroy_tcb`'s `unref_cspace` needs `cspace_resident_wf(cs)`
            // to drive the at-zero `destroy_cspace`, but by the time the destructor runs the
            // TCB's own cap is gone — so only the *live* Thread cap can witness it, supplied
            // through `delete` from this clause. Refs-free like every other arm:
            // `cspace_resident_wf` reads only `cspace_view` + `slot_view` dom + `tcb_view`, all
            // framed through teardown, so the `dec_ref` `-1` preserves it.
            &&& (store.tcb_view()[o].cspace matches Some(cs) ==> cspace_resident_wf(store, cs))
            // Waiter-coherence (the final-thread teardown, the sixth system invariant): a
            // BlockedNotif thread's `wait_notif` names a `notif_wf` notification — exactly the
            // precondition `destroy_tcb`'s `remove_waiter(wn, t)` needs (the refs side-condition
            // it then wants rides `refcount_sound`). The `notif_wf`-only form (no chain
            // membership) is what makes it framable: `destroy_notif` is a model view no-op, so a
            // notification never leaves `notif_view`, and a signal-shaped edit moves only
            // off-chain threads (`signal` wakes to `Runnable`, `remove_waiter` clears
            // `wait_notif`), neither of which can break a surviving blocked thread's `notif_wf`.
            &&& (store.tcb_view()[o].state == ThreadState::BlockedNotif ==>
                    (store.tcb_view()[o].wait_notif matches Some(wn) ==>
                        notif_wf(store.notif_view(), store.tcb_view(), wn)))
        }
        CapKind::Notification(o) => notif_wf(store.notif_view(), store.tcb_view(), o),
        CapKind::Timer(o) => {
            &&& store.timer_view().dom().contains(o)
            &&& store.timer_view().dom().finite()
            &&& timer_wf(store.timer_view(), store.timer_head_view())
        }
        CapKind::Irq(o) => {
            &&& store.irq_view().dom().contains(o)
            &&& store.irq_view().dom().finite()
            &&& irq_wf(store.irq_view())
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
    // The channel arena is finite too (the binding-census recount `lemma_binding_drop`
    // needs it — its triple set is a subset of `chan_view.dom() × {0,1} × {0,1,2}`). Refs-free
    // and structural like the slot-finiteness companion; every mutator frames `chan_view` or
    // `insert`s one channel, both finiteness-preserving.
    &&& store.chan_view().dom().finite()
    // The TCB arena is finite too (the `destroy_tcb` needs it for the
    // `thread_hold_refs` recount when it clears `tcb.cspace`/`tcb.aspace`). Refs-free and
    // structural like the slot/chan companions; every mutator frames `tcb_view` or `insert`s
    // one TCB, both finiteness-preserving.
    &&& store.tcb_view().dom().finite()
    // The IRQ arena is finite too: the `irq_binding_refs` recount is a
    // `dom().filter().len()`, so the lockstep census delta the bind/unbind/destroy ops export
    // needs it. Refs-free and structural like the slot/chan/tcb companions; every mutator
    // frames `irq_view` or `insert`s one IRQ, both finiteness-preserving.
    &&& store.irq_view().dom().finite()
    &&& forall|s: SlotId| #![trigger store.slot_view()[s]]
            store.slot_view().dom().contains(s) && !is_empty_cap(store.slot_view()[s].cap)
            ==> cap_consistent(store, store.slot_view()[s].cap)
}

/// CapRevocation `FireSafe` (rev2§5.1, the firing obligation), as a whole-store
/// corollary of `caps_consistent`: every *resident* TCB binding slot is empty ("NULL")
/// or names a *live* notification. So a thread-death firing (`thread::report_terminal`
/// → `notification::signal`) only ever signals a live object or skips a cleared slot —
/// it never touches freed memory. This is the TLA `FireSafe`
/// (`tla/cap_revocation/CapRevocation.tla:388`,
/// `\A t, k: bindings[t][k] = NULL \/ bindings[t][k] \in live`) mechanized over the live
/// first-order store: a bind slot holds only an empty or a `Notification` cap
/// (`thread::report_terminal`/`thread::bind`), so `cap_notif` returning `Some(nn)` is the
/// non-NULL case and the implication demands `nn` live. ONLY this local per-step half is
/// Verus-mechanized — the *global* cross-restart arm stays the TLA design oracle:
/// `DeadNowhere` over the whole `CapIds` space (`CapRevocation.tla:374`, which *implies*
/// `FireSafe`) and the preemptible revoke walk's `EventuallyRevoked` liveness.
pub open spec fn fire_safe<S: Store>(store: &S) -> bool {
    forall|t: ObjId, k: int|
        #![trigger store.tcb_view()[t].bind_slots[k]]
        store.tcb_view().dom().contains(t) && 0 <= k < store.tcb_view()[t].bind_slots.len()
            && store.slot_view().dom().contains(store.tcb_view()[t].bind_slots[k])
            ==> (cap_notif(store.slot_view()[store.tcb_view()[t].bind_slots[k]].cap)
                    matches Some(nn) ==> store.notif_view().dom().contains(nn))
}

/// `caps_consistent ⇒ fire_safe`: the rev2§5.1 LiveParent⇒FireSafe entailment, named
/// where it is cheaply discharged. A resident bind slot holding a `Notification(nn)` cap
/// is non-empty (`cap_notif` `Some` ⇒ not `CapKind::Empty`), so `caps_consistent`'s
/// per-slot clause yields `cap_consistent` of that cap, whose `Notification` arm is
/// `notif_wf(nn)`, whose first clause is `notif_view.dom().contains(nn)` — `nn` is live.
pub proof fn lemma_fire_safe_from_caps_consistent<S: Store>(store: &S)
    requires
        caps_consistent(store),
    ensures
        fire_safe(store),
{
    assert forall|t: ObjId, k: int|
        #![trigger store.tcb_view()[t].bind_slots[k]]
        store.tcb_view().dom().contains(t) && 0 <= k < store.tcb_view()[t].bind_slots.len()
            && store.slot_view().dom().contains(store.tcb_view()[t].bind_slots[k]) implies (cap_notif(
        store.slot_view()[store.tcb_view()[t].bind_slots[k]].cap,
    ) matches Some(nn) ==> store.notif_view().dom().contains(nn)) by {
        let s = store.tcb_view()[t].bind_slots[k];
        if let Some(nn) = cap_notif(store.slot_view()[s].cap) {
            // `cap_notif` is `Some` only on the `Notification` arm, never `Empty`, so the
            // resident slot's cap is non-empty and `caps_consistent` (keyed on
            // `store.slot_view()[s]`) gives `cap_consistent`, whose `Notification(nn)` arm
            // unfolds to `notif_wf(nn)` ⇒ `notif_view.dom().contains(nn)`.
            assert(store.slot_view()[s].cap.kind == CapKind::Notification(nn));
            assert(!is_empty_cap(store.slot_view()[s].cap));
            assert(cap_consistent(store, store.slot_view()[s].cap));
        }
    }
}

/// `fire_safe` is a frame property: it reads only slot caps, each TCB's `bind_slots`, and
/// the notification domain, so it carries across any edit that frames those — the shape a
/// thread-death fire (`thread::report_terminal`) has (`set_tcb_report` rewrites one
/// `report` field; `signal` frames `slot_view`, every TCB's `bind_slots`, and the
/// `notif_view` domain). The companion of `lemma_caps_consistent_frame`, but far lighter
/// (no per-cap `notif_wf` re-derivation), so a consuming caller pays almost nothing.
pub proof fn lemma_fire_safe_frame<S: Store>(s0: &S, s1: &S)
    requires
        fire_safe(s0),
        s1.slot_view() == s0.slot_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        s1.notif_view().dom() == s0.notif_view().dom(),
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].bind_slots == s0.tcb_view()[k].bind_slots,
    ensures
        fire_safe(s1),
{
    assert forall|t: ObjId, k: int|
        #![trigger s1.tcb_view()[t].bind_slots[k]]
        s1.tcb_view().dom().contains(t) && 0 <= k < s1.tcb_view()[t].bind_slots.len()
            && s1.slot_view().dom().contains(s1.tcb_view()[t].bind_slots[k]) implies (cap_notif(
        s1.slot_view()[s1.tcb_view()[t].bind_slots[k]].cap,
    ) matches Some(nn) ==> s1.notif_view().dom().contains(nn)) by {
        // `bind_slots` and `slot_view` are framed, so the same resident slot carries the
        // same cap in `s0`; `fire_safe(s0)` (instantiated at the framed
        // `s0.tcb_view()[t].bind_slots[k]`) then places the named notification in `s0`'s
        // notif domain, which equals `s1`'s.
        assert(s1.tcb_view()[t].bind_slots == s0.tcb_view()[t].bind_slots);
        let s = s0.tcb_view()[t].bind_slots[k];
        assert(s1.tcb_view()[t].bind_slots[k] == s);
    }
}

// `caps_consistent` is preserved by a **signal-shaped** edit: one that frames the slot/chan/
// timer/cspace views, changes the notif view only at the signalled `n` (keeping `notif_wf(n)`),
// and changes only TCBs that were waiting on `n`, leaving every TCB's `bind_slots` fixed. Each
// per-kind clause reads only framed data: Channel/CSpace/Timer off the framed views; the
// Notification + Channel-binding `notif_wf` carries from `s0` (the fired `n` by hypothesis,
// every other notification by `lemma_notif_wf_frame` — its waiters all name `m != n`, so they
// were untouched); Thread off the framed slot dom + fixed `bind_slots`. The frame `fire`'s
// `caps_consistent` preservation rests on.
pub proof fn lemma_caps_consistent_frame<S: Store>(s0: &S, s1: &S, n: ObjId)
    requires
        caps_consistent(s0),
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.timer_view() == s0.timer_view(),
        s1.timer_head_view() == s0.timer_head_view(),
        s1.cspace_view() == s0.cspace_view(),
        s1.irq_view() == s0.irq_view(),
        s1.notif_view() == s0.notif_view().insert(n, s1.notif_view()[n]),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        notif_wf(s1.notif_view(), s1.tcb_view(), n),
        // Frames an "**other waiter** (`wait_notif` some, ≠ `n`) ⇒ unchanged" claim. It excludes
        // the Runnable old ready-tail `p` (`wait_notif None`) that the faithful enqueue
        // re-threads, which a "non-`n`-waiter ⇒ unchanged" claim would wrongly cover. The body
        // needs only that *other notifications' waiters* are unchanged (to carry their `notif_wf`
        // via `lemma_notif_wf_frame`); a caller supplies it from `signal`'s contrapositive frame +
        // `ready_complete` (an `m`-waiter is `BlockedNotif`, non-Runnable, so not in the changed set).
        forall|k: ObjId| #[trigger] s0.tcb_view()[k].wait_notif is Some
            && s0.tcb_view()[k].wait_notif != Some(n) && s0.tcb_view().dom().contains(k)
            ==> s1.tcb_view()[k] == s0.tcb_view()[k],
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].bind_slots == s0.tcb_view()[k].bind_slots,
        // Every TCB's bound cspace is framed too: the strengthened
        // `cap_consistent(Thread)` clause carries `cspace_resident_wf` of the bound cspace, and
        // a signal-shaped edit moves a waiter's queue/wait/retval fields but never its cspace —
        // so which `cs` a Thread cap's TCB names is unchanged, and `cspace_resident_wf(cs)`
        // (refs-free, over the framed `cspace_view` + `slot_view` dom) carries from `s0`.
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].cspace == s0.tcb_view()[k].cspace,
        // A TCB the edit *changes* that is still blocked in `s1` is blocked on `n` — `signal`
        // wakes its head to `Runnable` (not blocked), `remove_waiter` either clears the spliced
        // thread's `wait_notif` (→ `None`) or only re-threads a predecessor still blocked on `n`.
        // This carries the waiter-coherence clause: a thread still
        // BlockedNotif-on-`wn` in `s1` was either *unchanged* (so BlockedNotif-on-`wn` in `s0`,
        // where `caps_consistent(s0)` gave `notif_wf(wn)`) or changed with `wn == n` (where
        // `notif_wf(s1, n)` is a hypothesis); either way `notif_wf(s1, wn)` holds.
        forall|k: ObjId| #[trigger] s1.tcb_view()[k] != s0.tcb_view()[k]
            && s1.tcb_view()[k].state == ThreadState::BlockedNotif
            ==> (s1.tcb_view()[k].wait_notif matches Some(wn) ==> wn == n),
    ensures
        caps_consistent(s1),
{
    // `notif_wf` carries from `s0` to `s1` for every notification (`n` by hypothesis, the rest
    // by the frame lemma).
    assert forall|m: ObjId| #[trigger] s0.notif_view().dom().contains(m)
        && notif_wf(s0.notif_view(), s0.tcb_view(), m) implies
        notif_wf(s1.notif_view(), s1.tcb_view(), m) by {
        if m != n {
            lemma_notif_wf_frame(s0.notif_view(), s0.tcb_view(), s1.notif_view(), s1.tcb_view(), m);
        }
    }
    assert forall|s: SlotId| #![trigger s1.slot_view()[s]]
        s1.slot_view().dom().contains(s) && !is_empty_cap(s1.slot_view()[s].cap)
        implies cap_consistent(s1, s1.slot_view()[s].cap) by {
        let c = s1.slot_view()[s].cap;
        assert(c == s0.slot_view()[s].cap);
        assert(cap_consistent(s0, c));
        match c.kind {
            CapKind::Notification(m) => {
                assert(s0.notif_view().dom().contains(m));
                assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
            }
            CapKind::Thread(m, _) => {
                // The strengthened Thread clause: re-derive `cspace_resident_wf(s1, cs)` from
                // `s0`. The bound cspace is framed (`s1.tcb[m].cspace == s0.tcb[m].cspace`), and
                // `cspace_resident_wf` reads only the framed `cspace_view` + `slot_view` dom.
                if let Some(cs) = s1.tcb_view()[m].cspace {
                    assert(s0.tcb_view()[m].cspace == Some(cs));
                    assert(cspace_resident_wf(s0, cs));
                    assert(cspace_resident_wf(s1, cs));
                }
                // Waiter-coherence: `m` BlockedNotif-on-`wn` in `s1` ⟹ `notif_wf(s1, wn)`.
                // Either `m` is unchanged (so BlockedNotif-on-`wn` in `s0`, giving
                // `notif_wf(s0, wn)`, carried to `s1` — at `n` by hypothesis, at `wn != n` by
                // `lemma_notif_wf_frame`), or `m` changed (then the off-chain hypothesis forces
                // `wn == n`, where `notif_wf(s1, n)` is a direct hypothesis).
                if s1.tcb_view()[m].state == ThreadState::BlockedNotif {
                    if let Some(wn) = s1.tcb_view()[m].wait_notif {
                        if s1.tcb_view()[m] == s0.tcb_view()[m] {
                            assert(notif_wf(s0.notif_view(), s0.tcb_view(), wn));
                            if wn != n {
                                lemma_notif_wf_frame(s0.notif_view(), s0.tcb_view(),
                                    s1.notif_view(), s1.tcb_view(), wn);
                            }
                        } else {
                            assert(wn == n);
                        }
                    }
                }
            }
            CapKind::Channel(co, _) => {
                assert forall|e: int, v: int|
                    (0 <= e < 2 && 0 <= v < 3
                        && #[trigger] s1.chan_view()[co].bindings[(e, v)].notif is Some) implies {
                        let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                        s1.notif_view().dom().contains(m) && notif_wf(s1.notif_view(), s1.tcb_view(), m)
                    } by {
                    let m = s1.chan_view()[co].bindings[(e, v)].notif->Some_0;
                    assert(s0.chan_view()[co].bindings[(e, v)].notif == Some(m));
                    assert(s0.notif_view().dom().contains(m));
                    assert(notif_wf(s0.notif_view(), s0.tcb_view(), m));
                }
            }
            _ => {}
        }
    }
}

// The rev2§3.3 per-endpoint cap census as a system invariant (the `caps_consistent`
// analog for channel endpoint counts; the body-removal census gate):
// every live channel's `end_caps[e]` equals the count of `Channel(ch, e)` caps in
// the arena. Refs-free (reads only `chan_view`/`slot_view`), so a `dec_ref` `-1`
// and any chan+slot-framing op preserve it for free — the same shallowness that
// makes `caps_consistent` cheap. This is the missing equality
// `cap_consistent`'s Channel arm (`end_caps[end] > 0`, a lower bound) never
// captured: with it, deleting one of several `(co, end)` caps provably leaves the
// siblings with `end_caps[end] >= 1`, so `delete`'s body re-proves `caps_consistent`.
pub open spec fn end_caps_sound<S: Store>(store: &S) -> bool {
    forall|ch: ObjId, e: int|
        store.chan_view().dom().contains(ch) && store.chan_view()[ch].end_caps.len() == 2
            && 0 <= e < 2
            ==> #[trigger] store.chan_view()[ch].end_caps[e] == end_cap_count(
            store.slot_view(),
            ch,
            e,
        )
}

// `end_caps_sound` except off by one at `(co, e0)`: `end_caps[co][e0] == end_cap_count + 1`.
// The state `delete` is in when it calls `endpoint_cap_dropped` — it cleared the deleted
// `Channel(co, end)` cap's slot (dropping `end_cap_count(co, e0)` by one) but has not yet run
// the matching `end_caps` decrement. `endpoint_cap_dropped` consumes it: the decrement lands
// `end_caps_sound`, and the off-by-one is exactly what guarantees no sibling `(co, e0)` cap is
// stranded (a live sibling makes `end_cap_count ≥ 1`, so `end_caps == end_cap_count + 1 ≥ 2`).
pub open spec fn end_caps_off_by_one<S: Store>(store: &S, co: ObjId, e0: int) -> bool {
    forall|ch: ObjId, e: int|
        store.chan_view().dom().contains(ch) && store.chan_view()[ch].end_caps.len() == 2
            && 0 <= e < 2
            ==> #[trigger] store.chan_view()[ch].end_caps[e] == end_cap_count(
            store.slot_view(),
            ch,
            e,
        ) + (if ch == co && e == e0 { 1nat } else { 0nat })
}

// ── Per-term recount lemmas. The single-key bump/drop building blocks
// 6b–6f compose: a one-key view edit raises/lowers exactly one census term by
// one, the others fixed. Each is the `lemma_designation_bump` shape over a
// different view (`slot_refs` already has its bump above); the drops are its
// `remove`-mirror, and the thread-hold pair frames the untouched half at `k`.
// The five single-domain terms (slot, frame-mapping, armed-timer, thread-hold
// ×2) are settled here. The sixth, `binding_refs`, counts over a *nested*
// domain (`(ch, end, ev)` triples), so its single-edit recount needs the triple
// set's finiteness (a subset of `cv.dom() × {0,1} × {0,1,2}`) — the n²
// trigger hazard. It lands with `destroy_channel`'s binding
// release, the op that consumes it (6d), per the "count steps single-purpose,
// where consumed" discipline — recorded, not dropped. ──

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

// Clearing a slot to an EMPTY cap, the per-object effect on the two slot-dependent census
// terms: `slot_refs`/`frame_map_refs` each drop by one at the cleared cap's designated object
// / mapped aspace and are unchanged at every other object (the EMPTY cap designates and maps
// nothing, so it never adds to any filter). `delete`'s body composes this with `set_slot`'s
// frames of the other four (non-slot) census terms to get the off-by-one (or no) census shift.
proof fn lemma_clear_slot_census(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, x: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        is_empty_cap(v.cap),
    ensures
        slot_refs(m.insert(k, v), x) == (slot_refs(m, x) - (if cap_obj(m[k].cap) == Some(x) {
            1nat
        } else {
            0nat
        })) as nat,
        frame_map_refs(m.insert(k, v), x) == (frame_map_refs(m, x) - (if cap_frame_aspace(
            m[k].cap,
        ) == Some(x) {
            1nat
        } else {
            0nat
        })) as nat,
{
    let m2 = m.insert(k, v);
    assert(m2.dom() =~= m.dom());
    let fs1 = m.dom().filter(|j: SlotId| cap_obj(m[j].cap) == Some(x));
    let fs2 = m2.dom().filter(|j: SlotId| cap_obj(m2[j].cap) == Some(x));
    assert(fs1.finite());
    if cap_obj(m[k].cap) == Some(x) {
        assert forall|j: SlotId| #![trigger fs2.contains(j)] fs2.contains(j) <==> fs1.remove(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fs2 =~= fs1.remove(k));
        assert(fs1.contains(k));
    } else {
        assert forall|j: SlotId| #![trigger fs2.contains(j)] fs2.contains(j) <==> fs1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fs2 =~= fs1);
    }
    let fm1 = m.dom().filter(|j: SlotId| cap_frame_aspace(m[j].cap) == Some(x));
    let fm2 = m2.dom().filter(|j: SlotId| cap_frame_aspace(m2[j].cap) == Some(x));
    assert(fm1.finite());
    if cap_frame_aspace(m[k].cap) == Some(x) {
        assert forall|j: SlotId| #![trigger fm2.contains(j)] fm2.contains(j) <==> fm1.remove(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fm2 =~= fm1.remove(k));
        assert(fm1.contains(k));
    } else {
        assert forall|j: SlotId| #![trigger fm2.contains(j)] fm2.contains(j) <==> fm1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fm2 =~= fm1);
    }
}

// Clearing a slot to EMPTY, the per-`(ch, e)` effect on the endpoint count: `end_cap_count`
// drops by one at the cleared cap's `(ch, e)` (if it is a `Channel(ch, e)` cap) and is fixed
// at every other `(ch2, e2)` — the EMPTY cap names no endpoint. `delete`'s Channel branch
// uses it to land `end_caps_off_by_one`; the non-Channel branches use the all-fixed case.
proof fn lemma_clear_slot_end_cap(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, ch: ObjId, e: int)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        is_empty_cap(v.cap),
    ensures
        end_cap_count(m.insert(k, v), ch, e) == (end_cap_count(m, ch, e) - (if cap_chan_end(
            m[k].cap,
        ) == Some((ch, e)) {
            1nat
        } else {
            0nat
        })) as nat,
{
    let m2 = m.insert(k, v);
    assert(m2.dom() =~= m.dom());
    let f1 = m.dom().filter(|j: SlotId| cap_chan_end(m[j].cap) == Some((ch, e)));
    let f2 = m2.dom().filter(|j: SlotId| cap_chan_end(m2[j].cap) == Some((ch, e)));
    assert(f1.finite());
    if cap_chan_end(m[k].cap) == Some((ch, e)) {
        assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(f2 =~= f1.remove(k));
        assert(f1.contains(k));
    } else {
        assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(f2 =~= f1);
    }
}

// `count_nonempty` drops by one when a non-empty slot is cleared to empty — the
// `lemma_designation_drop` shape over the `is_empty` filter. `delete`'s body consumes it for
// the strict `count_nonempty` decrease its `ensures` (and the SCC measure) state.
proof fn lemma_clear_drops_count(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        !is_empty_cap(m[k].cap),
        is_empty_cap(v.cap),
    ensures
        count_nonempty(m.insert(k, v)) == (count_nonempty(m) - 1) as nat,
{
    let m2 = m.insert(k, v);
    let f1 = m.dom().filter(|j: SlotId| !is_empty_cap(m[j].cap));
    let f2 = m2.dom().filter(|j: SlotId| !is_empty_cap(m2[j].cap));
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

// The full `obj_census` drop for a slot clear, isolated as a per-`x` query so `delete_prepare`'s
// forall instantiates it in a context-light SMT call. `s_new` is `s_old` with `slot`
// cleared after a link-only edit (`cdt_unlink`, preserving caps via `sv_mid`); the four non-slot
// terms are framed and a cap is either an object cap or a frame cap (never both), so the census
// drops by exactly one — at `cap_obj(cap)`, else at `cap_frame_aspace(cap)`.
pub proof fn lemma_clear_slot_obj_census<S: Store>(
    s_old: &S,
    s_new: &S,
    sv_mid: Map<SlotId, CapSlot>,
    slot: SlotId,
    es: CapSlot,
    cap: Cap,
    x: ObjId,
)
    requires
        s_new.slot_view() == sv_mid.insert(slot, es),
        s_old.slot_view().dom() == sv_mid.dom(),
        forall|k: SlotId| #[trigger] s_old.slot_view().dom().contains(k)
            ==> s_old.slot_view()[k].cap == sv_mid[k].cap,
        sv_mid.dom().contains(slot),
        sv_mid.dom().finite(),
        sv_mid[slot].cap == cap,
        is_empty_cap(es.cap),
        cap_obj(cap) is None || cap_frame_aspace(cap) is None,
        s_new.chan_view() == s_old.chan_view(),
        s_new.notif_view() == s_old.notif_view(),
        s_new.tcb_view() == s_old.tcb_view(),
        s_new.timer_view() == s_old.timer_view(),
        s_new.irq_view() == s_old.irq_view(),
    ensures
        // Additive form (no `nat` underflow): the deleted designating slot accounts for exactly
        // the one census unit lost — at `cap_obj(cap)`, else at `cap_frame_aspace(cap)`.
        obj_census(s_old, x) == obj_census(s_new, x) + (if cap_obj(cap) == Some(x)
            || cap_frame_aspace(cap) == Some(x) {
            1nat
        } else {
            0nat
        }),
{
    // slot_refs/frame_map_refs: `s_old` → `sv_mid` (caps equal), then `sv_mid` → the clear.
    lemma_same_caps_same_census(s_old.slot_view(), sv_mid, x);
    lemma_same_caps_same_frame_map(s_old.slot_view(), sv_mid, x);
    lemma_clear_slot_census(sv_mid, slot, es, x);
    // A cap is either an object cap or a frame cap, never both, so at most one delta fires.
    assert(!(cap_obj(cap) == Some(x) && cap_frame_aspace(cap) == Some(x)));
    // When the deleted cap designates `x`, `slot` itself is in the relevant census filter, so
    // the pre-clear count is ≥ 1 — the additive recombination has no `nat` underflow.
    if cap_obj(cap) == Some(x) {
        let f = sv_mid.dom().filter(|j: SlotId| cap_obj(sv_mid[j].cap) == Some(x));
        assert(f.contains(slot));
        assert(f.finite());
        if f.len() == 0 {
            assert(f =~= Set::empty());
        }
        assert(slot_refs(sv_mid, x) >= 1);
    }
    if cap_frame_aspace(cap) == Some(x) {
        let f = sv_mid.dom().filter(|j: SlotId| cap_frame_aspace(sv_mid[j].cap) == Some(x));
        assert(f.contains(slot));
        assert(f.finite());
        if f.len() == 0 {
            assert(f =~= Set::empty());
        }
        assert(frame_map_refs(sv_mid, x) >= 1);
    }
    // `sv_mid[slot].cap == cap` rewrites the lemma's `cap_obj(sv_mid[slot].cap)` deltas to
    // `cap_obj(cap)`/`cap_frame_aspace(cap)`; the four view terms read framed views (equal args).
}

// The dual of `lemma_clear_slot_census`: *installing* a cap into a previously EMPTY
// slot raises the two slot-dependent census terms by one at the new cap's designated object /
// mapped aspace (the EMPTY old cap added to no filter). `derive`/`retype_install` compose this
// with `set_slot`'s frames of the four non-slot census terms to get the +1 (or no) census shift.
proof fn lemma_set_slot_census(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, x: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        is_empty_cap(m[k].cap),
    ensures
        slot_refs(m.insert(k, v), x) == slot_refs(m, x) + (if cap_obj(v.cap) == Some(x) {
            1nat
        } else {
            0nat
        }),
        frame_map_refs(m.insert(k, v), x) == frame_map_refs(m, x) + (if cap_frame_aspace(v.cap)
            == Some(x) {
            1nat
        } else {
            0nat
        }),
{
    let m2 = m.insert(k, v);
    assert(m2.dom() =~= m.dom());
    // The EMPTY old cap designates and maps nothing, so `k` is in neither old filter set.
    assert(cap_obj(m[k].cap) is None);
    assert(cap_frame_aspace(m[k].cap) is None);
    let fs1 = m.dom().filter(|j: SlotId| cap_obj(m[j].cap) == Some(x));
    let fs2 = m2.dom().filter(|j: SlotId| cap_obj(m2[j].cap) == Some(x));
    assert(fs1.finite());
    assert(!fs1.contains(k));
    if cap_obj(v.cap) == Some(x) {
        assert forall|j: SlotId| #![trigger fs2.contains(j)] fs2.contains(j) <==> fs1.insert(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fs2 =~= fs1.insert(k));
    } else {
        assert forall|j: SlotId| #![trigger fs2.contains(j)] fs2.contains(j) <==> fs1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fs2 =~= fs1);
    }
    let fm1 = m.dom().filter(|j: SlotId| cap_frame_aspace(m[j].cap) == Some(x));
    let fm2 = m2.dom().filter(|j: SlotId| cap_frame_aspace(m2[j].cap) == Some(x));
    assert(fm1.finite());
    assert(!fm1.contains(k));
    if cap_frame_aspace(v.cap) == Some(x) {
        assert forall|j: SlotId| #![trigger fm2.contains(j)] fm2.contains(j) <==> fm1.insert(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fm2 =~= fm1.insert(k));
    } else {
        assert forall|j: SlotId| #![trigger fm2.contains(j)] fm2.contains(j) <==> fm1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fm2 =~= fm1);
    }
}

// The dual of `lemma_clear_slot_obj_census`: the full `obj_census` *rise* for
// installing a designating cap `v` into a previously EMPTY slot after a link-only edit
// (`sv_mid` is `s_old.slot_view` with `slot` set to `v`; `s_new.slot_view` is a cap-equal
// re-link of `sv_mid` — `cdt_insert_child`). A cap is either an object cap or a frame cap (never
// both), so the census rises by exactly one — at `cap_obj(v.cap)`, else at `cap_frame_aspace(v.cap)`.
pub proof fn lemma_set_slot_obj_census<S: Store>(
    s_old: &S,
    s_new: &S,
    sv_mid: Map<SlotId, CapSlot>,
    slot: SlotId,
    v: CapSlot,
    x: ObjId,
)
    requires
        sv_mid == s_old.slot_view().insert(slot, v),
        s_new.slot_view().dom() == sv_mid.dom(),
        forall|k: SlotId| #[trigger] s_new.slot_view().dom().contains(k)
            ==> s_new.slot_view()[k].cap == sv_mid[k].cap,
        s_old.slot_view().dom().contains(slot),
        s_old.slot_view().dom().finite(),
        is_empty_cap(s_old.slot_view()[slot].cap),
        cap_obj(v.cap) is None || cap_frame_aspace(v.cap) is None,
        s_new.chan_view() == s_old.chan_view(),
        s_new.notif_view() == s_old.notif_view(),
        s_new.tcb_view() == s_old.tcb_view(),
        s_new.timer_view() == s_old.timer_view(),
        s_new.irq_view() == s_old.irq_view(),
    ensures
        obj_census(s_new, x) == obj_census(s_old, x) + (if cap_obj(v.cap) == Some(x)
            || cap_frame_aspace(v.cap) == Some(x) {
            1nat
        } else {
            0nat
        }),
{
    // slot_refs/frame_map_refs: `s_old` → `sv_mid` (the install), then `sv_mid` → `s_new` (caps equal).
    lemma_set_slot_census(s_old.slot_view(), slot, v, x);
    assert forall|k: SlotId| #[trigger] sv_mid.dom().contains(k)
        implies sv_mid[k].cap == s_new.slot_view()[k].cap by {
        assert(s_new.slot_view().dom().contains(k));
    }
    lemma_same_caps_same_census(sv_mid, s_new.slot_view(), x);
    lemma_same_caps_same_frame_map(sv_mid, s_new.slot_view(), x);
    // A cap is either an object cap or a frame cap, never both, so at most one delta fires.
    assert(!(cap_obj(v.cap) == Some(x) && cap_frame_aspace(v.cap) == Some(x)));
    // The four view terms read framed views (equal args).
}

// ── The map-time mirror of the clear-slot census drop. Recording a frame mapping
// (`mapping: None → Some((asp, va))`) raises `frame_map_refs(asp)` — hence `obj_census(asp)`
// — by one, the inverse of `delete_prepare` clearing it. `lemma_set_slot_census` keyed its
// rise off `is_empty_cap(old)`; here the old cap is a (non-empty) unmapped Frame, so the
// precondition relaxes to "designates and maps nothing" (both hold for `Frame{mapping:None}`).

// The slot-term deltas for re-pointing a slot whose old cap designates/maps nothing to a new
// cap `v` (the `lemma_set_slot_census` shape, old-cap precondition relaxed from `is_empty_cap`).
proof fn lemma_map_frame_slot_census(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, x: ObjId)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_obj(m[k].cap) is None,
        cap_frame_aspace(m[k].cap) is None,
    ensures
        slot_refs(m.insert(k, v), x) == slot_refs(m, x) + (if cap_obj(v.cap) == Some(x) {
            1nat
        } else {
            0nat
        }),
        frame_map_refs(m.insert(k, v), x) == frame_map_refs(m, x) + (if cap_frame_aspace(v.cap)
            == Some(x) {
            1nat
        } else {
            0nat
        }),
{
    let m2 = m.insert(k, v);
    assert(m2.dom() =~= m.dom());
    let fs1 = m.dom().filter(|j: SlotId| cap_obj(m[j].cap) == Some(x));
    let fs2 = m2.dom().filter(|j: SlotId| cap_obj(m2[j].cap) == Some(x));
    assert(fs1.finite());
    assert(!fs1.contains(k));
    if cap_obj(v.cap) == Some(x) {
        assert forall|j: SlotId| #![trigger fs2.contains(j)] fs2.contains(j) <==> fs1.insert(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fs2 =~= fs1.insert(k));
    } else {
        assert forall|j: SlotId| #![trigger fs2.contains(j)] fs2.contains(j) <==> fs1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fs2 =~= fs1);
    }
    let fm1 = m.dom().filter(|j: SlotId| cap_frame_aspace(m[j].cap) == Some(x));
    let fm2 = m2.dom().filter(|j: SlotId| cap_frame_aspace(m2[j].cap) == Some(x));
    assert(fm1.finite());
    assert(!fm1.contains(k));
    if cap_frame_aspace(v.cap) == Some(x) {
        assert forall|j: SlotId| #![trigger fm2.contains(j)] fm2.contains(j) <==> fm1.insert(k).contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fm2 =~= fm1.insert(k));
    } else {
        assert forall|j: SlotId| #![trigger fm2.contains(j)] fm2.contains(j) <==> fm1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(fm2 =~= fm1);
    }
}

// The full `obj_census` rise for recording a frame mapping: `s_new.slot_view()` is
// `s_old.slot_view()` with `slot` set to a mapped Frame `v` (`cap_obj(v.cap) is None`,
// `cap_frame_aspace(v.cap)` the target aspace); the four non-slot terms read framed views.
// The census rises by one exactly at the mapped aspace — the `lemma_set_slot_obj_census`
// mirror with the relaxed old-cap precondition.
pub proof fn lemma_map_frame_census<S: Store>(
    s_old: &S,
    s_new: &S,
    slot: SlotId,
    v: CapSlot,
    x: ObjId,
)
    requires
        s_new.slot_view() == s_old.slot_view().insert(slot, v),
        s_old.slot_view().dom().contains(slot),
        s_old.slot_view().dom().finite(),
        cap_obj(s_old.slot_view()[slot].cap) is None,
        cap_frame_aspace(s_old.slot_view()[slot].cap) is None,
        cap_obj(v.cap) is None,
        s_new.chan_view() == s_old.chan_view(),
        s_new.notif_view() == s_old.notif_view(),
        s_new.tcb_view() == s_old.tcb_view(),
        s_new.timer_view() == s_old.timer_view(),
        s_new.irq_view() == s_old.irq_view(),
    ensures
        obj_census(s_new, x) == obj_census(s_old, x) + (if cap_frame_aspace(v.cap) == Some(x) {
            1nat
        } else {
            0nat
        }),
{
    lemma_map_frame_slot_census(s_old.slot_view(), slot, v, x);
    // slot_refs delta is 0 (`cap_obj(v.cap) is None`); frame_map_refs carries the `if`; the
    // four non-slot terms read framed views (equal args).
}

// Recording a frame mapping leaves every `end_cap_count` fixed — neither the old (unmapped
// Frame) nor the new (mapped Frame) cap is a channel-endpoint cap (`cap_chan_end is None`),
// so no `(ch, e)` filter moves. `end_caps_sound` rides through. The `lemma_clear_slot_end_cap`
// all-fixed case, keyed on emptiness-of-endpoint rather than emptiness-of-cap.
proof fn lemma_map_frame_end_cap(m: Map<SlotId, CapSlot>, k: SlotId, v: CapSlot, ch: ObjId, e: int)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_chan_end(m[k].cap) is None,
        cap_chan_end(v.cap) is None,
    ensures
        end_cap_count(m.insert(k, v), ch, e) == end_cap_count(m, ch, e),
{
    let m2 = m.insert(k, v);
    assert(m2.dom() =~= m.dom());
    let f1 = m.dom().filter(|j: SlotId| cap_chan_end(m[j].cap) == Some((ch, e)));
    let f2 = m2.dom().filter(|j: SlotId| cap_chan_end(m2[j].cap) == Some((ch, e)));
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1);
}

// Recording a frame mapping preserves `caps_consistent`. Every *other* live cap's consistency
// reads the slot arena only through its domain (an `insert` on an existing key fixes it) and
// the *emptiness* of specific slots (a Frame stays non-empty across `None → Some`, so emptiness
// is fixed everywhere) — plus object views this edit frames; the changed Frame cap is
// `cap_consistent` unconditionally (the `_ => true` arm). The mirror of the slot-clear's
// `caps_consistent` re-establishment, for the record (rather than clear) direction.
pub proof fn lemma_map_frame_caps_consistent<S: Store>(
    s_old: &S,
    s_new: &S,
    slot: SlotId,
    v: CapSlot,
)
    requires
        caps_consistent(s_old),
        s_new.slot_view() == s_old.slot_view().insert(slot, v),
        s_old.slot_view().dom().contains(slot),
        !is_empty_cap(s_old.slot_view()[slot].cap),
        v.cap.kind matches CapKind::Frame { .. },
        s_new.chan_view() == s_old.chan_view(),
        s_new.notif_view() == s_old.notif_view(),
        s_new.tcb_view() == s_old.tcb_view(),
        s_new.timer_view() == s_old.timer_view(),
        s_new.irq_view() == s_old.irq_view(),
        s_new.timer_head_view() == s_old.timer_head_view(),
        s_new.cspace_view() == s_old.cspace_view(),
    ensures
        caps_consistent(s_new),
{
    let sv0 = s_old.slot_view();
    let sv1 = s_new.slot_view();
    assert(sv1.dom() =~= sv0.dom());
    assert(!is_empty_cap(v.cap));
    // Emptiness is fixed at every slot: `slot` goes Frame→Frame (both non-empty); others equal.
    assert forall|s: SlotId| #[trigger] sv1.dom().contains(s)
        implies is_empty_cap(sv1[s].cap) == is_empty_cap(sv0[s].cap) by {
        if s != slot {
            assert(sv1[s] == sv0[s]);
        }
    }
    assert(sv1.dom().finite());
    assert forall|s: SlotId| #![trigger sv1[s]]
        sv1.dom().contains(s) && !is_empty_cap(sv1[s].cap)
        implies cap_consistent(s_new, sv1[s].cap) by {
        if s == slot {
            // The recorded Frame cap is consistent unconditionally (`_ => true`).
        } else {
            assert(sv1[s] == sv0[s]);
            assert(cap_consistent(s_old, sv0[s].cap));
            // A Channel cap's `chan_wf` reads slot emptiness; carry it via the emptiness frame.
            // Every other kind reads only framed object views + slot dom.
            if let CapKind::Channel(o, _) = sv0[s].cap.kind {
                lemma_chan_wf_emptiness_frame(s_old.chan_view(), s_new.chan_view(), sv0, sv1, o);
            }
        }
    }
}

// rev2§3.3 endpoint-cap census drop: clearing a `Channel(ch, e)` slot to a non-channel
// cap lowers `end_cap_count(ch, e)` by one and leaves every other `(ch2, e2)` fixed.
// The `lemma_designation_drop` shape over the `cap_chan_end` filter; `delete`'s body
// (PR2) consumes it when it empties a deleted channel cap's slot.
proof fn lemma_end_cap_count_drop(
    m: Map<SlotId, CapSlot>,
    k: SlotId,
    v: CapSlot,
    ch: ObjId,
    e: int,
)
    requires
        m.dom().finite(),
        m.dom().contains(k),
        cap_chan_end(m[k].cap) == Some((ch, e)),
        cap_chan_end(v.cap) is None,
    ensures
        end_cap_count(m.insert(k, v), ch, e) == (end_cap_count(m, ch, e) - 1) as nat,
        forall|ch2: ObjId, e2: int|
            (ch2 != ch || e2 != e) ==> #[trigger] end_cap_count(m.insert(k, v), ch2, e2)
                == end_cap_count(m, ch2, e2),
{
    let m2 = m.insert(k, v);
    assert(m2.dom() =~= m.dom());
    // The (ch, e) drop: the filter loses exactly k.
    let f1 = m.dom().filter(|j: SlotId| cap_chan_end(m[j].cap) == Some((ch, e)));
    let f2 = m2.dom().filter(|j: SlotId| cap_chan_end(m2[j].cap) == Some((ch, e)));
    assert forall|j: SlotId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(k).contains(j) by {
        if j != k {
            assert(m2[j] == m[j]);
        }
    }
    assert(f2 =~= f1.remove(k));
    assert(f1.contains(k));
    assert(f1.finite());
    // The others-fixed: for (ch2, e2) != (ch, e), k named (ch, e) in m and names
    // nothing in m2, so neither filter ever contained k — the set is unchanged.
    assert forall|ch2: ObjId, e2: int| (ch2 != ch || e2 != e) implies
        #[trigger] end_cap_count(m2, ch2, e2) == end_cap_count(m, ch2, e2) by {
        let g1 = m.dom().filter(|j: SlotId| cap_chan_end(m[j].cap) == Some((ch2, e2)));
        let g2 = m2.dom().filter(|j: SlotId| cap_chan_end(m2[j].cap) == Some((ch2, e2)));
        assert forall|j: SlotId| #![trigger g2.contains(j)] g2.contains(j) <==> g1.contains(j) by {
            if j != k {
                assert(m2[j] == m[j]);
            }
        }
        assert(g2 =~= g1);
    }
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

// Armed-timer drop, **disarm-shaped**. `disarm` (`timer.rs`) edits *two*
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

// Armed-timer **retarget**, the general single-timer transition (D-E1, `arm`). Like
// `lemma_armed_timer_disarm` but with no restriction on `post[t]`: `t`'s `(armed, notif)`
// may change arbitrarily (disarm, arm, or re-arm) while every other timer keeps both
// fields. `arm` produces exactly this shape end-to-end — its body `disarm`s then pushes,
// but only `t`'s `(armed, notif)` differs between the entry and exit maps (the predecessor
// splice touches only `next`, which `armed_timer_refs` ignores). The delta at `o` is the
// symmetric indicator form: `post`-count + `[t was bound to o]` == `pre`-count + `[t is now
// bound to o]`, so a consumer reads `o`'s census change off `t`'s membership transition.
pub proof fn lemma_armed_timer_retarget(
    pre: Map<ObjId, TimerView>,
    post: Map<ObjId, TimerView>,
    t: ObjId,
    o: ObjId,
)
    requires
        pre.dom().finite(),
        post.dom() == pre.dom(),
        pre.dom().contains(t),
        forall|j: ObjId| #![trigger post[j]]
            j != t ==> post[j].armed == pre[j].armed && post[j].notif == pre[j].notif,
    ensures
        armed_timer_refs(post, o)
            + (if pre[t].armed && pre[t].notif == Some(o) { 1nat } else { 0nat })
            == armed_timer_refs(pre, o)
            + (if post[t].armed && post[t].notif == Some(o) { 1nat } else { 0nat }),
{
    let f1 = pre.dom().filter(|j: ObjId| pre[j].armed && pre[j].notif == Some(o));
    let f2 = post.dom().filter(|j: ObjId| post[j].armed && post[j].notif == Some(o));
    let pre_in = pre[t].armed && pre[t].notif == Some(o);
    let post_in = post[t].armed && post[t].notif == Some(o);
    // Off `t`, the two filters agree (both fields framed); they differ only on `t`.
    if pre_in && post_in {
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
            if j != t { assert(post[j].armed == pre[j].armed && post[j].notif == pre[j].notif); }
        }
        assert(f2 =~= f1);
    } else if pre_in && !post_in {
        assert(!f2.contains(t));
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(t).contains(j) by {
            if j != t { assert(post[j].armed == pre[j].armed && post[j].notif == pre[j].notif); }
        }
        assert(f2 =~= f1.remove(t));
        assert(f1.contains(t));
        assert(f1.finite());
    } else if !pre_in && post_in {
        assert(!f1.contains(t));
        assert forall|j: ObjId| #![trigger f1.contains(j)] f1.contains(j) <==> f2.remove(t).contains(j) by {
            if j != t { assert(post[j].armed == pre[j].armed && post[j].notif == pre[j].notif); }
        }
        assert(f1 =~= f2.remove(t));
        assert(f2.contains(t));
        assert(f2.finite());
    } else {
        assert(!f1.contains(t));
        assert(!f2.contains(t));
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
            if j != t { assert(post[j].armed == pre[j].armed && post[j].notif == pre[j].notif); }
        }
        assert(f2 =~= f1);
    }
}

// IRQ-binding **retarget**, the general single-IRQ transition (the
// `lemma_armed_timer_retarget` twin). `i`'s `(bound, notif)` may change arbitrarily (bind,
// unbind, or rebind) while every other IRQ keeps both fields. The IRQ ops are single-key
// edits at `i` (no list splice), so this one lemma covers `irq_bind` (post bound), `irq_unbind`
// (post unbound), and `destroy_irq` (via unbind) uniformly: the delta at `o` is the symmetric
// indicator form, so a consumer reads `o`'s census change off `i`'s membership transition.
pub proof fn lemma_irq_binding_retarget(
    pre: Map<ObjId, IrqView>,
    post: Map<ObjId, IrqView>,
    i: ObjId,
    o: ObjId,
)
    requires
        pre.dom().finite(),
        post.dom() == pre.dom(),
        pre.dom().contains(i),
        forall|j: ObjId| #![trigger post[j]]
            j != i ==> post[j].bound == pre[j].bound && post[j].notif == pre[j].notif,
    ensures
        irq_binding_refs(post, o)
            + (if pre[i].bound && pre[i].notif == Some(o) { 1nat } else { 0nat })
            == irq_binding_refs(pre, o)
            + (if post[i].bound && post[i].notif == Some(o) { 1nat } else { 0nat }),
{
    let f1 = pre.dom().filter(|j: ObjId| pre[j].bound && pre[j].notif == Some(o));
    let f2 = post.dom().filter(|j: ObjId| post[j].bound && post[j].notif == Some(o));
    let pre_in = pre[i].bound && pre[i].notif == Some(o);
    let post_in = post[i].bound && post[i].notif == Some(o);
    if pre_in && post_in {
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
            if j != i { assert(post[j].bound == pre[j].bound && post[j].notif == pre[j].notif); }
        }
        assert(f2 =~= f1);
    } else if pre_in && !post_in {
        assert(!f2.contains(i));
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.remove(i).contains(j) by {
            if j != i { assert(post[j].bound == pre[j].bound && post[j].notif == pre[j].notif); }
        }
        assert(f2 =~= f1.remove(i));
        assert(f1.contains(i));
        assert(f1.finite());
    } else if !pre_in && post_in {
        assert(!f1.contains(i));
        assert forall|j: ObjId| #![trigger f1.contains(j)] f1.contains(j) <==> f2.remove(i).contains(j) by {
            if j != i { assert(post[j].bound == pre[j].bound && post[j].notif == pre[j].notif); }
        }
        assert(f1 =~= f2.remove(i));
        assert(f2.contains(i));
        assert(f2.finite());
    } else {
        assert(!f1.contains(i));
        assert(!f2.contains(i));
        assert forall|j: ObjId| #![trigger f2.contains(j)] f2.contains(j) <==> f1.contains(j) by {
            if j != i { assert(post[j].bound == pre[j].bound && post[j].notif == pre[j].notif); }
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
pub(crate) proof fn lemma_thread_hold_cspace_drop(m: Map<ObjId, TcbView>, k: ObjId, v: TcbView, o: ObjId)
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

pub(crate) proof fn lemma_thread_hold_aspace_drop(m: Map<ObjId, TcbView>, k: ObjId, v: TcbView, o: ObjId)
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

// ── `destroy_tcb`'s clear-before-unref census lemmas. ──
//
// `destroy_tcb` releases its halted subject's cspace/aspace holds by **clearing the field
// first, then `unref_cspace`/`unref_aspace`** — so at the unref call the census has already
// dropped by one at the held object while `refs` has not, i.e. `census_off_by_one(·, held)`,
// the exact window `unref_cspace`/`unref_aspace` consume (`cspace.rs` requires
// `refs[x] == census(x) + 1` + soundness elsewhere). These two lemmas establish that window
// from the single-field clear: `thread_hold_refs(held)` drops by one
// (`lemma_thread_hold_{cspace,aspace}_drop`); the five other census terms are framed (a tcb
// edit touches no slot/chan/timer view, and the waiter term rides
// `lemma_waiter_refs_frame_dequeued` because the halted subject `t` is off every chain — the
// edited field, `cspace`/`aspace`, is one `waiter_chain` never reads); `refcount_sound(s0)` +
// the unchanged `refs` then give the off-by-one at the held object and full soundness elsewhere.

pub proof fn lemma_census_after_hold_clear<S: Store>(s0: &S, s1: &S, t: ObjId, cs: ObjId)
    requires
        refcount_sound(s0),
        census_dom_complete(s0),
        s0.tcb_view().dom().finite(),
        s0.tcb_view().dom().contains(t),
        s0.tcb_view()[t].cspace == Some(cs),
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.notif_view() == s0.notif_view(),
        s1.timer_view() == s0.timer_view(),
        s1.irq_view() == s0.irq_view(),
        s1.refs_view() == s0.refs_view(),
        s1.tcb_view() == s0.tcb_view().insert(
            t, TcbView { cspace: None, ..s0.tcb_view()[t] }),
        // `t` is off every waiter chain (supplied by `lemma_thread_off_all_chains` once `t` is
        // halted with `wait_notif` cleared) — so the cspace clear perturbs no `waiter_refs`.
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s0.notif_view(), s0.tcb_view(), o, ws) ==> !ws.contains(t),
    ensures
        census_off_by_one(s1, cs),
        // Refs-domain coverage rides the clear (a census term only *dropped*, `refs` fixed) — the
        // precondition `unref_cspace` consumes alongside the off-by-one window.
        census_dom_complete(s1),
{
    let nv = s0.notif_view();
    let tv0 = s0.tcb_view();
    let tvf = s1.tcb_view();
    let v = TcbView { cspace: None, ..tv0[t] };
    assert(tvf == tv0.insert(t, v));
    assert(tvf.dom() =~= tv0.dom());
    // Queue/wait/state/aspace fields agree everywhere (only `cspace` moved, at `t`), so every
    // `waiter_chain` is preserved and the aspace half of `thread_hold_refs` is framed.
    assert forall|k: ObjId| #[trigger] tvf[k].qnext == tv0[k].qnext by {}
    assert forall|k: ObjId| #[trigger] tvf[k].wait_notif == tv0[k].wait_notif by {}
    assert forall|k: ObjId| #[trigger] tvf[k].state == tv0[k].state by {}
    assert forall|k: ObjId| #[trigger] tvf[k].aspace == tv0[k].aspace by {}

    lemma_thread_hold_cspace_drop(tv0, t, v, cs);
    // `cs` is referenced (the hold), so its census is positive — needed *before* the per-object
    // step so the `o == cs` drop is exact (not nat-saturated).
    assert(thread_hold_refs(tv0, cs) >= 1) by {
        let c1 = tv0.dom().filter(|j: ObjId| tv0[j].cspace == Some(cs));
        assert(c1.contains(t));
        assert(c1.finite());
        if c1.len() == 0 {
            assert(c1 =~= Set::<ObjId>::empty());
        }
    }
    assert(obj_census(s0, cs) >= 1);

    assert forall|o: ObjId| #[trigger] obj_census(s1, o)
        == (if o == cs { (obj_census(s0, o) - 1) as nat } else { obj_census(s0, o) }) by {
        // waiter term framed: `t` off every chain in both states (cspace ignored by the chain).
        assert forall|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws) implies !ws.contains(t) by {
            assert(waiter_chain(nv, tv0, o, ws));
        }
        lemma_waiter_refs_frame_dequeued(nv, tv0, tvf, t, o);
        // thread-hold term: drops at `cs`, framed elsewhere (`t`'s old/new cspace miss `o != cs`).
        if o != cs {
            let c1 = tv0.dom().filter(|j: ObjId| tv0[j].cspace == Some(o));
            let c2 = tvf.dom().filter(|j: ObjId| tvf[j].cspace == Some(o));
            assert(c2 =~= c1) by {
                assert forall|j: ObjId| #![trigger c2.contains(j)] c2.contains(j) <==> c1.contains(j) by {
                    if j != t { assert(tvf[j] == tv0[j]); }
                }
            }
            let a1 = tv0.dom().filter(|j: ObjId| tv0[j].aspace == Some(o));
            let a2 = tvf.dom().filter(|j: ObjId| tvf[j].aspace == Some(o));
            assert(a2 =~= a1) by {
                assert forall|j: ObjId| #![trigger a2.contains(j)] a2.contains(j) <==> a1.contains(j) by {
                    if j != t { assert(tvf[j] == tv0[j]); }
                }
            }
            assert(thread_hold_refs(tvf, o) == thread_hold_refs(tv0, o));
        }
    }

    lemma_in_refs_from_census(s0, cs);
    assert(s1.refs_view()[cs] == obj_census(s1, cs) + 1);
    assert forall|x: ObjId| x != cs && s1.refs_view().dom().contains(x)
        implies s1.refs_view()[x] == obj_census(s1, x) by {
        assert(s0.refs_view()[x] == obj_census(s0, x));
    }
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) >= 1
        implies s1.refs_view().dom().contains(o) by {
        if o != cs {
            assert(obj_census(s1, o) == obj_census(s0, o));
            lemma_in_refs_from_census(s0, o);
        }
    }
}

pub proof fn lemma_census_after_hold_clear_aspace<S: Store>(s0: &S, s1: &S, t: ObjId, a: ObjId)
    requires
        refcount_sound(s0),
        census_dom_complete(s0),
        s0.tcb_view().dom().finite(),
        s0.tcb_view().dom().contains(t),
        s0.tcb_view()[t].aspace == Some(a),
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.notif_view() == s0.notif_view(),
        s1.timer_view() == s0.timer_view(),
        s1.irq_view() == s0.irq_view(),
        s1.refs_view() == s0.refs_view(),
        s1.tcb_view() == s0.tcb_view().insert(
            t, TcbView { aspace: None, ..s0.tcb_view()[t] }),
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s0.notif_view(), s0.tcb_view(), o, ws) ==> !ws.contains(t),
    ensures
        census_off_by_one(s1, a),
        census_dom_complete(s1),
{
    let nv = s0.notif_view();
    let tv0 = s0.tcb_view();
    let tvf = s1.tcb_view();
    let v = TcbView { aspace: None, ..tv0[t] };
    assert(tvf == tv0.insert(t, v));
    assert(tvf.dom() =~= tv0.dom());
    assert forall|k: ObjId| #[trigger] tvf[k].qnext == tv0[k].qnext by {}
    assert forall|k: ObjId| #[trigger] tvf[k].wait_notif == tv0[k].wait_notif by {}
    assert forall|k: ObjId| #[trigger] tvf[k].state == tv0[k].state by {}
    assert forall|k: ObjId| #[trigger] tvf[k].cspace == tv0[k].cspace by {}

    lemma_thread_hold_aspace_drop(tv0, t, v, a);
    assert(thread_hold_refs(tv0, a) >= 1) by {
        let a1 = tv0.dom().filter(|j: ObjId| tv0[j].aspace == Some(a));
        assert(a1.contains(t));
        assert(a1.finite());
        if a1.len() == 0 {
            assert(a1 =~= Set::<ObjId>::empty());
        }
    }
    assert(obj_census(s0, a) >= 1);

    assert forall|o: ObjId| #[trigger] obj_census(s1, o)
        == (if o == a { (obj_census(s0, o) - 1) as nat } else { obj_census(s0, o) }) by {
        assert forall|ws: Seq<ObjId>| waiter_chain(nv, tvf, o, ws) implies !ws.contains(t) by {
            assert(waiter_chain(nv, tv0, o, ws));
        }
        lemma_waiter_refs_frame_dequeued(nv, tv0, tvf, t, o);
        if o != a {
            let a1 = tv0.dom().filter(|j: ObjId| tv0[j].aspace == Some(o));
            let a2 = tvf.dom().filter(|j: ObjId| tvf[j].aspace == Some(o));
            assert(a2 =~= a1) by {
                assert forall|j: ObjId| #![trigger a2.contains(j)] a2.contains(j) <==> a1.contains(j) by {
                    if j != t { assert(tvf[j] == tv0[j]); }
                }
            }
            let c1 = tv0.dom().filter(|j: ObjId| tv0[j].cspace == Some(o));
            let c2 = tvf.dom().filter(|j: ObjId| tvf[j].cspace == Some(o));
            assert(c2 =~= c1) by {
                assert forall|j: ObjId| #![trigger c2.contains(j)] c2.contains(j) <==> c1.contains(j) by {
                    if j != t { assert(tvf[j] == tv0[j]); }
                }
            }
            assert(thread_hold_refs(tvf, o) == thread_hold_refs(tv0, o));
        }
    }

    lemma_in_refs_from_census(s0, a);
    assert(s1.refs_view()[a] == obj_census(s1, a) + 1);
    assert forall|x: ObjId| x != a && s1.refs_view().dom().contains(x)
        implies s1.refs_view()[x] == obj_census(s1, x) by {
        assert(s0.refs_view()[x] == obj_census(s0, x));
    }
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) >= 1
        implies s1.refs_view().dom().contains(o) by {
        if o != a {
            assert(obj_census(s1, o) == obj_census(s0, o));
            lemma_in_refs_from_census(s0, o);
        }
    }
}

// `destroy_tcb`'s halt edit (clear `qnext`/`wait_notif`, set `state = Halted`; holds unchanged)
// of an off-chain thread `t` leaves every object's census fixed:
// the four non-tcb terms are framed (the halt setters touch only `tcb_view`), `thread_hold_refs`
// is framed (`t`'s cspace/aspace unchanged), and `waiter_refs` is framed because `t` is off every
// chain in both states (`lemma_waiter_refs_frame_offchain` — only `t` moved). So `refcount_sound`
// rides the halt unchanged, the precondition the bind-slot `delete`s need.
pub proof fn lemma_census_frame_thread_halt<S: Store>(s0: &S, s1: &S, t: ObjId)
    requires
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.notif_view() == s0.notif_view(),
        s1.timer_view() == s0.timer_view(),
        s1.irq_view() == s0.irq_view(),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|k: ObjId| k != t ==> #[trigger] s1.tcb_view()[k] == s0.tcb_view()[k],
        s1.tcb_view()[t].cspace == s0.tcb_view()[t].cspace,
        s1.tcb_view()[t].aspace == s0.tcb_view()[t].aspace,
        // `t` is off every chain in BOTH states — the full `dequeued` form (not the simple
        // `wait_notif is None || not-BlockedNotif` disjunct) so it also covers a thread that is
        // BlockedNotif-on-`wn` yet absent from `wn`'s queue (the `remove_waiter` absent path).
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s0.notif_view(), s0.tcb_view(), o, ws) ==> !ws.contains(t),
        forall|o: ObjId, ws: Seq<ObjId>|
            waiter_chain(s1.notif_view(), s1.tcb_view(), o, ws) ==> !ws.contains(t),
    ensures
        forall|o: ObjId| #[trigger] obj_census(s1, o) == obj_census(s0, o),
{
    let tv0 = s0.tcb_view();
    let tvf = s1.tcb_view();
    assert forall|k: ObjId| #[trigger] tvf[k].cspace == tv0[k].cspace by {
        if k != t {}
    }
    assert forall|k: ObjId| #[trigger] tvf[k].aspace == tv0[k].aspace by {
        if k != t {}
    }
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) == obj_census(s0, o) by {
        lemma_waiter_refs_frame_dequeued(s0.notif_view(), tv0, tvf, t, o);
        lemma_thread_hold_frame(tv0, tvf, o);
    }
}

// A wake/splice that dequeues exactly one waiter from notification `n`: only `n`'s notif view
// moved, and every changed TCB names `n` (or is detached), so `waiter_refs(n)` drops by one while
// every other census term is framed. The per-object census map below is the heavy
// `assert forall|o| obj_census(...)` that put `remove_waiter` among the gate's largest
// obligations; lifting it into its own `proof fn` gives it a small solver context (`verus.md`
// §10), keyed on `obj_census(s1, o)` so it stays out of census-agnostic callers. `remove_waiter`'s
// present path proves only the cheap local facts (the `-1` waiter delta + the changed-TCB shape)
// then reads `census_delta_frozen`, `refcount_sound`, and `census_dom_complete`-preservation off
// the map plus its own `refs[n] -= 1`. `notification::signal`'s wake path has the same edit shape
// but proves the map inline — its `make_runnable` enqueue leaves a context too large for the
// lemma's `requires` to discharge cheaply there. The
// `+1` enqueue twin is `lemma_waiter_enqueue_census`; the no-delta twin is
// `lemma_census_frame_thread_halt`; the cspace-clear analog is `lemma_census_after_hold_clear`.
pub proof fn lemma_waiter_dequeue_census<S: Store>(s0: &S, s1: &S, n: ObjId)
    requires
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.timer_view() == s0.timer_view(),
        s1.irq_view() == s0.irq_view(),
        // only `n`'s notif view moved — a single `insert` equality (§10), not a broad frame.
        s1.notif_view() == s0.notif_view().insert(n, s1.notif_view()[n]),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        // cspace/aspace fixed everywhere ⇒ `thread_hold_refs` is framed.
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].cspace == s0.tcb_view()[k].cspace,
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].aspace == s0.tcb_view()[k].aspace,
        // every changed TCB's `wait_notif` is "about `n`" (`Some(n)` or `None`) in BOTH states —
        // so for any `o != n` no changed node names `o`, the antecedent `lemma_waiter_refs_frame`
        // needs (the GLB across `signal`, whose changed Runnable ready-tail has `wait_notif None`,
        // and `remove_waiter`, whose changed nodes all name `n`).
        forall|k: ObjId| #[trigger] s1.tcb_view()[k] != s0.tcb_view()[k]
            ==> (s0.tcb_view()[k].wait_notif is None || s0.tcb_view()[k].wait_notif == Some(n))
                && (s1.tcb_view()[k].wait_notif is None || s1.tcb_view()[k].wait_notif == Some(n)),
        // exactly one waiter left `n`'s chain.
        waiter_refs(s1.notif_view(), s1.tcb_view(), n) + 1
            == waiter_refs(s0.notif_view(), s0.tcb_view(), n),
    ensures
        forall|o: ObjId| #[trigger] obj_census(s1, o)
            == (if o == n { (obj_census(s0, o) - 1) as nat } else { obj_census(s0, o) }),
{
    let nv0 = s0.notif_view();
    let tv0 = s0.tcb_view();
    let nvf = s1.notif_view();
    let tvf = s1.tcb_view();
    assert forall|o: ObjId| #[trigger] obj_census(s1, o)
        == (if o == n { (obj_census(s0, o) - 1) as nat } else { obj_census(s0, o) }) by {
        // thread-hold term framed everywhere (cspace/aspace fixed); the four non-tcb terms ride
        // the view equalities; `waiter_refs` rides `lemma_waiter_refs_frame` off `n` and the
        // `-1` delta at `n`.
        lemma_thread_hold_frame(tv0, tvf, o);
        if o != n {
            assert(nvf[o] == nv0[o]);
            assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k]
                implies tv0[k].wait_notif != Some(o) && tvf[k].wait_notif != Some(o) by {}
            lemma_waiter_refs_frame(nv0, tv0, nvf, tvf, n, o);
        }
    }
}

// The `+1` twin of `lemma_waiter_dequeue_census`: a block that enqueues exactly one waiter onto
// notification `n` (`notification::wait`'s block path), where `waiter_refs(n)` grows by one and
// every other census term is framed identically. The off-`n` frame is the same; only the delta at
// `n` flips. `wait` proves the cheap local facts then reads `census_delta_frozen` (and conditional
// `refcount_sound`) off this map plus its own `refs[n] += 1`.
pub proof fn lemma_waiter_enqueue_census<S: Store>(s0: &S, s1: &S, n: ObjId)
    requires
        s1.slot_view() == s0.slot_view(),
        s1.chan_view() == s0.chan_view(),
        s1.timer_view() == s0.timer_view(),
        s1.irq_view() == s0.irq_view(),
        s1.notif_view() == s0.notif_view().insert(n, s1.notif_view()[n]),
        s1.tcb_view().dom() == s0.tcb_view().dom(),
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].cspace == s0.tcb_view()[k].cspace,
        forall|k: ObjId| #[trigger] s1.tcb_view()[k].aspace == s0.tcb_view()[k].aspace,
        forall|k: ObjId| #[trigger] s1.tcb_view()[k] != s0.tcb_view()[k]
            ==> (s0.tcb_view()[k].wait_notif is None || s0.tcb_view()[k].wait_notif == Some(n))
                && (s1.tcb_view()[k].wait_notif is None || s1.tcb_view()[k].wait_notif == Some(n)),
        // exactly one waiter joined `n`'s chain.
        waiter_refs(s1.notif_view(), s1.tcb_view(), n)
            == waiter_refs(s0.notif_view(), s0.tcb_view(), n) + 1,
    ensures
        forall|o: ObjId| #[trigger] obj_census(s1, o)
            == (if o == n { (obj_census(s0, o) + 1) as nat } else { obj_census(s0, o) }),
{
    let nv0 = s0.notif_view();
    let tv0 = s0.tcb_view();
    let nvf = s1.notif_view();
    let tvf = s1.tcb_view();
    assert forall|o: ObjId| #[trigger] obj_census(s1, o)
        == (if o == n { (obj_census(s0, o) + 1) as nat } else { obj_census(s0, o) }) by {
        lemma_thread_hold_frame(tv0, tvf, o);
        if o != n {
            assert(nvf[o] == nv0[o]);
            assert forall|k: ObjId| #[trigger] tvf[k] != tv0[k]
                implies tv0[k].wait_notif != Some(o) && tvf[k].wait_notif != Some(o) by {}
            lemma_waiter_refs_frame(nv0, tv0, nvf, tvf, n, o);
        }
    }
}

// ── `delete`'s frame-unmap-branch census lemma. ──
//
// `delete` clears a deleted cap's slot (`cspace.rs`'s `s.cap = EMPTY; set_slot`)
// then, for a mapped Frame, calls `aspace_unmap` + `unref_aspace`. The census side
// of that branch, consumed by `delete`'s body: clearing a mapped Frame slot lowers
// exactly the target aspace's `frame_map_refs` by one and leaves *every* object's `slot_refs`
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

// ── `destroy_channel`'s binding-release census lemma. ──
//
// The sixth census term, `binding_refs`, was quarantined by 6a (the comment above
// `lemma_designation_drop`): unlike the five single-domain terms it counts over the
// *nested* `(ch, end, ev)` triple domain via `Set::new(..)`, so a single-edit recount
// needs the triple set's **finiteness** established by hand (the five `filter`-of-a-
// finite-map terms get it for free from `group_set_axioms`). `destroy_channel`'s
// binding-release loop is the op that consumes it (6d), so it lands here.

// The universe of in-bounds binding triples over a finite channel domain is finite:
// it is `⋃` of the six maps `d ↦ (c, e, ev)` (one per `(e, ev) ∈ {0,1}×{0,1,2}`), each
// finite because `d` is. `binding_refs`'s set is a subset of this, hence finite.
proof fn lemma_binding_triples_finite(d: Set<ObjId>)
    requires
        d.finite(),
    ensures
        Set::new(|t: (ObjId, int, int)| d.contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3).finite(),
{
    let univ = Set::new(|t: (ObjId, int, int)| d.contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3);
    let m00 = d.map(|c: ObjId| (c, 0int, 0int));
    let m01 = d.map(|c: ObjId| (c, 0int, 1int));
    let m02 = d.map(|c: ObjId| (c, 0int, 2int));
    let m10 = d.map(|c: ObjId| (c, 1int, 0int));
    let m11 = d.map(|c: ObjId| (c, 1int, 1int));
    let m12 = d.map(|c: ObjId| (c, 1int, 2int));
    d.lemma_map_finite(|c: ObjId| (c, 0int, 0int));
    d.lemma_map_finite(|c: ObjId| (c, 0int, 1int));
    d.lemma_map_finite(|c: ObjId| (c, 0int, 2int));
    d.lemma_map_finite(|c: ObjId| (c, 1int, 0int));
    d.lemma_map_finite(|c: ObjId| (c, 1int, 1int));
    d.lemma_map_finite(|c: ObjId| (c, 1int, 2int));
    let u1 = m00.union(m01);
    let u2 = u1.union(m02);
    let u3 = u2.union(m10);
    let u4 = u3.union(m11);
    let big = u4.union(m12);
    vstd::set_lib::lemma_set_union_finite_iff(m00, m01);
    vstd::set_lib::lemma_set_union_finite_iff(u1, m02);
    vstd::set_lib::lemma_set_union_finite_iff(u2, m10);
    vstd::set_lib::lemma_set_union_finite_iff(u3, m11);
    vstd::set_lib::lemma_set_union_finite_iff(u4, m12);
    assert forall|t: (ObjId, int, int)| univ.contains(t) implies big.contains(t) by {
        assert(d.contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3);
        if t.1 == 0 {
            if t.2 == 0 {
                assert(m00.contains((t.0, 0int, 0int)));
            } else if t.2 == 1 {
                assert(m01.contains((t.0, 0int, 1int)));
            } else {
                assert(m02.contains((t.0, 0int, 2int)));
            }
        } else {
            if t.2 == 0 {
                assert(m10.contains((t.0, 1int, 0int)));
            } else if t.2 == 1 {
                assert(m11.contains((t.0, 1int, 1int)));
            } else {
                assert(m12.contains((t.0, 1int, 2int)));
            }
        }
    }
    assert(univ.subset_of(big));
    vstd::set_lib::lemma_set_subset_finite(big, univ);
}

// Clearing one channel binding (`ch`'s `(e, ev)` binding, which named `o`) to a binding
// that does not name `o` lowers `binding_refs(o)` by exactly one and leaves every other
// object's binding census fixed. The new chan_view is `set_chan_binding`'s exact shape, so
// `destroy_channel` consumes this directly (the drop = `dec_ref`'s off-by-one at `o`, the
// others-fixed = `dec_ref`'s "sound elsewhere"). The removed triple is the only difference.
pub(crate) proof fn lemma_binding_drop(
    cv: Map<ObjId, ChanView>,
    ch: ObjId,
    e: int,
    ev: int,
    b: Binding,
    o: ObjId,
)
    requires
        cv.dom().finite(),
        cv.dom().contains(ch),
        0 <= e < 2,
        0 <= ev < 3,
        cv[ch].bindings[(e, ev)].notif == Some(o),
        b.notif is None,
    ensures
        binding_refs(
            cv.insert(ch, ChanView { bindings: cv[ch].bindings.insert((e, ev), b), ..cv[ch] }),
            o,
        ) == (binding_refs(cv, o) - 1) as nat,
        forall|x: ObjId| x != o ==> binding_refs(
            cv.insert(ch, ChanView { bindings: cv[ch].bindings.insert((e, ev), b), ..cv[ch] }),
            x,
        ) == #[trigger] binding_refs(cv, x),
{
    let v = ChanView { bindings: cv[ch].bindings.insert((e, ev), b), ..cv[ch] };
    let cv2 = cv.insert(ch, v);
    let s1 = Set::new(
        |t: (ObjId, int, int)|
            cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3 && cv[t.0].bindings[(t.1, t.2)].notif
                == Some(o),
    );
    let s2 = Set::new(
        |t: (ObjId, int, int)|
            cv2.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3 && cv2[t.0].bindings[(t.1, t.2)].notif
                == Some(o),
    );
    let univ = Set::new(|t: (ObjId, int, int)| cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3);
    lemma_binding_triples_finite(cv.dom());
    assert(s1.subset_of(univ));
    vstd::set_lib::lemma_set_subset_finite(univ, s1);
    let x = (ch, e, ev);
    assert(cv2.dom() =~= cv.dom());
    assert(cv2[ch] == v);
    assert forall|t: (ObjId, int, int)| #![trigger s2.contains(t)]
        s2.contains(t) <==> s1.remove(x).contains(t) by {
        if t != x {
            if t.0 == ch {
                if (t.1, t.2) != (e, ev) {
                    assert(v.bindings[(t.1, t.2)] == cv[ch].bindings[(t.1, t.2)]);
                }
            } else {
                assert(cv2[t.0] == cv[t.0]);
            }
        }
    }
    assert(s2 =~= s1.remove(x));
    assert(s1.contains(x));
    // Every other object's binding census is fixed: the cleared triple named `o`, never any
    // `y != o`, so neither its old nor its new value is in `y`'s set — pure extensionality.
    assert forall|y: ObjId| y != o implies #[trigger] binding_refs(cv2, y) == binding_refs(cv, y) by {
        let g1 = Set::new(
            |t: (ObjId, int, int)|
                cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3 && cv[t.0].bindings[(t.1, t.2)].notif
                    == Some(y),
        );
        let g2 = Set::new(
            |t: (ObjId, int, int)|
                cv2.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3 && cv2[t.0].bindings[(t.1, t.2)].notif
                    == Some(y),
        );
        assert forall|t: (ObjId, int, int)| #![trigger g2.contains(t)] g2.contains(t) <==> g1.contains(t) by {
            if t.0 == ch {
                if (t.1, t.2) != (e, ev) {
                    assert(v.bindings[(t.1, t.2)] == cv[ch].bindings[(t.1, t.2)]);
                } else {
                    // The cleared triple: now `None` (not `Some(y)`), and was `Some(o)` (not
                    // `Some(y)` since `y != o`) — absent from both `y`-sets.
                    assert(v.bindings[(e, ev)].notif is None);
                }
            } else {
                assert(cv2[t.0] == cv[t.0]);
            }
        }
        assert(g2 =~= g1);
    }
}

// Replacing a single binding at `(ch, e, ev)` with `b` (the generalization of
// `lemma_binding_drop`, which only cleared): the per-object binding census moves by at most one
// — down at the *old* notif, up at `b`'s notif. Stated in additive form (no `nat` underflow) so
// it composes with `bind_refs_post`'s matching refs delta to give `channel::bind`'s
// `refcount_sound` preservation (the lockstep the binding term was landed for).
pub proof fn lemma_binding_replace(
    cv: Map<ObjId, ChanView>,
    ch: ObjId,
    e: int,
    ev: int,
    b: Binding,
    x: ObjId,
)
    requires
        cv.dom().finite(),
        cv.dom().contains(ch),
        0 <= e < 2,
        0 <= ev < 3,
    ensures
        binding_refs(
            cv.insert(ch, ChanView { bindings: cv[ch].bindings.insert((e, ev), b), ..cv[ch] }),
            x,
        ) + (if cv[ch].bindings[(e, ev)].notif == Some(x) { 1nat } else { 0nat })
            == binding_refs(cv, x) + (if b.notif == Some(x) { 1nat } else { 0nat }),
{
    let v = ChanView { bindings: cv[ch].bindings.insert((e, ev), b), ..cv[ch] };
    let cv2 = cv.insert(ch, v);
    let s1 = Set::new(
        |t: (ObjId, int, int)|
            cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3 && cv[t.0].bindings[(t.1, t.2)].notif
                == Some(x),
    );
    let s2 = Set::new(
        |t: (ObjId, int, int)|
            cv2.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3 && cv2[t.0].bindings[(t.1, t.2)].notif
                == Some(x),
    );
    let univ = Set::new(|t: (ObjId, int, int)| cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3);
    lemma_binding_triples_finite(cv.dom());
    assert(s1.subset_of(univ));
    vstd::set_lib::lemma_set_subset_finite(univ, s1);
    let x0 = (ch, e, ev);
    assert(cv2.dom() =~= cv.dom());
    assert(cv2[ch] == v);
    // `x0` is the only triple whose binding moved; off it the two sets agree.
    assert forall|t: (ObjId, int, int)| t != x0 implies (#[trigger] s2.contains(t) <==> s1.contains(t)) by {
        if t.0 == ch {
            if (t.1, t.2) != (e, ev) {
                assert(v.bindings[(t.1, t.2)] == cv[ch].bindings[(t.1, t.2)]);
            }
        } else {
            assert(cv2[t.0] == cv[t.0]);
        }
    }
    assert(s1.contains(x0) == (cv[ch].bindings[(e, ev)].notif == Some(x)));
    assert(s2.contains(x0) == (b.notif == Some(x)));
    if cv[ch].bindings[(e, ev)].notif == Some(x) {
        if b.notif == Some(x) {
            assert(s2 =~= s1);
        } else {
            assert forall|t: (ObjId, int, int)| #![trigger s2.contains(t)]
                s2.contains(t) <==> s1.remove(x0).contains(t) by {}
            assert(s2 =~= s1.remove(x0));
            assert(s1.contains(x0));
        }
    } else {
        if b.notif == Some(x) {
            assert forall|t: (ObjId, int, int)| #![trigger s2.contains(t)]
                s2.contains(t) <==> s1.insert(x0).contains(t) by {}
            assert(s2 =~= s1.insert(x0));
            assert(!s1.contains(x0));
        } else {
            assert(s2 =~= s1);
        }
    }
}

// A binding naming `o` witnesses a positive binding census — `destroy_channel`'s release
// loop uses it (with `lemma_in_refs_from_census` + `refcount_sound`) to show the bound
// notification is live (`refs[o] >= 1`) before the `-1`, the underflow gate. The
// `lemma_slot_refs_positive` analog over the triple set.
pub proof fn lemma_binding_refs_pos(cv: Map<ObjId, ChanView>, ch: ObjId, e: int, v: int, o: ObjId)
    requires
        cv.dom().finite(),
        cv.dom().contains(ch),
        0 <= e < 2,
        0 <= v < 3,
        cv[ch].bindings[(e, v)].notif == Some(o),
    ensures
        binding_refs(cv, o) >= 1,
{
    let s = Set::new(
        |t: (ObjId, int, int)|
            cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3
                && cv[t.0].bindings[(t.1, t.2)].notif == Some(o),
    );
    let univ = Set::new(|t: (ObjId, int, int)| cv.dom().contains(t.0) && 0 <= t.1 < 2 && 0 <= t.2 < 3);
    lemma_binding_triples_finite(cv.dom());
    assert(s.subset_of(univ));
    vstd::set_lib::lemma_set_subset_finite(univ, s);
    assert(s.contains((ch, e, v)));
    if s.len() == 0 {
        assert(s =~= Set::empty());
    }
}

// ── Construction-side acyclicity preservation. ──
//
// Re-parenting one **detached, childless** slot `child` under `parent` in an
// acyclic store keeps it acyclic — this is the witness *construction* the acyclicity-rank
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
// `retype_install` leans on for its three `set_slot`s — the untyped's
// watermark bump (links + emptiness both fixed) and the two detached dst/dst2
// fills (an empty, hence detached, slot gains a cap with its links still null).
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
    assert forall|j: SlotId| #[trigger] m1.dom().contains(j) && is_empty_cap(m1[j].cap) implies {
        &&& m1[j].parent == None
        &&& m1[j].first_child == None
        &&& m1[j].next_sib == None
        &&& m1[j].prev_sib == None
    } by {
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

// Clearing an already-**detached** slot `k` (all four links null) to an empty,
// detached cap preserves `cspace_wf`. The non-empty→empty direction `lemma_local_cap_edit`
// forbids (it could strand a child) is safe here precisely because `k` is isolated: the
// link clauses read identical (null) links, and `empty_slots_detached` holds because `k`'s
// new (empty) cap is detached. `delete` uses it on the `cdt_unlink`-detached slot.
pub(crate) proof fn lemma_clear_detached_preserves_cspace_wf(
    m0: Map<SlotId, CapSlot>,
    k: SlotId,
    v: CapSlot,
)
    requires
        cspace_wf(m0),
        m0.dom().contains(k),
        m0[k].parent is None,
        m0[k].first_child is None,
        m0[k].next_sib is None,
        m0[k].prev_sib is None,
        v.parent is None,
        v.first_child is None,
        v.next_sib is None,
        v.prev_sib is None,
        is_empty_cap(v.cap),
    ensures
        cspace_wf(m0.insert(k, v)),
        m0.insert(k, v).dom() == m0.dom(),
{
    let m1 = m0.insert(k, v);
    assert(m1.dom() =~= m0.dom());
    // Every slot's four CDT links agree with m0 (k null in both, all others untouched).
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
    // empty_slots_detached: `k`'s new cap is empty and detached; every other slot is
    // unchanged (its emptiness ⟹ detachment carried from m0).
    assert forall|j: SlotId| #[trigger] m1.dom().contains(j) && is_empty_cap(m1[j].cap) implies {
        &&& m1[j].parent == None
        &&& m1[j].first_child == None
        &&& m1[j].next_sib == None
        &&& m1[j].prev_sib == None
    } by {
        if j != k {
            assert(m1[j] == m0[j]);
        }
    }
    assert(cdt_wf(m1));
    let r0 = choose|r: Map<SlotId, nat>| valid_prank(m0, r);
    assert(valid_prank(m1, r0));
    let s0 = choose|s: Map<SlotId, nat>| valid_srank(m0, s);
    assert(valid_srank(m1, s0));
    assert(acyclic(m1));
    assert(sib_acyclic(m1));
}

// ── slot_move as an identity transposition ──
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
                revoking: b.revoking,
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
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) && is_empty_cap(mf[k].cap) implies {
            &&& mf[k].parent == None
            &&& mf[k].first_child == None
            &&& mf[k].next_sib == None
            &&& mf[k].prev_sib == None
        } by {
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

// ── Child-chain reachability: the keystone for the
// children-walk loops' *completeness* — every child of a node lies on its
// `first_child → next_sib` chain, so a walk re-parents all of them. ──

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
        // measure: {child: s > s[j]} ⊊ {child: s > s[k]} (j is in the latter, not
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

// Per-iteration peel for the children re-parent walk (`cdt_unlink` / `slot_move`):
// advancing the cursor from a child `cur` to its `next_sib` `nn` leaves sibling
// reachability unchanged for every other node. For `x != cur`,
// `next_reach(m0, cur, x, srk)` unfolds one step through `m0[cur].next_sib == Some(nn)`,
// and `srk[nn] < srk[cur]` collapses the rank guard, leaving `next_reach(m0, nn, x, srk)`.
proof fn lemma_children_walk_peel(m0: Map<SlotId, CapSlot>, cur: SlotId, nn: SlotId, srk: Map<SlotId, nat>)
    requires
        m0[cur].next_sib == Some(nn),
        srk[nn] < srk[cur],
    ensures
        forall|x: SlotId| x != cur ==> #[trigger] next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk),
{
    assert forall|x: SlotId| x != cur
        implies #[trigger] next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk) by {}
}

// Replacing a slot's empty cap with another empty cap of the *same links* is
// invisible to `cspace_wf` (which reads only link structure + `is_empty_cap`).
// `slot_move` clears `src` to `CapSlot::empty` where the transposition leaves
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

// ── slot_move body-match support: the classification facts
// that turn the imperative neighbour-fixups into the transposition's renaming.
// All follow from `cspace_wf(m0)` + `dst` empty/detached; kept as small
// lemmas so each SMT call starts with a tiny context (the
// per-clause discipline). ──

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
    CapSlot { cap: s.cap, parent: p, first_child: s.first_child, next_sib: s.next_sib, prev_sib: s.prev_sib, revoking: s.revoking }
}
pub open spec fn set_first_child(s: CapSlot, f: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: s.parent, first_child: f, next_sib: s.next_sib, prev_sib: s.prev_sib, revoking: s.revoking }
}
pub open spec fn set_next_sib(s: CapSlot, n: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: s.parent, first_child: s.first_child, next_sib: n, prev_sib: s.prev_sib, revoking: s.revoking }
}
pub open spec fn set_prev_sib(s: CapSlot, p: Option<SlotId>) -> CapSlot {
    CapSlot { cap: s.cap, parent: s.parent, first_child: s.first_child, next_sib: s.next_sib, prev_sib: p, revoking: s.revoking }
}

// The transposition value at `dst`: exactly `m[src]` (unrenamed). `src`'s links
// avoid both `src` (self-link) and `dst` (detached empty), so the rename is the
// identity on them — which is why the body copying `src`'s links into `dst`
// *verbatim* still lands the transposition.
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
// neighbour-fixup case analysis reads each field off this.
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

// ── cdt_unlink body-match support: the sibling-list *merge*.
// Unlike slot_move's transposition, unlink grafts `slot`'s child chain into
// `slot`'s former sibling position one level up. `unlinked(m, slot, last)` is
// the closed-form result; `lemma_unlink_preserves_cspace_wf` proves it keeps
// `cspace_wf`. The structural clauses are factored per-clause (the per-clause
// SMT discipline of the transpose family); the sibling-acyclicity witness is
// the crux (a constant additive shift fails — the child band must be rescaled
// into the `prev..next` gap). ──

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
            CapSlot { cap: m[slot].cap, parent: None, first_child: None, next_sib: None, prev_sib: None, revoking: m[slot].revoking }
        } else if m[k].parent == Some(slot) {
            CapSlot {
                cap: m[k].cap,
                parent: p,
                first_child: m[k].first_child,
                next_sib: if m[k].next_sib is None { nx } else { m[k].next_sib },
                prev_sib: if m[k].prev_sib is None { pv } else { m[k].prev_sib },
                revoking: m[k].revoking,
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
                revoking: m[k].revoking,
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
// (the child chain's rank span can exceed the `prev..next` gap).
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
    assert forall|k: SlotId| #[trigger] mf.dom().contains(k) && is_empty_cap(mf[k].cap) implies {
            &&& mf[k].parent == None
            &&& mf[k].first_child == None
            &&& mf[k].next_sib == None
            &&& mf[k].prev_sib == None
        } by {
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

// The closed-form merge: the spliced arena `mfin` equals `unlinked(m0, slot, last)`.
// Keyed tightly to the straight-line splice chain (rev2§6, doc/guidelines/verus.md §10):
// the per-key case split is the heaviest sub-step of `cdt_unlink`, so it runs here in its
// own solver context, off the children walk's `next_reach`/`valid_srank` quantifiers and
// the `valid_srank` `choose` witness — none of which the merge needs. `requires` are the
// cheap local facts the op already has in hand (the slot-role bindings, the four single
// `Map::insert` splice steps, and `slot`'s untouched-then-cleared entry); `ensures` is the
// closed form. Isomorphic to `lemma_unlink_children`, which verifies cheaply isolated.
proof fn lemma_unlink_merge(
    m0: Map<SlotId, CapSlot>,
    mw: Map<SlotId, CapSlot>,
    ma: Map<SlotId, CapSlot>,
    mb: Map<SlotId, CapSlot>,
    mc: Map<SlotId, CapSlot>,
    md: Map<SlotId, CapSlot>,
    mfin: Map<SlotId, CapSlot>,
    slot: SlotId,
    last: Option<SlotId>,
    parent: Option<SlotId>,
    prev: Option<SlotId>,
    next: Option<SlotId>,
    first: Option<SlotId>,
    head: Option<SlotId>,
)
    requires
        cspace_wf(m0),
        m0.dom().finite(),
        m0.dom().contains(slot),
        last_wf(m0, slot, last),
        // The option locals name `slot`'s own links.
        parent == m0[slot].parent,
        prev == m0[slot].prev_sib,
        next == m0[slot].next_sib,
        first == m0[slot].first_child,
        head == (if first is None { next } else { first }),
        // The all-children-re-parented arena (the children walk's postcondition).
        mw == Map::<SlotId, CapSlot>::new(
            |k: SlotId| m0.dom().contains(k),
            |k: SlotId| if m0[k].parent == Some(slot) { set_parent(m0[k], parent) } else { m0[k] },
        ),
        // The four straight-line splice steps (one `Map::insert` each, §10).
        ma == match prev {
            Some(pv) => mw.insert(pv, set_next_sib(mw[pv], head)),
            None => match parent {
                Some(pa) => mw.insert(pa, set_first_child(mw[pa], head)),
                None => mw,
            },
        },
        mb == match head {
            Some(h) => ma.insert(h, set_prev_sib(ma[h], prev)),
            None => ma,
        },
        mc == match first {
            Some(_) => mb.insert(last->0, set_next_sib(mb[last->0], next)),
            None => mb,
        },
        md == match first {
            Some(_) => match next {
                Some(nx) => mc.insert(nx, set_prev_sib(mc[nx], last)),
                None => mc,
            },
            None => mc,
        },
        // `slot` rode every splice untouched; the clear lands the detached empty-links entry.
        md[slot] == m0[slot],
        mfin =~= md.insert(slot, mfin[slot]),
        mfin[slot] == (CapSlot {
            cap: m0[slot].cap,
            parent: None,
            first_child: None,
            next_sib: None,
            prev_sib: None,
            revoking: m0[slot].revoking,
        }),
    ensures
        mfin =~= unlinked(m0, slot, last),
{
    lemma_unlink_roles(m0, slot);
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
// modulo verus-friendly control flow). ──

/// Bump the refcount of the object a cap designates (no-op for bare caps).
///
/// pre: if the cap designates an object, that object is live and its refcount
/// is below `u32::MAX` (the overflow guard Verus makes explicit — an
/// unchecked refcount bump is a known kernel vulnerability class).
/// post: that object's refcount is +1, all others unchanged; the slot arena is
/// untouched.
pub fn obj_ref<S: Store>(store: &mut S, cap: Cap)
    requires
        cap_obj(cap) matches Some(o) ==> old(store).refs_view().dom().contains(o)
            && old(store).refs_view()[o] < u32::MAX as nat,
    ensures
        final(store).slot_view() == old(store).slot_view(),
        cap_obj(cap) matches Some(o) ==> final(store).refs_view()
            =~= old(store).refs_view().insert(o, (old(store).refs_view()[o] + 1) as nat),
        cap_obj(cap) is None ==> final(store).refs_view() == old(store).refs_view(),
        // The non-refs views are framed (the body is a single `set_obj_refs`, or nothing) — the
        // frame `derive`'s census reasoning composes to carry the four census-bearing views
        // (chan/notif/tcb/timer) across the bump.
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
{
    match cap.kind {
        CapKind::Aspace(o)
        | CapKind::CSpace(o)
        | CapKind::Thread(o, _)
        | CapKind::Channel(o, _)
        | CapKind::Notification(o)
        | CapKind::Timer(o)
        | CapKind::Irq(o) => {
            let r = store.obj_refs(o);
            store.set_obj_refs(o, r + 1);
        }
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => {}
    }
}

/// Insert `child` as the first child of `parent` (the CDT link surgery `derive`
/// and `retype` use).
///
/// pre: the cspace is well-formed; `parent` and `child` are distinct live
/// slots; `child` is detached (all four links null) and non-empty;
/// `parent` is non-empty.
/// post: `child` is `parent`'s first child and the previous children follow it
/// in order (the sibling list is spliced in unchanged); caps and refcounts
/// are untouched; the cspace stays well-formed **and acyclic** (the
/// construction-side acyclicity preservation — `child` is seated as a fresh
/// leaf, so a rank witness is re-exhibited).
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
        // needs to carry `chan_view` across the two inserts it threads
        // between `endpoint_cap_added(A)` and `endpoint_cap_added(B)`.
        final(store).chan_view() == old(store).chan_view(),
        // The other four object views are framed too (the surgery is pure `set_slot`) — the
        // frame `derive` carries the census-bearing notif/tcb/timer views across the splice.
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
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
        // `parent`, holding its cap. These per-slot frames spare callers the
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

/// Derive guard: walk up `start`'s `parent` chain and report whether `start`
/// itself or any ancestor carries the revoke-in-progress marker. `derive` refuses
/// (returns `Err`) on a hit, so no derivation grows the subtree of an in-flight
/// `revoke_step` — which is what keeps the multi-call walk terminating under
/// concurrent derivation (the cross-call liveness is mechanized in the
/// `CapRevocation` TLA model). The walk is the upward mirror of `descend_to_leaf`;
/// it **terminates** by the acyclicity rank — each step moves to a strictly
/// higher-ranked parent, so the already-visited set strictly grows toward the
/// finite domain (`decreases slot_view().dom().difference(visited).len()`). It is
/// read-only (`&S`), so on a hit `derive`'s `Err` path is an exact no-op.
pub fn ancestor_or_self_revoking<S: Store>(store: &S, start: SlotId) -> (res: bool)
    requires
        cdt_wf(store.slot_view()),
        acyclic(store.slot_view()),
        store.slot_view().dom().finite(),
        store.slot_view().dom().contains(start),
{
    let ghost m = store.slot_view();
    let ghost rank = choose|rk: Map<SlotId, nat>| valid_prank(m, rk);
    let mut cur = start;
    let ghost visited: Set<SlotId> = Set::empty();
    loop
        invariant
            store.slot_view() == m,
            valid_prank(m, rank),
            m.dom().finite(),
            m.dom().contains(cur),
            visited.subset_of(m.dom()),
            forall|y: SlotId| visited.contains(y) ==> rank[y] < rank[cur],
            !visited.contains(cur),
        decreases m.dom().difference(visited).len(),
    {
        if store.slot(cur).revoking {
            return true;
        }
        match store.slot(cur).parent {
            None => return false,
            Some(p) => {
                proof {
                    // `cur` claims `p` as parent, so `p` is live and strictly higher-
                    // ranked — `cur != p` and `p` is not yet visited (every visited node
                    // ranks below `cur` < `p`). Adding `cur` shrinks the remaining set.
                    assert(m[cur].parent == Some(p));
                    assert(m.dom().contains(p));
                    assert(rank[cur] < rank[p]);
                    m.dom().lemma_set_insert_diff_decreases(visited, cur);
                    visited = visited.insert(cur);
                }
                cur = p;
            }
        }
    }
}

/// Derive a child cap (rev2§2.3): copy with rights intersected — the only
/// derivation; there is no amplification path.
///
/// pre: the cspace is well-formed; `src`/`dst` are live; if `src` designates an
/// object, that object is live (in the refcount table).
/// post: on `Ok`, `dst` holds a faithful copy of `src`'s cap — same kind and
/// designated object (a fresh Frame copy starts unmapped, rev2§2.5) — with
/// rights ∩ `mask`, so its rights are a **subset** of `src`'s for every
/// `mask` (the load-bearing monotone-derivation theorem, proven ∀ rather
/// than sampled). For a thread cap the rev2§5.4 maximum-controlled-priority
/// ceiling rides along and so attenuates monotonically too (`child.max_prio
/// <= parent.max_prio`, rev2§2.3) — the priority axis of the lattice, here
/// realized as ceiling-preservation. `dst` is `src`'s first child; the object's refcount and
/// slot census both rise by exactly one; the cspace stays well-formed
/// **and acyclic** (`cspace_wf` — `dst` is seated as a fresh leaf).
/// On `Err` (empty/Untyped src, occupied dst, or a refcount already at
/// `u32::MAX`) the store is unchanged. Refusing at the ceiling makes the
/// refcount bump overflow-free for **all** inputs — no unchecked `+ 1`
/// wrap-to-zero (a UAF class); the production `CapCopy` path inherits this.
pub fn derive<S: Store>(store: &mut S, src: SlotId, dst: SlotId, mask: u8, prio_ceiling: u8) -> (res: Result<(), ()>)
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
                  == derived_kind(old(store).slot_view()[src].cap.kind, prio_ceiling)
            // monotone derivation: dst's rights are src's rights masked, hence a
            // subset for ALL masks — authority only ever shrinks.
            &&& final(store).slot_view()[dst].cap.rights.0
                  == (old(store).slot_view()[src].cap.rights.0 & mask)
            &&& (final(store).slot_view()[dst].cap.rights.0
                  & old(store).slot_view()[src].cap.rights.0)
                  == final(store).slot_view()[dst].cap.rights.0
            // rev2§5.4/rev2§2.3 monotone priority ceiling, now *reducing*: a derived thread
            // cap's maximum-controlled-priority ceiling is exactly `min(parent,
            // prio_ceiling)` — never above the parent's (the priority axis of the
            // derivation lattice, ∀) and never above the requested `prio_ceiling`
            // (the rev2§2.3 supervision grant). Discharged from the
            // `derived_kind` equality above (the ceiling rides the kind). With the
            // `cap_copy` sentinel `prio_ceiling = 0xFF` this collapses to exact
            // preservation.
            &&& (cap_max_prio(old(store).slot_view()[src].cap) matches Some(p_mp) ==>
                  cap_max_prio(final(store).slot_view()[dst].cap) matches Some(c_mp)
                    && c_mp == (if p_mp <= prio_ceiling { p_mp } else { prio_ceiling })
                    && c_mp <= p_mp
                    && c_mp <= prio_ceiling)
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
        // `refcount_sound` as a system invariant: a designating copy raises `refs[o]`
        // and the slot census by one in lockstep, so a sound census in yields a sound census out
        // (the Err paths are pure no-ops). Conditional + `requires`-free — the syscall shell, the
        // only caller, is undisturbed.
        refcount_sound(old(store)) ==> refcount_sound(final(store)),
{
    let ghost m0 = old(store).slot_view();
    // Guard: refuse derivation into the subtree of an in-flight revoke. The walk
    // is read-only, so this `Err` is a no-op (the `res is Err` frame holds).
    if ancestor_or_self_revoking(store, src) {
        return Err(());
    }
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

    // One mapping per cap copy (rev2§2.5): a fresh frame copy starts unmapped. A thread
    // copy's rev2§5.4 ceiling is attenuated to `min(parent, prio_ceiling)` (rev2§2.3).
    let kind = match s.cap.kind {
        CapKind::Frame { base, pages, mapping: _ } => CapKind::Frame { base, pages, mapping: None },
        CapKind::Thread(o, mp) => CapKind::Thread(o, if mp <= prio_ceiling { mp } else { prio_ceiling }),
        k => k,
    };
    let cap = Cap { kind, rights: s.cap.rights.masked(mask) };
    assert(kind == derived_kind(s.cap.kind, prio_ceiling));
    assert(cap_obj(cap) == cap_obj(s.cap));
    assert(!is_empty_cap(cap));

    // Refuse rather than wrap: an unchecked refcount bump is a UAF class
    //. Checking here (before any mutation) discharges obj_ref's
    // overflow precondition without trusting the caller, so the bump below is
    // provably total — and the Err path leaves the store untouched.
    let obj_opt = match cap.kind {
        CapKind::Aspace(o)
        | CapKind::CSpace(o)
        | CapKind::Thread(o, _)
        | CapKind::Channel(o, _)
        | CapKind::Notification(o)
        | CapKind::Timer(o)
        | CapKind::Irq(o) => Some(o),
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
        assert(store.slot_view()[dst].cap.kind == derived_kind(m0[src].cap.kind, prio_ceiling));
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
        // refcount_sound (conditional): the full census rises by one exactly at the
        // newly-designated object (`lemma_set_slot_obj_census` composes the slot delta with the
        // four framed non-slot terms), matched by `obj_ref`'s `refs[o] + 1`; a bare/unmapped-frame
        // copy moves neither. So `refs` and the census stay in lockstep ⇒ `refcount_sound` carries.
        if refcount_sound(old(store)) {
            // The derived cap maps no aspace (object caps and the freshly-unmapped frame copy
            // both have `cap_frame_aspace == None`), so the census shift lands only on `cap_obj`.
            assert(cap_frame_aspace(cap) is None);
            match cap_obj(cap) {
                Some(o) => {
                    assert forall|x: ObjId| #[trigger] obj_census(store, x)
                        == obj_census(old(store), x) + (if x == o { 1nat } else { 0nat }) by {
                        lemma_set_slot_obj_census(old(store), store, m1, dst, d, x);
                    }
                    // refs rose by one at `o` (`obj_ref`), matching the census; the domain is
                    // unchanged (`o` was already live), so `refs == census` carries everywhere.
                    assert(store.refs_view()
                        =~= old(store).refs_view().insert(o, (old(store).refs_view()[o] + 1) as nat));
                    assert(old(store).refs_view().dom().contains(o));
                    assert forall|x: ObjId| store.refs_view().dom().contains(x) implies
                        #[trigger] store.refs_view()[x] == obj_census(store, x) by {
                        assert(obj_census(store, x)
                            == obj_census(old(store), x) + (if x == o { 1nat } else { 0nat }));
                        assert(old(store).refs_view()[x] == obj_census(old(store), x));
                    }
                }
                None => {
                    assert forall|x: ObjId| #[trigger] obj_census(store, x)
                        == obj_census(old(store), x) by {
                        lemma_set_slot_obj_census(old(store), store, m1, dst, d, x);
                    }
                    assert(store.refs_view() == old(store).refs_view());
                    lemma_refcount_sound_from_census_eq(old(store), store);
                }
            }
        }
    }
    Ok(())
}

/// Unlink `slot` from the CDT, re-parenting its children one level up (rev2§2.3).
///
/// **Verified** (full body proof). Unlike `slot_move` (a
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
// The (CDT-inert) `revoking` field on `CapSlot` widens every slot
// equality the merge proof carries, nudging this body over the default rlimit on
// macOS. Isolate it in its own solver (`spinoff_prover`) with headroom — the same
// treatment the other heavy cspace bodies already use.
#[verifier::spinoff_prover]
#[verifier::rlimit(10)]
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
        // Unlink moves links, never caps: every slot's cap rides through unchanged (the
        // closed form `unlinked` rebuilds each entry's `cap` from `m0`). This is the
        // cap-frame `delete`'s "teardown only empties slots" reasoning rests on.
        forall|x: SlotId| old(store).slot_view().dom().contains(x)
            ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap,
        // Unlink edits only CDT links in `slot_view`; every object view is framed (each
        // `set_slot` frames them). `delete`'s census/`end_caps`/`caps_consistent` proofs
        // (the teardown body) read these across the `cdt_unlink` that precedes the teardown.
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
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
            store.chan_view() == old(store).chan_view(),
            store.notif_view() == old(store).notif_view(),
            store.tcb_view() == old(store).tcb_view(),
            store.timer_view() == old(store).timer_view(),
            store.timer_head_view() == old(store).timer_head_view(),
            store.ready_view() == old(store).ready_view(),
            store.cspace_view() == old(store).cspace_view(),
            store.irq_view() == old(store).irq_view(),
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
                    lemma_children_walk_peel(m0, cur, nn, srk);
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
        // The final arena is exactly the closed-form merge `unlinked` — the per-key
        // case split is keyed to the splice chain in `lemma_unlink_merge`.
        lemma_unlink_merge(m0, mw, ma, mb, mc, md, mfin, slot, last, parent, prev, next, first, head);
        // Every slot's cap rode through: `mfin == unlinked`, and `unlinked` rebuilds
        // each entry's `.cap` from `m0` (the cap-frame `delete`'s "only empties" rests on).
        assert forall|x: SlotId| m0.dom().contains(x) implies #[trigger] mfin[x].cap
            == m0[x].cap by {
            assert(unlinked(m0, slot, last)[x].cap == m0[x].cap);
        }
        lemma_unlink_preserves_cspace_wf(m0, slot, last);
        lemma_unlink_count(m0, slot, last);
    }
}

/// Move a cap between slots, preserving its CDT position (rev2§3.4: send and receive
/// move caps; a move is the same cap relocating, not a derivation).
///
/// **Verified** (full body proof). The body's whole effect is
/// the identity transposition π=(src dst): because nothing references the isolated
/// empty `dst` (`lemma_nothing_points_to_empty`), copying `src`'s slot onto `dst`
/// verbatim and redirecting `src`'s four neighbour classes (parent / prev / next /
/// children) is exactly the renaming `relabeled(m0, src, dst)`, followed by
/// clearing `src`. The proof shows the final arena equals
/// `relabeled(m0, src, dst).insert(src, CapSlot::empty)`, then reads off
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
        // without it a `slot_move` call havocs every channel cursor.
        final(store).chan_view() == old(store).chan_view(),
        // Likewise the notification/TCB/timer views: `set_slot` frames all of
        // them, so a queued-cap move preserves `binding_notif_wf` for `send`/`recv`.
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
        // `set_slot` frames `irq_view` too, so a queued-cap move preserves it (and
        // hence the `irq_binding_refs` census term — `thread::bind`'s relocation reads it off).
        final(store).irq_view() == old(store).irq_view(),
            final(store).ready_view() == old(store).ready_view(),
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
    let ghost irqv0 = old(store).irq_view();
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
    // The transposition copies the whole slot (incl. the revoke marker), so
    // `d == m0[src]` holds for all six fields. A queued/moved cap is never a
    // revoke root, so this only ever moves `revoking: false`, but the proof needs
    // the exact-slot equality.
    d.revoking = s.revoking;
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
        // exec `==` operator carries no spec — but `.0 ==.0` is native u64 eq.
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
            store.irq_view() == irqv0,
            store.ready_view() == old(store).ready_view(),
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
                    lemma_children_walk_peel(m0, cur, nn, srk);
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
        // Every mutation is `set_slot`, which frames `irq_view`, so the IRQ arena is
        // untouched end to end (the `timer_view` frame's twin).
        assert(store.irq_view() == old(store).irq_view());
    }
}

// ── Cross-object teardown: the refcount plumbing ──
//
// `obj_unref`/`unref_cspace`/`destroy_cspace`, the shared `dec_ref` helper, and `delete`
// (below) — the teardown cycle. With `delete`'s body proven, the cspace cycle
// `delete → obj_unref → destroy_cspace → delete` is closed under the shared lexicographic
// `decreases (count_nonempty(slot_view), height)` (`delete = 0 < destroy_cspace = 1 <
// unref_cspace = 2 < obj_unref = 4`); `delete`'s `delete_prepare` empties its slot before
// recursing, the one count-dropping edge, so the cycle strictly descends. `obj_unref`'s
// Channel/Thread arms recurse through `destroy_channel`/`destroy_tcb`, whose proven bodies
// carry the recursion. The load-bearing invariant is `refcount_sound`: the underflow gate
// for every `refs - 1` and, at the zero point, its census pins the *structural* emptiness
// each destructor's `requires` needs (no waiters, no armed timers, …).

/// Drop one reference to object `o` and restore the census. The shared
/// decrement step `obj_unref`/`unref_cspace` factor out: the caller hands an **off-by-one**
/// state — `refs[o] == census(o) + 1`, sound everywhere else (it already cleared the
/// reference that named `o`) — and the `-1` lands the matching decrement, restoring the
/// full `refcount_sound` invariant. Census-transparent (every object view framed), so the
/// caller can dispatch the at-zero destructor against an unmoved census. The `unref_aspace`
/// proof shape, minus the aspace-specific last-ref `aspace_destroy`.
pub(crate) fn dec_ref<S: Store>(store: &mut S, o: ObjId)
    requires
        old(store).refs_view().dom().contains(o),
        old(store).refs_view()[o] > 0,
        old(store).refs_view()[o] == obj_census(old(store), o) + 1,
        forall|x: ObjId| x != o && old(store).refs_view().dom().contains(x)
            ==> #[trigger] old(store).refs_view()[x] == obj_census(old(store), x),
        // The cap→object invariant rides through unchanged — it reads only object views, all
        // framed by `set_obj_refs`.
        caps_consistent(old(store)),
        // The rev2§3.3 endpoint-cap census is likewise refs-free (chan_view + slot_view, both
        // framed by `set_obj_refs`), so it rides through too.
        end_caps_sound(old(store)),
        // Refs-domain completeness rides through: the census is framed and `set_obj_refs`
        // keeps the domain (insert at an existing key), so the coverage carries.
        census_dom_complete(old(store)),
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
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        census_dom_complete(final(store)),
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
        // census + refs-domain unchanged ⇒ coverage carries.
        assert forall|x: ObjId| #[trigger] obj_census(final(store), x) >= 1
            implies final(store).refs_view().dom().contains(x) by {}
    }
}

/// Tear a cspace down once its last cap is gone (`refs == 0`): delete every cap it still
/// holds (its residents), each through the ordinary CDT cleanup. The loop reads residency
/// through the immutable `cspace_view` and re-reads each slot's emptiness, so a resident
/// already emptied by a sibling's teardown is skipped.
///
/// The recursion into `delete` is bounded by the shared lexicographic `decreases`: the loop
/// itself `decreases` on the resident-index countdown, and `delete`'s contract re-establishes
/// `cspace_wf`/`refcount_sound`/dom (and frames residency) each iteration.
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
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
        cspace_resident_wf(old(store), cs),
        // The resident `delete`s can fire / tear down threads, touching the ready queue.
        ready_wf(old(store).ready_view(), old(store).tcb_view()),
        ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        count_nonempty(final(store).slot_view()) <= count_nonempty(old(store).slot_view()),
        refcount_sound(final(store)),
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        census_dom_complete(final(store)),
        ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ready_complete(final(store).ready_view(), final(store).tcb_view()),
        only_empties(old(store).slot_view(), final(store).slot_view()),
        // Residency is immutable: the resident `delete`s frame `cspace_view`, and emptying a
        // resident never re-homes it, so the residency map rides through.
        final(store).cspace_view() == old(store).cspace_view(),
        // (No `irq_view` frame: a resident may be an `Irq` cap, whose `delete` runs
        // `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
        // The channel skeleton rides through every resident `delete`.
        chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        // Dead, queue-detached TCBs are frozen across the resident loop: each `delete`
        // carries `dead_tcb_frozen`, and the loop invariant
        // threads it (the antecedent is self-preserving). `unref_cspace`/`obj_unref` read it off;
        // `destroy_tcb` consumes it for its halted subject across its `unref_cspace`.
        dead_tcb_frozen(old(store), final(store)),
        // The home maps are framed: the residency is immutable, the channel skeleton
        // rides through, and each resident `delete` keeps the TCB domain + every `bind_slots`.
        home_views_frozen(old(store), final(store)),
        // Home-frame provenance: this destructor empties only homed slots (its residents — each itself a
        // resident of `cs` — and their recursive closure), so every un-homed slot keeps its cap.
        unhomed_frozen_free(old(store), final(store)),
        // Death-provenance: every emptied slot was a home handle of a dead object. A resident
        // `sid` emptied by a `delete` is homed by `cs` (it is `cs`'s resident `i`), and `cs` is dead
        // throughout (its `refs == 0` at entry, monotone-preserved); the recursive closure each
        // `delete` clears carries its own witness via the target-aware frame.
        emptied_via_dead_home_free(old(store), final(store)),
        // "Dead stays dead" across the resident loop (each `delete` only decrements/removes objects).
        refs_death_persist(old(store), final(store)),
    // SCC measure: `destroy_cspace` sits above `delete` (1 > 0); its
    // resident-loop `delete` calls are count-flat on the first iteration, so the height drops.
    decreases count_nonempty(old(store).slot_view()), 1int
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
            // `delete` preserves the cap→object invariant, so the resident-walk
            // maintains it for the next iteration's `delete`.
            caps_consistent(store),
            // …and the rev2§3.3 endpoint-cap census.
            end_caps_sound(store),
            // …and refs-domain completeness (each `delete` requires + re-establishes it).
            census_dom_complete(store),
            // The ready pair carries across the resident loop — each `delete` ensures it.
            ready_wf(store.ready_view(), store.tcb_view()),
            ready_complete(store.ready_view(), store.tcb_view()),
            // Teardown only empties slots — composes across the resident deletes.
            only_empties(old(store).slot_view(), store.slot_view()),
            // Residency is immutable — `delete` frames `cspace_view`, and dom is preserved,
            // so `cs`'s residents stay live and the getters stay in-bounds across the loop.
            store.cspace_view() == old(store).cspace_view(),
            // (No `irq_view` invariant: a resident may be an `Irq` cap, whose `delete` runs
            // `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
            // The channel skeleton composes across the resident deletes.
            chan_struct_frame(old(store).chan_view(), store.chan_view()),
            // Dead, queue-detached TCBs are frozen across the resident deletes so far
            // — `lemma_dead_tcb_frozen_trans` extends it past each `delete`.
            dead_tcb_frozen(old(store), store),
            // The home maps stay framed across the resident deletes.
            home_views_frozen(old(store), store),
            // Home-frame provenance composes across the resident deletes: only homed slots emptied.
            unhomed_frozen_free(old(store), store),
            // Death-provenance composes across the resident deletes.
            emptied_via_dead_home_free(old(store), store),
            refs_death_persist(old(store), store),
            // `cs` is dead throughout: `refs[cs] == 0` at entry, monotone-preserved by the
            // death-persistence above — the death witness for each resident `sid` being emptied.
            dead_obj(store, cs),
            cspace_resident_wf(store, cs),
        decreases n - i,
    {
        let sid = store.cspace_slot(cs, i);
        if !cap_is_empty(store.slot(sid).cap) {
            let ghost sv_before = store.slot_view();
            let ghost cv_before = store.chan_view();
            let ghost st_before = *store;
            delete(store, sid);
            proof {
                lemma_only_empties_trans(old(store).slot_view(), sv_before, store.slot_view());
                lemma_chan_struct_frame_trans(old(store).chan_view(), cv_before, store.chan_view());
                lemma_dead_tcb_frozen_trans(old(store), &st_before, store);
                // Home frame: `sid` is a resident of `cs` (the loop index `i` witnesses it), so it is
                // homed — lifting `delete`'s target-aware frame to the free frame this destructor
                // exports, then composing across the loop.
                assert(st_before.cspace_view()[cs].slots[i as int] == sid);
                assert(is_homed(&st_before, sid));
                lemma_unhomed_frozen_free_from_homed(&st_before, store, sid);
                lemma_unhomed_frozen_free_trans(old(store), &st_before, store);
                lemma_home_views_frozen_trans(old(store), &st_before, store);
                // Death-provenance: `cs` homes `sid` (resident `i`) at `st_before`, and `cs` stays dead
                // (`refs[cs] == 0` is monotone-preserved by `delete`'s `refs_death_persist`), so the
                // directly-deleted `sid` carries the death witness `cs`. Lift `delete`'s
                // target-aware frame to the free frame and compose across the loop.
                assert(homes_in_cspace(&st_before, cs, sid)) by {
                    assert(st_before.cspace_view().dom().contains(cs));
                    assert(st_before.cspace_view()[cs].slots[i as int] == sid);
                }
                assert(homes(&st_before, cs, sid));
                // `cs` is dead at `st_before` (`refs[cs] == 0`, in-domain) and `delete` preserves
                // death (its `refs_death_persist`), so `cs` is dead at `store`.
                assert(dead_obj(&st_before, cs));
                assert(dead_obj(store, cs));
                lemma_emptied_via_dead_home_free_from_homed(&st_before, store, sid, cs);
                lemma_emptied_via_dead_home_free_trans(old(store), &st_before, store);
                lemma_refs_death_persist_trans(old(store), &st_before, store);
            }
        }
        i += 1;
    }
    // Memory returns to the donor untyped only via revoke of the untyped cap; no
    // allocator hands it back early.
}

/// Drop the refcount a cap holds on its object; at zero, run the type-specific teardown
///. The shared decrement (`dec_ref`) carries the off-by-one census; at the zero
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
        cap.kind matches CapKind::Thread(o, _) ==> {
            &&& old(store).slot_view().dom().finite()
            &&& old(store).tcb_view().dom().contains(o)
            &&& old(store).tcb_view()[o].bind_slots.len() == 2
            &&& old(store).slot_view().dom().contains(old(store).tcb_view()[o].bind_slots[0])
            &&& old(store).slot_view().dom().contains(old(store).tcb_view()[o].bind_slots[1])
            // The bound cspace is resident-wf — `destroy_tcb`'s `unref_cspace` needs it to drive
            // the at-zero `destroy_cspace`, and by then the TCB's own cap is gone. `delete`
            // supplies it from `caps_consistent`'s strengthened Thread clause.
            &&& (old(store).tcb_view()[o].cspace matches Some(cs) ==>
                    cspace_resident_wf(old(store), cs))
            // Waiter-coherence — `destroy_tcb`'s BlockedNotif branch `remove_waiter` needs
            // `notif_wf(wn)`; same provenance.
            &&& (old(store).tcb_view()[o].state == ThreadState::BlockedNotif ==>
                    (old(store).tcb_view()[o].wait_notif matches Some(wn) ==>
                        notif_wf(old(store).notif_view(), old(store).tcb_view(), wn)))
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
        cap.kind matches CapKind::Irq(o) ==> {
            &&& old(store).irq_view().dom().contains(o)
            &&& old(store).irq_view().dom().finite()
            &&& irq_wf(old(store).irq_view())
            // `o`'s own binding names a live notification (the `destroy_irq` invariant). The
            // census rules out `o == n` (a self-bound IRQ would make `census(o) >= 1`, but the
            // zero branch has `census(o) == 0`), so the `-1` on `refs[o]` never touches `refs[n]`.
            &&& (old(store).irq_view()[o].bound ==>
                    (old(store).irq_view()[o].notif matches Some(n) ==>
                        old(store).refs_view().dom().contains(n)
                        && old(store).refs_view()[n] > 0))
        },
        // The system cap→object invariant: needed for the `destroy_channel`/
        // `destroy_tcb` arms (which delete arbitrary caps) and preserved through the `-1`.
        caps_consistent(old(store)),
        // The rev2§3.3 endpoint-cap census: the recursive
        // destructors delete arbitrary channel caps, so it threads through here too.
        end_caps_sound(old(store)),
        // Refs-domain completeness: threaded so the destructors keep
        // it for their recursive `delete`s; the at-zero teardown only ever removes an object
        // whose census is already 0, so coverage carries.
        census_dom_complete(old(store)),
        // The Thread/Channel arms reach the ready queue (`destroy_tcb`'s `unqueue_ready`,
        // `destroy_channel`'s peer-closed `fire`), so `obj_unref` carries the ready pair. `delete`
        // (its sole caller) supplies them.
        ready_wf(old(store).ready_view(), old(store).tcb_view()),
        ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        refcount_sound(final(store)),
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        count_nonempty(final(store).slot_view()) <= count_nonempty(old(store).slot_view()),
        ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ready_complete(final(store).ready_view(), final(store).tcb_view()),
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        census_dom_complete(final(store)),
        // Teardown only empties slots: `dec_ref` frames `slot_view`, so the
        // dispatched destructor's `only_empties` carries straight through.
        only_empties(old(store).slot_view(), final(store).slot_view()),
        // Residency is immutable across every arm: `dec_ref`/`unref_aspace` frame
        // `cspace_view`, and each at-zero destructor frames it too (a destroyed cspace keeps
        // its residency map). `delete` reads it off to discharge its own residency frame
        //.
        final(store).cspace_view() == old(store).cspace_view(),
        // Note: `obj_unref` does NOT frame `irq_view` — its Irq arm runs `destroy_irq`, which
        // mutates it (exactly as the Timer arm mutates `timer_view`, which is likewise not framed).
        // The channel skeleton survives every arm (each destructor preserves it — the
        // recursive ones carry it, `destroy_notif`/`destroy_timer` frame `chan_view` whole);
        // `delete` reads it off.
        chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        // Dead, queue-detached TCBs are frozen: `dec_ref` frames
        // `tcb`/`refs` whole except the unref'd `o` (which had `refs[o] > 0`, so it is not in the
        // dead set), and each at-zero destructor carries `dead_tcb_frozen` (its Thread arm with
        // its own subject excepted — but that subject is `o`, again refs-positive at entry). So
        // `delete` reads it off.
        dead_tcb_frozen(old(store), final(store)),
        // The home maps are framed: `dec_ref` frames them whole, every destructor
        // carries them. `delete` reads it off for its own `home_views_frozen`.
        home_views_frozen(old(store), final(store)),
        // Home-frame provenance: the dispatched destructor empties only homed slots (`dec_ref` frames
        // `slot_view`), so every un-homed slot keeps its cap. `delete` composes it onto `slot`.
        unhomed_frozen_free(old(store), final(store)),
        // Death-provenance: every slot the dispatched destructor empties was a home handle of a
        // dead object (the object whose `refs` reached zero). `delete` composes it onto `slot`.
        emptied_via_dead_home_free(old(store), final(store)),
        // "Dead stays dead" across this op (its decrement/destructor never re-refs an object) — the
        // refs-monotone fact the death-provenance composition needs.
        refs_death_persist(old(store), final(store)),
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
            &&& final(store).irq_view() == old(store).irq_view()
        },
        // Dropping a **notification** cap is robustly clean: `dec_ref` drops only `refs[n]`,
        // and at zero `destroy_notif` is a model view no-op, so every object view *and* every
        // slot's cap survive (only `refs[n]` moves). This is the additive enabling clause
        // `delete`'s notification frame (and `thread::bind`) reads off.
        cap_notif(cap) is Some ==> {
            &&& final(store).slot_view() == old(store).slot_view()
            &&& final(store).chan_view() == old(store).chan_view()
            &&& final(store).notif_view() == old(store).notif_view()
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).timer_view() == old(store).timer_view()
            &&& final(store).timer_head_view() == old(store).timer_head_view()
            &&& final(store).irq_view() == old(store).irq_view()
        },
    // SCC measure: `obj_unref` is the top of the height order — its
    // `dec_ref`-then-destructor calls are count-flat, so the descent to the destructors is by height.
    decreases count_nonempty(old(store).slot_view()), 4int
{
    let ghost st0 = *store;
    match cap.kind {
        CapKind::CSpace(o) => {
            dec_ref(store, o);
            let ghost st1 = *store;
            proof {
                lemma_dead_tcb_frozen_dec_ref(&st0, store, o);
                // `dec_ref` frames `ready_view` + `tcb_view`, so the ready pair carries to `st1`.
                lemma_ready_inv_frame(&st0, store);
                // Home frame: `dec_ref` frames `slot_view` + every object view (so the home maps are
                // framed and no slot is emptied) — the base the at-zero destructor composes onto.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: `dec_ref` empties no slot (free frame refl) and only drops `refs[o]` by
                // one from a positive count (death persists).
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_dec_ref(&st0, store, o);
            }
            if store.obj_refs(o) == 0 {
                destroy_cspace(store, o);
                proof {
                    lemma_dead_tcb_frozen_trans(&st0, &st1, store);
                    // Home frame: `destroy_cspace` exports the free + home frames; compose with `dec_ref`.
                    lemma_unhomed_frozen_free_trans(&st0, &st1, store);
                    lemma_home_views_frozen_trans(&st0, &st1, store);
                    // Death-provenance: compose `dec_ref`'s frame with `destroy_cspace`'s exported frame.
                    lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
                    lemma_refs_death_persist_trans(&st0, &st1, store);
                }
            }
        }
        CapKind::Thread(o, _) => {
            dec_ref(store, o);
            let ghost st1 = *store;
            proof {
                lemma_dead_tcb_frozen_dec_ref(&st0, store, o);
                // `dec_ref` frames `ready_view` + `tcb_view`, so the ready pair carries to `st1`.
                lemma_ready_inv_frame(&st0, store);
                // Home frame: `dec_ref` frames `slot_view` + every object view (so the home maps are
                // framed and no slot is emptied) — the base the at-zero destructor composes onto.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: `dec_ref` empties no slot and only drops a positive `refs[o]`.
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_dec_ref(&st0, store, o);
            }
            if store.obj_refs(o) == 0 {
                crate::thread::destroy_tcb(store, o);
                proof {
                    // `destroy_tcb` carries the dead frame with its own subject `o` excepted; but
                    // `o` had `refs[o] > 0` at entry (`st0`), so it is not in the dead set anyway.
                    // Compose `st0 → st1` (`dec_ref`) with `st1 → final` (`destroy_tcb`-except-`o`).
                    assert forall|x: ObjId| #[trigger] dead_tcb_frozen_at(&st0, store, x) by {
                        assert(dead_tcb_frozen_at(&st0, &st1, x));
                        if x != o {
                            assert(dead_tcb_frozen_at(&st1, store, x));
                        }
                        if st0.tcb_view().dom().contains(x) && st0.refs_view().dom().contains(x)
                            && st0.refs_view()[x] == 0 && st0.tcb_view()[x].wait_notif is None {
                            // `o` had `refs[o] > 0` at `st0`, so a dead `x != o`.
                            assert(x != o);
                            assert(st1.tcb_view()[x] == st0.tcb_view()[x]);
                        }
                    }
                    // Home frame: `destroy_tcb` exports the free + home frames; compose with `dec_ref`.
                    lemma_unhomed_frozen_free_trans(&st0, &st1, store);
                    lemma_home_views_frozen_trans(&st0, &st1, store);
                    // Death-provenance: compose `dec_ref`'s frame with `destroy_tcb`'s exported frame.
                    lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
                    lemma_refs_death_persist_trans(&st0, &st1, store);
                }
            }
        }
        CapKind::Channel(o, _) => {
            dec_ref(store, o);
            let ghost st1 = *store;
            proof {
                lemma_dead_tcb_frozen_dec_ref(&st0, store, o);
                // `dec_ref` frames `ready_view` + `tcb_view`, so the ready pair carries to `st1`.
                lemma_ready_inv_frame(&st0, store);
                // Home frame: `dec_ref` frames `slot_view` + every object view (so the home maps are
                // framed and no slot is emptied) — the base the at-zero destructor composes onto.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: `dec_ref` empties no slot and only drops a positive `refs[o]`.
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_dec_ref(&st0, store, o);
            }
            if store.obj_refs(o) == 0 {
                // Death-provenance: `o` is in `refs.dom()` (dec_ref preserved it) at `refs == 0`, so it is
                // dead — `destroy_channel`'s death-witness precondition.
                proof { assert(dead_obj(store, o)); }
                crate::channel::destroy_channel(store, o);
                proof {
                    lemma_dead_tcb_frozen_trans(&st0, &st1, store);
                    // Home frame: `destroy_channel` exports the free + home frames; compose with `dec_ref`.
                    lemma_unhomed_frozen_free_trans(&st0, &st1, store);
                    lemma_home_views_frozen_trans(&st0, &st1, store);
                    // Death-provenance: compose `dec_ref`'s frame with `destroy_channel`'s exported frame.
                    lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
                    lemma_refs_death_persist_trans(&st0, &st1, store);
                }
            }
        }
        CapKind::Notification(o) => {
            dec_ref(store, o);
            let ghost st1 = *store;
            proof {
                lemma_dead_tcb_frozen_dec_ref(&st0, store, o);
                // `dec_ref` frames `ready_view` + `tcb_view`, so the ready pair carries to `st1`.
                lemma_ready_inv_frame(&st0, store);
                // Home frame: `dec_ref` frames `slot_view` + every object view (so the home maps are
                // framed and no slot is emptied) — the base the at-zero destructor composes onto.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: `dec_ref` empties no slot and only drops a positive `refs[o]`.
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_dec_ref(&st0, store, o);
            }
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
                proof {
                    // `destroy_notif` frames every view (a model no-op), so `store == st1`.
                    lemma_dead_tcb_frozen_refl(&st1, store);
                    // The model no-op frames `ready_view` + `tcb_view`; carry the pair from `st1`.
                    lemma_ready_inv_frame(&st1, store);
                    lemma_dead_tcb_frozen_trans(&st0, &st1, store);
                    // Home frame: model no-op — free + home refl, composed onto `dec_ref`.
                    lemma_unhomed_frozen_free_from_slot_eq(&st1, store);
                    lemma_unhomed_frozen_free_trans(&st0, &st1, store);
                    lemma_home_views_frozen_refl(&st1, store);
                    lemma_home_views_frozen_trans(&st0, &st1, store);
                    // Death-provenance: model no-op — free + death-persist refl (refs framed equal),
                    // composed onto `dec_ref`.
                    lemma_emptied_via_dead_home_free_from_slot_eq(&st1, store);
                    lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
                    lemma_refs_death_persist_from_refs_eq(&st1, store);
                    lemma_refs_death_persist_trans(&st0, &st1, store);
                }
            }
        }
        CapKind::Timer(o) => {
            dec_ref(store, o);
            let ghost st1 = *store;
            proof {
                lemma_dead_tcb_frozen_dec_ref(&st0, store, o);
                // `dec_ref` frames `ready_view` + `tcb_view`, so the ready pair carries to `st1`.
                lemma_ready_inv_frame(&st0, store);
                // Home frame: `dec_ref` frames `slot_view` + every object view (so the home maps are
                // framed and no slot is emptied) — the base the at-zero destructor composes onto.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: `dec_ref` empties no slot and only drops a positive `refs[o]`.
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_dec_ref(&st0, store, o);
            }
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
                proof {
                    lemma_dead_tcb_frozen_trans(&st0, &st1, store);
                    // `destroy_timer` frames `ready_view` + `tcb_view`; carry the pair from `st1`.
                    lemma_ready_inv_frame(&st1, store);
                    // Home frame: `destroy_timer` frames `slot_view` + every object view — free + home refl.
                    lemma_unhomed_frozen_free_from_slot_eq(&st1, store);
                    lemma_unhomed_frozen_free_trans(&st0, &st1, store);
                    lemma_home_views_frozen_refl(&st1, store);
                    lemma_home_views_frozen_trans(&st0, &st1, store);
                    // Death-provenance: `destroy_timer` frames `slot_view` (free refl) and exports
                    // `refs_death_persist`; compose onto `dec_ref`.
                    lemma_emptied_via_dead_home_free_from_slot_eq(&st1, store);
                    lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
                    lemma_refs_death_persist_trans(&st0, &st1, store);
                }
            }
        }
        CapKind::Irq(o) => {
            // The `Timer(o)` arm, term-for-term (the IRQ object is the timer's census twin):
            // `dec_ref` then, at zero, `destroy_irq`. `irq_binding_refs` replaces
            // `armed_timer_refs` in the no-self-bind argument; the frame-lemma cascade is identical
            // (`destroy_irq` frames `slot_view` + every object view exactly as `destroy_timer` does).
            dec_ref(store, o);
            let ghost st1 = *store;
            proof {
                lemma_dead_tcb_frozen_dec_ref(&st0, store, o);
                lemma_ready_inv_frame(&st0, store);
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_dec_ref(&st0, store, o);
            }
            if store.obj_refs(o) == 0 {
                proof {
                    // census(o) == 0 ⟹ irq_binding_refs(o) == 0 ⟹ no IRQ is bound to `o`; in
                    // particular `o` is not self-bound, so `destroy_irq`'s bound-notif-live
                    // precondition (`o.notif == Some(n)` ⟹ n live, n ≠ o) is discharged.
                    assert(store.refs_view()[o] == obj_census(store, o));
                    let bound = store.irq_view().dom().filter(
                        |k: ObjId| store.irq_view()[k].bound && store.irq_view()[k].notif == Some(o));
                    assert(irq_binding_refs(store.irq_view(), o) == 0);
                    assert(bound.finite());
                    assert(bound.len() == 0);
                    assert(!bound.contains(o));
                    assert(!(store.irq_view()[o].bound && store.irq_view()[o].notif == Some(o)));
                    // dec_ref framed the irq view and dropped only refs[o] (to 0).
                    assert(store.irq_view() == old(store).irq_view());
                    assert(store.refs_view() == old(store).refs_view().insert(o, 0));
                }
                crate::irq::destroy_irq(store, o);
                proof {
                    lemma_dead_tcb_frozen_trans(&st0, &st1, store);
                    lemma_ready_inv_frame(&st1, store);
                    lemma_unhomed_frozen_free_from_slot_eq(&st1, store);
                    lemma_unhomed_frozen_free_trans(&st0, &st1, store);
                    lemma_home_views_frozen_refl(&st1, store);
                    lemma_home_views_frozen_trans(&st0, &st1, store);
                    lemma_emptied_via_dead_home_free_from_slot_eq(&st1, store);
                    lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
                    lemma_refs_death_persist_trans(&st0, &st1, store);
                }
            }
        }
        CapKind::Aspace(o) => {
            // Decrement-then-maybe-`aspace_destroy` — exactly `unref_aspace`'s body, reused.
            unref_aspace(store, o);
            proof {
                // `unref_aspace` frames `ready_view` + `tcb_view` (aspace teardown never
                // touches the ready queue); carry the pair across it.
                lemma_ready_inv_frame(&st0, store);
                // `unref_aspace` frames `tcb` whole and drops `refs` only at `o` (`refs[o] > 0` at
                // entry, so a dead `x != o`); `o` may leave the domain, but no dead object does.
                assert forall|x: ObjId| #[trigger] dead_tcb_frozen_at(&st0, store, x) by {
                    if st0.tcb_view().dom().contains(x) && st0.refs_view().dom().contains(x)
                        && st0.refs_view()[x] == 0 && st0.tcb_view()[x].wait_notif is None {
                        assert(x != o);
                    }
                }
                // Home frame: `unref_aspace` frames `slot_view` + every object view — free + home refl.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: `unref_aspace` frames `slot_view` (free refl) and exports
                // `refs_death_persist` (it only drops/removes `o`, never re-refs an object).
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
            }
        }
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => {
            proof {
                lemma_dead_tcb_frozen_refl(&st0, store);
                // A no-op (store untouched) — the ready pair carries.
                lemma_ready_inv_frame(&st0, store);
                // Home frame: a no-op (store untouched) — free + home refl.
                lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
                lemma_home_views_frozen_refl(&st0, store);
                // Death-provenance: a no-op — free refl + death-persist refl (refs framed equal).
                lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
                lemma_refs_death_persist_from_refs_eq(&st0, store);
            }
        }
    }
}

/// Drop one reference to cspace `cs` (a bound thread holds one — released by
/// `destroy_tcb`); at zero, tear it down. `obj_unref`'s CSpace arm in isolation,
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
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
        cspace_resident_wf(old(store), cs),
        // The at-zero `destroy_cspace` runs resident `delete`s that can touch the ready queue.
        ready_wf(old(store).ready_view(), old(store).tcb_view()),
        ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        refcount_sound(final(store)),
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        count_nonempty(final(store).slot_view()) <= count_nonempty(old(store).slot_view()),
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        census_dom_complete(final(store)),
        ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ready_complete(final(store).ready_view(), final(store).tcb_view()),
        only_empties(old(store).slot_view(), final(store).slot_view()),
        // Residency is immutable: `dec_ref` and `destroy_cspace` both frame `cspace_view`, so
        // `destroy_tcb`'s cspace release carries it.
        final(store).cspace_view() == old(store).cspace_view(),
        // (No `irq_view` frame: the at-zero `destroy_cspace` may `delete` an `Irq` resident,
        // which runs `destroy_irq` and mutates `irq_view` — the `timer_view` precedent.)
        // The channel skeleton rides through (`dec_ref` frames `chan_view`; `destroy_cspace`
        // carries the skeleton) — `destroy_tcb`'s cspace release reads it.
        chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        // Dead, queue-detached TCBs are frozen: `dec_ref` frames
        // `tcb` whole (and `refs` except `cs`, which is refs-positive at entry so not in the dead
        // set), and `destroy_cspace` carries it. `destroy_tcb`'s cspace release reads it off to
        // preserve its own halted subject.
        dead_tcb_frozen(old(store), final(store)),
        // The home maps are framed: `dec_ref` frames them whole, `destroy_cspace`
        // carries them. `destroy_tcb`'s cspace release reads it off.
        home_views_frozen(old(store), final(store)),
        // Home-frame provenance: `dec_ref` frames `slot_view` and `destroy_cspace` empties only homed
        // residents — so every un-homed slot keeps its cap. `destroy_tcb` reads it off.
        unhomed_frozen_free(old(store), final(store)),
        // Death-provenance: every emptied slot was a home handle of a dead object; "dead stays
        // dead" across the op. `destroy_tcb` reads these off (and composes onto its own subject).
        emptied_via_dead_home_free(old(store), final(store)),
        refs_death_persist(old(store), final(store)),
    // SCC measure: once `destroy_tcb` is a proven body the cycle
    // `destroy_tcb → unref_cspace → destroy_cspace → delete → obj_unref → destroy_tcb` is visible,
    // so `unref_cspace` joins the SCC and needs the shared lexicographic measure. Height 2 (above
    // `destroy_cspace`=1/`delete`=0, below `destroy_tcb`=3): its `dec_ref`-then-`destroy_cspace`
    // call is count-flat, so the descent is by height.
    decreases count_nonempty(old(store).slot_view()), 2int
{
    let ghost st0 = *store;
    dec_ref(store, cs);
    let ghost st1 = *store;
    proof {
        // `dec_ref` froze `tcb` whole and only dropped `refs[cs]` (cs ∉ dead set: `refs[cs] > 0`).
        lemma_dead_tcb_frozen_dec_ref(&st0, store, cs);
        // `dec_ref` frames `ready_view` + `tcb_view`, so the ready pair carries to `st1`
        // (and onward — `destroy_cspace` ensures it when the at-zero teardown runs).
        lemma_ready_inv_frame(&st0, store);
        // Home-frame base: `dec_ref` frames `slot_view` + every object view (free + home refl).
        lemma_unhomed_frozen_free_from_slot_eq(&st0, store);
        lemma_home_views_frozen_refl(&st0, store);
        // Death-provenance base: `dec_ref` empties no slot and only drops a positive `refs[cs]`.
        lemma_emptied_via_dead_home_free_from_slot_eq(&st0, store);
        lemma_refs_death_persist_dec_ref(&st0, store, cs);
    }
    if store.obj_refs(cs) == 0 {
        destroy_cspace(store, cs);
        proof {
            lemma_dead_tcb_frozen_trans(&st0, &st1, store);
            // Home frame: `destroy_cspace` exports the free + home frames; compose with `dec_ref`.
            lemma_unhomed_frozen_free_trans(&st0, &st1, store);
            lemma_home_views_frozen_trans(&st0, &st1, store);
            // Death-provenance: compose `dec_ref`'s frame with `destroy_cspace`'s exported frame.
            lemma_emptied_via_dead_home_free_trans(&st0, &st1, store);
            lemma_refs_death_persist_trans(&st0, &st1, store);
        }
    }
}

/// Drop a non-cap reference to an aspace — mapped frames and bound threads hold
/// these so the aspace can't die under them. The first
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
/// the seam contract). `refs[a] > 0` is the underflow gate for `obj_refs(a) - 1`.
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
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
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
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        // Aspaces appear in no `cap_consistent` arm and every object view is framed, so the
        // invariant is preserved.
        caps_consistent(final(store)),
        // The endpoint-cap census reads only chan_view + slot_view (both framed by
        // `set_obj_refs`/`aspace_destroy`), so it rides through.
        end_caps_sound(final(store)),
        // Refs-domain completeness: `a` only leaves the domain when its census is 0 (it was
        // last-ref, `refs[a] == 0 ⟹ census(a) == 0`); every other object's census and domain
        // membership are framed, so the coverage carries.
        census_dom_complete(final(store)),
        // Death-provenance: `unref_aspace` frames `slot_view`, so it empties no slot (the free frame is
        // vacuously true), and it only drops/removes `a` (never re-refs an object), so a dead
        // object stays dead. `obj_unref`'s Aspace arm reads these off.
        emptied_via_dead_home_free(old(store), final(store)),
        refs_death_persist(old(store), final(store)),
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
        // census_dom_complete: census framed; `a` only left the domain when `refs[a]` hit 0,
        // which (off-by-one) means `census(a) == 0`, so the coverage carries.
        assert forall|o: ObjId| #[trigger] obj_census(final(store), o) >= 1
            implies final(store).refs_view().dom().contains(o) by {
            assert(obj_census(old(store), o) >= 1);
        }
        // Death-provenance: `slot_view` is framed equal, so no slot is emptied (free frame trivial).
        lemma_emptied_via_dead_home_free_from_slot_eq(old(store), store);
        // Death-provenance "dead stays dead": `refs` only dropped/removed `a` (which had `refs[a] > 0`),
        // so a dead object `o != a` keeps its status, and `a` (now removed or 0) is itself dead.
        assert forall|o: ObjId| dead_obj(old(store), o) implies #[trigger] dead_obj(store, o) by {
            if old(store).refs_view().dom().contains(o) && old(store).refs_view()[o] == 0 {
                assert(o != a);
            }
        }
    }
}

/// **Map-time refcount increment** — the increment twin of [`unref_aspace`] (rev2§6.1(c)).
///
/// **Under-by-one census precondition.** The caller ([`map_frame`]) records the mapping that
/// names `a` *before* calling, so at entry `a`'s census has already risen by one while
/// `refs[a]` has not: `refs[a] + 1 == census(a)`, sound everywhere else. The `+1` here lands
/// the matching increment, restoring the full `refcount_sound` invariant — the exact mirror of
/// `unref_aspace`'s `-1` (which lands after the slot is *cleared*). `refs[a] < u32::MAX` is the
/// overflow gate for `obj_refs(a) + 1`.
///
/// The proof is light (the `unref_aspace` mirror): `obj_census` reads only the seven object
/// views (never `refs_view`), and `set_obj_refs` frames those views, so the census is invariant
/// across this op — no per-term recount is needed inside `ref_aspace`.
pub fn ref_aspace<S: Store>(store: &mut S, a: ObjId)
    requires
        old(store).refs_view().dom().contains(a),
        old(store).refs_view()[a] < u32::MAX as nat,
        old(store).refs_view()[a] + 1 == obj_census(old(store), a),
        forall|o: ObjId| o != a && old(store).refs_view().dom().contains(o)
            ==> #[trigger] old(store).refs_view()[o] == obj_census(old(store), o),
        caps_consistent(old(store)),
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
    ensures
        final(store).refs_view() == old(store).refs_view().insert(
            a,
            (old(store).refs_view()[a] + 1) as nat,
        ),
        refcount_sound(final(store)),
        final(store).slot_view() == old(store).slot_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        // Aspaces appear in no `cap_consistent` arm and every object view is framed, so the
        // invariant is preserved (the `unref_aspace` argument, unchanged by the `+1`).
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        // `a` was already in the domain (it gained a reference), and every census is framed
        // (only `refs[a]` rose, to meet its census), so coverage carries.
        census_dom_complete(final(store)),
{
    let r = store.obj_refs(a);
    store.set_obj_refs(a, r + 1);
    proof {
        // Every census view is framed unchanged by `set_obj_refs`, so the recount is invariant.
        assert forall|o: ObjId| #[trigger] obj_census(final(store), o)
            == obj_census(old(store), o) by {}
        // refcount_sound: `a`'s refs rose with the `+1` to meet its census (the under-by-one
        // closed); every other object's refs and census are both untouched.
        assert forall|o: ObjId| final(store).refs_view().dom().contains(o)
            implies #[trigger] final(store).refs_view()[o] == obj_census(final(store), o) by {
            if o != a {
                assert(old(store).refs_view()[o] == obj_census(old(store), o));
            }
        }
        // caps_consistent: every object view is framed equal to `old`, so each live cap's
        // (refs-free) consistency carries over.
        assert forall|s: SlotId| #![trigger final(store).slot_view()[s]]
            final(store).slot_view().dom().contains(s)
                && !is_empty_cap(final(store).slot_view()[s].cap)
            implies cap_consistent(final(store), final(store).slot_view()[s].cap) by {
            assert(cap_consistent(old(store), old(store).slot_view()[s].cap));
        }
        // census_dom_complete: census framed; the domain only grew the count at `a` (present).
        assert forall|o: ObjId| #[trigger] obj_census(final(store), o) >= 1
            implies final(store).refs_view().dom().contains(o) by {
            assert(obj_census(old(store), o) >= 1);
        }
    }
}

/// **Map-time cap-side record** (rev2§6.1(c)) — [`delete`]'s frame-unmap branch run
/// backwards. Records the mapping on a previously-unmapped frame cap and bumps the target
/// aspace's refcount, driving the page-table write through the [`Store::aspace_map`] seam
/// exactly as `delete` drives the unmap through `aspace_unmap`. This makes the cap-side mapping
/// guarantee **symmetric**: `derive` proves a derived copy starts unmapped (`derived_kind`
/// clears `mapping`), `map_frame` proves record-on-map, `delete` proves clear-on-unmap. The
/// page-table write itself stays the trusted join (the `aspace_map` realization), exactly as
/// `aspace_unmap`'s is.
///
/// On `Err` (the page-table map failed — pool exhausted / already mapped) nothing is recorded
/// and the store is unchanged: `aspace_map` frames every view, and neither the cap-side record
/// nor the ref bump has run.
pub fn map_frame<S: Store>(store: &mut S, frame_slot: SlotId, asp: ObjId, va: u64, perms: u64)
    -> (res: Result<(), crate::aspace::MapError>)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(frame_slot),
        old(store).slot_view()[frame_slot].cap.kind matches CapKind::Frame { mapping: None, .. },
        old(store).refs_view().dom().contains(asp),
        old(store).refs_view()[asp] < u32::MAX as nat,
        refcount_sound(old(store)),
        caps_consistent(old(store)),
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
    ensures
        old(store).slot_view()[frame_slot].cap.kind matches CapKind::Frame { base, pages, .. } ==> (
            res is Ok ==> {
                &&& final(store).slot_view()[frame_slot].cap == (Cap {
                        kind: CapKind::Frame { base, pages, mapping: Some((asp, va)) },
                        rights: old(store).slot_view()[frame_slot].cap.rights,
                    })
                &&& final(store).slot_view().dom() == old(store).slot_view().dom()
                &&& (forall|x: SlotId| final(store).slot_view().dom().contains(x) && x != frame_slot
                        ==> #[trigger] final(store).slot_view()[x] == old(store).slot_view()[x])
                &&& final(store).refs_view() == old(store).refs_view().insert(
                        asp, (old(store).refs_view()[asp] + 1) as nat)
                &&& cspace_wf(final(store).slot_view())
                &&& refcount_sound(final(store))
                &&& caps_consistent(final(store))
                &&& end_caps_sound(final(store))
                &&& census_dom_complete(final(store))
                &&& final(store).chan_view() == old(store).chan_view()
                &&& final(store).notif_view() == old(store).notif_view()
                &&& final(store).tcb_view() == old(store).tcb_view()
                &&& final(store).timer_view() == old(store).timer_view()
                &&& final(store).timer_head_view() == old(store).timer_head_view()
                &&& final(store).cspace_view() == old(store).cspace_view()
                &&& final(store).irq_view() == old(store).irq_view()
            }),
        res is Err ==> {
            &&& final(store).slot_view() == old(store).slot_view()
            &&& final(store).refs_view() == old(store).refs_view()
            &&& final(store).chan_view() == old(store).chan_view()
            &&& final(store).notif_view() == old(store).notif_view()
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).timer_view() == old(store).timer_view()
            &&& final(store).timer_head_view() == old(store).timer_head_view()
            &&& final(store).cspace_view() == old(store).cspace_view()
            &&& final(store).irq_view() == old(store).irq_view()
        },
{
    let cs = store.slot(frame_slot);
    // PA + extent from the cap drive the page-table map; `(asp, va)` records on success.
    let (base, pages) = match cs.cap.kind {
        CapKind::Frame { base, pages, .. } => (base, pages),
        // Unreachable: the requires pins `frame_slot` to a Frame cap.
        _ => return Err(crate::aspace::MapError::BadVa),
    };
    match store.aspace_map(asp, base, va, pages, perms) {
        Err(e) => Err(e),
        Ok(()) => {
            let ghost st_pre = *store;
            let new = CapSlot {
                cap: Cap {
                    kind: CapKind::Frame { base, pages, mapping: Some((asp, va)) },
                    rights: cs.cap.rights,
                },
                ..cs
            };
            store.set_slot(frame_slot, new);
            proof {
                // `aspace_map` framed every view, so `st_pre` equals `old(store)` on every
                // census input — hence the census and `refcount_sound` carry across it.
                assert(st_pre.slot_view() == old(store).slot_view());
                assert(st_pre.refs_view() == old(store).refs_view());
                assert(st_pre.chan_view() == old(store).chan_view());
                assert(st_pre.notif_view() == old(store).notif_view());
                assert(st_pre.tcb_view() == old(store).tcb_view());
                assert(st_pre.timer_view() == old(store).timer_view());
                // `refs_view` is literally unchanged until `ref_aspace` (set_slot + aspace_map
                // both frame it), so refs-domain facts carry straight from `old(store)`.
                assert(store.refs_view() == old(store).refs_view());
                assert(refcount_sound(&st_pre)) by {
                    assert forall|o: ObjId| #[trigger] st_pre.refs_view().dom().contains(o)
                        implies st_pre.refs_view()[o] == obj_census(&st_pre, o) by {
                        // census reads only views, all equal to `old`'s ⇒ census equal there.
                        assert(obj_census(&st_pre, o) == obj_census(old(store), o));
                        assert(old(store).refs_view()[o] == obj_census(old(store), o));
                    }
                }
                // The recorded frame raises `obj_census(asp)` by one, nothing else.
                assert forall|x: ObjId| #[trigger] obj_census(store, x)
                    == obj_census(&st_pre, x) + (if x == asp { 1nat } else { 0nat }) by {
                    lemma_map_frame_census(&st_pre, store, frame_slot, new, x);
                }
                // The under-by-one census window at `asp`; sound elsewhere (refs framed).
                assert(st_pre.refs_view()[asp] == obj_census(&st_pre, asp));
                assert(store.refs_view()[asp] + 1 == obj_census(store, asp));
                assert(store.refs_view()[asp] < u32::MAX as nat);
                assert forall|o: ObjId| o != asp && store.refs_view().dom().contains(o)
                    implies #[trigger] store.refs_view()[o] == obj_census(store, o) by {
                    assert(st_pre.refs_view()[o] == obj_census(&st_pre, o));
                }
                // cspace_wf: only the cap kind changed; the CDT links are identical.
                lemma_local_cap_edit_preserves_cspace_wf(st_pre.slot_view(), frame_slot, new);
                // caps_consistent across the Frame→Frame edit.
                lemma_map_frame_caps_consistent(&st_pre, store, frame_slot, new);
                // end_caps_sound: no endpoint-cap filter moves (neither cap names an endpoint).
                assert forall|ch: ObjId, e: int|
                    store.chan_view().dom().contains(ch) && store.chan_view()[ch].end_caps.len() == 2
                        && 0 <= e < 2
                    implies #[trigger] store.chan_view()[ch].end_caps[e]
                        == end_cap_count(store.slot_view(), ch, e) by {
                    lemma_map_frame_end_cap(st_pre.slot_view(), frame_slot, new, ch, e);
                }
                // census_dom_complete: `asp` already in dom; every other census is framed, so
                // `census_dom_complete(old)` covers it (refs domain unchanged here).
                assert forall|o: ObjId| #[trigger] obj_census(store, o) >= 1
                    implies store.refs_view().dom().contains(o) by {
                    if o != asp {
                        assert(obj_census(old(store), o) == obj_census(&st_pre, o));
                        assert(old(store).refs_view().dom().contains(o));
                    }
                }
            }
            ref_aspace(store, asp);
            Ok(())
        }
    }
}

/// `delete`'s first half — `cdt_unlink` + clear the slot — split out so its (heavy) census/
/// `end_caps`/`caps_consistent` off-by-one proof is a self-contained SMT query, keeping
/// `delete`'s body (the teardown branches + `obj_unref`) under the rlimit (split
/// the query, don't bump the limit). Non-recursive, so it carries no `decreases`. Returns the
/// deleted cap; leaves the store in the off-by-one window the teardown branches consume:
/// the deleted designating slot is cleared (census/`end_caps` off by one at its object/aspace/
/// end), with `cspace_wf`/`caps_consistent`/`census_dom_complete` and every object view intact.
fn delete_prepare<S: Store>(store: &mut S, slot: SlotId) -> (cap: Cap)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
        refcount_sound(old(store)),
        caps_consistent(old(store)),
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
    ensures
        cap == old(store).slot_view()[slot].cap,
        !is_empty_cap(cap),
        cap_consistent(final(store), cap),
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        is_empty_cap(final(store).slot_view()[slot].cap),
        count_nonempty(final(store).slot_view()) < count_nonempty(old(store).slot_view()),
        final(store).refs_view() == old(store).refs_view(),
        final(store).chan_view() == old(store).chan_view(),
        final(store).notif_view() == old(store).notif_view(),
        final(store).tcb_view() == old(store).tcb_view(),
        final(store).timer_view() == old(store).timer_view(),
        final(store).timer_head_view() == old(store).timer_head_view(),
            final(store).ready_view() == old(store).ready_view(),
        final(store).cspace_view() == old(store).cspace_view(),
        final(store).irq_view() == old(store).irq_view(),
        forall|x: SlotId| final(store).slot_view().dom().contains(x) && x != slot
            ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap,
        caps_consistent(final(store)),
        census_dom_complete(final(store)),
        cap_obj(cap) matches Some(o) ==> census_off_by_one(final(store), o),
        (cap_obj(cap) is None && cap_frame_aspace(cap) is Some)
            ==> census_off_by_one(final(store), cap_frame_aspace(cap)->Some_0),
        (cap_obj(cap) is None && cap_frame_aspace(cap) is None) ==> refcount_sound(final(store)),
        cap_chan_end(cap) is Some ==> end_caps_off_by_one(
            final(store),
            cap_chan_end(cap)->Some_0.0,
            cap_chan_end(cap)->Some_0.1,
        ),
        cap_chan_end(cap) is None ==> end_caps_sound(final(store)),
{
    let cap = store.slot(slot).cap;
    let ghost o_opt = cap_obj(cap);
    let ghost asp_opt = cap_frame_aspace(cap);
    proof {
        assert(cap_consistent(old(store), cap));
    }
    cdt_unlink(store, slot);
    proof {
        // `cdt_unlink` frames `refs` + every object view and preserves every cap, so the
        // invariants ride through and the deleted cap is still at `slot`.
        assert(store.slot_view()[slot].cap == cap);
        assert forall|x: ObjId| #[trigger] obj_census(store, x) == obj_census(old(store), x) by {
            lemma_same_caps_same_census(old(store).slot_view(), store.slot_view(), x);
            lemma_same_caps_same_frame_map(old(store).slot_view(), store.slot_view(), x);
        }
        assert(refcount_sound(store));
        assert forall|s2: SlotId| #![trigger store.slot_view()[s2]]
            store.slot_view().dom().contains(s2) && !is_empty_cap(store.slot_view()[s2].cap)
            implies cap_consistent(store, store.slot_view()[s2].cap) by {
            assert(cap_consistent(old(store), old(store).slot_view()[s2].cap));
        }
        assert(caps_consistent(store));
        assert forall|ch2: ObjId, e2: int|
            store.chan_view().dom().contains(ch2) && store.chan_view()[ch2].end_caps.len() == 2
                && 0 <= e2 < 2 implies #[trigger] store.chan_view()[ch2].end_caps[e2]
                == end_cap_count(store.slot_view(), ch2, e2) by {
            lemma_same_caps_same_end_cap(old(store).slot_view(), store.slot_view(), ch2, e2);
        }
        assert(end_caps_sound(store));
        assert(census_dom_complete(store));
    }
    let ghost sv1 = store.slot_view();
    let es = CapSlot::empty();
    store.set_slot(slot, es);
    proof {
        // The slot clear. count drops by one; the census drops by one at `o`/`asp` (slot terms
        // vs `sv1` by `lemma_clear_slot_census`, `sv1`-vs-`old` by the caps-preserved lemmas;
        // the four non-slot terms are framed); `end_cap_count` likewise drops at `(co, e0)`.
        lemma_clear_drops_count(sv1, slot, es);
        assert(store.slot_view() == sv1.insert(slot, es));
        // `cdt_unlink` left `slot` detached and `cspace_wf(sv1)`; clearing a detached slot to
        // an empty cap preserves it, and drops the live-slot count by one (cdt_unlink kept it).
        lemma_clear_detached_preserves_cspace_wf(sv1, slot, es);
        // `slot` is non-empty in `sv1`, so the live-slot count is ≥ 1 — the clear strictly
        // drops it (the count filter loses exactly `slot`).
        assert(count_nonempty(sv1) >= 1) by {
            let f = sv1.dom().filter(|j: SlotId| !is_empty_cap(sv1[j].cap));
            assert(f.contains(slot));
            assert(f.finite());
            if f.len() == 0 {
                assert(f =~= Set::empty());
            }
        }
        assert(count_nonempty(sv1) == count_nonempty(old(store).slot_view()));
        assert(count_nonempty(store.slot_view()) == (count_nonempty(sv1) - 1) as nat);
        assert(count_nonempty(store.slot_view()) < count_nonempty(old(store).slot_view()));
        // The cleared slot still names `cap` (cdt_unlink framed it), and a cap is either an
        // object cap or a frame cap, never both — so the slot-clear drops the census by exactly
        // one at a *single* object (its `cap_obj`, else its `cap_frame_aspace`).
        assert(sv1[slot].cap == cap);
        assert(o_opt is None || asp_opt is None);
        assert(store.refs_view() == old(store).refs_view());
        assert(store.chan_view() == old(store).chan_view());
        assert(store.notif_view() == old(store).notif_view());
        assert(store.tcb_view() == old(store).tcb_view());
        assert(store.timer_view() == old(store).timer_view());
        assert forall|x: ObjId| #[trigger] obj_census(old(store), x)
            == obj_census(store, x) + (if o_opt == Some(x) || asp_opt == Some(x) {
                1nat
            } else {
                0nat
            }) by {
            lemma_clear_slot_obj_census(old(store), store, sv1, slot, es, cap, x);
        }
        assert forall|ch2: ObjId, e2: int|
            store.chan_view().dom().contains(ch2) && store.chan_view()[ch2].end_caps.len() == 2
                && 0 <= e2 < 2 implies #[trigger] store.chan_view()[ch2].end_caps[e2]
                == end_cap_count(store.slot_view(), ch2, e2) + (if cap_chan_end(cap) == Some(
                (ch2, e2),
            ) {
                1nat
            } else {
                0nat
            }) by {
            lemma_clear_slot_end_cap(sv1, slot, es, ch2, e2);
            lemma_same_caps_same_end_cap(old(store).slot_view(), sv1, ch2, e2);
        }
        assert forall|s2: SlotId| #![trigger store.slot_view()[s2]]
            store.slot_view().dom().contains(s2) && !is_empty_cap(store.slot_view()[s2].cap)
            implies cap_consistent(store, store.slot_view()[s2].cap) by {
            assert(s2 != slot);
            assert(store.slot_view()[s2] == sv1[s2]);
        }
        assert(caps_consistent(store));
        // The deleted cap's object well-formedness rides through (its arms read framed object
        // views; `chan_wf` survives any slot clear — it requires no slot to be non-empty).
        assert(cap_consistent(store, cap)) by {
            assert(cap_consistent(old(store), cap));
        }
        // census_dom_complete: refs domain unchanged, census only dropped ⇒ coverage carries.
        assert forall|x: ObjId| #[trigger] obj_census(store, x) >= 1
            implies store.refs_view().dom().contains(x) by {
            // census(old,x) == census(store,x) + δ ≥ census(store,x) ≥ 1, then dom_complete(old).
            assert(obj_census(old(store), x) >= 1);
        }
        // The off-by-one census / end_caps at the deleted cap's object/aspace/end. `refs` is
        // framed (== old), `refcount_sound(old)` pins each `refs[x]`, and the additive census
        // delta moves the deleted designation's one unit at `o`/`asp` and nothing elsewhere.
        if let Some(o) = o_opt {
            // δ at `o` is 1 (the deleted cap designates `o`); δ is 0 off `o` (mutual exclusion
            // makes `asp_opt` None, and `o_opt == Some(o) != Some(x)` for `x != o`).
            assert(obj_census(old(store), o) == obj_census(store, o) + 1);
            lemma_in_refs_from_census(old(store), o);
            assert forall|x: ObjId| x != o && store.refs_view().dom().contains(x)
                implies store.refs_view()[x] == obj_census(store, x) by {
                assert(obj_census(old(store), x) == obj_census(store, x));
            }
            assert(census_off_by_one(store, o));
        } else if let Some(asp) = asp_opt {
            assert(obj_census(old(store), asp) == obj_census(store, asp) + 1);
            lemma_in_refs_from_census(old(store), asp);
            assert forall|x: ObjId| x != asp && store.refs_view().dom().contains(x)
                implies store.refs_view()[x] == obj_census(store, x) by {
                assert(obj_census(old(store), x) == obj_census(store, x));
            }
            assert(census_off_by_one(store, asp));
        } else {
            // Neither designation present ⇒ δ is 0 everywhere ⇒ full soundness carries.
            assert forall|x: ObjId| store.refs_view().dom().contains(x)
                implies #[trigger] store.refs_view()[x] == obj_census(store, x) by {
                assert(obj_census(old(store), x) == obj_census(store, x));
            }
            assert(refcount_sound(store));
        }
    }
    cap
}

/// Delete one cap (children survive, re-parented one level up).
///
/// **Proven body.** The real teardown — `delete_prepare` (`cdt_unlink` + clear)
/// → per-end channel `peer_closed` → frame unmap → `obj_unref` — is verified
/// against the full contract (`cspace_wf`/`refcount_sound`/`caps_consistent`/
/// `end_caps_sound`/`census_dom_complete`/the `count_nonempty` drop/residency
/// frame). The cross-object recursion `delete → obj_unref → destroy_cspace →
/// delete` (the seL4-zombie cycle) terminates under the shared lexicographic
/// measure `(count_nonempty(slot_view), height)` with `delete = 0` (`delete`'s
/// `delete_prepare` empties its slot before recursing, the one count-dropping
/// edge). `obj_unref`'s Channel/Thread arms recurse through the proven
/// `destroy_channel`/`destroy_tcb` bodies, which carry the thread-state /
/// binding-release census invariants.
///
/// The contract is stated for the **general** (possibly non-leaf) case on
/// purpose: `revoke` only ever passes a leaf, but `destroy_cspace` deletes
/// non-leaf residents, so the body handles `cdt_unlink`'s re-parenting — the
/// harder `cdt_wf`-preservation case.
///
/// **Refcount census.** The deleted cap lowers exactly its object's
/// `slot_refs`/`frame_map_refs`, matched by the `obj_unref`/`unref_aspace` `-1`
/// (and the per-end `endpoint_cap_dropped` by its `binding_refs` drop); the
/// `requires` census is the underflow gate (`refs[o] - 1` needs `refs[o] ≥ 1`).
/// The verified callers (`bind`, `revoke`, `destroy_cspace`) carry the four
/// system invariants in.
pub fn delete<S: Store>(store: &mut S, slot: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
        refcount_sound(old(store)),
        // Cap→object consistency: the body's `endpoint_cap_dropped`
        // and `obj_unref` calls need the deleted cap's object well-formed, which only this
        // system invariant supplies (discharged by the now-proven body). The verified callers
        // (`bind`, `revoke`, `destroy_cspace`) carry it like 6a's `refcount_sound`.
        caps_consistent(old(store)),
        // The rev2§3.3 endpoint-cap census (the body-removal census gate): the body's
        // Channel branch deletes one of possibly several `(co, end)` caps; this equality is
        // what lets it re-prove `caps_consistent`'s `end_caps[end] > 0` for the surviving
        // siblings. The verified callers carry it like `refcount_sound`.
        end_caps_sound(old(store)),
        // Refs-domain completeness: the body reads the deleted cap's
        // object into `refs.dom()` from the census via it (`delete_prepare` + `obj_unref`).
        // The verified callers carry it.
        census_dom_complete(old(store)),
        // The teardown can fire a peer-closed notification (Channel branch →
        // `endpoint_cap_dropped` → `fire`) or tear down a thread (→ `destroy_tcb` → `unqueue_ready`),
        // both of which touch the ready queue, so `delete` carries the ready-queue invariants.
        // The verified callers (`bind`/`revoke`/`destroy_cspace`/`destroy_channel`) supply them.
        ready_wf(old(store).ready_view(), old(store).tcb_view()),
        ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        is_empty_cap(final(store).slot_view()[slot].cap),
        count_nonempty(final(store).slot_view()) < count_nonempty(old(store).slot_view()),
        refcount_sound(final(store)),
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        census_dom_complete(final(store)),
        ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ready_complete(final(store).ready_view(), final(store).tcb_view()),
        // Teardown only empties slots, never fills one: the just-cleared `slot`
        // and every already-empty slot stay empty through the recursive `obj_unref`. The
        // frame `destroy_channel`'s ring-cap loop carries; host-checked (`check_delete`).
        only_empties(old(store).slot_view(), final(store).slot_view()),
        // Residency is immutable (the kernel fixes it at construction; every internal
        // mutator frames `cspace_view`, swept in 6a) — `delete` re-parents CDT links and
        // clears caps but never reassigns which slots a cspace owns. `delete_prepare` frames
        // it, the teardown branches frame it, and `obj_unref` frames it unconditionally (each
        // arm); `destroy_cspace`'s resident loop (6c) reads `cspace_view[cs]` across its
        // `delete` calls, so the frame is load-bearing there.
        final(store).cspace_view() == old(store).cspace_view(),
        // Note: `delete` does NOT frame `irq_view` — tearing down an `Irq` cap runs
        // `destroy_irq` (via `obj_unref`), which mutates it (the `timer_view` precedent).
        // The channel skeleton (`ring_cap`/`depth`/dom) is immutable across teardown: the
        // recursive `obj_unref` only clears bindings / drops `end_caps` / deletes ring caps,
        // never re-homing a channel or moving its slot handles. This is the frame
        // `destroy_channel`'s ring-cap loop reads off to keep `old.ring_cap[ch]` valid across
        // its `delete`s.
        chan_struct_frame(old(store).chan_view(), final(store).chan_view()),
        // Dead, queue-detached TCBs are frozen across the teardown:
        // `delete_prepare` frames `tcb`/`refs` whole, the teardown branches touch only `chan`/
        // `aspace`/`refs[asp]` (a dead `x ≠ asp`, since a mapping made `refs[asp] > 0`) or fire a
        // peer-closed notification (which wakes only `wait_notif`-bearing waiters, never a
        // `wait_notif is None` thread), and `obj_unref` carries it. This is the frame
        // `destroy_tcb` reads off to preserve its halted subject's `report`/`state`/`qnext`
        // across the recursive `unref_cspace`/`destroy_cspace`.
        dead_tcb_frozen(old(store), final(store)),
        // The home maps are framed — residency immutable, channel skeleton fixed, TCB
        // domain + every `bind_slots` preserved across the teardown.
        home_views_frozen(old(store), final(store)),
        // Home-frame provenance: `delete` empties its own `slot` plus the homed slots `obj_unref`'s
        // destructors clear (and their recursive closure), so every **un-homed** slot other than
        // `slot` keeps its cap. This is the frame `revoke` reads off for root-survival, and the
        // recursive destructors compose to their own target-free frame.
        unhomed_frozen(old(store), final(store), slot),
        // Death-provenance frame: every emptied slot *other than* the directly-deleted `slot`
        // was a home handle of a dead object (the object `obj_unref`'s destructors tore down). The
        // direct `slot` is exempt (it is cleared by the CDT delete, not a home-handle teardown).
        // `revoke` reads this off for the faithful resident-with-external-reference theorem.
        emptied_via_dead_home(old(store), final(store), slot),
        // "Dead stays dead" across the whole teardown (its `obj_unref` only decrements/removes
        // objects) — the refs-monotone fact the death-provenance composition needs.
        refs_death_persist(old(store), final(store)),
        // Conditional-on-notification frame: deleting a **notification**
        // cap is robustly clean — `delete_prepare` frames every object view, the
        // `Channel`/mapped-`Frame` teardown branches don't fire, and `obj_unref`'s
        // Notification arm only drops `refs[n]` (and at zero calls the view no-op
        // `destroy_notif`, its `cap_notif` ensure). So the object views and every
        // *other* slot's cap are untouched. This is the additive enabling clause
        // `thread::bind` reads off (the displaced bind cap is always a notification);
        // host-test-checked (`check_delete_notif`). `refs_view` is deliberately left
        // out — the `refs[n] -= 1` rides the host test, not `bind`'s verified contract.
        cap_notif(old(store).slot_view()[slot].cap) is Some ==> {
            &&& final(store).tcb_view() == old(store).tcb_view()
            &&& final(store).chan_view() == old(store).chan_view()
            &&& final(store).notif_view() == old(store).notif_view()
            &&& final(store).timer_view() == old(store).timer_view()
            &&& final(store).timer_head_view() == old(store).timer_head_view()
            &&& final(store).irq_view() == old(store).irq_view()
            &&& forall|x: SlotId| old(store).slot_view().dom().contains(x) && x != slot
                    ==> #[trigger] final(store).slot_view()[x].cap == old(store).slot_view()[x].cap
        },
    // SCC measure: `delete` is the bottom of the height order — the only
    // edge that drops `count_nonempty` (it empties its slot before recursing into `obj_unref`).
    decreases count_nonempty(old(store).slot_view()), 0int
{
    let ghost cv0 = store.chan_view();
    let ghost st0 = *store;
    // `cdt_unlink` + clear the slot, leaving the off-by-one census/`end_caps` window the
    // teardown branches consume (its proof is isolated in `delete_prepare`).
    let cap = delete_prepare(store, slot);
    let ghost o_opt = cap_obj(cap);
    let ghost asp_opt = cap_frame_aspace(cap);
    // `delete_prepare` framed `refs` + `tcb` whole, so it is dead-tcb-frozen
    // — the base the teardown branches + `obj_unref` compose onto.
    proof {
        lemma_dead_tcb_frozen_refl(&st0, store);
        // `delete_prepare` frames `ready_view` + `tcb_view`, so the ready pair carries.
        lemma_ready_inv_frame(&st0, store);
    }
    let ghost st_prep = *store;
    // Channel endpoint liveness is tracked per end for peer-closed (rev2§3.3).
    if let CapKind::Channel(ch, end) = cap.kind {
        proof {
            assert(cap_consistent(old(store), cap));
            let peer = 1 - crate::channel::end_idx_spec(end);
            if store.chan_view()[ch].bindings[(peer, crate::channel::EV_PEER_CLOSED as int)].notif
                is Some {
                let m = store.chan_view()[ch].bindings[(peer,
                    crate::channel::EV_PEER_CLOSED as int)].notif->Some_0;
                if store.notif_view()[m].wait_head is Some {
                    lemma_waiter_refs_pos_from_head(store.notif_view(), store.tcb_view(), m);
                    // A queued waiter makes `census(m) >= 1`, so refs-domain completeness
                    // (delete_prepare's `ensures`) places `m` in `refs.dom()`; the off-by-one
                    // then makes `refs[m] > 0` — `endpoint_cap_dropped`'s `binding_refs_ok`.
                    assert(obj_census(store, m) >= 1);
                    lemma_in_refs_from_census(store, m);
                    lemma_refs_pos_from_off_by_one(store, o_opt->Some_0, m);
                }
            }
        }
        crate::channel::endpoint_cap_dropped(store, ch, end);
        // `endpoint_cap_dropped` is dead-tcb-frozen + death-preserving (st_prep → here).
        // It also re-establishes the ready pair (its own ensures).
    } else {
        proof {
            lemma_dead_tcb_frozen_refl(&st_prep, store);
            lemma_refs_death_persist_from_refs_eq(&st_prep, store);
            // The non-Channel branch is object-only; the ready pair carries.
            lemma_ready_inv_frame(&st_prep, store);
        }
    }
    let ghost st_chan = *store;
    // Deleting a mapped frame cap unmaps it — the one revocation story
    // for shared memory (rev2§2.5).
    if let CapKind::Frame { pages, mapping: Some((asp, va)), .. } = cap.kind {
        let ghost s_pre = *store;
        store.aspace_unmap(asp, va, pages);
        let ghost st_unmap = *store;
        proof {
            // `aspace_unmap` frames every census view (it edits page tables, not caps/refs),
            // so the off-by-one window at `asp` and the refs-domain coverage ride through
            // unchanged from delete_prepare to `unref_aspace`.
            assert forall|x: ObjId| #[trigger] obj_census(store, x) == obj_census(&s_pre, x) by {}
            assert(census_off_by_one(store, asp));
            assert(census_dom_complete(store));
            // `aspace_unmap` frames `refs` + `tcb` whole (page-table maintenance), so
            // `st_chan → st_unmap` is dead-tcb-frozen.
            lemma_dead_tcb_frozen_refl(&st_chan, store);
        }
        unref_aspace(store, asp);
        proof {
            // `unref_aspace` frames `tcb` whole and drops `refs` only at `asp`; the off-by-one
            // makes `refs[asp] > 0`, so a dead `x != asp` is frozen. Compose with `aspace_unmap`.
            assert(st_unmap.refs_view()[asp] > 0);
            assert forall|x: ObjId| #[trigger] dead_tcb_frozen_at(&st_unmap, store, x) by {
                if st_unmap.tcb_view().dom().contains(x) && st_unmap.refs_view().dom().contains(x)
                    && st_unmap.refs_view()[x] == 0 && st_unmap.tcb_view()[x].wait_notif is None {
                    assert(x != asp);
                }
            }
            lemma_dead_tcb_frozen_trans(&st_chan, &st_unmap, store);
            // Death-provenance: `aspace_unmap` frames `refs` (st_chan → st_unmap refl); `unref_aspace`
            // exports `refs_death_persist`; compose `st_chan → st_unmap → final`.
            lemma_refs_death_persist_from_refs_eq(&st_chan, &st_unmap);
            lemma_refs_death_persist_trans(&st_chan, &st_unmap, store);
            // `aspace_unmap` + `unref_aspace` frame `ready_view` + `tcb_view`, so the pair carries.
            lemma_ready_inv_frame(&st_chan, store);
        }
    } else {
        proof {
            lemma_dead_tcb_frozen_refl(&st_chan, store);
            lemma_refs_death_persist_from_refs_eq(&st_chan, store);
            // The non-mapped-Frame branch is object-only; the ready pair carries.
            lemma_ready_inv_frame(&st_chan, store);
        }
    }
    let ghost st_frame = *store;
    proof {
        // Compose the dead-tcb-frozen segments so far: prepare → channel → frame-unmap.
        lemma_dead_tcb_frozen_trans(&st0, &st_prep, &st_chan);
        lemma_dead_tcb_frozen_trans(&st0, &st_chan, &st_frame);
        // Death-provenance: compose the death-persist segments: prepare (`refs` framed) → channel →
        // frame-unmap, giving `refs_death_persist(st0, st_frame)`.
        lemma_refs_death_persist_from_refs_eq(&st0, &st_prep);
        lemma_refs_death_persist_trans(&st0, &st_prep, &st_chan);
        lemma_refs_death_persist_trans(&st0, &st_chan, &st_frame);
    }
    proof {
        // The channel skeleton is preserved up to here: `delete_prepare` framed `chan_view`;
        // a Channel cap's `endpoint_cap_dropped` carries `chan_struct_frame` (its own ensures);
        // a mapped Frame's `aspace_unmap`/`unref_aspace` framed `chan_view`; any other cap left
        // it equal to `cv0`.
        assert(chan_struct_frame(cv0, store.chan_view()));
    }
    let ghost cv_pre_unref = store.chan_view();
    proof {
        // Discharge `obj_unref`'s Thread arm's bound-cspace-resident precondition from
        // `caps_consistent`. The deleted cap is the live Thread cap, so
        // `caps_consistent(old)` ⟹ `cap_consistent(old, cap)` ⟹ the bound cspace is
        // resident-wf. `cspace_resident_wf` reads only `cspace_view` + `slot_view` dom (both
        // framed by `delete_prepare`) and `tcb_view[o].cspace` (framed), and no Thread-cap
        // teardown branch ran above, so it survives unchanged to the `obj_unref` call.
        if let CapKind::Thread(o, _) = cap.kind {
            assert(cap_consistent(old(store), cap));
            if let Some(cs) = store.tcb_view()[o].cspace {
                assert(old(store).tcb_view()[o].cspace == Some(cs));
                assert(cspace_resident_wf(old(store), cs));
                assert(cspace_resident_wf(store, cs));
            }
            // Waiter-coherence rides through identically (`delete_prepare` frames `notif_view`
            // and `tcb_view`, no Thread-cap teardown branch ran): if `o` is blocked, its
            // `wait_notif` names a `notif_wf` notification — `obj_unref`'s Thread arm needs it
            // to discharge `destroy_tcb`'s BlockedNotif-branch `remove_waiter`.
            if store.tcb_view()[o].state == ThreadState::BlockedNotif {
                if let Some(wn) = store.tcb_view()[o].wait_notif {
                    assert(notif_wf(old(store).notif_view(), old(store).tcb_view(), wn));
                    assert(notif_wf(store.notif_view(), store.tcb_view(), wn));
                }
            }
        }
        // Re-establish `obj_unref`'s Channel-arm `chan_wf` precondition **deterministically**
        // (the final-thread teardown hazard — a plain `assert` here relied on
        // auto-derivation that flakes CI's Z3 once the strengthened `cap_consistent` widened the
        // context). `cap_consistent(old, cap)` gives `chan_wf(cv0, old.slot_view, o)`;
        // `delete_prepare` only emptied the deleted cap's slot and `endpoint_cap_dropped` only
        // decremented `end_caps[end]` (a count, not `.len()`) — exactly `lemma_chan_wf_frame`'s
        // window. The other arms left `chan_view`/`slot_view` framed.
        if let CapKind::Channel(o, _) = cap.kind {
            assert(cap_consistent(old(store), cap));
            assert(chan_wf(cv0, old(store).slot_view(), o));
            assert forall|s: SlotId| #[trigger] store.slot_view().dom().contains(s) implies
                store.slot_view()[s].cap == old(store).slot_view()[s].cap
                    || is_empty_cap(store.slot_view()[s].cap) by {}
            lemma_chan_wf_frame(cv0, store.chan_view(), old(store).slot_view(),
                store.slot_view(), o);
        }
        // Discharge `obj_unref`'s Timer armed-notif-live precondition from the census.
        if let CapKind::Timer(o) = cap.kind {
            if store.timer_view()[o].armed {
                if let Some(nn) = store.timer_view()[o].notif {
                    lemma_armed_timer_refs_pos(store.timer_view(), o, nn);
                    // The armed binding makes `census(nn) >= 1`, so refs-domain completeness
                    // places `nn` in `refs.dom()`; the off-by-one then makes `refs[nn] > 0`.
                    assert(obj_census(store, nn) >= 1);
                    lemma_in_refs_from_census(store, nn);
                    lemma_refs_pos_from_off_by_one(store, o, nn);
                }
            }
        }
        // Discharge `obj_unref`'s Irq bound-notif-live precondition from the census (the
        // Timer block's twin: a bound IRQ makes `irq_binding_refs(nn) >= 1`, hence `census(nn) >= 1`).
        if let CapKind::Irq(o) = cap.kind {
            if store.irq_view()[o].bound {
                if let Some(nn) = store.irq_view()[o].notif {
                    lemma_irq_binding_refs_pos(store.irq_view(), o, nn);
                    assert(obj_census(store, nn) >= 1);
                    lemma_in_refs_from_census(store, nn);
                    lemma_refs_pos_from_off_by_one(store, o, nn);
                }
            }
        }
    }
    obj_unref(store, cap);
    proof {
        // `obj_unref` preserves the skeleton; compose with the pre-unref preservation.
        lemma_chan_struct_frame_trans(cv0, cv_pre_unref, store.chan_view());
        // dead_tcb_frozen: compose the pre-`obj_unref` segments (`st0 → st_frame`) with
        // `obj_unref`'s own frame (`st_frame → final`). Plan the final-thread teardown
        lemma_dead_tcb_frozen_trans(&st0, &st_frame, store);
        // Home-frame provenance + home frame. Up to `obj_unref` only `slot` changed in the arena
        // (`delete_prepare` cleared it; `endpoint_cap_dropped`/`aspace_unmap`/`unref_aspace`
        // frame `slot_view`), and the home maps are framed (`cspace_view`/`chan_struct_frame`/the
        // TCB domain + `bind_slots`). `obj_unref` exports the target-free frame, composed onto `slot`.
        assert(home_views_frozen(&st0, &st_frame)) by {
            assert(st_frame.cspace_view() == st0.cspace_view());
            assert(chan_struct_frame(st0.chan_view(), st_frame.chan_view()));
            assert(st_frame.tcb_view().dom() == st0.tcb_view().dom());
            assert forall|k: ObjId| #[trigger] st_frame.tcb_view()[k].bind_slots
                == st0.tcb_view()[k].bind_slots by {}
        }
        assert(unhomed_frozen(&st0, &st_frame, slot)) by {
            assert forall|x: SlotId|
                st_frame.slot_view().dom().contains(x) && x != slot && !is_homed(&st0, x)
                implies #[trigger] st_frame.slot_view()[x].cap == st0.slot_view()[x].cap by {}
        }
        lemma_unhomed_frozen_compose(&st0, &st_frame, store, slot);
        lemma_home_views_frozen_trans(&st0, &st_frame, store);
        // Death-provenance: up to `st_frame` only the target `slot` was cleared (the teardown branches
        // frame `slot_view`), so `emptied_via_dead_home(st0, st_frame, slot)` holds vacuously (no
        // *non-target* slot is emptied). Compose with `obj_unref`'s exported free frame; the
        // death-persist segments above bridge `st0 → st_frame`, and `obj_unref` carries
        // `st_frame → final`.
        assert(emptied_via_dead_home(&st0, &st_frame, slot)) by {
            assert forall|x: SlotId|
                st_frame.slot_view().dom().contains(x) && x != slot
                    && !is_empty_cap(st0.slot_view()[x].cap)
                    && is_empty_cap(#[trigger] st_frame.slot_view()[x].cap)
                implies exists|o: ObjId| homes(&st0, o, x) && dead_obj(&st_frame, o) by {
                assert(st_frame.slot_view()[x].cap == st0.slot_view()[x].cap);
            }
        }
        lemma_emptied_via_dead_home_compose(&st0, &st_frame, store, slot);
        lemma_refs_death_persist_trans(&st0, &st_frame, store);
    }
}

/// Descend from `start` to a leaf of its subtree (the inner walk of `revoke`).
///
/// **Terminates** (`decreases prank[leaf]`): each step follows `first_child`,
/// and the child's parent rank is strictly below the leaf's (acyclicity) —
/// proven unbounded for all tree shapes by *using* the acyclicity witness.
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
/// in-flight queue slots alike, unconditionally (rev2§2.2).
///
/// **Terminates** (`decreases count_nonempty`): each iteration descends to a
/// leaf and deletes it, and `delete` strictly lowers the live-slot count — the
/// revocation-walk termination, proven here for all shapes against `delete`'s
/// teardown contract (above).
///
/// **Descendant-deletion is unconditional — and reachable from the real call path.** The
/// `ensures` `first_child is None` + `cspace_wf` hold for *any* live `slot`, with **no homing
/// precondition**: they ride `delete`'s unconditional teardown contract and the loop
/// `decreases`, independent of whether `slot` is some object's home handle. This is the
/// rev2§2.2 spec guarantee ("deletes all descendants … unconditional"), provable for the
/// `homed_in_cspace` target every `Sys::CapRevoke` actually supplies (`cur_slot` is a cell of
/// the caller's cspace) — not only for un-homed inputs the kernel never sends.
///
/// **Root-survival is conditional (an implication).** `revoke` deletes only descendants of
/// `slot`, never `slot` itself, so the *only* way `slot`'s cap can be emptied is the
/// **cross-object** teardown: deleting the last cap to some object that **homes** `slot` (a
/// cspace whose resident is `slot`, a channel whose ring cap is `slot`, a TCB whose bind slot
/// is `slot`) fires that object's destructor, which clears `slot`. The provenance frame
/// `unhomed_frozen` makes the contrapositive a theorem: a teardown clears a non-target slot
/// only if it is **homed**, so when `slot` is un-homed (a top-level / donated-untyped cap) no
/// cross-object teardown can reach it and `slot` survives — exported as the `ensures
/// !is_homed(old(store), slot) ==> !is_empty_cap(final …[slot].cap)`. A **homed** root is the
/// seL4-zombie (`revoke_can_empty_its_own_root_zombie`): `slot` is a resident of a cspace whose
/// last live cap lies in `slot`'s own subtree, so revoking the subtree destroys the cspace and
/// empties `slot` — the implication's antecedent is false there, so the contract admissibly
/// says nothing. Host-checked both ways (`check_revoke_root_survives` / the zombie negative
/// witness).
///
/// **Residue (the resident-with-external-reference case).** A `slot` that *is* a cspace
/// resident but whose homing cspace keeps a live reference outside `slot`'s subtree also
/// survives, but proving *that* needs the stronger "emptied ⟹ a homing object was destroyed"
/// frame (refs-monotone, the refcount cascade). `unhomed_frozen` is its foundation.
///
/// **Sees through queues — a named obligation.** rev2§3.4 / the M1 exit criterion demand that
/// `revoke` destroy descendants *including a cap queued in an in-flight message* "like any
/// other descendant, no special case." It is exported: `ensures no_live_descendant(final, slot)`
/// (no live slot — resident or in-flight ring cap — is a CDT descendant of `slot` afterward),
/// discharged at loop exit by `lemma_childless_no_descendant` from `first_child is None` +
/// `cspace_wf`, paired with `only_empties(old, final)` (the walk destroys, never relabels). A
/// queued cap is a genuine descendant because `slot_move` (what `send` uses) inherits the parent
/// edge into the ring slot; the real-op witness that such an in-flight queued cap is *emptied*
/// by the real `revoke` is `revoke_sees_through_queued_descendant`. The fully ∀-quantified
/// "every slot that was a descendant in the *initial* state is empty in the final state" is the
/// remaining follow-on — it is entangled with the cross-object teardown cascade (`delete` is
/// recursive and exports no per-slot parent-edge frame, so it would need the deferred subtree
/// induction), and is **not** a regression risk given the two facts above.
///
/// pre: the cspace is well-formed (and finite); `slot` is live and non-empty.
/// post: `slot` has no children (its subtree is gone) and the cspace stays well-formed
/// (unconditional); its cap **survives** when `slot` started **un-homed** (conditional).
pub fn revoke<S: Store>(store: &mut S, slot: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
        refcount_sound(old(store)),
        caps_consistent(old(store)),
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
        // The subtree `delete`s can fire / tear down threads, touching the ready queue.
        ready_wf(old(store).ready_view(), old(store).tcb_view()),
        ready_complete(old(store).ready_view(), old(store).tcb_view()),
    ensures
        // Descendant-deletion and well-formedness are **unconditional**: they
        // hold for *any* target, including the `homed_in_cspace` slot every `Sys::CapRevoke`
        // supplies — so the spec-mandated guarantee (rev2§2.2) is reachable from the real call path.
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom().contains(slot),
        final(store).slot_view()[slot].first_child is None,
        // Root-survival is **conditional** on the target being un-homed (the home-frame headline,
        // expressed as this implication rather than a whole-function precondition):
        // an un-homed (e.g. donated-untyped) root survives the cross-object teardown, while a
        // homed root may be self-emptied (the seL4-zombie case), which this `ensures` admissibly
        // leaves unconstrained.
        !is_homed(old(store), slot) ==> !is_empty_cap(final(store).slot_view()[slot].cap),
        // **The faithful resident-with-external-reference theorem.** If the revoked root `slot`
        // *was* emptied, then some object that **homed** it in the initial state was **destroyed**
        // — `dead_obj`: gone from `refs.dom()` (the aspace seam) *or* sitting there at `refs == 0`
        // (every cspace/channel/TCB destructor, which leaves its object in the domain — see
        // `dead_obj`; `o ∉ refs.dom()` alone is unprovable for these, the *dominant* homing case, so
        // the predicate is the sound disjunction). `revoke`
        // deletes only descendants — never `slot` itself (`slot != leaf` each step) — so the only
        // way `slot` empties is the cross-object teardown of a homing object whose last cap lay in
        // the revoked subtree. Caller-completable: a caller who knows all of `slot`'s homing objects
        // keep a live external reference (so none is `dead_obj`) concludes `slot` **survives** by
        // contraposition. We deliberately do no CDT-subtree reasoning inside `revoke` (there is no
        // subtree predicate, by design); the witness is the initial-state home + its destruction.
        is_empty_cap(final(store).slot_view()[slot].cap)
            ==> exists|o: ObjId| homes(old(store), o, slot) && dead_obj(final(store), o),
        // **Sees through queues — the rev2§3.4 / M1 subtree-deletion obligation, named.** After
        // `revoke`, no live slot anywhere — cspace resident *or* in-flight
        // channel ring cap — is a CDT descendant of `slot`. The transitive closure (not just the
        // `first_child is None` direct-child clause above) is what makes "revoke finds and deletes
        // in-flight caps like any other descendant" a *failing obligation* if a future change to
        // `cdt_wf`/`slot_move`/`delete` left a queued descendant attached. Read off at loop exit
        // from `first_child is None` + `cspace_wf` via `lemma_childless_no_descendant`.
        no_live_descendant(final(store).slot_view(), slot),
        // `revoke` only ever *empties* slots, never fills one — the teardown-monotonicity frame
        // composed from each `delete`'s `only_empties`. A queued cap the walk reaches is destroyed,
        // not relabelled.
        only_empties(old(store).slot_view(), final(store).slot_view()),
        ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ready_complete(final(store).ready_view(), final(store).tcb_view()),
{
    // `refcount_sound`, `caps_consistent`, the endpoint-cap census, and refs-domain
    // completeness ride the loop as `delete`'s preconditions: each `delete` requires them
    // (held by the invariant) and re-establishes them.
    while store.slot(slot).first_child.is_some()
        invariant
            cspace_wf(store.slot_view()),
            store.slot_view().dom().finite(),
            store.slot_view().dom().contains(slot),
            // The slot domain is fixed across the walk (`delete` frames it) — the equality
            // `only_empties`'s transitivity reads off to compose the per-step frame.
            store.slot_view().dom() == old(store).slot_view().dom(),
            // Teardown monotonicity so far: every slot empty at entry is still empty (rev2§3.4).
            only_empties(old(store).slot_view(), store.slot_view()),
            refcount_sound(store),
            caps_consistent(store),
            end_caps_sound(store),
            census_dom_complete(store),
            // The ready pair carries across the subtree loop — each `delete` ensures it.
            ready_wf(store.ready_view(), store.tcb_view()),
            ready_complete(store.ready_view(), store.tcb_view()),
            // `slot`'s home status is immutable across teardown (`home_views_frozen`, via
            // `lemma_is_homed_stable`), so the `old` homing carries through the loop unchanged.
            is_homed(store, slot) == is_homed(old(store), slot),
            // Conditional root-survival rides the loop only when the target started un-homed; the
            // homed (seL4-zombie) case is left unconstrained.
            !is_homed(old(store), slot) ==> !is_empty_cap(store.slot_view()[slot].cap),
            // The home maps are framed and death persists across the whole walk so far —
            // the stability the slot-specific witness invariant below rides.
            home_views_frozen(old(store), store),
            refs_death_persist(old(store), store),
            // Root provenance: if `slot` has been emptied, some object that homed it in the
            // *initial* state was destroyed (dead at the current step). Each `delete` step covers a
            // freshly-emptied `slot` via its target-aware frame (`slot != leaf`), and a death once
            // witnessed persists (`refs_death_persist`); `homes` is stable (`home_views_frozen`).
            is_empty_cap(store.slot_view()[slot].cap)
                ==> exists|o: ObjId| homes(old(store), o, slot) && dead_obj(store, o),
        decreases count_nonempty(store.slot_view()),
    {
        // The first child is live (it names `slot` as parent), so we descend from a
        // non-empty node; the leaf we reach is a strict descendant of `slot`.
        let first = store.slot(slot).first_child.unwrap();
        proof {
            assert(store.slot_view()[first].parent == Some(slot));
            assert(!is_empty_cap(store.slot_view()[first].cap));
        }
        let leaf = descend_to_leaf(store, first);
        let ghost pre = *store;
        delete(store, leaf);
        proof {
            // `slot != leaf`: `slot` has a child (the loop guard) while `descend_to_leaf`
            // returns a childless `leaf` — so `slot` is never the deleted target.
            assert(pre.slot_view()[slot].first_child is Some);
            assert(pre.slot_view()[leaf].first_child is None);
            assert(slot != leaf);
            // `delete` frames the home maps, so `slot`'s home status is stable across the step —
            // this maintains the `is_homed(store, slot) == is_homed(old, slot)` invariant.
            lemma_is_homed_stable(&pre, store, slot);
            // Conditional root-survival: only when the target started un-homed does
            // `unhomed_frozen` (target `leaf != slot`, `slot` un-homed) keep `slot`'s cap. The
            // homed case may empty `slot` (the seL4-zombie cross-object teardown), which the
            // conditional `ensures` admissibly leaves unconstrained.
            if !is_homed(old(store), slot) {
                assert(store.slot_view()[slot].cap == pre.slot_view()[slot].cap);
            }
            // Compose the home + death frames across this `delete` step (`old → pre` from
            // the loop invariant, `pre → store` from `delete`'s ensures).
            lemma_home_views_frozen_trans(old(store), &pre, store);
            lemma_refs_death_persist_trans(old(store), &pre, store);
            // Compose `only_empties` across this `delete` step (`old → pre` from the loop
            // invariant, `pre → store` from `delete`'s ensures); domains agree (the dom frame).
            lemma_only_empties_trans(old(store).slot_view(), pre.slot_view(), store.slot_view());
            // Root provenance: maintain the slot-specific witness invariant.
            if is_empty_cap(store.slot_view()[slot].cap) {
                if is_empty_cap(pre.slot_view()[slot].cap) {
                    // `slot` was already empty at `pre`: the loop invariant's witness `o` (dead at
                    // `pre`) stays dead at `store` (`delete`'s `refs_death_persist`).
                    let o = choose|o: ObjId| homes(old(store), o, slot) && dead_obj(&pre, o);
                    assert(dead_obj(store, o));
                } else {
                    // `slot` was freshly emptied by this `delete`. Since `slot != leaf`, `delete`'s
                    // target-aware frame `emptied_via_dead_home(pre, store, leaf)` supplies a witness
                    // `o` homing `slot` at `pre` and dead at `store`; `homes` is stable, so `o`
                    // homed `slot` in the initial state too.
                    let o = choose|o: ObjId| homes(&pre, o, slot) && dead_obj(store, o);
                    lemma_homes_stable(old(store), &pre, o, slot);
                    assert(homes(old(store), o, slot) && dead_obj(store, o));
                }
            }
        }
    }
    // The loop exited with `slot` childless; `cspace_wf` (hence `parent_has_first_child`) holds,
    // so the whole subtree — in-flight queued caps included — is provably gone (rev2§3.4).
    proof {
        lemma_childless_no_descendant(store.slot_view(), slot);
    }
}

/// The status of one bounded `revoke_step` quantum (rev2§2.2 "preemptible and
/// restartable"): `Done` when `slot`'s subtree is empty (the walk is complete),
/// `More` when the budget was spent with descendants still attached (re-issue from
/// the same root to continue). The kernel shell maps `More` to the `EAGAIN` retry
/// code and loops; the verified core only reports which it is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RevokeStatus {
    Done,
    More,
}

/// Marker frame: a single-slot edit that flips only the `revoking` bit — keeping
/// the slot's cap and all four CDT links, and framing every non-slot view — is
/// invisible to every cspace invariant. No structural (`cdt_wf`/`acyclic`) or census
/// (`obj_census`/`end_cap_count`/`cap_consistent`) predicate reads `revoking`; they
/// key off `.cap`, the links, and the framed object views, all unchanged here. This
/// is what lets `revoke_step` set/clear the marker on the root between quanta without
/// disturbing the loop's invariants. The structural half reuses
/// `lemma_local_cap_edit_preserves_cspace_wf`; the census/consistency halves ride the
/// cap-filter and emptiness-frame precedents (`lemma_map_frame_*`).
pub proof fn lemma_set_revoking_frames<S: Store>(s0: &S, s1: &S, slot: SlotId, v: CapSlot)
    requires
        s1.slot_view() == s0.slot_view().insert(slot, v),
        s0.slot_view().dom().contains(slot),
        s0.slot_view().dom().finite(),
        v.cap == s0.slot_view()[slot].cap,
        v.parent == s0.slot_view()[slot].parent,
        v.first_child == s0.slot_view()[slot].first_child,
        v.next_sib == s0.slot_view()[slot].next_sib,
        v.prev_sib == s0.slot_view()[slot].prev_sib,
        s1.refs_view() == s0.refs_view(),
        s1.chan_view() == s0.chan_view(),
        s1.notif_view() == s0.notif_view(),
        s1.tcb_view() == s0.tcb_view(),
        s1.timer_view() == s0.timer_view(),
        s1.timer_head_view() == s0.timer_head_view(),
        s1.ready_view() == s0.ready_view(),
        s1.cspace_view() == s0.cspace_view(),
        s1.irq_view() == s0.irq_view(),
        cspace_wf(s0.slot_view()),
        refcount_sound(s0),
        caps_consistent(s0),
        end_caps_sound(s0),
        census_dom_complete(s0),
    ensures
        s1.slot_view().dom() == s0.slot_view().dom(),
        cspace_wf(s1.slot_view()),
        count_nonempty(s1.slot_view()) == count_nonempty(s0.slot_view()),
        only_empties(s0.slot_view(), s1.slot_view()),
        only_empties(s1.slot_view(), s0.slot_view()),
        forall|x: SlotId| s0.slot_view().dom().contains(x)
            ==> #[trigger] s1.slot_view()[x].cap == s0.slot_view()[x].cap,
        refcount_sound(s1),
        caps_consistent(s1),
        end_caps_sound(s1),
        census_dom_complete(s1),
{
    let m0 = s0.slot_view();
    let m1 = s1.slot_view();
    assert(m1 =~= m0.insert(slot, v));
    assert(m1.dom() =~= m0.dom());
    // Pointwise cap + link equality (the edited `slot` by hypothesis, every other
    // slot untouched by the insert). Everything downstream rides this.
    assert forall|x: SlotId| m0.dom().contains(x) implies {
        &&& #[trigger] m1[x].cap == m0[x].cap
        &&& m1[x].parent == m0[x].parent
        &&& m1[x].first_child == m0[x].first_child
        &&& m1[x].next_sib == m0[x].next_sib
        &&& m1[x].prev_sib == m0[x].prev_sib
    } by {
        if x != slot {
            assert(m1[x] == m0[x]);
        }
    }
    // Structural well-formedness: links identical, cap-emptiness identical.
    lemma_local_cap_edit_preserves_cspace_wf(m0, slot, v);
    // count_nonempty + only_empties: the live-cap filter is identical (caps equal).
    assert(m1.dom().filter(|k: SlotId| !is_empty_cap(m1[k].cap))
        =~= m0.dom().filter(|k: SlotId| !is_empty_cap(m0[k].cap)));
    // Census: `slot_refs`/`frame_map_refs` are cap-filters (identical); the binding /
    // waiter / timer / hold terms read framed views (equal) — so `obj_census` is fixed.
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) == obj_census(s0, o) by {
        assert(m1.dom().filter(|k: SlotId| cap_obj(m1[k].cap) == Some(o))
            =~= m0.dom().filter(|k: SlotId| cap_obj(m0[k].cap) == Some(o)));
        assert(m1.dom().filter(|k: SlotId| cap_frame_aspace(m1[k].cap) == Some(o))
            =~= m0.dom().filter(|k: SlotId| cap_frame_aspace(m0[k].cap) == Some(o)));
    }
    lemma_refcount_sound_from_census_eq(s0, s1);
    // census_dom_complete: census fixed + refs domain fixed.
    assert forall|o: ObjId| #[trigger] obj_census(s1, o) >= 1 implies
        s1.refs_view().dom().contains(o) by {
        assert(obj_census(s0, o) >= 1);
    }
    // caps_consistent: `cap_consistent` reads `slot_view` only via `.dom()` plus the
    // framed object views; a Channel cap's `chan_wf` rides the emptiness frame.
    assert forall|s: SlotId| #![trigger m1[s]]
        m1.dom().contains(s) && !is_empty_cap(m1[s].cap)
        implies cap_consistent(s1, m1[s].cap) by {
        assert(m1[s].cap == m0[s].cap);
        assert(cap_consistent(s0, m0[s].cap));
        if let CapKind::Channel(o, _) = m0[s].cap.kind {
            lemma_chan_wf_emptiness_frame(s0.chan_view(), s1.chan_view(), m0, m1, o);
        }
    }
    // end_caps_sound: `end_cap_count` is a cap-filter (identical); `chan_view` framed.
    assert forall|ch: ObjId, e: int|
        s1.chan_view().dom().contains(ch) && s1.chan_view()[ch].end_caps.len() == 2 && 0 <= e < 2
        implies #[trigger] s1.chan_view()[ch].end_caps[e] == end_cap_count(m1, ch, e) by {
        assert(m1.dom().filter(|k: SlotId| cap_chan_end(m1[k].cap) == Some((ch, e)))
            =~= m0.dom().filter(|k: SlotId| cap_chan_end(m0[k].cap) == Some((ch, e))));
        assert(s0.chan_view()[ch].end_caps[e] == end_cap_count(m0, ch, e));
    }
}

/// Death-provenance carry across the marker write: the final `set_slot` keeps
/// `slot`'s cap and frames `refs_view`, so the loop invariant's witness (a homing
/// object dead at `s_pre`) is still a homing object dead at `s_new` — `dead_obj` reads
/// only `refs_view`. Lets `revoke_step`'s always-`ensures` carry the death-provenance
/// implication across the marker write in both the `More` and `Done` arms.
pub proof fn lemma_revoke_step_death_provenance<S: Store>(s_old: &S, s_pre: &S, s_new: &S, slot: SlotId)
    requires
        s_new.slot_view()[slot].cap == s_pre.slot_view()[slot].cap,
        s_new.refs_view() == s_pre.refs_view(),
        is_empty_cap(s_pre.slot_view()[slot].cap)
            ==> exists|o: ObjId| homes(s_old, o, slot) && dead_obj(s_pre, o),
    ensures
        is_empty_cap(s_new.slot_view()[slot].cap)
            ==> exists|o: ObjId| homes(s_old, o, slot) && dead_obj(s_new, o),
{
    if is_empty_cap(s_new.slot_view()[slot].cap) {
        let o = choose|o: ObjId| homes(s_old, o, slot) && dead_obj(s_pre, o);
        assert(homes(s_old, o, slot) && dead_obj(s_new, o));
    }
}

/// Revoke a **bounded quantum** of `slot`'s subtree, restartably (rev2§2.2). Does
/// at most `budget` leaf-deletions of the unbounded `revoke` walk, returning `Done`
/// when the subtree is empty and `More` when the budget runs out with descendants
/// still attached. A `More` return leaves the **revoke-in-progress marker** set on the
/// root (`derive` refuses growth into the subtree, so the multi-call walk terminates —
/// the cross-call liveness is mechanized in the `CapRevocation` TLA model); `Done`
/// clears it. The marker rides the last `set_slot`, *after* the loop, so the per-leaf
/// teardown (`delete`/`cdt_unlink`) is untouched and never has to frame it.
///
/// Each call re-establishes the full precondition bundle (so it is verifiably
/// restartable from the same root), exports the same descendant-deletion completeness
/// and root-survival/death-provenance theorems as `revoke` on `Done`, and a
/// **partial-progress** fact (`count_nonempty` strictly dropped) on `More`.
///
/// **Terminates per call** (`decreases budget - n`): a bounded counted loop.
pub fn revoke_step<S: Store>(store: &mut S, slot: SlotId, budget: usize) -> (res: RevokeStatus)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
        refcount_sound(old(store)),
        caps_consistent(old(store)),
        end_caps_sound(old(store)),
        census_dom_complete(old(store)),
        ready_wf(old(store).ready_view(), old(store).tcb_view()),
        ready_complete(old(store).ready_view(), old(store).tcb_view()),
        budget >= 1,
    ensures
        // ── Always (Done and More): the maintained invariant bundle, re-establishing
        // the next call's `requires` — this is what makes `revoke_step` restartable. ──
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom().finite(),
        final(store).slot_view().dom().contains(slot),
        refcount_sound(final(store)),
        caps_consistent(final(store)),
        end_caps_sound(final(store)),
        census_dom_complete(final(store)),
        ready_wf(final(store).ready_view(), final(store).tcb_view()),
        ready_complete(final(store).ready_view(), final(store).tcb_view()),
        only_empties(old(store).slot_view(), final(store).slot_view()),
        // Conditional root-survival + death-provenance (hold at every preemption point).
        !is_homed(old(store), slot) ==> !is_empty_cap(final(store).slot_view()[slot].cap),
        is_empty_cap(final(store).slot_view()[slot].cap)
            ==> exists|o: ObjId| homes(old(store), o, slot) && dead_obj(final(store), o),
        // ── Done: the subtree is empty (the `revoke` postcondition) and the marker is clear. ──
        res is Done ==> final(store).slot_view()[slot].first_child is None,
        res is Done ==> no_live_descendant(final(store).slot_view(), slot),
        res is Done ==> !final(store).slot_view()[slot].revoking,
        // ── More: descendants remain, the marker is set, and the walk made progress. ──
        res is More ==> final(store).slot_view()[slot].first_child is Some,
        res is More ==> !is_empty_cap(final(store).slot_view()[slot].cap),
        res is More ==> final(store).slot_view()[slot].revoking,
        res is More ==>
            count_nonempty(final(store).slot_view()) < count_nonempty(old(store).slot_view()),
{
    let mut n: usize = 0;
    // The bounded form of `revoke`'s walk: the loop body is identical (descend to a
    // leaf, delete it), counted by `n` and capped at `budget`. The invariant is
    // `revoke`'s plus the counter and the count-progress accumulator.
    while n < budget && store.slot(slot).first_child.is_some()
        invariant
            cspace_wf(store.slot_view()),
            store.slot_view().dom().finite(),
            store.slot_view().dom().contains(slot),
            store.slot_view().dom() == old(store).slot_view().dom(),
            only_empties(old(store).slot_view(), store.slot_view()),
            refcount_sound(store),
            caps_consistent(store),
            end_caps_sound(store),
            census_dom_complete(store),
            ready_wf(store.ready_view(), store.tcb_view()),
            ready_complete(store.ready_view(), store.tcb_view()),
            is_homed(store, slot) == is_homed(old(store), slot),
            !is_homed(old(store), slot) ==> !is_empty_cap(store.slot_view()[slot].cap),
            home_views_frozen(old(store), store),
            refs_death_persist(old(store), store),
            is_empty_cap(store.slot_view()[slot].cap)
                ==> exists|o: ObjId| homes(old(store), o, slot) && dead_obj(store, o),
            // The bounded-quantum bookkeeping.
            n <= budget,
            count_nonempty(store.slot_view()) + n <= count_nonempty(old(store).slot_view()),
        decreases budget - n,
    {
        let first = store.slot(slot).first_child.unwrap();
        proof {
            assert(store.slot_view()[first].parent == Some(slot));
            assert(!is_empty_cap(store.slot_view()[first].cap));
        }
        let leaf = descend_to_leaf(store, first);
        let ghost pre = *store;
        delete(store, leaf);
        proof {
            // `slot != leaf`: `slot` has a child (loop guard) while `descend_to_leaf`
            // returns a childless `leaf`.
            assert(pre.slot_view()[slot].first_child is Some);
            assert(pre.slot_view()[leaf].first_child is None);
            assert(slot != leaf);
            lemma_is_homed_stable(&pre, store, slot);
            if !is_homed(old(store), slot) {
                assert(store.slot_view()[slot].cap == pre.slot_view()[slot].cap);
            }
            lemma_home_views_frozen_trans(old(store), &pre, store);
            lemma_refs_death_persist_trans(old(store), &pre, store);
            lemma_only_empties_trans(old(store).slot_view(), pre.slot_view(), store.slot_view());
            // Root provenance witness (identical to `revoke`).
            if is_empty_cap(store.slot_view()[slot].cap) {
                if is_empty_cap(pre.slot_view()[slot].cap) {
                    let o = choose|o: ObjId| homes(old(store), o, slot) && dead_obj(&pre, o);
                    assert(dead_obj(store, o));
                } else {
                    let o = choose|o: ObjId| homes(&pre, o, slot) && dead_obj(store, o);
                    lemma_homes_stable(old(store), &pre, o, slot);
                    assert(homes(old(store), o, slot) && dead_obj(store, o));
                }
            }
            // Progress: `delete` strictly dropped the live count, so the accumulator
            // carries with `n + 1`.
            assert(count_nonempty(store.slot_view()) < count_nonempty(pre.slot_view()));
        }
        n = n + 1;
    }
    // ── The quantum is over. Set the marker (More) or clear it (Done) in one final
    // `set_slot`. The cap and links are untouched, so `lemma_set_revoking_frames`
    // carries every invariant across this write. ──
    let ghost s_pre = *store;
    let mut root = store.slot(slot);
    if root.first_child.is_some() {
        // More: the loop exited with children remaining, so `n == budget` (the only way
        // the `&&` guard fails while `first_child` is Some), hence `budget >= 1`
        // deletions happened — strict progress.
        proof {
            assert(n == budget);
            // `slot` has a child, so by `empty_slots_detached` it is non-empty.
            assert(!is_empty_cap(store.slot_view()[slot].cap));
        }
        root.revoking = true;
        store.set_slot(slot, root);
        proof {
            lemma_set_revoking_frames(&s_pre, store, slot, root);
            lemma_only_empties_trans(
                old(store).slot_view(),
                s_pre.slot_view(),
                store.slot_view(),
            );
            lemma_revoke_step_death_provenance(old(store), &s_pre, store, slot);
        }
        RevokeStatus::More
    } else {
        // Done: the subtree is empty. Clear the marker and read off no-live-descendant.
        root.revoking = false;
        store.set_slot(slot, root);
        proof {
            lemma_set_revoking_frames(&s_pre, store, slot, root);
            lemma_only_empties_trans(
                old(store).slot_view(),
                s_pre.slot_view(),
                store.slot_view(),
            );
            lemma_revoke_step_death_provenance(old(store), &s_pre, store, slot);
            lemma_childless_no_descendant(store.slot_view(), slot);
        }
        RevokeStatus::Done
    }
}

} // verus!
