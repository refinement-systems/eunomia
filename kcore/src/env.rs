//! The environment seam (plan §2.2 rules 3–4): the kernel-shell services
//! the object machinery needs but cannot itself contain without dragging in
//! inline asm, MMIO, and scheduler statics that CBMC can't model.
//!
//! Threaded as a generic `env: &mut E` parameter through exactly the
//! functions that need it (refcount teardown, event firing, frame unmap,
//! timer-list edits). The kernel monomorphizes [`Env`] to a zero-sized
//! `KernelEnv` over its existing statics — no indirection on the hot
//! `signal → make_runnable` path. The proof harnesses use a `GhostEnv` that
//! records every call into harness-owned state (no statics, so host tests
//! are parallel-safe by construction), letting a harness assert *which*
//! effects an operation produced and *in what order* — e.g. that a
//! channel teardown fires its peer-closed binding before reclamation.
//!
//! Phase note (plan §5): the aspace TLB/barrier hooks land here at phase 5
//! (the walker rewrite); this trait is the place they go, either as more
//! methods or by refactoring into `Env: Sched + Hal` supertraits — both
//! call-site-compatible with what is here today.

use crate::aspace::AspaceObj;
#[cfg(kani)]
use crate::channel::Channel;
#[cfg(kani)]
use crate::cspace::CSpaceObj;
use crate::thread::Tcb;
use crate::timer::TimerObj;

pub trait Env {
    /// Wake a thread (notification delivery, §3.6). The kernel appends it to
    /// the tail of its priority ready queue (`enqueue`); the ghost impl sets
    /// the thread Runnable — mirroring `enqueue`'s contract — and logs.
    ///
    /// pre:  `t` is detached from any wait queue and its register state is
    ///       already consistent for resumption.
    /// post: `t` is Runnable and schedulable.
    unsafe fn make_runnable(&mut self, t: *mut Tcb);

    /// Remove a Runnable thread from the ready structure (thread teardown).
    ///
    /// pre:  `t.state == Runnable` and `t` is on the ready structure.
    /// post: `t` is off it.
    unsafe fn unqueue_ready(&mut self, t: *mut Tcb);

    /// Unmap `pages` frames at `va` from `asp`, TLB maintenance included
    /// (frame-cap deletion, §2.5). The page-table walker stays kernel-side
    /// until the phase-5 rewrite; the ghost impl logs `(asp, va, pages)`.
    unsafe fn aspace_unmap(&mut self, asp: *mut AspaceObj, va: u64, pages: u64);

    /// Last-reference teardown of an address space. A no-op in the kernel
    /// today (the tables return to the donor untyped via revoke); the
    /// ghost impl logs it. The phase-5 seam.
    unsafe fn aspace_destroy(&mut self, asp: *mut AspaceObj);

    /// The TLB/barrier hooks the slice-indexed page-table walker
    /// ([`crate::aspace`]) calls (the §2.2 rule-3 `Hal` seam, landed as Env
    /// methods per the phase note above). The kernel implements them with the
    /// real `tlbi`/`dsb`/`isb`; the ghost impl records them so a harness can
    /// assert exactly which pages were invalidated.
    ///
    /// Invalidate the TLB entry for `va` in address space `asid`
    /// (per cleared page in `aspace::unmap_in`).
    unsafe fn tlb_invalidate_page(&mut self, asid: u16, va: u64);
    /// Ordering barrier after installing new (previously-invalid) mappings —
    /// no TLB shootdown needed, just a store barrier (`dsb ishst`).
    unsafe fn barrier_after_map(&mut self);
    /// Completion barrier after a batch of unmaps + their TLBIs
    /// (`dsb ish; isb`).
    unsafe fn barrier_after_unmap(&mut self);

    /// Head of the armed-timer list. The list *logic* (insert/unlink/expiry
    /// sweep) lives in [`crate::timer`]; only the anchor is environment
    /// state — a kernel static (`ARMED_HEAD`), a ghost field — because the
    /// kernel has no global pool and the proofs own their state explicitly.
    unsafe fn timer_armed_head(&mut self) -> *mut TimerObj;
    unsafe fn set_timer_armed_head(&mut self, head: *mut TimerObj);

    /// Proof-only DN-4 routing witnesses. The bounded teardown harnesses stub
    /// the recursive container destructors (`destroy_cspace`/`destroy_channel`/
    /// `destroy_tcb`) to no-ops so a top-level `delete` stays tractable under
    /// CBMC; these hooks let the no-op stub record *which* destructor arm
    /// `obj_unref` dispatched to, so `check_delete_cspace`/`_channel`/`_tcb`
    /// assert the routing instead of trusting source inspection (review-2
    /// rec. 3, `doc/results/14_kani-review-2.md` / `15_kani-findings-12.md`).
    /// The kernel `Env` and the real destructors never call them; default
    /// no-ops, `GhostEnv` overrides to log. `#[cfg(kani)]` so they never reach
    /// the production trait surface.
    #[cfg(kani)]
    unsafe fn ghost_destroy_cspace(&mut self, _cs: *mut CSpaceObj) {}
    #[cfg(kani)]
    unsafe fn ghost_destroy_channel(&mut self, _ch: *mut Channel) {}
    #[cfg(kani)]
    unsafe fn ghost_destroy_tcb(&mut self, _t: *mut Tcb) {}
}
