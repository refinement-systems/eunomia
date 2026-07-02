// SPDX-License-Identifier: 0BSD
//! The rev2§5.1 canonical parent loop, factored out of any one parent.
//!
//! A spawned child is a CDT subtree under one cap: the **donation**, a
//! child-sized untyped (`OBJ_UNTYPED`) the parent carved from its pool and
//! retyped the child's aspace, cspace, TCB, stack, segment frames and
//! bootstrap channel out of. That shape is the whole point — teardown is
//! not item-by-item reclamation but a single `revoke(donation)` that the
//! kernel walks to every descendant, followed by `reset` so the bytes (and
//! the parent's cspace slots the revoke emptied) come back for the next
//! child.
//!
//! Two disciplines live here rather than in each parent, so the next parent
//! — a retention-daemon supervisor, eventually init's service restarts —
//! inherits them:
//!
//!   * **bind before start, wait, then reap** (rev2§5.1). The on-exit/on-fault
//!     bindings are configured by the *holder* of the thread cap; a child
//!     holds no cap to its own threads, so it can neither silence nor forge
//!     its death notice.
//!   * **`read_report` strictly before `revoke`.** The report lives in the
//!     TCB the revoke destroys: read after revoke and the cap is gone, the
//!     status lost silently. `reap` does both in the one correct order and
//!     asserts it, so a caller cannot get it wrong by reordering two lines.
//!
//! What a parent reaps is the **main thread's** terminal report: a process's
//! status *is* its main thread's status. `thread_exit` ends one thread, not
//! a process — there is no Unix exit-kills-all here. A multithreaded child's
//! other threads keep running until the parent's `revoke(donation)` collapses
//! the whole subtree; the parent decides when the process is over, not any
//! one thread. A main thread that panics rather than exiting cleanly stops
//! through the runtime panic handler, which exits with `sys::STATUS_PANIC`
//! (the runtime exit path, rev2§5.1) — so `Exit::Exited(STATUS_PANIC)` is a
//! crash, distinct from any status a child passes deliberately.

use ipc::sys;

/// Why a child stopped, read out of its terminal report record (rev2§5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    /// Voluntary `thread_exit(status)`.
    Exited(u64),
    /// Unhandled fault — suspended, not destroyed (rev2§5.3). `esr` is the raw
    /// ESR_EL1 (decode the class/fault-status to taste); `far` the faulting
    /// address.
    Faulted { esr: u64, far: u64 },
}

/// The kernel-visible heart of a spawned process: its donation untyped and
/// main thread cap, plus the two notification bits its death raises. Plain
/// data; the parent owns the cspace-slot bookkeeping around it (see
/// `crate::slots`).
#[derive(Clone, Copy)]
pub struct SpawnRec {
    /// The child-sized untyped every child object descends from. Revoked
    /// and reset on `reap`.
    pub donation: u32,
    /// The main thread cap — `read_report` target and bind subject.
    pub main_thread: u32,
    /// Notification bit raised on voluntary exit.
    pub exit_bit: u64,
    /// Notification bit raised on fault. Distinct from `exit_bit` so the
    /// parent's notification word distinguishes the two terminations
    /// before it even reads the report — the rev2§3.6 bit-group scan in
    /// miniature.
    pub fault_bit: u64,
}

impl SpawnRec {
    /// True when `word` (a notification-wait result) carries this child's
    /// termination — either bit.
    pub fn terminated(&self, word: u64) -> bool {
        word & (self.exit_bit | self.fault_bit) != 0
    }

    /// Bind on-exit and on-fault to `event_notif`, the notification the
    /// parent retains a read cap to and waits on. The kernel *moves* a cap
    /// into each TCB slot, so we stage a duplicate through `scratch` (left
    /// empty on return). Call before starting the thread (rev2§5.1).
    ///
    /// Returns 0, or the first syscall error encountered.
    pub fn arm(&self, event_notif: u32, scratch: u32) -> i64 {
        for (which, bit) in [
            (sys::BIND_EXIT, self.exit_bit),
            (sys::BIND_FAULT, self.fault_bit),
        ] {
            // Duplicate first (rev2§3.4): the bind below moves the cap out of
            // `scratch` into the TCB, so the parent keeps `event_notif`.
            let r = sys::cap_copy(event_notif, scratch, sys::RIGHTS_ALL);
            if r < 0 {
                return r;
            }
            let r = sys::thread_bind(self.main_thread, which, scratch, bit);
            if r < 0 {
                // The copy is still in `scratch`; drop it so a retry/teardown
                // sees a clean slot.
                sys::cap_delete(scratch);
                return r;
            }
        }
        0
    }

    /// Read the terminal report, **then** revoke and reset the donation.
    ///
    /// The order is the invariant, not a preference: `read_report` after
    /// `revoke` finds the thread cap gone and cannot recover the status
    /// (rev2§5.1). Doing both here, in this order, is exactly so a parent can't
    /// lose exit statuses by swapping two lines.
    pub fn reap(&self) -> Exit {
        let (state, v1, v2) = sys::read_report(self.main_thread);
        // The forbidden ordering surfaces as a read failure: the donation
        // (and the TCB under it) already gone. Make it loud, not silent.
        debug_assert!(
            state >= 0,
            "read_report failed in reap — revoke ran before read"
        );
        if state < 0 {
            sys::debug_write(b"[urt] BUG: read_report before reclaim failed (ordering?)\n");
        }
        let exit = match state {
            sys::REPORT_FAULTED => Exit::Faulted { esr: v1, far: v2 },
            // A still-Running thread post-termination-wait would be a logic
            // error; fold it into Exited(status) rather than invent a case.
            _ => Exit::Exited(v1),
        };
        // Only now that the report is copied out: collapse the whole child
        // subtree, then reclaim its bytes for the next spawn.
        sys::cap_revoke(self.donation);
        let _ = sys::untyped_reset(self.donation);
        exit
    }
}
