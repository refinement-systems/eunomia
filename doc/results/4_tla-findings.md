# TLA+ / TLC optimization findings — B5

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines. (B1's outcome is in `0_tla-findings.md`, B2's in
`1_tla-findings.md`, B3's in `2_tla-findings.md`, B4's in `3_tla-findings.md`.)*

All measurements below are **cold** (TLC scratch wiped first), vendored
`tools/tla/tla2tools.jar` (matches its `.sha1`), Temurin 17, host Darwin arm64,
**`-workers 1 -fp 0 -fpmem 0.5 -coverage 1`** via `scripts/tla-baseline.sh`.
B5 is a semantics-preserving rewrite, so the bar is the strongest one in plan
§1: **byte-identical distinct *and* generated counts**. generated-states is only
deterministic single-worker, so both arms here ran at `-workers 1` (not the CI
count of 4) precisely so the generated comparison is exact; distinct and
diameter are worker-invariant either way.

---

## B5 — factor `Next` / `NextBad` / `NextNoGuard` through `CommonActions`

**Status: adopted — a clarity / maintainability refactor with zero metric
movement.** This is *not* a performance optimization and was never expected to
be one (the plan tags B5 *adopt (refactor)*, "Risk: none", "Readability:
improves"). It is the null-perf-but-clarity-positive case the engagement policy
says to merge: the transition relation is logically unchanged, every measured
number is byte-identical, and the spec is shorter and harder to let drift.

### What it fixes

The `cap_revocation` model had four near-identical next-state relations whose
disjunct lists were copy-pasted. Of the **ten** action disjuncts, **eight are
identical verbatim** across `Next`, `NextBad`, and `NextNoGuard`; only two
positions vary:

* `Copy` vs `CopyNoGuard` (the liveness control drops the derive guard), and
* `RevokeStep` vs `RevokeStepBad` (the safety control drops the `IsLeaf` filter).

`NextAsymBad` / `NextCapAsymBad` are already written as `Next \/ <leak>`, so they
inherit the fix transitively. Before B5, adding or editing a common action meant
editing three blocks by hand; if one drifted, the negative controls would
silently stop tracking the real `Next` and lose their entire value — a control
is only a proof that a guard has teeth if it is otherwise **identical** to
`Next`. B5 makes that lock-step *structural* by extracting the eight shared
disjuncts into one `CommonActions` operator.

### The change

Pure spec; **one file, no cfg / manifest / script / CI edit** (nothing pinned
moves because no count or verdict changes).

* **`tla/cap_revocation/CapRevocation.tla`** — a new `CommonActions` operator
  (the eight invariant disjuncts, lifted verbatim), and the three relations
  rewritten as their two varying disjuncts followed by `\/ CommonActions`, with
  the `/\ UNCHANGED tdVars` conjunct preserved exactly:

  ```
  Next        == /\ \/ Copy        \/ RevokeStep    \/ CommonActions  /\ UNCHANGED tdVars
  NextBad     == /\ \/ Copy        \/ RevokeStepBad \/ CommonActions  /\ UNCHANGED tdVars
  NextNoGuard == /\ \/ CopyNoGuard \/ RevokeStep    \/ CommonActions  /\ UNCHANGED tdVars
  ```

  Net `git diff`: **+23 / −24** (24 copy-pasted disjunct lines removed; eight
  lifted into `CommonActions`, three `\/ CommonActions` lines and orienting
  comments added). The action *bodies* (`Copy`, `Send`, `RevokeStep`, …) are
  untouched.

### A note on the plan's framing (one position short)

The plan describes the result as "write each `Next*` as
`CommonActions \/ <the one variant>`". That is one position short: there are
**two** varying positions (`Copy`/`RevokeStep`), even though each control swaps
only *one* of them relative to `Next`. A single trailing variant cannot express
`NextBad` (which keeps real `Copy` but swaps `RevokeStep`) and `NextNoGuard`
(which keeps real `RevokeStep` but swaps `Copy`) from the same `CommonActions`.
The faithful form keeps both varying disjuncts explicit in each relation and
factors only the eight that never change — which also reads better, since the
two lines at the top of each `Next*` are exactly "what is different about this
twin". The eight-disjunct `CommonActions` is the correct shared core.

### Measurements (cold, `-workers 1`, `-fp 0 -fpmem 0.5`)

Every column is byte-identical before and after; the authoritative
`N states generated, M distinct states found` line and the diameter line matched
exactly on all five arms.

| arm (cfg) | distinct | generated | diam | before == after |
|---|---:|---:|---:|:--:|
| `CapRevocation` (liveness) | 503,070 | 4,831,322 | 22 | ✓ identical |
| `CapRevocation_Safety` (symmetry) | 1,240,344 | 13,194,241 | 28 | ✓ identical |
| `CapRevocation_Teardown` (TSpec) | 132 | 919 | 8 | ✓ identical |
| `CommitProtocol` | 6,886 | 18,781 | 21 | ✓ identical |
| `IpcReactor` | 39 | 59 | 13 | ✓ identical |

The **only** difference anywhere in the logs is cosmetic: the per-action
`-coverage` label moved from `<Next line 296 …>` to `<Next line 314 …>` (and
`<TNext line 628>` → `<627>`) because `CommonActions` + its comment shifted the
later definitions down. Those are source line-number labels, not metrics. (The
intermediate `Progress(n)` coverage snapshots also differ between runs — they
are wall-clock-timed samples of whatever was on the BFS queue at that instant,
not final counts; the terminal totals are identical.)

### Why this is semantics-preserving — both the good spec and the bad specs

* **Good spec (`Next`, exercised in full by `CapRevocation.cfg` and
  `CapRevocation_Safety.cfg`).** Byte-identical distinct **and** generated
  **and** diameter at `-workers 1 -fp 0` is the gold-standard check for a
  semantics-preserving rewrite: distinct is the coverage metric (worker- and
  fp-invariant), generated is fully deterministic single-worker, and both are
  literally equal. Disjunction is order-insensitive for the generated successor
  set, so listing the two varying disjuncts first (rather than `Copy` first,
  `RevokeStep` eighth) did not move any count — confirmed, not assumed.
* **Bad specs (`NextBad`, `NextNoGuard`).** The baseline harness does not
  full-explore these (their specs exit on the first counterexample), so they
  have no full-exploration count to diff. Their preservation argument is
  structural + behavioural: each is built from the *same* `CommonActions` as
  `Next` plus its one swapped disjunct, and `scripts/tla-neg-controls.sh`
  confirms all nine controls still trip on the **same named invariant/property
  with the same exit codes** as before the refactor (verbatim-identical script
  output, diffed). A drift in `CommonActions` would change `NextBad`/
  `NextNoGuard` in lock-step with `Next`, which the good-spec byte-identical
  result rules out.

### Validation

1. **SANY** parses `CapRevocation.tla` clean (the new operator and the three
   rewritten relations are well-formed).
2. **Five baseline arms** byte-identical before/after (table above); the
   harness's own manifest assertion (distinct + diameter vs
   `tools/tla/model-manifest.tsv`) passes 5/5.
3. **Nine negative controls** trip identically before and after — a verbatim
   diff of the two `tla-neg-controls.sh` runs is empty:

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

### Transient negative control (used, then removed — not committed)

To confirm the measurement itself has teeth (that a *botched* extraction would
be caught, not silently pass), one disjunct — `Retype` — was temporarily deleted
from `CommonActions` and the safety arm re-run. The harness reported, as
required:

```
CapRevocation_Safety  1240038  12594325  10.2  30  COVERAGE REGRESSION: distinct 1240038 != expected 1240344
```

i.e. dropping a single shared disjunct moved distinct (1,240,344 → 1,240,038)
and diameter (28 → 30) and tripped the manifest assertion. The `Retype` line was
then restored and the byte-identical result above re-confirmed. This was a
throwaway sanity check on the harness, **not** a standing artifact, so per the
engagement policy it is recorded here rather than committed — the committed
`NextBad` / `NextNoGuard` controls *are* B5's standing lock-step guard, and B5's
whole purpose is to make their parity with `Next` structural. **No new committed
negative control is warranted by B5.**

### Cost / CI judgement

Zero. No count, verdict, diameter, cfg, manifest entry, negative control, or CI
arm changed; CI wall-clock is unaffected (the `model`, `model-safety`, and
neg-controls steps replay byte-for-byte). The benefit is entirely
maintainability: an action added to the model now lands in `Next` and both
negative controls from a single edit site, so the controls cannot silently fall
out of lock-step.

### Decision

**Adopted as a clarity refactor.** Byte-identical distinct/generated/diameter on
all five arms, identical verdicts on all nine negative controls, SANY clean, and
a −1-line net diff that removes 24 lines of copy-paste in favour of one shared
operator. There is **no performance change, by design**; the value is removing a
silent-drift hazard from the negative-control machinery, exactly as the plan
tagged B5. Per the engagement policy, a null performance result that improves
code clarity is merged with the lack of speedup reported — which this is.
