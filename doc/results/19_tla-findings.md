# TLA+ review-2 follow-up (6): retroactive #212 findings + doc-level nits

*Intermediate working document (doc/results). Records implementing follow-up (6)
from the second independent review (`doc/results/17_tla-review-2.md`, "Follow-ups"
§4.6 and findings-summary #4/#7). Per the project's comment discipline it is
temporary, will be removed, and must not be referenced from code, specs, or
guidelines.*

## Why this doc exists

`17_tla-review-2.md` finding #4 named one discipline gap in an otherwise complete
documentation trail: **#212 shipped without a findings doc.** Every liveness Step
(docs `11`–`16`) and the two earlier review-8 follow-ups #213 / #214 (docs `9` /
`10`) got one; #212 — *"tla: spend the RefSymmetry budget (Refs=3) and fix
comment-discipline slips"* (commit `643a02c`) — did not, against the plan §1 rule
"every step gets a findings doc." That PR bundled the window's single largest
coverage change **and** a load-bearing budget measurement into a record that lived
only in its commit message:

> #212 … bundles the single largest coverage change of the window (CommitProtocol
> +120×) *and* the load-bearing CapIds=5 out-of-budget measurement into a PR
> documented only by its commit message.

This doc is that missing record, written retroactively. The numbers below were
re-derived cold, not copied: a fresh local TLC run of `CommitProtocol.cfg`
reproduced 413,455 distinct / diameter 29 (`coverage ok` against the manifest pin,
"No error has been found"), and the review itself (`17` §0, §1.5) reproduced the
6.0× quotient on its own host.

## Part A — spend the RefSymmetry budget (CommitProtocol `Refs=3`)

The review-8 follow-up #1 ("spend the banked symmetry quotient where it fits CI")
landed for the one model where it was affordable: **CommitProtocol**, by raising
the commit-protocol ref set `Refs = {r1,r2}` → `{r1,r2,r3}` (`MaxWrites=2`,
`NULL=NULL` unchanged).

| metric | `Refs={r1,r2}` (before) | `Refs={r1,r2,r3}` (#212) |
|---|---:|---:|
| distinct states (pinned) | 3,444 | **413,455** |
| diameter (pinned) | 21 | **29** |

A **+120×** coverage increase. The diameter rise from 21 → 29 is the expected
deepening from a third ref entering the commit/recover interleavings — a larger
explored space, not a structural regression.

**The S₃ quotient is sound.** Both the positive `Spec` and the two negative
controls carry `SYMMETRY RefSymmetry == Permutations(Refs)`. At three refs the
permutation group is S₃ (order 6), and the quotient is valid because **no operator
in the real `Spec` distinguishes a particular ref** — there is no `CHOOSE r \in
Refs`. (The only `CHOOSE r \in Refs` in the module is inside the *asymmetric*
`SpecAsymBad` control at `CommitProtocol.tla:344`, where singling out a ref is the
intended planted asymmetry.) So the 2,480,585 full reachable states collapse to
413,455 post-quotient orbit representatives — a near-ideal **6.0×** reduction
(`2,480,585 / 413,455 = 6.0`), same behaviours, fewer representatives. This pins
the larger coverage at a small wall-clock: ~12 s with coverage on, in the **parallel
`model` job** off the critical path.

**The teeth survive the bump.** Both committed controls still trip
`RecoverReconstructs` at `Refs=3`:

* `CommitProtocol_NegControl` (`SpecNeg`) — the **symmetric** control, present in
  every orbit member, so it survives even a wrong quotient and proves the property
  is genuinely evaluated under `RefSymmetry`.
* `CommitProtocol_AsymBug` (`SpecAsymBad`) — the **asymmetric** control (its
  `CHOOSE r \in Refs` singles out one ref), the guard against an over-broad quotient
  that would wrongly merge an asymmetric violating state with a non-violating one.

The manifest row is re-pinned (`tools/tla/model-manifest.tsv`: `CommitProtocol …
413455 29`) and — since follow-up (2) (#221) armed `TLC_ASSERT_MANIFEST` — a silent
coverage shrink back toward 3,444 now fails CI in the `model` job, not just a local
baseline run.

## Part B — `CapIds=5` measured out of budget (correcting the prior estimate)

The same follow-up #1 asked whether the **CapRevocation safety arm** could also
spend its symmetry budget by raising `CapIds` from 4 to 5. It cannot, and #212
recorded the measurement that says so — the half of the PR most worth not losing to
a commit message.

`CapIds = 5` on the safety arm is a **~12–18M-state, >25-min-local / >50-min-CI
run** that blows the `model-safety` job's 15-min cap. The reason is a growth-rate
mismatch the prior review's estimate missed: the **CDT-forest count explodes far
faster than the `Procs × CapIds` symmetry group grows**. Going 4 → 5 caps multiplies
the reachable capability-derivation forests combinatorially, while the symmetry
group that quotients them grows only from order 48 to order 240 — nowhere near
enough to absorb the blow-up. So the net post-quotient state space still explodes.

This **corrects `8_tla-review.md`'s "near-zero wall-clock cost" estimate for
`CapIds=5`** — it did not survive measurement. Raising the safety arm to five caps
is out of the current CI budget; the arm stays at `CapIds=4`. (Recorded in session
memory as `tla-capids5-out-of-ci-budget` so the wrong "near-zero" figure is not
re-proposed.) `17_tla-review-2.md` §4 logs review-8 follow-up #1 as *partly closed*
on exactly this basis: Refs=3 landed (Part A), CapIds=5 measured out (Part B).

## Part C — comment-discipline fixes #212 also carried

#212 bundled the cleanup of the comment-discipline slips `8_tla-review.md` §4
found (CLAUDE.md: comments may reference only doc/spec / doc/guidelines):

* dropped the `doc/plans` / `doc/results` / `plan §…` citations and the `A7` phase
  marker from `scripts/tla-baseline.sh`, `scripts/tla-neg-controls.sh`, and
  `tools/tla/model-manifest.tsv`, stating each cited rule inline instead;
* refreshed two stale `.github/workflows/ci.yml` comments — the `model` job's
  `find-tla-tools.sh` note (no longer citing a removed macOS-Toolbox probing tier)
  and the `model-safety` job's `~12.2M reachable / ~5 min` line, corrected to
  `SYMMETRY SafetySymmetry`, 12,183,480 → 1,240,344 post-quotient, ~2 min at 4
  workers. (That 1,240,344 was the `Procs × CapIds` quotient at the time;
  follow-up #213 immediately after added the `Threads` permutation axis, taking it
  to the 635,034 the cfg/manifest/ci comments now pin — a 1.95× on top.)

## Files #212 changed

`.github/workflows/ci.yml`, `scripts/tla-baseline.sh`,
`scripts/tla-neg-controls.sh`, `tools/tla/model-manifest.tsv`,
`tla/commit_protocol/CommitProtocol.cfg`,
`tla/commit_protocol/CommitProtocol_AsymBug.cfg`,
`tla/commit_protocol/CommitProtocol_NegControl.cfg`.

## Soundness

This is a coverage / discipline record, not a behaviour change. Part A is a sound
symmetry quotient (no `CHOOSE` over the permuted `Refs` in the real `Spec`; both
controls trip at `Refs=3`), independently reproduced cold here (413,455 / 29,
"No error") and in `17` §1.5 (the 6.0× quotient on a second host). Part B is a
recorded null (a measured out-of-budget constants bump, no change adopted).
Part C touched only comments. No verified property was weakened.

---

## Follow-up (6) also fixed (doc-level nits)

Implemented alongside this retroactive doc, completing `17_tla-review-2.md`
follow-up (6) / finding #7:

* **`doc/results/15_tla-findings.md` budget sentence** — was internally stale: it
  claimed "the full negative-control suite runs in ~8 s today" in the same doc that
  *adds* the ~2m11s `NegFairness` control (stated seven lines earlier). Corrected to
  itemize the new control: the other twelve controls run in ~8 s, this fairness
  control adds ~2m11s, so the suite is ~2m20s and the `model` job lands near
  ~4–4.5 min — consistent with the doc's own 2m11s figure and the `17` §2 measured
  ~256 s.
* **`doc/results/15_tla-findings.md` lasso narration** — the counterexample section
  presented specific cap ids (`c1`/`c4`/`c2`/`c3`/`c0`) as if a literal TLC trace.
  Reworded to describe the lasso by cap *role* (a victim root with a persisting
  child; a second independently-refilled churned subtree under a non-revoking common
  root), with a one-clause note that the ids are illustrative and a real trace's
  labels depend on exploration order.
* **`.github/workflows/ci.yml` `-lncheck` note** — added one line where the flag is
  set: `-lncheck` is an internal TLC strategy selector (absent from its `-help`), so
  its semantics ride the vendored-jar pin verified in the same job — a jar bump must
  re-confirm the flag's behaviour.
* **`.github/workflows/ci.yml` spec-ref sweep** — prefixed the 13 remaining bare
  `§…` spec references with `rev2` per CLAUDE.md's reference discipline (e.g.
  `§3.3` → `rev2§3.3`, `rev2§2.2/§3` → `rev2§2.2/rev2§3`); every `§` in the file now
  reads `rev2§`.

The review's optional last item — collapsing the subsumption rationale triplicated
across `CapRevocation.cfg`, `model-manifest.tsv`, and `ci.yml` — was **not** taken:
comment discipline forbids the natural cross-file "see X" pointer, and each copy
serves a distinct reader (the cfg a TLC reader, the manifest the pin lineage, the
ci.yml comment the parallel-job split), so a clean collapse is not available without
stranding locally-relevant context.
