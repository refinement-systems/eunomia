# TLA+ / TLC optimization findings — B3

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines. (B1's outcome is in `0_tla-findings.md`, B2's in
`1_tla-findings.md`.)*

All measurements below are **cold** (TLC scratch wiped first), vendored
`tools/tla/tla2tools.jar` (matches its `.sha1`), Temurin 17, host Darwin arm64,
`-fp 0 -fpmem 0.5`. distinct-states and diameter are **worker-invariant**, so
the coverage numbers and verdicts are independent of `-workers`; only the
advisory wall-clock depends on it. The two big arms ran at `-workers 4` (the CI
count) through `scripts/tla-baseline.sh` (which adds `-coverage 1`); the
sub-second controls/probes ran single-worker.

---

## B3 — `SYMMETRY Permutations(Procs)` on the safety arm, `Permutations(Notifs)` on Teardown

**Status: adopted — a real, sound state-space quotient.** Declaring `SYMMETRY`
on the two *invariant-only* cap_revocation arms collapses each permutation-orbit
of the size-2 model-value sets (`Procs`, `Notifs`) to one representative. The
safety arm drops **12,183,480 → 7,264,485 distinct (1.68×)** and the Teardown
arm **252 → 132 (1.91×)**, with **every invariant verdict unchanged**, the
diameter unchanged, all seven negative controls (including a new *symmetric*
one) still tripping, and a deliberately-injected asymmetric bug still caught.
This is the one case where a distinct-state reduction is *not* a coverage
regression: a sound quotient checks the same behaviours in fewer states.

Symmetry is **unsound under a liveness property** in TLC, so neither symmetry
touches the liveness `CapRevocation.cfg` (or `IpcReactor.cfg`); both live only on
arms that check no temporal property. B2 (the safety arm) was the prerequisite,
and A7 (the negative-control runner) is the standing soundness monitor the plan
mandates before any symmetry.

### The change

Pure spec/cfg/tooling; **no action, invariant, or property body changed.**

* **`tla/cap_revocation/CapRevocation.tla`** — two symmetry operators
  (`Permutations` is already in scope via `EXTENDS ... TLC`):
  `ProcSymmetry == Permutations(Procs)` and
  `NotifSymmetry == Permutations(Notifs)`. Defining an unused operator is inert
  for every cfg that does not name it, so the liveness arm and the existing six
  controls are behaviourally untouched.
* **`CapRevocation_Safety.cfg`** — `SYMMETRY ProcSymmetry`.
* **`CapRevocation_Teardown.cfg`** — `SYMMETRY NotifSymmetry`.
* **`CapRevocation_Safety_NegControl.cfg`** (new) — the symmetric soundness
  guard: the existing `SpecBad` (interior-delete → `LiveParent` violation) at the
  safety-arm constants **under the same `SYMMETRY ProcSymmetry`**. Registered in
  `scripts/tla-neg-controls.sh`; the `model` CI job already runs that script.
* **`tools/tla/model-manifest.tsv`** — the two symmetrized rows re-pinned to the
  post-quotient distinct counts (7,264,485 and 132; diameters 28 and 8
  unchanged), with a comment that each is an orbit count, not a coverage loss.

### Why this is a sound quotient, not lost coverage

`Procs` and `Notifs` are interchangeable model values: every action and
invariant treats them uniformly. The only initial asymmetry is the spec's
`InitCap == CHOOSE c \in CapIds : TRUE` / `InitProc == CHOOSE p \in Procs : TRUE`
seeds — `Init` gives the root cap to exactly `InitProc`. That `CHOOSE` is the
structurally-risky case the plan flagged (an asymmetric choice over a symmetric
set), so adoption was gated on adversarial validation, not a passing run. The
choice is itself symmetric (any process serves equally as the seed), and TLC
canonicalises the `CHOOSE` consistently; the validation below confirms the
quotient is sound *at these constants* empirically.

### Measurements (cold, `-workers 4`, `-fp 0 -fpmem 0.5 -coverage 1`)

| arm (cfg) | distinct | generated | gen:dist | diam | wall (4w) | verdict |
|---|---:|---:|---:|---:|---:|---|
| Safety — before (no symmetry) | 12,183,480 | 138,167,803 | 11.3 | 28 | 6m26s | No error |
| **Safety — after (`ProcSymmetry`)** | **7,264,485** | **76,871,188** | **10.6** | **28** | **4m07s** | **No error** |
| Teardown — before (no symmetry) | 252 | 1,747 | 6.9 | 8 | <1s | No error |
| **Teardown — after (`NotifSymmetry`)** | **132** | **919** | **7.0** | **8** | **<1s** | **No error** |

* **Safety distinct 1.677×** (12,183,480 → 7,264,485). **Generated 1.797×**
  (138.2M → 76.9M) — the work drops *more* than the distinct count, because
  symmetry also avoids regenerating permuted successors, not just storing them.
* **Teardown distinct 1.909×** (252 → 132).
* **Diameter unchanged** on both arms (28, 8): the quotient reaches every
  orbit-representative at the same BFS depth.

### Why the safety factor is 1.68×, not ~2× (honest accounting)

A size-2 symmetry caps the reduction near 2×; the exact factor is
`2 / (1 + F/T)`, where `F` is the count of states **fixed** by the `p0↔p1` swap
(states that map to themselves). Measured `F`:

| arm | total `T` | fixed `F` | F/T | factor |
|---|---:|---:|---:|---:|
| Safety | 12,183,480 | 2,345,490 | 19.3% | 1.677× |
| Teardown | 252 | 12 | 4.8% | 1.909× |

The safety arm has a **large** fixed-point set: a state is proc-swap-fixed when
`cspaces[p0] = cspaces[p1]`, and ~19% of reachable states satisfy that — chiefly
the many states where both cspaces are empty because the caps reside in queues,
binding slots, or have been revoked. The plan measured 1.89× for this symmetry
at the *liveness floor* (`Threads=1`), where fewer such coincidences exist; B2's
restoration of `Threads=2`/`QueueDepth=2` enlarged the proc-symmetric region and
so *lowered* the Procs-symmetry factor. This is a real, measured interaction, not
a defect — the quotient is still sound and still removes 4.9M redundant states.

### Validation — B3's mandatory guards (all met)

1. **Verdicts unchanged.** Both arms still report "No error has been found": all
   six safety invariants + `ReportMonotone` on the safety arm; all four TSpec
   invariants (`TTypeOK`/`RefCountSound`/`ChannelFireSafe`/`ReclaimedReleased`)
   on Teardown.
2. **Negative controls still trip** (`scripts/tla-neg-controls.sh`) — all seven,
   including the new symmetric guard:

```
ok  CapRevocation_NegControl.cfg        LiveParent violated as expected (exit 12)
ok  CapRevocation_Safety_NegControl.cfg LiveParent violated as expected (exit 12)   # symmetry guard
ok  CapRevocation_NegLiveness.cfg       EventuallyRevoked violated as expected (13)
ok  CommitProtocol_NegControl.cfg       RecoverReconstructs violated as expected (13)
ok  IpcReactor_NegControl.cfg           NoLostWakeup violated as expected (12)
ok  IpcReactor_NegBackpressure.cfg      NoLostWakeupWritable violated as expected (12)
ok  IpcReactor_NegLostWakeup.cfg        NoLostWakeup violated as expected (12)
```

   The new `CapRevocation_Safety_NegControl` is the load-bearing one: it runs
   `SpecBad` at the *exact* safety-arm constants **and** `SYMMETRY ProcSymmetry`,
   and still finds the interior-delete `LiveParent` violation — proof the
   quotient does not prune the bug it should catch. It is committed, so the guard
   stands for as long as the safety arm carries symmetry.
3. **Injected-asymmetric-bug probe** (the direct test of the `CHOOSE` risk). A
   `LeakRevokedAsym` action (a throwaway during B3, reverted before that commit;
   since committed as a permanent control — see the Follow-up section) leaked a
   ghost-revoked, hence dead, cap into a *specific* non-`InitProc` process's
   cspace — a genuine `DeadNowhere` violation that singles out one model value
   and is reachable only after a real revoke, so its path traverses states the
   quotient collapses. Run under the safety constants both ways:

   | probe | result |
   |---|---|
   | without symmetry (control: bug is real) | `Invariant DeadNowhere is violated` (104 distinct) |
   | **with `SYMMETRY ProcSymmetry`** | **`Invariant DeadNowhere is violated`** (103 distinct) |

   The quotient does **not** hide the asymmetric violation. (TLC checks
   invariants on each generated orbit-representative before the seen-test, and
   `DeadNowhere` is itself symmetric, so a violating orbit cannot be silently
   canonicalised away — the probe confirms this holds in practice despite the
   asymmetric `CHOOSE` seed.)
4. **Liveness arm untouched.** `CapRevocation.cfg` (which names no symmetry)
   re-measures 503,070 / diameter 22 — byte-identical to its manifest pin, so
   `EventuallyRevoked`'s verdict is unchanged. SANY parses `CapRevocation.tla`
   clean (the two new operators add no obligations to any other cfg).

### Cost / CI-wall-clock judgement

* The safety arm's own run is **~36% faster** (6m26s → 4m07s with coverage; the
  CI `model-safety` job runs without `-coverage`, so proportionally faster
  still). It is a **separate parallel job**, so *total* CI wall-clock is
  unchanged — it remains gated by the pre-existing poles, not this arm.
* The `model` job gains one more negative control (the symmetric guard), which
  finds its counterexample in the first few BFS levels — a few seconds.
* **Honest framing:** like B2 this is fundamentally a **coverage / headroom**
  play, not a critical-path speedup. The dominant liveness arm
  (`CapRevocation.cfg`, the `model` job's pole) is untouched and gains nothing —
  symmetry is unsound under its `EventuallyRevoked` tableau. The win is (a) a
  real but off-critical-path speedup of the safety arm, (b) ~4.9M fewer
  redundant states explored, and (c) **de-risking B4**: this attempt proves the
  symmetry machinery sound against the asymmetric `CHOOSE` seeds, which is the
  exact structural risk B4 (`Permutations(CapIds)`, up to 24×) carries.

### Decision

**Adopted.** A sound ~1.68×/1.91× quotient with every verdict intact, the
diameter preserved, the negative controls — including a new symmetric guard —
still tripping, and an injected asymmetric bug still caught. The plan tagged B3
*adopt-if-measured, guarded*; the measurement and the four guards support
adopting. Recorded honestly: the safety factor came in below the plan's
anticipated ~2× because B2's larger constants enlarged the proc-symmetric region
(a measured interaction, not a regression).

### Follow-ups (out of scope here)

- **Threads symmetry.** B2 added `Threads={t0,t1}` to the safety arm, so a second
  size-2 set is now permutable. `Permutations(Procs) ∪ Permutations(Threads)`
  would compound for a larger reduction; deferred to keep this attempt to the
  plan's one-change-at-a-time discipline. The new `ProcSymmetry`/`NotifSymmetry`
  pattern and the symmetric-negative-control guard generalise to it directly.
- **B4** (`Permutations(CapIds)`, size 4, up to 24×) — the largest single
  reduction available and the lever to run the safety arm at `CapIds = 5,6` for
  deeper-CDT coverage. This attempt de-risks it: the `CHOOSE`-seed soundness
  concern (shared via `InitCap == CHOOSE`) is now empirically validated for the
  `Procs`/`Notifs` case with the exact guard methodology B4 needs (exact-factor
  check + symmetric negative control + injected asymmetric bug).
- **D1** hygiene (stray `*_TTrace_*` scratch in `tla/`) remains unrelated.

---

## Follow-up — promoting the throwaway controls to permanent form

B3 committed only the symmetric guard (Appendix §1) and ran the other two
artifacts as throwaways (Appendix §2, §3). A follow-up re-examined whether each
could become permanent; **both were promoted, in the form each warrants.**

**§2 — injected asymmetric bug → adopted as a committed negative control.**
Now in-tree as `SpecAsymBad`/`NextAsymBad`/`LeakRevokedAsym` in
`CapRevocation.tla`, driven by `CapRevocation_AsymBug.cfg` (`SpecAsymBad` at the
safety-arm constants under `SYMMETRY ProcSymmetry`, asserting `DeadNowhere`), and
wired into `scripts/tla-neg-controls.sh` (now eight controls). The original
throwaway rationale — "spec-surface cost" and B3's one-change-at-a-time rule — no
longer holds: `CapRevocation.tla` *already* commits two inline negative-control
bad-specs (`SpecBad`, `SpecNoGuard`) under its "Negative controls (committed)"
block, so `SpecAsymBad` is a third sibling of an established pattern, and B3 is
merged, so this is its own follow-up. The decisive reason to keep it is coverage
the committed §1 guard does **not** provide: §1 runs the *symmetric* `SpecBad`, a
bug present in every permutation-orbit member, so it survives even a wrong
quotient; only an *asymmetric* bug — one that singles out a non-`InitProc`
process and so breaks the symmetry premise — probes whether the declared symmetry
is **over-broad** (silently merging an asymmetric violating state with a
non-violating one). That is exactly the asymmetric-`CHOOSE`-seed risk, and the
same risk B4 (`Permutations(CapIds)`) will carry, so the guard de-risks B4. Cost
is ~103 states / a few seconds inside the existing `model` job, and it needs **no
manifest row** (negative controls are not coverage arms). Only the *Sym* variant
is committed as the standing guard; the *NoSym* reachability variant (Appendix §2)
stays a recorded one-time validation — it only proved the bug reachable, which the
guard does not need to re-assert on every CI run.

**§3 — no-symmetry re-derivation → adopted as a `--no-symmetry` harness flag,
not a committed cfg.** `scripts/tla-baseline.sh --no-symmetry [arm...]` now strips
the `SYMMETRY` line from each selected arm's cfg on the fly (to a temp sibling,
removed after the run) and reports the pre-quotient distinct/diameter without
asserting the post-quotient manifest pins — the folded-in form Appendix §3 itself
anticipated, so the before/after of a symmetry change is a one-flag re-run rather
than a manual cfg copy. It is deliberately **not** a committed `_NoSym.cfg`: that
is a measurement scaffold, not a soundness guard, and a committed *safety* NoSym
cfg would re-explore the full ~12.2M states (~6 min) the quotient removed and
need its own pre-quotient pin — standing cost for a number we only want
on-demand. The flag carries zero standing CI cost (nothing runs it automatically)
and reproduces 252 / diameter 8 for teardown and ~12,183,480 / diameter 28 for
safety.

---

## Appendix — validating a `SYMMETRY` declaration (the controls used, verbatim)

TLC never checks that a declared `SYMMETRY` is sound; a mis-scoped one silently
*hides* states and so hides bugs. The standing soundness monitor is therefore a
*negative control* — a deliberately-broken spec the symmetry must still catch.
This attempt used **two** kinds, recorded here verbatim as the worked example of
"how to prove a symmetry has teeth." Both have since been promoted to permanent
form (see "Follow-up — promoting the throwaway controls" above); the text below
is preserved as the record of the original attempt and is annotated where the
permanent form differs. The general recipe:

1. **Symmetric negative control** — the *real* bad spec, at the *same constants
   and the same `SYMMETRY`* as the arm being guarded, asserting the same
   violation. If the quotient ever silently drops the violating orbit, this stops
   tripping. Cheap (reuses an existing bad spec), so **commit it**.
2. **Injected asymmetric bug** — a bug that singles out one model value and so
   *breaks* the symmetry premise, reachable deep enough that its path crosses
   states the quotient collapses. The quotient must still report it. This is the
   direct probe of an asymmetric `CHOOSE` seed — the one control that catches an
   *over-broad* symmetry, which the symmetric kind above cannot. Originally run as
   a throwaway citing spec-surface cost; **now committed** as
   `CapRevocation_AsymBug.cfg` (the Follow-up section explains why that cost
   objection did not hold).

### 1. The committed symmetric control (in-tree)

`CapRevocation_Safety_NegControl.cfg` — `SpecBad` (interior-delete →
`LiveParent`) at the safety-arm constants under `SYMMETRY ProcSymmetry`, wired
into `scripts/tla-neg-controls.sh`. It is a permanent guard; see the diff. Of the
two patterns below, §2 has since been committed alongside it and §3 became a
harness flag (see the Follow-up section).

### 2. Injected asymmetric bug (`SpecAsymBad`) — since committed

Originally added to `CapRevocation.tla` (after `SpecNoGuard`) as a throwaway,
then reverted; **now committed verbatim** there, driven by the committed
`CapRevocation_AsymBug.cfg` and run by `scripts/tla-neg-controls.sh`:

```tla
\* Injected-asymmetric-bug probe: a genuine DeadNowhere violation that singles
\* out one model value — leak a ghost-revoked (dead) cap into a SPECIFIC
\* non-init process's cspace. Reachable only after a real revoke produces a
\* revoked cap, so the path traverses states the Procs quotient collapses.
LeakRevokedAsym ==
    /\ revoked /= {}
    /\ LET q == CHOOSE p \in Procs : p /= InitProc
           d == CHOOSE c \in revoked : TRUE
       IN cspaces' = [cspaces EXCEPT ![q] = @ \cup {d}]
    /\ UNCHANGED <<live, parent, queues, bindings, treport, revoked, revoking,
                   nlive, ncaps, pcbind, eopen>>

NextAsymBad == Next \/ LeakRevokedAsym

SpecAsymBad == Init /\ [][NextAsymBad]_vars
```

Two cfgs drove it. The symmetric one is **now committed** as
`tla/cap_revocation/CapRevocation_AsymBug.cfg` (the standing guard; its
header carries the rationale):

```
SPECIFICATION SpecAsymBad
CHECK_DEADLOCK FALSE
CONSTANTS
    CapIds = {c0, c1, c2, c3}
    Procs  = {p0, p1}
    Channels = {ch0}
    Threads = {t0, t1}
    Notifs = {nf0, nf1}
    QueueDepth = 2
    NULL = NULL
SYMMETRY ProcSymmetry
INVARIANT TypeOK
INVARIANT DeadNowhere
```

The reachability variant was a throwaway `..._NoSym_TMP.cfg`, byte-identical
**minus the `SYMMETRY ProcSymmetry` line** — it proved the bug is genuinely
reachable, so a "violated" verdict under symmetry means *caught*, not *absent*. It
was a one-time check (the committed guard does not need to re-assert reachability
on every CI run), so only the symmetric cfg was committed. To re-run both today,
the committed cfg covers the Sym side and `--no-symmetry` (Appendix §3, now a
harness flag) reproduces the NoSym side. The original throwaway pair ran as:

```sh
for v in NoSym Sym; do
  TLC_WORKERS=1 TLC_FLAGS="-fp 0 -fpmem 0.5" \
    bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla \
      CapRevocation_AsymBug_${v}_TMP.cfg
done
```

Both report `Invariant DeadNowhere is violated` (NoSym 104 distinct, Sym 103) —
the quotient does not hide the asymmetric violation. The key mechanism a
guideline should state: **TLC checks each invariant on the generated
orbit-representative *before* the seen-test, and a symmetric invariant
(`DeadNowhere`) cannot evaluate differently across an orbit — so a violating
orbit is never silently canonicalised away.** The probe confirms this holds in
practice despite the asymmetric `InitProc == CHOOSE` seed.

### 3. Clean before-number re-derivation — since folded into the harness

`scripts/tla-baseline.sh` asserts a single pinned count per cfg, so re-deriving
a *pre-symmetry* before-number after the cfg already carries `SYMMETRY` needs a
no-symmetry copy. The throwaway `tla/cap_revocation/CapRevocation_Teardown_NoSym_TMP.cfg`
was `CapRevocation_Teardown.cfg` minus its `SYMMETRY NotifSymmetry` line; running
it reproduced 252 / diameter 8 (the pre-quotient count), then was deleted. That
manual copy is **now folded into the harness** as the `--no-symmetry` flag this
section anticipated: `scripts/tla-baseline.sh --no-symmetry CapRevocation_Teardown`
strips the `SYMMETRY` line on the fly (to a temp sibling, removed after the run)
and reports 252 / 8 without asserting the post-quotient pin — a one-flag re-run.
It stays a flag rather than a committed `_NoSym.cfg` because a committed *safety*
NoSym cfg would re-explore the full ~12.2M states the quotient removed; the
Follow-up section gives the full rationale.

Of the three original artifacts, the symmetric control (§1) and the asymmetric
control (§2) are now committed in-tree, and §3 lives as the `--no-symmetry`
harness flag; the throwaway `*_TMP.cfg` copies were removed before commit.
