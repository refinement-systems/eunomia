# Verus findings 4 — `slot_move`'s body proof closes (the transposition lands)

Plan: `doc/plans/3_verus-rewrite.md` (phase 2, §4.1). Prior increments:
`21_verus-findings.md` (phase 2 + 2b), `22_verus-findings.md` (phase 2c),
`23_verus-findings.md` (phase 2 closeout — the *banked structural core*). This
increment **closes the `slot_move` body proof**: its `#[verifier::external_body]`
is removed, so the contract is now a theorem rather than an assumed,
host-test-checked boundary.

**Outcome.** `cargo verus verify -p kcore`: **39 verified, 0 errors** (was 32).
`cargo test -p kcore`: **13 passed** (the `test_store` differential check still
runs the real body — now also proven). The aarch64 `kernel` cross-build is
unchanged (ghost code erases). Trusted surface shrinks by one op: `slot_move` is
off the `external_body` list; only `cdt_unlink` and `delete` remain there.

---

## 1. What closed

Doc 23 §2 banked the *structural core* — `lemma_transpose_preserves_cspace_wf`
(a transposition keeps the full `cspace_wf`) and the child-chain reachability
machinery (`next_reach`, `lemma_child_on_chain`, `lemma_next_reach_extend`,
`lemma_next_reach_sr`) — but left the **body-match extensionality** open: proving
the imperative body produces *exactly* the transposition. That is what closed.

The proof shows the final arena `mfin` equals `relabeled(m0, src, dst)` with `src`
cleared to the empty slot, then reads everything off the banked lemmas:

- `cspace_wf(mfin)` ← `lemma_transpose_preserves_cspace_wf` (transposition keeps
  `cspace_wf`) + `lemma_replace_empty_cap` (swapping `relabeled[src]` — empty,
  all-`None` links — for the cleared `src` slot preserves it);
- `count_nonempty` unchanged ← `lemma_move_count`;
- `final[dst].cap == old[src].cap`, `is_empty(final[src].cap)`, `dom`/`refs_view`
  framed ← the per-slot equality + `set_slot`'s frame.

### The shape of the body-match

The key fact (doc 23 §2): nothing references the detached empty `dst`, so the
move is the identity transposition π=(src dst). Five small support lemmas turn
that into the per-slot characterization:

- **`lemma_nothing_points_to_empty`** — in a `cdt_wf` store no link names a
  detached empty slot. So `dst` is never a link *target* in `m0`, and `ren(·)`
  only ever rewrites `src → dst`, one-directionally.
- **`lemma_src_no_self_link`** — `src`'s four links avoid `src` (rank / sibling
  rank, via the two acyclicity witnesses).
- **`lemma_dst_relabeled`** — `relabeled[dst] == m0[src]` *verbatim* (src's links
  avoid both `src` and `dst`, so the rename is the identity on them — this is
  why the body copying src's slot onto dst *unrenamed* is correct).
- **`lemma_child_relabeled`** / **`lemma_generic_relabeled`** — the renamed value
  at a child of `src` (parent flips to `dst`, other links fixed) and at a generic
  non-`src`/`dst` slot (each `src`-naming link flips). The neighbour fixups read
  off these.

The straight-line fixups are tracked as ghost intermediate maps `m1…m4` (one per
`set_slot`), each a single `Map::insert`; the children walk re-parents every
child via a `next_reach`-split loop invariant; the final clear gives `mfin`. The
**C1/C2/C3** characterization (src untouched / children at `m0` until the loop /
non-children at their renamed value) ties `m4` to `m0` and `relabeled`, and the
classification (the only `src`-naming slots are its children, prev-sib, next-sib,
and head-parent — pairwise distinct) follows from the `cdt_wf` consistency
clauses.

### The children walk (completeness + termination)

The loop invariant splits `src`'s children by reachability from the cursor: those
`next_reach`-reachable from `cur` still hold `m0[x]`, the rest hold the renamed
value. Maintenance is local — peeling one `next_sib` edge and `lemma_next_reach_sr`
(reach weakly lowers the sibling rank, so the just-processed node is unreachable
from its successor). **Completeness** rests on `lemma_child_on_chain` (every child
is reachable from the first child), invoked once at loop entry so the "done"
branch starts vacuous. **Termination** is `decreases s[cur] + 1` (the `+1` lets
the `Some(cur) → None` exit step decrease even at sibling-rank 0).

---

## 2. Three Verus-mechanics findings worth recording

These cost iterations and will recur in the `cdt_unlink` / phase-3–5 ports:

1. **Exec `==` on an external type carries no spec.** `SlotId` is an
   `external_type_specification` (defined outside `verus!{}`), so the body's
   `if pas.first_child == Some(src)` gave the verifier *no* fact in either branch
   — the `==` operator is opaque. Fix: compare the `u64` tag (`c.0 == src.0`,
   native to Verus) and bridge to `SlotId` equality via `ext_equal`
   (`c == src <==> c.0 == src.0`). **The spec-level `==` (in `proof`/`spec`) is
   fine** — it is only the *exec operator* that is unspecified. (The existing
   verified ops sidestep this by using `if let`/`matches!`, never exec `==` on a
   handle.)
2. **Map equality after a conditional block must be proven *inside* it.** The
   neighbour handle (`pa`/`pv`/`nx`) is in scope only within its `if let`, so
   `store.slot_view() =~= m_i` cannot be reconstructed afterwards — it has to be
   asserted in each branch (including the `else`), where the handle is live.
3. **A loop-constant ghost `let` is not automatically known inside the loop.**
   `let ghost rl = relabeled(...)` verifies fine *outside* the loop, but inside
   the body `rl == relabeled(...)` had to be added to the invariant before the
   lemma facts (stated over `relabeled(...)`) could be applied to `rl[cur]`.

Also: the two `debug_assert!(…is_empty())` lines were dropped (the precondition
already guarantees them, and `Cap::is_empty` has no Verus spec); and
`CapSlot::empty` got an `assume_specification` (empty cap, all links `None`) so
the final clear is callable in verified exec.

---

## 3. What remains (Phase 2 residue, unchanged scope)

| | status |
|---|---|
| `slot_move` body (remove `external_body`) | **closed** (this increment) |
| `cdt_unlink` body | **open** — a sibling-list *merge* (children threaded between slot's prev/next), strictly harder than the transposition; needs a two-list splice invariant |
| `delete` body, `obj_unref`, `destroy_cspace`, full `refcount_sound` | deferred to phases 3–5 (cross-object; the destructors are not yet in `verus!{}`) |
| `revoke` "revoked cap survives" | conditional (zombie), §9-entangled (doc 23 §4) — unchanged |

So Phase 2's *tractable single-object residue* is now half-closed: `slot_move`
done; `cdt_unlink` is the remaining looping-op body proof, and the cross-object
teardown ops rightly close with phases 3–5.

---

## 4. CI / docs

- The `verus` job (`cargo verus verify -p kcore`) gates the new lemmas and the
  `slot_move` body proof (no per-proof filter).
- `host-tests` still runs `test_store` against the real `slot_move` body.
- `CLAUDE.md`'s Verus section + tier table move `slot_move` to the proven list,
  leaving `cdt_unlink`/`delete` as the assumed-contract (host-tested) ops.
