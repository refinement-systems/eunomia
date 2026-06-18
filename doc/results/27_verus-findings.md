# Verus findings 7 — Phase 3b: the channel ghost-view enabling refactor

Plan: `doc/plans/3_verus-rewrite.md` (§4.3 channel) and its detailed
decomposition `doc/plans/3_verus-rewrite_phase3-detail.md` (§3b). Prior
increments: `21`…`25` (phase 2 — the cspace/CDT core) and `26` (§3a — untyped
`retype_check`/`reset`). This is the **second** of phase 3's five sub-phases: the
*foundational, proof-light* refactor that extends the `Store`/`ExStore` seam with
a ghost channel view so 3c (`retype_install`'s channel arm), 3d (`send`/`recv`
FIFO), and 3e (`endpoint_cap_dropped`/`bind`) build on a settled representation.
Its risk was in the design of `ChanView` and the ring-cap ↔ `slot_view` coupling,
not solver time — by intent (detail §2-3b).

**Outcome.** `cargo verus verify -p kcore`: **60 verified, 0 errors** (was 57 —
`+end_idx`, `+fire`, `+endpoint_cap_added`; `signal` is now trusted
`external_body`, which is not *counted* as verified). `cargo test -p kcore`: **17
passed** (was 15 — `+signal_frame`, `+chan_wf_exec_has_teeth`). The aarch64
`kernel` cross-build is unchanged (ghost code erases; the `chan_*` contracts and
`chan_view` are spec-only, so the production `KernelStore` needs no change). One
new `external_body` boundary (`notification::signal`), host-test-checked. No new
SMT-heavy lemmas — the proof obligations are straight-line frames over the seam.

---

## 1. What closed

- **The `chan_view` ghost seam.** `ExStore` (cspace.rs) gains a third view
  `spec fn chan_view(&self) -> Map<ObjId, ChanView>` alongside `slot_view`/
  `refs_view`, with `requires/ensures` on every `chan_*` accessor relating it to
  `chan_view` exactly as `slot`/`set_slot` relate to `slot_view`. `ChanView`
  mirrors a `Channel`'s mutable state — `depth`, `end_caps`/`head`/`count`
  (length-2 `Seq`s), `bindings`, `msg_len`, and `ring_cap` — with **payload bytes
  abstracted out** (length + cap identity + order only). The load-bearing
  decision (detail §1.1): a ring message slot is a **real `CapSlot` in the single
  `slot_view` arena**, so the cap *contents* live in `slot_view` and `ring_cap`
  holds only the immutable slot *handles* (`Store` has a `chan_ring_cap` getter
  and no setter); `chan_ring_cap` is a deterministic projection of `chan_view`.
- **The view cross-framing.** `set_slot`/`set_obj_refs` gained
  `ensures final.chan_view() == old.chan_view()`, and every channel setter frames
  `slot_view`/`refs_view` unchanged. Purely additive (an extra `ensures` on a
  callee only adds facts), so the phase-2 + 3a proofs stayed green untouched —
  this is the mutual frame that lets 3d reason about `slot_move`'s arena edits
  without re-establishing channel structure.
- **`spec fn chan_wf`** — the §4.3 well-formedness: `depth > 0`; the FIFO cursors
  in range (`count[r] ≤ depth`, `head[r] < depth`); `ring_cap`/`msg_len`/
  `bindings` have their expected domains; every ring cap handle lives in the
  arena; and the **coupling** — a ring cap outside the live window
  `[head, head+count) mod depth` (the `in_live_window` existential) is empty in
  `slot_view`. No op *proves* `chan_wf` preserved in 3b (by design); it is
  defined for 3c–3e and exercised by `chan_wf_exec`.
- **The assumed `signal` contract** — `notification::signal` is now
  `external_body` with `ensures slot_view`/`chan_view` unchanged (nothing about
  `refs_view`/notif/TCB/scheduler). The single frame the §4.3 channel ops need;
  `external_body` because the body touches unported notification/TCB/scheduler
  state (its body proof is phase 4). The first new trusted boundary phase 3 adds.
- **`fire`** — verified against the assumed `signal`: reads a binding (a getter)
  and conditionally signals, so it leaves `slot_view`/`chan_view` unchanged.
- **`endpoint_cap_added`** — verified: bumps `end_caps[end]` by one, framing the
  other two views and every other channel field; the `requires` on the count
  discharges the `+ 1` (no `u32` wrap). `end_idx` gained a spec (`end_idx_spec`,
  A → 0 / B → 1) so the contracts can name the index a `ChanEnd` selects.

`signal`'s frame is **host-test-checked** against its real body: `signal_frame`
runs the real `signal` on a well-formed channel + a notification (both the
accumulate and the one-waiter delivery path) and asserts the arena (`fingerprint`)
and the channel state (`chans`) are unchanged, *while* the intended effects
(word accumulated / delivered + cleared, waiter dequeued, queued ref released)
still happen — so the frame is real, not a no-op. `chan_wf_exec` is the executable
mirror of `chan_wf`, with `chan_wf_exec_has_teeth` rejecting each single-clause
violation (incl. the windowing coupling). This is the `delete`/`test_store`
discipline applied to the new boundary.

### 1.1 `chan_wf` takes both views — a deliberate deviation from the detail text

The detail plan (§3b) writes `chan_wf(cv, ch)`, but its own clause list includes
"ring slots outside the live window are empty (their `SlotId` empty in
`slot_view`)" — which needs the arena. Resolved the faithful way (mirroring doc 26
§1.1's `reset` resolution): the signature is **`chan_wf(cv, sv, ch)`**. The
coupling between the channel cursors (`chan_view`) and the ring-cap occupancy
(`slot_view`) is the whole point of the predicate, so it inherently spans both
maps; the shorthand `(cv, ch)` could not express it.

---

## 2. Verus mechanics worth keeping (the channel-module port template)

Building on doc 26 §2 (the cross-module spec-import idiom — full-path spec fns in
`ensures`, `#[allow(unused_imports)] use crate::cspace::{ChanView, StoreSpec}`,
which 3b reused verbatim for `channel.rs`/`notification.rs`):

1. **An `exists` quantifier needs a manual trigger when its only term is
   arithmetic.** `in_live_window`'s `∃ j: i == (head[ring]+j) % depth` failed
   trigger inference ("Could not automatically infer triggers"); annotating
   `#![trigger (c.head[ring] + j) % (c.depth as int)]` on the binder fixed it.
   (The modular term *is* the trigger — the doc-25 §2 "quarantine `%`" discipline,
   here at the predicate's definition.)

2. **Spec struct-update syntax works for one-field setters.** The `set_chan_*`
   `ensures` express the single-key update as
   `chan_view().insert(ch, ChanView { count: …update(r, v), ..old[ch] })` —
   functional record update in spec, with `Seq::update` for the length-2 cursors
   and `Map::insert` for the keyed `bindings`/`msg_len`. Clean and reusable for 3d.

3. **`when_used_as_spec` needs matching return types; drop it when the exec fn is
   only called in exec.** `end_idx` returns `usize` but `end_idx_spec` returns
   `int`, so `#[verifier::when_used_as_spec(end_idx_spec)]` would mismatch — and
   it is unneeded, since `end_idx` is only called in exec (`endpoint_cap_added`'s
   body) and the proof uses its `ensures r as int == end_idx_spec(e)`. (Contrast
   `ObjType::align` in untyped.rs, where the types matched and `when_used_as_spec`
   was right.)

4. **`external_body` carries no body obligation, so `signal` needs no `requires`.**
   The body calls `obj_refs(n)`/etc. (which would demand `refs_view` contains `n`)
   and `make_runnable`, but `external_body` skips body verification entirely — so
   `fire` can call `signal` with only a binding in hand, no notification-liveness
   fact. The contract's `ensures` is all that crosses the boundary.

---

## 3. Scope held (what 3b did *not* touch)

- **`send`/`recv` are 3d** — the FIFO core (the ring-window ↔ arena coupling, the
  two-pass `recv` atomicity, null-slot tolerance). The phase-2 verified ops call
  no `chan_*` accessor, so they stayed green; `chan_wf` and `ChanView` are defined
  but unexercised by any op proof here.
- **`endpoint_cap_dropped`/`bind` are 3e**; **`destroy_channel` stays
  `external_body`** (the cross-object teardown, phases 4–5). All remain plain Rust.
- **Two `chan_wf` clauses deferred to 3d**: ring-cap **injectivity** (within the
  channel) and **cross-channel ring-slot disjointness**. Neither is part of the
  *shape* (the `ChanView`/coupling representation is settled), and both add
  quantifier weight `send`/`recv` will pay for only when they need it — so they
  are added as extra `chan_wf` clauses in 3d, not now.
- **No `CLAUDE.md` / spec edits** — the phase-3 closeout (moving the channel +
  untyped ops onto the proven list, recording the `signal`/`destroy_channel`
  residue) lands in **3e** per the detail plan; 3a–3d only seed their findings docs.
- Payload bytes stay abstracted; `chan_msg_write`/`chan_msg_read` are frame-only.
