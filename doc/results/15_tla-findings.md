# 15 — TLA+ liveness optimization, Step 4: fairness reformulation probe

Step 4 of `doc/plans/1_tla-liveness.md` — the first Tier-2 (theorem-touching)
probe. Tier 1 (Steps 0–3) exhausted the sound, behaviour-preserving levers for
the suite's one wall-clock pole, the `model` job's `EventuallyRevoked` liveness
check on `CapRevocation.cfg`. Tier 2 probes a change that alters *what is
assumed* — here, the fairness hypothesis — expecting a null but harvesting a
negative control from it.

The real liveness arm assumes **per-cap** weak fairness on `RevokeStep`:

```
Fairness == \A c \in CapIds : WF_crVars(RevokeStep(c))   (CapRevocation.tla)
Spec     == Init /\ [][Next]_vars /\ Fairness
```

The probe asks whether a strictly weaker **single existential** WF still proves
`EventuallyRevoked`:

```
ExistFairness == WF_crVars(\E c \in CapIds : RevokeStep(c))
SpecExistFair == Init /\ [][Next]_vars /\ ExistFairness   (same Init/Next, only fairness changed)
```

**Status: null (the weakening is unsound) → harvested a new negative control
`CapRevocation_NegFairness.cfg`.** The single-existential WF does **not** prove
`EventuallyRevoked`: at five caps TLC exhibits a starvation lasso. The real
`Spec`/`Fairness`/`CapRevocation.cfg` are left untouched; the weakened fairness
lives only in the `SpecExistFair` control twin, which is committed as the
suite's first fairness teeth-test. Per-cap fairness is load-bearing.

## Method

Cold runs (TLC scratch wiped before each), vendored `tla2tools.jar` matching its
SHA1, JDK 17 (Temurin 17.0.19), host Darwin arm64 (8 cores). The floor probe is
run as the **exact CI liveness invocation** (`workers=4`, `-Xmx4g`, default
`-lncheck`); the control is run as the **exact negative-control invocation** the
`model` job uses — `scripts/tla-neg-controls.sh`'s `TLC_WORKERS=1 -Xmx2g`, default
periodic liveness checking (no `-lncheck final`, so a livelock trips at a periodic
check rather than only after full exploration). `EventuallyRevoked`'s verdict is
the metric; distinct/diameter are reported to show the reachable graph is
unchanged by the fairness substitution.

## The probe: does existential WF prove `EventuallyRevoked`?

The fairness conjunct does not change the reachable state set (TLC checks the
safety graph over all of `[Next]_vars` regardless of fairness), so `SpecExistFair`
explores the **same graph** as `Spec` — only the temporal verdict can differ.

| run | constants | distinct | generated | diam | verdict |
|---|---|---:|---:|---:|---|
| real arm (`Spec`/`CapRevocation.cfg`) — baseline `11` | `CapIds=4` | 503,070 | 4,831,322 | 22 | No error |
| **probe at floor** (`SpecExistFair`, `CapIds=4`) | `CapIds=4` | **503,070** | **4,831,322** | **22** | **No error** |
| **control** (`SpecExistFair`, `CapIds=5`) | `CapIds=5` | — (trips early) | — | — | **`EventuallyRevoked` violated** |

**At the 4-cap floor the existential WF HOLDS** — byte-identical graph to the real
arm (503,070 / 4,831,322 / 22), "No error has been found" (1m49s at workers=4).
The floor cannot witness the weakening. This was the predicted outcome, not the
plan's first guess of a floor livelock; the next section is why.

## Why the floor holds and five caps is the threshold

`Init` seeds exactly one live cap (`InitCap`); every other cap is derived from it
by `Copy`, whose guard `~AncestorOrSelfRevoking(src)` forbids growth into any
revoking subtree. So a fixed revoking subtree only ever **shrinks** — it never
re-grows (this is exactly what the *liveness* control `SpecNoGuard` removes, and
why that control livelocks instead).

Under `ExistFairness`, WF forces `\E c : RevokeStep(c)` to fire infinitely **as
long as it is continuously enabled** — but it is satisfied by *some* `RevokeStep`,
not each cap's. To violate `EventuallyRevoked` a marked root `c*` must stay in
`revoking` with a non-empty subtree forever while the existential keeps firing on
*other* caps. If `c*` were the only revoking cap with a leaf, the existential
would be enabled only via `RevokeStep(c*)` and WF would force it — draining `c*`.
So the livelock requires a **second, independently-refilled** revoking subtree to
keep the existential busy elsewhere.

Two disjoint (non-nested) revoking subtrees both hanging off a non-revoking common
root need, at peak, **five simultaneously-live caps**: the common root (a
non-revoking `Copy` source — it cannot itself be revoking or all `Copy` is
blocked), plus two caps in each subtree (a revoking root and a leaf). The 4-cap
floor cannot hold that configuration, so `EventuallyRevoked` holds there even
under the existential WF; five is the minimum breadth that exhibits it.

## The counterexample (the lasso TLC found at `CapIds=5`)

`scripts/tla-neg-controls.sh`'s run of the control reports
`Temporal property EventuallyRevoked was violated` (exit 13) with a lasso back to
an earlier state. Its *shape* is the churn-while-starve configuration the threshold
argument predicts — described below by cap role; the specific ids are illustrative,
not a transcription of TLC's trace (whose labels depend on exploration order):

- **A victim root** is marked `revoking` and stays revoking through the whole
  cycle, with a **child persisting** as its leaf — so its subtree never empties and
  `(c \in revoking) ~> (Descendants(c) = {})` is violated for that root.
- **A second, independently-refilled subtree** (the other subtree under a
  non-revoking common root) is churned every cycle: `RevokeStep` fires there (its
  leaf is deleted and re-`Copy`'d), so `\E c : RevokeStep(c)` fires infinitely and
  the trace is fair — yet the victim root is never serviced.

This is the per-cap-vs-existential gap made concrete: per-cap WF would force the
victim's `RevokeStep` (it is continuously enabled) and drain it; the existential WF
does not, and the victim starves.

## Why adoption is rejected (theorem-touching discipline)

The plan's framing was: *if the weaker fairness still proves `EventuallyRevoked`,
that is a stronger theorem worth adopting.* It does **not**. The floor "No error"
is **not** a stronger theorem — it is the floor (4 caps) sitting one cap below the
threshold (5) at which the property genuinely fails under the existential WF. The
existential WF is therefore an **unsound** model of the kernel's revoke scheduler:
the real system requires each marked root to make progress, and at ≥5 caps the
weakened assumption lets a root starve. Keeping it would be precisely the
"weakening that only makes the check pass at the floor" the governing line
forbids. The real `Spec`/`Fairness`/`CapRevocation.cfg` are untouched; per-cap
fairness stands as the load-bearing hypothesis, now with a runnable proof.

## The harvested control

Pure spec/cfg/tooling; **no action, invariant, or property body changed**, and the
real liveness arm is byte-identical.

* **`tla/cap_revocation/CapRevocation.tla`** — a new `ExistFairness` /
  `SpecExistFair` twin, a sibling of the existing `SpecNoGuard` liveness control.
  `SpecExistFair` reuses the real `Next` verbatim, so it differs from `Spec` only
  in the fairness conjunct — the property that makes it a valid control. Defining
  an unused operator/spec is inert for every cfg that does not name it.
* **`tla/cap_revocation/CapRevocation_NegFairness.cfg`** (new) — `SPECIFICATION
  SpecExistFair`, `CHECK_DEADLOCK FALSE`, `PROPERTY EventuallyRevoked`, no
  `SYMMETRY` (unsound under liveness). Constants are the liveness floor with a
  **single change** — `CapIds={c0,c1,c2,c3,c4}` (five, one above the floor) — so it
  is intentionally not in the floor lock-step set.
* **`scripts/tla-neg-controls.sh`** — one new `CONTROLS` entry (now 13); the
  trailing count echo auto-updates. The `model` CI job already runs this script.
* **`tools/tla/model-manifest.tsv`** — a header-comment note documenting the
  fairness control's constants and why they sit one cap above the floor lock-step
  set (negative controls are not pinned rows — TLC stops at the first lasso, so
  there is no full distinct/diameter to assert).

### Cost (honest accounting)

The control is a *liveness* control, so it costs more than the suite's safety
controls (which trip on short safety traces in well under a second): it must
explore enough of the graph for a periodic liveness check to see the lasso's
cycle. At the committed constants it trips at **2m11s** (`workers=1`, `-Xmx2g`),
having generated ~2.33 M states / 596 K distinct before the check fires; the trip
point is deterministic at `workers=1`. Levers to cheapen it were measured and
rejected for faithfulness: dropping to one process is a **wash** (≈2m11s — with a
channel present, `Send`/`Receive` shuffle caps regardless of process count);
dropping the channel halves it (≈1m05s, the lasso is pure-cspace) but adds a
second departure from the real arm. The single-change `CapIds=5` config was kept
as the cleanest faithful witness. Even so the cost is **comfortably within the CI
budget**: the other twelve controls run in ~8 s, but this new fairness control adds
the ~2m11s above, so the negative-control suite is now ~2m20s and the `model` job
(liveness arm ~1m45s + the small `Teardown`/`CommitProtocol`/`IpcReactor` checks +
~2m20s of controls) lands near **4–4.5 min** against the **15-min** cap — under a
third of it, with ~10 min of headroom. (This is the early-terminating regime the
plan distinguished from an *exhaustive* `CapIds=5` run, which would blow the cap.)

## Negative controls / coverage

The real arm is untouched, so its coverage is unchanged by construction
(`CapRevocation.cfg` still pins 503,070 / 22, "No error found"). The suite gains
one control:

- `scripts/tla-neg-controls.sh` — all **13** controls FAIL as designed; the new
  `CapRevocation_NegFairness.cfg` reports `EventuallyRevoked violated as expected`
  (exit 13). The other 12 are untouched and still trip.

## Verdict: null — per-cap fairness is load-bearing; control harvested

The single-existential WF is not a sound weakening: `EventuallyRevoked` holds at
the 4-cap floor only because the floor is one cap below the 5-cap threshold at
which the existential WF lets a marked root starve. The probe therefore adopts
**no change to the real spec** and instead banks the gap as a committed negative
control — the suite's first fairness teeth-test, proving the per-cap `Fairness` is
load-bearing exactly as `SpecNoGuard` proves the `Copy` derive-guard is. Tier 2's
Step 4 closes as the plan anticipated for a theorem-touching probe (null →
control), with the precise 4-holds/5-breaks boundary recorded. Step 5 (the
`revoked` ghost-variable abstraction probe) remains.
