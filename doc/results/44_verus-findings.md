# Verus findings 24 — Phase 6d foundation: the `caps_consistent` cap→object invariant

Plan: `doc/plans/3_verus-rewrite.md` (§4.1 the cspace/CDT row, §3.2 the no-global-pool
discipline) and its cross-object-teardown decomposition
`doc/plans/3_verus-rewrite_phase6-detail.md` (§2 "6d"). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`40` (phase 5 — sysabi + the aspace
walker), `41` (6a — the `refcount_sound` census + `cspace_view` residency + the
strengthened teardown contracts), `42` (6b — `unref_aspace` + the frame-mapping term), `43`
(6c — `obj_unref`/`destroy_cspace`/`unref_cspace` against the opaque `delete`).

**6d is split.** Attempting the 6d body proofs directly surfaced an obstacle the detail plan
did not name: removing `external_body` from `delete` makes its body checkable, and the body
calls `endpoint_cap_dropped` (Channel branch) and `obj_unref`, **both of which demand the
designated object's well-formedness** — `chan_wf`/`notif_wf`/`cspace_resident_wf`/the
tcb-bind facts/`timer_wf` — none of which `delete`'s 6a contract (`cspace_wf` +
`refcount_sound`) carries. Because the teardown recursion deletes *arbitrary-kind* caps
(`destroy_cspace` over residents, `revoke` over descendants, `destroy_channel` over ring
caps), each caller needs that wf for caps it doesn't statically know — so it must be a
**system invariant over every live cap**, not a per-call precondition. There is no such
predicate in the codebase. Critically, the "cspace-only fallback" (detail §2-6d) does **not**
dodge it: `delete`'s body has the Channel/`obj_unref` calls regardless. So the real fork is
how to *phase* the new invariant, and the chosen course (the 6a idiom) is **a foundation
sub-phase first**: this PR lands `caps_consistent` as contracts (a wide-but-shallow
foundation, destructors stay `external_body`); the `delete`/`destroy_*` **body** proofs — the
SCC termination measure + the per-branch census composition — are the follow-on PR.

**Outcome.** `cargo verus verify -p kcore`: **244 verified, 0 errors** (unchanged item count
— the foundation adds two `spec fn`s and strengthens ~10 existing contracts/bodies with
preservation proofs, no new proof/exec *items*). `cargo test -p kcore`: **80 passed** (was
79; `+1` — `caps_consistent_exec_has_teeth`; the `check_delete`/`check_destroy_channel`/
`check_destroy_tcb` host tests gained a guarded `caps_consistent_exec` assertion). The
aarch64 `kernel` cross-build is unchanged — every change is ghost (`requires`/`ensures`/
`spec`/`proof`); confirmed by the cross-build.

---

## 1. What landed

- **`caps_consistent` / `cap_consistent` (`cspace.rs`).** `cap_consistent(store, c)` states
  one cap's designated-object consistency kind by kind — the clauses **mirror `obj_unref`'s
  per-`CapKind` `requires`** (Channel → `chan_wf` + the peer-closed end's live end-cap count
  + `binding_notif_wf`; CSpace → `cspace_resident_wf`; Thread → tcb-exists + both bind slots
  in-arena; Notification → `notif_wf`; Timer → `timer_view` contains + finite + `timer_wf`;
  Empty/Untyped/Frame/Aspace → trivially true). `caps_consistent(store)` is `slot_view`
  finite + every live cap consistent. Store-generic (`<S: Store>`), the `refcount_sound`
  idiom.

- **`caps_consistent` is deliberately *refs-free* — the design keystone (§2).** Every clause
  reads only object views (slot/chan/notif/tcb/timer/cspace), **never `refs_view`**. So a
  `dec_ref` `-1` preserves it *by framing alone*, and the foundation needs no census /
  finiteness gymnastics. The refs-coupled object-wf facts the body PR also needs —
  `endpoint_cap_dropped`'s `binding_refs_ok` and `obj_unref`'s Timer armed-notif-live — are
  **not** carried here: each is "a reference to `n` ⟹ `refs[n] > 0`", which is exactly a
  `refcount_sound` consequence, so the body PR derives them at the call site where `refs` is
  in scope (avoiding the nested-`(ch,e,v)` / armed-timer finiteness the recount lemmas were
  quarantined for, doc 41 §2).

- **Threaded through the cluster + callers.**
  - `delete`/`destroy_channel`/`destroy_tcb` (`external_body`): `requires`+`ensures
    caps_consistent`, **assumed**, host-checked — the 6a pattern. The body PR discharges them.
  - `dec_ref`/`obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace` (proven 6b/6c):
    `requires`+`ensures caps_consistent`, **proven preserved**. `dec_ref` is the load-bearing
    one — its `set_obj_refs` frames every object view, so a two-line `assert forall` over live
    slots carries each cap's consistency; `obj_unref`/`unref_cspace` then get it for free from
    `dec_ref` + the destructor's `ensures`. `destroy_cspace`'s loop invariant gains it (each
    `delete` re-establishes it).
  - `destroy_notif`/`destroy_timer` (proven phase 4): a model no-op and a `disarm`-then-frame,
    both `requires`+`ensures caps_consistent`. `destroy_timer` needed two extra frames
    (`cspace_view` + `timer_view.dom()`) so the Timer arm reads an unchanged domain + the
    ensured `timer_wf`.
  - `revoke` (loop invariant) and `thread::bind` (`requires`, `delete` is its first mutation)
    establish it for their `delete` calls — the 6a `refcount_sound` cascade, repeated.

- **`disarm`'s missing `cspace_view` frame, fixed (`timer.rs`).** The 6a "`cspace_view` sweep"
  (doc 41 §2) missed `disarm` — its `ensures` and its chain-walk loop invariant did not frame
  `cspace_view`. `destroy_timer`'s new `cspace_view` `ensures` exposed it; added the frame to
  both. (A latent gap the census never hit because no census term reads residency; the
  cap→object invariant's CSpace arm does.)

- **Host mirror with teeth (`test_store.rs`).** `caps_consistent_exec` recomputes the per-kind
  consistency over `ArrayStore` using the existing wf mirrors (`chan_wf_exec`/`notif_wf_exec`/
  `timer_wf_exec`/`binding_notif_wf_exec` + inline cspace-residency/tcb-bind checks). The new
  `caps_consistent_exec_has_teeth` builds an all-kinds-wf fixture and perturbs each arm
  (CSpace resident out of arena; notification chain malformed; armed-but-uncharted timer;
  Thread bind slot out of arena; CSpace cap with no live cspace) — five negative witnesses, so
  the mirror is demonstrably non-vacuous. The three `check_*` teardown tests assert
  `caps_consistent_exec` preserved, guarded on the precondition (the `refcount_sound` pattern).

---

## 2. Findings worth keeping

- **Refs-freeness is what makes the invariant *shallow*.** The first instinct — carry
  `endpoint_cap_dropped`'s `binding_refs_ok` and the Timer armed-notif-live inside
  `caps_consistent` (they are, after all, facts the body needs) — couples the invariant to
  `refs_view`, so every teardown `-1` must then re-prove it, dragging in the
  `binding_refs`/`armed_timer_refs` finiteness recounts (the n²-trigger hazard). Dropping
  *every* refs-coupled clause and observing that each is a pure `refcount_sound` consequence
  (a reference to `n` makes `census(n) ≥ 1`, so `refs[n] ≥ 1`) moves that work to the *one*
  place it is cheap — the body PR's call site, where `refs` and `refcount_sound` are both in
  scope. The invariant that survives is purely *structural*, and `dec_ref` preserves it with a
  framing `assert forall`, nothing more. This is the foundation's central design call.

- **The cap→object invariant is the teardown analog of `refcount_sound`, and entered the same
  way.** 6a stood `refcount_sound` up as contracts on the `external_body` destructors +
  `bind`/`revoke`, host-checked, before the bodies consumed it; 6d's foundation does exactly
  that for `caps_consistent`. The difference 6c forced: `obj_unref`/`destroy_cspace`/
  `unref_cspace` are now *proven*, so they must actively **preserve** the new invariant (not
  just assume it) — which is why this foundation lands real (if light) preservation proofs,
  not only contract additions.

- **`obj_unref`'s per-kind `requires` is the spec to mirror.** Stating `cap_consistent`'s
  arms as a literal mirror of `obj_unref`'s `requires` (the wf each destructor needs) is what
  will make the body PR mechanical: `delete`, holding `caps_consistent`, reads off the deleted
  cap's slot to get `cap_consistent(deleted)`, which *is* `obj_unref`'s precondition for that
  kind. The only gap left for the body PR is the two refs-coupled derivations.

- **The phase-4 destructors needed touching — the recursion reaches them.** `obj_unref`
  dispatches to `destroy_notif`/`destroy_timer`/`unref_aspace` (not just the cross-module
  trio), so `obj_unref`'s `ensures caps_consistent` cannot close unless those three preserve
  it. `destroy_notif` (a model no-op) and `unref_aspace` (frames all object views) are
  trivial; `destroy_timer` needed the `cspace_view`/`timer_view.dom()` frames `disarm` now
  exposes. None reads `refs` in a way that matters, because `caps_consistent` is refs-free.

---

## 3. What this sets up

- **6d bodies (the follow-on PR, doc 45).** Remove `external_body` from `delete`/
  `destroy_channel`/`destroy_tcb` together; the cluster `delete → obj_unref →
  destroy_{cspace,channel,tcb} → delete` (and `destroy_tcb → unref_cspace → destroy_cspace`)
  becomes a six-member SCC needing a shared `decreases (count_nonempty, height)`. The
  termination-measure **height direction is the crux**: the only count-dropping edge is
  `delete → obj_unref` (delete empties its slot first); every other intra-SCC edge is
  count-flat on its first call, so each must descend in height — forcing `delete` to the
  *lowest* tag and `obj_unref` to the *highest* (e.g. delete=0, destroy_cspace=destroy_channel
  =1, unref_cspace=2, destroy_tcb=3, obj_unref=4), measured on `old(store)`. With
  `caps_consistent` now available as a precondition, `delete`'s body discharges
  `endpoint_cap_dropped`/`obj_unref` per-kind, derives the two refs-coupled facts from
  `refcount_sound`, lands the deferred binding-term recount (`destroy_channel`'s binding
  release), and reorders `destroy_tcb`'s cspace/aspace release (the off-by-one).

- **Sub-phase / doc renumbering.** 6d is now **foundation (this, doc 44) + bodies (doc 45)**;
  the original `6e` (revoke root-survival) → doc 46, `6f` (system invariant + closeout) → doc
  47. The phase-9 master-plan closeout records the inserted sub-phase (the doc-30 §3 "spec
  edits ride the final closeout" convention). No `CLAUDE.md`/spec edit this sub-phase.
