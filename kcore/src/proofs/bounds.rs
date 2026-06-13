//! Verification bounds, in one place (plan §3, §5). These mirror the
//! TLC-checked `tla/cap_revocation/CapRevocation.cfg` configuration —
//! `CapIds = 4`, `Procs = 2`, one channel of `QueueDepth = 2`,
//! `Threads = 2`, `Notifs = 2`. These are **TLC-scale object bounds**, but do
//! not over-read the TLA↔Kani correspondence (review-2 critique 1,
//! `doc/results/16_kani-findings-13.md`): the suite is *complementary to*, not a
//! *reproduction of*, TLC's reachability fixpoint. Two kinds of coverage:
//!   - **per-op inductive harnesses** (`check_slot_move`, `check_derive_monotone`,
//!     `check_delete_step`, …) prove each op preserves its invariants over *all*
//!     wf states the bound admits — a *superset* of the states TLC reaches — but
//!     each is one inductive step Kani does **not** chain into a sequence;
//!   - **one multi-op transition harness** (`check_cdt_transition_system`) runs
//!     the **additive sub-alphabet only** — `{derive, slot_move}`, K = 3 — *not*
//!     the full Copy/Send/Receive/Bind/ThreadExit/Revoke/Retype alphabet TLC
//!     explores to fixpoint. The destructive ops do not compose here (DN-12):
//!     `delete` is the inductive `check_delete_step`, `revoke` the single
//!     concrete tree `check_revoke`.
//! So: the per-op inductive steps that correspond to the TLA actions (over a
//! superset of states) plus a K=3 *additive* multi-op check — not TLC's
//! full-alphabet multi-op reachability. Scaling any bound up is a one-line
//! change here; every `#[kani::unwind]` value is derived from a named bound
//! (`bound + 1`/`+ 2`), never a magic number.
//!
//! ## The `KANI_DEEP` knob (off the per-PR path)
//!
//! When the `kani_deep` cargo feature is enabled, the BarePool CDT bounds
//! widen for the heavy off-CI run driven by `scripts/deep-verify.sh kani`:
//! `POOL_SLOTS` 4 → 6 (more reachable shapes) and `K_STEPS` 3 → 4 (deeper
//! transition sequences) — the "raise K/scope toward 4–6" of plan §3 / review
//! rec. #2, kept off CI because at the wider bound a single harness can take
//! tens of minutes or OOM. CI never enables the feature, so the per-PR suite
//! stays at the TLC-scale 4 / 3.
//!
//! A *feature* (not an env var) is used precisely because the
//! `#[kani::unwind(N)]` annotations are integer literals (the attribute takes
//! no const expr): only `cfg_attr(feature = "kani_deep", kani::unwind(8))` can
//! switch a literal in lockstep with `POOL_SLOTS`. Only the two harnesses the
//! deep job runs — `check_cdt_transition_system` and `check_delete_step` — carry
//! that cfg_attr; the other BarePool harnesses keep their `unwind(6)` and so
//! must NOT be verified under the feature (their unwind would be too small at
//! `POOL_SLOTS = 6`). `deep-verify.sh` runs only the two deepened harnesses by
//! `--harness` name, which is why enabling the feature crate-wide is safe there.
//! `CS_SLOTS` / `CHAN_DEPTH` (the World harnesses) stay fixed — those are
//! concrete scenarios a wider bound only slows.

/// Compile-time deep-verification switch — see the module note. `false` on any
/// normal or CI build; `true` only under `--features kani_deep` via
/// `scripts/deep-verify.sh kani`.
const DEEP: bool = cfg!(feature = "kani_deep");

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
/// Widens to 6 under the `kani_deep` feature (the deepened harnesses carry the
/// matching `cfg_attr` unwind — see the module note).
pub const POOL_SLOTS: usize = if DEEP { 6 } else { 4 };

/// The full slot universe of a [`super::world::World`]: every cspace slot,
/// every channel ring cap slot, every TCB binding slot. This is the set the
/// `cdt_wf` walk and the refcount census range over.
pub const TOTAL_SLOTS: usize = NCSPACES * CS_SLOTS as usize          // cspace slots: 8
    + 2 * CHAN_DEPTH as usize * crate::channel::MSG_CAPS             // ring cap slots: 16
    + NTHREADS * 2; // TCB bind slots: 4  => 28

/// Op-sequence length for the transition-system harness (plan §4.1, consumed
/// by `transition::check_cdt_transition_system`). K = 3 on CI (already at the
/// per-harness ~5-min ceiling); widens to 4 under the `kani_deep` feature. The
/// exhaustive host replay (`proofs::exhaustive`) reaches deeper still, on its
/// own runtime depth knob, because plain Rust has none of CBMC's blow-up.
pub const K_STEPS: usize = if DEEP { 4 } else { 3 };

/// Unwinding for the bounded walks over the slot universe (`cdt_wf`
/// acyclicity, the census scans). `+ 2` covers the terminating null step
/// plus CBMC's loop-bound assertion.
pub const UNWIND_WF: u32 = TOTAL_SLOTS as u32 + 2;
/// Unwinding for walks over the bare structural pool.
pub const UNWIND_POOL: u32 = POOL_SLOTS as u32 + 2;
/// Unwinding for the transition harness (the census dominates its loops).
pub const UNWIND_TRANSITION: u32 = UNWIND_WF;
