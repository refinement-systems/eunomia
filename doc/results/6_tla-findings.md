# TLA+ / TLC optimization findings — C1

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines. (B1's outcome is in `0_tla-findings.md`, B2's in
`1_tla-findings.md`, B3's in `2_tla-findings.md`, B4's in `3_tla-findings.md`,
B5's in `4_tla-findings.md`, B6's in `5_tla-findings.md`.)*

All measurements below are **cold** (TLC scratch wiped first), vendored
`tools/tla/tla2tools.jar` (matches its `.sha1`), Temurin 17, host Darwin arm64,
`-workers 1 -fp 0 -fpmem 0.5 -coverage 1` via `scripts/tla-baseline.sh`.
distinct-states and diameter are **worker-invariant**; generated is deterministic
at `-workers 1`. The model is sub-second, so all runs (main arm + both controls)
ran single-worker.

---

## C1 — `SYMMETRY Permutations(Refs)` on `CommitProtocol.cfg` (and its negative control)

**Status: adopted — a sound, near-ideal `2!` state-space quotient.** Declaring
`SYMMETRY RefSymmetry` (= `Permutations(Refs)`) on the commit/recovery model
collapses each `r1`↔`r2` permutation-orbit. The main arm drops **6,886 → 3,444
distinct (a 1.9994× reduction — essentially the full `2!` ideal)**, with **every
invariant verdict unchanged**, the **diameter unchanged (21)**, the existing
symmetric negative control still tripping *under the symmetry*, and a new
*asymmetric* negative control (committed) catching an over-broad quotient. As in
B3/B4 this is the one case where a distinct-state reduction is *not* a coverage
regression: a sound quotient checks the same behaviours in fewer representatives.

The plan tagged C1 *adopt*, the value being **headroom to raise `Refs`** (≈6× at
`Refs=3`) for broader partial-commit coverage rather than the (negligible)
absolute saving on an already sub-second model. The flagged risk is that the main
cfg checks the *action* property `[][RecoverReconstructs]_vars`, which TLC does
not validate under symmetry any more than it validates a symmetry over invariants
— so the negative controls run under the same symmetry are the standing soundness
guard. Per the recorded decision, **`Refs` stays at 2** here: this is a
same-coverage speed/soundness result; raising the constant is a separate measured
change (see Follow-ups).

### The change

Pure spec/cfg/tooling; **no action, invariant, or property body changed.**

* **`tla/commit_protocol/CommitProtocol.tla`** — a `RefSymmetry ==
  Permutations(Refs)` operator, plus a new asymmetric negative-control bad-spec —
  `AsymRef`/`RecoverAsymBad`/`NextAsymBad`/`SpecAsymBad` — a sibling of the
  existing `RecoverNoop`/`SpecNeg` block (see Validation §3). Defining an unused
  operator/spec is inert for every cfg that does not name it.
* **`CommitProtocol.cfg`** — one new line `SYMMETRY RefSymmetry`; `SPECIFICATION
  Spec`, `CHECK_DEADLOCK FALSE`, the five invariants and `PROPERTY
  RecoverReconstructs` are otherwise unchanged. CI's `model` job runs this via the
  default-cfg path, so the symmetry is picked up with no workflow edit.
* **`CommitProtocol_NegControl.cfg`** — the same `SYMMETRY RefSymmetry` line so
  the symmetric `RecoverNoop` bug runs under the *exact* symmetry the main arm now
  declares (the plan's "(and its negative control)").
* **`CommitProtocol_AsymBug.cfg`** (new) — the Refs-axis over-broad-quotient
  guard: `SpecAsymBad` at the same constants under `SYMMETRY RefSymmetry`,
  asserting `RecoverReconstructs`. Registered in `scripts/tla-neg-controls.sh`
  (now ten controls); the `model` CI job already runs that script.
* **`tools/tla/model-manifest.tsv`** — the `CommitProtocol` row re-pinned to the
  post-quotient distinct count (3,444; diameter 21 unchanged), with the header
  comment updated to name `RefSymmetry` and the pre/post counts.

### Why this is a sound quotient, not lost coverage

`Refs` is interchangeable in exactly the way `Procs`/`CapIds` were on the safety
arm (B3/B4): `Init`, every action (`Write`/`Flush`/`CommitPrepare`/
`CommitFinish`/`Crash`/`Recover`), all five invariants, and the
`RecoverReconstructs` action property reference `Refs` only via `\A r \in Refs`,
function comprehension over `Refs`, or `\E r \in Refs`. **No action does `CHOOSE
… \in Refs`** — so unlike cap_revocation's `InitProc`/`InitCap` seeds there is not
even an asymmetric-`CHOOSE`-over-a-symmetric-set initial asymmetry to clear here.
`walLog` records carry a ref in their first component, so a ref permutation
carries a durable/volatile state to an equivalent one (e.g. `walLog =
<<<<r1,1>>,<<r2,1>>>>` ↔ `<<<<r2,1>>,<<r1,1>>>>` — distinct states, one orbit).
The quotient is therefore sound by construction; adoption was nonetheless gated on
the adversarial controls below, not on a passing run.

Symmetry is **unsound under a liveness property** in TLC, but the commit model
declares none — the only temporal check is the *safety* action property
`[][RecoverReconstructs]_vars` — so the liveness exclusion (which keeps symmetry
off `CapRevocation.cfg`/`IpcReactor.cfg`) does not apply.

### Measurements (cold, `-workers 1`, `-fp 0 -fpmem 0.5 -coverage 1`)

| arm (cfg) | distinct | generated | gen:dist | diam | verdict |
|---|---:|---:|---:|---:|---|
| `CommitProtocol.cfg` — no symmetry (before) | 6,886 | 18,781 | 2.7 | 21 | No error |
| **`CommitProtocol.cfg` — `RefSymmetry` (after)** | **3,444** | **9,393** | **2.7** | **21** | **No error** |

* **distinct 1.9994×** (6,886 → 3,444); **generated 1.9995×** (18,781 → 9,393).
* **Diameter unchanged** (21): the quotient reaches every orbit-representative at
  the same BFS depth — the structural-equivalence check a wrong quotient would
  fail.
* The `gen:dist` ratio is identical (2.7) before and after — the quotient removes
  whole orbits uniformly, it does not change the per-state work profile.

### Why the factor is essentially the full `2!` (honest accounting)

The symmetry group has order `|Refs|! = 2! = 2`, so the ceiling is `2×`. By
Burnside `orbits = (1/2)(|Fix(id)| + |Fix(swap)|) = (1/2)(6,886 + F)`; with
`orbits = 3,444` this gives **`F = 2`** states fixed by the `r1`↔`r2` swap (the
fully-symmetric states — `Init`, where both refs are empty, and the all-committed
terminal state). Only 2 of 6,886 states are self-symmetric, so the realized
1.9994× sits right at the `2×` ideal — in contrast to B4, where a large
fixed-point mass dragged a `48`-order group down to `9.82×`. The commit model has
no structure (like cap_revocation's "rarely all caps live at once") that inflates
the symmetric region, so the quotient is as free as a size-2 set allows.

### Validation — the mandatory guards (all met)

1. **Verdicts + structure unchanged.** The main arm under `RefSymmetry` reports
   "Model checking completed. No error has been found": all five invariants
   (`TypeOK`/`AtLeastOneValidSlot`/`GenerationsDistinct`/`CommittedRootsDurable`/
   `AckedWritesRecoverable`) **and** the `RecoverReconstructs` action property
   hold, and the **diameter is 21** — identical to before. SANY parses
   `CommitProtocol.tla` clean (TLC's semantic pass).
2. **Negative controls still trip** (`scripts/tla-neg-controls.sh`) — all ten,
   including the new Refs-asymmetric guard:

```
ok  CommitProtocol_NegControl.cfg  RecoverReconstructs violated as expected (exit 13)  # symmetric guard, RefSymmetry
ok  CommitProtocol_AsymBug.cfg     RecoverReconstructs violated as expected (exit 13)  # asym guard (new), RefSymmetry
```

   (The other eight cap_revocation / ipc_reactor controls are untouched and still
   trip — the runner ends "all 10 negative controls failed as designed".) The
   symmetric `CommitProtocol_NegControl` (`RecoverNoop`, rebuilds nothing →
   `RecoverReconstructs`) still trips under the symmetry — proof the quotient does
   not prune a bug present in every orbit member. But a symmetric bug survives even
   a *wrong* quotient, so it cannot probe over-breadth; that is the next guard's
   job. This is also C1's **"injected real bug still caught"** leg: a recovery that
   rebuilds nothing is a genuine recovery bug, and it is still caught under the
   quotient.
3. **Injected asymmetric-bug probe** (the over-broad-quotient test — the new
   control). `RecoverAsymBad` rebuilds every ref's overlay correctly **except one
   `CHOOSE`-singled ref** (`AsymRef`), for which it rebuilds nothing — a genuine
   `RecoverReconstructs` violation that singles out **one ref model value**, so it
   breaks the Refs-symmetry premise and its path (`Write(AsymRef) → Crash →
   RecoverAsymBad`) traverses states the quotient collapses. Run both ways:

   | probe | result |
   |---|---|
   | without symmetry (control: bug is real) | `Action property RecoverReconstructs is violated` (140 distinct) |
   | **with `SYMMETRY RefSymmetry`** | **`Action property RecoverReconstructs is violated`** (exit 13) |

   The quotient does **not** hide the asymmetric violation. This is the
   load-bearing Refs check: where the symmetric `RecoverNoop` guard (§2) cannot
   detect an over-broad Refs symmetry, this one — singling out a ref — does.
   (Mechanism: TLC evaluates the action property on each generated
   orbit-representative, and `RecoverReconstructs` is itself symmetric over `Refs`,
   so a violating orbit cannot be silently canonicalised away.)
4. **Other models untouched.** The new operator and bad-spec add no obligations to
   any cfg that does not name them. The other three CI model runs
   (`CapRevocation.cfg` liveness, `CapRevocation_Teardown.cfg`, `IpcReactor.cfg`)
   re-checked clean ("No error has been found"); `CapRevocation_Safety.cfg` and all
   eight other negative controls are unchanged.

### Cost / CI-wall-clock judgement

* The model is sub-second before and after; the ~2× state reduction is real but
  the absolute saving is below TLC's start-up noise (the plan's own framing of
  C1's value as headroom, not speed).
* The `model` job gains one negative control (the asymmetric guard), which finds
  its counterexample in the first few BFS levels — a fraction of a second. *Total*
  CI wall-clock is unaffected (the `model` job's pole is the `CapRevocation.cfg`
  liveness run, untouched here).

### Honest framing

Like C1's plan entry says, this is a **coverage / headroom** play, not a
critical-path speedup — the saving on a sub-second model is immaterial. The win is
(a) a sound, near-ideal `2×` quotient with a clean before/after, (b) the standing
asymmetric soundness guard the commit model previously lacked (its only control
was the symmetric `RecoverNoop`, which cannot probe an over-broad quotient), and
(c) the headroom the plan named C1 *for*: the commit arm can now run at larger
`Refs` (broader partial-commit / partial-flush coverage) inside the same budget.
Per the recorded decision that constant bump is **not** spent here — `Refs` stays
at 2, banking the soundness result and the clean quotient.

### Decision

**Adopted.** A sound `2!` quotient (6,886 → 3,444) with every verdict intact, the
diameter preserved, the symmetric control still tripping under the symmetry, and a
new asymmetric control catching the over-broad-quotient case (and confirmed
reachable without symmetry). C1 was the last open item in
`doc/plans/0_tla-optimization.md`; the Tier-A substrate, all of Tier-B, and now
C1 are complete.

### Follow-ups (out of scope here)

- **Raising `Refs` on the commit arm** — the headroom this quotient unlocks, and
  the payoff the plan attached to C1. At `Refs = 2` the arm explores 3,444 states
  in well under a second; the freed budget can instead buy broader partial-commit
  coverage (`Refs = 3`, ≈6× the un-quotiented set, ≈3× post-quotient). Deferred by
  decision to keep this attempt a same-coverage speed/soundness result with a clean
  before/after; raising the constant changes what is verified (a strict coverage
  gain) and needs its own measured PR — re-pin the manifest to the new reachable
  set. (`MaxWrites` is held by the §3 guardrail at ≥ 2.)

### Note on the throwaway reachability control

The asymmetric probe's *no-symmetry* arm (the §3 "bug is real" row, 140 distinct)
was run from a throwaway `CommitProtocol_AsymBug_NoSym_TMP.cfg` —
`CommitProtocol_AsymBug.cfg` minus its `SYMMETRY RefSymmetry` line — created on the
fly and removed after the run, exactly as B3/B4 did for their asymmetric probes. It
is deliberately **not** committed: it proves only that the bug is reachable, which
the standing guard does not need to re-assert on every CI run. The committed
`CommitProtocol_AsymBug.cfg` covers the symmetric side (the soundness monitor); the
no-symmetry side is reproducible on demand via `scripts/tla-baseline.sh
--no-symmetry` or by re-stripping the line.
