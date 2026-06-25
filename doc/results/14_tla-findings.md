# 14 — TLA+ liveness optimization, Step 3: heap / `-fpmem` / worker tuning

Step 3 of `doc/plans/1_tla-liveness.md` — the last Tier-1 (sound,
behaviour-preserving) lever for the suite's one wall-clock pole, the `model` job's
`EventuallyRevoked` liveness check on `CapRevocation.cfg`. After Step 1
(`-lncheck final`) and Step 2 (invariant trim), the plan flags pure resourcing —
JVM heap (`-Xmx`), the fingerprint-set fraction (`-fpmem`), and worker count — as
"the lowest-expected-payoff Tier-1 lever," to be adopted **only on a measured win**.
This step measures all three cold and reaches a **documented null**: no resourcing
change helps the CI arm, so the `model`/`model-safety` jobs and the manifest are
left untouched. A negative result is data; this records it.

The prior already pointed here. Step 1 (`12`) measured the arm's peak heap at
**1490 MB** under the CI `-Xmx4g` — ~2.6 GB of headroom — so a heap raise has
nothing to bite on; the CI runner is `ubuntu-latest` (4 vCPU), which
`TLC_WORKERS=4` already saturates, and the final liveness SCC pass is sequential,
so `workers>4` is closed a priori for CI. The measurements below confirm each and
quantify why.

## Method

Cold runs (TLC scratch wiped before each), vendored `tla2tools.jar` matching its
SHA1, JDK 17 (Temurin 17.0.19), host Darwin arm64 (8 cores, 16 GB). Each run is the
**exact CI `model` invocation** for the liveness arm — `tlc2.TLC … -lncheck final`
on the committed (trimmed) `CapRevocation.cfg`, `-coverage` off so the wall is
clean — with `-Xlog:gc` for peak heap-used and `/usr/bin/time -l` for max RSS.
Six configs, each run **n=3 interleaved** across three rounds (round-robin, so any
thermal drift hits every config equally), serial (one TLC at a time — cold timing
needs a quiet machine):

| config | `-Xmx` | workers | `-fpmem` | role |
|---|---|---|---|---|
| `control` | 4g | 4 | (TLC default) | the current CI `model` arm |
| `heap8g` | **8g** | 4 | default | heap lever |
| `fpmem05` / `fpmem025` / `fpmem075` | 4g | 4 | **0.5 / 0.25 / 0.75** | fpmem lever (CI sets none) |
| `workers8` | 4g | **8** | default | worker lever (local-only; CI is 4 vCPU) |

`control` reproduces the post-Step-1+2 CI shape: its 102.6 s mean is ~10 s under
`13`'s default-`lncheck` after-number, i.e. the `-lncheck final` saving Step 1
measured — confirming the harness mirrors CI, not the baseline-script pinning
(`-fp 0 -fpmem 0.5`).

## Graph invariance (every run, all 18)

| | distinct | diameter | verdict |
|---|---|---|---|
| every config × every round | **503,070** | **22** | No error |

Heap, fpmem and worker count change only parallelism and resourcing, never the
next-state relation — so the explored graph, the manifest pins (`503070`, `22`),
and the `EventuallyRevoked` verdict are byte-identical across all 18 runs. Whatever
the wall numbers say, no lever here can prove less. (`TLC_WORKERS>1` makes
*generated* and any counterexample nondeterministic, so generated is not compared
across worker counts; distinct/diameter are worker-invariant and are.)

## Numbers (wall = mean of n=3, workers=4 unless noted)

| config | wall mean (range) | Δ vs control | peak heap | max RSS |
|---|---|---|---|---|
| `control` (4g) | **102.6 s** (100.9–103.5) | — | ~1480 MB | ~1820 MB |
| `fpmem05` (0.5) | 103.3 s (101.4–105.9) | +0.7 s | ~1500 MB | ~1800 MB |
| `fpmem025` (0.25) | 104.9 s (102.6–106.9) | +2.3 s | ~1500 MB | ~1790 MB |
| `fpmem075` (0.75) | 105.1 s (103.9–106.9) | +2.5 s | ~1570 MB | ~1910 MB |
| `heap8g` (8g) | **110.7 s** (99.8–116.7) | **+8.1 s** | ~2030 MB | ~2380 MB |
| `workers8` (8 wkr) | 87.2 s (82.0–95.0) | −15.4 s | ~1460 MB | ~1840 MB |

### Heap `-Xmx` 4g→8g — reject (measured regression, no win)

`heap8g` is **~8 s slower** on average (110.7 vs 102.6 s) with markedly higher
variance (99.8–116.7 s vs the control's tight 100.9–103.5 s) and ~550 MB more peak
heap (~2030 vs ~1480 MB) and RSS. At a ~1.5 GB live set, enlarging the G1 heap to
8 GB does not speed the arm — it delays collections but does not help the
generation/fingerprint/SCC work, and the larger heap measured *worse*. There is no
resourcing win to bank, and 4g already runs OOM-free with ~2.6 GB headroom (`12`),
so there is nothing to mitigate either. **Keep `-Xmx4g`.** The decision rule "keep
4g unless 8g is reproducibly faster at workers=4" is not met — the opposite held.

### `-fpmem` 0.25 / 0.5 / 0.75 — null (wash)

CI sets no `-fpmem`, so TLC auto-sizes the fingerprint set; the arm holds only
~503 K reachable states (a few MB of 64-bit fingerprints), so the fraction is never
the constraint. Pinning 0.5, 0.25 or 0.75 lands at +0.7 / +2.3 / +2.5 s — all
inside the control's own 2.6 s spread, none faster than the default. No value wins.
**Add no explicit `-fpmem` to CI.**

### Worker count — keep 4 (the runner's core count is the ceiling)

`workers8` is the only configuration that moves the wall (−15 %, 87.2 vs 102.6 s),
but that speedup is bought with **four cores CI does not have**: the `model` runner
is `ubuntu-latest` = 4 vCPU, and `TLC_WORKERS=4` already saturates it. Running 8
TLC worker threads on 4 vCPUs would oversubscribe, not accelerate. The result also
quantifies the ceiling: doubling cores buys only −15 %, not −50 %, because only the
state-generation phase parallelizes — the final liveness SCC pass is single-threaded
and fingerprinting is contended, an irreducible sequential tail. So the right worker
count is exactly the runner's core count, which CI already pins. **Keep
`TLC_WORKERS=4`**; raising it would help only on a larger runner, and even there
the sequential tail caps the gain.

## Negative controls / coverage

This step changes no `.tla`, `.cfg`, CI, or manifest file — it adopts no
configuration change — so coverage is unchanged by construction. The verification
gate was nonetheless re-run on this branch:

- `scripts/tla-baseline.sh CapRevocation` (workers=4) — manifest assertion holds:
  `distinct=503070`, `diameter=22`, `EventuallyRevoked` "No error found".
- `scripts/tla-neg-controls.sh` — all 12 negative controls still FAIL as designed.

`tools/tla/model-manifest.tsv` pins are untouched (graph byte-identical, 18/18).

## Verdict: null — keep `-Xmx4g`, no `-fpmem`, `TLC_WORKERS=4`

The Tier-1 resourcing levers yield nothing for the CI critical path: 8g is a
measured regression, `-fpmem` is a wash, and the only real speedup (`workers=8`)
needs cores the 4-vCPU runner lacks and is capped by the sequential SCC tail
regardless. As the plan anticipated ("lowest-expected-payoff … accept only measured
wins"), Step 3 adopts **no change** — the `model`/`model-safety` jobs stay at
`-Xmx4g` / `TLC_WORKERS=4`, the manifest is untouched, and the deterministic wins
banked by Steps 1–2 stand as the Tier-1 result. Tier 1 is now exhausted; the
remaining levers (Steps 4–5) are theorem-touching probes.
