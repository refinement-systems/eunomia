# Verus findings 3 — phase 2 closeout: the looping-op body-proof core, and a §9 correction

Plan: `doc/plans/3_verus-rewrite.md` (phase 2, §4.1). Prior increments:
`21_verus-findings.md` (phase 2 + 2b), `22_verus-findings.md` (phase 2c). This
increment set out to **close the single-object looping-op body proofs**
(`slot_move`, `cdt_unlink` — remove their `external_body` trusted contracts) and
`revoke`'s "revoked cap survives" (§4.1).

**Honest outcome.** The *structural core* of those body proofs is now **proven**
(32 verified, 0 errors) — the genuinely hard part. The final *body-match*
(showing the imperative neighbour-fixups equal the transposition's renaming) did
**not** close; `slot_move`/`cdt_unlink` stay `external_body`, and the proven core
is banked for the next increment. Separately, the investigation **corrected an
unsound claim in doc 21 §9** about revoke-cap-survival, with executable evidence.

Toolchain unchanged. `cargo verus verify -p kcore`: **32 verified, 0 errors**.
`cargo test -p kcore`: **13 passed**.

---

## 1. Phase 2 closes only *modulo* phases 3–5 (the scoping finding)

Mapping §4.1 to the current state: `cdt_insert_child`/`derive`/`obj_ref` are
proven (full `cspace_wf`, phase 2c); `revoke`/`descend_to_leaf` termination is
proven against `delete`'s assumed contract (2b). The remaining §4.1 rows split:

- **Inherently cross-phase** — `delete`'s body, `obj_unref`, `destroy_cspace`,
  and the *full* `refcount_sound`. These recurse into the channel/notification/
  thread/timer destructors and reference those objects' non-slot refcount terms,
  none of which exist in `verus!{}` until phases 3–5. They cannot close now and
  rightly close *with* those phases (`delete` stays `external_body`, host-tested).
- **The tractable residue** — the two **single-object** looping ops `slot_move`
  and `cdt_unlink` (pure CDT-link surgery, `store.slot`/`set_slot` only). This
  increment targeted these.

So "close phase 2" means "close the single-object residue"; the teardown ops are
co-scoped with phases 3–5. (The plan's §7 lists "delete termination" under phase
2, but its *body* genuinely needs the later phases — phase 2b/2c already handle
this by making `delete` `external_body`.)

---

## 2. The proven core (the hard part of `slot_move`'s body proof)

`slot_move(src, dst)` moves `src`'s cap and CDT position onto the previously-empty
`dst`. The key insight: because **nothing references an isolated empty slot**
(provable from `cspace_wf`), the body's whole effect is the **identity
transposition** π=(src dst) — swap the slot contents at `src`/`dst` and rename
every link through π. A transposition is a pure renaming, so it preserves
well-formedness; and the children-walk re-parents *all* of `src`'s children.

Verified (in `kcore/src/cspace.rs`, no assumptions):

- **`lemma_transpose_preserves_cspace_wf`** — a transposition preserves the full
  `cspace_wf`. The acyclicity ranks transfer through π (`rank'[k] = rank[π(k)]`).
  *Load-bearing technique:* the monolithic proof blew the SMT `rlimit`; **factoring
  it per-clause** (one `proof fn` per `cdt_wf` clause + one per rank) fixed it —
  each solver call starts with a small context. Future relabeling proofs inherit
  this.
- **Child-chain reachability** — the loop-completeness keystone:
  - `next_reach(m, from, k, s)` — `k` reachable from `from` via `next_sib`,
    well-founded on a sibling rank `s` (the walk strictly lowers it);
  - `lemma_child_on_chain` — **every** child of `src` is reachable from `src`'s
    first child. Recurses toward the list head along `prev_sib`; the measure is
    the count of children ranked above `k` (each `prev` step has strictly higher
    rank, so the finite count drops). This is what makes "the loop visits every
    child" provable — it rests directly on phase 2c's `cdt_wf` strengthening
    (`siblings_share_parent`/`head_is_first_child`).
  - `lemma_next_reach_extend` (append a tail edge), `lemma_next_reach_sr`
    (reachability weakly lowers rank — so a node can't reach a higher-ranked one).
- **`lemma_replace_empty_cap`** — an empty cap's *rights bits* are invisible to
  `cspace_wf` (it reads only links + `is_empty_cap`), so clearing `src` to
  `CapSlot::empty()` matches the transposition's `m0[dst]` at `src`.
- **`lemma_move_count`** — a move keeps `count_nonempty` (the non-empty set loses
  `src`, gains `dst`).

The loop in `slot_move` (termination via the live-child count + completeness via
`next_reach`) was also brought to *nearly* verifying against these lemmas.

---

## 3. What did not close (the residue)

The **body-match extensionality**: proving the imperative body — the three
*conditional* neighbour fixups (`src`'s parent/prev/next redirected to `dst`) plus
the children loop — produces *exactly* `relabeled(m0, src, dst)`. This needs a
full characterization of the post-fixup map `m4` (which slot each conditional
`set_slot` touched) matched against the transposition's `ren`, case by case, with
the attendant distinctness facts (a child is none of `src`'s neighbours, etc.,
from acyclicity). It is **mechanical but laborious**, and it did not land this
increment. So:

- `slot_move` and `cdt_unlink` remain **`#[verifier::external_body]`** with their
  unchanged contracts (host-test-checked, `test_store.rs`). Their doc comments now
  record that the structural core is proven and only the body-match remains.
- Net **trusted surface is unchanged** — no `external_body` was removed. The gain
  is the proven core (banked) and the §4 correction.

The path is clear for the next increment: the core lemmas exist; the work is the
`m4`-characterization + extensionality (and the analogous splice for `cdt_unlink`,
which merges two sibling lists rather than relabelling — strictly harder).

---

## 4. Correction to doc 21 §9 (revoke's "revoked cap survives")

Doc 21 §9 documented that `revoke`'s postcondition does not assert the revoked
root stays non-empty, and proposed closing it by **framing `delete` to "empty
only the deleted slot's CDT subtree."** Investigating this, that proposed fix is
**unsound**:

- `delete` of a leaf whose cap is the **last reference** to a cspace runs
  `destroy_cspace`, which empties that cspace's **residents** — and a cspace's
  residents are generally **not** CDT-descendants of the leaf (their lineage is
  independent). So `delete`'s emptied-set reaches *outside* the deleted subtree.
  Host-test **`delete_empties_slots_outside_the_deleted_subtree`** runs the real
  `delete` and asserts exactly this.
- In the seL4-**zombie** case — the revoked root `slot` is itself a resident of a
  cspace whose last surviving cap lies in `slot`'s *own* subtree — `revoke`
  descends to that cap, deletes it, triggers `destroy_cspace`, and **empties its
  own root**. Host-test **`revoke_can_empty_its_own_root_zombie`** runs the real
  `revoke` on such a shape and witnesses the root being emptied (while
  `cspace_wf` is still preserved — that part of revoke's contract holds).

So "revoked cap survives" is genuinely **conditional** (it fails on zombies), and
stating the sound frame/precondition needs cspace-**residency** modelled in the
abstract `Store` spec — which lands with phases 3–5. It cannot be closed now; the
§9 subtree-frame is retracted, replaced by this evidenced characterization.

---

## 5. CI / docs

- The `verus` job (`cargo verus verify -p kcore`) gates the new core lemmas.
- The `host-tests` job gates the two new `test_store` cases (§4 evidence).
- `CLAUDE.md` Verus section notes the banked body-proof core and the §9 retraction;
  `slot_move`/`cdt_unlink`/`delete` remain the assumed-contract (host-tested) ops.

---

## 6. Honest scope summary

| | status |
|---|---|
| Transposition preserves `cspace_wf`; child-chain reachability | **proven** (banked core) |
| `slot_move`/`cdt_unlink` bodies (remove `external_body`) | **not closed** — body-match extensionality is the residue |
| `revoke` "revoked cap survives" | **shown conditional** (zombie); §9's fix corrected; phases-3-5-entangled |
| `delete` body, `obj_unref`/`destroy_cspace`, full `refcount_sound` | deferred to phases 3–5 (unchanged) |

Net: no trusted contract removed; the increment banks the proven structural core
and corrects the §9 revoke-survival claim with executable evidence.
