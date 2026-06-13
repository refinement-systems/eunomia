# Kani verification findings — part 8 (transition harness broadening)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `8_kani-findings-7.md`
(§4.7). This part implements recommendation #2 of the conformance review
(`9_kani-review.md`) — *broaden the transition harness once DN-4 lifts* — and
records what that broadening can and cannot tractably reach. Harnesses live in
`kcore/src/proofs/transition.rs` and run via `cargo kani -Z stubbing -p kcore`
(CI job `kani`, pinned cargo-kani **0.67.0**). The standing caveat and design
notes (DN-1…DN-11) of the earlier parts apply unchanged.

## Background

The review's gap #2: `check_cdt_transition_system` was the weakest §4.1
integration harness — `derive` + `slot_move` only, **K = 2**, over a bare pool
of notification caps — and the `RevokedDead`/destructive transition coverage
was missing. The review tied the fix to DN-4: once the recursive-teardown wall
lifts (closed in `2_kani-findings.md`, PR #19, via `-Z stubbing` the
`destroy_cspace`/`destroy_channel`/`destroy_tcb` recursion), add `delete`/
`revoke` to the alphabet and raise K.

DN-4's stubs are reused here through a shared `kcore/src/proofs/stubs.rs`
module (extracted from `teardown.rs`'s former private `mod stub`; the teardown
harnesses retarget to it and re-verify unchanged — `check_delete_frame` 9 s).

## What this part adds

| Harness | Genre | Property |
|---|---|---|
| `check_cdt_transition_system` (raised) | additive K-step, **K = 3** | K nondet `derive`/`slot_move` steps from `Init`; `cdt_wf` (TypeOK) + census (RefCountSound) after **every** step — the multi-step composition |
| `check_delete_step` (new) | inductive 1-step over nondet shape | `delete` of *any* cap of *any* wf shape: deleted slot dead (empty+detached, DeadNowhere), exactly one ref released, `cdt_wf` preserved (⊇ LiveParent), census holds |

`cdt_wf` subsumes TLA `LiveParent` — an occupied slot whose non-null parent is
empty fails the empty⇒detached / first-child consistency checks — so the
post-state `cdt_wf` *is* the LiveParent re-check on the real teardown path.
`check_delete_step` generalizes the concrete `check_delete_reparent` to all
shapes the bounds (`POOL_SLOTS = 4` = TLA `CapIds`) admit — strictly stronger
than any state reachable from a single root at a fixed K.

Both verify; **no defect found**. The substance of this part is the coverage
above plus the precise tractability boundary below.

## DN-12 — the destructive ops do not fit a *nondet multi-step* harness

This is the finding. Recommendation #2 assumed DN-4's closure would let
`delete`/`revoke` join the K-step nondet sequence and let K rise to 4–6. It
does not, and the reason is worth recording so the boundary is not re-litigated:

- **`delete`/`revoke` in the K-step nondet sequence OOM CBMC.** Each could-delete
  branch dispatches through `obj_unref`'s match on a slot-read (symbolic)
  discriminant; a possible delete at *every* one of K steps unrolls that — and
  the recursive `destroy_*` arms — into a formula CBMC runs out of memory on.
  Measured: the 4-op alphabet (derive/move/delete/revoke) **OOMs at K = 2**
  (~9.3 M SAT variables, 44 M clauses); even the 3-op alphabet (drop revoke)
  OOMs at K = 2 and verifies only at K = 1 (33 s — weaker than the old K = 2).
  DN-4's stubs make a *single concrete* delete tractable (the `check_delete_*`
  harnesses, seconds); they do **not** tame K independent nondet deletes.
- **CBMC also emits spurious unwinding-assertion failures** in this mode: with
  `delete` in the mix it can no longer bound the `cdt_unlink` / `slot_move`
  sibling-walks without `cdt_wf` as an *assumption* (the harness only *asserts*
  it), so it reports the walks as possibly non-terminating. These are not real:
  an exhaustive plain-Rust replay of **all** length-2 op sequences
  (3 ops × 4 × 4, both steps) confirmed `cdt_wf`, the census, and the
  `RevokedDead` ghost all hold — no reachable counterexample. The CBMC
  "SATISFIABLE" was a modeling artifact of the unbounded-walk question, not a
  kernel bug.

The sound resolution, taken here, is the **inductive single-step over a nondet
shape** (`check_delete_step`): one op over an arbitrary asserted-wf CDT, where
`cdt_wf` on the pre-state bounds the walks and the single op keeps the formula
small. This covers *more* states than any fixed K (all wf shapes, not just
those reachable-from-one-root in K steps) while staying in budget.

- **`revoke` gets no inductive harness either.** Its leaf-first walk over a
  *symbolic* tree shape OOMs (`check_revoke_step` was written and measured —
  OOM at the `POOL_SLOTS = 4` nondet shape; the *concrete*-tree `check_revoke`
  alone is already ~193 s). Revoke's transition coverage therefore stays the
  concrete `check_revoke` (a derive×4-then-revoke sequence over a fixed 5-cap
  tree, asserting the same invariants) — itself a small transition harness.

So the realized alphabet is: additive `derive`/`slot_move` as a **K = 3**
multi-step sequence; `delete` as an **inductive single step over all wf
shapes**; `revoke` as the concrete `check_revoke`. `send`/`recv` remain
`check_ring_fifo` (§4.3, K = 4); `retype` and the object-creating ops need
World-level objects the bare pool does not model.

## Solver times (informational; CI per-harness budget ≤5 min, §8)

Measured on the dev machine (cargo-kani 0.67.0).

| Harness | Bounds | Time |
|---|---|---|
| `check_cdt_transition_system` | bare pool, **K = 3** | ~315 s |
| `check_delete_step` | nondet shape, `POOL_SLOTS = 4` | ~160 s |

Note the K = 3 additive run (~315 s) sits **just over** the §8 ≤5-min
per-harness guideline (the review already flagged K = 3 as "right at the
ceiling"); it is kept per rec. #2's "raise K", and reverting to K = 2 is the
one-line `const K` change if a slower CI runner needs it. With
`check_delete_step` added, the aggregate `kani` job grows by ~8 min — worth
watching against the ~30-min budget the review's rec. #5 noted.

## Status of recommendation #2

Partially realized, with the gap honestly bounded:
- ✅ `delete` added to the verified alphabet (inductively, over all wf shapes —
  stronger than the planned fixed-K reachability).
- ✅ additive K raised 2 → 3.
- ⚠️ `revoke` **not** added as a nondet/inductive transition (OOM); stays the
  concrete `check_revoke`.
- ⚠️ K not raised to 4–6 and destructive ops not in the *nondet multi-step*
  sequence — both blocked by CBMC memory (DN-12), not by DN-4. The review's
  premise that closing DN-4 would unlock these was over-optimistic; this part
  records why and delivers the soundest coverage that fits.
