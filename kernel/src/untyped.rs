//! Untyped memory and retype (spec §1, §2.5, §3.2).
//!
//! An untyped cap names a physical range and carries a watermark; retype
//! carves the next object out of the range and installs the new cap as a
//! CDT child of the untyped cap. The watermark only ever advances —
//! reclaiming the range is `revoke(untyped)` (which deletes every object
//! cap derived from it) followed by watermark reset, proving exclusivity
//! exactly as the TLA+ Retype guard requires.
//!
//! Every kernel object is created this way: the kernel has no global pool
//! (§3.2). The one exception is the statically allocated root cspace,
//! which is morally init's memory baked into the image.

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
            _ => return None,
        })
    }

    fn align(self) -> u64 {
        match self {
            ObjType::Frame | ObjType::Aspace => 4096,
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

/// Carve one object of `ty` (sized by `param`: slot count for cspaces,
/// per-direction queue depth for channels) out of the untyped cap in
/// `ut_slot`, installing the new cap in `dst`. Channels mint two endpoint
/// caps: end A in `dst`, end B in `dst2` (both CDT children of the
/// untyped); `dst2` is ignored for every other type.
///
/// pre:  ut_slot holds an Untyped cap; dst (and dst2 for channels) is
///       empty and detached.
/// post: watermark advanced; dst holds a cap to the initialised object,
///       CDT child of ut_slot; object refs == caps installed.
pub unsafe fn retype(
    ut_slot: *mut CapSlot,
    ty: ObjType,
    param: u64,
    dst: *mut CapSlot,
    dst2: *mut CapSlot,
) -> Result<(), RetypeError> {
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
    } as u64;

    let align = ty.align();
    let start = (base + watermark + align - 1) & !(align - 1);
    let end = start.checked_add(bytes).ok_or(RetypeError::NoMemory)?;
    if end > base + size {
        return Err(RetypeError::NoMemory);
    }

    let kind = match ty {
        ObjType::CSpace => {
            let p = start as *mut CSpaceObj;
            CSpaceObj::init(p, param as u32);
            CapKind::CSpace(p)
        }
        ObjType::Thread => {
            let p = start as *mut crate::thread::Tcb;
            crate::thread::Tcb::init(p);
            CapKind::Thread(p)
        }
        ObjType::Channel => {
            let p = start as *mut crate::channel::Channel;
            crate::channel::Channel::init(p, param as u32);
            CapKind::Channel(p, cspace::ChanEnd::A)
        }
        ObjType::Notification => {
            let p = start as *mut crate::notification::NotifObj;
            crate::notification::NotifObj::init(p);
            CapKind::Notification(p)
        }
        ObjType::Timer => {
            let p = start as *mut crate::timer::TimerObj;
            crate::timer::TimerObj::init(p);
            CapKind::Timer(p)
        }
        ObjType::Frame => {
            // Zeroed: frames flow into fresh address spaces; leaking prior
            // contents across processes would break confinement.
            core::ptr::write_bytes(start as *mut u8, 0, bytes as usize);
            CapKind::Frame { base: start, pages: param, mapping: None }
        }
        ObjType::Aspace => {
            let p = start as *mut crate::aspace::AspaceObj;
            crate::aspace::AspaceObj::init(p, param);
            CapKind::Aspace(p)
        }
    };

    (*ut_slot).cap.kind = CapKind::Untyped {
        base,
        size,
        watermark: end - base,
    };
    // Frames inherit the untyped's rights so phys-read (§2.5) flows only
    // from boot untypeds along explicit grants; kernel objects get the
    // ordinary full mask.
    let rights = match ty {
        ObjType::Frame => (*ut_slot).cap.rights,
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
        (*(ch as *mut crate::cspace::ObjHeader)).refs += 1;
        cspace::cdt_insert_child(ut_slot, dst2);
        crate::channel::endpoint_cap_added(ch, cspace::ChanEnd::B);
    }
    Ok(())
}

/// Reset the watermark once exclusivity is proven.
///
/// pre:  ut_slot holds an Untyped cap with no CDT children (caller revoked).
/// post: watermark = 0; the whole range is reusable.
#[allow(dead_code)] // syscall surface arrives with M2 (driver memory churn)
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
