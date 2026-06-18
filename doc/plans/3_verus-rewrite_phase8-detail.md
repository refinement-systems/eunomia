# Plan detail: Verus phase 8 — the commit-protocol recovery core (§4.8)

**Status: proposed.** This is the per-phase detail for the next step of the
Verus rewrite (`doc/plans/3_verus-rewrite.md` §4.8). It is the master plan's
one *complement-to-TLA+* target rather than a Kani migration: it extracts the
**pure recovery decision core** from `cas::store` and proves the
`AckedWritesRecoverable` headline against the real bytes, closing the
model-to-code gap the `CommitProtocol` TLA+ spec cannot reach. The commit
protocol is, with the cap machinery, where the system's correctness pivots
(spec §6), so it gets the same mechanized tier as the kernel core — this work
is **mandatory, not optional** (master plan §4.8).

---

## Phase-number reconciliation (the slip)

The master plan's §7 phasing has drifted by one from the implementation, and
this phase is where the slip lands on the *name*. In the original §7 numbering
**step 7 was commit recovery and step 8 was the closeout**. But §4.1 had folded
the entire teardown cluster (`delete`/`revoke`/`destroy_*`/`obj_unref`) into a
single line of phase 2, and it could not be proven until *every* object type
existed — the mutual recursion `delete → obj_unref → destroy_{cspace,channel,tcb}
→ delete` spans channel + thread, and the `refcount_sound` census assembles
terms landed across phases 3/4/5. So it grew into a whole **phase 6** (16
findings docs, 41–56), pushing host chokepoints to **phase 7** and everything
after it by one. The phase 7 detail doc recorded the resulting map; the tail of
it is what this phase inherits:

| Plan §7 step | Actual repo phase | Status |
|---|---|---|
| *(folded into §4.1)* | **phase 6** cross-object teardown + refcount census (6a–6f, docs 41–56) | done |
| 6 host chokepoints (§4.7) | **phase 7** (7a–7g, docs 57–63) | done |
| **7 commit recovery (§4.8)** | **→ phase 8 — NEXT (this doc)** | proposed |
| 8 closeout | → phase 9 | pending |

**So: phase 8 is no longer the closeout — it is the commit-protocol recovery
core.** The closeout (spec §6, `CLAUDE.md`, the `0_kani-rewrite.md` banner)
slips to **phase 9**. CLAUDE.md already states the destination ("only the
commit-protocol recovery core (§4.8, phase 8) and the spec/`CLAUDE.md`/Kani
closeout (phase 9) remain").

---

## What this phase is *not* (the honest framing)

Phases 7a–7g each retired a Kani harness; **phase 8 retires nothing** — Kani
was fully retired in 7f, and the commit protocol never had a Kani harness (it
was always too `Vec`/`std`-heavy; CBMC OOM'd, as `cas::tlv` did). The incumbent
tiers for commit/recovery are **TLA+ `CommitProtocol`** (the design gate, TLC-
checked) and the **crash-injection proptest** `crash_recovery_preserves_acked_state`
in `store.rs` (the code-level differential coverage). **Verus is purely
additive here** (master plan §4.8): it neither replaces TLA+ (design) nor the
proptest (differential) — it closes the *gap between them*. Both stay.

The gap is the classic model-to-code distance. TLA+ proves the **protocol** —
an abstract model where writes are version numbers per ref and content is
last-write-wins — maintains `AckedWritesRecoverable` at every step. The proptest
samples the **real bytes** at finitely many crash points. Neither proves that
the *real decision functions* faithfully implement the protocol for **all**
inputs. That is exactly what Verus does, and only on the part that is pure
decision logic — see the scope split below.

---

## Scope: what the recovery core is, in the real code

The recovery path is `Store::mount` (`store.rs:388`) — mount and crash recovery
are the same code (§4.5). Its decision logic, and its dual on the write path
(`Store::commit`, `store.rs:976`), is what §4.8 names the "pure recovery
decision function (pick-survivor-superblock, replay-bound computation)." Three
pieces, each already nearly pure:

1. **Survivor selection** (`mount:394–419`). Decode both slots, discard the
   invalid, take the higher generation. The TLA+ `LiveSlot` / `OlderIsA`. Pure
   over `(generation, valid)` of the two decoded slots — the byte decode itself
   (`decode_checked`) and its totality + geometry validation are **already
   proven** (phase 7f). Its dual is `commit`'s A/B target choice (`commit:1014–1019`):
   always write the *older* slot, never overwrite the live commit — the code-side
   witness of the TLA+ `Crash` three-outcome safety (a torn write damages only
   the slot being written).

2. **Head advance** (`commit:988–1003`). After barrier 1, pop the contiguous
   *flushed* prefix of the WAL record queue; the new `wal_head`/`wal_next_seq` is
   the front of what remains (or a reset to offset 0 + current seq when the log
   drains — the WAL is linear, not circular, §4.4). The TLA+ `CommitPrepare`
   `newHead` ("longest contiguous prefix of records whose effects are flushed").
   Pure over the record-meta sequence `[(seq, off, flushed)]`.

3. **Replay bound** (`mount:509–549`). From `wal_head`, read contiguous,
   checksummed, seq-continuous records, stopping at the first torn or
   seq-discontinuous one (an unacked tail). The TLA+ `Recover` action. Pure over
   the WAL byte buffer + `(wal_head, wal_next_seq)`, modulo the `blake3` record
   checksum (the assumed-total seam — see below).

Everything *else* in `mount`/`commit` — the `BlockDev` reads/writes and the two
fsync barriers, the `ChunkStore` index frame, the prolly `tree::*` ops in
`flush_ref`, `RefTable::decode`, `apply_to_overlay`, the chunk store — is I/O-
bound or content-layer and stays plain Rust, outside the proof surface. This is
the same split 7f/7g drew: a `Hash`-free verified core fed already-decoded
scalars/slices, with thin plain-Rust delegators around it (`RawSuperblock`,
`RawEntry`). The recovery core extends that discipline to a *decision* function.

---

## The scope split, stated precisely (what Verus proves vs. what stays in TLA+)

`AckedWritesRecoverable` (TLA+) is: *every acked write `v` of ref `r` is either
`≤ LiveSlot.refRoots[r]` (covered by the committed root) or has a WAL record at
an index `> LiveSlot.walHead` (replayed).* Proving the *full* invariant on real
bytes would require relating "version covered by the committed root" to actual
prolly-tree content under last-write-wins — i.e. dragging the chunk store, the
tree, and `apply_to_overlay` into the proof. That is precisely the content layer
TLA+ abstracts to version numbers, and precisely what Verus cannot cheaply
reach. So the obligation splits honestly:

- **Verus (this phase): the structural / arithmetic half.** The three decision
  functions implement the protocol faithfully ∀ inputs: survivor = the higher-
  generation valid slot (deterministic under distinct generations); the commit
  target is never the live slot (so a torn write preserves `AtLeastOneValidSlot`
  by construction); the head advances past *only* flushed records and never past
  an unflushed one; replay from the head reconstructs exactly the maximal
  contiguous seq-run; and the **round-trip** — feeding `commit`'s computed head
  into `mount`'s replay re-applies exactly the records `commit` left pinned. Plus
  totality / termination / overflow-freedom / in-bounds for all three, ∀ bytes.

- **TLA+ (unchanged design gate): the content-coverage half.** That "flushed ⇒
  effects in the committed root" and "covered version need not replay" — the
  last-write-wins content semantics. The `CommitProtocol` spec already proves
  this maintains `AckedWritesRecoverable`, partial flushes included (`Refs ≥ 2`).

- **proptest (unchanged differential coverage): the seam between them.** The
  crash-injection `crash_recovery_preserves_acked_state` exercises the two halves
  *composed* against real tree content at sampled crash points, and stays the
  first-line debugging tier (concrete failing inputs, master plan §9).

The crux Verus *does* land is the gap-freedom lemma: **no record between the old
head and the new head is unflushed** (a `commit` invariant) ∧ **replay starts at
the new head** (a `mount` fact) ⇒ every unflushed (acked-but-uncommitted) record
is replayed. That implication is the code-level shadow of `AckedWritesRecoverable`,
and it is pure index/sequence arithmetic — Verus's sweet spot.

---

## The extraction (the real work, per master plan §4.8)

"Cleanly extracting that pure function from `store.rs` is part of the work, not
a precondition" (§4.8). The pattern is set by 7f/7g: lift the decision logic into
`verus!{}` functions over plain scalars/slices/`Vec`, returning `Hash`-free
results, and make `mount`/`commit` thin callers.

- **`pick_survivor(gen_a, valid_a, gen_b, valid_b) -> Survivor`** — replaces the
  `match decoded { … }` arms at `mount:394–419`. `Survivor` is an in-block enum
  (`SlotA`/`SlotB`/`Neither`) mapping 1:1 to the existing control flow (the
  7g `TlvErr`-maps-to-`FormatError` trick — an external enum can't be
  *constructed* inside `verus!{}`).
- **`commit_target(sb_in_b) -> Slot`** and the head computation
  `advance_head(records: &[RecMeta], wal_seq) -> (u64, u64)` — pure over the
  record metas (`RecMeta` is crate-local already, `store.rs:307`).
- **`replay_bound(wal: &[u8], wal_head, wal_next_seq) -> ReplaySpan`** — the
  loop at `mount:513–549` as a verified function returning the accepted span
  (count, end offset, end seq) without touching the overlay; `mount` then
  re-walks that span to `apply_to_overlay` (plain Rust). This is the 7g
  `decode_raw` move: a verified parser core, a plain-Rust applier.

`cas` already carries vstd with the `alloc` feature (added in 7g for the TLV
`Vec`), verified `--no-default-features`, and `verus!{}` blocks live in
`disk.rs` and `prolly.rs`. Phase 8's core most naturally lands in **`store.rs`'s
own `verus!{}` block** (new) or a small `recovery.rs` — TBD at 8a by where the
`RecMeta`/`Superblock` types sit cleanest. blake3 stays the **assumed-total
seam** (one `#[verifier::external_body]` over the WAL record checksum `Hash::of`,
exactly as 7f did for the superblock — totality needs no collision-freedom).

---

## What Verus verifies

| Function | Verus `ensures` (∀, total, terminating) |
|---|---|
| `pick_survivor` | total over all `(gen, valid)`; returns the valid slot of higher generation; both-valid ⇒ the strictly-higher one under the `gen_a != gen_b` precondition (the TLA+ `GenerationsDistinct` — distinct generations make `LiveSlot` deterministic); exactly-one-valid ⇒ that one; neither-valid ⇒ `Neither` (the `NoSuperblock`/`UnsupportedVersion` refusal) |
| `commit_target` | the written slot is **never** the current live slot (A/B alternation); so a crash mid-write can damage only the non-live slot ⇒ `AtLeastOneValidSlot` preserved by construction (the code witness of TLA+ `Crash` safety) |
| `advance_head` | new head = offset of the first non-flushed record, or the reset sentinel `(0, wal_seq)` when all flushed (linear-WAL reclaim, §4.4); **every record below the new head is flushed** (contiguous prefix); the record at the head, if any, is unflushed; head monotone (`≥` old head, modulo the reset); no overflow |
| `replay_bound` | **totality** ∀ bytes (no panic/OOB; the `off += rlen` stays `≤ wal.len()` because `decode_record` matched only in-bounds — `store.rs:539`'s comment becomes a theorem); **termination** (`decreases` on remaining buffer — `rlen ≥ WAL_HEADER > 0`, the 7g opt-loop recipe); accepts exactly the maximal contiguous seq-run from `wal_next_seq` (stops at first torn / seq-mismatch); the OVL-1 extent-range and seq-exhaustion forgery gates (`mount:524–546`) are total rejections |
| **composition** | `replay_bound(wal, advance_head(records).0, …)` re-applies exactly the records `advance_head` left pinned — the gap-freedom lemma: no unflushed record sits below the replay head, so every acked-uncommitted write is recovered (the code-level shadow of `AckedWritesRecoverable`) |

---

## Sub-phasing

One crate, one §, but sub-phased by **extraction risk** (the master plan's
de-risk-first discipline, re-applied): bank the extraction pattern on the
cleanest decision first, then the sequence reasoning, then the variable-length
buffer, then the composition. Each is one PR, green in CI before the next rests
on it, with a `doc/results/64+_verus-findings.md` writeup. They may collapse
into fewer PRs if the extraction proves clean.

### 8a — survivor selection (`pick_survivor` + `commit_target`)

The smallest, cleanest decision (pure scalar logic over two `(gen, valid)`
pairs) — the 7f-geometry analogue. Lands the `store.rs` `verus!{}` block / the
`Survivor` and `Slot` in-block enums, the `external_type`/`TlvErr`-style mapping
to the existing control flow, and confirms vstd-with-`alloc` still erases under
the userspace cross-build now that a *third* `cas` module carries proofs (the
storaged binary edge — re-confirm, the standing phase-7 risk). Banks the
workflow before any sequence proof.

### 8b — head advance (`advance_head`)

The contiguous-flushed-prefix computation over `&[RecMeta]`, with the linear-WAL
reset. Pure sequence reasoning (a loop invariant: everything popped is flushed;
the front of the remainder is not) — structurally the prefix-scan kcore already
did for the channel FIFO head and `cdt_unlink`'s sibling walk.

### 8c — replay bound (`replay_bound`) — the hard one

The variable-length WAL buffer: totality + `decreases` over arbitrary bytes, the
maximal-contiguous-seq-run characterization, the forgery gates. Reuses 7g's
`decode_raw` totality + `decreases` recipe and the `RawEntry`-style `Vec`
discipline directly. This is the phase's single hardest proof (the byte-level
parser totality plus the span characterization); sequenced after the workflow is
banked, with the existing replay loop as the fallback (no regression — it is
already exercised by the proptest and mount itself).

### 8d — the composition theorem + closeout note

The gap-freedom round-trip (`advance_head` ∘ `replay_bound`), and the
`doc/results` writeup stating plainly that this is **additive** to TLA+ (the
content-coverage half stays the `CommitProtocol` design gate; the proptest stays
the differential seam). No spec/`CLAUDE.md` edits here — those are phase 9 — but
8d records the precise TLA+↔code correspondence (`LiveSlot`→`pick_survivor`,
`CommitPrepare.newHead`→`advance_head`, `Recover`→`replay_bound`,
`AckedWritesRecoverable`→the gap-freedom lemma) so phase 9's closeout can cite it.

---

## CI / pinning deltas

- **No new `-p` and no new job.** `cargo verus verify -p cas --no-default-features`
  already runs in the `verus` CI job (since 7f). Phase 8 only *adds obligations*
  under it — and there is no per-proof filter, so each new `verus!{}` function
  auto-gates, as today.
- **No Kani change.** Kani was retired wholesale in 7f (job + install dance +
  scaffolding gone). Nothing to delete.
- **No Verus upgrade.** Stays pinned at `0.2026.06.07.cd03505` /
  `vstd =0.0.0-2026-05-31-0205`; an upgrade would be its own PR.
- **The `host-tests` `cas` leg is unchanged** — `crash_recovery_preserves_acked_state`
  and `crash_mid_gc_loses_no_data` stay as the differential / regression guard of
  the (now partly proven) recovery core, exactly as `test_store` stayed after
  phase 6 proved the teardown contracts.

---

## Risks specific to phase 8

- **`store.rs` is `std`/`Vec`/`Box`-heavy (chief).** Mitigated exactly as the
  master plan §4.8/§9 prescribes and 7f/7g demonstrated: Verus is scoped to the
  *extracted pure decision functions*, not the module. The I/O, the chunk store,
  the tree, and `apply_to_overlay` never enter the proof surface — they stay
  plain-Rust delegators around a `Hash`-free verified core.
- **The content-coverage half is deliberately out of scope — not a hidden gap.**
  "Flushed ⇒ in the committed root" stays the `CommitProtocol` TLA+ obligation
  and the proptest's seam. The doc/results writeup (8d) must state this boundary
  explicitly so the proof is not read as proving more than it does.
- **The composition round-trip (8d) is the genuinely hard proof** — it must
  relate two independently-extracted functions (`advance_head`, `replay_bound`)
  through the same record sequence. Fallback: land 8a–8c (each valuable
  standalone) even if the composition lemma needs to stay a documented prose
  argument + the proptest, rather than block the phase. (This is the §4.8
  parallel of phase 6's "system-clause as a recorded follow-on" — the per-piece
  contracts can land before the composed theorem.)
- **The extraction churn touches `mount`/`commit`** (the boot-critical recovery
  path). Mitigated by the on-OS / host gates: `crash_recovery_preserves_acked_state`,
  `crash_mid_gc_loses_no_data`, and `scripts/boot-test.sh` must stay green
  through the refactor — a regression shows up as a recovery failure immediately,
  before any proof rests on the new shape (the master plan §9 "gated by tests,
  not proofs" discipline).

---

## Explicitly *not* in this phase

- **Phase 9 — closeout.** Spec `2_spec_rev2.md` §6 (un-defer the Verus row /
  strike the Kani row), `CLAUDE.md` (the tier table + `### Verus` section), and
  the `0_kani-rewrite.md` closeout banner (master plan §8, §11). Phase 8 is the
  last *proof* phase; phase 9 is documentation only.
- **The content / tree layer.** The prolly tree, chunk store, `apply_to_overlay`,
  last-write-wins coverage — TLA+ (design) + proptest/fuzz (differential) keep
  these (master plan §10).
- **Rewriting `store.rs` / `disk.rs` wholesale** — only the extracted recovery
  core is in scope (master plan §10).
- **The commit concurrency** (the single-thread commit critical section vs.
  concurrent writers, the TLA+ `Flush`/`Commit` mutual exclusion) — that is the
  design tier's, not Verus's (master plan §2).
