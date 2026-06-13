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

use crate::cspace::{self, Cap, CapKind, CapSlot, CSpaceObj, Rights};

#[derive(Clone, Copy, PartialEq, Eq)]
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

    pub(crate) fn align(self) -> u64 {
        match self {
            ObjType::Frame | ObjType::Aspace | ObjType::Untyped => 4096,
            _ => 16,
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

/// Slot-state half of retype's validation: `ut_slot` must hold an Untyped
/// cap, `dst` (and `dst2` for channels) must be empty and detached. Runs
/// before [`carve`], so the error precedence is NotUntyped → DestOccupied →
/// (BadArg | NoMemory).
///
/// post: returns the untyped's `(base, size, watermark)` unchanged.
pub unsafe fn retype_check(
    ut_slot: *mut CapSlot,
    ty: ObjType,
    dst: *mut CapSlot,
    dst2: *mut CapSlot,
) -> Result<(u64, u64, u64), RetypeError> {
    let CapKind::Untyped { base, size, watermark } = (*ut_slot).cap.kind else {
        return Err(RetypeError::NotUntyped);
    };
    if !(*dst).cap.is_empty() {
        return Err(RetypeError::DestOccupied);
    }
    if ty == ObjType::Channel {
        if dst2.is_null() || dst2 == dst || !(*dst2).cap.is_empty() {
            return Err(RetypeError::DestOccupied);
        }
    }
    Ok((base, size, watermark))
}

/// Pure placement arithmetic: object size from `(ty, param)`, alignment,
/// watermark bump, bounds against `[base, base + size)`. No pointers — Kani
/// verifies it exhaustively over all inputs (`check_carve_no_overflow`,
/// plan §4.2): the function is **total**, returning an error rather than
/// panicking for any `(base, size, watermark, ty, param)`.
///
/// `param` arrives raw from user register `a[2]` (`syscall.rs`), so every
/// step that touches it is checked: the page round-up (`next_multiple_of`
/// panicked for `param` within a page of `u64::MAX` — plan §7.1, finding
/// UO-1) and the placement adds (`base + watermark + align - 1`, `base +
/// size` could overflow at the top of the address space — UO-2). A
/// pathological input now yields `BadArg`/`NoMemory`, never a
/// user-triggerable kernel panic.
pub fn carve(
    base: u64,
    size: u64,
    watermark: u64,
    ty: ObjType,
    param: u64,
) -> Result<Carve, RetypeError> {
    let bytes = match ty {
        ObjType::CSpace => {
            if param == 0 || param > 1024 {
                return Err(RetypeError::BadArg);
            }
            CSpaceObj::bytes_for(param as u32)
        }
        ObjType::Thread => core::mem::size_of::<crate::thread::Tcb>(),
        ObjType::Channel => {
            if param == 0 || param > 256 {
                return Err(RetypeError::BadArg);
            }
            crate::channel::Channel::bytes_for(param as u32)
        }
        ObjType::Notification => core::mem::size_of::<crate::notification::NotifObj>(),
        ObjType::Timer => core::mem::size_of::<crate::timer::TimerObj>(),
        ObjType::Frame => {
            if param == 0 || param > 1 << 16 {
                return Err(RetypeError::BadArg);
            }
            (param * 4096) as usize
        }
        ObjType::Aspace => {
            if param == 0 || param > 256 {
                return Err(RetypeError::BadArg);
            }
            crate::aspace::AspaceObj::bytes_for(param)
        }
        ObjType::Untyped => {
            // param is bytes; round up to a page so the carved range is
            // page-aligned at both ends (§2.3). 0 is meaningless; a param
            // within a page of the address space top has no rounded size.
            if param == 0 {
                return Err(RetypeError::BadArg);
            }
            match (param as usize).checked_next_multiple_of(4096) {
                Some(b) => b,
                None => return Err(RetypeError::BadArg),
            }
        }
    } as u64;

    // All placement arithmetic is checked so carve is total (plan §4.2): the
    // align round-up and the limit add can overflow only at the very top of
    // the 64-bit space, which no real untyped reaches, but an unchecked add
    // there would panic the kernel rather than reject the retype.
    let align = ty.align();
    let start = base
        .checked_add(watermark)
        .and_then(|x| x.checked_add(align - 1))
        .ok_or(RetypeError::NoMemory)?
        & !(align - 1);
    let end = start.checked_add(bytes).ok_or(RetypeError::NoMemory)?;
    let limit = base.checked_add(size).ok_or(RetypeError::NoMemory)?;
    if end > limit {
        return Err(RetypeError::NoMemory);
    }
    Ok(Carve { start, end, bytes })
}

/// Install half: advance the untyped's watermark, set the new cap's rights
/// per the inheritance table, link it as a CDT child, and run the channel
/// two-endpoint dance. All checks already passed; this is infallible.
///
/// pre:  `ut_slot` still holds the Untyped cap [`retype_check`] returned;
///       `kind` was built at `carve.start`; `end == carve.end`.
/// post: watermark = `end - base`; dst holds the cap; object refs == caps
///       installed (1, or 2 for channels).
pub unsafe fn retype_install(
    ut_slot: *mut CapSlot,
    ty: ObjType,
    kind: CapKind,
    end: u64,
    dst: *mut CapSlot,
    dst2: *mut CapSlot,
) {
    let CapKind::Untyped { base, size, .. } = (*ut_slot).cap.kind else {
        // Unreachable: retype_check established this and nothing mutates
        // ut_slot's kind between check and install.
        return;
    };
    (*ut_slot).cap.kind = CapKind::Untyped {
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
        ObjType::Frame => (*ut_slot).cap.rights,
        ObjType::Thread => Rights::THREAD_ALL,
        ObjType::Untyped => (*ut_slot).cap.rights.masked(Rights::READ | Rights::WRITE),
        _ => Rights::ALL,
    };
    (*dst).cap = Cap { kind, rights };
    cspace::cdt_insert_child(ut_slot, dst);
    if let CapKind::Channel(ch, _) = kind {
        crate::channel::endpoint_cap_added(ch, cspace::ChanEnd::A);
        (*dst2).cap = Cap {
            kind: CapKind::Channel(ch, cspace::ChanEnd::B),
            rights: Rights::ALL,
        };
        (*ch.cast::<crate::cspace::ObjHeader>()).refs += 1;
        cspace::cdt_insert_child(ut_slot, dst2);
        crate::channel::endpoint_cap_added(ch, cspace::ChanEnd::B);
    }
}

/// Reset the watermark once exclusivity is proven — the second half of the
/// reclaim primitive (§2.5: "reclaiming the range is revoke(untyped) then
/// watermark reset"). A parent reuses one child-sized donation across many
/// spawns this way (§5.1); the next retype re-zeroes the frames it carves.
///
/// pre:  ut_slot holds an Untyped cap with no CDT children (caller revoked).
/// post: watermark = 0; the whole range is reusable.
pub unsafe fn reset(ut_slot: *mut CapSlot) -> Result<(), RetypeError> {
    let CapKind::Untyped { base, size, .. } = (*ut_slot).cap.kind else {
        return Err(RetypeError::NotUntyped);
    };
    if !(*ut_slot).first_child.is_null() {
        return Err(RetypeError::BadArg);
    }
    (*ut_slot).cap.kind = CapKind::Untyped { base, size, watermark: 0 };
    Ok(())
}
