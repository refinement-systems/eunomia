# TLA+ review-2 follow-up (4): close the `MoveSemantics` / `RevokedDead` teeth-gap

*Intermediate working document (doc/results). Records implementing follow-up (4)
from the second independent review (`doc/results/17_tla-review-2.md`, "Follow-ups"
§4.4 and findings-summary #6). Per the project's comment discipline it is
temporary, will be removed, and must not be referenced from code, specs, or
guidelines.*

## The follow-up

`17_tla-review-2.md` named one remaining incompleteness in an otherwise complete
teeth-test discipline (findings-summary #6, follow-up 4):

> **`MoveSemantics` and `RevokedDead` have no negative control anywhere** —
> checked but with no committed teeth-test (the analogue of the `ReportMonotone`
> gap #214 just closed). … a committed negative control violating each, so every
> invariant checked under a quotient has a runnable teeth-test.

The effort's discipline is that *every* property checked under a symmetry quotient
carries a standing negative control that still trips under that quotient — the
runnable proof the quotient does not silently hide bugs, since TLC never validates
a declared symmetry itself. `CapRevocation_Safety.cfg` checks six obligations
under `SYMMETRY SafetySymmetry`; before this change five had a control and two did
not:

| obligation (safety arm) | control | injected bug |
|---|---|---|
| `LiveParent` | `SpecBad` (`_NegControl` / `_Safety_NegControl`) | `RevokeStepBad` deletes an interior node |
| `DeadNowhere` | `SpecAsymBad` / `SpecCapAsymBad` | leak a dead cap into a cspace |
| `FireSafe` | `SpecThreadAsymBad` | leak a dead cap into a binding slot |
| `ReportMonotone` | `SpecReportBad` (#214) | `ReportFlip` flips a terminal report |
| **`MoveSemantics`** | **`SpecMoveBad` (this change)** | **`DupOwner` duplicates a live cap** |
| **`RevokedDead`** | **`SpecRevokedDeadBad` (this change)** | **`ReviveRevoked` revives a revoked cap** |

No committed control violated `MoveSemantics` or `RevokedDead` at all, with or
without symmetry. This change adds one symmetric control for each, the direct
analogue of the symmetric `SpecBad`→`LiveParent` and `SpecReportBad`→
`ReportMonotone` controls.

## What was added (the two controls)

In `tla/cap_revocation/CapRevocation.tla` (alongside the other committed
negative-control specs):

```tla
DupOwner ==
    /\ \E p, q \in Procs, c \in live :
        /\ p /= q
        /\ c \in cspaces[p]
        /\ c \notin cspaces[q]
        /\ cspaces' = [cspaces EXCEPT ![q] = @ \cup {c}]
    /\ UNCHANGED <<live, parent, queues, bindings, treport, revoked, revoking,
                   nlive, ncaps, pcbind, eopen>>

NextMoveBad == Next \/ DupOwner

SpecMoveBad == Init /\ [][NextMoveBad]_vars
```

`DupOwner` copies a **live** cap that already resides in one process's cspace into
another's *without* removing it from the first, so the cap owns two residences at
once — `Cardinality(ProcPlaces(c)) + … = 2 /= 1`, a direct `MoveSemantics`
(rev2§3.4) violation. The real model only ever *moves* a cap between residences
(`Send`/`Receive`/`Bind`) and never duplicates one, so the single-owner invariant
holds; `DupOwner` is exactly the lock-step minimum that breaks it.

```tla
ReviveRevoked ==
    /\ \E p \in Procs, c \in revoked :
        /\ c \notin live
        /\ live'    = live \cup {c}
        /\ cspaces' = [cspaces EXCEPT ![p] = @ \cup {c}]
    /\ UNCHANGED <<parent, queues, bindings, treport, revoked, revoking,
                   nlive, ncaps, pcbind, eopen>>

NextRevokedDeadBad == Next \/ ReviveRevoked

SpecRevokedDeadBad == Init /\ [][NextRevokedDeadBad]_vars
```

`ReviveRevoked` returns a revoked (dead) cap to `live` and a cspace while leaving
it in the `revoked` ghost set, so `revoked \cap live /= {}` — a direct
`RevokedDead` (rev2§2.2) violation. The real `Copy` reuses a revoked slot id only
by *forgetting* it from the ghost (`revoked' = revoked \ {dst}`); `ReviveRevoked`
is that ghost-clear omitted, the runnable proof the clear is load-bearing.

Each injected action specifies all 12 variables of `vars` (it changes only
`cspaces`, resp. `live`+`cspaces`, and holds the rest — including the teardown
`tdVars` — `UNCHANGED`), exactly like `LeakRevokedAsym` / `ReportFlip`, so
`Next \/ …` stays a complete next-state relation.

The two new cfgs `CapRevocation_MoveSemanticsBad.cfg` and
`CapRevocation_RevokedDeadBad.cfg` clone `CapRevocation_ReportMonotoneBad.cfg` —
the **safety constants** (`Threads={t0,t1}`, `QueueDepth=2`, …), the same
`SYMMETRY SafetySymmetry`, `CHECK_DEADLOCK FALSE`, and an `INVARIANT TypeOK`
sanity guard — but `SPECIFICATION SpecMoveBad` / `SpecRevokedDeadBad` and
`INVARIANT MoveSemantics` / `RevokedDead`. Both are registered in
`scripts/tla-neg-controls.sh`; CI gates the whole array through one
`bash scripts/tla-neg-controls.sh` and the summary count is `${#CONTROLS[@]}`, so
no CI edit and no count edit were needed.

## Result — the gap is closed (positive; no performance claim)

Measured locally, vendored `tla2tools.jar` (sha1 matches its `.sha1` pin),
Temurin 17.0.19, Apple-Silicon host.

**Both controls trip as designed, under symmetry.** At the safety constants and
under `SYMMETRY SafetySymmetry`:

* `CapRevocation_MoveSemanticsBad.cfg` reports `Invariant MoveSemantics is
  violated` (exit 12) with a depth-2 counterexample — `Init` →
  `DupOwner`, which copies `c0` from `p0`'s cspace into `p1`'s
  (`cspaces = (p0 :> {c0} @@ p1 :> {c0})`), so `c0` has two owners.
* `CapRevocation_RevokedDeadBad.cfg` reports `Invariant RevokedDead is violated`
  (exit 12) with a depth-3 counterexample — `Init` → a real `Next` step that
  retypes `c0` away (`live = {}`, `revoked = {c0}`) → `ReviveRevoked`, which
  returns `c0` to `live` and `p0`'s cspace while `revoked` still holds it
  (`revoked = {c0}`, `live = {c0}`).

The harness names each cleanly, and the full suite now reports **all 15 negative
controls failed as designed** (13 prior + these two), every entry `ok`:

```
  ok    CapRevocation_MoveSemanticsBad.cfg MoveSemantics violated as expected (exit 12)
  ok    CapRevocation_RevokedDeadBad.cfg   RevokedDead violated as expected (exit 12)
  …
all 15 negative controls failed as designed
```

**No positive-arm regression.** The change is purely additive — no `Spec`,
`Next`, invariant, or property *body* was touched, only six new definitions
nothing in the checked arms references. `scripts/tla-baseline.sh` over the
cap-revocation arms reproduces every pin to the digit:

| arm | distinct (pin) | diameter (pin) | verdict |
|---|---:|---:|---|
| `CapRevocation.cfg` (liveness) | 503,070 | 22 | OK |
| `CapRevocation_Safety.cfg` | 635,034 | 28 | OK |
| `CapRevocation_SafetyFloor.cfg` | 46,599 | 22 | OK |

**This is not a performance optimization, and none is claimed.** It *adds* two
sub-second negative controls to the CI suite (each finds its counterexample at
depth ≤ 3, well under a second at the controls' `-workers 1 -Xmx2g`), so it adds
no measurable critical-path cost and creates no new pole — unlike the ~131 s
`NegFairness` control (`17_tla-review-2.md` finding #2). Its value is closing the
teeth-test gap so every invariant checked under `SafetySymmetry` now has a
standing soundness monitor.

## Design rationale — why symmetric (not asymmetric), and one per invariant

`MoveSemantics` and `RevokedDead` are **state invariants**, the case TLC's
symmetry reduction is soundly defined for (the textbook-sound case — unlike the
`ReportMonotone` action property, whose soundness under the quotient needed the
extra "it is itself symmetric" argument in #214). The meaningful teeth-test is a
**symmetric** bug (`\E` over `Procs`/`live`/`revoked`, no `CHOOSE`, no model value
singled out): present in every orbit member, so it survives even a wrong quotient
and confirms the invariant is genuinely *evaluated* under `SafetySymmetry`. The
*over-broad*-quotient failure mode — a quotient that wrongly merges an asymmetric
violating state with a non-violating one — is already guarded for each permuted
axis by the existing asymmetric `SpecAsymBad` (Procs), `SpecCapAsymBad` (CapIds),
and `SpecThreadAsymBad` (Threads). So one symmetric control per invariant is the
correct and sufficient closure, matching the review's ask and the #214 precedent.

## Files changed

* `tla/cap_revocation/CapRevocation.tla` — `DupOwner` / `NextMoveBad` /
  `SpecMoveBad` and `ReviveRevoked` / `NextRevokedDeadBad` / `SpecRevokedDeadBad`,
  each with the house-style negative-control comment block.
* `tla/cap_revocation/CapRevocation_MoveSemanticsBad.cfg` — new control cfg.
* `tla/cap_revocation/CapRevocation_RevokedDeadBad.cfg` — new control cfg.
* `scripts/tla-neg-controls.sh` — two entries added to the `CONTROLS` array
  (suite 13 → 15).
* (No change to `tools/tla/model-manifest.tsv` — it pins only the positive arms —
  or to `.github/workflows/ci.yml` — the suite is auto-gated.)
