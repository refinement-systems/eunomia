# Verus findings 33 — Phase 6d-final-thread-body: the `dead_tcb_frozen` cross-object frame (the load-bearing piece doc 52 §3 named), threaded through the whole teardown call graph; the `destroy_tcb` body assembly deferred on two census-after-clear lemmas

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6d", the *6d-final-thread-body* residue named in doc 52 §3). Prior increment: `52`
(6d-final-thread — the two new system invariants + frame lemmas + strengthened `remove_waiter`,
with the `destroy_tcb` body recorded as the residue, blocked on a **`tcb_view` teardown frame**).
This increment **builds and verifies that frame** — the `dead_tcb_frozen` "dead, queue-detached
TCB" frame doc 52 §3 designed — and threads it through every teardown op on the cross-object
call graph, plus lands the two remaining body-foundation lemmas. **`destroy_tcb` keeps its
`external_body` contract** (now carrying the `refs[t] == 0` precondition + the except-`t` frame the
recursion needs); its body assembly — drafted here — is deferred on two small census-after-clear
lemmas (§3), the standing "attempt full, fall back, record" discipline (the doc 50→51→52 cadence).

`cargo verus verify -p kcore`: **295 verified, 0 errors** (was 289 after doc 52's baseline).
`cargo test -p kcore`: **82 passed** (unchanged; `destroy_tcb_structural` gains the `refs[t] == 0`
precondition setup). The aarch64 `kernel` cross-build is unchanged (every change is ghost or a
contract/comment; confirmed `cd kernel && cargo build`).

---

## 1. What landed

The doc 52 §3 blocker was that the teardown recursion (`unref_cspace → destroy_cspace → delete →
…`) did not frame `tcb[t]`, so `destroy_tcb` could not prove its halted subject's `report`/`state`/
`qnext` survive the recursive `unref_cspace`. doc 52 §3's solved design — a self-composing
"dead, queue-detached TCB is frozen" frame — is now built and verified.

- **The `dead_tcb_frozen` system frame + its per-object kernel `dead_tcb_frozen_at`** (`cspace.rs`).
  `dead_tcb_frozen_at(s0, s1, x) := (x is a TCB, `refs[x] == 0`, `wait_notif is None` in s0) ⟹
  (x stays dead-in-domain and `tcb[x]` is unchanged in s1)`; `dead_tcb_frozen(s0, s1) := ∀x.
  dead_tcb_frozen_at(s0, s1, x)`. **Keyed on `wait_notif is None`** — the exact frame `signal`
  exposes (`forall k. wait_notif != Some(n) ⟹ tcb[k] unchanged`), so a dead, queue-detached `x` is
  provably untouched by any teardown fire/splice. **Self-composing** (the antecedent is preserved),
  so it threads through the cross-module recursion and `destroy_cspace`/`destroy_channel`'s loops
  with no external refs-monotonicity lemma. Helper lemmas: `lemma_dead_tcb_frozen_trans` (the
  composition), `_signal_shaped` (derive it from a signal-shaped edit), `_dec_ref` (the `dec_ref`
  step), `_refl` (a framed no-op).

- **Threaded through the whole teardown call graph** (the new `dead_tcb_frozen` ensures + its
  discharge): `delete`, `obj_unref`, `destroy_cspace`, `unref_cspace` (`cspace.rs`); `destroy_channel`
  (5 nested loops), `release_binding`, `endpoint_cap_dropped`, `fire` (`channel.rs`); `signal`,
  `remove_waiter` (`notification.rs`); `destroy_timer` (`timer.rs`). The fire/splice ops carry it
  via `_signal_shaped` (their woken/spliced waiter is `wait_notif`-bearing, never a detached `x`);
  the recursive ops compose via `_trans`; the total-frame ops (`destroy_notif`, `unref_aspace`,
  `delete_prepare`, `dec_ref`) via `_refl`/`_dec_ref`.

- **`destroy_tcb`'s contract strengthened** (`thread.rs`, still `external_body`, host-checked): the
  `refs[t] == 0` precondition (so the frame applies to `t` itself — supplied by `obj_unref`'s
  `if obj_refs(o) == 0` call site) and the **except-`t`** ensures `forall x. x != t ⟹
  dead_tcb_frozen_at(old, final, x)` (the subject is excepted — the body rewrites `tcb[t]`).
  `obj_unref`'s Thread arm composes this with `dec_ref` to carry the base `dead_tcb_frozen` up the
  recursion.

- **The two body-foundation lemmas, proven ahead of the body** (the doc-52 "land the frame lemmas
  before the body" pattern):
  - `lemma_caps_consistent_frame_thread_halt_clear` — halting + hold-clearing the single dead TCB
    `t` preserves `caps_consistent`. `t` is designated by no live cap (`refs[t] == 0`), so the only
    clauses reading `t`'s fields (a `Thread(t)` cap's `cspace_resident_wf`/waiter-coherence) are
    never instantiated; unlike the doc-52 `_dequeued` lemma it allows `t`'s `cspace`/`aspace` to
    change (the clear-before-unref step), traded for the no-`Thread(t)`-cap fact.
  - `lemma_thread_off_all_chains` — a thread that is not a blocked waiter of any live chain lies on
    no waiter chain (a `waiter_chain` node is `BlockedNotif` and names its notification, so `t` can
    be a node only of its own `wait_notif`'s chain). The post-detach fact the halt step needs.

---

## 2. Findings worth keeping

- **Map-index triggers are fragile across a composition; a per-object predicate is the fix.** The
  first `dead_tcb_frozen` quantified `forall|x| … s1.tcb_view()[x] …` with `#![trigger
  s1.tcb_view()[x]]`. Its single-op uses (`signal`) verified, but `lemma_dead_tcb_frozen_trans`
  could **not** chain two instances: the individual `assert`s passed yet the composed postcondition
  failed (the map-index trigger did not connect the two frames). Refactoring to a clean predicate
  `dead_tcb_frozen_at(s0, s1, x)` and quantifying `forall|x| #[trigger] dead_tcb_frozen_at(s0, s1, x)`
  fixed it immediately — predicate-application triggers compose where `Map::index` triggers do not.
  **Lesson: when a frame must compose (trans/loop), quantify over a named predicate, not a raw map
  index.**

- **Keying the frame on `wait_notif is None` (not a broader "off-chain") is what makes it provable.**
  A teardown fire (`endpoint_cap_dropped → fire → signal`) wakes a *blocked* waiter, so a frame over
  "all dead TCBs" is **false** (a dead blocked waiter's `tcb` changes). `signal` exposes exactly
  `wait_notif != Some(n) ⟹ unchanged`; restricting the frame's antecedent to `wait_notif is None`
  makes every dead, detached TCB land on the unchanged side of that frame — and `destroy_tcb`'s
  subject qualifies once halted with `wait_notif` cleared.

- **The `tcb_view().dom()` guard avoids map-junk.** Without it the frame claimed `tcb[x]` frozen for
  non-thread dead `x` (∉ `tcb_view.dom()`), where map-default "junk" values differ across states and
  the composition is unprovable. Guarding the antecedent with `tcb_view().dom().contains(x)` (+ dom
  preservation, threaded via `=~=` at each signal-shaped call) restricts the frame to real TCBs.

- **The `dead_tcb_frozen` invariant must enter *every* nested loop level.** `destroy_channel`'s
  3-deep ring loop + 2-deep binding loop need the invariant in all five (a `replace_all` keyed on
  the shared `chan_struct_frame` line only matched the two **outermost** loops — the inner loops are
  more-indented; the trans precondition then fails one level down). The inner loops carry it too,
  composed past each `delete`/`release_binding` by `_trans`.

---

## 3. The residue: `destroy_tcb`'s body (6d-final-thread-body-2) — logically complete, deferred on a `remove_waiter` field-frame + an rlimit decomposition

`destroy_tcb`'s body — detach (`unqueue_ready`/`remove_waiter`) → halt (clear `qnext`/`wait_notif`/
`state`, leaving `report`) → bind-slot `delete`s → **clear-before-unref** `cspace`/`aspace` releases
— is **drafted** (the structure consuming the landed lemmas is written out) but **not proven** here.
With the `dead_tcb_frozen` frame now in hand, the subject `t`'s `report`/`state`/`qnext` survive the
recursion (it is dead + detached once halted); `caps_consistent`/`refcount_sound` ride the halt via
`lemma_caps_consistent_frame_thread_halt_clear` + `lemma_waiter_refs_frame_dequeued` +
`lemma_thread_hold_frame` (the census is unchanged — `t` is off every chain, its `cspace`/`aspace`
fixed); and `t`'s off-all-chains fact rides `lemma_thread_off_all_chains` per detach branch. The
**two remaining pieces**, both small and mechanical:

- **`lemma_census_after_hold_clear` (`cspace`) and its `aspace` twin.** Clearing `tcb[t].cspace`
  (`Some(cs) → None`, the other fields fixed) drops the census by exactly one at `cs` and nowhere
  else — `lemma_thread_hold_cspace_drop` gives the `thread_hold_refs(cs)` `-1`, and the other five
  terms + `thread_hold_refs(o != cs)` are framed (a set-equality over the `cspace`/`aspace` filters
  when only `tcb[t].cspace` moves). The result is exactly `census_off_by_one(s1, cs)` — the off-by-one
  window `unref_cspace` consumes. (The `aspace` twin is identical with `lemma_thread_hold_aspace_drop`.)
- **The inline except-`t` frame** — compose `dead_tcb_frozen_at(·, ·, x)` for `x != t` across the
  body's captured ghost states (detach total-frame/base, the halt's single-`t` edit, the bind
  `delete`s' base frame, the clear-before-unref releases' base frame); a six-segment `_trans`-style
  chain that closes the subject `t`'s `state == Halted`/`qnext is None`/`report` postconditions and
  `obj_unref`'s except-`t` `dead_tcb_frozen_at` ensures simultaneously.

**Attempt outcome — logically complete, blocked on a frame-exposure + an rlimit decomposition.** A
full body proof was written and reaches **`297 verified, 1 error`**: the two census lemmas (above),
the off-all-chains/halt_clear foundations, the per-segment except-`t` chain, and the SCC `decreases`
(un-`external_body`-ing `destroy_tcb` opens the cycle `destroy_tcb → unref_cspace → destroy_cspace →
delete → obj_unref → destroy_tcb`, so `destroy_tcb` takes height 3 and `unref_cspace` height 2) **all
check**. The single remaining logical gap is mechanical: `remove_waiter` does not *expose* that it
preserves `t`'s non-queue TCB fields (`cspace`/`aspace`/`state`/`report`/`bind_slots`) — its body
already proves them (the `set_tcb_{qnext,wait_notif}` struct-updates), but they are not `ensures`, so
`destroy_tcb` cannot prove `cspace_resident_wf(cs)` (it needs `old.tcb[t].cspace == Some(cs)`, i.e.
the cspace survived the detach). Adding that frame to `remove_waiter` (the only caller is
`destroy_tcb`) closes it. The second blocker is **rlimit**: the body is one large query that exceeds
the limit even spun off; closing it for CI cross-platform robustness wants the except-`t` chain and
the clear-section frames extracted into `proof fn`s (the doc-25 §2 decomposition discipline) rather
than an rlimit bump. Both are recorded here as **6d-final-thread-body-2**; this increment lands the
load-bearing frame + every foundation the body consumes, so that step is now purely assembly.

`destroy_tcb` keeps its assumed-but-host-checked `external_body` contract (now carrying the
`refs[t] == 0` precondition, host-checked by `destroy_tcb_structural` with `refs[200] = 0`). The §6
spec-table goal "kcore carries zero `external_body`" is therefore **not yet met** — `destroy_tcb`
remains (plus the pre-existing `untyped.rs` helpers, out of 6d scope). This increment removes the
*load-bearing* obstacle the prior six increments could not (the cross-object `tcb_view` frame);
closing the body on the `remove_waiter` field-frame + the rlimit decomposition is
**6d-final-thread-body-2**, the last step of phase 6d.

---

## 4. Doc / CLAUDE.md

No `CLAUDE.md`/spec edit this increment (the doc-30 §3 convention — the sub-phase closeout edit rides
6f; and `destroy_tcb` is still `external_body`, so the trusted-residue note remains accurate). 6d's
foundations are now doc 53; the `destroy_tcb` body is the recorded 6d-final-thread-body-2 residue.
`cargo verus verify -p kcore` runs with no per-proof filter, so the `dead_tcb_frozen`/
`dead_tcb_frozen_at` system frame, its four composition lemmas, the cluster-wide `dead_tcb_frozen`
ensures (across `cspace`/`channel`/`notification`/`timer`), `destroy_tcb`'s strengthened contract,
and the two body-foundation lemmas all auto-gate; `host-tests` runs `destroy_tcb_structural` with the
new `refs[t] == 0` setup.
