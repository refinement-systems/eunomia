# 12 — TLA+ liveness optimization, Step 1: `-lncheck final`

Step 1 of `doc/plans/1_tla-liveness.md`. The guideline's "first liveness switch to
try", absent from CI. By default TLC checks the temporal properties *periodically*
as the reachable graph grows — Step 0 (`11`) saw 6 such intermediate SCC passes at
workers=1, 2 at workers=4 — plus one final pass over the complete tableau.
`-lncheck final` drops the intermediate passes and defers to that single final pass.
It is behaviour-preserving: it changes *when* liveness is checked, never *what* —
the same `EventuallyRevoked` verdict over the same 2,012,280-node tableau. This step
measures the wall-clock delta and peak heap, confirms no OOM at the CI `-Xmx4g`, and
establishes that the flag must be scoped to the liveness arm and kept off the
negative controls.

## Method

Cold runs (TLC scratch wiped first), vendored `tla2tools.jar` matching its SHA1,
JDK 17 (Temurin 17.0.19), host Darwin arm64. Each run via `tools/tla/tla-model-check.sh`
at `-fp 0 -fpmem 0.5`, with `-Xlog:gc` for peak heap. **`-coverage` is off** (no
instrumentation tax) so the wall is a clean default-vs-`final` delta — which is why
the default walls here run a little under `11`'s coverage-on baseline. A/B at the
CI-representative `TLC_WORKERS=4` (the `model` job pins 4) and the deterministic
`TLC_WORKERS=1`.

## Numbers (CapRevocation.cfg / EventuallyRevoked)

| pass | -lncheck | distinct | generated | diam | temporal passes | peak heap | verdict | wall |
|---|---|---|---|---|---|---|---|---|
| workers=4 | default | 503,070 | 4,831,322 | 22 | 2 curr + 1 final | 1461 MB | No error | 01min 57s |
| workers=4 | **final** | 503,070 | 4,831,322 | 22 | **0 curr + 1 final** | 1490 MB | No error | **01min 48s** |
| workers=1 | default | 503,070 | 4,831,322 | 22 | 6 curr + 1 final | 1443 MB | No error | 07min 07s |
| workers=1 | **final** | 503,070 | 4,831,322 | 22 | **0 curr + 1 final** | 785 MB | No error | **05min 57s** |

distinct (503,070), generated (4,831,322) and diameter (22) are **byte-identical**
with and without the flag at both worker counts — coverage intact, the manifest pins
untouched, a pure "do less scheduling work" change. The win is the eliminated
intermediate SCC passes (the generated count is identical, so no states are saved —
only the periodic passes over partial graphs):

- **workers=4 (CI):** −9s (01:57 → 01:48, ≈8%). Modest, as Step 0 predicted — only 2
  intermediate passes to cut.
- **workers=1:** −70s (07:07 → 05:57, ≈16%). Larger, the 6→0 pass collapse; this is
  the clean mechanistic demonstration (wall-clock is advisory and noisy, but the
  identical generated count isolates the delta to the dropped passes).

**Peak heap:** `final` retains the full graph before its one pass, but measured peak
is 1490 MB at workers=4 (vs 1461 MB default — within noise) and *lower* at workers=1
(785 MB vs 1443 MB, fewer partial-graph liveness materializations). Both sit far under
the 4096 MB cap; **OOM count 0** on every run. No heap concern at the CI `-Xmx4g`.

## Teeth re-check, and why the flag is scoped to the liveness arm

`CapRevocation_NegLiveness.cfg` (SpecNoGuard — derivation into a revoking subtree)
under `-lncheck final` at the neg-control runner's `-Xmx2g`: still reports
`EventuallyRevoked` VIOLATED (exit 13), peak heap 897 MB, no OOM. So deferring to one
final SCC pass does **not** blunt livelock detection — the deferred check still has
teeth.

But the same control exposes why `final` must **not** be set on the negative controls:

| NegLiveness | -lncheck | distinct explored | verdict | wall |
|---|---|---|---|---|
| default | periodic | 22,040 (early-exit) | VIOLATED | **00min 03s** |
| final | deferred | 503,070 (full graph) | VIOLATED | **05min 26s** |

On a *failing* spec, the periodic check finds the lasso after only 22,040 states and
stops in 3s; `final` forfeits that early-exit and must build the entire graph first —
5min 26s, a ~100× slowdown. A job-wide flag would inherit into `scripts/tla-neg-controls.sh`
and balloon this control, erasing the 9s pole win many times over. So the flag is
wired **per-invocation on the CapRevocation liveness check only**
(`.github/workflows/ci.yml`, `model` job), leaving the controls — and the other
model-job cfgs — at default. The other model-job cfgs are verdict-unchanged under the
flag anyway (Teardown has no temporal property → inert; CommitProtocol's
`RecoverReconstructs` is checked on-the-fly, no SCC pass; IpcReactor stays "No error").

## Verdict: keep, scoped

`-lncheck final` helps at CI's workers=4 (−9s, ≈8%) at zero coverage cost, zero heap
risk, and an unchanged verdict — the plan's "keep only if it helps at workers=4" bar
is met. Because the liveness arm is the suite's critical path (`model` runs longer
than the parallel `model-safety`), the 9s comes straight off the suite wall-clock.
Adopted as a per-invocation `TLC_FLAGS="-lncheck final"` on the CapRevocation liveness
model-check, deliberately **not** job-wide (the NegLiveness early-exit above).

## Negative controls / coverage

distinct/diameter unchanged → `tools/tla/model-manifest.tsv` pins are not touched. The
12 negative controls still FAIL as designed (re-run clean in Step 0 / `11`), and the
NegLiveness teeth-control was additionally confirmed to still trip under the flag here.
The controls themselves continue to run at default `-lncheck` in CI, by the scoping
decision above.
