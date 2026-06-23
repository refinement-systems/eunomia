# B9C — Preemptible revoke: TLA `CapRevocation` atomic → stepwise interleaving + liveness (findings)

Working notes from the implementation of **Phase B9C** (`doc/plans/9_b9-detail.md`,
sub-phase B9C — the formal-model deliverable, the TLA+ tier). Records what landed, the
modeling decisions, the TLC tooling facts worth keeping (two of which bit during the run),
and the deviations from the plan. Closes the audit §2.6 honesty point: the atomic
`Revoke` model was faithful *only because* the kernel was non-preemptible — the exact
assumption B9 removes — so the leaf-first deletion-order obligation, previously recorded as
a header comment, is now a **checked** property under arbitrary interleaving, and completion
of a restarted revoke is now a **checked** liveness property.

Independent of B9A (Verus) and B9B (shell) — models the same design, no code dependency.

---

## 0. Headline

All B9C gates green:

- **Main model (`CapRevocation.cfg`, stepwise) PASS** — `503,070` distinct states (depth 22).
  The six safety invariants (`TypeOK`/`MoveSemantics`/`DeadNowhere`/`LiveParent`/`FireSafe`/
  `RevokedDead`) hold at every interleaved mid-revoke state, `ReportMonotone` holds, and the
  new `EventuallyRevoked` liveness property holds under weak fairness on `RevokeStep`.
- **Safety negative control (`CapRevocation_NegControl.cfg`) FAILS as designed** — `LiveParent`
  counterexample: build `c0→c1→c2`, `RevokeBegin(c0)`, then a non-leaf `RevokeStepBad` deletes
  the *interior* `c1`, orphaning `c2` (a live cap whose parent is now dead). Proves leaf-first
  ordering is load-bearing.
- **Liveness negative control (`CapRevocation_NegLiveness.cfg`) FAILS as designed** —
  `EventuallyRevoked` livelock lasso: with the `Copy` guard dropped (`CopyNoGuard`),
  `RevokeStep` deletes a leaf and `CopyNoGuard` re-adds it forever ("Back to state 7"), so the
  subtree never empties even though `RevokeStep` keeps firing. Proves the derive guard is
  load-bearing for termination.
- **`CapRevocation_Teardown` (TSpec) PASS, unchanged** — 252 distinct states; the `revoking`
  variable joining `crVars` is framed by `TNext`'s existing `UNCHANGED crVars`, so the
  channel-teardown half is undisturbed.
- **SANY clean** (`tools/tla/tla-check.sh`).

Docs: the one clarifying rev1§2.2 sentence (signed off), the rev1§6.1 mechanized-status note
(no `[verifying]` flip), and the ledger scope-paragraph + `CapRevocation` Baselines line.

---

## 1. The model change (Design decision 3)

`Revoke(c)` (one atomic step deleting all `Descendants(c)`) became three interleaving actions
over a new `revoking \in SUBSET CapIds` variable:

- `RevokeBegin(c)` — marks a root with descendants (`revoking' = revoking ∪ {c}`), deletes
  nothing.
- `RevokeStep(c)` — one bounded quantum: pick a **leaf** descendant (`IsLeaf`) and delete only
  it (`DeleteOne`, the single-cap form of the old atomic body — removes the cap from cspaces,
  every queue slot, every binding slot, clears its parent, ghost-revokes it).
- `RevokeEnd(c)` — clears the marker once the subtree is empty.

`Copy` gained `~AncestorOrSelfRevoking(src)` (a `RECURSIVE` upward parent-walk, the TLA mirror
of B9A's verified `ancestor_or_self_revoking`) — the model of B9A's derive guard. `Send`/
`Receive`/`Bind`/`ThreadExit`/`ThreadFault`/`Retype` each gained `revoking` in their
`UNCHANGED` frame. `Spec` gained `/\ Fairness` (weak fairness on `RevokeStep`); the liveness
property `EventuallyRevoked == \A c : (c ∈ revoking) ~> (Descendants(c) = {})` was added.

**Why leaf-only deletion is the load-bearing safety choice:** deleting a childless cap orphans
nobody, so `LiveParent` (a live cap's parent is live) holds at *every* preemption point. The
safety control deletes interior caps and immediately violates it.

**Why the guard is the load-bearing liveness choice:** with no re-derivation into a revoking
subtree, the subtree shrinks monotonically, so weak fairness on `RevokeStep` drains it. The
liveness control drops the guard and livelocks.

---

## 2. Finding: `WF_vars(RevokeStep)` is rejected — fairness must use the `crVars` subscript

First TLC run died with:

> The action formula A appearing in a `WF_v(A)` operator does not specify the primed value of
> the variable `nlive` occurring in the state formula v.

`vars` is the full 12-tuple including the TSpec half (`nlive`/`ncaps`/`pcbind`/`eopen`).
`RevokeStep` is a revocation-half action — under `Next` the `UNCHANGED tdVars` is conjoined at
the *disjunction* level, so the action itself names no `tdVars` prime, and TLC requires the
fairness action to specify the primed value of every variable in the subscript. Fix:
`WF_crVars(RevokeStep(c))` — `crVars` contains only the eight revocation-half variables, all of
which `RevokeStep` specifies (six written, `treport`/`revoking` via `UNCHANGED`). Under `Spec`
the `tdVars` are constant, so `WF_crVars` and the intended `WF_vars` coincide; only the latter
is malformed. **Takeaway:** in a two-half spec where each half frames the other at the `Next`
level, fairness on a half-action must take that half's subscript, not the global `vars`.

## 3. Finding: the full-scale stepwise + liveness model exhausts heap — reduced constants

This is the headline tooling finding. Run at the **atomic baseline's** constants (4 caps,
2 procs, 2 threads, 1 channel, QueueDepth 2 — the ~799k-state, ~2-min atomic model), the
stepwise model explodes:

- Stepwise revoke creates an intermediate state per leaf-deletion (vs one atomic step), and the
  `revoking` `SUBSET` adds marker combinatorics → the reachable space grew to **9.3M+ distinct
  states at depth 17 and still counting** when it died.
- It died **during liveness checking**: `EventuallyRevoked` builds a behaviour tableau (×4
  branches here); that in-memory graph reached **>38M states** and exhausted the default 4 GB
  heap (`Java ran out of memory during liveness checking`). Invariant checking alone would
  *not* OOM — TLC spills fingerprints and the state queue to disk (the run left an 11 GB
  `states/` scratch dir) — it is specifically the liveness graph that is memory-bound.

**Resolution:** the stepwise model runs at **trimmed constants — Threads 2→1, QueueDepth 2→1**
(4 caps, 2 procs, 1 thread, 1 channel, QD 1) → **503,070 distinct states**, a ~2 M-state
liveness tableau that fits the default heap, ~1m40s with `-workers auto`. The reduction is
sound for B9C's purpose: the *atomic* model already established the safety invariants at the
full scale, and B9C's new obligation is that they hold at every **mid-revoke interleaved
state** plus completion liveness — both fully exercised by a structurally complete smaller
model. 4 caps still give multi-level subtrees (the leaf-first CEX needs `c0→c1→c2`); a cap
still reaches all three residences (cspace, queue slot, TCB binding slot), so
revoke-through-queue and the `FireSafe` binding-slot story are still covered. Threads and queue
depth only multiply *residence* combinatorics, not the revoke/guard dynamics the new properties
test.

This is a real change from the plan, which assumed the full constants with "the state graph
grows from ~799k". It does grow — to >9 M and an intractable liveness tableau — which is *why*
the constants are trimmed. Recorded here so the reduction is a deliberate, documented choice,
not a silent cap.

## 4. Finding: two cfgs for two controls; `tla-model-check.sh` cfg path is spec-relative

- **Two negative-control cfgs, not one.** The plan named a single `CapRevocation_NegControl.cfg`
  "(×2 controls)". A TLC `.cfg` admits exactly one `SPECIFICATION`, and the two controls drive
  different specs (`SpecBad` for the safety CEX, `SpecNoGuard` for the liveness CEX), so they
  are two files: `CapRevocation_NegControl.cfg` (safety) and `CapRevocation_NegLiveness.cfg`
  (liveness). Each control action is the real action **minus exactly one load-bearing
  conjunct** (the `IsLeaf` filter; the `~AncestorOrSelfRevoking` guard) — the B7 discipline,
  so a passing main model plus a failing control pins that conjunct.
- **`tools/tla/tla-model-check.sh <spec.tla> <cfg>` resolves the cfg *after* `cd`-ing into the
  spec's directory.** Pass the cfg **basename** (`CapRevocation_NegControl.cfg`), not a
  repo-root-relative path — a path like `tla/cap_revocation/CapRevocation_NegControl.cfg`
  becomes `tla/cap_revocation/tla/cap_revocation/…` and TLC reports "File not found." The plan's
  verification commands had this wrong.
- **Heap + workers for the gate.** The default-heap, single-worker `tools/tla/tla-model-check.sh`
  run of the main model completes (503 k states fit 4 GB), but `-workers auto` cuts it to
  ~1m40s. For larger TLA models, invoke TLC directly with `-workers auto` (the script does not
  forward extra flags). The committed `tla/.gitignore` keeps the `states/` scratch and the
  `*_TTrace_*` trace-exploration files TLC emits out of git.

## 5. Verified-/modeled-surface accounting

No Verus/kcore/kernel change (B9C is the TLA+ tier) — the kcore gate stays **381/0** (B9A). The
ledger Baselines `CapRevocation` line now records the stepwise model, the 503,070-state count,
the `EventuallyRevoked` liveness, and the two committed negative controls; the scope paragraph
adds the preemptible revoke walk (the B9A items, with the interleaving/liveness modeled here).
rev1§2.2 gains the one clarifying surfacing sentence (signed off); rev1§6.1's revoke line notes
preemptibility is mechanized (Verus per-step + TLA interleaving/liveness) with **no
`[verifying]` flip** (honesty note 4 — §6.1 carries no such tag for revoke; the scheduler
policy / exception entry / asm switch stay literally `[trusted]`).

## 6. What B9 now is (A+B+C)

- B9A — verified bounded `revoke_step`, the `revoking` marker, the `derive` guard (Verus, kcore
  381/0). Per-step safety + per-call termination.
- B9B — the `EAGAIN` syscall surface + userspace retry loop (shell). The latency bound.
- B9C — the stepwise TLA model: leaf-first safety at every preemption point + completion
  liveness under the guard, two committed negative controls. The cross-restart property Verus
  cannot express (`EventuallyRevoked`) lives here.

Audit M-1 (revoke not preemptible/restartable) and the §2.6 honesty point (atomic model
faithful only under non-preemption) are both closed.
