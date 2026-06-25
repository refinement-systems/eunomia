# TLA+ liveness-arm optimization plan

Follow-up to `0_tla-optimization.md`. That effort optimized the **safety** arms
(the `CapRevocation_Safety.cfg` split + `SafetySymmetry`, `CommitProtocol`'s
`RefSymmetry`) but, by design, never touched the wall-clock pole — the `model`
CI job's `EventuallyRevoked` liveness check on `CapRevocation.cfg` (rev2§2.2
restartable revoke). Its dominant lever, **symmetry, is categorically unsound
under any temporal property** (`CapRevocation.tla` "Symmetry" comment; review
`8_tla-review.md`), so no liveness cfg can carry it. The independent review left
the liveness arm as "the sole remaining pole" with `-lncheck final` and heap
tuning flagged as untried behaviour-preserving levers.

This plan is the disciplined ladder of the liveness-specific optimizations that
*can* still apply, drawn from the roadmap in `tla-liveness-optimization.md`.

---

## §0  What we are optimizing, and the governing constraint

The pole is a single check:

| cfg / property | distinct | generated | gen:dist | diam | floor constants |
|---|---|---|---|---|---|
| `CapRevocation.cfg` / `EventuallyRevoked` | 503,070 | ~4,831,322 | ~9.6:1 | 22 | `Threads=1 QueueDepth=1`, 4 caps, 2 procs |

`EventuallyRevoked == \A c \in CapIds : (c \in revoking) ~> (Descendants(c) = {})`.
The floor constants are set by the liveness tableau heap; the safety arm restores
`Threads=2, QueueDepth=2` precisely because dropping the temporal property frees
it from that tableau.

`IpcReactor.cfg` / `EventuallyDelivered` is **out of scope**: 39 distinct,
diameter 13, sub-second. Nothing to win; documented as a null in `11`.

CI shape: the `model` job runs the liveness arm; `model-safety` runs the safety
arm in **parallel** (15-min cap). Suite wall-clock = max(arms), and the liveness
arm is the only critical path symmetry never reached.

**The governing line (from `0_tla-optimization.md` and the guideline):** separate
"make TLC do less work" from "change what theorem TLC checks". For liveness it is
very easy to do the latter by accident. Behaviour-preserving steps must leave
distinct / generated / diameter byte-identical and the verdict unchanged;
theorem-touching steps (fairness, abstraction) must keep the verdict, keep every
negative control tripping, be flagged as such, and be reverted if they only make
the property pass by weakening it.

The gen:dist ≈ 9.6:1 ratio is the open question: it suggests the arm may be
**state-generation-bound, not SCC-bound**. Step 0's `-coverage` profile decides
this and re-prioritizes the sound levers, so it runs first.

---

## §1  Measurement discipline (unchanged from `0_tla-optimization.md`)

- **Every step gets a findings doc** `doc/results/<n>_tla-findings.md`, numbering
  continued from the existing sequence (next free is **11**). Null and failed
  runs get a findings doc too — a negative result is data.
- **Re-derive the before-number freshly** on the merge-base of the step's work;
  there is no committed baseline (`target/tla-baseline/` is gitignored). A merge
  or rebase moves the base — re-establish the before-number on the merged tree.
- **Cold runs only** via `scripts/tla-baseline.sh` (it wipes each metadir, pins
  `-fp 0 -fpmem 0.5 -coverage 1`, verifies the vendored jar SHA1, asserts the
  manifest). Pin `TLC_WORKERS` and JDK 17 (Temurin).
- **Metric roles:** `distinct` is the coverage metric (a drop = proves less);
  `diameter` is the worker-invariant structural depth; both are asserted against
  `tools/tla/model-manifest.tsv`. `generated` is comparable only at equal workers
  (`TLC_WORKERS=1` is fully deterministic). Wall-clock is advisory and host/worker
  dependent — judge correctness by distinct/diameter/verdict, speed by generated
  (at fixed workers) and wall-clock as a sanity cross-check.
- **Manifest is the regression alarm:** any pinned-count change is justified in
  the findings doc and reflected in the manifest, never silently.
- **Negative controls** (`scripts/tla-neg-controls.sh`) must all still FAIL after
  every step. Any control discovered along the way is committed and wired into
  the runner + manifest.
- **Comment discipline (CLAUDE.md):** `.tla`/`.cfg` comments reference only
  `doc/spec`/`doc/guidelines`. Findings docs are cited in commit messages / PRs,
  never in spec comments. No `cargo fmt` applies to TLA.
- Execution is **serial, human-in-the-loop PRs** (one step ≈ one PR ≈ one
  findings doc), not a fan-out: cold TLC timing needs a quiet machine, so parallel
  TLC runs would contend for CPU and destroy determinism.

---

## §2  The ladder

### Tier 1 — sound, behaviour-preserving (graph byte-identical)

**Step 0 — Re-baseline + profile → `11_tla-findings.md`.**
Cold `scripts/tla-baseline.sh CapRevocation IpcReactor` at `TLC_WORKERS=1`
(deterministic distinct/generated/diameter + per-action `-coverage`) plus a
`TLC_WORKERS=4` wall-clock pass that mirrors CI. Record the table above as the
authoritative before-number and answer the central question: **is wall-clock
dominated by state generation (which hot action?) or by the sequential
SCC/liveness pass?** This decides the order of Steps 1–3. Also record the
IpcReactor null (Step 6).

**Step 1 — `-lncheck final` → `12_tla-findings.md`.**
The guideline's "first liveness switch to try", absent from CI. By default TLC
runs liveness checks periodically as the state count grows; `final` defers to one
SCC pass over the complete graph. Wire via `TLC_FLAGS` on the `model` job (or
default it for liveness cfgs in `tla-model-check.sh`). Behaviour-preserving:
distinct / diameter / `EventuallyRevoked` verdict unchanged. Measure the
wall-clock delta **and peak heap** — `final` retains the full graph before
checking; confirm no OOM in the CI 4g (`-Xmx4g`). Caveat: if Step 0 shows the arm
is generation-bound, the win may be small; keep only if measured. **Teeth
re-check:** `CapRevocation_NegLiveness.cfg` must STILL livelock under the flag (a
deferred check that still has teeth).

**Step 2 — Trim redundant invariant checks from the liveness cfg → `13_tla-findings.md`.**
`CapRevocation.cfg` checks 6 invariants + `ReportMonotone` + `EventuallyRevoked`.
The **same** invariants + `ReportMonotone` are checked by `model-safety` at
strictly larger constants (`Threads=2, QueueDepth=2`) under `SafetySymmetry`,
whose reachable set embeds the floor via the idle-thread / shallow-queue
projection (every `Threads=1, QueueDepth=1` behaviour is the special case of the
safety arm where `t1` never acts and the ring stays depth-1). So the floor's
invariant obligations are a **subset** — redundant. Drop the expensive ones
(MoveSemantics, DeadNowhere, FireSafe, LiveParent, RevokedDead, ReportMonotone)
from the liveness cfg; keep `TypeOK` as the cheap sanity floor. This removes
per-state predicate evaluation while leaving generated / distinct / diameter
**byte-identical** and the `EventuallyRevoked` verdict unchanged — a clean
critical-path win **iff** Step 0 shows invariant evaluation is a meaningful
fraction of per-state cost (MoveSemantics/DeadNowhere compute `Cardinality` over
residence sets, the likely candidates). Document the subsumption argument in the
cfg header and the manifest; the safety arm's committed controls keep guarding
those invariants, so suite-level coverage is preserved.

**Step 3 — Heap / `-fpmem` / worker tuning → `14_tla-findings.md`.**
Pure resourcing: `-Xmx` (4g→8g), `-fpmem`, and worker count > 4. Distinct /
diameter / verdict invariant. The liveness SCC pass is sequential and the CI
runner is 4 vCPU, so more workers can only help the generation phase up to the
core count — document the ceiling. Accept only measured wins; this is the
lowest-expected-payoff Tier-1 lever.

### Tier 2 — theorem-touching probes (likely null → harvest a negative control)

**Step 4 — Fairness reformulation probe → `15_tla-findings.md`.**
Test whether weaker fairness still proves `EventuallyRevoked`: replace
`Fairness == \A c \in CapIds : WF_crVars(RevokeStep(c))` (4 per-cap conditions)
with a single `WF_crVars(\E c \in CapIds : RevokeStep(c))`. If it holds → a
stronger theorem (less fairness assumed) and a smaller temporal tableau. **The
likely outcome is a livelock** — a single existential WF only forces *some*
`RevokeStep` infinitely often and can starve one cap's subtree forever. On that
null: REVERT, and **commit a new negative control `CapRevocation_NegFairness.cfg`**
(single-existential WF livelocks `EventuallyRevoked`), proving the per-cap
fairness is load-bearing — a teeth-test the suite currently lacks — wired into
`scripts/tla-neg-controls.sh` and the manifest. Flag explicitly as
theorem-touching; never keep a weakening that only makes the check pass.

**Step 5 — `revoked` ghost-variable abstraction probe → `16_tla-findings.md`.**
`revoked` is a monotonically-growing ghost (rev2§2.2 reuse discipline) whose only
consumer is the `RevokedDead` invariant. The guideline flags such history
variables as graph-multipliers (TLC compares states by all variable values).
Determine whether at the floor it is reachably-redundant (functionally determined
by `live` + history → contributes no extra distinct states, so removal is free
but yields nothing) or genuinely splits states (removal shrinks the graph but
**alters what `RevokedDead` can express** — a coverage-bearing abstraction that
needs the obligation re-expressed as an operator/action property plus a fresh
control). Most likely a documented **reject/null**: abstraction is a proof
obligation, not a casual speed knob. Record either way.

### Tier 3 — synthesis

**Step 6 — IpcReactor out-of-scope null** (folded into `11`): 39 states /
sub-second; record why no optimization is warranted so the coverage decision is
on the record.

**Step 7 — Synthesis / adversarial review → `17_tla-review.md`** (mirroring
`8_tla-review.md`): re-run the full suite cold, confirm every manifest pin, that
every negative control still FAILS, that `EventuallyRevoked` + `EventuallyDelivered`
verdicts hold, and report the honest critical-path delta (max of `model` and
`model-safety`) against the Step 0 baseline. Independent soundness pass on any
theorem-touching change adopted in Steps 4/5.

---

## §3  Critical files

- `tla/cap_revocation/CapRevocation.tla` — `Fairness`, `EventuallyRevoked`, the
  invariant block, the `revoked` variable, `Spec`.
- `tla/cap_revocation/CapRevocation.cfg` — the liveness cfg (invariant + property
  list edited in Step 2; constants are the floor, not to be touched).
- `.github/workflows/ci.yml` — `model` job (liveness arm + neg-controls) and
  `model-safety` job (the parallel safety arm).
- `tools/tla/tla-model-check.sh` — `TLC_FLAGS` / `TLA_JAVA_OPTS` passthrough (the
  hook for `-lncheck final` and heap tuning).
- `tools/tla/model-manifest.tsv` — coverage pins (assertion targets).
- `scripts/tla-baseline.sh` — the cold A/B harness.
- `scripts/tla-neg-controls.sh` — the negative-control runner.
- New: `doc/results/11..17`; possibly `tla/cap_revocation/CapRevocation_NegFairness.cfg`.

## §4  Verification (per step and final)

- After each step: `scripts/tla-baseline.sh` (manifest assertions hold),
  `scripts/tla-neg-controls.sh` (all controls FAIL as designed), and the
  `EventuallyRevoked` / `EventuallyDelivered` verdicts read "No error found".
- Final: a cold full-suite run; CI green on both `model` and `model-safety`;
  critical-path wall-clock = max(arms), reported honestly against Step 0.
