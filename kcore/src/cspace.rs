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

/// pre:  cap designates a live object (or none).
/// post: object refcount incremented.
pub fn obj_ref<S: Store>(store: &mut S, cap: Cap) {
    if let Some(o) = cap.obj() {
        store.set_obj_refs(o, store.obj_refs(o) + 1);
    }
}

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

/// pre:  child is a detached slot (no links, non-empty cap already set);
///       parent is a live slot.
/// post: child is the first child of parent; previous children follow it.
pub fn cdt_insert_child<S: Store>(store: &mut S, parent: SlotId, child: SlotId) {
    let old_first = store.slot(parent).first_child;

    let mut c = store.slot(child);
    c.parent = Some(parent);
    c.prev_sib = None;
    c.next_sib = old_first;
    store.set_slot(child, c);

    if let Some(f) = old_first {
        let mut fs = store.slot(f);
        fs.prev_sib = Some(child);
        store.set_slot(f, fs);
    }

    let mut p = store.slot(parent);
    p.first_child = Some(child);
    store.set_slot(parent, p);
}

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

/// Derive a child cap (§2.3): copy with rights intersected — the only
/// derivation; there is no amplification path.
///
/// pre:  src non-empty, not Untyped (watermark has one owner); dst empty
///       and detached; mask ⊆ u8.
/// post: dst holds src's cap with rights ∩ mask, as a CDT child of src;
///       object refcount incremented.
pub fn derive<S: Store>(store: &mut S, src: SlotId, dst: SlotId, mask: u8) -> Result<(), ()> {
    let s = store.slot(src);
    if s.cap.is_empty() || matches!(s.cap.kind, CapKind::Untyped { .. }) {
        return Err(());
    }
    if !store.slot(dst).cap.is_empty() {
        return Err(());
    }
    let mut kind = s.cap.kind;
    // One mapping per cap copy (§2.5): a fresh frame copy starts unmapped.
    if let CapKind::Frame { mapping, .. } = &mut kind {
        *mapping = None;
    }
    let cap = Cap {
        kind,
        rights: s.cap.rights.masked(mask),
    };
    let mut d = store.slot(dst);
    d.cap = cap;
    store.set_slot(dst, d);
    obj_ref(store, cap);
    cdt_insert_child(store, src, dst);
    Ok(())
}

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
