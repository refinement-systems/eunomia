# A1+ / A1++ — freelist rlimit re-tune + `free`-loop trigger note (evaluation)

Tasks **A1+** (rank 24) and **A1++** (rank 25) from
`doc/plans/0_verus-optimization.md`, the two cheap follow-ons that finish the
freelist pass after A1 landed (commit `9d7be2b`). This file records the
per-attempt evaluation under the plan's §2 protocol. Temporary intermediate
report (per `CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** A1+ simplification/honesty (refactor); A1++ simplification (refactor).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p freelist` before each); gate from
  `cargo verus verify -p freelist`, timing from a separate cold run with
  `-- --time-expanded --output-json`. Baseline on disk
  (`target/verus-baseline/freelist.json`) is pre-A1; the relevant comparison for
  these two clarity tasks is post-A1 → final, measured in one session below.

These are **clarity / honesty** changes, not speed targets. The accept gate is
the crate holding `29 verified, 0 errors` with smaller, honest budgets and the
trigger note gone.

## The changes (`freelist/src/lib.rs`)

**A1++ — annotate the `free` search-loop invariant trigger** (one line):

```diff
-                forall|k: int| 0 <= k < i ==> self.free@[k].0 < off,
+                forall|k: int| #![trigger self.free@[k].0] 0 <= k < i ==> self.free@[k].0 < off,
```

**A1+ — re-tune the seven over-provisioned `#[verifier::rlimit]` budgets.** Four
drop to the default (annotation removed); three keep a much smaller explicit
budget. `#[verifier::spinoff_prover]` and all proof bodies are untouched.

| fn (site) | budget before | budget after |
|---|---:|---|
| `is_allocated` | rlimit(100) | **default** (removed) |
| `alloc` | rlimit(100) | **default** (removed) |
| `free` | rlimit(100) | **default** (removed) |
| `free_covers_both` | rlimit(50) | **default** (removed) |
| `free_insert` | rlimit(100) | **rlimit(50)** |
| `free_replace` | rlimit(120) | **rlimit(20)** |
| `free_both` | rlimit(50) | **rlimit(15)** |

Net: `+4 / −8` lines.

## A1++ — gate + note (§2 steps 2a, 4)

Before the annotation, every cold run prints a low-confidence trigger note keyed
exactly at the invariant (`freelist/src/lib.rs:1153`):

```
note: automatically chose triggers for this expression:
   --> freelist/src/lib.rs:1153:17
note:   trigger 1 of 1:
1153 |   forall|k: int| 0 <= k < i ==> self.free@[k].0 < off,
     |                                 ^^^^^^^^^^^^^   (self.free@[k].0)
note: Verus printed ... because it had low confidence in the chosen triggers.
```

Verus' auto-chosen trigger is `self.free@[k].0` — so the annotation names the
trigger the prover already infers. After the annotation: **note gone**, cold run
`29 verified, 0 errors`, SMT unchanged (it only documents the inferred trigger,
adds no new term). **A1++: PASS (Y), clarity win.**

## A1+ — empirical retune (§2 step 2a + the probe trail)

The rlimit unit is version-specific, so budgets were **probed**, not computed.
Cold whole-crate runs (every function is `spinoff_prover`, so an over-tight cap
surfaces as a *named* "Resource limit (rlimit) exceeded" error):

| run | budgets (insert/replace/both; others default) | result |
|---|---|---|
| all 7 removed | — | **26 verified, 3 errors** → `free_insert` (`:792`), `free_replace` (`:893`), `free_both` (`:999`) exceed default; the other four fit default |
| probe 1 | 50 / 20 / 15 | 29 verified, 0 errors |
| probe 2 | 45 / 16 / 12 | 29 verified, 0 errors |
| **final** | **50 / 20 / 15** | **29 verified, 0 errors** |

The "all removed" run is the key datum: it proves the four removals are correct
(those functions verify at the default) and that exactly three need a budget —
matching the plan's triage.

**Unit / floor calibration.** rlimit *consumption* is the work done and is
deterministic across runs (byte-identical totals), independent of the cap. From
the pass/fail boundary (default is `rlimit(10)`):

- `free` consumes 24.24 M, passes at default ⇒ 1 rlimit pt ≥ 2.42 M.
- `free_both` consumes 31.39 M, fails at default but passes at rlimit(12) ⇒ pt ∈ [2.62 M, 3.14 M).
- `free_replace` passes at rlimit(16) ⇒ pt ≥ 2.68 M.

So **1 rlimit pt ≈ 2.7–3.1 M** Z3 units. The genuine floors (smallest passing
cap) are ≈ 36–42 / 14–16 / 11–12 for insert/replace/both — probe 2 (45/16/12)
sits right on them. The final budgets (50/20/15) add a small notch of headroom
over the floor (so a future proof tweak or Z3 bump won't silently break CI)
while replacing the prior gross over-provisioning.

**Cap vs. consumption (the honesty win).** Consumption is unchanged by A1+; only
the cap moves, from far-above-need to just-above-need:

| fn | consumption | old cap (×need) | new cap (×need) |
|---|---:|---:|---:|
| `free_insert` | 110.9 M | 100 → ~2.4–2.8× | 50 → ~1.2–1.4× |
| `free_replace` | 42.8 M | 120 → ~7.5–8.8× | 20 → ~1.3–1.5× |
| `free_both` | 31.4 M | 50 → ~4.3–5.0× | 15 → ~1.3–1.5× |
| `free` | 24.2 M | 100 → ~11–13× | default |
| `alloc` | 2.9 M | 100 → ~90–110× | default |
| `free_covers_both` | 0.43 M | 50 → ~310–365× | default |
| `is_allocated` | 0.10 M | 100 → ~2700× | default |

## Measurement (§2 step 2b — SMT is flat, as expected)

Crate SMT total (ms), one cold session:

| state | SMT total | verified |
|---|---:|---:|
| pre-A1 baseline (on disk) | 13 017 | 29 / 0 |
| post-A1 (with A1++) | 5 919 | 29 / 0 |
| **final (A1+ / A1++)** | **5 768** | **29 / 0** |

post-A1 → final is **−2.5 %** (within the ±5–15 % noise band): reducing an rlimit
*cap* cannot change the work a passing proof does, so a flat total is the
correct outcome. The 13 017 → ~5.8 k drop is the A1 win (already landed), not
attributable here. (Absolute totals run a little higher than the A1 doc's
4.6 s — ordinary host/run variance; the post-A1-vs-final comparison is taken in
one session so the −2.5 % is the meaningful figure.)

## Clarity (§2 step 4) + Decision

**Cleaner.** Seven misleading "this proof is hard" budgets become four honest
defaults and three tight, near-floor budgets that actually reflect the proofs'
cost; the `free` loop's perpetual trigger note is silenced by naming the trigger
Verus already infers. No body, signature, or `spinoff_prover` change; `cargo fmt`
clean; `+4 / −8` lines.

**KEEP (both).** Simplification asymmetry satisfied: a clear readability/honesty
win with no material SMT regression (−2.5 %).

> A1+: verified **Y** · budgets 100/100/100/120/50/50/100 → default×4 + 50/20/15
> · crate-total **5 919 → 5 768 ms** (flat) · clarity **cleaner** → **KEEP**
> A1++: verified **Y** · low-confidence trigger note **present → gone** · SMT
> unchanged · clarity **cleaner** → **KEEP**
