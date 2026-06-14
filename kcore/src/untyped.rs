//! Untyped memory and retype (spec §1, §2.5, §3.2).
//!
//! An untyped cap names a physical range and carries a watermark; retype
//! carves the next object out of the range and installs the new cap as a
//! CDT child of the untyped cap. The watermark only ever advances —
//! reclaiming the range is `revoke(untyped)` (which deletes every object
//! cap derived from it) followed by watermark reset, proving exclusivity
//! exactly as the TLA+ Retype guard requires.
//!
//! Retype is the system's one int→pointer boundary (plan §2.3): kcore owns
//! the pure validation ([`retype_check`]) and placement ([`carve`])
//! arithmetic and the CDT install ([`retype_install`]); the `kernel` crate's
//! `retype` composes them and performs the `start as *mut T` object
//! construction in between, where CBMC never sees it. Every kernel object is
//! created this way: the kernel has no global pool (§3.2). The one exception
//! is the statically allocated root cspace, which is morally init's memory
//! baked into the image.

use crate::cspace::{self, Cap, CapKind, ChanEnd, CSpaceObj, Rights};
use crate::id::SlotId;
use crate::store::Store;
use vstd::prelude::*;

verus! {

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjType {
    CSpace,
    Thread,
    Channel,
    Notification,
    Timer,
    /// param = page count; 4 KiB-aligned, zeroed at retype.
    Frame,
    /// param = table-pool pages (pool-at-creation, §2.5).
    Aspace,
    /// A sub-range untyped (§2.3: untyped derivations are page-aligned
    /// sub-ranges). param = bytes, rounded up to a page. The carved cap
    /// is a CDT child of the parent untyped with its own watermark, so a
    /// whole subtree of objects can be retyped from it and reclaimed as a
    /// unit by `revoke(child) + reset(child)` — the per-spawn donation a
    /// parent funds for one child (§5.1).
    Untyped,
}

} // verus!

impl ObjType {
    pub fn from_u64(v: u64) -> Option<ObjType> {
        Some(match v {
            0 => ObjType::CSpace,
            1 => ObjType::Thread,
            2 => ObjType::Channel,
            3 => ObjType::Notification,
            4 => ObjType::Timer,
            5 => ObjType::Frame,
            6 => ObjType::Aspace,
            7 => ObjType::Untyped,
            _ => return None,
        })
    }

}

verus! {

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetypeError {
    NotUntyped,
    DestOccupied,
    NoMemory,
    BadArg,
}

/// Geometry of a carved object: the placed range and its size. Pure output
/// of [`carve`]; the int→ptr conversion that turns `start` into an object
/// pointer is the caller's job (plan §2.3 — the one sanctioned boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Carve {
    pub start: u64,
    pub end: u64,
    pub bytes: u64,
}

} // verus!

/// Slot-state half of retype's validation: `ut_slot` must hold an Untyped
/// cap, `dst` (and `dst2` for channels) must be empty and detached. Runs
/// before [`carve`], so the error precedence is NotUntyped → DestOccupied →
/// (BadArg | NoMemory).
///
/// post: returns the untyped's `(base, size, watermark)` unchanged.
pub fn retype_check<S: Store>(
    store: &mut S,
    ut_slot: SlotId,
    ty: ObjType,
    dst: SlotId,
    dst2: Option<SlotId>,
) -> Result<(u64, u64, u64), RetypeError> {
    let CapKind::Untyped { base, size, watermark } = store.slot(ut_slot).cap.kind else {
        return Err(RetypeError::NotUntyped);
    };
    if !store.slot(dst).cap.is_empty() {
        return Err(RetypeError::DestOccupied);
    }
    if ty == ObjType::Channel {
        match dst2 {
            Some(d2) if d2 != dst && store.slot(d2).cap.is_empty() => {}
            _ => return Err(RetypeError::DestOccupied),
        }
    }
    Ok((base, size, watermark))
}

verus! {

impl ObjType {
    /// The object's required alignment as a ghost value, so [`align`](Self::align)
    /// can be used in `carve`'s `ensures` (via `when_used_as_spec`).
    pub open spec fn spec_align(self) -> u64 {
        match self {
            ObjType::Frame => 4096,
            ObjType::Aspace => 4096,
            ObjType::Untyped => 4096,
            _ => 16,
        }
    }

    #[verifier::when_used_as_spec(spec_align)]
    pub(crate) fn align(self) -> (r: u64)
        ensures r == self.spec_align(),
    {
        match self {
            ObjType::Frame | ObjType::Aspace | ObjType::Untyped => 4096,
            _ => 16,
        }
    }
}

// Trusted boundary (plan doc/plans/3_verus-rewrite.md, phase 0): the per-object
// size helpers are not yet ported, so `carve` trusts only that they return a
// `usize` — their own overflow/positivity proofs land when cspace/channel/aspace
// port (phases 2–5). The single fact `carve`'s geometry needs from them,
// `0 < bytes`, is taken as an explicit `assume` at the trusted boundary below,
// not from these (deliberately empty) specs.
pub assume_specification [ CSpaceObj::bytes_for ](n: u32) -> usize;
pub assume_specification [ crate::channel::Channel::bytes_for ](n: u32) -> usize;
pub assume_specification [ crate::aspace::AspaceObj::bytes_for ](n: u64) -> usize;

// The fixed-size object arms take `core::mem::size_of::<T>()` of these kcore
// types, which live outside `verus!{}` (their own ports come in phases 2–5);
// register them as opaque so `size_of` typechecks in the verified `carve`.
// (`allow(dead_code)`: these wrappers are Verus-only scaffolding — after the
// macro erases ghost code in a normal build they are unread tuple structs.)
#[verifier::external_type_specification]
#[verifier::external_body]
#[allow(dead_code)]
pub struct ExTcb(crate::thread::Tcb);
#[verifier::external_type_specification]
#[verifier::external_body]
#[allow(dead_code)]
pub struct ExNotifObj(crate::notification::NotifObj);
#[verifier::external_type_specification]
#[verifier::external_body]
#[allow(dead_code)]
pub struct ExTimerObj(crate::timer::TimerObj);

// vstd has no spec for `checked_next_multiple_of` yet; trust its signature (the
// Untyped arm only needs that it returns an `Option`, then re-checks positivity).
pub assume_specification [ usize::checked_next_multiple_of ](
    a: usize,
    b: usize,
) -> Option<usize>;

/// The pure placement core: round `base + watermark` up to `align`, place
/// `bytes` there, and bounds-check against `[base, base + size)`. All `u64`
/// arithmetic, no pointers, no `usize`, no external calls — so Verus proves it
/// **total** (no panic/overflow for any inputs) and fully functional for **all**
/// `(base, size, watermark, align ∈ {16,4096}, bytes > 0)`. `carve` (below)
/// computes `bytes`/`align` and forwards here; this split keeps the geometry
/// proof free of the size-helper trusted boundary (plan §4.2 / phase 0).
///
/// The monotone-watermark/disjointness property the old Kani harness needed a
/// *second* carve to assert is now a free corollary of the containment `ensures`:
/// a follow-on carve at `new_wm = end - base` has `start' >= base + new_wm = end`.
pub fn carve_place(
    base: u64,
    size: u64,
    watermark: u64,
    align: u64,
    bytes: u64,
) -> (result: Result<Carve, RetypeError>)
    requires
        align == 16 || align == 4096,
        bytes > 0,
    ensures
        match result {
            Ok(c) => {
                &&& c.bytes == bytes
                &&& c.start % align == 0
                &&& c.start <= c.end
                &&& c.end - c.start == bytes
                &&& base + watermark <= c.start
                &&& c.end <= base + size
                &&& watermark < c.end - base
            }
            Err(_) => true,
        },
{
    let bpw = match base.checked_add(watermark) {
        Some(x) => x,
        None => return Err(RetypeError::NoMemory),
    };
    let s = match bpw.checked_add(align - 1) {
        Some(x) => x,
        None => return Err(RetypeError::NoMemory),
    };
    // Round `s = bpw + (align - 1)` down to a multiple of `align` — equivalently,
    // round `bpw` up. The mod/sub form is the verification-friendly equivalent of
    // a `& !(align - 1)` mask for a power-of-two align: no `!`, and no overflow
    // obligation (rem <= s, so the subtraction never underflows). `align != 0`
    // holds from the `requires`.
    let rem = s % align;
    let start = s - rem;
    proof {
        // Verus's built-in div/mod axioms give 0 <= rem < align and
        // s == align * (s / align) + rem, so start == align * (s / align): a
        // multiple of align, with s - start == rem <= align - 1. Combined with
        // s == bpw + (align - 1) (the checked_add result) this yields bpw <= start.
        assert(start % align == 0) by (nonlinear_arith)
            requires align > 0, rem == s % align, start == s - rem;
    }
    let end = match start.checked_add(bytes) {
        Some(e) => e,
        None => return Err(RetypeError::NoMemory),
    };
    let limit = match base.checked_add(size) {
        Some(l) => l,
        None => return Err(RetypeError::NoMemory),
    };
    if end > limit {
        return Err(RetypeError::NoMemory);
    }
    Ok(Carve { start, end, bytes })
}

/// Pure placement arithmetic: object size from `(ty, param)`, alignment,
/// watermark bump, bounds against `[base, base + size)`. No pointers.
///
/// Verus proves this **total** — no panic, no arithmetic overflow for **any**
/// `(base, size, watermark, ty, param)` (the UO-1/UO-2 findings as a theorem,
/// for all inputs, not bounded) — and forwards the geometry guarantees of
/// [`carve_place`]. The size helpers are a trusted boundary (`0 < bytes`); the
/// `param` arithmetic (`param * 4096`, the page round-up) is verified here.
///
/// `param` arrives raw from user register `a[2]` (`syscall.rs`); every step that
/// touches it is checked, so a pathological input yields `BadArg`/`NoMemory`,
/// never a user-triggerable kernel panic.
pub fn carve(
    base: u64,
    size: u64,
    watermark: u64,
    ty: ObjType,
    param: u64,
) -> (result: Result<Carve, RetypeError>)
    ensures
        match result {
            Ok(c) => {
                &&& c.bytes > 0
                &&& c.start <= c.end
                &&& c.end - c.start == c.bytes
                &&& c.start % ty.spec_align() == 0
                &&& base + watermark <= c.start
                &&& c.end <= base + size
                &&& watermark < c.end - base
            }
            Err(_) => true,
        },
{
    let bytes: u64 = match ty {
        ObjType::CSpace => {
            if param == 0 || param > 1024 {
                return Err(RetypeError::BadArg);
            }
            CSpaceObj::bytes_for(param as u32) as u64
        }
        ObjType::Thread => core::mem::size_of::<crate::thread::Tcb>() as u64,
        ObjType::Channel => {
            if param == 0 || param > 256 {
                return Err(RetypeError::BadArg);
            }
            crate::channel::Channel::bytes_for(param as u32) as u64
        }
        ObjType::Notification => core::mem::size_of::<crate::notification::NotifObj>() as u64,
        ObjType::Timer => core::mem::size_of::<crate::timer::TimerObj>() as u64,
        ObjType::Frame => {
            if param == 0 || param > 65536 {
                return Err(RetypeError::BadArg);
            }
            // param <= 65536, so param * 4096 <= 2^28 — Verus proves no overflow.
            param * 4096
        }
        ObjType::Aspace => {
            if param == 0 || param > 256 {
                return Err(RetypeError::BadArg);
            }
            crate::aspace::AspaceObj::bytes_for(param) as u64
        }
        ObjType::Untyped => {
            // param is bytes; round up to a page so the carved range is
            // page-aligned at both ends (§2.3). 0 is meaningless; a param
            // within a page of the address space top has no rounded size.
            if param == 0 {
                return Err(RetypeError::BadArg);
            }
            match (param as usize).checked_next_multiple_of(4096) {
                Some(b) => b as u64,
                None => return Err(RetypeError::BadArg),
            }
        }
    };
    // Trusted boundary (see the assume_specification note above): every size
    // helper returns a positive byte count. Carve's geometry needs only this.
    assume(bytes > 0);
    carve_place(base, size, watermark, ty.align(), bytes)
}

} // verus!

/// Install half: advance the untyped's watermark, set the new cap's rights
/// per the inheritance table, link it as a CDT child, and run the channel
/// two-endpoint dance. All checks already passed; this is infallible.
///
/// pre:  `ut_slot` still holds the Untyped cap [`retype_check`] returned;
///       `kind` was built at `carve.start`; `end == carve.end`.
/// post: watermark = `end - base`; dst holds the cap; object refs == caps
///       installed (1, or 2 for channels).
pub fn retype_install<S: Store>(
    store: &mut S,
    ut_slot: SlotId,
    ty: ObjType,
    kind: CapKind,
    end: u64,
    dst: SlotId,
    dst2: Option<SlotId>,
) {
    let mut ut = store.slot(ut_slot);
    let CapKind::Untyped { base, size, .. } = ut.cap.kind else {
        // Unreachable: retype_check established this and nothing mutates
        // ut_slot's kind between check and install.
        return;
    };
    ut.cap.kind = CapKind::Untyped {
        base,
        size,
        watermark: end - base,
    };
    // Frames inherit the untyped's rights so phys-read (§2.5) flows only
    // from boot untypeds along explicit grants; threads carry the full
    // §2.3 thread-rights set on the creator cap (attenuation strips from
    // here); other kernel objects get the ordinary full mask. A carved
    // sub-untyped inherits read/write but never phys-read: a spawn pool
    // funds child memory, not DMA authority — stripping here keeps phys
    // off ordinary derivation chains (§2.5) by construction.
    let rights = match ty {
        ObjType::Frame => ut.cap.rights,
        ObjType::Thread => Rights::THREAD_ALL,
        ObjType::Untyped => ut.cap.rights.masked(Rights::READ | Rights::WRITE),
        _ => Rights::ALL,
    };
    store.set_slot(ut_slot, ut);
    let mut d = store.slot(dst);
    d.cap = Cap { kind, rights };
    store.set_slot(dst, d);
    cspace::cdt_insert_child(store, ut_slot, dst);
    if let CapKind::Channel(ch, _) = kind {
        crate::channel::endpoint_cap_added(store, ch, ChanEnd::A);
        // dst2 is Some for channels (retype_check enforced it).
        if let Some(d2) = dst2 {
            let mut s2 = store.slot(d2);
            s2.cap = Cap {
                kind: CapKind::Channel(ch, ChanEnd::B),
                rights: Rights::ALL,
            };
            store.set_slot(d2, s2);
            store.set_obj_refs(ch, store.obj_refs(ch) + 1);
            cspace::cdt_insert_child(store, ut_slot, d2);
            crate::channel::endpoint_cap_added(store, ch, ChanEnd::B);
        }
    }
}

/// Reset the watermark once exclusivity is proven — the second half of the
/// reclaim primitive (§2.5: "reclaiming the range is revoke(untyped) then
/// watermark reset"). A parent reuses one child-sized donation across many
/// spawns this way (§5.1); the next retype re-zeroes the frames it carves.
///
/// pre:  ut_slot holds an Untyped cap with no CDT children (caller revoked).
/// post: watermark = 0; the whole range is reusable.
pub fn reset<S: Store>(store: &mut S, ut_slot: SlotId) -> Result<(), RetypeError> {
    let mut ut = store.slot(ut_slot);
    let CapKind::Untyped { base, size, .. } = ut.cap.kind else {
        return Err(RetypeError::NotUntyped);
    };
    if ut.first_child.is_some() {
        return Err(RetypeError::BadArg);
    }
    ut.cap.kind = CapKind::Untyped { base, size, watermark: 0 };
    store.set_slot(ut_slot, ut);
    Ok(())
}
