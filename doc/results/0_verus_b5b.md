# B5b — share the cspace children-walk loop (`cdt_unlink` + `slot_move`) (evaluation)

Task **B5b** (rank 5, Wave B) from `doc/plans/0_verus-optimization.md`: the explicit
*design spike* of the set — investigate factoring the whole ~13-invariant children-
reparent `while` loop, carried near-verbatim by `kcore::cspace::cdt_unlink` and
`slot_move`, into one shared exec helper `reparent_children<S: Store>`. The plan flags
it "high risk", confidence 0.50, dep `B5a` + `B3` (both landed). This file records the
per-attempt evaluation under the plan's §2 protocol. Temporary intermediate report (per
`CLAUDE.md`, not citable from code/specs/guidelines).

**Verdict: REVERT** — the helper is implementable and **sound** (gate 402/0, whole-crate),
but it is an *asymmetric measured regression*: it speeds `cdt_unlink` (−23 % rlimit) yet
slows `slot_move` (+46.5 % rlimit) for a net **+2.11 % crate-total rlimit** regression. It
fails the optimization axis outright and does not clear the "clear readability win" bar the
simplification axis needs to tolerate a regression. Details below. The finding is that
*sharing* — the essence of B5b — is what makes it net-negative.

- **Kind:** the plan rates B5b "both" (opt + simp) and expects it to "hit both hot ops."
  On the **optimization** axis (§2): keep only if the target fn(s) **and** the crate SMT
  total measurably drop. The crate total rose and `slot_move` rose, so it fails — "an
  optimization that does not measurably speed verification is worthless even if harmless —
  drop it." On the **simplification** axis: a clear readability win with <5 % crate
  regression; the +2.11 % is within tolerance but the diff is a complex shared abstraction
  for a net −25 lines, not a clear win (below).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each); `cargo verus verify -p kcore
  -- --time-expanded --output-json` (the JSON carries `verification-results` *and* the
  per-fn timing, so one run is both gate and measure). Per §2 the deterministic `rlimit`
  carries the claim; **two** cold "after" runs produced **byte-identical rlimits**,
  confirming determinism (ms wobbles ±5–15 %).
- **Baseline.** Fresh cold baseline on the unedited B5b-branch tip (post-all-landed-B-wave):
  **402 verified, kcore SMT 62 249 ms / crate rlimit 160 308 306**, `cdt_unlink`
  **4 553 ms / rlimit 7 156 805**, `slot_move` **3 824 ms / rlimit 6 268 877**.

## The change (reverted) — one file: `kcore/src/cspace.rs`

A new non-`pub` `fn reparent_children<S: Store>(store, parent_slot, new_parent, first,
Ghost(m_ref), Ghost(srk)) -> last`, placed beside `lemma_children_walk_peel` in the
`next_reach` cluster. It is the existing loop, generalized:

- **`new_parent: Option<SlotId>`** unifies the two reparent targets (`cdt_unlink`'s
  grandparent `parent`; `slot_move`'s `Some(dst)`) — no closure/trait-param needed, since
  `relabeled(m0,src,dst)[x] == set_parent(m0[x], Some(dst))` for a child `x`
  (`lemma_child_relabeled`), so both walks write `set_parent(m_ref[x], new_parent)`.
- **A ghost reference arena `m_ref` decoupled from the live store.** This is forced: in
  `slot_move` the live store at loop entry is the mid-transposition `m4`, which is **not**
  well-formed (`cspace_wf(m4)` is false), so the walk's reachability/termination must be
  driven off the original `m0`. The `requires` only ask that the live store *agree with
  `m_ref` on the child subtree* (true by `slot_move`'s C2 block); the non-child frame is
  stated against `old(store)` so each caller composes it onto its own arena (`mw`/`m4`).
- **Full `last_wf` in the `ensures`.** `cdt_unlink`'s tail-uniqueness clause (`last_wf`
  clause 3) has no other source once the loop is hidden, so the helper returns the whole
  predicate and the `lemma_unique_tail` block moves inside it.

Call sites: `cdt_unlink` replaces its loop with `let last = reparent_children(store, slot,
parent, first, Ghost(m0), Ghost(srk));` (live == `m0`, so the child-agreement `requires`
is reflexive) then its existing `=~= mw` / `lemma_unlink_roles` tail. `slot_move` replaces
its loop with `reparent_children(store, src, Some(dst), d.first_child, …)` then a bridge
`assert forall|k| … k != src ==> store[k] == rl[k]` (children via `lemma_child_relabeled`,
non-children via the existing C3 `m4[k]==rl[k]`). Net file change: **+168 / −193 = −25
lines** (the helper is ~160 lines, so the ~190-line two-loop removal nets only −25).

## Gate (§2 step 2a — cold, authoritative, whole-crate)

Both cold runs ended `verification results:: 402 verified, 0 errors`
(`verification-results.success == true`, `is-verifying-entire-crate == true`). **Gate:
PASS (Y).** The helper is sound; the rejection below is the §2 speed/clarity asymmetry,
not correctness.

**On the count (predicted 403, observed 402 — explained, not a red flag):** Verus counts
each `while` loop as its own obligation. Removing the two inline loops (−2) while adding
one helper fn + its one loop (+2) nets **zero**, so the tally holds at 402. Everything
verifies (the new fn appears in the SMT breakdown at 277 696 rlimit). The trusted base is
unchanged: `reparent_children` is an ordinary generic fn over `S: Store`, **not** a `Store`
method — no new seam, the `external_body`/`assume_specification` tally stays 14.

## Measurement (§2 step 2b — two cold runs, rlimits byte-identical)

| obligation | SMT ms (before → after r1/r2) | rlimit (before → after) | verdict |
|---|---:|---:|---|
| `cspace::cdt_unlink` | 4 553 → 3 357 / 3 314 | 7 156 805 → **5 501 349** (**−23.13 %**) | **improved** |
| `cspace::slot_move` | 3 824 → 5 715 / 5 673 | 6 268 877 → **9 184 489** (**+46.51 %**) | **regressed** |
| `cspace::reparent_children` (new) | — → 94 / 96 | — → 277 696 | cheap |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT-run (ms) | 62 249 | 63 945 / 63 638 | +2.2–2.7 % |
| kcore crate rlimit | 160 308 306 | **163 690 662** | **1.021× (+2.11 %)** |

Both "after" runs gave **identical** rlimits (crate 163 690 662; `cdt_unlink` 5 501 349;
`slot_move` 9 184 489; new fn 277 696), so these are deterministic proof-size facts, not
ms noise. The directly-attributable net is `cdt_unlink` −1.66 M, `slot_move` +2.92 M, new
fn +0.28 M ≈ **+1.54 M**, the remainder module-wide ripple from inserting an exec fn (the
same effect B6 measured for a proof fn). The crate total rose **+3.38 M (+2.11 %)** — the
wrong direction.

## Why the asymmetry — the substantive finding

Extracting the loop gives `cdt_unlink` a **fresh, smaller solver context** for the
reparent walk (its merge is already a separate lemma after B3), and `cdt_unlink`'s live
arena *equals* `m_ref`, so the helper's `set_parent(m_ref[x], new_parent)` ensures lands
its post-walk `=~= mw` directly. Result: **−23 %**, a genuine win for that op alone.

`slot_move` regresses for an **intrinsic** reason, not a tunable one. The original loop
established each child's value as the move-specific `rl[cur] = relabeled(m0,src,dst)[cur]`
*per iteration* (one cheap, isolated `lemma_child_relabeled` call inside the loop body),
so the post-loop `store[k]==rl[k]` was an empty `by {}`. The **generic** helper cannot
produce `rl` (it knows only `new_parent`), so it returns children in `set_parent` form and
forces `slot_move` to re-prove `== rl[x]` for *all* children at once, in a single forall
re-instantiated against `slot_move`'s full finalization context (m1..m4, C1/C2/C3, `rl`,
the `=~= rl.insert(src, …)` rebuild still ahead). That whole-children re-instantiation is
new work the inline loop never paid — exactly the B6 failure mode (load-bearing inline
intermediates re-derived once only the quantified conclusion survives). Un-sharing the two
loops is the only thing that removes it, which would defeat B5b.

So the *sharing itself* is the cost: the abstraction that suits `cdt_unlink` is a poor fit
for `slot_move`, and there is no `new_parent`-generic contract that yields `slot_move`'s
`rl`-shaped post-condition cheaply.

## Clarity (§2 step 4)

**Not a clear win.** The dedup is real (two near-verbatim ~95-line loops → one helper),
but the net is only −25 lines (the helper is large), and the contract is *more* demanding
to read than the inline loops: a reader must understand why `m_ref` is decoupled from the
live store (the `m4`-not-well-formed subtlety), the `s0` non-child frame, and the
`set_parent`→`rl` bridge at the `slot_move` site. B5a already extracted the one
verbatim-identical sub-block (the peel) at zero cost; what remains shared here is the exec
scaffolding, whose factoring trades two straightforward loops for one generic helper plus
two non-trivial call-site bridges. The +46 % `slot_move` blowup is a standing signal the
abstraction is forced. Per the plan's own §F note, the proof-fn fallback is strictly worse
(nothing verbatim left to share after B5a), so it was not pursued.

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change is a new
exec fn plus two call-site rewrites whose proof content is unchanged (the same lemmas, the
same loop), so exec behaviour is unchanged by construction. `cargo verus verify -p kcore`
reported 402/0 with the change; after reverting, the tree is byte-identical to the
baseline (402/0).

## Decision

**REVERT.** B5b fails the optimization asymmetry (crate total +2.11 % and `slot_move`
+46.5 %, both deterministic) and does not redeem itself as a simplification (a complex
shared abstraction for −25 lines, with a +46 % regression on a hot op, is not a clear
readability win). The plan's premise that the shared loop "hits both hot ops" is **refuted
by measurement**: it helps `cdt_unlink` and hurts `slot_move`, because a `new_parent`-
generic helper cannot reproduce `slot_move`'s `relabeled` post-condition without a costly
whole-children re-instantiation. The code is reverted; the trusted-base kcore row stays
**402**. Recorded so a future reader does not re-attempt the sharing as "new."

> *Adjacent observation (not a B5b keep):* the extraction **alone** speeds `cdt_unlink`
> −23 % because it earns a fresh solver context. A `cdt_unlink`-only loop extraction (no
> sharing, no `slot_move` change) could be a standalone optimization worth its own pass —
> but that is a single-op decomposition (B3-family), not the cross-op share B5b is, and is
> out of scope here.

> verified **Y** (gate **402 / 0**, whole-crate; loop-count nuance keeps N at 402) ·
> `cdt_unlink` **4 553 → 3 357 ms / rlimit 7 156 805 → 5 501 349 (−23.13 %, improved)** ·
> `slot_move` **3 824 → 5 715 ms / rlimit 6 268 877 → 9 184 489 (+46.51 %, regressed)** ·
> new fn **94 ms / 277 696 rlimit** · kcore crate **rlimit 160.31 M → 163.69 M (+2.11 %,
> regressed)** · net −25 lines, complex shared contract · clarity **not a clear win** →
> **REVERT**
