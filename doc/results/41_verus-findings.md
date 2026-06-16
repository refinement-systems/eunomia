# Verus findings 21 — Phase 6a: the `refcount_sound` census + `cspace_view` residency + seam contracts

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT refcount row, §3.2 the
no-global-pool discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6a"). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`40` (phase 5 — sysabi + the
aspace walker + the TLBI effect log). This is the **first sub-phase of phase 6**, the
cross-object-teardown phase: a *wide-but-shallow* foundation that stands up the full
refcount census and the residency view the teardown cluster (6b–6f) consumes, lands no
op **body** proof, and is designed-heavy / solver-light by intent (detail §2 "6a").

**Outcome.** `cargo verus verify -p kcore`: **234 verified, 0 errors** (was 225 after 5e;
`+9` — the nine single-key recount lemmas). `cargo test -p kcore`: **69 passed** (was 68;
`+1` — `refcount_sound_exec_has_teeth`, the per-term census-mirror teeth test; the
strengthened contracts' new clauses are also checked in-line by the existing
`check_delete`/`check_destroy_channel`/`check_destroy_tcb`). The aarch64 `kernel`
cross-build is unchanged (ghost erasure; every change is a `requires`/`ensures`/`spec`/
`proof` addition or a contract on an already-existing seam method — no `exec` body moved).

**6a adds no body proof and no new `external_body`.** The three teardown destructors
(`delete`, `channel::destroy_channel`, `thread::destroy_tcb`) stay `external_body`; only
their *contracts* grow (host-checked). The plain-Rust `obj_unref`/`destroy_cspace`/
`unref_cspace`/`unref_aspace` are untouched — they move into `verus!{}` in 6b/6c.

---

## 1. What landed

- **The census spec (`refcount_sound`/`obj_census`), six terms assembled.** `refs[o]` must
  equal `obj_census(store, o)`, the recount over every reference to `o`:
  - `slot_refs` (already existed, phase 2): cspace residents + channel ring caps + TCB bind
    caps, all in the one slot arena.
  - `binding_refs(chan_view, o)`: the `(ch, end, ev)` triples whose binding names `o`,
    encoded as `Set::new(|t| …).len()` over the triple space (the §3.6 binding term).
  - `waiter_refs(notif_view, tcb_view, o) = waiter_seq(o).len()` (the phase-4 waiter term).
  - `armed_timer_refs(timer_view, o)`: armed timers bound to `o` (the phase-4e term).
  - `frame_map_refs(slot_view, o)`: mapped Frame caps whose mapping targets aspace `o` — the
    **new phase-5-enabled term**. A mapped frame holds its aspace ref through the mapping
    field, *not* via cap designation (`cap_obj` is `None` for a Frame, and `obj_ref`/
    `obj_unref`'s Frame arm is a no-op), so it is a census term distinct from `slot_refs`.
    Mirrored by the new `cap_frame_aspace` spec projection.
  - `thread_hold_refs(tcb_view, o)`: a bound thread holds one ref on its cspace and one on
    its aspace (released by `destroy_tcb`'s `unref_cspace`/`unref_aspace`).
  - `obj_census` and `refcount_sound` are **store-generic** (`<S: Store>`, calling the view
    spec methods on `S`) — the first store-generic spec fns in the codebase. The established
    idiom passes views as `Map` arguments (`chan_wf`, `binding_refs_ok`); the store-generic
    form verifies cleanly and keeps the teardown contracts readable (`refcount_sound(store)`).

- **The `cspace_view` residency view — the one new `Store`-seam view (the 4a/5c analog).** A
  `CSpaceView { num_slots: nat, slots: Seq<SlotId> }` and a `spec fn cspace_view(&self) ->
  Map<ObjId, CSpaceView>`, with the two previously-uncontracted getters `cspace_num_slots`/
  `cspace_slot` (in the plain `Store` trait only) promoted to **contracted** `ExStore`
  declarations against it. Residency is immutable (the getters have no setter), so it is an
  immutable projection like `ChanView.ring_cap` / `TcbView.bind_slots`. `destroy_cspace`'s
  resident loop (6c) and revoke-root-survival (6e) name "the slots `cs` owns" through it.

- **The `aspace_destroy`/`aspace_unmap` seam contracts (assumed, host-checked — the
  `make_runnable` precedent).** Shell-owned page-table ops kcore never sees the body of:
  `aspace_unmap` frames every object view + `refs_view` + `cspace_view` (page-table
  maintenance, no object state; the TLBI log it may touch stays unconstrained like the other
  hardware effects); `aspace_destroy` (the last-ref teardown, `requires refs[a] == 0`) drops
  `a` from `refs_view` (`ensures refs_view() == old.refs_view().remove(a)`) and frames the
  rest. `ArrayStore::aspace_destroy` was made faithful (it now `refs.remove(a)`s).

- **Nine single-key recount lemmas — the settled API 6b–6f compose.** Bump/drop for each
  *single-domain* term, each the proven `lemma_designation_bump` shape over a different view:
  `lemma_designation_drop` (slot), `lemma_frame_map_bump`/`_drop`, `lemma_armed_timer_bump`/
  `_drop`, and the thread-hold pair `lemma_thread_hold_{cspace,aspace}_{bump,drop}` (each
  frames the untouched half at the edited key). "A one-key view edit raises/lowers exactly
  one census term by one, the others fixed."

- **The teardown cluster's strengthened (still-`external_body`) contracts.** `delete`,
  `destroy_channel`, `destroy_tcb` now **require and preserve `refcount_sound`**, and the two
  destructors state the `count_nonempty` non-increase 6d's measure needs. Stated against the
  *final* contracts now (the detail-§0 discipline) so 6b/6c verify against them and 6d's body
  closure adds no caller churn.

- **The census mirror with teeth (`test_store.rs`).** `refcount_sound_exec` recomputes all
  six terms over the concrete `ArrayStore` and checks `refs[o] == census(o)`. The new
  `refcount_sound_exec_has_teeth` test builds an all-six-terms sound fixture (a positive
  witness) and perturbs each term in isolation (one negative witness per term), so the mirror
  is demonstrably non-vacuous. The strengthened teardown contracts' census clause is
  host-checked in `check_delete`/`check_destroy_channel`/`check_destroy_tcb` (guarded on the
  precondition, since most generated forests carry no object caps and are vacuously sound).

---

## 2. Findings worth keeping

- **The cluster's `requires refcount_sound` cascades through `delete`'s verified callers —
  the one piece of caller churn the detail glossed.** `delete` is `external_body`, but two
  *verified* ops call it: `thread::bind` (the displaced-bind-cap teardown) and `revoke` (its
  revocation loop). Adding `requires refcount_sound` to `delete` forces both to establish it.
  The churn is cheap and terminates at the trusted shell:
  - `bind` gains one `requires` line — `delete` is its **first** mutation, so the census holds
    unmutated from entry to that call (no invariant maintenance).
  - `revoke` gains one `requires` line + one loop **invariant** `refcount_sound(store)`, which
    `delete`'s (assumed) `ensures refcount_sound(final)` re-establishes each iteration for free.
  - Neither `bind` nor `revoke` has a verified kcore caller, so the cascade stops at the
    kernel shell (which maintains `refcount_sound` as the system invariant 6f makes a theorem).
  This is the §1.3 underflow gate made concrete: the census is the precondition that will make
  `delete`'s body (6d, `obj_unref`'s `refs - 1`) verifiable, so it must thread through every
  verified caller — and doing it in this structural PR keeps 6d a pure body-proof.

- **The `cspace_view` sweep is forward-looking, not a census dependency.** Every `&mut self`
  `ExStore` mutator (29 setters + `make_runnable` + the barriers/TLBI) gains a
  `cspace_view() == old` frame clause — a purely additive, proof-light sweep (the phase-2…5
  proofs stay green; doc 27 §1's additive-frame argument). It is **not** needed by
  `refcount_sound` (whose terms are over the slot/chan/notif/tcb/timer views — residency is
  not a census term); it is needed by `destroy_cspace`'s resident loop (6c) and
  revoke-root-survival (6e), which read residency across the teardown ops' internal setter
  calls (those bodies land in 6d). Banking the churn here keeps 6c/6d focused on proof. This
  is the opposite call from `tlb_log_view` (phase 5e), which was deliberately *not* swept
  because no op interleaves an object setter with a TLBI — `cspace_view` *is* swept because
  the teardown ops mutate while walking residency.

- **The single-domain recount lemmas reuse the phase-2 idiom verbatim; the binding term does
  not.** Five of the six terms (`slot`, `frame-mapping`, `armed-timer`, `thread-hold` ×2) are
  filters over a single map domain, so their bump/drop lemmas are `lemma_designation_bump`
  clones (filter `=~=` `insert`/`remove`, `len ±1`) and verify on first try. `binding_refs`
  counts over a **nested** domain (`(ch, end, ev)` triples), so its single-edit recount needs
  the triple set's finiteness (a subset of `cv.dom() × {0,1} × {0,1,2}`) — the doc-35 §2.6
  n²-trigger hazard the plan flags (§3). Per the "count steps single-purpose, where consumed"
  discipline it is **deferred to 6d**, the sub-phase whose `destroy_channel` body consumes it
  (the binding release loop). Recorded, not silently dropped — the five settled terms plus the
  finiteness-deferred sixth are the recount API 6b–6f compose against.

- **`obj_census` is store-generic and reads `store.slot_view()` twice** (once for `slot_refs`,
  once for `frame_map_refs`). Verus accepts trait spec methods called on a generic `S: Store`
  in an `open spec fn` without friction — useful for keeping the seven-view census legible.

---

## 3. What 6a sets up

- **6b (aspace teardown):** `unref_aspace` + the `delete` frame-unmap branch consume
  `lemma_frame_map_drop` + the `aspace_destroy` contract — both landed here.
- **6c (`obj_unref`/`destroy_cspace`/`unref_cspace`):** the cspace-resident path verifies
  against `delete`'s final (census-bearing) contract and the `cspace_view` residency.
- **6d (the cross-module cycle):** the `delete`/`destroy_channel`/`destroy_tcb` body proofs
  inherit the final contracts unchanged (no caller churn) and land the binding-term recount.
- **6e (revoke root-survival):** `cspace_view` residency makes the non-zombie precondition
  stateable.
- **6f (system invariant + closeout):** the construction ops gain `refcount_sound` preservation
  using their landed deltas, and `CLAUDE.md`/spec edits land (no `CLAUDE.md` edit this
  sub-phase — detail §2 "6a").

No spec-doc / `CLAUDE.md` edit this sub-phase (the doc-30 §3 "spec edits ride the closeout"
convention; the phase-6 closeout is 6f).
