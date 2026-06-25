# TLA+ liveness-optimization effort — independent review (2)

*Intermediate working document (doc/results). An adversarial review of the
TLC-optimization effort landed since `7059556` ("doc: independent review of the
TLA+ optimization effort (8_tla-review.md)"), i.e. the *liveness*-arm round that
followed the *safety*-arm round `8_tla-review.md` judged. This is Step 7 of
`doc/plans/1_tla-liveness.md`. Per the project's comment discipline it is
temporary, will be removed, and must not be referenced from code, specs, or
guidelines.*

Scope: every change in `git diff 7059556..HEAD` touching `tla/`, `tools/tla/`,
`scripts/tla-*.sh`, and `.github/workflows/ci.yml` — i.e.

* three **prior-review follow-ups**: #212 (CommitProtocol `Refs=3` + the
  comment-discipline fixes `8_tla-review.md` §4 asked for), #213
  (`Permutations(Threads)` into `SafetySymmetry`), #214 (the `ReportMonotone`
  teeth-test);
* the **five liveness Steps** of the plan: #215 (`-lncheck final`), #216 (trim
  redundant invariants from the liveness cfg), #217 (heap/`-fpmem`/worker tuning
  — null), #218 (fairness-reformulation probe — null + a harvested control), #219
  (`revoked` ghost-variable probe — reject).

**Method.** This is not a re-read of the findings docs (`9…16`); they are the
thing under review. The load-bearing facts were re-derived independently:

* **TLC was re-run cold and locally** on the whole committed suite — vendored
  `tla2tools.jar` (matches its `.sha1`), Temurin 17.0.19, `-Xmx4g`,
  `-workers 4`, Apple-Silicon host — via the committed harness
  (`scripts/tla-baseline.sh`, `scripts/tla-neg-controls.sh`). Every manifest pin
  and every negative control was reproduced (§0).
* The full `CapRevocation.tla` / `CapRevocation_Safety.cfg` /
  `CommitProtocol.tla` were read by hand to check the subsumption and symmetry
  premises against the spec source, not the prose.
* A fan-out of five independent reader agents — one per dimension (Step 2
  subsumption; `-lncheck`+controls; the three follow-ups; the probes + the
  critical-path accounting; comment-discipline/clarity/plan) — each followed by
  an **adversary** tasked with *refuting* its soundness verdict by re-deriving
  from `.tla`/`.cfg`/spec source. Two adversaries reproduced the headline counts
  with their own live TLC runs.

---

## Bottom line

| Question | Verdict |
|---|---|
| **1. Sound? Did optimizing break anything?** | **Yes, sound — nothing important was broken.** Every change is either behaviour-preserving (graph byte-identical) or correctly handled as theorem-touching (the two probes were *reverted*, not adopted). All five manifest pins reproduce to the digit; all **13** negative controls trip as designed; both liveness verdicts hold; **no liveness arm carries `SYMMETRY`**. The one structural risk — Step 2 dropping five invariants from the liveness cfg — is sound by a subsumption argument that holds on inspection, with one *caveat* (it is argued, not mechanized, and the empirical floor-containment check the prior round relied on is now stale). |
| **2. Did model-checker performance / clarity improve?** | **Mixed, and honestly documented.** The *targeted pole* (the liveness arm) got ~12 % faster (`-lncheck final` −9 s + the invariant trim −3–4 s @ 4 workers); `model-safety` got ~24 % faster as a sound 1.95× quotient. **But the critical-path `model` job net got *slower*** (~128 s → ~256 s locally), because the same window *added* a ~131 s `NegFairness` liveness control and a 120× `CommitProtocol` coverage bump to that serial job. As in the prior round, the net is a *coverage* gain financed by compute, not a speedup — and the findings docs say so (one stale budget sentence excepted). Clarity is improved (the liveness cfg now states exactly its one unique obligation). |
| **3. Follow-ups?** | The prior review's follow-ups #2/#3/#4 are **closed** (this effort *is* them); #1 is partly closed (Refs=3 landed, CapIds=5 measured out-of-budget — *correcting* the prior "near-zero cost" claim); **#5 — seed the TLC guideline — is still open and is now the top item**, the plan having deferred it until the liveness work finished. New: arm the manifest coverage guard *in CI*; mechanize the Step 2 floor-containment; close the MoveSemantics/RevokedDead teeth-gap; keep the ~131 s control off the critical path. |

No change in this window weakens a verified property. The single most important
soundness fact — TLC symmetry is unsound under liveness — was respected
everywhere: the new `NegFairness` cfg (a *liveness* control) correctly carries no
`SYMMETRY`, and grep confirms no liveness arm does.

---

## 0. Independent re-verification (cold, local)

`scripts/tla-baseline.sh` (the script *fails* on any distinct/diameter drift) and
`scripts/tla-neg-controls.sh` both exited 0:

| arm | distinct (= manifest pin) | diameter | verdict |
|---|---:|---:|---|
| `CapRevocation.cfg` (liveness) | 503,070 ✓ | 22 ✓ | `EventuallyRevoked` — No error |
| `CapRevocation_Safety.cfg` | 635,034 ✓ | 28 ✓ | invariants + `ReportMonotone` — No error |
| `CapRevocation_Teardown.cfg` | 132 ✓ | 8 ✓ | No error |
| `CommitProtocol.cfg` | 413,455 ✓ | 29 ✓ | No error |
| `IpcReactor.cfg` | 39 ✓ | 13 ✓ | `EventuallyDelivered` — No error |

**All 13 negative controls failed as designed**, including the three this effort
added — `CapRevocation_ThreadAsymBug → FireSafe` (exit 12),
`CapRevocation_ReportMonotoneBad → ReportMonotone` (exit 13, under
`SafetySymmetry`), `CapRevocation_NegFairness → EventuallyRevoked` (exit 13). The
coverage ledger is honest and the controls are armed.

---

## 1. Soundness

### 1.1 Step 1 — `-lncheck final` is behaviour-preserving and correctly scoped (#215)

The flag defers the `EventuallyRevoked` check from TLC's *periodic* intermediate
SCC passes to a single final pass over the complete tableau. It changes *when*
liveness is checked, never *what*: distinct/generated/diameter are byte-identical
and the verdict unchanged (reproduced). The scoping is the load-bearing detail
and it is right: the flag is applied **per-invocation on the liveness arm only**
(`ci.yml:67`), not on the teardown/CommitProtocol/IpcReactor checks
(`ci.yml:68-71`) and **not** on `scripts/tla-neg-controls.sh` (a separate step at
default `-lncheck`). The reason is sound and documented — on a *failing* spec
`final` forfeits the periodic early-exit, ballooning the `NegLiveness`/`NegFairness`
livelock detection ~100×; a job-wide flag would have erased the pole win many
times over. Sound. (One robustness note: `-lncheck` is *not* listed in TLC's
`-help` — it is an internal strategy selector, so its semantics are pinned to the
vendored `tla2tools.jar`. That is an argument *for* the jar pin, and worth a
one-line CI/manifest note so a future jar-bump reviewer re-confirms the flag's
behaviour rather than assuming it.)

### 1.2 Step 2 — trimming five invariants from the liveness cfg is sound, with one caveat (#216)

The highest-structural-risk change: `CapRevocation.cfg` now checks only `TypeOK`
+ `EventuallyRevoked`, dropping `MoveSemantics`, `DeadNowhere`, `LiveParent`,
`FireSafe`, `RevokedDead`, and the `ReportMonotone` action property. The
subsumption argument checks out against the spec source:

* **Each dropped obligation is genuinely re-homed**, not lost: all six are
  listed on `CapRevocation_Safety.cfg` (`:37-47`), checked at the strictly larger
  residence constants `Threads=2, QueueDepth=2`. Nothing the liveness arm dropped
  is now checked nowhere; `EventuallyRevoked` (which symmetry forbids on the
  safety arm) correctly stays unique to the liveness arm.
* **The embedding is exact.** The dropped invariants are uniform universals that
  *degenerate cleanly* on the idle second thread / shallow queue: `MoveSemantics`/
  `DeadNowhere` sum `BindPlaces` over `Threads × BindKinds`, so on a floor state
  embedded into `Threads={t0,t1}` with `t1`'s slots `NULL` the count is identical;
  `FireSafe`/`ReportMonotone` are `\A t \in Threads` with the `t1` conjunct
  trivially satisfied; `LiveParent`/`RevokedDead` do not mention `Threads` at all
  (`CapRevocation.tla:367-399`). Raising `Threads`/`QueueDepth` only *adds*
  behaviours — it never restricts `t0`'s — and TLC explores the `t1`-idle,
  depth-≤1 projection, so every floor behaviour is a safety-arm behaviour.
  Symmetry does not disturb this: invariant checking on the quotient is sound, so
  an invariant verified on the orbit representative holds on the embedded floor
  state too. `TypeOK` is retained so a malformed-state bug cannot make
  `EventuallyRevoked` pass vacuously.

**Caveat (minor, not a defect).** The "strict superset" is an *argument*, with no
standing check. The prior round (`8_tla-review.md` §1.3) leaned on an *empirical*
floor row — the safety arm *at the liveness floor* reproducing 503,070/22 — but
the safety arm now runs only at `Threads=2`, so that containment is reproduced
nowhere. If a future edit ever made the safety arm's reachable set *not* a
superset of the floor (e.g. an action newly gated on `Threads`/`QueueDepth`), a
floor obligation could silently go unchecked and nothing would catch it. Cheap to
close (follow-up 3).

### 1.3 Step 3 — resourcing null is correct discipline (#217)

`-Xmx 8g` measured a reproducible *regression* (+8 s, higher variance, +550 MB
peak), `-fpmem` was a wash, and `workers=8` needs four cores the 4-vCPU CI runner
lacks (and is capped by the sequential SCC tail regardless). The step adopted
**no change** — `ci.yml` and the manifest are untouched. Exactly the "accept only
measured wins" the plan prescribed; a negative result correctly recorded as data.

### 1.4 Steps 4 & 5 — the theorem-touching probes were *reverted*, not adopted (#218, #219)

This is the discipline that matters most for liveness, and it held. The real
spec is **byte-identical** base→HEAD across the load-bearing region
(`CapRevocation.tla:1-437`: `Spec`@343, `Fairness`@341, `Next`@327,
`EventuallyRevoked`@409, and every invariant body — all unchanged):

* **Step 4 (fairness reformulation).** Weakening per-cap `WF` to a single
  existential `WF` does *not* prove `EventuallyRevoked` — it livelocks at
  `CapIds=5`. The probe correctly **rejected** the weakening and instead
  harvested `CapRevocation_NegFairness.cfg` (the suite's first fairness
  teeth-test). The control is faithful: `SpecExistFair` reuses the real `Next`
  verbatim and differs from `Spec` *only* in the fairness conjunct
  (`\A`→`\E`), so its livelock isolates the fairness weakening, not a planted
  bug. The 5-cap threshold is sound on the spec: the `Copy` derive-guard
  (`~AncestorOrSelfRevoking`, `:190`) means a revoking subtree only shrinks, so a
  starvation lasso needs *two* disjoint revoking subtrees under a non-revoking
  common root — five live caps minimum, unreachable at the 4-cap floor. The
  control trips with exit 13 (a genuine temporal violation, not a deadlock —
  `CHECK_DEADLOCK FALSE`), reproduced.
* **Step 5 (`revoked` ghost abstraction).** Freezing `revoked` shrinks the graph
  503,070 → 466,512 (−7.27 %) — a *theorem-touching* change by construction —
  and the variable is load-bearing for the safety arm and three symmetry
  controls, so it correctly **left no committed artifact**:
  `CapRevocation_NoRevoked.tla` does not exist and #219 changed only
  `doc/results/16`. Reject recorded with the precise multiplier on the record.

### 1.5 Follow-ups #212–#214 are sound

* **#213 `Permutations(Threads)`** (group order 48→96; safety arm 1,240,344 →
  635,034, a 1.95× near-ideal quotient, reproduced live by the adversary):
  `Threads` is genuinely interchangeable — no `CHOOSE` over `Threads`, no
  hard-coded `t0`/`t1`, `Init` seeds all threads identically, every action
  quantifies uniformly. The subtle point the prior review flagged is correctly
  re-justified: its old argument ("`Threads` is not permuted") is void now that
  `Threads` *is* permuted, but `FireSafe` and the `ReportMonotone` action
  property are `\A t \in Threads` universals naming no specific thread, hence
  invariant under any thread permutation — the quotient stays sound for both
  (`CapRevocation.tla:446-459`). A per-axis asymmetric control
  (`ThreadAsymBug → FireSafe`) was added.
* **#214 `ReportMonotone` teeth-test** closes `8_tla-review.md` finding #7. A
  *symmetric* control (the right kind for a symmetric property) violating
  `ReportMonotone`, run under `SafetySymmetry`; it trips (exit 13), reproduced.
* **#212 `Refs=3`** (CommitProtocol 3,444 → 413,455 distinct, diameter 21 → 29):
  sound. `RefSymmetry == Permutations(Refs)` is valid at S₃ — there is **no
  `CHOOSE` over `Refs`** in the real `Spec` (the only `CHOOSE r \in Refs` is in
  the asymmetric `SpecAsymBad` control, where asymmetry is intended;
  `CommitProtocol.tla:344`). The 6.0× quotient (2,480,585 full / 413,455) was
  reproduced live; the diameter increase is the expected deepening from a third
  ref, not a regression; both controls still trip `RecoverReconstructs` at
  `Refs=3`. (One discipline gap — no findings doc — under §3.)

### 1.6 The negative-control suite grew 10 → 13, all armed

`ThreadAsymBug` (Threads-axis asymmetric guard), `ReportMonotoneBad` (the
symmetric action-property guard), and `NegFairness` (the first fairness
teeth-test) were added and all trip. The methodology the prior round praised —
one symmetric + one asymmetric control per permuted axis, and a livelock control
per liveness property — is now *complete* for every permuted axis except the
two-invariant gap in §3.

---

## 2. Performance and clarity — honest accounting

**The liveness *arm* (the intended pole) did get faster**, soundly: ~118 s →
~104 s @ 4 workers (`-lncheck final` −9 s, invariant trim −3–4 s; Step 3 null),
graph byte-identical, verdict unchanged. And `model-safety` dropped 63 s → 48 s
as the 1.95× `Permutations(Threads)` quotient.

**But the critical-path `model` job got *slower*.** CI runs two parallel jobs and
the suite wall-clock is `max(model, model-safety)`. The `model` job is a *serial*
sequence: liveness arm + Teardown + CommitProtocol + IpcReactor + the 13 negative
controls (`ci.yml:67-73`). The same window that shaved ~14 s off the arm *added*
to that serial job:

* `CapRevocation_NegFairness` — a *liveness* control that, run at the controls'
  `workers=1 -Xmx2g`, takes **~131 s** to reach its lasso (it must explore ~596 K
  distinct states before a periodic check sees the cycle), and
* `CommitProtocol` at `Refs=3` — ~7 s coverage-on locally (was ~1 s).

Netting it out (local, 4-worker):

| | `model` job | `model-safety` job | critical path = max |
|---|---:|---:|---:|
| base `7059556` | ~128 s | ~63 s | **~128 s** |
| HEAD | ~256 s | ~48 s | **~256 s** |

So the suite critical path roughly **doubled**, dominated by the one ~131 s
serial liveness control — which more than erases the ~14 s arm win. This is *not*
a regression in any sound sense: it is more coverage checked (a fairness
teeth-test the suite lacked; a 120× larger commit-protocol space), well inside
the 15-min cap (~10 min headroom). But any reading of the effort as "the model
checker got faster end-to-end" is wrong — only the isolated liveness arm did.

**Honesty of the docs.** The findings docs (`9…16`) are scrupulous about this:
they localize wins to the arm and label every quotient/control as *coverage, not
critical-path speedup* (`9` §"Honest framing"; `10`; `13` §Verdict), and `15`
explicitly costs the `NegFairness` control at "2m11s" under a "Cost (honest
accounting)" header. The **one slip**: `15`'s budget sentence states "the full
negative-control suite runs in ~8 s today" *in the same PR that adds a ~131 s
control to that suite* — internally stale arithmetic (the headline "~5 min"
survives only because the 131 s hides in an un-itemized "+ controls"). Minor, and
in a temporary doc, but it should be corrected.

**Clarity** improved on net: the liveness cfg now states exactly its one unique
obligation plus a cheap floor, with the subsumption rationale in the cfg header
the model-checker actually reads; the manifest ledger documents the quotient
arithmetic; the inline rule statements replaced the forbidden plan citations the
prior review flagged. Minor regression: the subsumption rationale is now
*triplicated* near-verbatim across `CapRevocation.cfg`, `model-manifest.tsv`, and
`ci.yml`.

---

## 3. Comment discipline (CLAUDE.md)

**Upheld, and the prior round's defects are fixed.** `git grep` over `tla/`,
`tools/tla/`, `scripts/tla-*.sh`, `ci.yml` finds **no** `doc/plans`/`doc/results`
citation, no `plan §`, and no `A7`/phase marker — the four violations
`8_tla-review.md` §4 listed (`tla-baseline.sh:10,52`, `tla-neg-controls.sh:8`,
`model-manifest.tsv:6`) are gone, and both stale CI comments (the `~12.2M/~5 min`
`model-safety` line and the Toolbox-probing line) now describe reality. The
large new comment blocks added this round (the `CapRevocation.cfg` header, the
control-rationale blocks, the three new cfgs, the `-lncheck` comment) introduce
**no** new violation: they describe what *is*, carry only `rev2§…` refs, and the
"…is NOT re-checked here, the safety arm checks it" phrasing documents the current
division of obligations (with the rationale for a genuinely surprising
configuration), not a deletion. `spec_rev2.md` is unedited. (Pre-existing house
nit, not introduced here: `ci.yml` still has bare `§3.3`/`§5.1`-style refs that
want the `rev2§` prefix.)

**One discipline gap:** **#212 has no findings doc.** It bundles the single
largest coverage change of the window (CommitProtocol +120×) *and* the
load-bearing CapIds=5 out-of-budget measurement into a PR documented only by its
commit message — against the plan §1 rule "every step gets a findings doc." Every
liveness Step (11→0 … 16→5) got one; this follow-up did not.

---

## Findings summary (prioritized)

| # | severity | finding |
|---|---|---|
| 1 | — (confirm) | **Sound.** All 5 pins reproduce to the digit; all 13 controls trip; both liveness verdicts hold; no liveness arm carries `SYMMETRY`. Steps 4/5 probes correctly *reverted* (real spec byte-identical). #212/#213/#214 quotients sound (no `CHOOSE` over the permuted set; reproduced live). |
| 2 | major | **Critical-path `model` job ~doubled** (~128 s → ~256 s), dominated by the ~131 s serial `NegFairness` liveness control. The liveness *arm* got ~12 % faster, but the *suite* did not — it bought coverage with compute (as the prior round did). Honest in the docs except the one stale `15` budget sentence. |
| 3 | minor | **Step 2 subsumption is argued-only.** The empirical floor-containment check the prior round used is now stale (safety arm no longer runs at the floor); a future edit could silently drop a floor obligation with nothing to catch it. |
| 4 | minor | **#212 has no findings doc** — the largest coverage change + the CapIds=5 budget finding live only in a commit message (plan §1 violation). |
| 5 | minor | **Manifest coverage pins are not CI-gated.** CI runs `tla-model-check.sh` + `tla-neg-controls.sh` but never `tla-baseline.sh`, so a silent coverage *shrink* with an unchanged verdict passes CI. The "regression alarm" is armed only for local runs. (Pre-existing; the effort leans heavily on the re-pinned numbers.) |
| 6 | minor | **`MoveSemantics` and `RevokedDead` have no negative control** anywhere — checked but with no committed teeth-test (the analogue of the `ReportMonotone` gap #214 just closed). |
| 7 | info | Doc-level nits in the temporary findings: `15`'s "~8 s suite" budget sentence is internally stale, and `15`'s lasso narration (`:91-97`) uses illustrative cap labels that do not match a literal TLC trace. Subsumption rationale triplicated across cfg/manifest/ci. Safety arm's `SPECIFICATION Spec` carries an inert `WF` conjunct (sound, and the cfg already says so at `:14-15`). `-lncheck` is an internal, `-help`-absent TLC flag, so its behaviour rides the jar pin. |

## 4. Follow-ups

**Prior review (`8_tla-review.md` §3) status.** #2 (`Permutations(Threads)`),
#3 (`ReportMonotone` gap), #4 (push the liveness arm) — **closed; this effort is
them**. #1 (spend the symmetry budget) — **partly closed**: `Refs=3` landed;
`CapIds=5` was measured >25 min local / blows the 15-min cap, *correcting* the
prior review's "near-zero cost" estimate (and recorded in memory). #5 — **open**,
below.

1. **Seed the TLC guideline into `doc/guidelines/` (top priority; the plan's
   explicit deferral).** The liveness work is now done, which is the condition the
   compilation waited on. Distil the durable rules the two rounds established so
   they outlive the temporary `doc/results/0…17` + `doc/plans` (which can then be
   removed): TLC symmetry is unsound under any temporal property (no `SYMMETRY` on
   a liveness arm); one symmetric **and** one asymmetric control per permuted
   axis; set-emptiness over `\E` in enabling guards (B6); `-lncheck final` scoped
   per-invocation, never job-wide (the control early-exit); judge correctness by
   distinct/diameter/verdict and speed by generated-at-fixed-workers; per-cap
   fairness is load-bearing (the `NegFairness` finding). This is the analogue of
   `verus_trusted-base.md` for the TLC suite.
2. **Arm the manifest coverage guard in CI** (finding #5): run
   `scripts/tla-baseline.sh` (or fold its distinct/diameter assertion into the
   `model` job) so a coverage shrink fails CI, not just a local run.
3. **Give Step 2's floor-containment a standing check** (finding #3): a
   `CapRevocation_SafetyFloor.cfg` running the six safety invariants at the
   liveness floor (`Threads=1, QueueDepth=1`) — sub-second — mechanizes "the
   safety arm subsumes the floor," or run it periodically.
4. **Close the `MoveSemantics`/`RevokedDead` teeth-gap** (finding #6): a committed
   negative control violating each, so every invariant checked under a quotient
   has a runnable teeth-test (the discipline the effort otherwise completed).
5. **Keep the ~131 s `NegFairness` control off the critical path** (finding #2):
   it is the suite's new pole. Options — run the expensive liveness controls in a
   separate parallel job, or at a higher worker count — but any change must
   preserve the periodic early-exit the controls rely on (do **not** set
   `-lncheck final` on them).
6. **Document #212 retroactively** (finding #4) and **fix `15`'s doc-level nits**
   (the stale "~8 s" budget sentence and the illustrative lasso labels); add a
   one-line note that `-lncheck final` is an internal TLC flag pinned to the jar;
   optionally collapse the triplicated subsumption rationale and sweep `ci.yml`'s
   pre-existing bare-`§` refs to `rev2§`.

## Conclusion

The liveness round is sound and honestly documented. It did the disciplined thing
on the one part of the suite where it is dangerously easy not to: every
behaviour-preserving lever (`-lncheck final`, the invariant trim) left the graph
byte-identical, the one negative resourcing result was recorded as a null, and
both theorem-touching probes — fairness reformulation and ghost abstraction — were
*rejected* rather than adopted, with the fairness probe harvested into the suite's
first fairness teeth-test. The symmetry follow-ups are sound quotients with
standing controls. The user's likely suspicion — that "optimization" again means
"more compute, not less" — is well-founded as a *framing* point: the targeted
liveness arm is ~12 % faster, but the critical-path `model` job roughly doubled
because the effort spent the headroom (and more) on a ~131 s fairness control and
a 120× commit-protocol coverage bump. That is a coverage win, not a speedup, and
the findings docs call it that (one stale budget line excepted). The actionable
items are not soundness fixes — they are a deferred-but-now-due guideline
compilation, a CI gap (the coverage pins are unarmed in CI), and three
cheap teeth-/containment-test closures.
