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

// `cdt_insert_child` is verified — see the `verus!{}` block at the end of this
// file.

/// pre:  slot is linked in the CDT (possibly a root with null parent).
/// post: slot is detached; its children are spliced into slot's former
///       parent's child list (re-parented one level up). Authority remains
///       monotone: the children were derived through slot, so everything
///       they grant was already derivable from the parent.
///
/// `pub(crate)` so the proof harnesses can exercise the unlink directly
/// (plan §4.1 `check_cdt_unlink`); it has no callers outside this crate.
pub(crate) fn cdt_unlink<S: Store>(store: &mut S, slot: SlotId) {
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

/// Move a cap between slots, preserving its CDT position (§3.4: send and
/// receive move caps; a move is the same cap relocating, not a derivation).
///
/// pre:  src is non-empty and linked; dst is empty and detached.
/// post: dst holds src's cap and CDT position; src is empty and detached;
///       refcounts unchanged (same single owner throughout).
pub fn slot_move<S: Store>(store: &mut S, src: SlotId, dst: SlotId) {
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

// `derive` is verified — see the `verus!{}` block at the end of this file.

/// Delete one cap (children survive, re-parented one level up).
///
/// pre:  slot non-empty.
/// post: slot empty and detached; object unref'd (destroyed if last ref).
///
/// Ordering is load-bearing (TSpec `ChannelFireSafe`, plan DN-2): a channel
/// cap fires `endpoint_cap_dropped` (peer-closed) *before* `obj_unref`, so a
/// whole-object teardown signals each surviving peer's binding into a still-
/// live notification.
///
/// Last-ref teardown of container objects (cspaces, channels) recurses
/// through here; depth is bounded by the nesting of containers holding the
/// final cap to other containers. seL4 flattens this with zombie caps —
/// owed when the revoke walk becomes preemptible, tracked as M2 debt.
// kani_contracts spike retired in the arena rewrite.
pub fn delete<S: Store>(store: &mut S, slot: SlotId) {
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

/// Revoke: delete every CDT descendant of `slot` — cspace residents and
/// in-flight queue slots alike, unconditionally (§2.2). The cap itself
/// survives. Deletion is leaf-first so the tree stays consistent at every
/// step (the LiveParent invariant of the TLA+ model holds throughout,
/// which is what makes the walk restartable when it becomes preemptible).
///
/// pre:  slot non-empty.
/// post: slot has no descendants; slot's cap unchanged.
pub fn revoke<S: Store>(store: &mut S, slot: SlotId) {
    while let Some(mut leaf) = store.slot(slot).first_child {
        // Descend to a leaf of our subtree.
        while let Some(c) = store.slot(leaf).first_child {
            leaf = c;
        }
        delete(store, leaf);
    }
}

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

// A node's first child claims it as parent and heads the sibling list.
pub open spec fn first_child_parent_agree(m: Map<SlotId, CapSlot>) -> bool {
    forall|p: SlotId| #[trigger] m.dom().contains(p) ==>
        (m[p].first_child matches Some(c) ==>
            m.dom().contains(c) && m[c].parent == Some(p) && m[c].prev_sib == None)
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

pub open spec fn cdt_wf(m: Map<SlotId, CapSlot>) -> bool {
    &&& links_in_domain(m)
    &&& siblings_doubly_consistent(m)
    &&& first_child_parent_agree(m)
    &&& empty_slots_detached(m)
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
/// post: `child` is `parent`'s first child; the previous children follow it;
///       caps and refcounts are untouched; the cspace stays well-formed.
pub fn cdt_insert_child<S: Store>(store: &mut S, parent: SlotId, child: SlotId)
    requires
        cdt_wf(old(store).slot_view()),
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
        cdt_wf(final(store).slot_view()),
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
}

/// Derive a child cap (§2.3): copy with rights intersected — the only
/// derivation; there is no amplification path.
///
/// pre:  the cspace is well-formed; `src`/`dst` are live; if `src` designates an
///       object that object's refcount is below `u32::MAX`.
/// post: on `Ok`, `dst` holds `src`'s cap with rights ∩ `mask` — so its rights
///       are a **subset** of `src`'s for every `mask` (the load-bearing monotone
///       -derivation theorem, now proven ∀ rather than sampled); `dst` is `src`'s
///       first child; the object's refcount is +1; the cspace stays well-formed.
///       On `Err` (empty/Untyped src, or occupied dst) the store is unchanged.
pub fn derive<S: Store>(store: &mut S, src: SlotId, dst: SlotId, mask: u8) -> (res: Result<(), ()>)
    requires
        cdt_wf(old(store).slot_view()),
        old(store).slot_view().dom().finite(),
        old(store).slot_view().dom().contains(src),
        old(store).slot_view().dom().contains(dst),
        cap_obj(old(store).slot_view()[src].cap) matches Some(o) ==>
            old(store).refs_view().dom().contains(o)
                && old(store).refs_view()[o] < u32::MAX as nat,
    ensures
        res is Ok ==> {
            // monotone derivation: dst's rights are src's rights masked, hence a
            // subset for ALL masks — authority only ever shrinks.
            &&& final(store).slot_view()[dst].cap.rights.0
                  == (old(store).slot_view()[src].cap.rights.0 & mask)
            &&& (final(store).slot_view()[dst].cap.rights.0
                  & old(store).slot_view()[src].cap.rights.0)
                  == final(store).slot_view()[dst].cap.rights.0
            &&& cdt_wf(final(store).slot_view())
            &&& final(store).slot_view()[src].first_child == Some(dst)
            &&& (cap_obj(old(store).slot_view()[src].cap) matches Some(o) ==>
                  final(store).refs_view()
                      =~= old(store).refs_view().insert(o, (old(store).refs_view()[o] + 1) as nat))
            // refcount soundness preserved: the slot census rises by one in
            // lockstep with the stored refcount above.
            &&& (cap_obj(old(store).slot_view()[src].cap) matches Some(o) ==>
                  slot_refs(final(store).slot_view(), o)
                      == slot_refs(old(store).slot_view(), o) + 1)
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
    assert(cap_obj(cap) == cap_obj(s.cap));
    assert(!is_empty_cap(cap));

    let mut d = store.slot(dst);
    d.cap = cap;
    store.set_slot(dst, d);
    // Setting an empty, detached slot to a non-empty cap (links still null)
    // preserves well-formedness: dst gains no links and no slot links to it.
    let ghost m1 = store.slot_view();
    assert(m1 =~= m0.insert(dst, d));
    assert(cdt_wf(m1));

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

} // verus!
