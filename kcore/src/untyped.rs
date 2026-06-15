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
// `StoreSpec` carries the `Store` ghost-view extension (`slot_view`/`refs_view`/
// `chan_view`) the §3a/§3c contracts quantify over. Only referenced from
// `requires`/`ensures`, which erase in a normal build — hence unused there. The spec
// `fn`s (`cspace_wf`/`is_empty_cap`/`reset_slot`) and the proof lemma are reached by
// **full path** inside contracts/proofs: they erase to nothing in a normal build, so
// a `use` of them would be an unresolved import there. (The channel postcondition
// reads `chan_view()` fields directly, so the `ChanView` type name is never written.)
#[allow(unused_imports)]
use crate::cspace::StoreSpec;
use crate::id::SlotId;
// `ObjId` appears only in the §3c channel postcondition (the `forall|o: ObjId|`
// other-channels-untouched frame), which erases in a normal build — hence unused there.
#[allow(unused_imports)]
use crate::id::ObjId;
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

impl ObjType {
    /// Ghost model of [`from_u64`](Self::from_u64): the discriminant decode as a
    /// total `spec fn`, so the exec decoder can be used in `requires`/`ensures`
    /// (via `when_used_as_spec`) and the §4.6 "`from_u64` total" obligation is a
    /// theorem. `Some` for exactly the eight valid discriminants `0..8`, `None`
    /// otherwise — the characterization `decode` leans on for its BadObjType arm.
    pub open spec fn spec_from_u64(v: u64) -> Option<ObjType> {
        match v {
            0 => Some(ObjType::CSpace),
            1 => Some(ObjType::Thread),
            2 => Some(ObjType::Channel),
            3 => Some(ObjType::Notification),
            4 => Some(ObjType::Timer),
            5 => Some(ObjType::Frame),
            6 => Some(ObjType::Aspace),
            7 => Some(ObjType::Untyped),
            _ => None,
        }
    }

    #[verifier::when_used_as_spec(spec_from_u64)]
    pub fn from_u64(v: u64) -> (r: Option<ObjType>)
        ensures r == Self::spec_from_u64(v),
    {
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

    /// `from_u64` is a left inverse of the discriminant cast: every `ObjType`
    /// round-trips through its `u64` discriminant. The enum is field-less, so
    /// `ty as u64` is the discriminant `0..8` and the match in `spec_from_u64`
    /// recovers it. Stated as a `proof fn` so callers can invoke it where the
    /// round-trip is needed (none yet — `decode` only needs the `None`-iff-`v>=8`
    /// direction — but it pins the encode/decode pairing as a theorem).
    pub proof fn lemma_from_u64_roundtrip(ty: ObjType)
        ensures Self::spec_from_u64(ty as u64) == Some(ty),
    {
        match ty {
            ObjType::CSpace => {}
            ObjType::Thread => {}
            ObjType::Channel => {}
            ObjType::Notification => {}
            ObjType::Timer => {}
            ObjType::Frame => {}
            ObjType::Aspace => {}
            ObjType::Untyped => {}
        }
    }
}

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

verus! {

/// Slot-state half of retype's validation: `ut_slot` must hold an Untyped
/// cap, `dst` (and `dst2` for channels) must be empty and detached. Runs
/// before [`carve`], so the error precedence is NotUntyped → DestOccupied →
/// (BadArg | NoMemory).
///
/// Verified (plan doc/plans/3_verus-rewrite_phase3-detail.md §3a): pure
/// `slot_view` reasoning, no channel/notification coupling. It calls no `set_*`,
/// so the store is provably unchanged on **every** path; on `Ok` the returned
/// triple is the untyped's geometry and the destination(s) are empty and
/// distinct. The `Err` arms pin the precedence (NotUntyped before DestOccupied).
pub fn retype_check<S: Store>(
    store: &mut S,
    ut_slot: SlotId,
    ty: ObjType,
    dst: SlotId,
    dst2: Option<SlotId>,
) -> (result: Result<(u64, u64, u64), RetypeError>)
    requires
        old(store).slot_view().dom().contains(ut_slot),
        old(store).slot_view().dom().contains(dst),
        dst2 matches Some(d2) ==> old(store).slot_view().dom().contains(d2),
    ensures
        final(store).slot_view() == old(store).slot_view(),
        final(store).refs_view() == old(store).refs_view(),
        (result matches Ok((b, s, w)) ==> (
            old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { base, size, watermark }
                && b == base && s == size && w == watermark
                && crate::cspace::is_empty_cap(old(store).slot_view()[dst].cap)
                && (ty == ObjType::Channel ==> (
                        dst2 matches Some(d2) && d2 != dst
                            && crate::cspace::is_empty_cap(old(store).slot_view()[d2].cap)))
        )),
        (result matches Err(RetypeError::NotUntyped) ==>
            !(old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { .. })),
        (result matches Err(RetypeError::DestOccupied) ==> (
            old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { .. }
                && (!crate::cspace::is_empty_cap(old(store).slot_view()[dst].cap)
                    || (ty == ObjType::Channel
                        && !(dst2 matches Some(d2) && d2 != dst
                                && crate::cspace::is_empty_cap(old(store).slot_view()[d2].cap))))
        )),
{
    let (base, size, watermark) = match store.slot(ut_slot).cap.kind {
        CapKind::Untyped { base, size, watermark } => (base, size, watermark),
        _ => return Err(RetypeError::NotUntyped),
    };
    if !matches!(store.slot(dst).cap.kind, CapKind::Empty) {
        return Err(RetypeError::DestOccupied);
    }
    if matches!(ty, ObjType::Channel) {
        match dst2 {
            Some(d2) => {
                if d2.0 == dst.0 || !matches!(store.slot(d2).cap.kind, CapKind::Empty) {
                    return Err(RetypeError::DestOccupied);
                }
            }
            None => return Err(RetypeError::DestOccupied),
        }
    }
    Ok((base, size, watermark))
}

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

verus! {

/// Install half: advance the untyped's watermark, set the new cap's rights
/// per the inheritance table, link it as a CDT child, and run the channel
/// two-endpoint dance. All checks already passed; this is infallible.
///
/// Verified (plan doc/plans/3_verus-rewrite_phase3-detail.md §3c; doc/results/28).
/// The §2.5 rights-inheritance table is proven as theorems: a Frame inherits the
/// untyped's rights (so phys-read flows only along boot untypeds); a Thread carries
/// `THREAD_ALL`; a carved sub-Untyped is masked to `READ|WRITE` and so **provably
/// never carries `PHYS`** — phys stays off ordinary derivation chains by
/// construction, now ∀ rather than asserted; every other object gets `ALL`. The new
/// cap is a CDT child of the untyped (verified `cdt_insert_child`) and `cspace_wf`
/// is preserved. The channel arm installs endpoint B in `dst2`, bumps the channel
/// refcount to 2, and accounts both ends (verified `endpoint_cap_added`), leaving
/// the freshly-carved channel with `end_caps == [1, 1]`.
///
/// pre:  `ut_slot` still holds the Untyped cap [`retype_check`] returned, with
///       `base <= end`; `kind` (non-Empty) was built at `carve.start`;
///       `end == carve.end`; `dst` (and, for a channel, `dst2`) is empty; a
///       freshly-`init`'d channel has refs 1 and `end_caps == [0, 0]`.
/// post: watermark = `end - base`; `dst` holds `Cap { kind, <table rights> }` as a
///       CDT child of `ut_slot`; refs/end_caps account the installed cap(s)
///       (non-channel: refs/chan untouched — the object's `init` pre-counts `dst`;
///       channel: refs 2, both ends accounted, `dst2` = endpoint B).
pub fn retype_install<S: Store>(
    store: &mut S,
    ut_slot: SlotId,
    ty: ObjType,
    kind: CapKind,
    end: u64,
    dst: SlotId,
    dst2: Option<SlotId>,
)
    requires
        crate::cspace::cspace_wf(old(store).slot_view()),
        old(store).slot_view().dom().contains(ut_slot),
        old(store).slot_view().dom().contains(dst),
        old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { .. },
        (old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { base, size, watermark }
            ==> base <= end),
        crate::cspace::is_empty_cap(old(store).slot_view()[dst].cap),
        !(kind matches CapKind::Empty),
        (kind matches CapKind::Channel(ch, _) ==> (
            dst2 matches Some(d2)
                && d2 != dst
                && old(store).slot_view().dom().contains(d2)
                && crate::cspace::is_empty_cap(old(store).slot_view()[d2].cap)
                && old(store).chan_view().dom().contains(ch)
                && old(store).refs_view().dom().contains(ch)
                && old(store).chan_view()[ch].end_caps.len() == 2
                && old(store).chan_view()[ch].end_caps[0] == 0
                && old(store).chan_view()[ch].end_caps[1] == 0
                && old(store).refs_view()[ch] == 1
        )),
    ensures
        final(store).slot_view().dom() == old(store).slot_view().dom(),
        // watermark advanced to `end - base`.
        (old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { base, size, watermark }
            ==> final(store).slot_view()[ut_slot].cap.kind
                == CapKind::Untyped { base, size, watermark: (end - base) as u64 }),
        // `dst` holds the new cap as a CDT child of `ut_slot`.
        final(store).slot_view()[dst].cap.kind == kind,
        final(store).slot_view()[dst].parent == Some(ut_slot),
        // §2.5 rights-inheritance table, as theorems keyed on `ty`.
        (ty == ObjType::Frame ==> final(store).slot_view()[dst].cap.rights.0
            == old(store).slot_view()[ut_slot].cap.rights.0),
        (ty == ObjType::Thread ==> final(store).slot_view()[dst].cap.rights.0 == Rights::THREAD_ALL.0),
        (ty == ObjType::Untyped ==> (
            final(store).slot_view()[dst].cap.rights.0
                == (old(store).slot_view()[ut_slot].cap.rights.0 & (Rights::READ | Rights::WRITE))
            && (final(store).slot_view()[dst].cap.rights.0 & Rights::PHYS) == 0
        )),
        ((ty != ObjType::Frame && ty != ObjType::Thread && ty != ObjType::Untyped)
            ==> final(store).slot_view()[dst].cap.rights.0 == Rights::ALL.0),
        crate::cspace::cspace_wf(final(store).slot_view()),
        // refcount / chan_view deltas. Non-channel: untouched (the object's `init`
        // pre-counts `dst`, so there is no bump here).
        (!(kind matches CapKind::Channel(_, _)) ==> (
            final(store).refs_view() == old(store).refs_view()
            && final(store).chan_view() == old(store).chan_view()
        )),
        // Channel: refs → 2, both ends accounted (end_caps [1, 1]), other channels
        // untouched, `dst2` installed as endpoint B.
        (kind matches CapKind::Channel(ch, _) ==> (
            final(store).refs_view() == old(store).refs_view().insert(ch, 2 as nat)
            && final(store).chan_view().dom() == old(store).chan_view().dom()
            && final(store).chan_view()[ch].end_caps.len() == 2
            && final(store).chan_view()[ch].end_caps[0] == 1
            && final(store).chan_view()[ch].end_caps[1] == 1
            && (forall|o: ObjId| #[trigger] old(store).chan_view().dom().contains(o) && o != ch
                    ==> final(store).chan_view()[o] == old(store).chan_view()[o])
            && (dst2 matches Some(d2) ==> (
                final(store).slot_view()[d2].cap.kind == CapKind::Channel(ch, ChanEnd::B)
                && final(store).slot_view()[d2].cap.rights.0 == Rights::ALL.0
                && final(store).slot_view()[d2].parent == Some(ut_slot)
            ))
        )),
{
    let ghost m0 = store.slot_view();
    let ghost rv0 = store.refs_view();
    let ghost cv0 = store.chan_view();

    let mut ut = store.slot(ut_slot);
    let ghost ut_rights = ut.cap.rights;
    let (base, size) = match ut.cap.kind {
        CapKind::Untyped { base, size, .. } => (base, size),
        _ => {
            // Unreachable: the precondition pins `ut_slot` to an Untyped cap.
            assert(false);
            return;
        }
    };
    assert(base <= end);
    ut.cap.kind = CapKind::Untyped { base, size, watermark: end - base };
    let rights = match ty {
        ObjType::Frame => ut.cap.rights,
        ObjType::Thread => Rights::THREAD_ALL,
        ObjType::Untyped => ut.cap.rights.masked(Rights::READ | Rights::WRITE),
        _ => Rights::ALL,
    };
    proof {
        // §2.5 sub-untyped-never-PHYS: masking to READ|WRITE clears the PHYS bit for
        // every possible rights value — the theorem, ∀, not a sampled assert. (The
        // bit-vector tactic needs a plain `u8`, so bind `ut_rights.0` to `b` first.)
        assert(Rights::READ | Rights::WRITE == 3u8) by (compute);
        assert(Rights::PHYS == 4u8) by (compute);
        let b = ut_rights.0;
        assert((b & 3u8) & 4u8 == 0u8) by (bit_vector);
    }
    store.set_slot(ut_slot, ut);
    let ghost m_u = m0.insert(ut_slot, ut);
    proof {
        // Watermark bump: links + emptiness fixed, so `cspace_wf` carries.
        crate::cspace::lemma_local_cap_edit_preserves_cspace_wf(m0, ut_slot, ut);
        assert(store.slot_view() =~= m_u);
    }

    // `dst` is empty in m0, untouched by the ut_slot edit (dst != ut_slot: dst is
    // empty, ut_slot is not), so it is still empty and detached here.
    assert(dst != ut_slot);
    let mut d = store.slot(dst);
    d.cap = Cap { kind, rights };
    store.set_slot(dst, d);
    proof {
        // Detached fill: `dst` was empty (hence all-None links), now non-empty with
        // links still None — `cspace_wf` carries.
        crate::cspace::lemma_local_cap_edit_preserves_cspace_wf(m_u, dst, d);
        assert(store.slot_view() =~= m_u.insert(dst, d));
    }
    cspace::cdt_insert_child(store, ut_slot, dst);

    // After the first insert: `dst` is parented at `ut_slot` and holds `Cap{kind,
    // rights}`; `ut_slot` still holds the watermark-bumped Untyped; refs/chan are
    // unchanged from entry. Capture this so the channel arm can show it survives.
    let ghost m_a = store.slot_view();
    proof {
        assert(m_a[dst].cap == d.cap);
        assert(m_a[dst].parent == Some(ut_slot));
        assert(m_a[ut_slot].cap == ut.cap);
        assert(m_a[ut_slot].first_child == Some(dst));
        assert(store.refs_view() =~= rv0);
        assert(store.chan_view() =~= cv0);
    }

    if let CapKind::Channel(ch, _) = kind {
        crate::channel::endpoint_cap_added(store, ch, ChanEnd::A);
        // dst2 is Some for channels (retype_check enforced it; the precondition
        // carries it here).
        if let Some(d2) = dst2 {
            // endpoint_cap_added framed `slot_view`, so the arena is still `m_a`; d2
            // is in its domain (a cap-only edit of the read keeps d2's links, and the
            // lemma needs no fact about d2's old cap — it is non-empty after the fill).
            assert(store.slot_view() =~= m_a);
            assert(m_a.dom().contains(d2));
            let mut s2 = store.slot(d2);
            s2.cap = Cap { kind: CapKind::Channel(ch, ChanEnd::B), rights: Rights::ALL };
            store.set_slot(d2, s2);
            proof {
                crate::cspace::lemma_local_cap_edit_preserves_cspace_wf(m_a, d2, s2);
                assert(store.slot_view() =~= m_a.insert(d2, s2));
            }
            // refs[ch] is still 1 here (set_slot/cdt_insert_child/endpoint frame it).
            assert(store.refs_view()[ch] == 1);
            store.set_obj_refs(ch, store.obj_refs(ch) + 1);
            cspace::cdt_insert_child(store, ut_slot, d2);
            crate::channel::endpoint_cap_added(store, ch, ChanEnd::B);

            // ── Close the channel-arm postconditions ──
            proof {
                // dst survived both later inserts: its cap is preserved (cdt_insert_
                // child keeps all caps; endpoint/set_obj_refs frame slot_view) and its
                // parent is preserved (the second insert's old-first-child = dst keeps
                // its parent — the cdt_insert_child frame clause).
                assert(store.slot_view()[dst].cap == d.cap);
                assert(store.slot_view()[dst].parent == Some(ut_slot));
                assert(store.slot_view()[ut_slot].cap == ut.cap);
                // d2 is endpoint B, parented at ut_slot, rights ALL.
                assert(store.slot_view()[d2].cap == s2.cap);
                assert(store.slot_view()[d2].parent == Some(ut_slot));
                // end_caps: [0,0] →(A) [1,0] →(B) [1,1]; refs: 1 →(set) 2.
                assert(store.chan_view()[ch].end_caps[0] == 1);
                assert(store.chan_view()[ch].end_caps[1] == 1);
                assert(store.refs_view() =~= rv0.insert(ch, 2 as nat));
                assert(store.chan_view().dom() == cv0.dom());
                assert forall|o: ObjId| #[trigger] cv0.dom().contains(o) && o != ch
                    implies store.chan_view()[o] == cv0[o] by {}
            }
        }
    }
}

} // verus!

verus! {

/// The slot `reset` produces: an Untyped slot's watermark zeroed, every other
/// field (rights + CDT links) unchanged; a non-Untyped slot is left as-is (the
/// `Ok` arm is unreachable for it, so this branch is only a totality fallback).
pub open spec fn reset_slot(s: crate::cspace::CapSlot) -> crate::cspace::CapSlot {
    crate::cspace::CapSlot {
        cap: Cap {
            kind: match s.cap.kind {
                CapKind::Untyped { base, size, watermark } =>
                    CapKind::Untyped { base, size, watermark: 0 },
                _ => s.cap.kind,
            },
            rights: s.cap.rights,
        },
        parent: s.parent,
        first_child: s.first_child,
        next_sib: s.next_sib,
        prev_sib: s.prev_sib,
    }
}

/// Reset the watermark once exclusivity is proven — the second half of the
/// reclaim primitive (§2.5: "reclaiming the range is revoke(untyped) then
/// watermark reset"). A parent reuses one child-sized donation across many
/// spawns this way (§5.1); the next retype re-zeroes the frames it carves.
///
/// pre:  ut_slot holds an Untyped cap with no CDT children (caller revoked).
/// post: watermark = 0; the whole range is reusable.
///
/// Verified (plan §3a). It mirrors [`retype_check`]'s read-only-on-error
/// discipline: the contract is stated per-arm rather than via a
/// `requires`-Untyped (which would make the NotUntyped path dead and drop its
/// store-unchanged guarantee). On `Ok` the arena differs from entry by exactly
/// `reset_slot` at `ut_slot`; both `Err` arms leave it untouched.
pub fn reset<S: Store>(store: &mut S, ut_slot: SlotId) -> (result: Result<(), RetypeError>)
    requires
        old(store).slot_view().dom().contains(ut_slot),
    ensures
        final(store).refs_view() == old(store).refs_view(),
        (result is Ok ==> (
            old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { .. }
                && old(store).slot_view()[ut_slot].first_child is None
                && final(store).slot_view()
                    == old(store).slot_view().insert(ut_slot, reset_slot(old(store).slot_view()[ut_slot]))
        )),
        (result matches Err(RetypeError::NotUntyped) ==> (
            !(old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { .. })
                && final(store).slot_view() == old(store).slot_view()
        )),
        (result matches Err(RetypeError::BadArg) ==> (
            old(store).slot_view()[ut_slot].cap.kind matches CapKind::Untyped { .. }
                && old(store).slot_view()[ut_slot].first_child is Some
                && final(store).slot_view() == old(store).slot_view()
        )),
{
    let mut ut = store.slot(ut_slot);
    let (base, size) = match ut.cap.kind {
        CapKind::Untyped { base, size, .. } => (base, size),
        _ => return Err(RetypeError::NotUntyped),
    };
    if ut.first_child.is_some() {
        return Err(RetypeError::BadArg);
    }
    ut.cap = Cap { kind: CapKind::Untyped { base, size, watermark: 0 }, rights: ut.cap.rights };
    proof {
        assert(ut == reset_slot(old(store).slot_view()[ut_slot]));
    }
    store.set_slot(ut_slot, ut);
    Ok(())
}

} // verus!
