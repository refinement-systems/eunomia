// SPDX-License-Identifier: 0BSD
//! Syscall dispatch (M1 surface).
//!
//! ABI: SVC #0 with the syscall number in x7, arguments in x0..x5, primary
//! result in x0 (negative = error), secondary in x1. Capability arguments
//! are slot indices into the calling thread's cspace. User pointers must
//! lie inside the identity-mapped user window (until M3 address spaces).
//!
//! This ABI is a scaffold, not a stable surface — the public syscall ABI
//! is deferred (rev2§3.7, rev2§8).

use crate::channel::{self, ChanError, MSG_CAPS, MSG_PAYLOAD};
use crate::cspace::{self, CSpaceObj, CapKind, CapSlot, Rights};
use crate::irq;
use crate::mmu::{USER_BASE, USER_SIZE};
use crate::notification;
use crate::store::KernelStore;
use crate::thread::{self, ThreadState, TrapFrame};
use crate::timer;
use crate::untyped::{self, ObjType, RetypeError};
use core::ptr;
use kcore::aspace::AspaceObj;
use kcore::channel::Channel;
use kcore::id::{ObjId, SlotId};
use kcore::irq::IrqObj;
use kcore::notification::NotifObj;
use kcore::sysabi::{self, Sys, SysError};
use kcore::thread::Tcb;
use kcore::timer::TimerObj;

// Handle → pointer for the architectural shell paths that legitimately own the
// object (this file places objects in donated untyped, switches frames, and
// bumps refcounts directly). The `CapKind`/`Tcb` fields carry opaque
// `ObjId` handles; resolve them back to the live address the shell code
// dereferences — the same boundary the scheduler shell
// (`kernel/src/thread.rs`) uses.
#[inline]
unsafe fn cspace_ptr(o: ObjId) -> *mut CSpaceObj {
    o.0 as *mut CSpaceObj
}
#[inline]
unsafe fn tcb_ptr(o: ObjId) -> *mut Tcb {
    o.0 as *mut Tcb
}
#[inline]
unsafe fn aspace_ptr(o: ObjId) -> *mut AspaceObj {
    o.0 as *mut AspaceObj
}
#[inline]
unsafe fn chan_ptr(o: ObjId) -> *mut Channel {
    o.0 as *mut Channel
}
#[inline]
unsafe fn notif_ptr(o: ObjId) -> *mut NotifObj {
    o.0 as *mut NotifObj
}
#[inline]
unsafe fn timer_ptr(o: ObjId) -> *mut TimerObj {
    o.0 as *mut TimerObj
}
#[inline]
unsafe fn irq_ptr(o: ObjId) -> *mut IrqObj {
    o.0 as *mut IrqObj
}

pub const ERR_BADSLOT: i64 = -1;
pub const ERR_TYPE: i64 = -2;
pub const ERR_PERM: i64 = -3;
pub const ERR_FULL: i64 = -4;
pub const ERR_EMPTY: i64 = -5;
pub const ERR_NOSLOT: i64 = -6;
pub const ERR_FAULT: i64 = -7;
pub const ERR_NOMEM: i64 = -8;
pub const ERR_ARG: i64 = -9;
pub const ERR_CLOSED: i64 = -10;
pub const ERR_STATE: i64 = -11;
/// Retry-later: `CapRevoke` returns this when a bounded quantum ends with
/// descendants still remaining, and `CapCopy` returns it when the source is under
/// an in-flight revoke (rev2§2.2 preemptible/restartable walk). The caller loops.
pub const ERR_AGAIN: i64 = -12;

pub const SLOT_NONE: u32 = u32::MAX;

/// Leaf-deletions performed per `CapRevoke` call before the kernel returns
/// `ERR_AGAIN`. A shell-policy constant (rev2§6.1(d)), chosen ≪ a 10 ms
/// scheduler tick's worth of deletions so interrupt latency stays bounded by one
/// quantum; the caller (`ipc::sys::cap_revoke_all`) re-issues from EL0 where IRQs
/// are unmasked. Tuning is left to measurement — not a verified parameter.
pub const REVOKE_QUANTUM: usize = 16;

/// Validate a user buffer for the current thread before the kernel
/// dereferences it. Identity-map threads (M1 scaffold, idle) get the
/// fixed EL0 window; aspace threads get a page-table walk — the kernel
/// reads user memory through the thread's own translation, so the walk
/// is exactly the check that the access cannot fault at EL1.
unsafe fn user_range_ok(ptr: u64, len: u64) -> bool {
    let t = thread::current();
    match (*t).aspace {
        None => {
            len <= USER_SIZE
                && ptr >= USER_BASE
                && ptr
                    .checked_add(len)
                    .is_some_and(|end| end <= USER_BASE + USER_SIZE)
        }
        Some(a) => crate::aspace::range_mapped(aspace_ptr(a), ptr, len, false),
    }
}

unsafe fn user_range_writable(ptr: u64, len: u64) -> bool {
    let t = thread::current();
    match (*t).aspace {
        None => user_range_ok(ptr, len),
        Some(a) => crate::aspace::range_mapped(aspace_ptr(a), ptr, len, true),
    }
}

/// Slot `idx` of the current thread's cspace, or null.
unsafe fn cur_slot(idx: u64) -> *mut CapSlot {
    if idx > u32::MAX as u64 {
        return ptr::null_mut();
    }
    let Some(cs) = (*thread::current()).cspace else {
        return ptr::null_mut();
    };
    CSpaceObj::slot(cspace_ptr(cs), idx as u32)
}

/// Resolve a user array of 4 slot indices (SLOT_NONE = absent) into slot
/// pointers. `must_be_full(i)` distinguishes send (slots must hold caps)
/// from recv (slots must be empty); recv emptiness is checked by the
/// channel itself.
unsafe fn resolve_cap_list(
    list_ptr: u64,
    out: &mut [*mut CapSlot; MSG_CAPS],
    require_full: bool,
) -> Result<(), i64> {
    *out = [ptr::null_mut(); MSG_CAPS];
    if list_ptr == 0 {
        return Ok(());
    }
    if !user_range_ok(list_ptr, (MSG_CAPS * 4) as u64) || list_ptr % 4 != 0 {
        return Err(ERR_FAULT);
    }
    let arr = list_ptr as *const u32;
    for i in 0..MSG_CAPS {
        let idx = arr.add(i).read();
        if idx == SLOT_NONE {
            continue;
        }
        let s = cur_slot(idx as u64);
        if s.is_null() {
            return Err(ERR_BADSLOT);
        }
        if require_full && (*s).cap.is_empty() {
            return Err(ERR_BADSLOT);
        }
        out[i] = s;
    }
    Ok(())
}

/// Map a decode-time validation failure to the errno. Every such
/// condition returns `ERR_ARG`, the value the dispatch returns for any
/// argument-validation failure.
fn errno_of(_e: SysError) -> i64 {
    ERR_ARG
}

/// Returns Some(result for x0), or None when the thread blocked (its x0
/// will be written by whoever wakes it).
///
/// The pure decode + argument validation lives in [`kcore::sysabi::decode`];
/// this consumes the typed [`Sys`] and does the capability lookup, rights
/// checks, user-pointer validation, and the operation.
pub unsafe fn dispatch(frame: *mut TrapFrame) -> Option<i64> {
    let nr = (*frame).x[7];
    let a = [
        (*frame).x[0],
        (*frame).x[1],
        (*frame).x[2],
        (*frame).x[3],
        (*frame).x[4],
        (*frame).x[5],
        (*frame).x[6],
    ];
    match sysabi::decode(nr, a) {
        Ok(sys) => execute(sys, frame),
        Err(e) => Some(errno_of(e)),
    }
}

/// Execute a decoded syscall against live kernel state. Argument extraction
/// is done by `decode`; each arm performs the capability lookup (`cur_slot`),
/// rights checks, user-range validation, and the operation.
unsafe fn execute(sys: Sys, frame: *mut TrapFrame) -> Option<i64> {
    match sys {
        // DebugPutc/DebugWrite (rev2§7): a disclosed *kernel-diagnostic* path behind
        // the `debug-log` build feature (default-on for dev images). With the feature
        // off these EL0 syscalls are inert no-ops — gated off for EL0. The decoder
        // still produces both variants, so the arms stay; only the body is
        // conditional. The shell does not call them for terminal I/O (that crosses
        // the console channel); their users are server diagnostics and panic
        // reporting.
        Sys::DebugPutc { ch } => {
            #[cfg(feature = "debug-log")]
            {
                use core::fmt::Write;
                let mut uart = crate::uart::Uart::new();
                let _ = uart.write_char(ch as u8 as char);
            }
            #[cfg(not(feature = "debug-log"))]
            let _ = ch;
            Some(0)
        }
        Sys::DebugWrite { ptr, len } => {
            #[cfg(feature = "debug-log")]
            {
                if !user_range_ok(ptr, len) || len > sysabi::DEBUG_WRITE_MAX {
                    return Some(ERR_FAULT);
                }
                use core::fmt::Write;
                let mut uart = crate::uart::Uart::new();
                for i in 0..len {
                    let b = ((ptr + i) as *const u8).read();
                    let _ = uart.write_char(b as char);
                }
            }
            #[cfg(not(feature = "debug-log"))]
            let _ = (ptr, len);
            Some(0)
        }
        Sys::Yield => {
            // Set our own return value before the frame may be swapped for
            // the incoming thread's — writing x0 after the switch would
            // clobber the winner's register state.
            (*frame).x[0] = 0;
            thread::maybe_switch(frame, true);
            None
        }
        Sys::Retype {
            ut,
            ty,
            param,
            dst,
            dst2,
        } => {
            let ut = cur_slot(ut);
            let dst = cur_slot(dst);
            if ut.is_null() || dst.is_null() {
                return Some(ERR_BADSLOT);
            }
            let dst2 = if ty == ObjType::Channel {
                let d2 = cur_slot(dst2);
                if d2.is_null() {
                    return Some(ERR_BADSLOT);
                }
                d2
            } else {
                ptr::null_mut()
            };
            Some(match untyped::retype(ut, ty, param, dst, dst2) {
                Ok(()) => 0,
                Err(RetypeError::NotUntyped) => ERR_TYPE,
                Err(RetypeError::DestOccupied) => ERR_NOSLOT,
                Err(RetypeError::NoMemory) => ERR_NOMEM,
                Err(RetypeError::BadArg) => ERR_ARG,
            })
        }
        // cap_copy — derive, monotone (rev2§2.3). `prio_ceiling` attenuates a thread-cap
        // copy's rev2§5.4 ceiling to `min(parent, prio_ceiling)` (the supervision grant);
        // `NO_PRIO_CEILING` (0xFF) from the default `cap_copy` leaves it unchanged.
        Sys::CapCopy {
            src,
            dst,
            mask,
            prio_ceiling,
        } => {
            let src = cur_slot(src);
            let dst = cur_slot(dst);
            if src.is_null() || dst.is_null() {
                return Some(ERR_BADSLOT);
            }
            // Refuse derivation into an in-flight revoke subtree with a
            // distinguishable `ERR_AGAIN` (retry once the revoke finishes).
            // `derive` already rejects this internally, but only as a bare
            // `Err(())` indistinguishable from a structural failure, so we
            // pre-check the (read-only) ancestor-walk here. Single-core, masked
            // at EL1: the check and the derive run atomically (no TOCTOU).
            if cspace::ancestor_or_self_revoking(&KernelStore, SlotId(src as u64)) {
                return Some(ERR_AGAIN);
            }
            Some(
                match cspace::derive(
                    &mut KernelStore,
                    SlotId(src as u64),
                    SlotId(dst as u64),
                    mask as u8,
                    prio_ceiling as u8,
                ) {
                    Ok(()) => 0,
                    Err(()) => ERR_NOSLOT,
                },
            )
        }
        Sys::CapDelete { slot } => {
            let s = cur_slot(slot);
            if s.is_null() || (*s).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            cspace::delete(s);
            Some(0)
        }
        Sys::CapRevoke { slot } => {
            let s = cur_slot(slot);
            if s.is_null() || (*s).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            // Do one bounded quantum of leaf-deletions and return. `More`
            // means the subtree still has descendants — userspace re-issues
            // (`ERR_AGAIN`); `Done` means the walk completed. The masked-EL1
            // entry is unchanged: preemption is delivered at the syscall
            // boundary, not by unmasking mid-walk (honesty note 2).
            match cspace::revoke_step(s, REVOKE_QUANTUM) {
                cspace::RevokeStatus::Done => Some(0),
                cspace::RevokeStatus::More => Some(ERR_AGAIN),
            }
        }
        // cap_install — move a cap into another cspace (explicit child-cspace
        // construction, rev2§5.1).
        Sys::CapInstall { cs, src, dst_index } => {
            let cs_slot = cur_slot(cs);
            let src = cur_slot(src);
            if cs_slot.is_null() || src.is_null() || (*src).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::CSpace(cs) = (*cs_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if dst_index > u32::MAX as u64 {
                return Some(ERR_ARG);
            }
            let dst = CSpaceObj::slot(cspace_ptr(cs), dst_index as u32);
            if dst.is_null() {
                return Some(ERR_BADSLOT);
            }
            if !(*dst).cap.is_empty() {
                return Some(ERR_NOSLOT);
            }
            cspace::slot_move(&mut KernelStore, SlotId(src as u64), SlotId(dst as u64));
            Some(0)
        }
        // chan_send: `len` is already <= MSG_PAYLOAD (decode), so the
        // `data.len() as u16` inside channel::send is lossless.
        Sys::ChanSend {
            chan,
            buf,
            len,
            caps,
        } => {
            let s = cur_slot(chan);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Channel(ch, end) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            if !user_range_ok(buf, len) {
                return Some(ERR_FAULT);
            }
            let data = core::slice::from_raw_parts(buf as *const u8, len as usize);
            let mut capslots = [ptr::null_mut(); MSG_CAPS];
            if let Err(e) = resolve_cap_list(caps, &mut capslots, true) {
                return Some(e);
            }
            Some(match channel::send(chan_ptr(ch), end, data, &capslots) {
                Ok(()) => 0,
                Err(ChanError::Full) => ERR_FULL,
                Err(ChanError::PeerClosed) => ERR_CLOSED,
                Err(_) => ERR_ARG,
            })
        }
        // chan_recv → x0=len, x1=mask
        Sys::ChanRecv { chan, buf, dests } => {
            let s = cur_slot(chan);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Channel(ch, end) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::READ) {
                return Some(ERR_PERM);
            }
            if !user_range_ok(buf, MSG_PAYLOAD as u64) {
                return Some(ERR_FAULT);
            }
            if !user_range_writable(buf, MSG_PAYLOAD as u64) {
                return Some(ERR_FAULT);
            }
            let mut destslots = [ptr::null_mut(); MSG_CAPS];
            if let Err(e) = resolve_cap_list(dests, &mut destslots, false) {
                return Some(e);
            }
            let mut rbuf = [0u8; MSG_PAYLOAD];
            match channel::recv(chan_ptr(ch), end, &mut rbuf, &destslots) {
                Ok((len, mask)) => {
                    core::ptr::copy_nonoverlapping(rbuf.as_ptr(), buf as *mut u8, len);
                    (*frame).x[1] = mask as u64;
                    Some(len as i64)
                }
                Err(ChanError::Empty) => Some(ERR_EMPTY),
                Err(ChanError::NoCapSlot) => Some(ERR_NOSLOT),
                Err(_) => Some(ERR_ARG),
            }
        }
        // chan_bind: `event` is already <= 2 (decode).
        Sys::ChanBind {
            chan,
            event,
            notif,
            bits,
        } => {
            let s = cur_slot(chan);
            let ns = cur_slot(notif);
            if s.is_null() || ns.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Channel(ch, end) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::Notification(n) = (*ns).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*ns).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            channel::bind(&mut KernelStore, ch, end, event, Some(n), bits);
            Some(0)
        }
        Sys::NotifSignal { slot, bits } => {
            let s = cur_slot(slot);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Notification(n) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            notification::signal(notif_ptr(n), bits);
            Some(0)
        }
        // notif_wait → accumulated word
        Sys::NotifWait { slot } => {
            let s = cur_slot(slot);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Notification(n) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::READ) {
                return Some(ERR_PERM);
            }
            match notification::wait(&mut KernelStore, n, ObjId(thread::current() as u64)) {
                Some(word) => Some(word as i64),
                None => None, // blocked; signal() writes x0 on wake
            }
        }
        // thread_start: `prio` is already masked + < NUM_PRIOS (decode).
        Sys::ThreadStart {
            tcb,
            cspace,
            entry,
            sp,
            prio,
            arg,
        } => {
            let ts = cur_slot(tcb);
            let cs_slot = cur_slot(cspace);
            if ts.is_null() || cs_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t, max_prio) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::CSpace(cs) = (*cs_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            let tp = tcb_ptr(t);
            let csp = cspace_ptr(cs);
            if (*tp).state != ThreadState::Inactive {
                return Some(ERR_STATE);
            }
            if !user_range_ok(entry, 4) || !user_range_ok(sp, 0) {
                return Some(ERR_FAULT);
            }
            // rev2§5.4 maximum-controlled-priority: the ceiling is carried on the
            // thread cap (`max_prio`, stamped at retype = the retyper's
            // priority), so spawn gates on the cap, not the caller's live
            // priority — the lattice stays monotone and the ceiling is
            // cap-attenuated through `kcore::cspace::derive`. The refusal is
            // the verified `set_priority` op's own branch (rev2§6.1(d)): an
            // over-ceiling `prio` returns `Err` and leaves the TCB untouched.
            // Gated here, before any state mutation, so no refcount is bumped on
            // refusal.
            if thread::set_priority(tp, prio, max_prio).is_err() {
                return Some(ERR_PERM);
            }
            (*csp).hdr.refs += 1;
            (*tp).cspace = Some(cs);
            (*tp).frame = TrapFrame::zeroed();
            (*tp).frame.elr = entry;
            (*tp).frame.sp_el0 = sp;
            (*tp).frame.spsr = 0; // EL0t, interrupts enabled
            (*tp).frame.x[0] = arg;
            thread::enqueue(tp);
            Some(0)
        }
        // timer_arm
        Sys::TimerArm {
            timer,
            notif,
            bits,
            delta,
        } => {
            let ts = cur_slot(timer);
            let ns = cur_slot(notif);
            if ts.is_null() || ns.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Timer(t) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::Notification(n) = (*ns).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*ns).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            timer::arm(
                timer_ptr(t),
                notif_ptr(n),
                bits,
                timer::counter().saturating_add(delta),
            );
            Some(0)
        }
        // irq_bind — the TimerArm twin: bind an IRQ-handler cap to a
        // (notification, bits) pair, so a hardware interrupt on the line signals
        // that notification (rev2§1, rev2§3.6). WRITE on the notif is required —
        // the kernel signals through it (as TimerArm/ChanBind do).
        Sys::IrqBind {
            irq: irq_cap,
            notif,
            bits,
        } => {
            let is = cur_slot(irq_cap);
            let ns = cur_slot(notif);
            if is.is_null() || ns.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Irq(i) = (*is).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::Notification(n) = (*ns).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*ns).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            irq::bind(irq_ptr(i), notif_ptr(n), bits);
            Some(0)
        }
        // irq_ack — the "acknowledge" half of rev2§1: clear the mask
        // and re-enable the line after the driver has serviced the device.
        Sys::IrqAck { irq: irq_cap } => {
            let is = cur_slot(irq_cap);
            if is.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Irq(i) = (*is).cap.kind else {
                return Some(ERR_TYPE);
            };
            irq::ack(irq_ptr(i));
            Some(0)
        }
        // thread_exit — the only voluntary stop (rev2§5.1). The kernel records
        // the status, so a child can neither lie about nor forget its own
        // death; the on-exit binding fires here.
        Sys::ThreadExit { status } => {
            let t = thread::current();
            (*t).state = ThreadState::Halted;
            thread::report_terminal(t, thread::Report::Exited(status));
            // maybe_switch at exception exit picks someone else; the dead
            // frame is never restored, so there is no x0 to write.
            None
        }
        // map(aspace, frame, va, perms) — rev2§2.5: the mapping lives in the
        // frame cap; mapping an already-mapped cap fails (copy the cap to
        // map the frame twice).
        Sys::Map {
            aspace,
            frame: fr,
            va,
            perms,
        } => {
            let asp_slot = cur_slot(aspace);
            let fr_slot = cur_slot(fr);
            if asp_slot.is_null() || fr_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Aspace(asp) = (*asp_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*asp_slot).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            let CapKind::Frame { mapping, .. } = (*fr_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if mapping.is_some() {
                return Some(ERR_STATE);
            }
            // Monotone: an RO frame cap cannot become writable memory.
            if perms & crate::aspace::PERM_W != 0 && !(*fr_slot).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            // Device mappings only via phys-capable caps (rev2§2.5).
            if perms & crate::aspace::PERM_DEVICE != 0 && !(*fr_slot).cap.rights.has(Rights::PHYS) {
                return Some(ERR_PERM);
            }
            // Cap-side bookkeeping (the mapping record + aspace refcount bump) is the verified
            // `cspace::map_frame` (rev2§6.1(c)), symmetric with the delete/unmap path; it
            // drives the page-table write through the `aspace_map` Store seam. The shell keeps
            // only the access-control validation above (rev2§6.1(d)-style).
            match crate::cspace::map_frame(fr_slot, asp, va, perms) {
                Ok(()) => Some(0),
                Err(crate::aspace::MapError::NeedMemory) => Some(ERR_NOMEM),
                Err(crate::aspace::MapError::AlreadyMapped) => Some(ERR_STATE),
                Err(_) => Some(ERR_ARG),
            }
        }
        // frame_write(frame, offset, buf, len) — spawn-time program loading.
        // The offset+len overflow / bounds check stays here: it is against the
        // frame cap's `pages` (live state) and is already panic-safe.
        Sys::FrameWrite {
            frame: fr,
            off,
            buf,
            len,
        } => {
            let fr_slot = cur_slot(fr);
            if fr_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Frame { base, pages, .. } = (*fr_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*fr_slot).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            if off.checked_add(len).is_none_or(|end| end > pages * 4096) {
                return Some(ERR_ARG);
            }
            if !user_range_ok(buf, len) {
                return Some(ERR_FAULT);
            }
            core::ptr::copy_nonoverlapping(buf as *const u8, (base + off) as *mut u8, len as usize);
            Some(0)
        }
        // thread_start_as: `prio` already masked + < NUM_PRIOS (decode).
        Sys::ThreadStartAs {
            tcb,
            cspace,
            aspace,
            entry,
            sp,
            prio,
            arg,
        } => {
            let ts = cur_slot(tcb);
            let cs_slot = cur_slot(cspace);
            let asp_slot = cur_slot(aspace);
            if ts.is_null() || cs_slot.is_null() || asp_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t, max_prio) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::CSpace(cs) = (*cs_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::Aspace(asp) = (*asp_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            let tp = tcb_ptr(t);
            let csp = cspace_ptr(cs);
            let asp_ptr = aspace_ptr(asp);
            if (*tp).state != ThreadState::Inactive {
                return Some(ERR_STATE);
            }
            // rev2§5.4 ceiling carried on the thread cap (see ThreadStart). The
            // verified `set_priority` op makes the refusal itself (rev2§6.1(d)),
            // gated here — before the range check and any refcount bump, the prior
            // shell gate's position — so an over-ceiling `prio` returns `Err` →
            // `ERR_PERM` (ahead of any `ERR_FAULT`) with the TCB untouched.
            if thread::set_priority(tp, prio, max_prio).is_err() {
                return Some(ERR_PERM);
            }
            // Entry/SP live in the child's aspace, not the caller's.
            if !crate::aspace::range_mapped(asp_ptr, entry, 4, false) {
                return Some(ERR_FAULT);
            }
            (*csp).hdr.refs += 1;
            (*asp_ptr).hdr.refs += 1;
            (*tp).cspace = Some(cs);
            (*tp).aspace = Some(asp);
            (*tp).frame = TrapFrame::zeroed();
            (*tp).frame.elr = entry;
            (*tp).frame.sp_el0 = sp;
            (*tp).frame.spsr = 0; // EL0t, interrupts enabled
            (*tp).frame.x[0] = arg; // rev2§5.1: the new thread's initial x0 (in-process spawn)
            thread::enqueue(tp);
            Some(0)
        }
        // frame_paddr → PA. Gated on the phys-read bit (rev2§2.5): only the
        // DmaPool holder's caps carry it.
        Sys::FramePaddr { slot } => {
            let s = cur_slot(slot);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Frame { base, .. } = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::PHYS) {
                return Some(ERR_PERM);
            }
            Some(base as i64)
        }
        // thread_bind(tcb, which, notif, bits) — configure the on-exit /
        // on-fault slot (rev2§5.1). `which` is already <= 1 (decode). The notif
        // cap moves into the TCB's CDT-visible slot; notif = SLOT_NONE
        // unbinds. A child holds no cap to its own threads, so it can neither
        // silence nor forge its own death notice.
        Sys::ThreadBind {
            tcb,
            which,
            notif,
            bits,
        } => {
            let ts = cur_slot(tcb);
            if ts.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t, _) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            // bind-reports gates slot configuration (rev2§2.3): a supervisor holds
            // it; an attenuated observer does not.
            if !(*ts).cap.rights.has(Rights::BIND_REPORTS) {
                return Some(ERR_PERM);
            }
            let ns = if notif == SLOT_NONE as u64 {
                ptr::null_mut()
            } else {
                let ns = cur_slot(notif);
                if ns.is_null() {
                    return Some(ERR_BADSLOT);
                }
                let CapKind::Notification(_) = (*ns).cap.kind else {
                    return Some(ERR_TYPE);
                };
                // The kernel will signal through this cap (rev2§3.6).
                if !(*ns).cap.rights.has(Rights::WRITE) {
                    return Some(ERR_PERM);
                }
                ns
            };
            thread::bind(tcb_ptr(t), which, ns, bits);
            Some(0)
        }
        // read_report(tcb) → x0 = 0 running | 1 exited | 2 faulted,
        // x1 = status / cause, x2 = faulting address (rev2§5.1).
        Sys::ReadReport { tcb } => {
            let ts = cur_slot(tcb);
            if ts.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t, _) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            // read-report gates the read (rev2§2.3).
            if !(*ts).cap.rights.has(Rights::READ_REPORT) {
                return Some(ERR_PERM);
            }
            let (code, v1, v2) = match (*tcb_ptr(t)).report {
                thread::Report::Running => (0, 0, 0),
                thread::Report::Exited(status) => (1, status, 0),
                thread::Report::Faulted { cause, far } => (2, cause, far),
            };
            (*frame).x[1] = v1;
            (*frame).x[2] = v2;
            Some(code)
        }
        // untyped_reset(ut) — watermark back to 0 once the caller has revoked
        // every object carved from it (rev2§2.5). Pairs with cap_revoke for
        // per-spawn donation reuse (rev2§5.1); refuses while CDT children remain
        // so a live object can never be reused under.
        Sys::UntypedReset { slot } => {
            let s = cur_slot(slot);
            if s.is_null() || (*s).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            Some(match untyped::reset(&mut KernelStore, SlotId(s as u64)) {
                Ok(()) => 0,
                Err(RetypeError::NotUntyped) => ERR_TYPE,
                // reset's only other failure is "still has children".
                Err(_) => ERR_STATE,
            })
        }
        // aspace_topup(aspace, ut, pages) — grow the aspace's page-table pool by
        // `pages` tables carved to abut its current end from the donated untyped
        // (rev2§2.5 "accepts top-ups"). Makes an exhausted-pool `ERR_NOMEM`
        // recoverable: the caller donates untyped, grows the pool, retries `map`.
        // Validation mirrors `Sys::Map` (aspace cap + WRITE right); the untyped
        // needs no rights check, as `Sys::Retype`. The carve/accounting is the
        // trusted shell (`untyped::aspace_topup`); growth soundness is the
        // verified `lemma_grow_pool`.
        Sys::AspaceTopUp { aspace, ut, pages } => {
            let asp_slot = cur_slot(aspace);
            let ut_slot = cur_slot(ut);
            if asp_slot.is_null() || ut_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Aspace(asp) = (*asp_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*asp_slot).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            Some(
                match untyped::aspace_topup(ut_slot, aspace_ptr(asp), pages) {
                    Ok(()) => 0,
                    // not an untyped cap
                    Err(RetypeError::NotUntyped) => ERR_TYPE,
                    // the untyped has no room for `pages` more tables
                    Err(RetypeError::NoMemory) => ERR_NOMEM,
                    // non-abutting untyped, or pages == 0 / overflow
                    Err(_) => ERR_ARG,
                },
            )
        }
    }
}
