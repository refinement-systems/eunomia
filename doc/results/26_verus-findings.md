# Verus findings 6 — Phase 3 opens: untyped `retype_check` + `reset` (§3a)

Plan: `doc/plans/3_verus-rewrite.md` (phase 3, §4.2 remainder) and its detailed
decomposition `doc/plans/3_verus-rewrite_phase3-detail.md` (§3a). Prior
increments: `21`…`25_verus-findings.md` (phase 2 — the cspace/CDT core, closed
with `slot_move`'s and `cdt_unlink`'s body proofs). This increment is the first
of phase 3's five sub-phases: the **confidence-builder**, porting the two pure
slot-state halves of `untyped.rs` into `verus!{}` with no channel or
notification coupling.

**Outcome.** `cargo verus verify -p kcore`: **57 verified, 0 errors** (was 55 —
`+retype_check`, `+reset`). `cargo test -p kcore`: **15 passed** (was 13 —
`+retype_check_arms`, `+reset_arms`, the executable differential checks). The
aarch64 `kernel` cross-build is unchanged (ghost code erases). No new
`external_body` boundary, no new lemmas — both proofs are straight-line over the
already-verified `Store` seam (`slot_view`/`refs_view`). `carve`/`carve_place`
(the geometry, phase 0) already covered the placement arithmetic; 3a completes
the *slot-state* validation half.

---

## 1. What closed

- **`untyped::retype_check`** — the slot-state half of retype's validation. Now
  carries: the **read-only frame** (`slot_view`/`refs_view` unchanged on *every*
  path — it calls no `set_*`); on `Ok`, the returned `(base, size, watermark)`
  **is** the untyped's geometry and the destination(s) are empty/detached; and
  the **error precedence** (`NotUntyped` before `DestOccupied`, with the channel
  `dst2` validity — `Some`, `≠ dst`, empty — pinned in the `DestOccupied` arm).
- **`untyped::reset`** — the watermark-reset half of the reclaim primitive (§2.5).
  Per-arm: `Ok` differs from entry by exactly `reset_slot` at `ut_slot` (watermark
  zeroed, base/size/rights/CDT-links and every other slot intact); `BadArg` when a
  CDT child remains (caller has not revoked); `NotUntyped` otherwise; both `Err`
  arms read-only. A new spec fn `reset_slot(s)` is the closed-form "watermark
  zeroed" slot.

Both contracts are also **host-test-checked** against the real bodies in
`test_store.rs` (`check_retype_check`/`check_reset` re-derive the spec result
from the store state and assert the body matches it *and* left the arena
untouched, via a `fingerprint` snapshot). This is belt-and-suspenders — the
bodies are *proven*, not assumed `external_body` — but it keeps the `test_store`
cadence and guards against a future spec/body drift.

### 1.1 The `reset` contract: a deliberate deviation from the detail-plan text

The detail plan (§3a) listed *both* `requires ut_slot is Untyped` *and* an
`Err(NotUntyped)` arm — mutually exclusive. Resolved the faithful way, mirroring
the already-verified read-only-on-error `retype_check`: **no `requires Untyped`;
per-arm postconditions** instead. The body genuinely has a `NotUntyped` path, so
a `requires` would make it dead code and silently drop that path's store-unchanged
guarantee. The per-arm form proves strictly more.

---

## 2. Verus mechanics worth keeping (the workflow re-banked on a clean module)

These are the friction points a port from plain Rust into `verus!{}` hits; none
are deep, but they recur, so they are the reusable lessons for 3c–3e.

1. **The read-only `&mut S` frame is free.** `retype_check` keeps its `&mut S`
   signature (lowest churn — the kernel composes it unchanged) yet proves
   `final(store) == old(store)` simply by calling no mutating method; Verus
   tracks that the reference target is untouched. No `&S` narrowing needed.

2. **`matches`-with-`&&` must be parenthesised as an operand.** Verus rejects
   `A ==> B matches Pat && C` ("matches with && is currently not allowed on the
   right-hand-side of most binary operators"). Wrap the implication RHS:
   `A ==> (B matches Pat && C)`. The matched bindings stay in scope across the
   parenthesised `&&` chain (the `m[a].next_sib matches Some(b) ==> …` idiom).

3. **Cross-module spec imports erase — reference them, don't `use` them.** A
   normal `cargo build` drops local `spec fn`s entirely, so a module-level
   `use crate::cspace::is_empty_cap` is an **unresolved import** in the erased
   build (the contract that references it has erased, but the `use` survives).
   Fix: full-path the spec fn *inside* the (erased) `ensures` —
   `crate::cspace::is_empty_cap(…)` — and full-path the spec-only type
   `crate::cspace::CapSlot` in `reset_slot`. The `external_trait_extension` trait
   `StoreSpec` (needed in scope to resolve `store.slot_view()` during
   verification) *does* survive erasure as a nameable trait, but is unused there,
   so it is imported under `#[allow(unused_imports)]`. This is the template for
   every later cross-module spec consumer (`channel.rs` in 3b–3e).

4. **Exec `==` is unavailable on the handle/`ObjType` types.** `SlotId`/`ObjId`
   are plain-Rust newtypes (their `PartialEq::eq` is external), and a verus-native
   enum's derived `eq` is likewise ignored in exec. So the bodies compare the raw
   field (`d2.0 == dst.0`) and use `matches!(ty, ObjType::Channel)` instead of
   `==`. Verus relates both to the structural spec equality used in the contract.
   (The `let-else` struct patterns and `.cap.is_empty()` also became `match` /
   `matches!(…, CapKind::Empty)`, the `derive`-body idiom — behaviour identical
   after erasure.)

---

## 3. Scope held (what 3a did *not* touch)

- **`retype_install` is 3c, not 3a** — it installs the rights-inheritance table
  and runs the channel two-endpoint dance, which needs the `chan_view` trait
  extension landing in 3b. Left plain Rust.
- No `CLAUDE.md` / spec edits this PR — the phase-3 closeout (moving the untyped
  ops onto the proven list, recording the new division) lands in **3e** per the
  detail plan; 3a only seeds this doc.
- No new `external_body`, no notification/channel coupling — that is 3b onward.
