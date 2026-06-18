# Verus findings 1 — phase 2: cspace / CDT

Plan: `doc/plans/3_verus-rewrite.md` (phase 2, §4.1). This is the first
substantive Verus implementation on `kcore` after the phase-0 pilot
(`untyped::carve`) and the phase-1 arena/handle/`Store` rewrite. It records the
deletion sweep, the proof architecture, what is proven, the load-bearing Verus
idioms for the pinned toolchain, the trusted boundaries, and what remains.

Toolchain: Verus `0.2026.06.07.cd03505`, `vstd =0.0.0-2026-05-31-0205` (the
`CLAUDE.md` pin). `cargo verus verify -p kcore`: **13 verified, 0 errors**.

---

## 1. The deletion sweep (whole tier)

The phase-1 work had already gated the `kcore` Kani harnesses off
(`legacy_ptr_harness`, never enabled in CI) and frozen them against the old
raw-pointer model — they no longer compile against the handle/`Store` core, and
`cargo kani -p kcore` was already dropped from the `kani` CI job. Per plan §5
("merciless"), the whole subsumed tier was deleted rather than carried as dead
code a documented feature flag could not even build:

- `kcore/src/proofs/` (every harness + the `wf`/`world`/`ghost`/`bounds`/`stubs`/
  `contracts` scaffolding);
- `kcore/src/env.rs` (the old `*mut Env` seam; folded into `Store` in phase 1,
  referenced only by the deleted proofs);
- `scripts/deep-verify.sh` + `.github/workflows/kani-deep.yml` (the off-CI
  deep-Kani machinery those harnesses fed);
- the `legacy_ptr_harness` / `kani_deep` / `kani_contracts` cargo features;
- the now-vacuous `ci.yml` exhaustive-replay step (it matched zero tests once the
  proofs module was gated off).

The `wf` predicates are **not lost**: they are re-expressed as Verus `spec fn`s
(`cdt_wf`, `slot_refs`; see §3). The Kani **host** chokepoints (`urt`, `ipc`,
`cas`, `dma-pool`) keep their harnesses until their own Verus ports (plan
phase 6). Net: 24 files, ~4040 lines deleted.

---

## 2. The proof architecture: an abstract `Store` view

Phase 1 made the cspace/CDT operations generic over a `Store` **trait** (the
object store seam), not the concrete arena *arrays* the plan §3.2 sketched. The
phase-2 decision (chosen by the maintainer) was to **spec the trait** rather than
verify a parallel concrete model — so the proofs cover the *real* generic
`fn op<S: Store>` code the kernel runs, not a transcription.

The mechanism (validated by a feasibility spike before any real edit):

- The plain-Rust `Store` trait and the handle/value types (`SlotId`, `ObjId`,
  `Cap`, `CapKind`, `CapSlot`, `Rights`, `ChanEnd`) stay **outside** `verus!{}`
  — the kernel's `KernelStore` impl (kernel crate) is untouched and carries no
  ghost members.
- A `verus!{}` block in `cspace.rs` attaches the spec:
  - `#[verifier::external_type_specification]` (+ `ext_equal`) for each value
    type, so they appear in spec expressions (mixed-variant enums like `CapKind`
    work — field access and `matches` in both spec and exec);
  - `#[verifier::external_trait_specification]` +
    `#[verifier::external_trait_extension(StoreSpec via StoreSpecImpl)]` for the
    trait, which **adds ghost spec methods the trait never declared**: the
    abstract views `slot_view() -> Map<SlotId,CapSlot>` and
    `refs_view() -> Map<ObjId,nat>`, plus view-relational `requires`/`ensures` on
    the handful of methods cspace touches (`slot`/`set_slot`/`obj_refs`/
    `set_obj_refs`). Only that subset is contracted; the rest of the ~70-method
    seam is left unconstrained (later phases).
- The generic operations are verified once against these contracts — for **all**
  stores. `KernelStore` is **trusted** to satisfy the contract: this is the TCB
  seam (plan §2/§3.2), the deliberate boundary between the verified core and the
  unverifiable shell.

`verus!{}` erases to plain Rust in an ordinary build, so the moved operations
compile and run exactly as before — confirmed by `cargo build/test -p kcore` and
the aarch64 kernel cross-build.

---

## 3. What is proven (unbounded, on the real code)

`spec fn`s (the migrated `wf`):

- `cdt_wf(m)` — the **structural** CDT invariant (the structural half of the TLA
  `TypeOK`, total and ∀; acyclicity is layered on as `cspace_wf` in §9):
  `links_in_domain`, `siblings_doubly_consistent` (both directions),
  `first_child_parent_agree` (+ its converse `head_is_first_child`),
  `empty_slots_detached`.
- `slot_refs(m, obj)` — the refcount **census**: the count of slots designating
  `obj` (`m.dom().filter(|k| cap_obj(m[k].cap) == Some(obj)).len()`).

Verified operations:

- `cdt_wf(m)` is the **structural** CDT invariant; it does **not** include
  acyclicity (see §6 — acyclicity lands with the termination/ghost-rank
  increment). It carries `links_in_domain`, `siblings_doubly_consistent`,
  `first_child_parent_agree` **and its converse `head_is_first_child`** (a
  list-head node is its parent's first child — added after the review, §8), and
  `empty_slots_detached`.

| op | contract proven |
|---|---|
| `obj_ref` | refcount bump, the slot arena untouched. Overflow-free given a `< u32::MAX` precondition (which `derive` discharges — see below). |
| `cdt_insert_child` | **`cdt_wf` preserved** by the doubly-linked insertion, ∀ shapes; child becomes parent's first child and the **prior children follow in order** (the sibling list is spliced in unchanged); caps and refcounts framed unchanged. |
| `derive` | the **monotone-derivation security theorem** (§2.3): on `Ok`, `dst.rights == src.rights & mask`, hence a **subset for every mask** (proven ∀, not sampled); a **faithful copy** — `dst`'s kind/object equals `src`'s, a Frame copy unmapped (§2.5); refuses empty/Untyped src, occupied dst, **or a refcount already at `u32::MAX`**, store unchanged; **overflow-free for all inputs** (no unchecked `+ 1` wrap — a theorem, not an assumption); `cdt_wf` preserved; the stored refcount **and** the slot census both rise by exactly one (the refcount-soundness *delta*; the full `refs == census`, incl. non-slot refs, is deferred). |

This is the security pivot (monotone derivation), the structural invariant
(`cdt_wf`), and the refcount discipline (delta) — the heart of the CapRevocation
invariant set — proven **unbounded** where Kani checked at TLC-scale bounds, for
the three non-recursive cspace/CDT operations. The looping/teardown ops remain
(§6).

---

## 4. Load-bearing Verus idioms (pinned toolchain)

Banked from the spikes and the real proofs; future phases inherit them.

- **Abstract trait view**: `external_trait_specification` +
  `external_trait_extension(Spec via SpecImpl)` adds ghost `spec fn` views/preds
  to an external trait; generic `fn op<S: Trait>` verifies against them with no
  concrete impl in scope. A subset of methods may be contracted; the rest are
  unconstrained.
- **`&mut` postconditions** use `final(self)` / `old(self)` — bare `self` is a
  hard error in this version (the single most version-specific gotcha; old web
  examples use bare `self` and will not compile).
- **External enums**: `external_type_specification` on a mixed unit/struct/tuple
  enum supports `matches`/field access in spec and exec; or-patterns binding the
  same variable across variants (`A(o) | Channel(o,_) | ...`) connect to a
  `cap_obj`-style spec fn automatically.
- **Census over a `Map`**: `m.dom().filter(pred).len()`. Deltas need
  `broadcast use {vstd::map::group_map_axioms, vstd::set::group_set_axioms};`
  (one combined module-level `broadcast use`). Set-extensionality proofs need an
  explicit per-key `assert forall k: s1.contains(k) <==> s2.contains(k) by {…}`
  bridge; `axiom_set_insert_len` (the `+1`) needs the set **finite** — so census
  deltas require the slot arena finite (a genuine system invariant: a cspace has
  finitely many slots).
- **Bit reasoning**: the monotone-subset corollary `(r & mask) & r == r & mask`
  is discharged `by (bit_vector)`.
- **Frame conditions**: give a helper a *specific* ensures (`final[child].cap ==
  old[child].cap`) in addition to the `forall` — directly usable by callers
  without trigger gymnastics.

---

## 5. Trusted boundaries (the TCB of these proofs)

- **The `Store` contract** — `KernelStore` is assumed to satisfy the abstract
  view/method specs (slots and refcounts are separate storage; `set_slot` leaves
  refcounts untouched and vice-versa; accessors are total over live handles).
  This is the seam, the intended TCB boundary.
- **`assume_specification [Rights::masked]`** = `out.0 == r.0 & mask` — states
  what the plain-Rust method computes (it is `Rights(self.0 & mask)`).
- **Cross-module destructors** (`channel::destroy_channel`,
  `notification::destroy_notif`, …, `endpoint_cap_dropped`, `aspace_unmap`) are
  not yet in `verus!{}`; the teardown ops that call them stay plain Rust for now
  (see §6).
- **`untyped::retype`** (plain Rust) calls the now-verified `cdt_insert_child`;
  its calls are trusted to meet the precondition until `retype` is itself
  verified (it creates fresh, detached, non-empty children, so it does).

No `assume(...)` is used inside the verified bodies; no operation is
`external_body` (the bodies are genuinely checked).

---

## 6. What remains (the hard tail)

The looping and recursive operations are deferred to a follow-on increment:

- `slot_move`, `cdt_unlink` — single-level sibling-list surgery with a
  children-walk loop;
- `delete`, `revoke`, `destroy_cspace`, `obj_unref` — the teardown recursion.

Two obstacles, both anticipated by the plan (§4.1, "the chief risk"):

1. **Loop / recursion termination.** Verus requires `decreases` on every exec
   loop. The sibling-walk and the revoke descent need a well-founded measure;
   the natural one is **acyclicity**, which the *structural* `cdt_wf` deliberately
   omits. The spike validated the encoding — a ghost `rank: Map<handle,nat>` with
   a wf invariant `rank[child] < rank[parent]` (and an `srank` for `next_sib`),
   `decreases rank[cur]` for the descent and `decreases live().len()` for the
   delete-a-leaf loop. Adding it means threading a rank view through the `Store`
   model and maintaining it across ops — the next phase-2 increment.
2. **Cross-object teardown.** `revoke`→`delete`→`obj_unref`→`destroy_cspace`→
   `delete` recurses across *objects* (a cspace cap inside a cspace) — the
   seL4-zombie measure. This needs the cross-module destructors ported first
   (plan phases 3–5) so the recursion is closed under contracts.

The honest scope of this increment: the **non-recursive** core of §4.1's `derive`
/ `cdt_insert_child` rows (the security + structural + refcount properties),
proven unbounded. The termination headline lands with the rank machinery and the
teardown port.

---

## 7. CI / docs

- The `verus` CI job (`cargo verus verify -p kcore`, no per-proof filter) gates
  all of the above; a new `verus!{}` obligation auto-gates.
- The `kani` job lost its `kcore` leg (host chokepoints only).
- `CLAUDE.md` (the Kani + Verus sections, the tiers table, the CI section, the
  workspace-layout note) and `0_kani-rewrite.md`'s deviation paragraph track the
  new division: Verus is the mechanized kernel-core tier (the spec's original §6
  assignment); Kani is retained only for the host chokepoints.

---

## 8. Adversarial review (and the fixes it forced)

Before finalizing, the proofs were put through a three-lens adversarial review
(vacuity & trusted-boundary soundness; fidelity to the CapRevocation invariants /
the deleted predicates; Verus-soundness & erasure). The reviewers independently
converged — and confirmed in Verus, against the real impl, that the proofs are
**sound** (the `Rights::masked` assumed spec matches the impl; the abstract Store
contract is satisfiable and faithful to `KernelStore`; the body rewrites are
behavior-preserving; no `assume`/`external_body` weakens the bodies). They also
found that several *green* proofs proved **less** than the comments claimed. Each
was fixed before landing:

- **Refcount overflow was a real, reachable UAF, not just a documented one.** The
  `< u32::MAX` precondition was never discharged by the production `CapCopy →
  derive` path, so the kernel kept an unchecked `r + 1` that wraps to zero and
  triggers premature last-ref teardown. **Fixed**: `derive` now refuses at the
  ceiling (Err) before any mutation; the precondition is dropped; the bump is
  proven overflow-free for all inputs (the way `carve` was handled). The
  production path inherits the guarantee.
- **`cdt_wf` was weaker than the predicate it replaced** — it admitted cyclic
  CDTs (acyclicity dropped) and orphan-head nodes (the reverse first-child check
  missing). **Fixed** the reverse check (`head_is_first_child`); acyclicity is
  honestly scoped as deferred to the termination increment, and `cdt_wf` is
  labeled the *structural* fragment, not the full TLA TypeOK.
- **`derive` pinned only rights, not the "copy".** A derive that changed the cap
  kind / channel end would have satisfied the contract. **Fixed**: `derived_kind`
  pins dst's kind/object = src's (Frame unmapped, §2.5); + the bare-cap
  refcount-unchanged clause.
- **`cdt_insert_child` didn't prove the prior children survive.** **Fixed**: the
  sibling-list splice (child heads, prior children follow in order) is now an
  ensures.
- **"Refcount soundness preserved" overstated two co-increments.** Reworded
  everywhere to the **delta** it is (stored refcount and slot census both +1);
  the full `refs == census` invariant (with non-slot refs) is deferred.

Acknowledged-and-deferred (documented, not fixed this increment): full acyclic
`cdt_wf` + the six looping/recursive ops + their termination (§6); the full
`refs == census` soundness predicate; the finiteness invariant living on the
`Store` contract rather than as a `derive` precondition; and a host-test guard
that `KernelStore`'s slot/refcount storage is disjoint (the trusted-layout
assumption the abstract contract rests on, §5).

---

## 9. Phase 2b — acyclicity + the revoke-walk termination

A second increment (PR #2, stacked on the phase-2 PR) lands the **termination**
headline the plan promised over Kani's `debug_assert`.

**Acyclicity.** `acyclic(m) = exists r. valid_prank(m, r)` — an existential
parent-rank witness, where a strict decrease parent→child makes the parent
relation well-founded (no cycle; the adversarial review confirmed in Verus that
2-cycles, 3-cycles, and self-cycles are all provably non-acyclic). The rank is
**ghost-only**, so it needs no home in the abstract `Store`; a proof that needs
termination *chooses* a witness. `cspace_wf = cdt_wf && acyclic` is the invariant
the recursive ops require. `count_nonempty` is the live-slot measure.

**Proven (no assumptions):** `descend_to_leaf` — the inner descent of `revoke`
terminates `decreases prank[leaf]`: each `first_child` step lands on a node whose
parent-rank is strictly lower (acyclicity), and returns a true non-empty leaf.
The acyclicity witness is *used*, proven unbounded for all tree shapes.

**Proven against an assumed contract:** `revoke` terminates
`decreases count_nonempty` and preserves `cspace_wf` — each iteration descends to
a live leaf and `delete`s it, strictly lowering the live-slot count.

**The trusted boundary, made explicit:** `delete` is `#[verifier::external_body]`
with a contract (`cspace_wf` preserved, domain preserved, the slot emptied, live
count strictly drops). Its body is the real teardown whose last-ref path recurses
across objects (`destroy_cspace` → `delete`) — the seL4-zombie measure that needs
the channel/notification/thread destructors ported (plan phases 3–5) before its
body can be verified. The adversarial review **stress-tested this assumed
contract against the real body** (array-backed store; hand-built non-leaf,
middle-sibling, foreign-child, self-resident, cspace-in-cspace, refcount>1 shapes
+ a 400-trial randomized sweep, 96 of which drove cross-object teardown) — all
five ensures held every time, so the contract is honest, not a hollow assumption.

### Key design discovery (why the structural ops are still deferred)

Acyclicity is easy to **use** (choose a witness, `decreases`) but hard to
**construct** (re-exhibit a witness after a mutation) — and construction is what
`cdt_insert_child`/`derive`/`slot_move`/`cdt_unlink` would need to *preserve*
`acyclic`. The blocker: the structural `cdt_wf` does **not** pin parent↔child-list
reachability — a node can name a parent while being absent from that parent's
child list (a non-head sibling), so "`first_child` is `None`" does **not** imply
"no node has this as parent." Without that, the `insert_child` acyclicity witness
(give the fresh leaf the lowest rank) is unsound — a phantom child would need a
still-lower rank. So the next increment is a **`cdt_wf` strengthening** (siblings
share a parent; a node's children are exactly its first_child→next_sib chain),
after which construction-side acyclicity preservation — and with it
`slot_move`/`cdt_unlink` full `cspace_wf` preservation and `delete`'s body —
become provable.

### Adversarial review of this increment (findings + dispositions)

Two lenses confirmed the increment **sound** (the assumed `delete` contract is
honest per the sweep above; the acyclicity encoding genuinely excludes cycles;
`descend_to_leaf`/`revoke` decreases are real and non-vacuous; erasure is clean —
`delete`'s erased body is byte-identical to the original, `revoke`'s restructuring
behaviorally equivalent, the kernel builds and calls through unchanged). The
findings were omissions/overclaims, dispositioned:

- **Fixed — revoke "the cap itself survives" (§2.2) overclaim.** The coded
  postcondition omits `slot` non-emptiness, and the review *demonstrated* the real
  `revoke` emptying its own root via cross-object teardown (the root is a resident
  of a cspace whose last cap is in the root's CDT subtree) while the green
  postcondition passed vacuously. The doc comment no longer claims cap-survival;
  it documents the gap, which closes only with `delete`'s frame (the reachability
  strengthening above).
- **Documented — acyclicity doesn't compose.** `derive`/`cdt_insert_child` ensure
  only `cdt_wf` (not `acyclic`), so no verified op hands `revoke` a provably
  acyclic store; the precondition is discharged only at the trusted kernel
  boundary. Closing it is the same construction-side work (blocked on the `cdt_wf`
  strengthening).
- **Documented — `delete`'s contract is silent on `refs_view`** (refcount effects
  of teardown) and its `dom`/finiteness clauses rest on the production invariant
  that destroyed-object slot memory stays addressable until untyped reset (the
  trusted Store layout, §5). Both land with the body proof.
- **Recommended follow-up — a CI host-test** (array-backed `Store`) that runs the
  real `delete`/teardown and asserts the five `delete` ensures, making the
  external_body contract continuously checked against the body (the executable
  counterpart of the deferred proof). The review's 400-trial sweep is the interim
  evidence; kcore still lacks a concrete host `Store`, so this is its own task.
