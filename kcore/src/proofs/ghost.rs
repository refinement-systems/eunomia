//! `GhostEnv`: the [`Env`] implementation the proofs use in place of the
//! kernel's scheduler/walker statics. It records every environment call into
//! a fixed-size log owned by the harness — no statics, so host tests are
//! parallel-safe — letting a harness assert *which* effects an operation
//! produced and *in what order*. The one unified log (rather than a log per
//! hook) is what makes cross-hook ordering checkable, e.g. that a channel
//! teardown fires its peer-closed binding (a `MakeRunnable`) before the
//! reclamation unmaps a queued frame (an `AspaceUnmap`) — the observable
//! form of the TSpec fire-before-reclaim obligation (plan §4.1, DN-2).

use crate::aspace::AspaceObj;
use crate::channel::Channel;
use crate::cspace::CSpaceObj;
use crate::env::Env;
use crate::thread::{Tcb, ThreadState};
use crate::timer::TimerObj;
use core::ptr;

/// Max environment events recorded per harness run. Larger than any single
/// operation's effect set at the §3 bounds; overflow is a harness bug
/// (raise this), asserted rather than silently dropped.
pub const MAX_EVENTS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GhostEvent {
    MakeRunnable(*mut Tcb),
    UnqueueReady(*mut Tcb),
    AspaceUnmap(*mut AspaceObj, u64, u64),
    AspaceDestroy(*mut AspaceObj),
    /// (asid, va) of a page-table TLB invalidation (`aspace::unmap_in`).
    TlbInvalidate(u16, u64),
    BarrierMap,
    BarrierUnmap,
    /// DN-4 routing witnesses (review-2 rec. 3): the recursive container
    /// destructor `obj_unref` dispatched to, recorded by the no-op stub that
    /// replaces it so a `check_delete_*` harness asserts the dispatch reached
    /// the right arm (not just that the refcount hit zero).
    DestroyCspace(*mut CSpaceObj),
    DestroyChannel(*mut Channel),
    DestroyTcb(*mut Tcb),
}

pub struct GhostEnv {
    pub log: [Option<GhostEvent>; MAX_EVENTS],
    pub len: usize,
    /// The armed-timer list head (the kernel's `ARMED_HEAD`, owned here as a
    /// plain field so the timer-list logic in [`crate::timer`] runs unchanged
    /// against ghost state).
    pub armed_head: *mut TimerObj,
}

impl GhostEnv {
    pub fn new() -> GhostEnv {
        GhostEnv { log: [None; MAX_EVENTS], len: 0, armed_head: ptr::null_mut() }
    }

    fn push(&mut self, e: GhostEvent) {
        assert!(self.len < MAX_EVENTS, "ghost event log overflow (raise MAX_EVENTS)");
        self.log[self.len] = Some(e);
        self.len += 1;
    }

    /// The recorded events, in order.
    pub fn events(&self) -> &[Option<GhostEvent>] {
        &self.log[..self.len]
    }

    /// Count of a specific event in the log.
    pub fn count(&self, e: GhostEvent) -> usize {
        self.log[..self.len].iter().filter(|&&x| x == Some(e)).count()
    }

    /// Whether event `a` was recorded strictly before event `b` (both must
    /// be present). The observable witness for ordering obligations.
    pub fn ordered_before(&self, a: GhostEvent, b: GhostEvent) -> bool {
        let mut ia = None;
        let mut ib = None;
        for (i, ev) in self.log[..self.len].iter().enumerate() {
            if *ev == Some(a) && ia.is_none() {
                ia = Some(i);
            }
            if *ev == Some(b) && ib.is_none() {
                ib = Some(i);
            }
        }
        matches!((ia, ib), (Some(x), Some(y)) if x < y)
    }
}

impl Default for GhostEnv {
    fn default() -> GhostEnv {
        GhostEnv::new()
    }
}

impl Env for GhostEnv {
    unsafe fn make_runnable(&mut self, t: *mut Tcb) {
        // Mirror `thread::enqueue`'s contract (it sets Runnable) so the
        // post-state a harness inspects matches the kernel's.
        (*t).state = ThreadState::Runnable;
        self.push(GhostEvent::MakeRunnable(t));
    }

    unsafe fn unqueue_ready(&mut self, t: *mut Tcb) {
        self.push(GhostEvent::UnqueueReady(t));
    }

    unsafe fn aspace_unmap(&mut self, asp: *mut AspaceObj, va: u64, pages: u64) {
        self.push(GhostEvent::AspaceUnmap(asp, va, pages));
    }

    unsafe fn aspace_destroy(&mut self, asp: *mut AspaceObj) {
        self.push(GhostEvent::AspaceDestroy(asp));
    }

    unsafe fn tlb_invalidate_page(&mut self, asid: u16, va: u64) {
        self.push(GhostEvent::TlbInvalidate(asid, va));
    }

    unsafe fn barrier_after_map(&mut self) {
        self.push(GhostEvent::BarrierMap);
    }

    unsafe fn barrier_after_unmap(&mut self) {
        self.push(GhostEvent::BarrierUnmap);
    }

    unsafe fn timer_armed_head(&mut self) -> *mut TimerObj {
        self.armed_head
    }

    unsafe fn set_timer_armed_head(&mut self, head: *mut TimerObj) {
        self.armed_head = head;
    }

    #[cfg(kani)]
    unsafe fn ghost_destroy_cspace(&mut self, cs: *mut CSpaceObj) {
        self.push(GhostEvent::DestroyCspace(cs));
    }

    #[cfg(kani)]
    unsafe fn ghost_destroy_channel(&mut self, ch: *mut Channel) {
        self.push(GhostEvent::DestroyChannel(ch));
    }

    #[cfg(kani)]
    unsafe fn ghost_destroy_tcb(&mut self, t: *mut Tcb) {
        self.push(GhostEvent::DestroyTcb(t));
    }
}
