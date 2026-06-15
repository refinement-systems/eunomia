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
use crate::store::Store;
use vstd::prelude::*;

/// Rights bits — monotone under derivation (§2.3): `derive` may only clear
/// bits, never set them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub u8);

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

    pub fn masked(self, mask: u8) -> Rights {
        Rights(self.0 & mask)
    }
}

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

    /// The refcounted object handle behind this cap, if any. Frames are
    /// bare memory like untyped — no object, no refcount.
    fn obj(&self) -> Option<ObjId> {
        match self.kind {
            CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
            CapKind::Aspace(o) => Some(o),
            CapKind::CSpace(o) => Some(o),
            CapKind::Thread(o) => Some(o),
            CapKind::Channel(o, _) => Some(o),
            CapKind::Notification(o) => Some(o),
            CapKind::Timer(o) => Some(o),
        }
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

/// pre:  cap designates a live object (or none); refs > 0.
/// post: refcount decremented; if it reached zero the object is destroyed
///       (type-specific teardown).
fn obj_unref<S: Store>(store: &mut S, cap: Cap) {
    let Some(o) = cap.obj() else { return };
    store.set_obj_refs(o, store.obj_refs(o) - 1);
    if store.obj_refs(o) == 0 {
        match cap.kind {
            CapKind::CSpace(_) => destroy_cspace(store, o),
            CapKind::Thread(_) => crate::thread::destroy_tcb(store, o),
            CapKind::Channel(_, _) => crate::channel::destroy_channel(store, o),
            CapKind::Notification(_) => crate::notification::destroy_notif(store, o),
            CapKind::Timer(_) => crate::timer::destroy_timer(store, o),
            CapKind::Aspace(_) => store.aspace_destroy(o),
            CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => {}
        }
    }
}

/// Drop a non-cap reference to an aspace (mapped frames and bound
/// threads hold these so the aspace can't die under them).
pub fn unref_aspace<S: Store>(store: &mut S, a: ObjId) {
    store.set_obj_refs(a, store.obj_refs(a) - 1);
    if store.obj_refs(a) == 0 {
        store.aspace_destroy(a);
    }
}

// kani_contracts spike retired in the arena rewrite.
pub fn unref_cspace<S: Store>(store: &mut S, cs: ObjId) {
    store.set_obj_refs(cs, store.obj_refs(cs) - 1);
    if store.obj_refs(cs) == 0 {
        destroy_cspace(store, cs);
    }
}

/// pre:  cspace refs == 0.
/// post: every cap the cspace still held is deleted (their objects unref'd).
///
/// `pub(crate)` so the proof harness can drive the resident-teardown loop
/// directly (plan §4.1 `check_destroy_cspace`); it has no callers outside
/// this crate.
pub(crate) fn destroy_cspace<S: Store>(store: &mut S, cs: ObjId) {
    let n = store.cspace_num_slots(cs);
    for i in 0..n {
        let sid = store.cspace_slot(cs, i);
        if !store.slot(sid).cap.is_empty() {
            delete(store, sid);
        }
    }
    // Memory returns to the donor untyped only via revoke of the untyped
    // cap; no allocator hands it back early (§3.2).
}

// ── CDT structure ───────────────────────────────────────────────────────

// `cdt_insert_child` and `derive` are verified — see the `verus!{}` block at the
// end of this file.
//
// `cdt_unlink` and `slot_move` are also in that block, carrying assumed contracts
// (`#[verifier::external_body]`, the `delete` precedent): both are
// `first_child→next_sib` children walks whose body proofs need the linked-list-
// splice invariant the looping ops share (doc/results/22 §3). Their termination
// measure — sibling-acyclicity (`sib_acyclic`) — is now part of `cspace_wf`, and
// their contracts are host-test-checked against the real bodies (ArrayStore).
//
// `delete` and `revoke` are likewise in that block (`delete` carries an assumed
// teardown-recursion contract; `revoke`'s termination is proven against it).

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

    fn slot(&self, s: SlotId) -> (r: CapSlot)
        requires self.slot_view().dom().contains(s),
        ensures r == self.slot_view()[s];

    fn set_slot(&mut self, s: SlotId, v: CapSlot)
        requires old(self).slot_view().dom().contains(s),
        ensures
            final(self).slot_view() == old(self).slot_view().insert(s, v),
            final(self).refs_view() == old(self).refs_view();

    fn obj_refs(&self, o: ObjId) -> (r: u32)
        requires self.refs_view().dom().contains(o),
        ensures r as nat == self.refs_view()[o];

    fn set_obj_refs(&mut self, o: ObjId, r: u32)
        requires old(self).refs_view().dom().contains(o),
        ensures
            final(self).refs_view() == old(self).refs_view().insert(o, r as nat),
            final(self).slot_view() == old(self).slot_view();
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

// `Rights::masked` clears bits (rights ∩ mask); its bit-level spec is what makes
// monotone derivation provable. (Trusted boundary: the method is plain Rust;
// this states what it computes.)
pub assume_specification [ Rights::masked ](r: Rights, mask: u8) -> (out: Rights)
    ensures out.0 == (r.0 & mask);

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
/// **Trusted boundary (assumed contract).** The body is a `first_child→next_sib`
/// children walk that splices `slot`'s children into its former parent's child
/// list in `slot`'s position, then detaches `slot`. The body proof needs the
/// linked-list-splice invariant (the partial-progress characterization relative
/// to the entry map) that all three looping ops share — the scoped residue
/// (doc/results/22 §3); its termination measure, sibling-acyclicity, is now part
/// of `cspace_wf`. The contract — `cspace_wf` preserved, `slot` detached with its
/// cap intact, domain and refcounts framed — is host-test-checked against the
/// real body (ArrayStore, kcore tests). `pub(crate)`: no callers outside `kcore`.
#[verifier::external_body]
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
    let s = store.slot(slot);
    let parent = s.parent;
    let prev = s.prev_sib;
    let next = s.next_sib;
    let first = s.first_child;

    // Children take slot's place in the sibling list: prev → C1…Ck → next.
    let mut last = None;
    let mut c = first;
    while let Some(cur) = c {
        let mut cs = store.slot(cur);
        cs.parent = parent;
        let nx = cs.next_sib;
        store.set_slot(cur, cs);
        last = Some(cur);
        c = nx;
    }

    let head = if first.is_none() { next } else { first };
    if let Some(pv) = prev {
        let mut ps = store.slot(pv);
        ps.next_sib = head;
        store.set_slot(pv, ps);
    } else if let Some(pa) = parent {
        let mut pas = store.slot(pa);
        pas.first_child = head;
        store.set_slot(pa, pas);
    }
    if let Some(h) = head {
        let mut hs = store.slot(h);
        hs.prev_sib = prev;
        store.set_slot(h, hs);
    }
    if first.is_some() {
        let l = last.unwrap();
        let mut ls = store.slot(l);
        ls.next_sib = next;
        store.set_slot(l, ls);
        if let Some(nx) = next {
            let mut ns = store.slot(nx);
            ns.prev_sib = last;
            store.set_slot(nx, ns);
        }
    }

    let mut s = store.slot(slot);
    s.parent = None;
    s.first_child = None;
    s.next_sib = None;
    s.prev_sib = None;
    store.set_slot(slot, s);
}

/// Move a cap between slots, preserving its CDT position (§3.4: send and receive
/// move caps; a move is the same cap relocating, not a derivation).
///
/// **Trusted boundary (assumed contract).** The body re-points `src`'s CDT
/// neighbours and children to `dst`, then empties `src` — the same children-walk
/// shape as `cdt_unlink`, with the same scoped body-proof residue
/// (doc/results/22 §3). The contract — `cspace_wf` preserved, `dst` inherits
/// `src`'s cap, `src` emptied, the live-slot count and refcounts unchanged (a
/// move is one owner relocating) — is host-test-checked against the real body.
#[verifier::external_body]
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
        final(store).slot_view()[dst].cap == old(store).slot_view()[src].cap,
        is_empty_cap(final(store).slot_view()[src].cap),
        count_nonempty(final(store).slot_view()) == count_nonempty(old(store).slot_view()),
{
    let s = store.slot(src);
    debug_assert!(!s.cap.is_empty());
    debug_assert!(store.slot(dst).cap.is_empty());

    let mut d = store.slot(dst);
    d.cap = s.cap;
    d.parent = s.parent;
    d.first_child = s.first_child;
    d.next_sib = s.next_sib;
    d.prev_sib = s.prev_sib;
    store.set_slot(dst, d);

    if let Some(pa) = d.parent {
        let mut pas = store.slot(pa);
        if pas.first_child == Some(src) {
            pas.first_child = Some(dst);
            store.set_slot(pa, pas);
        }
    }
    if let Some(pv) = d.prev_sib {
        let mut pvs = store.slot(pv);
        pvs.next_sib = Some(dst);
        store.set_slot(pv, pvs);
    }
    if let Some(nx) = d.next_sib {
        let mut nxs = store.slot(nx);
        nxs.prev_sib = Some(dst);
        store.set_slot(nx, nxs);
    }
    let mut c = d.first_child;
    while let Some(cur) = c {
        let mut cs = store.slot(cur);
        cs.parent = Some(dst);
        let nx = cs.next_sib;
        store.set_slot(cur, cs);
        c = nx;
    }

    store.set_slot(src, CapSlot::empty());
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
/// — the harder `cdt_wf`-preservation case. The contract is silent on
/// `refs_view` (the teardown's refcount effects); the refcount discipline across
/// teardown lands with the cross-object body proof (doc/results/21 §9).
#[verifier::external_body]
pub fn delete<S: Store>(store: &mut S, slot: SlotId)
    requires
        cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(slot),
        !is_empty_cap(old(store).slot_view()[slot].cap),
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        final(store).slot_view().dom().finite(),
        is_empty_cap(final(store).slot_view()[slot].cap),
        count_nonempty(final(store).slot_view()) < count_nonempty(old(store).slot_view()),
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
    ensures
        cspace_wf(final(store).slot_view()),
        final(store).slot_view().dom().contains(slot),
        final(store).slot_view()[slot].first_child is None,
{
    while store.slot(slot).first_child.is_some()
        invariant
            cspace_wf(store.slot_view()),
            store.slot_view().dom().finite(),
            store.slot_view().dom().contains(slot),
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
