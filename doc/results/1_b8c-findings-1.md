# B8C — Ready-queue verification: findings (part 1)

Working notes from the first implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3 — the 32-level ready queue moved into the
Verus-verified `kcore`, at the **full seam-integration** depth chosen for this work). This
records what landed, what remains (subdivided into resumable sub-phases), and the proof
techniques worth generalizing.

Branch `b8c-ready-queue` off the B8B merge (`origin/main` @ `24275bf`). Checkpoint commit
`6c25814`; draft PR #138. Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

The **conceptual core** of B8C is done and verified: the ghost view, the per-level
witnesses, all the chain/splice lemmas, the bitmap reasoning, and two of the four ops
(`top_ready` and `ready_enqueue` — the `make_runnable` path). `cargo verus verify -p kcore`
rose **342 → 362, 0 errors**; `cargo test -p kcore` is green (94 passed); `cd kernel &&
cargo build` is green. The change is **behaviour-identical**: the new ops are verified but
not yet wired, so `make_runnable`/`unqueue_ready` keep the old path and the QEMU boot is
unchanged.

What is **not** done: `ready_dequeue`, the `ready_unqueue` splice-walk, re-contracting the
two seam ops against the verified ops, threading the new invariants through `signal` and the
`destroy_tcb` teardown SCC, the kernel rewiring, and the op tests + ledger bump. The audit
item is therefore **not yet closed** — the running scheduler still uses the unverified list
logic.

The hard conceptual problems are solved and the heavy lemmas are reusable; the remaining
ops are more mechanical, **except** the `destroy_tcb` cascade, which is the genuine risk the
plan flagged.

---

## 1. What was done (verified, green)

### 1.1 The model: ready queue = per-level waiter queue + completeness + bitmap

The decisive modeling insight: the ready queue is, **per priority level**, structurally
identical to the notification *waiter queue* (head + tail + `Tcb.qnext` thread + tail-fixup
splice), with three substitutions and two additions:

| waiter queue | ready queue |
|---|---|
| notification `n` | priority `level: int` |
| `nv[n].wait_head` / `wait_tail` | `rv.heads[level]` / `rv.tails[level]` |
| covenant `wait_notif == Some(n) && state == BlockedNotif` | covenant `state == Runnable && priority == level` |
| (single queue) | **32 independent levels** |
| (no completeness clause) | **`ready_complete`** (timer-list discipline) |
| (no bitmap) | **`ready_bitmap_coherent`** (u32 presence map) |

The intrusive link is the *same* `Tcb.qnext` the waiter queue uses, disambiguated by state —
a thread is on the ready chain (`Runnable`) **or** a waiter chain (`BlockedNotif`), never
both.

**Two simplifications vs. the templates, both load-bearing:**

1. **Ready-queue membership carries no refcount.** A waiter holds `refs[n] += 1`; an armed
   timer holds `refs[notif] += 1`; a *ready* thread holds nothing. So the ready ops touch
   only `tcb_view` (`qnext`, `state`) and `ready_view` (heads/tails/bitmap), **frame
   `refs_view`, and skip the entire `obj_census` / `refcount_sound` / `census_delta_frozen`
   machinery** that dominates `wait`/`signal`/`remove_waiter`'s proofs. This is why
   `ready_enqueue` is materially simpler than `wait` despite the per-level fan-out.

2. **The census never reads Runnable threads.** `waiter_refs`/`obj_census` count threads
   with `wait_notif == Some(o)`; a Runnable thread has `wait_notif == None`. So enqueuing a
   thread (changing its `state`/`qnext` and the old tail's `qnext`) **cannot perturb any
   census term** — which is what will keep the `signal`/`destroy_tcb` *census* reasoning
   untouched in the cascade. The cascade only has to absorb the `tcb_view` equality change
   and carry the new `ready_*` conjuncts.

### 1.2 The artifacts (file by file)

- **`kcore/src/cspace.rs`**
  - `ReadyView { heads: Map<int,Option<ObjId>>, tails: Map<int,Option<ObjId>>, bitmap: u32 }`,
    beside `TcbView`/`TimerView`.
  - `spec fn ready_view()` on the `ExStore` extension; the six concrete seam-method
    contracts (`ready_head`/`set_ready_head`/… ) modeled on `timer_armed_head`.
  - **Frame sweep:** `final.ready_view() == old.ready_view()` added to every cspace setter
    and op ensures that frames `timer_head_view` (33 setters + 8 ops + 2 loop invariants).
  - Witnesses `ready_chain` / `ready_seq` / `ready_complete` / `ready_bitmap_coherent` /
    `ready_wf`; lemmas `lemma_ready_chain_eq_at` / `_not_strict_prefix` / `_unique` /
    `lemma_ready_remove_chain` (copied from the waiter set) and the per-level frame helpers
    `lemma_ready_chain_frame` / `lemma_ready_seq_frame`.
- **`kcore/src/ready.rs`** (new module, registered in `lib.rs`)
  - Bit lemmas `lemma_bit_set_eqv`, `lemma_set_bit_self/other`, `lemma_clear_bit_self/other`.
  - `top_ready` — `None` iff bitmap 0, else `31 - leading_zeros`, proven the highest
    non-empty level via vstd's `axiom_u32_leading_zeros` + bitmap coherence.
  - `ready_enqueue` — the verified core of `make_runnable`.
  - Spinoff lemmas `lemma_ready_push_wf` (the 32-level `ready_wf`/`ready_complete`
    re-establishment) and `lemma_ready_coherent_after_set` (the bitmap-coherence sweep).
- **`kcore/src/store.rs`** — six new base-`Store` methods.
- **`kcore/src/test_store.rs`** — `ArrayStore` ready backing (`ready_heads`/`ready_tails`
  vecs + `ready_bitmap`) + the six impls.
- **`kernel/src/thread.rs`** — by-handle accessors over `READY`/`READY_BITMAP`
  (`ready_head_at` etc.); **`kernel/src/store.rs`** — the six `KernelStore` realizations.
  (So the kernel builds. `make_runnable`/`unqueue_ready` realizations are **unchanged**.)

### 1.3 The constraint that shapes everything downstream

Today's seam contracts deliberately model the ready queue as *scheduler state below the
abstract `tcb_view`*: `make_runnable` (`cspace.rs`) ensures **only `t`'s `state` → Runnable**
and frames the rest; `unqueue_ready` is a **total no-op on every view**. A Runnable thread's
`qnext` is unconstrained by any verified predicate. Full integration overturns this, and the
overturn is the whole cost of the remaining phases (§2):

- A faithful `make_runnable` enqueues to the **tail**, which writes the *old tail's* `qnext`
  (a second TCB) — breaking `signal`'s "only the woken thread changed" ensures
  (`notification.rs`, 3 kcore callers).
- A faithful `unqueue_ready` is a real splice preserving `ready_wf` — so `destroy_tcb`
  (`#[verifier::rlimit(30)]`, `spinoff_prover`, anchor of the recursive SCC
  `delete → dec_ref → unref_cspace/destroy_cspace → destroy_tcb`) can no longer discharge
  its detach step with `lemma_sysinv_frame_equal_views` ("unqueue_ready frames every view").

The §1.1 simplifications (no refcount, census-blind) are what keep this cascade to the
`tcb_view`-equality + new-conjunct level rather than a full re-proof of the census cluster.

---

## 2. What is left (resumable sub-phases)

Ordered by dependency. Each is independently verifiable; the gate (`cargo verus verify -p
kcore`) stays green between them. Risk in parentheses.

### Phase B8C-1 — `ready_dequeue` + `ready_unqueue` (M, the splice-walk)

Complete the four verified ops, standalone (not yet wired).

- **`ready_dequeue(level)`** — pop head (`k = 0` removal), clear the bit if the level
  empties. Only `maybe_switch` (trusted) calls it, so its ensures need **not** carry
  `ready_complete` — just `ready_wf` + `ready_seq(level) == old.drop_first()` + the returned
  head. Reuse `lemma_ready_remove_chain` (the `k = 0` case).
- **`ready_unqueue(t)`** — the arbitrary-position splice walk, copied term-for-term from
  `remove_waiter` (`notification.rs`): `while cur.is_some()`, ghost rank `k`, `decreases
  rs.len() - k`, position invariants, head-vs-middle splice, tail-fixup, clear bit if empty.
  Requires `ready_wf` + `ready_complete` (so a Runnable `t` is *found* — the completeness
  totality). Ensures `ready_wf` + `ready_seq == old.remove(index_of(t))` + **`ready_complete`
  except `t`** (the op leaves `t` transiently Runnable-and-off-chain; `destroy_tcb` halts it
  after — see B8C-4).
- New support needed: `lemma_ready_coherent_after_clear` (the clear-bit analogue of
  `lemma_ready_coherent_after_set`, with an `ready_seq(rv0,level).len() > 0` precond so the
  still-non-empty-after case can use rv0 coherence) and a `lemma_ready_remove_wf` (the
  removal analogue of `lemma_ready_push_wf`). `lemma_clear_bit_self/other` already exist.

### Phase B8C-2 — re-contract `make_runnable` / `unqueue_ready` (M)

Replace the abstract seam contracts (`cspace.rs`) with the `ready_enqueue` / `ready_unqueue`
ensures lifted to the seam (require `ready_wf`/`ready_complete`, ensure them + the precise
`ready_seq` delta + the `tcb_view` delta, frame the rest). Update the **`ArrayStore`** host
model (`test_store.rs`) so `make_runnable`/`unqueue_ready` actually manipulate the ready
arrays and `check_*` validates the new contracts. (Leave the **`KernelStore`** realizations
to B8C-5.)

### Phase B8C-3 — `signal` + its 3 callers (M)

Add `ready_wf`/`ready_complete` to `signal`'s requires/ensures; **weaken** the wake-path
`tcb_view` ensures minimally to admit the old-ready-tail `qnext` write (characterize via the
`ready_seq` push, or an explicit two-key `insert`). Re-establish the three kcore callers:
`timer.rs` (timer fire), `channel.rs` (channel event), `thread.rs` (thread report). Keep the
weakening minimal so callers that only need the `wf` predicates are unaffected.

### Phase B8C-4 — `destroy_tcb` + the teardown SCC (L, highest risk)

Thread `ready_wf`/`ready_complete` through `destroy_tcb` and the functions that recurse into
it — `delete`, `dec_ref`, `unref_cspace`, `destroy_cspace`. Rework `destroy_tcb`'s detach
block: replace the `lemma_sysinv_frame_equal_views` step with the new `unqueue_ready`
contract (splice preserves `ready_wf`; non-tcb/non-ready views framed; `tcb_view` changes
only at `t` and the live predecessor — `dead_tcb_frozen`/`home_views_frozen` constrain only
*dead* TCBs / residency, so a live predecessor's `qnext` move is fine). After the unqueue,
`destroy_tcb` halts `t` (state → Halted), which discharges the **`ready_complete`-except-`t`**
back to full completeness. Expect to **bump `rlimit`** and likely add a small
`lemma_ready_wf_frame` (carry `ready_wf` cheaply across a step that frames `ready_view` +
the relevant `tcb_view`). The §1.1 census-blindness should keep the existing census lemmas
untouched — verify that assumption early.

### Phase B8C-5 — kernel rewiring (S–M)

Rewrite the kernel `enqueue`/`dequeue`/`top_ready`/`unqueue_ready` wrappers (`thread.rs`) as
thin pointer-convert + `kcore::ready::*` calls via `KernelStore` (the `cspace::delete`
wrapper pattern); route the `make_runnable`/`unqueue_ready` `KernelStore` realizations to
the verified ops. `maybe_switch` stays trusted shell (it keeps orchestrating the four ops).
Confirm `cd kernel && cargo build` and the QEMU boot smoke are unchanged.

### Phase B8C-6 — tests + ledger + final gate (S)

`test_store` units: enqueue → `top_ready` → dequeue round-robin-within-a-level ordering;
arbitrary-position `ready_unqueue` splice; bitmap coherence across a randomized op sequence;
extend `check_destroy_tcb` to assert `ready_wf` + a Runnable `t` is spliced out. Ledger
(`verus_trusted-base.md`): add the ready queue to the verified-surface scope paragraph
(beside "notification waiter queue, timer armed list") and bump the kcore baseline to the
final total. **No `[verifying]` table row, no §6.1 / spec prose edit** (Honesty note 4 — the
ready queue has no blessed `[verifying]` tag).

---

## 3. Generalizable proof techniques

These came out of the verification grind and apply beyond B8C.

1. **Adding a global ghost view = mirror an existing global scalar's frame discipline,
   exactly.** A new `Store` view that every object op leaves alone (here `ready_view`, like
   `timer_head_view`) is introduced by `grep`-ing every `X.timer_head_view() ==
   Y.timer_head_view()` line and adding a sibling `X.ready_view() == Y.ready_view()`. The
   `grep` *is* the completeness checklist.
   - **Corollary (a real bug I hit):** do **not** mirror it into framing-*lemma* `requires`.
     A lemma like `lemma_caps_consistent_frame` requires a pile of view-equalities but its
     *conclusion* (`caps_consistent`) doesn't read the new view — adding it there only
     manufactures spurious obligations at every call site (and an rlimit blow-up in
     `remove_waiter`). Mirror it into **ensures (frame guarantees)**, not into requires of
     lemmas whose conclusion ignores it.

2. **Triggerable existence — the single highest-leverage fix.** A well-formedness conjunct
   `forall level: ... ==> exists rs. P(rs)` is *unprovable on re-check* if its body mentions
   the trigger anchor only in the `#![trigger ...]` annotation, not in the body: when Verus
   re-proves the `forall`, the witness term is never surfaced, so the stored fact never
   fires. **Fix:** state the conjunct as `forall level: ... ==> P(seq(level))` where
   `seq = choose|rs| P(rs)` is the deterministic selector. Now the body contains
   `seq(level)` (the trigger), it is re-provable without a witness-surfacing `by`-block, and
   it is *stronger* (gives the canonical witness, not mere existence). This is exactly what
   unblocked `ready_wf` after ~6 failed attempts.

3. **Decompose for resource with `spinoff_prover` lemmas keyed on local post-state facts.**
   When an op's `wf` re-establishment blows `rlimit` even at 150, extract it into a
   `#[verifier::spinoff_prover] #[verifier::rlimit(N)]` lemma whose **requires are the cheap,
   local facts the op body can prove** (the pushed/spliced chain at one level, the per-level
   head/tail/bitmap frame, the tcb frame) and whose **ensures is the global invariant**. The
   op body then proves only the local facts and calls the lemma. Recurse: when *that* lemma
   is still too big, split off the per-concern sub-sweep (here the 32-way bitmap coherence →
   `lemma_ready_coherent_after_set`).

4. **Bitwise facts: per-element `bit_vector` lemmas, never `forall`-over-bits.** A bitwise
   `&`/`<<`/`|` is **not a valid Verus trigger**, so you cannot write
   `forall j: #![trigger x & (1<<j)] ...`. Instead prove small parameterized lemmas
   (`lemma_set_bit_other(x, k, j) requires j != k ensures (x | (1<<k)) & (1<<j) == x &
   (1<<j)`, body `by (bit_vector) requires ...`) and **instantiate them per level** inside
   the coherence `assert forall`. Confine `bit_vector` to those tiny lemmas; reason through
   their contracts above (the `verus.md` packed-bitmap recipe, applied to a single `u32`).

5. **Bridge `int`-level and `u32`-bit reasoning with explicit cast-equality asserts.** When
   a forall ranges over `lv: int` but the bit lemma speaks `lv as u32`, Verus will not
   silently equate `lv as u32` with `level as u32` even when `lv == level`. Assert
   `lv as u32 == level as u32` (and `!=` in the other branch) explicitly.

6. **Case-split on chain membership, not the frame, for an element whose fields moved.** In
   `ready_enqueue`'s completeness proof, the *old tail* changed (`qnext`) so it is **not**
   covered by the "every other TCB unchanged" frame — yet it must stay charted. Splitting on
   `pushed_seq.contains(x)` (chain elements get their covenant from `ready_chain`; non-chain
   elements are framed) handles `t`, the old tail, and everyone else uniformly. The
   `lx != level` sub-case is discharged by contradiction (a Runnable thread at `level` would
   be in `rs0 ⊆ pws`).

7. **Keep transient-invalidating liveness separate from structural `wf`, with an "except-t"
   carve-out.** `ready_unqueue` leaves `t` Runnable-and-off-chain until `destroy_tcb` halts
   it, so `ready_complete` *cannot* be a clean `wf` conjunct preserved by the op. Keep it a
   **separate predicate**, have the op preserve only `ready_complete`-except-`t`, and let the
   caller close the gap by removing `t` from the Runnable set. This mirrors `destroy_tcb`'s
   existing `dead_tcb_frozen`-except-`t` discipline — a reusable pattern for any op that
   transiently violates a global liveness invariant for one object.

8. **Bundle a structural bound into the liveness predicate that travels with it.** Threads
   need `priority < NUM_PRIOS` for `ready_seq(priority)` to be meaningful, but threading a
   standalone `prio_bounded` conjunct through the whole cascade is costly. Folding
   `tv[t].priority < NUM_PRIOS` *into* `ready_complete` (which already quantifies over
   Runnable threads) gives it for free wherever completeness is carried.

9. **Verify intrinsics via `vstd::std_specs`, not a new `assume_specification`.** `top_ready`
   needs `u32::leading_zeros`; vstd ships `axiom_u32_leading_zeros` (`i == 0 ⟺ lz == 32`;
   bit `31-lz` set; bits above clear). `broadcast use` it — this verifies the bit-scan
   **without widening the local trusted base** (the four `external_body` + the
   `assume_specification`s stay untouched, per the plan's honesty note 1).

10. **Match asserted-`forall` ranges and triggers *exactly* to the target conjunct.** A
    `forall` proven with range `0 <= l < NUM_PRIOS` does not always discharge a conjunct
    written `0 <= level < NUM_PRIOS as int`, and trigger mismatches silently prevent reuse.
    When proving a `wf` predicate, write the helper `assert forall` with the predicate's
    *verbatim* range, trigger, and body shape.

---

## 4. Gate state at checkpoint

| gate | before | after |
|---|---|---|
| `cargo verus verify -p kcore` | 342 / 0 | **362 / 0** |
| `cargo test -p kcore` | green | green (94 passed) |
| `cd kernel && cargo build` | green | green |
| QEMU boot | — | unchanged (ops verified, not wired) |

`external_body` seams and `assume_specification`s: **unchanged** (none added).
