# 13 — TLA+ liveness optimization, Step 2: trim redundant invariants from the liveness cfg

Step 2 of `doc/plans/1_tla-liveness.md`. The `model` job's heap-bound liveness arm
(`CapRevocation.cfg` / `EventuallyRevoked`) re-checked six invariants
(`TypeOK, MoveSemantics, DeadNowhere, LiveParent, FireSafe, RevokedDead`) plus the
`ReportMonotone` action property on every one of its ~4.8M generated states. Step 0
(`11`) fingered `MoveSemantics` — three `Cardinality` calls over residence sets per
live cap — as the hottest per-state predicate. Every one of those obligations except
`EventuallyRevoked` is **already** checked on the parallel `model-safety` arm
(`CapRevocation_Safety.cfg`) at strictly larger constants. This step drops the five
redundant invariants + `ReportMonotone` from the liveness cfg, keeping `TypeOK` (the
cheap well-typedness floor) and `EventuallyRevoked`, removing per-state predicate
work while leaving the state graph byte-identical and the verdict unchanged.

## Subsumption — why the drop is sound

The trimmed obligations are not dropped from the suite, only from this arm:

- `CapRevocation_Safety.cfg` checks `TypeOK, MoveSemantics, DeadNowhere, LiveParent,
  FireSafe, RevokedDead` **and** `PROPERTY ReportMonotone` at `Threads={t0,t1},
  QueueDepth=2` under `SYMMETRY SafetySymmetry`. Its reachable set is a strict
  superset of the liveness floor (`Threads=1, QueueDepth=1`): this floor is the
  special case where the second thread never acts and the channel ring stays depth-1.
  So every floor state's invariant obligation is a special case of one the safety arm
  already discharges.
- `ReportMonotone` is a `[][...]_vars` safety action-property (rev2§5.1) — no
  fairness, no liveness tableau — so it is symmetry-safe (`FireSafe` and
  `ReportMonotone` are `\A t \in Threads` universals, invariant under
  `ThreadSymmetry`; see `CapRevocation.tla`), and rides the safety arm soundly. The
  symmetry-unsound class is *liveness* properties; `EventuallyRevoked` therefore stays
  on this un-quotiented arm alone.
- `TypeOK` is retained as the cheap structural sanity floor, so a malformed-state spec
  bug cannot make `EventuallyRevoked` pass vacuously.

The negative controls confirm the dropped obligations remain actively guarded
elsewhere (see below) — this is added safety coverage on the safety arm, not lost
coverage on the liveness arm.

## Method

Cold runs (TLC scratch wiped first), vendored `tla2tools.jar` matching its SHA1, JDK
17 (Temurin 17.0.19), host Darwin arm64. Two instruments, mirroring Step 1:

- **coverage-ON** via `scripts/tla-baseline.sh CapRevocation` (`-fp 0 -fpmem 0.5
  -coverage 1`, workers=1) — asserts `distinct`/`diameter` against the manifest and
  exposes the per-invariant evaluation cost.
- **coverage-OFF** direct `tools/tla/tla-model-check.sh` (`-fp 0 -fpmem 0.5`, `-Xmx4g`)
  for a clean wall, at the CI-representative `TLC_WORKERS=4` and the deterministic
  `TLC_WORKERS=1`. The workers=4 arm was additionally run as a B/A interleave
  (before/after/before/… so any thermal drift hits both arms equally), the pre-edit
  cfg reconstructed from `git HEAD`.

This A/B holds `-lncheck` at its default on *both* arms, isolating Step 2's effect
alone; the combined Step 1 (`-lncheck final`) + Step 2 critical-path number is the
Step 7 synthesis, not measured here.

## Numbers (CapRevocation.cfg / EventuallyRevoked)

| | distinct | generated | diam | verdict |
|---|---|---|---|---|
| before (6 invariants + ReportMonotone) | 503,070 | 4,831,322 | 22 | No error |
| after (TypeOK + EventuallyRevoked) | 503,070 | 4,831,322 | 22 | No error |

`distinct` / `generated` / `diameter` are **byte-identical** — invariant evaluation
does not change the next-state relation, so the explored graph, the manifest pins
(`503070`, `22`), and the `EventuallyRevoked` verdict are all untouched. A pure
"do less per-state work" change.

Cold walls (coverage-off):

| workers | before | after | delta |
|---|---|---|---|
| 4 (CI) | ~115.6 s mean (113–118 s, n=5) | ~112.2 s mean (110–115 s, n=5) | **≈ −3–4 s (≈3%)** |
| 1 | 07min 19s (439 s) | 06min 31s (391 s) | **−48 s (≈11%)** |

The three interleaved workers=4 B/A pairs gave per-pair deltas of 4 / 2 / 5 s — a
small but consistent reduction (the trimmed arm was faster in every pairing). The
workers=4 win is smaller than the workers=1 win because the eliminated per-state
evaluation is parallelized across the four workers: its ≈48 s of single-core cost
compresses to a few seconds of wall at workers=4, while the sequential phases
(fingerprinting, the final liveness SCC pass) are unaffected. So the modest CI-scale
wall delta is the *expected* 4-worker image of the clear single-worker delta, not noise.

## Eliminated per-state work (the deterministic signal)

Wall is advisory; the worker-invariant evidence is TLC's `-coverage` evaluation
counts over the fixed graph. Before, the final coverage block reports (peak
sub-expression cost in parentheses):

- **`MoveSemantics`** — `\A c \in live` body evaluated **1,893,453×**; its residence-set
  comprehensions (`ProcPlaces`/`QueuePlaces`/`BindPlaces` under `Cardinality`) cost up
  to **7,573,812** operations. The dominant per-state cost, as Step 0 predicted.
- **`DeadNowhere`** — per-dead-cap body **118,827×** (cost up to 475,308).
- **`LiveParent`** — **1,893,453** parent-membership checks.
- **`FireSafe`** — **1,006,140** slot-liveness checks.
- **`RevokedDead`** — **503,070** single set-intersection emptiness checks.
- **`ReportMonotone`** — `[][...]_vars` action property, checked per transition.

After the trim, the final coverage block contains **only `TypeOK`** (the five
invariants above are absent); `TypeOK`'s own checks (cheap structural type
constraints, peak 4,024,560) are all that remain. Every counted evaluation above is
eliminated from the liveness arm.

## Negative controls / coverage preservation

`scripts/tla-neg-controls.sh`: **all 12 controls still FAIL as designed**. The four
controls whose target invariants were dropped from this arm continue to trip on their
own cfgs — `CapRevocation_NegControl` (LiveParent), `CapRevocation_AsymBug` /
`CapRevocation_CapAsymBug` (DeadNowhere), `CapRevocation_ThreadAsymBug` (FireSafe),
`CapRevocation_ReportMonotoneBad` (ReportMonotone) — the runnable proof those
obligations remain guarded suite-wide. `CapRevocation_NegLiveness` still livelocks
`EventuallyRevoked`, so the property kept on this arm still has teeth. `distinct` /
`diameter` unchanged → `tools/tla/model-manifest.tsv` pins are not touched (the
manifest gains a comment recording the intentional invariant-subset).

## Verdict: keep

The trim removes genuinely redundant per-state work (the counted `MoveSemantics`
residence-set construction and the other four invariants + `ReportMonotone`, all
subsumed by the safety arm) at **zero coverage cost** — graph byte-identical, verdict
unchanged, every negative control still tripping, manifest pins intact. It helps the
CI workers=4 critical path by a small but consistent ≈3% (≈3–4 s) and the
single-worker wall by ≈11%, and it is also a hygiene win: the liveness arm now states
exactly its one unique obligation (`EventuallyRevoked`) plus the cheap well-typedness
floor, with the safety suite explicitly the home of the rest. Adopted.
