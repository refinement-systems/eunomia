//! The trusted userspace syscall shell (rev2§6.1(d)).
//!
//! The typed wrappers over the raw `svc #0` shim. The wrappers are the surface the PAL
//! calls: each builds a [`Call`](crate::encode::Call), runs it through the **verified**
//! [`encode`](crate::encode::encode) (which places the arguments and refuses any
//! out-of-range field), and on success issues the `svc`; an `encode` refusal maps to
//! [`ERR_ARG`] without ever reaching the kernel. So the only trusted logic here is the
//! `Call` construction — the argument *placement* is the verified encoder's, and the
//! raw register marshalling is `ipc::sys::imp` (a target-only dep), reused rather than
//! copied so the trusted `svc` asm has a single home.

use crate::encode::{encode, Call};

// ---------------------------------------------------------------------------
// ABI constants (rev2§3.7), the userspace surface this seam owns. An independent twin
// of the kernel block (`kcore`/`kernel/src/syscall.rs`), like `ipc::sys` — kept in
// lockstep by review; the encoder's bound constants live in `crate::encode` and are
// pinned against kcore by its host test.
// ---------------------------------------------------------------------------

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
/// Retry-later: `cap_revoke` returns this when a bounded quantum ends with descendants
/// remaining, and `cap_copy` when the source is under an in-flight revoke. Kept in
/// lockstep with the kernel block.
pub const ERR_AGAIN: i64 = -12;

pub const SLOT_NONE: u32 = u32::MAX;

pub const OBJ_CSPACE: u64 = 0;
pub const OBJ_THREAD: u64 = 1;
pub const OBJ_CHANNEL: u64 = 2;
pub const OBJ_NOTIF: u64 = 3;
pub const OBJ_TIMER: u64 = 4;
pub const OBJ_FRAME: u64 = 5;
pub const OBJ_ASPACE: u64 = 6;
/// A carved sub-range untyped (rev2§2.3); retype param is bytes.
pub const OBJ_UNTYPED: u64 = 7;

pub const RIGHT_READ: u64 = 1;
pub const RIGHT_WRITE: u64 = 2;
pub const RIGHTS_ALL: u64 = 3;

pub const PERM_W: u64 = 1;
pub const PERM_X: u64 = 2;
pub const PERM_DEVICE: u64 = 4;

pub const RIGHT_PHYS: u64 = 4;
/// Thread rights (rev2§2.3): configure on-exit/on-fault binding slots.
pub const RIGHT_BIND_REPORTS: u64 = 8;
/// Thread rights (rev2§2.3): read the terminal report record.
pub const RIGHT_READ_REPORT: u64 = 16;

pub const EV_READABLE: u64 = 0;
pub const EV_WRITABLE: u64 = 1;
pub const EV_PEER_CLOSED: u64 = 2;

/// TCB binding slots (rev2§5.1).
pub const BIND_EXIT: u64 = 0;
pub const BIND_FAULT: u64 = 1;

/// `cap_copy`'s "no priority-ceiling reduction" sentinel (rev2§5.4): a thread-cap copy
/// passing this leaves the parent's ceiling unchanged (kcore `NO_PRIO_CEILING`).
pub const NO_PRIO_CEILING: u64 = 0xFF;

/// Terminal report states returned by `read_report` (rev2§5.1).
pub const REPORT_RUNNING: i64 = 0;
pub const REPORT_EXITED: i64 = 1;
pub const REPORT_FAULTED: i64 = 2;

/// Reserved terminal exit status: the process stopped through its panic handler rather
/// than an orderly `thread_exit(status)` (rev2§5.1). Sits at the top of the `u64` range
/// so no well-behaved child returns it deliberately.
pub const STATUS_PANIC: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Trusted inline-asm shell (rev2§6.1(d)). SVC #0, number in x7, args x0..x6, result in
// x0 (negative = error), secondary results in x1/x2. The userspace mirror of the
// kernel-side trusted register marshalling — inherently unverifiable.
//
// The asm itself is *not* redefined here: it lives once, in `ipc::sys::imp` (a
// target-only dep of this crate), and the target arm re-uses it — the eight-arg
// `syscall7` matches this seam's always-pass-x6 form, so it is aliased to `syscall`.
// This crate's only trusted addition is the `Call` construction; the argument
// *placement* is the verified `encode`'s. A non-Eunomia (host) build has no `ipc`
// edge, so it keeps a local `unreachable!` stub for the protocol/encode host tests.
// ---------------------------------------------------------------------------

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "none", target_os = "eunomia")
))]
use ipc::sys::imp::{syscall2, syscall3, syscall7 as syscall};

#[cfg(not(all(
    target_arch = "aarch64",
    any(target_os = "none", target_os = "eunomia")
)))]
mod imp {
    /// Host builds (tests of the protocol/encode layers) must never reach a raw
    /// syscall.
    pub unsafe fn syscall(_: u64, _: u64, _: u64, _: u64, _: u64, _: u64, _: u64, _: u64) -> i64 {
        unreachable!("Eunomia syscall on a non-Eunomia target")
    }

    pub unsafe fn syscall2(_: u64, _: u64, _: u64, _: u64, _: u64) -> (i64, u64) {
        unreachable!("Eunomia syscall on a non-Eunomia target")
    }

    pub unsafe fn syscall3(_: u64, _: u64) -> (i64, u64, u64) {
        unreachable!("Eunomia syscall on a non-Eunomia target")
    }
}

#[cfg(not(all(
    target_arch = "aarch64",
    any(target_os = "none", target_os = "eunomia")
)))]
use imp::{syscall, syscall2, syscall3};

// ---------------------------------------------------------------------------
// Typed wrappers — the thin trusted shell the PAL calls.
// ---------------------------------------------------------------------------

/// Encode `c` and issue a single-result syscall; an `encode` refusal is `ERR_ARG`.
fn dispatch(c: Call) -> i64 {
    match encode(c) {
        Ok(e) => unsafe { syscall(e.nr, e.a0, e.a1, e.a2, e.a3, e.a4, e.a5, e.a6) },
        Err(_) => ERR_ARG,
    }
}

/// Encode `c` and issue a two-result syscall (`chan_recv`).
fn dispatch2(c: Call) -> (i64, u64) {
    match encode(c) {
        Ok(e) => unsafe { syscall2(e.nr, e.a0, e.a1, e.a2, e.a3) },
        Err(_) => (ERR_ARG, 0),
    }
}

/// Encode `c` and issue a three-result syscall (`read_report`).
fn dispatch3(c: Call) -> (i64, u64, u64) {
    match encode(c) {
        Ok(e) => unsafe { syscall3(e.nr, e.a0) },
        Err(_) => (ERR_ARG, 0, 0),
    }
}

/// EL0 debug-print scaffold (rev2§7): a kernel-diagnostic path for pre-console
/// diagnostics and panic reporting — never user-facing terminal I/O.
pub fn debug_putc(c: u8) {
    dispatch(Call::DebugPutc { ch: c as u64 });
}

pub fn debug_write(msg: &[u8]) {
    dispatch(Call::DebugWrite {
        ptr: msg.as_ptr() as u64,
        len: msg.len() as u64,
    });
}

pub fn yield_now() {
    dispatch(Call::Yield);
}

pub fn retype(ut: u32, ty: u64, param: u64, dst: u32, dst2: u32) -> i64 {
    dispatch(Call::Retype {
        ut: ut as u64,
        ty,
        param,
        dst: dst as u64,
        dst2: dst2 as u64,
    })
}

/// A plain cap copy: `0xFF` is the rev2§5.4 "no ceiling reduction" sentinel.
pub fn cap_copy(src: u32, dst: u32, rights: u64) -> i64 {
    dispatch(Call::CapCopy {
        src: src as u64,
        dst: dst as u64,
        mask: rights,
        prio_ceiling: NO_PRIO_CEILING,
    })
}

/// Like [`cap_copy`], but caps the copied thread cap's rev2§5.4 priority ceiling at
/// `min(parent, prio_ceiling)` (ignored for non-thread caps).
pub fn cap_copy_prio(src: u32, dst: u32, rights: u64, prio_ceiling: u8) -> i64 {
    dispatch(Call::CapCopy {
        src: src as u64,
        dst: dst as u64,
        mask: rights,
        prio_ceiling: prio_ceiling as u64,
    })
}

pub fn cap_delete(slot: u32) -> i64 {
    dispatch(Call::CapDelete { slot: slot as u64 })
}

pub fn cap_revoke(slot: u32) -> i64 {
    dispatch(Call::CapRevoke { slot: slot as u64 })
}

/// Fully revoke `slot`'s subtree (rev2§2.2): `cap_revoke` runs one bounded quantum per
/// call and returns [`ERR_AGAIN`] while descendants remain; loop, yielding between
/// tries, until it returns a terminal status.
pub fn cap_revoke_all(slot: u32) -> i64 {
    loop {
        let r = cap_revoke(slot);
        if r != ERR_AGAIN {
            return r;
        }
        yield_now();
    }
}

pub fn cap_install(cspace: u32, src: u32, dst_index: u32) -> i64 {
    dispatch(Call::CapInstall {
        cs: cspace as u64,
        src: src as u64,
        dst_index: dst_index as u64,
    })
}

pub fn chan_send(chan: u32, data: &[u8], caps: Option<&[u32; 4]>) -> i64 {
    let cp = caps.map(|c| c.as_ptr() as u64).unwrap_or(0);
    dispatch(Call::ChanSend {
        chan: chan as u64,
        buf: data.as_ptr() as u64,
        len: data.len() as u64,
        caps: cp,
    })
}

/// Returns (len, cap-present mask). `buf` must hold 256 bytes.
pub fn chan_recv(chan: u32, buf: *mut u8, dests: Option<&[u32; 4]>) -> (i64, u64) {
    let dp = dests.map(|d| d.as_ptr() as u64).unwrap_or(0);
    dispatch2(Call::ChanRecv {
        chan: chan as u64,
        buf: buf as u64,
        dests: dp,
    })
}

pub fn chan_bind(chan: u32, event: u64, notif: u32, bits: u64) -> i64 {
    dispatch(Call::ChanBind {
        chan: chan as u64,
        event,
        notif: notif as u64,
        bits,
    })
}

pub fn notif_signal(slot: u32, bits: u64) -> i64 {
    dispatch(Call::NotifSignal {
        slot: slot as u64,
        bits,
    })
}

pub fn notif_wait(slot: u32) -> i64 {
    dispatch(Call::NotifWait { slot: slot as u64 })
}

pub fn timer_arm(timer: u32, notif: u32, bits: u64, delta: u64) -> i64 {
    dispatch(Call::TimerArm {
        timer: timer as u64,
        notif: notif as u64,
        bits,
        delta,
    })
}

/// Bind an IRQ-handler cap to a notification (rev2§1/rev2§3.6): a hardware interrupt on
/// the line signals `notif` with `bits`. `notif` must carry WRITE.
pub fn irq_bind(irq: u32, notif: u32, bits: u64) -> i64 {
    dispatch(Call::IrqBind {
        irq: irq as u64,
        notif: notif as u64,
        bits,
    })
}

/// Acknowledge an IRQ: re-enable the line so the next interrupt is delivered.
pub fn irq_ack(irq: u32) -> i64 {
    dispatch(Call::IrqAck { irq: irq as u64 })
}

/// The only voluntary stop (rev2§5.1): the kernel records the status — a child can
/// neither lie about nor forget its own death.
pub fn thread_exit(status: u64) -> ! {
    dispatch(Call::ThreadExit { status });
    loop {
        core::hint::spin_loop();
    }
}

pub fn exit() -> ! {
    thread_exit(0)
}

pub fn map(aspace: u32, frame: u32, va: u64, perms: u64) -> i64 {
    dispatch(Call::Map {
        aspace: aspace as u64,
        frame: frame as u64,
        va,
        perms,
    })
}

/// Grow `aspace`'s page-table pool by `pages` tables, carved from `ut` (rev2§2.5
/// "accepts top-ups"). `ut` must abut the pool's current end.
pub fn aspace_topup(aspace: u32, ut: u32, pages: u64) -> i64 {
    dispatch(Call::AspaceTopUp {
        aspace: aspace as u64,
        ut: ut as u64,
        pages,
    })
}

/// Map `frame` at `va`, topping up `aspace`'s pool from `ut` and retrying once if `map`
/// fails for lack of pool memory — the recoverable `NEED_MEMORY` story.
pub fn map_grow(aspace: u32, ut: u32, frame: u32, va: u64, perms: u64, step: u64) -> i64 {
    let r = map(aspace, frame, va, perms);
    if r != ERR_NOMEM {
        return r;
    }
    let t = aspace_topup(aspace, ut, step);
    if t != 0 {
        return t;
    }
    map(aspace, frame, va, perms)
}

pub fn frame_write(frame: u32, offset: u64, data: &[u8]) -> i64 {
    dispatch(Call::FrameWrite {
        frame: frame as u64,
        off: offset,
        buf: data.as_ptr() as u64,
        len: data.len() as u64,
    })
}

pub fn thread_start(tcb: u32, cspace: u32, entry: u64, sp: u64, prio: u64, arg: u64) -> i64 {
    dispatch(Call::ThreadStart {
        tcb: tcb as u64,
        cspace: cspace as u64,
        entry,
        sp,
        prio,
        arg,
    })
}

pub fn thread_start_as(
    tcb: u32,
    cspace: u32,
    aspace: u32,
    entry: u64,
    sp: u64,
    prio: u64,
    arg: u64,
) -> i64 {
    dispatch(Call::ThreadStartAs {
        tcb: tcb as u64,
        cspace: cspace as u64,
        aspace: aspace as u64,
        entry,
        sp,
        prio,
        arg,
    })
}

/// Physical address of a frame — phys-read right required (rev2§2.5); the DmaPool is the
/// only legitimate caller.
pub fn frame_paddr(frame: u32) -> i64 {
    dispatch(Call::FramePaddr { slot: frame as u64 })
}

/// Reset a carved untyped's watermark to 0 so its range can be reused (rev2§2.5).
pub fn untyped_reset(ut: u32) -> i64 {
    dispatch(Call::UntypedReset { slot: ut as u64 })
}

/// Configure a thread's on-exit / on-fault binding slot (rev2§5.1). The notification cap
/// MOVES into the TCB; `notif = SLOT_NONE` unbinds.
pub fn thread_bind(tcb: u32, which: u64, notif: u32, bits: u64) -> i64 {
    dispatch(Call::ThreadBind {
        tcb: tcb as u64,
        which,
        notif: notif as u64,
        bits,
    })
}

/// Read a thread's terminal report record (rev2§5.1). Returns (state, status-or-cause,
/// faulting-address); negative state = error.
pub fn read_report(tcb: u32) -> (i64, u64, u64) {
    dispatch3(Call::ReadReport { tcb: tcb as u64 })
}
