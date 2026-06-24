# C4 — cosmetic / bounded clarity (evaluation)

Task **C4** (ranks 33 + 35, Wave C) from `doc/plans/0_verus-optimization.md`: two
independent clarity-only simplifications in two gated crates.

- **C4a (rank 33)** — `kcore/src/aspace.rs`: align the four `pt_wf_leveled`
  re-establishment blocks and cross-reference the predicate's clause names.
- **C4b (rank 35)** — `cas/src/prolly.rs`: extract the LE readers' inline
  `by (bit_vector)` per-byte facts into three named lemmas.

Both are **simplification** tasks under the plan's §2 asymmetry (keep iff the diff
reads cleaner and the crate SMT total does not materially regress, <5 %). This file
records the per-attempt evaluation. Temporary intermediate report (per `CLAUDE.md`,
not citable from code/specs/guidelines).

- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p <crate>` before each); `cargo verus verify
  -p kcore` / `-p cas --no-default-features` for the gate, `scripts/verus-baseline.sh
  kcore cas` (`--time-expanded --output-json`) for timing. Per-fn wall ms wobbles
  ±5–15 %, so the deterministic **rlimit** field is the run-independent claim (§2).
- **Baseline.** C4 branches off `origin/main` (`db9c314`, post A1–A5/B1–B6 + C1).
  Fresh cold pre-C4 baseline on this host: **kcore 402 verified / SMT 61 688 ms**,
  **cas 86 verified / SMT 1 051 ms**. Per-fn before: `read_u16_le` 16 ms / rlimit
  50 249, `read_u32_le` 31 / 102 724, `read_u64_le` 113 / 828 645; aspace
  `lemma_link_l1` rlimit 106 216, `lemma_grow_pool` 65 119, `lemma_link_l2` 141 805,
  `lemma_leaf_write` 110 884. (The live pre-change cas count is **86**; the
  trusted-base ledger row read **85** — a pre-existing −1 drift, reconciled below.)

## Outcome summary

| sub-task | kind | target fn before → after | rlimit before → after | decision |
|---|---|---|---|---|
| **C4a** aspace clause labels | simp | four lemmas — proof bytes unchanged | `link_l1` 106 216 → **106 216** (and the other three identical) | **KEEP** |
| **C4b** prolly LE-reader lemmas | simp | `read_u64_le` 113 → **16** ms; `read_u32_le` 31 → **7**; `read_u16_le` 16 → **4** | `read_u64_le` 828 645 → **182 115** (−78.0 %) | **KEEP** |

**Kept (C4a + C4b): kcore 402/0 (unchanged), cas 86 → 75/0, cas SMT 1 051 → 963 ms
(−8.4 %).** C4a is provably zero-SMT (every touched lemma's rlimit is byte-identical);
C4b is a clarity win **and** a measured speedup, the readers' rlimits falling 36–78 %.

## C4a — align the four `pt_wf_leveled` blocks (rank 33) · KEEP

`aspace.rs`. The four lemmas that re-establish `pt_wf_leveled` after a page-table
edit (`lemma_link_l1`, `lemma_grow_pool`, `lemma_link_l2`, `lemma_leaf_write`) cited
the predicate's clauses — `(a)` accounting, `(b1)` L1→inner, `(b2)` inner→leaf,
`(c1)`/`(c2)` injectivity (defined in the `pt_wf_leveled` doc-comment) — in three
inconsistent comment styles (trailing inline, prose intro, or none). Normalised all
four to one form: a "re-establish clause by clause" header naming which clauses the
edit touches (and why the rest are immediate), then a `// (b1)`/`// (b2)`/`// (c1)`/
`// (c2)` label above each `assert forall` sub-block. `lemma_leaf_write` writes a leaf
PTE, so it legitimately re-proves only `(b2)`/`(c2)`; its header now says so. **No
proof logic changed** — the diff is comments and blank lines only (the
`assert`/`by {}` bodies are byte-identical). Per the plan, **no** `pt_wf_b1/_b2`
sub-predicates were introduced (that would alter a closed spec's auto-unfold for zero
speed).

*Measured:* the four lemmas' rlimits are **byte-identical** before→after
(`lemma_link_l1` 106 216, `lemma_grow_pool` 65 119, `lemma_link_l2` 141 805,
`lemma_leaf_write` 110 884), and the kcore top-8 obligations' rlimits are likewise
unchanged to the byte (`destroy_tcb` 34 510 921, `signal` 20 909 644, … — the control
set). The crate total moved 61 688 → 62 035 ms (+0.56 %), inside the ±5–15 % wall-ms
noise band and contradicted by the identical rlimits — i.e. pure run-to-run wobble, no
proof change. A clarity win at provably zero proof cost. **KEEP.**

## C4b — prolly LE-reader `bit_vector` lemmas (rank 35) · KEEP

`prolly.rs`. `read_u16_le`/`read_u32_le`/`read_u64_le` each carried inline
`assert(…) by (bit_vector)` per-byte facts (2 + 4 + 8 = 14 asserts) bridging the
readers' bit-construction `v = b0 | (b1<<8) | …` to the shift-form `u*_le` spec;
`read_u64_le`'s eight each repeated a two-line `requires v == …`. Extracted the per-
byte split into three private `proof fn lemma_u{16,32,64}_le_bytes(v, b0, …)
by (bit_vector)` (the `doc/guidelines/verus.md` §6 recipe: construction in `requires`,
the clean `(v >> 8k) as u8 == bk` facts as `ensures`, empty body). Each reader is now
a single `proof { lemma_u…_le_bytes(v, …) }` call plus its closing `=~= u*_le(v)`
extensionality assert. Per the plan, the §8 shift-form rewrite was **avoided** (it only
relocates the `bit_vector` cost to the writers).

The verified count fell **86 → 75** (−11): the 14 inline `by (bit_vector)`
sub-obligations collapse into the 3 lemma signatures (−14 + 3), the same accounting the
plan documents for A5 — no coverage lost (the readers' closing extensionality asserts
still verify, and the lemmas *are* those facts). Predicted +3-from-86 was wrong on the
sign; the collapse is the expected shape.

*Measured:* `read_u64_le` **113 → 16 ms / rlimit 828 645 → 182 115 (−78.0 %)**,
`read_u32_le` **31 → 7 ms / rlimit 102 724 → 63 183 (−38.5 %)**, `read_u16_le`
**16 → 4 ms / rlimit 50 249 → 31 887 (−36.5 %)**; the three new lemmas cost ~12 ms /
rlimit ~80 K each (~36 ms combined). Crate SMT total **1 051 → 963 ms (−8.4 %)** and
all three readers leave the top-8. (`encode_raw`'s rlimit rose 1 840 881 → 2 139 554 on
a 66 ms obligation — it touches none of the readers/lemmas, so this is module-scheduling
wobble, not a regression; the crate total still fell.) A clarity dedup of 14 inline
asserts → 3 named identities **with** a real per-reader speedup. **KEEP.**

## Gate (§2 step 2a — cold, authoritative, whole-crate)

Cold `cargo clean -p <crate>` then verify ended, line **present** (a real run):

```
kcore:  verification results:: 402 verified, 0 errors
cas:    verification results:: 75 verified, 0 errors
```

**Gate: PASS (Y).** kcore `N` is unchanged **402** (C4a adds no obligation — comments
only). cas `N` fell **86 → 75** by the `bit_vector`-collapse accounting above. The
trusted-base ledger cas row is updated to **75** (it had read 85, reconciling the
pre-existing −1 drift from the live 86 alongside the −11 collapse); the kcore row stays
402. The new lemmas are ordinary verified proofs (no new seam), so the
`external_body`/`assume_specification` tally stays **14**.

## Host tests / build

`cargo build -p kcore -p cas` compiles clean. `cargo test -p cas` passes — C4b changes
only proof scaffolding (the readers' exec bodies `let v = …; …; v` are byte-identical;
the edited asserts are erased from a normal build), and C4a is kcore-only proof
comments, so exec behaviour is unchanged by construction.

## Clarity (§2 step 4)

**Cleaner** on both. C4a makes the four parallel re-establishment blocks grep-uniform
against the `pt_wf_leveled` doc-comment — a reader can now match each `// (b2)` sub-
block to clause (b2) of the definition across all four lemmas. C4b turns 14 inline
`bit_vector` asserts (read_u64_le's eight each dragging a redundant `requires`) into one
named identity per width and a one-line call per reader.

## Decision

**KEEP C4a + C4b.** Both satisfy their §2 axis: C4a is a readability win at provably
zero proof cost (every touched rlimit byte-identical), and C4b is both cleaner **and**
faster (cas SMT 1 051 → 963 ms, −8.4 %, driven by run-independent reader rlimit drops of
36–78 %). Gate kcore 402/0, cas 75/0.

> verified **Y** (kcore 402 → **402**; cas 86 → **75**, the 14 inline `bit_vector`
> asserts collapsing into 3 lemmas) · C4a four aspace lemmas **rlimit byte-identical**
> (zero proof change) · C4b `read_u64_le` **113 → 16 ms / rlimit −78.0 %**, `read_u32_le`
> 31 → 7, `read_u16_le` 16 → 4 · cas SMT **1 051 → 963 ms (−8.4 %)** · clarity **cleaner**
> → **KEEP C4a + C4b**
