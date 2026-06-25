# TLA+ optimization — follow-up (2): `Permutations(Threads)` on the safety arm

*Intermediate working document (doc/results). Records implementing follow-up
(2) from the independent review (`doc/results/8_tla-review.md`, "Possible
follow-ups" §2 and findings-summary #8). Per the project's comment discipline it
is temporary, will be removed, and must not be referenced from code, specs, or
guidelines.*

## The follow-up

`8_tla-review.md` named an explicit, unspent payoff:

> **Add `Permutations(Threads)` to the safety arm.** B2 introduced a second
> size-2 permutable set (`Threads={t0,t1}`); folding it into `SafetySymmetry`
> compounds the quotient further.

The safety arm (`CapRevocation_Safety.cfg`, `Spec` with no liveness property)
ran under `SYMMETRY SafetySymmetry = Permutations(Procs) ∪ Permutations(CapIds)`
— group order 48. `Threads={t0,t1}` is a third interchangeable model-value set
but was not in the group. This change folds it in:

```
ThreadSymmetry == Permutations(Threads)
SafetySymmetry == ProcSymmetry \cup CapSymmetry \cup ThreadSymmetry
```

new group order **96** (48 × 2! for the size-2 `Threads` axis).

## Result — a near-ideal 1.95× further quotient (positive)

Measured locally, vendored `tla2tools.jar` (sha1 matches its `.sha1` pin),
Temurin 17, `-workers 4 -Xmx4g`, Apple-Silicon host. The before-number was
re-derived on stock `main` (not trusted from the table); the after-number is the
edited tree.

| safety arm | distinct | diameter | generated | wall (4w) |
|---|---:|---:|---:|---:|
| before (Procs×CapIds, order 48) | 1,240,344 | 28 | 13,194,241 | 62 s |
| after (×Threads, order 96) | **635,034** | **28** | 6,836,621 | 48 s |

* **Distinct states 1,240,344 → 635,034 = 1.953×**, almost the ideal 2.0× the
  size-2 `Threads` axis can give (much of the state space *is* thread-asymmetric:
  the second TCB's `treport`/binding-slot interleavings — exactly the residence
  axis B2 restored — are what the quotient now folds).
* **Diameter is unchanged at 28** — symmetry collapses orbits, it does not alter
  reachability depth; an unchanged diameter is the expected, sound signature.
* **Total quotient over the full reachable set is now 12,183,480 → 635,034 =
  19.18×** (was 9.82×). The full-reachable figure (12,183,480) is carried over
  from the B4 measurement; it is a property of the model, not of the symmetry
  declaration, so adding `ThreadSymmetry` does not change it.
* Wall-clock dropped 62 s → 48 s on this arm — real but **off the critical
  path** (see framing below).

## Soundness — and the action-property subtlety the review flagged

TLC never validates a declared symmetry, so soundness rests on (a) the model
values being genuinely interchangeable and (b) standing negative controls.

**`Threads` is genuinely interchangeable.** There is no `CHOOSE` over `Threads`
and no hardcoded `t0`/`t1` anywhere — the only `CHOOSE` seeds are
`InitCap`/`InitProc`, neither over `Threads`. `Init` seeds all threads
identically (`treport="running"`, all binding slots `NULL`). Every action
quantifies uniformly over `Threads` (`Bind`/`ThreadExit`/`ThreadFault` via
`\E t \in Threads`; `DeleteOne` rewrites every thread's slots at once). So the
`Threads` axis carries *none* of the asymmetric-`CHOOSE`-seed risk the
`Procs`/`CapIds` quotients carry — it is, if anything, the safest of the three.

**The action-property subtlety.** §1.1 of `8_tla-review.md` noted that the
safety arm checks `ReportMonotone`, a safety *action* property (`[][…]_vars`),
under symmetry, and that this was sound *because `ReportMonotone` ranges over
`Threads`, which was not permuted*. This change permutes `Threads`, so that exact
justification no longer applies and is replaced by the stronger one: TLC symmetry
is sound for a property iff the property is itself invariant under the group, and
both thread-ranging properties on this arm —

* the `FireSafe` invariant: `\A t \in Threads, k \in BindKinds : bindings[t][k] =
  NULL \/ bindings[t][k] \in live`, and
* the `ReportMonotone` action property: `[][\A t \in Threads : treport[t] /=
  "running" => treport'[t] = treport[t]]_vars`

— are `\A t \in Threads` universals with no asymmetric thread reference, hence
invariant under any permutation of `Threads`. The quotient therefore stays sound
for both, the action property included. (This is recorded in the `ThreadSymmetry`
rationale comment in the spec.)

**Per-axis negative control added.** The effort's praised discipline is one
*asymmetric* control per permuted axis (Procs → `CapRevocation_AsymBug`,
CapIds → `CapRevocation_CapAsymBug`, Refs → `CommitProtocol_AsymBug`). Adding the
`Threads` axis requires its dual, so this change adds one:

```
AsymThread == CHOOSE t \in Threads : TRUE
LeakRevokedThreadAsym ==           \* leak a dead cap into AsymThread's "exit" slot
    /\ revoked /= {}
    /\ bindings[AsymThread]["exit"] = NULL
    /\ LET d == CHOOSE c \in revoked : TRUE
       IN bindings' = [bindings EXCEPT ![AsymThread]["exit"] = d]
    /\ UNCHANGED <<…all other vars…>>
```

It injects a single dead cap into **one specific thread's** binding slot — a
`FireSafe` violation that singles out a thread model value, breaking the
`Threads`-symmetry premise. The `= NULL` guard keeps the injected fact to exactly
one dead cap in one otherwise-empty slot, so it trips `FireSafe` alone and
displaces no live cap. It runs at the safety constants under the *same*
`SafetySymmetry` (`CapRevocation_ThreadAsymBug.cfg`, wired into
`scripts/tla-neg-controls.sh`), and the quotient must still report it — catching
an over-broad thread symmetry that merged an asymmetric violating state with a
non-violating one.

**Both soundness checks pass:**

* `scripts/tla-neg-controls.sh` → **all 11 controls fail as designed**,
  including `CapRevocation_ThreadAsymBug.cfg → FireSafe violated`. Crucially,
  *every pre-existing* control still fails under the enlarged
  `Procs×CapIds×Threads` group (the over-broad-quotient check):
  `AsymBug`/`CapAsymBug` → `DeadNowhere`, `Safety_NegControl` → `LiveParent`,
  etc. Adding `Threads` to the group hid no existing bug.
* `scripts/tla-baseline.sh CapRevocation_Safety` → `distinct 635034 diam 28
  verdict ok` (the re-pinned manifest row, regression guard armed).
* The main arm itself reports **0 errors** (all 6 invariants + `ReportMonotone`
  hold) at the new quotient.

## Honest framing — coverage quotient, not a critical-path speedup

Consistent with the review's §3 accounting: the safety arm runs as a *separate,
parallel* CI job (`model-safety`), so the model-checking critical path is
`max(model, model-safety)` and the pole is the **liveness arm**'s
`EventuallyRevoked` tableau (on which symmetry is unsound and therefore absent).
This change makes the `model-safety` job ~22% faster locally (62 → 48 s) but does
**not** move that pole. Its real value is a **larger sound quotient** — the same
24×-larger safety state space (rev2§3.3/§5.1's second TCB and depth-2 ring) now
checked from half as many representatives, with the thread axis gaining a
standing teeth-test it previously lacked. Stated plainly: coverage/headroom and
clarity, not a speedup of the work that gates CI.

## Negative controls

* `CapRevocation_ThreadAsymBug.cfg` + `SpecThreadAsymBad`/`LeakRevokedThreadAsym`
  — **committed** (the per-axis asymmetric control for the new quotient; fails on
  `FireSafe` as designed, wired into the CI neg-controls gate).
* No temporary-and-removed controls were used in this effort.

## Not in scope (still open)

* **Follow-up (3)** — the `ReportMonotone`-under-symmetry teeth-gap. §1.1 of the
  review noted `ReportMonotone` has *no* negative control that violates it.
  The thread control added here violates `FireSafe`, not `ReportMonotone`, so it
  strengthens the `Threads`-axis soundness story but does **not** close that gap;
  a `ReportMonotone`-violating bad-spec remains a separate follow-up. (It is now
  marginally more salient, since `ReportMonotone` is checked under a group that
  permutes the very set it ranges over — though, as argued above, soundly.)
* **Follow-up (1) tail** — raising `CapIds` to 5 on the safety arm remains out of
  budget (measured >25-min local / >50-min CI; the CDT forest count explodes far
  faster than the symmetry group grows), so it was left at 4. `Refs=3` already
  landed (PR #212).

## Files changed

* `tla/cap_revocation/CapRevocation.tla` — `ThreadSymmetry`; `SafetySymmetry`
  now unions all three sets; `AsymThread`/`LeakRevokedThreadAsym`/
  `NextThreadAsymBad`/`SpecThreadAsymBad` control + rationale comments.
* `tla/cap_revocation/CapRevocation_ThreadAsymBug.cfg` — new control cfg.
* `tla/cap_revocation/CapRevocation_Safety.cfg` — symmetry comment now names
  `Threads`.
* `scripts/tla-neg-controls.sh` — new control row (`FireSafe`).
* `tools/tla/model-manifest.tsv` — safety-arm row re-pinned `1240344 → 635034`;
  ledger comment updated (group order 48 → 96, quotient figure).
* `.github/workflows/ci.yml` — `model-safety` comment names the three permuted
  sets and the new quotient count; timeout estimate `~2 → ~1.5 min`.
