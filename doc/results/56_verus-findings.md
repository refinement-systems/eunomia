# Verus findings 36 — Phase 6f: `refcount_sound` as a system invariant on the construction ops + phase-6 closeout

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6f"). Prior increment: `55` (6e — `revoke`'s conditional non-zombie root-survival theorem).
This increment closes **6f**, the **last sub-phase of phase 6**: it promotes the
`refcount_sound` census from a *teardown-family* invariant to a genuine *system* invariant
(preserved by the ref-touching construction ops, not only the teardown ops), and does the
phase-6 documentation closeout.

`cargo verus verify -p kcore`: **316 verified, 0 errors** (was 312 after doc 55's baseline — the
four new census-recount lemmas; every existing proof re-verified unchanged). `cargo test -p
kcore`: **83 passed** (unchanged — every addition is a ghost contract clause or a `proof fn`, so
the `ArrayStore` host bodies and their `check_*` tests are untouched). The aarch64 `kernel`
cross-build is unchanged (every change is ghost or an additive contract clause; `verus!{}` erases
it — confirmed `cd kernel && cargo build`). No rlimit bump or `spinoff_prover` was needed.

---

## 1. What landed

### 1.1 The conditional-implication form (the no-churn technique)

Every construction-op clause is stated as the **implication**
`cspace::refcount_sound(old(store)) ==> cspace::refcount_sound(final(store))`, not a bare
`requires refcount_sound` + `ensures refcount_sound`. This is the load-bearing design choice of
6f: the construction ops do **not need** `refcount_sound` to function, so imposing it as a
`requires` would force every caller — the production syscall shell **and** the intra-kcore
callers (`channel::fire`/`timer::check_expired` → `signal`, `delete` → `endpoint_cap_dropped`,
`destroy_tcb` → `remove_waiter`) — to establish it, ballooning the diff. The implication form is
strictly additive (the doc-27 §1 additive-frame argument): a caller without `refcount_sound`
simply does not get the conclusion and verifies **unchanged**. The idiom was already named in-tree
(the `census_delta_frozen` doc comment, `cspace.rs`), and the `delete`-window callers confirm it:
they run `endpoint_cap_dropped`/`signal` where `refcount_sound` is *false* (the off-by-one window)
and consume the unconditional `census_delta_frozen` directly, untouched by the new clause.

### 1.2 The ops wired (the system invariant holds for these)

- **`derive`** (`cspace.rs`) — the slot term. A designating copy raises `slot_refs(o)` by one
  (`lemma_designation_bump`) matched by `obj_ref`'s `refs[o] + 1`; a bare/unmapped-frame copy
  moves neither. The new **`lemma_set_slot_census` / `lemma_set_slot_obj_census`** (the duals of
  the teardown-side `lemma_clear_slot_census`/`lemma_clear_slot_obj_census`) give the per-object
  `obj_census` rise for *installing* a cap into a previously-empty slot, composing the slot delta
  with the four framed non-slot terms. To make the bridge work, **`obj_ref` and `cdt_insert_child`
  gained the missing non-slot view frames** (`notif_view`/`tcb_view`/`timer_view`/… — purely
  additive, since their bodies are a single `set_obj_refs` / `set_slot` chain that frames them).
- **`channel::bind`** (`channel.rs`) — the binding term. The new **`lemma_binding_replace`** (the
  general single-binding-edit delta, the §6f generalization of `lemma_binding_drop` which only
  *cleared*) and **`lemma_bind_refs_post_at`** (the per-object `bind_refs_post` delta) state, in
  additive form, that the binding-census delta and the refs delta move term-for-term at the old
  and new notifications — the lockstep the binding term was landed for. Its clause carries a
  `chan_view().dom().finite()` antecedent (the `binding_refs` `len` well-definedness `caps_consistent`
  already supplies on any well-formed store).
- **`channel::endpoint_cap_added`** (`channel.rs`) — census-neutral (`end_caps` is no census term,
  the bindings frame; refs unchanged).
- **`signal` / `remove_waiter`** (`notification.rs`) and **`endpoint_cap_dropped`** (`channel.rs`)
  — the ops that already emit `census_delta_frozen`. The clause is a one-line
  `lemma_refcount_sound_from_frozen` under the hypothesis, in each path that establishes the frozen
  delta (signal's accumulate + wake, remove_waiter's present + absent, endpoint_cap_dropped's post-fire).

Census-term coverage after 6f: **slot** (derive, construction; `delete`, teardown) and **binding**
(bind/endpoint_cap_added, construction; `endpoint_cap_dropped`/`destroy_channel`, teardown) are
covered on both sides; the **waiter** term is covered on the teardown side (signal/remove_waiter).

### 1.3 The closeout

- `CLAUDE.md` `### Verus` section: the stale **"Trusted (assumed `external_body`…)"** paragraph
  (which still named `delete`/`destroy_channel`/`destroy_tcb` as `external_body` — 6d removed all
  three) is **retired** and replaced with the phase-6-done record. The §6 verification-tier table
  Verus row and the `host-tests` CI bullet are likewise corrected (the `check_*` tests stay as
  differential regression guards of the now-**proven** contracts). Recorded: **kcore's object
  operations carry zero `external_body` and zero plain-Rust**; the trusted base is now exactly the
  `Store` hardware/scheduler seam; the master-plan §7 renumber (teardown = phase 6; host
  chokepoints §4.7 = phase 7 — **next**; commit core §4.8 = phase 8; spec/`CLAUDE.md`/Kani closeout
  = phase 9). No `doc/spec` edit — that rides phase 9 (the doc-30 §3 convention).

---

## 2. Findings worth keeping

- **The implication form is what makes the retrofit additive.** The plan called 6f
  "broad-but-mechanical assembly." The *mechanics* per op were not free (see §3), but the
  **no-caller-churn property is entirely due to the conditional form** — without it, threading
  `requires refcount_sound` up through `fire`/`check_expired`/`delete`/`destroy_tcb` would have
  re-opened the very contracts phase 6d closed.

- **The construction census is the mirror of the teardown census, lemma-for-lemma.** `derive`
  needed exactly the duals of the `delete`-side clear lemmas (`lemma_set_slot_census` ↔
  `lemma_clear_slot_census`, `lemma_set_slot_obj_census` ↔ `lemma_clear_slot_obj_census`); the same
  "a cap is an object cap **xor** a frame cap, never both" disjointness fact carries the census
  onto the single touched term. The construction side is genuinely the inverse, not a new model.

- **`bind`'s lockstep is two additive identities, subtracted.** Proving
  `census_delta_frozen` at `bind` does **not** need `census_delta_frozen` as an intermediate: with
  the binding-census delta (`lemma_binding_replace`, additive) and the refs delta
  (`lemma_bind_refs_post_at`, additive) both in `+[old==Some(x)] == +[new==Some(x)]` form,
  subtracting them is pure linear `nat` arithmetic the SMT closes — no `nat`-underflow case split.
  Stating both lemmas additively (rather than with `- 1`) was the unlock.

- **Bridging through `census_delta_frozen` is brittle; proving `refcount_sound` directly is robust.**
  `derive`'s first cut asserted `census_delta_frozen(old, final)` then called the bridge lemma; it
  failed (the inner-`forall` trigger on `obj_census(final, x)` did not connect the refs-insert to
  the census). Re-proving `refcount_sound(final)` **directly** per object — `refs[x] == census(x)`
  from the refs-insert + the per-`x` census delta + `refcount_sound(old)` at `x` — closed it
  immediately. The bridge lemma is still the right tool for ops that *already produce* the frozen
  delta (signal et al.); for ops that only have a per-object census delta, go direct.

---

## 3. The recorded residue (the deferred construction ops, with their obstructions)

Per the plan §6f scope-decision/fallback ("attempt full, fall back, **record** which construction
ops carry only their per-op delta"), the remaining ref-touching construction ops keep their landed
phase-3/4/5 per-op refs/census delta but do **not** yet carry the system clause. The obstructions
are concrete, not effort-laziness — each needs a fact or lemma the construction op does not have:

- **`untyped::retype_install` — the *creation off-by-one*, so the simple implication is false.**
  At entry the freshly-`init`'d object is deliberately off-by-one (`refs == 1`, census `0` — no
  designating cap yet); install closes it. So for a non-channel object `refcount_sound(old)` would
  give `refs[o] == census[o]` at entry, after which census rises by one while refs is unchanged —
  `refcount_sound(final)` is **false**. The faithful clause is a `census_off_by_one(old, o) ==>
  refcount_sound(final)` creation-transition theorem requiring fresh-object preconditions
  (`census(old, o) == 0`) the contract does not state — a 6d-scale strengthening, not 6f wiring.
- **`channel::send` / `recv` and `thread::bind` — `slot_move`'s permutation-neutrality.** These
  move a cap between slots; `slot_refs(o)` is invariant under the move (one cap, different slot),
  but proving it needs a census-preservation property of the two-slot transposition `slot_move`
  performs (and `send`/`recv` additionally compose the move with `fire`'s `census_delta_frozen`).
  `slot_move` ensures `refs` unchanged but not the slot-census invariance under its permutation.
- **`notification::wait` — the thread-on-one-chain invariant.** The block path's `waiter_refs(o)`
  frame for `o != n` requires that the blocking thread `cur` is on no *other* wait chain; the
  contract states only `cur.wait_notif != Some(n)`, so a `cur` already blocked on `o' != n` would
  break `waiter_refs(o')`. The realistic caller (a running CURRENT thread, `wait_notif is None`)
  satisfies it, but the fact is not in the contract.
- **`timer::arm` / `disarm` — the loop-threaded armed-timer recount.** The armed-timer census
  delta itself is clean (`t` leaves the `armed && notif == Some(n)` filter), but `disarm`'s unlink
  happens inside the list-walk loop, so the per-`j` `armed`/`notif` frames and the
  set-extensionality recount must thread the loop; `arm` builds on `disarm`.

These are the honest follow-on (the §3 residue of a phase that the plan explicitly bounded), with
the per-op deltas as the standing evidence. The census is thus a **teardown-family invariant + a
construction-side system invariant on the slot and binding terms**, with the waiter/armed-timer/
thread-hold construction sides documented as the named follow-on.

---

## 4. Doc / CLAUDE.md

`CLAUDE.md` is edited this increment (the phase-6 closeout the doc-30 §3 / doc-55 §4 convention
deferred to 6f): the trusted-residue paragraph retired, the proven teardown cluster + the
construction-op system invariant + the master-plan §7 renumber recorded, **phase 7 (host
chokepoints, §4.7) reaffirmed as next**. No `doc/spec` edit (phase 9). `cargo verus verify -p
kcore` runs with no per-proof filter, so the four new lemmas and every construction-op clause
auto-gate; `host-tests` reruns the `check_*` differential guards.

**Doc-numbering note.** Plan §6-detail budgeted 6f as findings doc **46**; it lands as doc **56**
(6d having spanned docs 44–54, 6e doc 55 — the doc-55 §4 landing-order convention). **Phase 6 is
complete.** Next: **phase 7** — the host chokepoints (`urt`/`ipc`/`dma-pool`/`cas`), the master-plan
§4.7 Kani-to-Verus migration.
