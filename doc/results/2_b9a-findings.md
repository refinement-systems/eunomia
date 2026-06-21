# B9A — Preemptible revoke: verified bounded `revoke_step` + marker + guard (findings)

Working notes from the implementation of **Phase B9A** (`doc/plans/9_b9-detail.md`,
sub-phase B9A — the Verus deliverable). Records what landed, the proof techniques and
tooling facts worth keeping, the design refinement chosen during implementation, and what
remains (B9B/B9C).

Closes the verification core of audit **M-1** (the `revoke` walk is not
preemptible/restartable). Conforms rev1§2.2 ("because that walk is unbounded, it is
preemptible and restartable").

---

## 0. Headline

`cargo verus verify -p kcore` rose **374 → 381, 0 errors**; `cargo test -p kcore` is green
(100 → **103 passed**, three new B9 units); `cd kernel && cargo build` (aarch64-none-softfloat
cross-build) is green. The change is **purely additive to the verified surface** — no
`external_body`/`assume_specification` seam was touched except to extend `CapSlot::empty`'s
spec for the new field, and no kernel behaviour changed (the `EAGAIN` ABI is B9B).

What landed, all in `kcore/src/cspace.rs` unless noted:

- **`revoking: bool` on `CapSlot`** (+ `empty()`, the `assume_specification`, and every spec
  mirror — see §2). A CDT-inert field: no structural or census predicate reads it.
- **`RevokeStatus { Done, More }`** and **`revoke_step(store, slot, budget) -> RevokeStatus`** —
  the bounded, restartable form of `revoke`'s walk. Same `requires` bundle (+ `budget >= 1`);
  on `Done` the same `revoke` postconditions (descendant-deletion completeness, root-survival,
  death-provenance) plus the cleared marker; on `More` the marker set + the partial-progress
  fact `count_nonempty` strictly dropped. Per-call termination by a bounded counter
  (`decreases budget - n`).
- **`lemma_set_revoking_frames`** — the marker frame: a single-slot edit flipping only
  `revoking` (cap + four links kept, every non-slot view framed) preserves the whole invariant
  bundle (`cspace_wf`/`count_nonempty`/`only_empties`/`refcount_sound`/`caps_consistent`/
  `end_caps_sound`/`census_dom_complete`).
- **`lemma_revoke_step_death_provenance`** — carries the root death-provenance witness across
  the marker write (`dead_obj` reads only `refs_view`, which `set_slot` frames).
- **`ancestor_or_self_revoking(store, start)`** — the verified upward ancestor-walk; and the
  **`derive` guard** (refuse, as an exact no-op, when `src`'s ancestor chain reaches a revoking
  root).
- **`kcore/src/test_store.rs`** — `gen_chain` + three units (bounded completion across
  ⌈n/budget⌉ quanta; single-quantum Done; the marker blocking/unblocking `derive`).
- **`doc/guidelines/verus_trusted-base.md`** — kcore Baselines total `374 → 381`.

`revoke` itself is **kept intact** (additive `revoke_step`), so the kernel still links the
run-to-completion form until B9B rewires the `CapRevoke` handler. No `exceptions.rs` change.

---

## 1. The design refinement: set/clear the marker *after* the loop

Design-decision-2 in the plan says "set the marker on entry." Implementation took the
equivalent but cheaper **set-after-loop** form: `revoke_step` runs the bounded delete loop
first, then in *one* final `set_slot` sets `revoking = true` (returning `More`, children
remain) or clears it (returning `Done`, subtree empty).

Why it is better, not just different:

- The marker only has to hold **during the gap between quanta** (when EL0 runs IRQs-unmasked
  and a `derive` could interleave). Inside a single `revoke_step` call the kernel is
  non-preemptible, so nothing interleaves mid-loop. Setting it at the end of every `More`
  return covers every gap.
- **It keeps `delete`/`delete_prepare`/`cdt_unlink` literally unchanged.** Had the marker been
  set *before* the loop, the loop would need `delete` to *frame* the root's `revoking` bit
  across each leaf teardown — but those functions' `ensures` say nothing about `revoking`, and
  the plan forbids editing them. Overwriting the bit from the post-loop structure sidesteps the
  whole question: the only `set_slot` that touches `revoking` is the single post-loop write, so
  the frame obligation collapses to one lemma (`lemma_set_revoking_frames`).

Consequence: the `revoke_step` loop body and invariant are **identical** to `revoke`'s, plus a
counter `n <= budget` and the accumulator `count_nonempty(store) + n <= count_nonempty(old)`.
The `More` arm derives `n == budget` from the `&&`-guard's negation (the only way the loop
exits with `first_child` still `Some`), hence `budget >= 1` deletions, hence strict progress.

---

## 2. The field-addition frame audit (what the gate caught)

Adding a sixth `CapSlot` field perturbs every place that builds or compares a *whole*
`CapSlot`. `cargo verus verify` (not `cargo test`) is the gate that finds them, because spec
`fn`s are erased under plain `cargo`. The complete list:

- **Spec mirrors** (each gets one `revoking: …` line, kept explicit per the file's
  portability comment): `set_parent`/`set_first_child`/`set_next_sib`/`set_prev_sib`,
  `relabeled`, `unlinked` (×3 branches) — all in `cspace.rs`; **`reset_slot` in `untyped.rs`**
  (the one the exploration missed — only the Verus build flagged it, since it is a spec `fn`).
- **`CapSlot::empty()`** body (`revoking: false`) + its `assume_specification` (`!r.revoking`).
- **`slot_move` exec** needed `d.revoking = s.revoking;` — it copies the source slot field by
  field into the transposition target, and the proof's `assert(d == m0[src])` is a *whole-slot*
  equality, so the new field had to be copied too. (A queued/moved cap is never a revoke root,
  so this only ever moves `false`, but the proof needs the exact equality.)
- **`cdt_unlink`** hit the **rlimit** on macOS: the extra equality term tipped an already-heavy
  body over the default budget. Fixed with the project's established pattern —
  `#[verifier::spinoff_prover] + #[verifier::rlimit(60)]` (same as the two heavy bodies at
  ~5035/5085). No proof restructuring needed; the body's read-modify-write mutations preserve
  `revoking` automatically.
- Test fixtures in `test_store.rs` (12 literals) got `revoking: false`; `frame_map_va`'s `..cs`
  struct-update was already correct.

**Tooling fact worth keeping:** `cargo test -p kcore` compiled clean while `cargo verus verify`
failed on `untyped.rs:reset_slot` — because spec `fn`s erase under plain `cargo`. When adding a
field to a Verus-modelled struct, **the Verus build is the only complete frame audit**; a green
`cargo test` says nothing about spec-`fn` literals.

---

## 3. Proof techniques worth generalizing

- **The marker frame lemma** reuses existing precedents wholesale, because flipping a
  CDT-inert field is *strictly easier* than B8A's `frame_map_va` (which changed the cap):
  - structural `cspace_wf`: `lemma_local_cap_edit_preserves_cspace_wf` (links + cap-emptiness
    preserved — here the cap is identical, so trivially);
  - census: `obj_census` is fixed because `slot_refs`/`frame_map_refs` are **cap-filters**
    (identical caps ⇒ identical filtered sets, via `=~=`) and every other term reads a framed
    view → `lemma_refcount_sound_from_census_eq`;
  - `caps_consistent`: `cap_consistent` reads `slot_view` **only via `.dom()`** plus framed
    object views; a `Channel` cap's `chan_wf` rides `lemma_chan_wf_emptiness_frame`
    (emptiness-only, which cap-equality gives). This is the mirror of
    `lemma_map_frame_caps_consistent`.
  - `end_caps_sound`: `end_cap_count` is a `cap_chan_end` filter — identical.
- **Upward acyclic walk termination** (the `derive` guard's ancestor walk): `descend_to_leaf`
  walks *down* `first_child` with `decreases rank[leaf]` (child rank < parent). Walking *up*
  `parent`, the rank *increases*, so `rank[cur]` is not a measure. The clean idiom is a **ghost
  visited-set** with `decreases slot_view().dom().difference(visited).len()`, discharged by
  `vstd::set_lib`'s purpose-built **`Set::lemma_set_insert_diff_decreases`** (requires
  `dom.contains(cur)`, `!visited.contains(cur)`, `dom.finite()`). Distinctness of `cur` from
  `visited` follows from the rank invariant (every visited node ranks strictly below `cur`).
  This avoids the "finite nat-image has a max" detour the rank-bound alternative would need.

---

## 4. Verified-surface accounting

`374 → 381` (+7): `lemma_set_revoking_frames`, `lemma_revoke_step_death_provenance`,
`revoke_step`, `ancestor_or_self_revoking`, plus the re-verified `derive`/`slot_move`/`cdt_unlink`
deltas net to the recorded total. The four `external_body` seams and all `assume_specification`s
are unchanged save the one extended `CapSlot::empty` spec line. The trusted base did **not**
widen: the marker rides the already-framed `slot_view`, with no new Store seam (the rejected
`revoking_view` alternative).

---

## 5. What remains (out of scope for B9A)

- **B9B** — the `EAGAIN` syscall surface: `ERR_AGAIN` in both errno blocks, the `CapRevoke`
  handler calling `revoke_step(.., REVOKE_QUANTUM)` and mapping `More → EAGAIN`, the
  `CapCopy`/`CapMint` handlers mapping the new `derive` refusal, the userspace `cap_revoke_all`
  loop, and the one shell caller. No `exceptions.rs` change (honesty note 2). `revoke` can be
  retired there.
- **B9C** — the TLA `CapRevocation` atomic→stepwise conversion (`RevokeBegin`/`RevokeStep`/
  `RevokeEnd` + the `revoking` variable + the `Copy` guard + `EventuallyRevoked` liveness + the
  two committed negative controls), the one clarifying rev1§2.2/§2.7 sentence, the §6.1
  mechanized-status note, and the ledger scope-paragraph + `CapRevocation` Baselines update.

The cross-call liveness (a started revoke *eventually* completes under the guard) is **not** a
Verus property — `revoke_step` carries per-call termination + per-step safety only; completion
across restarts is `EventuallyRevoked` in B9C's TLA model.
