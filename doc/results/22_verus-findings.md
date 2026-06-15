# Verus findings 2 — phase 2c: cdt_wf strengthening, acyclicity composition, the looping-op contracts

Plan: `doc/plans/3_verus-rewrite.md` (phase 2, §4.1). This is the increment the
phase-2/2b findings (`doc/results/21_verus-findings.md`) deferred — its §6 ("the
hard tail") and §9 ("Key design discovery"). It does the **`cdt_wf`
strengthening** §9 named as the blocker, uses it to close the documented
**"acyclicity doesn't compose"** gap for both the parent *and* the sibling
relation, ports the two remaining looping ops into the verified module under
assumed contracts (the `delete` precedent), and lands the §9 **recommended
host-test** — now covering all three trusted ops.

Toolchain: Verus `0.2026.06.07.cd03505`, `vstd =0.0.0-2026-05-31-0205`.
`cargo verus verify -p kcore`: **19 verified, 0 errors**. `cargo test -p kcore`:
**11 passed** (the new `test_store` suite). aarch64 kernel cross-build: clean.

---

## 1. The blocker, restated (why §9 stopped where it did)

Phase 2b proved acyclicity *usable* (`revoke`/`descend_to_leaf` choose a rank
witness and `decreases` on it) but not *constructible* — no verified op could
re-exhibit a witness after a mutation, so `cdt_insert_child`/`derive` ensured
only `cdt_wf`, never `acyclic`. The root cause §9 identified: the **structural**
`cdt_wf` does not pin parent↔child-list reachability. A node may name a parent
while being absent from that parent's child list, so "`first_child == None`" did
**not** imply "no node has this as parent" — and the natural insert witness (give
the fresh leaf the lowest rank) is then unsound, because a phantom child would
need a still-lower rank.

The fix is two first-order reachability anchors.

---

## 2. Stage 1 — the `cdt_wf` strengthening

Two clauses added to the `cdt_wf` conjunction (`kcore/src/cspace.rs`), keeping it
the *structural* predicate (acyclicity stays separate):

- `siblings_share_parent`: `m[a].next_sib == Some(b) ⟹ m[a].parent == m[b].parent`.
- `parent_has_first_child`: `m[k].parent == Some(p) ⟹ m[p].first_child is Some` —
  the **"no phantom child"** anchor (contrapositive: a childless node has no
  resident naming it parent).

The three existing non-recursive ops (`obj_ref`, `cdt_insert_child`, `derive`)
re-verify against the strengthened predicate with **no added proof hints** — the
SMT prover discharges the new clauses from the strengthened precondition and the
link edits directly. (`obj_ref` is untouched: it frames `slot_view`, so its
`cdt_wf` is unconditioned.)

---

## 3. Stage 2 — parent-acyclicity composition

`cdt_insert_child` and `derive` now have **pre- and postcondition `cspace_wf`**
(= `cdt_wf && acyclic && sib_acyclic`), up from `cdt_wf`. The new proof obligation
— `acyclic(final)` — is discharged by `lemma_reparent_preserves_acyclic`:

> Re-parenting one **detached, childless** `child` under `parent` in an acyclic
> store stays acyclic. Witness: shift every old rank up by one (`r[k]+1`) and seat
> `child` at the bottom (`0`). The `+1` shift makes room below `parent` even when
> its old rank was `0`; bottom-seating `child` is sound because **no slot names
> `child` as parent** — discharged from Stage 1's `parent_has_first_child` plus
> `child`'s `first_child == None`. So nothing needs a rank below `0`.

`derive` inherits `cspace_wf(final)` via its `cdt_insert_child` call (`dst` is a
fresh detached leaf); the intermediate state after `set_slot(dst)` is shown
acyclic by re-using the old witness (`dst` joins with no parent edge).

**Net:** a verified op now hands `revoke` a provably-`acyclic` store — the
documented "acyclicity doesn't compose" gap is closed on the parent side.

---

## 4. Stage 3a — sibling-acyclicity (the looping ops' termination measure)

The children-walk loops (`slot_move`/`cdt_unlink`) follow `first_child → next_sib`,
so they need a **sibling** rank to terminate. The structural `cdt_wf` does **not**
exclude a *floating* sibling cycle — a ring of `next_sib`/`prev_sib`-consistent
nodes, none a `first_child` head, all sharing a parent whose `first_child` points
elsewhere satisfies every structural clause. So sibling termination is its own
existential rank, mirroring the parent one:

- `valid_srank(m, s)`: `m[a].next_sib == Some(n) ⟹ s[n] < s[a]`;
  `sib_acyclic(m) = exists s. valid_srank(m, s)`; folded into `cspace_wf`.
- `lemma_insert_preserves_sib_acyclic`: the `cdt_insert_child` shape makes `child`
  a new list head whose only `next_sib` edge points at `old_first`, and **no slot
  points `next_sib` at `child`** (it had `prev_sib == None`); the witness seats
  `child` one above its successor and leaves every other rank untouched.

`cdt_insert_child`/`derive` now preserve all three components of `cspace_wf`. The
measure the future body proofs need is therefore in place.

---

## 5. Stage 3b — the looping ops, ported under assumed contracts

`slot_move` and `cdt_unlink` move from plain Rust into the `verus!{}` block as
**`#[verifier::external_body]`** with full contracts — the established `delete`
precedent (phase 2b §9). Honest status: these are **trusted, not proven**.

| op | assumed contract |
|---|---|
| `cdt_unlink` | `cspace_wf` preserved; `dom`/finiteness preserved; `slot` ends fully detached with its **cap intact**; `refs_view` unchanged; `count_nonempty` unchanged. |
| `slot_move` | `cspace_wf` preserved; `dom`/finiteness preserved; `dst` inherits `src`'s cap; `src` emptied; `refs_view` and `count_nonempty` unchanged (one owner relocating). |

Both contracts were **strengthened** to the new `cspace_wf` (so they now assert
preservation of `siblings_share_parent`/`parent_has_first_child`/`sib_acyclic`).
The same strengthening flows to the existing `delete` contract.

**Why the bodies aren't proven (the honest residue).** Each body is an in-place
`first_child → next_sib` walk that splices children into a sibling list and
re-points neighbours. The body proof needs a **partial-progress invariant
relative to the entry map** (which children have been re-parented; the chain
ahead still matches the start) — the classic linked-list-mutation proof. Two
specific obstacles:

1. *Mid-walk inconsistency.* `slot_move` momentarily leaves `src` **and** `dst`
   both claiming the same children/neighbours (until the final `set_slot(src,
   empty)`), so `cspace_wf` is genuinely false during the loop; it can only be
   re-established at the end, from a characterization of the final map.
2. *The fixup stage shifts `next_sib` edges*, so even the loop's `decreases`
   needs an `srank` witness re-exhibited for the post-fixup state.

These are tractable with the rank machinery now in `cspace_wf`, but are a
multi-hundred-line proof each; scoped to a follow-on increment. The trusted
surface this adds (two `external_body` ops) is mitigated by Stage 4.

---

## 6. Stage 4 — the host-test (the §9 recommended follow-up, generalized)

`kcore/src/test_store.rs` (`#[cfg(test)]`) is the **executable counterpart** of
the deferred body proofs — the §9 review's recommendation, now implemented and
extended from `delete` to **all three** `external_body` ops.

- **`ArrayStore`** — a concrete `Store` over a `Vec<CapSlot>` arena + refcount /
  cspace-resident maps. All **61** trait methods are implemented; the 8 the
  CDT/teardown path touches (`slot`/`set_slot`/`obj_refs`/`set_obj_refs`/
  `cspace_*`/`aspace_*`) are real, the channel/notification/thread/timer seam is
  `unimplemented!()` (these tests build only Frame/Untyped/CSpace/Aspace caps, so
  teardown never reaches it — a stray call panics rather than silently models
  nothing).
- **Executable mirrors** (`cdt_wf_exec`/`no_cycle`/`cspace_wf_exec`/
  `count_nonempty_exec`) re-express the ghost `spec fn`s (erased, uncallable from
  exec code). A `checker_has_teeth` test feeds four deliberately-malformed CDTs
  (parent cycle, half-linked siblings, **phantom child**, **sibling self-loop**)
  and asserts each is rejected — so the green contract checks are not vacuous.
- **Shapes** are grown with the *verified* `derive`, so the generator cannot
  start from a non-`cspace_wf` state. Hand-built cases cover the §9 list:
  non-leaf re-parent, middle-sibling unlink, subtree-root move, **cspace-in-
  cspace cross-object teardown**, and **refcount > 1** (delete decrements without
  destroying; the last ref reclaims residents). A **randomized sweep** (>500
  trials) deletes/unlinks/moves a random eligible slot and asserts the full
  `ensures` each time.

**Result of value beyond evidence:** the sweep *confirms the real bodies preserve
the strengthened `cspace_wf`* — i.e. strengthening `cdt_wf`/adding `sib_acyclic`
did not make the (assumed) `delete`/`slot_move`/`cdt_unlink` contracts dishonest.
Had a body violated a new clause, an assertion would have fired.

---

## 7. What is proven vs. trusted (the boundary, precisely)

**Proven, unbounded, no new assumptions:**
- the strengthened structural `cdt_wf` (Stage 1);
- `cdt_insert_child`/`derive` preserve full `cspace_wf` — parent- *and*
  sibling-acyclicity construction (Stages 2, 3a);
- (carried from 2b) `descend_to_leaf`/`revoke` termination, now over the stronger
  `cspace_wf`.

**Trusted (assumed `external_body` contracts), host-test-checked:**
- `delete` (unchanged from 2b, contract strengthened to the new `cspace_wf`);
- `slot_move`, `cdt_unlink` (new this increment).

**Residue (next increments):**
- the body proofs of the three looping/teardown ops (the linked-list-splice
  partial-progress invariant; for `delete`, additionally the cross-object
  destructors, plan phases 3–5);
- the full `refs == census` soundness predicate (still the slot-only delta).

---

## 8. Adversarial self-review (and the checks it forced)

Three lenses, in the phase-2/2b discipline:

- **Vacuity.** Could the new `cdt_wf` clauses or `cspace_wf` be unsatisfiable
  (making everything vacuously green)? No — `checker_has_teeth` exhibits both
  accepted (a valid two-node CDT) and rejected shapes for every clause, and the
  randomized sweep runs the ops over hundreds of genuinely-`cspace_wf` forests.
- **Honest strengthening.** Folding `sib_acyclic` into `cspace_wf` and
  strengthening `cdt_wf` makes the *existing* `delete` contract stronger (it must
  now preserve the additions). Risk: the real `delete` body might not. The host-
  test's cross-object and sweep cases drive the real `delete` and assert the
  strengthened `cspace_wf` — it holds, so the contract is not silently broken.
- **Overclaim.** `slot_move`/`cdt_unlink` are **not proven** — §5 and the doc
  comments label them assumed contracts (the `delete` precedent), not theorems.
  The headline of this increment is the *acyclicity composition* (Stages 1–3a,
  fully proven), plus the executable contract evidence (Stage 4) — not a body
  proof of the looping ops.
- **Erasure.** `verus!{}` erases ghost code; the looping-op bodies are byte-for-
  byte the originals (moved verbatim under `external_body`). Confirmed by
  `cargo build/test -p kcore` and the aarch64 cross-build; the kernel's
  `KernelStore` callers (channel send/recv, thread, syscall) are unchanged.

---

## 9. CI / docs

- The `verus` job (`cargo verus verify -p kcore`, no per-proof filter) gates the
  new lemmas and the strengthened contracts.
- The new `test_store` suite runs in the existing `host-tests` job
  (`cargo test --workspace --exclude kernel`) — no CI wiring, auto-gated.
- `CLAUDE.md` (Verus section + tiers table) updated: the non-recursive cspace/CDT
  ops now carry full `cspace_wf` (acyclicity composition); `slot_move`/`cdt_unlink`
  join `delete` as assumed-contract ops with host-test validation.
