//! Kernel-side retype: the system's one int→pointer boundary.
//! The validation ([`retype_check`]), placement arithmetic ([`carve`]),
//! CDT install ([`retype_install`]), and `reset` are pure/verifiable
//! and live in [`kcore::untyped`]; this wrapper composes them and performs
//! the `start as *mut T` object construction in between — the cast the
//! verified core never sees, kept here by design.

pub use kcore::untyped::*;

use crate::store::KernelStore;
use kcore::aspace::AspaceObj;
use kcore::channel::Channel;
use kcore::cspace::{CSpaceObj, CapKind, CapSlot, ChanEnd};
use kcore::id::{ObjId, SlotId};
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
    // The int→ptr boundary stays in this wrapper; the kcore halves
    // speak handles, so translate the slot pointers once here. `dst2` is
    // nullable for every non-channel type → `None`.
    let ut_id = SlotId(ut_slot as u64);
    let dst_id = SlotId(dst as u64);
    let dst2_id = if dst2.is_null() {
        None
    } else {
        Some(SlotId(dst2 as u64))
    };

    let (base, size, watermark) = retype_check(&mut KernelStore, ut_id, ty, dst_id, dst2_id)?;
    let c = carve(base, size, watermark, ty, param)?;

    let kind = match ty {
        ObjType::CSpace => {
            let p = c.start as *mut CSpaceObj;
            CSpaceObj::init(p, param as u32);
            CapKind::CSpace(ObjId(p as u64))
        }
        ObjType::Thread => {
            let p = c.start as *mut Tcb;
            Tcb::init(p);
            // rev2§5.4 maximum-controlled-priority ceiling: a fresh thread cap is
            // born capped at the retyper's own priority, so a descendant can
            // never be started above its creator. The ceiling is a
            // cap-carried value that `kcore::cspace::derive` attenuates
            // monotonically (rev2§2.3).
            CapKind::Thread(ObjId(p as u64), (*crate::thread::current()).priority)
        }
        ObjType::Channel => {
            let p = c.start as *mut Channel;
            Channel::init(p, param as u32);
            CapKind::Channel(ObjId(p as u64), ChanEnd::A)
        }
        ObjType::Notification => {
            let p = c.start as *mut NotifObj;
            NotifObj::init(p);
            CapKind::Notification(ObjId(p as u64))
        }
        ObjType::Timer => {
            let p = c.start as *mut TimerObj;
            TimerObj::init(p);
            CapKind::Timer(ObjId(p as u64))
        }
        ObjType::Frame => {
            // Zeroed: frames flow into fresh address spaces; leaking prior
            // contents across processes would break confinement.
            core::ptr::write_bytes(c.start as *mut u8, 0, c.bytes as usize);
            CapKind::Frame {
                base: c.start,
                pages: param,
                mapping: None,
            }
        }
        ObjType::Aspace => {
            let p = c.start as *mut AspaceObj;
            crate::aspace::init(p, param);
            CapKind::Aspace(ObjId(p as u64))
        }
        ObjType::Untyped => CapKind::Untyped {
            base: c.start,
            size: c.bytes,
            watermark: 0,
        },
    };

    retype_install(&mut KernelStore, ut_id, ty, kind, c.end, dst_id, dst2_id);
    Ok(())
}

/// Top-up an aspace's intermediate-page-table pool from a donated untyped
/// (rev2§2.5 "accepts top-ups"). Carve `pages` zeroed 4 KiB tables that
/// physically abut the pool's current end out of `ut_slot`, advance the
/// untyped's watermark — so the bytes are debited from the caller's untyped and
/// reclaimed by the same `revoke + UntypedReset` that frees the aspace (no new
/// cap) — then grow the pool. The pages check, byte count, abutment guard, and
/// verified placement are the host-tested [`kcore::untyped::topup_carve`]; the
/// abutment guard is the discharge of [`crate::aspace::grow_pool`]'s "fresh tables
/// contiguous at `pool_base + old_len*PAGE`" premise. Soundness of the growth
/// itself is the verified [`kcore::aspace::lemma_grow_pool`].
///
/// pre:  `ut_slot` is a live cap slot; `asp` points at a live AspaceObj.
/// post: on `Ok`, the untyped's watermark advanced by `pages * PAGE`, the new
///       tables are zeroed, and `pool_pages += pages`.
pub unsafe fn aspace_topup(
    ut_slot: *mut CapSlot,
    asp: *mut AspaceObj,
    pages: u64,
) -> Result<(), RetypeError> {
    use kcore::aspace::PAGE;

    let CapKind::Untyped {
        base,
        size,
        watermark,
    } = (*ut_slot).cap.kind
    else {
        return Err(RetypeError::NotUntyped);
    };
    // The pool's current physical end (the raw aspace reads stay in the shell).
    // `topup_carve` does the pages check, byte count, abutment guard (the donor's
    // free pointer must equal `pool_end`, so the carved tables extend the pool with
    // no gap and `pool_index_spec`'s single affine base stays valid), and the
    // verified placement + room check (NoMemory if it overruns the untyped).
    // `c.end == pool_end + pages * PAGE`.
    let pool_end = (*asp).pool_base + (*asp).pool_pages * PAGE;
    let c = topup_carve(base, size, watermark, pool_end, pages)?;
    // Advance the watermark exactly as `retype_install` would, but install no
    // cap: the pool is internal to the aspace (rev2§2.5 gives up per-table caps).
    // Only the watermark value changes — no verified cspace invariant constrains
    // it — so this direct write is the trusted int→ptr posture (`KernelStore::
    // set_slot` is literally `*slot_ptr = v`).
    (*ut_slot).cap.kind = CapKind::Untyped {
        base,
        size,
        watermark: c.end - base,
    };
    // Zero the carved tables and bump `pool_pages`; `pool_view`/`map` then rebuild
    // the larger slice automatically (no map-path change).
    crate::aspace::grow_pool(asp, pages);
    Ok(())
}
