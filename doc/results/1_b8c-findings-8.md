# B8C — Ready-queue verification: findings (part 8)

Working notes from the eighth (final) implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass is **B8C-4 — host-test polish**: the
optional deeper `ArrayStore` assertions findings-6/7 flagged as the only remaining tail item.
After this pass the ready-queue work is complete on every axis — verified, integrated, executed in
production (B8C-3), and host-exercised with teeth.

Continues `doc/results/1_b8c-findings-7.md` (B8C-3, the kernel rewiring). Branch
`b8c-ready-queue`; draft PR #138. Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

**B8C-4 is done.** Added executable mirrors of the ready-queue invariant
(`ready_seq_exec`/`ready_wf_exec`/`ready_complete_exec`), six new host tests exercising the four
verified ops directly, and deepened the two existing scheduler `check_*` helpers to assert the
precise ready-queue post-state. `cargo test -p kcore` rises **94 → 100, 0 failed**; `cargo verus
verify -p kcore` **374/0** (unchanged — all changes are `#[cfg(test)]`); `cd kernel && cargo build`
green.

This pass adds **no verified item and no trusted-base change** — it is pure test-oracle work, in
the established `*_exec` / `*_exec_has_teeth` / `check_*` discipline the cspace/channel/notif/timer
mirrors already follow. The ledger needed no edit (B8C-2 already added the ready queue to the
verified-surface scope paragraph and set the baseline to 374).

---

## 1. What was added

### 1.1 Executable invariant mirrors (`test_store.rs`)

Three plain-Rust mirrors of the ghost (hence uncallable-from-test) predicates, beside the existing
`notif_wf_exec`/`timer_wf_exec`:

- **`ready_seq_exec(st, level)`** — walks `level`'s chain from the head through `qnext`, bounded by
  `tcbs.len()+1` so a malformed cycle surfaces a duplicate rather than looping (the `notif_wf_exec`
  cycle-guard idiom). Plus `ready_ids` — the same as raw `u64`s, since `ObjId` has no `Debug`, so
  sequence `assert_eq!`s compare tags (the `wait_signal_fifo` `.0` idiom).
- **`ready_wf_exec(st)`** — `cspace::ready_wf` ∧ `ready_bitmap_coherent` over all 32 levels:
  head-None iff tail-None; **bit set iff level non-empty**; the chain is duplicate-free with
  head/tail its first/last node; every charted node a resident Runnable TCB at that level threaded
  by `qnext`. Deliberately does **not** fold in `ready_complete` — that is a separate predicate
  (`ready_unqueue` preserves only `ready_complete_except`), so conflating them would make the oracle
  wrong for post-unqueue states (proof technique 7, parts 1–6).
- **`ready_complete_exec(st)`** — every Runnable thread is charted on its level's chain (the
  `timer_complete` analogue).

### 1.2 New tests (six)

- **`ready_enqueue_top_dequeue_round_robin`** — enqueue across levels 5 and 9; `top_ready` picks
  the highest non-empty level; `ready_dequeue` is FIFO within a level and clears each presence bit
  as the level empties; `ready_wf` holds throughout; dequeued threads are left Runnable-and-off-
  chain (the `maybe_switch` hand-off shape).
- **`ready_unqueue_splices_arbitrary_position`** — splice from head / middle / tail / sole,
  asserting the re-thread, the tail fixup, and that the bit clears only when the level empties.
- **`randomized_ready_sweep`** — 400 seeds × 30 ops of enqueue/unqueue/dequeue over a 10-thread
  pool spread across levels {0,3,5,9,31}, asserting `ready_wf` **and** `ready_complete` after every
  op (12 000 trials; asserts all three ops were exercised). The ready-queue analogue of
  `randomized_fifo_sweep`. The model recycles a removed thread's state to `Inactive`, keeping
  "Runnable iff on a chain" so `ready_complete` stays meaningful.
- **`ready_wf_exec_has_teeth`** / **`ready_complete_exec_has_teeth`** — the oracle-teeth discipline:
  each malformation (bit/level disagreement either way, non-Runnable charted node, wrong-level node,
  tail mismatch, cycle, head/tail-None disagreement, off-chain Runnable thread) is rejected.
- **`destroy_tcb_splices_out_of_ready_queue`** — `destroy_tcb` on a thread genuinely **in the
  middle** of its ready chain (between two siblings at a shared level). Exercises the faithful
  `unqueue_ready` splice (predecessor re-thread; level stays non-empty so the bit survives) followed
  by the halt that promotes `ready_complete_except(t)` back to `ready_complete`. Mirrors the
  known-valid `destroy_tcb_structural` fixture so all of `check_destroy_tcb`'s preconditions hold.

### 1.3 Deepened existing checks

- **`check_signal_frame`** now asserts `ready_wf_exec` post (the wake path enqueues the woken waiter
  via `make_runnable` → `ready_enqueue`); the **`signal_frame`** test additionally asserts the
  precise placement — the woken waiter is the sole node at the tail of its level, presence bit set,
  `qnext` cleared.
- **`check_destroy_tcb`** now asserts `ready_wf_exec` post **and** that `t` sits on no chain at any
  level (the splice-out). These hold for the existing `destroy_tcb_structural` (empty queue → both
  vacuously true), so that test stays green unchanged.

---

## 2. Notes

- **The oracle is the value, not the op call.** The verified ops already run in the faithful
  `ArrayStore::make_runnable`/`unqueue_ready` (B8C-2) and the kernel (B8C-3); what B8C-4 adds is an
  *independent* structural oracle (`ready_wf_exec`/`ready_complete_exec`) the tests check the ops
  *against* — and `*_has_teeth` proves the oracle is non-vacuous. Without teeth, an `assert!(wf)`
  that an empty queue always satisfies would be worthless.
- **`ready_wf` and `ready_complete` are kept distinct in the mirror**, exactly as in the proofs.
  `randomized_ready_sweep` asserts both (its model maintains completeness); `check_destroy_tcb`
  asserts `ready_wf` + an explicit "t off every chain" rather than full `ready_complete` (the op's
  promotion is what restores completeness — host-confirmed by the splice test's survivors check).
- **No trusted-base / spec / ledger change.** `#[cfg(test)]` only; the verus gate and the kernel
  build are untouched. No `external_body` / `assume_specification` added.

---

## 3. Gate state

| gate | B8C-3 (findings-7) | this pass (B8C-4) |
|---|---|---|
| `cargo verus verify -p kcore` | 374 / 0 | **374 / 0** (test-only change) |
| `cargo test -p kcore` | green (94) | **green (100)** (+6 ready-queue tests) |
| `cd kernel && cargo build` | green | green |
| QEMU boot | reaches `eunomia>` shell | unchanged |

**Phase B8C is complete.** The 32-level ready queue is verified in `kcore` (witnesses, four ops,
bitmap coherence, splice walks), integrated through the `make_runnable`/`unqueue_ready` seams that
`signal`/`fire`/the IPC fast path/the teardown SCC/`destroy_tcb` lean on, executed in production by
the rewired kernel scheduler (B8C-3), and host-exercised with teeth (B8C-4). The scheduler *policy*
(`maybe_switch`) and the asm context switch stay trusted shell per rev1§6.1(d). The audit §4.2
ready-queue item is closed end-to-end.
