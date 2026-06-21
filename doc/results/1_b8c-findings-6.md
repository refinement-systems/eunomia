# B8C — Ready-queue verification: findings (part 6)

Working notes from the sixth implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass **completed the atomic
seam-integration** that parts 3–5 built up: re-applied the integration patch, threaded the
*entire* remaining cascade (`check_expired`, the IPC fast path, the cspace teardown SCC, `bind`),
and resolved **`destroy_tcb` Step F** — the one genuine remaining risk. `cargo verus verify -p
kcore` is **374 verified, 0 errors**, in the tree; `cargo test -p kcore` green (94); `cd kernel &&
cargo build` green. **The integration is landed in-tree — the patch is retired — and the audit
§4.2 ready-queue item is closed.**

Continues `doc/results/1_b8c-findings-5.md`. Branch `b8c-ready-queue`; draft PR #138.
Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

**B8C-2 is done.** The five prior passes solved the conceptual core (the contrapositive `signal`
frame, the tightened enqueue frame, the dom-guarded/weakened consumer lemmas) and threaded the
first carrier layer; this pass mechanically threaded everything that remained and closed
`destroy_tcb`. The atomic unit verifies green and is **in the tree** (no more WIP patch).

**Threaded + verified this pass:**

- **`check_expired`** (`timer.rs`) — weakened `lemma_signal_ok_after_fire`'s un-guarded
  `wait_notif != Some(n) ==> unchanged` precondition to the **contrapositive + non-Runnable
  helper** form (the same shape that fixed `fire`), then threaded the armed-walk loop. Also added
  `ready_view` frame ensures to **`disarm`/`destroy_timer`** (the patch never touched `timer.rs`).
- **IPC fast path** — `endpoint_cap_dropped`, `send`, `recv`, `destroy_channel` (`channel.rs`).
  `send`/`recv` needed the ready pair pinned in their cap/ring loop invariants (the function
  `requires` don't survive into the loop). `destroy_channel`'s five nested loops each carry the
  pair; `release_binding` gained `tcb_view`/`ready_view` frame ensures.
- **cspace teardown SCC** — `delete`, `obj_unref` (all six cap arms), `destroy_cspace`,
  `unref_cspace`, `revoke` (`cspace.rs`). Object-only steps → `lemma_ready_inv_frame`; the
  destructor arms ride the callee pair ensures; loops carry the pair.
- **`bind`** (`thread.rs`) — finished: pair carried across the displaced-cap `delete`,
  `set_tcb_bind_bits` (`lemma_ready_inv_frame_fields`), and the notification `slot_move`.
- **`destroy_tcb` Step F** (`thread.rs`, §2) — the faithful `unqueue_ready` detach + the
  **halt-promote**. The hard one.
- **ArrayStore** (`test_store.rs`) — `make_runnable`/`unqueue_ready` made **faithful** (route
  through `ready_enqueue`/`ready_unqueue`), so the host model realizes the seam contracts Verus
  assumes; 94 tests stay green.
- **Ledger** — ready queue added to the verified-surface scope paragraph; kcore baseline bumped
  to **374** (it was stale at 342 — never bumped through B8C-1/A+B; corrected now).

No new `external_body` / `assume_specification` (trusted base unchanged). No `[verifying]` table
edit, no §6.1 prose edit (Honesty note 4 — the ready queue has no blessed `[verifying]` tag).

---

## 1. The cascade was mechanical — the carrier recipe held

Every carrier outside `signal`/`destroy_tcb` reduced to the part-5 §4 recipe: add
`ready_wf`+`ready_complete` to requires/ensures; discharge object-only segments with
`lemma_ready_inv_frame(old, store)`; pin the pair in every loop invariant; lean on the callee pair
ensures at `fire`/`signal`/`delete`/destructor calls. The cspace SCC (`delete`/`obj_unref`/
`destroy_cspace`/`unref_cspace`/`revoke`) verified **188/0 at the module level** once their
contracts were threaded together and the destructors' contracts (`destroy_tcb`/`destroy_channel`)
exposed the pair — confirming the SCC closes against contracts, body order irrelevant.

**One patch-coverage gap:** the patch never touched `timer.rs`, so `disarm`/`destroy_timer` did
not frame `ready_view` (the B8C-1 sweep was cspace-only). `check_expired` carries the pair across a
`disarm`, so `disarm` needed the frame ensure added (its splice loop never touches the ready queue,
so the setters discharge it for free). Same for `destroy_notif`/`release_binding` (notification/
channel no-ops missing only the `ready_view` clause).

---

## 2. Step F — the faithful `unqueue_ready` detach + halt-promote (the genuine risk)

`destroy_tcb`'s old Runnable-branch detach was `store.unqueue_ready(t)` discharged by
`lemma_sysinv_frame_equal_views` — valid only while `unqueue_ready` was a total no-op. The faithful
op changes `tcb_view` at `t` **and the spliced predecessor** (both Runnable), so the no-op frame is
gone. Three sub-problems, all solved:

### 2a. The census reads the spliced predecessor's `cspace`/`aspace`

`obj_census` includes `thread_hold_refs(tv, o)` = `|{k : tv[k].cspace == Some(o)}| + |{k :
tv[k].aspace == Some(o)}|`. The faithful unqueue re-threads the predecessor `p`'s `qnext`, so `p`
is a *changed* node — and to prove `refcount_sound` survives, `destroy_tcb` needs `p`'s
`cspace`/`aspace` preserved. The B8C-1/part-2 `ready_unqueue` contract exposed only `wait_notif` +
`bind_slots` for changed nodes. **Fix:** extend the op's (and seam's) signal-shaped frame to also
preserve `state`, `cspace`, `aspace` (and `t`'s `report`) for changed nodes — the op writes *only*
`qnext`, so the setter discharges all of them for free. With that, the **off-chain frame lemmas**
discharge the detach: `lemma_waiter_refs_frame_offchain` (changed = Runnable = not BlockedNotif),
`lemma_thread_hold_frame` (`cspace`/`aspace` preserved), `lemma_caps_consistent_frame_thread_offchain`
(changed not blocked-waiters), then `lemma_refcount_sound_from_census_eq`.

*Subtlety:* the off-chain lemmas range over **all** keys (phantom included). Exposing
`final[x].state == old[x].state` for changed nodes (not just `old.state == Runnable`) makes the
"changed ⇒ not BlockedNotif in *both* states" precondition discharge for every key without
phantom/`ready_complete` reasoning — strictly the cleanest closure (technique 31).

### 2b. The halt-promote: `ready_complete_except(t)` → `ready_complete`

`unqueue_ready` ensures `ready_complete_except(t)` (it leaves `t` Runnable-and-off-chain). The
subsequent halt (`state → Halted`) promotes it to full `ready_complete`. New
**`lemma_ready_complete_halt_promote`**: given `ready_wf` + `ready_complete_except(t)`, `ready_view`
framed, only `t` changed, `t` now non-Runnable, and **`t` off every ready chain in the pre-state**,
it yields `ready_wf` + `ready_complete`. The "`t` off every ready chain" premise is supplied by new
**`lemma_thread_off_all_ready_chains`** (a non-Runnable thread, or one absent from its own level's
chain, is off all chains — by `ready_wf`'s `state == Runnable && priority == level` covenant), and
for the Runnable branch the "off its own level" half is the splice fact `!rs0.remove(index_of(t))
.contains(t)` from new **`lemma_seq_remove_drops`** (removing a `no_duplicates` seq's unique
occurrence drops it).

### 2c. Branch convergence

All three detach branches (Runnable→unqueue / BlockedNotif→`remove_waiter` / else→no-op) must reach
the post-detach snapshot with the *same* facts: `ready_wf`, `ready_complete_except(t)`, and "`t`
off all ready chains". The no-op branches ride `lemma_ready_inv_frame`; the `remove_waiter` branch
rides its pair ensures (it preserves `t.state`, so `t` stays non-Runnable). Full `ready_complete`
weakens to `_except(t)` explicitly per branch so the merge carries it. After the halt-promote, the
pair carries through the bind-slot `delete`s (callee ensures) and the cspace/aspace
clear+unref (`lemma_ready_inv_frame_fields` across the field clears; `unref_aspace` frames both
views).

---

## 3. New lemmas (all verified)

| lemma | file | role |
|---|---|---|
| `lemma_seq_remove_drops` | cspace.rs | removing a `no_duplicates` seq's unique occurrence drops it |
| `lemma_thread_off_all_ready_chains` | cspace.rs | non-Runnable / off-own-level ⇒ off every ready chain |
| `lemma_ready_complete_halt_promote` | cspace.rs | `ready_complete_except(t)` + halt(`t`) ⇒ `ready_complete` |

Contract strengthenings: `ready_unqueue` (ready.rs) + the `unqueue_ready` seam (cspace.rs) — the
signal-shaped frame gains `state`/`cspace`/`aspace` preservation for changed nodes and `report` for
`t`. `disarm`/`destroy_timer` (timer.rs), `destroy_notif` (notification.rs), `release_binding`
(channel.rs) — `ready_view` (and for `release_binding`, `tcb_view`) frame ensures added.
`lemma_signal_ok_after_fire` (timer.rs) — precondition weakened to the contrapositive + non-Runnable
helper form.

---

## 4. Proof techniques (continuing parts 1–5, #1–29)

30. **A faithful intrusive splice perturbs the predecessor's *census-relevant* fields too —
    expose them all.** It is not enough to frame the spliced neighbour's `wait_notif`; the
    refcount census reads `cspace`/`aspace` (`thread_hold_refs`), so the op contract must preserve
    those for changed nodes. Since the op writes only the link field, "every field but `qnext`"
    is provable for free — expose exactly the fields a census-bearing caller reads.

31. **State `final.state == old.state` for changed nodes, not just `old.state == P`.** The
    off-chain frame lemmas demand a property of the *post-state* of changed threads (e.g. "not
    BlockedNotif in both states"). Exposing only `old.state == Runnable` forces the caller into
    `ready_complete`/phantom reasoning to recover `final.state`; exposing
    `final.state == old.state` discharges it for every key (phantoms vacuously) directly.

32. **A transient-liveness "except-t" predicate is promoted by the event that removes `t` from the
    quantified class.** `ready_complete_except(t)` becomes `ready_complete` precisely when `t`
    leaves the Runnable set (the halt). The promotion lemma needs "`t` off every chain" (so the
    chains are `t`-free and unchanged) — itself derived from the `wf` covenant for non-Runnable `t`,
    or from the splice for a still-Runnable-but-unqueued `t`.

33. **The mutually-recursive SCC closes against *contracts*, so thread them all first, fix bodies in
    any order.** Adding the ready pair to `delete`/`obj_unref`/`destroy_cspace`/`unref_cspace`/
    `revoke`/`destroy_channel`/`destroy_tcb` contracts together let each body verify against the
    others' new contracts independently; the cspace module hit 188/0 before `destroy_tcb`'s body
    was even reworked (it verified against `destroy_tcb`'s contract).

34. **A patch scoped to one module silently under-frames a global view in *other* modules.** The
    B8C-1 `ready_view` frame sweep was cspace-only; the integration patch was cspace/channel/
    notification/ready/thread. `timer.rs` was never swept, so `disarm`/`destroy_timer` lacked the
    `ready_view` frame the moment a cross-module caller (`check_expired`) carried the pair across
    them. When a global view crosses a module boundary, re-grep *every* module's op ensures.

---

## 5. Gate state

| gate | A+B (findings-4/5, in tree) | this pass (in tree) |
|---|---|---|
| `cargo verus verify -p kcore` | 368 / 0 | **374 / 0** |
| `cargo test -p kcore` | green (94) | green (94) |
| `cd kernel && cargo build` | green | green |
| QEMU boot | unchanged | unchanged (behaviour-identical) |
| integration | in WIP patch (365/6) | **landed in-tree; patch retired** |

`external_body` seams and `assume_specification`s: **unchanged** (none added). The **audit §4.2
ready-queue item is closed** — the ready-queue list logic (witnesses, four ops, bitmap coherence,
splice walks) is verified in `kcore` and integrated through the `make_runnable`/`unqueue_ready`
seams that `signal`/`fire`/the IPC fast path/the teardown SCC/`destroy_tcb` lean on.

**Remaining (renumbered tail, *not* B8C-2):** **B8C-3** — kernel `KernelStore` rewiring (route the
real `make_runnable`/`unqueue_ready`/`enqueue`/`dequeue`/`top_ready` wrappers through the verified
ops; `maybe_switch` + asm switch stay trusted shell). **B8C-4** — optional deeper host-test
assertions (`check_signal_frame`/`check_destroy_tcb` could assert the precise bit-set/tail-position/
splice-out, beyond the faithful-impl exercise the 94 tests already give) + ledger polish. Neither is
load-bearing for the verified surface, which is complete.
