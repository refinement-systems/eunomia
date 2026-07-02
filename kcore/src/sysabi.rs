// SPDX-License-Identifier: 0BSD
//! Syscall ABI decode + validation (rev2§3.7). The pure half of
//! `kernel/src/syscall.rs`: turn the raw register file `(nr, a[0..7])` into a
//! typed [`Sys`] value, performing every check that needs **no** capability or
//! thread state — `ObjType` totality, the message-length cap before
//! `channel::send`'s `as u16` truncation, the event/which/priority ranges. The
//! kernel consumes the `Sys` value and does the capability lookup, rights
//! checks, user-pointer validation, and the operation itself (all of which
//! need live state and stay kernel-side).
//!
//! `decode` is **total**: for any `(nr, a) : u64⁸` it returns `Ok(Sys)` or
//! `Err(SysError)`, never panics, never overflows, never UB — an unknown
//! `nr` is an error, never a crash (rev2§3.7). This makes "no
//! user-controlled value reaches kernel arithmetic unvalidated" a checked
//! property (`kcore::proofs::sysabi`) rather than a review convention.
//!
//! No pointers, no `unsafe`, no kernel dependencies — pure data.
use crate::channel::MSG_PAYLOAD;
use crate::untyped::ObjType;
use vstd::prelude::*;

verus! {

/// Scheduler priority levels. Canonical home (the kernel's ready-queue array
/// and this decoder's range check share it); `kernel::thread` re-exports it.
/// Inside `verus!{}` so it is spec-visible to the `decode`/`decode_prio`
/// contracts (the `channel::MSG_PAYLOAD` idiom); erases to a plain `pub const`,
/// so the `kernel::thread` re-export and the aarch64 build are unchanged.
pub const NUM_PRIOS: usize = 32;

/// `cap_copy`'s "no priority-ceiling reduction" sentinel (rev2§2.3/rev2§5.4):
/// a thread-cap copy passing this leaves the parent's ceiling unchanged. Any value
/// `>= NUM_PRIOS - 1` would do (priorities are `< NUM_PRIOS = 32`); `0xFF` is the
/// canonical one. A lower `prio_ceiling` strictly attenuates (`derived_kind`).
pub const NO_PRIO_CEILING: u8 = 0xFF;

/// The maximum byte length the `DebugWrite` syscall (rev2§7) accepts in one
/// call; a longer write is refused outright (`ERR_FAULT`, writing nothing). The
/// canonical home for both the kernel's `Sys::DebugWrite` length guard
/// (`kernel::syscall`) and the userspace chunker that re-establishes it at the
/// seam (`eunomia_sys::stdio`, whose host test pins its local twin against this).
/// A plain ABI value (the `NUM_PRIOS` idiom); not a decode obligation — `decode`
/// passes the length through, the kernel checks it at use time.
pub const DEBUG_WRITE_MAX: u64 = 1024;

/// A decoded, shape-validated syscall. Slot indices stay `u64` — the
/// cspace-size bound is `CSpaceObj::slot`'s job at *use* time (kernel
/// `cur_slot`), so error codes and ordering for bad slots are unchanged.
/// Fields that decode *does* validate are stored in their narrowed form
/// (`ObjType`, `event`/`which : usize`, `prio : u8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sys {
    DebugPutc { ch: u64 },
    DebugWrite { ptr: u64, len: u64 },
    Yield,
    Retype { ut: u64, ty: ObjType, param: u64, dst: u64, dst2: u64 },
    CapCopy { src: u64, dst: u64, mask: u64, prio_ceiling: u64 },
    CapDelete { slot: u64 },
    CapRevoke { slot: u64 },
    CapInstall { cs: u64, src: u64, dst_index: u64 },
    ChanSend { chan: u64, buf: u64, len: u64, caps: u64 },
    ChanRecv { chan: u64, buf: u64, dests: u64 },
    ChanBind { chan: u64, event: usize, notif: u64, bits: u64 },
    NotifSignal { slot: u64, bits: u64 },
    NotifWait { slot: u64 },
    ThreadStart { tcb: u64, cspace: u64, entry: u64, sp: u64, prio: u8, arg: u64 },
    TimerArm { timer: u64, notif: u64, bits: u64, delta: u64 },
    ThreadExit { status: u64 },
    Map { aspace: u64, frame: u64, va: u64, perms: u64 },
    FrameWrite { frame: u64, off: u64, buf: u64, len: u64 },
    ThreadStartAs { tcb: u64, cspace: u64, aspace: u64, entry: u64, sp: u64, prio: u8, arg: u64 },
    FramePaddr { slot: u64 },
    ThreadBind { tcb: u64, which: usize, notif: u64, bits: u64 },
    ReadReport { tcb: u64 },
    UntypedReset { slot: u64 },
    AspaceTopUp { aspace: u64, ut: u64, pages: u64 },
    IrqBind { irq: u64, notif: u64, bits: u64 },
    IrqAck { irq: u64 },
}

/// A decode-time validation failure. The kernel maps every variant to
/// `ERR_ARG` (the code each of these conditions already returns), so the
/// observable errno is unchanged for well-formed-but-rejected requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysError {
    UnknownCall,
    BadObjType,
    MsgTooLong,
    BadEvent,
    BadWhich,
    BadPrio,
}

/// Mask the priority to its low byte and bound it. The `(raw & 0xFF) as u8`
/// truncating cast is total; the range check is what `ThreadStart`/`ThreadStartAs`
/// rely on, so the bound is a postcondition the kernel's ready-queue index trusts.
fn decode_prio(raw: u64) -> (result: Result<u8, SysError>)
    ensures
        result matches Ok(p) ==> (p as usize) < NUM_PRIOS,
{
    let prio = (raw & 0xFF) as u8;
    if prio as usize >= NUM_PRIOS {
        return Err(SysError::BadPrio);
    }
    Ok(prio)
}

/// Decode the register file into a typed syscall (rev2§3.7). `nr` is x7;
/// `a` is x0..x6 (the kernel's trap-frame read — seven argument registers).
/// The seventh (x6) carries `ThreadStartAs`'s initial-`x0` arg (rev2§5.1); every
/// other opcode ignores it (`ThreadStart` already carries its arg in x5/a[5]).
///
/// Verified **total**: for any `(nr, a) : (u64, [u64;7])` it returns
/// `Ok`/`Err`, never panics, overflows, or UBs (the rev2§3.7 "unknown `nr` is
/// an error, never a crash" as a theorem, not a review convention). The
/// `ensures` pin the shape-validation: the `ChanSend` length cap that precedes
/// `channel::send`'s
/// `as u16` truncation (so send's `data.len() <= MSG_PAYLOAD` precondition is
/// discharged at the source); the event/which/priority ranges; and that an
/// unknown call or bad `ObjType` discriminant maps to the right error. The body
/// uses explicit `match`/early-return (not `?`/`ok_or`) so the control flow is
/// in the verified fragment.
pub fn decode(nr: u64, a: [u64; 7]) -> (result: Result<Sys, SysError>)
    ensures
// rev2§3.7: every `nr` outside the defined 0..=26 range is `UnknownCall`.

        nr >= 27 ==> result matches Err(SysError::UnknownCall),
        // Retype with an out-of-range `ObjType` discriminant is `BadObjType`
        // (via `ObjType::from_u64`'s `None`-iff-`v >= 8` characterization).
        (nr == 3 && a@[1] >= 8) ==> result matches Err(SysError::BadObjType),
        // The load-bearing rev2§3.1 cap: a decoded send length never exceeds the
        // payload bound, so the downstream `as u16` truncation is lossless and
        // `channel::send`'s `data.len() <= MSG_PAYLOAD` precondition holds.
        result matches Ok(Sys::ChanSend { len, .. }) ==> len <= MSG_PAYLOAD as u64,
        // Bounded-before-use: event/which/priority are validated at decode time.
        result matches Ok(Sys::ChanBind { event, .. }) ==> event < 3,
        result matches Ok(Sys::ThreadBind { which, .. }) ==> which < 2,
        result matches Ok(Sys::ThreadStart { prio, .. }) ==> (prio as usize) < NUM_PRIOS,
        result matches Ok(Sys::ThreadStartAs { prio, .. }) ==> (prio as usize) < NUM_PRIOS,
{
    Ok(
        match nr {
            0 => Sys::DebugPutc { ch: a[0] },
            1 => Sys::DebugWrite { ptr: a[0], len: a[1] },
            2 => Sys::Yield,
            3 => {
                // `from_u64` is `None` exactly when `a[1] >= 8`, so the BadObjType
                // postcondition follows from its contract.
                match ObjType::from_u64(a[1]) {
                    Some(ty) => Sys::Retype { ut: a[0], ty, param: a[2], dst: a[3], dst2: a[4] },
                    None => return Err(SysError::BadObjType),
                }
            }
            // a[3] is the rev2§5.4 priority-ceiling cap on a thread-cap copy (rev2§2.3
            // supervision grant); `NO_PRIO_CEILING` (0xFF) means "no reduction". Carried
            // raw (it only ever *shrinks* an existing ceiling in `derive`, so no decode
            // validation is needed — see `derived_kind`).
            ,
            4 => Sys::CapCopy { src: a[0], dst: a[1], mask: a[2], prio_ceiling: a[3] },
            5 => Sys::CapDelete { slot: a[0] },
            6 => Sys::CapRevoke { slot: a[0] },
            7 => Sys::CapInstall { cs: a[0], src: a[1], dst_index: a[2] },
            8 => {
                // Length is capped here, before channel::send truncates it `as u16`.
                if a[2] > MSG_PAYLOAD as u64 {
                    return Err(SysError::MsgTooLong);
                }
                Sys::ChanSend { chan: a[0], buf: a[1], len: a[2], caps: a[3] }
            },
            9 => Sys::ChanRecv { chan: a[0], buf: a[1], dests: a[2] },
            10 => {
                if a[1] > 2 {
                    return Err(SysError::BadEvent);
                }
                Sys::ChanBind { chan: a[0], event: a[1] as usize, notif: a[2], bits: a[3] }
            },
            11 => Sys::NotifSignal { slot: a[0], bits: a[1] },
            12 => Sys::NotifWait { slot: a[0] },
            13 => {
                match decode_prio(a[4]) {
                    Ok(prio) => Sys::ThreadStart {
                        tcb: a[0],
                        cspace: a[1],
                        entry: a[2],
                        sp: a[3],
                        prio,
                        arg: a[5],
                    },
                    Err(e) => return Err(e),
                }
            },
            14 => Sys::TimerArm { timer: a[0], notif: a[1], bits: a[2], delta: a[3] },
            15 => Sys::ThreadExit { status: a[0] },
            16 => Sys::Map { aspace: a[0], frame: a[1], va: a[2], perms: a[3] },
            17 => Sys::FrameWrite { frame: a[0], off: a[1], buf: a[2], len: a[3] },
            18 => {
                match decode_prio(a[5]) {
                    Ok(prio) => Sys::ThreadStartAs {
                        tcb: a[0],
                        cspace: a[1],
                        aspace: a[2],
                        entry: a[3],
                        sp: a[4],
                        prio,
                        arg: a[6],
                    },
                    Err(e) => return Err(e),
                }
            },
            19 => Sys::FramePaddr { slot: a[0] },
            // 20 is unassigned: the userspace console driver owns the PL011 RX line, so
            // there is no ambient input syscall — opcode 20 falls through to
            // `UnknownCall` (rev2§7 carve-out exit condition met).
            21 => {
                if a[1] > 1 {
                    return Err(SysError::BadWhich);
                }
                Sys::ThreadBind { tcb: a[0], which: a[1] as usize, notif: a[2], bits: a[3] }
            },
            22 => Sys::ReadReport { tcb: a[0] },
            23 => Sys::UntypedReset { slot: a[0] },
            // Grow an aspace's page-table pool from a donated untyped (rev2§2.5
            // "accepts top-ups"). Three raw `u64`s, all validated downstream by the
            // abutment carve + `grow_pool`, so no decode-time range `ensures`.
            24 => Sys::AspaceTopUp { aspace: a[0], ut: a[1], pages: a[2] },
            // Bind/ack an IRQ-handler cap (rev2§1, rev2§3.6). Raw `u64`s,
            // all validated downstream by the cap lookup + the verified `irq_bind`,
            // so no decode-time range `ensures` (the `TimerArm`/`AspaceTopUp` precedent).
            25 => Sys::IrqBind { irq: a[0], notif: a[1], bits: a[2] },
            26 => Sys::IrqAck { irq: a[0] },
            _ => return Err(SysError::UnknownCall),
        },
    )
}

} // verus!
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_calls_decode() {
        assert_eq!(
            decode(0, [b'x' as u64, 0, 0, 0, 0, 0, 0]),
            Ok(Sys::DebugPutc { ch: b'x' as u64 })
        );
        assert_eq!(decode(2, [0; 7]), Ok(Sys::Yield));
        assert_eq!(
            decode(8, [1, 0x1000, MSG_PAYLOAD as u64, 0, 0, 0, 0]),
            Ok(Sys::ChanSend {
                chan: 1,
                buf: 0x1000,
                len: MSG_PAYLOAD as u64,
                caps: 0
            })
        );
        // The top-up opcode packs three raw u64s.
        assert_eq!(
            decode(24, [3, 5, 8, 0, 0, 0, 0]),
            Ok(Sys::AspaceTopUp {
                aspace: 3,
                ut: 5,
                pages: 8
            })
        );
        // The two IRQ opcodes pack raw u64s.
        assert_eq!(
            decode(25, [2, 4, 0xF, 0, 0, 0, 0]),
            Ok(Sys::IrqBind {
                irq: 2,
                notif: 4,
                bits: 0xF
            })
        );
        assert_eq!(
            decode(26, [2, 0, 0, 0, 0, 0, 0]),
            Ok(Sys::IrqAck { irq: 2 })
        );
    }

    #[test]
    fn validation_rejects() {
        assert_eq!(decode(99, [0; 7]), Err(SysError::UnknownCall));
        // The defined range is 0..=26, so 27 is the first unknown opcode.
        assert_eq!(decode(27, [0; 7]), Err(SysError::UnknownCall));
        // Opcode 20 is unassigned (no ambient input syscall), so it is an interior
        // gap that decodes to UnknownCall.
        assert_eq!(decode(20, [0; 7]), Err(SysError::UnknownCall));
        assert_eq!(decode(3, [0, 99, 0, 0, 0, 0, 0]), Err(SysError::BadObjType)); // bad ObjType
        assert_eq!(
            decode(8, [0, 0, MSG_PAYLOAD as u64 + 1, 0, 0, 0, 0]),
            Err(SysError::MsgTooLong)
        );
        assert_eq!(decode(10, [0, 3, 0, 0, 0, 0, 0]), Err(SysError::BadEvent)); // event > 2
        assert_eq!(decode(21, [0, 2, 0, 0, 0, 0, 0]), Err(SysError::BadWhich)); // which > 1
        assert_eq!(
            decode(13, [0, 0, 0, 0, NUM_PRIOS as u64, 0, 0]),
            Err(SysError::BadPrio)
        );
    }

    #[test]
    fn prio_is_masked_then_bounded() {
        // Low byte < NUM_PRIOS decodes; the high bits are ignored.
        assert_eq!(
            decode(13, [0, 0, 0, 0, 0xFF00 | 5, 0, 0]),
            Ok(Sys::ThreadStart {
                tcb: 0,
                cspace: 0,
                entry: 0,
                sp: 0,
                prio: 5,
                arg: 0
            })
        );
    }
}
