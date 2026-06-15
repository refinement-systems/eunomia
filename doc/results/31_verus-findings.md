# Verus findings 11 — Phase 4a: the notification/TCB/timer ghost-view enabling refactor

Plan: `doc/plans/3_verus-rewrite.md` (§4.4 notification + thread/reports) and its
detailed decomposition `doc/plans/3_verus-rewrite_phase4-detail.md` (§4a). Prior
increments: `21`…`25` (phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — the
untyped remainder + channel). This is the **first** of phase 4's five sub-phases:
the *foundational, proof-light* refactor that extends the `Store`/`ExStore` seam
with three ghost object views (`notif_view`/`tcb_view`/`timer_view`) so 4b
(`signal`/`wait`), 4c (`remove_waiter`), 4d (`report_terminal`/`bind`), and 4e
(timer + closeout) build on a settled representation. Its risk was in the *design*
of the three views and the **waiter-queue model**, not solver time — by intent
(detail §2-4a). It is the direct analog of phase 3's 3b (the `chan_view`
introduction, doc 27).

**Outcome.** `cargo verus verify -p kcore`: **90 verified, 0 errors** (unchanged —
4a lands no new verified operation; the additions are ghost `spec fn`s + assumed
trait `ensures`, and the phase-2/3 proofs stayed green untouched under the additive
cross-frame). `cargo test -p kcore`: **27 passed** (was 26 — `+notif_wf_exec_has_teeth`).
The aarch64 `kernel` cross-build is unchanged (ghost code erases; the three views,
the `notif_wf`/`waiter_seq` predicates, and the new accessor contracts are
spec-only, so the production `KernelStore` needs no change — the doc 27 §1 result,
repeated). One new `external_trait` boundary (`make_runnable`), host-test-checked.
No new SMT-heavy lemmas — the obligations are straight-line frames over the seam.

---

## 1. What closed

- **The three object views.** `ExStore` (cspace.rs) gains `spec fn notif_view`
  (`Map<ObjId, NotifView>`), `spec fn tcb_view` (`Map<ObjId, TcbView>`), and
  `spec fn timer_view` (`Map<ObjId, TimerView>`) alongside `slot_view`/`refs_view`/
  `chan_view`, plus a `spec fn timer_head_view() -> Option<ObjId>` scalar (the
  armed-timer list head, a `Store`-seam static). Each view struct mirrors an
  object's *mutable* fields (`hdr.refs` already lives in `refs_view`):
  `NotifView { word, wait_head, wait_tail }`; `TcbView { state, qnext, wait_notif,
  report, retval, cspace, aspace, bind_bits, bind_slots }`; `TimerView { armed,
  deadline, notif, bits, next }`. Each notif/tcb/timer accessor (`store.rs:76–131`)
  gets `requires/ensures` relating it to its view exactly as `slot`/`set_slot`
  relate to `slot_view`: getters project a field; setters update one key and frame
  the *other* five views + the scalar unchanged.
- **The view cross-framing.** The existing setters (`set_slot`, `set_obj_refs`, the
  five `set_chan_*`, `chan_msg_write`) each gained four `ensures` clauses —
  `notif_view`/`tcb_view`/`timer_view`/`timer_head_view` `== old`. **Purely
  additive** (an extra `ensures` on a callee only adds facts at call sites), so the
  phase-2/3 proofs are undisturbed (90 verified, unchanged) — the mutual-frame
  discipline doc 27 §1 established, extended from a three-view world to a six-view-
  plus-scalar one. This is the bulk of the diff and the thing that lets a §4.4 op
  reason about one view without re-establishing the rest.
- **`ThreadState`/`Report` type-specs.** Both plain-Rust `Copy` enums
  (`crate::thread`) gained `#[verifier::external_type_specification]` +
  `#[verifier::ext_equal]` (joining the eight `Ex*` specs), so they live in
  `TcbView` and compare with structural `==` — the `ExChanEnd` pattern.
- **`spec fn notif_wf` + `spec fn waiter_seq` + the shared list-rank** — the central
  new machinery (see §2). Defined for 4b/4c, exercised by `notif_wf_exec`; no op
  *proves* `notif_wf` preserved in 4a (the `chan_wf` discipline, doc 27 §1).
- **The assumed `make_runnable` contract** — the single new trusted boundary phase 4
  adds. `ExStore`'s `make_runnable(t)` now `ensures` the woken thread's
  `tcb_view[t].state` becomes `Runnable` with **every other view and every other TCB
  field framed unchanged**. The frame `signal`'s body proof (4b) needs across the
  wake; host-test-checked against `ArrayStore` (the `set_slot`/assumed-`signal`
  discipline). `unqueue_ready` waits for 4e; the aspace/TLB/barrier seam waits for
  phase 5.
- **`ArrayStore` real notif/TCB/timer state.** Every `unimplemented!()` notif/tcb/
  timer accessor (`test_store.rs`) is replaced by a field read/write over extended
  `TcbState` (+`state`/`report`/`cspace`/`aspace`/`bind_bits`/`bind_slots`), a new
  `TimerState`, and a `timer_armed_head` scalar; `make_runnable` is now the faithful
  "set state Runnable" (was a no-op). `notif_wf_exec` mirrors `notif_wf` (the
  `chan_wf_exec` plain-Rust re-expression), and `notif_wf_exec_has_teeth` rejects one
  shape per clause (head/tail disagreement, a `qnext` cycle, a waiter naming the
  wrong notification, a non-`BlockedNotif` waiter, a `wait_tail` off the chain end, a
  charted node with no live TCB) — so 4b/4c's `notif_wf` precondition is non-vacuous.

## 2. Design decisions worth keeping

- **The waiter queue is modeled as an explicit FIFO `Seq` witness.** The queue is a
  **singly-linked** intrusive list (`NotifView.wait_head/tail` + per-TCB `qnext`),
  with **no back-pointer** — so the CDT's doubly-consistent membership trick (what
  pins each sibling to its parent's child list) does not apply. The clean model is
  `waiter_chain(nv, tv, n, ws)`: a `Seq<ObjId> ws` with distinct elements (the index
  *is* the acyclicity rank), head/tail agreeing with `ws`'s ends, `qnext` threading
  `ws[i] → ws[i+1]` (and the last to `None`), and every charted node naming `n` and
  `BlockedNotif`. `notif_wf` asserts such a witness exists; `waiter_seq := choose|ws|
  waiter_chain(…)` is the unique FIFO order under it. This is the direct analog of
  3d's `ring_fifo` `Seq` — and **lighter than 3d** (no modular arithmetic; a linked
  list, not a ring). `wait` ⇒ `Seq::push`, `signal` ⇒ `Seq::drop_first`,
  `remove_waiter` ⇒ a splice — the push/pop/splice lemmas land in 4b/4c (the doc 29
  §1 "add the clause when the op pays for it" discipline; the exact clause set may
  gain a clause when 4b first consumes it).
- **One generic list-acyclicity rank, shared with the timer.** `valid_list_rank(succ:
  Map<ObjId, Option<ObjId>>, r)` / `list_acyclic(succ)` is the `valid_srank`/
  `sib_acyclic` analog over an **abstract successor map** — instantiated for the
  waiter queue (`succ = qnext`) in 4a and reusable for the armed-timer list (`succ =
  timer_next`) in 4e, rather than duplicating the rank twice. Over the `qnext`
  projection it is *implied* by `waiter_chain`'s `no_duplicates` (rank = chain
  position), so `notif_wf` need not assert it separately; it is the `decreases`
  mechanism 4c/4e instantiate. (This resolves the detail-plan §4a "decide in 4a"
  question; fallback if the generic map proves awkward under the SMT encoding:
  specialize a `valid_qrank` mirroring `valid_srank` verbatim and add a separate
  timer rank in 4e.)
- **Bitwise/comparison fields are `u64`, not `nat`.** `NotifView.word`,
  `TcbView.retval`/`bind_bits`, `TimerView.bits`/`deadline` are `u64` so 4b's
  `word | bits` (a bitvector op) and 4e's `deadline <= now` are expressible directly
  — the one deliberate departure from `ChanView`'s all-`nat` choice, justified by the
  semantics those ops need. `ChanView`'s `nat` counts are unchanged.
- **`bind_slots` is an immutable handle projection.** `TcbView.bind_slots` is a
  length-2 `Seq<SlotId>` of slot *handles* (the seam has `tcb_bind_slot` as a getter
  with no setter); the cap *contents* stay in the single `slot_view` arena, exactly
  as `ChanView.ring_cap` does — so the §5.1 TCB binding caps remain revoke-visible
  through the arena (4d reads off this).
- **The `make_runnable` modeling boundary.** The production ready queue physically
  reuses the TCB `qnext` field, but the ready queue is *scheduler* state below the
  abstract `tcb_view` (the asm/scheduler shell is the trusted base, master plan §2).
  A thread is off every kcore queue once Runnable — `signal` sets `qnext = None`
  before calling `make_runnable` (`notification.rs:76`) — so modeling `make_runnable`
  as "state → Runnable, all else fixed" is faithful at the abstraction `kcore`
  reasons over. `ArrayStore::make_runnable` implements exactly that, and the
  strengthened `signal_frame` test asserts the woken waiter is `Runnable` — the
  executable check of the new contract.

## 3. Doc-numbering note

Phase 4 produces docs 31–35 ("findings 11–15"), one per sub-phase, **numbered in
landing order** (the doc-29/30 convention: the file number follows landing, not plan
order). This is 4a, the first to land, so it is doc 31 / findings 11.

## 4. What's next (4b)

`signal`/`wait` (+ `destroy_notif`) — the waiter-queue FIFO core, phase 4's hardest
sub-phase: drop `signal`'s `external_body` and prove the real body against the
strengthened (additive) frame + the `waiter_seq` push/pop lemmas, and prove `wait`'s
consume/block paths with the `refcount_sound` waiter-term acquire. The
representation those proofs build on is now settled here.
