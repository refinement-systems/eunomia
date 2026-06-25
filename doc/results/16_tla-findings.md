# 16 — TLA+ liveness optimization, Step 5: `revoked` ghost-variable abstraction probe

Step 5 of `doc/plans/1_tla-liveness.md` — the second and last Tier-2
(theorem-touching) probe, closing the tier. Tier 1 (Steps 0–3) exhausted the
sound, behaviour-preserving levers for the suite's one wall-clock pole, the
`model` job's `EventuallyRevoked` liveness check on `CapRevocation.cfg`. Tier 2
probes changes that alter *what is checked or assumed*, expecting a null but
quantifying the gap. Step 4 probed the fairness hypothesis (null → harvested
`CapRevocation_NegFairness.cfg`). Step 5 probes whether the `revoked` ghost
variable can be abstracted away to shrink the liveness graph.

`revoked` is a monotonically-accumulating history set (rev2§2.2 reuse discipline)
whose only checked consumer is `RevokedDead == revoked \cap live = {}`. Step 2
already dropped `RevokedDead` from this liveness cfg, so in the liveness arm
`revoked` carries *zero* checked obligation — its only effect there is enlarging
the state fingerprint (TLC distinguishes states by every variable). The plan's
question: at the floor, is `revoked` **reachably-redundant** (functionally
determined by the rest of the state → removal is free but yields nothing) or does
it **genuinely split states** (removal shrinks the graph but is a coverage-bearing
abstraction — "a proof obligation, not a casual speed knob")?

**Status: reject / null — `revoked` genuinely splits states (the not-free
branch), but adoption is rejected.** Freezing `revoked` shrinks distinct from
503,070 to 466,512 (−7.27%) with `EventuallyRevoked` still "No error" and
diameter unchanged at 22. So it is a real graph-multiplier, not redundant — yet
the removal is theorem-touching (the graph changes), the variable is load-bearing
for the safety arm and three symmetry-soundness controls and so cannot be deleted
from the shared module, and the modest ~6% wall-clock win does not justify forking
the module. The real spec is left untouched; no new control is warranted.

## Method

Cold runs (TLC scratch wiped before each), vendored `tla2tools.jar` matching its
SHA1 (`b1f5c956…`), JDK 17 (Temurin 17.0.19), host Darwin arm64. Both runs use
the exact baseline-harness flag set (`-fp 0 -fpmem 0.5 -coverage 1`,
`TLC_WORKERS=4`, `-Xmx4g`) so distinct / generated / diameter / wall-clock are
directly comparable. `distinct` and `diameter` are worker-invariant and the
authoritative metrics; `generated` and wall-clock are advisory at `workers>1`.

The probe needs a `revoked`-free model of the *liveness arm only*. Rather than
delete `revoked` from its ~25 sites in the shared module (the `VARIABLES` entry,
`crVars`/`vars`, every `UNCHANGED <<…>>` tuple, the three write actions, `TypeOK`,
`RevokedDead`, and the control-only `LeakRevoked*` actions), the measurement uses
a **freeze-to-constant** technique on a throwaway copy
(`CapRevocation_NoRevoked.tla`): the three write conjuncts that mutate `revoked`
— `Copy` (`revoked' = revoked \ {dst}`), `DeleteOne` (`revoked \cup {d}`, the
deletion shared by `RevokeStep`), and `Retype` (`revoked \cup {c}`) — are each
replaced with `revoked' = revoked`. `Init` already sets `revoked = {}`, so under
the liveness `Next` the variable is now constant `{}` in every reachable state.

This is **measurement-equivalent to removing `revoked`**, for two reasons that
also carry the soundness argument:

1. **A constant variable contributes a multiplicative factor of exactly 1 to the
   distinct-state count.** Every reachable state has `revoked = {}`, so the
   reachable set is `{(obs, {}) : obs ∈ reachable observable states}`. Hence
   `distinct(frozen) = |reachable obs states| = distinct(removed)`. The frozen
   model's 466,512 is exactly the count a `revoked`-deleted model would report.
2. **`revoked` gates no action.** It is *written* by `Copy`/`DeleteOne`/`Retype`
   and *read* only by `RevokedDead`, `TypeOK`'s `revoked \subseteq CapIds`
   conjunct, and the `LeakRevoked*` control actions — never in an action's
   enabling guard (`Copy`'s guard is `dst \notin live`, not on `revoked`). So
   freezing it leaves the transition relation on all other variables **identical**;
   the observable behaviour graph, and therefore the `EventuallyRevoked` verdict,
   is unchanged. Removing or freezing `revoked` is a bisimulation on the observable
   variables — only the states that differed *solely* in `revoked` collapse.

The throwaway copy SANY-parses clean and was deleted after measurement; the real
`CapRevocation.tla`/`.cfg` are byte-identical to HEAD.

## The probe: does the `revoked` dimension add reachable states?

| run | constants | distinct | generated | diam | verdict | wall (w4) |
|---|---|---:|---:|---:|---|---|
| real arm (`CapRevocation.cfg`) — baseline `11` | `CapIds=4` | 503,070 | 4,831,322 | 22 | No error | 02:01 |
| **probe** (`CapRevocation_NoRevoked.cfg`, `revoked` frozen) | `CapIds=4` | **466,512** | 4,539,161 | **22** | **No error** | 01:54 |
| **delta** | — | **−36,558 (−7.27%)** | −292,161 (−6.05%, adv.) | **0** | unchanged | −7s (−5.8%, adv.) |

The reachable set shrinks by 36,558 states — a multiplier of
`503,070 / 466,512 ≈ 1.078×`. So `revoked` is a **genuine state-multiplier, not
reachably-redundant**: it is true history that cannot be reconstructed from the
rest of the state, and TLC counts the difference. The witness is concrete: `Init`
has `live={c0}, revoked={}`; after `Copy(c0→c1)` then revoking `c1`, the state is
`live={c0}, revoked={c1}` with every other variable back at its `Init` value — two
distinct states under `Spec` that the frozen model collapses to one. The "free but
yields nothing" (reachably-redundant) branch is ruled out by measurement.

Two corroborating observations:

- **Verdict preserved** ("No error has been found"): `EventuallyRevoked` holds in
  the frozen model exactly as in the real arm, the empirical confirmation of the
  bisimulation argument above — the abstraction is *valid for the property*.
- **Diameter unchanged (22)**: the deepest behaviour is not lengthened by
  `revoked` accumulation. The states `revoked` distinguishes sit *beside* the
  floor's behaviours (reachable at the same BFS depth via the observable path),
  not *below* them. This is why the win is bounded — `revoked` widens the graph by
  ~7.8% but does not deepen it, and the liveness pole is dominated by depth-bound
  tableau structure (Step 0: the liveness tableau is ~4× the reachable set).

## Why adoption is rejected (theorem-touching discipline)

The 7.27% reachable-state reduction is real, but rejected on four grounds — any
one sufficient:

1. **Theorem-touching, not behaviour-preserving.** The reduction *is* the change
   to the graph: distinct 503,070 → 466,512, generated and the action-coverage
   profile all move. By the governing line of `0_tla-optimization.md`, a Tier-1
   sound optimization must leave distinct/generated/diameter byte-identical; this
   one cannot, by construction. It can only be judged as a theorem-touching
   abstraction, which must clear a far higher bar than a scheduling-only win.

2. **The variable is load-bearing and non-removable from the shared module.**
   `CapRevocation.tla` is one module serving the liveness, safety, teardown, and
   all control cfgs. `RevokedDead` (rev2§2.2 reuse discipline — a revoked slot
   stays dead until explicitly reused) is actively checked by
   `CapRevocation_Safety.cfg` (635,034 / 28), and the three `LeakRevoked*`
   symmetry-soundness controls (`CapRevocation_AsymBug` / `_CapAsymBug` /
   `_ThreadAsymBug`) read `revoked` to inject a ghost-revoked cap. Deleting
   `revoked` breaks the safety arm and three negative controls outright.

3. **Liveness-only removal requires forking the module — a soundness liability.**
   A declared variable is always part of the state fingerprint; there is no
   per-cfg way to drop it. Removing it from the liveness arm alone would mean a
   *separate* `revoked`-free liveness module kept in sync with the safety module by
   hand. The liveness arm would then model-check a **different state machine** than
   safety, with silent-drift risk on every future edit — exactly the failure mode
   "abstraction is a proof obligation, not a casual speed knob" warns against. The
   maintenance and soundness cost dwarfs a one-time ~6% wall-clock saving.

4. **The win is modest and off the dominant pole.** Step 2 already eliminated
   `RevokedDead`'s *per-state* evaluation cost (it was the cheapest invariant, one
   set-intersection). The only effect `revoked` still has on the liveness arm is
   the ~7.8% state-count widening measured here, which buys ~6% wall-clock — and
   Step 0 showed the arm is partly SCC-bound over a tableau ~4× the reachable set,
   so a width-only reduction does not attack the depth-bound pole. A ~6% advisory
   wall-clock delta, gated behind a module fork, is not a favorable trade.

The abstraction is therefore *valid for the property but not worth adopting*: a
clean theoretical reduction whose engineering cost (a forked trusted artifact)
and modest, off-pole payoff make the disciplined call a reject.

## Why no new negative control is warranted

Step 4's null harvested a control because it exposed an *unguarded* gap (per-cap
fairness had no teeth-test). Step 5 exposes no such gap: the `revoked` /
`RevokedDead` obligation is already guarded suite-wide by `CapRevocation_Safety.cfg`
and the three `LeakRevoked*` symmetry controls, all of which still FAIL as
designed. Nothing is removed from the suite (the probe artifacts are throwaway),
so coverage is intact by construction — there is no obligation to re-express and
no new teeth-test to add. This mirrors Steps 2/3, which also adopted no control.

## Negative controls / coverage

The real arm is untouched, so its coverage is unchanged by construction
(`CapRevocation.cfg` still pins 503,070 / 22, "No error found"; re-derived cold
this step). The suite is unchanged:

- `scripts/tla-neg-controls.sh` — all **13** controls still FAIL as designed
  (re-run this step), including the safety-arm guardians of the `revoked`
  discipline (`CapRevocation_AsymBug` / `_CapAsymBug` / `_ThreadAsymBug` trip
  `DeadNowhere`/`FireSafe`).
- `tools/tla/model-manifest.tsv` — no pin change; `CapRevocation` stays 503,070 /
  22.

## Verdict: reject / null — `revoked` is a genuine but non-adoptable multiplier

`revoked` genuinely splits the liveness graph (freezing it shrinks distinct
503,070 → 466,512, −7.27%, multiplier ≈1.078×), ruling out the
reachably-redundant branch; and the abstraction is valid for `EventuallyRevoked`
(verdict preserved, diameter unchanged). But the removal is theorem-touching, the
variable is load-bearing for the safety arm and three symmetry controls and so
cannot be deleted from the shared module, a liveness-only fork is a soundness
liability, and the ~6% off-pole wall-clock win does not justify it. The real
`Spec` / `CapRevocation.tla` / `CapRevocation.cfg` are untouched and no control is
added. Tier 2 closes as the plan anticipated for its theorem-touching probes
(Step 4 null → control; Step 5 documented reject), with the precise 7.27%
multiplier on the record. Step 6 (the IpcReactor out-of-scope null, folded into
`11`) and Step 7 (the synthesis / adversarial review) remain.
