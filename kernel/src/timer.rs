//! Kernel-side time and timer surface (spec §2.6, §3.6). The armed-timer
//! list *logic* lives in [`kcore::timer`]; this module keeps what is
//! architectural — the generic-timer register access (CNTVCT/CNTV, the
//! 10 ms tick), and the list head itself (`ARMED_HEAD`), which kcore reaches
//! through the [`kcore::env::Env`] seam.

pub use kcore::timer::*;

use crate::env::KernelEnv;
use core::arch::asm;
use core::ptr;
use kcore::notification::NotifObj;

pub const TICK_HZ: u64 = 100;

/// The armed-timer list head. kcore owns the insert/unlink/sweep logic and
/// addresses this anchor through `Env::{timer_armed_head,set_timer_armed_head}`.
static mut ARMED_HEAD: *mut TimerObj = ptr::null_mut();

pub(crate) unsafe fn armed_head() -> *mut TimerObj {
    ARMED_HEAD
}

pub(crate) unsafe fn set_armed_head(head: *mut TimerObj) {
    ARMED_HEAD = head;
}

pub fn counter() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {v}, cntvct_el0", v = out(reg) v) };
    v
}

pub fn freq() -> u64 {
    let v: u64;
    unsafe { asm!("mrs {v}, cntfrq_el0", v = out(reg) v) };
    v
}

/// See [`kcore::timer::arm`].
pub unsafe fn arm(t: *mut TimerObj, notif: *mut NotifObj, bits: u64, deadline: u64) {
    kcore::timer::arm(t, notif, bits, deadline, &mut KernelEnv);
}

/// See [`kcore::timer::check_expired`].
pub unsafe fn check_expired(now: u64) {
    kcore::timer::check_expired(now, &mut KernelEnv);
}

// ── Kernel tick ─────────────────────────────────────────────────────────

fn tick_interval() -> u64 {
    freq() / TICK_HZ
}

pub fn start_tick() {
    unsafe {
        // Let EL0 read the virtual counter (time page basis, §2.6).
        asm!("msr cntkctl_el1, {v}", v = in(reg) 0b10u64); // EL0VCTEN
        rearm_tick();
        // CNTV_CTL: ENABLE=1, IMASK=0.
        asm!("msr cntv_ctl_el0, {v}", v = in(reg) 1u64);
    }
}

pub fn rearm_tick() {
    unsafe {
        asm!("msr cntv_tval_el0, {v}", v = in(reg) tick_interval());
    }
}
