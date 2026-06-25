# TLA+ optimization — follow-up (3): close the `ReportMonotone`-under-symmetry guard gap

*Intermediate working document (doc/results). Records implementing follow-up
(3) from the independent review (`doc/results/8_tla-review.md`, "Possible
follow-ups" §3 and findings-summary #7). Per the project's comment discipline it
is temporary, will be removed, and must not be referenced from code, specs, or
guidelines.*

## The follow-up

`8_tla-review.md` named one incompleteness in an otherwise sound symmetry
methodology (findings-summary #7, and §1.1):

> `ReportMonotone` is checked under `SafetySymmetry` with **no dedicated negative
> control** (sound by inspection, but the effort's own teeth-test discipline is
> incomplete for it). … Cheap to close (a `ReportMonotone`-violating bad-spec at
> the safety constants).

The effort's discipline is that *every* property checked under a symmetry
quotient carries a standing negative control that still trips under that quotient
— the runnable proof the quotient does not silently hide bugs, since TLC never
validates a declared symmetry itself. Every other guarded property had one:
`LiveParent` (the symmetric `SpecBad`, run under `SafetySymmetry` as
`CapRevocation_Safety_NegControl.cfg`), `DeadNowhere`/`FireSafe` (the asymmetric
`AsymBug`/`CapAsymBug`/`ThreadAsymBug`), and `RecoverReconstructs` in
CommitProtocol (`CommitProtocol_AsymBug`). `ReportMonotone` — the one safety
*property* (an action property `[][...]_vars`, rev2§5.1, not a state invariant)
the safety arm checks under `SafetySymmetry` — had none. No committed control
violated it at all, with or without symmetry.

### Sharpened relevance after follow-up (2)

The review's soundness argument for `ReportMonotone` was that it "ranges only
over `Threads`, which is *not* permuted." Follow-up (2) (`9_tla-findings.md`,
commit `efd6df0`) folded `Permutations(Threads)` into `SafetySymmetry`, so
`Threads` **is** now permuted. `ReportMonotone` is therefore now a symmetric
property checked under a quotient that *does* act on the set it ranges over —
making a standing teeth-test more than bookkeeping. The property stays sound
because it still quantifies *uniformly* over `Threads` (`\A t \in Threads`,
naming no specific thread), so it is invariant under any thread permutation; this
control is the runnable confirmation of that.

## What was added (the control)

A **symmetric** `ReportMonotone`-violating bad-spec — the direct analog of the
symmetric `SpecBad`→`LiveParent` control, but targeting the action *property*.

In `tla/cap_revocation/CapRevocation.tla` (alongside the other committed
negative-control specs):

```tla
ReportFlip(t) ==
    /\ treport[t] /= "running"
    /\ treport' = [treport EXCEPT
                       ![t] = IF @ = "exited" THEN "faulted" ELSE "exited"]
    /\ UNCHANGED <<live, parent, cspaces, queues, bindings, revoked, revoking,
                   nlive, ncaps, pcbind, eopen>>

NextReportBad == Next \/ (\E t \in Threads : ReportFlip(t))

SpecReportBad == Init /\ [][NextReportBad]_vars
```

`ReportFlip` lets any thread whose report already reached a terminal state
(`/= "running"`) flip to the *other* terminal value — a direct `ReportMonotone`
violation. The injected action is the lock-step minimum: it changes only
`treport` and leaves every other variable (including the teardown `tdVars`)
unchanged, exactly like `LeakRevokedThreadAsym`, so `Next \/ …` remains a
complete next-state relation.

The new cfg `CapRevocation_ReportMonotoneBad.cfg` clones
`CapRevocation_Safety_NegControl.cfg` — the **safety constants**
(`Threads={t0,t1}`, `QueueDepth=2`, …), the same `SYMMETRY SafetySymmetry`,
`CHECK_DEADLOCK FALSE`, and an `INVARIANT TypeOK` sanity guard — but
`SPECIFICATION SpecReportBad` and `PROPERTY ReportMonotone`. It is registered in
`scripts/tla-neg-controls.sh`; CI gates the whole array through one
`bash scripts/tla-neg-controls.sh` and the summary count is `${#CONTROLS[@]}`,
so no CI edit and no count edit were needed.

## Result — the gap is closed (positive; no performance claim)

Measured locally, vendored `tla2tools.jar` (sha1 matches its `.sha1` pin),
Temurin 17, Apple-Silicon host.

**The control trips as designed, under symmetry.** At the safety constants and
under `SYMMETRY SafetySymmetry`, TLC reports `Action property ReportMonotone is
violated` (exit 13) with a depth-3 counterexample — `Init` →
`ThreadExit(t0)` (running→exited) → `ReportFlip(t0)` (exited→faulted):

```
State 1: <Initial predicate>   treport = (t0 :> "running" @@ t1 :> "running")
State 2: <Next ...>            treport = (t0 :> "exited"  @@ t1 :> "running")
State 3: <ReportFlip(t0) ...>  treport = (t0 :> "faulted" @@ t1 :> "running")
Error: Action property ReportMonotone is violated.
```

The harness names it cleanly (`ReportMonotone violated as expected (exit 13)`),
and the full suite now reports **all 12 negative controls failed as designed**
(11 prior + this one), every entry `ok`.

**No positive-arm regression.** The change is purely additive — no `Spec`,
`Next`, invariant, or property *body* was touched, only three new definitions
that nothing in the checked arms references. `scripts/tla-baseline.sh` over the
whole manifest reproduces every pin to the digit:

| arm | distinct (pin) | diameter (pin) | verdict |
|---|---:|---:|---|
| `CapRevocation.cfg` (liveness) | 503,070 | 22 | OK |
| `CapRevocation_Safety.cfg` | 635,034 | 28 | OK |
| `CapRevocation_Teardown.cfg` | 132 | 8 | OK |
| `CommitProtocol.cfg` | 413,455 | 29 | OK |
| `IpcReactor.cfg` | 39 | 13 | OK |

**This is not a performance optimization, and none is claimed.** Per the
engagement policy this is the null-perf-but-soundness-positive case: it *adds* one
more sub-second negative control to the CI suite (a negligible compute
*increase*, not a speedup), and its value is closing the teeth-test gap so every
symmetric property checked under a quotient now has a standing soundness monitor.
The change is desirable on correctness/discipline grounds and is merged on those
grounds.

## Design rationale — why symmetric, and the controls considered and rejected

**Why a *symmetric* control (not asymmetric).** The two control kinds guard
different failure modes (`8_tla-review.md` §1.1): a *symmetric* bug (present in
every orbit member) proves the property is genuinely *evaluated* under the
quotient; an *asymmetric* bug (singling out one model value) catches an
*over-broad* quotient that wrongly merges an asymmetric violating state with a
non-violating one. `ReportMonotone` is itself symmetric, so the meaningful
teeth-test is the symmetric one — it confirms the action-property machinery still
fires under `SafetySymmetry`. The over-broad-`Threads` axis is *already* guarded
by the asymmetric `SpecThreadAsymBad` (which trips `FireSafe`); an asymmetric
`ReportMonotone` variant would add nothing for a genuinely symmetric property
(TLC canonicalises, and a symmetric property's violation is preserved in every
orbit member regardless of which thread is singled out). So one symmetric control
is the correct and sufficient closure, matching the review's singular ask ("*a*
`ReportMonotone`-violating bad-spec").

**Negative control on the negative control (temporary, not committed).** To
confirm the violation is a genuine property failure and not an artifact of the
symmetry declaration, a diagnostic cfg identical to the committed control but with
the `SYMMETRY` line stripped was run once: it *also* reports `Action property
ReportMonotone is violated` at depth 3 (exploring 98 distinct states before
halting vs 34 under the quotient — the quotient reaches the same verdict via
fewer representatives). This `_NoSym` cfg was a temporary diagnostic and is **not
committed**: the gap the review named is specifically about the *under-symmetry*
configuration, the committed control runs under `SafetySymmetry` (the
configuration whose soundness it guards), and the existing symmetric control
(`CapRevocation_Safety_NegControl`) likewise commits only the under-symmetry
form. The cross-check is recorded here per the policy on temporarily-used
controls.

## Files changed

* `tla/cap_revocation/CapRevocation.tla` — `ReportFlip` / `NextReportBad` /
  `SpecReportBad` plus the house-style negative-control comment block.
* `tla/cap_revocation/CapRevocation_ReportMonotoneBad.cfg` — new control cfg.
* `scripts/tla-neg-controls.sh` — one entry added to the `CONTROLS` array.
* (No change to `tools/tla/model-manifest.tsv` — it pins only the positive arms —
  or to `.github/workflows/ci.yml` — the suite is auto-gated.)
