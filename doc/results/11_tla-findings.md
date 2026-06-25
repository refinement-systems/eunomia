# 11 — TLA+ liveness optimization, Step 0: baseline + profile

Step 0 of `doc/plans/1_tla-liveness.md`. No spec or cfg change — this
re-establishes the authoritative before-number for the liveness pole and
profiles it to decide which sound levers (Steps 1–3) are worth their PR. The
central question it answers: **is the `CapRevocation` liveness arm's wall-clock
dominated by state generation or by the sequential SCC/liveness pass?**

## Method

Cold `scripts/tla-baseline.sh` on the merge-base (`main` @ 765fa30), vendored
`tla2tools.jar` matching its SHA1 pin, JDK 17 (Temurin 17.0.19), pinned
`-fp 0 -fpmem 0.5 -coverage 1 -Xmx4g`. Two passes:
- `TLC_WORKERS=1` — fully deterministic generated count + per-action/per-invariant
  `-coverage` attribution.
- `TLC_WORKERS=4` — the CI-representative wall-clock (the `model` job pins 4).

## Numbers (CapRevocation.cfg / EventuallyRevoked)

| pass | distinct | generated | gen:dist | diameter | verdict | wall |
|---|---|---|---|---|---|---|
| workers=1 | 503,070 | 4,831,322 | 9.6:1 | 22 | No error | 07min 40s |
| workers=4 | 503,070 | 4,831,322 | 9.6:1 | 22 | No error | **02min 07s** |

distinct (503,070) and diameter (22) reproduce the manifest pins exactly at both
worker counts — coverage intact, the run is a true cold baseline. workers=4 is
≈3.6× faster (matches `8_tla-review.md`'s 3.6× for `-workers` on this arm).
(generated is formally nondeterministic at >1 worker; here it happened to match,
but the comparison-valid generated number is the workers=1 one.)

IpcReactor.cfg / EventuallyDelivered (Step 6, the documented null): 39 distinct,
59 generated, diameter 13, sub-second. Nothing to optimize — out of scope.

## Profile

**The liveness graph is ~4× the reachable state set.** The final SCC pass runs
over **2,012,280 total distinct states** — the tableau-augmented behaviour graph
(reachable states × the `EventuallyRevoked` tableau / fairness product), versus
503,070 plain reachable states. This product is what the liveness check pays for
and what symmetry could never have reduced.

**Generation is redundancy-heavy (9.6:1).** Each reachable state is regenerated
~9.6× by the interleaving. TLC's `-coverage` rolls every sub-action up into the
top-level `Next` (503,069 : 4,853,388), so no single hot disjunct is exposed at
this granularity; the redundancy is the interleaving itself (the existential-guard
branching source was already removed by finding `5` / B6).

**The SCC/liveness check is run repeatedly, not once.** Default `-lncheck`
(absent `final`) checks the temporal properties periodically as the graph grows.
Observed intermediate passes ("Checking 4 branches of temporal properties for the
*current* state space"): **6 at workers=1, 2 at workers=4** — plus the one final
pass over the complete 2,012,280-node graph. Fewer intermediate passes fire at
workers=4 because the shorter run crosses fewer periodic boundaries.

**Per-invariant cost — `MoveSemantics` is the hot predicate.** Peak
sub-expression evaluation counts from the final coverage report (a cost proxy):

| invariant | peak sub-expr evals | note |
|---|---|---|
| **MoveSemantics** | **~7,573,812** | residence-Cardinality sum (ProcPlaces+QueuePlaces+BindPlaces) per live cap — the `\X` / `Cardinality` block at lines 360–363 |
| TypeOK | ~4,024,560 | the `\A c : parent[c] \in CapIds ∪ {NULL}` quantifier over the tableau-expanded graph |
| LiveParent | ~1,893,453 | one quantifier over live caps |
| FireSafe | ~1,006,140 | `\A t,k` over the (single) thread's binding slots |
| DeadNowhere | ~475,308 | only ranges over dead caps (`CapIds \ live`), so cheap |
| RevokedDead | ~503,070 | one set-intersection, the cheapest |

## Verdict: both sides are material; each next step has a concrete target

The arm is **not** purely generation-bound (the review's open hypothesis):
generation (4.8M states, 9.6:1) and per-state invariant work (MoveSemantics ~7.6M
sub-evals) and the repeated SCC passes over a 2M-node graph all contribute. That
gives two sound levers with distinct, quantified targets:

- **Step 1 (`-lncheck final`)** collapses the intermediate SCC passes (6 at w1 /
  2 at w4) to the single final pass. Honest expectation: the payoff is
  worker-dependent and **bounded at CI's workers=4** (only 2 intermediate passes
  to cut), larger at workers=1. Measure both; keep only if it helps at workers=4,
  and confirm the 2,012,280-node graph still fits 4g (peak heap) and that
  `CapRevocation_NegLiveness.cfg` still livelocks under the flag.

- **Step 2 (trim invariants from the liveness cfg)** removes per-state predicate
  work that scales with state count regardless of worker count — so it is the
  more worker-robust CI win. The prime target is **MoveSemantics** (the single
  hottest predicate, ~7.6M sub-evals); LiveParent/FireSafe/DeadNowhere/RevokedDead
  add the rest. Keep TypeOK as the cheap type-sanity floor. All are subsumed by
  the `model-safety` arm at larger constants, so suite-level coverage is preserved.

- **Step 3 (resourcing)** is the smallest expected lever and the SCC pass is
  sequential, so it is last.

## Negative controls / coverage

No change was made, so `scripts/tla-neg-controls.sh` was not re-run here; the
manifest pins (CapRevocation 503070/22, IpcReactor 39/13) reproduced exactly,
which is the coverage check for a no-change baseline. The before-numbers above are
the control every subsequent step re-derives against.
