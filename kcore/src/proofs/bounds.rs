//! Verification bounds, in one place (plan §3, §5). These mirror the
//! TLC-checked `tla/cap_revocation/CapRevocation.cfg` configuration —
//! `CapIds = 4`, `Procs = 2`, one channel of `QueueDepth = 2`,
//! `Threads = 2`, `Notifs = 2`. At these bounds the Kani harnesses give two
//! kinds of coverage: the contract/inductive harnesses prove each op preserves
//! its invariants over *all* wf states the bound admits (a superset of the
//! states TLC reaches), while the K-step transition harness re-runs a bounded
//! prefix of the action alphabet from `Init`. This is complementary to — not a
//! reproduction of — TLC's full-alphabet reachability fixpoint. Scaling any of
//! these up is a one-line change here; every `#[kani::unwind]` value is
//! derived from a named bound (`bound + 1`/`+ 2`), never a magic number.
//!
//! ## The `KANI_DEEP` knob (off the per-PR path)
//!
//! When the `KANI_DEEP` env var is set at *compile time*, `K_STEPS` widens
//! (3 → 4) for the heavy off-CI run driven by `scripts/deep-verify.sh` — the
//! "raise K toward 4–6" of plan §3 / review rec. #2, kept off CI because the
//! additive transition harness is already at the per-harness budget at K = 3.
//! CI never sets the var, so the per-PR suite stays at K = 3.
//!
//! Only `K_STEPS` scales automatically, because the `#[kani::unwind(N)]`
//! annotations are integer literals (the attribute takes no const expr) tuned
//! to `UNWIND_POOL`; raising the *object-count* bounds (`POOL_SLOTS`,
//! `CS_SLOTS`, `CHAN_DEPTH`) is therefore a deliberate manual edit that must
//! bump those literals in lockstep, not an env toggle. Widening `K_STEPS`
//! alone is safe: at K = 4 the transition harness's K-loop is ≤ 5 and its
//! census scans are unchanged (still `POOL_SLOTS`), so `unwind(6)` still holds.

/// Compile-time deep-verification switch — see the module note. `false` on any
/// normal or CI build (the env var is unset); `true` only under
/// `KANI_DEEP=1 cargo kani …` via `scripts/deep-verify.sh`.
const DEEP: bool = option_env!("KANI_DEEP").is_some();

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
/// Raising it is a manual edit (bump the `#[kani::unwind]` literals too — see
/// the module note); it is deliberately NOT on the `KANI_DEEP` toggle.
pub const POOL_SLOTS: usize = 4;

/// The full slot universe of a [`super::world::World`]: every cspace slot,
/// every channel ring cap slot, every TCB binding slot. This is the set the
/// `cdt_wf` walk and the refcount census range over.
pub const TOTAL_SLOTS: usize = NCSPACES * CS_SLOTS as usize          // cspace slots: 8
    + 2 * CHAN_DEPTH as usize * crate::channel::MSG_CAPS             // ring cap slots: 16
    + NTHREADS * 2; // TCB bind slots: 4  => 28

/// Op-sequence length for the transition-system harness (plan §4.1, consumed
/// by `transition::check_cdt_transition_system`). K = 3 on CI (already at the
/// per-harness ~5-min ceiling); widens to 4 under `KANI_DEEP`. The exhaustive
/// host replay (`proofs::exhaustive`) reaches deeper still, on its own runtime
/// `EXHAUSTIVE_DEPTH` knob, because plain Rust has none of CBMC's blow-up.
pub const K_STEPS: usize = if DEEP { 4 } else { 3 };

/// Unwinding for the bounded walks over the slot universe (`cdt_wf`
/// acyclicity, the census scans). `+ 2` covers the terminating null step
/// plus CBMC's loop-bound assertion.
pub const UNWIND_WF: u32 = TOTAL_SLOTS as u32 + 2;
/// Unwinding for walks over the bare structural pool.
pub const UNWIND_POOL: u32 = POOL_SLOTS as u32 + 2;
/// Unwinding for the transition harness (the census dominates its loops).
pub const UNWIND_TRANSITION: u32 = UNWIND_WF;
