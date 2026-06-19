# 70 — Follow-on-fix ledger: D-A1 (revoke split) · D-B1 (priority ceiling)

> A *change* ledger accreting the post-doc-69 follow-on fixes, one top-level entry per
> finding (newest last). **D-A1** — `revoke`'s contract split — is first; **D-B1** — the
> §5.4 priority ceiling — is appended at the end of the file.

## Provenance

This document records the resolution of **D-A1** — the single highest-severity finding of
the doc-69 drift audit — together with the genuinely new observations the fix surfaced. It
is a *change* ledger (one finding fixed, its consequences for the neighbouring D-A2/D-A3
findings, and one new structural observation), not a fresh re-derivation. Authority is the
spec (`doc/spec/2_spec_rev2.md` §2.2) and the verified source; the disposition followed is
the one doc 69 recorded for D-A1 ("split the contract so `first_child is None` + `cspace_wf`
are proven **without** `!is_homed`, keeping the extra `!is_empty_cap` root-survival under
`!is_homed`").

*(The **D-B1** entry — the §5.4 maximum-controlled-priority ceiling, now a cap-carried `u8`
with Verus-verified monotone attenuation — is the second major section at the bottom of this
file.)*

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
  reference residue stays follow-on (doc/results/55, refs-monotone frame). **— now closed in
  doc/results/72:** the `emptied_via_dead_home` frame + the refs-monotone *dead-stays-dead*
  argument landed and `revoke` exports the faithful theorem, plus the §2.2/§2.5 spec note.
- **D-A3:** still follow-on — add a `subtree_empty` `ensures`/lemma and a queued-descendant
  test on real `revoke`; the now-reachable descendant-deletion makes this strictly easier.
- The §2.2/§2.5 spec-prose notes (doc 69 "Inputs to 9e", item 4) can drop their D-A1 sub-clause
  about unreachability — the descendant-deletion guarantee is now reachable from the real call
  path. The D-A2 (un-homed-only survival, zombie admissible) and D-A3 (queue-reaching inferred,
  not yet a named obligation) sub-notes remain valid 9e inputs.

---

# D-B1 closed — the thread cap gains a verified, monotone priority ceiling

## Provenance

This second entry records the resolution of **D-B1** (doc 69, Class D1, severity high): spec
§2.3 (line 71) says "the §5.4 maximum-controlled-priority ceiling is a **value on the cap** …
attenuates the same monotone way," and §5.4 (line 360) says spawn bounds a thread's priority
by "a maximum carried in the spawner's own thread cap … monotone like every other derivation."
The verified core proved **nothing** about priority: `CapKind::Thread(ObjId)` carried no
ceiling, `derive` attenuated `rights & mask` only, and the only enforcement was an *unverified*
shell gate on the caller's live run-priority.

**Disposition followed (chosen scope):** the doc-69 disposition's first clause — add a
`max_prio` field, prove `derive` attenuates it monotonically, gate spawn on the cap's ceiling.
The fuller end-to-end-verified variant ("Option 2") is recorded as a **recommended follow-on**
at the foot of this entry rather than built now.

**Headline:** the §5.4 ceiling is now a `u8` on `CapKind::Thread`; `kcore::cspace::derive`
proves `child.max_prio ≤ parent.max_prio` for **all** derivations (∀, not sampled), exactly
like the rights mask; and both spawn syscalls gate `prio ≤ cap.max_prio`. Verified: **316
verified, 0 errors** (unchanged count — **no new lemma** was needed), **86** `kcore` host tests
(+1 ceiling witness), the AArch64 shell builds, and the kernel boots in QEMU with init's two
children (storaged @5, shell @4) both spawning under the stamped ceiling (16). **Runtime
behaviour is preserved** on the real boot path — this is a verification-faithfulness fix that
also tightens enforcement to the spec's cap-carried model.

## The change

| Layer | Edit |
|---|---|
| Cap model (`kcore/src/cspace.rs`) | `CapKind::Thread(ObjId)` → `CapKind::Thread(ObjId, u8)`; new `spec fn`s `cap_max_prio` and `is_thread_cap_for`. |
| `derive` (cspace.rs) | explicit monotone-ceiling `ensures` (`cap_max_prio(src) = Some(p) ⟹ cap_max_prio(dst) = Some(c) ∧ c ≤ p`), discharged from the pre-existing `derived_kind` equality. |
| Dead-object lemmas | the `!= CapKind::Thread(t)` equality sites re-expressed via `is_thread_cap_for` (field-shape-stable). |
| Shell (`kernel/`) | retype stamps `max_prio = (*current()).priority` (untyped.rs); both spawn gates become `prio > max_prio` (syscall.rs); init's boot cap ceiling = init's priority (main.rs). |
| Tests (`test_store.rs`) | cap-shape fixes; structural `cap_kind_eq` now compares the ceiling; new `derive_preserves_thread_priority_ceiling` witness. |

## Findings

### F-70-6 — The cap↔TCB seam is the exact residual boundary (novel)

Priority lives in the **verified** `kcore` `Tcb` struct (`thread.rs:75`) yet **outside** the
verified Store view: `tcb_view()` / the host `TcbState` carry no `priority` field, and the
shell writes `(*tp).priority = prio` through a raw pointer. So the model→TCB write is the
*single* unverified hop. The cap-carried ceiling and its monotone derivation are now verified;
what remains a trusted seam is precisely this one write — which is why Option 2's first move is
to thread priority into the Store seam. This is a *satisfiable*, normal trusted-base seam (like
D-C1/C3/F1 — the shell carries it correctly), not a vacuous one (contrast D-A1's `!is_homed`).

### F-70-7 — Behaviour preservation is exact on the real boot path, by construction

Stamping the fresh thread cap's ceiling = the **retyper's live priority** reproduces the old
`prio > current().priority` gate for the common carve-then-start path: the retyper and the
starter are the same thread with stable priority in every init/loader spawn, so the new gate
`prio > cap.max_prio` and the old gate coincide. The QEMU boot witnesses this — storaged (prio
5) and shell (prio 4) both start under ceiling 16 with no spurious `ERR_PERM`. The two gates
**diverge** only when the cap is carved by A and started by B, or the retyper's priority
changes between carve and start — and there the cap-carried reading is *more* faithful to §5.4
("a maximum carried in the spawner's own thread cap"), authority travelling with the cap rather
than with whoever happens to call `thread_start`.

### F-70-8 — Monotonicity was a missing *statement*, not a missing *proof* (novel, mirrors D-A1)

`derived_kind`'s catch-all `_ => k` arm already preserves the **whole** kind, so the new
ceiling field rides along untouched and the monotone `ensures` (`==`, hence `≤`) discharges
directly from the pre-existing `final[dst].kind == derived_kind(old[src].kind)` clause — **316
verified, 0 errors, no new lemma**. Doc 69's "monotonicity entirely unverified" was therefore
accurate about the *contract* but understated the *code*: the lattice property was latent in
`derived_kind` and merely never surfaced as an `ensures`. This is the same shape as the D-A1
fix (surface a property that was already structurally present) — the recurring lesson of this
ledger: the gap is usually an unstated obligation, not an unsound body.

### F-70-9 — The axis is realized as ceiling-*preservation*, not strict reduction (honest scope)

`derive` carries no priority parameter (unlike the rights `mask`), so a derived thread cap
keeps its parent's ceiling exactly. That proves monotonicity (`≤`) and is sufficient for the
spawn-monotonicity the runtime needs, but it does **not** yet let a supervisor hand out a
*strictly lower* ceiling ("attenuated as desired", §2.3). Ceiling reduction currently happens
only at retype (a fresh child capped at its creator's priority). Strict per-copy attenuation is
the Option-2 `derive` parameter below — narrower today than the full §2.3 supervision-grant
story, and flagged so the gap is budgeted, not silent.

## Verification evidence

| Gate | Command | Result |
|---|---|---|
| Proof (primary) | `cargo verus verify -p kcore` | **316 verified, 0 errors** (no new lemma) |
| Witnesses | `cargo test -p kcore` | **86 passed, 0 failed** (incl. `derive_preserves_thread_priority_ceiling`) |
| Shell build | `cd kernel && cargo build` | clean (AArch64 bare-metal) |
| Boot/spawn smoke | QEMU (`virt`, gic v3) ~16 s | `[init] system up`; storaged + shell both spawn; `eunomia>` prompt; no panic, no spurious `ERR_PERM` |

## Recommended follow-on — Option 2 (full end-to-end verified enforcement)

> **Implemented — see doc/results/71.** All three items below landed: priority is in the
> verified Store view, `thread::set_priority` carries the write, and `derive` gained a reducing
> `prio_ceiling` (wired through `CapCopy` / `cap_copy_prio`). 318 verified, 0 errors.

Per the chosen scope, the fuller fix is recorded here for a subsequent task. It closes the
F-70-6 seam and the F-70-9 reduction gap:

1. **Priority into the Store seam.** Add a `priority` field to `tcb_view()` (and the host
   `TcbState`), with `tcb_priority` getter + `set_tcb_priority` setter on the `Store` trait —
   the analogue of `set_tcb_report` / `set_tcb_bind_bits`.
2. **A verified `thread::set_priority(store, t, prio, ceiling)`** with `requires prio ≤ ceiling`
   and `ensures tcb_view()[t].priority == prio` (hence `≤ ceiling`). Route the shell's
   `(*tp).priority = prio` through it; the spawn site discharges `requires` from the cap's
   `max_prio`. This delivers the disposition's `ensures child.priority ≤ ceiling`
   **end-to-end**, eliminating the one unverified hop (F-70-6).
3. **Optional — a reducing `prio_ceiling: u8` parameter on `derive`** (symmetric with `mask`),
   setting `child.max_prio = min(parent.max_prio, prio_ceiling)` with `ensures ≤`, so a
   supervisor can strictly attenuate a handed-out thread cap (closes F-70-9, the §2.3
   supervision-grant story). **ABI note:** the `Thread`-cap layout already changed in this fix;
   a `derive` ceiling parameter would additionally change the `CapCopy` syscall ABI.

Size/risk: medium-large (Store trait + `test_store` + new proofs). The spawn-gate semantics are
already in place from this fix, so Option 2 is purely about moving the priority *write* under a
verified contract — no further runtime-behaviour change.

## Disposition feed-forward

- **D-B1:** closed (this change) at the cap-model + monotone-derivation level; the
  model→TCB-write seam (F-70-6) and strict per-copy attenuation (F-70-9) are the Option-2
  follow-on above — **now implemented in doc/results/71** (verified `set_priority` + reducing
  `derive` ceiling).
- The doc-69 "Inputs to 9e" **item 3** is superseded: §2.3/§5.4 no longer carry an "entirely
  unverified" note — the cap-carried ceiling and its monotone `derive` attenuation are now
  Verus-verified. The spec text was updated in place with the honest residual-seam note.
