//! Verification bounds, in one place (plan §3, §5). These mirror the
//! TLC-checked `tla/cap_revocation/CapRevocation.cfg` configuration —
//! `CapIds = 4`, `Procs = 2`, one channel of `QueueDepth = 2`,
//! `Threads = 2`, `Notifs = 2` — so the Kani harnesses re-check the same
//! state space TLC found sound, now against the real code. Scaling any of
//! these up is a one-line change here; every `#[kani::unwind]` value is
//! derived from a named bound (`bound + 1`/`+ 2`), never a magic number.

/// Slots per cspace (TLA `CapIds = 4`: a small but non-trivial cspace).
pub const CS_SLOTS: u32 = 4;
/// Cspaces in the world (TLA `Procs = 2`).
pub const NCSPACES: usize = 2;
/// Per-direction channel queue depth (TLA `QueueDepth = 2`).
pub const CHAN_DEPTH: u32 = 2;
/// TCBs in the world (TLA `Threads = 2`).
pub const NTHREADS: usize = 2;
/// Notification objects (TLA `Notifs = 2`).
pub const NNOTIFS: usize = 2;
/// Timer objects (one armed timer exercises the list logic).
pub const NTIMERS: usize = 1;
/// Address spaces (one suffices for the frame-mapping refcount edge).
pub const NASPACES: usize = 1;

/// Bare-slot pool for the structural CDT harnesses (insert/unlink/move/
/// derive). Equal to TLA `CapIds = 4`: large enough for a parent, several
/// children, and a free destination slot, small enough that the all-subsets
/// nondet shape stays inside the CI solver budget (plan §3, §8 — a 6-slot
/// pool put `check_cdt_insert_child` at ~6.5 min, over the ≤5 min target).
pub const POOL_SLOTS: usize = 4;

/// The full slot universe of a [`super::world::World`]: every cspace slot,
/// every channel ring cap slot, every TCB binding slot. This is the set the
/// `cdt_wf` walk and the refcount census range over.
pub const TOTAL_SLOTS: usize = NCSPACES * CS_SLOTS as usize          // cspace slots: 8
    + 2 * CHAN_DEPTH as usize * crate::channel::MSG_CAPS             // ring cap slots: 16
    + NTHREADS * 2; // TCB bind slots: 4  => 28

/// Op-sequence length for the transition-system harness (plan §4.1). Start
/// small; raise toward 4–6 as the CI solver budget allows (plan §3).
pub const K_STEPS: usize = 3;

/// Unwinding for the bounded walks over the slot universe (`cdt_wf`
/// acyclicity, the census scans). `+ 2` covers the terminating null step
/// plus CBMC's loop-bound assertion.
pub const UNWIND_WF: u32 = TOTAL_SLOTS as u32 + 2;
/// Unwinding for walks over the bare structural pool.
pub const UNWIND_POOL: u32 = POOL_SLOTS as u32 + 2;
/// Unwinding for the transition harness (the census dominates its loops).
pub const UNWIND_TRANSITION: u32 = UNWIND_WF;
