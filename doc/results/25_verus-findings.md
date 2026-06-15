# Verus findings 5 — `cdt_unlink`'s body proof closes (Phase 2 single-object done)

Plan: `doc/plans/3_verus-rewrite.md` (phase 2, §4.1). Prior increments:
`21_verus-findings.md` (phase 2 + 2b), `22_verus-findings.md` (phase 2c),
`23_verus-findings.md` (phase 2 closeout — banked structural core),
`24_verus-findings.md` (`slot_move`'s body proof). This increment **closes the
`cdt_unlink` body proof**: its `#[verifier::external_body]` is removed, so the
sibling-list *merge* is now a theorem rather than an assumed, host-test-checked
boundary. That closes **Phase 2's tractable single-object residue** — only the
cross-object `delete` remains assumed (rightly, plan phases 3–5).

**Outcome.** `cargo verus verify -p kcore`: **55 verified, 0 errors** (was 39).
`cargo test -p kcore`: **13 passed** (the `test_store` differential check still
runs the real `cdt_unlink` body — now also proven). The aarch64 `kernel`
cross-build is unchanged (ghost code erases). Trusted surface shrinks by one op:
`cdt_unlink` is off the `external_body` list; only `delete` remains there.

---

## 1. What closed, and how it differs from `slot_move`

`slot_move` (doc 24) was an **identity transposition** π=(src dst) — a renaming of
the whole map, an involution, so `relabeled` + `lemma_transpose_preserves_cspace_wf`
carried it. `cdt_unlink` is a **merge**, not a renaming: it re-parents `slot`'s
children to the grandparent and splices the child chain into `slot`'s former
sibling position (`prev → first → … → last → next`), then detaches `slot`. There
is no clean involution, so **both** halves had to be built fresh:

- **The target map `unlinked(m, slot, last)`** — the closed-form result, per slot:
  cap kept everywhere; `slot` fully detached; each child re-parented to the
  grandparent, with the chain head's `prev_sib` rewired to `slot`'s old `prev` and
  the chain tail's `next_sib` to `slot`'s old `next`; the neighbour fixups
  (`prev.next`/`parent.first_child` → head, `head.prev` → prev, `next.prev` → tail)
  applied. `last` (the chain tail) is a parameter — it appears only in `next`'s new
  `prev_sib`, so the `next_sib` structure (hence sibling-acyclicity) is independent
  of it.
- **`lemma_unlink_preserves_cspace_wf`** — proves `unlinked` keeps the full
  `cspace_wf`, factored per-clause (the transpose family's per-clause SMT
  discipline; the monolith blows the rlimit): `lemma_unlink_links`,
  `lemma_unlink_siblings` (double-consistency + share-parent across the merged
  chain), `lemma_unlink_children` (first-child/head/parent clauses),
  `lemma_unlink_empty`, `lemma_unlink_acyclic`, `lemma_unlink_sib`.

### The acyclicity asymmetry (the cleanest finding)

The two acyclicity witnesses behave oppositely under the merge:

- **Parent-acyclicity reuses the *same* witness `r0` unchanged.** Each child moves
  from `parent=slot` to `parent=grandparent`, and `r0[child] < r0[slot] <
  r0[grandparent]` already holds in `m0`, so the strict drop survives the re-parent;
  `slot` becomes a root (no constraint). No reseating — unlike
  `lemma_reparent_preserves_acyclic`, whose childless precondition the children
  (which may have their own children) cannot meet.

- **Sibling-acyclicity needs the witness *rescaled*** — the crux. The merged chain
  joins two independent sibling lists (slot's siblings at `slot.parent`; slot's
  children under `slot`) whose ranks are unrelated. A *constant additive shift* of
  the child band provably fails: the child chain's rank span can exceed the
  `prev..next` gap, so no single offset both seats `first` below `prev` and `last`
  above `next`. The witness `lemma_unlink_sib` builds instead opens a gap by
  **rescaling**:
  - `B` = a strict upper bound of the old sibling-rank `s0` over the finite domain
    (a fresh `lemma_rank_bounded`, finite-set induction);
  - non-children sit at `(s0[k]+1)·(B+1)` — multiples of the band width `B+1`;
  - re-parented children sit in the band just above `next`'s scaled rank,
    `(D+1)·(B+1) + s0[k] + 1` (D = `s0[next]` or 0).
  `B`'s bound keeps the whole child band strictly inside `((D+1)(B+1),
  (s0[prev]+1)(B+1))`, internally ordered by `s0`. The `+1` on children is needed so
  the `last → next` edge is strict even when `s0[last] == 0`.

### The body-match (the imperative body lands `unlinked`)

Mirroring `slot_move`: a `children walk` loop re-parents every child to the
grandparent — reusing `next_reach` / `lemma_child_on_chain` (completeness) /
`lemma_next_reach_sr` (per-iteration peel), `decreases srk[cur] + 1` — then four
straight-line splice fixups, each tracked by a ghost `Map::insert` (`mw → ma → mb →
mc → md → mfin`) with `store.slot_view() =~= m_i` asserted *inside* each conditional
(the doc 24 §2 scoping rule). The final `mfin =~= unlinked(m0, slot, last)` is a
per-slot case analysis (slot / child / the four non-child roles), then `cspace_wf`
and the count read off the Half-A lemmas.

Unlike `slot_move`, the walk comes **first** and the fixups depend on the tail
`last` it produces. `last` is threaded into `unlinked` (it is `next`'s new
`prev_sib`), so the proof needs **tail uniqueness**: `lemma_unique_tail`
(via `lemma_reach_comparable` — two nodes reachable from a common start are
comparable, the `next_sib` graph being functional) discharges `last_wf`'s
uniqueness clause. `count_nonempty` is *immediate* here (unlink moves links only,
never empties a cap — the filter set is identical), simpler than `slot_move`'s
`lemma_move_count`.

---

## 2. Verus-mechanics findings worth recording

1. **Isolate nonlinear arithmetic in tiny helpers.** The band witness is the only
   nonlinear reasoning in the whole port. Two one-line lemmas — `lemma_scaled_lt`
   (`x<y, w>0 ⟹ x·w < y·w`, via `lemma_mul_strict_inequality`) and
   `lemma_band_below` (the gap-fit inequality, via `broadcast use
   group_mul_is_commutative_and_distributive` + `lemma_mul_inequality`) — keep the
   product reasoning out of the big case analysis, which then stays first-order and
   linear. Z3 was reliable once the multiplication was quarantined this way.
2. **A scoped `if let` fact does not survive the block; a standing implication
   does.** Needing "`slot`'s next sibling is a non-child" outside its `if let`, the
   working form was `assert(nx is Some ==> m[nx->0].parent != Some(slot))` (a single
   persistent implication over the unwrap), not an `assert` inside `if let Some(nn)
   = nx { … }` (which the verifier forgets on exit). (Recurs from doc 24 §2.2.)
3. **Per-clause lemma factoring is load-bearing, not stylistic.** The combined
   `cspace_wf` preservation over the merge blows the rlimit as a monolith; splitting
   into one lemma per `cdt_wf` clause (each its own SMT query, the transpose-family
   discipline) verified each on the first or second try. `lemma_unlink_siblings`
   (the largest case analysis — both list directions × five chain roles) verified
   first try once structured this way.

---

## 3. What remains (Phase 2 residue → phases 3–5)

| | status |
|---|---|
| `slot_move` body | **closed** (doc 24) |
| `cdt_unlink` body | **closed** (this increment) |
| `delete` body, `obj_unref`, `destroy_cspace`, full `refcount_sound` | deferred to phases 3–5 (cross-object; the destructors are not yet in `verus!{}`) |
| `revoke` "revoked cap survives" | conditional (zombie), §9-entangled (doc 23 §4) — unchanged |

Phase 2's **single-object** residue is now fully closed: both looping ops
(`slot_move`, `cdt_unlink`) carry full body proofs. `delete` is the lone remaining
`external_body` op and closes with the cross-object teardown work (phases 3–5); its
contract stays host-test-checked (`test_store`) until then.

---

## 4. CI / docs

- The `verus` job (`cargo verus verify -p kcore`, no per-proof filter) gates the new
  lemmas (`unlinked`/`last_wf` specs; `lemma_rank_bounded`, `lemma_unlink_roles`,
  `lemma_scaled_lt`, `lemma_band_below`, `lemma_unlink_{sib,acyclic,empty,links,
  siblings,children,count}`, `lemma_unlink_preserves_cspace_wf`,
  `lemma_reach_comparable`, `lemma_unique_tail`) and the `cdt_unlink` body proof.
- `host-tests` still runs `test_store` against the real `cdt_unlink` body.
- `CLAUDE.md`'s Verus section + tier table move `cdt_unlink` to the proven list,
  leaving `delete` as the lone assumed-contract (host-tested) op.
