# B8C — Ready-queue verification: findings (part 5)

Working notes from the fifth implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass **re-applied the integration patch**
(`doc/results/b8c-2-integration-wip.patch`) onto the A+B foundation, **fixed two real soundness
gaps the patch authors missed** (they had only module-verified `ready`+`notification`, never the
cascade modules), **reformulated `signal`'s caller-facing frame** into a form that actually carries
through the cascade, and **threaded the ready pair through the first layer of carriers**
(`remove_waiter`, `fire`, `report_terminal`) — each verified. The teardown SCC, the IPC fast-path
loops, `check_expired`, and `destroy_tcb` remain; the integration stays **out of the tree** (it is
atomic — not gate-green until the whole cascade lands) and is re-saved as the updated patch.

Continues `doc/results/1_b8c-findings-4.md`. Branch `b8c-ready-queue`; draft PR #138.
Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

**The patch as saved was not internally sound** — it module-verified `ready` (19/0) and
`notification` (5/0) but **never `cspace`/`channel`/`thread`/`timer`**, so two breakages and a
caller-frame mismatch were latent. Re-applying it (`git apply`, clean) and running a full
`cargo verus verify -p kcore` surfaced **364 verified, 7 errors**. This pass drove the genuine
problems to ground and threaded the first carrier layer.

**Fixed (verified):**

1. **The `dead_tcb_frozen` weakening broke a *consumer*** — `lemma_dead_tcb_frozen_trans`
   (`cspace.rs`). Its case-split still matched the *old* antecedent (`refs 0 ∧ wait_notif None`),
   so the weakened `dead_tcb_frozen_at` (now also `state != Runnable`) no longer fired inside it.
   Adding `&& state != ThreadState::Runnable` to the `if` (and asserting `s1.state != Runnable`
   for the chain to `s2`) fixes it. This *also* resolved a spurious downstream error in `revoke`
   (a failed callee lemma poisons the caller's later obligations).

2. **`signal`'s weakened frame did not cover out-of-domain phantom keys.** The patch weakened
   `signal`'s caller frame to `wait_notif != Some(n) && state != Runnable ==> unchanged`. For a
   *phantom* key `k` (`!tcb_view().dom().contains(k)`, value arbitrary), `state` is unknown, so the
   frame says nothing — yet `lemma_notif_wf_frame`'s precondition (`forall k: wait_notif==Some(m)
   ==> unchanged`) ranges over *all* `k`, phantom included. The old (`!= Some(n)`-only) frame
   covered phantoms unconditionally; the `&& state != Runnable` carve-out silently dropped them,
   so **`fire` could no longer frame a non-fired notification `m`'s waiters**. Two-part fix (§2).

3. **The contrapositive frame still cannot preserve `report`** of a *Runnable* subject. A thread
   reported terminal (`report_terminal`) may itself be the Runnable old ready-tail the faithful
   enqueue re-threads; the frame then admits a change and cannot pin `report`. Fixed by exposing a
   **field-frame** on `signal` (§3).

4. **`signal`'s caller-facing frame was both too weak (phantoms) and too strong (loses
   `report`/state of the Runnable tail).** Resolving `fire` end-to-end forced a coherent
   reformulation of `signal`'s entire caller frame into **four** ensures (§2–§4) and a
   **tightening of `make_runnable`/`ready_enqueue`'s global frame** (the original `{state, qnext,
   ..old}` freed `state` for *every* thread, so the Runnable old ready-tail could adversarially
   appear `BlockedNotif` — no caps_consistent proof could survive it). Tightened to "non-woken
   threads change **only `qnext`**" (§5).

**Threaded + verified this pass:** `remove_waiter` (off-chain splice), `fire` (the central event
wrapper — the hardest of the three, it exercised the full caps_consistent + `notif_wf` frame
machinery), `report_terminal` (the patch's broken carrier, repaired), plus the reworked `signal`
(four caller-frame ensures + rlimit), the tightened `make_runnable`/`ready_enqueue` global frame,
`lemma_notif_wf_frame` (dom-guard), `lemma_caps_consistent_frame` (redundant clause dropped),
and `lemma_dead_tcb_frozen_trans` (weakened-antecedent fix). **`fire` is the proof that the
reformulated frame carries the whole cascade** — every downstream carrier reuses the same
unblocked `lemma_notif_wf_frame`/`lemma_caps_consistent_frame`.

**Remaining (the bulk, now mechanically unblocked):** `check_expired` (needs
`lemma_signal_ok_after_fire` weakened the same way as `lemma_notif_wf_frame`), `bind`,
`send`/`recv`/`endpoint_cap_dropped`/`destroy_channel`, the cspace teardown SCC
(`delete`/`obj_unref`/`destroy_cspace`/`unref_cspace`/`revoke`), `destroy_tcb` (Step F), the
ArrayStore host model, and the ledger. **Gate stays red mid-cascade** (atomic — `signal`
requiring the pair breaks every un-threaded caller), so the work is re-saved as the patch and the
tree is left at the green A+B baseline.

**Workflow hazard (recorded — cost real time):** `cargo verus verify` *caches per build*, and a
function/module `--verify-function`/`--verify-only-module` run **silently returns `EXIT=0` from
stale cache** (no `verification results::` line) after the crate was last built — a *false pass*.
Only `cargo clean -p kcore && cargo verus verify -p kcore` is authoritative. Treat a missing
`verification results::` line as "not actually re-verified", and clean-verify before trusting green.

---

## 1. The patch was never cascade-verified — re-applying it surfaced 7 errors

`git apply doc/results/b8c-2-integration-wip.patch` is clean, but the saved patch was only ever
checked with `--verify-module ready`/`notification`. A full `cargo verus verify -p kcore` after
applying gives **364 verified, 7 errors**, at:

| location | function | nature |
|---|---|---|
| `cspace.rs:4269` | `lemma_dead_tcb_frozen_trans` | consumer of the weakened `dead_tcb_frozen_at` (§0.1) |
| `cspace.rs:11065` | `revoke` | **spurious** — downstream of the broken `trans` lemma (resolved by §0.1) |
| `channel.rs:483` | `fire` | `signal` now requires the pair (expected — thread it) |
| `channel.rs:501` | `fire` | `lemma_notif_wf_frame` precondition under the new frame (§0.2) |
| `thread.rs:183,194` | `report_terminal` | the patch's own carrier, never verified (§0.3) |
| `thread.rs:323,324` | `bind` | downstream of `delete` (needs the pair first) |
| `timer.rs:792,803` | `check_expired` | `signal` precondition + the old-frame assertion (§4) |
| `thread.rs:546,548` | `destroy_tcb` | the Step-F detach (`unqueue_ready` seam + `lemma_…_refl`) |

The lesson (technique 24): **module-verifying the changed module is not enough for a contract
change — every transitive *consumer* of a weakened predicate or reshaped frame must be
re-checked.** A weakening that is sound at the definition can still break a consumer that
case-splits on the *old* shape (the `trans` lemma) or relies on the *old* frame's phantom-key
coverage (`fire`).

---

## 2. Discovery #4 — `signal`'s frame must be **contrapositive** (phantom-key safe), and
## `lemma_notif_wf_frame` **dom-guarded**

The faithful enqueue re-threads the Runnable old ready-tail `p`, so `signal`'s old caller frame
(`wait_notif != Some(n) ==> unchanged`) is false at `p`; the patch's fix added `&& state !=
Runnable`. But that guard is **unknown for phantom keys** (out-of-domain `k` whose `tcb_view()[k]`
is an arbitrary value), and `lemma_notif_wf_frame`'s precondition is an *un-guarded* `forall k`.
The old frame covered phantoms (no state mention); the patched frame does not.

**Fix, two parts, both verified:**

- **`signal`'s caller frame → contrapositive form** (`notification.rs`):
  ```rust
  forall|k: ObjId| #[trigger] final(store).tcb_view()[k] != old(store).tcb_view()[k]
      ==> old(store).tcb_view()[k].wait_notif == Some(n)
          || old(store).tcb_view()[k].state == ThreadState::Runnable,
  ```
  "a *changed* TCB was an `n`-waiter or was Runnable." This is **vacuous on phantom keys** (signal
  never perturbs them, so `final[k] == old[k]` and the antecedent is false), and `signal` already
  proves exactly this disjunction internally for its `dead_tcb_frozen_signal_shaped` step — so the
  reformulation re-verifies with no body change.

- **`lemma_notif_wf_frame` precondition → dom-guarded** (`cspace.rs`): add `&& tv.dom().contains(k)`.
  The lemma only ever reads `m`'s in-domain waiter chain, so this is a **pure weakening** (every
  existing caller, which supplied the un-guarded `forall`, still satisfies it). A caller then needs
  to frame only the *in-domain* waiters of `m`, which the contrapositive frame + `ready_complete`
  (Runnable ⇒ `wait_notif None`, so an `m`-waiter is non-Runnable, hence not in the changed set)
  discharge cleanly.

Technique 25: **a frame keyed on a per-key *property* (`state`, `wait_notif`) cannot constrain
phantom keys; state it contrapositively (keyed on the *change* itself, which phantoms don't
exhibit) and dom-guard the lemmas that consume it.**

---

## 3. Discovery #5 — expose a **field-frame** on `signal` for the non-scheduler fields

The contrapositive frame says *whether* a TCB changed, not *how*. `report_terminal` needs `t`'s
`report` preserved across the bound `signal`, but if `t` is the Runnable old ready-tail the
contrapositive frame admits a change. In reality `signal` writes only the wake/scheduler fields —
the fixups set `t`'s `qnext`/`wait_notif`/`retval`, and `make_runnable` sets `state`/`qnext` (on
`t` + the re-threaded tail). So **every other field is preserved**. Exposed as a `signal` ensures
(verified):

```rust
forall|k: ObjId| #[trigger] final(store).tcb_view()[k] == (cspace::TcbView {
    state: final(store).tcb_view()[k].state,
    qnext: final(store).tcb_view()[k].qnext,
    retval: final(store).tcb_view()[k].retval,
    wait_notif: final(store).tcb_view()[k].wait_notif,
    ..old(store).tcb_view()[k]
}),
```

This rides `make_runnable`'s own global frame (`cspace.rs:1147`, the seam's "all but state/qnext"
ensures) plus the fixups' setter frames. `report_terminal` reads `t.report` off it; `bind`,
`destroy_tcb`, and the delete-chain will read `cspace`/`aspace`/`bind_slots` the same way.
Technique 26: **when a contrapositive "what changed" frame is too weak for a caller that needs a
specific field preserved, add a positive field-frame keyed on the small written-field set — it is
the dual the caller actually consumes.**

---

## 3b. Discovery #6 — the enqueue's global frame was too loose for *any* caps_consistent proof

`fire` end-to-end exposed that **`make_runnable`/`ready_enqueue`'s global frame freed `state` for
*every* thread** (`final[x] == TcbView { state: final[x].state, qnext: …, ..old[x] }`). So the
contract permitted the Runnable old ready-tail `p` to *appear* `BlockedNotif` in the post-state —
and no `caps_consistent` / waiter-coherence proof can survive a thread that adversarially became a
stray blocked waiter. The body only ever writes `p`'s `qnext`, so the frame was needlessly weak.

**Fix (verified):** tighten the global frame on both the seam (`cspace.rs`) and the verified op
(`ready.rs`) to **"every thread *but* the woken `t` changes only its `qnext`"**:
```rust
forall|x: ObjId| #![trigger final(self).tcb_view()[x]]
    x != t ==> final(self).tcb_view()[x] == (TcbView { qnext: …, ..old(self).tcb_view()[x] }),
```
This pins `p`'s `state` (stays Runnable) and `wait_notif` (stays `None`). From it `signal` proves
a clean, directly-usable ensures — **"a thread that ends `BlockedNotif` was unchanged"** (the wake
produces only Runnable threads) — which is exactly what every caps/waiter-coherence frame needs:
the changed nodes are non-blocked, so they cannot be stray waiters. Technique 28: **an intrusive-
list op that re-threads a neighbour's link should frame that neighbour as "only the link field
changed", not "state+link free" — the looser frame defeats every invariant that reads `state`.**

## 3c. Discovery #7 — `lemma_caps_consistent_frame`'s "non-`n`-waiter ⇒ unchanged" clause

The same lemma that `lemma_notif_wf_frame` mirrors. Its precondition demanded *every* non-`n`-waiter
be fully unchanged — false for the Runnable `p`. But its body reads only `cspace`/`bind_slots` (kept
for all by `signal`'s field-frame), the waiter-coherence clause (discharged by Discovery #6's
"BlockedNotif ⇒ unchanged"), and — for *other* notifications' `notif_wf` — that **`m`-waiters
(`m != n`) are unchanged**. So the clause weakens to **"`wait_notif` Some, `≠ n`, in-domain ⇒
unchanged"** (drop the `wait_notif None` case that captured `p`; dom-guard out phantom keys).
A caller supplies it from `signal`'s contrapositive frame + `ready_complete` (an `m`-waiter is
`BlockedNotif`, non-Runnable, hence not in the changed set). Both the lemma body and `fire`'s call
re-verify. **This unblocks `caps_consistent` for the entire teardown SCC** (every member calls this
lemma across its fire), not just `fire`.

---

## 4. The carrier recipe (validated on `remove_waiter`/`fire`/`report_terminal`)

For each cascade function `F` reaching `signal`/`make_runnable`/`unqueue_ready`:

1. Add `cspace::ready_wf(old…)` + `cspace::ready_complete(old…)` to **requires**, and the
   `final…` pair to **ensures**. (cspace-module functions drop the `cspace::` prefix.)
2. **Loops:** pin `store.ready_view() == old(store).ready_view()` *and* the entry-state pair
   (`ready_wf(old)`/`ready_complete(old)`) in the loop invariant — the function `requires` are not
   visible inside the loop (part-2 technique 16). Expect an **rlimit bump** (`remove_waiter` went
   to `spinoff_prover`+`rlimit(40)`): carrying the pair through a heavy loop body adds load.
3. **Discharge at each segment:** object-only steps → `lemma_ready_inv_frame` (equal views);
   `BlockedNotif` splices → `lemma_ready_inv_frame_offchain` (changed nodes non-Runnable in both
   states — supply `state` equality + `wait_notif == Some(·)` ⇒ non-Runnable via `ready_complete`);
   field-rewriting steps → `lemma_ready_inv_frame_fields` (preserve `state`/`priority`/`qnext`/
   `wait_notif`; **place the `dom() == dom()` hint *before* the lemma call** — the `report_terminal`
   bug); `signal`/`fire` calls → the callee's own pair ensures.

`remove_waiter` (off-chain), `fire` (equal-views unbound branch + `signal` ensures bound branch,
plus the §2 `lemma_notif_wf_frame` discharge), and `report_terminal` (field-frame + the moved dom
hint) are the three worked examples in the tree now.

**`check_expired` (next):** identical shape to `fire`, but its `lemma_signal_ok_after_fire`
(`timer.rs:595`) carries the *same* un-guarded `forall th: wait_notif != Some(n) ==> unchanged`
precondition that broke `fire` — weaken it the same way (dom-guard / contrapositive) since
`timer_signal_ok` reads only waiters, never the Runnable re-threaded tail. Then `check_expired`
threads like `fire` (pair in the armed-walk loop invariant).

---

## 5. Remaining work (precise, ordered)

1. **`check_expired`** — weaken `lemma_signal_ok_after_fire` (§4), then thread (loop invariant).
2. **IPC fast path:** `send` (1 cap loop), `recv` (2 cap loops), `endpoint_cap_dropped`,
   `destroy_channel` (ring-cap loop). Each frames `ready_view`+`tcb_view` across its object steps
   (`lemma_ready_inv_frame`) and leans on `fire`'s pair ensures; loops carry the pair.
3. **cspace teardown SCC** (mutually recursive — add the pair to all together): `delete`,
   `obj_unref`, `destroy_cspace` (resident loop), `unref_cspace`, `revoke` (delete loop). Object
   steps → `lemma_ready_inv_frame`; the `delete → endpoint_cap_dropped → fire` arm rides the
   callee ensures.
4. **`bind`** (`thread.rs`) — once `delete` carries the pair, `bind`'s displaced-cap `delete` does
   too; the patch already added `bind`'s ensures pair.
5. **`destroy_tcb` (Step F, highest risk)** — replace the Runnable-branch
   `lemma_sysinv_frame_equal_views` (no longer valid — `unqueue_ready` changes `tcb_view` at `t` +
   predecessor) with the faithful `unqueue_ready` frame; supply its `priority < NUM_PRIOS` precond
   from `ready_complete` (`t` Runnable); the post-detach **halt** promotes
   `ready_complete_except(t) → ready_complete`. Expect a new `lemma_ready_complete_halt_promote`
   and/or `lemma_ready_wf_frame`, plus an rlimit bump.
6. **ArrayStore + checks** (`test_store.rs`) and **ledger** (scope paragraph + baseline).

---

## 6. Proof techniques (continuing parts 1–4, #1–23)

24. **Module-verifying the changed module is insufficient for a *contract* change.** A weakened
    predicate / reshaped frame must be re-checked against every transitive *consumer*: a consumer
    that case-splits on the old antecedent (`lemma_dead_tcb_frozen_trans`) or relies on the old
    frame's phantom coverage (`fire`) breaks even though the change is sound at the definition.
    Always run a full `cargo verus verify -p kcore` after a shared-predicate edit.

25. **A frame keyed on a per-key property can't constrain phantom out-of-domain keys.** State the
    caller frame *contrapositively* (keyed on the change itself, which phantoms never exhibit) and
    *dom-guard* the lemmas that consume it. The un-guarded `forall k` ranges over arbitrary phantom
    values; only "changed ⇒ …" or "in-domain ∧ …" is discharge-able.

26. **Pair a contrapositive "what changed" frame with a positive field-frame.** The contrapositive
    says *whether* a node moved; a caller needing a specific field preserved (`report`, `cspace`)
    needs the dual — a frame keyed on the small written-field set (`signal` writes only
    `state`/`qnext`/`retval`/`wait_notif`). Both are cheap (they ride the op's setter frames).

27. **A failed callee lemma poisons the caller's *unrelated* later obligations.** `revoke`'s
    census precondition "failed" only because `lemma_dead_tcb_frozen_trans` (which it calls) failed
    to verify; fixing the lemma cleared the caller with no change to `revoke`. When an error looks
    unrelated to the change, check whether a lemma it depends on is itself red.

28. *(see §3b)* **Frame a re-threaded neighbour as "only its link changed", never "state free".**

29. **`cargo verus` caches per build; a `--verify-function`/`--verify-only-module` run silently
    returns `EXIT=0` from stale cache after the crate was last built.** The tell is a *missing*
    `verification results::` line (cached) vs. a present one (real run). Only `cargo clean -p kcore
    && cargo verus verify -p kcore` is authoritative — clean-verify before trusting any green.
    Cost several wasted cycles this pass (`fire`/`signal` both reported false greens mid-work).

---

## 7. Gate state

| gate | A+B (findings-4) | this pass (in updated patch) |
|---|---|---|
| `cargo verus verify -p kcore` | 368 / 0 (in tree) | **365 / 6** (was 364/7 on bare patch — `fire` + infra now green) |
| in-tree (HEAD) | 368 / 0 | 368 / 0 (integration kept in patch — atomic) |
| `cargo test -p kcore` | green (94) | unchanged |
| `cd kernel && cargo build` | green | green |

The 6 remaining errors are exactly the **un-threaded** cascade bodies — `endpoint_cap_dropped`,
`send`, `recv` (→ `fire`), `check_expired` (→ `signal`), `bind` (→ `delete`), and `destroy_tcb`
(Step F) — each now *mechanically* unblocked by this pass's infrastructure (the reformulated
`signal` frame, the tightened enqueue frame, and the weakened `lemma_notif_wf_frame` /
`lemma_caps_consistent_frame`). The next wavefront (`delete`/`obj_unref`/`destroy_cspace`/
`unref_cspace`/`revoke`/`destroy_channel`) currently verifies green *unthreaded* (its callees'
contracts are unchanged) and will break — and be discharged the same way — as the pair is added.

This pass's verified deliverables (in the updated patch): `lemma_dead_tcb_frozen_trans` (sound
weakening consumer), `signal` (contrapositive frame + field-frame + "BlockedNotif ⇒ unchanged" +
rlimit), `make_runnable`/`ready_enqueue` (tightened global frame), `lemma_notif_wf_frame`
(dom-guard), `lemma_caps_consistent_frame` (weakened + dom-guarded clause), `remove_waiter`,
`fire`, `report_terminal`. `external_body` seams and `assume_specification`s: **unchanged** (none
added). The audit item stays **open**.
