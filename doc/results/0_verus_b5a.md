# B5a — cspace children-walk per-iteration peel lemma (evaluation)

Task **B5a** (rank 6, Wave B) from `doc/plans/0_verus-optimization.md`: extract the
*verbatim-duplicated* per-iteration "peel" proof block carried inline by the two
children-reparent walks in `kcore::cspace` — `cdt_unlink` and `slot_move` — into one
shared `proof fn lemma_children_walk_peel`. Both walks step a cursor from a child `cur`
to its `next_sib` `nn` and must re-key the loop invariant's `next_reach(m0, cur, …)`
clauses onto `nn`; the bridge is the assertion that for every other node `x`,
`next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk)`. The two copies were
byte-for-byte identical (same `m0`/`cur`/`nn`/`srk` names). Per `doc/guidelines/verus.md`
§10 (decomposition is the default fix; key it tightly), the block becomes one named,
tightly-keyed lemma. This file records the per-attempt evaluation under the plan's §2
protocol. Temporary intermediate report (per `CLAUDE.md`, not citable from
code/specs/guidelines).

- **Kind:** the plan rates B5a "both" but frames it as "primarily a clarity dedup with a
  modest speed effect." The modest speed effect did **not** materialise (SMT-neutral, see
  Measurement), so it is kept on the **simplification** axis. Simplification keep/drop bar
  (§2): keep only if the diff is a clear readability win and the crate SMT total does
  **not** materially regress (<5 % tolerance); the deterministic `rlimit` is the
  run-independent signal.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each); `cargo verus verify -p
  kcore` for the gate, `--time-expanded --output-json` for timing. Per §2, per-fn wall ms
  wobbles ±5–15 %, so the deterministic `rlimit` carries the claim.
- **Baseline.** The committed `target/verus-baseline/` is the *pre-B-wave* tree (391;
  `cdt_unlink` 29 716 ms / rlimit 63.7 M before B3 crushed it), so a fresh cold post-B4
  baseline was taken on the unedited B5a-branch tip: **401 verified, kcore SMT 61 888 ms /
  rlimit 160 357 260**, `cdt_unlink` **4 249 ms / rlimit 7 079 635**, `slot_move`
  **3 855 ms / rlimit 6 267 744**.

## The change — one file: `kcore/src/cspace.rs`

One new non-`pub` `proof fn` placed in the `next_reach` lemma cluster (after
`lemma_child_on_chain`):

```rust
proof fn lemma_children_walk_peel(m0: Map<SlotId, CapSlot>, cur: SlotId, nn: SlotId, srk: Map<SlotId, nat>)
    requires
        m0[cur].next_sib == Some(nn),
        srk[nn] < srk[cur],
    ensures
        forall|x: SlotId| x != cur ==> #[trigger] next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk),
{
    assert forall|x: SlotId| x != cur
        implies #[trigger] next_reach(m0, cur, x, srk) == next_reach(m0, nn, x, srk) by {}
}
```

The two `requires` are exactly the cheap local facts each loop body already asserts on
the line above the old peel (`m0[cur].next_sib == Some(nn)`; `srk[nn] < srk[cur]` from
`valid_srank`), so the call sites discharge them with no new work; the body is the
original empty-`by {}` assert (`next_reach` is `pub open spec`, so it unfolds one step at
the symbolic argument). The `ensures` carries the identical `#[trigger]
next_reach(m0, cur, x, srk)`, so the next-iteration invariant re-keys exactly as before.

The two inline peel asserts (in `cdt_unlink`'s `Some(nn) =>` arm and `slot_move`'s, each
two lines) collapse to a single call:

```rust
lemma_children_walk_peel(m0, cur, nn, srk);
```

Every other line in each arm (the four shape asserts and the conditional
`lemma_next_reach_sr` call, which feed *other* invariant clauses) is untouched. Net file
change: +18 / −4 lines.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p kcore && cargo verus verify -p kcore` (after `cargo fmt`) ended

```
verification results:: 402 verified, 0 errors
```

**present** (a real cold run). `N` rose **401 → 402**, **+1**, exactly the one new
`proof fn` — the predicted delta. **Gate: PASS (Y).** The trusted base is unchanged (one
ordinary verified proof, no new seam); the `external_body`/`assume_specification` tally
stays **14**; the ledger kcore row updates 401 → 402.

## Measurement (§2 step 2b — cold timing vs. the post-B4 baseline)

| obligation | SMT ms (before → after) | rlimit (before → after) | verdict |
|---|---:|---:|---|
| `cspace::cdt_unlink` | 4 249 → 4 323 (+1.7 %) | 7 079 635 → 7 156 805 (+1.1 %) | flat |
| `cspace::slot_move` | 3 855 → 3 967 (+2.9 %) | 6 267 744 → 6 268 877 (+0.02 %) | flat |
| `cspace::lemma_children_walk_peel` (new) | — → 3 | — → 17 753 | cheap |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT total (ms) | 61 888 | 63 901 | 1.03× (+3.3 %) |
| kcore SMT total (rlimit) | 160 357 260 | 160 308 306 | **1.00× (−0.03 %)** |

The deterministic **crate-total rlimit is flat** (−0.03 %, a hair *lower*): there is no
proof-size regression. The +3.3 % crate-**ms** is wall-clock jitter, proven the same way
the B4 evaluation did — the three big untouched teardown ops have **byte-identical
rlimits** before and after (`remove_waiter` 19 005 838, `signal` 20 909 644, `destroy_tcb`
34 510 921), so this change provably did not touch them, yet their ms rose 2.4–5 %
(9 197→9 422, 11 362→11 759, 13 430→14 100). That op-level ms drift *is* the crate-total
ms drift; it is jitter, not a B5a effect (§2: trust the rlimit over ms noise). The two
edited ops' rlimits barely moved (`cdt_unlink` +1.1 %, `slot_move` +0.02 %) and the new
lemma costs 3 ms / 17 753 rlimit. No `rlimit`/`spinoff_prover` attribute is involved on
either op, so there is no last-resort lever to retire here.

## Clarity (§2 step 4)

**Cleaner.** Two byte-identical inline peel blocks — each an `assert forall|x| … implies
next_reach(…)==next_reach(…) by {}` whose intent had to be reconstructed from the
surrounding loop — become one named lemma with an explicit `requires`/`ensures` contract
that *states* the property (cursor advance `cur`→`nn` preserves sibling reachability for
`x != cur`), plus a one-line call at each site. The duplication is gone, the contract is
self-documenting, and the lemma sits beside its `next_reach` kin
(`lemma_next_reach_sr`/`lemma_next_reach_extend`/`lemma_child_on_chain`).

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change is confined
to one new `proof fn` plus two `proof {}` call-site swaps (all erase in a normal build),
so exec behaviour is unchanged by construction. `cargo build -p kcore` compiles clean.

## Decision

**KEEP.** The simplification asymmetry is satisfied: the diff is a clear readability win
(two verbatim blocks → one contracted lemma) and the crate SMT total did not materially
regress — the deterministic crate-total rlimit is flat (−0.03 %), and the +3.3 % ms is
shown to be jitter by the byte-identical untouched-op rlimits. The expected "modest speed
effect" did not materialise (the peel was already a 3-line empty assert), so this lands as
a clarity dedup, exactly as the plan framed it. Gate 402/0.

> verified **Y** (401 → **402**, +1 lemma) · `cdt_unlink` **4 249 → 4 323 ms / rlimit
> 7 079 635 → 7 156 805** (+1.7 % / +1.1 %) · `slot_move` **3 855 → 3 967 ms / rlimit
> 6 267 744 → 6 268 877** (+2.9 % / +0.02 %) · new lemma **3 ms / 17 753 rlimit** · kcore
> SMT **61 888 → 63 901 ms (+3.3 %, jitter) / rlimit 160.36 M → 160.31 M (−0.03 %, flat)** ·
> clarity **cleaner** → **KEEP**
