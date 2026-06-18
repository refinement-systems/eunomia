# Phase 4 detail: notification + thread/reports + timer (Verus rewrite)

**Status:** proposed. Detailed, step-by-step decomposition of **phase 4** of
`doc/plans/3_verus-rewrite.md` (§4.4 + §7 step 4), written *before* any code so the
implementation does not repeat phase 2's mid-flight splits — the same treatment
`3_verus-rewrite_phase3-detail.md` gave phase 3, which then landed cleanly across
five sub-phase PRs (docs 26…30).

**Baselines:** `3_verus-rewrite.md` (§4.4 notification + thread/reports, §7 phasing);
`3_verus-rewrite_phase3-detail.md` (the structure this mirrors);
`doc/results/21…25_verus-findings.md` (phase 2 — the cspace/CDT core, the
acyclicity-rank and linked-list-merge mechanics this reuses) and `26…30` (phase 3 —
the ghost-view enabling-refactor and FIFO-core mechanics this is the direct analog
of); the current `kcore` source as of `main` `e03717b` (phase-3e merge).

---

## 0. Purpose, and the phase-2/3 lessons it acts on

`3_verus-rewrite.md` §7 gives phase 4 a single line: *"notification + thread/reports
(§4.4). Delete `proofs/{notification,thread}.rs`."* Phase 2's entry was just as terse
(*"cspace / CDT (§4.1)"*) and became a five-PR scramble because the structural-`wf`
strengthening, the looping-op ghost machinery, and the cross-object-teardown
entanglement were all discovered mid-implementation. Phase 3 front-loaded that
discovery into a detail plan and went smoothly. This document does the same for
phase 4: the new ghost views, the one genuinely new proof model (the intrusive
waiter queue), the assumed-contract boundaries, the timer-folding decision, and the
cross-object scope-out are **all decided here**, so each sub-phase PR is a known
quantity going in.

Two facts about the master-plan line, resolved up front:

- **The "delete `proofs/{notification,thread}.rs`" clause is already discharged.**
  There is no `kcore/src/proofs/` directory — it was deleted wholesale in the
  Kani→Verus migration (CLAUDE.md; plan phase 2 closeout). The "delete the Kani it
  subsumes" discipline is already satisfied for these modules; phase 4 has nothing to
  delete.
- **`kcore::timer` is folded into phase 4.** §7 never assigns the timer module a
  phase (§4.4 names only `kcore::notification` and `kcore::thread`), but timer is
  tightly coupled to notification — `check_expired` calls `notification::signal`, and
  an armed timer holds a refcount on its bound notification (the **armed-timer term**
  of §4.1's `refcount_sound` census). It is ~100 lines. Phase 4 is its natural home;
  this plan resolves the gap by placing it in sub-phase 4e.

### Discipline carried from phases 2–3 (applies to every sub-phase below)

- **One PR per sub-phase.** It merges only when green.
- **`cargo verus verify -p kcore` green before merge** — the CI `verus` job runs with
  no per-proof filter, so a new `verus!{}` obligation auto-gates (`3_verus-rewrite.md`
  §8). Current baseline: **90 verified, 0 errors** (doc 30).
- **`cargo test -p kcore` green** — the `test_store` differential harness is the
  executable check of every `external_body` contract against its real body. Current
  baseline: **26 passed** (doc 30).
- **The aarch64 `kernel` cross-build is unaffected** — `verus!{}` erases ghost code,
  so the erased `exec` body is byte-identical to today's plain Rust
  (`3_verus-rewrite.md` §6). Confirm with `cd kernel && cargo build` per sub-phase.
- **A `doc/results/N_verus-findings.md` increment per sub-phase**, recording what
  closed and the Verus-mechanics findings worth keeping (the doc-26…30 cadence).
  Doc 30 was "findings 10"; phase 4 produces **docs 31–35** ("findings 11–15"), one
  per sub-phase, **numbered in landing order** (the doc-29/30 convention: the file
  number follows landing, not plan order).

---

## 1. Dependency analysis — why this order

Phase 4 touches `notification.rs` (4 fns), `thread.rs` (3 fns), `timer.rs` (4 fns),
`cspace.rs` (the `ExStore` spec — three new ghost views and the scheduler-seam
contracts), and `test_store.rs` (real notif/TCB/timer state behind the
`unimplemented!()` stubs). Three couplings, one trait extension, and one deliberate
scope-out dictate the order.

### 1.1 Everything reads the new views — so there is no phase-3-style "clean win" first

Phase 3 opened with **3a** (`retype_check`/`reset`), a confidence-builder that
touched only `slot_view` — a view-independent island that re-banked the workflow
before the wide refactor. **Phase 4 has no such island.** Every phase-4 op reads the
notification, TCB, or timer state, none of which is in the verified seam yet (the
`ExStore` extension contracts only `slot_view`/`refs_view`/`chan_view`; the
`notif_*`/`tcb_*`/`timer_*` accessors are plain trait methods with no Verus
contract, and `ArrayStore` `unimplemented!()`s half of them). So phase 4 **opens
directly with the enabling refactor** (the 3b analog, here 4a) — there is nothing to
prove until the views exist. This is the one structural difference from phase 3, and
it is stated plainly so 4a's wide diff is expected, not a surprise.

### 1.2 The intrusive waiter queue is the one new proof model

The notification waiter queue is a **singly-linked intrusive list threaded through
the TCBs**: a `NotifObj` holds `wait_head`/`wait_tail`; each waiting TCB holds
`qnext` (next waiter) and `wait_notif` (back-pointer to its notification). This is a
*third* shape, distinct from the two phase-3 settled:

- the **channel ring** (3d) is an array with `head`/`count` cursors — modular-index
  reasoning (four `%`-lemmas, doc 29 §2.4);
- the **CDT sibling list** (2e) is linked with re-parenting — a rank-bounded merge
  (doc 25);
- the **waiter queue** is linked *without* re-parenting, but its model is a
  **list-reachability `Seq`** (`waiter_seq` = follow `qnext` from `wait_head`) — new
  to phase 4.

`wait` pushes the tail (`Seq::push`), `signal` pops the head (`Seq::drop_first`),
`remove_waiter` removes a named element (a `Seq` splice). "Wake order = block order"
(§4.4) is exactly FIFO-ness of `waiter_seq`. Loop termination for `remove_waiter`
(and the well-foundedness of the reachability `Seq`) needs a **queue acyclicity
rank** — the `qnext` analog of phase 2's `sib_acyclic` (`cspace.rs:733`). This model
+ rank is the new spec work and the chief design risk; it is **designed in 4a**
before any `signal`/`wait`/`remove_waiter` proof, exactly as 3b settled the channel
ring↔arena coupling before 3d.

### 1.3 The scheduler/hardware seam needs two minimal assumed contracts

`signal` calls `make_runnable` (`notification.rs:82`) and `destroy_tcb` calls
`unqueue_ready` (`thread.rs:174`) — the kernel-side scheduler hooks folded into the
`Store` seam (`store.rs:115–131`). They have no Verus contract today. To verify
`signal`'s body (4b), `make_runnable` needs a contract; it is the scheduler TCB
boundary, so — like `set_slot` and the assumed `signal` of phase 3 — it is an
**assumed (`external_trait` `ensures`) contract, host-test-checked** against
`ArrayStore`. The only promise `signal` needs: `make_runnable` **frames `slot_view`/
`chan_view`/`refs_view`/`notif_view` unchanged and touches only the woken thread's
`tcb_view` (`state → Runnable`)**. This is the single new trusted boundary 4a adds
(the `signal`-3b discipline, one level on). `unqueue_ready`'s contract is added in 4e
when `destroy_tcb` needs it.

### 1.4 Cross-object teardown and the full `refcount_sound` census are OUT of phase 4

`delete` (`cspace.rs:3536`) is `external_body`; its body recurses
`obj_unref → destroy_{cspace,channel,tcb,notif,timer} → delete` (`cspace.rs:219–268`,
plus the per-type destructors). It cannot close until **all** destructors are ported
**and** aspace's `aspace_destroy`/`aspace_unmap` (phase 5) are ported **and** a
recursion measure (the seL4-zombie measure) **and** the full `refcount_sound`
invariant exist. Phase 4 ports the notification/TCB/timer destructors that *don't*
recurse cross-object (`destroy_notif`, `destroy_timer`), but:

- **`delete` stays `external_body`** (inherited contract, host-test-checked).
- **`obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace` stay plain Rust.**
- **`destroy_tcb`/`destroy_channel` stay `external_body`**, host-test-checked — they
  recurse through `delete`/`unref_*` (the `destroy_channel` deferral, doc 30 §1,
  applied to `destroy_tcb`).
- **Full `refcount_sound` is deferred.** Phase 4 lands only the **per-op refcount
  deltas** — the waiter acquire/release (`wait`/`signal`/`remove_waiter`) and the
  armed-timer acquire/release (`arm`/`disarm`) — the **waiter** and **armed-timer**
  terms of the census, exactly as 3e's `bind` landed the binding term (doc 30 §2.2).

**The master-plan gap, flagged.** §7 lists `delete`/`revoke`/`destroy_cspace`/
`obj_unref` under §4.1 (phase 2) but they were deferred there; §7 then never gives
the cross-object teardown its own phase. After phase 4, every object destructor
except aspace's is ported; after phase 5 (aspace + PTE), the last one is. **This plan
recommends the cross-object teardown — closing `delete`/`obj_unref`/`destroy_cspace`/
`destroy_tcb`/`destroy_channel`'s bodies, the seL4-zombie recursion measure, and the
full `refcount_sound` census — become its own dedicated phase immediately after phase
5**, since it can only be attempted once aspace is ported and it needs all the
destructors and the census together. This is the single biggest correction phase 4's
detail makes to the master plan's phasing.

### 1.5 Resulting order

```
4a  notif/tcb/timer ghost-view refactor   (enabling: views + notif_wf + waiter_seq + make_runnable contract)
4b  signal / wait (+ destroy_notif)        (the waiter-queue FIFO core — the hard sub-phase)
4c  remove_waiter                          (the mid-queue unlink/splice)
4d  report_terminal + thread::bind         (ReportMonotone + FireSafe; the TCB-binding edge)
4e  timer + destroy_tcb deferral + closeout (armed-timer refs; the scope-out; docs)
```

`destroy_notif` (`notification.rs:137`) is trivial (a `refs == 0` ⇒ no-waiters no-op)
and rides with 4b, because the `notif_wf`-no-waiters fact it rests on is settled
there. `destroy_timer` (`timer.rs:98`) is `disarm`, so it rides with 4e.

---

## 2. The sub-phases

Each carries: scope · specs/contracts landed · key lemmas/risks · `test_store`
additions · the "done =" gate.

### 4a — The notification/TCB/timer ghost-view enabling refactor (foundational, proof-light)

The 3b analog: a **wide but shallow** change that *enables* 4b–4e and lands almost no
op proof, so its risk is in the design — the three views and the waiter-queue model —
not the SMT. Keep the diff structural.

- **Extend the `ExStore` seam (cspace.rs) with three new ghost views**, each mirroring
  the `chan_view` pattern (`cspace.rs:400` + the `chan_*` accessor contracts at
  `:430–`):
  - `spec fn notif_view(&self) -> Map<ObjId, NotifView>` with
    `NotifView { word: nat, wait_head: Option<ObjId>, wait_tail: Option<ObjId> }`
    (mirrors `NotifObj`'s mutable fields, `notification.rs:18`).
  - `spec fn tcb_view(&self) -> Map<ObjId, TcbView>` mirroring the TCB *mutable*
    fields the verified ops read/write: `state`, `qnext`, `wait_notif`, `report`,
    `retval`, `cspace`, `aspace`, `bind_bits: Seq<u64>` (len 2), and the two
    `bind_slots` as **`SlotId` handles** — the cap *contents* stay in `slot_view`,
    exactly as channel ring caps do (doc 27 §1), since `tcb_bind_slot` is a getter
    with no setter (`store.rs:93`), an immutable projection.
  - `spec fn timer_view(&self) -> Map<ObjId, TimerView>` with
    `TimerView { armed, deadline, notif: Option<ObjId>, bits, next: Option<ObjId> }`,
    plus the armed-list head `timer_armed_head` (a `Store`-seam scalar, the kernel
    static — `store.rs:130`).
  - Attach `requires/ensures` to every `notif_*`/`tcb_*`/`timer_*` accessor: getters
    project one field; setters update one key and **frame the other views unchanged**.
    This is the mutual-frame discipline (every setter ensures the *other* views fixed,
    so a downstream proof reasons about one view without re-establishing the rest —
    doc 27 §1, doc 29 §2.1) extended to a six-view world
    (`slot`/`refs`/`chan`/`notif`/`tcb`/`timer`). The existing `set_slot`/
    `set_obj_refs`/`set_chan_*` ensures gain `notif_view`/`tcb_view`/`timer_view`
    unchanged clauses (purely additive — phase-2/3 proofs stay green, as in doc 27 §1).
    This is the bulk of the diff.
- **Contract the scheduler seam method `signal` needs** (assumed, host-checked):
  - `make_runnable(t)`: `ensures` `slot_view`/`chan_view`/`refs_view`/`notif_view`
    unchanged and `tcb_view` updated only at `t` (`state → Runnable`, other fields
    fixed). The frame that lets `signal`'s body (4b) keep its slot/chan/refs invariant
    across the wake. (`unqueue_ready`'s contract waits for 4e.)
- **`spec fn notif_wf(nv, tv, n)`** — the waiter-queue well-formedness (the `chan_wf`
  analog, `cspace.rs:766`):
  - `wait_head is None ⇔ wait_tail is None` (empty-queue agreement);
  - the chain `wait_head -[qnext]-> … -> wait_tail` is finite and ends at `wait_tail`
    (`qnext == None`);
  - every node on the chain has `wait_notif == Some(n)` and `state == BlockedNotif`;
  - **queue acyclicity**: `q_acyclic(tv, head) = ∃ q. valid_qrank(tv, q)` where a
    strict decrease along `qnext` makes the relation well-founded — the `qnext` analog
    of `valid_srank`/`sib_acyclic` (`cspace.rs:727–735`). The ghost rank is the
    `decreases` measure for `remove_waiter`'s loop (4c) and for `waiter_seq`'s
    well-definedness.
- **`spec fn waiter_seq(nv, tv, n) -> Seq<ObjId>`** — the FIFO model: the `Seq` of TCB
  handles obtained by following `qnext` from `wait_head` (well-defined under
  `q_acyclic`). **The central new machinery of phase 4.** `wait` ⇒ `Seq::push`,
  `signal` ⇒ `Seq::drop_first`, `remove_waiter` ⇒ remove-element. Settle its shape and
  the head/tail ↔ Seq coupling here so 4b/4c build on a settled representation (the
  3b→3d discipline). Defer to 4b/4c any extra clause an op needs (the doc 27 §3 /
  doc 29 §1 "add the clause when the op pays for it" discipline).
- **Give `ArrayStore` real notif/TCB/timer state** (`test_store.rs`): extend
  `NotifState`/`TcbState` (`:61–72`) and add a `TimerState`, replacing the
  `unimplemented!()` accessors (`:191–285`) with field reads/writes; add a
  `notif_wf_exec` executable mirror with a `_has_teeth` rejecter per clause (the
  `chan_wf_exec`/`chan_wf_exec_has_teeth` pattern, doc 27 §1).
- **No op proofs yet.** The phase-2/3 verified ops call none of the new accessors, so
  they stay green untouched (the additive-frame argument, doc 27 §1).
- **Done =** verus green + `cargo test -p kcore` + `cd kernel && cargo build`. Findings
  doc **31**. (Risk is design review of the three views + `notif_wf`/`waiter_seq` and
  the queue coupling, not solver time — keep this PR's diff structural.)

### 4b — Notification `signal` / `wait` (+ `destroy_notif`): the waiter-queue FIFO core

The 3d analog and phase 4's **hardest** sub-phase. `signal`'s body proof is **the
piece phase 3 explicitly deferred** (the assumed-`signal` boundary, doc 27 §1,
doc 30 §4). Group `signal`/`wait` as the queue's pop-head/push-tail (as `send`/`recv`
were grouped in 3d). Isolate the difficulty here.

- **`signal` (`notification.rs:58`):** drop `external_body`; prove the real body. The
  contract **strengthens** the 3b assumed frame (additive, so the phase-3 callers
  `fire`/`send`/`recv`/`endpoint_cap_dropped` — which depend only on `slot_view`/
  `chan_view` unchanged — stay green):
  - `slot_view` and `chan_view` unchanged (retained from 3b);
  - **no-waiter / null-word path**: the word accumulates (`word' == old word | bits`),
    queue and `refs_view` unchanged;
  - **wake path**: the head waiter is dequeued (`waiter_seq(n)` loses its head,
    `Seq::drop_first`); it receives the whole accumulated word in `retval`; `word → 0`;
    the **wake-release `refs[n] -= 1`** (the waiter held a ref while queued —
    `notification.rs:81`; `requires refs[n] > 0` discharges the `- 1`); `make_runnable`
    flips it Runnable; every *other* TCB and notification untouched (framed via
    `make_runnable`'s 4a contract);
  - `notif_wf(n)` preserved.
- **`wait` (`notification.rs:91`):** prove the body —
  - **consume path**: nonzero word ⇒ return `Some(word)`, `word → 0`, queue and
    `refs_view` unchanged;
  - **block path**: append `cur` at the tail (`waiter_seq(n)` `Seq::push`), set
    `state = BlockedNotif`/`wait_notif = Some(n)`/`qnext = None`, fix
    `wait_head`/`wait_tail`, **acquire `refs[n] += 1`** (`requires refs[n] < u32::MAX`);
  - `notif_wf(n)` preserved.
  - The acquire (here) and release (signal/remove_waiter) are the **first installments
    of `refcount_sound`'s waiter term** — per-op deltas, the 3e `bind`-delta template
    (doc 30 §2.2), not the full census.
- **`destroy_notif` (`notification.rs:137`):** trivial — `requires refs == 0` ⇒ by
  `notif_wf` the queue is empty (a waiter would hold a ref, contradiction), so the body
  is a no-op; prove it.
- **Key lemmas / risk.** The `waiter_seq` push/pop lemmas relating the imperative
  `wait_head`/`wait_tail`/`qnext` fixups to `Seq::push`/`Seq::drop_first` — the
  list-reachability analog of 3d's window↔arena coupling. There is **no modular
  arithmetic** (linked list, not a ring), so it should be *lighter* than 3d's four
  `%`-lemmas; but the reachability `Seq` is new — quarantine the awkward reachability
  steps into one-line helper lemmas (the doc 25 §2 / doc 29 §2.4 discipline). The
  load-bearing fact is that pushing the tail / popping the head leaves the rest of the
  chain (and its per-node `wait_notif`/`state`) intact — the queue analog of 3d's
  per-class frame split (doc 29 §2.3).
- **`test_store`.** Strengthen `check_signal_frame` (`:767`) into the full proven
  contract (accumulate path + one-waiter delivery + the queued-ref release, asserting
  the dequeue *and* the frame); add `check_wait` (consume vs. block, the ref acquire),
  a FIFO sweep (block, block, signal, signal ⇒ wake order = block order), and
  `check_destroy_notif`.
- **Done =** verus green + test + cross-build. Findings doc **32**.

### 4c — Notification `remove_waiter`: the mid-queue unlink

The `cdt_unlink` analog (doc 25): a loop that walks the queue and unlinks `t` from
head / middle / tail with the tail fixup. Isolated per the phase-2/3 lesson that the
looping unlink deserves containment — though it is **simpler** than `cdt_unlink`
(singly linked, no re-parenting), so it may run short.

- **`remove_waiter` (`notification.rs:110`):** prove the body. Loop `decreases` on the
  4a queue rank (`q_acyclic` witness), with the `prev`/`cur` cursor invariant tracking
  the `waiter_seq` prefix already walked (the list-walk-re-establishes-structure shape
  of `descend_to_leaf`, `cspace.rs:3574`, and the `cdt_unlink` children walk). Contract:
  - if `t` is on `n`'s queue: it is removed (`waiter_seq(n)` loses exactly the `t`
    element, the order of the rest preserved — a `Seq` splice); `wait_head`/`wait_tail`
    fixed (the `wait_tail == Some(t)` branch resets the tail to `prev`,
    `notification.rs:120`); `t.qnext`/`t.wait_notif` cleared; **`refs[n] -= 1`**
    (release);
  - if `t` is not on the queue: the store is unchanged;
  - `notif_wf(n)` preserved.
- **Key lemmas / risk.** The splice-preserves-order proof and re-establishing
  `notif_wf` (and `q_acyclic`) after the unlink — the singly-linked, no-re-parenting
  case of doc 25's `lemma_unlink_sib`, so expect a meaningfully smaller proof.
- **`test_store`.** `check_remove_waiter` for head / middle / tail / absent, each
  asserting the ref release and the resulting queue shape.
- **Done =** verus green + test + cross-build. Findings doc **33**. *(If 4b's
  `waiter_seq` machinery makes this cheap, it MAY fold into 4b — but it is budgeted
  separately by default, the phase-2 `slot_move`/`cdt_unlink` precedent.)*

### 4d — Thread `report_terminal` (ReportMonotone + FireSafe) + `thread::bind`

The §4.4 thread/report obligations, now that `signal` is proven (4b).

- **`report_terminal` (`thread.rs:123`):** prove the two §4.4 properties.
  - **ReportMonotone** — the `if tcb_report(t) != Running { return }` guard
    (`thread.rs:124`) makes the transition `Running → Exited|Faulted` happen **at most
    once** and terminal states **absorbing**: `set_tcb_report(t, r)` then a `signal`
    that 4b proves does **not** touch `t`'s `tcb_report` (the dying thread is `Halted`/
    `Faulted`, not a `BlockedNotif` waiter on the notification it fires into — so by
    `notif_wf` it is not the dequeued waiter). State it as `tcb_report` monotone over
    any op sequence.
  - **FireSafe** — the body reads `slot(bind_slot).cap.kind` (`thread.rs:134`): either
    the slot is empty (a revoke raced the death and emptied it ⇒ the `if let
    Notification` fails ⇒ no-op) or it holds a `Notification(n)` cap, and **a
    cap-in-slot ⇒ `n` is live** (`refs[n] > 0`), so `signal` only ever fires a live
    object, never freed memory. State the cap-designates-live-object fact as a local
    precondition/invariant — the **first down payment on `refcount_sound`'s cspace-slot
    term**, *not* the full census (the 3e per-op-delta precedent).
- **`thread::bind` (`thread.rs:147`):** the TCB-binding config — the direct analog of
  `channel::bind` (3e, doc 30 §1): delete the displaced cap if present (`cspace::delete`
  — the `external_body` contract is sufficient), move the new cap in if `notif_src` is
  `Some` (verified `cspace::slot_move`), set `bind_bits`. Contract: the bind slot ends
  holding the moved cap (or empty on a `None` src); `tcb_view`'s `bind_bits` updated at
  `which`; `slot_view` reflects the delete+move; the other views framed; `cspace_wf`
  preserved (read off `delete`/`slot_move`'s ensures).
- **Key risk.** FireSafe's "cap ⇒ live object" without the global census — scope it to
  the minimal local fact, mirroring how 3e's `bind` stated only its per-op delta
  (doc 30 §2.3, "do not over-specify"). The full justification (the cspace-slot census
  term) lands with `refcount_sound` in the teardown phase.
- **`test_store`.** `check_report_terminal` (first-call-wins; the fire on each of the
  exit/fault arms; second-call no-op — ReportMonotone); `check_report_terminal_firesafe`
  (empty bind slot ⇒ no fire, no panic); `check_thread_bind` (install onto unbound /
  rebind / unbind, mirroring 3e's `check_bind`).
- **Done =** verus green + test + cross-build. Findings doc **34**.

### 4e — Timer + `destroy_tcb` deferral + phase-4 closeout

The edges (timer's armed-list + refs coupling), the declared scope-out
(`destroy_tcb`), and the documentation closeout (the 3e analog).

- **`timer::arm`/`disarm`/`check_expired`/`destroy_timer` (`timer.rs`).**
  - `arm` (`:41`): `disarm` first (idempotent re-arm), **`+1`** on the notif ref
    (`timer.rs:43`), set the timer fields, push onto the armed list
    (`set_timer_armed_head`).
  - `disarm` (`:52`): loop-unlink `t` from the armed list (`decreases` on an
    armed-list acyclicity rank — the `timer_next` analog of the 4a queue rank), then
    **`-1`** on the notif ref (`timer.rs:72`; `requires refs > 0`). Contract: `t`
    removed from the list, `armed → false`, `notif → None`, the ref released.
  - `check_expired` (`:80`): loop the armed list, `disarm` + `signal` (4b-proven) each
    timer whose deadline `<= now`. The frame is composed from `disarm`'s and `signal`'s
    contracts (object views per each); termination on the armed-list rank.
  - `destroy_timer` (`:98`) = `disarm`; `requires refs == 0`.
  - The notif-ref `+1`/`-1` are the **armed-timer term of `refcount_sound`** (per-op
    deltas, like the waiter term in 4b/4c).
  - Needs a small `timer_wf` (armed-list well-formed + the list acyclicity rank);
    define it here (or fold the rank into 4a's `valid_qrank` machinery if a shared
    list-rank spec is cleaner — decide in 4a).
- **`destroy_tcb` (`thread.rs:173`): keep `external_body`, host-test-checked** — the
  **declared scope-out** (the `destroy_channel` deferral, doc 30 §1, applied here). Its
  body calls `remove_waiter` (now proven — fine), `cspace::delete` (`external_body`),
  and `unref_cspace`/`unref_aspace` (plain Rust, the cross-object teardown recursion),
  so it cannot close until the teardown phase. Give it an assumed contract stating the
  **structural** effect: `t` off every queue, `state == Halted`, both bind slots
  emptied, **no report** (`tcb_report` unchanged — "destruction is the parent acting,
  not the thread dying," §5.1, `thread.rs:165`), `cspace_wf` preserved. Host-checked in
  `test_store` (`check_destroy_tcb`), the `check_destroy_channel` discipline (doc 30
  §2.3 — assume only the robustly-true, checkable core).
- **`obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace` stay plain Rust;
  `delete`/`destroy_channel` stay `external_body`** — unchanged, deferred to the
  teardown phase (§1.4).
- **Closeout.**
  - Write `doc/results/35_verus-findings.md` (what closed in 4a–4e; the `waiter_seq`
    list-reachability model and the `make_runnable`-contract mechanics worth keeping;
    the doc-numbering note).
  - Update `CLAUDE.md`'s `### Verus` section + the §6 verification-tier table: move
    notification `signal`/`wait`/`remove_waiter`/`destroy_notif`, thread
    `report_terminal`/`bind`, and timer `arm`/`disarm`/`check_expired`/`destroy_timer`
    onto the **proven** list (with ReportMonotone + FireSafe + the FIFO order theorem
    among them); record `thread::destroy_tcb` as the new host-test-checked
    `external_body` joining `delete`/`destroy_channel`/`notification::signal`'s former
    slot — and note `signal` graduates from `external_body` to **proven**. Flag the
    recommended dedicated **cross-object-teardown phase** after phase 5 (§1.4). No
    spec-doc edit — that is the phase-8 closeout (doc 30 §3).
- **Done =** verus green + test + cross-build. Findings doc **35**.

---

## 3. Risks & mitigations (phase-2/3-informed)

- **The `waiter_seq` list-reachability model balloons (4b).** The realistic repeat of
  the 3d FIFO surprise, in a new (linked-list-reachability) shape. Mitigation: the
  model + the queue acyclicity rank are *designed in 4a* before any `signal`/`wait`
  proof; there is no modular arithmetic (lighter than 3d); isolate reachability steps
  into one-line lemmas (doc 29 §2.4).
- **The trait extension churns every notif/tcb/timer accessor (4a).** Mitigation: an
  isolated, proof-light PR gated by the phase-2/3 proofs staying green. The production
  `KernelStore` is the trusted boundary, so the three new ghost `spec fn` views need
  **no production-code change** — only the `ExStore` spec and `ArrayStore` (host) gain
  real bodies (doc 27 §2.4).
- **`make_runnable` pulls scheduler state into a contract.** Mitigation: a *single*
  minimal assumed contract (frames the object views, sets `t` Runnable), host-checked —
  the `set_slot`/assumed-`signal` discipline; nothing else of the scheduler is touched.
- **FireSafe needs a refcount fact before the census exists (4d).** Mitigation: state
  only the local "cap-in-slot ⇒ object live" fact (the 3e per-op-delta precedent), not
  the full `refcount_sound`; the global justification lands with the teardown phase.
- **Scope creep into the cross-object teardown.** Mitigation: declared a non-goal in
  §1.4 and §4 below, not surfaced mid-PR — and the recommended dedicated phase gives it
  an explicit home so it is not silently absorbed into phase 4 or 5.
- **Re-arm / same-target edge cases (timer; the doc 30 §2.2 read-after-write lesson).**
  `arm` calls `disarm` first, and a timer can re-arm to the same notification.
  Mitigation: model the ref delta in the body's order (`disarm`'s `-1` then `arm`'s
  `+1`) so the same-notif case is provably net-zero for free, exactly as `bind`'s
  `bind_refs_post` handled rebinding to the same notif (doc 30 §2.2).

---

## 4. Out of scope (phase-4 non-goals)

- **Cross-object `delete` body, `obj_unref`, `destroy_cspace`, `unref_cspace`/
  `unref_aspace`, and the full `refcount_sound` census** — the recommended dedicated
  teardown phase after phase 5 (§1.4). Phase 4 lands only the waiter and armed-timer
  per-op refcount deltas.
- **`thread::destroy_tcb` / `channel::destroy_channel` body proofs** — they close with
  the cross-object teardown.
- **`notification::signal`'s callers' contracts** are not re-opened — `signal`'s
  strengthened contract is additive, so `fire`/`send`/`recv`/`endpoint_cap_dropped`
  (3b–3e) are unaffected.
- **aspace + PTE (phase 5), sysabi (phase 5), host chokepoints (phase 6), commit
  recovery core (phase 7).**
- **Byte-for-byte payload modelling** — inherited as abstracted from phase 3.

---

## 5. Exit criterion for phase 4

`cargo verus verify -p kcore` proves notification `signal`/`wait`/`remove_waiter`/
`destroy_notif`, thread `report_terminal`/`bind`, and timer `arm`/`disarm`/
`check_expired`/`destroy_timer` against `notif_wf` + the `waiter_seq` FIFO model (so
**wake order = block order** is a theorem) + `timer_wf` + the inherited `cspace_wf`/
`chan_wf`; **ReportMonotone** (at most one `Running → Exited|Faulted`, terminal
absorbing) and **FireSafe** (a terminal fire reads an empty slot or a live
notification, never freed memory) are proven; the **waiter** and **armed-timer**
refcount deltas are landed as the first installments of `refcount_sound`;
`notification::signal` graduates from `external_body` to **proven**, and
`thread::destroy_tcb` is the only new `external_body` op, host-test-checked alongside
the inherited `delete`/`channel::destroy_channel`; the aarch64 `kernel` build and
`cargo test -p kcore` are green; `doc/results/35` and `CLAUDE.md` record the new
division and the recommended cross-object-teardown phase. The cross-object `delete`
body and the full `refcount_sound` census pass forward to that dedicated post-phase-5
phase.
