# Verus findings 28 — Phase 6d body-removal: the census-delta lockstep + off-by-one

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT row) and
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6d"). Prior increments: `41` (6a — the
`refcount_sound` census), `42` (6b), `43` (6c), `44` (6d foundation — `caps_consistent`),
`45` (6d frames), `46` (the `end_caps` per-endpoint census — the body-removal gate), `47`
(the teardown preservation chain — the waiter-chain transport frames + conditional
`refcount_sound` on the fire/wait ops).

**The off-by-one window — the next obstacle, and why a stronger contract was needed.** Doc
47 §2 flagged it: `delete`'s body calls `endpoint_cap_dropped` in the window **after**
clearing the deleted cap's slot (the census is off by one at the channel object) but
**before** `obj_unref`'s `dec_ref` restores the count. The conditional `refcount_sound(old)
==> refcount_sound(final)` doc 47 landed is *false-hypothesis* there, so it gives `delete`
nothing. The fix is a strictly stronger, **unconditional** contract — the refcount census
and the stored refs move in **lockstep**, so *any* off-by-one shape survives — landed here.
This is the last contract-infrastructure piece before the body proofs themselves.

**This increment is contract infrastructure, not the body removal.** Like docs 44–47 it
removes **no** `external_body`; it lands the lockstep contracts the body proofs (doc 49)
consume. The body removal proper — `delete`/`destroy_channel`/`destroy_tcb` bodies + the
cross-module SCC `decreases` — is the follow-on (see §3).

**Outcome.** `cargo verus verify -p kcore`: **255 verified, 0 errors** (was 253 after doc
47; `+2` — `lemma_refcount_sound_from_frozen`, `lemma_off_by_one_frozen`; the
`census_delta_frozen`/`census_off_by_one` specs and the upgraded ensures on
`signal`/`fire`/`endpoint_cap_dropped`/`remove_waiter` add no new *items*). `cargo test -p
kcore`: **81 passed** (unchanged). The aarch64 `kernel` cross-build is unchanged (every
change is ghost).

---

## 1. What landed (all proven; no `external_body` removed)

- **`census_delta_frozen(s0, s1)` — the lockstep contract (`cspace.rs`).** `refs[x] -
  census(x)` is unchanged at every `x` across an edit (stated additively, `refs1[x] +
  census0[x] == refs0[x] + census1[x]`, so no `nat` underflow). The fire/wait ops
  (`signal`/`fire`/`endpoint_cap_dropped`/`remove_waiter`) now carry it **unconditionally**
  (replacing doc 47's conditional `refcount_sound`): a wake/splice drops one waiter's queued
  `refs` and its `waiter_seq` length together; an `end_caps` decrement touches no census
  term. Proven from the doc-47 transport frames (`lemma_waiter_refs_frame` etc.). Its trigger
  is `obj_census(s1, x)` (the *final* census) so census-agnostic callers — crucially
  `check_expired`'s `signal`-in-a-loop — never instantiate it (a deliberate fix after the
  naive `refs_view().dom().contains` trigger blew the rlimit there).

- **`census_off_by_one(store, z)` + its preservation as an `ensures` (`cspace.rs` +
  fire/wait ops).** The census sound everywhere except `refs[z] == census(z) + 1` — exactly
  `obj_unref`'s precondition after `delete` clears the deleted cap's slot. `signal`/`fire`/
  `endpoint_cap_dropped` now `ensure forall z: census_off_by_one(old, z) ==>
  census_off_by_one(final, z)`. **Stating it as an `ensures` (not a separately-applied
  lemma) is the design key**: Verus applies it to the call automatically, so `delete` carries
  the deleted-slot off-by-one across the peer-closed fire without naming the (un-nameable)
  mid-call store snapshot. The trigger `census_off_by_one(final, z)` keeps it out of
  census-agnostic callers. Each op proves its own clause from its own `census_delta_frozen`
  via `lemma_off_by_one_frozen` (where `old`/`final` are both nameable inside the op).

- **`lemma_refcount_sound_from_frozen` / `lemma_off_by_one_frozen` (`cspace.rs`).** The two
  consumers of a frozen delta: it turns `refcount_sound`-at-start into `refcount_sound`-at-end
  (the form `destroy_tcb` will apply to `remove_waiter`, whose `old` is `destroy_tcb`'s own
  `old`), and it carries a `census_off_by_one(z)` from start to end (the form the fire/wait
  ops prove their off-by-one `ensures` with).

---

## 2. Findings worth keeping

- **The off-by-one must be an `ensures`, because mid-call snapshots aren't nameable.** The
  natural instinct — give the ops `census_delta_frozen` and have `delete` apply
  `lemma_off_by_one_frozen` to the result — fails: the lemma needs the *pre-call* store as a
  named `&S`, which Verus does not expose across a `&mut` call (the call's ensures references
  it only via `old`). Making off-by-one-preservation a contract `ensures` sidesteps this
  entirely — Verus instantiates the ensures' `forall z` against the call automatically, with
  the pre-call state handled by the ensures' own `old`. The lesson: **properties a caller
  must thread across a call belong in the callee's `ensures`, not in a caller-applied lemma**,
  whenever the intermediate state can't be named.

- **Trigger choice is load-bearing for hot-path callees.** `signal` is called in
  `check_expired`'s loop. A `census_delta_frozen` with a `refs_view().dom().contains(x)`
  trigger instantiated `obj_census` (six terms) per object per loop iteration → rlimit. Moving
  the trigger to `obj_census(s1, x)` — a term census-agnostic callers never mention — makes the
  ensures free to carry. Same device on the `census_off_by_one` ensures (trigger
  `census_off_by_one(final, z)`). A heavy ensures on a looping-callee's contract must trigger
  only on terms its callers actually reason about.

- **`census_delta_frozen` is transitive and composes through census-neutral steps.**
  `endpoint_cap_dropped` proves its lockstep by observing `set_chan_end_caps` is refs- *and*
  census-neutral (`end_caps` is no census term, `binding_refs` framed via
  `lemma_binding_refs_frame`), so the net delta from entry equals `fire`'s frozen delta. The
  clean separation of `end_caps` bookkeeping (doc 46) from the refcount census (doc 41) is
  what makes this a one-line composition.

---

## 3. What this sets up — the body removal (doc 49)

With the lockstep + off-by-one `ensures` in place, the follow-on removes `external_body` from
`delete`/`destroy_channel`/`destroy_tcb` and closes the SCC. What it still needs — the
remaining (substantial) work, now precisely scoped:

1. **`caps_consistent` preservation across the peer-closed fire.** `delete` needs
   `caps_consistent` before `obj_unref`, but `endpoint_cap_dropped` does **not** preserve it
   in isolation: the `set_chan_end_caps` decrement can drop a sibling `Channel(co, end)` cap's
   `end_caps[end]` (sound only because `end_caps_sound` makes it `== end_cap_count ≥ 1` when a
   sibling exists), and the fire's `signal` perturbs `notif`/`tcb` (sound because `signal`
   preserves every notification's `notif_wf`). So `fire` needs a **conditional
   `caps_consistent` ensures** (proven from `signal`'s `notif_wf` frames — a forall over live
   caps, the `binding_notif_wf` forall generalized), and `endpoint_cap_dropped` needs to
   require `end_caps_sound` and thread it through the decrement.
2. **A slot-clear count-drop lemma** (`count_nonempty(m.insert(k, empty)) ==
   count_nonempty(m) - 1`) — the `lemma_designation_drop` shape over the `is_empty` filter.
3. **`delete`'s body proof** — `cdt_unlink` (frames, doc 45/47) → clear slot
   (`lemma_designation_drop`/`lemma_frame_map_drop`/`lemma_end_cap_count_drop` establish the
   off-by-one at the cap's object/aspace + the `end_caps` off-by-one for a Channel) →
   `endpoint_cap_dropped` (off-by-one + `caps_consistent` preserved by its ensures, `end_caps`
   restored) → frame-unmap branch (`unref_aspace` fixes the aspace off-by-one) → `obj_unref`
   (consumes the off-by-one for `Some o`, `refcount_sound` for `None`) — plus the
   notification-frame ensures `thread::bind` reads.
4. **`destroy_channel`/`destroy_tcb` bodies** + the **lexicographic SCC `decreases`**
   `(count_nonempty, height)` (doc 44 §3 directions), added to all six SCC members at once
   when the cycle becomes visible.

The recorded fallback (detail §2-6d) still stands: if the cross-module cycle is
disproportionate, close the cspace-only cycle and keep `destroy_channel`/`destroy_tcb`
`external_body`.

**Sub-phase / doc renumbering.** 6d's body removal is now **the census-delta lockstep (this,
doc 48) + the body removal (doc 49)**; the original `6e` (revoke root-survival) → doc 50,
`6f` (system invariant + closeout) → doc 51. The phase-9 closeout records the inserted
sub-phases. No `CLAUDE.md`/spec edit this sub-phase.
