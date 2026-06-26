# 19 — Verus findings: `timer_wf` selector restatement declined on measurement (Phase 5.4)

Date: 2026-06-26. Crate: `kcore`. This is a temporary intermediate record per CLAUDE.md;
it is not referenced from comments, specs, or guidelines.

## Purpose

Phase 5.4 of `doc/plans/0_verus-improvements.md` proposed restating
`kcore::cspace::timer_wf` from a bare existential to the explicit deterministic-selector
form, mirroring the already-migrated `ready_wf` (`verus.md` §10 "eliminate a bare
existential with a deterministic selector trigger anchor" + §2). The intended effect is
**clarity/consistency** (timer parity with ready; the per-op witness-surfacing asserts in
`timer.rs` drop), gated **measure-and-keep-if-flat-or-better**. This record documents that
the restatement was implemented, verified green, measured cold against a byte-identical
control, found to **regress the crate `rlimit` total by +2.58%** — concentrated as a +44%
butterfly on the unrelated `notification::remove_waiter` — and therefore **reverted**. The
status quo is unchanged: no obligation moved, no spec/`ensures` touched, the
`external_body`/`assume_specification` tally stays 14, and the kcore Baseline stays
`406 verified, 0 errors`.

## The change evaluated (now reverted)

Logically-equivalent restatement (`new ⟹ old` because `timer_seq` is a concrete witness;
`old ⟹ new` by the `choose` axiom — both spec fns are `open`):

- `kcore/src/cspace.rs` `timer_wf` body:
  `exists|ts| #[trigger] timer_chain(tmv,head,ts) && timer_complete(tmv,ts)`
  → `timer_chain(tmv,head,timer_seq(tmv,head)) && timer_complete(tmv,timer_seq(tmv,head))`.
  `timer_seq` (the `choose` selector) and `lemma_timer_chain_unique` were left
  byte-identical.
- `kcore/src/timer.rs`: the four now-redundant witness-surfacing asserts
  (`assert(timer_chain(..,ts) && timer_complete(..,ts));`) dropped from `disarm`, `arm`,
  `check_expired`, and `destroy_timer` — each fact now comes straight from the
  explicit-selector `timer_wf` pre/ensure.

Both establishment sites (`disarm`, `arm`) re-established `timer_wf` via the `choose` axiom
off their already-proven witnesses (`ts0.remove(k)` / `pts`) with **no** added
`lemma_timer_chain_unique` call needed — the plan's contingency at `arm` was not exercised.
Both trees: `406 verified, 0 errors`, `is-verifying-entire-crate: true`.

## Measurement

Cold (`cargo clean -p kcore` before each), `cargo verus verify -p kcore --
--time-expanded --output-json`, per-fn `rlimit` summed from
`times-ms.smt.smt-run-module-times[].function-breakdown[]`. **Determinism confirmed**: a
second cold run of the changed tree reproduced every number byte-identically (crate total
`157,388,397` and `remove_waiter` `27,419,073` both runs), so the deltas are real
solver-cost changes, not noise.

| metric | before (control) | after (restated) | delta |
|--------|-----------------:|-----------------:|------:|
| crate fn-`rlimit` total | 153,435,734 | 157,388,397 | **+2.58%** |
| `notification::remove_waiter` | 19,005,838 | 27,419,073 | **+44.27%** (+8,413,235) |
| `timer::disarm` | 4,567,023 | 1,372,243 | −69.95% |
| `timer::arm` | 1,069,373 | 1,052,207 | −1.61% |
| `timer::check_expired` | 1,072,806 | 1,100,601 | +2.59% |
| `timer::destroy_timer` | 300,201 | 168,194 | −43.97% |
| **sum(disarm+arm+check_expired)** — the plan's local gate | 6,709,202 | 3,525,051 | **−47.46%** |

The single `remove_waiter` regression (+8.41M) exceeds the entire net crate increase
(+3.95M): every other delta roughly cancels (next-largest regressions
`lemma_remove_chain` +0.44M, `lemma_unlink_merge` +0.15M; next-largest improvements
`lemma_ready_remove_chain` −0.78M, `destroy_tcb` −0.63M). Net of `remove_waiter` the crate
*improves* ~4.46M.

## Why it regresses (the existential→`choose` backfire, in the shared prelude)

The targeted timer ops get much cheaper because they no longer surface the existential
witness by hand. But `remove_waiter` — which references `ready_wf`/`notif_wf` and the
`refcount_sound`/`census_delta_frozen` census machinery, and does **not** mention
`timer_wf` in its own contract — deterministically gains +44%. The restated `timer_wf` is
no longer a single opaque `exists` with one `#[trigger]`; it unfolds to two ground
conjuncts mentioning the `timer_seq` `choose` plus `timer_chain`/`timer_complete` (which
carry their own index `forall`s). Wherever `timer_wf`'s definition lands in a function's
SMT background (the shared `kcore::cspace` prelude), the richer body perturbs Z3's search.
This is exactly the documented hazard: `verus.md` §10 / CLAUDE.md warn that "extraction
around quantified/existential predicates is a known backfire," and the effect rides the
crate total, not the locally-improved ops.

## Decision

**Reverted.** The plan gives two metrics that here disagree: the §5.4-local gate (sum
across `disarm`/`arm`/`check_expired`) passes strongly (−47%), but the Phase 5 intro
("the rest are clarity/uniformity **unless `rlimit` moves**"), the cross-cutting rule
("keep … only if it … does not regress the crate's `rlimit` total"), and the Phase 5.3
precedent (declined on a smaller +0.7% crate regression, `doc/results/18`) all govern by
the **crate total**, which regresses +2.58%. Per that discipline — and the decision was
confirmed against it — the clarity/consistency win does not justify a measured crate-total
regression of the known existential→`choose` kind. A future sweep should treat 5.4 as
**declined on measurement** and not re-attempt the restatement without new evidence (e.g. a
mitigation that neutralizes the `remove_waiter` perturbation so the crate total goes
flat-or-better, at which point net-of-`remove_waiter` the change is already a ~4.46M win).

## Verification

- No trusted seam or Baseline change (count stays `406`; the only committed artifact is
  this report). The reverted `kcore/src/cspace.rs` and `kcore/src/timer.rs` are
  byte-identical to the pre-change tree, which this measurement verified cold at
  `406 verified, 0 errors` — so the final committed kcore tree carries that result
  unchanged. The `external_body`/`assume_specification` tally stays 14.
- `cargo build -p kcore` unaffected (no behaviour change; spec-only restatement reverted).
