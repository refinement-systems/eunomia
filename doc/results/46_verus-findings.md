# Verus findings 26 — Phase 6d body-removal gate: the `end_caps` per-endpoint census

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT row, §3.2 the no-global-pool
discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6d"). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`40` (phase 5 — sysabi + the aspace
walker), `41` (6a — the `refcount_sound` census + `cspace_view` residency), `42` (6b —
`unref_aspace` + the frame-mapping term), `43` (6c — `obj_unref`/`destroy_cspace`/
`unref_cspace` against the opaque `delete`), `44` (6d foundation — the `caps_consistent`
cap→object invariant), `45` (6d bodies, part 1 — the teardown *frames*: `only_empties`,
`cdt_unlink`'s all-caps frame, `lemma_binding_drop`).

**The gating blocker doc 45 §2 named, now closed.** Doc 45's body proofs surfaced the
**`end_caps`-census blocker**: `delete`'s body, deleting one of several `Channel(co, end)`
caps, calls `endpoint_cap_dropped` (which decrements `end_caps[end]` and, at zero, fires
peer-closed) and must still prove `caps_consistent(final)` — whose Channel arm requires
`end_caps[end] > 0` for *every* surviving live `(co, end)` cap. With `end_caps[end] == 1`
but two `(co, end)` caps present, dropping one would strand the sibling at
`end_caps[end] == 0`. The kernel never lets `end_caps` undercount (it is exactly the
per-endpoint cap count), but `caps_consistent` only stated the lower bound `end_caps[end]
≥ 1`, never the **equality** — so Verus could not rule the stranding out. Doc 45 §2 flagged
the fix as "its own sub-phase before the bodies, threading `end_caps == count-of-(co,end)-
caps` through the channel construction ops … critically, **the cspace-only fallback does
not dodge this** (doc 44 §1): `delete`'s body has the Channel branch regardless of which
destructors stay opaque." This is that sub-phase.

**Sub-phase split.** To keep the body-removal PR a pure body-proof, 6d's bodies split once
more: this **`end_caps` census foundation** (doc 46 — the gating invariant as contracts,
no `external_body` removed) + the **body-removal** follow-on (doc 47 — flip the three
`external_body` attributes and close the SCC). This is the 6a→6f / 6d foundation→frames→
bodies idiom: *settle the invariant before the op*. The doc renumbering from doc 45 shifts
by one: the body removal is now doc 47, the original 6e (revoke root-survival) → doc 48,
6f (system invariant + closeout) → doc 49 (recorded at the 6f closeout, per the doc-30 §3
"spec edits ride the final closeout" convention).

**Outcome.** `cargo verus verify -p kcore`: **248 verified, 0 errors** (was 247 after doc
45; `+1` — `lemma_end_cap_count_drop`; the `end_caps_sound` requires/ensures threaded
through the teardown family + delete's callers add no new *items*, only contract clauses,
all proven preserved). `cargo test -p kcore`: **81 passed** (was 80; `+1` —
`end_caps_sound_exec_has_teeth`; the three `check_*` teardown tests gained a guarded
`end_caps_sound_exec` assertion, no new `#[test]`). The aarch64 `kernel` cross-build is
unchanged (every change is ghost — `spec`/`proof`/`requires`/`ensures`; confirmed by the
cross-build).

---

## 1. What landed (all proven; no `external_body` removed)

- **`end_caps_sound` — the §3.3 per-endpoint census, refs-free (`cspace.rs`).** A new
  system invariant `spec fn end_caps_sound(store)`: for every live channel `ch` and end
  `e ∈ {0,1}`, `chan_view[ch].end_caps[e] == end_cap_count(slot_view, ch, e)`, where
  `end_cap_count` counts live `Channel(ch, e)` caps in the slot arena (a `slot_refs`-shaped
  filter over the new `cap_chan_end` projection — narrower than `cap_obj`, which drops the
  end). Like `caps_consistent` (doc 44 §2) it reads **only** `chan_view`/`slot_view`, never
  `refs_view`, so `dec_ref`'s `-1` and every chan+slot-framing op preserve it *by framing
  alone* — the recurring lesson that teardown invariants belong off `refs_view`.

- **`lemma_end_cap_count_drop` — the settled recount (`cspace.rs`).** Clearing a
  `Channel(ch, e)` slot to a non-channel cap lowers `end_cap_count(ch, e)` by one and
  **leaves every other `(ch2, e2)` fixed**. The `lemma_designation_drop` clone over the
  `cap_chan_end` filter (the drop is the `f1.remove(k)` step; the others-fixed is a
  per-`(ch2,e2)` `=~=` since `k` named `(ch, e)` and the replacement names nothing). This
  is what `delete`'s body (doc 47) consumes when it empties the deleted channel cap's slot.

- **Threaded through the teardown family + `delete`'s callers.** `requires`+`ensures
  end_caps_sound` added to: `delete`/`destroy_channel`/`destroy_tcb` (`external_body` —
  **assumed**, host-checked, discharged by doc 47); the proven 6b/6c members
  `obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace`/`dec_ref` (preservation
  **proven** — each by framing or via its inner `delete`s, no per-arm hint needed); the
  phase-4 destructors `destroy_notif`/`destroy_timer` (reached by `obj_unref`'s dispatch —
  both frame chan+slot, so trivial); and `revoke` (loop invariant) + `thread::bind`
  (entry). The cascade stops at the trusted shell exactly as 6a's `refcount_sound` did —
  `revoke`/`bind` have no verified kcore caller (doc 41 §2).

- **Construction-op preservation deferred to 6f.** `derive`/`retype_install`/`send`/`recv`/
  `endpoint_cap_added` are *not* re-touched here; the trusted `KernelStore` shell
  establishes `end_caps_sound` before calling `delete`/`revoke`, identically to how
  `refcount_sound` (6a) and `caps_consistent` (6d-foundation) deferred their construction-op
  preservation. 6f makes `end_caps_sound` a genuine system invariant.

- **Host mirror with teeth (`test_store.rs`).** `end_caps_sound_exec` recomputes the
  per-endpoint count over the concrete `ArrayStore`. `end_caps_sound_exec_has_teeth` builds
  a matched fixture (channel 7, `end_caps == [1,1]`, one `(7,A)` + one `(7,B)` cap) and
  rejects both an over-count and an under-count (the doc 45 §2 stranding shape) — two
  negative witnesses, so the mirror is demonstrably non-vacuous. The three `check_*`
  teardown tests assert `end_caps_sound_exec` preserved, guarded on the precondition (the
  `refcount_sound`/`caps_consistent` pattern).

---

## 2. Findings worth keeping

- **The `end_caps` census is the `caps_consistent` analog, and refs-freeness made it
  cheap.** The blocker is real — without the equality, `delete`'s Channel branch is
  unprovable (doc 45 §2) — but the fix is shallow because the invariant reads no `refs`. The
  first instinct (fold `end_caps == count` into `caps_consistent`'s Channel arm) is worse:
  `caps_consistent` is *per-cap*, while `end_caps_sound` is *per-channel* (a forall over
  channels), and folding would force every `caps_consistent`-bearing op to re-prove the new
  conjunct against a changed predicate shape. A **separate** invariant (the `refcount_sound`
  precedent — also separate from `caps_consistent`) is added only where the teardown needs
  it, and the proven members preserve it by framing. This is the foundation's central design
  call, the same one doc 44 §2 made for `caps_consistent`.

- **`endpoint_cap_dropped` is the off-by-one, not a preserver — like `dec_ref`.** It
  decrements `end_caps[end]` *without* touching the cap count, so in isolation it **breaks**
  `end_caps_sound` (leaves `end_caps[end] == count + 1`). It is not given an `ensures
  end_caps_sound`; instead `delete`'s body (doc 47) reasons across the pair — the slot-clear
  drops `end_cap_count` by one (`lemma_end_cap_count_drop`), `endpoint_cap_dropped` drops
  `end_caps[end]` by one, and the two land the equality. This is exactly `dec_ref`'s
  relationship to `refcount_sound`: the destructor is the *correction*, the caller proves
  the invariant across it. Recording it here so doc 47's `delete` body consumes a settled
  off-by-one rather than rediscovering it.

- **The recount cloned verbatim; only the projection changed.** `lemma_end_cap_count_drop`
  is `lemma_designation_drop` with `cap_obj … == Some(obj)` swapped for `cap_chan_end … ==
  Some((ch, e))` — the same `f1.remove(k)`/`=~=`/`len` skeleton, first-try clean. The
  single-domain filter terms keep paying off (doc 41 §2): five of the six census terms and
  now the endpoint count are all `lemma_designation_*` shapes; only `binding_refs`' nested
  triple domain needed the bespoke finiteness work (doc 45).

---

## 3. What this sets up — the body-removal follow-on (doc 47)

With `end_caps_sound` available as a precondition+postcondition on `delete`/`destroy_*`,
the body PR (doc 47) flips `external_body` on `delete`/`destroy_channel`/`destroy_tcb` and
closes the SCC `delete → obj_unref → {destroy_cspace, destroy_channel, destroy_tcb} →
delete`. The residue it still needs (doc 45 §3, now minus the `end_caps` gate):

1. **`refcount_sound` preservation on `signal`/`remove_waiter`/`fire`/`endpoint_cap_dropped`**
   — these phase-3/4 ops drop `refs` and a census term in lockstep but their contracts
   predate the 6a census; the body PR adds the preservation (the analog of 6c's
   `destroy_timer` strengthening).
2. **`destroy_tcb`'s prereqs** — `tcb_view().dom().finite()` for the thread-hold recount;
   `pub(crate)` `lemma_thread_hold_{cspace,aspace}_drop`; and the **unref/clear reorder**
   (clear `tcb.cspace`/`tcb.aspace` *before* `unref_cspace`/`unref_aspace`, the off-by-one —
   `unref_cspace`'s `requires refcount_sound` is the `refs == census + 1` form, so the
   thread-hold term must drop first).
3. **The shared lexicographic `decreases (count_nonempty(slot_view), height)`** with the
   doc-44 §3 height direction (`delete = 0 < {destroy_cspace, destroy_channel} = 1 <
   unref_cspace = 2 < destroy_tcb = 3 < obj_unref = 4`): the only count-dropping edge is
   `delete → obj_unref` (delete empties its slot first), every other intra-SCC edge is
   count-flat and descends in height.

The recorded fallback (detail §2-6d) stands: if the cross-module cycle proves
disproportionate, close the cspace-only cycle (`delete`+`obj_unref`+`destroy_cspace`+
`unref_cspace`), keep `destroy_channel`/`destroy_tcb` `external_body` with their now
`end_caps`-bearing host-checked contracts, and record the residue — the `end_caps` census
landed here gates `delete`'s body either way.

No `CLAUDE.md`/spec edit this sub-phase (the doc-30 §3 convention; the phase-6 closeout is
6f / doc 49).
