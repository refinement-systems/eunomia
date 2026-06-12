//! Syscall dispatch (M1 surface).
//!
//! ABI: SVC #0 with the syscall number in x7, arguments in x0..x5, primary
//! result in x0 (negative = error), secondary in x1. Capability arguments
//! are slot indices into the calling thread's cspace. User pointers must
//! lie inside the identity-mapped user window (until M3 address spaces).
//!
//! This ABI is a milestone scaffold, not a stable surface — the public
//! syscall ABI is an explicit later milestone (§3.7, §8).

use crate::channel::{self, ChanError, MSG_CAPS, MSG_PAYLOAD};
use crate::cspace::{self, CapKind, CapSlot, CSpaceObj, Rights};
use crate::mmu::{USER_BASE, USER_SIZE};
use crate::notification;
use crate::thread::{self, ThreadState, TrapFrame, NUM_PRIOS};
use crate::timer;
use crate::untyped::{self, ObjType, RetypeError};
use core::ptr;

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

pub const SLOT_NONE: u32 = u32::MAX;

/// Validate a user buffer for the current thread before the kernel
/// dereferences it. Identity-map threads (M1 scaffold, idle) get the
/// fixed EL0 window; aspace threads get a page-table walk — the kernel
/// reads user memory through the thread's own translation, so the walk
/// is exactly the check that the access cannot fault at EL1.
unsafe fn user_range_ok(ptr: u64, len: u64) -> bool {
    let t = thread::current();
    if (*t).aspace.is_null() {
        len <= USER_SIZE
            && ptr >= USER_BASE
            && ptr.checked_add(len).is_some_and(|end| end <= USER_BASE + USER_SIZE)
    } else {
        crate::aspace::AspaceObj::range_mapped((*t).aspace, ptr, len, false)
    }
}

unsafe fn user_range_writable(ptr: u64, len: u64) -> bool {
    let t = thread::current();
    if (*t).aspace.is_null() {
        user_range_ok(ptr, len)
    } else {
        crate::aspace::AspaceObj::range_mapped((*t).aspace, ptr, len, true)
    }
}

/// Slot `idx` of the current thread's cspace, or null.
unsafe fn cur_slot(idx: u64) -> *mut CapSlot {
    if idx > u32::MAX as u64 {
        return ptr::null_mut();
    }
    let cs = (*thread::current()).cspace;
    if cs.is_null() {
        return ptr::null_mut();
    }
    CSpaceObj::slot(cs, idx as u32)
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

/// Returns Some(result for x0), or None when the thread blocked (its x0
/// will be written by whoever wakes it).
pub unsafe fn dispatch(frame: *mut TrapFrame) -> Option<i64> {
    let nr = (*frame).x[7];
    let a = [
        (*frame).x[0],
        (*frame).x[1],
        (*frame).x[2],
        (*frame).x[3],
        (*frame).x[4],
        (*frame).x[5],
    ];
    match nr {
        // debug_putc(ch)
        0 => {
            use core::fmt::Write;
            let mut uart = crate::uart::Uart::new();
            let _ = uart.write_char(a[0] as u8 as char);
            Some(0)
        }
        // debug_write(ptr, len)
        1 => {
            if !user_range_ok(a[0], a[1]) || a[1] > 1024 {
                return Some(ERR_FAULT);
            }
            use core::fmt::Write;
            let mut uart = crate::uart::Uart::new();
            for i in 0..a[1] {
                let b = ((a[0] + i) as *const u8).read();
                let _ = uart.write_char(b as char);
            }
            Some(0)
        }
        // yield
        2 => {
            // Set our own return value before the frame may be swapped for
            // the incoming thread's — writing x0 after the switch would
            // clobber the winner's register state.
            (*frame).x[0] = 0;
            thread::maybe_switch(frame, true);
            None
        }
        // retype(ut_slot, ty, param, dst_slot, dst2_slot)
        3 => {
            let ut = cur_slot(a[0]);
            let dst = cur_slot(a[3]);
            if ut.is_null() || dst.is_null() {
                return Some(ERR_BADSLOT);
            }
            let Some(ty) = ObjType::from_u64(a[1]) else {
                return Some(ERR_ARG);
            };
            let dst2 = if ty == ObjType::Channel {
                let d2 = cur_slot(a[4]);
                if d2.is_null() {
                    return Some(ERR_BADSLOT);
                }
                d2
            } else {
                ptr::null_mut()
            };
            Some(match untyped::retype(ut, ty, a[2], dst, dst2) {
                Ok(()) => 0,
                Err(RetypeError::NotUntyped) => ERR_TYPE,
                Err(RetypeError::DestOccupied) => ERR_NOSLOT,
                Err(RetypeError::NoMemory) => ERR_NOMEM,
                Err(RetypeError::BadArg) => ERR_ARG,
            })
        }
        // cap_copy(src, dst, rights_mask) — derive, monotone (§2.3)
        4 => {
            let src = cur_slot(a[0]);
            let dst = cur_slot(a[1]);
            if src.is_null() || dst.is_null() {
                return Some(ERR_BADSLOT);
            }
            Some(match cspace::derive(src, dst, a[2] as u8) {
                Ok(()) => 0,
                Err(()) => ERR_NOSLOT,
            })
        }
        // cap_delete(slot)
        5 => {
            let s = cur_slot(a[0]);
            if s.is_null() || (*s).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            cspace::delete(s);
            Some(0)
        }
        // cap_revoke(slot)
        6 => {
            let s = cur_slot(a[0]);
            if s.is_null() || (*s).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            cspace::revoke(s);
            Some(0)
        }
        // cap_install(cspace_cap_slot, src_slot, dst_index) — move a cap
        // into another cspace (explicit child-cspace construction, §5.1).
        7 => {
            let cs_slot = cur_slot(a[0]);
            let src = cur_slot(a[1]);
            if cs_slot.is_null() || src.is_null() || (*src).cap.is_empty() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::CSpace(cs) = (*cs_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if a[2] > u32::MAX as u64 {
                return Some(ERR_ARG);
            }
            let dst = CSpaceObj::slot(cs, a[2] as u32);
            if dst.is_null() {
                return Some(ERR_BADSLOT);
            }
            if !(*dst).cap.is_empty() {
                return Some(ERR_NOSLOT);
            }
            cspace::slot_move(src, dst);
            Some(0)
        }
        // chan_send(chan_slot, buf, len, cap_list_ptr)
        8 => {
            let s = cur_slot(a[0]);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Channel(ch, end) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            if a[2] > MSG_PAYLOAD as u64 {
                return Some(ERR_ARG);
            }
            if !user_range_ok(a[1], a[2]) {
                return Some(ERR_FAULT);
            }
            let data = core::slice::from_raw_parts(a[1] as *const u8, a[2] as usize);
            let mut caps = [ptr::null_mut(); MSG_CAPS];
            if let Err(e) = resolve_cap_list(a[3], &mut caps, true) {
                return Some(e);
            }
            Some(match channel::send(ch, end, data, &caps) {
                Ok(()) => 0,
                Err(ChanError::Full) => ERR_FULL,
                Err(ChanError::PeerClosed) => ERR_CLOSED,
                Err(_) => ERR_ARG,
            })
        }
        // chan_recv(chan_slot, buf[256], dest_list_ptr) → x0=len, x1=mask
        9 => {
            let s = cur_slot(a[0]);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Channel(ch, end) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::READ) {
                return Some(ERR_PERM);
            }
            if !user_range_ok(a[1], MSG_PAYLOAD as u64) {
                return Some(ERR_FAULT);
            }
            if !user_range_writable(a[1], MSG_PAYLOAD as u64) {
                return Some(ERR_FAULT);
            }
            let mut dests = [ptr::null_mut(); MSG_CAPS];
            if let Err(e) = resolve_cap_list(a[2], &mut dests, false) {
                return Some(e);
            }
            let mut buf = [0u8; MSG_PAYLOAD];
            match channel::recv(ch, end, &mut buf, &dests) {
                Ok((len, mask)) => {
                    core::ptr::copy_nonoverlapping(buf.as_ptr(), a[1] as *mut u8, len);
                    (*frame).x[1] = mask as u64;
                    Some(len as i64)
                }
                Err(ChanError::Empty) => Some(ERR_EMPTY),
                Err(ChanError::NoCapSlot) => Some(ERR_NOSLOT),
                Err(_) => Some(ERR_ARG),
            }
        }
        // chan_bind(chan_slot, event, notif_slot, bits)
        10 => {
            let s = cur_slot(a[0]);
            let ns = cur_slot(a[2]);
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
            if a[1] > 2 {
                return Some(ERR_ARG);
            }
            channel::bind(ch, end, a[1] as usize, n, a[3]);
            Some(0)
        }
        // notif_signal(slot, bits)
        11 => {
            let s = cur_slot(a[0]);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Notification(n) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            notification::signal(n, a[1]);
            Some(0)
        }
        // notif_wait(slot) → accumulated word
        12 => {
            let s = cur_slot(a[0]);
            if s.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Notification(n) = (*s).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*s).cap.rights.has(Rights::READ) {
                return Some(ERR_PERM);
            }
            match notification::wait(n, thread::current()) {
                Some(word) => Some(word as i64),
                None => None, // blocked; signal() writes x0 on wake
            }
        }
        // thread_start(tcb_slot, cspace_slot, entry, sp, prio, arg)
        13 => {
            let ts = cur_slot(a[0]);
            let cs_slot = cur_slot(a[1]);
            if ts.is_null() || cs_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::CSpace(cs) = (*cs_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if (*t).state != ThreadState::Inactive {
                return Some(ERR_STATE);
            }
            if !user_range_ok(a[2], 4) || !user_range_ok(a[3], 0) {
                return Some(ERR_FAULT);
            }
            let prio = a[4] & 0xFF;
            if prio as usize >= NUM_PRIOS {
                return Some(ERR_ARG);
            }
            // Spawner's priority is the ceiling (§5.4 maximum-controlled
            // priority): the lattice stays monotone.
            if prio > (*thread::current()).priority as u64 {
                return Some(ERR_PERM);
            }
            (*cs).hdr.refs += 1;
            (*t).cspace = cs;
            (*t).priority = prio as u8;
            (*t).frame = TrapFrame::zeroed();
            (*t).frame.elr = a[2];
            (*t).frame.sp_el0 = a[3];
            (*t).frame.spsr = 0; // EL0t, interrupts enabled
            (*t).frame.x[0] = a[5];
            thread::enqueue(t);
            Some(0)
        }
        // map(aspace_slot, frame_slot, va, perms) — §2.5: the mapping
        // lives in the frame cap; mapping an already-mapped cap fails
        // (copy the cap to map the frame twice).
        16 => {
            let asp_slot = cur_slot(a[0]);
            let fr_slot = cur_slot(a[1]);
            if asp_slot.is_null() || fr_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Aspace(asp) = (*asp_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*asp_slot).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            let CapKind::Frame { base, pages, mapping } = (*fr_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if mapping.is_some() {
                return Some(ERR_STATE);
            }
            let perms = a[3];
            // Monotone: an RO frame cap cannot become writable memory.
            if perms & crate::aspace::PERM_W != 0 && !(*fr_slot).cap.rights.has(Rights::WRITE)
            {
                return Some(ERR_PERM);
            }
            // Device mappings only via phys-capable caps (§2.5).
            if perms & crate::aspace::PERM_DEVICE != 0
                && !(*fr_slot).cap.rights.has(Rights::PHYS)
            {
                return Some(ERR_PERM);
            }
            match crate::aspace::AspaceObj::map(asp, base, a[2], pages, perms) {
                Ok(()) => {
                    (*asp).hdr.refs += 1;
                    (*fr_slot).cap.kind =
                        CapKind::Frame { base, pages, mapping: Some((asp, a[2])) };
                    Some(0)
                }
                Err(crate::aspace::MapError::NeedMemory) => Some(ERR_NOMEM),
                Err(crate::aspace::MapError::AlreadyMapped) => Some(ERR_STATE),
                Err(_) => Some(ERR_ARG),
            }
        }
        // frame_write(frame_slot, offset, buf, len) — spawn-time program
        // loading: the kernel copies caller bytes into the (unmapped or
        // mapped) frame through the identity map.
        17 => {
            let fr_slot = cur_slot(a[0]);
            if fr_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Frame { base, pages, .. } = (*fr_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if !(*fr_slot).cap.rights.has(Rights::WRITE) {
                return Some(ERR_PERM);
            }
            let (off, len) = (a[1], a[3]);
            if off.checked_add(len).is_none_or(|end| end > pages * 4096) {
                return Some(ERR_ARG);
            }
            if !user_range_ok(a[2], len) {
                return Some(ERR_FAULT);
            }
            core::ptr::copy_nonoverlapping(
                a[2] as *const u8,
                (base + off) as *mut u8,
                len as usize,
            );
            Some(0)
        }
        // thread_start_as(tcb_slot, cspace_slot, aspace_slot, entry, sp, prio)
        18 => {
            let ts = cur_slot(a[0]);
            let cs_slot = cur_slot(a[1]);
            let asp_slot = cur_slot(a[2]);
            if ts.is_null() || cs_slot.is_null() || asp_slot.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::CSpace(cs) = (*cs_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            let CapKind::Aspace(asp) = (*asp_slot).cap.kind else {
                return Some(ERR_TYPE);
            };
            if (*t).state != ThreadState::Inactive {
                return Some(ERR_STATE);
            }
            let prio = a[5] & 0xFF;
            if prio as usize >= NUM_PRIOS {
                return Some(ERR_ARG);
            }
            if prio > (*thread::current()).priority as u64 {
                return Some(ERR_PERM);
            }
            // Entry/SP live in the child's aspace, not the caller's.
            if !crate::aspace::AspaceObj::range_mapped(asp, a[3], 4, false) {
                return Some(ERR_FAULT);
            }
            (*cs).hdr.refs += 1;
            (*asp).hdr.refs += 1;
            (*t).cspace = cs;
            (*t).aspace = asp;
            (*t).priority = prio as u8;
            (*t).frame = TrapFrame::zeroed();
            (*t).frame.elr = a[3];
            (*t).frame.sp_el0 = a[4];
            (*t).frame.spsr = 0;
            thread::enqueue(t);
            Some(0)
        }
        // timer_arm(timer_slot, notif_slot, bits, delta_counter_ticks)
        14 => {
            let ts = cur_slot(a[0]);
            let ns = cur_slot(a[1]);
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
            timer::arm(t, n, a[2], timer::counter().saturating_add(a[3]));
            Some(0)
        }
        // frame_paddr(frame_slot) → PA. Gated on the phys-read bit
        // (§2.5): only the DmaPool holder's caps carry it.
        19 => {
            let s = cur_slot(a[0]);
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
        // debug_getc() → byte, or ERR_EMPTY. Scaffold console input until
        // the userspace UART driver exists (§7).
        20 => Some(match crate::uart::getc() {
            Some(b) => b as i64,
            None => ERR_EMPTY,
        }),
        // thread_bind(tcb_slot, which, notif_slot, bits) — configure the
        // on-exit / on-fault slot (§5.1). The notif cap moves into the
        // TCB's CDT-visible slot; notif_slot = SLOT_NONE unbinds. A child
        // holds no cap to its own threads, so it can neither silence nor
        // forge its own death notice.
        21 => {
            let ts = cur_slot(a[0]);
            if ts.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            // bind-reports gates slot configuration (§2.3): a supervisor
            // holds it; an attenuated observer does not.
            if !(*ts).cap.rights.has(Rights::BIND_REPORTS) {
                return Some(ERR_PERM);
            }
            if a[1] > 1 {
                return Some(ERR_ARG);
            }
            let ns = if a[2] == SLOT_NONE as u64 {
                ptr::null_mut()
            } else {
                let ns = cur_slot(a[2]);
                if ns.is_null() {
                    return Some(ERR_BADSLOT);
                }
                let CapKind::Notification(_) = (*ns).cap.kind else {
                    return Some(ERR_TYPE);
                };
                // The kernel will signal through this cap (§3.6).
                if !(*ns).cap.rights.has(Rights::WRITE) {
                    return Some(ERR_PERM);
                }
                ns
            };
            thread::bind(t, a[1] as usize, ns, a[3]);
            Some(0)
        }
        // read_report(tcb_slot) → x0 = 0 running | 1 exited | 2 faulted,
        // x1 = status / cause, x2 = faulting address (§5.1).
        22 => {
            let ts = cur_slot(a[0]);
            if ts.is_null() {
                return Some(ERR_BADSLOT);
            }
            let CapKind::Thread(t) = (*ts).cap.kind else {
                return Some(ERR_TYPE);
            };
            // read-report gates the read (§2.3).
            if !(*ts).cap.rights.has(Rights::READ_REPORT) {
                return Some(ERR_PERM);
            }
            let (code, v1, v2) = match (*t).report {
                thread::Report::Running => (0, 0, 0),
                thread::Report::Exited(status) => (1, status, 0),
                thread::Report::Faulted { cause, far } => (2, cause, far),
            };
            (*frame).x[1] = v1;
            (*frame).x[2] = v2;
            Some(code)
        }
        // thread_exit(status) — the only voluntary stop (§5.1). The
        // kernel records the status, so a child can neither lie about
        // nor forget its own death; the on-exit binding fires here.
        15 => {
            let t = thread::current();
            (*t).state = ThreadState::Halted;
            thread::report_terminal(t, thread::Report::Exited(a[0]));
            // maybe_switch at exception exit picks someone else; the
            // dead frame is never restored, so there is no x0 to write.
            None
        }
        _ => Some(ERR_ARG),
    }
}
