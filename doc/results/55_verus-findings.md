# Verus findings 35 — Phase 6e: `revoke` root-survival, the conditional non-zombie theorem

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6e"). Prior increment: `54` (6d-final-thread-body-2 — `destroy_tcb`'s body proven, the
cross-module SCC closed; **phase 6d complete**). This increment closes **6e**: the long-deferred
`revoke` root-survival postcondition (the doc 23 §4 gap), now a theorem under an explicit
non-zombie precondition.

`cargo verus verify -p kcore`: **312 verified, 0 errors** (was 305 after doc 54's baseline — the
seven new §6e foundation lemmas; every teardown-cluster body re-verified against the strengthened
contracts with no new proof obligation failing). `cargo test -p kcore`: **83 passed** (was 82 —
`check_revoke_root_survives`; the now-strengthened `revoke` contract keeps its zombie test as a
negative witness). The aarch64 `kernel` cross-build is unchanged (every change is ghost or a
contract; `verus!{}` erases it — confirmed `cd kernel && cargo build`). No rlimit bump or
`spinoff_prover` was needed.

---

## 1. What landed

- **The structured-emptying provenance frame (`cspace.rs`).** A teardown op clears a slot's cap
  only for the directly-deleted target or for a slot that is some object's **internal home
  handle** — a `cspace` resident, a channel `ring_cap`, or a TCB `bind_slot`. The new specs:
  - `homed_in_cspace` / `homed_in_chan` / `homed_in_tcb` and their union `is_homed(s, x)`;
  - `unhomed_frozen(s0, s1, target)` — every un-homed slot other than `target` keeps its **exact
    cap** — and the target-free `unhomed_frozen_free` (for the destructors, which have no single
    target — every slot they clear is one of their residents/ring-caps/bind-slots or its
    recursive closure);
  - `home_views_frozen(s0, s1)` — the three home maps are framed (`cspace_view` equal, `ring_cap`
    via `chan_struct_frame`, the TCB domain + every immutable `bind_slots`), the stability
    `is_homed` (hence the frame) composes on.

  Seven foundation lemmas (`lemma_is_homed_stable`, `lemma_home_views_frozen_{refl,trans}`,
  `lemma_unhomed_frozen_free_{from_slot_eq,from_homed,trans}`, `lemma_unhomed_frozen_compose`)
  carry it across the recursion exactly as `only_empties`/`dead_tcb_frozen` already do.

- **Threaded through the whole teardown cluster.** `delete` exports `unhomed_frozen(·, ·, slot)`;
  `obj_unref`, `destroy_cspace`, `unref_cspace`, `channel::destroy_channel`, `thread::destroy_tcb`
  export `unhomed_frozen_free`; all export `home_views_frozen`. Each destructor lifts its
  resident/ring/bind `delete`s from the target-aware frame to the target-free one via
  `lemma_unhomed_frozen_free_from_homed` (the deleted handle is itself homed). Three signal-shaped
  ops gained the missing TCB-stability `ensures` (`forall k. bind_slots` fixed + the TCB domain):
  `channel::fire`, `channel::endpoint_cap_dropped`, `notification::remove_waiter` (and
  `channel::release_binding` gained the bundled `home_views_frozen`) — they keep `bind_slots`
  (a getter-only field) but did not previously expose it.

- **`revoke`'s conditional root-survival theorem (`cspace.rs`).** `revoke` deletes only descendants
  of `slot`, never `slot` itself (the deleted `leaf` is childless while `slot` has a child — the
  loop guard — so `slot != leaf`). So `slot` can be emptied only by a **cross-object** teardown
  reaching a homing object. Under the precondition **`!is_homed(slot)`** (the revoke root is no
  object's home handle — a top-level / root-cspace cap), `unhomed_frozen` makes survival a theorem:
  the new `ensures !is_empty_cap(final.slot_view()[slot].cap)`. `is_homed(slot)` is immutable across
  the walk (`home_views_frozen`), so the precondition rides the loop.

- **Host-checked both ways (`test_store.rs`).** `check_revoke_root_survives` runs the real `revoke`
  on a **non-zombie** shape (an un-homed `slot 0` whose child is the last cap to a cspace whose
  resident is a *different* slot) — the cross-object teardown fires (the resident is emptied) yet
  `slot 0` survives. `revoke_can_empty_its_own_root_zombie` (doc 23 §4) stays as the precondition's
  **negative witness**, now asserting `is_homed_exec(slot 0)` (the zombie root *is* a cspace
  resident, so it fails `!is_homed`). The executable `is_homed_exec` mirror checks the precondition.

---

## 2. Findings worth keeping

- **The frame `revoke` needs is the *provenance* of emptying, not `only_empties`.** The cluster
  already proved `only_empties` (teardown never *fills* a slot), but that is silent on *which*
  slots empty — so it could not rule out a cross-object teardown emptying `slot` (doc 23 §4's gap).
  `unhomed_frozen` adds exactly the missing fact: emptying is confined to home handles. The
  realization that turned a hard reachability problem into a mechanical frame is that the three
  home maps are *immutable* across teardown, so "is `x` a home handle" is a stable, decidable
  property — no CDT-subtree reachability needed.

- **`homed_in_chan` must NOT require `ring_cap.dom()` membership.** The first cut required
  `ring_cap.dom().contains(k) && ring_cap[k] == x`; proving the domain membership at the
  `destroy_channel` ring-cap delete needed `chan_wf`'s ring-cap-dom clause with a `u32`→`nat`
  bound bridge that fought the trigger. Dropping it to just `ring_cap[k] == x` (the value the
  `chan_ring_cap` getter already pins) is **sound** — the value equality is what the frame uses,
  and `is_homed` stability rides `chan_struct_frame`'s per-channel `ring_cap` equality regardless
  of `k`'s domain status — and made the witness a three-line assert.

- **Signal-shaped ops expose `bind_slots` stability for their *subject* only; the home frame needs
  it globally.** `remove_waiter`/`signal` ensured `bind_slots[t]` fixed for the woken/spliced `t`;
  `home_views_frozen` needs `forall k. bind_slots[k]` fixed (a frozen home map for *every* TCB).
  Adding the global `forall` was free (these ops write only queue/wait links, never `bind_slots`),
  but it had to be stated — the per-subject form did not compose. The lesson generalizes: a *frame*
  predicate over all keys cannot be assembled from per-subject postconditions.

- **Clear-before-recurse keeps the home frame target-free.** Each destructor's directly-deleted
  handle (a resident / ring cap / bind slot) is itself homed, so `delete`'s target-aware
  `unhomed_frozen(·, ·, target)` lifts to the destructor's target-free `unhomed_frozen_free` by a
  one-line `lemma_unhomed_frozen_free_from_homed` — no per-destructor exemption set that would have
  to grow up the recursion (the trap that nearly forced TCB bind slots out of `is_homed`).

- **The whole frame composes with no rlimit pressure.** Threaded as eight named `proof fn`s plus
  small per-segment `home_views_frozen`/`unhomed_frozen_free` establishments (the doc-54 §2
  discipline: lemma calls, not inline multi-term `forall`s), `destroy_tcb` — the rlimit-sensitive
  body — stayed green with no bump.

---

## 3. The recorded residue (the faithful resident-with-external-reference theorem)

> **Closed in doc/results/72** (D-A2). The `emptied_via_dead_home` frame and the refs-monotone
> *dead-stays-dead* argument described below landed: `revoke` now exports
> `is_empty_cap(final[slot]) ⟹ ∃o. o homes slot ∧ o destroyed`, the caller-completable form of
> the faithful theorem. (The death model is the disjunction `o ∉ refs.dom ∨ refs[o] == 0`, since
> the cspace/channel/TCB destructors leave their object in `refs.dom` at `refs == 0` — see doc 72
> F-72-1. The conservative `!is_homed` theorem remains the sound floor beside it.)

`revoke`'s precondition is the plan §6e **conservative** form (`slot` homed by no object). The
**faithful** form — a `slot` that *is* a cspace resident but whose homing cspace keeps a live
reference *outside* `slot`'s subtree also survives — is **not** closed here, and is the explicit
follow-on (the plan §3 cross-object-cascade risk, the user-approved "attempt full, record fallback").

The obstruction, precisely: `unhomed_frozen` protects only **un-homed** slots. To protect a *homed*
`slot` whose homing object survives, one needs the strictly stronger frame **"a non-target slot is
emptied ⟹ some object that homes it was destroyed (its `refs` reached 0)"**, plus a refs-monotone
("dead stays dead") argument so a destroyed home in an inner teardown step stays destroyed in the
outer final state. That frame — call it `emptied_via_dead_home` — would let `revoke` carry the loop
invariant "every cspace homing `slot` keeps `refs ≥ 1`" (witnessed by an un-homed external cap that
`unhomed_frozen` already proves survives), and conclude `slot` survives by contraposition. The
`is_homed` / `home_views_frozen` machinery this PR lands is exactly its foundation; the additional
work is the `refs`-monotone provenance threading through the cross-module cluster (a 6d-scale
effort). Recorded here, not silently dropped — the conservative theorem is the sound floor and the
zombie/non-zombie host pair pins the precondition's faithfulness.

---

## 4. Doc / CLAUDE.md

No `CLAUDE.md`/spec edit this increment — per the doc-30 §3 / doc-54 §4 convention the phase-6
closeout (the proven-list move, the retired trusted-residue note, the master-plan §7 renumber)
rides **6f** (the `refcount_sound`-on-construction-ops system invariant + documentation closeout).
After 6f, `delete`/`revoke`(survival)/`obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace`/
`channel::destroy_channel`/`thread::destroy_tcb` move onto the proven list; this increment makes
`revoke`'s survival clause real (modulo the §3 residue). `cargo verus verify -p kcore` runs with no
per-proof filter, so the eight new lemmas, the strengthened cluster contracts, and `revoke`'s new
`ensures` all auto-gate; `host-tests` reruns `check_revoke_root_survives` + the annotated zombie
witness.

**Doc-numbering note.** Plan §6-detail budgeted 6e as findings doc **45**; it lands as doc **55**
(6d having spanned docs 44–54, doc 54 §4). **Next: 6f** (the system invariant on the construction
ops + the phase-6 closeout) — the last sub-phase of phase 6.
