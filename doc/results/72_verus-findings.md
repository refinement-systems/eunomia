# 72 — Follow-on-fix ledger: D-A2 (revoke root-survival, the resident-with-external-reference theorem)

> A *change* ledger continuing docs 70 (D-A1 split · D-B1 stage 1) and 71 (D-B1 Option 2).
> This increment closes **D-A2** — both the documented §2.2/§2.5 spec note and the deeper
> *resident-with-external-reference* survival theorem that doc 70 left as follow-on and
> `doc/results/55 §3` tracked as the refs-monotone residue.

## Provenance

D-A2 (doc 69, Class D2, medium): `revoke`'s root-cap **survival** is narrower than §2.5's
reclaim/grant patterns silently assume. Two layers, both closed here:

1. **Contract-level gap** — already closed as a byproduct of **D-A1** (doc 70): `revoke`
   dropped `requires !is_homed` and exported the conditional
   `ensures !is_homed(old) ==> !is_empty_cap(final[slot].cap)`. That made the gap
   *contract-faithful* (survival guaranteed for un-homed roots; the seL4-zombie self-empty
   admissibly silent), but survival for a **homed** root whose homing object keeps a live
   reference *outside* the revoked subtree was left unproven (`doc/results/55 §3`,
   "`emptied_via_dead_home` + a refs-monotone *dead-stays-dead* argument", flagged a
   6d-scale effort under the user-approved "attempt full, record fallback").
2. **The spec prose** — D-A2's literal disposition: §2.2/§2.5 gain an honest root-survival
   note. Not previously in the spec.

**Headline:** both land. The §2.2/§2.5 note is added; and the resident-with-external-
reference theorem is now **Verus-verified** via a dual of the `unhomed_frozen` provenance
frame. `revoke` exports a new
`ensures is_empty_cap(final[slot].cap) ==> exists o. homes(old, o, slot) && dead_obj(final, o)`
— "if the revoked root was emptied, some object that homed it was destroyed." A caller that
keeps any homing object alive (the reclaim flow: the granter's cspace outlives the revoked
subtree) concludes the root survives by contraposition. Verified: **327 verified, 0 errors**
(`cargo verus verify -p kcore`, +9 vs the doc-70/71 baseline of 318), **89** host tests
(+1 witness), AArch64 `kernel` cross-build clean. Runtime behaviour unchanged — every edit is
ghost / a contract.

## The change

| Layer | Edit |
|---|---|
| Spec (`doc/spec/2_spec_rev2.md`) | §2.2: parenthetical scoping the "unconditional" guarantee to *descendant-deletion* (D-A1), distinct from *root-survival*. §2.5: a "Root-survival across `revoke`, recorded honestly (D-A2)" note — un-homed survival verified, seL4-zombie admissible, resident-with-external-reference now verified. |
| New frame (`kcore/src/cspace.rs`) | object-indexed `homes_in_{cspace,chan,tcb}` / `homes`; `dead_obj`; `emptied_via_dead_home` (target-aware) + `_free`; `refs_death_persist` ("dead stays dead"). |
| Lemmas | `lemma_is_homed_iff_homes`, `lemma_homes_stable`, `lemma_refs_death_persist_{from_refs_eq,trans,dec_ref}`, `lemma_emptied_via_dead_home_free_{from_slot_eq,from_homed,trans}`, `lemma_emptied_via_dead_home_compose` — mirroring the `unhomed_frozen` set. |
| Cluster threading | the dual frame (`emptied_via_dead_home_free` + `refs_death_persist`) added to the `ensures` of and composed across `delete`, `obj_unref`/`dec_ref`, `destroy_cspace`, `unref_cspace`, `unref_aspace` (cspace.rs); `destroy_channel`, `release_binding`, `endpoint_cap_dropped`, `fire` (channel.rs); `destroy_tcb` (thread.rs); `signal`, `remove_waiter` (notification.rs); `destroy_timer` (timer.rs). `destroy_channel` gained one precondition `dead_obj(old, ch)` (discharged at its only at-zero call site). |
| `revoke` (cspace.rs) | new `ensures` (above) + a slot-specific loop invariant `is_empty_cap(store[slot]) ==> exists o. homes(old, o, slot) && dead_obj(store, o)`, maintained per `delete` step (freshly-emptied `slot` via the target-aware frame since `slot != leaf`; already-empty via death-persistence). The existing `!is_homed` floor is untouched. |
| Test (`test_store.rs`) | `check_revoke_root_survives_homed_external_ref` — a homed `slot 0` (resident of cspace 10) with an external un-homed cap to 10 (`refs[10]=2`); after `revoke`, `refs[10]=1`, cspace 10 never dies, `slot 0` survives. The contrapositive witness. |

## Findings worth keeping

### F-72-1 — The death model is a disjunction, not `∉ refs.dom` (correction)

The natural reading of "the homing object was destroyed" — `!s.refs_view().dom().contains(o)`
— is **unprovable for the dominant case**. Only `aspace_destroy` removes an object from
`refs.dom`; `destroy_cspace` / `destroy_channel` / `destroy_tcb` / `destroy_notif` /
`destroy_timer` all leave their object **in** `refs.dom` at `refs == 0` (and
`home_views_frozen` even keeps a destroyed cspace nominally homing its now-emptied residents).
Since the homing objects are exactly cspaces / channels / TCBs, the sound predicate is the
disjunction `dead_obj(s, o) := o ∉ refs.dom || refs[o] == 0` — which matches `doc/results/55
§3`'s own wording ("its `refs` reached 0"). The disjunction is monotone across the whole
cluster (`refs_death_persist`): no teardown op re-refs or re-adds a dead object.

### F-72-2 — Caller-completable, with no CDT-subtree reasoning (the design that closes it)

The "external reference keeps the home alive" half is genuinely a *system-level* fact (the
granter's cspace is kept alive by thread holds and other caps, none inside the revoked
subtree), and the codebase has **no CDT-subtree/descendant predicate** by design (doc 55: the
`unhomed_frozen` frame was built precisely to avoid subtree reachability). So `revoke` does
**not** try to prove "the external cap is outside the subtree." It exports the dual provenance
fact — *emptied ⟹ a homing object died* — and leaves "the homing objects stay alive" to the
caller. This sidesteps the obstruction that made the literal "refs ≥ 1 loop invariant" form
6d-scale, and is the faithful split: kcore proves the mechanism, the system supplies the
liveness of the funder.

### F-72-3 — A dual of `unhomed_frozen`, threaded the same way (mirrors D-A1/F-70-8)

`unhomed_frozen` says *un-homed slots keep their cap*; `emptied_via_dead_home` says *a slot
that got emptied was a home handle of a destroyed object*. They reason about the **same**
event (a destructor clearing its home handles), so the threading structure — leaf-establish,
compose-across-recursion, lift-target-aware-to-free-when-the-deleted-handle-is-homed — is
identical, and needed **no** new machinery beyond the refs-monotone `refs_death_persist` (the
one genuinely new ingredient, the "dead stays dead" the cross-object cascade required). Same
lesson as F-70-8: the missing piece was an unstated frame, not an unsound body.

### F-72-4 — Witness correspondence is exact

The two homed-root host tests now map onto the new `ensures`' two outcomes:
`revoke_can_empty_its_own_root_zombie` (the homing cspace's *last* cap inside the subtree →
`refs→0` → destroyed → root self-empties → the `ensures`' existential is witnessed) and
`check_revoke_root_survives_homed_external_ref` (an external un-homed cap keeps `refs ≥ 1` →
no homing object dead → by contraposition the root survives). The `!is_homed` floor's
un-homed witness `check_revoke_root_survives` is unchanged.

## Verification evidence

| Gate | Command | Result |
|---|---|---|
| Proof (primary) | `cargo verus verify -p kcore` | **327 verified, 0 errors** (318 baseline + 9) |
| Witnesses | `cargo test -p kcore` | **89 passed, 0 failed** (incl. `check_revoke_root_survives_homed_external_ref`) |
| Shell build | `cd kernel && cargo build` | clean (AArch64 bare-metal; ghost/contract edits erase) |

## Disposition feed-forward

- **D-A2:** **closed.** Contract-level under D-A1 (doc 70); the §2.2/§2.5 spec note and the
  resident-with-external-reference theorem here. The doc-70 D-A2 feed-forward line ("the
  resident-with-external-reference residue stays follow-on (doc/results/55)") is superseded.
- **`doc/results/55 §3` residue:** **closed** — the `emptied_via_dead_home` frame + the
  refs-monotone "dead stays dead" argument it called for are landed and verified. The
  conservative `!is_homed` theorem remains the sound floor; the faithful theorem now sits
  beside it.
- **D-A3** ("revoke sees through queues" as a named `ensures`/lemma + a queued-descendant
  test on real `revoke`) is unaffected and remains the open revoke follow-on (doc 69 / doc 70).
