# Verus findings 40 — Phase 7d: `urt::time` — the tick→ns conversion (trophy #1)

Plan: `doc/plans/3_verus-rewrite.md` (§4.7, §7 step 6) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7d). Prior increment: `59`
(phase 7c — `urt::slots`). This increment is the fourth host-chokepoint migration:
the overflow-safe wall-clock conversion `Sample::utc_ns_at` in `urt/src/time.rs` —
from Kani (totality only, bounded) to Verus (totality **and** monotonicity,
unbounded ∀). It is **trophy #1** of the §4.7 thesis: the property Kani's own
harness recorded as intractable becomes a theorem.

`cargo verus verify -p urt`: **29 verified, 0 errors** (was 21 — +8 for the `time`
obligations; no rlimit bump or `spinoff_prover`). `cargo test -p urt`: **17 passed**
(the `verus!{}` block erases — every `time` proptest, incl. `conversion_is_monotone`,
`_matches_wide_reference`, `_is_total`, and the `torn_writes_are_never_observed`
seqlock test, runs the same code). `cargo kani`: `urt` is **gone** from the job
(`urt/src/proofs.rs` deleted — `check_time_conversion_total` was its last harness);
the `kani` job now runs only `-p dma-pool` and `-p cas -Z stubbing`. `cd kernel &&
cargo build`: green — the new `verus!{}` block in `time.rs` erases into all five user
binaries (vstd already arrived transitively via `ipc` since 7a).

---

## 1. What was bounded, and the trophy

Kani's `check_time_conversion_total` proved **totality** — no panic/overflow for any
`(wall_base_ns, cntvct_base, cntfrq, cntvct)` — and *deliberately stopped there*:
its own doc-comment records that **monotonicity** (`c1 ≤ c2 ⇒ utc_ns_at(c1) ≤
utc_ns_at(c2)`) "did not terminate in many minutes" because it forces CBMC to relate
two u128 *divisions* with a symbolic `cntfrq` (`doc/results/8_kani-findings-7.md`,
the SOLVER note). Monotonicity was left to the `conversion_is_monotone` proptest —
a *sample*, not a proof. Phase 7d makes both ∀ theorems on the real code, so the
proptest converts from probabilistic to theorem-backed differential coverage (kept,
per §5 discipline).

## 2. The model (`closed`, public-field struct)

`Sample`'s fields are **public**, so — unlike 7c's private `SlotAlloc` — no
`closed`-accessor dance is forced by *field* privacy. The spec is four fns:

- `freq(self)`: `cntfrq` floored to 1 (`pub open` — names only public fields).
- `delta_spec(self, cntvct)`: the saturating counter delta as a ghost int (`pub open`).
- `ideal_ns(self, cntvct)`: `wall_base + delta·10⁹ / freq` — **one** mathematical
  division (`pub closed`).
- `result_spec(self, cntvct)`: `clamp_i64(ideal_ns)` — the value the exec returns
  (`pub closed`).

The two that are `closed` are so for a *different* reason than 7c: their bodies name
the module-private `const NANOS_PER_SEC`, and **a `pub open spec fn` body may name
only public items** (`error: in pub open spec function, cannot refer to private const
item`). `closed` hides the body, so the private const is allowed; the body stays
transparent to the in-module `utc_ns_at` proof and the monotonicity lemma, opaque to
callers — who get the contract through `utc_ns_at`'s `ensures` and
`lemma_utc_ns_at_monotone` instead. (Alternatively `NANOS_PER_SEC` could be made
`pub`; keeping it private + `closed` matches the 7b/7c opaque-field house style.)

## 3. The obligations (∀ all four u64/i64 inputs)

| Item | `ensures` (the unbounded theorem) |
|---|---|
| `utc_ns_at` | `r as int == result_spec(cntvct)` — **totality** (proving the postcond *is* the no-overflow/no-panic proof) **and** the exact functional value |
| `lemma_utc_ns_at_monotone` | `c1 ≤ c2 ⇒ result_spec(c1) ≤ result_spec(c2)` — with the `ensures` above, the exec-level `utc_ns_at(c1) ≤ utc_ns_at(c2)` is a theorem |

Totality is **a corollary of the functional postcondition**, not a separate harness:
Verus cannot prove `r as int == result_spec` without first proving every u128/i128
multiply, add, and cast in the body is panic/overflow-free. The one harness Kani had
is thus *subsumed*, not merely matched.

## 4. The crux: decomposition (`lemma_decompose`)

The exec avoids the `Δ·10⁹` u64 overflow (≈5 min of uptime at 62.5 MHz) by computing
`secs = delta/f`, `frac_ns = (delta%f)·10⁹/f`, `total = wall + secs·10⁹ + frac_ns`.
The functional proof needs **`secs·10⁹ + frac_ns == (delta·10⁹)/f`** — relating two
divisions, exactly the step CBMC could not take. It is three lines once the right
vstd lemma is found:

1. `lemma_fundamental_div_mod(delta, f)` ⇒ `delta = q·f + r` (`q=delta/f`, `r=delta%f`).
2. `assert(d·n == r·n + (q·n)·f) by (nonlinear_arith) requires d == q·f + r` — the only
   nonlinear step, a pure rearrangement.
3. `lemma_hoist_over_denominator(x = r·n, j = q·n, d = f)` ⇒
   `(r·n)/f + q·n == (r·n + (q·n)·f)/f` — i.e. `(delta·n)/f == frac + secs·n`.

`lemma_hoist_over_denominator` (`vstd::arithmetic::div_mod`, `x/d + j == (x + j·d)/d`
for `0 < d`) is the load-bearing find; the rest is `lemma_fundamental_div_mod` + one
`nonlinear_arith`. The u64→int bridge is automatic: `(delta/f) as int == delta as int
/ f as int` for unsigned, and the u128 `(m·10⁹)/f`'s `as int` equals `(m as int ·
10⁹)/(f as int)` once no-overflow is established.

## 5. Overflow accounting (the totality half)

Two helper lemmas keep `utc_ns_at` clean, both **coarse** bounds against `i128`'s vast
headroom rather than tight ones:

- `lemma_u128_frac_fits(m, f)`: `m·10⁹ ≤ u128::MAX` (the exec u128 multiply) — `m ≤
  u64::MAX` and `(u64::MAX)·10⁹ ≤ u128::MAX` `by (compute)`, lifted by
  `lemma_mul_inequality`.
- `lemma_secs_term_fits(secs)`: `0 ≤ secs·10⁹ ≤ (u64::MAX)·10⁹` — `secs·10⁹ ≈ 1.8e28 ≪
  i128::MAX ≈ 1.7e38`, so the i128 multiply and the three `total` adds (which also
  absorb `wall_base ∈ ±9.2e18` and `frac < secs`-bounded) have orders of magnitude of
  slack. `lemma_mul_nonnegative` + `lemma_mul_inequality`.

A first cut mistakenly bounded `secs·10⁹ ≤ i64::MAX` — **false**: `secs·10⁹` routinely
exceeds i64 (that is exactly what the final clamp is *for*). The bound is `i128`, and
the i64 saturation is the clamp, not an overflow. `secs ≤ delta` (so the bound holds
∀) comes from `lemma_div_is_ordered_by_denominator(delta, 1, f)` — dividing by `f ≥ 1`
only shrinks.

## 6. Monotonicity (`lemma_utc_ns_at_monotone`) — the easy half, at the spec level

Stated over `result_spec` (the `int` closed form), monotonicity is short because the
decomposition already moved the hard part into `utc_ns_at`'s `ensures`:

- `delta_spec` monotone in `cntvct` (Verus discharges the two-branch case split).
- `lemma_mul_inequality` (×10⁹ ≥ 0) then `lemma_div_is_ordered` (÷`freq` > 0) ⇒
  `ideal_ns` monotone.
- `clamp_i64` monotone — Verus closes it automatically from the piecewise definition.

The exec ordering the proptest checks now follows from the `ensures` + this lemma.

## 7. Toolchain notes worth recording

- **`pub open spec fn` cannot name a private const** (`NANOS_PER_SEC`) — distinct from
  7c's private-*field* rule but the same fix (`closed`). §2.
- **`.max(1)` / `.saturating_sub(..)` restructured** into explicit `if` branches inside
  `verus!{}` (the std combinators are unspecced — the 7a `to_le_bytes` / 7c
  `.find().map()` precedent). Behaviour-identical; the kept proptests witness the
  equivalence.
- **A `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` struct lives inside `verus!{}`**
  with no friction — Verus treats the derived impls as external, and the erased struct
  is an ordinary `Sample` to the seqlock/atomics code, the tests, and the loom/shuttle
  models (all outside the block, unchanged).
- **Vacuity guard:** a temporary `assert(false)` in `utc_ns_at`'s proof block was
  rejected (`28 verified, 1 errors`), confirming the body is really checked and the
  postcondition non-vacuous, before reverting.
- `NANOS_PER_SEC` was moved **into** the `verus!{}` block (Verus knows its value there);
  the seqlock/`TimePage`/`encode_boot`/aarch64-asm code and all tests stay outside.

## 8. What changed

- `urt/src/time.rs` — one `verus!{}` block holding `const NANOS_PER_SEC`, `struct
  Sample`, the `closed`/`open` model, `utc_ns_at` (contract + branch restructure +
  proof glue), `lemma_utc_ns_at_monotone`, and the three helper lemmas
  (`lemma_decompose`, `lemma_u128_frac_fits`, `lemma_secs_term_fits`); the crate-local
  `use vstd::prelude::*;`. `TimePage`, `sample()`, `encode_boot`, the asm fns, and the
  `tests`/`loom_tests`/`shuttle_tests` modules stay outside, verbatim.
- `urt/src/proofs.rs` — **deleted** (`check_time_conversion_total`, the last harness).
- `urt/src/lib.rs` — `#[cfg(kani)] mod proofs;` removed.
- `urt/Cargo.toml` — `cfg(kani)` dropped from `unexpected_cfgs` (urt is fully off Kani);
  `vstd` dep comment refreshed for 7d.
- `.github/workflows/ci.yml` — `kani` job: `cargo kani -p urt -p dma-pool` →
  `-p dma-pool` (+ comment); `verus` job: `-p urt` unchanged (it now covers `time`
  too, auto-gated) with a refreshed per-PR comment.
- `CLAUDE.md` — the `cargo kani`/`cargo verus` examples, the `kani`/`verus` CI bullets,
  the Verus-tier table row, and the `### Kani` / `### Verus` prose (urt fully off Kani;
  add the 7d note).

## 9. Next

**7e — `dma-pool`** (trophy #2): a `spec fn pool_wf` over the sorted-disjoint extent
list, preserved by `alloc` (split / alignment round-up) and `free` (the two-sided
merge via `copy_within`) — structurally the array-splice reasoning kcore did for
`cdt_unlink`/`slot_move`. Unlocks the DN-10 two-buffer-disjointness case Kani OOM'd on.
Then **7f — `cas::disk`** superblock + the holdout decision (whether Kani retires
wholesale at 7g). §4–§6's `by (compute)` / mul-div-ordered / `closed`-glue notes carry
forward.
