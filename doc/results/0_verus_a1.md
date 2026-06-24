# A1 — freelist `wf` sortedness-trigger projection fix (evaluation)

Task **A1** (rank 1) from `doc/plans/0_verus-optimization.md`: the single biggest
lever in the worklist, and one of the pre-measured entries. This file records the
per-attempt evaluation under the plan's §2 protocol. Temporary intermediate
report (per `CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** optimization (quantifier-profiling).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p freelist` before each); gate and timing
  taken from separate cold `cargo verus verify -p freelist` runs. Compared
  against the on-disk baseline captured 2026-06-24
  (`target/verus-baseline/freelist.json` / `summary.txt`).

## The change

One line in `pub closed spec fn wf(self)` (`freelist/src/lib.rs:82`), the third
`&&&` conjunct (strict sortedness). No proof-body change.

```diff
-        &&& (forall|k: int| #![trigger self.free@[k]]
+        &&& (forall|k: int| #![trigger self.free@[k].0, self.free@[k].1]
                 0 <= k < self.nfree - 1
                 ==> (self.free@[k].0 as int + self.free@[k].1 as int)
                         < self.free@[k + 1].0 as int)
```

The bare whole-tuple trigger `self.free@[k]` self-perpetuated a matching loop
(the body keeps reintroducing `self.free@[k+1]` of the same tuple shape), which
flooded the solver context. The projection pair `.0, .1` covers exactly the
terms the body reads and stops the loop — matching the two sibling conjuncts on
lines 79/81, which already trigger on the projections.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p freelist && cargo verus verify -p freelist` ended with the line

```
verification results:: 29 verified, 0 errors
```

**present** (a real run, not stale cache). `N` is unchanged at **29** — A1 adds
no new `proof`/`spec fn`, exactly as predicted. **Gate: PASS (Y).**

(The pre-existing "low confidence trigger" note at `freelist/src/lib.rs:1157` is
unrelated to A1 — it is the target of the separate task A1++ and is not an error.)

## Measurement (§2 step 2b — cold timing vs. baseline)

Crate-level (ms):

| metric | before | after | ratio |
|---|---:|---:|---:|
| **SMT cpu total** | **13 017** | **4 569** | **2.85×** |
| verify phase | 4 907 | 2 080 | 2.36× |
| verus wall total | 5 141 | 2 314 | 2.22× |

Per-function (SMT `ms` / `rlimit`):

| function | before | after | ms ratio |
|---|---:|---:|---:|
| `FreeList::free` | 4 539 / 169 316 355 | **145 / 1 149 427** | **31.3×** |
| `FreeList::free_insert` | 2 799 / 194 377 606 | 1 722 / 110 940 697 | 1.63× |
| `FreeList::free_replace` | 2 614 / 84 966 835 | 1 480 / 42 806 087 | 1.77× |
| `FreeList::free_both` | 1 574 / 94 568 156 | 587 / 31 387 180 | 2.68× |
| `FreeList::alloc` | 1 041 / 17 078 475 | 295 / 2 938 547 | 3.53× |

Every targeted obligation dropped on both ms and rlimit; the headline `free`
obligation fell 31× in time and ~147× in rlimit. The crate-total drop (8.4 s) is
an order of magnitude beyond the ±5–15 % run-to-run noise band, so one cold
measure is decisive — no median-of-three needed. The result reproduces the
plan's `[measured]` projection (13.3 → 4.7 s, 2.85×; `free` ~4.6 s → ~149 ms).

## Clarity (§2 step 4)

**Neutral → cleaner.** All three of `wf`'s sortedness/bounds conjuncts (lines
79/81/82) now trigger consistently on the `.0`/`.1` projections instead of one
of them using a divergent whole-tuple trigger. Single-line diff, no body change.

## Decision

**KEEP.** Optimization asymmetry satisfied: the target function **and** the crate
SMT total both dropped hard, clarity did not regress (it improved).

> verified **Y** · `free` **4539 → 145 ms** · crate-total **13 017 → 4 569 ms**
> (2.85×) · clarity **neutral/cleaner** → **KEEP**
