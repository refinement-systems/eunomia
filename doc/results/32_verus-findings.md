# Verus findings 12 — Phase 4b: notification `signal`/`wait`/`destroy_notif`, the waiter-queue FIFO core

Plan: `doc/plans/3_verus-rewrite.md` (§4.4) and its decomposition
`doc/plans/3_verus-rewrite_phase4-detail.md` (§4b). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31` (phase 4a — the notification/TCB/timer ghost-view enabling refactor). This is
phase 4's **hardest** sub-phase: it drops `signal`'s phase-3 `external_body` and proves
the real body against the `waiter_seq` FIFO model — so **wake order = block order** is
now a theorem — and proves `wait` and `destroy_notif`.

**Outcome.** `cargo verus verify -p kcore`: **98 verified, 0 errors** (was 90 after 4a;
`+8` = the five new lemmas/spec-proofs `lemma_chain_eq_at`,
`lemma_chain_not_strict_prefix`, `lemma_waiter_chain_unique`, `lemma_drop_first_chain`,
`lemma_notif_wf_frame`, plus `wait`/`destroy_notif` graduating to proven; `signal` was
already counted but moves from `external_body` to a proven body). `cargo test -p kcore`:
**31 passed** (was 27 — `+wait_consume`, `+wait_signal_fifo`, `+destroy_notif_noop`,
`+binding_notif_wf_exec_has_teeth`). The aarch64 `kernel` cross-build is unchanged
(ghost erasure; confirmed `cd kernel && cargo build`).

---

## 1. What closed

- **`signal` is proven** (graduates from `external_body`, doc 27 §1 / doc 30 §4 — *the*
  piece phase 3 explicitly deferred). The `slot_view`/`chan_view`-unchanged frame is
  *retained* (additive on the `ensures` side, so the phase-3 callers' use of the result
  is undisturbed); the body now also proves: `notif_wf(n)` preserved; on the
  **accumulate** path the word grows and the queue/refs/TCBs are untouched; on the
  **wake** path the head waiter is dequeued (`waiter_seq` loses its head via
  `Seq::drop_first`), receives the whole accumulated word, the word clears, the queued
  ref is released (`refs -= 1`), and the thread is made `Runnable`. New preconditions:
  the notification is live + `notif_wf`, and a queued waiter ⇒ `refs > 0`.
- **`wait` is proven.** Consume path (nonzero word ⇒ returns it, clears it, queue/refs
  untouched) and block path (`cur` appended at the tail — `waiter_seq` grows by
  `Seq::push`, the block-order half of the FIFO theorem — marked `BlockedNotif`, one ref
  acquired). The acquire (`wait`) and release (`signal`) are the first installments of
  `refcount_sound`'s **waiter term** as per-op deltas (the 3e `bind`-delta template).
- **`destroy_notif` is proven** — a no-op, requiring `wait_head is None` directly
  (see §2).
- **The `waiter_seq` uniqueness theorem** (`lemma_waiter_chain_unique`), the central new
  lemma: two `waiter_chain` witnesses for the same notification are equal (heads agree;
  `qnext` threads each successor; a strict prefix's last node would need
  `qnext == None` by its own chain yet `Some(·)` by the longer — contradiction). Because
  `waiter_seq` is a `choose`, this is what makes the op effects expressible as
  `waiter_seq` **equalities** (`drop_first` / `push`) rather than mere existence — i.e.
  what makes "wake order = block order" a clean theorem.
- **The named binding invariant `binding_notif_wf`** (the resolution of the
  precondition-discharge problem, §2) — every bound endpoint event names a live,
  `notif_wf` notification — threaded through the fire-callers `fire`/`send`/`recv`/
  `endpoint_cap_dropped` in **both `requires` and `ensures`** (require-and-preserve,
  paralleling `chan_wf`), plus the precondition-only `binding_refs_ok` waiter-delta
  clause. `fire` discharges `signal`'s structural preconditions from it and re-establishes
  it after the wake via `lemma_notif_wf_frame`.
- **`slot_move` strengthened** to frame `notif_view`/`tcb_view`/`timer_view`/
  `timer_head_view` unchanged (additive; its body mutates only via `set_slot`, which 4a
  made frame them). This is the one place 4a's wide cross-frame refactor reached a phase-2
  op rather than a setter — needed so `binding_notif_wf` survives a queued-cap move inside
  `send`/`recv`.

## 2. Design decisions worth keeping

- **The plan's "callers unaffected" non-goal was infeasible; the named binding invariant
  is the sound resolution.** Proving `signal`'s body *forces* it to gain preconditions
  (it walks and dequeues the waiter queue, so it needs the queue well-formed; and the
  wake-release `-1` needs `refs > 0`). Its only verified caller is `channel::fire`
  (reached from `send`/`recv`/`endpoint_cap_dropped`), and no binding-liveness invariant
  existed. The resolution (chosen deliberately over threading bare preconditions or
  deferring `signal`'s body): a standalone `binding_notif_wf`, a require-and-preserve
  companion to `chan_wf`, pre-staging the refcount census. **Structural only** (`nv`
  domain + `notif_wf`, no `refs` clause) — which is exactly what makes it *preservable*
  across a fire.
- **Why `refs > 0` is precondition-only (`binding_refs_ok`), not part of the preserved
  invariant.** It is the waiter term of the refcount census: a bound notification *does*
  have `refs > 0` (the binding holds a ref), but proving that survives `signal`'s wake
  `-1` needs `refs ≥ 1 + |waiters|`, i.e. the full census — deferred to the post-phase-5
  teardown phase (plan §1.4). So it rides as a per-call precondition the trusted kernel
  shell supplies, never a preserved postcondition.
- **Cross-notification preservation rests on "a TCB is on at most one queue."**
  `fire`/`signal` perturbs one TCB `t` (the woken head, whose old `wait_notif == Some(n)`);
  any *other* bound notification `m`'s chain nodes all name `m ≠ n`, so none is `t`, so
  `notif_wf(m)` is untouched. `lemma_notif_wf_frame` packages exactly this (`notif_wf(m)`
  survives any edit leaving `m`'s view and every `wait_notif == Some(m)` TCB fixed), and
  `signal` exposes the frame fact it needs (`∀k. old.wait_notif[k] ≠ Some(n) ⇒ tcb[k]
  unchanged`) so the caller never case-splits on whether the signal woke anyone.
- **`destroy_notif` requires the empty queue directly, not "`refs == 0` ⇒ empty".** The
  plan suggested deriving emptiness from `notif_wf`, but `notif_wf` is *structural* and
  carries no refcount fact (a waiter holding a ref is the census, deferred). Requiring
  `wait_head is None` — exactly what the production `debug_assert` checks — is the honest
  scoped contract.
- **Decomposition beats an rlimit bump (the doc 25 §2 discipline).** `signal`'s wake-path
  chain construction blew the solver rlimit once the `∀k` frame ensures was added;
  extracting it into `lemma_drop_first_chain` (the head-pop ↔ `drop_first` correspondence,
  given the head/tail re-point and the single-TCB edit) brought `signal`'s own query back
  under budget — no `#[verifier::rlimit]` needed, matching the project's no-rlimit-bump
  convention.
- **Loop invariants carry the new frames.** `send`/`recv`'s cap-move loops and
  `slot_move`'s children walk each had to add `notif_view`/`tcb_view == old` to their
  invariants (the frame is otherwise havocked across the loop) — the small,
  mechanical tax of threading a state-wide invariant past a loop, the same shape 4a paid
  for the setters.

## 3. Doc-numbering note

Phase 4 produces docs 31–35 ("findings 11–15"), numbered in landing order (the doc-29/30
convention). This is 4b, the second to land, so doc 32 / findings 12.

## 4. What's next (4c)

`remove_waiter` — the mid-queue unlink/splice (the `cdt_unlink` analog, doc 25, but
singly-linked with no re-parenting). It walks the queue with a loop (`decreases` on the
4a list-acyclicity rank), removes a named element (`waiter_seq` splice), fixes the tail,
and releases the waiter's ref — building on the settled `waiter_chain`/`waiter_seq`
representation and the uniqueness lemma landed here.
