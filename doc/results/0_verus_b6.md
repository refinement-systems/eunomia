# B6 — `slot_move` C3 relabel-block lemma (evaluation)

Task **B6** (rank 7, Wave B) from `doc/plans/0_verus-optimization.md`: extract the
**C3 non-child relabel** `assert forall|k|` block out of `kcore::cspace::slot_move`
into a tightly-keyed `proof fn lemma_slot_move_m4_nonchild`, on the theory that
`slot_move` (≈4 % of kcore SMT) carries that block's cost inside one oversized
solver context and a fresh-context lemma would shrink it (`doc/guidelines/verus.md`
§10: decomposition is the default fix). This file records the per-attempt evaluation
under the plan's §2 protocol. Temporary intermediate report (per `CLAUDE.md`, not
citable from code/specs/guidelines).

**Verdict: REVERT** — the change verifies (it is sound, gate 403/0) but it is a
measured *regression*, not a speedup, and the clarity case is weak. Details below.

- **Kind:** the plan rates B6 "both" (opt + simp). On the **optimization** axis the
  bar (§2) is: keep only if the target fn **and** the crate SMT total measurably
  drop. Both rose, so it fails outright — "an optimization that does not measurably
  speed verification is worthless even if harmless — drop it." On the
  **simplification** axis the bar is a clear readability win with no material
  (<5 %) crate regression; the diff is *net +48 lines* that restates the splice
  maps and serves a single site, so the clarity win does not carry it either.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each); `cargo verus verify -p
  kcore` for the gate, `--time-expanded --output-json` for timing. Per §2, per-fn
  wall ms wobbles ±5–15 %, so the deterministic `rlimit` carries the claim; the two
  cold "after" runs produced **byte-identical rlimits**, confirming determinism.
- **Baseline.** The committed `target/verus-baseline/` is the *pre-B-wave* tree (391),
  so a fresh cold baseline was taken on the unedited B6-branch tip (post-B5a): **402
  verified, kcore SMT-run 61 840 ms / rlimit 160 308 306**, `slot_move` **3 881 ms /
  rlimit 6 268 877**.

## The change (reverted) — one file: `kcore/src/cspace.rs`

A new non-`pub` `proof fn lemma_slot_move_m4_nonchild(m0, src, dst, m1, m2, m3, m4,
rl)` placed immediately before `slot_move`. Its `requires` restate the `m1..m4`
insert-chain (the straight-line splice map, `m1 == m0.insert(dst, m0[src])` then the
three neighbour fixups verbatim from the `let ghost` bindings) plus `cspace_wf(m0)` /
`is_empty_cap(m0[dst].cap)`; its `ensures` is the quantified
`forall|k| … k != src && m0[k].parent != Some(src) ==> #[trigger] m4[k] == rl[k]`.
The body is the original C3 block verbatim (the `k == dst` arm + the
`lemma_generic_relabeled` else arm), prefaced by one `lemma_dst_relabeled(m0, src,
dst)` call to supply `rl[dst] == m0[src]`. The 24-line inline C3 block in `slot_move`
collapses to a single call:

```rust
// C3: each non-child slot other than src lands on its renamed value rl[k].
lemma_slot_move_m4_nonchild(m0, src, dst, m1, m2, m3, m4, rl);
```

Net file change: **+72 / −24** (the requires must re-declare the four splice maps,
so the lemma is larger than the block it replaces). The `#[trigger] m4[k]` is
preserved, so the downstream loop/post-loop behaviour is unchanged.

**Reuse finding.** The plan note "the same forall recurs post-loop … and the lemma
can serve those too" does **not** hold. The post-loop forall (now ~10270) is already
an empty-body `by {}` riding the loop invariant `store == m4` (non-children) + C3; the
final-block foralls (~10302–10311) call `lemma_generic_relabeled` directly for
*cap-only* facts (`is_empty_cap`, `.cap` equality), a different shape. So B6 is a
**single-site** extraction — no dedup payoff.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p kcore && cargo verus verify -p kcore` (after `cargo fmt`) ended

```
verification results:: 403 verified, 0 errors
```

**present** (a real cold run). `N` rose **402 → 403**, **+1**, exactly the one new
`proof fn` — the predicted delta. The lemma is **sound**; the rejection below is on
the §2 speed/clarity asymmetry, not correctness. (After the decision, the code was
reverted; the tree returns to **402**, the ledger is untouched.)

## Measurement (§2 step 2b — cold timing vs. baseline, two cold "after" runs)

| obligation | SMT ms (before → after) | rlimit (before → after) | verdict |
|---|---:|---:|---|
| `cspace::slot_move` | 3 881 → 4 033 / 4 023 (+3.8 %) | 6 268 877 → **6 540 882** (**+4.3 %**) | **regressed** |
| `cspace::lemma_slot_move_m4_nonchild` (new) | — → 157 / 146 | — → 417 742 | added cost |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT-run (ms) | 61 840 | 62 230 / 62 061 | +0.4–0.6 % |
| kcore SMT-run (rlimit) | 160 308 306 | **162 120 800** | **1.011× (+1.13 %)** |

The deterministic signal is decisive and the *wrong* way: `slot_move`'s **own**
rlimit rose **+272 005 (+4.3 %)** — extracting the C3 block did not isolate cost, it
*added* it. The inline asserts (`m4[dst] == m1[dst]`, the five per-field equalities)
were evidently load-bearing intermediates for `slot_move`'s later blocks; once only
the quantified `m4[k] == rl[k]` conclusion survives, the rest of the proof re-derives
them, and the new lemma costs a further 417 742 rlimit on top. Both runs gave
identical rlimits, so this is not ms noise.

The crate-total rlimit rose **+1 812 494 (+1.13 %)**. Only ~689 k of that is directly
attributable (`slot_move` +272 k, new lemma +418 k); the remainder is **module-wide
ripple** from inserting a `proof fn` into the `cspace` module — Verus's per-function
rlimit is reproducible for identical source but shifts unrelated functions' queries
when the module's definition set changes (e.g. `delete` +837 726, `lemma_remove_chain`
+358 497, `cdt_insert_child` +101 854, offset by `lemma_ready_remove_chain` −478 779;
none call the new lemma). The ripple is not a clean per-op signal, but its net is
clearly upward, and the directly-attributable target cost (`slot_move` +4.3 %) settles
the question on its own.

## Clarity (§2 step 4)

**Net negative-to-neutral, not the hoped-for win.** Unlike B5a (two byte-identical
blocks → one shared lemma, a genuine dedup), B6 is single-site: the lemma's `requires`
must **re-declare the four `m1..m4` splice-map definitions verbatim** (they exist only
as `let ghost` locals inside `slot_move`), so the contract duplicates the
straight-line construction rather than hiding it. The result trades a 24-line inline
`assert forall` for a ~70-line lemma + call that restates the maps — more surface area,
no reuse, and the reader must now cross-reference the lemma against the local map
bindings to confirm they match. The named contract has some documentary value, but it
does not clear the "clear readability win" bar that the simplification asymmetry
requires to tolerate a crate regression.

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change was
confined to one new `proof fn` plus one `proof {}` call-site swap (all erase in a
normal build), so exec behaviour is unchanged by construction. `cargo verus verify -p
kcore` reported 403/0 with the change; after reverting, the tree is back at 402/0.

## Decision

**REVERT.** B6 fails the optimization asymmetry decisively — the target fn
(`slot_move`) and the crate SMT total both *rose* (deterministic rlimit, two
identical cold runs), the opposite of the goal — and it does not redeem itself as a
simplification, being a single-site extraction whose `requires` restate the splice
maps for a net +48 lines. The plan's premise (the C3 block's cost is isolable and a
fresh-context lemma shrinks `slot_move`) is **refuted by measurement**: the inline
asserts are load-bearing intermediates, so removing them makes `slot_move` more
expensive, not less. The lemma is sound but unprofitable; the code is reverted and the
trusted-base kcore row stays **402**. Recorded so a future reader does not re-attempt
it as "new."

> verified **Y** (402 → **403**, +1 lemma; reverted to **402**) · `slot_move`
> **3 881 → 4 033 ms / rlimit 6 268 877 → 6 540 882** (**+3.8 % / +4.3 %, regressed**) ·
> new lemma **~150 ms / 417 742 rlimit** · kcore SMT-run **61 840 → 62 230 ms (+0.6 %) /
> rlimit 160.31 M → 162.12 M (+1.13 %, regressed)** · single-site, net +48 lines ·
> clarity **not a clear win** → **REVERT**
