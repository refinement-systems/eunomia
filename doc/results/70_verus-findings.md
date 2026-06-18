# 70 — D-A1 closed: `revoke`'s contract split (vacuous → reachable descendant-deletion)

## Provenance

This document records the resolution of **D-A1** — the single highest-severity finding of
the doc-69 drift audit — together with the genuinely new observations the fix surfaced. It
is a *change* ledger (one finding fixed, its consequences for the neighbouring D-A2/D-A3
findings, and one new structural observation), not a fresh re-derivation. Authority is the
spec (`doc/spec/2_spec_rev2.md` §2.2) and the verified source; the disposition followed is
the one doc 69 recorded for D-A1 ("split the contract so `first_child is None` + `cspace_wf`
are proven **without** `!is_homed`, keeping the extra `!is_empty_cap` root-survival under
`!is_homed`").

**Headline:** `kcore::cspace::revoke`'s verified contract was *vacuous over every real
syscall input* because its `requires !is_homed(old(store), slot)` is false for the
`homed_in_cspace` target every `Sys::CapRevoke` supplies. Dropping that precondition and
demoting root-survival to a conditional `ensures` makes the spec-mandated **descendant-
deletion (`first_child is None`) and `cspace_wf` unconditional and reachable from the real
call path**, while preserving root-survival exactly where it genuinely holds (un-homed
targets). Verified: **316 verified, 0 errors** (`cargo verus verify -p kcore`); all 85
`kcore` host tests pass, including both revoke witnesses. Runtime behaviour is unchanged —
preconditions erase, so this is a verification-scope fix, not a code-defect fix.

---

## The change (kcore/src/cspace.rs, `pub fn revoke`, now line 9300)

**Before** (doc 69 D-A1 anchor):

```
requires …, !is_homed(old(store), slot)              // false for every Sys::CapRevoke target
ensures  cspace_wf(final), dom contains slot,
         final[slot].first_child is None,            // ← gated behind the always-false requires
         !is_empty_cap(final[slot].cap)              // root-survival, unconditional
```

**After:**

```
requires …                                           // (!is_homed dropped)
ensures  cspace_wf(final), dom contains slot,
         final[slot].first_child is None,            // ← now UNCONDITIONAL (cspace.rs:9316)
         !is_homed(old(store), slot)
             ==> !is_empty_cap(final[slot].cap)       // root-survival, CONDITIONAL (cspace.rs:9322)
```

The loop invariant changed in lockstep: the unconditional `!is_homed(store, slot)` /
`!is_empty_cap(store[slot].cap)` pair became home-status *stability*
`is_homed(store, slot) == is_homed(old(store), slot)` (cspace.rs:9338, maintained from
`delete`'s `home_views_frozen` via `lemma_is_homed_stable`) plus the conditional survival
`!is_homed(old(store), slot) ==> !is_empty_cap(store[slot].cap)`. The loop body's
cap-unchanged assertion is now guarded by `if !is_homed(old(store), slot)`, since the homed
(seL4-zombie) case may legitimately empty `slot` via cross-object teardown.

The proof needed **no new lemmas**: `delete` already exports both frames the split consumes —
`home_views_frozen` (cspace.rs:9021) for home-status stability and `unhomed_frozen` (9026) for
the conditional cap-preservation. The split is a *guarded form of the proof that was already
there*.

---

## Findings

### F-70-1 — D-A1 resolved: the contract is no longer vacuous over real inputs

The spec-mandated descendant-deletion (§2.2: "deletes all descendants … the guarantee is
**unconditional**") is now a Verus `ensures` that holds for *any* live target, including the
`homed_in_cspace` slot `cur_slot` resolves for every `Sys::CapRevoke` (syscall.rs:248 → the
unverified wrapper kernel/src/cspace.rs:20 → kcore `revoke`). Before, the entire contract —
descendant-deletion included — was witnessed on the real call shape only by an executable unit
test; it is now a machine-checked postcondition reachable from the kernel's actual inputs.

**Zero-caller safety (re-confirmed during the fix).** Grepping all of `kcore`/`kernel`/`user`
for `revoke(`: the only callers of `kcore::cspace::revoke` are the unverified kernel wrapper
(kernel/src/cspace.rs:20) and the two host tests (test_store.rs:2796, :2842). There is **no
verified kcore caller**, so weakening `requires` and conditionalizing `ensures` cannot break a
verified consumer — the change is contract-monotone for everyone except the proof of `revoke`
itself, which only got stronger (the headline `ensures` lost a hypothesis).

### F-70-2 — Precondition taxonomy: `!is_homed` was *uniquely* poisonous (novel)

The instructive structural observation. `revoke`'s preconditions split into two kinds:

- **Satisfiable system invariants** — `cspace_wf`, `refcount_sound`, `caps_consistent`,
  `end_caps_sound`, `census_dom_complete`. These are the *same* invariants every
  syscall-reached kcore teardown op (`delete`, `destroy_*`) requires; the kernel shell carries
  them across the unverified wrapper as a trusted base. They are *plausibly true* of the live
  store, so a contract gated on them is verified-modulo-a-normal-trusted-seam.
- **The unsatisfiable one** — `!is_homed(old(store), slot)`. For a `cur_slot` target (a cell of
  the caller's cspace) this is *structurally always false*. A contract gated on an
  always-false precondition is not "trusted-modulo-a-seam"; it is **vacuous** — true of no
  input the implementation can ever present.

This is the precise reason D-A1 outranked every other proof-boundary finding in doc 69: the
other accepted-documented seams (D-C1/C2/C3, D-F1/F3) are *trusted but satisfiable*; D-A1 was
*unsatisfiable*, which is a strictly worse failure mode (a green proof of nothing). Removing it
moves `revoke` from "vacuously verified" to "verified at the same trusted-base posture as every
other syscall-reached kcore op." **Corollary / honest caveat:** this is reachability of the
*preconditions*, not an end-to-end proof from the syscall — the five system invariants remain a
shell-carried trusted seam (the unverified wrapper still discharges none of them). The fix
removes the one precondition no input could satisfy; it does not, and was not meant to, close
the wrapper seam.

### F-70-3 — D-A2 is now contract-faithful, not a gap

Doc 69 D-A2 flagged that root-survival was gated by `!is_homed` while a homed (seL4-zombie)
target's cap *can* be self-emptied — a faithfulness gap, because the blanket precondition hid
the homed case entirely. The conditional `ensures !is_homed(old) ==> !is_empty_cap(final)` *is*
the honest statement of D-A2: it **guarantees** survival for un-homed (e.g. donated-untyped)
roots and is **explicitly silent** for homed roots, matching the negative witness
`revoke_can_empty_its_own_root_zombie` (test_store.rs:2764) exactly. The gap *between* contract
and reality closes: the contract now says precisely what holds, with the seL4-zombie self-empty
as the admissible case the antecedent excludes. (D-A2's deeper residue — survival of a
*resident-with-external-reference* root — still needs the refs-monotone "emptied ⟹ a homing
object was destroyed" frame, unchanged and still follow-on.)

### F-70-4 — D-A3 ("sees through queues") is strictly closer, still not closed

With descendant-deletion now a *reachable* `ensures` rather than a vacuous one, the
load-bearing "revoke sees through queues" property (§3.4; M1 exit criterion) is one inference
step from a *reachable* fact: a queued cap is a real CDT descendant (queue slots carry the
parent edge via `slot_move`), and `first_child is None` + `cdt_wf` structurally force the whole
subtree empty. The foundation D-A3's eventual `subtree_empty`/`no_live_descendant` `ensures`
would build on is no longer vacuous. It remains a separate follow-on — there is still no
explicit queue-reaching `ensures`, lemma, or test driving real `revoke` through a queued
descendant — and was intentionally kept out of this change's scope (which targets D-A1 only).

### F-70-5 — Witness correspondence is now exact

The two host tests now map one-to-one onto the conditional `ensures`' two branches:
`check_revoke_root_survives` (test_store.rs:2815, un-homed root) witnesses the
`!is_homed ==> survives` branch; `revoke_can_empty_its_own_root_zombie` (test_store.rs:2764,
homed root) witnesses the silent branch. The negative witness no longer tests behaviour
*outside any contract* (as it did when the blanket `!is_homed` precondition rejected its input
outright); it now tests the homed branch the contract is explicitly silent about. Both still
pass unchanged — runtime behaviour is identical; only the contracts moved.

---

## Verification evidence

| Gate | Command | Result |
|---|---|---|
| Proof (primary) | `cargo verus verify -p kcore` | 316 verified, **0 errors** |
| Plain build | `cargo build -p kcore` | clean (verus erases to plain Rust) |
| Witnesses | `cargo test -p kcore` | 85 passed, 0 failed (incl. both revoke tests) |

## Disposition feed-forward

- **D-A1:** closed (this change). The doc-69 stopgap note ("this contract is unreachable from
  CapRevoke … witnessed only by `revoke_can_empty_its_own_root_zombie`") is **superseded** and
  was removed from `revoke`'s doc comment.
- **D-A2:** the faithfulness gap is closed at the contract level; the resident-with-external-
  reference residue stays follow-on (doc/results/55, refs-monotone frame).
- **D-A3:** still follow-on — add a `subtree_empty` `ensures`/lemma and a queued-descendant
  test on real `revoke`; the now-reachable descendant-deletion makes this strictly easier.
- The §2.2/§2.5 spec-prose notes (doc 69 "Inputs to 9e", item 4) can drop their D-A1 sub-clause
  about unreachability — the descendant-deletion guarantee is now reachable from the real call
  path. The D-A2 (un-homed-only survival, zombie admissible) and D-A3 (queue-reaching inferred,
  not yet a named obligation) sub-notes remain valid 9e inputs.
