# 18 — Verus findings: flushed-only `RecMeta` trigger projection declined on measurement (Phase 5.3)

Date: 2026-06-26. Crate: `cas`. This is a temporary intermediate record per CLAUDE.md;
it is not referenced from comments, specs, or guidelines.

## Purpose

Phase 5.3 of `doc/plans/0_verus-improvements.md` proposed projecting the flushed-only
`RecMeta` `forall`s in `cas/src/store.rs` from the whole-aggregate trigger `records@[k]`
onto the field projection `records@[k].flushed` (`verus.md` §10/§13). The task is explicitly
**measure-and-keep-if-helps**: "Keep only if each obligation's `rlimit` is flat-or-better
AND the crate total does not regress; revert otherwise." This record documents that the
projection was implemented, measured cold against a byte-identical control, found to
**regress** `recover_records` (+28.2%) and the crate total (+0.7%), and therefore **reverted**
per that gate. The status quo is unchanged: no code obligation moved, no spec/`ensures`
touched, the `external_body`/`assume_specification` tally stays 14, and the cas Baseline
stays `79 verified, 0 errors`.

## The change evaluated (now reverted)

Eight whole-aggregate trigger annotations were projected onto `.flushed`, the only field
each body reads (annotation-only; every quantifier body, `requires`/`ensures`/invariant, and
input range byte-identical):

| Fn | Clause | Before | After (reverted) |
|----|--------|--------|------------------|
| `advance_head` | ensures | `#![trigger records@[j]]` | `#![trigger records@[j].flushed]` |
| `advance_head` | loop invariant | `#![trigger records@[j]]` | `#![trigger records@[j].flushed]` |
| `lemma_gap_freedom` | requires | `(#[trigger] records[j]).flushed` | `(#[trigger] records[j].flushed)` |
| `lemma_gap_freedom` | ensures | `!(#[trigger] records[i]).flushed` | `!(#[trigger] records[i].flushed)` |
| `lemma_gap_freedom` | assert forall | `#![trigger records[i]]` | `#![trigger records[i].flushed]` |
| `recover_records` | ensures | `!(#[trigger] r.records@[k]).flushed` | `!(#[trigger] r.records@[k].flushed)` |
| `recover_records` | loop invariant | `!(#[trigger] records@[k]).flushed` | `!(#[trigger] records@[k].flushed)` |
| `recover_records` | loop ensures | `!(#[trigger] records@[k]).flushed` | `!(#[trigger] records@[k].flushed)` |

The two `rec_ok(wal@, records@, k)` `forall`s in `recover_records` (the loop invariant and
loop ensures siblings) were correctly **excluded** — their body reads `.off`/`.seq` and
relates neighbours `k`/`k+1` through `rec_ok`, so they are not flushed-only. The
`lemma_gap_freedom` assert-forall was projected in lockstep with its target `ensures` forall
per §10 ("a helper `assert forall` must mirror the target conjunct's trigger verbatim").

## Measurement

Cold (`cargo clean -p cas` before each), `cargo verus verify -p cas --no-default-features --
--time-expanded --output-json`, per-fn `rlimit` summed from
`times-ms.smt.smt-run-module-times[].function-breakdown[]`. Only the eight trigger
annotations differ between the two trees. **Determinism confirmed**: a second cold run of the
projected tree reproduced every number byte-identically (crate total and all three functions),
so the deltas below are real solver-cost changes, not noise.

| metric | before (control) | after (projected) | delta |
|--------|-----------------:|------------------:|------:|
| crate fn-`rlimit` total | 15,403,513 | 15,516,663 | **+0.7%** |
| `advance_head` | 38,866 | 38,942 | +0.2% (flat) |
| `lemma_gap_freedom` | 24,881 | 24,156 | −2.9% |
| `recover_records` | 284,385 | 364,624 | **+28.2%** |

Both trees: `79 verified, 0 errors`, `is-verifying-entire-crate: true`.

## Why it regresses (the matching-loop hazard is absent here)

The §10 projection win comes from breaking a *self-perpetuating matching loop*: a
whole-aggregate trigger `records@[k]` re-matches when the body relates a same-shape neighbour
`records@[k+1]`, flooding the context. In `cas/src/store.rs` that hazard does not exist for
these `forall`s — none of the flushed-only bodies mention `records@[k+1]`, and the only
neighbour relation in `recover_records` lives inside the `rec_ok` **predicate** (a
`spec fn` application), not in a tuple/struct-index forall the projection could decouple. The
plan anticipated this ("These do not relate same-shape neighbours … so the matching-loop
hazard is largely absent and the win may be small"). With no loop to break, narrowing the
trigger from the whole element to the `.flushed` projection only changes *which* terms Z3
instantiates on — and in `recover_records` (which threads `rec_ok`, `laid_out`, and the
`.flushed` invariant together through a `decreases` loop) the narrower trigger fires on fewer
of the terms the surrounding obligations need, so the solver re-derives them by other paths
at net +28% cost. `advance_head` is flat and `lemma_gap_freedom` dips slightly, but the gate
is conjunctive (every touched obligation flat-or-better **and** the crate total flat), and
`recover_records` plus the crate total both fail it.

## Decision

**Reverted.** The projection is not applied. This is the plan's explicit "revert otherwise"
branch, not a partial keep: the projected mirror pair (`lemma_gap_freedom`'s `ensures` forall
and its discharging `assert forall`) must move together, and keeping only the flat
`advance_head` pair would make the trigger shape non-uniform within the `RecMeta` family for
no measured benefit — the opposite of §10's uniformity aim. A future sweep should treat 5.3
as **declined on measurement** and not re-attempt it without new evidence that the cas
context has changed.

## Numbering note

The plan instructed the first findings report to begin at `14` ("the directory is currently
empty … begin at `14`"). That guidance is now stale: phases 2.2, 2.3, 3.2, and 3.3 already
filled `doc/results/14`–`17`. Per the plan's own "next unused `N`" rule this report is **18**.

## Verification

- No trusted seam or Baseline changes (count stays `79`; the only committed artifact is this
  report). The reverted `cas/src/store.rs` is byte-identical to the pre-change tree, which
  this measurement verified cold at `79 verified, 0 errors` — so the final committed cas tree
  carries that result unchanged.
- `cargo build -p cas` clean; `cargo test -p cas` unaffected (no behaviour change).
