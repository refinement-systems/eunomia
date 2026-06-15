# Phase 3 detail: untyped `retype`/`reset` remainder + channel (Verus rewrite)

**Status:** proposed. Detailed, step-by-step decomposition of **phase 3** of
`doc/plans/3_verus-rewrite.md` (§4.2 remainder + §4.3 + §7 step 3), written *before*
any code so the implementation does not repeat phase 2's mid-flight splits.

**Baselines:** `3_verus-rewrite.md` (§4.2 untyped, §4.3 channel, §7 phasing);
`doc/results/21…25_verus-findings.md` (the phase-2 increments this inherits the
mechanics from); the current `kcore` source as of `main` `2bf3301` (phase-2e merge).

---

## 0. Purpose, and the phase-2 lesson it acts on

`3_verus-rewrite.md` §7 gives phase 3 a single line: *"untyped retype + reset (§4.2
remainder), channel (§4.3)."* Phase 2's entry was just as terse — *"cspace / CDT
(§4.1)"* — and it became a five-PR effort (2 → 2b termination → 2c `cspace_wf`
strengthening → 2d `slot_move` body → 2e `cdt_unlink` body), because three things were
discovered mid-implementation rather than planned:

1. the structural `cdt_wf` was too weak and had to gain reachability anchors and a
   second (sibling) acyclicity rank (`doc/results/22`);
2. the looping-op bodies needed bespoke ghost machinery — a transposition renaming for
   `slot_move` (`doc/results/24`), a sibling-list *merge* with a rescaled rank witness
   for `cdt_unlink` (`doc/results/25`);
3. the cross-object teardown (`delete`'s body) is entangled with object destructors
   that are not yet ported, so it could not close and was left `external_body`.

This document front-loads the equivalent discovery for phase 3. The two cross-module
couplings, the one trait extension, and the one deliberate scope-out are all decided
here, so each sub-phase PR is a known quantity going in.

### Discipline carried from phase 2 (applies to every sub-phase below)

- **One PR per sub-phase.** It merges only when green.
- **`cargo verus verify -p kcore` green before merge** — the CI `verus` job runs with no
  per-proof filter, so a new `verus!{}` obligation auto-gates (`3_verus-rewrite.md` §8).
- **`cargo test -p kcore` green** — the `test_store` differential harness is the
  executable check of every `external_body` contract against its real body.
- **The aarch64 `kernel` cross-build is unaffected** — `verus!{}` erases ghost code, so
  the erased `exec` body is byte-identical to today's plain Rust (`3_verus-rewrite.md`
  §6). Confirm with a `cd kernel && cargo build` per sub-phase.
- **A `doc/results/N_verus-findings.md` increment per sub-phase**, recording what
  closed and the Verus-mechanics findings worth keeping (the doc-21…25 cadence).

---

## 1. Dependency analysis — why this order

Phase 3 touches `untyped.rs` (3 fns), `channel.rs` (7 fns), `store.rs` (the trait),
`cspace.rs` (the `ExStore` spec), and `test_store.rs`. Two couplings dictate the order.

### 1.1 Channel ↔ the CDT arena (reuse, no new trusted boundary)

A channel ring message slot is a **real `CapSlot` in the single `slot_view` arena**
(`store.rs` doc: a `CapSlot` is touched only via `Store::slot`/`set_slot`, however it is
homed — cspace resident, channel ring cap, or TCB bind slot). `send`/`recv` move queued
caps with the **already-verified `slot_move`** (`channel.rs:175,223`). So the FIFO proof
*reuses* `slot_move`'s `slot_view` contract — but it must add a **coupling invariant**
tying the channel's ghost `head`/`count` to which ring `SlotId`s are non-empty in
`slot_view`. That invariant is this phase's new spec work; it is the §4.3 analog of the
`cdt_unlink` merge machinery and lives in sub-phase 3d.

### 1.2 Channel ↔ notification (one minimal assumed contract)

`send`, `recv`, `endpoint_cap_dropped`, and `fire` all call `notification::signal`
(`channel.rs:114,122,179,233`), which is phase-4 code (TCB waiter queue, `make_runnable`,
`obj_refs`). To verify the channel ops in phase 3 without porting notification, introduce
a **minimal assumed `signal` contract** — `notification::signal` becomes
`#[verifier::external_body]`, host-test-checked, exactly the discipline that lets
`revoke` be verified against `delete` today. The *only* promise phase 3 needs from it:

> `signal` leaves `slot_view` and `chan_view` unchanged.

It may arbitrarily perturb `refs_view`/notification/TCB/scheduler state — phase 3 asserts
nothing about those. That minimal frame is enough for `fire`/`send`/`recv`/
`endpoint_cap_dropped` to preserve `cdt_wf`/`chan_wf` and the FIFO. `signal`'s **body
proof lands in phase 4.** This is the single new trusted boundary phase 3 adds.

### 1.3 Cross-object teardown is OUT of phase 3 (the deliberate scope-out)

`destroy_channel` (`channel.rs:240`) recurses through `cspace::delete` (still
`external_body`) and releases binding refs; `delete → obj_unref → destroy_{cspace,
channel,tcb,notif,timer} → delete` is a mutual recursion (`cspace.rs:208–257`,
`channel.rs:240`, `thread.rs:173`, etc.). It cannot close until notif/thread/timer
destructors are ported (phases 4–5) **and** a recursion measure (the seL4-zombie
measure) + the full `refcount_sound` invariant exist. Phase 3 therefore keeps
`destroy_channel` `external_body` with a host-test-checked contract — *scoped here, not
discovered later.* `obj_unref`/`destroy_cspace`/`unref_aspace`/`unref_cspace` stay plain
Rust through phase 3.

### 1.4 Resulting order

```
3a  untyped retype_check + reset      (slot_view only — clean win, banks the workflow)
3b  chan_view trait extension         (enabling refactor: ExStore + chan_wf + assumed signal)
3c  untyped retype_install            (needs chan_view for the channel-endpoint clause)
3d  channel send / recv               (the FIFO core — the hard sub-phase)
3e  endpoint_cap_dropped + bind,      (notification-coupled edges + closeout)
    destroy_channel deferral, docs
```

`endpoint_cap_added` is trivial (a `chan_end_caps` bump, `channel.rs:103`) and is
verified in **3c**, because `retype_install` calls it; the notification-coupled
`endpoint_cap_dropped` waits for 3e.

---

## 2. The sub-phases

Each carries: scope · specs/contracts landed · key lemmas/risks · `test_store` additions
· the "done =" gate.

### 3a — Untyped `retype_check` + `reset` (the confidence-builder)

The carve/phase-0 analog: pure `slot_view` reasoning, no channel or notification
coupling. First PR to re-bank the workflow on a clean module.

- **Scope.** Move `retype_check` (`untyped.rs:93`) and `reset` (`untyped.rs:400`) into
  `verus!{}` with contracts. Both already read/write only via `store.slot`/`set_slot`.
- **`retype_check` contract.**
  - `ensures` error precedence: `NotUntyped` when `ut_slot` is not `Untyped`; else
    `DestOccupied` when `dst` (or, for `Channel`, `dst2`) is non-empty/aliased; the
    store is unchanged on every path (it is read-only).
  - on `Ok((base,size,watermark))`: the returned triple equals the untyped's geometry;
    `dst` is empty; for `Channel`, `dst2` is `Some(d2)`, `d2 != dst`, `d2` empty.
- **`reset` contract.**
  - `requires` `ut_slot` is `Untyped`.
  - `ensures` on `Ok`: `first_child is None` held on entry and the watermark is now 0,
    every other field of `ut_slot` and all other slots unchanged; on `Err(BadArg)`:
    `first_child` was `Some` and the store is unchanged; `Err(NotUntyped)` when not
    untyped.
- **Mechanics.** Needs spec accessors for `CapKind::Untyped { base, size, watermark }`
  (a `matches`/projection in spec) and for `first_child`. No new lemmas.
- **`test_store`.** `check_retype_check` (each error/Ok arm) and `check_reset` (the
  children-present refusal + the zeroing) on `ArrayStore`; `Untyped`/`Channel` caps are
  already representable. Wire them into the existing `#[test]` set.
- **Done =** verus green + `cargo test -p kcore` + `cd kernel && cargo build`.

### 3b — The channel ghost-view enabling refactor (foundational, proof-light)

The phase-1-arena-rewrite analog: a wide but shallow change that *enables* 3c–3e and
lands almost no new op proof, so its risk is in the design, not the SMT.

- **Extend the `Store`/`ExStore` seam with channel state.**
  - Add to `ExStore` (cspace.rs) a ghost view `spec fn chan_view(&self) -> Map<ObjId,
    ChanView>`, where `ChanView` mirrors `Channel`'s mutable state: `depth: nat`,
    `end_caps: [nat;2]`, `head: [nat;2]`, `count: [nat;2]`, `bindings`, and the ring as
    a `Seq`/`Map` of `(len, cap: SlotId)` per `(ring, index)` — **payload bytes
    abstracted out** (model length + cap identity + order, not 256 bytes/message).
  - Attach `requires/ensures` to every `chan_*` accessor — `chan_depth`, `chan_end_caps`
    /`set_…`, `chan_head`/`set_…`, `chan_count`/`set_…`, `chan_binding`/`set_…`,
    `chan_ring_cap`, `chan_msg_len`/`set_…`, `chan_msg_write`/`chan_msg_read` — relating
    each to `chan_view`, the way `slot`/`set_slot` relate to `slot_view`
    (cspace.rs:348–367). The setters frame the *other* views unchanged
    (`set_slot` already ensures `refs_view` unchanged; the chan setters ensure
    `slot_view`/`refs_view` unchanged and `chan_view` updated at one key).
  - **Decide the ring-cap ↔ `slot_view` coupling here**: `chan_ring_cap(ch,ring,i,c) ->
    SlotId` is the bridge; the live-window cap slots are exactly the `SlotId`s present
    in `slot_view`. Fix the representation (a deterministic spec map from `(ch,ring,i,c)`
    to `SlotId`, plus the windowing predicate) so 3d builds on a settled shape.
- **`spec fn chan_wf(cv: Map<ObjId,ChanView>, ch: ObjId) -> bool`**: `depth > 0`;
  `count[r] ≤ depth`; `head[r] < depth`; ring slots **outside** the live window
  `[head, head+count) mod depth` are empty (their `SlotId` empty in `slot_view`);
  `end_caps[r]` within sane bounds. (The §4.3 `chan_wf`.)
- **The assumed `signal` contract.** Make `notification::signal` `external_body` with
  `ensures final.slot_view() == old.slot_view()` and `final.chan_view() ==
  old.chan_view()` (and nothing about `refs_view`/notif/TCB). Verify `fire`
  (`channel.rs:118`) against it — `fire` reads a binding and conditionally calls
  `signal`, so its `ensures` is the same frame (`slot_view`/`chan_view` unchanged).
- **Verify `endpoint_cap_added`** (`channel.rs:103`): bumps `chan_end_caps[end]` by one;
  `ensures` `chan_view` updated at that one field, `slot_view`/`refs_view` unchanged.
- **`test_store`.** Give `ArrayStore` real channel state (a `Map<ObjId, ChanState>`
  backing the `chan_*` methods, replacing the stubs at `test_store.rs:77–116`); add
  `chan_wf_exec` and `check_signal_frame` (the executable check that the real `signal`
  body keeps `slot_view`/`chan_view` fixed — the `delete`-contract discipline).
- **No `send`/`recv` proofs yet.** The phase-2 verified ops (derive, slot_move, …) do not
  call `chan_*` accessors, so they stay green unchanged.
- **Done =** verus green + test + cross-build. (Risk is design review of `ChanView` and
  the coupling, not solver time — keep this PR's diff structural.)

### 3c — Untyped `retype_install` (the §2.5 rights-inheritance theorem)

- **Scope.** Move `retype_install` (`untyped.rs:338`) into `verus!{}`. It is infallible
  (all checks passed in `retype_check`); reuses verified `cdt_insert_child` and (3b)
  `endpoint_cap_added`.
- **Contract.**
  - watermark advanced to `end - base`;
  - **rights inheritance table as theorems** (`untyped.rs:365–370`): `Frame` inherits
    the untyped's rights; `Thread → THREAD_ALL`; **sub-`Untyped` masked to
    `READ|WRITE`, provably never `PHYS`** — the §2.5 "phys stays off ordinary derivation
    chains by construction" claim, now ∀ rather than asserted; all other kinds `ALL`;
  - `dst` holds `Cap { kind, rights }` and is a CDT child of `ut_slot` (read off
    `cdt_insert_child`'s `ensures`); `cspace_wf` preserved;
  - `obj_refs` of the new object bumped by one (non-channel), and the **channel arm**:
    `dst2` installed as endpoint B, `ch` refs == 2, both ends' `end_caps` bumped
    (verified `endpoint_cap_added` + `chan_view` from 3b).
- **Mechanics.** The `Rights::masked` spec (`cspace.rs:400`) already states `out.0 ==
  r.0 & mask`, so `READ|WRITE`-masked rights have `PHYS` clear by bit reasoning — the
  load-bearing step. The channel two-endpoint dance threads two `cdt_insert_child` calls
  and two `endpoint_cap_added`s; frame the intermediate `chan_view`/`refs_view` as in the
  phase-2 body proofs.
- **`test_store`.** `check_retype_install` across the Frame / Thread / Untyped / Channel
  arms (assert the rights table and, for Channel, refs == 2 and both `end_caps`).
- **Done =** verus green + test + cross-build.

### 3d — Channel `send` / `recv` (the FIFO core — budget the most time)

The §4.3 centerpiece and this phase's hardest sub-phase; the analog of `cdt_unlink`'s
merge. Isolate it so the difficulty is contained.

- **Ghost FIFO model.** A `spec fn` projecting each ring of a `ChanView` to a `Seq` of
  `(len, cap)` from the live window `[head, head+count) mod depth`. `send` appends,
  `recv` pops the head; the proofs relate the imperative `head`/`count`/index arithmetic
  to `Seq::push`/`Seq::drop_first`.
- **`send` contract** (`channel.rs:153`).
  - guards: `PeerClosed` when the opposite end has no caps; `Full` at `count == depth`;
  - on `Ok`: message enqueued at `(head+count) % depth`; caps moved out of the sender's
    slots into the ring cap slots via verified `slot_move` (a cross-home move within the
    one `slot_view`); the readable event fired (3b `fire`, so `slot_view`/`chan_view`
    of *other* channels untouched);
  - `ensures` `chan_wf` preserved; the ring's FIFO `Seq` gains the new message **at the
    tail, in order**; indices in bounds; **caps left the sender's slots exactly on
    `Ok`** (move totality) and are **untouched on `Full`/`PeerClosed`**.
- **`recv` contract** (`channel.rs:191`).
  - guard: `Empty` at `count == 0`;
  - **two-pass atomicity**: pass 1 checks every arriving cap has a free, empty
    destination, else `NoCapSlot` with the message left **fully queued** (no partial cap
    install, payload intact); pass 2 moves caps and dequeues;
  - **null-slot tolerance**: a ring cap emptied in flight by revocation is delivered as
    absent (its mask bit clear), never a panic (`channel.rs:204–226`, the §3.4 null-slot
    semantics);
  - `ensures` FIFO **dequeue order** (head consumed); payload length + cap identity
    delivered in order; writable fired.
- **The crux.** The `chan_view(head,count)` ↔ `slot_view`(ring-cap occupancy) coupling
  invariant from 3b — proving the moved caps stay inside the window and the window math
  (`%depth`) stays in bounds. Expect bespoke lemmas; **quarantine any non-linear modular
  arithmetic into one-line helpers** (the doc-25 §2 finding: Z3 is reliable once
  multiplication/mod is isolated). The `slot_move` reuse means no new CDT lemmas — the
  work is the windowing.
- **`test_store`.** FIFO order across wraparound; full/empty boundaries; `NoCapSlot`
  atomicity (message survives a failed recv); null-slot tolerance (empty a ring cap, then
  recv); a randomized send/recv sweep (the `randomized_sweep` cadence,
  `test_store.rs:573`).
- **Done =** verus green + test + cross-build.

### 3e — Event/binding edges + `destroy_channel` deferral + closeout

- **`endpoint_cap_dropped`** (`channel.rs:110`): decrement `chan_end_caps[end]`; when it
  hits zero, fire the *other* end's peer-closed event (3b `fire`). `ensures` the
  count decrement and the conditional fire's frame.
- **`bind`** (`channel.rs:127`): the **binding-refcount discipline** — release the old
  notification's ref, add the new one, install the binding. This is the *first*
  installment toward `refcount_sound`'s binding term (the full census lands phases 4–5);
  state it as the per-op `refs_view` delta on the bound notification, `chan_view` updated
  at the one binding, `slot_view` unchanged.
- **`destroy_channel`: keep `external_body`, host-test-checked.** Give it a contract
  stating its CDT/structural effect (ring caps deleted, bindings released), and leave the
  body to the cross-object teardown (phases 4–5) — it recurses through the still
  `external_body` `delete` and releases refs whose soundness needs `refcount_sound`. This
  mirrors exactly how phase 2 left `delete`. `obj_unref`/`destroy_cspace`/`unref_*` stay
  plain Rust.
- **`test_store`.** `check_endpoint_cap_dropped` (peer-closed fires at zero),
  `check_bind` (ref released/added), `check_destroy_channel` (the new external_body
  contract vs. the real body) — the latter needs real channel + notification state in
  `ArrayStore`, already added in 3b.
- **Closeout.**
  - Write `doc/results/26_verus-findings.md` (what closed in 3a–3e; the FIFO-coupling and
    assumed-`signal` mechanics findings).
  - Update `CLAUDE.md`'s Verus section + verification-tier table: move untyped
    `retype_check`/`retype_install`/`reset` and channel `send`/`recv`/`endpoint_cap_*`/
    `bind` to the proven list; record `channel::destroy_channel` and
    `notification::signal` as the new host-test-checked `external_body` residue alongside
    `delete`.
- **Done =** verus green + test + cross-build.

---

## 3. Risks & mitigations (phase-2-informed)

- **The FIFO coupling balloons (3d).** The realistic repeat of the `cdt_unlink`-merge
  surprise. Mitigation: it is its own sub-phase; the `ChanView` shape and the
  ring↔arena coupling are *designed in 3b* before any `send`/`recv` proof; non-linear
  mod arithmetic is quarantined into helpers (doc-25 §2).
- **The trait extension churns every `chan_*` accessor (3b).** Mitigation: an isolated,
  proof-light PR gated by the phase-2 proofs staying green. The production `KernelStore`
  is the trusted boundary, so adding ghost `spec fn` views needs **no production-code
  change** — only the `ExStore` spec and `ArrayStore` (host) gain real bodies.
- **Notification coupling pulls phase-4 work forward.** Mitigation: a *single* minimal
  assumed `signal` frame (`slot_view`/`chan_view` unchanged), host-test-checked — nothing
  else of notification is touched; the body proof stays in phase 4.
- **Scope creep into cross-object teardown.** Mitigation: the `destroy_channel` deferral
  is declared a non-goal in §1.3 and §2-3e, not surfaced mid-PR.
- **Payload modelling cost.** Mitigation: payload bytes are abstracted (length + cap
  identity + order); `chan_msg_write`/`chan_msg_read` get frame-only specs, not a
  byte-level model.

---

## 4. Out of scope (phase-3 non-goals)

- Cross-object `delete` body, `obj_unref`, `destroy_cspace`, `unref_*`, and the **full**
  `refcount_sound` (phases 4–5).
- `notification::signal` **body** proof, and the rest of notification/thread/reports
  (phase 4); timer, aspace+PTE, sysabi (phases 4–5).
- `destroy_channel` body proof (closes with the cross-object teardown).
- Byte-for-byte channel payload modelling.

---

## 5. Exit criterion for phase 3

`cargo verus verify -p kcore` proves untyped `retype_check`/`retype_install`/`reset` and
channel `send`/`recv`/`endpoint_cap_added`/`endpoint_cap_dropped`/`bind`/`fire` against
`cspace_wf` + the new `chan_wf` + the FIFO `Seq` model, with the **§2.5 sub-untyped-never-
PHYS** rights theorem among them; `notification::signal` and `channel::destroy_channel`
are the only new `external_body` ops, both host-test-checked in `test_store`; the aarch64
`kernel` build and `cargo test -p kcore` are green; `doc/results/26` and `CLAUDE.md`
record the new division. The cross-object `delete` body and full `refcount_sound` pass
forward to phases 4–5 unchanged.
