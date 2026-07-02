// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! The verified syscall-argument encoder (rev2§3.7) — the inverse of
//! `kcore::sysabi::decode`.
//!
//! [`encode`] turns a typed [`Call`] into the register file [`Encoded`]
//! `{nr, a0..a5}`, proven over **every** `Call`: it never panics or overflows, always
//! emits a defined opcode (`nr < 27`), places each argument in exactly the register
//! `decode` reads it from, and *refuses* (`Err`) exactly the out-of-range fields the
//! kernel rejects — the message-length cap (before the kernel's `as u16` truncation),
//! the `ObjType` range, and the event/which/priority ranges. So the PAL provably
//! cannot hand the kernel a syscall it would reject by shape: the §11 inverse-leak
//! rule, re-established at the seam as a theorem (rather than a review convention).
//!
//! The opcode/bound constants here are a local independent twin of rev2§3.7 (the
//! `ipc::sys` posture — userspace does not depend on the kernel object core); the host
//! `constants_match_kcore` test pins them against the real kernel decoder, and the
//! `encode_round_trips_through_kernel_decode` test pins the whole inverse.
#[allow(unused_imports)]
use vstd::prelude::*;

verus! {

/// Channel message payload cap; a `chan_send` length above this is refused before the
/// kernel's `as u16` truncation (`kcore::channel::MSG_PAYLOAD`).
pub const MSG_PAYLOAD: u64 = 256;

/// Scheduler priority levels; a `thread_start` priority `>= NUM_PRIOS` is refused
/// (`kcore::sysabi::NUM_PRIOS`).
pub const NUM_PRIOS: u64 = 32;

/// Number of valid `ObjType` discriminants; a `retype` type `>= OBJ_COUNT` is refused
/// (`kcore::untyped::ObjType::from_u64` is `None` iff `v >= 8`).
pub const OBJ_COUNT: u64 = 8;

/// The register file a syscall lowers to: `nr` (x7) and the seven argument registers
/// (x0..x6). The pure output of [`encode`]; the trusted shell spreads it into `svc`.
/// `a6` (x6) carries only `ThreadStartAs`'s initial-`x0` arg (rev2§5.1); every other
/// call leaves it 0 (`ThreadStart` already carries its arg in `a5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoded {
    pub nr: u64,
    pub a0: u64,
    pub a1: u64,
    pub a2: u64,
    pub a3: u64,
    pub a4: u64,
    pub a5: u64,
    pub a6: u64,
}

/// Why [`encode`] refused a call: a field the kernel decode would reject for shape.
/// The twin of `kcore::sysabi::SysError`, minus `UnknownCall` (a typed `Call` always
/// names a defined opcode, so `encode` never produces it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    BadObjType,
    MsgTooLong,
    BadEvent,
    BadWhich,
    BadPrio,
}

/// A typed syscall request — the field-for-field mirror of `kcore::sysabi::Sys`, the
/// canonical typed ABI (rev2§3.7). Slot indices and the validated fields (`ty`,
/// `event`, `which`, `prio`) ride as `u64`; [`encode`] is what validates them against
/// the kernel's accept bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    DebugPutc { ch: u64 },
    DebugWrite { ptr: u64, len: u64 },
    Yield,
    Retype { ut: u64, ty: u64, param: u64, dst: u64, dst2: u64 },
    CapCopy { src: u64, dst: u64, mask: u64, prio_ceiling: u64 },
    CapDelete { slot: u64 },
    CapRevoke { slot: u64 },
    CapInstall { cs: u64, src: u64, dst_index: u64 },
    ChanSend { chan: u64, buf: u64, len: u64, caps: u64 },
    ChanRecv { chan: u64, buf: u64, dests: u64 },
    ChanBind { chan: u64, event: u64, notif: u64, bits: u64 },
    NotifSignal { slot: u64, bits: u64 },
    NotifWait { slot: u64 },
    ThreadStart { tcb: u64, cspace: u64, entry: u64, sp: u64, prio: u64, arg: u64 },
    TimerArm { timer: u64, notif: u64, bits: u64, delta: u64 },
    ThreadExit { status: u64 },
    Map { aspace: u64, frame: u64, va: u64, perms: u64 },
    FrameWrite { frame: u64, off: u64, buf: u64, len: u64 },
    ThreadStartAs { tcb: u64, cspace: u64, aspace: u64, entry: u64, sp: u64, prio: u64, arg: u64 },
    FramePaddr { slot: u64 },
    ThreadBind { tcb: u64, which: u64, notif: u64, bits: u64 },
    ReadReport { tcb: u64 },
    UntypedReset { slot: u64 },
    AspaceTopUp { aspace: u64, ut: u64, pages: u64 },
    IrqBind { irq: u64, notif: u64, bits: u64 },
    IrqAck { irq: u64 },
}

/// Encode a typed [`Call`] into its register file — the verified inverse of
/// `kcore::sysabi::decode` (rev2§3.7). See the module docs for the contract.
///
/// The placement clauses are keyed on `Ok` (a refused validated call is vacuously
/// covered) and nest the two `matches` as `result matches Ok(e) ==> (call matches Pat
/// ==> placement)` so each is a clean single-`matches` binding form: a flat
/// `a && b ==> c` would make `result matches Ok(e)` a hard conjunct (failing at the
/// `Err` exits) because Verus's `matches`-binding `&&` extends rightward. The
/// inverse-leak clauses pair each validated field's refusal with its in-range
/// acceptance. The body mirrors `decode`'s explicit `match`/early-return so the
/// control flow is in the verified fragment.
pub fn encode(call: Call) -> (result: Result<Encoded, CallError>)
    ensures
        result matches Ok(e) ==> e.nr < 27,
        result matches Ok(e) ==> (call matches Call::DebugPutc { ch } ==> e.nr == 0 && e.a0 == ch),
        result matches Ok(e) ==> (call matches Call::DebugWrite { ptr, len } ==> e.nr == 1 && e.a0
            == ptr && e.a1 == len),
        result matches Ok(e) ==> (call matches Call::Yield ==> e.nr == 2),
        result matches Ok(e) ==> (call matches Call::Retype { ut, ty, param, dst, dst2 } ==> e.nr
            == 3 && e.a0 == ut && e.a1 == ty && e.a2 == param && e.a3 == dst && e.a4 == dst2),
        result matches Ok(e) ==> (call matches Call::CapCopy { src, dst, mask, prio_ceiling }
            ==> e.nr == 4 && e.a0 == src && e.a1 == dst && e.a2 == mask && e.a3 == prio_ceiling),
        result matches Ok(e) ==> (call matches Call::CapDelete { slot } ==> e.nr == 5 && e.a0
            == slot),
        result matches Ok(e) ==> (call matches Call::CapRevoke { slot } ==> e.nr == 6 && e.a0
            == slot),
        result matches Ok(e) ==> (call matches Call::CapInstall { cs, src, dst_index } ==> e.nr == 7
            && e.a0 == cs && e.a1 == src && e.a2 == dst_index),
        result matches Ok(e) ==> (call matches Call::ChanSend { chan, buf, len, caps } ==> e.nr == 8
            && e.a0 == chan && e.a1 == buf && e.a2 == len && e.a3 == caps),
        result matches Ok(e) ==> (call matches Call::ChanRecv { chan, buf, dests } ==> e.nr == 9
            && e.a0 == chan && e.a1 == buf && e.a2 == dests),
        result matches Ok(e) ==> (call matches Call::ChanBind { chan, event, notif, bits } ==> e.nr
            == 10 && e.a0 == chan && e.a1 == event && e.a2 == notif && e.a3 == bits),
        result matches Ok(e) ==> (call matches Call::NotifSignal { slot, bits } ==> e.nr == 11
            && e.a0 == slot && e.a1 == bits),
        result matches Ok(e) ==> (call matches Call::NotifWait { slot } ==> e.nr == 12 && e.a0
            == slot),
        result matches Ok(e) ==> (call matches Call::ThreadStart {
            tcb,
            cspace,
            entry,
            sp,
            prio,
            arg,
        } ==> e.nr == 13 && e.a0 == tcb && e.a1 == cspace && e.a2 == entry && e.a3 == sp && e.a4
            == prio && e.a5 == arg),
        result matches Ok(e) ==> (call matches Call::TimerArm { timer, notif, bits, delta } ==> e.nr
            == 14 && e.a0 == timer && e.a1 == notif && e.a2 == bits && e.a3 == delta),
        result matches Ok(e) ==> (call matches Call::ThreadExit { status } ==> e.nr == 15 && e.a0
            == status),
        result matches Ok(e) ==> (call matches Call::Map { aspace, frame, va, perms } ==> e.nr == 16
            && e.a0 == aspace && e.a1 == frame && e.a2 == va && e.a3 == perms),
        result matches Ok(e) ==> (call matches Call::FrameWrite { frame, off, buf, len } ==> e.nr
            == 17 && e.a0 == frame && e.a1 == off && e.a2 == buf && e.a3 == len),
        result matches Ok(e) ==> (call matches Call::ThreadStartAs {
            tcb,
            cspace,
            aspace,
            entry,
            sp,
            prio,
            arg,
        } ==> e.nr == 18 && e.a0 == tcb && e.a1 == cspace && e.a2 == aspace && e.a3 == entry && e.a4
            == sp && e.a5 == prio && e.a6 == arg),
        result matches Ok(e) ==> (call matches Call::FramePaddr { slot } ==> e.nr == 19 && e.a0
            == slot),
        result matches Ok(e) ==> (call matches Call::ThreadBind { tcb, which, notif, bits } ==> e.nr
            == 21 && e.a0 == tcb && e.a1 == which && e.a2 == notif && e.a3 == bits),
        result matches Ok(e) ==> (call matches Call::ReadReport { tcb } ==> e.nr == 22 && e.a0
            == tcb),
        result matches Ok(e) ==> (call matches Call::UntypedReset { slot } ==> e.nr == 23 && e.a0
            == slot),
        result matches Ok(e) ==> (call matches Call::AspaceTopUp { aspace, ut, pages } ==> e.nr
            == 24 && e.a0 == aspace && e.a1 == ut && e.a2 == pages),
        result matches Ok(e) ==> (call matches Call::IrqBind { irq, notif, bits } ==> e.nr == 25
            && e.a0 == irq && e.a1 == notif && e.a2 == bits),
        result matches Ok(e) ==> (call matches Call::IrqAck { irq } ==> e.nr == 26 && e.a0 == irq),
        call matches Call::Retype { ty, .. } ==> (ty >= OBJ_COUNT ==> result matches Err(
            CallError::BadObjType,
        )),
        call matches Call::Retype { ty, .. } ==> (ty < OBJ_COUNT ==> result is Ok),
        call matches Call::ChanSend { len, .. } ==> (len > MSG_PAYLOAD ==> result matches Err(
            CallError::MsgTooLong,
        )),
        call matches Call::ChanSend { len, .. } ==> (len <= MSG_PAYLOAD ==> result is Ok),
        call matches Call::ChanBind { event, .. } ==> (event > 2 ==> result matches Err(
            CallError::BadEvent,
        )),
        call matches Call::ChanBind { event, .. } ==> (event <= 2 ==> result is Ok),
        call matches Call::ThreadStart { prio, .. } ==> (prio >= NUM_PRIOS ==> result matches Err(
            CallError::BadPrio,
        )),
        call matches Call::ThreadStart { prio, .. } ==> (prio < NUM_PRIOS ==> result is Ok),
        call matches Call::ThreadStartAs { prio, .. } ==> (prio >= NUM_PRIOS ==> result matches Err(
            CallError::BadPrio,
        )),
        call matches Call::ThreadStartAs { prio, .. } ==> (prio < NUM_PRIOS ==> result is Ok),
        call matches Call::ThreadBind { which, .. } ==> (which > 1 ==> result matches Err(
            CallError::BadWhich,
        )),
        call matches Call::ThreadBind { which, .. } ==> (which <= 1 ==> result is Ok),
{
    Ok(
        match call {
            Call::DebugPutc { ch } => Encoded {
                nr: 0,
                a0: ch,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::DebugWrite { ptr, len } => Encoded {
                nr: 1,
                a0: ptr,
                a1: len,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::Yield => Encoded { nr: 2, a0: 0, a1: 0, a2: 0, a3: 0, a4: 0, a5: 0, a6: 0 },
            Call::Retype { ut, ty, param, dst, dst2 } => {
                if ty >= OBJ_COUNT {
                    return Err(CallError::BadObjType);
                }
                Encoded { nr: 3, a0: ut, a1: ty, a2: param, a3: dst, a4: dst2, a5: 0, a6: 0 }
            },
            Call::CapCopy { src, dst, mask, prio_ceiling } => Encoded {
                nr: 4,
                a0: src,
                a1: dst,
                a2: mask,
                a3: prio_ceiling,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::CapDelete { slot } => Encoded {
                nr: 5,
                a0: slot,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::CapRevoke { slot } => Encoded {
                nr: 6,
                a0: slot,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::CapInstall { cs, src, dst_index } => Encoded {
                nr: 7,
                a0: cs,
                a1: src,
                a2: dst_index,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::ChanSend { chan, buf, len, caps } => {
                if len > MSG_PAYLOAD {
                    return Err(CallError::MsgTooLong);
                }
                Encoded { nr: 8, a0: chan, a1: buf, a2: len, a3: caps, a4: 0, a5: 0, a6: 0 }
            },
            Call::ChanRecv { chan, buf, dests } => Encoded {
                nr: 9,
                a0: chan,
                a1: buf,
                a2: dests,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::ChanBind { chan, event, notif, bits } => {
                if event > 2 {
                    return Err(CallError::BadEvent);
                }
                Encoded { nr: 10, a0: chan, a1: event, a2: notif, a3: bits, a4: 0, a5: 0, a6: 0 }
            },
            Call::NotifSignal { slot, bits } => Encoded {
                nr: 11,
                a0: slot,
                a1: bits,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::NotifWait { slot } => Encoded {
                nr: 12,
                a0: slot,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::ThreadStart { tcb, cspace, entry, sp, prio, arg } => {
                if prio >= NUM_PRIOS {
                    return Err(CallError::BadPrio);
                }
                Encoded { nr: 13, a0: tcb, a1: cspace, a2: entry, a3: sp, a4: prio, a5: arg, a6: 0 }
            },
            Call::TimerArm { timer, notif, bits, delta } => Encoded {
                nr: 14,
                a0: timer,
                a1: notif,
                a2: bits,
                a3: delta,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::ThreadExit { status } => Encoded {
                nr: 15,
                a0: status,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::Map { aspace, frame, va, perms } => Encoded {
                nr: 16,
                a0: aspace,
                a1: frame,
                a2: va,
                a3: perms,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::FrameWrite { frame, off, buf, len } => Encoded {
                nr: 17,
                a0: frame,
                a1: off,
                a2: buf,
                a3: len,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::ThreadStartAs { tcb, cspace, aspace, entry, sp, prio, arg } => {
                if prio >= NUM_PRIOS {
                    return Err(CallError::BadPrio);
                }
                Encoded {
                    nr: 18,
                    a0: tcb,
                    a1: cspace,
                    a2: aspace,
                    a3: entry,
                    a4: sp,
                    a5: prio,
                    a6: arg,
                }
            },
            Call::FramePaddr { slot } => Encoded {
                nr: 19,
                a0: slot,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::ThreadBind { tcb, which, notif, bits } => {
                if which > 1 {
                    return Err(CallError::BadWhich);
                }
                Encoded { nr: 21, a0: tcb, a1: which, a2: notif, a3: bits, a4: 0, a5: 0, a6: 0 }
            },
            Call::ReadReport { tcb } => Encoded {
                nr: 22,
                a0: tcb,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::UntypedReset { slot } => Encoded {
                nr: 23,
                a0: slot,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::AspaceTopUp { aspace, ut, pages } => Encoded {
                nr: 24,
                a0: aspace,
                a1: ut,
                a2: pages,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::IrqBind { irq, notif, bits } => Encoded {
                nr: 25,
                a0: irq,
                a1: notif,
                a2: bits,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
            Call::IrqAck { irq } => Encoded {
                nr: 26,
                a0: irq,
                a1: 0,
                a2: 0,
                a3: 0,
                a4: 0,
                a5: 0,
                a6: 0,
            },
        },
    )
}

} // verus!
#[cfg(test)]
mod tests {
    use super::*;
    use kcore::sysabi::{decode, Sys, SysError};
    use kcore::untyped::ObjType;
    use proptest::prelude::*;

    /// The args of an [`Encoded`] in the `[u64;7]` form `kcore::sysabi::decode` reads.
    fn args(e: &Encoded) -> [u64; 7] {
        [e.a0, e.a1, e.a2, e.a3, e.a4, e.a5, e.a6]
    }

    /// Bridge a local [`Call`] to the kernel `Sys` value it must decode back to —
    /// untrusted test glue mapping the twin's `u64` validated fields to their narrowed
    /// kernel types. Only defined for in-range calls (the ones `encode` accepts).
    fn bridge(call: Call) -> Sys {
        match call {
            Call::DebugPutc { ch } => Sys::DebugPutc { ch },
            Call::DebugWrite { ptr, len } => Sys::DebugWrite { ptr, len },
            Call::Yield => Sys::Yield,
            Call::Retype {
                ut,
                ty,
                param,
                dst,
                dst2,
            } => Sys::Retype {
                ut,
                ty: ObjType::from_u64(ty).unwrap(),
                param,
                dst,
                dst2,
            },
            Call::CapCopy {
                src,
                dst,
                mask,
                prio_ceiling,
            } => Sys::CapCopy {
                src,
                dst,
                mask,
                prio_ceiling,
            },
            Call::CapDelete { slot } => Sys::CapDelete { slot },
            Call::CapRevoke { slot } => Sys::CapRevoke { slot },
            Call::CapInstall { cs, src, dst_index } => Sys::CapInstall { cs, src, dst_index },
            Call::ChanSend {
                chan,
                buf,
                len,
                caps,
            } => Sys::ChanSend {
                chan,
                buf,
                len,
                caps,
            },
            Call::ChanRecv { chan, buf, dests } => Sys::ChanRecv { chan, buf, dests },
            Call::ChanBind {
                chan,
                event,
                notif,
                bits,
            } => Sys::ChanBind {
                chan,
                event: event as usize,
                notif,
                bits,
            },
            Call::NotifSignal { slot, bits } => Sys::NotifSignal { slot, bits },
            Call::NotifWait { slot } => Sys::NotifWait { slot },
            Call::ThreadStart {
                tcb,
                cspace,
                entry,
                sp,
                prio,
                arg,
            } => Sys::ThreadStart {
                tcb,
                cspace,
                entry,
                sp,
                prio: prio as u8,
                arg,
            },
            Call::TimerArm {
                timer,
                notif,
                bits,
                delta,
            } => Sys::TimerArm {
                timer,
                notif,
                bits,
                delta,
            },
            Call::ThreadExit { status } => Sys::ThreadExit { status },
            Call::Map {
                aspace,
                frame,
                va,
                perms,
            } => Sys::Map {
                aspace,
                frame,
                va,
                perms,
            },
            Call::FrameWrite {
                frame,
                off,
                buf,
                len,
            } => Sys::FrameWrite {
                frame,
                off,
                buf,
                len,
            },
            Call::ThreadStartAs {
                tcb,
                cspace,
                aspace,
                entry,
                sp,
                prio,
                arg,
            } => Sys::ThreadStartAs {
                tcb,
                cspace,
                aspace,
                entry,
                sp,
                prio: prio as u8,
                arg,
            },
            Call::FramePaddr { slot } => Sys::FramePaddr { slot },
            Call::ThreadBind {
                tcb,
                which,
                notif,
                bits,
            } => Sys::ThreadBind {
                tcb,
                which: which as usize,
                notif,
                bits,
            },
            Call::ReadReport { tcb } => Sys::ReadReport { tcb },
            Call::UntypedReset { slot } => Sys::UntypedReset { slot },
            Call::AspaceTopUp { aspace, ut, pages } => Sys::AspaceTopUp { aspace, ut, pages },
            Call::IrqBind { irq, notif, bits } => Sys::IrqBind { irq, notif, bits },
            Call::IrqAck { irq } => Sys::IrqAck { irq },
        }
    }

    /// One in-range value of every variant, each with distinct per-register values so a
    /// misplaced argument fails the round-trip (the teeth requirement).
    fn all_variants() -> Vec<Call> {
        vec![
            Call::DebugPutc { ch: 0x11 },
            Call::DebugWrite {
                ptr: 0x21,
                len: 0x22,
            },
            Call::Yield,
            Call::Retype {
                ut: 0x41,
                ty: 7,
                param: 0x43,
                dst: 0x44,
                dst2: 0x45,
            },
            Call::CapCopy {
                src: 0x51,
                dst: 0x52,
                mask: 0x53,
                prio_ceiling: 0x54,
            },
            Call::CapDelete { slot: 0x61 },
            Call::CapRevoke { slot: 0x71 },
            Call::CapInstall {
                cs: 0x81,
                src: 0x82,
                dst_index: 0x83,
            },
            Call::ChanSend {
                chan: 0x91,
                buf: 0x92,
                len: MSG_PAYLOAD,
                caps: 0x94,
            },
            Call::ChanRecv {
                chan: 0xA1,
                buf: 0xA2,
                dests: 0xA3,
            },
            Call::ChanBind {
                chan: 0xB1,
                event: 2,
                notif: 0xB3,
                bits: 0xB4,
            },
            Call::NotifSignal {
                slot: 0xC1,
                bits: 0xC2,
            },
            Call::NotifWait { slot: 0xD1 },
            Call::ThreadStart {
                tcb: 0xE1,
                cspace: 0xE2,
                entry: 0xE3,
                sp: 0xE4,
                prio: NUM_PRIOS - 1,
                arg: 0xE6,
            },
            Call::TimerArm {
                timer: 0xF1,
                notif: 0xF2,
                bits: 0xF3,
                delta: 0xF4,
            },
            Call::ThreadExit { status: 0x101 },
            Call::Map {
                aspace: 0x111,
                frame: 0x112,
                va: 0x113,
                perms: 0x114,
            },
            Call::FrameWrite {
                frame: 0x121,
                off: 0x122,
                buf: 0x123,
                len: 0x124,
            },
            Call::ThreadStartAs {
                tcb: 0x131,
                cspace: 0x132,
                aspace: 0x133,
                entry: 0x134,
                sp: 0x135,
                prio: NUM_PRIOS - 1,
                arg: 0x137,
            },
            Call::FramePaddr { slot: 0x141 },
            Call::ThreadBind {
                tcb: 0x151,
                which: 1,
                notif: 0x153,
                bits: 0x154,
            },
            Call::ReadReport { tcb: 0x161 },
            Call::UntypedReset { slot: 0x171 },
            Call::AspaceTopUp {
                aspace: 0x181,
                ut: 0x182,
                pages: 0x183,
            },
            Call::IrqBind {
                irq: 0x191,
                notif: 0x192,
                bits: 0x193,
            },
            Call::IrqAck { irq: 0x1A1 },
        ]
    }

    /// The §11 host witness: every accepted `encode` round-trips through the *real*
    /// kernel decoder back to the same typed call — userspace-encode ↔ kernel-decode
    /// agreement, the cross-side check Verus cannot express (decode's `ensures` are
    /// shape-only).
    #[test]
    fn encode_round_trips_through_kernel_decode() {
        for call in all_variants() {
            let e = encode(call).expect("in-range call encodes");
            assert_eq!(
                decode(e.nr, args(&e)),
                Ok(bridge(call)),
                "round-trip {call:?}"
            );
        }
    }

    /// Negative control (anti-theatre): the oracle has teeth — perturbing a single
    /// placed register makes the kernel decode a *different* call, so the round-trip
    /// above is not vacuously true.
    #[test]
    fn round_trip_oracle_has_teeth() {
        let call = Call::ChanSend {
            chan: 1,
            buf: 2,
            len: 3,
            caps: 4,
        };
        let e = encode(call).unwrap();
        assert_eq!(decode(e.nr, args(&e)), Ok(bridge(call)));
        // Perturb the buf register: the decoder must see a different ChanSend.
        let mut a = args(&e);
        a[1] ^= 1;
        assert_ne!(decode(e.nr, a), Ok(bridge(call)));
        // Perturb the opcode: a different (or rejected) call entirely.
        assert_ne!(decode(e.nr + 1, args(&e)), Ok(bridge(call)));
    }

    /// `encode` refuses exactly the out-of-range fields the kernel rejects, and the raw
    /// register file those fields would form decodes to the matching kernel error — the
    /// inverse-leak refusal, both sides.
    #[test]
    fn encode_refuses_what_the_kernel_rejects() {
        assert_eq!(
            encode(Call::Retype {
                ut: 0,
                ty: OBJ_COUNT,
                param: 0,
                dst: 0,
                dst2: 0
            }),
            Err(CallError::BadObjType)
        );
        assert_eq!(
            decode(3, [0, OBJ_COUNT, 0, 0, 0, 0, 0]),
            Err(SysError::BadObjType)
        );

        assert_eq!(
            encode(Call::ChanSend {
                chan: 0,
                buf: 0,
                len: MSG_PAYLOAD + 1,
                caps: 0
            }),
            Err(CallError::MsgTooLong)
        );
        assert_eq!(
            decode(8, [0, 0, MSG_PAYLOAD + 1, 0, 0, 0, 0]),
            Err(SysError::MsgTooLong)
        );

        assert_eq!(
            encode(Call::ChanBind {
                chan: 0,
                event: 3,
                notif: 0,
                bits: 0
            }),
            Err(CallError::BadEvent)
        );
        assert_eq!(decode(10, [0, 3, 0, 0, 0, 0, 0]), Err(SysError::BadEvent));

        assert_eq!(
            encode(Call::ThreadBind {
                tcb: 0,
                which: 2,
                notif: 0,
                bits: 0
            }),
            Err(CallError::BadWhich)
        );
        assert_eq!(decode(21, [0, 2, 0, 0, 0, 0, 0]), Err(SysError::BadWhich));

        assert_eq!(
            encode(Call::ThreadStart {
                tcb: 0,
                cspace: 0,
                entry: 0,
                sp: 0,
                prio: NUM_PRIOS,
                arg: 0
            }),
            Err(CallError::BadPrio)
        );
        assert_eq!(
            decode(13, [0, 0, 0, 0, NUM_PRIOS, 0, 0]),
            Err(SysError::BadPrio)
        );
        assert_eq!(
            encode(Call::ThreadStartAs {
                tcb: 0,
                cspace: 0,
                aspace: 0,
                entry: 0,
                sp: 0,
                prio: NUM_PRIOS,
                arg: 0
            }),
            Err(CallError::BadPrio)
        );
        assert_eq!(
            decode(18, [0, 0, 0, 0, 0, NUM_PRIOS, 0]),
            Err(SysError::BadPrio)
        );
    }

    /// The local ABI bound constants are an independent twin of the kernel's; pin them
    /// so a drift between this crate and kcore is caught here, not silently in
    /// production (the rev2§3.7 contract is a *shared* one).
    #[test]
    fn constants_match_kcore() {
        assert_eq!(MSG_PAYLOAD, kcore::channel::MSG_PAYLOAD as u64);
        assert_eq!(NUM_PRIOS, kcore::sysabi::NUM_PRIOS as u64);
        assert!(ObjType::from_u64(OBJ_COUNT - 1).is_some());
        assert!(ObjType::from_u64(OBJ_COUNT).is_none());
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Random in-range register values round-trip for the unvalidated four-arg
        /// shapes (placement is value-independent, so random values exercise it).
        #[test]
        fn map_round_trips(aspace: u64, frame: u64, va: u64, perms: u64) {
            let call = Call::Map { aspace, frame, va, perms };
            let e = encode(call).unwrap();
            prop_assert_eq!(decode(e.nr, args(&e)), Ok(bridge(call)));
        }

        /// ChanSend across the payload boundary: `encode` accepts iff `len <=
        /// MSG_PAYLOAD`, and an accepted one round-trips — the verified refusal, tested.
        #[test]
        fn chan_send_honors_payload_cap(chan: u64, buf: u64, len in 0u64..512, caps: u64) {
            let call = Call::ChanSend { chan, buf, len, caps };
            match encode(call) {
                Ok(e) => {
                    prop_assert!(len <= MSG_PAYLOAD);
                    prop_assert_eq!(decode(e.nr, args(&e)), Ok(bridge(call)));
                }
                Err(err) => {
                    prop_assert!(len > MSG_PAYLOAD);
                    prop_assert_eq!(err, CallError::MsgTooLong);
                }
            }
        }

        /// ThreadStart across the priority boundary: accepts iff `prio < NUM_PRIOS`.
        #[test]
        fn thread_start_honors_prio_cap(
            tcb: u64, cspace: u64, entry: u64, sp: u64, prio in 0u64..64, arg: u64,
        ) {
            let call = Call::ThreadStart { tcb, cspace, entry, sp, prio, arg };
            match encode(call) {
                Ok(e) => {
                    prop_assert!(prio < NUM_PRIOS);
                    prop_assert_eq!(decode(e.nr, args(&e)), Ok(bridge(call)));
                }
                Err(err) => {
                    prop_assert!(prio >= NUM_PRIOS);
                    prop_assert_eq!(err, CallError::BadPrio);
                }
            }
        }
    }
}
