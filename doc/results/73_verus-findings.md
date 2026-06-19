# 73 — Follow-on-fix ledger: D-A3 ("revoke sees through queues" as a named obligation)

> A *change* ledger continuing docs 70 (D-A1 split · D-B1 stage 1), 71 (D-B1 Option 2),
> and 72 (D-A2 root-survival theorem). This increment closes the **named-obligation +
> real-op-test** half of **D-A3** and records the remaining ∀-quantified theorem as
> explicit follow-on with its obstruction.

## Provenance

D-A3 (doc 69, Class D1, medium): the load-bearing **"revoke sees through queues"** property
— §3.4 ("revocation finds and deletes in-flight caps like any other descendants … no caveat
in its specification") and the **M1** exit criterion ("revoke verifiably destroys descendants
**including a cap queued in an in-flight message**") — was *provable* from the structural CDT
invariants but was **never a Verus `ensures`, lemma, or test driving real `revoke`**. The
prior nod only *simulated* an emptied ring slot. TLA+ checks it explicitly (`queues' = queues
\ dead`); Verus did not. Disposition: add a `no_live_descendant` predicate + an `ensures`
(or lemma) on `revoke` plus a test driving real `revoke` through a queued descendant; until
then, document that queue-reaching is *inferred* (a change to `cdt_wf`/`slot_move` could
silently break it with no failing obligation).

**Headline:** the named obligation and the real-op witness land; the honest note is written.
`revoke`'s `first_child is None` post pinned only the **direct**-child clause; the transitive
reach (a queued cap is typically a *deeper* descendant) was the inferred half. `revoke` now
exports `ensures no_live_descendant(final, slot)` — *afterward no live slot, in-flight ring
cap included, is a CDT descendant of the target* — discharged at loop exit by the new
`lemma_childless_no_descendant` from `first_child is None` + `cspace_wf`, paired with
`only_empties(old, final)` (the walk destroys, never relabels). A regression in
`cdt_wf`/`slot_move`/`delete` that left a queued (or any) descendant attached now **fails
verification**. Verified: **328 verified, 0 errors** (`cargo verus verify -p kcore`, +1 vs
the doc-72 baseline of 327), **90** host tests (+1 witness), AArch64 `kernel` cross-build
clean. Runtime behaviour unchanged — every code edit is ghost / a contract.

## The change

| Layer | Edit |
|---|---|
| Spec (`doc/spec/2_spec_rev2.md`) | §3.4 "Queue slots are real" bullet: a "Verus-witnessed (D-A3)" note — `no_live_descendant(final, slot)` `ensures` + the `revoke_sees_through_queued_descendant` real-op test; the ∀-quantified initial-descendant emptiness recorded as follow-on. |
| New predicates (`kcore/src/cspace.rs`) | `is_parent_path` (a child→parent walk), `is_descendant` (strict transitive CDT descendant via such a walk), `no_live_descendant` (no live slot is a descendant of the anchor). The subtree predicate the design had deliberately lacked (doc 55 / doc 72 F-72-2). |
| Lemma | `lemma_childless_no_descendant`: `parent_has_first_child(m) ∧ m[anc].first_child is None ⟹ no_live_descendant(m, anc)`. Non-inductive — any descendant chain's topmost step names `anc` as parent, which `parent_has_first_child` forbids when `anc` is childless. |
| `revoke` (cspace.rs) | two new `ensures` — `no_live_descendant(final, slot)` (discharged by the lemma after the loop) and `only_empties(old, final)` (composed from each `delete`'s `only_empties` via `lemma_only_empties_trans`, needing the added dom-equality loop invariant). The §6e / §6e-dual provenance frames are untouched. |
| Test (`test_store.rs`) | `revoke_sees_through_queued_descendant` — a Frame target whose only child is a real channel ring cap (registered in `ChanState`, in the live window); the **real** `revoke` empties the in-flight queued cap, leaves the ring handle pointing at the now-null slot (§3.4), and the un-homed target survives. |

## Findings worth keeping

### F-73-1 — The cheap, robust half is the final-state structural theorem

`first_child is None` + the `parent_has_first_child` clause of `cdt_wf` forces the *whole*
subtree gone, non-inductively: a strict descendant `d` of `anc` has a parent-walk whose
penultimate node names `anc` as parent, and `parent_has_first_child` makes that impossible
when `anc.first_child is None`. So the transitive `no_live_descendant` follows from facts
`revoke` already establishes — one lemma, no loop-invariant threading, no `delete` contract
change. This is the obligation that turns "queue-reaching is structurally implied" into "a
drift in `cdt_wf`/`slot_move` is a *failing* obligation."

### F-73-2 — Why the ∀-quantified old→final theorem is a separate, deeper follow-on

The fully faithful statement — *every slot that was a descendant of `slot` in the **initial**
state is empty in the final state* — does **not** follow from final-state structure. It needs
to connect *old* descendants to *final* emptiness across the walk, and `delete` is **recursive**
(cross-object teardown cascade via `obj_unref`/`destroy_*`) and exposes **no per-slot
parent-edge frame**. A loop invariant tracking "non-empty old-descendants stay descendants"
would have to survive cascade steps that empty arbitrary homed slots while preserving
`cspace_wf` — exactly the CDT-subtree reasoning the design deliberately deferred (doc 72
F-72-2: the codebase has no subtree predicate *by design*; `unhomed_frozen`/`emptied_via_dead_home`
were built to avoid subtree reachability). It is **not** a regression risk given F-73-1 +
the real-op witness, so it stays follow-on rather than blocking this increment.

### F-73-3 — The witness is a *real* queued ring cap, not a simulation

Doc 69 flagged the only existing nod (test ~2965) as one that *simulates* an emptied ring
slot. `revoke_sees_through_queued_descendant` instead registers the descendant as a genuine
channel ring cap (`is_homed_exec` confirms it homes in a channel) and drives the **real**
`revoke`: the queued cap is destroyed, and the channel's `ring_cap` handle still points at the
now-empty slot — the §3.4 "receivers must tolerate null cap slots" outcome, observed rather
than assumed. A queued cap is a descendant because `slot_move` (what `send` uses) inherits the
source's parent edge into the ring slot — the mechanism the audit named as load-bearing.

## Verification evidence

| Gate | Command | Result |
|---|---|---|
| Proof (primary) | `cargo verus verify -p kcore` | **328 verified, 0 errors** (327 baseline + 1) |
| Witnesses | `cargo test -p kcore` | **90 passed, 0 failed** (incl. `revoke_sees_through_queued_descendant`) |
| Shell build | `cd kernel && cargo build` | clean (AArch64 bare-metal; ghost/contract edits erase) |

## Disposition feed-forward

- **D-A3:** the **named-obligation + real-op-test** half is **closed** — `no_live_descendant`
  (+ `only_empties`) is exported by `revoke` and machine-checked, and a real queued ring cap is
  driven to empty through the real `revoke`. The doc-72 feed-forward line ("D-A3 … remains the
  open revoke follow-on") is updated: what remains is only the ∀-quantified initial-descendant
  emptiness theorem (F-73-2), entangled with the cross-object teardown cascade.
- **Open revoke follow-ons (doc 69):** D-A1 (closed, doc 70) and D-A2 (closed, doc 72) leave the
  ∀-quantified old→final subtree-emptiness as the last revoke-side item; it shares the
  no-subtree-predicate-by-design obstruction with the doc-55 §3 work and would build on the
  `is_descendant` predicate introduced here.
