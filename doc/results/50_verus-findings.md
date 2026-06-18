# Verus findings 30 — Phase 6d body removal: `delete`'s body proven, the cspace teardown cycle closed

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6d"). Prior increments: `44` (6d foundation — `caps_consistent`), `45` (frames), `46`
(the `end_caps` census), `47` (preservation chain), `48` (the census-delta lockstep +
off-by-one), `49` (the teardown-frame foundations + `delete` body 90%). This is the
**`external_body` removal proper** the foundations 44–49 set up: `cspace::delete`'s
`#[verifier::external_body]` is **gone** and its real teardown body is proven, closing the
visible cross-object cycle `delete → obj_unref → destroy_cspace → delete` under the shared
lexicographic measure.

**Outcome — the recorded fallback (plan "attempt full, fall back, record").** `delete`'s body
lands fully; `channel::destroy_channel` and `thread::destroy_tcb` **keep** their (now
`cspace_view`-bearing) `external_body` contracts. Their bodies need *new* census invariants
beyond the four 44–49 built (§3) — disproportionate to bundle here — so the **cspace-only
cycle** is closed and the channel/thread destructor bodies are the recorded residue (6d-final).
`delete`'s body was needed either way (its Channel/Frame branches fire regardless of which
destructors stay opaque), so this is the load-bearing half of the removal.

`cargo verus verify -p kcore`: **273 verified, 0 errors** (was 265 after doc 49). `cargo test
-p kcore`: **81 passed**. The aarch64 `kernel` cross-build is unchanged (every change is ghost
or a doc comment).

---

## 1. What landed

- **`delete`'s `external_body` removed; the body is proven.** The real teardown —
  `delete_prepare` (`cdt_unlink` + slot clear) → per-end channel `peer_closed`
  (`endpoint_cap_dropped`) → mapped-frame unmap (`aspace_unmap` + `unref_aspace`) →
  `obj_unref` — verifies against the full contract: `cspace_wf`, the four system invariants
  (`refcount_sound`/`caps_consistent`/`end_caps_sound`/`census_dom_complete`), the strict
  `count_nonempty` drop, `only_empties`, the `cspace_view` residency frame, and the §4d
  conditional-on-notification frame. The shared lexicographic
  `decreases (count_nonempty(slot_view), height)` (`delete = 0`) is accepted across the
  visible `delete → obj_unref → destroy_cspace → delete` cycle (doc 44 §3 mechanism, now real).

- **`delete_prepare` — the slot-clear half, split out (doc 25 §2 decomposition).** A
  non-recursive helper carrying `delete`'s heaviest single query: the `cdt_unlink` + `set_slot`
  clear and its census/`end_caps`/`caps_consistent` **off-by-one** proof, with a rich contract
  (the deleted cap; the off-by-one census/end at its object/aspace/end; every object view +
  every *other* slot's cap framed; `cspace_wf`/`caps_consistent`/`census_dom_complete`
  preserved; the strict count drop). Extracting it is what brought `delete`'s body under the
  solver rlimit (the 90%-but-too-heavy state of doc 49).

- **Two new structural lemmas.**
  - `lemma_clear_slot_obj_census` — the **full `obj_census` drop for a slot clear**, isolated
    as a per-`x` query in **additive form** (`census(old) == census(new) + δ`, no `nat`
    underflow): the slot/frame-map terms drop via `lemma_clear_slot_census` + the
    caps-preserved lemmas, the four view terms are framed, and a cap designates *either* an
    object *or* a frame aspace (never both), so the census loses exactly one unit. The
    positivity side-conditions (`slot_refs(sv_mid, x) ≥ 1` when the deleted cap designates `x`)
    are discharged inside the lemma, keeping its caller's forall context-light.
  - `lemma_clear_detached_preserves_cspace_wf` — clearing an **already-detached** slot (all
    four CDT links null) to an empty cap preserves `cspace_wf`. The non-empty→empty direction
    `lemma_local_cap_edit_preserves_cspace_wf` forbids (it could strand a child) is safe here
    *because* `cdt_unlink` left the slot isolated: the link clauses read identical (null)
    links, and `empty_slots_detached` holds since the new cap is detached.

- **`cspace_view` (residency) threaded through the teardown chain.** `obj_unref` now states an
  **unconditional** `cspace_view` frame (proven: every arm — `dec_ref`/`unref_aspace` and each
  at-zero destructor — frames it; a destroyed cspace keeps its residency map, its residents
  emptied not re-homed). To prove it, `fire`/`endpoint_cap_dropped`/`destroy_cspace`/
  `unref_cspace` gained the `cspace_view` frame (verified), and `destroy_channel`/`destroy_tcb`
  gained it as an assumed clause (host-checked, `check_destroy_*`). This is what lets `delete`'s
  body discharge its own residency-frame `ensures`.

- **`obj_unref`'s `cap_notif`-conditional view frame.** Deleting a **notification** cap leaves
  every object view (and every slot's cap) untouched — `dec_ref` drops only `refs[n]`, and at
  zero `destroy_notif` is a model view no-op. `obj_unref` now states this for the Notification
  arm (proven); `delete`'s §4d conditional frame reads it off (the `thread::bind` enabling
  clause that 49 had carried only as an assumed `delete` contract).

---

## 2. Findings worth keeping

- **Additive census beats subtractive at the slot clear.** Doc 49 wrote the census drop as
  `census(new) == (census(old) - δ) as nat`. That form forces the SMT solver to re-prove
  `census(old) ≥ δ` (no `nat` underflow) when recombining the six terms — and it *silently
  rlimited* in the inline body, masking that the recombination was never actually closing.
  Decomposing into `delete_prepare` turned the rlimit into a real assertion failure, and
  switching the lemma to **additive** (`census(old) == census(new) + δ`) made the recombination
  underflow-free; the downstream off-by-one / dom-completeness proofs read it through explicit
  `census(old, ·)` triggers. The lesson: an rlimit on a census equation can hide an
  underflow-shaped gap — decompose first, then prefer the additive shape.

- **The detached-slot clear needed its own `cspace_wf` lemma.** `lemma_local_cap_edit`
  (reused by `retype_install`) bars non-empty→empty precisely to avoid stranding a child; the
  teardown's clear *is* non-empty→empty, but only ever on a `cdt_unlink`-isolated slot, so a
  narrower lemma keyed on "already detached" discharges it. A general lemma's safe-direction
  guard is not always the guard the specific call needs.

- **Residency (`cspace_view`) is a teardown-wide frame, not a local one.** `delete`'s contract
  *claimed* `cspace_view` unchanged while `external_body`; proving it forced the frame all the
  way down `obj_unref` and every destructor (including the still-opaque ones, as host-checked
  clauses). A "the kernel never changes this" frame that crosses an opaque boundary has to be
  stated on that boundary's contract to survive into the verified caller.

---

## 3. The residue: `destroy_channel` / `destroy_tcb` bodies (6d-final)

The full cross-module SCC (the channel/thread destructor bodies) is **not** closed here. Each
needs a *new* system invariant beyond the four 44–49 built — the reason it is disproportionate
to bundle with `delete`'s body:

- **`destroy_tcb`** sets `state = Halted` / `qnext = None` and (reordered: clear the hold
  *before* the unref) releases `cspace`/`aspace`. Census preservation across the state/qnext
  writes needs `waiter_refs` to frame — `lemma_waiter_refs_frame` exists but is keyed on
  `wait_notif != Some(o)`, which a Runnable thread with a **stale `wait_notif`** can violate
  even though it is not on any chain (`waiter_chain` requires `state == BlockedNotif`). So
  `destroy_tcb`'s body needs a **thread-state/waiter-coherence invariant** (a non-`BlockedNotif`
  thread is on no waiter chain), threaded teardown-wide like `end_caps_sound` (doc 46) — a
  fifth incrementally-discovered foundation. It also needs an `unqueue_ready` Store contract
  (the ready list is scheduler state below `tcb_view`, so it frames every object view — total,
  host-checked; the "contract waits for 4e" note in the seam).
- **`destroy_channel`** runs the ring-cap delete loops + the per-binding `refs -= 1` release
  loop. The binding-release census reasoning (each `-1` matched by a `binding_refs` drop, over
  arbitrary ring-cap kinds, with the SCC `decreases` height 1 and loop invariants carrying the
  four invariants + the count non-increase) is the channel analog of `destroy_tcb`'s.

Both keep their assumed-but-host-checked `external_body` contracts (now also stating the
`cspace_view` frame), checked against their real bodies in `test_store.rs`
(`check_destroy_channel`/`check_destroy_tcb`, both strengthened with the residency assertion
this increment). The SCC height directions (doc 44 §3) and the cspace cycle's measure are
validated; closing the two destructor bodies is the remaining 6d work.

The §6 spec-table goal "kcore carries zero `external_body`" is therefore **not yet met** — the
two channel/thread destructors remain (plus the pre-existing `untyped.rs` helpers, out of 6d
scope). `delete`'s removal is the largest single step toward it.

---

## 4. Doc / CLAUDE.md

No `CLAUDE.md`/spec edit this increment (the doc-30 §3 convention — the sub-phase closeout edit
rides 6f). 6d's body removal is now docs 46–50; the channel/thread destructor bodies are the
recorded 6d-final residue. `cargo verus verify -p kcore` runs with no per-proof filter, so the
new `delete` body, `delete_prepare`, and the two structural lemmas auto-gate; `host-tests` runs
the strengthened `check_destroy_*` residency assertions.
