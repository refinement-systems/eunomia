//! Well-formedness predicates: the executable `TypeOK` of the CapRevocation
//! TLA+ model (plan §4.1). `cdt_wf` is the CDT structural invariant,
//! `chan_wf` the ring invariant, and `refcount_sound` the implementation-
//! only `RefCountSound` census — `hdr.refs` equals the number of references
//! that keep each object alive (caps in the slot universe, plus the
//! type-specific extras: notification waiters/bindings/armed-timers, the
//! cspace/aspace edges on TCBs, and frame mappings).
//!
//! All walks are bounded by the slot universe size (the `UNWIND_WF` budget),
//! so they terminate under CBMC.

use super::bounds::*;
use super::world::World;
use crate::channel::Channel;
use crate::cspace::{CapKind, CapSlot};
use crate::notification::NotifObj;
use crate::thread::ThreadState;

/// Is `p` null or one of the universe's slots? Every CDT link must point
/// inside the world (plan §4.1).
unsafe fn is_member(p: *mut CapSlot, slots: &[*mut CapSlot]) -> bool {
    p.is_null() || slots.iter().any(|&s| s == p)
}

/// The executable `TypeOK` for the CDT (plan §4.1): sibling lists doubly
/// consistent, parent/first-child consistent, empty slots fully detached,
/// roots sibling-free, no cycles, all links inside the universe.
pub unsafe fn cdt_wf(slots: &[*mut CapSlot]) -> bool {
    let n = slots.len();

    for &s in slots {
        let empty = (*s).cap.is_empty();
        let parent = (*s).parent;
        let fc = (*s).first_child;
        let ns = (*s).next_sib;
        let ps = (*s).prev_sib;

        if !is_member(parent, slots)
            || !is_member(fc, slots)
            || !is_member(ns, slots)
            || !is_member(ps, slots)
        {
            return false;
        }

        if empty {
            // Empty slot ⇒ all four links null (fully detached).
            if !parent.is_null() || !fc.is_null() || !ns.is_null() || !ps.is_null() {
                return false;
            }
            continue;
        }

        // Sibling list doubly consistent.
        if !ns.is_null() && (*ns).prev_sib != s {
            return false;
        }
        if !ps.is_null() && (*ps).next_sib != s {
            return false;
        }

        // first_child consistency: it points back, and it heads the list.
        if !fc.is_null() {
            if (*fc).parent != s || !(*fc).prev_sib.is_null() {
                return false;
            }
        }

        // A first child (parent set, no prev sib) is its parent's first_child.
        if !parent.is_null() && ps.is_null() && (*parent).first_child != s {
            return false;
        }

        // Roots have no siblings (only cdt_insert_child makes siblings, and
        // always under a non-null parent).
        if parent.is_null() && (!ps.is_null() || !ns.is_null()) {
            return false;
        }
    }

    // Acyclicity: every non-empty slot's parent chain reaches null within
    // the universe size.
    for &s in slots {
        if (*s).cap.is_empty() {
            continue;
        }
        let mut cur = (*s).parent;
        let mut steps = 0usize;
        while !cur.is_null() {
            steps += 1;
            if steps > n {
                return false;
            }
            cur = (*cur).parent;
        }
    }

    true
}

/// Ring invariant (plan §4.1): counts within depth, head in range, and every
/// cap slot *outside* the live window `[head, head+count)` is cap-empty.
/// Slots inside the window MAY be empty — revocation empties queued slots in
/// flight (§3.4 null-slot rule) — so this never asserts `count == #caps`.
pub unsafe fn chan_wf(ch: *mut Channel) -> bool {
    let depth = (*ch).depth;
    if depth == 0 {
        return false;
    }
    for r in 0..2 {
        if (*ch).count[r] > depth || (*ch).head[r] >= depth {
            return false;
        }
        for i in 0..depth {
            let rel = (i + depth - (*ch).head[r]) % depth;
            let in_window = rel < (*ch).count[r];
            if !in_window {
                let slot = Channel::slot(ch, r, i);
                for c in 0..crate::channel::MSG_CAPS {
                    if !(*slot).caps[c].cap.is_empty() {
                        return false;
                    }
                }
            }
        }
    }
    true
}

// ── refcount census ──────────────────────────────────────────────────────

unsafe fn count_notif_refs(w: &mut World, n: *mut NotifObj, slots: &[*mut CapSlot]) -> u32 {
    let mut c = 0u32;
    for &s in slots {
        if let CapKind::Notification(p) = (*s).cap.kind {
            if p == n {
                c += 1;
            }
        }
    }
    for t in 0..NTHREADS {
        let tcb = w.tcb(t);
        if (*tcb).state == ThreadState::BlockedNotif && (*tcb).wait_notif == n {
            c += 1;
        }
    }
    let ch = w.channel();
    for end in 0..2 {
        for ev in 0..3 {
            if (*ch).bindings[end][ev].notif == n {
                c += 1;
            }
        }
    }
    for ti in 0..NTIMERS {
        let tm = w.timer(ti);
        if (*tm).armed && (*tm).notif == n {
            c += 1;
        }
    }
    c
}

unsafe fn count_channel_refs(ch: *mut Channel, slots: &[*mut CapSlot]) -> u32 {
    let mut c = 0u32;
    for &s in slots {
        if let CapKind::Channel(p, _) = (*s).cap.kind {
            if p == ch {
                c += 1;
            }
        }
    }
    c
}

unsafe fn count_cspace_refs(
    w: &mut World,
    cs: *mut crate::cspace::CSpaceObj,
    slots: &[*mut CapSlot],
) -> u32 {
    let mut c = 0u32;
    for &s in slots {
        if let CapKind::CSpace(p) = (*s).cap.kind {
            if p == cs {
                c += 1;
            }
        }
    }
    for t in 0..NTHREADS {
        if (*w.tcb(t)).cspace == cs {
            c += 1;
        }
    }
    c
}

unsafe fn count_aspace_refs(
    w: &mut World,
    a: *mut crate::aspace::AspaceObj,
    slots: &[*mut CapSlot],
) -> u32 {
    let mut c = 0u32;
    for &s in slots {
        match (*s).cap.kind {
            CapKind::Aspace(p) if p == a => c += 1,
            CapKind::Frame { mapping: Some((ap, _)), .. } if ap == a => c += 1,
            _ => {}
        }
    }
    for t in 0..NTHREADS {
        if (*w.tcb(t)).aspace == a {
            c += 1;
        }
    }
    c
}

unsafe fn count_tcb_refs(tcb: *mut crate::thread::Tcb, slots: &[*mut CapSlot]) -> u32 {
    let mut c = 0u32;
    for &s in slots {
        if let CapKind::Thread(p) = (*s).cap.kind {
            if p == tcb {
                c += 1;
            }
        }
    }
    c
}

unsafe fn count_timer_refs(tm: *mut crate::timer::TimerObj, slots: &[*mut CapSlot]) -> u32 {
    let mut c = 0u32;
    for &s in slots {
        if let CapKind::Timer(p) = (*s).cap.kind {
            if p == tm {
                c += 1;
            }
        }
    }
    c
}

/// `RefCountSound` (plan §4.1): every object's `hdr.refs` equals its census.
/// Note this is `refs == census` and not "destroyed exactly at zero" — three
/// sites legitimately drop a refcount to zero with a bare decrement and no
/// teardown (findings DN-1), and `0 == 0` still holds here.
pub unsafe fn refcount_sound(w: &mut World) -> bool {
    let slots = w.collect_slots();
    for i in 0..NNOTIFS {
        let n = w.notif(i);
        if (*n).hdr.refs != count_notif_refs(w, n, &slots) {
            return false;
        }
    }
    {
        let ch = w.channel();
        if (*ch).hdr.refs != count_channel_refs(ch, &slots) {
            return false;
        }
    }
    for i in 0..NCSPACES {
        let cs = w.cspace(i);
        if (*cs).hdr.refs != count_cspace_refs(w, cs, &slots) {
            return false;
        }
    }
    for i in 0..NASPACES {
        let a = w.aspace(i);
        if (*a).hdr.refs != count_aspace_refs(w, a, &slots) {
            return false;
        }
    }
    for i in 0..NTHREADS {
        let t = w.tcb(i);
        if (*t).hdr.refs != count_tcb_refs(t, &slots) {
            return false;
        }
    }
    for i in 0..NTIMERS {
        let tm = w.timer(i);
        if (*tm).hdr.refs != count_timer_refs(tm, &slots) {
            return false;
        }
    }
    true
}

/// Set every object's `hdr.refs` to its census value, so a hand-assembled
/// world is refcount-sound by construction. Builders call this after placing
/// caps/bindings/waiters; a harness then runs an op and re-checks soundness.
pub unsafe fn recompute_refs(w: &mut World) {
    let slots = w.collect_slots();
    for i in 0..NNOTIFS {
        let n = w.notif(i);
        (*n).hdr.refs = count_notif_refs(w, n, &slots);
    }
    {
        let ch = w.channel();
        (*ch).hdr.refs = count_channel_refs(ch, &slots);
    }
    for i in 0..NCSPACES {
        let cs = w.cspace(i);
        (*cs).hdr.refs = count_cspace_refs(w, cs, &slots);
    }
    for i in 0..NASPACES {
        let a = w.aspace(i);
        (*a).hdr.refs = count_aspace_refs(w, a, &slots);
    }
    for i in 0..NTHREADS {
        let t = w.tcb(i);
        (*t).hdr.refs = count_tcb_refs(t, &slots);
    }
    for i in 0..NTIMERS {
        let tm = w.timer(i);
        (*tm).hdr.refs = count_timer_refs(tm, &slots);
    }
}
