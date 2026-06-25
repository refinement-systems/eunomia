# 4 — storage-server `attenuate()` + rights lattice under Verus (Task 4)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 4
(storage-server `attenuate()` + rights lattice — monotone delegation; Tier 1, the
storage-server crate-onboarding pilot). Outcome: **verified, shipped**.
`cargo verus verify -p storage-server --no-default-features --lib` is a new gate
line reading **`14 verified, 0 errors`**. No reverts. No collateral change to any
other crate's proofs or `rlimit` budgets — the whole gate stays green at its prior
numbers.

## What was attempted

Bring `storage-server/src/lib.rs`'s rights lattice under Verus and, in doing so,
onboard `storage-server` into the verification gate for the first time. The sole
arithmetic of capability delegation (rev2§2.3) is `attenuate(parent, mask) =
parent & mask`; previously it was guarded only by the `tests/rights_lattice.rs`
proptests. The rights bits, `attenuate`, and a `has_right` spec reading of the
dispatch guards now live in one `verus!{}` island.

Contract added (plain Verus, `by (bit_vector)` for the u8 identities):

```rust
pub open spec fn has_right(bits: u8, r: u8) -> bool { bits & r != 0 }

pub fn attenuate(parent: u8, mask: u8) -> (r: u8)
    ensures
        r == parent & mask,
        r & !parent == 0,                                       // monotone
        (mask & R_STAT_STORE == 0) ==> (r & R_STAT_STORE == 0), // stat-store strip
{
    let r = parent & mask;
    assert(r & !parent == 0) by (bit_vector) requires r == parent & mask;
    assert((mask & R_STAT_STORE == 0) ==> (r & R_STAT_STORE == 0)) by (bit_vector)
        requires r == parent & mask;
    r
}

// monotonicity in the has_right reading: a derived handle holds a right only if
// its parent did (∀ single-bit or composite `right`).
pub proof fn lemma_attenuate_monotone(parent: u8, mask: u8, right: u8)
    ensures has_right(parent & mask, right) ==> has_right(parent, right)
{ assert(((parent & mask) & right != 0) ==> (parent & right != 0)) by (bit_vector); }

// deny-by-default: masking by R_ALL (bits 0..=4, omits bit 5) clears R_STAT_STORE.
pub proof fn lemma_attenuate_r_all_denies_stat_store(parent: u8)
    ensures !has_right(parent & R_ALL, R_STAT_STORE)
{
    assert(R_ALL == 0b1_1111u8);
    assert(R_STAT_STORE == 1u8 << 5);
    assert((parent & 0b1_1111u8) & (1u8 << 5) == 0) by (bit_vector);
}
```

These mechanize, ∀ `u8`, the three properties the `rights_lattice` proptests
check by example: intersection-only (`expected & !parent_rights == 0`),
`R_STAT_STORE` stripped when the mask omits it, and the deny-by-default corollary
that `R_ALL` always strips it. The proptests are **kept** as the companion oracle
tier.

## Result

Full gate, re-run cold (`scripts/verus-baseline.sh`; every crate real-run, results
line present; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`):

| crate | result |
|---|---|
| kcore | 404 verified, 0 errors |
| ipc | 68 verified, 0 errors |
| urt | 25 verified, 0 errors (freelist dep 29) |
| freelist | 29 verified, 0 errors (no-alloc gate) |
| dma-pool | 0 verified, 0 errors |
| cas `--no-default-features` | 75 verified, 0 errors |
| virtio-blk | 29 verified, 0 errors (re-verifies freelist under alloc) |
| **storage-server** `--no-default-features --lib` | **14 verified, 0 errors** |

The 14 own-surface items are the seven `pub const` rights bits (`mode: spec`),
`has_right`, `attenuate`, and the two `lemma_attenuate_*` proofs, with their
const-correctness obligations. All three exec/proof obligations are negligible
(`rlimit`: `attenuate` 30,889; `lemma_attenuate_monotone` 44,064;
`lemma_attenuate_r_all_denies_stat_store` 4,115 — vs freelist's heavy proofs at
110M+).

Notes that turned out to matter:

- **The constants must sit *inside* the `verus!{}` block.**
  `lemma_attenuate_r_all_denies_stat_store` needs Verus to know `R_ALL` is
  `0b1_1111` and `R_STAT_STORE` is `1 << 5` so the SMT can connect the `by
  (bit_vector)` literal fact `(parent & 0b1_1111) & (1<<5) == 0` to the
  const-keyed ensures. (`by (bit_vector)` itself sees only literals, never named
  consts — verus.md §6 — so the two pin asserts bridge the names to their values.)
- **The conditional stat-store-strip clause proves with `R_STAT_STORE` symbolic.**
  `(mask & X == 0) ==> ((parent & mask) & X == 0)` is true for *all* `X` (it is
  `parent & (mask & X) == parent & 0`), so the `attenuate` clause discharges by
  `bit_vector` without needing bit 5's literal value — only the deny-by-default
  *corollary* (which depends on `R_ALL` actually omitting bit 5) needs the literals.
- **No transitive `rlimit` regression.** storage-server links cas (no_std+alloc)
  and ipc; a cold session re-verifies them, but their `rlimit` totals are
  byte-identical to their standalone gates, and storage-server pulls **no** heavy
  `freelist` merge proof into a new context (the Task-3 alloc-prelude budget story
  does not recur here). No budget anywhere needed touching.

## The onboarding finding (the real content of this task)

**storage-server is the first gated crate with a separate binary target.**
`cargo verus verify -p storage-server` verifies *every* target — lib **and**
`src/main.rs` — and the placeholder bin (a one-line `eprintln!`) carries no
`use vstd::prelude::*`, so Verus aborts it:

```
error: Error: The verus_builtin crate was not imported. This is usually
imported via `vstd`, and it is necessary to run Verus.
  --> storage-server/src/main.rs:1:1
```

The bin holds no proofs (it is the on-OS server entrypoint, library-only until the
M3 IPC transport exists), so verifying it buys nothing. **Fix: scope the gate to
`--lib`.** Two gotchas:

1. **Argument order is load-bearing.** cargo-verus splits flags into
   "Verus-relevant" (`--package`, `--features`, `--no-default-features`) and
   "cargo-only" (`--lib`), and **the Verus-relevant ones must come first**:
   `cargo verus verify -p storage-server --no-default-features --lib`. The reverse
   order (`--lib --no-default-features`) is a hard error ("Args forwarded to Cargo
   must precede args forwarded to Verus").
2. `--lib` still compiles and re-verifies the transitive gated deps (cas, ipc) —
   it only drops storage-server's own bin target, not the dependency closure.

The alternative — adding `use vstd::prelude::*` to `main.rs` — was rejected: it
forces the macro crate into a proof-free binary for no benefit and would have to be
carried by every future bin in a gated crate.

## Reverted vs kept

Nothing reverted — the proof succeeded on the first verifying shape. Kept: the
storage-server gate (Cargo.toml `vstd` + `metadata.verus` + lints), the `verus!{}`
rights-lattice island (constants relocated inside it, `has_right`, the `attenuate`
contract, the two lemmas), and the bookkeeping (CI line, CLAUDE.md gate list,
`verus-baseline.sh` `ALL_CRATES` + the `storage-server → --no-default-features
--lib` arg case, ledger Baseline row). The `rights_lattice`/`sessions` proptests
and the dispatch fuzz corpora are unchanged (the kept companion oracle tier); the
rights constants keep their numeric values (the committed fuzz corpora depend on
them).

## Proposed guideline additions (`doc/guidelines/verus.md`)

Onboarding a gated crate **with a binary target** is structurally different from a
lib-only crate and deserves a note alongside the Task-3 "upper crate" notes:

1. **`cargo verus verify -p <crate>` verifies every target, bins included.** A bin
   with no `verus!{}` still must import `vstd` (`The verus_builtin crate was not
   imported`). For a proof-free bin, scope to `--lib` rather than polluting the bin
   with a vstd import.
2. **cargo-verus requires Verus-relevant flags (`--no-default-features`,
   `--features`, `-p`) *before* cargo-only flags (`--lib`)** on the command line, or
   it errors. The gate line, the CI job, and `verus-baseline.sh`'s `verus_args_for`
   all use `--no-default-features --lib` in that order.
3. **Put named constants used by `by (bit_vector)` ensures inside the `verus!{}`
   block.** A clause keyed on a named const's *value* (here `R_ALL` omitting bit 5)
   needs Verus to know the definition; pin it (`assert(R_ALL == 0b1_1111u8)`) and
   supply the literal `bit_vector` fact separately. A clause true for *all* values
   of the const (here the conditional strip) does not.

## Trusted base

**Tally unchanged at 14.** storage-server adds no
`external_body`/`assume_specification` — the rights lattice is pure `u8` bit-mask
reasoning citing no vstd axiom, and the postcard wire body / cas-store seams stay
external (outside `verus!{}`), unverified-by-construction as before (Task 8 will
revisit the wire header). New Baseline row: `-p storage-server
--no-default-features --lib` → 14 verified, 0 errors. CLAUDE.md gate list, CI
`verus` job, and `verus-baseline.sh` `ALL_CRATES` updated to include
storage-server.
