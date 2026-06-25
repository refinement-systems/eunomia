# TLA+ / TLC optimization findings — B6

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines. (B1's outcome is in `0_tla-findings.md`, B2's in
`1_tla-findings.md`, B3's in `2_tla-findings.md`, B4's in `3_tla-findings.md`,
B5's in `4_tla-findings.md`.)*

All measurements below are **cold** (TLC scratch wiped first), vendored
`tools/tla/tla2tools.jar` (matches its `.sha1`), Temurin 17, host Darwin arm64,
**`-workers 1 -fp 0 -fpmem 0.5 -coverage 1`** via `scripts/tla-baseline.sh`.
B6 is a semantics-preserving guard rewrite, so the bar is the strongest one in
plan §1: **byte-identical distinct *and* generated counts**. generated-states is
only deterministic single-worker, so all arms ran at `-workers 1` (not CI's 4)
so the generated comparison is exact; distinct and diameter are worker-invariant
either way.

---

## B6 — `Descendants(c) = {}` guards → a direct children-set test

**Status: adopted, in the branch-free `Children(c)` form — a null-performance
clarity refactor. The existential form the plan literally proposes
(`~\E x \in CapIds : parent[x] = c`) was implemented, measured, and REJECTED: it
is a deterministic ~3.1% *generated*-states regression. The adopted form
realises B6's intent (drop the `RECURSIVE` walk from the guards) without that
cost.**

This is the null-perf-but-clarity-positive case the engagement policy says to
merge: the transition relation is logically unchanged, every coverage and work
metric is byte-identical to baseline, and the spec reads slightly better while
gaining an explicit guard against re-introducing the regression.

### What B6 targets

`Descendants(c) = {}` (an emptiness test of a `RECURSIVE` transitive-closure
operator) guards three actions in `tla/cap_revocation/CapRevocation.tla`:
`RevokeBegin` (`/= {}`), `RevokeEnd` (`= {}`), `Retype` (`= {}`). The plan notes
it is equivalent to a one-level "no children" test and suggests replacing the
recursive call there, while keeping the genuine set in `RevokeStep`
(`\E l \in Descendants(c) : …`) and the `EventuallyRevoked` RHS.

The equivalence is exact and structural:
`Descendants(cap) == children \cup UNION {Descendants(c) : c \in children}` with
`children == {x \in CapIds : parent[x] = cap}`, so the closure is empty **iff**
the direct-children set is empty:

```
Descendants(c) = {}  <=>  {x \in CapIds : parent[x] = c} = {}  <=>  ~\E x \in CapIds : parent[x] = c
```

No appeal to any invariant is needed — it is a one-line set identity.

### Two candidate rewrites — and why the obvious one regresses

There are two equivalent ways to write the local test:

* **(a) the existential** the plan names — `HasChild(c) == \E x \in CapIds :
  parent[x] = c`, guards `HasChild(c)` / `~HasChild(c)`; and
* **(b) the children set** — `Children(c) == {x \in CapIds : parent[x] = c}`,
  guards `Children(c) /= {}` / `Children(c) = {}`.

Both are logically identical to the old guard. They are **not** identical to TLC:

| arm (cfg) | distinct | generated | gen:dist | diam | wall | verdict |
|---|---:|---:|---:|---:|---:|---|
| `CapRevocation` — baseline (`Descendants`) | 503,070 | **4,831,322** | 9.6 | 22 | 7m32s | No error |
| `CapRevocation` — (a) existential | 503,070 | **4,986,158** | 9.9 | 22 | 7m36s | No error |
| **`CapRevocation` — (b) children** | 503,070 | **4,831,322** | 9.6 | 22 | 7m38s | No error |
| `CapRevocation_Safety` — baseline | 1,240,344 | **13,194,241** | 10.6 | 28 | 4m10s | No error |
| `CapRevocation_Safety` — (a) existential | 1,240,344 | **13,603,471** | 11.0 | 28 | 4m13s | No error |
| **`CapRevocation_Safety` — (b) children** | 1,240,344 | **13,194,241** | 10.6 | 28 | 4m08s | No error |
| `CapRevocation_Teardown` (TSpec, no guards) | 132 | 919 | 7.0 | 8 | <1s | No error (all 3) |

Distinct and diameter are byte-identical across all three variants on every arm
(the harness's manifest assertion passes 3/3 each run), so **coverage is
provably and empirically unchanged** — the rewrite proves exactly the same
thing. The existential form (a) nonetheless raises *generated* states
deterministically by **+154,836 (+3.2%)** on the liveness arm and **+409,230
(+3.1%)** on the safety arm. The children form (b) is **byte-identical to
baseline** on generated too. Wall-clock is advisory and within run-to-run noise
across all three (the existential, which demonstrably does more work, is only a
few seconds slower in wall-clock; the deterministic signal is generated-states).

The per-action `-coverage` line localises the regression precisely. TLC reports
each action as `distinct:generated`; for `<Next …>` the cumulative totals were:

```
baseline      503069:4853388     (liveness)     1240343:13194240   (safety)
existential   503069:5008440     (liveness)     1240343:13603470   (safety)
children      503069:4853388     (liveness)     1240343:13194240   (safety)
```

i.e. under the existential the **distinct component is unchanged** (503069 /
1240343) and **only the generated component grows** — every extra state is a
*duplicate* successor, not a new one.

### Mechanism — a positive `\E` in an action guard branches in TLC

A one-guard isolation pins the cause. Rewriting **only** `RevokeBegin`
(`/= {}` → positive `HasChild(c)`) and leaving `RevokeEnd`/`Retype` on
`Descendants` reproduces the **entire** safety-arm regression — generated
13,603,471, identical to the full three-guard existential rewrite. So the two
negated guards (`~HasChild`) cost **zero**; the single positive existential
accounts for all of it.

The reason is how TLC generates successors. When it expands an action, TLC walks
the conjunction and, at a positive `\E x \in S : P(x)`, **enumerates every
witness `x`** and continues state generation down each branch where `P(x)`
holds — even when `P(x)` (`parent[x] = c`) constrains no primed variable. The
action body (`revoking' = revoking \cup {c}`) does not depend on `x`, so a root
`c` with *k* children yields *k* **identical** successor states, each counted in
*generated* but collapsing to one in *distinct*. The recursive
`Descendants(c) /= {}` and the set comparison `Children(c) /= {}` are
value-level set tests — TLC evaluates them to a single Boolean and generates one
successor. Under negation (`~HasChild`, i.e. `\A x : parent[x] /= c`) there is no
witness to branch on, which is why `RevokeEnd`/`Retype` were free. This is a
known TLC gotcha (a positive existential in an enabling guard multiplies
generated states); plan §1's "byte-identical generated" bar caught it exactly as
intended.

### The change (adopted form (b))

Pure spec; **one file, no cfg / manifest / script / CI edit** (no pinned count,
diameter, or verdict moves, so nothing in `tools/tla/model-manifest.tsv`
changes).

* **`tla/cap_revocation/CapRevocation.tla`** — extract `Children(c) == {x \in
  CapIds : parent[x] = c}`, redefine `Descendants` as its transitive closure
  (`LET ch == Children(cap) IN ch \cup UNION {Descendants(c) : c \in ch}`), and
  point the three guards at `Children(c) = {}` / `/= {}`. `Descendants` is kept
  verbatim where the genuine subtree is enumerated (`RevokeStep`,
  `RevokeStepBad`) and where "the subtree is empty" reads as the intended
  property (`EventuallyRevoked`, left as `Descendants(c) = {}`). The `Children`
  comment records the load-bearing reason the guard is a set comparison rather
  than the equivalent `\E` (the branching above) so the regression cannot be
  re-introduced by a well-meaning "simplification".

The only non-metric difference in the logs is cosmetic: the per-action
`-coverage` source-line label shifts (`<Next line 314 …>` → `<… 323 …>`,
`<TNext 627>` → `<636>`) because the new operator + comment move later
definitions down — line-number labels, not counts.

### Why it is semantics-preserving — good spec and bad specs

* **Good spec (`Next`, fully explored by `CapRevocation.cfg` and
  `CapRevocation_Safety.cfg`).** Byte-identical distinct **and** generated
  **and** diameter at `-workers 1 -fp 0` is the gold standard for a
  semantics-preserving rewrite; all three match baseline on both arms.
  `Descendants(c) = {} <=> Children(c) = {}` is now structurally evident in the
  spec (`Descendants` *is* the closure of `Children`).
* **Bad specs (`NextBad`, `NextNoGuard`, and the asym leak twins).** These exit
  on the first counterexample, so they have no full-exploration count to diff;
  their preservation is structural + behavioural. They reuse the same rewritten
  `RevokeBegin`/`RevokeEnd`/`Retype` through `CommonActions`, and
  `scripts/tla-neg-controls.sh` confirms all **nine** controls still trip on the
  same named invariant/property with the same exit codes — a verbatim diff of
  the before/after runs is empty.

### Validation

1. **SANY** parses `CapRevocation.tla` clean.
2. **Three baseline arms** byte-identical to baseline before/after (table
   above); the harness's manifest assertion (distinct + diameter vs
   `tools/tla/model-manifest.tsv`) passes 3/3.
3. **Nine negative controls** trip identically — verbatim diff of the
   `tla-neg-controls.sh` runs is empty:

   ```
   ok  CapRevocation_NegControl.cfg        LiveParent violated (exit 12)
   ok  CapRevocation_Safety_NegControl.cfg LiveParent violated (exit 12)
   ok  CapRevocation_AsymBug.cfg           DeadNowhere violated (exit 12)
   ok  CapRevocation_CapAsymBug.cfg        DeadNowhere violated (exit 12)
   ok  CapRevocation_NegLiveness.cfg       EventuallyRevoked violated (exit 13)
   ok  CommitProtocol_NegControl.cfg       RecoverReconstructs violated (exit 13)
   ok  IpcReactor_NegControl.cfg           NoLostWakeup violated (exit 12)
   ok  IpcReactor_NegBackpressure.cfg      NoLostWakeupWritable violated (exit 12)
   ok  IpcReactor_NegLostWakeup.cfg        NoLostWakeup violated (exit 12)
   ```

### Transient diagnostics (used, then removed — not committed)

Per the engagement policy, performance-diagnostic spec variants that were run and
discarded are recorded rather than committed:

* **The existential form (a)** — `HasChild == \E x \in CapIds : parent[x] = c`,
  guards `HasChild`/`~HasChild`. This is the plan's literal proposal and the
  measured regression above. It is **not** a committed negative control: it does
  not violate any invariant (it is an *equivalent* spec that merely costs TLC
  more), so it has no "teeth" to stand guard over. Its value — the demonstration
  that a positive `\E` guard branches — is captured by the adopted `Children`
  comment and by this report.
* **The one-guard isolation** (only `RevokeBegin` rewritten to `HasChild`) — the
  attribution run that pinned the regression to the single positive existential.
  A throwaway, not an artifact.

A negative control aimed at the equivalence itself (e.g. `HasChild` quantified
over `live` instead of `CapIds`) was considered and **not** added: dead caps have
`parent = NULL`, so the two ranges coincide in every reachable state — such a
"control" can never fail, has no teeth, and is therefore not useful. The nine
committed controls, which exercise the rewritten guards through `CommonActions`,
are the standing soundness check; **no new committed control is warranted.**

### Cost / CI judgement

Zero. No distinct count, diameter, verdict, cfg, manifest entry, negative
control, or CI arm changes; the `model`, `model-safety`, and neg-control steps
replay byte-for-byte. The benefit is local clarity (the guards now state the
one-level condition they actually need, and `Descendants` is visibly the closure
of `Children`) plus a regression guard in the comment.

### Decision

**Adopted as a clarity refactor, in the children-set form; the existential form
is rejected.** Byte-identical distinct / generated / diameter on all three arms
and identical verdicts on all nine negative controls (SANY clean), with **no
measurable performance change** — the hoped-for time-per-state win does not
materialise at `CapIds = 4` (the recursion is too shallow for the saving to clear
wall-clock noise, and generated-states, the deterministic proxy, is unchanged),
confirming the plan's own "likely marginal" prediction. The plan's specific
existential rewrite would have *regressed* generated-states by ~3.1%, so per
plan §1 and the engagement policy it is reverted; the equivalent branch-free
form is merged because it improves clarity at zero metric cost. Per the policy,
a null performance result that improves code clarity is merged with the lack of
speedup reported — which this is.

### Follow-ups (out of scope here)

* The set-emptiness-over-positive-`\E` guidance is general: any future enabling
  guard of the form `\E x \in S : <unprimed predicate>` over a model-value set
  should be written as a set comparison to avoid the same generated-state
  inflation. Worth a line in the eventual TLC guideline this plan seeds.
