//! Capability spaces and the capability derivation tree (spec §2.1–2.3,
//! §3.4).
//!
//! Every kernel object is reached through a `Cap` living in a `CapSlot`.
//! Slots form the CDT: parent/first-child/sibling pointers threaded through
//! the slots themselves (seL4-style). Channel queue slots are ordinary
//! `CapSlot`s owned by the channel, so the revoke walk sees in-flight caps
//! with no special case — the property checked unconditionally by the
//! CapRevocation TLA+ model.
//!
//! Concurrency invariant carried by every function here: the kernel is
//! single-core and non-preemptible (IRQs masked at EL1), so whoever is
//! executing kernel code has exclusive access to all kernel objects. All
//! raw-pointer dereferences below rely on that plus the ownership rules
//! stated per function.
//!
//! Verification debt (spec §6): these operations are the designated Verus
//! target. Until the Verus toolchain is wired into the build, each op
//! carries its contract as a comment in pre/post form, structured for
//! direct translation.

use core::ptr;

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
    pub const ALL: Rights = Rights(0b11);

    pub fn has(self, bits: u8) -> bool {
        self.0 & bits == bits
    }

    pub fn masked(self, mask: u8) -> Rights {
        Rights(self.0 & mask)
    }
}

/// Common header at the start of every kernel object. `refs` counts every
/// kernel pointer that keeps the object alive: cap slots, channel-event
/// bindings, blocked waiters, armed timers.
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
/// mapping per cap copy, and deleting the cap unmaps it (§2.5).
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
        mapping: Option<(*mut crate::aspace::AspaceObj, u64)>,
    },
    Aspace(*mut crate::aspace::AspaceObj),
    CSpace(*mut crate::cspace::CSpaceObj),
    Thread(*mut crate::thread::Tcb),
    Channel(*mut crate::channel::Channel, ChanEnd),
    Notification(*mut crate::notification::NotifObj),
    Timer(*mut crate::timer::TimerObj),
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

    /// The refcounted object header behind this cap, if any. Frames are
    /// bare memory like untyped — no object, no refcount.
    fn header(&self) -> Option<*mut ObjHeader> {
        match self.kind {
            CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
            CapKind::Aspace(p) => Some(p.cast()),
            CapKind::CSpace(p) => Some(p.cast()),
            CapKind::Thread(p) => Some(p.cast()),
            CapKind::Channel(p, _) => Some(p.cast()),
            CapKind::Notification(p) => Some(p.cast()),
            CapKind::Timer(p) => Some(p.cast()),
        }
    }
}

/// A capability slot, CDT links included. Slots live inside cspace objects
/// and inside channel message slots — both are CDT-visible (§3.4).
#[repr(C)]
pub struct CapSlot {
    pub cap: Cap,
    pub parent: *mut CapSlot,
    pub first_child: *mut CapSlot,
    pub next_sib: *mut CapSlot,
    pub prev_sib: *mut CapSlot,
}

impl CapSlot {
    pub const fn empty() -> CapSlot {
        CapSlot {
            cap: Cap::EMPTY,
            parent: ptr::null_mut(),
            first_child: ptr::null_mut(),
            next_sib: ptr::null_mut(),
            prev_sib: ptr::null_mut(),
        }
    }
}

/// A capability space: header + inline slot array.
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
            return ptr::null_mut();
        }
        let base = this.add(1) as *mut CapSlot;
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
pub unsafe fn obj_ref(cap: &Cap) {
    if let Some(h) = cap.header() {
        (*h).refs += 1;
    }
}

/// pre:  cap designates a live object (or none); refs > 0.
/// post: refcount decremented; if it reached zero the object is destroyed
///       (type-specific teardown).
unsafe fn obj_unref(cap: &Cap) {
    let Some(h) = cap.header() else { return };
    (*h).refs -= 1;
    if (*h).refs == 0 {
        match cap.kind {
            CapKind::CSpace(p) => destroy_cspace(p),
            CapKind::Thread(p) => crate::thread::destroy_tcb(p),
            CapKind::Channel(p, _) => crate::channel::destroy_channel(p),
            CapKind::Notification(p) => crate::notification::destroy_notif(p),
            CapKind::Timer(p) => crate::timer::destroy_timer(p),
            CapKind::Aspace(p) => crate::aspace::destroy_aspace(p),
            CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => {}
        }
    }
}

/// Drop a non-cap reference to an aspace (mapped frames and bound
/// threads hold these so the aspace can't die under them).
pub unsafe fn unref_aspace(a: *mut crate::aspace::AspaceObj) {
    (*a).hdr.refs -= 1;
    if (*a).hdr.refs == 0 {
        crate::aspace::destroy_aspace(a);
    }
}

pub unsafe fn unref_cspace(cs: *mut CSpaceObj) {
    (*cs).hdr.refs -= 1;
    if (*cs).hdr.refs == 0 {
        destroy_cspace(cs);
    }
}

/// pre:  cspace refs == 0.
/// post: every cap the cspace still held is deleted (their objects unref'd).
unsafe fn destroy_cspace(cs: *mut CSpaceObj) {
    for i in 0..(*cs).num_slots {
        let s = CSpaceObj::slot(cs, i);
        if !(*s).cap.is_empty() {
            delete(s);
        }
    }
    // Memory returns to the donor untyped only via revoke of the untyped
    // cap; no allocator hands it back early (§3.2).
}

// ── CDT structure ───────────────────────────────────────────────────────

/// pre:  child is a detached slot (no links, non-empty cap already set);
///       parent is a live slot.
/// post: child is the first child of parent; previous children follow it.
pub unsafe fn cdt_insert_child(parent: *mut CapSlot, child: *mut CapSlot) {
    (*child).parent = parent;
    (*child).prev_sib = ptr::null_mut();
    (*child).next_sib = (*parent).first_child;
    if !(*parent).first_child.is_null() {
        (*(*parent).first_child).prev_sib = child;
    }
    (*parent).first_child = child;
}

/// pre:  slot is linked in the CDT (possibly a root with null parent).
/// post: slot is detached; its children are spliced into slot's former
///       parent's child list (re-parented one level up). Authority remains
///       monotone: the children were derived through slot, so everything
///       they grant was already derivable from the parent.
unsafe fn cdt_unlink(slot: *mut CapSlot) {
    let parent = (*slot).parent;
    let prev = (*slot).prev_sib;
    let next = (*slot).next_sib;
    let first = (*slot).first_child;

    // Children take slot's place in the sibling list: prev → C1…Ck → next.
    let mut last = ptr::null_mut();
    let mut c = first;
    while !c.is_null() {
        (*c).parent = parent;
        last = c;
        c = (*c).next_sib;
    }

    let head = if first.is_null() { next } else { first };
    if !prev.is_null() {
        (*prev).next_sib = head;
    } else if !parent.is_null() {
        (*parent).first_child = head;
    }
    if !head.is_null() {
        (*head).prev_sib = prev;
    }
    if !first.is_null() {
        (*last).next_sib = next;
        if !next.is_null() {
            (*next).prev_sib = last;
        }
    }

    (*slot).parent = ptr::null_mut();
    (*slot).first_child = ptr::null_mut();
    (*slot).next_sib = ptr::null_mut();
    (*slot).prev_sib = ptr::null_mut();
}

/// Move a cap between slots, preserving its CDT position (§3.4: send and
/// receive move caps; a move is the same cap relocating, not a derivation).
///
/// pre:  src is non-empty and linked; dst is empty and detached.
/// post: dst holds src's cap and CDT position; src is empty and detached;
///       refcounts unchanged (same single owner throughout).
pub unsafe fn slot_move(src: *mut CapSlot, dst: *mut CapSlot) {
    debug_assert!(!(*src).cap.is_empty());
    debug_assert!((*dst).cap.is_empty());
    (*dst).cap = (*src).cap;
    (*dst).parent = (*src).parent;
    (*dst).first_child = (*src).first_child;
    (*dst).next_sib = (*src).next_sib;
    (*dst).prev_sib = (*src).prev_sib;
    if !(*dst).parent.is_null() && (*(*dst).parent).first_child == src {
        (*(*dst).parent).first_child = dst;
    }
    if !(*dst).prev_sib.is_null() {
        (*(*dst).prev_sib).next_sib = dst;
    }
    if !(*dst).next_sib.is_null() {
        (*(*dst).next_sib).prev_sib = dst;
    }
    let mut c = (*dst).first_child;
    while !c.is_null() {
        (*c).parent = dst;
        c = (*c).next_sib;
    }
    *src = CapSlot::empty();
}

/// Derive a child cap (§2.3): copy with rights intersected — the only
/// derivation; there is no amplification path.
///
/// pre:  src non-empty, not Untyped (watermark has one owner); dst empty
///       and detached; mask ⊆ u8.
/// post: dst holds src's cap with rights ∩ mask, as a CDT child of src;
///       object refcount incremented.
pub unsafe fn derive(src: *mut CapSlot, dst: *mut CapSlot, mask: u8) -> Result<(), ()> {
    if (*src).cap.is_empty() || matches!((*src).cap.kind, CapKind::Untyped { .. }) {
        return Err(());
    }
    if !(*dst).cap.is_empty() {
        return Err(());
    }
    let mut kind = (*src).cap.kind;
    // One mapping per cap copy (§2.5): a fresh frame copy starts unmapped.
    if let CapKind::Frame { mapping, .. } = &mut kind {
        *mapping = None;
    }
    (*dst).cap = Cap {
        kind,
        rights: (*src).cap.rights.masked(mask),
    };
    obj_ref(&(*dst).cap);
    cdt_insert_child(src, dst);
    Ok(())
}

/// Delete one cap (children survive, re-parented one level up).
///
/// pre:  slot non-empty.
/// post: slot empty and detached; object unref'd (destroyed if last ref).
///
/// Last-ref teardown of container objects (cspaces, channels) recurses
/// through here; depth is bounded by the nesting of containers holding the
/// final cap to other containers. seL4 flattens this with zombie caps —
/// owed when the revoke walk becomes preemptible, tracked as M2 debt.
pub unsafe fn delete(slot: *mut CapSlot) {
    debug_assert!(!(*slot).cap.is_empty());
    let cap = (*slot).cap;
    cdt_unlink(slot);
    (*slot).cap = Cap::EMPTY;
    // Channel endpoint liveness is tracked per end for peer-closed (§3.3).
    if let CapKind::Channel(ch, end) = cap.kind {
        crate::channel::endpoint_cap_dropped(ch, end);
    }
    // Deleting a mapped frame cap unmaps it — the one revocation story
    // for shared memory (§2.5).
    if let CapKind::Frame { pages, mapping: Some((asp, va)), .. } = cap.kind {
        crate::aspace::AspaceObj::unmap(asp, va, pages);
        unref_aspace(asp);
    }
    obj_unref(&cap);
}

/// Revoke: delete every CDT descendant of `slot` — cspace residents and
/// in-flight queue slots alike, unconditionally (§2.2). The cap itself
/// survives. Deletion is leaf-first so the tree stays consistent at every
/// step (the LiveParent invariant of the TLA+ model holds throughout,
/// which is what makes the walk restartable when it becomes preemptible).
///
/// pre:  slot non-empty.
/// post: slot has no descendants; slot's cap unchanged.
pub unsafe fn revoke(slot: *mut CapSlot) {
    while !(*slot).first_child.is_null() {
        // Descend to a leaf of our subtree.
        let mut leaf = (*slot).first_child;
        while !(*leaf).first_child.is_null() {
            leaf = (*leaf).first_child;
        }
        delete(leaf);
    }
}
