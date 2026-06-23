# B8C — Ready-queue verification: findings (part 3)

Working notes from the third implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass began the **seam-integration**
work originally scoped as B8C-2 (re-contract `make_runnable`/`unqueue_ready`) and discovered
that B8C-2, B8C-3 (`signal`) and B8C-4 (`destroy_tcb`) are **one atomic unit**, not three
independently gate-green sub-phases. It lands the first piece of that unit — the
**`waiter_chain` priority covenant** — gate-green, and records the full, now-precisely-bounded
remaining scope so the integration can be completed confidently.

Continues `doc/results/1_b8c-findings-2.md`. Branch `b8c-ready-queue`; draft PR #138.
Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

**Course correction (confirmed with the user, who chose "full integration now").** The
part-1/part-2 decomposition assumed B8C-2/3/4 each keep `cargo verus verify -p kcore` green on
their own. They cannot: each seam flip breaks its sole verified caller, the ready invariants
and the behaviour change are atomic, and — decisively — `signal` is reachable from `delete`
(`delete → endpoint_cap_dropped → signal`), so the moment `signal` requires `ready_wf`/
`ready_complete` those preconditions cascade through `delete` and the **entire destroy/delete
SCC**. B8C-3 and B8C-4 are the *same* SCC. The integration is therefore one gate-green unit.

**Landed this pass (gate green, committed `074d412`):** the `waiter_chain` priority covenant —
`forall i: (tv[ws[i]].priority as int) < NUM_PRIOS` folded into `waiter_chain` (`cspace.rs`),
a leaf precondition on `wait` (the sole appender), and the two per-field frame lemmas
(`lemma_waiter_refs_frame_fields`, `lemma_remove_chain`) updated to carry it. This supplies the
woken head's `priority < NUM_PRIOS` to `signal` (for the faithful `make_runnable`) **via the
already-threaded `notif_wf`**, avoiding a global `prio_bounded` invariant cascade. `cargo verus
verify -p kcore` is green (0 errors); behaviour-identical; the ready-queue seam is **not** yet
flipped.

**Not landed (the remaining atomic unit, reverted to keep the tree green; WIP patch for the
seam flip + frame lemma saved at `/tmp/b8c_seamflip_wip.patch` and the contracts reproduced in
§4 below):** the two seam-contract flips, the `ready_complete` `wait_notif` strengthening, the
`signal` census rework, the 12-function `ready_wf`/`ready_complete` cascade, and the
`destroy_tcb` detach + halt-promote. This is genuinely **multi-session proof work** (the
`signal` census interaction alone reopens the B8C-1 ready ops — see §3).

---

## 1. Why B8C-2/3/4 are one unit (the three couplings)

1. **Each seam flip breaks its sole kcore caller.** `make_runnable`'s only caller is
   `notification::signal` (notification.rs:234); the faithful op writes the *old level-tail's*
   `qnext` (a second TCB), breaking signal's single-key `final.tcb_view() == old.insert(t, …)`
   ensures and its `wait_notif != Some(n) ==> unchanged` frame (the old tail is Runnable ⇒
   `wait_notif == None`, so the frame wrongly claims it unchanged). `unqueue_ready`'s only
   caller is `destroy_tcb` (thread.rs:531), whose detach proof is
   `lemma_sysinv_frame_equal_views` — which needs `unqueue_ready` to be a total no-op.

2. **Invariant and behaviour are atomic.** With today's non-faithful `make_runnable`, a woken
   thread is Runnable but *not enqueued*, so `ready_complete` is **false** after `signal`. You
   cannot add `ready_complete` to `signal` before making `make_runnable` faithful, nor vice
   versa.

3. **The cascade.** `delete` (cspace.rs:10570) → `endpoint_cap_dropped` (channel.rs:237) →
   `signal`. So `signal` requiring `ready_wf`/`ready_complete` forces those preconditions up
   through `delete` and the entire destroy/delete SCC (`delete → delete_prepare? no`;
   `delete → obj_unref → {dec_ref, destroy_cspace, destroy_tcb, destroy_channel}`,
   `destroy_cspace → delete`, `destroy_tcb → delete + unref_cspace`, `unref_cspace →
   destroy_cspace`). That SCC *is* B8C-4.

---

## 2. The threading surface (exact)

`ready_wf` + `ready_complete` must be added to requires **and** ensures of every kcore
function on the transitive caller closure of `{signal, delete, make_runnable, unqueue_ready}`.
Enumerated and bounded this pass:

| function | file | role |
|---|---|---|
| `signal` | notification.rs:66 | **hard** — enqueues; weaken wake-path tcb ensures + rework census |
| `report_terminal` | thread.rs:148 | leaf (kernel supplies); carries across `signal` |
| `check_expired` | timer.rs:691 | leaf (kernel supplies); carries across `signal` |
| `endpoint_cap_dropped` | channel.rs:237 | carries across `signal` |
| `delete` | cspace.rs:10570 | carries across `endpoint_cap_dropped` + `obj_unref` |
| `obj_unref` | cspace.rs:9609 | dispatcher → `dec_ref`/`destroy_cspace`/`destroy_tcb`/`destroy_channel` |
| `destroy_cspace` | cspace.rs:9469 | carries across its `delete` resident loop |
| `unref_cspace` | cspace.rs:9963 | carries across `destroy_cspace` |
| `destroy_channel` | channel.rs:1725 | carries across `delete` |
| `revoke` | cspace.rs:10953 | leaf (kernel supplies); carries across `delete` |
| `bind` | thread.rs:281 | leaf (kernel supplies); carries across `delete` |
| `destroy_tcb` | thread.rs:435 | **hard** — `unqueue_ready` detach + halt-promote |

**OUT** (do not reach the trio): `slot_move`, `dec_ref` (pure decrement), `delete_prepare`
(frames everything, no `signal`/destructor call), `unref_aspace`/`ref_aspace`/`map_frame`/
`descend_to_leaf` (aspace teardown never wakes a thread). All 12 functions above already carry
the four sysinv predicates (`refcount_sound`/`caps_consistent`/`end_caps_sound`/
`census_dom_complete`), so the ready pair slots in alongside.

**Mechanics.** Most carriers only frame `ready_view` + `tcb_view` across each object-only
step, discharged by **`lemma_ready_inv_frame`** (the `lemma_sysinv_frame_equal_views` analogue
for the ready pair — see §4). The genuine reasoning is concentrated in `signal` (enqueue) and
`destroy_tcb` (unqueue + halt). Decision: thread the pair as **two explicit conjuncts** (matching
the four-predicate style), **not** folded into the sysinv bundle; add to op requires/ensures,
never to framing-lemma requires (part-1 technique-1 corollary).

---

## 3. The deeper discovery — `signal`'s census needs `ready_complete` to carry `wait_notif is None`

This is the new finding that makes the unit bigger than part-2 anticipated.

The faithful `make_runnable` changes **two** TCBs: the woken `t` and the **old ready-tail `p`**
(its `qnext` retargeted to `t`). `signal`'s wake-path census proof (notification.rs:280-330)
currently assumes *only* `t` changed (`assert(k == t)` at :289) and frames `waiter_refs(o)` for
`o != n` via `lemma_waiter_refs_frame`, whose precondition is
`tvf[k] != tv0[k] ==> tv0[k].wait_notif != Some(o) && tvf[k].wait_notif != Some(o)`.

For the new changed thread `p` this needs `tv0[p].wait_notif != Some(o)`. `p` is **Runnable**
(it is on the ready chain), and `make_runnable` preserves its `wait_notif` (only `qnext`
moves) — but **there is no invariant that a Runnable thread has `wait_notif is None`**
(`priority` is a `u8`; the only state↔wait_notif facts are local, e.g.
`lemma_thread_off_all_chains`). So `tv0[p].wait_notif` is unconstrained, and the census frame
cannot be discharged.

**Chosen resolution (Option X):** fold `tv[x].wait_notif is None` into **`ready_complete`**
(and `ready_complete_except`), exactly as the priority bound was folded into `ready_chain`.
Then `p` (Runnable, charted by `ready_complete`) has `wait_notif is None`, the existing
`lemma_waiter_refs_frame` applies to both `t` (`wait_notif Some(n) → None`) and `p`
(`None`), and the census proof needs only to relax `assert(k == t)` to handle `k ∈ {t, p}`.

**Cost (reopens B8C-1):** `ready_complete`/`ready_complete_except` gain the conjunct;
`make_runnable` gains require `old.tcb_view()[t].wait_notif is None` (signal supplies it — it
clears `wait_notif` at notification.rs:229 before the call); **`ready::ready_enqueue` gains
require `t.wait_notif is None`** and must re-prove its `ready_complete` ensures;
`ready::ready_unqueue` must re-prove `ready_complete_except`; and **`lemma_ready_push_wf`**
(ready.rs:229, which establishes `ready_complete`) must carry the conjunct. `ready_dequeue`
(no `ready_complete` ensure) is unaffected.

**Rejected alternative (Option Y):** new `lemma_chain_frame_set_off` / `lemma_waiter_refs_frame_off`
keyed on `!ws.contains(k)` (chain membership) instead of `wait_notif != Some(o)`, using
`lemma_thread_off_all_chains` to put the Runnable `p` off every waiter chain. Avoids touching
B8C-1 but needs two new lemmas plus fiddlier per-thread reasoning in `signal` (t and p reach
"off o's chain" by different arguments). Option X is conceptually cleaner (a Runnable thread is
not waiting) and keeps the existing `signal` census lemmas; reconsider Option Y only if Option
X's B8C-1 ripple proves expensive.

---

## 4. Resumable plan for the remaining unit (one gate-green commit)

Order chosen to land prerequisites first and keep verify feedback tight (`--verify-module`).
The seam-flip + frame-lemma WIP is at `/tmp/b8c_seamflip_wip.patch`; the contracts are below.

**Step A — `ready_complete` `wait_notif` strengthening (gate-green standalone).** Add
`&& tv[x].wait_notif is None` to `ready_complete` and `ready_complete_except` (cspace.rs:2987,
2993). Add require `t.wait_notif is None` to `ready::ready_enqueue` (ready.rs:477) and carry
it through `lemma_ready_push_wf`'s `ready_complete` re-establishment. Re-prove
`ready::ready_unqueue`'s `ready_complete_except`. Verify `-p kcore` green (no seam flipped yet).

**Step B — frame lemma (gate-green standalone).** Add `lemma_ready_inv_frame` (cspace.rs, next
to `lemma_sysinv_frame_equal_views` ~4882):

```rust
pub proof fn lemma_ready_inv_frame<S: Store>(s0: &S, s1: &S)
    requires
        ready_wf(s0.ready_view(), s0.tcb_view()),
        ready_complete(s0.ready_view(), s0.tcb_view()),
        s1.ready_view() == s0.ready_view(),
        s1.tcb_view() == s0.tcb_view(),
    ensures
        ready_wf(s1.ready_view(), s1.tcb_view()),
        ready_complete(s1.ready_view(), s1.tcb_view()),
{ }
```

**Step C — flip the two seam contracts** (cspace.rs `#[verifier::external_trait_method]` block,
~1112/1134). Lift `ready_enqueue`/`ready_unqueue` ensures (drop the old `ready_view ==` frame).
`make_runnable` requires: `t ∈ dom`, `priority < NUM_PRIOS`, `state != Runnable`,
**`wait_notif is None`** (Step A), `ready_wf`, `ready_complete`; ensures: frame other views +
`ready_wf` + `ready_complete` + the single-point `tcb_view[t]` update + the
`x != t && old.tails[level] != Some(x) ==> unchanged` frame + `ready_seq(level).push(t)`.
`unqueue_ready` requires: `t ∈ dom`, `state == Runnable`, `priority < NUM_PRIOS`, `ready_wf`,
`ready_complete`; ensures: frame other views + `ready_wf` + `ready_complete_except(t)` +
`ready_seq(level) == rs0.remove(rs0.index_of(t))` + `t`'s preserved fields + the signal-shaped
frame. (Exact text in `/tmp/b8c_seamflip_wip.patch`; it mirrors ready.rs:484-508 and 738-764
with `store`→`self` and `cspace::`→bare.)

**Step D — `signal`** (notification.rs). Add `ready_wf`/`ready_complete` to requires + ensures.
Before `make_runnable(t)`: capture `st_pre`, prove `ready_wf(st_pre)`/`ready_complete(st_pre)`
(the fixups change only `t`, still `BlockedNotif`; needs a "frame across a non-Runnable
thread's edit" lemma — generalise `lemma_ready_inv_frame` to allow changed threads that are
non-Runnable in both states, via `lemma_ready_seq_frame` cspace.rs:3006); derive
`t.priority < NUM_PRIOS` from the strengthened `waiter_chain` (`notif_wf`), `t.wait_notif is
None` from the :229 clear, `t.state != Runnable` (still BlockedNotif). `make_runnable`'s
ensures then give `signal`'s `ready_wf`/`ready_complete`. **Weaken** the wake-path tcb ensures
(:121, :136) to admit `p = old.ready_view().tails[level]`'s `qnext` write. **Rework the census
block** (:280-330): replace `assert(k == t)` with handling `k ∈ {t, p}` — both have
`wait_notif != Some(o)` for `o != n` (`t`: `Some(n)`; `p`: `None` from `ready_complete`).
Verify `--verify-module notification`.

**Step E — the 12-function cascade carry** (§2). Add the pair to each carrier; discharge each
object-only segment with `lemma_ready_inv_frame`; the `signal`/`destroy_tcb` calls use the
callee ensures. Watch the recursive SCC (`delete`/`obj_unref`/`destroy_cspace`/`destroy_tcb`) —
add the pair to all of them together. Verify per module (`cspace`, `channel`, `thread`, `timer`).

**Step F — `destroy_tcb`** (thread.rs:435, detach ~531). Replace the
`lemma_sysinv_frame_equal_views` no-op detach with the faithful `unqueue_ready`: splice
preserves `ready_wf`; non-tcb/non-ready views framed; `tcb_view` changes only at `t` + its live
predecessor (`dead_tcb_frozen`/`home_views_frozen` permit a *live* predecessor's `qnext` move —
discharge from `unqueue_ready`'s signal-shaped frame). After the unqueue, the existing halt
(`state → Halted`) promotes `ready_complete_except(t)` → `ready_complete`. Add `ready_complete`
to ensures. Expect an `rlimit` bump. Verify `--verify-module thread`.

**Step G — ArrayStore + checks** (test_store.rs, plain Rust). `make_runnable`/`unqueue_ready`
→ `crate::ready::ready_enqueue`/`ready_unqueue`. Extend `signal_frame` (:2681) to assert the
enqueue (bit set, `t` at the chain tail); extend `check_destroy_tcb` (:1850) to assert `t`
spliced out. Note `signal_frame`/`wait_*` fixtures must seed `cur.priority < NUM_PRIOS`
(the new `wait` precondition) and Runnable threads with `wait_notif: None` (Step A).

**Step H — gates + ledger.** `cargo verus verify -p kcore` green (record total); `cargo test -p
kcore`; `cd kernel && cargo build`; QEMU boot smoke. Ledger (`verus_trusted-base.md`): add the
ready queue to the verified-surface scope paragraph + bump the baseline. (Kernel rewiring of
the `KernelStore` realizations stays the renumbered **B8C-3** (was B8C-5); tests/ledger polish
the renumbered **B8C-4** (was B8C-6).)

---

## 5. Proof techniques (continuing part 1/2)

17. **Fold a per-element bound into the chain covenant that already travels, not a global
    invariant.** A blocked thread's `priority < NUM_PRIOS` rides `notif_wf` because it is folded
    into `waiter_chain`; a Runnable thread's `priority < NUM_PRIOS` and `wait_notif is None`
    ride `ready_complete`. No `prio_bounded`/`runnable_not_waiting` invariant has to be threaded
    through the cap-op surface — the covenant the predicate already quantifies over carries it.
    Only the **sole appender** (`wait` for waiters; `make_runnable` for the ready chain) needs a
    matching leaf precondition.

18. **Strengthening a covenant ripples to its per-field frame lemmas, not its full-view ones.**
    Adding `priority` to `waiter_chain` broke exactly the two lemmas that preserve chains via
    *field* hypotheses (`qnext`/`wait_notif`/`state`) — `lemma_waiter_refs_frame_fields`,
    `lemma_remove_chain` — and nothing that preserves them via full `TcbView` equality
    (`signal`/channel ops verified untouched). Fix: add the matching field hypothesis
    (`priority == `, here) to those lemmas; their single callers already preserve it.

19. **A faithful tail-append perturbs a *second* node; the census survives only because that
    node is off every census chain.** `make_runnable`'s old-tail `qnext` write is invisible to
    the refcount census *iff* the old tail is not on any waiter chain — which needs
    `wait_notif is None` for Runnable threads (technique 17), or `lemma_thread_off_all_chains`.
    Budget for this when lifting any intrusive-tail-append op behind a census-bearing caller.

---

## 6. Gate state

| gate | after B8C-1 | this pass (`074d412`) |
|---|---|---|
| `cargo verus verify -p kcore` | 367 / 0 | **green / 0** (waiter_chain priority covenant) |
| `cargo test -p kcore` | green (94) | green (unchanged) |
| `cd kernel && cargo build` | green | green (contract-erased; unaffected) |
| QEMU boot | unchanged | unchanged (seam not flipped) |

`external_body` seams and `assume_specification`s: **unchanged** (none added). The audit item
stays **open** — the running scheduler still uses the unverified list logic. The remaining unit
(§4) is the bulk of the integration and is multi-session proof work.
