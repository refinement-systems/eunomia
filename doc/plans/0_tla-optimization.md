# TLA+ / TLC model-checking optimization plan

*Intermediate working document (doc/plans). It records the findings of the
TLC-optimization effort so they can seed a future guideline. Per the
project's comment discipline it is temporary, will be removed, and must not
be referenced from code, specs, or guidelines.*

The project model-checks four committed configurations on every PR (the CI
`model` job, `.github/workflows/ci.yml:54-60`), all through
`tools/tla/tla-model-check.sh`. The job uses **TLC only** — no Apalache, no
Toolbox. Apalache is explicitly out of scope here.

## Governing rules (the acceptance test for every change below)

These are hard constraints; an item that violates one is rejected outright.

1. **No change to what is verified.** The set of behaviours each checked
   invariant / property can observe must be unchanged. Unlike Verus there is
   **no automatic check that the executable Rust conforms to the TLA+**, so a
   model edit is judged against the real code (§4 maps each property to its
   `kcore` / `cas` / `ipc` mechanism). A change that narrows coverage —
   shrinking a load-bearing constant, an unsound `SYMMETRY`/`VIEW`, fusing an
   interleaving a property depends on — is a regression even if every run
   still passes, exactly as a Verus change that silently drops an obligation
   is.
2. **Measure every change** against *both* the original baseline *and* the
   state after the previously-accepted change (§1). Reject any attempt that
   regresses the metrics.
3. **Reject perf changes that significantly hurt readability**, and **reject
   cosmetic changes that significantly hurt perf.** Genuine grey-area
   trade-offs (a real win one way, a small cost the other) are left to
   implementer judgement and must be called out in the change description.

## Where the cost actually is (measured reference baseline)

Measured locally on an 8-core machine with the vendored
`tools/tla/tla2tools.jar` (v1.8.0-era), single worker; distinct/generated/
diameter are deterministic and machine-independent, wall-clock is advisory.

| cfg (what it checks) | distinct | generated | gen:dist | diam. | wall (1 wkr) | wall (8 wkr) |
|---|---:|---:|---:|---:|---:|---:|
| **`CapRevocation.cfg`** (Spec: 6 safety inv + `ReportMonotone` + `EventuallyRevoked` liveness) | **503,070** | **4,831,322** | **9.6:1** | 22 | **472 s** | **104 s** |
| `CapRevocation_Teardown.cfg` (TSpec: 4 safety inv) | 252 | 1,747 | 6.9 | 8 | ~1 s | <1 s |
| `CommitProtocol.cfg` (5 safety inv + `RecoverReconstructs`) | 6,886 | 18,781 | 2.7 | 21 | <1 s | ~1 s |
| `IpcReactor.cfg` (4 safety inv + `EventuallyDelivered` liveness) | 39 | 59 | 1.5 | 13 | ~1 s | ~1 s |

**The entire CI model-checking cost is `CapRevocation.cfg`.** The other three
together are under three seconds and below TLC's own start-up noise — they are
*not* optimization targets and any "speedup" of them would only risk shrinking
coverage. Two structural facts about the dominant run drive the whole plan:

* Its **9.6:1 generated:distinct ratio** is the signature of permutation-
  equivalent states being regenerated — the case `SYMMETRY` is built for.
* It is dominated by the **`EventuallyRevoked` liveness tableau**, and *TLC
  symmetry is unsound in the presence of a liveness property.* So symmetry
  cannot be applied to the run that is actually expensive. The realistic
  wall-clock levers on the critical path are therefore **`-workers`**,
  **expression cost**, and **heap/fpmem resourcing**; symmetry's value is
  *coverage headroom on a separated safety arm*, not critical-path speed.

> Numbers to reconcile once the toolchain is pinned (§2 A2) and
> `tla-baseline.sh` (§2 A6) exists: the auto-memory records `IpcReactor=19`
> (the current spec measures **39**), and the survey's adversarial runs saw
> different `NegControl`/`Teardown` counts than its assessment runs (a
> jar/measurement difference). Re-derive all before-numbers locally; do not
> trust the table above as a committed baseline.

---

## 1. Establishing the performance baseline

This mirrors the Verus proof-perf discipline in `CLAUDE.md`
(`scripts/verus-baseline.sh`, "measure every proof change, correctness
first"). The mapping is exact:

| Verus | TLC analogue |
|---|---|
| obligation count / `verification results:: N verified` | **distinct states found** — the coverage metric; a drop means you proved *less* |
| `rlimit` (deterministic, cold-run) | **distinct states + diameter** (worker-invariant) for correctness; **generated states** (under pinned flags) for the speed claim |
| wall-clock ms (noisy, advisory) | TLC's `Finished in …` wall-clock (advisory) |
| `--time-expanded` per-function SMT breakdown | **`-coverage <min>`** per-action generated/distinct breakdown |
| `cargo clean` (no stale cache) | **cold run**: delete `states/` + `*_TTrace_*` first (a checkpoint/warm fingerprint set is the false-green equivalent) |
| pinned Verus binary + toolchain | pinned `tla2tools.jar` bytes (§2 A2) + JDK major |

### What to capture, per cfg

* **distinct states** — the primary coverage number (the obligation-count
  analogue).
* **generated states** — work done; comparable only under identical
  `-workers` and `-fp`.
* **diameter** (`depth of the complete state graph`) — worker-invariant
  structural metric; must not change for an equivalent model.
* **wall-clock** — advisory.
* **per-action `-coverage`** — attributes generation cost to the hot disjunct
  (e.g. `RevokeStep`/`Copy`/`Send`), the data that *justifies* an
  expression-level change.
* For the **liveness** cfgs: the `-Xmx` the run needs and whether it OOMs. The
  liveness tableau is heap-bound and is the scaling wall here, not disk.

### Determinism controls (the A/B harness)

1. **Never change the cfg `CONSTANTS` as part of a perf experiment** — the
   constants *are* the model scale, i.e. what is proven. Record the canonical
   constants alongside the expected distinct count.
2. **Pin a fixed worker count `K`** on *both* arms (not `auto`, which floats
   with host cores). `K>1` is fine for distinct/diameter (worker-invariant)
   but makes generated-states and the reported counterexample
   nondeterministic, so either use `K=1` (fully deterministic, slower) or the
   *same* fixed `K` on both arms.
3. **Pin `-fp 0`** (fingerprint polynomial) on both arms so the hash/collision
   profile is identical.
4. **Cold runs only**: `git clean -fdX tla/` (or a fresh `-metadir`) before
   each run.
5. **Pinned toolchain**: identical jar bytes (verify against
   `tools/tla/tla2tools.jar.sha1`) and JDK major on both arms.

### The coverage-is-the-obligation-count rule

A distinct-state **reduction is only a win when it is a sound quotient** of
the state space (a valid `SYMMETRY` over genuinely interchangeable model
values, §2 A-tier C1/B4). A distinct-state reduction from **shrinking a
constant, a `VIEW`, fusing actions, or a bad symmetry set is a coverage
regression and is rejected** — the dividing line between the adopt items and
the reject guardrails (§3). For a change that is *meant* to be
semantics-preserving (an expression rewrite), the bar is stronger: distinct
*and* generated counts must be **byte-identical** to the control; if they
move, it changed behaviour.

### Mechanize it: `scripts/tla-baseline.sh`

Add the TLC analogue of `scripts/verus-baseline.sh` (§2 A6): for each
model/cfg it cleans scratch, runs at pinned `-workers K -fp 0 -coverage 1`,
captures distinct/generated/diameter/wall-clock + the per-action table to
`target/tla-baseline/` (gitignored, like `target/verus-baseline/`), and prints
a summary plus the slowest actions.

### Per-attempt protocol (mirrors the Verus rule in `CLAUDE.md`)

* **Re-derive the before-number freshly** from the base of your work (`git
  stash` your edits or check out the merge-base, run `tla-baseline.sh`). There
  is no committed baseline; re-establish it after any merge/rebase.
* Apply the change, re-run on byte-identical constants/jar/workers/`-fp`.
* Accept only if it **measurably reduces** generated-states / wall-clock (or
  at least does not regress them) **AND leaves distinct-states, diameter, and
  every invariant/property verdict unchanged** (or, for a sound quotient,
  reduces distinct by exactly the expected factor with all verdicts intact).
* Judge correctness-preservation by the deterministic worker-invariant metrics
  (distinct, diameter) and the verdict; judge speed by generated-states /
  per-action cost under pinned flags; treat wall-clock as advisory. Revert any
  measured regression.

---

## 2. Prioritized improvements

Ordered most-promising first (impact ÷ (risk + effort)). Each is tagged
**self-contained?**, **risk to verified properties**, **readability**, and a
recommendation: **adopt** / **adopt-if-measured** / **investigate** /
**reject**. Sequencing matters: the Tier-A tooling substrate must land first
because everything else is measured through it, and the negative controls must
be wired into CI *before* any symmetry is adopted (they are the only standing
check that a symmetry is sound — TLC never validates one itself).

### Tier A — tooling substrate (land first; unblocks measurement and the rest)

**A1. `-workers` (default `auto`, env-overridable) + a TLC-flag passthrough in
`tla-model-check.sh`.**  *adopt — highest impact/(risk+effort) in the plan.*
The runner calls TLC with no `-workers`, so every run (local and CI) is
single-threaded. Adding `-workers` parallelises exploration of the *identical*
state graph checking the *identical* properties. Measured: `CapRevocation.cfg`
**472 s → 104 s (4.5×)** at 8 workers; the three small models are unaffected.
The same edit should add a generic flag passthrough (e.g. `TLC_FLAGS`) so A4/
A5 can reuse it.
*Risk:* **none** — worker count cannot change the behaviour set; the only
liveness/symmetry caveat does not apply because no symmetry is declared here.
*CI note:* pin a **fixed** `K` (e.g. 2–4, matching `ubuntu-latest` vCPUs), not
`auto`, for reproducibility, and carry the determinism caveat (A1's twin
below). *Readability:* neutral. *Self-contained:* yes.

> Determinism caveat (document alongside A1): with `K>1` TLC interleaves
> expansion across threads, so the reported counterexample and the generated-
> state count become nondeterministic; distinct-states and diameter stay
> worker-invariant. Hence the baseline harness pins `K` and `-fp` (§1).

**A2. Make CI use the byte-identical vendored jar (or verify the download
against `tla2tools.jar.sha1`).**  *adopt.* CI `curl`s the jar from the rolling
`v1.8.0` tag (`ci.yml:43-46`) whose asset is *replaced* on upstream pushes,
while a pinned jar is vendored at `tools/tla/tla2tools.jar`. A moving
toolchain under a formal gate invalidates every baseline the way a Verus
version bump does — so this ranks directly under A1, *before* any measurement
is trusted. *Risk:* low (the jar swap is itself the integrity surface being
closed; it changes no spec/property). *Self-contained:* yes.

**A3. Regenerate `tla2tools.jar.sha1` with a relative filename.**  *adopt.*
The committed `.sha1` records a correct hash but an absolute, out-of-repo path
(`/Users/mjm/repo/tlaplus/…`), so `shasum -c` cannot use it portably. Trivial
fix that makes A2's "verify the download" option actually enforceable.
*Risk:* none. *Self-contained:* yes (pairs with A2).

**A4. Expose `-coverage` through the A1 passthrough.**  *adopt.* The profiling
substrate the "measure every change" discipline depends on (the
`--time-expanded` analogue). *Risk:* none (instrumentation only). *Depends on:*
A1. *Self-contained:* yes.

**A5. Set explicit `-Xmx` for the liveness cfgs and tune `-fpmem` for the
large safety run.**  *adopt.* No heap sizing is set anywhere; the JVM default
governs the liveness tableau, and the auto-memory documents a real
"out-of-memory during liveness checking" wall at full constants. Explicit
`-Xmx` makes runs reproducible across machines and prevents OOM as constants
grow; `-fpmem` tuning cuts disk-spill thrash on the 503k-state safety
exploration. *Risk:* none (resourcing only — does not change the explored
graph). *Depends on:* A1. *Self-contained:* yes.

**A6. Add `scripts/tla-baseline.sh`** (the §1 harness).  *adopt.* Operationalises
the measure-every-change rule and the cold-run/pinned-flag determinism caveats
so a coverage regression (a drop in distinct-states) is caught mechanically.
*Risk:* none. *Depends on:* A1, A4. *Self-contained:* yes (new script).

**A7. Run the six negative-control cfgs in CI and assert their expected
non-zero exit.**  *adopt — and a prerequisite for all symmetry below.*
`CapRevocation_NegControl`, `CapRevocation_NegLiveness`,
`CommitProtocol_NegControl`, and the three `IpcReactor_Neg*` cfgs are committed
"runnable proofs that the invariants have teeth" but no script or CI runs
them, so they could rot undetected (e.g. an invariant weakened until a "bad"
spec passes). They are short (each produces a quick counterexample) and are
the **standing soundness monitor** that catches a mis-scoped/silently-unsound
`SYMMETRY` — TLC never validates a symmetry itself, so without these guards a
bad symmetry set silently hides bugs. *Risk:* none (additive verification).
*Self-contained:* yes.

### Tier B — the dominant model (`cap_revocation`, the only real target)

**B1. `Send`: quantify over `SUBSET cspaces[p]` instead of `SUBSET CapIds`.**
*adopt.* `Next` enables `Send` with `cs \in SUBSET CapIds` and then the body
filters `cs \subseteq cspaces[p]`, so TLC enumerates all `2^|CapIds|` subsets
per `Send`-enabled state only to discard most — the textbook
huge-set-then-filter. Quantifying over `SUBSET cspaces[p]` (a process's own
caps are the only ones that can pass the guard) enumerates exactly the enabling
assignments. **A/B-validated: byte-identical generated *and* distinct counts
(4,831,322 / 503,070)** for the control and the rewrite, with the full
liveness verdict unchanged — i.e. a pure time-per-state win on the biggest
model's hot action. *Risk:* low (keep the `cs /= {}` and `Cardinality(cs) <=
MaxCapsPerMsg` conjuncts; they filter identically). *Readability:* improves.
*Self-contained:* yes — best done **together with B5** (both touch the
`Next`/`NextBad`/`NextNoGuard` bodies).
*Implementation note:* the one-line `cs \in SUBSET cspaces[p]` form inside the
existing flat `\E` fails to parse; use the nested
`\E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(...)`
or a named `SendableSets(p)` operator.

**B2. Split the safety invariants out of `CapRevocation.cfg` into a separate
safety-only cfg, run at larger constants.**  *adopt-if-measured.* Keep
`CapRevocation.cfg` for the liveness arm (`EventuallyRevoked`, unchanged small
constants) and add `CapRevocation_Safety.cfg` checking only `TypeOK`/
`MoveSemantics`/`DeadNowhere`/`LiveParent`/`FireSafe`/`RevokedDead`/
`ReportMonotone`. This is **strictly additive coverage** (the gating liveness
run is byte-identical, so `EventuallyRevoked`'s result is untouched), and it is
the **prerequisite that makes symmetry usable** — symmetry is unsound with the
liveness property, so it can only live on an invariant-only cfg.
*Honest framing:* this is a **coverage** play, not a critical-path speedup.
The liveness arm stays the wall-clock pole (its tableau dominates and gains
nothing here); the safety arm merely becomes a place symmetry + larger
constants can run cheaply. Adopt only if the added arm does not meaningfully
regress *total* CI wall-clock (run the two arms as parallel CI steps so
wall-clock ≈ `max`, not `sum`), or if the broadened safety coverage justifies a
modest cost — an explicit judgement call to record. *Risk:* low (does not
weaken any property; only the chosen safety constants must still terminate in
CI budget). *Self-contained:* yes. *Enables:* B3, B4.

**B3. Add `SYMMETRY Permutations(Procs)` (and `Permutations(Notifs)` on
`Teardown`) to the safety-only cfgs only — never a liveness cfg.**
*adopt-if-measured, guarded.* On the safety arm the model-value sets are
interchangeable (no action hard-codes a specific id). **Measured ~2×**: the
safety-only main 503,070 → 265,677; `Teardown` 252 → 132. The factor is capped
near 2× because these sets are size-2, **not** the `|S|!` the technique
suggests in general — temper expectations. *Risk:* **medium and silent** — TLC
never validates a symmetry, and the spec contains `InitProc == CHOOSE p`
(asymmetric `CHOOSE` over a symmetric set is the classic unsound case; the
adversarial check found it empirically safe here because TLC canonicalises the
`CHOOSE`, but the risk is structural). *Mandatory guards:* A7 must be in CI
first; validate by confirming the reduction equals the exact `2!` factor with
all invariants passing **and** the relevant negative control still failing, and
by injecting a deliberate asymmetric bug and confirming it is still caught.
*Readability:* minor cost (one cfg line + a `Permutations` use). *Depends on:*
B2, A7. *Self-contained:* no.

**B4. Investigate `SYMMETRY Permutations(CapIds)` on the safety arm.**
*investigate — highest potential reduction, highest silent-unsoundness risk.*
`CapIds` is size 4, so a sound quotient is up to `4! = 24×` (before
boundary effects) — by far the largest single reduction available, and the
lever that would let the **safety arm run at `CapIds = 6,7`** (materially
broader CDT coverage) inside the same budget. It was *not* among the
empirically-validated symmetry items, so treat it as unproven: the `parent`
forest and `InitCap == CHOOSE c` make this the riskiest set to declare. Validate
exactly as B3 (exact-factor check, neg-controls, injected asymmetric bug) and
**only** ever on an invariant-only cfg. Like B2/B3 this is coverage headroom,
not critical-path speed. *Depends on:* B2, A7. *Self-contained:* no.

**B5. Factor the duplicated `Next` / `NextBad` / `NextNoGuard` bodies through a
shared `CommonActions` operator.**  *adopt (refactor).* The three differ only
in one disjunct (`RevokeStep` vs `RevokeStepBad` vs the `CopyNoGuard`
substitution); today the nine common disjuncts are copy-pasted three times, so
adding an action means editing three places or the negative controls silently
drift out of lock-step — which destroys their whole value. Extract
`CommonActions` and write each `Next*` as `CommonActions \/ <the one variant>`.
*Risk:* none (the expanded relations are logically identical; validate that
SANY parses and all four runs reproduce their current verdicts). *Readability:*
improves. *Self-contained:* yes — do it in the **same change as B1**.

**B6. Investigate replacing `Descendants(c) = {}` with a direct
"no children" predicate in the action guards.**  *investigate — likely
marginal.* `Descendants(c) = {}` (used in `RevokeBegin`/`RevokeEnd`/`Retype`,
and in the `EventuallyRevoked` RHS) is provably equivalent to
`~\E x \in CapIds : parent[x] = c` (an empty subtree ⟺ no children, since dead
caps have `parent = NULL`), replacing a `RECURSIVE` transitive-closure call
with one existential. The genuine set is still needed in `RevokeStep`
(`\E l \in Descendants(c) : IsLeaf(l) /\ …`), so keep `Descendants` there.
*Caveats / judgement:* TLC likely already short-circuits the `= {}` check, so
the win may be negligible — measure for **byte-identical** state counts before
keeping it, and **leave `EventuallyRevoked` written with `Descendants(c) = {}`**
("the subtree is empty" reads as the intended property; swapping in
"no children" is equivalent but less direct). Adopt only the action-guard
rewrite, only if measurement shows a real time-per-state gain. *Risk:* low
(equivalent rewrite). *Self-contained:* yes.

### Tier C — the small models

**C1. Add `SYMMETRY Permutations(Refs)` to `CommitProtocol.cfg` (and its
negative control).**  *adopt.* `Refs = {r1, r2}` are interchangeable model
values. **A/B-validated four ways:** exact `2!` reduction 6,886 → 3,444 with
all five invariants and `RecoverReconstructs` still passing, the negative
control still violating `RecoverReconstructs`, and injected real/asymmetric
bugs still caught. The absolute saving is tiny (the model is already
sub-second), so the value is **headroom to raise `Refs`** (≈6× at `Refs=3`)
for broader partial-commit coverage, plus a cleaner/faster run. *Caveat:* the
main cfg checks the *action* property `[][RecoverReconstructs]_vars`, which TLC
also does not validate under symmetry — so the symmetric negative control
**must** stay in CI (A7) as the guard. *Risk:* low. *Readability:* minor cost.
*Self-contained:* yes.

**C2. Do not optimize `IpcReactor` or `CapRevocation_Teardown` for speed.**
They are 39 and 252 distinct states — already trivial. See the §3 guardrails.

### Tier D — hygiene / cosmetic (do not regress perf)

**D1. Delete the ~245 stray TLC scratch files and point `-metadir` outside the
source tree.**  *adopt.* `tla/.gitignore` correctly ignores `states/` and
`*_TTrace_*` (git status is clean), but the working tree physically holds ~64
`*_TTrace_*` files and 3 `states/` trees (~181 files) from past runs. Deleting
them de-noises the dirs and stops SANY from ever parsing a generated trace
module; routing `-metadir` to a scratch path keeps them from re-accumulating
inside `tla/`. *Risk:* none (generated scratch, untracked). *Self-contained:*
yes.

**D2. Retire the dead Toolbox tier-3 fallback in `find-tla-tools.sh`.**
*adopt (cosmetic).* The auto-memory (verified 2026-06-20) records that local
runs land on tier 2 (vendored jar + native Temurin 17) and the
`/Applications/TLA+ Toolbox.app` path is unused; the ~40-line deprecated
fallback (`find-tla-tools.sh:65-104`) can go. *Risk:* none. *Self-contained:*
yes. *Low priority.*

**D3. Add a constants/expected-distinct-state manifest (the TLC analogue of
the Verus trusted-base ledger).**  *adopt (documentation).* Record per cfg the
canonical constants and expected distinct count (`CapRevocation` 503,070;
`Teardown` 252; `CommitProtocol` 6,886; `IpcReactor` 39). This lets a
coverage-shrinking change be caught the way the trusted-base ledger catches a
dropped Verus obligation, and gives `tla-baseline.sh` (A6) its assertion
targets. The four `cap_revocation` cfgs also duplicate near-identical
`CONSTANTS` blocks that must stay in lock-step for the negative controls —
fold the shared values into the manifest. *Risk:* none. *Self-contained:* yes.

**D4. Tighten the `CapRevocation.cfg` constants-rationale comment.**
*investigate (comment hygiene).* The header comment narrates the *history*
("the atomic-revoke baseline ran 4 caps / 2 procs … ~799k states … Splitting
revoke … explodes …"). `CLAUDE.md` allows documenting a path-not-taken and its
rationale, but the wording leans toward "what was" rather than "what is";
consider trimming to the load-bearing rationale (why 4 caps / why
Threads=QueueDepth=1 are the floor for the liveness arm). *Risk:* none.
*Self-contained:* yes. *Low priority — judgement call, do not lose the
rationale.*

---

## 3. Guardrails — explicit rejects (record so they are never mis-attempted)

These are the "obvious" levers that would shrink the state count by **removing
coverage**, i.e. by changing what is verified. Each was checked against the
real code (§4) and/or empirically; all are **rejected**.

* **Do not shrink `CapRevocation` `CapIds` below 4**, and do not raise
  `Threads`/`QueueDepth` back on the *liveness* arm. Four caps are the minimum
  that builds a ≥3-deep / multi-level CDT subtree, which is what makes
  leaf-first deletion (`IsLeaf` / the `RevokeStepBad` control) non-trivial;
  the cfg already trimmed `Threads 2→1`, `QueueDepth 2→1` to fit the liveness
  tableau, and those are a floor, not slack. (Larger constants are welcome on
  the *safety-only* arm via B2–B4.)
* **Never apply `SYMMETRY` to a liveness cfg** (`CapRevocation.cfg`,
  `IpcReactor.cfg`) — symmetry is unsound with liveness in TLC and would
  silently corrupt the result.
* **Do not shrink `CommitProtocol` `Refs` or `MaxWrites` below 2.** `Refs ≥ 2`
  is the entire point of the model (partial flush / partial commit; unflushed
  refs keep their prior committed root). `MaxWrites ≥ 2` is needed for
  last-write-wins and the `v > refRoots[r]` idempotence filter in `Recover`.
  At `MaxWrites=1` a dropped-idempotence-filter mutant goes undetected; at
  `Refs=1` an off-by-one `walHead` mutant goes undetected.
* **Do not shrink / symmetry / `VIEW` / action-fuse `IpcReactor`.** It is at
  its minimal sound size (`QueueDepth=2` needed for the FIFO ring + Full
  backpressure; `MaxMsgs=3 > QueueDepth` needed so a full queue still has a
  pending sender; both `word` and `wword`; the three-valued `recv`; `bound`
  starting `FALSE` for the send-before-bind hazard). Each tempting reducer was
  shown to break a negative control or hide a lost-wakeup.
* **No `VIEW` that projects away state a property observes** — in particular
  `treport` (`ReportMonotone` is an *action* property over its transition) and
  the `revoked`/`revoking` ghosts (named by `RevokedDead` / the revoke
  termination argument). `VIEW` is reserved for genuinely auxiliary state and
  is not needed by any item above.
* **Do not fuse the stepwise `RevokeBegin`/`RevokeStep`/`RevokeEnd`
  interleaving** back into an atomic revoke — its interleaving with every
  other action *is* what `LiveParent` checks at each mid-revoke state.

---

## 4. What each cfg verifies, and what must be preserved (conformance to the real code)

Because nothing automatically checks that the Rust conforms to the TLA+, every
model edit must be judged against the mechanism it abstracts.

**`CapRevocation.cfg` — `Spec` (kernel CDT revocation, `kcore` cspace/thread/
channel).** `parent`/`live` abstract the `CapSlot` arena's four CDT links down
to the single parent edge the safety properties need; `RevokeBegin/Step/End` +
the `IsLeaf` filter model the real preemptible `revoke` loop
(`descend_to_leaf` + `delete`, one leaf per iteration) and its leaf-first/
DFS-post-order discipline; the `revoking` set + `AncestorOrSelfRevoking` mirror
the per-slot `revoking` field and the `ancestor_or_self_revoking` walk
`derive` consults (the derive guard that makes revoke restartable — and the
reason `EventuallyRevoked` holds). The three cap residences (`cspaces`,
`queues`, `bindings`) model the real move discipline across cspace / channel
ring caps / TCB `bind_slots:[CapSlot;2]`; `treport` + `ReportMonotone` mirror
the `Report` enum and its at-most-one-transition guard. **Load-bearing:** 4
caps, all three residences reachable, both `BindKinds`, the marker + derive
guard, the stepwise `IsLeaf` model. **Reducible (already done / safe):**
`Threads 2→1`, `QueueDepth 2→1`, `Procs=2`, the 4 CDT links → one `parent`
pointer, opaque cap rights / retype target types.

**`CapRevocation_Teardown.cfg` — `TSpec` (rev2§3.3 channel teardown).**
`nlive`/`ncaps`/`pcbind`/`eopen` model the notification refcount discipline
(alive iff a cap *or* a channel hold references it — the `obj_census`
`slot_refs + binding_refs` terms). Channel peer-closed bindings are a refcount
hold, **not** a cap move, so revocation does not see through them — which is
why they are a *separate* mechanism from the TCB bind slots and cannot be
folded into the CDT half. **Load-bearing:** the dual cap-vs-hold reference
kinds, 2 `Ends` per channel, the `RevokeNotif`-while-held and
fire-before-reclaim paths. **Reducible:** `obj_census` collapsed to 2 terms,
`Notifs=2`, `MaxNCaps=2`, `Channels=1`.

**`CommitProtocol.cfg` (cas Store WAL + A/B-superblock commit/recovery).**
`slotA`/`slotB` + generation tie-break + the three-outcome `Crash` model the
real two-slot superblock atomicity; the `CommitPrepare`/`CommitFinish` split
models the two `flush()` barriers and encodes the trusted `FsyncMeansFsync`
axiom; `walLog` + `durableRoots` + `walHead` mirror the circular WAL ring +
contiguous-prefix head advance + replay-past-head. **Load-bearing:** `Refs ≥ 2`,
`MaxWrites ≥ 2`, the two-slot structure, the two-phase split with `Crash`
reachable between, `RecoverReconstructs` + its negative control. **Reducible:**
the WAL ring's physical wrap geometry (modeled as a flat sequence), write
*content* (a bare version counter), page-cache/GC details.

**`IpcReactor.cfg` (userspace reactor lost-wakeup + backpressure, one channel/
one source).** `word`/`wword` + the three-valued `recv` + `bound` model the
`register → loop { wait(); while recv_nb() }` discipline and the symmetric
`send_blocking` backpressure over two kernel notification words.
**Load-bearing:** `QueueDepth ≥ 2`, `MaxMsgs > QueueDepth`, both direction
words, the three-valued control state, `bound=FALSE` at init, and all three
negative controls + `CHECK_DEADLOCK FALSE` + the `EventuallyDelivered`
property (which is the *only* check that catches a genuine lost-wakeup
deadlock once deadlock detection is off). **Reducible:** the 64-bit OR-mask →
one bit per direction, message payload / cap marshalling, the multi-source
dispatch (proptest-routed elsewhere).

---

## 5. Suggested sequencing

1. **Tooling substrate:** A1 → (A2, A3) → A4 → A5 → A6, then A7. A1 unblocks
   A4/A5/A6; A2/A3 must precede trusting any number; **A7 must precede any
   symmetry** (B3, B4, C1) as the standing soundness guard.
2. **Cheap clean wins on the dominant model:** B1 + B5 in one change (both
   touch the three `Next` bodies); B6 as a measured side-investigation.
3. **Clean small-model win:** C1.
4. **Coverage restructure (judgement-heavy):** B2, then B3, then the B4
   investigation — each measured against the §1 baseline, kept only if total
   CI wall-clock is not meaningfully regressed and coverage strictly grows.
5. **Hygiene:** D1–D4 at any time; they touch no model semantics.

Every step is gated by the §1 A/B protocol and the §0 governing rules: a
measured regression, a readability/perf trade that clearly fails the §0.3 bar,
or any drift in a verified property's coverage is rejected.
