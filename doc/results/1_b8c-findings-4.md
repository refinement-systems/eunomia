# B8C — Ready-queue verification: findings (part 4)

Working notes from the fourth implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass executed the **seam-integration
unit** (findings-3 §4, Steps A–H — the now-atomic "B8C-2"). It **landed Steps A and B
gate-green** and **built + module-verified the hard core of the integration** (the seam flip and
the full `signal` rework, including a model-level fix), but then **discovered the cascade is
materially larger than findings-3 §2 estimated** — `signal` is reachable from the IPC *fast path*
(`send`/`recv`), not only from `delete`. Per this branch's discipline (keep `cargo verus verify -p
kcore` green between landings — findings-1/2/3), the integration beyond A+B is **kept out of the
tree and saved as a patch** (`doc/results/b8c-2-integration-wip.patch`, also `/tmp/b8c2_integration_wip.patch`).

Continues `doc/results/1_b8c-findings-3.md`. Branch `b8c-ready-queue`; draft PR #138.
Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

**Landed this pass (gate green, committed):** the two independently-green foundation steps.

- **Step A — `ready_complete` `wait_notif` strengthening.** `&& tv[x].wait_notif is None` folded
  into `ready_complete` / `ready_complete_except` (`cspace.rs`); carried through `ready_enqueue`'s
  new leaf precondition, `lemma_ready_push_wf`, and `ready_unqueue`'s `ready_complete_except`
  re-proof (`ready.rs`). This is the §3 (findings-3) discovery, realized.
- **Step B — `lemma_ready_inv_frame`.** The `lemma_sysinv_frame_equal_views` analogue for the
  ready pair (carries `ready_wf`+`ready_complete` across any step that frames `ready_view`+`tcb_view`).

`cargo verus verify -p kcore` rose **367 → 368, 0 errors**; `cargo test -p kcore` green (94);
behaviour-identical; no new `external_body`/`assume_specification`.

**Built, module-verified, but NOT landed (in the patch):** the seam-contract flip
(`make_runnable`/`unqueue_ready`), the **complete `signal` rework** (enqueue + census, **verified —
`--verify-module notification` = 5 verified, 0 errors**), the strengthened `ready_enqueue`/
`ready_unqueue` ops (a global "all-fields-but-state/qnext" frame, **`--verify-module ready` = 19/0**),
three new frame lemmas (`lemma_ready_inv_frame_offchain`, `lemma_ready_inv_frame_fields`,
`lemma_ready_chain_frame_fields`), a **`dead_tcb_frozen` model fix** (see §2), the
`lemma_drop_first_chain` weakening, and the `report_terminal`/`bind` carrier contracts.

**Why not landed:** the integration is atomic *and* its cascade reaches the IPC fast path (§3), so
it cannot be gate-green until the whole cascade + `destroy_tcb` are done — genuinely multi-session.
Two model-level discoveries (below) are the substantive new results; both are **solved and proven**
in the patch, de-risking the remainder.

---

## 1. The two seam ops are already faithful; the integration is the cost

Confirmed against the live tree (B8C-1's ops): `ready_enqueue` (`ready.rs`) and `ready_unqueue`
already carry the full faithful contracts the seam needs — the seam flip is a near-verbatim lift
(`store`→`self`, `cspace::`→bare). This pass additionally **strengthened both ops** with a global
frame and re-verified them:

- `ready_enqueue` now ensures `forall x: tvf[x] == TcbView { state: .., qnext: .., ..tv0[x] }` —
  every field but `state`/`qnext` is preserved for *every* thread. `signal`'s census/caps proofs
  read the **old ready-tail `p`**'s `wait_notif`/`cspace`/`aspace`/`bind_slots` off this.
- `ready_unqueue` now also ensures `t`'s `wait_notif`/`bind_slots` preserved and extends its
  signal-shaped frame with `wait_notif`/`bind_slots` preservation for the spliced predecessor —
  the fields `destroy_tcb`'s census/`home_views_frozen` will need (B8C-2 Step F, still pending).

Both re-verify green (the global frame is an empty-`by`-block: the `set_tcb_*` setters frame all
non-written fields).

---

## 2. Discovery #1 — `dead_tcb_frozen` is incompatible with a faithful enqueue (SOLVED)

The faithful `make_runnable` perturbs a **second** TCB beyond the woken `t`: the **old ready-tail
`p`** of `t`'s level (its `qnext` retargeted to `t`). `p` is **Runnable**, and — because ready-queue
membership carries no refcount (findings-1 §1.1) — `p` may legitimately have `refs == 0`.

`signal`'s (unconditional) `dead_tcb_frozen(old, final)` ensure says: a *dead* thread (`refs == 0`,
`wait_notif is None`) is **frozen** (unchanged). A refs-0 Runnable `p` matches that "dead" antecedent
yet its `qnext` changed — so the ensure is **false**. There is **no existing invariant** tying
Runnable to `refs > 0` (grep-confirmed: every `Runnable` mention in `cspace.rs` is a ready-queue
predicate). A faithful enqueue therefore breaks `dead_tcb_frozen` as written.

**Fix (proven in the patch): weaken `dead_tcb_frozen_at` with `state != Runnable`.**

```rust
(s0.refs_view()[x] == 0 && s0.tcb_view()[x].wait_notif is None
    && s0.tcb_view()[x].state != ThreadState::Runnable)   // ← B8C
    ==> (... s1.tcb_view()[x] == s0.tcb_view()[x])
```

A Runnable thread is *scheduled*, not *dead* — and the teardown consumers only ever freeze
Halted/Blocked dead threads (`destroy_tcb` halts its subject before recursing). It is a **weakening**
(stronger antecedent), so every one of the ~52 existing `dead_tcb_frozen` producers satisfies it
automatically; `lemma_dead_tcb_frozen_signal_shaped` gains a matching `|| state == Runnable`
disjunct (also a weakening — its 9 callers pass the old 2-disjunct form unchanged). `signal`
re-verifies with this. **This is the linchpin that made `signal` provable** and is the key reusable
result of the pass.

---

## 3. Discovery #2 — the cascade reaches the IPC fast path (the dominant remaining cost)

findings-3 §2 enumerated the threading surface as the transitive caller-closure of `signal` and
traced it through `delete → endpoint_cap_dropped → signal` (12 functions). **It missed that `signal`
is also reachable from the IPC fast path.** Ground truth (grep-confirmed):

```
fire (channel.rs:426) → signal           # the notification-fire wrapper
  ├─ endpoint_cap_dropped (channel.rs:358)   [teardown — findings-3 had this]
  ├─ send                (channel.rs:974)    [EV_READABLE — MISSED]
  └─ recv                (channel.rs:1426)   [EV_WRITABLE — MISSED]
signal also ← report_terminal (thread.rs:218), check_expired (timer.rs:792)
```

Because `signal` now **requires** `ready_wf`/`ready_complete`, so does `fire`, and therefore so do
**`send`, `recv`**, and *their* transitive callers — i.e. essentially the whole channel/IPC
operation surface, not just the destroy/delete SCC. This is the bulk of the remaining work and the
reason the integration is genuinely multi-session. (It does not change *what* must be proven on each
carrier — each frames `ready_view`+`tcb_view` across its object steps and leans on the callee
ensures at the `fire`/`signal` call — but it multiplies the number of carriers.)

Mechanically each carrier is cheap (add the pair to requires/ensures; discharge object-only
segments with `lemma_ready_inv_frame`; loops carry the pair in their invariant), but the **count**
is large, and `send`/`recv` have substantial bodies.

---

## 4. The validated approach + toolkit (so the next pass is fast)

The patch (`doc/results/b8c-2-integration-wip.patch`) contains all of the below, module-verified.
Re-apply it onto the A+B foundation (`git apply doc/results/b8c-2-integration-wip.patch`) and finish
the cascade.

**Frame-lemma toolkit (in `cspace.rs`, all proven):**
- `lemma_ready_inv_frame(s0,s1)` — `ready_view` *and* `tcb_view` equal ⇒ pair carries (object-only
  cascade steps). Landed (Step B).
- `lemma_ready_inv_frame_offchain(s0,s1)` — `ready_view` equal, `tcb_view` changes only at threads
  **non-Runnable in both states** ⇒ pair carries. For `signal`'s pre-enqueue fixups (the woken `t`
  is still `BlockedNotif`), `remove_waiter` (a `BlockedNotif` splice), and `destroy_tcb`'s blocked/
  halt branches.
- `lemma_ready_inv_frame_fields(s0,s1)` — `ready_view` equal, the four ready-relevant fields
  (`state`/`priority`/`qnext`/`wait_notif`) preserved for **every** thread (other fields may change)
  ⇒ pair carries. For `report_terminal` (writes `report`) and `bind` (writes `cspace`/`aspace`/
  `bind_*`). Built on `lemma_ready_chain_frame_fields` (the field-based chain frame — `ready_chain`
  reads only `qnext`/`state`/`priority`).

**`signal` rework (notification.rs, proven):** requires/ensures gain the pair; the wake-path
`tcb_view` ensures are weakened — the single-key `insert(t,..)` becomes a frame admitting the old
ready-tail `p`, and the `wait_notif != Some(n) ==> unchanged` frame gains `&& state != Runnable`
(so a faithful enqueue's Runnable `p` is admitted while waiters of any `m != n`, being
`BlockedNotif`, are still framed by callers). The census block handles the changed set `{t, p}`:
`t` (was `Some(n)`, now `None`) and `p` (Runnable ⇒ `wait_notif None`, preserved) both satisfy
`wait_notif != Some(o)` for `o != n`. `lemma_drop_first_chain`'s precondition is weakened to
"only `n`'s waiters unchanged" so the Runnable `p`'s change is admitted.

**Seam contracts (cspace.rs `ExStore`):** `make_runnable` requires `t ∈ dom`, `priority < NUM_PRIOS`,
`state != Runnable`, **`wait_notif is None`**, `ready_wf`, `ready_complete`; ensures the lift of
`ready_enqueue` + the global frame. `unqueue_ready` mirrors `ready_unqueue`.

---

## 5. Remaining work (revised, ordered) — the renumbered B8C-2 tail + B8C-3/4

1. **Re-apply the patch** onto A+B (it module-verifies `ready` + `notification`).
2. **Cascade — teardown SCC** (findings-3 §2 list): `remove_waiter`, `endpoint_cap_dropped`,
   `destroy_channel`, `delete`, `obj_unref`, `destroy_cspace`, `unref_cspace`, `revoke`, `bind`,
   `report_terminal`, `check_expired`. Pattern: pair in requires/ensures; `lemma_ready_inv_frame`
   (object steps) / `_offchain` (blocked splices) / `_fields` (`report_terminal`/`bind`); pair in
   loop invariants (`destroy_cspace`/`revoke`).
3. **Cascade — IPC fast path (NEW, §3):** `fire`, `send`, `recv`, and their transitive callers.
   The dominant cost; same mechanics.
4. **`destroy_tcb` (Step F, highest risk):** replace the no-op `lemma_sysinv_frame_equal_views`
   detach with the faithful `unqueue_ready`; discharge sysinv across the splice (the strengthened
   `unqueue_ready` frame already exposes the predecessor's `wait_notif`/`bind_slots`); the post-
   detach halt promotes `ready_complete_except(t) → ready_complete`. Expect an `rlimit` bump.
5. **ArrayStore + checks (Step G):** route `make_runnable`/`unqueue_ready` through the verified ops;
   extend `signal_frame`/`check_destroy_tcb`; seed `wait_notif: None` Runnable fixtures.
6. **Gates + ledger (Step H):** full `cargo verus verify -p kcore` green (record total); ledger
   scope paragraph + baseline. **B8C-3** (kernel `KernelStore` rewiring) and **B8C-4** (extra tests
   + ledger polish) follow.

---

## 6. Proof techniques (continuing parts 1–3, #17–19)

20. **A faithful intrusive-list op perturbs a *second, live* node; weaken any "dead ⇒ frozen"
    invariant with `state != Runnable`.** The old tail (enqueue) / spliced predecessor (unqueue)
    is Runnable and may be refs-0, so a `dead = (refs 0 ∧ detached)` predicate wrongly demands it
    frozen. Excluding Runnable threads is sound (they are scheduled, not dead) and is a *weakening*,
    so every producer/consumer of the predicate re-verifies for free. (Discovery #1.)

21. **Trace the fired-event wrapper's *full* call graph before sizing a notification cascade.** A
    `signal`/`fire` precondition propagates to **every** event source — the IPC fast path
    (`send`/`recv`), not just teardown. Grep `fn fire`/`signal(` callers first; the destroy/delete
    SCC is only one arm. (Discovery #2 — what findings-3 §2 undercounted.)

22. **Field-based frame lemmas for an invariant that reads a field subset.** `ready_wf`/
    `ready_complete` read only `state`/`priority`/`qnext`/`wait_notif`, so an edit to any *other*
    field (`report`, `cspace`, `bind_*`) carries them via a frame keyed on the read subset
    (`lemma_ready_chain_frame_fields` / `lemma_ready_inv_frame_fields`) — cheaper and more reusable
    than full-`TcbView`-equality frames where the op rewrites an unrelated field.

23. **Three graded ready-frame lemmas, picked by what the step preserves.** equal-views
    (`_inv_frame`) ⊂ non-Runnable-edit (`_offchain`) ⊂ ready-fields-preserved (`_fields`). Match the
    lemma to the step; the strongest-precondition one that applies is the cheapest to discharge.

---

## 7. Gate state

| gate | after B8C-1 (findings-2) | this pass (landed) |
|---|---|---|
| `cargo verus verify -p kcore` | 367 / 0 | **368 / 0** (Steps A+B) |
| `cargo test -p kcore` | green (94) | green (94) |
| `cd kernel && cargo build` | green | green |
| QEMU boot | unchanged | unchanged (seam not flipped) |

Integration (patch, not in tree): `--verify-module ready` 19/0, `--verify-module notification`
5/0 — i.e. the seam ops and the reworked `signal` verify; the cascade (§3, §5) and `destroy_tcb`
remain. `external_body` seams and `assume_specification`s: **unchanged** (none added). The audit
item stays **open** — the running scheduler still uses the unverified list logic.
