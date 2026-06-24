# B2 — shared `signal`/`remove_waiter` census-delta lemma (evaluation)

Task **B2** (rank 3, Wave B) + its follow-ons **rank 20/21/23** from
`doc/plans/0_verus-optimization.md`: lift the heavy inline per-object census-delta
map carried by the notification fire/wait ops into a tightly-keyed `proof fn`
(`doc/guidelines/verus.md` §10: decomposition is the default fix once
`spinoff_prover`/`rlimit` are exhausted, which `signal`/`remove_waiter` both are).
Each op proved `assert forall|o| obj_census(store, o) == (if o == n { … ∓ 1 } else
{ … })` against its *whole* 14-clause context. The plan's thesis was a context-size
problem hitting two multi-second obligations at once. This file records the
per-attempt evaluation under the plan's §2 protocol. Temporary intermediate report
(per `CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** both — optimization + simplification (decompose). Optimization keep/drop
  bar (§2): keep a lemma application only if the target fn **and** the crate SMT
  total measurably drop (rlimit drop decisive); a simplification follow-on only on a
  clear clarity win with <5 % crate regression.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each);
  `cargo verus verify -p kcore` for the gate, `scripts/verus-baseline.sh kcore`
  (`--time-expanded --output-json`) for timing. The deterministic `rlimit` field is
  the run-independent signal (§2); confirmed stable — `cdt_unlink` (63 727 883),
  `destroy_tcb` (34 510 921), and the *reverted* `signal` (20 909 644) reproduce to
  the digit across runs. Per-fn wall ms wobbles ±5–15 %, so the rlimit is the claim.
- **Baseline.** Developed off pre-B1 `main`, then **rebased onto post-#189 `main`**
  (`63b1222`, B1's `destroy_tcb` decomposition landed) and re-measured there — the
  authoritative apples-to-apples base. B2 is independent of B1 (B1 edits `thread.rs`;
  B2 edits `notification.rs` + a new lemma in `cspace.rs`) and the B2 targets
  (`signal`/`remove_waiter`/`wait`) are untouched by B1, so the per-fn deltas are
  B1-independent. Post-#189 cold pre-B2 baseline on this host: **394 verified, kcore
  SMT 93 386 ms**.

## The change

Two new `pub proof fn`s in `cspace.rs` (beside the `lemma_census_*` family, modelled
on the no-delta twin `lemma_census_frame_thread_halt`):

- **`lemma_waiter_dequeue_census<S>(s0, s1, n)`** — the `−1` map: a wake/splice that
  dequeues one waiter from `n`. `ensures forall|o| #[trigger] obj_census(s1, o) ==
  (if o == n { (obj_census(s0,o) − 1) as nat } else { obj_census(s0,o) })`, keyed on
  the final census so it stays out of census-agnostic callers. `requires` = the cheap
  local facts (the four non-tcb view frames; per-key cspace/aspace frozen; a single
  `notif_view().insert(n, …)` equality; the changed-TCB shape "`wait_notif` is `None`
  or `Some(n)` in both states" — the GLB across the callers; the `waiter_refs(n) − 1`
  delta). Body mirrors `lemma_census_frame_thread_halt`: `lemma_thread_hold_frame`
  + `lemma_waiter_refs_frame` off `n`, the delta at `n`.
- **`lemma_waiter_enqueue_census<S>(s0, s1, n)`** — the `+1` twin (rank 20), identical
  off-`n` frame, delta flipped, for `wait`'s block path.

Call-site rewrites in `notification.rs`:

- **`remove_waiter`** (present path) — proves the cheap local facts then calls the
  dequeue lemma once; `census_delta_frozen`, conditional `refcount_sound`, and
  `census_dom_complete`-preservation derive from the map + its local `refs[n] −= 1`.
  The former ~30-line per-object `forall` and the separate `census_dom_complete`
  block (**rank 21**) collapse to a one-line citation of the map.
- **`wait`** (block path, **rank 20**) — calls the enqueue twin, removing the third
  hand-rolled copy.
- **`remove_waiter` rlimit (rank 23)** — `#[verifier::rlimit(40)]` → `(25)`: the op
  was at the 40 M cap; post-lemma it consumes 19.0 M, so the budget is reduced (still
  comfortably above consumption, re-verified 0 errors).
- **`signal`** (wake path) — **reverted to the inline map** (see Decision); a comment
  records why.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

On the rebased branch, `cargo clean -p kcore && cargo verus verify -p kcore` ended

```
verification results:: 396 verified, 0 errors
```

**present** (a real cold run). `N` rose **394 → 396**, **+2**, exactly the two new
`proof fn`s — the predicted delta. **Gate: PASS (Y).** `cargo build -p kcore`
compiles clean (proof code erases). The trusted-base tally is unchanged (the lemmas
are ordinary verified proofs, not new seams); the ledger kcore row is updated 394 →
396.

## Measurement (§2 step 2b — cold timing vs. the post-#189 baseline)

| obligation | SMT ms (before → after) | rlimit (before → after) | verdict |
|---|---:|---:|---|
| `notification::remove_waiter` | 13 205 → **9 994** (−24.3 %) | 39 285 761 → **19 005 838** (−51.6 %) | **win** |
| `notification::wait` (rank 20) | 463 → **415** (−10.4 %) | 1.82 M → 1.43 M (−21 %) | **win** |
| `notification::signal` | 12 731 → 12 334 (≈ flat) | 20 909 644 → 20 909 644 (identical) | reverted |
| `cspace::lemma_waiter_dequeue_census` | — → 43 | — → 112 253 | new |
| `cspace::lemma_waiter_enqueue_census` | — → 28 | — → 130 166 | new |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT total | 93 386 ms | 87 807 ms | **0.94× (−6.0 %)** |

The decisive run-independent signal is `remove_waiter`'s **rlimit halving**
(39.3 M → 19.0 M, −51.6 %): moving the per-object census map into its own small-context
query is a genuine proof-size reduction, not ms noise. The two new lemmas cost **71 ms
combined**. The kcore SMT total fell **5 579 ms (−6.0 %)**; the deterministic floor of
that is `remove_waiter`'s ~3.2 s (the balance is within the wall-ms noise band on the
unrelated heavy ops `cdt_unlink`/`destroy_tcb`).

### Why `signal` was reverted (the §2 asymmetry, applied per-function)

Applying the *same* dequeue lemma to `signal`'s wake path **regressed** it: rlimit
20 909 644 → **34 076 881 (+63 %)** (and ~12 → ~19 s wall), pushing the crate total
*up*. The cause is structural: `signal`'s wake runs `make_runnable` (a faithful
enqueue), so at the census step the context still carries the ready-queue /
old-ready-tail (`p_opt`) term families. Discharging the lemma's `requires` there costs
**more** than the inline derivation saved, and the imported map `ensures` fires across
that larger context. `remove_waiter`'s post-splice context is small enough that the
extraction pays off; `signal`'s is not. §10's "small context" payoff only lands when
the *caller's* context is already small — a real boundary on the decomposition
technique, now documented inline at the `signal` site. Per the §2 optimization
asymmetry (an optimization that does not measurably speed verification is worthless),
the `signal` application was **dropped** and the inline map restored (rlimit identical
to baseline, confirming a faithful revert). `signal`'s `#[verifier::rlimit(50)]` is
left untouched (B2 did not improve `signal`, so the rank-23 reduction does not apply
to it).

## Clarity (§2 step 4)

**Cleaner** on the kept sites. `remove_waiter` loses ~50 lines of inline per-object
`forall` (the census-frozen block + the dom-complete block) for two lemma calls and a
one-line coverage citation; `wait` loses its hand-rolled copy. The two named lemmas
carry explicit `requires`/`ensures` contracts (+~100 lines in `cspace.rs`) — the §10
decomposition tradeoff — and join the existing `lemma_census_*` family. The `signal`
site is unchanged but now carries a comment recording the measured regression so a
future reader does not re-attempt the extraction.

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change is
confined to `proof {}` / `proof fn` bodies plus one `rlimit` attribute (all erase in a
normal build), so exec behaviour is unchanged by construction. `cargo build -p kcore`
compiles clean.

## Decision

**KEEP** the `remove_waiter` dequeue-lemma application, the rank-20 enqueue twin in
`wait`, the rank-21 dom-complete collapse, and the rank-23 `rlimit(40)→(25)` on
`remove_waiter`. **DROP** the `signal` application (inline map retained). The kept
optimization asymmetry is satisfied — `remove_waiter` fell 13 205 → 9 994 ms with its
rlimit **−51.6 %**, `wait` fell 463 → 415 ms, the crate total fell 93 386 → 87 807 ms
(−6.0 %), and the new lemmas add only 71 ms — and the simplification axis is a clear
win (named lemmas in the `lemma_census_*` family, ~50 fewer inline lines on the hot
op). Gate 396/0.

> verified **Y** (394 → **396**, +2 lemmas) · `remove_waiter` **13 205 ms / rlimit
> 39 285 761 → 9 994 ms / rlimit 19 005 838** (−24.3 % / −51.6 %) · `wait` 463 → 415 ms
> · `signal` **reverted** (extraction regressed it +63 % rlimit) · new lemmas **71 ms**
> · kcore SMT **93 386 → 87 807 ms** (−6.0 %) · `remove_waiter` `rlimit(40)→(25)` ·
> clarity **cleaner** → **KEEP** (signal **DROP**)
