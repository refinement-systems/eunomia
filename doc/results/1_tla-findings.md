# TLA+ / TLC optimization findings — B2

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines. (B1's outcome is in `0_tla-findings.md`.)*

All measurements below are **cold** (TLC scratch wiped first), vendored
`tools/tla/tla2tools.jar` (matches its `.sha1`), Temurin 17, host Darwin arm64,
`-fp 0 -fpmem 0.5`. distinct-states and diameter are **worker-invariant**, so
the coverage numbers and verdicts are independent of `-workers`; only the
advisory wall-clock depends on it. Runs are at `-workers 4` (the CI count)
unless noted. The `scripts/tla-baseline.sh` runs additionally pass `-coverage 1`
(per-action attribution), which adds instrumentation overhead — called out where
it matters for the wall-clock.

---

## B2 — split the safety invariants into `CapRevocation_Safety.cfg`, run at larger constants

**Status: adopted — a real coverage win.** A new safety-only arm checks the
six safety invariants + `ReportMonotone` at **24.2× the state space** of the
liveness arm (12,183,480 vs 503,070 distinct), with **no measurable total-CI
wall-clock regression** (it runs as a separate, parallel CI job). Unlike B1 this
is not a null result: it is the coverage play the plan describes, and it lands
the prerequisite that makes symmetry (B3/B4) usable.

### The change

Pure cfg + tooling; **no `.tla` edit**.

* **New `tla/cap_revocation/CapRevocation_Safety.cfg`** — `SPECIFICATION Spec`
  (the same `Init`/`Next` as `CapRevocation.cfg`), `CHECK_DEADLOCK FALSE`, the
  six safety invariants (`TypeOK`, `MoveSemantics`, `DeadNowhere`, `LiveParent`,
  `FireSafe`, `RevokedDead`) and `PROPERTY ReportMonotone`. The
  `EventuallyRevoked` liveness property is **omitted** — that is the whole point.
  `Spec`'s `WF(RevokeStep)` fairness is simply unused by TLC when no liveness
  property is checked, so `Spec` is reused as-is (no fairness-free `SafetySpec`
  needed).
* **`tools/tla/model-manifest.tsv`** — a `CapRevocation_Safety` row pins the
  exact `expected_distinct` (12,183,480) and `expected_diameter` (28); the
  harness asserts strict equality, so the arm's coverage is now guarded against
  silent shrinkage. The canonical-constants comment block documents the safety
  arm's constants as deliberately larger and **not** part of the floor lock-step
  set.
* **`.github/workflows/ci.yml`** — a new `model-safety` job (mirrors `model`'s
  env + jar-pin verify) runs only this cfg. It is a **separate job**, so it runs
  in parallel with `model`: total model-checking wall-clock is `max(liveness arm,
  safety arm)`, not their sum. A `timeout-minutes: 15` guard catches a runaway.

### Why this is sound coverage, not a re-run

The liveness arm trims `Threads 2→1` and `QueueDepth 2→1` **specifically to fit
the `EventuallyRevoked` tableau in heap** (the cfg header and the manifest say
so). Dropping the liveness property frees the safety arm to restore exactly
those two residence axes. The floor's reachable behaviours embed in the larger
arm (every floor state is the `t1`-idle / second-queue-slot-unused projection of
a safety-arm state), and the invariants are universally quantified over the
`Threads`/queue structure — so the safety arm's reachable set is a **strict
superset** of the liveness arm's, checked against the *same* invariants. No
safety coverage is lost; 24.2× more is added (a second TCB's
bind/exit/fault interleavings against a mid-revoke CDT, and a depth-2 channel
ring carrying in-flight caps).

The liveness arm `CapRevocation.cfg` is **not touched** (byte-identical), so
`EventuallyRevoked`'s verdict is unchanged — the change is strictly additive.

### Constants selection (the "adopt-if-measured" gate)

Candidate configs, cold, `-workers 4`, all six invariants + `ReportMonotone`
(`Δ` is from the liveness floor `CapIds=4 Procs=2 Channels=1 Threads=1
QueueDepth=1`):

| constants | distinct | × floor | diam | generated | wall (4w) | verdict |
|---|---:|---:|---:|---:|---:|---|
| floor (same) — *sanity* | 503,070 | 1.00× | 22 | 4,831,322 | 12 s | No error |
| `QueueDepth=2` | 1,232,298 | 2.45× | 27 | 11,949,685 | 29 s | No error |
| `Threads=2` | 5,708,340 | 11.35× | 23 | 64,217,224 | 2m08 s | No error |
| **`Threads=2, QueueDepth=2`** (adopted) | **12,183,480** | **24.22×** | **28** | **138,167,803** | **~5 min** | **No error** |

* The **sanity** row is load-bearing: the safety arm at the floor reproduces the
  liveness arm's reachable space **exactly** (503,070 distinct, diameter 22, same
  4,831,322 generated) — proof that it is a sound projection and not a different
  model.
* `QueueDepth` and `Threads` **compound**: 2.45× × ~10× ≈ 24× — the two axes
  exercise largely independent residence combinatorics.
* `CapIds` was **not** bumped. Deeper CDT breadth is a separate axis whose large
  factors need a sound symmetry quotient to stay in budget (a follow-up); keeping
  `CapIds=4` makes the adopted arm's cost predictable.

**Decision rule applied:** pick the largest distinct-count config that fits the
CI budget with every invariant passing. `Threads=2, QueueDepth=2` is the full
restore of the liveness-trimmed axes (≈ the stepwise model's complete reachable
space) and finishes well inside budget, so it is the pick.

### Cost / CI wall-clock judgement

* The adopted arm at `-workers 4` is **~5 min** without coverage instrumentation;
  the `scripts/tla-baseline.sh` validation run (which adds `-coverage 1`) was
  **6 min 34 s**. The CI `model-safety` job runs without `-coverage`, so ~5 min
  is the representative figure. Both are comfortably under the job's
  `timeout-minutes: 15` and under the existing CI poles (`verus` ~up to 20 min,
  `on-os` QEMU boot).
* Because `model-safety` is a **separate parallel job**, total CI wall-clock is
  unchanged (it is gated by the pre-existing poles, not by this arm). This is the
  plan's "wall-clock ≈ max, not sum" requirement, met.
* **Memory:** 12.18M distinct completed under `-Xmx4g` with no OOM — the safety
  arm escapes the heap wall that caps the liveness arm, because it builds **no**
  liveness tableau (the tableau, not the reachable enumeration, was the
  >40M-state heap killer).
* **Honest framing:** this is a **coverage** play, not a critical-path speedup.
  The liveness arm remains the wall-clock pole of the `model` job and gains
  nothing here; B2 *adds* an arm rather than speeding an existing one.

### Correctness / regression checks

* **Sound projection:** floor-constants safety run = 503,070 / diam 22 / 4,831,322
  generated — identical to the liveness arm (above).
* **Invariants:** "No error has been found" at the adopted constants — all six
  safety invariants + `ReportMonotone` hold across the full 12.18M-state space.
* **Coverage guard armed:** `scripts/tla-baseline.sh CapRevocation_Safety` is
  green and asserts distinct = 12,183,480, diameter = 28.
* **No regression on the existing arms:** the cold before-baseline reproduced
  `CapRevocation` 503,070/22, `Teardown` 252/8, `CommitProtocol` 6,886/21,
  `IpcReactor` 39/13 — all matching the manifest.
* **Negative controls intact:** all six still fail as designed (B2 touches no
  spec body):

```
ok  CapRevocation_NegControl.cfg     LiveParent violated as expected (exit 12)
ok  CapRevocation_NegLiveness.cfg    EventuallyRevoked violated as expected (13)
ok  CommitProtocol_NegControl.cfg    RecoverReconstructs violated as expected (13)
ok  IpcReactor_NegControl.cfg        NoLostWakeup violated as expected (12)
ok  IpcReactor_NegBackpressure.cfg   NoLostWakeupWritable violated as expected (12)
ok  IpcReactor_NegLostWakeup.cfg     NoLostWakeup violated as expected (12)
```

* **SANY** parses `CapRevocation.tla` clean.

### Decision

**Adopted.** A 24.2× broadening of the safety state space at no total-CI
wall-clock cost (parallel job), every invariant passing, the coverage assertion
armed in the manifest, and the negative controls untouched. The plan tagged B2
*adopt-if-measured*; the measurement supports adopting.

### Follow-ups (out of scope here)

- **B3** (`SYMMETRY Permutations(Procs)` — and, now that a second TCB is in the
  model, potentially `Permutations(Threads)` — on this safety arm). Symmetry is
  unsound under a liveness property, so it can only live on an invariant-only cfg
  like this one; B2 is its prerequisite. **A7's negative controls must guard any
  symmetry** (TLC never validates one itself); a symmetric negative control would
  need adding before B3 lands.
- **B4** (`SYMMETRY Permutations(CapIds)`), which would let the safety arm raise
  `CapIds` past 4 for deeper-CDT coverage inside budget.
- **Tooling note:** the single-worker `scripts/tla-baseline.sh` default makes a
  full cold baseline of this 12.18M-state arm slow (~20 min serial). distinct and
  diameter are worker-invariant, so its coverage assertion is valid at any worker
  count — run `TLC_WORKERS=4 scripts/tla-baseline.sh CapRevocation_Safety` to
  re-derive it quickly.
- **D1** hygiene is still pending (stray `*_TTrace_*` scratch in `tla/`); note
  that a **relative** `TLC_METADIR` resolves under the spec dir (the runner
  `cd`s there), so always pass an absolute scratch path or let the runner default
  it to repo-root `target/`.
