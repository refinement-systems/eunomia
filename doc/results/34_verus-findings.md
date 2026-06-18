# Verus findings 14 — Phase 4d: thread `report_terminal` (ReportMonotone + FireSafe) + `bind`

Plan: `doc/plans/3_verus-rewrite.md` (§4.4) and its decomposition
`doc/plans/3_verus-rewrite_phase4-detail.md` (§4d). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31` (phase 4a — the notification/TCB/timer ghost-view refactor), `32` (phase 4b —
`signal`/`wait`/`destroy_notif`), `33` (phase 4c — `remove_waiter`). This is the
**fourth** of phase 4's five sub-phases: the §5.1 thread/report obligations, now that
`signal` (4b) is proven. It closes the two named §4.4 properties — **ReportMonotone**
and **FireSafe** — and proves `thread::bind`.

**Outcome.** `cargo verus verify -p kcore`: **105 verified, 0 errors** (was 101 after 4c;
`+4` = `report_terminal` and `bind` graduating from plain Rust to proven bodies, plus the
new `cap_notif` spec fn and the `BIND_EXIT`/`BIND_FAULT` consts entering the `verus!{}`
module). `cargo test -p kcore`: **38 passed** (was 32 — `+delete_notif_frame`,
`+thread_bind_install_rebind_unbind`, `+report_terminal_first_call_wins_and_fires`,
`+report_terminal_fault_arm_fires_fault_binding`, `+report_terminal_accumulate_no_waiter`,
`+report_terminal_firesafe_empty_slot`). The aarch64 `kernel` cross-build is unchanged
(ghost erasure; confirmed `cd kernel && cargo build`). No new lemmas — both proofs are
straight-line over the 4a/4b seam, reusing `lemma_notif_wf_frame` (4b).

---

## 1. What closed

- **`report_terminal` is proven** (`thread.rs`), moved into a `verus!{}` block — the two
  §5.1 properties:
  - **ReportMonotone** — the `report != Running` guard makes the contract a clean
    no-op/transition split: an already-terminal report ⇒ the store is untouched
    (`refs`/`notif`/`tcb`/`slot`/`chan`/`timer` all `== old`); a `Running` report ⇒
    `final.tcb_view()[t].report == r`. "At most one transition, terminal absorbing" is the
    corollary — any later call hits the no-op path. `report_terminal_first_call_wins_and_fires`
    exercises both (record-then-fire, then a second-call no-op).
  - **FireSafe** — *not a separate `ensures`*; it is discharged **by the body verifying at
    all**. The empty-slot path fires nothing (the `if let Notification` fails — the
    revoke-raced-the-death case, `report_terminal_firesafe_empty_slot`); on the
    notification-cap path the `requires` (a cap-in-slot at the bind slot designates a live,
    `notif_wf` notification with `wait_head is Some ⇒ refs > 0`) **is** `signal`'s
    precondition set, so the fired object is provably live, never freed memory. This is the
    first cspace-slot installment of `refcount_sound`'s census — scoped to the local
    cap-liveness fact (the 3e per-op-delta precedent), not the full census.
- **`thread::bind` is proven** (`thread.rs`) — the §3.6 TCB-binding config. Contract:
  `cspace_wf` preserved; `tcb_view` changes only `bind_bits[which]`; the bind slot ends
  holding the moved cap (or empty on a `None` src) with `src` emptied; the object views are
  framed. Composes the §4d-strengthened `delete` (§2.1) + the verified `slot_move`.
- **`delete`'s contract strengthened** (`cspace.rs`) with an **additive,
  conditional-on-notification-cap frame** (§2.1) — the enabling change for `bind`,
  host-test-checked (`check_delete_notif`).
- **`signal`'s contract strengthened** (`notification.rs`) with `timer_view`/
  `timer_head_view == old` frames (§2.2) — additive, no body change, so `report_terminal`
  (which fires `signal` and otherwise touches no timer) can frame timers across the wake.

All four are **host-test-checked** against the real `ArrayStore` bodies (`test_store.rs`):
the notification-cap delete frame; the TCB bind install/rebind/unbind with the move's
net-zero refs; first-call-wins + the fault arm + the accumulate path + the empty-slot
firesafe path.

## 2. Design decisions / mechanics worth keeping

### 2.1 `delete`'s "the external_body contract is sufficient" was too optimistic — the conditional-notification frame is the load-bearing correction

The detail plan §4d expected `bind` to delete the displaced cap with `delete`'s existing
contract. It can't: `delete` (external_body) ensures only `cspace_wf` + `slot_view` domain
+ slot emptied + `count_nonempty` drop — **silent on `tcb_view`, `refs_view`, the other
object views, and every other slot's `.cap`.** So after the `delete`, `bind` cannot even
discharge `set_tcb_bind_bits`'s `tcb_view().dom().contains(t)`, nor `slot_move`'s "`src`
still non-empty." The resolution (the doc-30 §2.1 conditional-frame + §2.3 robust-core
discipline): a bind slot only ever holds a **notification** cap, and deleting one is
robustly clean — `cdt_unlink`/`set_slot` frame every object view, the `Channel`/mapped-
`Frame` teardown branches don't fire, and `obj_unref` only drops `refs[n]` (and at zero
calls the no-op `destroy_notif`). So `delete` gains:

> `cap_notif(old.slot[slot].cap) is Some ⇒` `tcb_view`/`chan_view`/`notif_view`/`timer_view`/
> `timer_head_view == old` **and** every `x != slot` keeps its `.cap`.

**Additive** (existing callers — `revoke`, `destroy_cspace` — ignore it, so phases 2–3 stay
green) and **host-checked** (`check_delete_notif`, mandatory: the clause is *assumed*, so the
real body must witness it). `refs_view` is deliberately left out — the `refs[n] -= 1` rides
the host test, not `bind`'s verified contract, exactly as `destroy_channel`'s per-binding
release did (doc 30 §2.3).

### 2.2 `signal` strengthened with the timer frame — the cheap additive fix

`signal`'s 4b contract framed `slot_view`/`chan_view`/`notif_view`/`tcb_view`/`refs_view`
but **not** `timer_view`/`timer_head_view` (no 4b caller needed them). `report_terminal`'s
"timers untouched" frame can't be proven through a `signal` call without it. Since every
setter in `signal`'s body already frames the timer views (and `make_runnable` does too),
adding the two `ensures` clauses verifies with **no body change** — the additive-strengthening
template (doc 27 §1), here on a *proven* function. It also pre-pays 4e (`check_expired`
calls `signal` and will want the same frame).

### 2.3 FireSafe is "the call site type-checks," not an `ensures`

The honest Verus encoding of "a terminal fire reads an empty slot or a live notification,
never freed memory" is **the fact that `report_terminal`'s body verifies**: the only fire is
`signal`, and its precondition `notif_wf(n) ∧ (wait_head ⇒ refs>0)` *is* "n is a live,
well-formed object." Stating a separate `ensures` would be redundant. The `requires` carries
the obligation; the proof carries the guarantee.

### 2.4 The dying thread is provably not the woken waiter (so its report survives the fire)

On the transition path `signal` may perturb the woken waiter's `tcb_view` entry, threatening
`final.tcb_view()[t].report == r`. The `requires` clause `tcb_view()[t].wait_notif != Some(nn)`
is what closes it: `set_tcb_report` preserves `wait_notif`, so at the `signal` call `t` still
isn't waiting on `n`; `signal`'s `forall k. wait_notif[k] != Some(n) ⇒ final[k] == old[k]`
frame then leaves `t`'s entry **entirely** untouched, report included. (`signal` never writes
any thread's `report` field, but its *contract* doesn't promise that for the woken thread —
the "`t` ≠ woken" route is the one that closes.) The same clause feeds `lemma_notif_wf_frame`
to carry `notif_wf(n)` across the `set_tcb_report` so `signal`'s own precondition holds.

### 2.5 `bind`'s move is net-zero — unlike `channel::bind`'s `+1`

`thread::bind` is the §4d analog of `channel::bind` (3e) in *shape* (release old / install
new / set bits), but the refs arithmetic differs: the TCB slots are CDT-visible cap slots, so
the new cap **moves** in via `slot_move` (a move, not a copy — no `+1`); only the displaced
notification is released (the `delete`'s `-1`). `channel::bind`'s binding is refcount-based,
so it did old `-1`, new `+1` (`bind_refs_post`). `check_thread_bind` asserts the net-zero
move; the verified contract omits refs entirely (it reads off `delete`, which omits them).

### 2.6 Mechanics: consts into `verus!{}`; the `cspace::` path for erased spec fns; `cap_notif`

- `BIND_EXIT`/`BIND_FAULT` had to **move inside the `verus!{}` block** — a const declared
  outside the macro is "ignored," uncallable from spec (the `report_terminal` `requires`
  computes `which` from them). They erase to ordinary `pub const`s, so `destroy_tcb` (plain
  Rust) and the kernel crate still see them.
- The spec fns `cspace_wf`/`is_empty_cap` are **erased in a normal `cargo build`/`test`**, so
  a top-level `use crate::cspace::{cspace_wf, …}` breaks the host build. They are referenced
  via the `cspace::` path *inside* the `verus!{}` block instead (the doc-26 §2.3 idiom that
  `notification.rs` already uses).
- A new `spec fn cap_notif(c) -> Option<ObjId>` (`Some` only for `Notification`) — narrower
  than `cap_obj` (any object cap) — is the projection both the `delete` frame guard and the
  `report_terminal`/`bind` contracts speak, via the `… matches Some(nn) ⇒` binding sugar.

## 3. Doc-numbering note

Phase 4 produces docs 31–35 ("findings 11–15"), numbered in landing order (the doc-29/30
convention). This is 4d, the fourth to land, so doc 34 / findings 14.

## 4. What's next (4e)

Timer (`arm`/`disarm`/`check_expired`/`destroy_timer`) — the armed-list + notif-ref coupling
(the armed-timer term of `refcount_sound`); `destroy_tcb` kept `external_body` with a
host-checked structural contract (the declared scope-out); and the phase-4 documentation
closeout — `CLAUDE.md`'s `### Verus` section + the §6 tier table move
`signal`/`wait`/`remove_waiter`/`destroy_notif`, `report_terminal`/`bind`, and the timer ops
onto the **proven** list, record `destroy_tcb` as the new host-checked `external_body`, and
flag the recommended cross-object-teardown phase after phase 5.
