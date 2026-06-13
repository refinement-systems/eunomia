# Kani verification findings — part 13 (TLA↔Kani framing correction)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `15_kani-findings-12.md`.
This part implements **recommendation #4** of the second conformance review
(`14_kani-review-2.md`): correct the `bounds.rs` framing of how the Kani suite
relates to the TLC model. **No code, no harness, no defect** — a one-comment
framing correction so the next reader does not over-read the TLA↔Kani
correspondence. The standing caveat and design notes (DN-1…DN-13) of the earlier
parts apply unchanged; this adds no new DN.

## The conflation (review-2 critique 1)

The first-review prose and an earlier `bounds.rs` comment described the Kani
harnesses as "re-checking the same state space TLC found sound." That conflates
two different things:

- **Object counts are shared.** Both tools work at `CapIds = 4`, `Procs = 2`,
  `QueueDepth = 2`, `Threads = 2`, `Notifs = 2` — the TLC-checked
  `CapRevocation.cfg` scope. True, and the right scope (the model-checking
  tradition both tools share: the interesting interleavings of this state space
  manifest at small scope).
- **Exploration is *not* shared.** TLC does a full breadth-first reachability
  over the *entire* action alphabet (Copy/Send/Receive/Bind/ThreadExit/
  ThreadFault/Revoke/Retype) to fixpoint, but only over *reachable* states. The
  Kani suite does something different — and in one respect stronger, in another
  weaker — which the "same state space" phrasing flattened.

## The accurate picture (now in `bounds.rs`)

Three points, which the corrected module comment states:

1. **Per-op inductive harnesses** (`check_slot_move`, `check_derive_monotone`,
   `check_delete_step`, …) prove each op preserves its invariants over *all* wf
   states the bound admits — a **superset** of the states TLC reaches, including
   ones TLC never does. This is **stronger than TLC per-op**. But each is a
   single inductive step: **Kani does not chain them** into a sequence.
2. **One multi-op transition harness** (`check_cdt_transition_system`) is the
   only composition check, and it runs the **additive sub-alphabet only** —
   `{derive, slot_move}`, K = 3 — **not** the full TLA action alphabet. This is
   where the old "a bounded prefix of the action alphabet" phrasing over-read:
   it suggested the full alphabet merely truncated by length, when in fact only
   2 of the ~8 actions are present, and only the additive ones.
3. **The destructive ops do not compose in the K-step harness** (finding DN-12):
   putting `delete`/`revoke` into the nondet K-step sequence OOMs CBMC. So
   `delete` is covered by the *inductive* `check_delete_step` (one op over all
   wf shapes) and `revoke` by the single concrete tree `check_revoke` — not by
   multi-op reachability.

Net: the suite proves the per-op inductive steps that correspond to the TLA
actions (over a superset of states) **plus** a K=3 *additive* multi-op check — it
does **not** reproduce TLC's full-alphabet multi-op reachability fixpoint. That is
a complementary result, which is the point: it is not redundant with TLC.

## What changed, precisely

PR #25 (the `kani_deep` knob) had already rewritten the `bounds.rs` module
comment — it removed the literal "same state space TLC found sound" phrase and
added "complementary to — not a reproduction of — TLC's full-alphabet
reachability fixpoint." So most of critique 1 was already addressed incidentally.
The **residual** this part closes is the one remaining over-read in that rewrite:
the transition harness was still described as re-running "a bounded prefix of the
action alphabet." That clause is now replaced with the explicit **additive
sub-alphabet only (`{derive, slot_move}`, K = 3)** framing above, plus the note
that the destructive ops are covered inductively / concretely rather than in the
composition.

`kcore/src/proofs/transition.rs` needs **no change**: its module comment already
splits the two genres accurately ("## 1. Additive K-step sequence —
`check_cdt_transition_system`" naming `{derive, slot_move}`, and "## 2. Inductive
single-step over a nondet shape — `check_delete_step`" with the DN-12 reason the
destructive ops cannot join). The fix was localized to `bounds.rs`, the single
file the recommendation named.

## Verification

Comment-only change — no harness, no bound value, no `#[kani::unwind]` literal
touched — so **no Kani re-run is semantically required** (consistent with how
part 11's docs-only change was handled). Sanity-checked that the `//!` block
still parses and nothing regressed: `cargo build -p kcore` clean and
`cargo test -p kcore` green (11 passed, 2 heavy `#[ignore]`d).

## Status of recommendation #4

✅ Done. The `bounds.rs` comment now states the Kani↔TLC relationship precisely:
TLC-scale object bounds, per-op inductive coverage over a superset of states, and
a K=3 *additive* transition check — explicitly not TLC's full-alphabet
reachability. Remaining open review-2 items: #5 (tighten the CI cover post-check
message to distinguish "no cover lines found" from "cover unreachable") and #6
(the off-path `-Z function-contracts` spike on `revoke`/`obj_unref`).
