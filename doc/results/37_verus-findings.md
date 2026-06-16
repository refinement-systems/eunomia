# Verus findings 17 — Phase 5b: `pte_encode` / `pte_output_pa` / `va_range_ok` (the §2.5 PTE isolation theorem)

Plan: `doc/plans/3_verus-rewrite.md` (§4.5 + §7 step 5) and its decomposition
`doc/plans/3_verus-rewrite_phase5-detail.md` (§5b). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer), `36` (phase 5a — the sysabi decoder, the
slice-free confidence-builder). This is the **second** sub-phase of phase 5: the §2.5/§4.5
**PTE isolation theorem**. Still pure bit/index arithmetic — no `Store`, no slices, no
recursion, **no termination obligation** (the first sub-phase since phase 2's revoke with
none) — so 5b banks the bit-vector machinery before the genuinely new page-table partial-map
model arrives in 5c (detail §1.2/§3).

**Outcome.** `cargo verus verify -p kcore`: **155 verified, 0 errors** (was 127 after 5a;
`+28`). The `+28` is the new `verus!{}` items: the eighteen geometry/permission/descriptor
**consts** moved inside the macro (each counts as a verified item — see §2), the
`saturating_mul` `assume_specification`, the three `spec_*`/exec index-helper pairs, `va_range_ok`,
`pte_encode`, `pte_output_pa`, and the two lemmas `lemma_pte_bits`/`lemma_user_va_l1_index`.
`cargo test -p kcore`: **49 passed** (was 43; `+6` — the **first executable aspace-walker host
tests**, detail §5b: the `pte_encode` arms, the `pte_output_pa` round-trip, the `va_range_ok`
boundaries, and the `l1_index ≥ 2` corollary). The aarch64 `kernel` cross-build is unchanged
(ghost erasure; confirmed `cd kernel && cargo build`).

**5b adds no `external_body`.** `pte_encode`/`pte_output_pa`/`va_range_ok`/the index helpers are
**fully proven** — nothing is assumed. This continues phase 5's zero-trusted-residue property
(detail §0): aspace + sysabi are the first modules since phase 2 to add no trusted residue
(3e left `destroy_channel`/`signal`, 4e left `destroy_tcb`).

**The bit-vector const-disjointness proof was not the risk it might have been.** Detail §5b
flagged the mask/shift goals as the place to "isolate the hard step; decomposition beats an
rlimit bump." A single `proof fn lemma_pte_bits` discharged **all eight** PTE field-extraction
facts in one `assert(...) by (bit_vector)` (§2), and `va_range_ok`'s saturating equivalence and
the `l1_index ≥ 2` corollary each closed in one `bit_vector`/`compute` step. No `external_body`,
no fallback, no rlimit bump.

---

## 1. What closed

- **`pte_encode` — the §2.5/§4.5 isolation theorem, ∀ `(pa, perms)`** (`aspace.rs`). The §4.5
  security property as a theorem rather than a sampled assert:
  - **AF + PXN unconditional** (`pte & AF == AF`, `pte & PXN == PXN`) — user pages are never
    EL1-executable;
  - **`AP` grants EL0 write iff `PERM_W`**: `perms & PERM_W != 0 ==> (pte >> 6) & 0b11 == 0b01`
    (`AP_EL0_RW`), else `== 0b11` (`AP_EL0_RO`). The `(pte >> 6) & 0b11` form deliberately matches
    `range_mapped_in`'s writability test (`aspace.rs`) — the 5c bridge;
  - **device is never executable** (the historical **AS-1** bug as a theorem): `perms &
    PERM_DEVICE != 0 ==> pte & UXN == UXN` **even when `PERM_X` is set**, plus `pte & SH_INNER ==
    0` (`SH_NONE`) and `pte & ATTR_DEVICE == ATTR_DEVICE`. The old kernel walker honoured `PERM_X`
    on device memory; that can no longer encode an executable MMIO page;
  - a non-device non-`X` page is `UXN` (`(perms & PERM_DEVICE == 0 && perms & PERM_X == 0) ==>
    pte & UXN == UXN`);
  - the **address field round-trips and is disjoint from the control bits**: `pte & ADDR_MASK ==
    pa & ADDR_MASK`.
  - The **security corollary** — no `perms` combination yields an EL1-writable or EL0-kernel-
    executable page — is the conjunction of PXN-always + the `AP`/`UXN` clauses (no extra
    obligation).

- **`pte_output_pa` round-trip** — `ensures r == pte & ADDR_MASK`; composed with `pte_encode`'s
  address-field `ensures` it gives `pte_output_pa(pte_encode(pa, perms)) == pa & ADDR_MASK` (the
  host-tested corollary).

- **`va_range_ok` — total + fully functional, ∀ `(va, pages)`**: the result is exactly the
  integer predicate `va % PAGE == 0 ∧ va ≥ USER_VA_BASE ∧ (va + pages·PAGE ≤ USER_VA_END)`,
  including the `pages == 0` and saturating-overflow edges. The saturating arithmetic equals the
  int condition because `USER_VA_END = 2³⁹ ≪ 2⁶⁴`, so any saturation forces the range past the
  top (§2).

- **`lemma_user_va_l1_index`** — the §4.5 "user L1 indices never touch the two shared kernel
  entries (0/1)" theorem: every page VA in `[USER_VA_BASE, USER_VA_END)` has `l1_index ≥ 2`.
  Stated over the half-open mapped-page range, so the `pages == 0` edge (`va` can equal
  `USER_VA_END`, whose `l1_index` wraps to 0) is excluded by construction. Consumed by 5d's
  `walk_alloc` reasoning.

- **The L1/L2/L3 index helpers** ported with `spec_*` mirrors (`when_used_as_spec`), so the
  corollary lemma — and 5c's `pt_lookup` spec walk — can name them in spec position.

---

## 2. Verus mechanics worth keeping

- **A const inside `verus!{}` is a verified item — that is the `+28` count jump, not a
  proof-effort signal.** Moving the eighteen consts inside the macro (so the contracts can name
  them, the 5a `NUM_PRIOS` precedent) makes each one a counted verification obligation. The bulk
  of `+28` is bookkeeping; the actual new proof work is the two lemmas + five exec contracts.

- **A `pub` function's contract cannot name a `pub(crate)` item — narrow the function, not the
  const.** `pte_encode`/`pte_output_pa` were `pub`; their `ensures` name the **crate-internal**
  descriptor bits (`AF`/`PXN`/`ADDR_MASK`/…, deliberately `pub(crate)`). Verus rejects this
  ("cannot refer to private const item in 'ensures' clause of public function"). Two clean
  resolutions: promote the bits to `pub` (contradicts their "not part of the public API" intent),
  or narrow the two encoders to `pub(crate)`. The encoders have **no non-test caller** (the public
  aspace surface is `map_in`/`unmap_in`/`range_mapped_in`/`va_range_ok`, which call them), so
  narrowing preserved the descriptor-bits-are-internal design. `va_range_ok` stayed `pub` (it
  names only the `pub` geometry consts). A consequence: `pub(crate) pte_output_pa` then drew a
  dead-code warning (it had been suppressed by `pub`; its only caller is the `cfg(test)`
  round-trip), resolved with a one-line `#[allow(dead_code)]` — it is the documented decoder half
  of the encode/decode pair.

- **The bit-vector discipline: pin every named const to its literal, then bit-blast over
  symbolic-but-constrained fields.** `lemma_pte_bits` first fixes the const literals with `assert(
  AF == 0x400) by (compute)` (etc., the `untyped.rs:540` `by (compute)` precedent), then a single
  `assert(<eight-conjunct field facts>) by (bit_vector)` whose `requires` carry both the
  `pte == (pa & ADDR_MASK) | … | ap | …` construction and the const==literal facts. The
  descriptor-bit masks are **pairwise disjoint**, so each PTE field is independent: bits [7:6]
  come only from `ap` (constrained to `{AP_EL0_RW, AP_EL0_RO}` by the `pte_encode` if-arm), bit 54
  only from `xn`, etc. With the consts pinned, the bit-blast is a tautology and all eight
  ensures fall out of one query. `pte_encode`'s body is then one `lemma_pte_bits` call; the
  chaining from `perms & PERM_W != 0 ==> ap == AP_EL0_RW` (the `let ap = if …`) to the writability
  ensures is automatic.

- **vstd specs `saturating_add`/`saturating_sub` but not `saturating_mul`.** `va_range_ok` needs
  the latter (`std_specs/num.rs` stops at add/sub). A one-line `assume_specification[
  u64::saturating_mul ] returns (if x * y > u64::MAX { u64::MAX } else { (x * y) as u64 })`
  mirrors the vstd form (the `untyped.rs` `checked_next_multiple_of` precedent). With it, the
  saturating equivalence proved automatically given one hint — `assert(USER_VA_END ==
  0x80_0000_0000) by (compute)` to pin `1 << 39` to its value — because the only nonlinearity,
  `pages * PAGE`, is linear once `PAGE = 4096` is a literal const, and the rest is a linear
  case-split over the two saturation branches (both of which push the sum past `USER_VA_END`).

- **No termination obligation — the first phase-5 sub-phase with none, and the first since phase
  2's revoke.** The index walk is fixed 3-level depth and `pte_encode`/`va_range_ok` are
  straight-line, so there is no `decreases`. The proof effort is entirely the bit-vector field
  isolation and the saturating equivalence. (Termination returns in 5c's one `range_mapped_in`
  `while` loop and is the chief theme nowhere else in phase 5.)

- **The `usize` cast bridge for the index corollary.** `lemma_user_va_l1_index` proves `((va >>
  30) & 0x1FF) >= 2` (and `< 0x200`) by `bit_vector` over the `[USER_VA_BASE, USER_VA_END)`
  bounds, then a one-line `assert(((va >> 30) & 0x1FF) as usize >= 2)` bridges to
  `spec_l1_index`'s `usize` value (the masked value `< 2¹⁶` casts exactly). Cheap, but worth
  recording as the first `usize`-cast reasoning in an aspace proof.

---

## 3. What 5b does **not** touch (carried forward)

Per detail §1.5 / §4, 5b ports the §2.5 PTE/range pure arithmetic and adds no `external_body`
and no slice/`Store` work. Still ahead in phase 5:

- **5c** — `range_mapped_in` + the new `pt_lookup`/`pt_wf` page-table partial-map model (the one
  genuinely new proof model and the chief design risk; **Verus slice reasoning lands here**; the
  5b `(pte >> 6) & 0b11` writability bridge and the `spec_*` index helpers feed it).
- **5d** — `map_in` (the two-pass walk-alloc; the tree-shape no-aliasing frame lemma; consumes
  `lemma_user_va_l1_index` and `pte_encode`'s isolation ensures).
- **5e** — `unmap_in` + the TLBI/barrier effect-ordering ghost log + the phase-5 closeout (the
  `CLAUDE.md` `### Verus` / §6-tier-table update covering 5a–5e at once; the already-discharged
  §7-step-5 clauses; the reaffirmed cross-object-teardown phase).

The cross-object teardown and the full `refcount_sound` census remain the recommended dedicated
phase **after** phase 5 (unblocked once aspace's walker is ported, §1.5).
