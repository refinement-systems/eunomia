# TLA+/TLC — design-level model checking on Eunomia OS

This work is licensed under a [CC0 1.0 Universal](https://creativecommons.org/publicdomain/zero/1.0) license.

This is the single, self-contained guideline for writing and checking TLA+
specifications on this project. It assumes familiarity with TLA+ syntax; it does
not teach the language. Every rule below is load-bearing: the suite's value
rests on a small set of soundness disciplines that TLC does **not** enforce for
you, and a measurement discipline that keeps "optimization" honest.

---

## 1. Purpose & scope

TLA+/TLC models **design-level state machines** — the protocol-level
interleavings and the reachable state space, the half of correctness that a
per-function deductive proof does not address. Three families live here:

- **Capability revocation** — the capability derivation tree (CDT), per-process
  cspaces, in-flight caps in channel queues, TCB binding slots, and the
  *stepwise* (preemptible, leaf-first) revoke walk interleaved with every other
  action (rev2§2.2, rev2§3.4, rev2§5.1).
- **The storage commit protocol** — superblock double-buffering, the WAL, the
  two-barrier commit, and crash recovery / replay (rev2§4).
- **The IPC reactor** — one channel's bounded FIFO queue plus its two
  notification words, modeling lost-wakeup and backpressure (rev2§3.5).

What TLA+ is **not** for here: it does not touch code. A model proves the
*design* is correct; it says nothing about whether the executable Rust refines
that design. That model-to-code gap is closed by a separate method (deductive
verification of an extracted function for all inputs). There is **no automatic
check** that the Rust conforms to the TLA+ — so a model that silently proves
*less* (a narrowed graph, a weakened property) still passes, and nothing
downstream notices. This is why the coverage and teeth disciplines below are not
optional polish; they are the only thing standing between a green run and a
vacuous one.

---

## 2. The specs and tooling in this repo

### Layout

Each subdirectory of `tla/` holds **one** base `.tla` module driven by **many**
`.cfg` files:

```
tla/cap_revocation/CapRevocation.tla   + 14 cfgs (two specs in one module: Spec, TSpec)
tla/commit_protocol/CommitProtocol.tla +  3 cfgs
tla/ipc_reactor/IpcReactor.tla         +  4 cfgs
```

A `.cfg` admits exactly one `SPECIFICATION`, so each role gets its own cfg naming
a different operator defined inside the shared module (`Spec`, `TSpec`,
`SpecBad`, `SpecNoGuard`, `SpecExistFair`, …). The filename suffix encodes the
role:

| suffix | role |
|---|---|
| *(base)* `Foo.cfg` | the headline positive arm |
| `_Safety.cfg` | invariant-only projection at **larger** constants (no liveness) |
| `_SafetyFloor.cfg` | the safety obligations re-run at the **liveness floor** constants |
| `_Teardown.cfg` | drives a second spec (`TSpec`) in the same module |
| `_NegControl`, `_NegLiveness`, `_NegFairness`, `_NegBackpressure`, `_NegLostWakeup` | negative controls — expected to **fail** |
| `_*Bad.cfg` | symmetric teeth-tests (`MoveSemanticsBad`, `ReportMonotoneBad`, …) |
| `_*AsymBug.cfg` | asymmetric symmetry-soundness guards (`AsymBug`, `CapAsymBug`, `ThreadAsymBug`) |

### cfg structure

```
SPECIFICATION Spec
CHECK_DEADLOCK FALSE
CONSTANTS
    CapIds = {c0, c1, c2, c3}
    Procs  = {p0, p1}
    Channels = {ch0}
    Threads = {t0, t1}
    QueueDepth = 2
    NULL = NULL
SYMMETRY SafetySymmetry         \* invariant-only arms ONLY — never with a PROPERTY
INVARIANT TypeOK
INVARIANT MoveSemantics
PROPERTY ReportMonotone
```

- CONSTANTS assign **model values** (barewords `c0`, `p0`, `NULL=NULL` — `NULL`
  is a model value, not a keyword). Model values catch type errors: a model
  value is unequal to any TLA+-expressible value, so an accidental `p+1` or
  `p=2` is an error, not a silent `FALSE`.
- `CHECK_DEADLOCK FALSE` is universal here — terminal states are legitimate
  (all caps retyped, all messages delivered, all notifs freed). The consequence:
  with deadlock detection off, a genuine lost-wakeup *deadlock* is caught **only**
  by the positive liveness `PROPERTY`. Never drop that `PROPERTY` line.
- `SYMMETRY` appears only on invariant-only cfgs (see §5).

### The coverage ledger: `model-manifest.tsv`

`tools/tla/model-manifest.tsv` is the TLC analogue of a trusted-base ledger.
Tab-separated: `name  spec_relpath  cfg  expected_distinct  expected_diameter`.

```
name                       spec_relpath                          cfg                          expected_distinct  expected_diameter
CapRevocation              tla/cap_revocation/CapRevocation.tla  CapRevocation.cfg            503070             22
CapRevocation_Safety       tla/cap_revocation/CapRevocation.tla  CapRevocation_Safety.cfg     635034             28
CapRevocation_SafetyFloor  tla/cap_revocation/CapRevocation.tla  CapRevocation_SafetyFloor.cfg 46599             22
CapRevocation_Teardown     tla/cap_revocation/CapRevocation.tla  CapRevocation_Teardown.cfg   132                8
CommitProtocol             tla/commit_protocol/CommitProtocol.tla CommitProtocol.cfg          413455             29
IpcReactor                 tla/ipc_reactor/IpcReactor.tla        IpcReactor.cfg               39                 13
```

- **distinct-states is the coverage metric.** A drop means TLC explored a
  smaller behaviour set — the model proves *less*. The harness asserts strict
  equality, so a silent coverage shrink fails the run.
- **diameter** is worker-invariant structural depth; it must not change for an
  equivalent model.
- Only **positive** arms are pinned. Negative controls have no row: TLC stops at
  the first counterexample/lasso, so there is no full distinct/diameter.
- The header documents the **canonical CONSTANTS** for each lock-step group so
  paired cfgs cannot silently drift. When a *sound* change alters the reachable
  set (adopting a symmetry, raising a constant), re-pin the row **and** update
  the header comment naming the change and the pre/post counts.

### Running a check

A SANY parse check (syntax/semantics only, no cfg):

```sh
bash tools/tla/tla-check.sh tla/cap_revocation/CapRevocation.tla
```

A TLC model check (cfg defaults to `<spec-basename>.cfg`):

```sh
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla CapRevocation_Safety.cfg
```

`tla-model-check.sh` cd's into the spec dir, routes `-metadir` to a scratch path
under the repo's `target/`, and **always** passes `-noGenerateSpecTE` (so a
violation never litters the source tree with `*_TTrace_*.{tla,bin}` — every
negative control would otherwise drop a pair per run; `-metadir` does **not**
redirect those). Pass a relative `TLC_METADIR` at your peril: the runner cd's
into the spec dir first, so a relative path resolves *there*, not at the repo
root — always use an absolute scratch path or accept the `target/` default.

Environment knobs (all wrap the **identical** state graph — they change only
parallelism, resourcing, instrumentation, scratch):

| knob | effect |
|---|---|
| `TLA_JAVA_OPTS` | JVM args (e.g. `-Xmx4g` for the heap-bound liveness tableau) |
| `TLC_WORKERS` | worker threads (default `auto`; CI pins a fixed count) |
| `TLC_FLAGS` | extra TLC flags after the class (`-coverage 1 -fp 0 -fpmem 0.5`, `-lncheck final`) |
| `TLC_METADIR` | scratch dir (defaults under `target/`) |
| `TLC_ASSERT_MANIFEST` | after a clean run, assert this cfg's distinct-states against its pinned row |

The toolchain is a vendored, sha1-pinned `tools/tla/tla2tools.jar` (the upstream
`v1.8.0` asset is a *rolling* release, so the committed `.sha1` is the real
version pin). Every CI job runs `shasum -c tla2tools.jar.sha1` first. A moving
checker silently invalidates every baseline — confirm the pin before trusting
any number, exactly as a verified crate confirms its prover binary.

### The CI time budget

CI runs **three parallel jobs**, so suite wall-clock is `max(jobs)`, not their
sum:

- `model` (4 workers, `-Xmx4g`): the positive arms with `TLC_ASSERT_MANIFEST=1`,
  the CapRevocation liveness arm with `TLC_FLAGS="-lncheck final"`, plus the
  **safety** negative controls (single-worker, for a deterministic short trace).
- `model-safety` (15-min timeout): `CapRevocation_Safety.cfg` and
  `CapRevocation_SafetyFloor.cfg`.
- `model-neg-liveness` (15-min timeout): the **liveness** negative controls,
  which are the slowest checks (one explores a 5-cap set).

Each job has a hard **15-minute cap**. Suite latency is the pole job
(`model`); a parallel coverage arm adds coverage "for free" on latency only
until it itself becomes the pole.

---

## 3. Writing specs

**Structure.** Each module declares `VARIABLES`, an `Init`, a set of named
actions, a `Next` disjunction, and the `Spec == Init /\ [][Next]_vars [/\ Fairness]`
formula. Where two independent lifetime mechanisms coexist (e.g. CDT moves vs
channel refcount teardown), use **two specs in one module** — `Spec` and `TSpec`
— each holding the other's variables `UNCHANGED` so neither multiplies the
other's state space.

**Invariants vs temporal properties.** A *state invariant* is a predicate over a
single state (`INVARIANT TypeOK`). A *temporal/action property* relates states
across a step or over a behaviour (`PROPERTY`). The distinction is load-bearing
for faithfulness (§4): a reconstruction/equality/recovery claim is a **step
relation** between `post'` and the inputs, and must be written as one — never as
a single-state invariant, which a no-op action body still satisfies.

```
\* state invariant: single-owner discipline (rev2§3.4)
MoveSemantics ==
    \A c \in live :
        Cardinality(ProcPlaces(c)) + Cardinality(QueuePlaces(c))
            + Cardinality(BindPlaces(c)) = 1

\* action property: a terminal thread report transitions at most once
ReportMonotone ==
    [][\A t \in Threads :
        treport[t] /= "running" => treport'[t] = treport[t]]_vars
```

**Fairness (WF/SF).** Weak fairness `WF` requires a step be taken if it remains
*continuously* enabled; strong fairness `SF` requires it if enabled *infinitely
often*. SF is strictly stronger as an assumption. Attach fairness only to
**genuine-progress** actions, never to blocking actions or an over-free action
(§4 trap 3). Pick the subscript carefully: an action that specifies only a
subset of the variables must use that subset, or TLC rejects `WF_vars`:

```
\* per-cap WF, subscript crVars (RevokeStep names no teardown-half prime, so
\* WF_vars would be rejected even though the two coincide under this Spec)
Fairness == \A c \in CapIds : WF_crVars(RevokeStep(c))
Spec == Init /\ [][Next]_vars /\ Fairness

EventuallyRevoked ==
    \A c \in CapIds : (c \in revoking) ~> (Descendants(c) = {})
```

Note `\A c : WF(A(c))` (each instance progresses) is **strictly stronger** than
`WF(\E c : A(c))` (some instance progresses) — do not let the per-instance form
degrade into the existential one (§6, §8).

**Reusing one base spec across many cfgs.** Defining an unused operator or
spec is **inert** for any cfg that does not name it: it adds no obligations and
cannot change any other arm's verdict or counts. This is what lets a single
module carry the real `Spec`, its negative-control twins, and the symmetry
operators side by side, each activated only by the cfg that names it. Factor
shared action disjuncts into a common operator so the twins stay in lock-step
(§4).

**Spec-writing efficiency rules** (do not change *what* is proven, only
time-per-state):

- **Quantify over the enabling set, not a superset then filter.** Enumerate a
  process's own caps, not every subset of all caps.
- **Construct small sets directly; never generate-huge-then-filter.**
- **Set-emptiness over a positive existential in an enabling guard** (§5).

---

## 4. The negative-control ("teeth") discipline

**This is the central rule of the suite.** Every safety invariant and every
liveness property earns its keep only with a **runnable negative control**: the
real action minus exactly one load-bearing conjunct, asserted to **fail** and
confirmed to reach a concrete bad state at a stated depth. A control that finds
**no** violation is the alarm, not the all-clear — it means the model is not
constraining what you think.

### Three faithfulness traps

A model can pass a control while checking nothing. Three traps do exactly that:

1. **A state invariant standing in for a relational property.** Reconstruction /
   equality / recovery properties are step relations (`post'` vs the inputs), not
   single-state invariants. Asserting the recovery ingredients are merely
   *durable* — instead of asserting the post-state *equals* the inputs across the
   step — lets a no-op action body pass while doing nothing.
2. **An over-permissive action makes the defect unexpressible.** Model each
   action behind the **same** enabling guard the code runs, and gate a wait/block
   on the **exact** primitive the loop sleeps on (a notification word), never a
   coupled proxy (queue-empty). A freely-enabled drain delivers the message
   regardless of wakeups, so a lost-wakeup control can never bite.
3. **Fairness on an over-free action masks the defect.** Fairness on an
   over-free action *compels* the system down the masking path, so the defect
   never manifests and the liveness check looks satisfied. Attach weak fairness
   only to genuine-progress actions.

The THEATRE/FAITHFUL/SpecBad idiom — keep the deliberately-broken action next to
the faithful one to document the trap, and define the control as the real `Next`
minus exactly one conjunct:

```tla
\* THEATRE: drain enabled on queue non-empty -> lost-wakeup is unexpressible
DrainBad == Len(queue) > 0 /\ ...
\* FAITHFUL: gate on the primitive the loop actually waits on
Drain    == woken = TRUE   /\ ...
\* control: real Next minus ONE load-bearing conjunct, asserted to FAIL
SpecBad  == Init /\ [][NextBad]_vars   \* must reach a concrete bad state
```

This is the design-tier analogue of the host-test teeth discipline used for
verified trusted seams: a transient mutation that the differential oracle must
catch. The negative control proves the property has teeth, one tier up.

### Lock-step: factor shared disjuncts into a common operator

A control is a proof a guard has teeth **only if it is otherwise identical to
`Next` except for the one swapped conjunct.** If the shared disjuncts drift
between `Next` and a twin, the twin silently stops tracking `Next` and loses its
value. Make the lock-step **structural**: factor the shared disjuncts into a
`CommonActions` operator and write each relation as its few varying disjuncts
followed by `\/ CommonActions`, preserving any trailing `UNCHANGED` exactly.

```tla
CommonActions ==
    \/ \E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(p, ch, cs)
    \/ \E p \in Procs, ch \in Channels : Receive(p, ch)
    \/ ...                                  \* the shared disjuncts

Next ==        \* real Copy + real RevokeStep
    /\ \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
       \/ \E c \in CapIds : RevokeStep(c)
       \/ CommonActions
    /\ UNCHANGED tdVars

NextBad ==     \* swaps RevokeStep -> RevokeStepBad (drops the IsLeaf filter; LiveParent fails)
    /\ \/ \E p \in Procs, s, d \in CapIds : Copy(p, s, d)
       \/ \E c \in CapIds : RevokeStepBad(c)
       \/ CommonActions
    /\ UNCHANGED tdVars

NextNoGuard == \* swaps Copy -> CopyNoGuard (drops the derive-guard; EventuallyRevoked livelocks)
```

Disjunction order is irrelevant to TLC, so listing the varying disjuncts first
is free. Note there may be **two** varying positions (one twin swaps `Copy`, the
other swaps `RevokeStep`) — factor only the genuinely shared disjuncts, and keep
each varying position explicit.

### Verdict convention and the runner

`scripts/tla-neg-controls.sh` runs every committed control and **inverts** the
verdict: a control that makes TLC exit 0 is the failure it reports. For each it
asserts TLC exits non-zero and the log shows a violation, then best-effort-greps
the expected name. Exit codes distinguish the kind:

```
ok  CapRevocation_NegControl.cfg    LiveParent violated as expected (exit 12)   # safety invariant
ok  CapRevocation_NegLiveness.cfg   EventuallyRevoked violated as expected (13) # liveness/temporal
```

Liveness violations print only a generic "Temporal properties were violated", so
a missing property name **warns** rather than fails; the exit code is the gate.

### Adding a control

Purely additive: add one `CONTROLS` entry to `scripts/tla-neg-controls.sh`
(`spec|cfg|expected_name|kind`); add a `.cfg` naming the twin via
`SPECIFICATION`, with `CHECK_DEADLOCK FALSE`, the `INVARIANT`/`PROPERTY`, and
**no** `SYMMETRY` for a liveness control; if the twin spec is new, define it as
an inert operator in the shared `.tla`. The CI count is `${#CONTROLS[@]}`, so no
CI or count edit is needed. Negative controls need no manifest row. Re-run the
whole suite after any change and confirm every control still trips — and, when
you drop an invariant from an arm, confirm its obligation is still guarded
**elsewhere** by a control that still trips.

### Verify the harness itself has teeth

Before trusting a "byte-identical" refactor, confirm the *measurement* would
catch a botched rewrite: deliberately delete one shared disjunct from
`CommonActions`, re-run, and confirm distinct/diameter move and the manifest
assertion trips "COVERAGE REGRESSION". This is a **throwaway** sanity check —
record it, do not commit it.

---

## 5. Model-checking optimization

State-count dominates: reducing the graph 10× is common; shaving per-state cost
buys 5–10%. So shrink the graph first — but only by **sound** moves, each paired
with its soundness caveat.

### Symmetry

A `SYMMETRY` declaration quotients the state graph by permutations of
interchangeable model values; the quotient factor approaches the permutation
group order. Define one operator per permutable set as `Permutations(S)`, and
combine disjoint sets with a **union**:

```tla
ProcSymmetry   == Permutations(Procs)
CapSymmetry    == Permutations(CapIds)
ThreadSymmetry == Permutations(Threads)
SafetySymmetry == ProcSymmetry \cup CapSymmetry \cup ThreadSymmetry
\* cfg:  SYMMETRY SafetySymmetry
```

A sound symmetry quotient is the **one** case where fewer distinct states is
**not** a coverage regression — it checks the same behaviours through fewer orbit
representatives. The tells that it is sound: every verdict unchanged **and the
diameter unchanged** (a wrong quotient moves the diameter or flips a verdict).
Generated states drop **more** than distinct (it also avoids regenerating
permuted successors). When you adopt a quotient, re-pin the manifest row to the
post-quotient orbit count and note that it is an orbit count, not a coverage
loss.

**Soundness caveats — symmetry is not free, and TLC validates none of it:**

- **Unsound under any temporal/liveness property.** The quotient can collapse
  the lasso a fairness/eventually property distinguishes. So `SYMMETRY` may live
  **only** on an invariant-only cfg — never on one with a `PROPERTY` that is a
  liveness property. This forces the safety/liveness split (§2, §6).
- **An action property checked under symmetry is sound only if the property is
  itself symmetric** over the permuted set (a `\A t \in Threads` universal naming
  no specific thread qualifies, even when `Threads` is permuted). The general
  rule supersedes "the property only ranges over a non-permuted set": symmetry is
  sound for a property iff the property is invariant under the group.
- **The interchangeability premise.** A set is permutable only if every action,
  invariant, and property references it **uniformly** (no hard-coded member, no
  `CHOOSE` over it), `Init` seeds all members identically, and any structure
  indexed by it (the CDT `parent` forest, a WAL record's ref field) permutes
  *with* the ids. The classic silent-unsoundness hazard is an **asymmetric
  `CHOOSE` over a symmetric set** (`InitCap == CHOOSE c \in CapIds : TRUE`):
  structurally risky, not automatically disqualifying (the `CHOOSE` is itself
  symmetric and TLC canonicalises it consistently, and TLC checks each symmetric
  invariant on the orbit representative *before* the seen-test, so a violating
  orbit cannot be canonicalised away) — but it must be confirmed **empirically**
  at the actual constants, never assumed. Confine such `CHOOSE` to `Init`.

**The mandatory teeth for any symmetry — adopt on adversarial controls, never on
a passing run.** Four guards, each catching a different failure:

1. verdicts **and** diameter unchanged before/after;
2. a **symmetric** negative control (a bug present in every orbit member) still
   trips — proves the property is genuinely evaluated under the quotient, but
   **cannot** probe over-breadth (a symmetric bug survives even a wrong quotient);
3. an **injected asymmetric** bug singling out one model value (via `CHOOSE`)
   still caught — the **only** control that catches an over-broad quotient that
   would merge an asymmetric violating state with a non-violating one;
4. the liveness arm confirmed untouched.

Verify the asymmetric bug is genuinely **reachable without** symmetry first
(otherwise "still violated under symmetry" proves nothing). Commit the
**under-symmetry** controls (the standing soundness monitors); keep the
no-symmetry reachability cross-check as a throwaway diagnostic, not committed —
reproducible on demand by stripping the `SYMMETRY` line (the baseline harness has
a `--no-symmetry` flag for exactly this). Every permuted axis gets its own
asymmetric control; every property checked under a quotient (invariants **and**
action properties) gets its own control.

**Expectations — use Burnside, not the group order.** The realized factor is
almost always far below `|S|!` because of fixed points (states a permutation maps
to itself). By Burnside `orbits = (1/|G|) Σ_g |Fix(g)|`. A near-ideal factor
means few self-symmetric states; a large fixed-point mass (states where not all
model values are "live" at once) drags the factor down. The factor also **shifts
with the constants** — restoring residence axes enlarges the symmetric region and
*lowers* the factor — so report the measured factor against the anticipated one
and explain the interaction; it is sound, just smaller.

### State constraints & VIEW

- A **state/action constraint** stops TLC exploring successors of a state, i.e.
  you are now checking a *different* behaviour graph. Fine as an honest modeling
  bound or for fast dev-time iteration (`TLCGet("level") < N`), but it can hide
  the very state where a violation appears. Never use one as a final correctness
  optimization for a liveness property. None are used in this suite.
- A **`VIEW`** changes state identity to a projection. Powerful, but it can
  silently merge genuinely-distinct states and break enabledness/fairness/cycle
  existence. Reject any `VIEW` that erases state a property observes (an action
  property's variables, ghost variables a property names). None are used here.

### Quantify over reachable sets, not full domains

In an action body, quantify over the enabling assignments directly (`cs \in
SUBSET cspaces[p]`), not a superset you then filter (`cs \in SUBSET CapIds` with
a `cs \subseteq cspaces[p]` guard). Caveat: a bound cannot reference an earlier
variable in the **same** quantifier list, so split into nested existentials:

```tla
\* parses; the flat `\E p \in Procs, cs \in SUBSET cspaces[p]` does NOT
\/ \E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(p, ch, cs)
```

### A positive existential in an enabling guard inflates generated states

When TLC expands an action whose enabling guard contains a **positive** `\E x \in
S : P(x)`, it **branches per witness** — a state with `k` witnesses yields `k`
identical successors (all counted in generated, collapsing to one distinct).
Write the test as a value-level set comparison instead, which evaluates to one
Boolean and generates one successor. A **negated** existential (`~\E` ≡ `\A`)
costs nothing — there is no witness to branch on. Keep the genuine recursive set
where the real subtree is enumerated; rewrite only the emptiness-test guards:

```tla
\* SLOW (branches per witness):  guard contains  \E x \in CapIds : parent[x] = c
\* FAST (single Boolean):        Children(c) /= {}   (or = {})
Children(c) == {x \in CapIds : parent[x] = c}
```

Leave a comment recording **why** the set-comparison form is used, so a
well-meaning "simplification" cannot reintroduce the cost.

### -workers and other flags

- `-workers` parallelizes the identical graph at **zero** coverage risk; worker
  count cannot change the behaviour set. The CLI defaults to **one** worker —
  always pass `-workers`. Caveat: with `>1` worker, **generated**-states and the
  reported counterexample are nondeterministic (and reported depth can wobble);
  **distinct** and **diameter** stay worker-invariant.
- `-fp 0` pins the fingerprint polynomial (later TLC versions default to a random
  one) so generated-state counts are comparable across runs.
- `-lncheck final` — see §6.

### Coverage to find dead transitions

`-coverage <min>` prints per-action generated:distinct cost. Two payoffs: an
action with **zero** new-state count never fires (a dead transition — likely an
unsatisfiable guard or modeling bug); and it ranks where TLC spends time so you
optimize the right action. Caveats: `Next` itself is not reported (TLC splits its
disjuncts), `ASSUME` coverage is not collected, and intermediate snapshots are
sampled at wall-clock boundaries — **only the final per-action block is
deterministic**. To profile, split `Next` into **named** actions; do not hide all
work behind one giant disjunction. Per-action source-line labels shift when you
add an operator above them — that is cosmetic, not a metric change.

---

## 6. Liveness checking

Liveness reasons about whole infinite behaviours, not bad finite prefixes, and
TLC implements it with a **sequential** Tarjan SCC search. Consequences:

- **Liveness is the critical-path pole.** The `EventuallyRevoked` arm dominates
  CI wall-clock; the cost is the **tableau/fairness product** (~4× the reachable
  set), which symmetry could never reduce — symmetry is unsound here and absent.
- **`-workers` parallelizes only state generation, not the SCC pass.** The
  speedup is sublinear (≈3.6× at 4 workers), and a higher worker count helps only
  up to the runner's core count. A high generated:distinct ratio (≈9.6:1) is the
  tell the arm may be *generation*-bound rather than SCC-bound — profile before
  choosing a lever.

**Levers, ordered by worker-robust payoff:**

1. **`-lncheck final`** — by default TLC runs the SCC pass *periodically* as the
   graph grows; `final` defers it to a single pass over the complete tableau.
   Behaviour-preserving (distinct/generated/diameter and verdict unchanged), and
   it eliminates the intermediate passes. Two gates before keeping it: confirm
   the full tableau still fits peak heap, and confirm the negative-liveness
   control still livelocks **under** the flag. **Scope it per-invocation to the
   passing liveness arm only — never job-wide.** On a *failing* spec, `final`
   forfeits the early-exit and balloons livelock detection ~100× (the controls
   rely on periodic checking to fail fast). Its payoff is worker-dependent and
   bounded at the CI worker count, so measure both worker counts and keep it only
   if it helps at the CI count.
2. **Trim redundant per-state invariants from the liveness cfg** — the more
   worker-robust win, because per-state invariant evaluation scales with state
   count regardless of worker count. Sound **only** when each dropped invariant
   is re-homed on a safety arm at strictly **larger** constants whose reachable
   set embeds the liveness floor as a special case. Keep `TypeOK` as the cheap
   well-typedness floor so the property cannot pass **vacuously** on a malformed
   state. distinct/generated/diameter stay byte-identical (invariant evaluation
   does not change the next-state relation). Mechanize the subsumption with a
   dedicated floor cfg (the safety obligations re-run at the floor constants),
   because a subsumption *argument* is not a standing check and goes stale.
3. **Resourcing last** — heap/`-fpmem`/worker count are the lowest-payoff levers,
   adopted only on a measured win. The SCC pass is sequential, so it does not
   parallelize.

**Pitfalls found:**

- **Trimming invariants from a liveness cfg** is sound only with the re-homing
  caveat above; without it you silently drop an obligation.
- **Fairness reformulation is theorem-touching.** Weakening `\A c : WF(A(c))` to
  `WF(\E c : A(c))` does **not** make the check faster for free — it changes the
  theorem and typically **livelocks** (existential WF forces only *some* instance
  to progress, so one element can starve). Reject the weakening; **harvest** the
  gap as a committed fairness negative control proving the per-instance fairness
  is load-bearing.
- **Probes that touch theorems without earning their keep get reverted.** A
  ghost-variable abstraction (freezing a state variable) shrank the graph but
  changed what is proven and was load-bearing for other arms — recorded as a
  precise measurement and reverted, leaving the real spec byte-identical.

**Symmetry, restated:** never put `SYMMETRY` on a cfg with a liveness `PROPERTY`,
and never put a liveness/livelock control under `SYMMETRY`. The negative
fairness/liveness controls must keep TLC's **default periodic** checking so they
fail fast.

---

## 7. Performance & measurement discipline

**Correctness and coverage outrank checker speed.** A faster run that proves
*less* is a regression; a slower one that proves *more* is correct. Never weaken
a spec, drop an obligation, loosen a property, or narrow constants to go faster.

**Two kinds of change, and the bar for each:**

- A **behaviour-preserving** change (expression rewrite, action refactor,
  resourcing, `-workers`, `-lncheck final`) must leave **distinct, generated, and
  diameter byte-identical** and the verdict unchanged. If any of the three moves,
  the change is theorem-touching by construction.
- A **theorem-touching** change (fairness, symmetry, abstraction, `VIEW`,
  constraints) changes what is proven; it must keep the verdict, keep every
  control tripping, be flagged explicitly, and be reverted if it only makes the
  property pass by weakening it. These probes are **null-by-default**: the
  deliverable is the recorded evidence (a threshold, a multiplier, a harvested
  control), not adoption.

**Measure deterministically, not by wall-clock:**

- **Cold runs only** — wipe TLC scratch first; a warm fingerprint set is the
  false-green of a stale cache.
- Pin the whole environment: the sha1-verified vendored jar, a pinned JDK,
  `-fp 0 -fpmem 0.5 -coverage 1`, a fixed host. Run **serially** on a quiet
  machine; parallel TLC runs destroy timing determinism.
- **distinct and diameter are worker-invariant** and authoritative for
  correctness/coverage. **generated** is the deterministic performance proxy but
  only at **`-workers 1`**. Run two passes: `-workers 1` for the deterministic
  generated count and per-action coverage; the CI worker count for the
  representative wall-clock. **Wall-clock is advisory and noisy** — judge by the
  deterministic counts.
- The gold standard for a semantics-preserving rewrite is **byte-identical
  distinct AND generated AND diameter** at `-workers 1 -fp 0`, plus identical
  verdicts and all controls tripping in lock-step.
- **Re-derive the before-number freshly on the merge-base** of your work — never
  trust the manifest table or a saved number; a merge or rebase moves the base,
  so re-establish it on the merged tree. `scripts/tla-baseline.sh` automates the
  cold A/B run over the manifest; there is no committed baseline.
- **One change at a time**, so a measured delta is attributable.
- **Report null results honestly.** A change that provably does strictly less
  work is non-regressing, but that does not entitle you to claim a speedup the
  measurement cannot see; adopt it on the axis it actually moves (readability,
  soundness) and say plainly there is no speedup. A null is data — record it so
  the dead-end is not re-tried.

**Honest CI accounting.** "Optimization" of this suite has repeatedly meant
**more coverage financed by compute**, not an end-to-end speedup. Report
wall-clock (the latency a PR waits = `max` of the parallel jobs) and total
compute (core-seconds) **separately**. Never present a *sum* of parallel jobs as
a latency, and never present added coverage as the existing work getting faster.
A symmetry quotient off the critical path is **headroom**, not a speedup — it
lets an arm later run at larger constants within the same budget.

**The CI cap is real — spend headroom only after re-measuring.** Each job has a
15-minute cap. A symmetry quotient pays only when the group grows as fast as the
state space. Worked example: raising the CapRevocation safety arm to **CapIds=5**
**blows the 15-minute cap** (>25-min local / >50-min CI). The CDT forest count
explodes combinatorially from 4→5 caps while the `Procs × CapIds` group grows
only from order 48 to 240 — nowhere near enough to absorb it. A prior
"near-zero cost" estimate for this bump was **wrong**; always re-measure the
budget on a cold run before spending banked headroom. The arm stays at CapIds=4.

---

## 8. Pitfalls & dead-ends (do not re-attempt)

- **`SYMMETRY` on a liveness arm** — unsound; collapses lassos. Hard reject.
- **Trusting a passing symmetric run** — TLC validates no symmetry; only the
  committed symmetric + asymmetric controls (run under the same symmetry) prove a
  quotient has teeth and is not over-broad.
- **Asymmetric `CHOOSE` over a symmetric set outside `Init`** — breaks
  interchangeability. Confine to `Init` and validate empirically.
- **A floor result mistaken for a theorem.** A liveness property passing at the
  smallest model can be a false negative one step below a failure threshold.
  Derive the minimum model size that could exhibit the counterexample by hand and
  check *there* (the existential-WF weakening passes at CapIds=4 but starves at
  CapIds=5).
- **Positive existential in an enabling guard** — branches per witness, inflates
  generated states; use set-emptiness (§5).
- **Optimizing a disjunct read off coverage** — coverage groups all `Next`
  disjuncts under one source location, so per-disjunct cost is invisible;
  identify the true hot path (the high gen:dist source) first. Shrinking the
  `Send` subset enumeration was a measured **null**.
- **Raising the liveness arm's residence axes** (Threads/QueueDepth) — they were
  trimmed to fit the heap-bound tableau; they are a floor, not slack. Bigger
  constants belong on the separate safety arm.
- **Fusing the stepwise revoke into an atomic action** — the interleaving *is*
  what `LiveParent` checks at each mid-revoke state. Fusing hides the concurrency
  property.
- **Removing a load-bearing variable from a shared module** — a declared variable
  is always part of the state fingerprint; there is no per-cfg opt-out. A
  liveness-only removal would fork the module and silently check a different state
  machine than the safety arm. Reject even when the abstraction is valid for the
  property; abstraction is a proof obligation, not a speed knob.
- **`-Xmx` / `-fpmem` raises on a small live set** — measured null/regression
  (8g was *slower* with higher variance; `-fpmem` was a wash). Workers beyond the
  runner's core count oversubscribe.
- **CapIds=5 on the safety arm** — out of the 15-minute budget (§7). Do not
  re-propose it.
- **Optimizing a sub-second arm** (IpcReactor: 39 distinct, diameter 13) — there
  is nothing to win; wall-clock is JVM/TLC startup. Confirm an arm is the pole
  before spending effort.
- **Stale baseline numbers** — state counts drift with the jar; a number copied
  from a prior report can be wrong by a factor. Re-derive cold.

---

## 9. Comment & doc discipline for specs

Comments in `.tla`/`.cfg`/scripts/CI describe **what is**, not what was or was
removed. They may reference only `doc/spec` and `doc/guidelines`, with a revision
number (`rev2§3.4`); never a temporary working report. No phase markers, no
deletion notices.

- State load-bearing rationale, not narrated history. A constants comment should
  justify the **current floor** ("4 caps is the minimum that builds a multi-level
  CDT subtree reaching all three residences; Threads/QueueDepth held at 1 to keep
  the tableau within heap"), not recount the model's evolution.
- When a non-obvious encoding is chosen for a measured reason (set-emptiness over
  `\E`), record **why** so it is not "simplified" back. This is the allowed
  "document a surprising path-not-taken" case.
- When a symmetry or trim changes *why* a check is sound, update the in-spec
  rationale comment to the now-applicable argument — and let the new negative
  control be the runnable confirmation of that argument.
- CI comments rot: when a PR changes a model's constants, symmetry, or runtime,
  update every comment that quotes the old numbers/behaviour.
- Controlled duplication is preferred over a forbidden cross-file "see X" pointer:
  if the same rationale serves three distinct readers (the cfg, the manifest, the
  CI comment), state it inline in each rather than pointing across files.
- `cargo fmt` does not apply to TLA; there is no formatter gate for these files.
