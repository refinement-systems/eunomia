# Kani verification findings — part 4 (§4.4 notification + thread reports)

Continuation of `doc/results/2_kani-findings.md` (§4.1),
`3_kani-findings-2.md` (§4.2) and `4_kani-findings-3.md` (§4.3) for the
notification + thread-report suite (plan `doc/plans/0_kani-rewrite.md` §4.4).
Harnesses live in `kcore/src/proofs/notification.rs` and
`kcore/src/proofs/thread.rs` under `#[cfg(kani)]` and run with the rest of the
suite via `cargo kani -p kcore` (CI job `kani`, pinned cargo-kani **0.67.0**).
The standing caveat, the bounds policy, and the design notes (DN-1…DN-5) of
parts 1–3 apply unchanged; only what is *new* to §4.4 is recorded here.

## Standing caveat (unchanged)

**Every result here is bounded.** These harnesses use stack-allocated objects
at the TLC scope: a notification, up to **3 waiter TCBs** (one more than
`World`'s `NTHREADS = 2`, because the FIFO-unlink case needs a *middle*
element — see DN-6), and a `GhostEnv`. The report-monotonicity harness runs
`K = 3` repeated terminal calls (`= bounds::K_STEPS`). Scaling any of these is
a one-line change.

## What §4.4 verifies

| Harness | Property | Plan row |
|---|---|---|
| `check_signal_wait` | signal ORs bits; a no-waiter signal accumulates; `wait` on a nonzero word consumes it without blocking; a blocked waiter is woken with the whole word (cleared), its ref released, one `make_runnable` recorded | row 1 |
| `check_waiter_fifo` | three waiters woken in block order (witnessed by the ghost `make_runnable` log via `ordered_before`); notification refcount exact through each block/wake | row 2 |
| `check_remove_waiter` | unlink head / middle / tail (incl. the `wait_tail` fixup) relinks the queue in original order, nulls the removed TCB's links, releases one ref; removing a non-queued thread is a no-op | row 3 |
| `check_report_monotone` | TLA `ReportMonotone`: over any sequence of terminal `report_terminal` calls the report leaves `Running` at most once and is then fixed (absorbing); the bound notification fires ≤ once | row 4 |
| `check_bind_fire_safe` | TLA `FireSafe`: `report_terminal` only ever reads a binding slot that is empty (no-op) or holds a notification cap whose ref keeps the object live through the fire — never a freed object | row 5 |
| `check_thread_teardown` | a thread blocked on a notification is unlinked and its ref released, halted, and **no report is produced** (§5.1: destruction is the parent acting) | row 6 |

All six verify. No defects found — every property held on the real code at the
stated bounds.

## Design / engineering notes new to §4.4

- **DN-6 — standalone TCBs, not `World`, for the waiter harnesses.** The FIFO
  and unlink harnesses need three waiters (a head, a *middle*, and a tail) but
  `World` carries only `NTHREADS = 2`. Rather than raise `bounds.rs` (which
  would enlarge `TOTAL_SLOTS` and slow every World-based census harness), these
  harnesses allocate standalone `Tcb`s on the stack plus a standalone
  `NotifObj` and a `GhostEnv`, and read liveness straight off `(*n).hdr.refs`
  instead of the 28-slot census — the §4.3 "scope the harness to the objects it
  touches" lesson. Notification refcounting here is the **DN-1 family**:
  `signal`'s wake release and `remove_waiter`'s unlink both drop `hdr.refs`
  with a bare `-= 1` and no teardown at zero (a notification with a blocked
  waiter can't reach zero anyway, and the backing memory returns via `revoke`),
  so the harnesses assert exact counts, not destruction-at-zero.

- **DN-4 reappears in `destroy_tcb` — bind-cap teardown is scoped out of the
  Kani proof.** `destroy_tcb` deletes its on-exit/on-fault binding caps via
  `cspace::delete`, which dispatches through `obj_unref`. As recorded in DN-4,
  CBMC does not constant-fold the cap kind read from the slot, so it explores
  *every* `obj_unref` arm — including `destroy_cspace`'s loop over a symbolic
  `num_slots`, which is effectively unbounded. A first cut of
  `check_thread_teardown` that gave the dying thread a bound notification cap
  hit exactly this wall: ~17 min and an unwinding-assertion failure on an
  infeasible recursive arm. The harness therefore tears down a thread with
  **empty** binding slots (and null cspace/aspace), so `destroy_tcb` invokes no
  `obj_unref` and stays tractable (~3 s). The bind-cap delete it also performs
  is a `delete` of a notification cap — already proven by the §4.1 delete
  harnesses — and the full reclaim path is exercised end-to-end by
  `scripts/spawn-test.sh` in QEMU. The harness's novel obligations (the
  notification-waiter unlink and the no-report rule) are fully proven.

## Findings

None. `signal`/`wait`/`remove_waiter` preserved the FIFO and refcount
discipline, `report_terminal` was monotone and fire-safe, and `destroy_tcb`
unlinked the waiter and produced no report — all at the TLC bounds.

| ID | Date | Harness | Bounds | Severity | Description | Status |
|----|------|---------|--------|----------|-------------|--------|
| —  | —    | —       | —      | —        | (no defects found) | — |

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Measured on the dev machine (cargo-kani 0.67.0).

| Harness | Bounds | Time |
|---------|--------|------|
| `check_signal_wait` | 1 notif + 1 TCB | ~3.7 s |
| `check_waiter_fifo` | 1 notif + 3 TCBs | ~11.6 s |
| `check_remove_waiter` | 1 notif + 4 TCBs (nondet victim) | ~2.2 s |
| `check_report_monotone` | 1 notif + 2 TCBs, K=3 nondet reports | ~4.5 s |
| `check_bind_fire_safe` | 1 notif + 1 TCB, nondet report + slot state | ~1.1 s |
| `check_thread_teardown` | 1 notif + 1 TCB, empty bind slots | ~3.2 s |

`check_waiter_fifo` dominates (three blocked waiters plus the ghost-log
ordering walk); all six are comfortably inside the ≤5-min per-harness budget.
The intractable variant of `check_thread_teardown` (bind cap held) is the DN-4
worked example above — kept out of the Kani proof, not silently reduced.
