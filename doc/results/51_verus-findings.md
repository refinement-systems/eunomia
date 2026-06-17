# Verus findings 31 — Phase 6d-final: `channel::destroy_channel`'s body proven, the channel arm of the cross-object SCC closed

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6d", the *6d-final* residue). Prior increment: `50` (6d body-removal — `delete`'s body
proven, the **cspace-only** cycle `delete → obj_unref → destroy_cspace → delete` closed; the two
cross-module destructor bodies `channel::destroy_channel` and `thread::destroy_tcb` recorded as the
residue, doc 50 §3). This increment closes the **channel** half of that residue:
`channel::destroy_channel`'s `#[verifier::external_body]` is **gone** and its real teardown body is
proven, closing the channel arm of the cross-object SCC `obj_unref → destroy_channel → delete →
obj_unref` under the shared lexicographic `decreases (count_nonempty(slot_view), height)` with
`destroy_channel` at **height 3**.

**Outcome — the recorded fallback again (plan §2 "6d": *attempt full, fall back, record*).**
`destroy_channel`'s body lands fully; `thread::destroy_tcb` **keeps** its (proven-`chan_struct_frame`-
bearing) `external_body` contract. `destroy_tcb`'s body needs a **new system invariant** beyond the
ones in hand — the bound cspace's `cspace_resident_wf` must enter `cap_consistent` — and a trial
strengthening rippled into a generic frame lemma *and* destabilized an unrelated timer proof's rlimit
(§3). Disproportionate to bundle here, so the **channel arm** is closed and the **thread arm** is the
recorded residue (6d-final-thread). `destroy_channel`'s removal is the load-bearing half: it was the
triple-nested ring-cap loop the plan flagged as the live hazard (`ring_cap`-stability through the
recursive `delete`s), now solved by the `chan_struct_frame` skeleton frame.

`cargo verus verify -p kcore`: **283 verified, 0 errors** (was 273 after doc 50's baseline). `cargo
test -p kcore`: **82 passed** (was 81; +1 the sound bound-channel differential). The aarch64 `kernel`
cross-build is unchanged (every change is ghost or a doc comment; confirmed `cd kernel && cargo build`).

---

## 1. What landed

- **`destroy_channel`'s `external_body` removed; the body is proven.** The real teardown — the
  triple ring-cap `delete` loop (every queued cap cleaned up by ordinary CDT teardown) then the
  per-event binding release — verifies against the full contract: `cspace_wf`, the four system
  invariants (`refcount_sound`/`caps_consistent`/`end_caps_sound`/`census_dom_complete`), the
  `count_nonempty` non-increase, `only_empties`, the `cspace_view` residency frame, the new
  `chan_struct_frame` skeleton frame, and **every ring-cap slot emptied**. The shared
  `decreases (count_nonempty(slot_view), 3int)` is accepted across the visible
  `obj_unref(4) → destroy_channel(3) → delete(0) → obj_unref` cycle.

- **`chan_struct_frame` — the channel-skeleton teardown frame (the increment's central new spec).**
  `chan_struct_frame(cv0, cvf) := cvf.dom() == cv0.dom() && ∀ch. cvf[ch].ring_cap == cv0[ch].ring_cap
  && cvf[ch].depth == cv0[ch].depth`. A channel's `ring_cap`/`depth` skeleton is fixed at
  construction and changed by *no* op (teardown clears `bindings`/`end_caps`, `send`/`recv` move
  head/count, but the layout never moves). This is exactly what lets `destroy_channel`'s ring-cap
  loop read `old.ring_cap[ch]` across the recursive `delete`s — the channel `ch` is not re-homed, its
  slot handles do not move, so the slots the loop empties **are** the slots the `ensures` quantifies
  over. Threaded through the six SCC members (`delete`/`obj_unref`/`destroy_cspace`/`unref_cspace`/
  `destroy_channel`/`destroy_tcb`); the non-recursive callees supply it for free —
  `endpoint_cap_dropped` (its `end_caps`-only insert) and `set_chan_binding` (its `bindings`-only
  insert) via the one lemma `lemma_chan_field_update_struct_frame`, and `destroy_notif`/`destroy_timer`
  via their whole-`chan_view` frame. `delete`'s body composes it from `delete_prepare`
  (reflexive) → `endpoint_cap_dropped` (its new `chan_struct_frame` ensures, **no `cap.kind`
  case-split** — see §2) → the frame-unmap branch (framed) → `obj_unref`, with one
  `lemma_chan_struct_frame_trans`.

- **`release_binding` — the binding-release census, quarantined (doc 25 §2 decomposition).** A
  non-recursive helper (not an SCC member — no `delete`) carrying `destroy_channel`'s heaviest single
  query: drop `refs[n]` then **clear the binding** (`set_chan_binding(.., None)`) so `binding_refs(n)`
  falls in lockstep — the census's answer to the "no clean closed form here" the §6a contract
  anticipated (`channel.rs:1199`). Its contract: `refcount_sound`/`caps_consistent`/`end_caps_sound`/
  `census_dom_complete` preserved, `slot_view`/`cspace_view` framed, `chan_view.dom` unchanged,
  `chan_struct_frame`. The recount reuses the already-landed `lemma_binding_drop`; the underflow gate
  (`refs[n] >= 1`) is discharged by the **new** `lemma_binding_refs_pos` (a binding naming `n`
  witnesses `binding_refs(n) >= 1`, the `lemma_slot_refs_positive` analog) + `lemma_in_refs_from_census`
  + `refcount_sound`.

- **The triple-nested ring-cap loop with lexicographic-prefix invariants.** Each of the three loops
  carries the four system invariants + `count`/`only_empties`/`cspace_view`/`chan_struct_frame` + a
  prefix-emptiness clause (completed rings, completed rows in the current ring, completed positions in
  the current row). The loop reads `chan_ring_cap(ch, ring, i, c) == rc[(ring,i,c)]` via the
  carried `chan_view()[ch].ring_cap == rc`, deletes if non-empty (the `delete` precondition met by the
  guard), and re-establishes the prefix from `only_empties` (prior empties stay empty) + `delete`'s
  `is_empty` (the just-handled slot). The outer invariant at `ring == 2` **is** the `ensures`.

- **The host differential strengthened.** The new `destroy_channel_bound_preserves_refcount_sound`
  test builds a genuinely `refcount_sound` bound channel (one binding to a notification whose only
  reference is that binding, empty rings, no endpoint caps), so `check_destroy_channel`'s
  `refcount_sound` assertion — *skipped* on the pre-existing unsound fixtures — actually fires; the
  existing test now also asserts each binding is **cleared**. `check_destroy_tcb` gained a
  `chans`-unchanged assertion (host-checking `destroy_tcb`'s new assumed `chan_struct_frame` ensures).

---

## 2. Findings worth keeping

- **A `cap.kind` case-split in a proof block destabilized an unrelated precondition.** The first cut
  established `delete`'s `chan_struct_frame` with `assert(..) by { if let CapKind::Channel(ch,_) =
  cap.kind { .. } }`. That case-split shifted Verus's auto-trigger selection and broke `delete`'s
  *long-standing* `obj_unref` Channel-arm `chan_wf` precondition (auto-derived from
  `cap_consistent`, "low confidence" triggers). The fix was to push the case analysis *out* of
  `delete`: give `endpoint_cap_dropped` its own `chan_struct_frame` ensures (trivial from its
  `end_caps`-only insert), so `delete` discharges the skeleton with a **plain** `assert` (the Channel
  path reads `endpoint_cap_dropped`'s ensures; every other path is reflexive). Lesson: a `by`-block
  that case-splits on a sum type can perturb auto-trigger choice elsewhere in the same body —
  prefer exposing the needed fact on the callee's contract over re-deriving it with a case-split.

- **The binding clear is a real (benign) body change the proof forces.** The model never removes a
  channel from `chan_view().dom()` (only `.insert` exists), so decrementing `refs[n]` *without*
  clearing the binding leaves `binding_refs(n)` over-counting — `refcount_sound(final)` would break.
  The proof therefore requires the body to clear each binding to `None` (a write to the dying channel
  object, the `destroy_tcb`-clears-`qnext` precedent). The pre-existing host test never caught the gap
  because its fixture was `refcount_sound`-unsound (so the `refcount_sound` assert was skipped) — the
  new sound test closes that.

- **`chan_struct_frame` is the minimal frame the ring loop needs — not a full `chan_view` frame.**
  Teardown legitimately mutates `bindings` (the release) and `end_caps` (a recursive
  `endpoint_cap_dropped`), and a recursive `destroy_channel` clears *another* channel's bindings — so
  "`delete` preserves all of `chan_view`" is false. Restricting the frame to the **immutable**
  skeleton (`ring_cap`/`depth`/dom) is what makes it both true and composable across the cross-module
  recursion.

---

## 3. The residue: `thread::destroy_tcb`'s body (6d-final-thread)

The thread arm of the SCC is **not** closed here. The waiter-census half is *easier* than doc 50 §3
feared — the proposed global waiter-coherence invariant is **not** needed: a state-keyed frame lemma
(`waiter_chain`'s clause 6 forces every chain node `BlockedNotif`, so an edit touching only
non-`BlockedNotif` threads — `destroy_tcb`'s `Runnable→Halted` transition — frames every
`waiter_refs(m)` regardless of a stale `wait_notif`) discharges it, with `remove_waiter`'s
`wait_notif`-cleared post-state covering the `BlockedNotif` entry case. The seam side is also
straightforward: `unqueue_ready` wants a **total-frame** `ExStore` contract (the ready queue is
scheduler state below `tcb_view`; the `make_runnable` precedent, host-checked against `ArrayStore`'s
no-op), and the body needs the documented **reorder** (clear `tcb.cspace`/`aspace` *before* the
`unref_cspace`/`unref_aspace`, so the cleared `thread_hold_refs` lands the off-by-one the unrefs
require).

The blocker is a **fifth system invariant**: `unref_cspace` requires `cspace_resident_wf(cs)` for the
thread's bound cspace (to drive the at-zero `destroy_cspace`), and the only sound source is
`cap_consistent`'s Thread arm — so `delete` (which deletes the Thread cap) supplies it from
`caps_consistent`. Strengthening `cap_consistent(Thread)` with `tcb[o].cspace matches Some(cs) ==>
cspace_resident_wf(cs)` is the right move, but the trial run rippled into the generic
`lemma_caps_consistent_frame` (it must now also frame the bound cspace across `signal`/`remove_waiter`
edits) **and** blew the rlimit on the unrelated `lemma_timer_remove_chain` — the trigger-fragility
doc 50 §3 anticipated for `destroy_tcb`. Closing it is the 6d-final-thread follow-up: strengthen
`cap_consistent(Thread)` + `lemma_caps_consistent_frame` (a `cspace`-frame hypothesis + a Thread case),
fix the callers, manage the trigger economy, then land the `unqueue_ready` contract + the state-keyed
waiter lemma + the `lemma_clear_thread_hold` off-by-one + `destroy_tcb`'s body (the entry-state
case-split + reorder).

`destroy_tcb` keeps its assumed-but-host-checked `external_body` contract (now also stating
`chan_struct_frame`, host-checked by `check_destroy_tcb`'s `chans`-unchanged assertion this
increment). The §6 spec-table goal "kcore carries zero `external_body`" is therefore **not yet met** —
`destroy_tcb` remains (plus the pre-existing `untyped.rs` helpers, out of 6d scope). The channel arm's
closure is the larger step toward it.

---

## 4. Doc / CLAUDE.md

No `CLAUDE.md`/spec edit this increment (the doc-30 §3 convention — the sub-phase closeout edit rides
6f). 6d-final's channel arm is now docs 50–51; the thread arm is the recorded 6d-final-thread residue.
`cargo verus verify -p kcore` runs with no per-proof filter, so the new `destroy_channel` body,
`release_binding`, and the `chan_struct_frame`/`lemma_binding_refs_pos`/`lemma_chan_field_update_struct_frame`
lemmas auto-gate; `host-tests` runs the strengthened `check_destroy_channel`/`check_destroy_tcb` and
the new sound bound-channel differential.
