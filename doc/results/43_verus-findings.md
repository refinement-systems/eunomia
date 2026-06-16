# Verus findings 23 — Phase 6c: `obj_unref` / `destroy_cspace` / `unref_cspace` (the opaque-`delete` cluster members)

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT refcount row, §3.2 the
no-global-pool discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6c"). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`40` (phase 5 — sysabi + the
aspace walker + the TLBI effect log), `41` (6a — the `refcount_sound` census +
`cspace_view` residency + the strengthened-`external_body` cluster contracts), `42`
(6b — the non-recursive `unref_aspace` + the frame-mapping census term). This is the
**third sub-phase of phase 6**: the three teardown members that recurse only through
the *opaque* `delete` — `obj_unref`, `unref_cspace`, `destroy_cspace` — ported into
`verus!{}` and proven, the census-preservation discipline landed on the cspace-resident
path.

**Outcome.** `cargo verus verify -p kcore`: **244 verified, 0 errors** (was 238 after
6b; `+6` — the disarm-shaped recount lemma `lemma_armed_timer_disarm`, the shared
`dec_ref` helper, `destroy_cspace`, `obj_unref`, `unref_cspace`, and the strengthened
`destroy_timer`). `cargo test -p kcore`: **79 passed** (was 73; `+6` — two
`destroy_cspace` cases incl. the nested cross-object recursion, two `unref_cspace`
cases, two `obj_unref` cases). The aarch64 `kernel` cross-build is unchanged (ghost
erasure; moving plain-Rust fns into `verus!{}` and matching `cap.kind` per-arm instead
of calling the now-removed `Cap::obj()` leaves the erased exec behaviour identical —
decrement the object's refs, dispatch the destructor at zero).

**6c adds no new `external_body` and removes none from the cluster.** `delete`,
`channel::destroy_channel`, `thread::destroy_tcb` stay `external_body` (their bodies are
6d — the cross-module cycle). `obj_unref`/`unref_cspace`/`destroy_cspace` move from the
plain-Rust refcount-plumbing cluster into `verus!{}`; with `delete` still opaque, Verus
sees **no recursion cycle**, so they verify against `delete`'s contract under plain
index-countdown loops — **no `decreases` / no termination measure anywhere in 6c**.

---

## 1. What landed

- **The three opaque-`delete` cluster members proven, no cycle visible.** `destroy_cspace`
  walks `cs`'s residents (through the immutable `cspace_view`) and calls the **opaque**
  `delete` on each non-empty one; `obj_unref` decrements an object's refcount and, at zero,
  dispatches the type-specific destructor; `unref_cspace` is `obj_unref`'s CSpace arm in
  isolation (the non-cap holder path a bound thread's `destroy_tcb` releases). Because
  `delete`/`destroy_channel`/`destroy_tcb` are still `external_body`, none of these recurses
  *visibly*: `destroy_cspace`'s loop measure is the resident-index countdown (`n - i`), and
  `obj_unref`/`unref_cspace` are non-looping. This is exactly detail §1.5's plan — close the
  members that recurse only through the opaque `delete` first, fixing the loop-invariant
  shape so 6d's visible-`delete` re-verification reuses it unchanged.

- **`dec_ref` — the shared off-by-one decrement, factored out.** `obj_unref`'s six object
  arms and `unref_cspace` all do "drop one ref, restore the census." That step is `dec_ref`:
  it takes the **off-by-one** state (`refs[o] == census(o) + 1`, sound elsewhere — the caller
  already cleared the reference that named `o`) and lands the matching `-1`, restoring full
  `refcount_sound`, **census-transparent** (every object view framed). It is `unref_aspace`'s
  proof shape (doc 42) minus the aspace last-ref `aspace_destroy` — the two-line proof body
  (`obj_census` is invariant under `set_obj_refs`'s view-frame, then the off-by-one carries to
  soundness). The Aspace arm of `obj_unref` reuses the proven `unref_aspace` verbatim rather
  than re-deriving the same shape.

- **The underflow gate made operational (the §1.3 headline).** Every destructor `obj_unref`
  dispatches has a *structural* precondition (`destroy_notif` needs `wait_head is None`;
  `destroy_timer` needs its armed binding to name a live, distinct notification). The census is
  what discharges them: at the zero point `refcount_sound` ⟹ `refs[o] == 0 == obj_census(o)`,
  and every census term is `≥ 0`, so **each term is individually zero** — `waiter_refs(o) == 0`
  (⟹ `waiter_seq(o).len() == 0` ⟹, via `notif_wf`'s chain, `wait_head is None`),
  `armed_timer_refs(o) == 0` (⟹ no armed timer is bound to `o`, so `o` is not self-bound).
  The census is not a soundness garnish bolted onto teardown — it is **the precondition that
  makes the dispatch verifiable**.

- **`destroy_timer` strengthened to carry `refcount_sound` (`timer.rs`).** A necessary
  consequence of `obj_unref`'s Timer arm: `destroy_timer` (proven 4e) exposed neither its
  refs delta nor `refcount_sound`, so `obj_unref` could not conclude the invariant after the
  dispatch. It now `requires`+`ensures refcount_sound` (plus `timer_view` finiteness — the
  recount lemma's gate). The only ref it touches is `disarm`'s release of `t`'s notification
  `n`: that `-1` is matched by `armed_timer_refs(n)` dropping by one, and `disarm` frames the
  slot/chan/notif/tcb views so every other census term is fixed.

- **`lemma_armed_timer_disarm` — the disarm-shaped recount.** `disarm` edits *two* timer-view
  keys (it disarms `t` *and* re-points the predecessor's `next` to splice `t` out), so the
  post-state is **not** a single-key `insert` and 6a's `lemma_armed_timer_drop` does not apply
  directly. But `armed_timer_refs` reads only `armed`/`notif`, which is exactly `disarm`'s
  frame (every `j != t` keeps both; `t` is disarmed), so the census delta is still ±1 at `t`'s
  notification only. The lemma is stated in the **`+1` form** (`armed_timer_refs(pre) ==
  armed_timer_refs(post) + 1`) rather than the `(x - 1) as nat` drop form, so the consumer's
  census arithmetic has no `nat`-saturation ambiguity (the pre-count is provably `≥ 1` — `t`
  is in the set).

- **`delete`'s contract gains a `cspace_view` frame (additive, host-checked).** `delete` stays
  `external_body`, but `destroy_cspace`'s loop reads `cspace_view[cs]` *across* its `delete`
  calls, so `delete` must frame residency. Added `final.cspace_view() == old.cspace_view()` —
  true of the real body (every internal mutator frames `cspace_view`, swept in 6a; `delete`
  re-parents CDT links and clears caps but never reassigns which slots a cspace owns), and
  host-checked in `check_delete`.

- **`cspace_resident_wf(store, cs)` — the residency-loop precondition.** A `spec fn` bundling
  the three facts `destroy_cspace`'s loop needs: `cs` is a known cspace, its residency `Seq`
  agrees with `num_slots` (the getter bounds), and every resident slot handle is live in the
  arena. The kernel maintains it by construction (residency is fixed when the cspace is carved,
  §3.2); `obj_unref`/`unref_cspace` thread it to the loop.

- **Host checks (`test_store.rs`).** `check_obj_unref`/`check_unref_cspace`/`check_destroy_cspace`
  drive the real `ArrayStore` bodies. Two `destroy_cspace` cases — bare-frame residents, and
  the **nested** `cspace 10 ▶ CSpace(11) ▶ frames` shape that exercises the
  opaque-`delete`-recurses path at runtime (`delete` keeps a real body under its
  `external_body` contract, so the cross-object teardown actually fires). Two `unref_cspace`
  cases (non-last decrement; last-ref destroy) and two `obj_unref` cases (CSpace last-ref
  destroy; the non-designating Frame no-op). `obj_unref`/`unref_cspace` are now **proven**, so
  these are differential regression guards (the erasure + the `ArrayStore` seam), the
  free-regression-guard role the doc-42 convention assigns a now-proven contract's `check_*`.

---

## 2. Findings worth keeping

- **6c needs no `decreases` — the opaqueness of `delete` is load-bearing.** The whole point of
  detail §1.5's split is that removing `external_body` from a cluster member makes its
  recursive call back into the cluster *visible* to Verus's termination checker. Keeping
  `delete` opaque means `destroy_cspace`'s `delete` calls, `obj_unref`'s `destroy_*` calls, and
  the nested `obj_unref → destroy_cspace → delete` chain are all just **contract applications**,
  not recursion. So `destroy_cspace`'s loop is a plain `n - i` countdown and the three fns need
  no shared measure. The seL4-zombie lexicographic `(count_nonempty, height)` measure is purely
  6d's burden; 6c lands the bodies and the census, and 6d only flips three `external_body`
  attributes and adds the now-visible edges. This is the structural reason the detail ordered
  the phase this way.

- **The per-destructor structural precondition cascade is the real work, and the census pays
  for most of it.** `obj_unref`'s contract carries, *conditioned on `cap.kind`*, exactly the
  well-formedness each at-zero destructor requires — `cspace_wf` + residency-wf for CSpace,
  `chan_wf` for Channel, the bind-slot facts for Thread, `notif_wf` for Notification,
  `timer_wf` for Timer. These are stateable per arm (`cap.kind matches CapKind::X(o) ==> …`).
  What the contract does **not** have to carry — because the census derives it — is the
  *emptiness* each destructor needs (no waiters; no self-bound armed timer). Those fall out of
  `obj_census(o) == 0` term-by-term. The one fact the census cannot supply is **domain
  membership** (`o`'s notification is in `refs_view.dom()`): `refcount_sound` constrains only
  objects already in `refs_view.dom()`, so "`census(n) ≥ 1`" gives "`refs[n] ≥ 1`" only once
  `n ∈ dom` is known independently. That single fact is the timer arm's pass-through precondition
  (the kernel invariant `disarm`/`destroy_timer` already require) — everything else is census.

- **The `n ≠ o` edge case is the sharpest instance of the census-as-precondition idea.**
  `obj_unref`'s Timer arm drops `refs[o]` to zero, then `destroy_timer(o)` wants to drop
  `refs[n]` for `o`'s notification `n`. If `n == o` (a timer self-bound as its own
  notification — type-allowed in the abstract model), that second `-1` would underflow the now-
  zero `refs[o]`. The census **rules it out**: a self-bound armed `o` makes `armed_timer_refs(o)
  ≥ 1`, hence `census(o) ≥ 1`, contradicting the zero branch's `census(o) == 0`. So `n ≠ o` is
  *derived*, not assumed — the off-by-one soundness at `n ≠ o` then carries `refs[n] > 0`
  through the `o`-only decrement. (Deriving `armed_timer_refs(o) == 0 ⟹ o ∉ {armed, bound-to-o}`
  needs `timer_view.dom().finite()` — a `len == 0` finite set is empty — which is why
  `obj_unref`'s Timer arm and `destroy_timer` both carry finiteness.)

- **Matching `cap.kind` per arm beats a shared decrement + a re-match.** The original plain-Rust
  `obj_unref` extracted `o` once via `Cap::obj()`, decremented, then matched `cap.kind` for the
  dispatch. `Cap::obj()` lives outside `verus!{}` (plain Rust, shared with the kernel shell), so
  it is not spec-callable; rather than add `external_fn_specification` machinery, the verified
  body matches `cap.kind` directly (the `obj_ref` precedent) with `dec_ref` factoring the shared
  decrement+restore. Each arm then has `o` and the kind statically known, so its postcondition
  is proven locally against the one destructor it calls — no "`o` is one of six kinds"
  case-explosion in the proof. `Cap::obj()` was its only caller, so it is removed (dead).

- **`dec_ref` is the off-by-one interface, reused.** Doc 42 banked the off-by-one census shape
  on `unref_aspace` (the single frame-mapping term); 6c generalizes it to the shared `dec_ref`
  that *every* ref-dropping object arm threads. Banking the interface on the simplest term first
  (3b→3d / 4a→4b "settle the model before the op") is why generalizing it here was mechanical —
  `dec_ref`'s proof is `unref_aspace`'s, minus the destroy.

---

## 3. What 6c sets up

- **6d (the cross-module cycle — the centerpiece).** Removes `external_body` from `delete`,
  `destroy_channel`, `destroy_tcb` *together*; the now-visible edges `delete → obj_unref →
  destroy_{cspace,channel,tcb} → delete` close under the seL4-zombie lexicographic
  `(count_nonempty, height)` measure. 6c's contracts are unchanged — only the three bodies
  become checked — so `obj_unref`/`unref_cspace`/`destroy_cspace` and their proofs do not churn.
  `destroy_cspace`'s loop invariant (`cspace_wf` + finite + `refcount_sound` + residency stable
  + residents-live + `count_nonempty ≤`) is the shape 6d's visible-`delete` re-verification
  reuses; `delete`'s body will consume the `cspace_view` frame and the frame-unmap census lemma
  (doc 42) plus 6c's `obj_unref` dispatch contract.

- **6e (revoke root-survival).** Unaffected by 6c directly, but `destroy_cspace`'s now-proven
  resident loop + the `cspace_view` residency are the machinery 6e's conditional non-zombie
  theorem reasons over ("`slot` is/ isn't a resident of a cspace in its own subtree").

- **6f (system invariant + closeout).** `obj_unref`/`dec_ref`'s "refs and census move by the
  same amount" is the teardown-side mirror of the construction ops' deltas 6f wires into the
  global `refcount_sound`-preservation clause.

No spec-doc / `CLAUDE.md` edit this sub-phase (the doc-30 §3 "spec edits ride the closeout"
convention; the phase-6 closeout is 6f).
