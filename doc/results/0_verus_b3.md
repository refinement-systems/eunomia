# B3 — `cdt_unlink` merge-block extraction (evaluation)

Task **B3** (rank 4, Wave B) from `doc/plans/0_verus-optimization.md`: lift the heavy
inline merge case-split carried by `kcore::cspace::cdt_unlink` — the gate's **single
largest obligation** — into a tightly-keyed `proof fn` (`doc/guidelines/verus.md` §10:
decomposition is the default fix once `spinoff_prover`/`rlimit` are exhausted, which
`cdt_unlink` already was at `spinoff_prover + rlimit(60)`). The op closes by proving
`assert(mfin =~= unlinked(m0, slot, last)) by { … }` — a per-key case split over
`slot`/child/non-child roles — against its *whole* body context: the 35-invariant
children-walk loop, the `next_reach`/`valid_srank` recursion, and the `valid_srank`
`choose` witness, none of which the merge needs. This file records the per-attempt
evaluation under the plan's §2 protocol. Temporary intermediate report (per
`CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** both — optimization + simplification (decompose). Optimization keep/drop
  bar (§2): keep only if the target fn **and** the crate SMT total measurably drop
  (rlimit drop decisive); simplification axis judged on the diff.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each);
  `cargo verus verify -p kcore` for the gate, `--time-expanded --output-json` for
  timing. The deterministic `rlimit` field is the run-independent signal (§2); per-fn
  wall ms wobbles ±5–15 %, so the rlimit carries the claim.
- **Baseline.** Developed on the post-B2 branch (`cdt_unlink` is untouched by B1/B2 —
  they edit `thread.rs` / `notification.rs` and add sibling lemmas — so its cost is
  B1/B2-independent). Fresh cold pre-B3 baseline on this host: **396 verified, kcore
  SMT 87 861 ms**, `cdt_unlink` **26 071 ms / rlimit 63 727 883**. (The plan's §1 table
  cited 29 716 ms / same rlimit off the pre-B1 391-tree; the rlimit is identical, the
  ms differs within the noise band — the run-independent rlimit is the anchor.)

## The change

One new `proof fn` in `cspace.rs`, beside the `lemma_unlink_*` family (modelled on the
isomorphic `lemma_unlink_children`, which verifies cheaply in isolation):

- **`lemma_unlink_merge(m0, mw, ma, mb, mc, md, mfin, slot, last, parent, prev, next,
  first, head)`** — `ensures mfin =~= unlinked(m0, slot, last)`. `requires` = exactly
  the cheap local facts the op already holds at the merge point: the slot-role bindings
  (`parent == m0[slot].parent`, …, `head == if first is None { next } else { first }`);
  the re-parented arena `mw`; the **four straight-line splice steps as single
  `Map::insert` equalities** (`ma`/`mb`/`mc`/`md`, §10 "prefer a single `Map::insert`
  equality over a broad frame `forall`"); and `slot`'s untouched-then-cleared entry
  (`md[slot] == m0[slot]`, `mfin =~= md.insert(slot, mfin[slot])`, `mfin[slot]` = the
  detached empty-links slot). Body = the verbatim per-key case split, prefaced by
  `lemma_unlink_roles(m0, slot)` (its role facts follow from the `cspace_wf(m0)`
  `requires`). The merge references **none** of `next_reach`/`valid_srank`/the `choose`.

Call-site rewrite in `cdt_unlink`:

- The inline `assert(mfin =~= unlinked(…)) by { … }` (the ~38-line case split) collapses
  to a single `lemma_unlink_merge(…)` call. The op already proves every `requires` fact
  (the `=~= ma/mb/mc/md` store-view asserts, `md[slot] == m0[slot]`, the `mfin` insert),
  so the call site is a one-line citation. The trailing cap-frame `assert forall|x| …
  mfin[x].cap == m0[x].cap` and the `lemma_unlink_preserves_cspace_wf` /
  `lemma_unlink_count` calls are left in place (cheap, read off the established merge).
- **`cdt_unlink` rlimit (§10 cleanup)** — `#[verifier::rlimit(60)]` → `(10)`: post-
  extraction the op consumes 7.08 M; `rlimit(10)` budgets ~2× that (the deterministic
  floor from `destroy_tcb`'s `rlimit(24)`/34.5 M puts the unit at ≥1.44 M/point), re-
  verified 0 errors. The misleading "60-budget monster" signal retires.
- **Rank-9 (optional spinoff on the extracted lemma): omitted.** The plan scopes it
  "only if the extraction alone doesn't bring it down enough." The extraction cut
  `cdt_unlink` 6.1×; the lemma is 1.85 s and already its own isolated query, so a fresh
  `spinoff_prover` instance buys nothing. Skipped per the plan's "otherwise omit".

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p kcore && cargo verus verify -p kcore` ended

```
verification results:: 397 verified, 0 errors
```

**present** (a real cold run). `N` rose **396 → 397**, **+1**, exactly the one new
`proof fn` — the predicted delta. **Gate: PASS (Y).** Re-run at `rlimit(10)` also ended
`397 verified, 0 errors`. `cargo build -p kcore` compiles clean (proof code erases). The
trusted-base tally is unchanged (the lemma is an ordinary verified proof, not a new
seam); the ledger kcore row updates 396 → 397.

## Measurement (§2 step 2b — cold timing vs. the pre-B3 baseline)

| obligation | SMT ms (before → after) | rlimit (before → after) | verdict |
|---|---:|---:|---|
| `cspace::cdt_unlink` | 26 071 → **4 302** (−83.5 %) | 63 727 883 → **7 079 635** (−88.9 %) | **win** |
| `cspace::lemma_unlink_merge` (new) | — → 1 851 | — → 4 561 284 | new |
| **cdt_unlink + lemma (combined)** | 26 071 → **6 153** (−76.4 %, **4.24×**) | 63 727 883 → **11 640 919** (−81.7 %) | **win** |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT total | 87 861 ms | 62 301 ms | **0.71× (−29.1 %)** |

The decisive run-independent signal is `cdt_unlink`'s **rlimit collapse 63.7 M → 7.08 M
(−88.9 %)**: a genuine proof-size reduction, not ms noise. Even charging the full cost
of the new lemma against the op, the combined query is **4.24×** smaller in wall ms and
**−81.7 %** in rlimit. The hypothesis is confirmed to the letter — the per-key case split
solving inside `cdt_unlink`'s full context (the walk loop's term families, the
`next_reach`/`valid_srank` quantifiers, the `choose` witness) was the dominant cost;
isolated to a fresh solver keyed only on the splice chain, it is 1.85 s and the op's
remainder is 4.3 s. The kcore SMT total fell **25 560 ms (−29.1 %)**; its deterministic
floor is `cdt_unlink`'s ~19.9 s saving (the balance is within the wall-ms noise band on
the unrelated heavy ops `destroy_tcb`/`signal`/`remove_waiter`, which my change does not
touch). This **beats the plan's own pessimistic projection** ("the merge alone is
unlikely to halve it") by a wide margin — the merge was not merely a slice of the cost,
it was poisoning the whole `cdt_unlink` query.

## Clarity (§2 step 4)

**Cleaner.** `cdt_unlink`'s closing block — the gate's heaviest single case split — is
now a one-line citation of `lemma_unlink_merge`; the heavy per-key reasoning lives in a
named, explicitly-contracted lemma that joins the existing `lemma_unlink_*` family
(`_roles`, `_sib`, `_links`, `_siblings`, `_children`, `_count`,
`_preserves_cspace_wf`). The lemma carries +~125 lines of `requires`/`ensures` + body
(the §10 decomposition tradeoff: an explicit contract for a small-context query);
net file change is +119/−39. The retired `rlimit(60)` → `(10)` removes a misleading
"this proof is hard" signal now that it is not.

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change is
confined to a new `proof fn`, one `proof {}` call-site swap, and one `rlimit` attribute
(all erase in a normal build), so exec behaviour is unchanged by construction.
`cargo build -p kcore` compiles clean.

## Decision

**KEEP.** The optimization asymmetry is satisfied decisively on both required axes —
`cdt_unlink` fell 26 071 → 4 302 ms with its rlimit **−88.9 %**, and the crate SMT total
fell 87 861 → 62 301 ms (**−29.1 %**), the single largest gate-wide win in the worklist
to date — while the new lemma adds 1.85 s and the simplification axis is a clear win
(the gate's heaviest case split is now a one-line lemma citation). Rank-9 spinoff
omitted (unnecessary); `cdt_unlink` `rlimit(60)→(10)`. Gate 397/0.

> verified **Y** (396 → **397**, +1 lemma) · `cdt_unlink` **26 071 ms / rlimit
> 63 727 883 → 4 302 ms / rlimit 7 079 635** (−83.5 % / −88.9 %) · new `lemma_unlink_merge`
> **1 851 ms** · combined **4.24× / −81.7 % rlimit** · kcore SMT **87 861 → 62 301 ms**
> (−29.1 %) · `cdt_unlink` `rlimit(60)→(10)` · clarity **cleaner** → **KEEP**
