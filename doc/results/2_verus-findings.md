# 2 — Admission quota as the verified accounting template (Task 2)

Date: 2026-06-25. Attempt against `doc/plans/0_verus-concurrency.md` Task 2
(document `Admission` quota as the verified accounting template; Tier 4,
already-verified). Outcome: **documentation-only**. `cargo verus verify -p ipc`
stays flat at `68 verified, 0 errors`; no proof, spec, or exec-logic change; no
reverts.

## What was attempted

`ipc::session::Admission` (`ipc/src/session.rs:329`) is *already* Verus-verified —
its proof is part of the `-p ipc` count of `68`. Task 2 records it as the
**verified accounting template** the reactor `used`-mask dispatch work reuses
(Task 1, already landed; Task 6, future), without touching the proof:

- **`doc/guidelines/verus.md` §14 (new)** — landed the durable pattern: a
  `closed spec fn well_formed` accounting cap (`granted <= budget`), a
  `closed spec fn` observable (`spec_remaining = budget - granted`) provably
  non-negative under `well_formed` so the exec accessor never underflows, and a
  `requires self.well_formed()` / `ensures final(self).well_formed()` on every
  mutator that closes the never-over-grant bound over *all* sequences by modular
  composition. The section maps the template onto the dispatch `used`-mask
  accounting (`used` ↔ `granted`, full word ↔ `budget`, `alloc`/`drain` ↔
  `admit`/`release`).
- **`doc/guidelines/verus_trusted-base.md`** — the `-p ipc` Baseline parenthetical
  now *enumerates* `Admission` (it was previously unnamed though counted in the
  `68`), with the count held **flat at 68**; and a new "IPC `Admission` quota
  routing note" records the `fairness_smoke` invariant-overlap correction below.
- **`ipc/src/session.rs`** — one cross-reference comment on the `Admission` struct
  doc pointing at the §14 template. No field/spec/contract/logic change.

## The honest correction (fairness_smoke overlap)

The plan flagged the one real subtlety: the Verus `well_formed` invariant
(`granted <= budget`) makes the *invariant* arm of the concurrent `fairness_smoke`
harness (`ipc/src/model.rs:638`, asserting exactly `min(budget, N)` grants under N
client threads — `model.rs:735`–750) **redundant** with the proof. But that
harness is **kept**: it additionally witnesses what Verus does not — that the
concurrent plumbing calls `admit` *atomically* under thread interleaving (the
Shuttle-routed `fairness_smoke_shuttle` arm; `fairness_smoke_std` is the std
smoke). The invariant overlaps; the interleaving-atomicity check does not. This is
recorded in the new ledger routing note so a reviewer does not read the overlap as
license to delete the harness arm.

## Result

`cargo clean -p ipc && cargo verus verify -p ipc` → **`68 verified, 0 errors`**
(real run, results line present; prover `0.2026.06.07.cd03505` / toolchain
`1.95.0`). **Flat** vs the pre-change tree — Task 2 adds no `verus!{}` obligation,
so the count cannot move; a real run that printed a different number would have
meant the documentation claimed a count it could not back. The cross-reference
comment erases (`verus!{}`-adjacent doc-comment, not code), so the host build, the
aarch64 cross-build, and the verified exec are byte-identical.

Companion oracle tier kept and green: `cargo test -p ipc` passes, including
`fairness_smoke_std` (the kept concurrent harness arm).

No `rlimit` measurement applies: no `verus!{}` item was added, changed, or removed,
so there is no proof-cost delta to baseline (per the §10 discipline, measurement is
required only for changes touching `verus!{}` code).

## Reverted vs kept

Nothing reverted — there was no proof attempt to fail. Nothing in plain exec logic
changed. Kept: the new `verus.md` §14 template, the ledger Baseline enumeration +
routing note, the findings doc, and the one-line `session.rs` cross-reference
comment.

## Proposed guideline additions (`doc/guidelines/verus.md`)

The template pattern was **landed**, not merely proposed: `verus.md` §14, "The
verified accounting template." Future accounting cores (refcount ceilings, slot
masks, byte budgets) should cite it, and Task 6's reactor coherence invariant is
its bitmask instance. No further guideline change proposed by this task.

## Trusted base

Unchanged. Task 2 adds **no** `external_body`/`assume_specification` and onboards
no crate, so the 14-seam tally and every Baseline number stay fixed; the `-p ipc`
Baseline count stays `68`.
