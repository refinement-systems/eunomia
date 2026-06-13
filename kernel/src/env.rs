//! `KernelEnv`: the concrete [`kcore::env::Env`] over the real kernel
//! statics. Zero-sized, so the object machinery monomorphizes against it
//! with no indirection. Every method delegates to the architectural half
//! that stays in this crate — the scheduler ready queues, the page-table
//! walker, and the armed-timer list head.

use kcore::aspace::AspaceObj;
use kcore::env::Env;
use kcore::thread::Tcb;
use kcore::timer::TimerObj;

pub struct KernelEnv;

impl Env for KernelEnv {
    unsafe fn make_runnable(&mut self, t: *mut Tcb) {
        crate::thread::enqueue(t);
    }

    unsafe fn unqueue_ready(&mut self, t: *mut Tcb) {
        crate::thread::unqueue_ready(t);
    }

    unsafe fn aspace_unmap(&mut self, asp: *mut AspaceObj, va: u64, pages: u64) {
        crate::aspace::unmap(asp, va, pages);
    }

    unsafe fn aspace_destroy(&mut self, asp: *mut AspaceObj) {
        crate::aspace::destroy_aspace(asp);
    }

    // The page-table walker (kcore::aspace) is architecture-independent; these
    // are the real AArch64 TLB/barrier sequences it drives. They are exactly
    // the asm the old kernel `map`/`unmap` ran inline (plan §2.4).
    unsafe fn tlb_invalidate_page(&mut self, asid: u16, va: u64) {
        // TLBI VAE1: [63:48] ASID, [43:0] VA[55:12].
        let arg = ((asid as u64) << 48) | ((va >> 12) & 0xFFF_FFFF_FFFF);
        core::arch::asm!("tlbi vae1, {v}", v = in(reg) arg);
    }

    unsafe fn barrier_after_map(&mut self) {
        core::arch::asm!("dsb ishst");
    }

    unsafe fn barrier_after_unmap(&mut self) {
        core::arch::asm!("dsb ish", "isb");
    }

    unsafe fn timer_armed_head(&mut self) -> *mut TimerObj {
        crate::timer::armed_head()
    }

    unsafe fn set_timer_armed_head(&mut self, head: *mut TimerObj) {
        crate::timer::set_armed_head(head);
    }
}
