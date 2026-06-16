# Verus findings 20 — Phase 5e: `unmap_in` + the TLBI effect-ordering log

Plan: `doc/plans/3_verus-rewrite.md` (§4.5 + §7 step 5) and its decomposition
`doc/plans/3_verus-rewrite_phase5-detail.md` (§5e). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36`…`39` (phase 5a–5d — the sysabi
decoder, the §2.5 PTE isolation theorem, `range_mapped_in` + the page-table partial-map
model, and `map_in`). This is the **fifth and final** sub-phase of phase 5: `unmap_in`
(the read-mostly leaf-clear counterpart to `map_in`) and the decided §4.5 TLBI/barrier
**effect-ordering** proof — the first time the `Store` seam carries a *hardware effect
log* rather than object state.

**Outcome.** `cargo verus verify -p kcore`: **225 verified, 0 errors** (was 214 after 5d;
`+11`). The `+11`: the `unmap_log` effect-log spec, the ported `unmap_in`, five proof
lemmas (`lemma_unmap_log_step`, `lemma_present_leaf_in_leaves`, `lemma_leaf_clear_none`,
`lemma_leaf_clear`, `lemma_unmap_in_step`), and the strengthened `lookup` (now also yields
the leaf *slot*, `pt_leaf_slot`). `cargo test -p kcore`: **68 passed** (was 62; `+6` — the
first executable `unmap_in` host checks: clear-and-log, the absent-L3-region skip, the
region-boundary partial skip, the partial-overlap frame, the present-L3-including-zero-
leaves TLBI census, and the map→unmap→remap round-trip). The aarch64 `kernel` cross-build
is unchanged (ghost erasure; `unmap_in`'s public signature is byte-identical — only
`requires`/`ensures`/ghost added).

**5e adds no `external_body`** — `unmap_in` and every lemma are **fully proven**. Phase 5
is therefore the **first phase since phase 2 to add zero trusted residue** (3e left
`destroy_channel`/`signal`'s assumed contracts, 4e left `destroy_tcb`'s; aspace + sysabi
leave nothing). **No termination theorem** (detail §1.3): the unmap loop is a bounded
`while i < pages` with `decreases pages - i`; the fixed-depth walk needs none.

**The full effect-ordering proof landed — the fallback was not needed.** Detail §2.5e/§3
budgeted a fallback (a page-table-only postcondition + a structural host-test of the TLBI
order) in case the log-frame interaction with the leaf-clear mutation proved
disproportionate. It did not: the two mutable targets (`pool`, `store`) are disjoint `&mut`
borrows, so the page-table facts (`pt_lookup`/`pt_wf`) and the log facts (`tlb_log_view`)
compose without interference, and the ordering theorem is a real `unmap_in` postcondition.

---

## 1. What closed

- **The `ExStore` TLBI effect-log seam — the one Store-seam touch (detail §1.4).** A seventh
  ghost view `spec fn tlb_log_view(&self) -> Seq<(u16, u64)>` joins the six object views,
  with three contracts: `tlb_invalidate_page(asid, va)` **appends** `(asid, va)` (the
  load-bearing clause — it is what makes "one TLBI per cleared page, in order" provable);
  `barrier_after_unmap` frames the log (a pure fence, so the loop's accumulated log survives
  it); and `barrier_after_map` (from 5d) gains a frame clause so the log only ever grows via
  `tlb_invalidate_page`. The new view is left **unconstrained** across the ~70 object setters
  — no cspace/channel/notif/timer op interleaves a setter with a TLBI, so adding it is a
  *localized* seam change, not a per-setter sweep. `ArrayStore` gets a real
  `Vec<(u16, u64)>` behind the hooks (replacing the `unimplemented!()`s), so `check_unmap`
  host-verifies the contract against the real body.

- **`unmap_in` — the full §4.5 contract, ∀ `(va, pages)`** (against `pt_wf` + the log):
  - **range unmapped:** ∀ `i < pages`, `pt_lookup(va + i·PAGE)` is `None` or `Some(0)` — no
    live mapping survives in the range;
  - **the frame:** every aligned user page **outside** `[va, va+pages·PAGE)` keeps its
    mapping (`pt_lookup` unchanged) — the no-aliasing tree theorem reused from 5d
    (`lemma_distinct_pages_slots`) makes the per-page clears non-interfering;
  - **`pt_wf` preserved:** clearing a leaf keeps the tree (leaf tables are unconstrained by
    `pt_wf_leveled`; **no table is freed** — only leaves zeroed, so accounting/closure/no-
    aliasing all hold with the *same* `pool_used`);
  - **the effect-ordering theorem:** `tlb_log_view()` grows by exactly
    `unmap_log(l1, old_pool, base, asid, va, pages)` — one `(asid, va+i·PAGE)` per
    **present-chain** page, in ascending `i`, then the (log-framing) barrier.

- **`unmap_log` — the §4.5 "one TLBI per cleared page" as a closed-form spec.** A recursive
  `Seq<(u16, u64)>` over the *original* tables: page `i` contributes `(asid, pg(va,i))` iff
  `pt_lookup(original, pg(va,i)) is Some`. Defined over the original pool because a clear sets
  a leaf to `0` (still `Some`) and frees no table, so **a page's present-ness is invariant
  across the whole unmap** — the runtime branch (`lookup` of the *current* table) provably
  agrees with this original-table predicate (bridged through the frame invariant).

- **`lookup` strengthened to yield the leaf slot.** Its `Some` arm now also `ensures
  pt_leaf_slot(va) == Some((l3, e))` (not just the leaf *value*), so `unmap_in`'s clear can
  hand `lemma_leaf_clear` the slot the §5d leaf-write frame needs. The body already returned
  exactly that slot, so the extra clause was free; `range_mapped_in` (the other consumer)
  ignores it.

- **The clear-and-frame lemmas, almost entirely reused from 5d:**
  - `lemma_leaf_clear` = `lemma_leaf_write` (5d) with `pte == 0` — preserves `pt_wf`, makes
    `va` read `Some(0)`, frames every page whose slot differs — **plus** the new
    `lemma_leaf_clear_none` for the *absent*-page case the §5d frame did not cover (a `None`
    walk dead-ends at `l1`/an inner table, none of which is the cleared leaf table, so it
    stays `None`);
  - `lemma_present_leaf_in_leaves`: a present leaf slot's table is a leaf (`∈ leaves`) — the
    tail of `lemma_walk_alloc_resolves`, restarted from `pt_leaf_slot` (what `lookup` hands
    over) so `lemma_leaf_clear` can invoke the leaf-write frame;
  - `lemma_unmap_in_step`: the per-step advance of the "range-unmapped" + "outside-range
    framed" invariants — the `unmap` analog of `lemma_map_in_step`, leaning on the same
    distinct-slots tree theorem.

---

## 2. Verus mechanics worth keeping

- **The unmap "skip" is per-L3-table (2 MiB), not per-leaf — and the spec says so.**
  `unmap_in` clears + TLBIs every page whose **table chain is present**, *including zero
  leaves* (`lookup` is `Some` whenever the L3 table exists). Only a page in a wholly-absent
  L3 region is skipped. `unmap_log`'s "present" is exactly `pt_lookup is Some` = chain
  present, so the spec is faithful to the original walker — a page already `Some(0)` still
  gets a (harmless, hardware-no-op) TLBI. The host tests were written against the *wrong*
  intuition (per-leaf skip) first and failed loudly, which is how the semantics got pinned
  down; the corrected `unmap_skips_absent_l3_at_region_boundary` test exercises the genuine
  per-L3 skip across the 2 MiB boundary.

- **The existential `pool_used` avoids a kernel-signature change.** `pt_wf` is parameterised
  by `pool_used`, but `unmap_in` allocates nothing, so threading a real `pool_used` argument
  would force a kernel-shell edit. Instead the contract quantifies it
  (`requires/ensures exists|pu| pt_wf(…, pu, …)`) and the body `choose`s one witness, fixed
  through the loop — the leaf-clear preserves `pt_wf` at the *same* `pu`, so the existential
  closes trivially. The kernel `unmap` shell calls the byte-identical erased signature
  unchanged.

- **`push` distributes over `+` by extensionality.** The log advance needs
  `(old ++ unmap_log(i)).push(x) == old ++ unmap_log(i).push(x)`; Verus discharges it with
  the seq extensional-equality operator `=~=` in one `assert`, no hand seq-lemma.

- **A `closed` *recursive* spec fn needs an explicit successor reveal to unfold a symbolic
  arg.** `unmap_log((i+1) as nat)` does not auto-unfold for symbolic `i`;
  `lemma_unmap_log_step` does it once (`reveal_with_fuel(unmap_log, 2)` plus
  `(i+1) as nat > 0` / `((i+1) as nat - 1) == i as nat`), giving the clean
  "append-iff-present" step the loop's log invariant consumes.

- **Two disjoint `&mut` borrows compose for free.** `tlb_invalidate_page` takes neither
  page-table slice and the leaf clear does not touch `store`, so Verus already knows the log
  append cannot perturb `pt_lookup`/`pt_wf` and the clear cannot perturb the log — the page-
  table postcondition and the effect-ordering postcondition are independently established and
  simply conjoined. This is why the budgeted fallback was unnecessary.

- **`#[allow(unused_imports)]` on the `StoreSpec` import (the doc-26 §2.3 idiom).** `unmap_in`
  names `tlb_log_view` (spec-only) on the generic `S: Store`, so `StoreSpec` must be in
  scope for verification but erases in the normal build — same as `channel`/`notification`/
  `timer`.

---

## 3. Phase 5 closeout

With 5e, **phase 5 of the Verus rewrite is complete** (§4.5 aspace + §4.6 sysabi + §7
step 5). `cargo verus verify -p kcore` now proves, fully and with **no `external_body`**:

- **sysabi** `decode` (total over `(u64, [u64;6])`; per-arm length/event/which/prio
  validation) + `ObjType::from_u64` (total) — 5a, doc 36;
- **aspace** `pte_encode` (the §2.5/§4.5 isolation theorem incl. device-never-executable),
  `pte_output_pa`, `va_range_ok` (+ the user-L1-never-touches-kernel corollary) — 5b, doc 37;
  `range_mapped_in` (full functional equivalence to the `pt_lookup` ghost containment) + the
  `pt_lookup`/`pt_wf` tree model — 5c, doc 38; `map_in` (adds exactly the requested pages or
  fails atomically; the two-pass totality; the no-clobber frame) — 5d, doc 39; and `unmap_in`
  (range unmapped; the outside-range frame; one TLBI per present-chain page, in order) — 5e,
  this doc;

all against the `pt_wf` tree-shape/pool-accounting invariant; with **no termination
obligation**; and the only `Store`-seam change the three effect-hook contracts +
`tlb_log_view`.

**The two §7-step-5 master-plan clauses were already discharged** (detail §0): there is no
`kcore/src/proofs/` to delete (the Kani→Verus migration removed it wholesale), and
`cargo kani -p kcore` was already retired in phase 2. Phase 5 added neither.

**Next: the recommended cross-object-teardown phase, now unblocked.** Detail §1.5 / doc 35
§4 reaffirm a dedicated phase to close `delete`/`revoke`/`obj_unref`/`destroy_cspace`/
`destroy_channel`/`destroy_tcb` bodies, `unref_cspace`/`unref_aspace`/`aspace_destroy`/
`aspace_unmap`, the seL4-zombie recursion measure, and the **full `refcount_sound` census**
— of which phases 3/4 landed the binding/waiter/armed-timer terms and which includes the
**frame-mapping term** the aspace mappings now contribute. That phase was blocked on the
aspace walker being ported; 5e ports it, so it is the next phase. Phase 5 adds zero teardown
work itself. No spec-doc edit — that is the phase-8 closeout (doc 30 §3).
