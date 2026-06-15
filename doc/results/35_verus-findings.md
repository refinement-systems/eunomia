# Verus findings 15 — Phase 4e: timer (`arm`/`disarm`/`check_expired`/`destroy_timer`) + `destroy_tcb` + phase-4 closeout

Plan: `doc/plans/3_verus-rewrite.md` (§4.4 + §7 step 4) and its decomposition
`doc/plans/3_verus-rewrite_phase4-detail.md` (§4e). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31` (4a — the notification/TCB/timer ghost-view refactor), `32` (4b — `signal`/`wait`/
`destroy_notif`), `33` (4c — `remove_waiter`), `34` (4d — `report_terminal`/`bind`). This
is the **fifth and final** sub-phase of phase 4: the timer object — the §4.1
`refcount_sound` armed-timer term — plus the declared scope-out (`thread::destroy_tcb`)
and the phase-4 documentation closeout. With it every object destructor except aspace's
is ported, and the cross-object teardown + the full `refcount_sound` census pass forward
to a dedicated phase after phase 5 (detail §1.4).

**Outcome.** `cargo verus verify -p kcore`: **120 verified, 0 errors** (was 105 after 4d;
`+15` = the armed-list model's seven lemmas, the four proven timer ops `arm`/`disarm`/
`destroy_timer`/`check_expired`, and `check_expired`'s two re-establishment lemmas). The
new `timer_chain`/`timer_complete`/`timer_wf`/`timer_seq`/`timer_signal_ok`/
`timer_notif_injective` spec fns and `thread::destroy_tcb` (`external_body`) carry no body
proof, so they do not bump the count. `cargo test -p kcore`: **43 passed** (was 38 —
`+timer_wf_exec_has_teeth`, `+arm_disarm_lifecycle`, `+check_expired_wake_and_skip`,
`+destroy_timer_disarms`, `+destroy_tcb_structural`). The aarch64 `kernel` cross-build is
unchanged (ghost erasure; confirmed `cd kernel && cargo build`).

**`check_expired` is fully proven — the `external_body` fallback was not needed.** The
detail-plan §4e flagged the multi-fire census tension as the chief risk; it resolved
cleanly against a **distinct-notification** precondition (see §2.3).

---

## 1. What closed

- **The armed-timer list model** (`cspace.rs`), the head-only analog of the notification
  waiter model (4a/4b): `timer_chain` (a duplicate-free `Seq` threaded through `next` from
  the `timer_head_view` scalar, every node armed with a bound notification),
  `timer_complete` (**armed ⇒ on the chain** — what makes `disarm`'s walk sound),
  `timer_wf := ∃ts. timer_chain ∧ timer_complete`, and `timer_seq` (the unique chain).
  Lemmas: `lemma_timer_chain_unique` (+ `lemma_tchain_eq_at`/`lemma_tchain_not_strict_prefix`),
  `lemma_timer_remove_chain` (the `disarm` splice — `lemma_remove_chain` minus the tail
  fixup), `lemma_timer_push_head_chain` (+ `lemma_push_head_nodup`, the `arm` prepend), and
  `lemma_seq_remove_keeps`.
- **`disarm` is proven** — the `remove_waiter` analog over the global armed list: a
  read-only walk that splices `t` out (head or predecessor re-pointed past it), releases
  the queued notification ref (`refs[notif] -= 1`, the **armed-timer term** of
  `refcount_sound`), and clears `t`. `timer_wf` preserved; `!armed` ⇒ a no-op.
- **`arm` is proven** — `disarm` first, `+1` on the notification ref, prepend onto the
  list head (`timer_seq` gains `t` at the front). Ordering the ref delta as `disarm`'s
  `-1` then `arm`'s `+1` makes a same-notification re-arm **provably net-zero** (the
  `bind_refs_post` precedent, doc 30 §2.2). `timer_wf` preserved.
- **`check_expired` is proven** (full body — no `external_body` fallback): the walk that
  `disarm`s + `signal`s every expired timer, preserving `timer_wf` and framing
  `slot_view`/`chan_view`. The census tension is resolved by the distinct-notification
  precondition (§2.3).
- **`destroy_timer` is proven** — `disarm` of the object (refs == 0).
- **`thread::destroy_tcb` carries an assumed `external_body` contract**, host-test-checked
  (`check_destroy_tcb`): the robustly-true structural core — `t` ends `Halted` with its
  queue link and both binding slots cleared, **its report UNCHANGED** (destruction fires
  no report, §5.1), `cspace_wf` preserved. The declared scope-out (§2.4).

All host-checked against the real `ArrayStore` bodies (`test_store.rs`) via `timer_wf_exec`
(+ its `_has_teeth` rejecter) and the five new tests.

## 2. Design decisions / mechanics worth keeping

### 2.1 The armed list is head-only, so its model is lighter than the waiter queue

The waiter queue is per-notification with a `wait_tail`; the armed list is one global list
headed by the `timer_head_view` scalar with **no tail pointer**. So `timer_chain` drops the
tail clause, `lemma_timer_remove_chain` drops `lemma_remove_chain`'s tail-fixup branch, and
`arm` is a head-**prepend** (`[t] ++ ts`, simpler than `wait`'s tail-push). `signal` already
frames `timer_view`/`timer_head_view` (the 4d strengthening, doc 34 §2.2), so `timer_wf`
survives a `signal` **for free** inside `check_expired` — no timer-frame lemma needed.

### 2.2 `timer_complete` (armed ⇒ on the chain) is the load-bearing addition

Unlike `remove_waiter` (which tolerates an absent waiter — `waiter_seq.contains(t)` splits
its contract), `disarm` leans on completeness: an armed `t` is **guaranteed** on the list,
so the walk finds it and the no-op fall-through (`!ts0.contains(t)` after the walk) is
`assert(false)` — the `remove_waiter` read-only-walk shape (doc 33 §2), but with the found
path always taken. `timer_wf` carries `timer_complete`; `arm`/`disarm` re-establish it
(`lemma_seq_remove_keeps`: a still-armed timer other than the removed one survives the
splice).

### 2.3 The multi-fire census tension resolves against distinct notifications

`check_expired` fires `signal` per expired timer; `signal`'s `wait_head is Some ⇒ refs > 0`
must hold at each fire, but `disarm` releases the timer's own ref *before* the `signal`, and
a wake's `-1` would threaten a *later* fire on the same notification — the census problem
phase 4 defers (`binding_refs_ok`, doc 32 §2). The resolution (the detail-plan §4e
"attempt full, fall back" — the full proof landed):

- a precondition-only `timer_signal_ok` (the armed-timer census fragment, supplied by the
  trusted shell): each armed timer's notification is live + `notif_wf`, holds the timer's
  ref (`refs ≥ 1`), and — when a waiter is queued — the waiter's too (`refs ≥ 2`), so after
  `disarm`'s `-1` the `signal` still sees `refs ≥ 1`;
- a `timer_notif_injective` precondition: **armed timers bind pairwise-distinct
  notifications**. This is what makes the sweep non-interfering — a `disarm(c)`+`signal(n)`
  touches only `n`'s refs/queue, so every *other* armed timer's `timer_signal_ok` survives
  (its notification `≠ n`, framed; `notif_wf` carried across the `signal` by
  `lemma_notif_wf_frame`). Both are carried as **loop invariants** over the post-fire state
  (`lemma_inj_after_disarm`, `lemma_signal_ok_after_fire`).

Realistic at MVP scale (one timer per notification — the M1 witness, `scripts/m1-test.sh`).
The general shared-notification case needs the full census and rides forward to the
post-phase-5 teardown phase (plan §1.4) — recorded, not silently dropped.

### 2.4 `check_expired`'s walk-while-mutate: read `next` before `disarm`, track the entry snapshot

The body saves `next = timer_next(c)` **before** `disarm(c)`, so the cursor continues from a
node still on the (mutated) list. The loop tracks the entry snapshot `ts0 = timer_seq` with
a `ghost mut` position `k` (the doc-33 read-only-walk counter, here over a *mutating* walk):
`cur == ts0[k]`, and the **unprocessed suffix `ts0[k+1..]` is provably intact** — a prior
`disarm` touches only earlier nodes' links (`disarm`'s four-field frame, §2.5), and `signal`
frames all timer views. `timer_wf` (preserved by `disarm`/`signal`) gives the postcondition.

### 2.5 `disarm` exposes a four-field frame so `check_expired` can prove the suffix intact

`disarm`'s first frame — *fully* unchanged for every timer other than `t` and its
predecessor — gives the suffix's `next`-threading. But the **predecessor**'s `next` does
move, and a still-armed predecessor's `timer_signal_ok` needs its `notif` preserved. So
`disarm` exposes a second frame: **every timer other than `t` keeps its `armed`/`notif`/
`deadline`/`bits`** (only the predecessor's `next` moves). The two frames together carry
both the suffix-intact invariant and the global `timer_signal_ok` across the splice.

### 2.6 `Seq::no_duplicates`'s n² trigger is the rlimit trap — extract the proof

`lemma_timer_push_head_chain` blew the solver rlimit: `no_duplicates`'s body
`forall i,j. self[i] != self[j]` (an n² trigger) fired over **424k** instantiations once it
shared a query with the threading index terms. Extracting the no-duplicates step into
`lemma_push_head_nodup` (so the only `Seq`-index terms in its query are `pts`/`ts0`'s)
brought it under budget — the doc-25 §2 "decomposition beats an rlimit bump" discipline, no
`#[verifier::rlimit]`. The frame the push lemma needs is a single `Map::insert` equality
(`tmvf == tmv1.insert(t, tmvf[t])`), not a broad `forall|j| j != t ⇒ …`, for the same
trigger-economy reason.

### 2.7 `destroy_tcb` external_body; `unqueue_ready` needs no contract

`destroy_tcb` recurses through the still-`external_body` `cspace::delete` and the plain-Rust
`unref_cspace`/`unref_aspace` (the cross-object teardown, deferred), so it stays
`external_body` with a host-checked structural contract (the `destroy_channel` discipline,
doc 30 §1). Because its body is unverified, **`Store::unqueue_ready` needs no Verus
contract** — a small simplification of the detail-plan §1.3 note (which budgeted one in case
`destroy_tcb` were verified). Its contract omits refs (the cross-object recursion, like
`destroy_channel`); the displaced-cap refs ride `check_destroy_tcb`.

## 3. Doc-numbering note

Phase 4 produces docs 31–35 ("findings 11–15"), numbered in landing order (the doc-29/30
convention). This is 4e, the fifth and last to land, so doc 35 / findings 15.

## 4. What's next

Phase 4 is complete: notification `signal`/`wait`/`remove_waiter`/`destroy_notif`, thread
`report_terminal`/`bind`, and timer `arm`/`disarm`/`check_expired`/`destroy_timer` are
proven; `thread::destroy_tcb` joins `delete`/`channel::destroy_channel` as the host-checked
`external_body` residue. Phase 5 is aspace + PTE + sysabi. The **recommended dedicated
cross-object-teardown phase** (closing `delete`/`obj_unref`/`destroy_cspace`/`destroy_tcb`/
`destroy_channel`'s bodies, the seL4-zombie recursion measure, and the full `refcount_sound`
census — of which phase 4 landed the waiter and armed-timer terms) should follow phase 5,
since it can only be attempted once aspace is ported and needs all the destructors and the
census together (detail §1.4).
