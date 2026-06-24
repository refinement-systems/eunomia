# A5 — urt bit-frame lemmas → §6 recipe form (evaluation)

Task **A5** (rank 18, Wave A) from `doc/plans/0_verus-optimization.md`: rewrite the
two `by (bit_vector)` bridge lemmas in the `urt` slot allocator —
`lemma_set_bit` and `lemma_bit_other` (`urt/src/slots.rs`) — from their inline,
`free: bool`-selected shape into the canonical **packed-bitmap recipe**
(`doc/guidelines/verus.md` §6, lines 816–857): `by (bit_vector)` on the
*signature*, both write directions as plain unconditional `ensures`, empty body,
no runtime selector. One of the pre-measured (`[measured]`) entries, kind
**both** (opt + simp, technique `vstd-reuse`): a small genuine speedup *and* a
clarity win (recipe conformance). This file records the per-attempt evaluation
under the plan's §2 protocol. Temporary intermediate report (per `CLAUDE.md`, not
citable from code/specs/guidelines).

- **Kind:** both — optimization + simplification (packed-bitmap recipe / `vstd`-reuse).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p urt` before each);
  `cargo verus verify -p urt`. Gate from the plain-text `verification results::`
  line; timing from a separate cold `-- --time-expanded --output-json` run,
  ranking `.["times-ms"].smt["smt-run-module-times"][]."function-breakdown"[]`.
- **Baseline note (which number is A5's).** A5 is branched off `main` (`2489bda`,
  carrying A1+A2+A3 but **not** A4 — A4/PR #187 was still in flight) and edits only
  `urt/src/slots.rs` (the two lemmas + their two call sites) plus the urt ledger
  row, disjoint from A4's `cas` work. To isolate A5 cleanly, *both* before/after
  were measured cold on **this branch's base** via a `git stash` round-trip
  (`a5-before.json` = the stashed pre-A5 tree, `a5-after.json` = the edited tree).
  The deterministic `rlimit` field — run-independent — is the decisive signal here
  (§2: a large rlimit drop is strong evidence even when ms wobble ±5–15 %).

## The change

Both lemmas carried a runtime `free: bool` parameter and proved their bit identity
with **two inline `assert … by (bit_vector)`** in the body, one per write
direction, behind a `free ==>` / `!free ==>` guard on each `ensures`. They now take
the §6 recipe form: the `free` parameter is dropped, `by (bit_vector)` moves onto
the **signature**, and each lemma states *both* write directions as plain
unconditional `ensures` with an empty body —

```rust
proof fn lemma_set_bit(x: u64, k: u64) by (bit_vector)
    requires k < 64,
    ensures (x | (1u64 << k)) & (1u64 << k) != 0,
            (x & !(1u64 << k)) & (1u64 << k) == 0,
{ }
```

`lemma_bit_other`'s `ensures` also adopts the §6 **mask-equal** form
(`(x | (1u64 << k)) & (1u64 << m) == x & (1u64 << m)`, and the clear twin) in place
of the old boolean `(…!=0)==(…!=0)` equivalence — the mask equality propagates
through the `!= 0` test the call site needs and reads cleaner. At the two call
sites in `SlotAlloc::set` the `free` argument is dropped
(`lemma_set_bit(old_word, bi)`, `lemma_bit_other(old_word, bi, (j % 64) as u64)`);
the `if free { … | b } else { … & !b }` exec branch is **unchanged** — each lemma
now proves both directions and the exec-branch fact
(`self.free@[w] == old_word | b` vs `== old_word & !b`) selects the relevant one.
No logic change. Matches `verus.md` §6's stated recipe verbatim.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p urt && cargo verus verify -p urt` ended with

```
verification results:: 25 verified, 0 errors
```

**present** (a real cold run, not stale cache). `N` fell **29 → 25**, **−4**,
exactly as predicted: the four inline `by (bit_vector)` asserts (two per lemma) are
no longer separate obligations — each lemma is now a single signature-level
`by (bit_vector)` obligation, so the four sub-obligations collapse into the two
signatures and nothing else shifted. **Gate: PASS (Y).** (The transitively
re-checked `freelist` dep stayed 29/0, untouched.)

## Measurement (§2 step 2b — cold timing vs. branch base)

Both target lemmas and the write-helper that calls them shed SMT time and rlimit;
the crate total drops:

| obligation | SMT ms (before → after) | rlimit (before → after) |
|---|---:|---:|
| `lemma_bit_other` | 33 → **19** | 263 683 → **115 806** (2.28×) |
| `SlotAlloc::set` | 26 → **19** | 417 511 → **314 877** |
| `lemma_set_bit` | 18 → **14** | 99 405 → **94 353** |

Crate (A5's only surface):

| metric | before | after | ratio |
|---|---:|---:|---:|
| crate SMT total | 148 ms | 123 ms | **0.83× (−17 %)** |

The decisive, run-independent signal is `lemma_bit_other`'s **rlimit halving**
(263 683 → 115 806, 2.28×): replacing two guarded body asserts with one
unconditional signature-level `by (bit_vector)` query is a genuine proof-size
reduction, not ms noise. The crate-total drop (148 → 123 ms) reproduces the plan's
`[measured]` projection (158 → 121 ms) in direction and magnitude; the small
absolute differences are run-to-run variation against a slightly different
reference. **Optimization criterion met: the target fns and the crate SMT total
both measurably dropped.**

## Clarity (§2 step 4)

**Cleaner.** The recipe form is exactly the shape `verus.md` §6 holds up as
canonical for allocators/presence maps, so the lemmas now *match their own
guideline*. The runtime `free: bool` selector and the `free ==>` / `!free ==>`
guards disappear; each lemma is a flat two-clause unconditional contract with an
empty body, and the `by (bit_vector)` tactic sits visibly on the signature rather
than buried in two body asserts. The call sites lose a redundant argument while the
exec branch they sit beside is untouched. Net −6 lines, and the proof reads as a
direct citation of the bit identity instead of a direction-cased re-derivation.

## Host tests

`cargo test -p urt` — green: **22 passed, 0 failed** (the slot-bitmap family —
`alloc_free_reuse_same_slots`, `contiguous_search_skips_holes`,
`exhaustion_returns_none`, `spans_multiple_words`, `double_free_panics` — exercises
the `set`/`is_free_spec` bridge the lemmas back, plus the time and heap suites). The
change is proof-only (lives inside `proof { }` and lemma signatures), so runtime
behavior is unaffected by construction.

## Decision

**KEEP.** Both asymmetries satisfied: as an *optimization* the target fns and the
crate SMT total measurably dropped (148 → 123 ms, −17 %; `lemma_bit_other` rlimit
2.28×), and as a *simplification* the diff is a clear readability win (recipe
conformance, no runtime selector, fewer lines) with the gate passing 25/0 and all
22 host tests green.

> verified **Y** (29 → **25**, −4 collapsed sub-obligations) · `lemma_bit_other`
> **33 ms / rlimit 263 683 → 19 ms / rlimit 115 806** (2.28×) · `SlotAlloc::set`
> **26 → 19 ms** · `lemma_set_bit` **18 → 14 ms** · crate SMT **148 → 123 ms**
> (−17 %) · clarity **cleaner** → **KEEP**
