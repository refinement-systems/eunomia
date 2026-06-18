# Verus findings 22 — Phase 6b: aspace teardown (`unref_aspace` + the frame-mapping census term)

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT refcount row, §3.2 the
no-global-pool discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6b"). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`40` (phase 5 — sysabi + the
aspace walker + the TLBI effect log), `41` (phase 6a — the `refcount_sound` census +
`cspace_view` residency + the seam contracts). This is the **second sub-phase of phase
6**: the separable, non-recursive teardown win the foundation (6a) enabled — `unref_aspace`
ported into `verus!{}`, and the `delete` frame-unmap-branch census lemma landed for 6d.

**Outcome.** `cargo verus verify -p kcore`: **238 verified, 0 errors** (was 234 after 6a;
`+4` — `unref_aspace` + three recount lemmas). `cargo test -p kcore`: **73 passed** (was
69; `+4` — two `check_unref_aspace` cases and two mapped-frame `check_delete` cases). The
aarch64 `kernel` cross-build is unchanged (ghost erasure; moving a plain-Rust fn into
`verus!{}` and adding `requires`/`ensures` moves no `exec` code — the erased body is
byte-identical to the prior plain Rust).

**6b adds no new `external_body` and removes none.** `delete`/`destroy_channel`/
`destroy_tcb` stay `external_body` (their bodies are 6d). `unref_aspace` moves from the
plain-Rust refcount-plumbing cluster into `verus!{}` as the **first teardown op proven**;
the rest of that cluster (`obj_unref`/`unref_cspace`/`destroy_cspace`) is 6c/6d.

---

## 1. What landed

- **`unref_aspace` proven against the 6a `aspace_destroy` seam contract + the census.**
  The body is unchanged (`set_obj_refs(a, obj_refs(a) - 1)`; at zero, `aspace_destroy(a)`).
  It is the **non-recursive** teardown op — an aspace owns page tables, not caps, and
  `aspace_destroy` is a shell-owned seam black box (the trusted base, plan §2), so it
  closes **without** the cross-module recursion cluster (6c/6d). This is the 3a/5a
  confidence-builder of phase 6, here on real teardown rather than a slice-free island.

- **The off-by-one census precondition — forced by `delete`'s body order.** The caller
  (`delete`'s frame-unmap branch at `cspace.rs`; `destroy_tcb`'s aspace release) clears the
  mapping/hold that named `a` *before* calling `unref_aspace` (the deleted slot's cap is
  emptied first, lowering `frame_map_refs(a)`), so at entry `a`'s census has **already
  dropped by one** while `refs[a]` has not: `refs[a] == obj_census(a) + 1`, sound at every
  other object. `unref_aspace`'s `-1` lands the matching decrement, restoring the full
  `refcount_sound` invariant at exit. The detail (§2 "6b") anticipated this — "the term
  drops by one when the caller removed the mapping/binding, which the contract takes as the
  matching precondition." The off-by-one is *not* an arbitrary interface choice: it is
  dictated by the existing `delete` body, which clears the slot (`s.cap = EMPTY; set_slot`)
  before reaching the `Frame { mapping: Some((asp, va)) }` arm that calls
  `aspace_unmap` + `unref_aspace`. `refs[a] > 0` is the underflow gate for `obj_refs(a) - 1`
  (the §1.3 obligation), discharged by the precondition.

- **The proof is light — the census is invariant across the op, so no recount is needed
  inside `unref_aspace`.** `obj_census` reads only the seven object views (slot/chan/notif/
  tcb/timer/timer-head/cspace), **never `refs_view`**; and both `set_obj_refs` and
  `aspace_destroy` frame all seven views unchanged. So `obj_census(final, o) ==
  obj_census(old, o)` for every `o` follows definitionally from the frame clauses (an
  `open spec` over unchanged inputs) — the per-term recount lemmas (the `lemma_*_bump`/
  `_drop` family) are for the *slot-clearing* teardown ops (6d), not for `unref_aspace`,
  which touches no census view. The body then case-splits on `obj_refs(a) - 1 == 0`: the
  non-zero branch restores `refs[a] == census(a)` from the off-by-one precondition; the
  zero branch discharges `aspace_destroy`'s `refs[a] == 0` precondition and `a` leaves
  `refs_view().dom()`, so its soundness clause is vacuous. The two map-extensionality
  hints — `(old.insert(a,0)).remove(a) =~= old.remove(a)` and the soundness `assert
  forall` — are the whole proof body.

- **The `delete` frame-unmap-branch census lemma — landed for 6d, `delete` stays
  `external_body`.** A standalone `lemma_frame_clear_census(m, k, v, asp)`: replacing a
  mapped Frame slot `k` (target aspace `asp`) with a non-designating, non-targeting cap `v`
  (an empty cap qualifies) drops `frame_map_refs(asp)` by one and **fixes every other
  slot-view census term** — `slot_refs(o)` for *all* `o` (a Frame designates no object,
  `cap_obj` is `None` on both sides) and `frame_map_refs(o)` for `o != asp`. It composes
  the proven `lemma_frame_map_drop` (6a) with two new single-key "unchanged" helpers,
  `lemma_nondesignating_edit_slot_refs` and `lemma_nontargeting_edit_frame_map` — the
  `lemma_same_caps_same_census` analog for a single *changed* key whose designation/target
  of `o` is absent on both sides (a pure set-extensionality `=~=` step, no finiteness
  needed). 6d's `delete` body consumes this lemma for the frame branch; the four non-slot
  census terms ride `set_slot`'s view-frame at the call site.

- **Host checks (`test_store.rs`).** `check_unref_aspace` re-derives the off-by-one
  precondition from the concrete `ArrayStore`, calls the real body, and asserts the `-1` /
  last-ref-destroy split + `refcount_sound` restored — the executable check of the assumed
  `aspace_destroy`/`aspace_unmap` seam contracts against `ArrayStore`. Two tests drive it
  (`unref_aspace_non_last_decrements` off the all-terms `refcount_sound_fixture` with
  `refs[A]` bumped by one; `unref_aspace_last_ref_destroys` on the sole-dangling-ref store).
  Two more drive the **real `delete` body** down its `aspace_unmap` + `unref_aspace` path
  via a `mapped_frame_fixture` (`delete_mapped_frame_drops_aspace_ref` — the aspace ref
  drops, the object survives; `delete_last_mapped_frame_destroys_aspace` — the last frame
  fires `aspace_destroy`). The generic `check_delete` already asserts `refcount_sound`
  preserved when the fixture was sound, so these add only the aspace-specific outcome.

---

## 2. Findings worth keeping

- **No verified-caller churn this sub-phase — the off-by-one precondition stops at the
  trusted boundary.** Moving `unref_aspace` into `verus!{}` with a richer (off-by-one
  census) precondition could in principle force every caller to establish it. But its only
  kcore callers are `delete`'s frame-unmap branch and `destroy_tcb`'s aspace release — both
  **`external_body`** this phase (their bodies are opaque, so the call is unchecked) — and
  the unverified kernel shell (`kernel/src/cspace.rs`). So, unlike 6a's `delete`-`requires`-
  `refcount_sound` cascade (which churned `bind`/`revoke`, doc 41 §2), 6b adds **zero**
  caller churn. The precondition is established by `delete`/`destroy_tcb` only when their
  bodies become checked (6d) — exactly where the slot-clear that creates the off-by-one
  state is visible. This is why the aspace teardown is genuinely separable (detail §1.1).

- **`unref_aspace` is the one teardown op whose proof needs *no* recount lemma — because
  it touches no census view.** Every *other* teardown body edits the slot arena (clearing
  a cap) and so must recount `slot_refs`/`frame_map_refs` with the `lemma_*_drop` family.
  `unref_aspace` edits only `refs_view` (and, at zero, drops `a` via `aspace_destroy`,
  which frames all views), and the census is defined over the *other* seven views, so its
  recount is the identity. This is the cleanest possible instance of the 6a design
  decision that **`refs_view` is not a census term** (doc 41 §1: "`refcount_sound` does not
  read residency"; here, more sharply, it does not read `refs_view` either) — the census is
  a recount of *references*, and the stored count is what it must equal, so the census can
  never read the stored count without circularity. `unref_aspace`'s lightness is the payoff.

- **The off-by-one precondition is the shape the whole teardown family will use at every
  `obj_refs(o) - 1`.** `unref_aspace` is the first ref-dropping op proven, and its
  precondition — *sound except `a` is one high, because the caller already removed `a`'s
  reference* — is the template `obj_unref`/`unref_cspace` (6c) and the `destroy_*` bodies
  (6d) will each instantiate per object. Banking it here on the simplest term (a single
  frame mapping, no recursion) settles the interface before the cross-module cluster has to
  thread it through a recursion (the 3b→3d / 4a→4b "settle the model before the op"
  discipline). The named view-frame `ensures` (all seven views unchanged) is what lets a
  6d caller treat `unref_aspace` as census-transparent across the call.

- **The two "unchanged" helpers fill the gap `lemma_same_caps_same_census` left.** The
  phase-2 census-frame lemma (`lemma_same_caps_same_census`, doc 21) frames `slot_refs`
  when the edit changes *no* cap (link-only edits). A teardown *clear* changes the cap, so
  it does not apply; the per-term `_drop` lemmas (6a) cover the *changed-designation* key.
  The missing case — a key whose cap changes but whose designation/target of a *given* `o`
  is absent on both sides — is exactly what `delete`'s frame branch needs to frame the
  *other* objects' census while one object's term drops. The two new helpers are that case,
  one per slot-view term; they are the last small recount primitives the slot-clearing
  teardown bodies (6d) need, landed here while the frame branch is the thing under test.

---

## 3. What 6b sets up

- **6c (`obj_unref`/`destroy_cspace`/`unref_cspace`):** the off-by-one precondition shape
  and the "census-transparent `-1`" view-frame `ensures` are reused per object; the
  cspace-resident path verifies against `delete`'s 6a contract and the `cspace_view`
  residency (6a).
- **6d (the cross-module cycle):** the `delete` body's frame-unmap branch consumes
  `lemma_frame_clear_census` (drop `frame_map_refs(asp)`, fix every other slot-view term)
  + `unref_aspace`'s proven contract — the frame-mapping term's contribution to `delete`'s
  `refcount_sound` preservation is fully assembled, leaving 6d the binding/slot/thread-hold
  terms and the seL4-zombie `decreases`.
- **6e (revoke root-survival):** unaffected by 6b (the residency view it needs is 6a's).
- **6f (system invariant + closeout):** the frame-mapping construction side (`map_in`'s
  aspace ref) gains `refcount_sound` preservation using the same `frame_map_refs` deltas.

No spec-doc / `CLAUDE.md` edit this sub-phase (the doc-30 §3 "spec edits ride the
closeout" convention; the phase-6 closeout is 6f).
