# Verus findings 25 — Phase 6d bodies, part 1: the teardown frames (+ the `end_caps`-census blocker)

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT row, §3.2 the no-global-pool
discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6d"). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`40` (phase 5 — sysabi + the aspace
walker), `41` (6a — the `refcount_sound` census + `cspace_view` residency), `42` (6b —
`unref_aspace` + the frame-mapping term), `43` (6c — `obj_unref`/`destroy_cspace`/
`unref_cspace` against the opaque `delete`), `44` (6d foundation — the `caps_consistent`
cap→object invariant).

**6d's bodies split again.** Doc 44 already split 6d into a *foundation* (the
`caps_consistent` invariant) and the *bodies* (this). Attempting the body proofs surfaced a
**second** obstacle the detail plan did not name — one that, unlike `caps_consistent`, the
6d-foundation idiom (state it, host-check it, consume it) does **not** resolve, because it
is a genuinely *missing system invariant* whose construction-side establishment is a
sub-phase of its own (§2). So the bodies split into a **frames** PR (this, doc 45 — the
reusable teardown-frame machinery, all `verus!{}`-proven, no `external_body` removed) and a
**body-removal** PR (the follow-on, doc 46 — flip the three `external_body` attributes and
close the SCC, once the missing invariant lands). This is the same shape as the 6a→6b…6f
and the 6d foundation→bodies decompositions: *settle the frame before the op*.

**Outcome.** `cargo verus verify -p kcore`: **247 verified, 0 errors** (was 244 after doc
44; `+3` — `lemma_only_empties_trans`, `lemma_binding_drop`, `lemma_binding_triples_finite`;
plus the cap-frame on `cdt_unlink`, the `only_empties`/`chan_view`-finiteness conjuncts on
the existing contracts, and the `only_empties` preservation proofs on the proven 6c members,
none of which add new *items*). `cargo test -p kcore`: **80 passed** (unchanged count — the
three `check_*` teardown tests gained an `assert_only_empties` guard, no new `#[test]`). The
aarch64 `kernel` cross-build is unchanged (every change is ghost — `spec`/`proof`/
`requires`/`ensures`; confirmed by the cross-build).

---

## 1. What landed (all proven, no `external_body` removed)

- **`only_empties` — the "teardown only empties slots" frame (`cspace.rs`).** A refs-free,
  slot-local `spec fn only_empties(sv0, sv1)`: every slot empty in `sv0` is empty in `sv1`.
  Teardown never *fills* a slot (`cdt_unlink` moves links not caps; `set_slot` only clears;
  the recursive destructors only delete), so this composes transitively
  (`lemma_only_empties_trans`) along the recursion. It is the frame **`delete`'s own
  `is_empty_cap(final[slot])` ensures rests on** (`obj_unref` must leave the just-cleared
  slot empty) and that `destroy_channel`'s ring-cap loop would carry to conclude every ring
  slot ends empty. Threaded as a real `ensures` through the proven 6c members
  (`obj_unref`/`destroy_cspace`/`unref_cspace` — preservation proven, no per-arm hint
  needed: `dec_ref` frames `slot_view`, so each destructor's `only_empties` carries straight
  through) and as an **assumed, host-checked** `ensures` on the still-`external_body`
  `delete`/`destroy_channel`/`destroy_tcb` (the 6a/6d-foundation pattern). Host-checked by a
  new `assert_only_empties` in `check_delete`/`check_destroy_channel`/`check_destroy_tcb`.

- **`cdt_unlink`'s all-caps-preserved frame (`cspace.rs`).** `cdt_unlink` already proves its
  result equals the closed form `unlinked(m0, slot, last)` (doc 25), and `unlinked` rebuilds
  every entry's `.cap` from `m0` — so it preserves *every* slot's cap, not just `slot`'s.
  Exposed as `forall x ∈ dom: final.slot_view()[x].cap == old.slot_view()[x].cap` (a one-line
  assert off the closed form). This is what makes `only_empties` discharge through
  `cdt_unlink` and is the basis of `delete`'s §4d notification-frame.

- **`lemma_binding_drop` + `lemma_binding_triples_finite` — the quarantined binding recount
  (`cspace.rs`).** 6a deliberately **quarantined** the sixth census term's recount (doc 41
  §2; the comment above `lemma_designation_drop`): unlike the five `filter`-of-a-finite-map
  terms, `binding_refs` counts over the *nested* `(ch, end, ev)` triple domain via
  `Set::new(..)`, whose `.len()` recount needs the triple set's **finiteness** established by
  hand. `lemma_binding_triples_finite` builds it — the binding universe is `⋃` of the six
  maps `chan_view.dom() ↦ (c, e, ev)`, each finite by `Set::lemma_map_finite`, the union
  finite by `lemma_set_union_finite_iff`, and the binding set a `lemma_set_subset_finite`
  subset. `lemma_binding_drop` then lands the drop: clearing one binding (`Some(o) → None`)
  lowers `binding_refs(o)` by one and **leaves every other object's term fixed** (the
  "others-fixed" companion `dec_ref`'s off-by-one precondition needs — only sound because the
  clear target is `None`, never `Some(y≠o)`). This is the **deferred binding-term recount**
  the detail plan (§2-6d) and the `destroy_channel` comment named — now proven, ready for the
  body PR to consume one clear-then-`dec_ref` step at a time.

- **`chan_view().dom().finite()` added to `caps_consistent` (`cspace.rs`).** The natural home
  for the channel-arena finiteness `lemma_binding_drop` needs (beside the existing
  `slot_view().dom().finite()` conjunct) — refs-free and structural, so every mutator carries
  it by framing or by single-channel `insert` (both finiteness-preserving). **Verified clean
  across the whole crate with zero downstream breakage** (the additive-frame argument, doc 27
  §1) — the strongest evidence the conjunct is the right shape.

- **`dec_ref` and `lemma_binding_drop` made `pub(crate)`** so the cross-module
  `channel::destroy_channel` body (the follow-on) can call them.

---

## 2. Findings worth keeping

- **The `end_caps`-census blocker — the real reason the bodies don't land here, and the
  intellectual core of this increment.** `delete`'s body, deleting a `Channel(co, end)` cap,
  must preserve `caps_consistent(final)`. The Channel arm of `cap_consistent` requires
  `end_caps[end] > 0` for every live `(co, end)` cap. `delete` calls `endpoint_cap_dropped`,
  which decrements `end_caps[end]` and, at zero, fires peer-closed. **If `end_caps[end] == 1`
  but two `(co, end)` caps exist, deleting one strands the sibling with `end_caps[end] == 0`
  — violating `caps_consistent`.** The kernel never lets `end_caps` undercount (it is exactly
  the per-endpoint cap count, maintained by `endpoint_cap_added`/`_dropped`), but **the spec
  does not capture that**: `caps_consistent` only states `end_caps[end] ≥ 1` per live cap, a
  lower bound, not the equality. So Verus cannot rule the stranding out. Closing `delete`'s
  body needs a new **`end_caps == per-endpoint-cap-count` census** — a `refcount_sound`-sized
  invariant threading through *every* channel-cap producer (`retype`'s install, `derive`'s
  `endpoint_cap_added`, `slot_move`/`send`/`recv`'s relocations) — which is its own sub-phase,
  not a `delete`-body detail. Critically, **the cspace-only fallback does not dodge this**
  (doc 44 §1): `delete`'s body has the Channel branch regardless of which destructors stay
  opaque. So the headline (SCC termination) waits on the `end_caps` census.

- **`signal`'s wake is census-delta-neutral, but its contract doesn't say so.** The other
  half of `delete`'s Channel branch: `endpoint_cap_dropped`'s fire calls `signal`, whose wake
  path drops **both** `refs[n]` and `waiter_seq(n)` by one in lockstep (`notification.rs`
  lines 104-107) — so it preserves `refs[x] − census(x)` for every `x`, i.e. it preserves
  `refcount_sound`. But `signal`/`fire`/`endpoint_cap_dropped` predate the 6a census and
  their contracts expose only the raw deltas, not the soundness preservation. The body PR
  must add `refcount_sound` preservation to that chain (the analog of 6c's `destroy_timer`
  strengthening, doc 43) — mechanical given the lockstep, but a real contract edit on three
  phase-3/4 ops.

- **Refs-free, slot-local frames compose for free; that is why `only_empties` was cheap.**
  `only_empties` reads only `slot_view` emptiness and never `refs`, so `dec_ref`/
  `set_obj_refs` preserve it by framing, and the three proven 6c members re-established it
  with **no per-arm proof** (Verus connected `dec_ref`'s `slot_view`-frame to each
  destructor's `only_empties` automatically). The same refs-freeness that made
  `caps_consistent` shallow (doc 44 §2) makes `only_empties` shallow — the recurring lesson:
  keep teardown frames off `refs_view`.

- **The binding-set finiteness was the quarantine's whole substance.** Once
  `lemma_binding_triples_finite` is in hand, `lemma_binding_drop` is a `lemma_designation_drop`
  clone (the set differs by exactly the removed triple). The finiteness — a six-way
  `map`/`union`/`subset` argument over `vstd::set_lib` — is the part 6a flagged as the
  `Set::new`-vs-`filter` distinction (doc 41 §2). Landing it here de-risks the `destroy_channel`
  body's central census step in advance, exactly the "settle the model before the op"
  discipline.

---

## 3. What this sets up — the body-removal follow-on (doc 46)

The follow-on flips `external_body` on `delete`/`destroy_channel`/`destroy_tcb` and closes the
SCC. The frames here are precisely what it consumes; what it additionally needs (the recorded
residue, in dependency order):

1. **The `end_caps` per-endpoint census** (the gating blocker, §2) — likely its own sub-phase
   before the bodies, threading `end_caps == count-of-(co,end)-caps` through the channel
   construction ops. Without it `delete`'s `caps_consistent(final)` (Channel case) is
   unprovable.
2. **`refcount_sound` preservation on `signal`/`fire`/`endpoint_cap_dropped`** (§2) — so
   `delete`'s Channel branch threads the census across the peer-closed fire.
3. **`delete`'s `chan_view[ch]` immutable-layout frame** (depth + `ring_cap`) — so
   `destroy_channel`'s ring-cap loop reads `ch`'s handles across its `delete` calls. True
   (`endpoint_cap_dropped`/`destroy_channel` preserve them via `..cv[co]` / binding-only
   edits), so an assumed+host-checked frame, dischargeable when `delete`'s body lands.
4. **`tcb_view().dom().finite()` + `pub(crate)` `lemma_thread_hold_{cspace,aspace}_drop` +
   `remove_waiter` census-neutrality** — for `destroy_tcb`'s body (its bind caps are
   notifications, so it dodges the `end_caps` blocker entirely; its work is the thread-hold
   reorder — clear `tcb.cspace`/`tcb.aspace` *before* `unref_cspace`/`unref_aspace`, the
   off-by-one — and deriving `remove_waiter`'s lockstep soundness).

The SCC lexicographic measure `(count_nonempty, height)` and the height direction (`delete`
lowest, `obj_unref` highest — doc 44 §3) are unchanged and need no new work; they apply once
the bodies are checkable.

**Sub-phase / doc renumbering.** 6d's bodies are now **frames (this, doc 45) + body removal
(doc 46)**; the original `6e` (revoke root-survival) → doc 47, `6f` (system invariant +
closeout) → doc 48. The phase-9 master-plan closeout records the inserted sub-phases (the
doc-30 §3 "spec edits ride the final closeout" convention). No `CLAUDE.md`/spec edit this
sub-phase.
