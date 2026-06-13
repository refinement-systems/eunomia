//! Untyped/retype harnesses (plan §4.2): the carve arithmetic, the CDT
//! install, the rights-inheritance table, and watermark reset.
//!
//! Two genres here. [`carve`](crate::untyped::carve) is pure `u64` math with
//! no pointers and no loops, so its harnesses (`check_carve_no_overflow`,
//! `check_carve_geometry`) run over *fully nondeterministic* inputs with no
//! unwinding — the strongest statement Kani can make: the function is total
//! over all `(base, size, watermark, ty, param)`. The install/reset harnesses
//! exercise the real CDT machinery over a [`BarePool`] of provenance-carrying
//! slots, asserting the §2.3/§2.5 derivation rules on the carved cap.
//!
//! These re-check the TLA `Retype` action on the implementation: a retype
//! installs the new cap as a CDT child of the untyped (so `revoke(untyped)`
//! reaches it), the watermark advances monotonically, and `reset` is gated on
//! the same `Descendants = {}` guard the model uses.

#![cfg(kani)]

use super::world::BarePool;
use crate::channel::{Binding, Channel};
use crate::cspace::{self, Cap, CapKind, ChanEnd, ObjHeader, Rights};
use crate::untyped::{self, ObjType, RetypeError};
use core::ptr;

/// A nondeterministic, valid [`ObjType`] — one of the eight tags.
fn any_objtype() -> ObjType {
    let v: u64 = kani::any();
    kani::assume(v < 8);
    ObjType::from_u64(v).unwrap()
}

/// `check_carve_no_overflow` (plan §4.2, finding UO-1/UO-2): `carve` never
/// panics or overflows for **any** input. No assumptions — `param` is raw
/// user input and `base`/`size`/`watermark` are unconstrained; the hardened
/// `carve` returns `BadArg`/`NoMemory` rather than panicking. Kani checks
/// arithmetic-overflow and panic freedom on every operation, so simply
/// driving the call over nondet inputs is the proof.
#[kani::proof]
fn check_carve_no_overflow() {
    let base: u64 = kani::any();
    let size: u64 = kani::any();
    let watermark: u64 = kani::any();
    let param: u64 = kani::any();
    let ty = any_objtype();
    let _ = untyped::carve(base, size, watermark, ty, param);
}

/// `check_carve_geometry` (plan §4.2): on a *successful* carve the placed
/// range is aligned per type, sits inside `[base + watermark, base + size)`,
/// and a follow-on carve at the bumped watermark is disjoint and strictly
/// advances the watermark — the §2.5 monotone-watermark guarantee.
#[kani::proof]
fn check_carve_geometry() {
    let base: u64 = kani::any();
    let size: u64 = kani::any();
    let watermark: u64 = kani::any();
    let param: u64 = kani::any();
    let ty = any_objtype();

    let Ok(c) = untyped::carve(base, size, watermark, ty, param) else {
        return;
    };
    let align = ty.align();

    // Aligned per type; non-empty; end is start + bytes with no wrap.
    assert!(c.start % align == 0);
    assert!(c.bytes > 0);
    assert!(c.end >= c.start && c.end - c.start == c.bytes);
    // Inside [base + watermark, base + size). carve succeeded, so both adds
    // were checked-Some; CBMC carries that, the unchecked adds here are safe.
    assert!(c.start >= base + watermark);
    assert!(c.end <= base + size);

    // The watermark retype_install would record, then a second carve there:
    // it cannot overlap the first range and the watermark strictly advances.
    let new_wm = c.end - base;
    assert!(new_wm > watermark);
    let p2: u64 = kani::any();
    let ty2 = any_objtype();
    if let Ok(c2) = untyped::carve(base, size, new_wm, ty2, p2) {
        assert!(c2.start >= c.end);
    }
}

/// `check_retype_cdt` (plan §4.2): installing a single object hangs the new
/// cap off the untyped as a CDT child (so a later `revoke(untyped)` reaches
/// it) and advances the watermark to `end - base`.
#[kani::proof]
fn check_retype_cdt() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        let ut = pool.slot(0);
        let dst = pool.slot(1);
        let base: u64 = 0x4000_0000;
        let end: u64 = 0x4000_1000;
        (*ut).cap = Cap {
            kind: CapKind::Untyped { base, size: 0x10_000, watermark: 0x800 },
            rights: Rights::ALL,
        };

        untyped::retype_install(ut, ObjType::Notification, CapKind::Notification(n), end, dst, ptr::null_mut());

        assert!((*dst).parent == ut);
        assert!((*ut).first_child == dst);
        assert!((*dst).prev_sib.is_null());
        match (*ut).cap.kind {
            CapKind::Untyped { watermark, .. } => assert!(watermark == end - base),
            _ => assert!(false, "ut stays Untyped"),
        }
    }
}

/// `check_retype_cdt` channel half (plan §4.2): a channel retype installs
/// *both* endpoint caps as CDT children of the untyped and lands the object
/// at exactly two references (`endpoint_cap_added` × 2, `refs == 2`) — the
/// invariant `endpoint_cap_dropped`'s peer-closed accounting rests on.
#[kani::proof]
fn check_retype_channel() {
    let mut pool = BarePool::new();
    // A bare Channel header (retype's channel dance touches only end_caps and
    // the refcount, never the ring slots, so no trailing array is needed).
    let mut ch = Channel {
        hdr: ObjHeader { refs: 1 }, // endpoint A's cap (Channel::init's post)
        depth: 2,
        end_caps: [0, 0],
        head: [0, 0],
        count: [0, 0],
        bindings: [[Binding { notif: ptr::null_mut(), bits: 0 }; 3]; 2],
    };
    unsafe {
        let chp: *mut Channel = &mut ch;
        let ut = pool.slot(0);
        let dst = pool.slot(1);
        let dst2 = pool.slot(2);
        (*ut).cap = Cap {
            kind: CapKind::Untyped { base: 0x4000_0000, size: 0x10_000, watermark: 0 },
            rights: Rights::ALL,
        };

        untyped::retype_install(
            ut,
            ObjType::Channel,
            CapKind::Channel(chp, ChanEnd::A),
            0x4000_1000,
            dst,
            dst2,
        );

        // Both endpoints installed under the untyped; object at refs == 2.
        assert!((*dst).parent == ut);
        assert!((*dst2).parent == ut);
        assert!((*chp).hdr.refs == 2);
        assert!((*chp).end_caps[0] == 1 && (*chp).end_caps[1] == 1);
        assert!(!(*dst).cap.is_empty() && !(*dst2).cap.is_empty());
    }
}

/// `check_retype_rights` (plan §4.2): the rights-inheritance table. Frames
/// inherit the untyped's rights (so `PHYS` flows only from boot caps along
/// explicit grants); a sub-untyped is masked to `READ|WRITE` and **never**
/// carries `PHYS` (§2.5's by-construction claim, now a proof); threads get
/// `THREAD_ALL`; everything else gets `ALL`. Rights are a function of `ty`
/// and the parent's rights alone (install ignores the kind for this), so a
/// notification kind exercises every arm.
#[kani::proof]
fn check_retype_rights() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        let ut = pool.slot(0);
        let dst = pool.slot(1);
        let r: u8 = kani::any();
        (*ut).cap = Cap {
            kind: CapKind::Untyped { base: 0x4000_0000, size: 0x10_000, watermark: 0 },
            rights: Rights(r),
        };

        // Any type except Channel (two-endpoint case is check_retype_channel).
        let v: u64 = kani::any();
        kani::assume(v < 8 && v != 2);
        let ty = ObjType::from_u64(v).unwrap();

        untyped::retype_install(ut, ty, CapKind::Notification(n), 0x4000_1000, dst, ptr::null_mut());

        let got = (*dst).cap.rights.0;
        match ty {
            ObjType::Frame => assert!(got == r),
            ObjType::Thread => assert!(got == Rights::THREAD_ALL.0),
            ObjType::Untyped => {
                assert!(got == r & (Rights::READ | Rights::WRITE));
                assert!(got & Rights::PHYS == 0); // §2.5: PHYS never on a sub-untyped
            }
            _ => assert!(got == Rights::ALL.0),
        }
    }
}

/// `check_reset` (plan §4.2): a childless untyped resets its watermark to 0,
/// making the whole range reusable (the second half of the §2.5 reclaim
/// primitive).
#[kani::proof]
fn check_reset() {
    let mut pool = BarePool::new();
    unsafe {
        let ut = pool.slot(0);
        let wm: u64 = kani::any();
        (*ut).cap = Cap {
            kind: CapKind::Untyped { base: 0x4000_0000, size: 0x10_000, watermark: wm },
            rights: Rights::ALL,
        };
        assert!(untyped::reset(ut).is_ok());
        match (*ut).cap.kind {
            CapKind::Untyped { watermark, .. } => assert!(watermark == 0),
            _ => assert!(false, "stays Untyped"),
        }
    }
}

/// `check_reset` negative (plan §4.2): reset refuses while children exist —
/// the implementation form of the TLA `Retype` guard `Descendants(c) = {}` /
/// the `untyped_reset` precondition. The watermark is left untouched.
#[kani::proof]
fn check_reset_refuses_children() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        let ut = pool.slot(0);
        let child = pool.slot(1);
        (*ut).cap = Cap {
            kind: CapKind::Untyped { base: 0, size: 0x1000, watermark: 0x800 },
            rights: Rights::ALL,
        };
        (*child).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        cspace::cdt_insert_child(ut, child);

        assert!(untyped::reset(ut) == Err(RetypeError::BadArg));
        match (*ut).cap.kind {
            CapKind::Untyped { watermark, .. } => assert!(watermark == 0x800),
            _ => assert!(false, "stays Untyped"),
        }
    }
}

/// `check_reset` type guard (plan §4.2): reset of a non-untyped cap errs.
#[kani::proof]
fn check_reset_refuses_not_untyped() {
    let mut pool = BarePool::new();
    unsafe {
        let n = pool.notif_ptr();
        let ut = pool.slot(0);
        (*ut).cap = Cap { kind: CapKind::Notification(n), rights: Rights::ALL };
        assert!(untyped::reset(ut) == Err(RetypeError::NotUntyped));
    }
}
