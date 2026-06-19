//! Raw syscall wrappers for Eunomia userspace (aarch64-none only).
//!
//! ABI (scaffold, not stable — rev0§3.7): SVC #0, number in x7, args
//! x0..x5, result in x0 (negative = error), secondary result in x1.

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

pub const OBJ_CSPACE: u64 = 0;
pub const OBJ_THREAD: u64 = 1;
pub const OBJ_CHANNEL: u64 = 2;
pub const OBJ_NOTIF: u64 = 3;
pub const OBJ_TIMER: u64 = 4;
pub const OBJ_FRAME: u64 = 5;
pub const OBJ_ASPACE: u64 = 6;
/// A carved sub-range untyped (rev0§2.3); retype param is bytes. The per-spawn
/// donation a parent funds for one child (rev0§5.1).
pub const OBJ_UNTYPED: u64 = 7;

pub const RIGHT_READ: u64 = 1;
pub const RIGHT_WRITE: u64 = 2;
pub const RIGHTS_ALL: u64 = 3;

pub const PERM_W: u64 = 1;
pub const PERM_X: u64 = 2;
pub const PERM_DEVICE: u64 = 4;

pub const RIGHT_PHYS: u64 = 4;
/// Thread rights (rev0§2.3): configure on-exit/on-fault binding slots.
pub const RIGHT_BIND_REPORTS: u64 = 8;
/// Thread rights (rev0§2.3): read the terminal report record.
pub const RIGHT_READ_REPORT: u64 = 16;

pub const EV_READABLE: u64 = 0;
pub const EV_WRITABLE: u64 = 1;
pub const EV_PEER_CLOSED: u64 = 2;

/// TCB binding slots (rev0§5.1).
pub const BIND_EXIT: u64 = 0;
pub const BIND_FAULT: u64 = 1;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod imp {
    #[inline(always)]
    pub unsafe fn syscall(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
        let ret: u64;
        core::arch::asm!(
            "svc #0",
            inout("x0") a0 => ret,
            inout("x1") a1 => _,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
            in("x7") nr,
            options(nostack),
        );
        ret as i64
    }

    #[inline(always)]
    pub unsafe fn syscall2(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> (i64, u64) {
        let ret: u64;
        let ret2: u64;
        core::arch::asm!(
            "svc #0",
            inout("x0") a0 => ret,
            inout("x1") a1 => ret2,
            in("x2") a2,
            in("x3") a3,
            in("x7") nr,
            options(nostack),
        );
        (ret as i64, ret2)
    }

    #[inline(always)]
    pub unsafe fn syscall3(nr: u64, a0: u64) -> (i64, u64, u64) {
        let ret: u64;
        let ret2: u64;
        let ret3: u64;
        core::arch::asm!(
            "svc #0",
            inout("x0") a0 => ret,
            out("x1") ret2,
            out("x2") ret3,
            in("x7") nr,
            options(nostack),
        );
        (ret as i64, ret2, ret3)
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
mod imp {
    /// Host builds (tests of the protocol layers) must never reach a raw
    /// syscall.
    pub unsafe fn syscall(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> i64 {
        unreachable!("Eunomia syscall on a non-Eunomia target")
    }

    pub unsafe fn syscall2(_: u64, _: u64, _: u64, _: u64, _: u64) -> (i64, u64) {
        unreachable!("Eunomia syscall on a non-Eunomia target")
    }

    pub unsafe fn syscall3(_: u64, _: u64) -> (i64, u64, u64) {
        unreachable!("Eunomia syscall on a non-Eunomia target")
    }
}

use imp::{syscall, syscall2, syscall3};

pub fn debug_putc(c: u8) {
    unsafe { syscall(0, c as u64, 0, 0, 0, 0, 0) };
}

pub fn debug_write(msg: &[u8]) {
    unsafe { syscall(1, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0) };
}

pub fn yield_now() {
    unsafe { syscall(2, 0, 0, 0, 0, 0, 0) };
}

pub fn retype(ut: u32, ty: u64, param: u64, dst: u32, dst2: u32) -> i64 {
    unsafe { syscall(3, ut as u64, ty, param, dst as u64, dst2 as u64, 0) }
}

pub fn cap_copy(src: u32, dst: u32, rights: u64) -> i64 {
    // a[3] = 0xFF is the rev0§5.4 "no ceiling reduction" sentinel (kcore `NO_PRIO_CEILING`);
    // a plain copy never lowers a thread cap's priority ceiling. Use `cap_copy_prio`
    // to attenuate it (the rev0§2.3 supervision grant).
    unsafe { syscall(4, src as u64, dst as u64, rights, 0xFF, 0, 0) }
}

/// Like [`cap_copy`], but caps the copied thread cap's rev0§5.4 priority ceiling at
/// `min(parent, prio_ceiling)` (rev0§2.3 supervision grant; ignored for non-thread caps).
pub fn cap_copy_prio(src: u32, dst: u32, rights: u64, prio_ceiling: u8) -> i64 {
    unsafe { syscall(4, src as u64, dst as u64, rights, prio_ceiling as u64, 0, 0) }
}

pub fn cap_delete(slot: u32) -> i64 {
    unsafe { syscall(5, slot as u64, 0, 0, 0, 0, 0) }
}

pub fn cap_revoke(slot: u32) -> i64 {
    unsafe { syscall(6, slot as u64, 0, 0, 0, 0, 0) }
}

pub fn cap_install(cspace: u32, src: u32, dst_index: u32) -> i64 {
    unsafe { syscall(7, cspace as u64, src as u64, dst_index as u64, 0, 0, 0) }
}

pub fn chan_send(chan: u32, data: &[u8], caps: Option<&[u32; 4]>) -> i64 {
    let cp = caps.map(|c| c.as_ptr() as u64).unwrap_or(0);
    unsafe { syscall(8, chan as u64, data.as_ptr() as u64, data.len() as u64, cp, 0, 0) }
}

/// Returns (len, cap-present mask). `buf` must hold 256 bytes.
pub fn chan_recv(chan: u32, buf: *mut u8, dests: Option<&[u32; 4]>) -> (i64, u64) {
    let dp = dests.map(|d| d.as_ptr() as u64).unwrap_or(0);
    unsafe { syscall2(9, chan as u64, buf as u64, dp, 0) }
}

pub fn chan_bind(chan: u32, event: u64, notif: u32, bits: u64) -> i64 {
    unsafe { syscall(10, chan as u64, event, notif as u64, bits, 0, 0) }
}

pub fn notif_signal(slot: u32, bits: u64) -> i64 {
    unsafe { syscall(11, slot as u64, bits, 0, 0, 0, 0) }
}

pub fn notif_wait(slot: u32) -> i64 {
    unsafe { syscall(12, slot as u64, 0, 0, 0, 0, 0) }
}

pub fn timer_arm(timer: u32, notif: u32, bits: u64, delta: u64) -> i64 {
    unsafe { syscall(14, timer as u64, notif as u64, bits, delta, 0, 0) }
}

/// The only voluntary stop (rev0§5.1): the kernel records the status — a
/// child can neither lie about nor forget its own death.
pub fn thread_exit(status: u64) -> ! {
    unsafe {
        syscall(15, status, 0, 0, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

pub fn exit() -> ! {
    thread_exit(0)
}

pub fn map(aspace: u32, frame: u32, va: u64, perms: u64) -> i64 {
    unsafe { syscall(16, aspace as u64, frame as u64, va, perms, 0, 0) }
}

pub fn frame_write(frame: u32, offset: u64, data: &[u8]) -> i64 {
    unsafe {
        syscall(17, frame as u64, offset, data.as_ptr() as u64, data.len() as u64, 0, 0)
    }
}

pub fn thread_start_as(tcb: u32, cspace: u32, aspace: u32, entry: u64, sp: u64, prio: u64) -> i64 {
    unsafe {
        syscall(18, tcb as u64, cspace as u64, aspace as u64, entry, sp, prio)
    }
}

/// Physical address of a frame — phys-read right required (rev0§2.5); the
/// DmaPool is the only legitimate caller.
pub fn frame_paddr(frame: u32) -> i64 {
    unsafe { syscall(19, frame as u64, 0, 0, 0, 0, 0) }
}

/// Reset a carved untyped's watermark to 0 so its range can be reused
/// (rev0§2.5). Pairs with `cap_revoke`: revoke deletes every object carved
/// from the untyped, then reset reclaims the bytes. Fails with ERR_STATE
/// if the untyped still has live children (revoke first).
pub fn untyped_reset(ut: u32) -> i64 {
    unsafe { syscall(23, ut as u64, 0, 0, 0, 0, 0) }
}

/// Non-blocking console byte (scaffold until the userspace UART driver).
pub fn debug_getc() -> i64 {
    unsafe { syscall(20, 0, 0, 0, 0, 0, 0) }
}

/// Configure a thread's on-exit / on-fault binding slot (rev0§5.1). The
/// notification cap MOVES into the TCB (duplicate first to keep access);
/// `notif` = SLOT_NONE unbinds.
pub fn thread_bind(tcb: u32, which: u64, notif: u32, bits: u64) -> i64 {
    unsafe { syscall(21, tcb as u64, which, notif as u64, bits, 0, 0) }
}

/// Terminal report states returned by `read_report` (rev0§5.1).
pub const REPORT_RUNNING: i64 = 0;
pub const REPORT_EXITED: i64 = 1;
pub const REPORT_FAULTED: i64 = 2;

/// Reserved terminal exit status: the process stopped through its panic
/// handler rather than an orderly `thread_exit(status)` (urt convention,
/// rev0§5.1). A faulting thread is `REPORT_FAULTED`, but a Rust `panic!`
/// unwinds to the handler and exits normally — without a reserved status
/// it would be indistinguishable from `thread_exit(0)`, and a child could
/// disguise a crash as clean success. The kernel records whatever status
/// the child passes, so this only works because every urt panic handler
/// agrees to pass *this* value and no well-behaved child returns it
/// deliberately (it sits at the top of the u64 range, out of the small
/// statuses real programs use).
pub const STATUS_PANIC: u64 = u64::MAX;

/// Read a thread's terminal report record (rev0§5.1). Returns
/// (state, status-or-cause, faulting-address); negative state = error.
pub fn read_report(tcb: u32) -> (i64, u64, u64) {
    unsafe { syscall3(22, tcb as u64) }
}
