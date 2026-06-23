# Plan — Part B9 detail: preemptible / restartable capability revoke (bounded-quantum `EAGAIN` syscall + a verified revoke-in-progress marker + the atomic→stepwise TLA model)

Detailed, separately-implementable decomposition of **Phase B9** from
`doc/plans/0_address_audit_rev0.md`. B9 is the Wave-3 kernel item that closes the **last
correctness gap between the running kernel and rev1§2.2's revocation contract**: the
descendant-deletion walk must be *"preemptible and restartable,"* but `kcore::cspace::revoke`
(`kcore/src/cspace.rs:11390`) is a straight-line run-to-completion `while` loop executed with IRQs
masked at EL1 (`kernel/src/exceptions.rs:7`, `kcore/src/lib.rs:20-21`). A deep CDT subtree therefore
runs atomically with interrupts masked — unbounded interrupt latency, defeating the rev1§5.4
preemptive scheduler. The existing Verus proof establishes the walk's *termination* and
*descendant-deletion completeness*; it says nothing about *preemptibility* (audit M-1, confirmed).

Unlike B7 and B8 (verification-only, behaviour-identical), **B9 changes observable behaviour**: per
the resolved resume-model decision, `CapRevoke` runs one bounded quantum per call and returns a new
`EAGAIN` retry code, and the single userspace caller loops. The new errno and the caller loop are
the only ABI changes; the opcode and argument shape are unchanged. This is recorded up front as
honesty note 1 so no reviewer expects byte-for-byte behavioural identity.

**Closes (from the parent plan):**
- **M-1 [medium] — revoke is not preemptible/restartable.** rev1§2.2 states the unbounded walk *"is
  preemptible and restartable."* `kcore::cspace::revoke` (`cspace.rs:11390`, loop `:11448-11540`) is
  a straight-line `while store.slot(slot).first_child.is_some()` loop with no preemption point or
  restart entry, run non-preemptibly. A large subtree monopolizes the CPU with interrupts masked
  (unbounded interrupt latency); the Verus proof establishes *termination*
  (`decreases count_nonempty(store.slot_view())`), not *preemptibility*. B9 makes each `CapRevoke`
  call do a **bounded quantum** of leaf-deletions and return — so interrupt latency is bounded by one
  quantum — and makes the operation **restartable** across calls under a verified
  revoke-in-progress marker that keeps it terminating despite concurrent derivation.

**Conforms rev1§2.2 (preemptible, restartable walk) and rev1§5.4 (preemptive scheduler).** B9 is a
*conformance* phase: rev1§2.2 already blesses "preemptible and restartable" as the target (Part A);
B9 brings the code into conformance and mechanizes the new step-safety with both Verus (per-step) and
TLA+ (interleaving + liveness). It does **not** soften the spec.

**Spec target (blessed in rev1 — B9 conforms code to it; one clarifying touch, see honesty note 5):**
- **rev1§2.2 "Capabilities and revocation"** (`spec_rev1.md:48`) — *"Revoking a cap eagerly deletes
  all of its descendants; because that walk is unbounded, it is preemptible and restartable."* This
  is the exact claim B9 makes true. rev1§2.2 also fixes the structural exclusivity story revoke
  serves (`:48-52`): *"untyped memory may be retyped only once the kernel can establish that no
  outstanding cap references the region … operationalized structurally, as the untyped has no
  immediate CDT child,"* and the sees-through-queues guarantee (`:52`): *"revocation reaches caps in
  flight: the descendant-deletion guarantee has no 'except messages in flight' exception."* Both are
  already proven (`revoke`'s `no_live_descendant` ensures) and B9 must **preserve them at every
  preemption point**.
- **rev1§5.4 "Scheduling"** (`spec_rev1.md:383`) — *"strict fixed-priority preemptive scheduling: 32
  levels, round-robin within a level, on a periodic 10 ms tick."* The mechanism whose interrupt
  latency M-1 says revoke defeats; B9's bounded quantum restores the bound.
- **rev1§6.1 "Verification boundary"** (`spec_rev1.md:409-420`) — the preface already lists *"the
  revoke walk structurally forces every transitive descendant gone"* among the fully-proved
  properties (`:411-413`). B9 records, in the trusted-base ledger's verified-surface scope paragraph
  and in a §6.1 note, that the walk's **preemptibility** is now mechanized too — per-step safety in
  Verus, the cross-restart interleaving + liveness in the TLA `CapRevocation` model. The scheduler
  *policy*, the exception entry, and the asm context switch stay **[trusted]** (§6.1(d)) — B9 adds no
  kernel-entry preemption (honesty note 2 explains why the `EAGAIN` design needs none).

Because Part A is blessed first, **B9 makes essentially no normative spec edits** — rev1§2.2 is the
fixed target. The one sanctioned touch is a single clarifying sentence in rev1§2.2/§2.7 recording
that "restartable" is surfaced as a bounded-quantum syscall returning a retry status (honesty note
5), plus the A4-style "record the mechanized status" updates to §6.1's revoke line and the ledger.

**Primary files:**
- `kcore/src/cspace.rs` — the **verified core**:
  - `revoke` `:11390` (`requires`/`ensures` `:11392-11447`, the loop `:11448-11540`, `decreases
    count_nonempty(store.slot_view())`, the exit lemma `lemma_childless_no_descendant` call `:11537`).
    B9A refactors the unbounded loop into a **bounded `revoke_step`** returning a status.
  - `descend_to_leaf` `:11304-11335` (the leaf-finding parent-walk with a ghost-rank `decreases`) —
    the template for B9A's **ancestor-walk guard** (`descend_to_leaf` run *upward*).
  - `delete` `:10988-11288`, `delete_prepare` `:10783-10961`, `cdt_unlink` `:9071-9370` — the
    single-cap teardown the loop already calls leaf-first; **unchanged** (a leaf has no children, so
    `cdt_unlink`'s re-parent branch is trivial — the deletion order B9C's TLA now checks).
  - `derive` `:8845` (`Result<(),()>`) — B9A adds the **revoking-marker guard**: refuse if `src`'s
    ancestor chain reaches a revoking root.
  - `Cap` `:120-135`, `CapSlot` `:142-161` — the marker's home (Design decision 2). `is_empty_cap`,
    `count_nonempty`, `only_empties` `:3671+`, `cspace_wf`/`cdt_wf` `:1579-1627`, `is_descendant`/
    `is_parent_path`/`no_live_descendant` `:1515-1573` — the CDT predicates the marker must **not**
    perturb (they key off `cap`/links, not the new field).
  - the Store view accessors `:427-471` (`slot_view` `:427`, `slot`/`set_slot` `:467-472`) — the
    marker rides `slot_view` if it lands on `CapSlot` (Design decision 2, adopted), needing **no new
    Store seam**.
- `kcore/src/store.rs` — the `Store` trait `slot`/`set_slot` `:49-50`. Touched only if the marker is
  given its own view (the rejected alternative).
- `kcore/src/sysabi.rs` — `Sys::CapRevoke { slot }` `:50`, decode `6 => …` `:145` (opcode unchanged;
  the verified `sysabi::decode` is **not** touched — the ABI *shape* is identical, only the return
  contract changes, which lives in the kernel shell).
- `kernel/src/syscall.rs` — the `Sys::CapRevoke` handler `:269-276` (rewrite to call `revoke_step`
  with a quantum budget and map `Done → 0`, `More → ERR_AGAIN`); the errno block `:59-69` (add
  `ERR_AGAIN`); the `CapCopy`/`CapMint` handlers that call `derive` (map the new refusal to an
  errno).
- `kernel/src/cspace.rs` — the trusted shell wrapper `revoke` `:31-33` (becomes the bounded-step
  wrapper returning a status).
- `ipc/src/sys.rs` — the userspace errno block `:6-16` (add `ERR_AGAIN`, kept in sync with the kernel
  block); the `cap_revoke(slot)` libcall `:157` (pass `EAGAIN` through, add a `cap_revoke_all`
  looping convenience).
- `user/shell/src/main.rs:575` — `sys::cap_revoke(DONATION)` → the looping form (the one caller that
  must adapt to `EAGAIN`).
- `tla/cap_revocation/CapRevocation.tla` — `Revoke(c)` `:201-214` (atomic) → **stepwise**
  `RevokeBegin`/`RevokeStep`/`RevokeEnd` over a new `revoking` variable; `Copy` `:142-149` gains the
  ancestor-revoking guard; `Descendants` `:115-118`; the invariants `LiveParent` `:277-278`,
  `FireSafe` `:285-287`, `DeadNowhere` `:271-273`, `MoveSemantics` `:264-267` (now checked at every
  mid-revoke state); the header comment `:50-55` (drop "Revoke is atomic here"). The `Next`
  disjunction `:230-239`.
- `tla/cap_revocation/CapRevocation.cfg` — add `PROPERTY EventuallyRevoked` (+ the fairness the
  liveness needs); new `CapRevocation_NegControl.cfg` (×2 controls — see Design decision 3).
- `doc/spec/spec_rev1.md` — the one clarifying sentence in rev1§2.2 (`:48`) / §2.7; the §6.1 revoke
  line `:411-413` records preemptibility mechanized (no [verifying] flip — honesty note 4).
- `doc/guidelines/verus_trusted-base.md` — the verified-surface scope paragraph `:15-35` (add "the
  preemptible revoke walk: bounded `revoke_step`, the revoking marker, the derive guard"); the
  Baselines `:126-138` (raise the kcore total; update the `CapRevocation` line with the stepwise
  re-check + the committed negative controls).

Secondary: `kcore/src/test_store.rs` (host unit tests: a bounded `revoke_step` over a synthetic deep
subtree completes in ⌈N/budget⌉ calls, leaves no descendant, and refuses an interleaved `derive`
into the subtree); `tools/tla/tla-model-check.sh` (the TLC runner for the re-checked model + the
negative controls).

---

## Verification tier & baseline (applies to all sub-phases)

B9 spans **three tiers**, mirroring the parent plan's "preserve termination while adding a re-entrant
restart state; re-verify the walk's well-formedness and termination across restart":
- **Verus (`kcore`)** — per-step safety + per-call termination: each `revoke_step` call preserves
  every cspace invariant, only empties, makes bounded progress, and on completion yields
  `no_live_descendant`; `derive` refuses growth into a revoking subtree.
- **TLA+ (`CapRevocation`)** — the *cross-step* properties Verus cannot express: that safety
  (`LiveParent`/`FireSafe`/`DeadNowhere`) holds at every preemption point under arbitrary
  interleaving, and that a started revoke *eventually* completes (liveness) given the guard. This is
  the home of the leaf-first obligation the model header `:50-55` records today as a comment.
- **Shell (kernel + userspace)** — the `EAGAIN` plumbing and the QEMU boot, trusted (§6.1(d)).

Five honesty notes up front:

1. **B9 is NOT behaviour-identical — it changes the `CapRevoke` ABI (deliberately).** Per the
   resolved resume-model decision, `CapRevoke` returns a new `EAGAIN` (`-12`) when a quantum ends
   with work remaining, and the userspace caller loops. This is the one place B9 diverges from
   B7/B8's verification-only posture. The *opcode, argument, and decode* are unchanged
   (`sysabi.rs:50,145` untouched); only the return contract and the single caller adapt. The
   regression gate is therefore **the QEMU boot still green AND the shell's `cap_revoke(DONATION)`
   still fully revokes** (now via the loop), not byte-for-byte behavioural identity.

2. **The `EAGAIN` design needs NO kernel-entry preemption change — preemption falls out of the
   existing EL0 path.** Because each `CapRevoke` returns within a bounded quantum, the normal
   syscall-exit `maybe_switch` (`thread.rs:155-184`) runs promptly, and the caller re-issues from
   EL0 where IRQs are *unmasked* — so a pending 10 ms tick is taken between retries and the scheduler
   round-robins. **`kernel/src/exceptions.rs` and the EL1 IRQ-masking model are left unchanged**: B9
   adds no mid-syscall unmask, no continuation in the TCB, no scheduler-tick hook. Interrupt latency
   is bounded by *one quantum's deletions*, chosen ≪ the tick period. (The parent plan listed
   `exceptions.rs`/"the scheduler tick path" as candidate touches; the `EAGAIN` choice makes them
   no-ops — recorded here so their absence is not read as a gap.)

3. **The gate is a floor that rises; no existing proof is weakened.** `cargo verus verify -p kcore`
   is **374/0** today (ledger `:132`). B9A adds verified items — the bounded `revoke_step` and its
   status `ensures`, the ancestor-walk guard, the marker set/clear discipline and its frame lemmas —
   so the count goes **above 374** (record the new total in the ledger). The four `external_body`
   seams and the `assume_specification`s are **untouched**; B9 adds verified ops and one `CapSlot`
   field, it does not widen the trusted base. The TLA `CapRevocation` re-check replaces the atomic
   `Revoke` with a richer stepwise model — its state count grows from the recorded ~799k; record the
   new figure and commit the negative controls.

4. **No §6.1 `[verifying]` flip — B9 is a conformance + verified-surface gain (like B8C's ready
   queue).** rev1§6.1 carries no `[verifying]` tag for revoke preemptibility; rev1§2.2 simply states
   the walk *is* preemptible/restartable as a standing claim, and §6.1's preface already lists the
   descendant-deletion completeness among proved properties. So B9 makes **no normative §6.1 edit**:
   it records the *preemptibility* gain in the ledger scope paragraph + Baselines and adds a §6.1
   sentence noting the per-step safety is mechanized (Verus) and the interleaving/liveness modeled
   (TLA). The scheduler *policy*, exception entry, and asm switch stay literally [trusted].

5. **The one clarifying spec touch.** rev1§2.2 says "restartable" but does not say *how* it is
   surfaced. Because the resolved design surfaces it as a userspace-visible bounded-quantum syscall
   returning a retry status, B9C adds **one sentence** to rev1§2.2 (cross-referencing the §2.7
   syscall-boundary decode discipline): "restartable is surfaced as a bounded per-call quantum
   returning a retry status (`EAGAIN`) until the subtree is empty; a revoke-in-progress marker
   refuses derivation into the subtree so the walk terminates under concurrent derivation." This is a
   clarification of an already-blessed claim, not a new claim — flagged for sign-off in case the
   reviewer prefers to treat the surfacing as a pure implementation detail (then drop the sentence).

**Baseline to re-establish at end of B9:**
- `cargo verus verify -p kcore` ≥ **374/0**, **> 374** after B9A (record the new total in the ledger).
  The four `external_body` + `assume_specification`s unchanged.
- TLA: `CapRevocation` (stepwise) re-checks green with `LiveParent`/`FireSafe`/`DeadNowhere`/
  `MoveSemantics`/`RevokedDead` at every state and `EventuallyRevoked` satisfied; **both committed
  negative controls fail** (non-leaf delete → `LiveParent` CEX; guard removed → `EventuallyRevoked`
  livelock CEX). Record the new state count. `CapRevocation_Teardown` (TSpec) unchanged.
- The aarch64 build boots: `cd kernel && cargo build` + the QEMU boot smoke pass; the shell's
  donation revoke completes via the `EAGAIN` loop (functional, not just compiling).
- `cargo test -p kcore` green (the `test_store` bounded-step + guard units); `cargo build -p ipc`
  and the user binaries build against the new errno + libcall.

---

## Design decision 1 — the preemption mechanism: a bounded-quantum `EAGAIN` syscall, restartable from the root *(resolve in B9A/B9B)*

The revoke loop is `while store.slot(slot).first_child.is_some() { let leaf =
descend_to_leaf(first_child); delete(leaf); }` — it already deletes **leaf-first** and is **already
restartable from just the root slot**: re-entering `revoke(store, root)` from any well-formed
intermediate state resumes correctly because the loop condition re-reads `first_child` and the tree
is strictly smaller. No cursor beyond the root is needed.

- **Adopted (resume model = userspace-visible restart) — split the loop into a verified bounded
  `revoke_step` and surface it as an `EAGAIN`-returning syscall the caller loops.** Concretely:
  1. **`kcore::cspace::revoke_step<S: Store>(store, slot, budget: usize) -> RevokeStatus`** — runs at
     most `budget` leaf-deletions of the existing loop body, returning `Done` when
     `first_child.is_none()` (subtree empty) and `More` when the budget is exhausted with children
     remaining. `requires` the same precondition bundle as `revoke` (`cspace_wf`, `refcount_sound`,
     `caps_consistent`, `end_caps_sound`, `census_dom_complete`, `ready_wf`/`ready_complete`, the slot
     live and non-empty); `ensures` the **same** invariant bundle + `only_empties` + the
     conditional-root-survival/death-provenance theorems + the *partial-progress* fact
     (`count_nonempty` strictly dropped by the work done, or `Done` with `no_live_descendant`). The
     per-call `decreases` is `min(budget, count_nonempty)` — bounded termination of one quantum.
  2. **The kernel handler** (`syscall.rs:269`) calls `revoke_step(&mut KernelStore, SlotId(..),
     REVOKE_QUANTUM)` and maps `Done → Some(0)`, `More → Some(ERR_AGAIN)`. `REVOKE_QUANTUM` is a
     small shell constant (e.g. 16–64 deletions) chosen so a quantum's wall-clock is ≪ the 10 ms
     tick; tuning is shell policy (§6.1(d)), not a verified parameter.
  3. **The userspace libcall** (`ipc/src/sys.rs:157`) passes `EAGAIN` through unchanged, plus a
     `cap_revoke_all(slot)` convenience that loops `while cap_revoke(slot) == EAGAIN {}` (optionally
     relinquishing the CPU between tries if a yield syscall exists; otherwise the round-robin tick
     preempts the busy loop). The shell's `cap_revoke(DONATION)` (`user/shell/src/main.rs:575`)
     becomes `cap_revoke_all(DONATION)`.
  - **Decisive reasons:** (a) it makes "preemptible" true with the *minimum kernel surface* — no
    mid-syscall unmask, no TCB continuation, no scheduler-tick hook (honesty note 2): the bounded
    return + EL0-unmasked retry path delivers the latency bound for free; (b) it makes "restartable"
    true in the **strongest** sense — a fresh syscall from the same root resumes, exploiting the
    loop's natural restartability, so the only state crossing the gap is the slot itself; (c) the
    verified core stays a pure, bounded, total step — exactly what Verus proves well — while the
    *when-to-yield* policy lives in the trusted shell where §6.1(d) already puts the scheduler.
- **Rejected — transparent kernel continuation (thread "blocks in revoke," kernel re-drives).**
  Would keep the ABI byte-identical (no `EAGAIN`), but requires a new `BlockedRevoke` TCB state + a
  `revoking: Option<SlotId>` continuation field in the verified `Tcb`/`tcb_view` (broad frame-churn
  across cspace.rs) and a scheduler-side re-drive hook (new trusted scheduler logic). The user chose
  the userspace-visible form; recorded here as the considered alternative and why it is heavier.
- **Rejected — leave the loop unbounded and only unmask IRQs at a preemption point.** Bounds latency
  but reintroduces full mid-syscall concurrency *inside* the masked region (the hardest case to
  reason about) and still needs the marker; the `EAGAIN` form gets the same latency bound with a
  clean syscall boundary and no EL1 unmask.

**Recommendation: add the verified bounded `revoke_step` returning `Done`/`More`; map `More → EAGAIN`
in the handler; add `EAGAIN` to both errno lists; loop in the userspace `cap_revoke_all` and the one
shell caller. Leave `exceptions.rs` and the masking model unchanged.**

---

## Design decision 2 — preserving termination across restarts: a verified revoke-in-progress marker + a `derive` guard *(resolve in B9A)*

Once revoke yields between `EAGAIN` calls, another thread can `derive` a **new** child into the
subtree at a preemption point (single-core, but interleaved). The existing termination measure
`decreases count_nonempty(store.slot_view())` then breaks across calls: revoke frees a slot, an
adversary reuses it to derive a fresh descendant, and the multi-call revoke can livelock. Per the
resolved decision, B9 adds a **marker** that forbids such growth.

- **Adopted — a `revoking: bool` field on `CapSlot`, set on the root for the duration of the walk,
  with `derive` refusing any derivation whose source's ancestor chain reaches a revoking root.**
  Concretely:
  1. **Marker placement — on `CapSlot`** (`cspace.rs:143`), so it rides the already-framed
     `slot_view` with **no new Store seam** (a new seam would enlarge the trusted base). Only the
     **root** is ever marked (one bit per active revoke). The CDT predicates are undisturbed:
     `is_empty_cap`/`count_nonempty`/`only_empties` key off `.cap`, and `cdt_wf`/`is_descendant` off
     the links — none read `revoking`. The churn is confined to the few **full-`CapSlot`-equality**
     frames (audited mechanically; `unhomed_frozen`/`emptied_via_dead_home` compare `.cap` only and
     are unaffected). `CapSlot::empty()` sets `revoking: false`.
  2. **Set/clear discipline (idempotent, re-entrant across `EAGAIN`).** `revoke_step` sets
     `root.revoking = true` on entry when the root has children and is not yet marked; clears it when
     `first_child` becomes `None` (the `Done` path). Because the root survives the walk and only
     leaves are deleted, the bit persists naturally across calls. A `revoke_step` on an
     already-`Done` (childless) root clears the bit and returns `Done` — so a caller that gives up
     mid-loop leaves only its *own* subtree frozen until it resumes (a privileged, self-inflicted
     condition — recorded).
  3. **The `derive` guard (verified).** Add to `kcore::cspace::derive` (`cspace.rs:8845`) a check
     `!ancestor_or_self_revoking(store, src)` — an **exec ancestor-walk** from `src` up the `parent`
     chain (the mirror of `descend_to_leaf`, terminating on the `acyclic`/`valid_prank` ghost rank)
     that returns `true` iff any node on the path (including `src`) has `revoking`. On a hit, `derive`
     returns `Err`; the `CapCopy`/`CapMint` shell handlers map it to `ERR_AGAIN` (retry after the
     revoke finishes) or `ERR_STATE` (decide and record — `ERR_AGAIN` is recommended so the deriver
     can simply retry). Verus `ensures`: when the guard fires, `slot_view` is unchanged (no new cap);
     when it passes, the existing derive `ensures` stand.
  4. **What this buys the proofs.** With the guard, `count_nonempty` restricted to the root's subtree
     is **non-increasing across the gap between `EAGAIN` calls** (no derivation adds a descendant
     below a revoking root; deletes only remove). Verus proves *per-call* progress + safety; the
     *cross-call* "subtree count never grows, so the loop of calls terminates" is the **liveness**
     property mechanized in TLA (Design decision 3) — the natural division (Verus = per-step
     safety/termination; TLA = interleaved liveness).
  - **Decisive reasons:** (a) it is the minimum that restores termination under the chosen
    userspace-restart model; (b) only `derive` needs guarding — `Send`/`Bind` *move* caps (parent
    unchanged), so a moved descendant stays a descendant and revoke still reaches it (the
    sees-through-queues guarantee), and `retype` is already gated on `Descendants = {}`; (c) the
    ancestor-walk reuses the proven `descend_to_leaf`/`valid_prank` machinery — no new proof shape.
- **Rejected — mark every node in the subtree (O(1) `derive` check, O(subtree) marking).** The
  marking pass is itself an unbounded walk needing its own preemption; root-only + a bounded
  ancestor-walk in the (cold) `derive` path is strictly cheaper.
- **Rejected — a separate `revoking_view(): Set<SlotId>` Store seam.** Isolates the marker from
  `CapSlot` frame churn but adds a **new trusted Store seam** and forces the guard to relate
  `slot_view` (the parent chain) to a second view; the on-`CapSlot` field keeps everything inside the
  one already-verified map. (Fall back to this only if the `CapSlot`-equality frame churn proves
  disproportionate — record which was taken.)
- **Rejected — no marker, rely on finite slots + restart (the "best-effort" reading).** Safe (retype
  stays blocked until clean) but livelockable under hostile concurrent derivation; the user chose the
  marker for guaranteed termination.

**Recommendation: add `revoking: bool` to `CapSlot`; set/clear it on the root in `revoke_step`; add
the verified ancestor-walk guard to `derive`; map the refusal to `ERR_AGAIN`. Confirm the
`CapSlot`-equality frame churn is mechanical (fall back to a `revoking_view` seam only if not).**

---

## Design decision 3 — the TLA model: atomic `Revoke` → stepwise interleaving + liveness + two negative controls *(resolve in B9C)*

`CapRevocation.tla` models `Revoke(c)` as **one atomic step** deleting all `Descendants(c)`
(`:201-214`). Its header already anticipates B9 (`:50-55`): *"Revoke is atomic here. The kernel walk
is preemptible/restartable; its postcondition (no live descendants on completion) is what this model
checks. The deletion order constraint the implementation must respect (delete leaf-first / DFS
post-order, so LiveParent holds at every preemption point) is recorded here as the obligation."*
Audit §2.6 confirms the atomic model is faithful **only because** the kernel is non-preemptible — the
very assumption B9 removes. So B9C must make the recorded obligation a **checked** property.

- **Adopted (TLA scope = full) — convert `Revoke` to a stepwise leaf-deletion action interleaving
  with all other actions, guard `Copy`, and add a liveness property + two negative controls.**
  Concretely:
  1. **New variable `revoking`** (a `SUBSET CapIds` — the marked roots), `revoking = {}` in `Init`,
     added to `crVars`/`vars` and framed in the TSpec half (`UNCHANGED` under `TNext`).
  2. **`RevokeBegin(c)`** — `c \in live /\ Descendants(c) /= {} /\ c \notin revoking`: set
     `revoking' = revoking \cup {c}` (the marker), no deletion yet (mirrors `revoke_step` marking the
     root).
  3. **`RevokeStep(c)`** — `c \in revoking`: pick a **leaf** `l \in Descendants(c)` (a live cap with
     no live children), delete *only* `l` (remove from `live`/`cspaces`/`queues`/`bindings`, set
     `parent[l] = NULL`, add to `revoked`). One preemption-point-sized step.
  4. **`RevokeEnd(c)`** — `c \in revoking /\ Descendants(c) = {}`: `revoking' = revoking \ {c}` (clear
     the marker), `c` survives.
  5. **Guard `Copy`** (`:142-149`) — add `~AncestorOrSelfRevoking(src)`, where `AncestorOrSelfRevoking
     (x)` walks `parent` from `x` and is true iff it meets a member of `revoking`. This models the
     `derive` guard that makes the walk terminate.
  6. **`Next`** (`:230-239`) replaces `Revoke(c)` with `RevokeBegin/RevokeStep/RevokeEnd`; the
     interior single-cap deletes (rebind, channel-queue cleanup) stay unmodeled as before.
  7. **Safety, now checked at every mid-revoke state.** `LiveParent`/`FireSafe`/`DeadNowhere`/
     `MoveSemantics`/`RevokedDead` are unchanged invariants but now evaluated across all interleavings
     of partial revokes with `Copy`/`Send`/`Receive`/`Bind`/`ThreadExit`/`Retype`. Leaf-only deletion
     is what keeps `LiveParent` true at every step (deleting a childless cap orphans nobody) — the
     header obligation, mechanized.
  8. **Liveness `EventuallyRevoked`** — `\A c : (c \in revoking) ~> (Descendants(c) = {})`, under
     weak fairness on `RevokeStep`. Holds **because** the `Copy` guard forbids re-insertion below a
     revoking root, so the subtree shrinks monotonically.
  9. **Two committed negative controls** (`CapRevocation_NegControl.cfg`, the B7 pattern):
     - **Safety control:** a `RevokeStepBad` that deletes an *interior* (non-leaf) cap → orphans its
       children → `LiveParent` counterexample. Proves the leaf-first ordering is load-bearing.
     - **Liveness control:** drop the `Copy` guard → an adversarial `Copy`↔`RevokeStep` cycle re-grows
       the subtree → `EventuallyRevoked` livelock counterexample. Proves the marker/guard is
       load-bearing for completion.
  10. **Header + ledger.** Drop "Revoke is atomic here" from `:50-55`; state that the walk is modeled
      stepwise with the leaf-first obligation now checked and completion proven under the guard.
      Record the new state count and the committed controls in the ledger Baselines `:126-138`.
  - **Decisive reasons:** (a) it closes the audit §2.6 honesty point in full — the property that was
    "faithful only under non-preemption" is now checked *under* preemption; (b) it matches the
    project's blessed discipline (B7's `RecoverReconstructs` + committed negative control); (c) the
    two controls pin *both* load-bearing design choices (leaf-first ordering, the derive guard) so a
    future refactor that breaks either is caught.
- **Rejected — lighter (Verus per-step only; TLA stays atomic).** Leaves the audit §2.6 point only
  partially addressed and the leaf-first/guard obligations unchecked. The user chose the full fork.

**Recommendation: stepwise `RevokeBegin`/`RevokeStep`/`RevokeEnd` over a `revoking` variable; guard
`Copy`; check safety at every state; add `EventuallyRevoked` under fairness; commit the two negative
controls; update the header + ledger with the new state count.**

---

## Sub-phase B9A — verified bounded `revoke_step` + the revoking marker + the `derive` guard *(closes M-1's verification core; conforms rev1§2.2)*

The Verus deliverable. Refactors the unbounded `revoke` loop into a bounded, restartable step, adds
the revoke-in-progress marker on `CapSlot`, and guards `derive` against growth into a revoking
subtree — preserving every existing cspace invariant and the descendant-deletion completeness, and
adding the partial-progress and marker `ensures`. Independent of B9C; B9B depends on its signatures.

- **Touches:**
  - `kcore/src/cspace.rs` — `revoke` `:11390` → bounded `revoke_step(store, slot, budget) ->
    RevokeStatus` (Design decision 1.1); `revoking: bool` on `CapSlot` `:143` + `CapSlot::empty()`
    `:152` (Design decision 2.1); the set/clear in `revoke_step` (2.2); the exec ancestor-walk guard
    in `derive` `:8845` (2.3, mirroring `descend_to_leaf` `:11304`); audit the full-`CapSlot`-equality
    frames for the new field. Keep `delete`/`delete_prepare`/`cdt_unlink` and `lemma_childless_no_
    descendant` `:1551` unchanged (the leaf-step machinery already exists).
  - `kcore/src/test_store.rs` — host units: a deep synthetic subtree revokes to empty across
    ⌈n/budget⌉ `revoke_step` calls (`Done` only on the last) leaving `no_live_descendant`; a `derive`
    whose source is under a revoking root is refused and mutates nothing; the marker clears on `Done`.
  - `doc/guidelines/verus_trusted-base.md` — record the raised kcore total `:132`.
- **Depends on:** Part A blessed. No intra-B9 dependency (B9B consumes its signatures; B9C is a
  separate tier).
- **Work:** Design decisions 1 (the `revoke_step` split + status `ensures` + per-call `decreases`) and
  2 (the marker field + set/clear + the verified guard). Re-establish the full invariant bundle on
  `revoke_step` and the partial-progress fact; prove the guard's no-op-on-refusal `ensures`.
- **Acceptance:**
  - `revoke_step` verifies with the `revoke`-equivalent `ensures` on `Done` (`no_live_descendant`,
    `cspace_wf`, all invariants, `only_empties`, conditional-root survival/death-provenance) and the
    partial-progress `ensures` on `More`; per-call termination via `decreases min(budget,
    count_nonempty)`.
  - `derive` refuses (no state change) when the source's ancestor chain reaches a revoking root, and
    is otherwise unchanged.
  - `cargo verus verify -p kcore` **> 374/0** (record the new total); `cargo test -p kcore` green.
- **Effort/Risk:** M–L / medium-high. The bounded-step `ensures` reuse the existing loop invariants;
  the substance is the `CapSlot`-field frame audit and the ancestor-walk guard proof.

---

## Sub-phase B9B — the `EAGAIN` syscall surface + userspace retry loop *(closes M-1's latency bound; conforms rev1§5.4)*

The shell deliverable (trusted, §6.1(d)). Adds the `EAGAIN` errno, wires the `CapRevoke` handler to
`revoke_step` with a quantum budget, and adapts the userspace libcall + the one caller to loop. Leaves
the exception/masking model untouched (honesty note 2). Depends on B9A's signatures.

- **Touches:**
  - `kernel/src/syscall.rs` — add `pub const ERR_AGAIN: i64 = -12` to the errno block `:59-69`;
    rewrite the `Sys::CapRevoke` handler `:269-276` to call `cspace::revoke_step(.., REVOKE_QUANTUM)`
    and map `Done → Some(0)`, `More → Some(ERR_AGAIN)`; map the new `derive` refusal in the
    `CapCopy`/`CapMint` handlers to `ERR_AGAIN` (or `ERR_STATE` — record the choice).
  - `kernel/src/cspace.rs` — the wrapper `revoke` `:31-33` → a bounded-step wrapper returning the
    status (or fold into the handler).
  - `ipc/src/sys.rs` — add `ERR_AGAIN` to the errno block `:6-16` (kept in lockstep with the kernel
    block); keep `cap_revoke` `:157` passing `EAGAIN` through; add `cap_revoke_all(slot)` that loops
    until non-`EAGAIN`.
  - `user/shell/src/main.rs:575` — `sys::cap_revoke(DONATION)` → `sys::cap_revoke_all(DONATION)`.
- **Depends on:** B9A (the `revoke_step` signature + `RevokeStatus`). Independent of B9C.
- **Work:** the errno addition (both lists), the handler mapping + `REVOKE_QUANTUM` constant, the
  libcall loop, the shell caller. Confirm no other userspace caller of `cap_revoke` exists (grep
  found only the shell). **No `exceptions.rs` change** — verify the boot-time donation revoke still
  completes via the loop and that a long revoke no longer stalls the tick (a synthetic deep-subtree
  smoke; M-1 acceptance: "does not block timer interrupts beyond one quantum").
- **Acceptance:**
  - `CapRevoke` returns `EAGAIN` mid-walk and `0` on completion; the shell's donation revoke
    completes via `cap_revoke_all`; the kernel and userspace errno blocks agree.
  - QEMU boot green; a synthetic deep-CDT revoke yields the CPU each quantum (timer ticks observed
    interleaving — the M-1 latency bound).
  - `cargo build` (kernel) + `cargo build -p ipc` + the user binaries build.
- **Effort/Risk:** S–M / low–medium. Mostly mechanical wiring; the only judgment is `REVOKE_QUANTUM`
  and the refused-derive errno. The masking model is untouched, which removes the riskiest kernel
  work.

---

## Sub-phase B9C — TLA `CapRevocation`: atomic → stepwise interleaving + liveness + negative controls *(closes the audit §2.6 honesty point)*

The formal-model deliverable. Converts the atomic `Revoke` to a stepwise, interleaved model that
checks the leaf-first safety obligation and completion liveness, with two committed negative controls.
Independent of B9A/B9B (a separate tier) — recommend landing alongside them so the ledger Baselines
update once.

- **Touches:**
  - `tla/cap_revocation/CapRevocation.tla` — add the `revoking` variable (Init/`crVars`/`vars`,
    `UNCHANGED` in `TNext`); replace `Revoke(c)` `:201-214` with `RevokeBegin`/`RevokeStep`/
    `RevokeEnd`; guard `Copy` `:142-149` with `~AncestorOrSelfRevoking(src)`; add the
    `AncestorOrSelfRevoking` operator and a leaf predicate; update `Next` `:230-239`; add
    `EventuallyRevoked`; drop "Revoke is atomic here" from the header `:50-55` (record the stepwise
    model + the now-checked obligation). `LiveParent`/`FireSafe`/`DeadNowhere`/`MoveSemantics`/
    `RevokedDead` unchanged but now checked across interleavings. TSpec half untouched.
  - `tla/cap_revocation/CapRevocation.cfg` — add `PROPERTY EventuallyRevoked` and the fairness it
    needs; the `RevokeStep`-style actions in the spec.
  - `tla/cap_revocation/CapRevocation_NegControl.cfg` (new) — the two controls (non-leaf delete →
    `LiveParent` CEX; guard removed → `EventuallyRevoked` livelock CEX), each driven by a spec flag /
    alternate action per the B7 negative-control style.
  - `doc/spec/spec_rev1.md` — the one clarifying rev1§2.2/§2.7 sentence (honesty note 5); the §6.1
    revoke-line note that preemptibility is mechanized (Verus per-step + TLA interleaving/liveness) —
    **no [verifying] flip** (honesty note 4).
  - `doc/guidelines/verus_trusted-base.md` — add the preemptible revoke walk to the verified-surface
    scope paragraph `:15-35`; update the `CapRevocation` Baselines line `:126-138` (stepwise re-check,
    new state count, the committed negative controls).
- **Depends on:** Part A blessed. Independent of B9A/B9B (models the same design; no code dependency).
- **Work:** Design decision 3 — the stepwise actions + the `revoking` variable + the `Copy` guard +
  the liveness property + the two negative controls; re-run TLC (`tools/tla/tla-model-check.sh`),
  record the state count; the header/spec/ledger updates.
- **Acceptance:**
  - `CapRevocation` (stepwise) re-checks green: all safety invariants hold at every interleaved state;
    `EventuallyRevoked` holds under fairness.
  - **Both negative controls fail** with the expected counterexamples (non-leaf delete →
    `LiveParent`; no guard → liveness livelock), committed.
  - The header no longer claims atomicity; the ledger scope paragraph + Baselines record the walk's
    preemptibility and the new state count; §6.1's revoke line notes the mechanized status; no
    [verifying] flip.
- **Effort/Risk:** M / medium. The richest modeling change (the state graph grows from ~799k); the
  two controls and the liveness fairness are the substance, both with B7 precedent.

---

## Execution order

```
B9A  verified bounded revoke_step + revoking marker + derive guard   [Verus; the core; independent]
B9B  EAGAIN syscall surface + userspace retry loop                   [shell; depends on B9A signatures]
B9C  TLA atomic -> stepwise interleaving + liveness + neg controls   [TLA/docs; independent of A/B]
```

- **B9A is the long pole** (the bounded-step `ensures` + the `CapSlot`-field frame audit + the guard
  proof). **B9B depends on B9A's signatures** (`revoke_step`/`RevokeStatus`); it is otherwise small
  and carries the only *behaviour* change (the `EAGAIN` ABI). **B9C is independent** of both — it
  models the same design and can proceed in parallel; land it alongside so the ledger Baselines (the
  shared kcore total and the `CapRevocation` line) update once.
- The parent plan sequences **B9 after B8** so B8's freshly-verified surface (cap-side MAP, priority
  gate, ready queue) is not churned by this refactor; B9 is otherwise independent of B10/B-IRQ. It is
  the parent plan's "L / high — touches the kernel's non-preemption assumption; the hardest kernel
  item." The `EAGAIN` choice deliberately keeps the *kernel-entry* assumption (IRQs masked at EL1)
  **intact** — preemption is delivered at the syscall boundary, not by unmasking mid-syscall — which
  is what de-risks the "high" rating.

## Out of scope for B9 (recorded so it is not mistaken for a gap)

- **Any change to the EL1 IRQ-masking / exception-entry model.** The `EAGAIN` design bounds latency
  via the bounded quantum + the existing EL0-unmasked retry path; `kernel/src/exceptions.rs`, the
  vector table, and `maybe_switch` are unchanged (honesty note 2). B9 adds no mid-syscall unmask, no
  TCB continuation, no scheduler-tick revoke hook.
- **Verifying cross-syscall liveness in Verus.** Completion-across-restarts is a *liveness* property
  over interleaved syscalls — outside Verus's per-function model. It is mechanized in the TLA
  `CapRevocation` model (`EventuallyRevoked` + the guard); Verus carries per-step safety + per-call
  termination only.
- **A transparent (no-ABI-change) resume model.** Rejected in Design decision 1; would need a
  `BlockedRevoke` TCB state + continuation field + scheduler re-drive. The user chose userspace-
  visible `EAGAIN`.
- **Concurrent / incremental / persisted-marking GC and streaming WAL replay.** That is Part C4 (and
  depends on B6, not B9). B9's marker is a *kernel-CDT* revoke-in-progress flag, unrelated to the CAS
  GC mark set.
- **The storage-cap generation counter (rev1§2.2 mass-revocation).** That O(1) handle-invalidation
  path is storage-server/CAS (B1/B5 territory), distinct from the kernel CDT walk B9 makes
  preemptible.
- **Tuning `REVOKE_QUANTUM`.** A shell policy constant (§6.1(d)), not a verified parameter; B9 picks a
  safe default (quantum ≪ tick) and leaves tuning to measurement.
- **Softening rev1§2.2.** B9 conforms code to the blessed "preemptible and restartable" claim; it does
  not weaken it. The one spec touch is the clarifying surfacing sentence (honesty note 5), flagged for
  sign-off.
