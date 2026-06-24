# C2 — kcore local clarity lemmas (evaluation)

Task **C2** (ranks 26/27/28/22, Wave C) from `doc/plans/0_verus-optimization.md`:
four kcore proof simplifications, each naming a repeated proof block behind a
small `proof fn` or scoping it with `assert … by { }`. The plan rates C2a/C2b/C2d
**simplification** (clarity, kept iff the diff reads cleaner and the crate SMT
total does not materially regress, <5 %) and C2c an **optimization** (kept only on
a measured per-fn drop). This file records the per-attempt evaluation under the
plan's §2 protocol. Temporary intermediate report (per `CLAUDE.md`, not citable
from code/specs/guidelines).

- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each);
  `cargo verus verify -p kcore` for the gate, `scripts/verus-baseline.sh kcore`
  (`--time-expanded --output-json`) for timing. Per-fn wall ms wobbles ±5–15 %, so
  the deterministic **rlimit** field is the run-independent claim (§2). The
  untouched `cspace::cdt_unlink` is the control: its rlimit is **byte-identical
  (7 156 805)** across every run below, so the reported deltas are real, not noise.
- **Baseline.** C2 branches off `origin/main` (`d7eab16`, post A1–A5/B1–B6 with the
  B5b/B6 experiments reverted). Fresh cold pre-C2 baseline on this host: **402
  verified, 0 errors, kcore SMT 61 762 ms**. Per-fn before: `recv` 745 ms /
  rlimit 1 727 721, `send` 388 / 1 091 622, `destroy_tcb` 13 417 / 34 510 921,
  `cdt_insert_child` 1 082 / 3 216 582, `remove_waiter` 9 333 / 19 005 838,
  `signal` 11 344 / 20 909 644.

## Outcome summary

| sub-task | kind | target fn before → after | rlimit before → after | decision |
|---|---|---|---|---|
| **C2a** ring-fifo frame | simp | `recv` 745 → **531** ms; `send` 388 → **373** | `recv` 1 727 721 → **1 220 848** (−29.3 %) | **KEEP** |
| **C2b** running-frame trans | simp | `destroy_tcb` 13 417 → **10 494** ms | 34 510 921 → **24 609 374** (−28.7 %) | **KEEP** |
| **C2c** remove_waiter tail wrap | opt | `remove_waiter` 9 219 → **14 205** ms | 19 005 838 → **28 614 661** (+50.6 %) | **DROP** |
| **C2c** signal tail wrap | opt | — (terminal block) | — | **SKIP** |
| **C2d** cdt_insert_child asserts | simp | `cdt_insert_child` 1 082 → **5 226** ms | 3 216 582 → **24 434 013** (+659 %) | **DROP** |

**Kept (C2a + C2b): gate 404/0, kcore SMT 61 762 → 58 830 ms (−4.7 %).** The two
new lemmas cost **11 ms combined**. C2c/C2d were reverted; `cdt_insert_child`
(1 089 / 3 216 582) and `remove_waiter` (9 219 / 19 005 838) return to baseline,
confirming faithful reverts.

## C2a — shared ring-fifo "other ring untouched" frame lemma (rank 26) · KEEP

`channel.rs`. `send` and `recv` each carried a byte-identical ~14-line block
proving the *other* ring (`1 - rr`) is unchanged. Extracted into a private
`proof fn lemma_ring_fifo_frame(cv0, sv0, cvf, svf, ring)` modelled on the sibling
`lemma_send_fifo_push`: it proves the `=~=` extensional ring-fifo equality
internally, keyed on the **`ring_msg` congruence** (`forall|idx| 0 <= idx <
cv0.depth ==> ring_msg(cvf, svf, ring, idx) == ring_msg(cv0, sv0, ring, idx)`) — a
predicate-application trigger that composes, not a raw `ring_cap`-index frame (§10,
`verus.md`:1143–1147). Each call site is now a thin `assert forall|idx| 0 <= idx <
depth … by { lemma_ring_msg_eq(…) }` (per-index `lemma_ring_msg_eq` discharged from
the ambient frame for valid ring positions) + one lemma call.

The **bound `0 <= idx < cv0.depth`** is load-bearing: a first unbounded attempt
errored (`lemma_ring_msg_eq` precondition not satisfied — the ring frame only covers
valid positions `[0, depth)`, matching `lemma_send_fifo_push`'s `0 <= i < dd`
requires). Bounding it fixed the gate.

*Measured:* `recv` **745 → 531 ms / rlimit 1 727 721 → 1 220 848 (−29.3 %)**,
`send` **388 → 373 ms / rlimit 1 091 622 → 1 038 980**; new `lemma_ring_fifo_frame`
4 ms / rlimit 21 288. A clarity dedup (two identical blocks → one named lemma)
with a free speed bonus on both ops. **KEEP.**

## C2b — composite `running`-frame transitivity lemma (rank 27) · KEEP

`thread.rs`. `destroy_tcb` and its B1 per-phase frame lemmas repeated **seven**
identical 4-lemma clusters composing the four running cross-object frames
(`unhomed_frozen_free`, `home_views_frozen`, `emptied_via_dead_home_free`,
`refs_death_persist`) over `(a,b)` + `(b,c)`. Added one private
`proof fn lemma_running_frame_trans<S: Store>(a, b, c)` (requires = the union of the
four `cspace::lemma_*_trans` preconditions; ensures = the four `(a,c)` frames; body
= the four trans calls). Each of the seven sites now makes a single call. In the
three phase lemmas the `(b,c)`-edge exporters (`*_from_slot_eq`/`*_from_refs_eq`)
are consolidated above the call. Kept scoped to `destroy_tcb` — cspace's trans
lemmas are not co-grouped elsewhere (the plan's cross-unit claim is false).

*Measured:* `destroy_tcb` **13 417 → 10 494 ms / rlimit 34 510 921 → 24 609 374
(−28.7 %)**; new `lemma_running_frame_trans` 7 ms / rlimit 12 789. The rlimit drop
is run-independent: folding four lemma applications per phase into one tightly-keyed
call shrinks `destroy_tcb`'s context. Clarity dedup **and** a measurable speedup on
the gate's #2 obligation. **KEEP.**

## C2c — hide signal/remove_waiter dead-frozen tail (rank 22, opt) · DROP / SKIP

`notification.rs`. An optimization, conditioned in the plan on **"skip if B2
lands."** B2 (`cspace::lemma_waiter_dequeue_census`, PR #190) **has landed** and
already extracted `remove_waiter`'s heavy census query (its rlimit fell to
19 005 838 there).

- **`signal`** tail is the function's **terminal** block, so scoping its two
  `ObjId` foralls yields ≈0 — **SKIP** (never applied).
- **`remove_waiter`** — the one place a wrap could help (it precedes the ready-frame
  tail). Attempted: wrapped the `dead_tcb_frozen`/`refs_death_persist` establishment
  in `assert(dead_tcb_frozen(old, store) && refs_death_persist(old, store)) by { … }`.
  *Measured:* `remove_waiter` **9 219 → 14 205 ms / rlimit 19 005 838 → 28 614 661
  (+50.6 %)**, crate total 58 830 → 65 123 (+10.7 %). A clear **regression** — proving
  the two quantified predicates as an explicit `assert-by` goal and then re-consuming
  them costs more than letting the lemma outputs flow directly into the tail (the same
  failure mode as C2d). Per §2's optimization asymmetry, **DROP** (inline restored;
  control rlimit byte-identical, confirming a faithful revert).

## C2d — cdt_insert_child acyclic asserts → scoped assert-by (rank 28) · DROP

`cspace.rs`. The plan rated this "~0 speed, pure hygiene" (terminal block). Wrapping
the two acyclicity-preservation halves in `assert(acyclic(m1)) by { … }` /
`assert(sib_acyclic(m1)) by { … }` instead **regressed badly**: `cdt_insert_child`
**1 082 → 5 226 ms / rlimit 3 216 582 → 24 434 013 (+659 %)**. `acyclic`/`sib_acyclic`
are `exists`-quantified predicates; asserting them as an explicit `by {}` goal forces
the solver to re-handle the existential witnesses the lemmas already produced, far more
expensively than the inline calls. The §2 simplification asymmetry reverts any real
regression — **DROP** (inline restored; `cdt_insert_child` returns to 1 089 / 3 216 582,
a faithful revert).

## Gate (§2 step 2a — cold, authoritative, whole-crate)

On the kept state (C2a + C2b; C2c/C2d reverted), `cargo clean -p kcore && cargo verus
verify -p kcore` ended

```
verification results:: 404 verified, 0 errors
```

**present** (a real cold run). `N` rose **402 → 404**, **+2**, exactly the two new
`proof fn`s (`lemma_ring_fifo_frame`, `lemma_running_frame_trans`) — the predicted
delta. **Gate: PASS (Y).** The trusted-base ledger kcore row is updated 402 → 404;
the lemmas are ordinary verified proofs (no new seam), so the
`external_body`/`assume_specification` tally stays **14**.

## Host tests / build

kcore has no host unit suite — its verification *is* its test, and every C2 edit
lives in `proof fn`/`proof {}` bodies (erased from a normal build), so exec
behaviour is unchanged by construction. `cargo build -p kcore` compiles clean.

## Clarity (§2 step 4)

**Cleaner** on the two kept sites. C2a turns two identical multi-line ring-frame
blocks into one named `lemma_ring_fifo_frame` + two thin call sites. C2b turns seven
4-lemma trans clusters (28 lemma calls) into seven single `lemma_running_frame_trans`
citations + one ~25-line lemma, and the cross-object composition reads as one named
step. C2c/C2d were dropped on measurement, so no clarity claim attaches.

## Decision

**KEEP C2a + C2b. DROP C2c (remove_waiter) + C2d. SKIP C2c (signal).** Both kept
sub-tasks satisfy their §2 axis with margin: clear readability wins **and** a
measurable speedup (the crate SMT total *fell* 61 762 → 58 830 ms, −4.7 %, driven by
run-independent rlimit drops of ~29 % on `destroy_tcb` and `recv`). The two dropped
sub-tasks each regressed their target's rlimit by 50–660 % — the recurring lesson
that wrapping the establishment of a **quantified/existential** predicate in an
explicit `assert-by` goal is costly, not free. Gate 404/0.

> verified **Y** (402 → **404**, +2 lemmas) · C2a `recv` **745 → 531 ms / rlimit
> −29.3 %**, `send` 388 → 373 · C2b `destroy_tcb` **13 417 → 10 494 ms / rlimit
> −28.7 %** · C2c `remove_waiter` **+50.6 % rlimit → DROP**, signal **SKIP** · C2d
> `cdt_insert_child` **+659 % rlimit → DROP** · kcore SMT **61 762 → 58 830 ms
> (−4.7 %)** · control rlimit byte-identical · clarity **cleaner** → **KEEP C2a+C2b**
