# Plan — Part B8 detail: extend the verified kernel surface into the syscall shell (symmetric cap-side MAP + verified priority-ceiling gate + ready-queue list surgery)

Detailed, separately-implementable decomposition of **Phase B8** from
`doc/plans/0_address_audit_rev0.md`. B8 is the Wave-3 kernel **verified-surface** work: three
places where the running kernel already *calls* verified `kcore` decisions but the load-bearing
step itself sits in unverified shell. B8 moves each behind a verified `kcore` object operation, so
the `cargo verus verify -p kcore` gate covers what the audit found exposed. B8 is the **first
kernel-focused detail plan** (B1–B7 were storage/driver/loader/dma/cas), so it establishes the
kernel pattern — and like B7 it is **behaviour-identical**: it changes no syscall, no ABI, no
scheduling order, no observable kernel behaviour; it changes *what is proven about* the existing
behaviour. The QEMU boot is byte-for-byte the same before and after.

**Closes (from the parent plan):**
- **frame-MAP unverified vs. verified unmap** [audit §4.2, medium] — the cap-side *unmap* is proven
  over object state (`kcore::cspace::delete` drives `aspace_unmap` + `unref_aspace`,
  `cspace.rs:9799-9817`), but the matching cap-side *map* — recording `mapping: Some((asp, va))` on
  the frame cap and bumping the aspace refcount — runs as **plain shell logic** in the `Sys::Map`
  handler (`syscall.rs:555-560`). The guarantee is asymmetric: delete is mechanized, map is not.
  B8A moves the map-time bookkeeping behind a verified `kcore` op symmetric to the delete path.
- **spawn-time priority-ceiling *check* unverified** [audit §4.2, medium] — the priority *write*
  is already verified (`kcore::thread::set_priority`, `thread.rs:222`, `ensures priority == prio`
  ∧ `priority <= ceiling`), but it rests on a `requires prio <= ceiling` (`:225`) **discharged by
  the unverified shell `if prio > max_prio { return ERR_PERM }`** (`syscall.rs:456`, and `:627` for
  `ThreadStartAs`). The *refusal decision* — the gate the spec calls verified — is trusted shell.
  B8B makes the refusal a verified branch so the spawn-time check is mechanized, not promised.
- **ready-queue list logic unverified** [audit §4.2, low–medium] — the 32-level ready queue
  (`thread.rs:66-151`: `enqueue`/`dequeue`/`top_ready`/`unqueue_ready`, an intrusive `Tcb.qnext`
  list + a `u32` priority bitmap) is plain unsafe Rust, **and is already a trusted `Store` seam the
  verified `destroy_tcb` leans on** (`kcore::thread::destroy_tcb` calls `store.unqueue_ready(t)`,
  `thread.rs:515`). It is the same shape as the **verified** notification waiter queue and timer
  armed list — the one intrusive list in the kernel without a verified twin. B8C moves it into
  `kcore` with `requires`/`ensures`, copying those templates.

**Conforms rev1§5.4 and rev1§6.1(c)/(d).** B8A and B8B each flip a blessed `[verifying]` seam;
B8C is a pure verified-surface gain with **no** `[verifying]` tag (see the honesty notes).

**Spec target (already blessed in rev1 — B8 only conforms code to it):**
- **rev1§5.4 "Scheduling"** (`spec_rev1.md:383`) — "strict fixed-priority preemptive scheduling:
  32 levels, round-robin within a level" (the structure B8C verifies); "The cap-carried ceiling and
  its monotone attenuation are verified; **this revision's verification work moves the spawn-time
  check that a thread's priority does not exceed its ceiling into the verified surface alongside
  them** (§6.1). The one raw priority write behind the object seam is trusted." B8B makes that
  sentence true. (Prose target, no `[verifying]` tag of its own — see §6.1(d).)
- **rev1§2.5 "Memory"** (`spec_rev1.md:109`) — "The cap-side bookkeeping is verified — a derived
  copy starts unmapped, and deletion drives an unmap at the cap's recorded coordinates, **with the
  matching map-time recording joining the verified surface this revision** (§6.1) — while the
  actual clearing of page-table entries is verified separately over raw page-table memory." B8A
  delivers the "map-time recording joins the verified surface" half. The "derived copy starts
  unmapped" half is **already** proven (`derived_kind` clears `mapping: None`, `cspace.rs:1211`).
- **rev1§2.3** (`spec_rev1.md:75`) — the thread cap's "maximum-controlled-priority ceiling …
  attenuates monotonically: a derived thread cap's ceiling is the minimum of the parent's ceiling
  and any lower ceiling the deriver requests." Already verified in `derive`/`derived_kind`; B8B
  composes the spawn gate on top of it.
- **rev1§6.1(c) "Cap-to-page-table correspondence"** (`spec_rev1.md:417`) — the cap-side unmap is
  proven; "**The matching cap-side map is [verifying]:** this revision moves the map-time
  bookkeeping — a derived copy starting unmapped, and mapping recording the entry's coordinates in
  the cap — behind the same kind of verified object operation, **making the cap-side guarantee
  symmetric** where only the unmap half was mechanized. … What stays **[trusted]** is the join —
  that the cap's recorded mapping is the true entry location and that map and unmap truly write and
  clear it." B8A flips this `[verifying]` part to mechanized; the join stays trusted.
- **rev1§6.1(d) "Thread-lifecycle shell"** (`spec_rev1.md:418`) — "The spawn-time priority-ceiling
  gate … is **[verifying]:** this revision moves the gate out of the syscall shell into a verified
  kernel operation, joining the cap-carried ceiling and its monotone attenuation, which are already
  proven. What stays **[trusted]**: the 'suspended, never rescheduled' state (exception entry,
  syscall exit, **scheduler**), the anti-forgery and anti-suppression access control … and the
  exit/read-report syscall dispatch and register marshalling." B8B flips the gate `[verifying]`
  part; the scheduler *policy*, the access control, and the asm context switch stay trusted.

Because Part A is blessed first (the parent plan's hard dependency), **B8 makes no normative spec
edits** — the rev1 text above is the fixed target, and every citation here is `rev1§`. The only
doc-touches B8 makes are the sanctioned **"flip your own `[verifying]` status line" edits**
(parent plan A4): B8A flips §6.1(c)'s map part, B8B flips §6.1(d)'s gate part, and each updates the
matching trusted-base-ledger `[verifying]` row + the kcore baseline count. B8C touches **no** spec
prose — it has no `[verifying]` tag; it updates only the ledger's verified-surface scope paragraph
and the baseline (see Honesty note 4).

**Primary files:**
- `kernel/src/syscall.rs` — the `Sys::Map` handler `:516-567` (the cap-side bookkeeping
  `(*asp_ptr).hdr.refs += 1` `:555` + `mapping: Some((asp, va))` `:556-560`, after the verified
  page-table `crate::aspace::map` call `:553`); `Sys::ThreadStart` `:424` (the gate
  `if prio > max_prio` `:456`, the cap-kind destructure `CapKind::Thread(t, max_prio)` `:437`, the
  refs bumps `:459`, the `thread::set_priority(tp, prio, max_prio)` call `:464`); `Sys::ThreadStartAs`
  `:597` (gate `:627`, refs `:634-635`, `set_priority` `:639`); `Sys::CapDelete` `:261-266` (→
  `cspace::delete`, the verified unmap entry that is B8A's template).
- `kernel/src/thread.rs` — the ready queue: `struct Queue` `:66`, `READY`/`READY_BITMAP` `:76-77`
  (`NUM_PRIOS = 32`, `:25`), `enqueue` `:90`, `dequeue` `:104`, `top_ready` `:117`, `unqueue_ready`
  `:129`, `maybe_switch` `:159` (the scheduler orchestration — **stays trusted shell**, §6.1(d));
  the `as_tcb`/`tcb_id` ObjId↔`*mut Tcb` link seam `:31-40` (**stays trusted by construction**, the
  same posture as the notif/timer `Option<ObjId>` links); the `set_priority`/`enqueue`/
  `unqueue_ready` shell wrappers `:62-` that convert pointers and call `kcore`.
- `kernel/src/aspace.rs` — `map` `:84` (→ verified `kcore::aspace::map_in` `:97`), `unmap` `:105`
  (→ verified `kcore::aspace::unmap_in` `:111`) — the page-table walker, already verified; B8A does
  not touch it (the join stays trusted, §6.1(c)).
- `kcore/src/cspace.rs` — the **unmap template** for B8A: `delete` `:9663`, `delete_prepare` `:9459`,
  `unref_aspace` `:9367`, and the Frame-unmap branch `aspace_unmap` + `unref_aspace` `:9799-9817`;
  the **ceiling/attenuation precedent** for B8B: `derive` `:7783`, `derived_kind` `:1211` (Frame arm
  clears `mapping: None`; Thread arm clamps the ceiling to `min(mp, prio_ceiling)`), `cap_max_prio`
  `:1160`; the `Store`-method spec `aspace_unmap` `:1101`. B8A adds the symmetric verified map op
  (+ a `ref_aspace` increment, the twin of `unref_aspace`); the **intrusive-list spec witnesses**
  B8C copies live here too: `waiter_chain`/`waiter_seq`/`notif_wf` `:1574-1606`,
  `timer_chain`/`timer_seq`/`timer_wf`/`timer_complete` `:2538-2573`.
- `kcore/src/thread.rs` — `set_priority` `:222` (`requires prio <= ceiling` `:225`,
  `ensures priority == prio` `:231` ∧ `priority <= ceiling` `:232`) — B8B refactors this to make
  the refusal verified; `Tcb` `:75` (`priority` `:79`, `qnext` `:86`); `destroy_tcb` `:419` (calls
  `store.unqueue_ready(t)` `:515` — the seam B8C verifies the twin of).
- `kcore/src/notification.rs` — the **waiter-queue template** B8C's per-level lists copy: `wait`
  (enqueue-to-tail), `signal` `:46` (wake/dequeue head, `waiter_seq` `.drop_first()`),
  `remove_waiter` `:580` (the splice walk: a `while cur.is_some()` loop with a `decreases` bound and
  a position-tracking `invariant`).
- `kcore/src/timer.rs` — the **armed-list template** (completeness + per-element frame): `arm`
  (enqueue), `disarm` `:69` (splice walk, `timer_complete` makes the walk terminate).
- `kcore/src/store.rs` — the `Store` trait seams: `make_runnable` `:125`, `unqueue_ready` `:127`,
  `aspace_unmap` `:129`, `aspace_destroy` `:131`. B8A adds `aspace_map` (symmetric with
  `aspace_unmap`); B8C verifies the list logic behind `unqueue_ready`/the ready-queue ops.
- `kernel/src/store.rs` — the trusted-shell `Store` impl: `make_runnable` `:250`, `unqueue_ready`
  `:253`, `aspace_unmap` `:256`, `aspace_destroy` `:259` (the realizations that stay trusted).
- `doc/guidelines/verus_trusted-base.md` — the `[verifying]` rows `:115` (MAP) and `:116`
  (priority) → flip to landed; the verified-surface scope paragraph `:17-30` (B8C adds the ready
  queue alongside "notification waiter queue, timer armed list"); the kcore baseline `:127`
  (335 → the new totals, recorded per sub-phase).
- `doc/spec/spec_rev1.md` — the rev1§6.1(c) `:417` and §6.1(d) `:418` `[verifying]` markers (flip
  MAP + the gate to mechanized). No other prose changes; §5.4 `:383` becomes *true* without an edit.

Secondary: `kcore/src/test_store.rs` (the array-backed in-memory `Store` the verified ops execute
against in host unit tests — exercises the new map op, the gated `set_priority`, and the ready-queue
ops); the proof-support lemmas the templates already carry (`lemma_waiter_remove_chain`,
`lemma_timer_remove_chain` — B8C writes the ready-queue analogues).

---

## Verification tier & baseline (applies to all sub-phases)

B8 is a single verification surface: the **`kcore` Verus chokepoint** (rev1§6 routing — kernel
chokepoints get Verus). All three sub-phases add verified object operations and their support
lemmas to `kcore` and re-establish the gate above its prior number. Four honesty notes up front so
nothing is silently dropped or over-claimed:

1. **The gate is a floor that *rises*; no existing proof is weakened.** `cargo verus verify -p
   kcore` is **335/0** today (ledger `:127`). Each sub-phase adds verified items (a new op + its
   support lemmas), so the count goes **above 335** — each records its new total in the ledger.
   B8A adds the verified map op + `ref_aspace` + their census/frame lemmas; B8B adds the gated
   `set_priority` refusal branch (and removes one shell `if` worth of trust, not a verified item);
   B8C adds the most — a verified ready-queue module (the per-level `ready_chain`/`ready_seq`
   witnesses, the four ops, the bitmap-coherence lemma, the splice-walk lemmas), the parent plan's
   "M / medium (proof engineering, but patterns exist in `kcore`)." The four `external_body` seams
   and the `assume_specification`s (ledger `:98-100`) are **untouched** — B8 adds verified ops, it
   does not widen the trusted base.

2. **B8 is behaviour-identical — verification-only at the runtime level (like B7).** Every
   sub-phase produces byte-identical kernel behaviour: the same `ERR_PERM` on `prio > ceiling`, the
   same `mapping`/refcount transitions, the same scheduling order. The refactors move *where the
   decision is proven*, not *what the kernel does*. Concretely: B8B's `set_priority` changes from
   "`requires prio <= ceiling` (caller's shell `if` guards it)" to "returns `Result`, refuses
   internally" — observably identical (`prio > ceiling` → `ERR_PERM` either way), but the refusal
   is now a verified branch instead of an unverified shell `if`. So the regression gate includes
   **the QEMU boot still green** (`cd kernel && cargo build` + the boot smoke), not just the Verus
   count: B8 must not perturb the running system.

3. **What stays trusted is named and unchanged.** B8 shrinks the verified/trusted boundary at three
   points but leaves the irreducible remainder exactly where rev1§6.1 puts it: **(c)** the
   page-table write/clear *join* — that the cap's recorded `(asp, va)` is the true entry and that
   `map_in`/`unmap_in` truly write/clear it — stays trusted (B8A verifies the *cap-side* record,
   not the join); **(d)** the scheduler *policy* (`maybe_switch`, the "suspended/never-rescheduled"
   state machine, the asm context switch), the anti-forgery/anti-suppression access control, and the
   spawn-time cap-distribution convention stay trusted (B8C verifies the *queue data structure*, not
   the scheduling decision; B8B verifies the *refusal*, not the cap-distribution convention that
   decides who holds which ceiling); and the **ObjId ↔ `*mut Tcb` address identity** (`as_tcb`/
   `tcb_id`) stays trusted by construction — the same `Store` seam the verified notif waiter queue
   and timer armed list already lean on via their `Option<ObjId>` links.

4. **B8C flips no spec claim — it is an additive verified-surface gain.** The blessed `[verifying]`
   table (ledger `:113-119`, mirroring rev1§6.1) has exactly **two** B8 rows: the cap-side MAP
   (§6.1(c), B8A) and the priority-ceiling gate (§6.1(d), B8B). The **ready queue has no
   `[verifying]` tag** — it is an audit §4.2 [low–medium] "extend the verified surface" item, not a
   spec over-claim being conformed. So B8C makes **no normative §6.1 edit** (consistent with B8's
   "no normative spec edits" discipline): rev1§6.1(d)'s "scheduler [trusted]" line stays literally
   true (the scheduler *policy* and asm switch remain trusted; only the queue *data structure* joins
   the verified surface). B8C records the gain in the **ledger** alone — adding the ready queue to
   the verified-surface scope paragraph (`:17-30`, beside "notification waiter queue, timer armed
   list") and bumping the baseline. Recorded here so no reviewer expects a §6.1 ready-queue flip and
   no over-claim is read into the ledger.

**Baseline to re-establish at end of B8:**
- `cargo verus verify -p kcore` ≥ **335/0**, **> 335** after each sub-phase (record the new total in
  the ledger per B8A/B8B/B8C). The four `external_body` + the `assume_specification`s unchanged.
- The aarch64 build boots: `cd kernel && cargo build` and the QEMU boot smoke pass unchanged (B8
  changes no syscall signatures `storaged`/`init`/`shell` depend on; the shell wrappers keep their
  call shapes, only their bodies route through the new verified ops).
- `cargo test -p kcore` green (the `test_store` host unit tests exercise the new map op, the gated
  `set_priority`, and the ready-queue ops over the array-backed store).
- The three flipped artifacts agree line-for-line: rev1§6.1(c)/(d) `[verifying]` parts read
  mechanized; the ledger `[verifying]` rows `:115-116` read landed; the ledger scope paragraph names
  the ready queue; the kcore baseline `:127` reflects the final total.

---

## Design decision 1 — the cap-side MAP: a verified map-time op symmetric to the delete/unmap path *(resolve in B8A)*

The cap-side unmap is mechanized: `kcore::cspace::delete` (`cspace.rs:9663`), on a mapped Frame,
calls `store.aspace_unmap(asp, va, pages)` (the page-table side, a `Store` seam) **and**
`unref_aspace(store, asp)` (`:9367`, the verified refcount decrement), and clears the cap as it
empties the slot. The map side is asymmetric: the shell's `Sys::Map` handler calls the verified
page-table `crate::aspace::map` (`:553` → `map_in`), then does the **cap-side bookkeeping in plain
shell** — `(*asp_ptr).hdr.refs += 1` (`:555`) and `mapping: Some((asp, va))` (`:556-560`). The
"derived copy starts unmapped" half of §6.1(c) is **already verified** (`derived_kind` sets
`mapping: None` on every derive, `cspace.rs:1211`); only the **map-time `None → Some` recording +
the refcount increment** is exposed.

- **Adopted — a verified `kcore::cspace::map_frame` op (the mirror of the delete Frame branch),
  plus a `ref_aspace` increment (the twin of `unref_aspace`), driving the page-table write through a
  new `Store::aspace_map` seam.** Concretely:
  1. Add `pub fn ref_aspace<S: Store>(store, a: ObjId)` — the exact inverse of `unref_aspace`
     (`:9367`): `requires` the off-by-one census shape on the *increment* side and `refs[a]`
     present; `ensures refs == old.insert(a, old[a] + 1)`, every other view framed, `refcount_sound`
     preserved. (`derive` already proves a designating-copy increment of `refs`,
     `cspace.rs:7783` — `ref_aspace` reuses that census-lemma vocabulary.)
  2. Add `pub fn map_frame<S: Store>(store, frame_slot: SlotId, asp: ObjId, va: u64)` that, with
     `requires`: the slot holds a `Frame { mapping: None, .. }` (unmapped — the state the shell
     checks at `syscall.rs:541`), `asp` live in `refs_view`, `cspace_wf`/`refcount_sound`/
     `caps_consistent`; **records `mapping: Some((asp, va))`** on the frame cap (a `slot_view`
     update, the inverse of the slot-clear in `delete`) and calls `ref_aspace(store, asp)`;
     `ensures`: the cap's kind is now `Frame { mapping: Some((asp, va)), .. }`, `refs[asp]` bumped by
     one, `cspace_wf`/`refcount_sound`/`caps_consistent` preserved, all other views framed —
     **symmetric, term-for-term, with the delete Frame branch's `ensures`**.
  3. Add `Store::aspace_map(&mut self, a, pa, va, pages, perms) -> Result<(), MapError>` to the
     `kcore` trait (mirroring `aspace_unmap` `:1101`/`store.rs:129`), realized in the kernel shell
     by the existing `crate::aspace::map` (`kernel/src/aspace.rs:84`). The verified `map_frame`
     calls it for the page-table side exactly as `delete` calls `aspace_unmap` — so MAP and unmap
     become structurally identical: *verified cap-side record + `Store`-seam page-table side*.
  4. Rewrite the `Sys::Map` Ok-arm (`syscall.rs:554-561`): drop the raw `hdr.refs += 1` and the raw
     `cap.kind = Frame { mapping: Some(..) }`; instead call `cspace::map_frame(&mut KernelStore,
     SlotId(fr_slot ..), asp, va)` after (or composing) the page-table map, mapping its `Result` to
     the existing errno arms. The shell keeps the *validation* (rights, RO-monotonicity,
     device-`PHYS` gate, `mapping.is_some()` → `ERR_STATE` at `:541-550`) — that is access-control
     shell, §6.1(d)-style — but the *state mutation* is now the verified op's.
  - **Decisive reasons:** (a) it makes the cap-side guarantee **symmetric**, which is exactly what
    §6.1(c) promises and the ledger MAP row pre-commits; (b) it reuses the delete branch's proven
    census/frame machinery (the off-by-one census discipline, the mutual-frame `ensures`) rather
    than inventing a new proof shape — `map_frame` is `delete`'s Frame branch run backwards; (c) the
    page-table write stays the trusted join §6.1(c) keeps — `map_frame` verifies the *record*, the
    raw entry write is `aspace_map`'s realization, trusted exactly as `aspace_unmap`'s is.
- **State-the-bar note.** The "derived copy starts unmapped" obligation needs **no new work** —
  `derived_kind`'s Frame arm already forces `mapping: None`, and `derive`'s `ensures` already
  carries it. B8A cites that as the already-mechanized half and delivers only the map-time half, so
  the §6.1(c) "symmetric" claim is *derive (unmapped-on-copy) + map_frame (record-on-map) + delete
  (clear-on-unmap)* — three verified ops, one trusted join.
- **Rejected — verify the page-table entry write itself.** Out of scope: §6.1(c) explicitly keeps
  "the real writing and clearing of page-table entries … proven separately over raw page-table
  memory" and the *join* trusted. Pulling the raw entry write into `map_frame` would conflate the
  cap-side record (object state, SMT-tractable) with the page-table memory (a different proof,
  `map_in`'s, already done) and the trusted join (irreducible). B8A verifies the record, nothing
  more.
- **Rejected — leave the cap-side bookkeeping in the shell and only re-document it.** That is the
  asymmetry the audit names: unmap mechanized, map not. Re-documenting without moving the mutation
  leaves the §6.1(c) `[verifying]` part trusted, contradicting the blessed target.

**Recommendation: add `ref_aspace` + `map_frame` (the delete-branch mirror) and a `Store::aspace_map`
seam; rewrite the `Sys::Map` Ok-arm to call `map_frame`; flip §6.1(c)'s map part and the ledger MAP
row, recording the new kcore total.**

---

## Design decision 2 — the priority-ceiling gate: make the *refusal* a verified branch, not a shell `if` *(resolve in B8B)*

The priority *write* is verified and the post-state `priority <= ceiling` is already a reachable
`ensures` (`thread.rs:231-232`, whose doc-comment brags it is "a reachable `ensures`, not a shell
promise"). The gap is narrower and exact: `set_priority` *`requires prio <= ceiling`* (`:225`), so it
is only ever **called** in the admissible case — the **inadmissible case is rejected by the
unverified shell `if prio > max_prio { return ERR_PERM }`** (`syscall.rs:456`, `:627`). The
*decision to refuse* is the trusted shell; the audit's "spawn-time priority-ceiling *check*
unverified" is precisely this `if`.

- **Adopted — refactor `set_priority` into a total, refusing op that returns `Result`, replacing the
  `requires prio <= ceiling` with an internal verified branch; the shell drops its `if`.**
  Concretely:
  1. Change `kcore::thread::set_priority` (or add `start_with_priority` and leave `set_priority` for
     internal use) to `-> Result<(), ()>`, **dropping** `requires prio <= ceiling` and adding the
     decision in the body: `if prio > ceiling { return Err(()) } else { store.set_tcb_priority(t,
     prio); Ok(()) }`. The contract becomes: `ensures res is Ok ==> final.tcb_view()[t].priority ==
     prio && prio <= ceiling && (all other views framed)`, `res is Err ==> prio > ceiling &&
     final.tcb_view() == old.tcb_view() && (everything framed/unchanged)`. The **refusal is now a
     machine-checked branch**: Verus proves both that an accepted priority is within the ceiling and
     that an over-ceiling request leaves the thread untouched and rejected.
  2. Rewrite the two shell sites (`syscall.rs:456-464`, `:627-639`): **delete the `if prio >
     max_prio { return ERR_PERM }`**, call `thread::set_priority(.., prio, max_prio)`, and map
     `Err(()) → ERR_PERM`, `Ok(()) → 0` (then proceed to `enqueue`). Observably identical
     (`prio > max_prio` → `ERR_PERM`), but no unverified `if` decides admissibility.
  - **Decisive reasons:** (a) it closes the *exact* gap the audit names — the refusal moves from
    shell to a verified branch — while reusing everything already proven (the write, the
    `priority <= ceiling` post-condition, the mutual-frame discipline); (b) it composes cleanly on
    the **already-verified** ceiling: `max_prio` is the cap's `cap_max_prio` (`cspace.rs:1160`),
    whose monotone attenuation `derive`/`derived_kind` already prove (`:1211` Thread arm clamps to
    `min(mp, prio_ceiling)`), so the spawn check is verified *end to end* — attenuation (done) +
    refusal (new); (c) it is the smallest of the three sub-phases — one signature change, one branch,
    two shell-site rewrites — the "patterns exist in kcore" case.
- **Variant — read the ceiling from the cap's `slot_view` inside the op (tighter seam).** The
  stronger form takes the thread-cap `SlotId` and reads `ceiling = cap_max_prio(slot_view[cap])`
  *inside* the verified op, so the op no longer trusts the shell to pass the right `ceiling: u8`
  (today the shell destructures `CapKind::Thread(t, max_prio)` at `:437`/`:611` and passes it). This
  removes the "shell passes the correct ceiling" trust the current `ceiling: u8` parameter carries.
  **Recommended if cheap** (the slot read + `cap_max_prio` are already verified vocabulary);
  acceptable to keep `ceiling: u8` as the passed param (same value-trust as today, but the
  *decision* is verified either way) if coupling `set_priority` to `slot_view` proves
  disproportionate — record which seam was taken.
- **Rejected — a pure `ceiling_ok(prio, ceiling) -> bool` predicate the shell calls before
  `set_priority`.** A verified function whose `ensures r == (prio <= ceiling)` adds an item but
  leaves the *composition* — "if `!ceiling_ok` then refuse" — in the shell, so the refusal decision
  is still unverified glue. Folding the gate into the op (so there is no shell branch to get wrong)
  is what makes the check mechanized.
- **Rejected — leave the `if` in the shell and soften the spec.** The parent open decision #2
  resolved **conform, not soften** ("both are cheap relative to the existing `kcore` proof
  surface"), and rev1§5.4/§6.1(d) are blessed with the gate as verified target. Softening would
  contradict Part A.

**Recommendation: make `set_priority` return `Result` with an internal verified refusal; delete the
two shell `if`s and map `Err → ERR_PERM`; prefer reading the ceiling from the cap's `slot_view` if
cheap. Flip §6.1(d)'s gate part and the ledger priority row, recording the new kcore total.**

---

## Design decision 3 — the ready queue: a verified intrusive-list module copying the waiter-queue / armed-list templates *(resolve in B8C)*

The ready queue (`kernel/src/thread.rs:66-151`) is a 32-level intrusive `Tcb.qnext` list plus a
`u32` priority bitmap — `enqueue` (append-to-tail + set bit), `dequeue` (pop-head + clear-bit-if-
empty), `top_ready` (`leading_zeros` → highest non-empty level), `unqueue_ready` (linear-search
splice + clear-bit-if-empty). It is plain unsafe Rust **and already a trusted `Store` seam the
verified `destroy_tcb` depends on** (`store.unqueue_ready(t)`, `kcore/src/thread.rs:515`). It is the
**same shape** as two already-verified intrusive lists: the notification waiter queue
(`waiter_chain`/`waiter_seq`/`remove_waiter`) and the timer armed list
(`timer_chain`/`timer_seq`/`timer_complete`/`disarm`).

- **Adopted — a verified `kcore` ready-queue module modeled exactly on the waiter-queue/armed-list
  witnesses, reusing the existing `Option<ObjId>` link seam, with one added bitmap-coherence
  invariant.** Concretely:
  1. **Spec witnesses, per level.** Define `ready_chain(tv, level, head, tail, seq)` and
     `ready_seq(...)` as the `waiter_chain`/`waiter_seq` analogue (`cspace.rs:1574-1606`): a
     `Seq<ObjId>` with `no_duplicates()` (no cycles), membership in `tcb_view`, head/tail agreement,
     `qnext`-threading (`tv[seq[i]].qnext == seq[i+1]` or `None` at tail), and the per-element
     covenant `tv[seq[i]].priority == level && state == Runnable` (the ready-queue analogue of the
     waiter queue's `wait_notif == Some(n) && state == BlockedNotif`). `ready_wf` then bundles
     head/tail-None agreement + chain existence **across all 32 levels**, plus the
     **bitmap-coherence invariant** — `READY_BITMAP & (1 << level) != 0  <==>  ready_seq(level)`
     non-empty — the one structure the single-queue templates lack. (The packed-bitmap reasoning is
     a solved pattern in this codebase: `verus.md:417,434` — the `i/64`/`i%64` bit-locating lemma
     via `nonlinear_arith`; here the bitmap is a single `u32`, so the `leading_zeros` link is a
     32-bit bit-scan lemma.)
  2. **The four verified ops.** `ready_enqueue` (append-to-tail + set bit) ⟵ the waiter-queue `wait`
     enqueue; `ready_dequeue(level)` (pop-head + clear bit if now empty) ⟵ `signal`'s wake path
     (`waiter_seq` `.drop_first()`, `notification.rs:46`); `ready_unqueue(t)` (the splice walk) ⟵
     **`remove_waiter` `:580` term-for-term** — the same `while cur.is_some()` loop with a
     `decreases ws.len() - k` bound, the `invariant` pinning `cur`/`prev` to positions in the
     sequence, the head-vs-middle splice, and `lemma_..._remove_chain` proving `ready_seq` updates
     to `seq.remove(index_of(t))`; `top_ready` (a verified `leading_zeros` ↔ "highest non-empty
     level" via the bitmap-coherence invariant — returns `None` iff bitmap `== 0`, else the top set
     bit's level, proven non-empty by coherence). The timer **`timer_complete`** discipline
     (`cspace.rs:2538`, "every armed timer is on the list") is the template for proving
     `ready_unqueue`'s walk terminates and finds a `Runnable` thread.
  3. **The link seam stays trusted by construction.** The representation is **already**
     `qnext: Option<ObjId>` (`thread.rs:86`) — the same ObjId-handle indirection the notif/timer
     queues verify over; the `as_tcb`/`tcb_id` `ObjId ↔ *mut Tcb` address identity
     (`kernel/src/thread.rs:31-40`) stays the trusted `Store` seam, exactly as the notif/timer link
     resolution does. **No representation change is needed** — the queue is already Verus-friendly;
     the verification is the list-invariant proof, not a refactor of the data.
  4. **Call-site rewiring.** The kernel `enqueue`/`dequeue`/`unqueue_ready`/`top_ready` shell
     wrappers become thin pointer-convert + `kcore` calls (the `cspace::delete` wrapper pattern,
     `kernel/src/cspace.rs:14-16`); `maybe_switch` (`thread.rs:159`) keeps orchestrating them but is
     **not** pulled into `kcore` — the scheduler *policy* and the asm context switch stay trusted
     (§6.1(d)). `destroy_tcb`'s `store.unqueue_ready(t)` now resolves to the **verified** twin (the
     `Store` seam realization calls the verified op), tightening the one ready-queue seam the
     verified core already leaned on.
  - **Decisive reasons:** (a) the proof templates already exist and ship green — B8C is "copy
    `remove_waiter`/`disarm`, add the per-level + bitmap-coherence wrapper," the parent plan's
    "patterns exist in kcore"; (b) the representation is already ObjId-based, so the trusted seam is
    identical to the notif/timer one — no new trust, no refactor; (c) it closes the *only* intrusive
    list the kernel ships without a verified twin, and the one `destroy_tcb` trusts.
- **Rejected — keep the ready queue a trusted `Store` seam.** Status quo; it is the audit finding,
  and it is anomalous precisely because its two structural siblings (waiter queue, armed list) are
  verified and `destroy_tcb` already trusts this seam — the cheapest verified twin in the kernel to
  add given the templates.
- **Rejected — a fully pointer-graph-verified version (no ObjId indirection).** Verus has no
  intrusive-raw-pointer memory model; the ObjId-handle indirection is *exactly* the abstraction that
  makes the waiter queue and armed list tractable. B8C must use the same seam, not eliminate it —
  the address identity stays trusted by construction (Honesty note 3).

**Recommendation: add a verified `kcore` ready-queue module copying the waiter-queue/armed-list
witnesses, with the per-level + bitmap-coherence invariant; keep `maybe_switch`/the asm switch
trusted; rewire the shell wrappers + the `unqueue_ready` seam to the verified ops; record the new
kcore total and add the ready queue to the ledger's verified-surface scope paragraph (no §6.1
flip).**

---

## Sub-phase B8A — symmetric cap-side MAP *(closes the frame-MAP asymmetry [medium]; conforms rev1§6.1(c))*

The map/unmap-symmetry deliverable. Moves the cap-side map-time bookkeeping (`mapping: None → Some`
+ aspace refcount increment) out of the `Sys::Map` shell into a verified `kcore` op symmetric to the
delete/unmap path. Independent of B8B and B8C (different files, different proof surface). After B8A
the cap-side mapping guarantee is symmetric: derive proves unmapped-on-copy, `map_frame` proves
record-on-map, `delete` proves clear-on-unmap; the page-table write stays the trusted join.

- **Touches:**
  - `kcore/src/cspace.rs` — add `ref_aspace` `:9367`-adjacent (the increment twin of `unref_aspace`);
    add `map_frame` (the mirror of `delete`'s Frame-unmap branch `:9799-9817`); cite `derived_kind`
    `:1211` (the already-proven unmapped-on-copy half). (Design decision 1.)
  - `kcore/src/store.rs` — add the `aspace_map` trait method (symmetric with `aspace_unmap` `:129`).
  - `kernel/src/store.rs` — realize `aspace_map` (the trusted page-table side) via the existing
    `crate::aspace::map`; this is the §6.1(c) trusted join, the twin of `aspace_unmap` `:256`.
  - `kernel/src/syscall.rs` — rewrite the `Sys::Map` Ok-arm `:554-561`: drop the raw `hdr.refs += 1`
    `:555` and the raw `mapping: Some(..)` write `:556-560`; call `cspace::map_frame(..)` and map its
    `Result` to the errno arms. Keep the validation `:522-551` (access-control shell).
  - `doc/spec/spec_rev1.md` — flip the §6.1(c) `:417` map `[verifying]` part to mechanized
    (symmetric cap-side guarantee).
  - `doc/guidelines/verus_trusted-base.md` — flip the MAP `[verifying]` row `:115` to landed; update
    the Page-table-join row `:56` to note the cap-side *record* is now verified on both map and
    unmap (the join itself stays trusted); record the new `cargo verus verify -p kcore` total `:127`.
- **Depends on:** Part A blessed (rev1§2.5/§6.1(c) text). No intra-B8 dependency — parallel with
  B8B/B8C.
- **Work:** Design decision 1 — `ref_aspace` (reuse `unref_aspace`'s census vocabulary on the
  increment side); `map_frame` (mirror the delete Frame branch's `ensures`); the `Store::aspace_map`
  seam; the `Sys::Map` Ok-arm rewrite. Add a `test_store` host unit that a `map_frame` over an
  unmapped frame records `Some((asp, va))` and bumps `refs[asp]`, and that mapping an
  already-mapped frame is refused (the `mapping: None` precondition).
- **Acceptance:**
  - `map_frame` verifies with the delete-branch-symmetric `ensures` (cap mapping recorded, `refs[asp]`
    bumped, `cspace_wf`/`refcount_sound`/`caps_consistent` preserved, all other views framed); the
    `Sys::Map` Ok-arm no longer mutates `hdr.refs` or `cap.kind` directly.
  - `cargo verus verify -p kcore` **> 335/0** (record the new total).
  - `cargo test -p kcore` green; QEMU boot unchanged (map still works end to end).
  - rev1§6.1(c)'s map part and the ledger MAP row read mechanized/landed; the join stays trusted.
- **Effort/Risk:** M / medium. The proof is `delete`'s Frame branch run backwards — the census/frame
  machinery exists; the substance is getting `map_frame`'s `ensures` to mirror the unmap branch
  term-for-term and threading the new `Store::aspace_map` seam through the shell.

---

## Sub-phase B8B — verified priority-ceiling gate *(closes the spawn-time check [medium]; conforms rev1§5.4, §6.1(d))*

The gate deliverable, and the smallest sub-phase. Moves the spawn-time `prio > ceiling` *refusal*
out of the syscall shell into a verified branch of `set_priority`, composing on the
already-verified cap-carried ceiling attenuation. Independent of B8A and B8C. After B8B the
spawn-time check is mechanized end to end — attenuation (already proven) + refusal (new) — with no
unverified `if` deciding admissibility.

- **Touches:**
  - `kcore/src/thread.rs` — refactor `set_priority` `:222` to return `Result`, dropping
    `requires prio <= ceiling` `:225` and adding the internal verified refusal; strengthen the
    `ensures` to characterize both the Ok (write, `prio <= ceiling`) and Err (`prio > ceiling`,
    unchanged) cases (Design decision 2). Optionally read `ceiling = cap_max_prio(slot_view[cap])`
    inside the op (the tighter-seam variant) — coordinate with `cspace::cap_max_prio` `:1160`.
  - `kernel/src/syscall.rs` — delete the shell gate `if prio > max_prio { return ERR_PERM }` at
    `:456` (ThreadStart) and `:627` (ThreadStartAs); call the refusing `set_priority` and map
    `Err → ERR_PERM`, `Ok → 0` before `enqueue`. Update the `:462` doc-comment (the precondition is
    no longer "discharged by the shell `if`" — the op refuses).
  - `doc/spec/spec_rev1.md` — flip the §6.1(d) `:418` gate `[verifying]` part to mechanized; §5.4
    `:383` becomes true without an edit.
  - `doc/guidelines/verus_trusted-base.md` — flip the priority `[verifying]` row `:116` to landed;
    note in the Thread-lifecycle-shell row `:57` that the gate is now verified (the scheduler/asm/
    access-control parts stay trusted); record the new kcore total `:127`.
- **Depends on:** Part A blessed (rev1§5.4/§6.1(d)). No intra-B8 dependency — parallel with B8A/B8C.
  Builds on the already-verified `derive`/`derived_kind` ceiling attenuation (no change there).
- **Work:** Design decision 2 — the `set_priority` `Result` refactor + the two shell-site rewrites;
  decide and record the ceiling seam (passed `u8` vs read-from-`slot_view`). Add a `test_store` unit:
  `set_priority` with `prio > ceiling` returns `Err` and leaves `tcb_view` unchanged; with
  `prio <= ceiling` writes `prio`.
- **Acceptance:**
  - `set_priority` verifies with the Ok/Err-characterizing `ensures`; the two shell sites carry no
    `prio > max_prio` `if` — the refusal is the verified op's.
  - `cargo verus verify -p kcore` **> 335/0** (record the new total).
  - QEMU boot unchanged; a spawn requesting `prio > ceiling` still returns `ERR_PERM` (behaviour
    identical, refusal now verified).
  - rev1§6.1(d)'s gate part and the ledger priority row read mechanized/landed; §5.4 is satisfied.
- **Effort/Risk:** S–M / low. One signature change, one verified branch, two shell rewrites; the
  ceiling attenuation it composes on is already proven. The only judgment is the ceiling-seam
  variant (read-from-slot vs passed param).

---

## Sub-phase B8C — ready-queue list surgery *(closes the ready-queue [low–medium]; no spec [verifying] flip — Honesty note 4)*

The largest sub-phase: a fresh verified intrusive-list module, copying the
waiter-queue/armed-list templates. Moves the 32-level ready queue + bitmap into `kcore` with
`requires`/`ensures`, verifying the one intrusive list the kernel ships without a verified twin (and
the one `destroy_tcb` already trusts). Independent of B8A/B8B. After B8C the ready-queue operations
join the verified surface beside the notification waiter queue and timer armed list; the scheduler
policy and asm context switch stay trusted shell.

- **Touches:**
  - `kcore/src/cspace.rs` (or a new `kcore/src/ready.rs` module + `lib.rs` `:64` registration) — the
    `ready_chain`/`ready_seq`/`ready_wf` spec witnesses with the per-level + bitmap-coherence
    invariant (Design decision 3, modeled on `waiter_chain` `:1574` / `timer_chain` `:2538`); the
    `lemma_ready_remove_chain` splice lemma (⟵ `lemma_waiter_remove_chain`).
  - `kcore` — the four verified ops `ready_enqueue`/`ready_dequeue`/`ready_unqueue`/`top_ready`
    (⟵ `wait`/`signal`/`remove_waiter` `notification.rs:46,580` and `disarm` `timer.rs:69`); the
    `leading_zeros` ↔ top-non-empty-level bit-scan lemma.
  - `kernel/src/thread.rs` — rewrite `enqueue` `:90`, `dequeue` `:104`, `top_ready` `:117`,
    `unqueue_ready` `:129` as thin pointer-convert + `kcore` shell wrappers (the `cspace::delete`
    wrapper pattern); leave `maybe_switch` `:159` orchestrating them (trusted shell); keep
    `as_tcb`/`tcb_id` `:31-40` as the trusted link seam.
  - `kernel/src/store.rs` — route `unqueue_ready` `:253` (and `make_runnable` `:250`, the enqueue
    side) through the verified ops; `kcore/src/store.rs` `unqueue_ready` `:127`/`make_runnable` `:125`
    seam contracts gain the `ready_wf`-preserving `ensures` the verified `destroy_tcb` can rely on.
  - `doc/guidelines/verus_trusted-base.md` — add the **ready queue** to the verified-surface scope
    paragraph `:17-30` (beside "notification waiter queue, timer armed list"); record the new kcore
    total `:127`. **No `[verifying]` table edit, no §6.1 spec edit** (Honesty note 4).
- **Depends on:** Part A blessed. No intra-B8 dependency — parallel with B8A/B8B (recommend landing
  after them only to coordinate the shared kcore verify-gate count, not for any proof dependency).
- **Work:** Design decision 3 — the per-level witnesses + bitmap-coherence invariant; the four ops
  copied from the waiter-queue/armed-list templates; the splice-walk termination via the
  `timer_complete`-style completeness; the shell rewiring + the `unqueue_ready` seam tightening. Add
  `test_store` units exercising enqueue→top_ready→dequeue ordering (round-robin within a level), the
  arbitrary-position `ready_unqueue` splice, and the bitmap-coherence invariant (bit set iff level
  non-empty) across a randomized op sequence.
- **Acceptance:**
  - The four ops verify with `ready_wf`-preserving `ensures` (no duplicates/cycles, head/tail
    agreement, `qnext`-threading, per-element `priority`/`Runnable` covenant, bitmap coherence);
    `top_ready` proven to return the highest non-empty level (or `None` iff empty).
  - `destroy_tcb`'s `store.unqueue_ready(t)` resolves to the verified op (the seam `requires`/
    `ensures` are discharged by verified code, not trusted by assumption).
  - `cargo verus verify -p kcore` **> 335/0** (record the new total — the largest single bump);
    `cargo test -p kcore` green; QEMU boot unchanged (scheduling order identical).
  - The ledger scope paragraph names the ready queue; **no §6.1 prose changed**; the scheduler
    policy + asm switch remain trusted per §6.1(d).
- **Effort/Risk:** M / medium — the proof-engineering sub-phase. The templates exist
  (`remove_waiter`/`disarm` are the splice walk; `waiter_chain`/`timer_chain` the witnesses), so the
  novelty is the **per-level fan-out + the bitmap-coherence invariant** (the single-queue templates
  lack both) and the `leading_zeros` bit-scan lemma — bounded, but the largest of the three.

---

## Execution order

```
B8A  symmetric cap-side MAP            [§6.1(c); mirror of the delete/unmap path; independent]
B8B  verified priority-ceiling gate    [§6.1(d), §5.4; smallest; composes on verified derive; independent]
B8C  ready-queue list surgery          [audit §4.2; largest; copies waiter-queue/armed-list templates; independent]
```

- All three are **mutually independent** (different files, different proof surfaces) and each is a
  complete, mergeable unit — mirroring B5A/B5B/B5C, B6A/B6B/B6C, B7A/B7B/B7C. Order them by §6.1
  letter (c → d) then the unflagged ready queue; within the kcore wave they may land in any order or
  in parallel. The only coupling is the **shared `cargo verus verify -p kcore` gate**: each records
  its new total, and the last to land states the final ≥ 335 figure in the ledger baseline `:127`.
- **B8B is the smallest** (one `Result` refactor + two shell rewrites on top of already-verified
  attenuation) and the cleanest demonstration; **B8C is the largest** (a fresh verified module). B8A
  sits between — the delete-branch mirror.
- The parent plan sequences **B8 before B9 and B-IRQ** so this freshly-verified surface is not
  churned by the preemptible-revoke refactor (B9) or the new `CapKind::Irq` cap-set widening
  (B-IRQ). B8 is otherwise independent of B9/B10.

## Out of scope for B8 (recorded so it is not mistaken for a gap)

- **Verifying the page-table entry write/clear itself.** §6.1(c) keeps "the real writing and
  clearing of page-table entries … proven separately over raw page-table memory" (that is `map_in`/
  `unmap_in`, already done) and the **join** — that the cap's recorded `(asp, va)` is the true entry
  — trusted. B8A verifies the *cap-side record*, not the join; the raw entry write stays the
  `aspace_map`/`aspace_unmap` realization, trusted exactly as today.
- **Verifying the scheduler policy / `maybe_switch` / the asm context switch.** §6.1(d) keeps the
  "suspended, never rescheduled" state machine, the scheduler, and the asm switch trusted. B8C
  verifies the ready-queue **data structure**, not the scheduling **decision**; `maybe_switch`
  (`thread.rs:159`) stays trusted shell, the asm switch inherently unverifiable.
- **The `ObjId ↔ *mut Tcb` address identity** (`as_tcb`/`tcb_id`). Stays trusted by construction —
  the same `Store` link seam the verified notif waiter queue and timer armed list already use via
  `Option<ObjId>`. B8C does not eliminate it (Verus has no intrusive-raw-pointer model); it verifies
  the list invariants *over* it.
- **The anti-forgery/anti-suppression access control and the spawn-time cap-distribution
  convention** (§6.1(d), trusted). B8B verifies the *refusal* of an over-ceiling priority, not the
  rights gates or the convention that decides who holds which thread cap and ceiling.
- **No new syscall, no ABI change, no on-disk/wire change.** B8 is behaviour-identical (Honesty
  note 2): `sysabi` opcodes, the `Sys::Map`/`ThreadStart`/`ThreadStartAs` ABIs, and scheduling order
  are unchanged; `storaged`/`init`/`shell` see identical signatures. The QEMU boot is the regression
  gate alongside the Verus count. (Contrast B5's format-v4 bump; like B7, B8 changes no persistent or
  wire bytes.)
- **Preemptible/restartable revoke (M-1), aspace pool top-up (M-2), the IRQ-handler object.** Those
  are **B9**, **B10**, and **B-IRQ** — sequenced after B8 in the kernel wave. B8 owns only the three
  verified-surface items (cap-side MAP, priority gate, ready queue); it adds no preemption point, no
  top-up syscall, and no new cap kind.
- **A §6.1 ready-queue `[verifying]` flip.** There is none (Honesty note 4): the ready queue has no
  blessed `[verifying]` tag; B8C records its gain in the ledger scope paragraph + baseline only and
  makes no normative spec edit, leaving §6.1(d)'s "scheduler [trusted]" line literally true.
