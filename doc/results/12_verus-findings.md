# 12 — CommitProtocol `AtLeastOneValidSlot` + `GenerationsDistinct` labels (Task 12)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 12 (Tier 3,
TLA-routed; "near-zero-cost label"; Effort S, Risk low). Outcome: **shipped, first
attempt.** `cargo clean -p cas && cargo verus verify -p cas --no-default-features` still
reads **`75 verified, 0 errors`** (unchanged — a clause comment carries no proof
obligation). No reverts; no new trusted seam (tally stays 14); the cas Baseline is flat at
75. This is the sibling of the Task 9 and Task 10 labeling tasks.

## What was attempted

The two CAS recovery-decision functions in `cas/src/store.rs` (rev2§4.5) **already prove**,
and already verify, the facts that are the per-call half of two `CommitProtocol` safety
invariants:

- `pick_survivor` (`store.rs:465`) — `ensures (valid_a && valid_b) ==> ((r is SlotA) <==>
  gen_a >= gen_b)`: under two valid slots the winner is fixed by generation. This is the
  per-call witness of TLA `GenerationsDistinct` (`tla/commit_protocol/CommitProtocol.tla:247`)
  — two valid slots never share a generation, so the `>=` tie-break is a strict `>` and
  `LiveSlot` is deterministic.
- `commit_target` (`store.rs:509`) — `ensures r != live_slot(sb_in_b)`: the next commit
  always writes the non-live slot, so a torn write damages only the slot being written. This
  is the by-construction witness of TLA `AtLeastOneValidSlot` (`CommitProtocol.tla:244`, the
  rev2§4.5 `Crash` three-outcome safety).

The function-level doc comments already *mentioned* both invariants; what was missing was the
explicit labeling discipline Tasks 9/10 established — a comment **on the specific `ensures`
clause** marking it the local per-call witness, plus a trusted-base routing note recording
that the *global* invariant stays TLA-owned. This was **labeling only**: two inline comments
in `store.rs` (no `verus!{}` logic change) and one ledger routing note.

**Lightest of the three labeling tasks — comments, not new `spec fn`s.** Tasks 9 and 10
introduced named `open spec fn`s because their `ensures` were complex expressions over store
views (`ring_fifo(...).push(...)`; the whole-store `fire_safe` predicate). Here the two
clauses are self-contained booleans over each function's own scalar args, and the function
doc already names the invariant — so a one-line comment on the clause is the faithful
instrument (per the Task 12 text). A wrapper `spec fn` would be the vacuous/over-engineered
label the Task 9 finding warns against, with no reader benefit. Used comments.

## Result

- `cargo clean -p cas && cargo verus verify -p cas --no-default-features` →
  **`75 verified, 0 errors`** (cold, authoritative; the `verification results::` line was
  present == a real, non-cached run; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`).
  The count is **unchanged** from the pre-change 75: a comment on an `ensures` clause adds no
  obligation, leaves the proven proposition byte-identical, and is invisible to the solver.
- **Not a proof change (§2.1), so the measurement is a single confirming cold run, not a
  before/after `rlimit` diff.** No `verus!{}` body, signature, spec, or predicate changed —
  only comment text inside two `ensures` lists — so the per-function `rlimit` is flat by
  construction (the same proposition discharged by the same SMT terms). The §2 "measure every
  proof change" rule is satisfied vacuously: there is no proof change to measure.
- `cargo build -p cas` clean; `cargo fmt --check -p cas` clean (rustfmt does not descend into
  `verus!{}`, and the edits are comments). Erasure leaves the exec code byte-identical, so
  the recovery proptests are unaffected.

## What stayed TLA-owned (no over-claim)

The TLA `CommitProtocol` model is **not** retired or demoted. Only the two *local per-call*
witnesses moved to a named reading in Verus. The *global* arms the verified pure core cannot
witness stay the design oracle: `AtLeastOneValidSlot`/`GenerationsDistinct` as crash-step
invariants over the whole `slotA`/`slotB` × `walLog`/`writeCtr` state (checked by the
6886-state TLC run + `CommitProtocol_NegControl.cfg`), the `Crash` three-outcome safety, the
cross-restart `Recover`/`RecoverReconstructs` replay-equality (the headline recovery arm),
and the `FsyncMeansFsync` storage axiom. This folds in the §4
`commit-crash-recover-interleaving-tla-only` one-line routing-note audit the plan directs be
executed alongside Task 12 (crash three-outcome + cross-restart Recover stay TLA;
`FsyncMeansFsync` stays the axiom row already in the ledger — a confirmation, no new row, no
proof).

## Deviation from the task text (intentional)

Task 12 says "Bundle into a larger conformance-doc change, not a standalone PR." Per the
explicit request and the repo's actual workflow — one PR per task (PRs #234/#235/#236 shipped
Tasks 9/10/11 standalone) — this ships as a **standalone PR**.

## Reverted vs kept

Nothing reverted — labeling only, everything verifies. Kept: the two inline `ensures`-clause
comments in `cas/src/store.rs` (`pick_survivor`'s `GenerationsDistinct` determinism witness,
`commit_target`'s `AtLeastOneValidSlot` by-construction witness); the
"CommitProtocol `AtLeastOneValidSlot` + `GenerationsDistinct` routing note" in
`verus_trusted-base.md` (no number change — cas stays `75 verified`, tally stays 14); and this
findings doc. No incidental code changes were needed.

## Proposed additions to `doc/guidelines/verus.md`

**None new.** Task 9's appended bullet on "labeling an already-proven fact under an invariant
name" already covers the comment-only case (its `open`-vs-`closed` and vacuous-label guidance
generalizes to "when the clause is a self-contained boolean and the function doc already names
the invariant, an inline comment on the clause — not a wrapper `spec fn` — is the faithful,
non-vacuous label"). Padding the guideline with a near-duplicate bullet would be exactly the
kind of restate-what-is-already-said the discipline discourages.
