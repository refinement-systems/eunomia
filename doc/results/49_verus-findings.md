# Verus findings 29 — Phase 6d body-removal: the teardown-frame foundations (delete body 90%)

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6d"). Prior increments: `44` (6d foundation — `caps_consistent`), `45` (frames), `46`
(the `end_caps` census), `47` (preservation chain), `48` (the census-delta lockstep +
off-by-one). This is the **fourth and final body-removal foundation** before the
`external_body` removal itself; it lands the cap→object machinery the `delete` body needs to
carry across its teardown branches, gets the `delete` body proven to ~90%, and pins the one
remaining missing system invariant (§3).

**Still contract/lemma infrastructure — no `external_body` removed.** The three destructors
stay `external_body`; this lands the preservation lemmas and the fire-chain `caps_consistent`/
`end_caps` contracts the body proof consumes, exactly as 44–48 did. The body removal proper
is the immediate follow-on (doc 50), gated on one more foundation (§3).

**Outcome.** `cargo verus verify -p kcore`: **265 verified, 0 errors** (was 255 after doc 48;
`+10` lemmas). `cargo test -p kcore`: **81 passed**. The aarch64 `kernel` cross-build is
unchanged (every change is ghost).

---

## 1. What landed (all proven; no `external_body` removed)

- **`caps_consistent` preservation across the peer-closed fire (`channel.rs` + `cspace.rs`).**
  `lemma_caps_consistent_frame` proves a **signal-shaped edit** preserves `caps_consistent`
  (every notification stays well-formed — the fired one by `signal`'s `ensures`, the rest by
  `lemma_notif_wf_frame`; every TCB's `bind_slots` fixed). `fire` carries a **conditional**
  `caps_consistent(old) ==> caps_consistent(final)` (so `send`/`recv` keep no obligation), and
  `endpoint_cap_dropped` carries an **unconditional** `caps_consistent` + `end_caps_sound`
  under a new `end_caps_off_by_one` precondition (its `set_chan_end_caps` decrement restores
  soundness; no sibling stranded because the off-by-one keeps a live sibling's count ≥ 1).
  `signal` gained the small additive `ensures` the frame needs (`bind_slots`-at-`t` and
  `cspace_view` frames).

- **`end_caps_off_by_one` + `census_off_by_one` (`cspace.rs`).** The off-by-one shapes
  `delete` is in when it calls `endpoint_cap_dropped` — it cleared the deleted cap's slot
  (dropping `end_cap_count`/the census by one) before the matching `end_caps`/`refs`
  decrement. `endpoint_cap_dropped` consumes `end_caps_off_by_one` to restore `end_caps_sound`;
  the fire/wait ops carry `census_off_by_one`-preservation as an `ensures` so `delete` carries
  the census off-by-one across the fire (doc 48).

- **The slot-clear and positivity lemmas (`cspace.rs`).** `lemma_clear_slot_census` /
  `lemma_clear_slot_end_cap` / `lemma_clear_drops_count` (a cleared slot drops exactly the
  designated object's `slot_refs`/`frame_map_refs`, the channel's `end_cap_count`, and
  `count_nonempty`, all else fixed); `lemma_same_caps_same_frame_map` /
  `lemma_same_caps_same_end_cap` (link-only edits like `cdt_unlink` preserve the slot-derived
  terms); `lemma_end_cap_count_positive` / `lemma_armed_timer_refs_pos` /
  `lemma_waiter_refs_pos_from_head` / `lemma_refs_pos_from_off_by_one` (the refs-coupled
  preconditions — `binding_refs_ok`, the Timer armed-notif-live — derived from the census in
  the off-by-one window).

- **The cross-module SCC measure, validated.** `delete`'s body was written in full and
  type/measure-checked: the lexicographic `decreases (count_nonempty(slot_view), height)`
  with `delete = 0 < destroy_cspace = 1 < … < obj_unref = 4` (doc 44 §3) is **accepted by
  Verus across the visible `delete → obj_unref → destroy_cspace → delete` cycle** — the
  termination headline's mechanism is confirmed sound (the body is reverted to `external_body`
  in this increment pending §3, but the measure is no longer a question mark).

---

## 2. Findings worth keeping

- **`caps_consistent` is *not* preserved by `endpoint_cap_dropped` in isolation — the
  end_caps off-by-one is what saves it.** The `set_chan_end_caps` decrement can drop a sibling
  `Channel(co, e)` cap's `end_caps[e]`; it stays `> 0` only because the off-by-one makes a live
  sibling's count `≥ 1` (so `end_caps == count + 1 ≥ 2` before the decrement). And the fire's
  `signal` perturbs `notif`/`tcb`, sound only because `signal` preserves every notification's
  `notif_wf`. Both facts had to become contract `ensures` (with `census_off_by_one(final, z)`
  / `caps_consistent(final)` triggers so the looping caller `check_expired` is undisturbed) —
  the recurring lesson that a property a caller threads across a call belongs in the callee's
  `ensures`, not a caller-applied lemma (doc 48 §2).

- **The delete body is assembly of the landed pieces — and it nearly closes.** With the
  off-by-one census/end_caps (doc 48 + this), the slot-clear lemmas, the
  `caps_consistent`/`end_caps_sound` restoration on `endpoint_cap_dropped`, and the
  refs-positivity lemmas, `delete`'s body proves: `cdt_unlink` frames everything → the clear
  lands the off-by-one + `caps_consistent` + the count drop → the Channel branch carries the
  off-by-one and restores `end_caps_sound`/`caps_consistent` across the fire → the frame branch
  → `obj_unref`. All of this verified **except** the one gap below.

- **The last missing invariant: refs-domain completeness (the gating residue for doc 50).**
  `obj_unref` (and `unref_aspace`) require `refs_view().dom().contains(o)` for the designated
  object, and `census_off_by_one(store, o)` itself includes it. Nothing in the codebase forces
  a **designated/referenced object into the refs domain** — `refcount_sound` only constrains
  `o ∈ refs.dom`, never asserts coverage. So `delete` cannot conclude the deleted cap's object
  is in `refs.dom`. Closing the body needs a small new system invariant —
  `forall live cap: cap_obj/cap_frame_aspace = Some(o) ==> refs.dom.contains(o)` (refs-coupled
  only via `dom`, preserved by teardown because the designating cap is cleared before its
  object leaves `refs.dom`) — threaded teardown-only, like `end_caps_sound` (doc 46). This is
  the fourth such incrementally-discovered foundation invariant; **it is the recurring shape of
  this sub-phase** (each teardown branch's precondition is a refs-coupled fact that needs a
  refs-free or dom-stable system invariant to discharge).

---

## 3. What this sets up — the `external_body` removal (doc 50)

The follow-on:
1. **The refs-domain-completeness invariant** (§2) — threaded teardown-only (the `end_caps_sound`
   cascade shape): `delete`/`obj_unref`/`destroy_*`/`dec_ref`/`revoke`/`thread::bind`.
2. **Re-apply `delete`'s body** (written and 90%-verified here; reverted pending #1) — it
   consumes every lemma landed in 46–49.
3. **`destroy_channel`/`destroy_tcb` bodies** + the lexicographic SCC `decreases` (the height
   directions validated here) on all six SCC members at once.

The recorded fallback (detail §2-6d) stands: the cspace-only cycle, keeping
`destroy_channel`/`destroy_tcb` `external_body`, if the full cross-module cycle is
disproportionate. `delete`'s body is needed either way.

**Doc renumbering.** 6d's body removal is now docs 46–49 (foundations) + doc 50 (the removal);
6e → doc 51, 6f → doc 52. The phase-9 closeout records the inserted sub-phases. No `CLAUDE.md`/
spec edit this sub-phase.
