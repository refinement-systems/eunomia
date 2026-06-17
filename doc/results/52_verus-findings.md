# Verus findings 32 — Phase 6d-final-thread: the `destroy_tcb`-body foundations (the 5th + 6th system invariants and their frame lemmas), body deferred on a `tcb_view` teardown frame

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6d", the *6d-final-thread* residue named in doc 51 §3). Prior increment: `51`
(6d-final — `channel::destroy_channel`'s body proven, the channel arm of the cross-object SCC
closed; `thread::destroy_tcb` recorded as the residue). This increment lands **every
foundation `destroy_tcb`'s body needs** — the two new system invariants, four new frame lemmas,
a strengthened `remove_waiter`, the `unqueue_ready` seam contract, and the full precondition
threading into `destroy_tcb`'s contract — all proven. **`destroy_tcb` itself keeps its
`external_body` contract**: closing its body needs one more piece (a `tcb_view` frame through
the teardown recursion, §3) whose design is solved and recorded here but whose threading is a
follow-on, disproportionate to bundle (the standing "attempt full, fall back, record"
discipline; the doc 50→51 cadence).

`cargo verus verify -p kcore`: **289 verified, 0 errors** (was 283 after doc 51's baseline).
`cargo test -p kcore`: **82 passed** (unchanged). The aarch64 `kernel` cross-build is unchanged
(every change is ghost or a contract/comment; confirmed `cd kernel && cargo build`).

---

## 1. What landed

The reason doc 51 §3 stopped at the channel arm was the **fifth system invariant**:
`destroy_tcb`'s `unref_cspace` needs `cspace_resident_wf(cs)` for the bound cspace, sourced
through `delete` from `caps_consistent`. Standing it up uncovered a **sixth** invariant
(`destroy_tcb`'s `remove_waiter` needs `notif_wf(wn)` for a blocked thread's notification). Both
are now in `caps_consistent`, framed, and threaded; the body's local frame lemmas are proven.

- **The fifth system invariant — `cspace_resident_wf` in `cap_consistent`'s Thread arm**
  (`cspace.rs`). A live `Thread(o)` cap now witnesses `tcb[o].cspace matches Some(cs) ==>
  cspace_resident_wf(store, cs)`. Refs-free (reads `cspace_view` + `slot_view` dom + `tcb_view`),
  so the `dec_ref` `-1` preserves it by framing. `delete` extracts it for the deleted Thread cap
  and threads it (it survives `delete_prepare`'s `cspace_view`/`tcb_view`/dom frame) to
  `obj_unref`'s Thread arm and on to `destroy_tcb`'s contract — exactly the provenance doc 51 §3
  named ("only the live Thread cap can witness it; by the time the destructor runs the TCB's own
  cap is gone").

- **The sixth system invariant — waiter-coherence in `cap_consistent`'s Thread arm**
  (`cspace.rs`). `tcb[o].state == BlockedNotif && tcb[o].wait_notif matches Some(wn) ==>
  notif_wf(notif_view, tcb_view, wn)`. This is the precondition `destroy_tcb`'s BlockedNotif
  branch's `remove_waiter(wn, t)` needs (its refs side-condition then rides `refcount_sound` +
  `census_dom_complete`). The **`notif_wf`-only** form (no chain *membership*) is what makes it
  framable (§2): `destroy_notif` is a model view no-op (it never removes a notification from
  `notif_view`), so the only edits to worry about are signal-shaped, which touch only off-chain
  threads.

- **`lemma_caps_consistent_frame` extended** (`cspace.rs`) with two hypotheses and two Thread
  sub-proofs, and its callers (`fire`, the strengthened `remove_waiter`) updated:
  - a `cspace`-frame hypothesis (`s1.tcb[k].cspace == s0.tcb[k].cspace`) so the fifth clause's
    bound cspace is fixed across the wake;
  - an **off-chain** hypothesis (a *changed* TCB still BlockedNotif in `s1` is blocked on the
    fired `n`) so the sixth clause carries: a thread still BlockedNotif-on-`wn` in `s1` was
    either unchanged (→ `notif_wf(s0, wn)` carries) or changed with `wn == n` (→ `notif_wf(s1, n)`
    is a direct hypothesis). `signal` (woken head → `Runnable`) and `remove_waiter` (spliced
    thread → `wait_notif = None`; predecessor stays blocked-on-`n`) both satisfy it.

- **`lemma_chan_wf_frame`** (`cspace.rs`) — a quarantined `chan_wf` lift (a channel edit touching
  only a binding *value* carries `chan_wf`, which reads only `depth`/`end_caps`/`head`/`count`/
  `ring_cap`/`msg_len`/`bindings.dom()`). Proving the lift inline in `release_binding` blew the
  trigger context once the strengthened `cap_consistent` widened it (the doc 51 §2 hazard); the
  lemma isolates a clean context (doc 25 §2 decomposition).

- **Four body-frame lemmas, proven** (`cspace.rs`) — the tools `destroy_tcb`'s body consumes
  (landed ahead of the body, the `notif_wf`-defined-for-4b/4c precedent):
  - `lemma_waiter_refs_frame_offchain` — `waiter_refs(o)` framed by an edit changing only TCBs
    off-chain by *predicate* (`wait_notif is None || state != BlockedNotif`) in both states.
    `waiter_chain` clause 6 forces every chain node `BlockedNotif` *and* naming `o`, so an
    off-chain thread is on no chain. This is what `lemma_waiter_refs_frame` (keyed on
    `wait_notif`) cannot give for a Runnable thread with a stale `wait_notif`.
  - `lemma_waiter_refs_frame_dequeued` — the *membership* companion: `waiter_refs(o)` framed by a
    single-thread `t` edit when `t` lies on `o`'s chain in neither state (the `remove_waiter`
    absent path: a blocked-but-dequeued thread `!waiter_seq(wn).contains(t)`, which the predicate
    cannot see).
  - `lemma_caps_consistent_frame_thread_offchain` / `lemma_caps_consistent_frame_thread_dequeued`
    — the `caps_consistent` analogs (notif-view-frozen) for the off-chain `set_tcb_qnext`/
    `set_tcb_state` step and the membership `wait_notif`-clear step respectively.

- **`remove_waiter` strengthened** (`notification.rs`) — the `signal`→`fire` precedent. Beyond
  its existing `census_delta_frozen`/`notif_wf(n)`/view frames it now ensures
  `final.cspace_view() == old.cspace_view()` (unconditional) and the **conditional** preservation
  of `caps_consistent`/`end_caps_sound`/`census_dom_complete` (it is a signal-shaped edit — only
  `n`'s notif view + `n`'s waiters move, every TCB's `bind_slots`/`cspace` fixed — so
  `lemma_caps_consistent_frame` applies; the census only drops while the refs domain is fixed).
  A `cspace_view`-frame loop invariant was added for the absent (read-only) path.

- **`unqueue_ready` total-frame seam contract** (`cspace.rs`, the `ExStore` trait) — the
  `make_runnable` precedent. The ready queue is scheduler state below `tcb_view` (a thread is
  Runnable both before and after), so it is a model no-op on every object view; host-checked
  against `ArrayStore`'s empty body. Retires the `thread.rs` note that it "needs no Verus
  contract".

- **`obj_unref`'s Thread arm + `destroy_tcb`'s contract** gained the `cspace_resident_wf` and
  waiter-coherence preconditions (host-checked; the exec mirror `cap_consistent_exec`'s Thread
  arm gained both clauses, so `caps_consistent_exec` differentially checks them).

---

## 2. Findings worth keeping

- **`destroy_notif` being a model view no-op is what makes weak waiter-coherence sound.** The
  first cut tried a *membership* coherence (`waiter_seq(wn).contains(t)`); it broke
  `lemma_caps_consistent_frame` (the generic frame cannot know the chain edit preserved a
  specific member when the edited notification's chain changes). The `notif_wf`-only form is
  preserved by everything *because* `destroy_notif` never removes a notification from
  `notif_view` — so no surviving blocked thread's `notif_wf(wn)` is ever invalidated by a
  last-ref notification teardown. The pessimistic "destroy_notif strands a blocked thread"
  worry was wrong on exactly this point. **Lesson: check whether the destructor is a view no-op
  before assuming an invariant needs the stronger (membership) form.**

- **The off-chain hypothesis is the minimal frame the sixth clause needs.** A changed-and-still-
  blocked thread must be blocked on the *fired* `n` (`signal` wakes its head to `Runnable`;
  `remove_waiter` either clears the spliced thread's `wait_notif` or only re-threads a
  predecessor still blocked on `n`). Stating it as "changed ⟹ off-chain in `s1`" was too strong
  (the predecessor stays blocked-on-`n`); "changed ∧ BlockedNotif-in-`s1` ⟹ blocked on `n`" is
  exactly right and both callers satisfy it.

- **The strengthened `cap_consistent` re-triggered the doc 51 §2 perturbation.** Widening
  `cap_consistent(Thread)` un-fired `delete`'s auto-derived Channel-arm `chan_wf` precondition for
  `obj_unref` (and an unrelated timer lemma's rlimit, which self-resolved once the clause was
  weakened to `notif_wf`-only and the frame lemma fixed). The fix is the same as doc 51 §2:
  expose the fact (a `chan_wf` assert via `lemma_chan_wf_frame`, and an explicit Channel-arm
  re-establish in `delete`'s pre-`obj_unref` block) rather than re-derive it next to a sum-type
  case-split.

---

## 3. The residue: `destroy_tcb`'s body (6d-final-thread-body) — and its solved design

`destroy_tcb`'s body — detach (`unqueue_ready` / `remove_waiter`) → halt → bind-slot `delete`s →
`unref_cspace`/`unref_aspace` (with the documented clear-before-unref reorder) — is **not** proven
here. The blocker is a single, now-characterized gap:

- **The teardown recursion does not frame `tcb[t]`.** `unref_cspace → destroy_cspace → delete`
  carries no `tcb_view` ensures, so after it Verus cannot prove `destroy_tcb`'s
  **`report`-unchanged** and **`state == Halted` / `qnext is None`** postconditions for the
  subject `t`. The fact is *true* — `refs[t] == 0` at entry (it is the destructor) ⟹ by
  `refcount_sound`, `census(t) == 0` ⟹ no `Thread(t)` cap exists ⟹ the recursion never calls
  `destroy_tcb(t)` ⟹ `tcb[t]` is untouched — but it is not exposed on the contracts.

- **The clean frame (solved, recorded for the follow-on):** add to `obj_unref`/`destroy_cspace`/
  `unref_cspace`/`delete` (and the assumed `destroy_channel`/`destroy_tcb` contracts) the
  ensures **`∀x. refs_view()[x] == 0` at entry `==> final.tcb_view()[x] == old.tcb_view()[x]`**.
  It is correct (a teardown op changes `tcb[x]` only via `destroy_tcb(x)`, which fires when
  `dec_ref` drops `refs[x]` from ≥1 to 0 — so `x` had `refs[x] ≥ 1` at entry; thus an `x` with
  `refs[x] == 0` at entry is never re-destroyed) and **composes through the recursion** (refs
  only decrease in teardown, so `refs_entry[x] == 0 ⟹ refs_subentry[x] == 0` at every nested
  op). This is the `cspace_view`-style frame doc 50 threaded, specialized to the dead-object
  case. With it, `destroy_tcb(t)` reads its postconditions for `t` (which has `refs[t] == 0`)
  straight off the frame.

- The remaining body assembly then consumes the four §1 frame lemmas: the off-chain pair for the
  `set_tcb_qnext`/`set_tcb_state` halt step (the entry-state case-split — Runnable / BlockedNotif-
  present / BlockedNotif-absent all leave `t` off every chain), the dequeued pair for the
  `remove_waiter` absent path, the existing `lemma_thread_hold_{cspace,aspace}_drop` for the
  clear-before-unref off-by-one, and `lemma_refcount_sound_from_frozen` across `remove_waiter`.

`destroy_tcb` keeps its assumed-but-host-checked `external_body` contract (now carrying the two
new preconditions, host-checked by `check_destroy_tcb` against the real `ArrayStore` body). The
§6 spec-table goal "kcore carries zero `external_body`" is therefore **not yet met** —
`destroy_tcb` remains (plus the pre-existing `untyped.rs` helpers, out of 6d scope). This
increment removes every *obstacle* to it but the `tcb_view` frame; closing that frame + the body
assembly is **6d-final-thread-body**, the last step of phase 6d.

---

## 4. Doc / CLAUDE.md

No `CLAUDE.md`/spec edit this increment (the doc-30 §3 convention — the sub-phase closeout edit
rides 6f). 6d-final-thread's foundations are now doc 52; the `destroy_tcb` body is the recorded
6d-final-thread-body residue. `cargo verus verify -p kcore` runs with no per-proof filter, so the
two new `cap_consistent` clauses, the extended `lemma_caps_consistent_frame`, `lemma_chan_wf_frame`,
the four off-chain/dequeued frame lemmas, the strengthened `remove_waiter`, and the `unqueue_ready`
contract all auto-gate; `host-tests` runs the strengthened `caps_consistent_exec` (both new Thread
clauses) and `check_destroy_tcb`.
