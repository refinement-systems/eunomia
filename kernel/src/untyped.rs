//! Kernel-side retype: the system's one int→pointer boundary (plan §2.3).
//! The validation ([`retype_check`]), placement arithmetic ([`carve`]),
//! CDT install ([`retype_install`]), and `reset` are pure/host-verifiable
//! and live in [`kcore::untyped`]; this wrapper composes them and performs
//! the `start as *mut T` object construction in between — the cast CBMC
//! never sees, kept here by design.

pub use kcore::untyped::*;

use kcore::aspace::AspaceObj;
use kcore::channel::Channel;
use kcore::cspace::{CapKind, CapSlot, ChanEnd, CSpaceObj};
use kcore::notification::NotifObj;
use kcore::thread::Tcb;
use kcore::timer::TimerObj;

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
    let (base, size, watermark) = retype_check(ut_slot, ty, dst, dst2)?;
    let c = carve(base, size, watermark, ty, param)?;

    let kind = match ty {
        ObjType::CSpace => {
            let p = c.start as *mut CSpaceObj;
            CSpaceObj::init(p, param as u32);
            CapKind::CSpace(p)
        }
        ObjType::Thread => {
            let p = c.start as *mut Tcb;
            Tcb::init(p);
            CapKind::Thread(p)
        }
        ObjType::Channel => {
            let p = c.start as *mut Channel;
            Channel::init(p, param as u32);
            CapKind::Channel(p, ChanEnd::A)
        }
        ObjType::Notification => {
            let p = c.start as *mut NotifObj;
            NotifObj::init(p);
            CapKind::Notification(p)
        }
        ObjType::Timer => {
            let p = c.start as *mut TimerObj;
            TimerObj::init(p);
            CapKind::Timer(p)
        }
        ObjType::Frame => {
            // Zeroed: frames flow into fresh address spaces; leaking prior
            // contents across processes would break confinement.
            core::ptr::write_bytes(c.start as *mut u8, 0, c.bytes as usize);
            CapKind::Frame { base: c.start, pages: param, mapping: None }
        }
        ObjType::Aspace => {
            let p = c.start as *mut AspaceObj;
            crate::aspace::init(p, param);
            CapKind::Aspace(p)
        }
        ObjType::Untyped => CapKind::Untyped { base: c.start, size: c.bytes, watermark: 0 },
    };

    retype_install(ut_slot, ty, kind, c.end, dst, dst2);
    Ok(())
}
