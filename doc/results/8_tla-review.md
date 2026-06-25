# TLA+ optimization effort — independent review

*Intermediate working document (doc/results). An adversarial review of the
TLC-optimization effort landed since `a717f0f` ("TLA optimization resources").
Per the project's comment discipline it is temporary, will be removed, and must
not be referenced from code, specs, or guidelines.*

Scope: every change in `git diff a717f0f..HEAD` touching `tla/`, `tools/tla/`,
`scripts/tla-*.sh`, and `.github/workflows/ci.yml` — i.e. PRs #202–#210 (B1, B2,
B3, B3-followup, B4, B5, B6, C1, Tier-D) plus the Tier-A substrate. The four
review questions are answered against `doc/spec/spec_rev2.md`, the Rust in
`kcore`/`cas`/`ipc`, and the implementers' own findings (`doc/results/0…7`).

**Method.** The review is not a re-read of the findings docs. It re-derived the
load-bearing facts independently: (a) the full `CapRevocation.tla` /
`CommitProtocol.tla` were read to check the symmetry premise by hand; (b) **TLC
was re-run locally** (vendored `tla2tools.jar`, matches its `.sha1`; Temurin 17;
`-Xmx4g`; Apple-Silicon host) on every committed arm to reproduce the pinned
counts and to measure the real `-workers` speedup; (c) a fan-out of independent
reader agents plus two adversaries (tasked with *refuting* the symmetry
soundness and the speedup framing) cross-checked the conclusions; (d) the
comment-discipline audit is a `git grep` over the committed tree, not a sample.

---

## Bottom line

| Question | Verdict |
|---|---|
| **1. Sound? Coverage kept, still proves what it should?** | **Yes.** The three symmetry families are sound quotients; the three rewrites (B1/B5/B6) are semantics-preserving (byte-identical counts, reproduced locally); coverage is *strictly grown*, never shrunk. |
| **2. Do the clarification/simplification claims hold?** | **Largely yes**, and stated with unusual honesty (B1/B6 are recorded as *null* perf results; the plan's own two miscounts were caught and corrected by the findings). |
| **3. Total speedup? Honest?** | The **only critical-path speedup is `-workers` on the liveness arm — measured 3.6× (427 s → 118 s)**, a sound resourcing change. The per-PR wall-clock drop (~10 → ~4 min) is honest *as parallelization*; the "combined 10 → 6 min" framing is **misleading** (it sums two parallel jobs and hides that total compute *rose* ~78%). The implementers' own docs never make that claim — they call the symmetry work "coverage, not speedup" throughout. |
| **4. Comment discipline upheld?** | **Mostly, with a real recurring slip:** four committed tooling files cite `doc/plans`/`doc/results`/"plan §…" (forbidden); two CI comments are stale. The `.tla`/`.cfg`/`tla-model-check.sh` comments are clean. |

No change in this window weakens a verified property. The single most important
soundness fact — that TLC symmetry is unsound under liveness — was respected
everywhere: **no liveness cfg carries `SYMMETRY`** (verified by grep, below).

---

## 1. Soundness

### 1.1 Symmetry — the highest-risk change, and it is sound

Three symmetry families were declared, all on **invariant-only** arms:
`SafetySymmetry == Permutations(Procs) \cup Permutations(CapIds)` on
`CapRevocation_Safety.cfg`; `NotifSymmetry == Permutations(Notifs)` on the
teardown arm; `RefSymmetry == Permutations(Refs)` on `CommitProtocol.cfg`. TLC
never validates a declared symmetry, so soundness rests on two things — the
model values being genuinely interchangeable, and the standing negative
controls. Both check out:

* **The `CHOOSE` trap is avoided.** `InitProc == CHOOSE p \in Procs` and
  `InitCap == CHOOSE c \in CapIds` are the textbook unsound-symmetry hazard (an
  asymmetric choice over a symmetric set). Verified by grep that both appear
  **only inside `Init`** (`CapRevocation.tla:164-170`) — never in any action or
  invariant the real `Spec`'s `Next` reaches. Their only other uses are in the
  negative-control bad-specs (`LeakRevokedAsym`, `LeakRevokedCapAsym`,
  `AsymCap`), which are *meant* to be asymmetric. Because the seeds touch only
  `Init`, the orbit argument holds: the orbit of the init state under the group,
  plus a fully symmetric `Next`, yields the same reachable orbit-set a symmetric
  `Init` would — TLC canonicalises every initial state too. Sound.
* **The premise is real.** All seven safety invariants
  (`TypeOK`/`MoveSemantics`/`DeadNowhere`/`LiveParent`/`FireSafe`/`RevokedDead`/
  `ReportMonotone`, `CapRevocation.tla:347-399`) and every action quantify
  uniformly over `Procs`/`CapIds`; `parent : CapIds -> CapIds` carries a CDT
  forest to an isomorphic one under a cap permutation. Likewise `Refs` in
  `CommitProtocol.tla` (no `CHOOSE` over `Refs` at all in the real spec).
* **The guards have teeth.** The methodology is genuinely rigorous: for each
  axis there is both a *symmetric* control (a bug present in every orbit member —
  catches a quotient that drops a whole orbit) **and** an *asymmetric* control (a
  bug that singles out one model value — catches an *over-broad* quotient that
  merges an asymmetric violating state with a non-violating one). The cap axis
  and proc axis each get their own asymmetric control
  (`CapRevocation_CapAsymBug` / `CapRevocation_AsymBug`), both run under the full
  `SafetySymmetry`; the commit arm gets `CommitProtocol_AsymBug`. All are wired
  into `scripts/tla-neg-controls.sh` and gated by CI. Locally, **all 10 controls
  fail as designed** (re-run, 8 s).
* **Arithmetic is correct.** Burnside accounting reproduces: B4's 9.82× total
  (group order 48, large fixed-point mass from "rarely all four caps live at
  once"), C1's near-ideal 1.9994× (group order 2, only 2 fixed states). The
  pinned post-quotient counts reproduced locally to the digit (see §3).

**One subtlety worth a follow-up (minor, not a defect).** Both the safety arm
and `CommitProtocol.cfg` check a *safety action property* (`[][...]_vars`:
`ReportMonotone`, `RecoverReconstructs`) **under symmetry**. TLC's symmetry is
soundly defined for state invariants; an action property is sound only if it is
itself symmetric — which both are (`ReportMonotone` ranges only over `Threads`,
which is *not* permuted; `RecoverReconstructs` ranges uniformly over `Refs`).
The implementers correctly flagged this (plan C1) and guard
`RecoverReconstructs` with a dedicated asymmetric control. **`ReportMonotone` on
the safety arm has no analogous negative control** — no committed control
violates it at all, with or without symmetry. It is sound by inspection, but the
"prove every symmetric property has teeth under the quotient" discipline the
effort otherwise applies is incomplete for this one property. Cheap to close
(a `ReportMonotone`-violating bad-spec at the safety constants).

### 1.2 The three rewrites are semantics-preserving

Each met the strict bar (byte-identical *distinct and generated and diameter*),
reproduced locally:

* **B1** — `Send` quantifies `cs \in SUBSET cspaces[p]` instead of
  `SUBSET CapIds` (`CapRevocation.tla:316`). Since `cspaces[p] \subseteq CapIds`
  always and the body keeps `cs \subseteq cspaces[p]`, the enabling set is
  *identical*; counts unchanged (503,070 / 4,831,322).
* **B5** — `CommonActions` factors the 8 shared disjuncts out of
  `Next`/`NextBad`/`NextNoGuard`; each relation is now `(one of Copy/CopyNoGuard)
  \/ (one of RevokeStep/RevokeStepBad) \/ CommonActions \/ UNCHANGED tdVars`
  (`CapRevocation.tla:315-331,455-487`). Logically identical to the pre-`a717f0f`
  ten-disjunct relations; all five arms byte-identical.
* **B6** — guard `Descendants(c)={}` → `Children(c)={}` on
  `RevokeBegin`/`RevokeEnd`/`Retype` only; `Descendants` (the genuine closure) is
  *kept* in `RevokeStep`/`RevokeStepBad` and in `EventuallyRevoked`. The identity
  is exact (`Descendants` is defined *as* the closure of `Children`, so empty iff
  `Children={}`). Counts byte-identical.

### 1.3 Coverage was grown, not shrunk — conformance preserved

The model still abstracts the same Rust mechanisms (no action/invariant/property
*body* changed except the equivalent B1/B6 rewrites). The B2 safety arm is a
**strict superset**: at the liveness floor it reproduces the liveness arm
*exactly* (503,070 / diameter 22 — the sanity row), then restores `Threads=2`,
`QueueDepth=2` (the TCB `bind_slots:[CapSlot;2]` second thread and the depth-2
channel ring of rev2§3.3/§5.1) for 24.2× more reachable states checked against
the same invariants. Dropping `EventuallyRevoked` there is harmless — it is
still checked, byte-unchanged, by `CapRevocation.cfg`. The §3 guardrails hold in
every committed cfg (grepped): `CapIds=4` floor intact, `Refs={r1,r2}`,
`MaxWrites=2`, and — the load-bearing one — **no `SYMMETRY` on any liveness cfg**:

```
$ grep -rl SYMMETRY tla/ | while read f; do
    grep -qE 'PROPERTY (EventuallyRevoked|EventuallyDelivered)' "$f" && echo "BAD: $f"; done
  (empty — no liveness arm carries symmetry)
```

*Caveat recorded:* the "strict superset" property is argued + empirically
sanity-checked (the floor row), not mechanized as a containment predicate. That
is the right level of rigor for this work, but it is an argument, not a proof.

---

## 2. Do the clarification / simplification claims hold?

Yes, and the engagement was scrupulous about *not* overclaiming:

* **B1 and B6 are recorded as null performance results** and adopted on
  readability grounds only (`0_tla-findings.md`, `5_tla-findings.md`). B6 went
  further and *rejected* the plan's own literal proposal (the `\E x : parent[x]=c`
  existential) after measuring it as a deterministic +3.1% generated-states
  regression — a positive existential in an enabling guard makes TLC branch per
  witness. Keeping the recursion out of the guards via a set-emptiness test
  (`Children`) realises the intent at zero cost, and the `Children` comment
  records *why* so the regression can't be reintroduced. This is exactly the
  "measure every change, correctness first" discipline working as intended.
* **B5's `CommonActions`** is a real maintainability win: it makes the
  negative-control lock-step *structural* (a control is only a proof a guard has
  teeth if it is otherwise identical to `Next`). The findings even caught two
  errors in the plan's own prose (the plan said "9 common disjuncts" and
  "`CommonActions \/ <one variant>`"; there are **8** shared and **two** varying
  positions) and implemented the correct form. Reviewer-confirmed against the
  spec.
* **The Tier-A substrate genuinely clarifies**: `model-manifest.tsv` is a real
  coverage ledger (the TLC analogue of the Verus trusted-base), `tla-baseline.sh`
  asserts distinct *and* diameter against it (so a coverage drop fails the
  script), and `tla-neg-controls.sh` makes the previously-unrun negative controls
  a standing CI gate. The script logic is sound (verified: exit codes propagate;
  `exit 0` from a control is treated as failure).

The simplification claims are not inflated. Where a change did nothing for speed,
the docs say so plainly.

---

## 3. Total speedup, and is it honest?

This is the question the effort's framing most needs scrutiny on. **Measured
locally**, every committed arm (CI flags: `-workers 4 -Xmx4g`, no `-coverage`):

| arm | 1 worker | 4 workers | distinct (= manifest pin) |
|---|---:|---:|---:|
| `CapRevocation.cfg` (liveness) | **427 s** | **118 s** | 503,070 ✓ |
| `CapRevocation_Safety.cfg` | — | **63 s** | 1,240,344 ✓ |
| `CapRevocation_Teardown.cfg` | — | <1 s | 132 ✓ |
| `CommitProtocol.cfg` | — | ~1 s | 3,444 ✓ |
| `IpcReactor.cfg` | — | <1 s | 39 ✓ |
| `scripts/tla-neg-controls.sh` (×10) | 8 s | — | all fail ✓ |

Two facts fall out immediately:

1. **Every manifest pin reproduces to the digit** — the coverage ledger is
   honest and the regression guard is correctly armed.
2. **The one real critical-path speedup is `-workers` on the liveness arm:
   427 s → 118 s = 3.6×** (Tier-A A1). This is a sound, behaviour-preserving
   resourcing change (the state graph and every verdict are identical; only
   exploration is parallelized). The plan's headline "472 s → 104 s (4.5×) at 8
   workers" is consistent with this sublinear scaling — the liveness SCC/tableau
   work is sequential, so 4 workers buys 3.6×, not 4×. (Note: the 8-worker
   number is *not* re-derived anywhere post-effort and CI uses 4, so treat 3.6×
   at the CI worker count as the load-bearing figure.)

### The honest accounting (and where "10 → 6" misleads)

Reconstructing the CI shape with the measured numbers:

* **Before** (`a717f0f`): one `model` job, **single-threaded** (no `-workers`).
  Liveness arm dominates at 427 s locally (slower on CI's `ubuntu-latest` cores —
  the user's "~10 min"). Compute ≈ **430 core-seconds** on 1 core.
* **After**: two **parallel** jobs.
  `model` (4 workers): liveness 118 s + small models + 10 neg-controls 8 s ≈
  **128 s** of TLC ≈ the user's "~4 min" CI job. `model-safety` (4 workers):
  **63 s** ≈ the user's "~2 min" CI job. Compute ≈ 128·4 + 63·4 ≈ **764
  core-seconds** on 4 cores.

So:

* **Per-PR wall-clock dropped ~10 → ~4 min (≈2.5×).** This is *honest* and
  comes from (a) `-workers 4` parallelizing the liveness arm (3.6× measured) and
  (b) the jar-pin/`-Xmx` resourcing — all sound, behaviour-preserving Tier-A.
* **The "combined TLA+ time went 10 → 6" framing is misleading.** "4 + 2 = 6" is
  the *sum* of two **parallel** jobs — no PR ever waits 6 min (it waits
  `max(model, model-safety) ≈ the model job`). And it is not a compute number
  either: **total CI compute went *up* ~78%** (430 → ~764 core-seconds), because
  the effort *added* the `model-safety` arm (1.24M states) and 10 gating
  negative controls that did not exist before. You cannot read "10 → 6" as "we
  made the work 40% faster"; the work got *bigger and broader*, and parallelism
  hid the added latency.
* **None of the symmetry/coverage work touches the critical path.** The
  `model` job's pole is the `EventuallyRevoked` liveness tableau, on which
  symmetry is unsound and therefore absent. B2/B3/B4/C1 are coverage headroom on
  a parallel arm. **The implementers state this explicitly and repeatedly** —
  every one of `1_…`/`2_…`/`3_…`/`6_tla-findings.md` says "coverage/headroom
  play, not critical-path speedup" and "total CI wall-clock unchanged, gated by
  the pre-existing poles." The "10 → 6" reading is *not* a claim the effort
  makes; the findings actively refute it.

**Net, stated honestly:** *the effort cut per-PR wall-clock ~2.5× by soundly
parallelizing the liveness arm and splitting CI into two parallel jobs, and in
the same move spent ~78% more total compute to add a 24×-larger safety state
space, ten gating negative controls, and 2–10× sound symmetry quotients — i.e.
it bought broad new coverage while keeping latency down, not a speedup of the
existing work.* That is a genuinely good outcome; it is just not the "made TLA+
40% faster" sentence the "10 → 6" phrasing invites.

### Possible follow-ups (assuming, as verified, nothing is broken)

1. **Spend the symmetry headroom (the explicit deferred payoff).** B4 banked a
   9.82× quotient and C1 a 2× quotient *without* raising constants. The safety
   arm now runs in 63 s, leaving the 15-min CI budget almost untouched — raise
   `CapIds` to 5–6 (deeper CDT breadth) and `Refs` to 3 (broader partial-commit
   coverage), re-pinning the manifest. This is strict coverage gain at near-zero
   wall-clock cost and is the entire point of the quotients.
2. **Add `Permutations(Threads)` to the safety arm.** B2 introduced a second
   size-2 permutable set (`Threads={t0,t1}`); folding it into `SafetySymmetry`
   compounds the quotient further (carried over as a B3/B4 follow-up).
3. **Close the `ReportMonotone`-under-symmetry guard gap** (§1.1): a
   `ReportMonotone`-violating bad-spec at the safety constants, so every
   symmetric property checked under a quotient has a standing teeth-test.
4. **Push liveness-arm wall-clock further** (the only critical path): it is the
   sole remaining pole. `-lncheck final` and `-fpmem`/heap tuning are the
   behaviour-preserving levers (`tla-liveness-optimization.md`); a higher CI
   worker count helps state generation but not the sequential SCC pass. Measure
   before adopting — the gen:dist 9.6:1 ratio suggests generation, not SCC, may
   still dominate at these constants.
5. **Seed the TLC guideline** the plan anticipates (the set-emptiness-over-`\E`
   rule from B6; the symmetry-validation recipe from B3/B4/C1) into
   `doc/guidelines/`, so it outlives the temporary findings docs.

---

## 4. Comment discipline (CLAUDE.md)

Rule: committed comments describe *what is*, may reference **only** `doc/spec`
and `doc/guidelines` (with a revision number), and carry **no** phase markers,
deletion notices, or `doc/plans`/`doc/results` citations.

**Mostly upheld — the `.tla`/`.cfg` work is exemplary.** The symmetry-rationale
blocks (`CapRevocation.tla:412-447`, `CommitProtocol.tla:289-305`), the cfg
headers, `tla-model-check.sh`, and `find-tla-tools.sh` all describe current
state, carry `rev2§…` refs, and contain no phase markers. `spec_rev2.md` is
unedited (`git diff` empty). The Tier-D `find-tla-tools.sh` cleanup correctly
*replaced* (not just deleted) the Toolbox tier so the not-found path still
returns 1.

**But four committed tooling files cite the temporary docs — a real, recurring
violation** (`git grep`, complete list):

| file:line | violation |
|---|---|
| `scripts/tla-baseline.sh:10-11` | cites `doc/plans/0_tla-optimization.md §1` (and bare "§1" at :45) |
| `scripts/tla-baseline.sh:52` | cites `doc/results/2_tla-findings.md §3` |
| `scripts/tla-neg-controls.sh:8` | cites `doc/plans/0_tla-optimization.md §2 A7` (also the `A7` phase marker) |
| `tools/tla/model-manifest.tsv:6` | cites "plan §1 'coverage is the obligation count'" |

These are new files, so the effort *introduced* the slips. They are easily
fixed: state the rule in-line ("distinct-states is the coverage metric; a drop
means TLC proved less") without sourcing it to the plan, and drop the `A7` tag.
None affects behaviour, but the discipline is explicit that the temporary
plan/findings docs must not be referenced from committed artifacts.

**Two stale CI comments** (`describe what *was*, before later PRs landed):

* `.github/workflows/ci.yml:73-79` — the `model-safety` block says
  "~12.2M reachable states" and "~5 min at 4 workers." Post-B3/B4 the arm
  carries `SYMMETRY SafetySymmetry` and explores **1,240,344** distinct states in
  **63 s** (measured). The comment never mentions symmetry and is ~5× high on
  time — it is the B2-era description left unrevised by B3/B4. Misleads a reader
  about what CI actually does and costs.
* `.github/workflows/ci.yml:39-40` — "find-tla-tools.sh honours a pre-set JAVA +
  TLA_TOOLS instead of probing for the macOS Toolbox." Tier-D (#210) *removed*
  the Toolbox tier entirely, so this describes machinery that no longer exists.

(Minor, pre-existing house style: `ci.yml` references spec sections as bare
`§3.3`/`§3.6` without the `rev2` prefix the rule wants. The effort's additions
matched the surrounding file rather than the rule; worth a sweep but not
introduced here.)

---

## Findings summary (prioritized)

| # | severity | finding |
|---|---|---|
| 1 | — (confirm) | Symmetry quotients are **sound**: `CHOOSE` seeds are init-only; sets genuinely interchangeable; symmetric + asymmetric controls guard each axis; no `SYMMETRY` on any liveness arm; Burnside arithmetic correct. |
| 2 | — (confirm) | B1/B5/B6 are **semantics-preserving** (byte-identical counts reproduced locally); coverage **grown** (24× safety state space), never shrunk; all manifest pins reproduce to the digit. |
| 3 | major | The **"10 → 6 min" framing is misleading** (sums parallel jobs; hides ~78% more total compute). Honest statement: ~2.5× *wall-clock* win from sound parallelization + a large *coverage* addition. The findings docs themselves do not make the misleading claim. |
| 4 | major | `ci.yml:73-79` `model-safety` comment is **stale** (~12.2M / ~5 min vs the real 1.24M / 63 s post-symmetry); omits that the arm runs under `SafetySymmetry`. |
| 5 | minor–major | **Four committed tooling files cite `doc/plans`/`doc/results`/"plan §…"** (`tla-baseline.sh:10,52`; `tla-neg-controls.sh:8`; `model-manifest.tsv:6`) — a comment-discipline violation. |
| 6 | minor | `ci.yml:39-40` references the Toolbox probing that Tier-D removed (stale). |
| 7 | minor | `ReportMonotone` is checked under `SafetySymmetry` with **no dedicated negative control** (sound by inspection, but the effort's own teeth-test discipline is incomplete for it). |
| 8 | info | Deferred payoff is real and unspent: raise `CapIds`/`Refs` on the quotiented arms; add `Permutations(Threads)`; push the liveness arm (the sole critical-path pole). |

## Conclusion

The effort is sound, honest in its own documentation, and a net win: it converts
a single ~10-minute single-threaded gate into a ~4-minute parallel one, and uses
the freed structure to *add* substantial verified coverage (a 24×-larger safety
state space, ten gating negative controls, and 2–10× sound symmetry quotients)
rather than to trade coverage for speed. The user's suspicion is well-founded as
a framing critique — "combined 10 → 6 min" is the wrong way to count, because the
only real speedup is the sound `-workers` parallelization of the liveness arm
(measured 3.6×) and the rest is parallel-hidden *added* compute — but it is not
evidence of anything broken: no symmetry is unsound, no rewrite changed what is
verified, and the liveness arm (where symmetry would be unsound) was correctly
left alone. The actionable defects are documentation-level: two stale CI comments
and four tooling-file citations of the temporary plan/findings docs.
