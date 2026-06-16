# Verus findings 27 — Phase 6d body-removal: the teardown preservation chain

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT row, §3.2 the no-global-pool
discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6d"). Prior increments: `21`…`25`
(phase 2), `26`…`30` (phase 3), `31`…`35` (phase 4), `36`…`40` (phase 5), `41` (6a — the
`refcount_sound` census + `cspace_view` residency), `42` (6b), `43` (6c), `44` (6d
foundation — `caps_consistent`), `45` (6d bodies part 1 — the teardown *frames*:
`only_empties`, `cdt_unlink`'s all-caps frame, `lemma_binding_drop`), `46` (the **`end_caps`
per-endpoint census** — the body-removal gate, doc 45 §2).

**The body-removal split again.** Doc 46 closed the `end_caps` gate. Attempting the actual
`external_body` removal on `delete`/`destroy_channel`/`destroy_tcb` surfaced that the
recorded residue (doc 45 §3) — "`refcount_sound` preservation on `signal`/`fire`/
`endpoint_cap_dropped`/`remove_waiter`" — is **larger and subtler** than a contract line: it
needs a family of reusable *frame lemmas* (the waiter-chain transport across a wake/splice)
that no prior phase built, plus a `cdt_unlink` object-view-frame strengthening, plus a
robustness fix to `waiter_refs` itself. So this sub-phase lands the **preservation chain**
(all `verus!{}`-proven, **no `external_body` removed**), and the `external_body` removal +
SCC closure rides the follow-on (doc 48), exactly as 6a→6b…6f and the 6d
foundation→frames→census decompositions did: *settle the machinery before the op*.

**Outcome.** `cargo verus verify -p kcore`: **253 verified, 0 errors** (was 248 after doc
46; `+5` — `lemma_chain_frame_set`, `lemma_waiter_refs_frame`, `lemma_waiter_refs_frame_nv`,
`lemma_thread_hold_frame`, `lemma_binding_refs_frame`; the conditional `refcount_sound`
preservation threaded onto `signal`/`fire`/`endpoint_cap_dropped`/`remove_waiter`, the
`cdt_unlink` object-view frames, the `tcb_view` finiteness conjunct, and the robust
`waiter_refs` add no new *items*). `cargo test -p kcore`: **81 passed** (unchanged from doc
46 — all changes are ghost or `spec`). The aarch64 `kernel` cross-build is unchanged (every
change is `spec`/`proof`/`requires`/`ensures`).

---

## 1. What landed (all proven; no `external_body` removed)

- **The waiter-chain transport frames (`cspace.rs`).** The heart of the increment. A
  `signal` wake / `remove_waiter` splice perturbs notification `n`'s view and a *set* of
  TCBs (the dequeued head, and for a splice its predecessor). For any other object `o != n`
  the waiter census must be unchanged, but `waiter_seq` is a `choose` over a per-state
  predicate, so the two states' `choose`s are not syntactically equal. Resolved by
  **transport**: `lemma_chain_frame_set` shows a chain for `o` rides verbatim across the
  edit (no chain node is a changed TCB, since chain nodes name `o` but every changed TCB
  names `n != o` in the relevant state), and `lemma_waiter_refs_frame` composes it both ways
  + `lemma_waiter_chain_unique` to conclude `waiter_refs(o)` is equal — with the no-chain
  case handled by the robust `waiter_refs` (below). `lemma_waiter_refs_frame_nv` is the
  `tv`-fixed companion (the accumulate path's word-only edit). `lemma_thread_hold_frame`
  frames the cspace/aspace term across any edit that leaves those two TCB fields fixed.

- **`waiter_refs` made robust (`cspace.rs`).** `waiter_refs(o)` is now `if exists chain then
  waiter_seq(o).len() else 0` — 0 when `o` is not a well-formed notification, instead of the
  `choose`-garbage the bare `waiter_seq(o).len()` would yield. This **aligns the spec with
  the exec mirror** (`waiter_count_exec` already returns 0 for a non-notification `o`) and is
  what lets the transport frames cover *every* `o` (a no-chain `o` stays at 0 in both states,
  which `choose`-equality cannot deliver). Re-verified clean across the crate — the existing
  `obj_unref` Notification proof is unaffected (it carries `notif_wf(o)`, so the chain exists
  and the `if` reduces to the plain length).

- **Conditional `refcount_sound` preservation on the fire/wait chain
  (`notification.rs`/`channel.rs`).** `signal`, `fire`, `endpoint_cap_dropped`, and
  `remove_waiter` gained `ensures refcount_sound(old(store)) ==> refcount_sound(final(store))`,
  proven via the transport frames + `lemma_thread_hold_frame`. **Conditional** (no new
  `requires`) is the key design call: the kernel-shell-facing `report_terminal`/`check_expired`
  and the construction-op callers `send`/`recv` (which call `fire`) keep **no**
  `refcount_sound` obligation, so the wide 6f construction-op cascade is avoided — only the
  teardown path, which supplies the hypothesis, uses it.

- **`cdt_unlink`'s object-view frames (`cspace.rs`).** `cdt_unlink` edits only slot CDT
  links, so it frames `chan`/`notif`/`tcb`/`timer`/`cspace` views — but its contract only
  stated the `refs` frame. Added the five object-view frames (to the contract and the
  children-walk loop invariant; mechanical, each `set_slot` frames them), so `delete`'s body
  (doc 48) can read the census/`end_caps` views across the `cdt_unlink` that precedes the
  teardown.

- **`tcb_view().dom().finite()` in `caps_consistent` + `pub(crate)` thread-hold drops.** The
  finiteness `destroy_tcb`'s `thread_hold_refs` recount needs (beside the slot/chan
  finiteness companions; additive-frame clean, the doc-45 §1 evidence). `lemma_thread_hold_
  {cspace,aspace}_drop` made `pub(crate)` for the cross-module `destroy_tcb` body.

---

## 2. Findings worth keeping

- **The `choose`-equality wall, and why robustness + transport is the way through.** The
  natural statement "`waiter_seq` is unchanged for `o != n`" is *false at the `choose` level*
  for a non-notification `o` (two unsatisfiable predicates' `choose`s need not agree), even
  though the predicates are extensionally equal. Two independent fixes were both required:
  (a) **robust `waiter_refs`** so the no-chain case is a hard 0 (not `choose`-garbage), and
  (b) **witness transport** (`lemma_chain_frame_set`) so the *satisfiable* case is settled by
  `lemma_waiter_chain_unique` on a common predicate rather than by equating two `choose`s.
  This is the central new proof technique of the increment, and the analog of the doc-45
  binding-set-finiteness work for the waiter term.

- **The off-by-one is the wall the `external_body` removal hits next (the headline finding).**
  `delete`'s body clears the deleted slot (dropping `slot_refs`/`frame_map_refs`/
  `end_cap_count` by one — the census off-by-one `obj_unref`'s `dec_ref` later restores) and
  *then* calls `endpoint_cap_dropped`, whose peer-closed `fire` can wake a waiter and drop a
  notification's `refs` + census in lockstep. So `endpoint_cap_dropped` runs in a window where
  `refcount_sound` does **not** hold (it is off by one at the channel object). The
  **conditional `refcount_sound(old) ==> refcount_sound(final)`** landed here is therefore
  *insufficient for `delete`* — its hypothesis is false in that window. The fix the body PR
  needs is to upgrade `signal`/`fire`/`endpoint_cap_dropped` to a **lockstep-delta /
  off-by-one-preserving** form (`refs[x]` and `census(x)` move by the same amount at every
  `x`, so any off-by-one shape is preserved), which the transport frames here already make
  provable. `remove_waiter`'s conditional, by contrast, *is* final: `destroy_tcb` calls it as
  its first mutation, where `refcount_sound` genuinely holds. Recording this so the body PR
  starts from the right contract shape rather than rediscovering the window.

- **Conditional preservation is the cascade firebreak.** Phrasing the preservation as an
  implication keyed on `refcount_sound(old)` (rather than a new `requires`) is what keeps the
  6f construction-op retrofit out of this sub-phase: `fire`'s `send`/`recv` callers and
  `signal`'s `report_terminal`/`check_expired` callers are wholly undisturbed. The same
  device will carry the off-by-one form in the body PR.

- **`end_caps` is not a census term — so `endpoint_cap_dropped`'s decrement is census-neutral.**
  `binding_refs` reads only `bindings`, never `end_caps`, so `set_chan_end_caps` frames the
  census (via the new `lemma_binding_refs_frame`); the only census motion in
  `endpoint_cap_dropped` is the peer-closed `fire`. This cleanly separates the `end_caps`
  bookkeeping (doc 46) from the refcount census.

---

## 3. What this sets up — the `external_body` removal (doc 48)

The follow-on flips `external_body` on `delete`/`destroy_channel`/`destroy_tcb` and closes
the SCC `delete → obj_unref → {destroy_cspace, destroy_channel, destroy_tcb} → delete`. The
machinery here is precisely what it consumes; what it additionally needs:

1. **The off-by-one/lockstep upgrade** on `signal`/`fire`/`endpoint_cap_dropped` (§2) — so
   `delete`'s Channel branch threads the census across the peer-closed fire in the
   slot-cleared (off-by-one) window. Provable from the transport frames already landed.
2. **`delete`'s body proof** — `cdt_unlink` (now object-view-framed) → clear slot
   (`lemma_designation_drop` + `lemma_end_cap_count_drop`, the off-by-one) →
   `endpoint_cap_dropped` (off-by-one preserved) / frame-unmap branch (`lemma_frame_map_drop`
   + `unref_aspace`) → `obj_unref` (`dec_ref` restores the off-by-one). Re-prove `cspace_wf`/
   `refcount_sound`/`caps_consistent`/`end_caps_sound`/`only_empties`/`count_nonempty` drop.
3. **`destroy_channel`'s body** — ring-cap delete loops + the binding-release loop (each `-1`
   matched by `lemma_binding_drop`), with the `count_nonempty`-non-increase loop invariant.
4. **`destroy_tcb`'s body** — `remove_waiter` (conditional `refcount_sound`, final) + the
   **unref/clear reorder** (clear `tcb.cspace`/`tcb.aspace` *before* `unref_cspace`/
   `unref_aspace`, the off-by-one — `unref_cspace` requires the `refs == census + 1` form, so
   the `thread_hold` term must drop first) + the two bind-slot `delete`s.
5. **The shared lexicographic `decreases (count_nonempty(slot_view), height)`** (doc 44 §3:
   `delete = 0 < {destroy_cspace, destroy_channel} = 1 < unref_cspace = 2 < destroy_tcb = 3 <
   obj_unref = 4`). `count_nonempty` is already `revoke`'s/`destroy_cspace`'s measure; the
   only count-dropping edge is `delete → obj_unref` (delete empties its slot first), every
   other intra-SCC edge is count-flat and descends in height.

The recorded fallback (detail §2-6d) stands: if the cross-module cycle is disproportionate,
close the cspace-only cycle and keep `destroy_channel`/`destroy_tcb` `external_body` with
their host-checked contracts. The `end_caps` census (doc 46) gates `delete`'s body either way.

**Sub-phase / doc renumbering.** 6d's body-removal is now **the preservation chain (this, doc
47) + the `external_body` removal (doc 48)**; the original `6e` (revoke root-survival) → doc
49, `6f` (system invariant + closeout) → doc 50. The phase-9 master-plan closeout records the
inserted sub-phases (the doc-30 §3 "spec edits ride the final closeout" convention). No
`CLAUDE.md`/spec edit this sub-phase.
