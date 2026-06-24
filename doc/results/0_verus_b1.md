# B1 — `destroy_tcb` per-phase frame lemmas (evaluation)

Task **B1** (rank 2, Wave B) from `doc/plans/0_verus-optimization.md`: decompose
`kcore::thread::destroy_tcb` — the **#2 obligation in the gate** — by lifting each
teardown phase's frame re-establishment into its own tightly-keyed `proof fn`
(`doc/guidelines/verus.md` §10: decomposition is the default fix once
`spinoff_prover`/`rlimit` are exhausted). The op is a linear teardown
(detach → halt → bind-slot `delete`s → clear-before-unref cspace/aspace); each
phase took a ghost snapshot, performed one edit, then ran a dense inline `proof {}`
block re-proving the running cross-object frame on the entry snapshot `st0`. The
whole sequence verified inside **one** isolated query carrying all ~20 `ensures`
(six quantified `pub open spec` frames). This file records the per-attempt
evaluation under the plan's §2 protocol. Temporary intermediate report (per
`CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** both — optimization + simplification (decompose). Pitched as the
  Wave-B wall-clock mover; the keep/drop bar is the **balanced §2-as-written**
  reading confirmed for this task (KEEP a phase if it measurably drops the op
  **and** the crate total; a speed-neutral phase only if it is a clear clarity
  win with <5 % crate regression).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each);
  `cargo verus verify -p kcore`. Gate from the plain-text `verification results::`
  line; timing from a separate cold `scripts/verus-baseline.sh kcore`
  (`--time-expanded --output-json`), ranking
  `.["times-ms"].smt["smt-run-module-times"][]."function-breakdown"[]`. The
  deterministic `rlimit` field is the run-independent signal (§2).
- **Baseline note.** Branched off `main` (`007c9f8`). The cold baseline on this
  host reported **391 verified** — reconciling the count: the trusted-base ledger
  row read **389**, stale by 2 (it had not been bumped for the rev1-audit kcore
  additions, PRs #180/#181); **391** is the true pre-B1 count and the apples-to-
  apples reference here. Each new `proof fn` raises the count by 1.

## The change

Three teardown phases each had their inline `proof {}` block replaced by a call to
a new private `proof fn lemma_destroy_tcb_<phase>_frame` (in `thread.rs`, beside
`destroy_tcb`), keyed per §10: `requires` = the prior snapshot's running
invariants + the single edit's frame shape, `ensures` = the system invariants +
ready pair + the five cross-object frames composed onto `st0`. Each lemma defers
to the same `cspace::lemma_*` composition machinery the inline block used, so it
is a faithful relocation, not a re-proof. The in-tree precedent is
`cspace::destroy_cspace`'s per-iteration `lemma_*_trans` frame calls.

- **`lemma_destroy_tcb_halt_frame`** (halt: clear `qnext`/`wait_notif`, mark
  `Halted`). The three `set_tcb_*` setters compose to a multi-field edit, so the
  caller passes the field-by-field frame shape and the lemma promotes
  `ready_complete_except(t)` → full completeness, freezes the census, and composes
  the except-`t` dead frame + the four home/death frames onto `st0`.
- **`lemma_destroy_tcb_cspace_clear_frame`** (clear `tcb[t].cspace` before
  `unref_cspace`). `set_tcb_cspace`'s post is exactly the
  `.insert(t, TcbView { cspace: None, .. })` shape `lemma_census_after_hold_clear`
  keys on, so the edit shape is a single `==`. The lemma opens the
  `census_off_by_one(cs)` window the `unref` consumes and carries `cspace_resident_wf`
  + the frames across.
- **`lemma_destroy_tcb_aspace_clear_frame`** (clear `tcb[t].aspace`). The twin of
  the cspace clear minus the `cspace_resident_wf` carry, over
  `lemma_census_after_hold_clear_aspace`.

The op body now reads, per phase, as "do the edit, call the frame lemma" (plus the
cheap t-survival asserts the downstream phases consume). The detach phase (three
branches with differing exec edit shapes — `unqueue_ready` / `remove_waiter` /
no-op) and the two bind-slot `delete` merge blocks (light `_trans` compositions
with no measurable SMT to recover) were **assessed and deferred**: detach is a
design spike rather than a single-setter drop-in, and the merges are below the
optimization bar. As a §10 follow-on the now-overprovisioned
`#[verifier::rlimit(30)]` was tightened to `rlimit(24)` (the per-phase derivations
no longer share the one query).

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p kcore && cargo verus verify -p kcore` ended with

```
verification results:: 394 verified, 0 errors
```

**present** (a real cold run). `N` rose **391 → 394**, **+3**, exactly the three
new `proof fn`s — the predicted delta. **Gate: PASS (Y).** The trusted-base tally
stays **14** (the lemmas are ordinary verified proofs, not new seams); the ledger
kcore row is updated to 394 (which also corrects the prior 389→391 staleness).

## Measurement (§2 step 2b — cold timing vs. baseline)

The op sheds nearly a quarter of its SMT time and rlimit; the three new lemmas are
essentially free; the crate total drops:

| obligation | SMT ms (before → after) | rlimit (before → after) |
|---|---:|---:|
| `thread::destroy_tcb` | 19 804 → **15 267** (−22.9 %) | 46 118 651 → **34 510 921** (−25.2 %) |
| `lemma_destroy_tcb_halt_frame` | — → 17 | — → 56 442 |
| `lemma_destroy_tcb_cspace_clear_frame` | — → 21 | — → 56 472 |
| `lemma_destroy_tcb_aspace_clear_frame` | — → 13 | — → 55 052 |

Per-phase landing (cold, incremental):

| state | `destroy_tcb` ms / rlimit | kcore SMT total |
|---|---:|---:|
| baseline | 19 804 / 46 118 651 | 97 216 ms |
| + halt | 17 083 / 39 424 476 | 94 185 ms |
| + cspace + aspace (full B1) | **15 267 / 34 510 921** | **91 286 ms** |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT total | 97 216 ms | 91 286 ms | **0.94× (−6.1 %)** |

The decisive run-independent signal is `destroy_tcb`'s **rlimit drop**
(46.1 M → 34.5 M, −25.2 %): moving each phase's derivation into its own query is a
genuine proof-size reduction of the op's context, not ms noise. The three lemmas
cost **51 ms combined** against the **4 537 ms** removed from the op. The op
landed squarely in the plan's projected "20–40 % cut, not a 2×."
**Optimization criterion met: the target fn and the crate SMT total both
measurably dropped, at every phase.**

## Clarity (§2 step 4)

**Cleaner.** Net line count grew (the lemmas carry explicit `requires`/`ensures`
contracts — +205 lines), which is the §10 decomposition tradeoff: the main
`destroy_tcb` body is now a short "edit, then call the phase frame lemma" sequence
instead of ~130 lines of inline frame derivation, and each phase is a named,
independently-checkable unit with a stated contract — matching the in-tree
`destroy_cspace` precedent the plan cites. The over-provisioned `rlimit(30)` →
`rlimit(24)` retires a now-misleading "this proof is hard" signal (§10), with its
rationale comment updated to describe the post-decomposition budget.

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change is
confined to `proof {}` / `proof fn` bodies (which erase in a normal build), so
exec behavior is unchanged by construction. `cargo build -p kcore` compiles clean
in normal mode (the erased proof code yields identical exec).

## Decision

**KEEP** (all three extractions + the rlimit tightening). The optimization
asymmetry is satisfied at every phase — `destroy_tcb` fell **19 804 → 15 267 ms
(−22.9 %)** with its rlimit **−25.2 %**, the crate total fell **97 216 → 91 286 ms
(−6.1 %)**, and the new lemmas add only 51 ms — and the simplification axis is a
clear win (named per-phase contracts mirroring `destroy_cspace`, a shorter op
body, an honest budget). Gate 394/0.

> verified **Y** (391 → **394**, +3 lemmas) · `destroy_tcb` **19 804 ms / rlimit
> 46 118 651 → 15 267 ms / rlimit 34 510 921** (−22.9 % / −25.2 %) · new lemmas
> **51 ms total** · kcore SMT **97 216 → 91 286 ms** (−6.1 %) · `rlimit(30)→(24)` ·
> clarity **cleaner** → **KEEP**
