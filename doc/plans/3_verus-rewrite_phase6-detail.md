# Phase 6 detail: cross-object teardown + the full `refcount_sound` census (Verus rewrite)

**Status:** proposed. Detailed, step-by-step decomposition of the **cross-object-teardown
phase** of `doc/plans/3_verus-rewrite.md`, written *before* any code so the implementation
does not repeat phase 2's mid-flight splits — the same treatment
`3_verus-rewrite_phase{3,4,5}-detail.md` gave their phases, which then landed cleanly
(phase 3 across docs 26…30, phase 4 across 31…35, phase 5 across 36…40).

**Baselines:** `3_verus-rewrite.md` (§4.1 the cspace/CDT row's `delete`/`revoke`/
`obj_unref`/`destroy_cspace` entries, §3.2 the no-global-pool refcount discipline, §7
phasing); `3_verus-rewrite_phase{4,5}-detail.md` (the structure this mirrors, and the §1.4/
§1.5 scope-outs that named this phase); `doc/results/21…25` (phase 2 — the cspace/CDT core,
the acyclicity-rank and linked-list-merge mechanics, **doc 23 §4** the retracted revoke-cap-
survival fix + the two seL4-zombie host-test witnesses), `26…30` (phase 3 — the FIFO core +
the `bind`/`bind_refs_post` binding-refcount delta), `31…35` (phase 4 — the waiter/armed-
timer refcount deltas + the `external_body` `destroy_tcb` discipline), `36…40` (phase 5 —
the aspace walker + the **frame-mapping refcount term** this phase now collects). The
current `kcore` source as of `main` `992661b` (phase-5e merge). Current Verus baseline:
**225 verified, 0 errors** (doc 40); `cargo test -p kcore`: **68 passed** (doc 40).

---

## 0. Purpose, and the phase-2…5 lessons it acts on

Every findings doc since phase 2 has named this phase and pushed work toward it. Doc 21 §6
deferred revoke/delete termination's *teardown* half "until the cross-module destructors are
ported"; doc 23 §4 **retracted** the proposed revoke-cap-survival fix as unsound and pinned
the seL4-zombie counterexample with two executable witnesses; phases 3/4/5 each landed one
*term* of the refcount census (binding, waiter, armed-timer, frame-mapping) as a per-op
delta but explicitly deferred the **full `refs == census` equality**; and phase 4 detail §1.4
made "the cross-object teardown becomes its own dedicated phase immediately after phase 5"
the single biggest correction to the master plan's phasing. Phase 5e (doc 40 §3) reaffirmed
it as **now unblocked** — the aspace walker it was waiting on is ported. This document is
that phase.

**It is the hardest remaining kernel-core phase**, because it is the one place three
deferred difficulties finally have to be discharged *together*: (a) the **cross-module
mutual recursion** `delete → obj_unref → destroy_{cspace,channel,tcb} → delete`, which
Verus's `decreases` must close as one cluster; (b) the **seL4-zombie** termination measure
(a cspace whose last live cap is a resident of its own subtree — doc 23 §4); and (c) the
**full `refcount_sound` census** that discharges the `refs - 1` underflow-freedom at every
unref on the recursion path. None can be cleanly isolated from the others (§1.5), so the
risk management here is sub-phase ordering + a recorded fallback, not a magic decomposition.

**Three master-plan-line facts, resolved up front** (the phase-4/5-detail §0 discipline):

- **This phase is NOT master-plan §7's "phase 6."** §7 step 6 is *"Host chokepoints
  (§4.7)"*; §7 never gave the cross-object teardown its own phase (it listed
  `delete`/`revoke`/`destroy_cspace`/`obj_unref` under §4.1/phase 2, where they were
  deferred). The phase-4 detail §1.4 correction inserts the teardown phase here, so the
  master-plan numbering shifts by one from this point on: **teardown = phase 6**, host
  chokepoints (§4.7) = phase 7, commit-protocol recovery core (§4.8) = phase 8, the
  spec/`CLAUDE.md`/Kani closeout = phase 9. The phase-9 closeout edits §7 to record the
  inserted phase (no §7 edit this phase — the doc-30 §3 "spec edits ride the final closeout"
  convention).
- **There is nothing to delete.** The "delete the Kani it subsumes" clause was discharged
  wholesale in the phase-2 Kani→Verus migration (no `kcore/src/proofs/`); `cargo kani -p
  kcore` was retired then too. Phase 6 touches no Kani machinery (that is phase 7, when the
  host chokepoints move and their `proofs.rs` go).
- **The substantive work is closing the last un-ported kcore object machinery.** After phase
  5, the *only* kcore code outside a proven `verus!{}` contract is the teardown residue:
  `delete`/`destroy_channel`/`destroy_tcb` (`external_body`, host-checked) and
  `obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace` (plain Rust). Phase 6 closes all
  of it. **After phase 6 the kernel object core carries zero `external_body` and zero
  plain-Rust operations** — the rewrite's stated goal (plan §1.2) for `kcore` is met; only
  the host chokepoints (phase 7) and the commit core (phase 8) remain in the whole plan.

### The new machinery, and the design forks decided here

- **The full `refcount_sound` census — defined in 6a, not before.** Phases 3/4/5 landed the
  five *terms* as per-op deltas (`binding_refs_ok`/`bind_refs_post`, the waiter `±1`, the
  armed-timer `±1`, and — newly enabled but not yet collected — the frame-mapping aspace
  ref). 6a assembles them into one `spec fn obj_census(store, o)` and
  `spec fn refcount_sound(store) := ∀ o. refs_view[o] == obj_census(store, o)`, the
  census-equality the teardown family assumes and preserves (the §4.1 "every object's `refs`
  equals the recount over all designating slots + bindings + waiters + armed timers + frame
  mappings" obligation). This is the phase's central new spec — the 4a-`waiter_seq` / 5c-
  `pt_wf` analog. **Decided:** the census is a precondition+postcondition of the teardown
  family *and* the ref-touching construction ops (so it is a genuine system invariant, not a
  fact only teardown respects). The breadth of retrofitting it onto the already-proven
  construction ops is the chief scope risk (§3); the landed deltas are exactly the local
  facts that make each retrofit mechanical.

- **The seL4-zombie recursion measure — `count_nonempty` lexicographic with a function-
  height tag.** The mutual-recursion cluster is closed under `decreases (count_nonempty(
  slot_view), height)`: every `delete` empties its target slot *before* recursing, so the
  global live-slot count strictly drops across each `delete`→…→`delete` cycle, and the
  height tag (`delete` > `obj_unref` > `destroy_*`) orders the within-count edges that do not
  drop it (the standard lexicographic device for a mutually-recursive cluster where some
  edges are count-flat). `count_nonempty` is already `revoke`'s proven measure
  (`cspace.rs:2180,4959`), reused. **The zombie case is a termination non-issue** — emptying
  the self-resident still drops `count_nonempty` — but a *cap-survival* issue (§1.4, 6e).

- **cspace residency must enter the abstract `Store` spec (6a).** Doc 23 §4 established that
  stating revoke-root-survival (and `destroy_cspace`'s resident loop) needs cspace
  **residency** modelled in the seam — today `cspace_slot`/`cspace_num_slots`
  (`store.rs:54-55`) are uncontracted getters. 6a adds a `cspace_view` (the residency map,
  the §4a `chan_view` analog) so `destroy_cspace`'s loop and `refcount_sound`'s slot term can
  name "the slots cspace `cs` owns." **Decided:** model residency as a ghost view, not a
  recomputed predicate, mirroring every other object's state.

- **`aspace_destroy`/`aspace_unmap` get minimal seam contracts, not bodies (6a/6b).** They
  are shell-owned hardware/page-table operations (`store.rs:122-124`), exactly like
  `make_runnable` (phase 4a) — kcore never sees their bodies. They get assumed,
  host-checked `ExStore` contracts (frame the object views; `aspace_destroy` drops the
  aspace from `refs_view`'s live set), so the `delete` frame-unmap branch and `unref_aspace`
  can be verified calling them. **Decided against** porting the page-table free into kcore
  (it is the trusted shell, plan §2 row "kernel shell … stays the trusted base").

### Discipline carried from phases 2–5 (applies to every sub-phase below)

- **One PR per sub-phase.** It merges only when green.
- **`cargo verus verify -p kcore` green before merge** — the CI `verus` job runs with no
  per-proof filter, so a new `verus!{}` obligation auto-gates (`3_verus-rewrite.md` §8).
  Baseline: **225 verified, 0 errors** (doc 40).
- **`cargo test -p kcore` green** — `test_store` is the executable check of every assumed
  contract against its real `ArrayStore` body. As each `external_body` op becomes proven, its
  `check_*` test *stays* (it now differentially checks a proven contract — a free regression
  guard), and the seam-contract additions (`aspace_*`, `cspace_view`) gain `check_*` teeth.
  Baseline: **68 passed** (doc 40).
- **The aarch64 `kernel` cross-build is unaffected** — `verus!{}` erases ghost code, so the
  erased `exec` body is byte-identical to today's plain Rust (`3_verus-rewrite.md` §6).
  Removing an `external_body` attribute and moving a plain-Rust fn into a `verus!{}` block
  changes *no* `exec` code. Confirm with `cd kernel && cargo build` per sub-phase.
- **A `doc/results/N_verus-findings.md` increment per sub-phase**, recording what closed and
  the Verus-mechanics findings worth keeping (the doc-26…40 cadence). Phase 5 produced docs
  36–40 ("findings 16–20"); phase 6 produces **docs 41–46** ("findings 21–26"), one per
  sub-phase, **numbered in landing order** (the doc-29/30 convention).

---

## 1. Dependency analysis — why this order

Phase 6 touches `cspace.rs` (the `ExStore` spec — one new `cspace_view` residency view + the
`aspace_*` seam contracts + the `refcount_sound` census spec + the cluster bodies
`obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace`/`delete`/`revoke`), `channel.rs`
(`destroy_channel` body), `thread.rs` (`destroy_tcb` body), `store.rs` (the `aspace_*`/
`cspace_*` seam methods gain contracts — spec-only), and `test_store.rs` (residency state +
strengthened `check_*` for the now-proven bodies). The structure differs from phases 3–5 in
three ways, all stated so the sub-phase shapes are expected, not surprises.

### 1.1 There is a separable easier win (the aspace teardown) — but no slice-free clean island

Phases 3 and 5 opened with a confidence-builder on view-independent or slice-free code (3a
`retype_check`, 5a `sysabi::decode`); phase 4 had none and opened with its refactor. Phase 6
is in between:

- **No fully clean island.** Every teardown op reads the census and/or the recursion, both of
  which 6a must stand up first. So phase 6 **opens directly with the foundation** (6a — the
  4a analog), where the design risk lives.
- **But the aspace teardown is genuinely separable (6b).** `aspace_destroy` is a seam black
  box and `unref_aspace`/the `delete` frame-unmap branch do **not** recurse into `delete`
  (an aspace owns page tables, not caps — `aspace.rs`'s `unmap_in` is leaf-clears, no cap
  deletion). So the **frame-mapping census term** + `unref_aspace` close *without* the
  cross-module recursion cluster — the early win that banks the census-preservation discipline
  on the simplest term before the cluster (the 3a/5a role, one phase in).

### 1.2 The cross-module mutual recursion is the structural novelty

Every phase-2…5 op was either non-recursive or self-recursive within one module (`revoke`'s
`descend_to_leaf` loop, `destroy_cspace`'s resident loop over the *opaque* `delete`). Phase 6
is the first to close a **cycle that spans three modules**:
`cspace::delete → cspace::obj_unref → {cspace::destroy_cspace, channel::destroy_channel,
thread::destroy_tcb} → cspace::delete`. Verus checks such a cluster's termination only when
**all** its members are non-`external_body` and share a `decreases` measure (an `external_body`
member is an opaque black box — its recursive call back into the cluster is invisible, so the
cluster's `decreases` need not see through it). This dictates the cluster's all-or-nothing
closure (§1.5) and the lexicographic measure (§0).

### 1.3 The full `refcount_sound` census is the second novelty — and the underflow gate

`obj_unref` (`cspace.rs:222`), `unref_aspace` (`:239`), `unref_cspace` (`:247`), and
`destroy_channel`'s binding release (`channel.rs:1099`) all do `set_obj_refs(o, obj_refs(o) -
1)` — an underflow (and a premature last-ref teardown, the doc-21 §8 UAF class) unless
`refs[o] > 0` at the call. The only thing that *proves* `refs[o] > 0` for an object reached
deep in the teardown recursion is the census equality (`refs[o] == obj_census(o) ≥ 1`
whenever a live reference to `o` exists). So the census is not an optional soundness garnish —
it is the **precondition that makes the cluster bodies verifiable at all**. This is why 6a
defines it before any body is closed, and why the cluster sub-phases (6c/6d) carry it as the
load-bearing invariant rather than a structural `cspace_wf`-only contract.

### 1.4 revoke-root-survival is conditional (the zombie), and needs residency

Doc 23 §4's retraction stands: "the revoked cap survives" is **false in the seL4-zombie
case** (the revoked root is a resident of a cspace whose last live cap lies in the root's own
subtree; revoke deletes that cap, `destroy_cspace` fires, and the root is emptied). The
host-tests `delete_empties_slots_outside_the_deleted_subtree` and
`revoke_can_empty_its_own_root_zombie` (added in doc 23) witness it. So 6e cannot prove
unconditional survival; it proves the **conditional** theorem — `slot`'s cap survives revoke
*unless* `slot` is a zombie root — which needs cspace **residency** in the spec (6a) to even
state "`slot` is/ isn't a resident of a cspace in its own subtree." This is the long-standing
documented gap finally closed (as a characterized conditional, doc 23 §4's "evidenced
characterization" promoted to a theorem with an explicit precondition).

### 1.5 The cluster closes all-or-nothing — so 6c/6d split by *cluster membership*, not concern

Because removing `external_body` from a cluster member makes its recursive call back into the
cluster *visible*, you cannot close the cluster's termination one member at a time and keep
`decreases` sound the whole way. The feasible incremental path (the slot_move/cdt_unlink
"split the hard looping ops into separate PRs" precedent, docs 24/25):

- **6c closes the members that recurse only through the *opaque* `delete`:** `obj_unref`,
  `destroy_cspace`, `unref_cspace`, `unref_aspace`. With `delete`/`destroy_channel`/
  `destroy_tcb` still `external_body` (richer contracts from 6a), these four are verified
  against `delete`'s **contract** — Verus sees no cycle, so their `decreases` is the simple
  resident-loop measure. The census-preservation for the cspace-resident path lands here.
- **6d closes the cycle:** remove `external_body` from `delete`, `destroy_channel`,
  `destroy_tcb` *together*. Now `delete → obj_unref → destroy_cspace → delete` (and the
  channel/tcb edges) are visible; the lexicographic `decreases` (§0) closes the whole
  cluster at once. 6c's proofs are unchanged (contracts didn't move; only bodies got
  checked). This is the hardest single sub-phase in the entire rewrite — budget accordingly,
  with the recorded fallback (§3, the doc-35 `check_expired` / doc-40 5e-TLBI precedent).

### 1.6 No new object model, but the most reuse of prior machinery

Unlike 4a (three new views) or 5c (a new slice model), 6a adds only **one** view
(`cspace_view`); everything else is *assembly*. The phase reuses, in load order:
`count_nonempty` + the `valid_prank`/`acyclic` rank (the measure + `descend_to_leaf`,
`cspace.rs:1198,2180,4888`); `slot_refs` + `lemma_same_caps_same_census` +
`lemma_designation_bump` (the slot-census recount, `cspace.rs:2190,2198,2223`);
`binding_refs_ok`/`bind_refs_post` (the binding term, `cspace.rs:1495`, `channel.rs:273`);
the proven `cdt_unlink`/`slot_move` (which `delete` calls); and `obj_ref`'s `+1` discipline
(`cspace.rs:3839`, the census's construction-side mirror). The proof effort is in stitching
these into a global invariant across a recursion, not in inventing a model.

### 1.7 Resulting order

```
6a  refcount_sound census + cspace_view residency + aspace_* seam contracts + richer cluster contracts   (foundation; the 4a analog — design-heavy, proof-light)
6b  aspace teardown: unref_aspace + the frame-mapping census term + delete's frame-unmap branch facts     (the separable, non-recursive win — banks the census discipline)
6c  obj_unref / destroy_cspace / unref_cspace — cluster members recursing through the opaque delete        (census preservation on the cspace-resident path)
6d  delete + destroy_channel + destroy_tcb bodies — close the cross-module cycle; the seL4-zombie decreases (the centerpiece; the termination + census headline; with the fallback)
6e  revoke root-survival — the conditional non-zombie theorem (doc 23 §4 closed)
6f  the full refcount_sound as system invariant on the construction ops + phase-6 closeout
```

6f folds the system-invariant retrofit (re-stating the already-proven construction ops'
contracts to preserve `refcount_sound`, using their landed deltas) with the documentation
closeout — it is broad-but-mechanical assembly, not new proof difficulty, so it rides the
last sub-phase (the 3e/4e/5e closeout-plus-tail-work pattern). If the retrofit proves heavier
than expected it may split into its own PR before the closeout (the 4c "budgeted together,
may split" convention).

---

## 2. The sub-phases

Each carries: scope · specs/contracts landed · key lemmas/risks · test additions · the
"done =" gate.

### 6a — `refcount_sound` census + `cspace_view` residency + seam contracts (foundation; the 4a analog)

A **wide but shallow** change that *enables* 6b–6f and lands almost no body proof, so its
risk is in the design — the census shape, the residency view, the cluster's richer contracts
— not the SMT. Keep the diff structural (the 4a discipline).

- **Add the `cspace_view` residency view to `ExStore`** (`cspace.rs:447`, the `chan_view`
  analog): `spec fn cspace_view(&self) -> Map<ObjId, CSpaceView>` with
  `CSpaceView { num_slots: nat, slots: Seq<SlotId> }` (the residency the kernel fixes at
  construction — `cspace_slot`/`cspace_num_slots` are getters with no setter, so this is an
  immutable projection, exactly as `chan_ring_cap`/`tcb_bind_slot` are). Contract
  `cspace_num_slots`/`cspace_slot` against it; every existing setter frames it unchanged
  (purely additive — the phase-2…5 proofs stay green, the doc-27 §1 additive-frame argument).
- **The census spec.** Define each term as a `spec fn` over the relevant view, then sum:
  - `slot_refs(slot_view, o)` — already exists (`cspace.rs:2190`): cspace residents + channel
    ring caps + TCB bind caps, all in the one arena.
  - `binding_refs(chan_view, o)` — the count of channel bindings (over `(end, ev)`) whose
    `notif == Some(o)` (the §3.6 binding term; the `binding_refs_ok` companion).
  - `waiter_refs(notif_view, tcb_view, o)` — the length of `waiter_seq(o)` (the phase-4
    waiter term; a blocked TCB holds one ref).
  - `armed_timer_refs(timer_view, o)` — the count of armed timers whose `notif == Some(o)`
    (the phase-4e armed-timer term).
  - `frame_map_refs(slot_view, o)` — the count of `CapKind::Frame { mapping: Some((o, _)), ..}`
    slots (the **new** phase-5-enabled aspace term — a mapped frame holds an aspace ref via
    its mapping field, *not* via cap designation; confirmed by `obj_ref`/`obj_unref`'s Frame
    arm being a no-op, `cspace.rs:3855`).
  - `thread_hold_refs(tcb_view, o)` — the count of TCBs with `cspace == Some(o)` plus those
    with `aspace == Some(o)` (a bound thread holds a ref on each — released by `destroy_tcb`'s
    `unref_cspace`/`unref_aspace`, `thread.rs:355,359`).
  - `obj_census(store, o) := slot_refs + binding_refs + waiter_refs + armed_timer_refs +
    frame_map_refs + thread_hold_refs`.
  - `refcount_sound(store) := ∀ o ∈ refs_view().dom(): refs_view()[o] == obj_census(store, o)`.
- **The recount lemmas (the spec basis).** Generalize the two existing slot-census lemmas
  (`lemma_same_caps_same_census` `:2198`, `lemma_designation_bump` `:2223`) to a per-term
  bump/drop lemma for each census term: "this single-key view edit raises/lowers exactly
  one term by one, leaving the others fixed." These are the building blocks 6b–6d compose;
  most are the `lemma_designation_bump` shape over a different view. Design them here so the
  cluster sub-phases consume a settled recount API (the 3b→3d / 4a→4b "settle the model
  before the op" discipline).
- **Strengthen the cluster's `external_body` contracts** (still `external_body` — only the
  contracts grow, host-checked): give `delete` (`cspace.rs:4831`), `destroy_channel`
  (`channel.rs:1070`), `destroy_tcb` (`thread.rs:313`) the **`refcount_sound`-preservation +
  `count_nonempty`-drop** clauses they will have to satisfy as bodies in 6c/6d. Stating them
  now means 6b/6c verify against the *final* contracts, so 6d's body closure adds no caller
  churn. `delete`'s current contract is already silent on `refs_view` (`cspace.rs:4851`); 6a
  fills that in with the census clause.
- **Add the `aspace_destroy`/`aspace_unmap` seam contracts** (assumed, host-checked — the
  `make_runnable` precedent): `aspace_unmap(a, va, pages)` frames every object view + the
  refs (it is page-table maintenance, no object state); `aspace_destroy(a)` removes `a` from
  `refs_view().dom()` (last-ref teardown) and frames the rest. These let 6b verify the
  `delete` frame-unmap branch and `unref_aspace`.
- **Give `ArrayStore` real residency state + the census mirror** (`test_store.rs`): a
  `cspace_view`-backing field; a `refcount_sound_exec` executable mirror with a `_has_teeth`
  rejecter per term (the `chan_wf_exec`/`notif_wf_exec` pattern), so the census is checkable
  on the concrete store.
- **No op proofs yet** — the phase-2…5 verified ops call none of the new accessors, so they
  stay green untouched (the additive-frame argument, doc 27 §1). The construction-ops'
  census-preservation retrofit is 6f.
- **Done =** verus green + `cargo test -p kcore` + `cd kernel && cargo build`. Findings doc
  **41**. (Risk is design review of the census terms + the residency view, not solver time —
  keep this PR's diff structural.)

### 6b — Aspace teardown: `unref_aspace` + the frame-mapping census term (the separable win)

The non-recursive teardown, banked first to exercise the census-preservation discipline on
the simplest term before the cross-module cluster (the 3a/5a confidence-builder role, here
on real teardown rather than a slice-free island).

- **Scope.** Move `unref_aspace` (`cspace.rs:238`) into `verus!{}`; prove it against the 6a
  `aspace_destroy` seam contract + the census.
- **`unref_aspace` contract.** `requires refcount_sound` + `refs[a] > 0` (the underflow gate,
  discharged here by the frame-mapping/thread-hold terms ≥ 1 whenever a live mapping or bound
  thread named `a`); `ensures`: `refs[a]` decremented; at zero, `aspace_destroy` fires and `a`
  leaves `refs_view().dom()`; `refcount_sound` preserved (the released reference and the `-1`
  move in lockstep — the `frame_map_refs`/`thread_hold_refs` term drops by one when the caller
  removed the mapping/binding, which the contract takes as the matching precondition).
- **The `delete` frame-unmap branch reasoning** (`cspace.rs:4875-4878`): the `Frame { mapping:
  Some((asp, va)), .. }` arm calls `aspace_unmap(asp, va, pages)` then `unref_aspace(store,
  asp)`. Prove (as a lemma `delete` will consume in 6d) that this branch lowers
  `frame_map_refs(asp)` by exactly one (the deleted frame was the mapping) and is matched by
  `unref_aspace`'s `-1` — the frame-mapping term's contribution to `delete`'s census
  preservation. `delete` itself stays `external_body` here; only the *lemma* lands, ready for
  6d.
- **Key lemmas / risk.** The frame-mapping recount (a `lemma_designation_bump` analog over
  `frame_map_refs` rather than `slot_refs`); the interaction of clearing a Frame cap's slot
  (which lowers *both* `slot_refs` for any object the cap designated — none, Frame designates
  no object — *and* `frame_map_refs` for `asp`). Lightest of the teardown sub-phases (no
  recursion, one term).
- **Tests.** Strengthen `check_delete` (`test_store.rs:655`) for the mapped-frame arm
  (delete a mapped frame ⇒ `aspace_unmap` called, aspace ref dropped, census preserved); a
  `check_unref_aspace` (last-ref ⇒ destroy; non-last ⇒ just the decrement).
- **Done =** verus green + test + cross-build. Findings doc **42**.

### 6c — `obj_unref` / `destroy_cspace` / `unref_cspace`: cluster members over the opaque `delete`

The cspace-resident teardown path, verified against `delete`'s 6a contract (Verus sees no
cycle — `delete` is still `external_body`, so these are *not* mutually recursive from its
view, §1.5). This is where the census-preservation discipline for the slot term lands, and
where `destroy_cspace`'s resident-loop measure is designed (so 6d's cycle closure inherits a
compatible `decreases`).

- **Scope.** Move `obj_unref` (`cspace.rs:220`), `destroy_cspace` (`:259`), `unref_cspace`
  (`:246`) into `verus!{}`.
- **`obj_unref` contract.** `requires refcount_sound` + `cap_obj(cap)` live (or `None`);
  `ensures`: `refs[o]` decremented; at zero, the matching destructor fires (dispatch by
  `CapKind` — `destroy_cspace`/`destroy_channel`/`destroy_tcb`/`destroy_notif`/`destroy_timer`/
  `aspace_destroy`, `cspace.rs:224-231`, the non-recursive ones proven in phase 4, the
  recursive ones via their 6a contracts); `refcount_sound` preserved; `count_nonempty` non-
  increasing (it drops only inside the recursive destructors). The underflow `obj_refs(o) - 1`
  is discharged by `refcount_sound` ⇒ `refs[o] ≥ 1` (the caller `delete` emptied the
  designating slot but the census-at-entry still had `refs[o] ≥ 1`).
- **`destroy_cspace` contract + measure** (`requires refs[cs] == 0`): every resident cap
  deleted (the loop `cspace.rs:261-265`), `cspace_wf` preserved, `count_nonempty` strictly
  drops if any resident was non-empty. Its loop calls the **opaque** `delete(sid)`; the loop
  `decreases` is the resident-index countdown (`num_slots - i`), straightforward because
  `delete` is a black box satisfying its count-drop contract. Design the loop invariant —
  the prefix of residents already emptied, the suffix intact (`delete`'s slot-frame, the
  doc-35 §2.4 walk-while-mutate discipline) — so 6d's visible-`delete` re-verification reuses
  it unchanged.
- **`unref_cspace`** = `obj_unref`'s CSpace arm in isolation (`refs[cs] -= 1`; at zero
  `destroy_cspace`); prove it as the simple decrement-then-maybe-destroy.
- **Key lemmas / risk.** The census preservation across `destroy_cspace`'s loop — each
  `delete` lowers `slot_refs` for the deleted resident's object *and* `refs` for that object,
  in lockstep (composed from `delete`'s 6a census contract). The risk is the **interaction of
  many per-resident census deltas** accumulating over the loop while `cspace_wf` and
  `refcount_sound` are both maintained — quarantine the per-step delta into one lemma
  (`lemma_destroy_cspace_step`), the doc-25 §2 / doc-35 §2.6 "decomposition beats an rlimit
  bump" discipline.
- **Tests.** Strengthen `check_destroy_cspace` (drive the resident loop, assert every resident
  emptied + `refcount_sound`); a nested-cspace case (a resident that is itself a CSpace cap,
  exercising the opaque-`delete`-recurses path — checked structurally, the body recursion is
  6d).
- **Done =** verus green + test + cross-build. Findings doc **43**.

### 6d — `delete` + `destroy_channel` + `destroy_tcb`: close the cross-module cycle (the centerpiece)

The hardest single sub-phase in the rewrite. Remove `external_body` from all three together
(§1.5 — the cycle is all-or-nothing), closing the `delete → obj_unref → destroy_{cspace,
channel,tcb} → delete` mutual recursion under the seL4-zombie lexicographic measure, against
`refcount_sound`. Budget the most time; carry the recorded fallback.

- **Scope.** Drop `external_body` from `delete` (`cspace.rs:4831`), `destroy_channel`
  (`channel.rs:1070`), `destroy_tcb` (`thread.rs:313`); prove their real bodies. The 6a
  contracts are unchanged — only the bodies become checked, so no caller (6b/6c/`revoke`)
  churns.
- **The lexicographic `decreases`.** All cluster members share
  `decreases (count_nonempty(store.slot_view()), height)` where `height` tags `delete = 2`,
  `obj_unref = 1`, `destroy_* = 0` (the within-count edges `delete→obj_unref→destroy_*` drop
  the tag; the cross edges `destroy_*→delete` drop `count_nonempty` because `delete` empties
  its slot before recursing — `cspace.rs:4866-4868` precede `obj_unref` at `:4879`). `revoke`/
  `destroy_cspace`/`obj_unref`'s existing/6c measures must be set to compose with this — fix
  the tags in 6c so 6d only *adds* the now-visible `delete` edge.
- **`delete` body proof.** `cdt_unlink` (proven, doc 25) + empty-the-slot + the per-`CapKind`
  teardown branches (`endpoint_cap_dropped`; the frame-unmap branch from 6b; `obj_unref`):
  - `cspace_wf` preserved (off `cdt_unlink`'s ensures + the slot empty);
  - `count_nonempty` strictly drops (the slot went empty; the destructors only lower it
    further);
  - **`refcount_sound` preserved** — the deleted cap lowers exactly its object's `slot_refs`
    (or `frame_map_refs`), matched by `obj_unref`'s `-1`; the per-end `endpoint_cap_dropped`
    binding release is matched by its `binding_refs` drop (the phase-3 delta);
  - terminates (the lexicographic measure).
- **`destroy_channel` body** (`channel.rs:1084-1102`): the ring-cap delete loops (each
  `delete` now a *visible* cluster member) + the binding-ref release loop; `chan_wf`/
  `cspace_wf` preserved, every ring cap emptied (its current contract, `channel.rs:1078`),
  the binding releases matched by `binding_refs` — `refcount_sound` preserved, `count_nonempty`
  drops. The binding release's "no clean closed form" caveat (`channel.rs:1067`) is *resolved*
  here by the census: each `-1` is matched by the corresponding `binding_refs` term.
- **`destroy_tcb` body** (`thread.rs:336-362`): unqueue/`remove_waiter` (proven) + clear
  state + the two bind-slot `delete`s (visible) + `unref_cspace`/`unref_aspace` (proven 6c/6b);
  the structural contract (`thread.rs:323-335`) preserved, now with `refcount_sound` (the
  bind-cap deletes' census drops + the cspace/aspace `thread_hold_refs` releases matched).
- **Key lemmas / risk — the chief risk of phase 6.** Composing many census deltas across a
  *cross-module* recursion while three well-formedness predicates (`cspace_wf`, `chan_wf`,
  `notif_wf`/`timer_wf` via the framed views) are maintained, under a lexicographic measure
  Verus must accept across module boundaries. Expect bespoke step lemmas per destructor and
  heavy trigger-economy work (the doc-35 §2.6 n²-trigger trap is a live hazard with the census
  `filter`/`len` terms — extract no-duplicates/count steps into single-purpose lemmas).
- **Fallback (the doc-35 `check_expired` / doc-40 5e-TLBI "attempt full, fall back"
  precedent).** If the full census-through-the-cross-module-cycle proves disproportionate,
  fall back to: close `delete` + `obj_unref` + `destroy_cspace` (the cspace-only cycle — one
  module, the smaller cluster) with the full census; **keep `destroy_channel`/`destroy_tcb`
  `external_body`** with their 6a richer (census-bearing, host-checked) contracts; and **record
  the deferral** with the precise residue. This still lands the termination headline and the
  cspace census; the two cross-module destructor bodies ride a follow-on. *Attempt full, fall
  back — recorded, not silently dropped.*
- **Tests.** The existing `check_delete`/`check_destroy_channel`/`check_destroy_tcb`
  (`test_store.rs:655,1014,1223`) stay as differential checks of the now-proven contracts (a
  free regression guard); add a deep-recursion case (a cspace holding a channel holding a
  queued cspace cap, exercising the full cross-module cycle) asserting `refcount_sound` +
  termination.
- **Done =** verus green + test + cross-build. Findings doc **44**.

### 6e — `revoke` root-survival: the conditional non-zombie theorem (doc 23 §4 closed)

The long-deferred revoke postcondition (`cspace.rs:4930-4938`), now stateable because residency
is modelled (6a) and the cluster is verified (6d). Conditional on non-zombie, per doc 23 §4 —
this promotes that doc's "evidenced characterization" to a theorem with an explicit precondition.

- **Scope.** Strengthen `revoke`'s `ensures` (`cspace.rs:4943`) with the cap-survival clause,
  guarded by a non-zombie precondition.
- **The non-zombie precondition.** `slot` is **not** a resident of any cspace whose only
  surviving live cap lies in `slot`'s own CDT subtree — i.e. revoking `slot`'s subtree cannot
  trigger a `destroy_cspace` that empties `slot`. State it via `cspace_view` residency + the
  CDT subtree reachability (the `descend_to_leaf` reachability already in hand). The honest,
  checkable form: `slot`'s designating object is not last-referenced from within `subtree(slot)`
  (so no `delete` on the revoke walk reaches `obj_refs == 0` on a container holding `slot`).
- **The theorem.** Under the precondition: `revoke`'s post adds
  `!is_empty_cap(final.slot_view[slot].cap)` (the revoked root survives) alongside the existing
  `first_child is None` + `cspace_wf`. The proof leans on 6d's `delete` census/frame contract
  (which slots a `delete` may empty: the deleted slot + any resident of a cspace it last-
  unreferences — the precondition excludes `slot` from that set).
- **Key lemmas / risk.** Relating "the revoke walk only deletes nodes in `subtree(slot)`" to
  "`delete` empties only `subtree(slot)` ∪ (last-unref'd cspaces' residents)"; the precondition
  is exactly what makes the second set miss `slot`. The risk is stating the precondition
  *faithfully* (not vacuously) — host-check it against `revoke_can_empty_its_own_root_zombie`
  (the precondition must reject that shape) and a non-zombie shape (must accept, and survival
  holds).
- **Tests.** A `check_revoke_root_survives` on a non-zombie shape (assert the root non-empty
  post-revoke); the existing `revoke_can_empty_its_own_root_zombie` stays, now annotated as the
  precondition's negative witness.
- **Done =** verus green + test + cross-build. Findings doc **45**. *(If 6d's census machinery
  makes this cheap it MAY fold into 6d — but it is budgeted separately, the 4c precedent.)*

### 6f — `refcount_sound` as a system invariant on the construction ops + phase-6 closeout

The census becomes a genuine *system* invariant (preserved by every ref-touching op, not just
teardown), then the documentation closeout (the 3e/4e/5e analog).

- **Scope.** Add the `refcount_sound`-preservation clause to the already-proven **construction
  ops** that touch refs — `derive` (`+1`, `cspace.rs`), `retype_install` (`+1`/`+2`), channel
  `bind` (`bind_refs_post`), `send`/`recv` (cap moves — census-neutral), notification
  `wait`/`signal`/`remove_waiter` (waiter `±1`), timer `arm`/`disarm` (armed `±1`),
  `endpoint_cap_added`/`dropped` (binding `±1`), thread `bind` — each using its **landed phase-
  3/4/5 delta** (the local "refs and census move by the same amount" fact) to discharge the
  global clause. This is broad-but-mechanical assembly: every delta already exists; 6f wires
  each into `refcount_sound` preserved.
- **Scope decision / fallback.** If the breadth balloons (every construction op's contract
  re-touched), scope to the ops on the teardown-reachable refs paths + the ref-creating ops,
  and **record** which construction ops carry only their per-op delta (not yet the assembled
  global clause), with the deltas as the standing evidence — the census is then a teardown-
  family invariant + a documented per-op-delta system property, the honest fallback (do not
  over-claim a system invariant that isn't fully wired).
- **Closeout.**
  - Write `doc/results/46_verus-findings.md` (what closed in 6a–6f; the seL4-zombie
    lexicographic-measure mechanics; the cross-module mutual-recursion `decreases` findings;
    the census-assembly trigger-economy notes; the conditional revoke-survival characterization;
    the doc-numbering note).
  - Update `CLAUDE.md`'s `### Verus` section + the §6 verification-tier table: move
    `delete`/`revoke` (root-survival), `obj_unref`/`destroy_cspace`/`unref_cspace`/
    `unref_aspace`, `channel::destroy_channel`, `thread::destroy_tcb` onto the **proven** list;
    record that **kcore now carries zero `external_body` and zero plain-Rust object
    operations** — the `delete`/`destroy_channel`/`destroy_tcb` trusted-residue note from
    phases 2–5 is *retired*; record `refcount_sound` as a proven invariant (with the scope
    fallback if taken); note the master-plan §7 renumbering (teardown = phase 6; host
    chokepoints → 7; commit core → 8; closeout → 9). **Reaffirm the host chokepoints (§4.7) as
    the next phase (phase 7).** No spec-doc edit — that is the phase-9 closeout (doc 30 §3).
- **Done =** verus green + test + cross-build. Findings doc **46**.

---

## 3. Risks & mitigations (phase-2…5-informed)

- **The cross-module mutual recursion's `decreases` is the deepest proof in the rewrite
  (6d, chief risk).** Verus must accept a lexicographic measure across three modules' bodies.
  Mitigation: 6c verifies the opaque-`delete` members first (no cycle visible) and *fixes the
  measure tags*; 6d only adds the visible `delete` edge; the recorded fallback closes the
  smaller cspace-only cycle if the cross-module cycle is disproportionate (the doc-35/doc-40
  precedent). `count_nonempty` is already `revoke`'s proven measure — the primary component is
  not new.
- **The full `refcount_sound` census balloons (6a definition, 6d/6f preservation).** Six terms,
  each summed with `filter`/`len` (the doc-35 §2.6 n²-trigger trap). Mitigation: the terms are
  the *already-landed* per-op deltas, assembled, not invented; the recount lemmas are
  `lemma_designation_bump` analogs; quarantine every count step into a single-purpose lemma;
  6f carries an explicit scope fallback (teardown-family invariant + documented per-op deltas)
  if the system-wide retrofit is too broad.
- **The underflow gate couples the census to the cluster bodies (6c/6d).** You cannot close
  the bodies without the census (the `refs - 1` needs `refs > 0`, §1.3), so there is no
  termination-only sub-phase to retreat to. Mitigation: the census is stood up first (6a) and
  banked on the simplest term (6b) before the cluster; the fallback keeps the *contracts*
  (host-checked) even if a body stays `external_body`.
- **revoke root-survival's precondition is stated vacuously (6e).** A precondition that
  excludes too much (or nothing) makes the theorem hollow. Mitigation: host-check it both ways
  — it must *reject* `revoke_can_empty_its_own_root_zombie` and *accept* a non-zombie shape on
  which survival genuinely holds (the doc-23 §4 witnesses are the oracle).
- **Scope creep — there is no further phase to defer into.** Phases 4/5 could push teardown
  forward; phase 6 is where it stops. Mitigation: the fallbacks (6d cspace-only cluster, 6f
  teardown-family census) are explicit *recorded* deferrals to a named follow-on, not silent
  drops — and they still land the headline (termination + the cspace census) so the phase is
  not all-or-nothing.
- **The `cspace_view` residency view churns every setter's frame (6a).** Mitigation: an
  isolated, proof-light PR gated by the phase-2…5 proofs staying green; the production
  `KernelStore` is the trusted boundary, so the view needs *no* production-code change — only
  the `ExStore` spec and `ArrayStore` (host) gain bodies (the doc-27 §2.4 argument).

---

## 4. Out of scope (phase-6 non-goals)

- **The host chokepoints (§4.7)** — `urt::time`/`urt::slots`/`dma-pool`/`ipc`/`cas` and the
  Kani-host-harness deletions are **phase 7** (master-plan §7 step 6, renumbered).
- **The commit-protocol recovery core (§4.8)** — phase 8.
- **The spec `2_spec_rev2.md` §6 edit, the master-plan §7 re-write, and the Kani retirement** —
  the phase-9 closeout (doc 30 §3 "spec edits ride the final closeout").
- **Re-opening the page-table / sysabi contracts (phase 5)** — phase 6 touches no aspace
  *walker* op; it consumes only `aspace_destroy`/`aspace_unmap` (the seam) and the frame-
  mapping refcount term.
- **Byte-for-byte payload modelling** — inherited as abstracted from phase 3.
- **A general (non-conditional) revoke-root-survival theorem** — doc 23 §4 proves the zombie
  case is a genuine counterexample; 6e closes the *conditional* theorem, which is the strongest
  true statement.

---

## 5. Exit criterion for phase 6

`cargo verus verify -p kcore` proves the **full cross-object teardown**:

- the cluster bodies `delete`, `obj_unref`, `destroy_cspace`, `unref_cspace`, `unref_aspace`,
  `channel::destroy_channel`, and `thread::destroy_tcb` — **all `external_body` and plain-Rust
  residue removed** (or, on the recorded 6d fallback, the cspace-only cycle closed and the two
  cross-module destructor bodies named as the explicit follow-on residue);
- **termination** of the cross-module recursion under the seL4-zombie lexicographic
  `(count_nonempty, height)` measure — the headline gain the whole rewrite was reaching for
  (plan §1.1: revoke/delete/`obj_unref`/`destroy_*` recursion as theorems, not `debug_assert`);
- the **full `refcount_sound` census** (`refs == slot + binding + waiter + armed-timer +
  frame-mapping + thread-hold` for every object), preserved by the teardown family and by the
  ref-touching construction ops (or the recorded 6f teardown-family-plus-documented-deltas
  fallback) — the §4.1 "every object's `refs` equals the recount" obligation, assembling the
  binding/waiter/armed-timer/frame-mapping terms phases 3/4/5 landed as deltas;
- **`revoke` root-survival** as the conditional non-zombie theorem — doc 23 §4's evidenced
  characterization promoted to a precondition'd `ensures`;

all against the inherited `cspace_wf`/`chan_wf`/`notif_wf`/`timer_wf`/`pt_wf` and the new
`cspace_view` residency + `refcount_sound`; the only `Store`-seam change is the `cspace_view`
residency view + the `aspace_destroy`/`aspace_unmap` minimal contracts; **kcore carries zero
`external_body` and zero plain-Rust object operations** after this phase (the rewrite's `kcore`
goal met — only the host chokepoints (phase 7) and commit core (phase 8) remain in the plan);
the aarch64 `kernel` build and `cargo test -p kcore` are green (the now-proven contracts keep
their `check_*` differential tests as regression guards); `doc/results/41…46` and `CLAUDE.md`
record the new division, the retired trusted-residue note, the master-plan §7 renumbering, and
reaffirm the host chokepoints (§4.7) as phase 7.
