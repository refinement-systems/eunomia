# Verus findings 47 — Phase 8d: `cas::store` composition (gap-freedom round-trip + maximal-run equality)

Plan: `doc/plans/3_verus-rewrite.md` (§4.8) and
`doc/plans/3_verus-rewrite_phase8-detail.md` (§8d). Prior increment: `66` (phase
8c — `replay_bound`, the replay-bound decision: totality + termination ∀ bytes).
Phase 8 is the master plan's one *complement-to-TLA+* target: extract the **pure
recovery-decision core** from `cas::store` and prove it implements the commit
protocol faithfully ∀ inputs, closing the model-to-code gap that the
`CommitProtocol` TLA+ spec (design) and the crash-injection proptest (sampled
bytes) leave open. It is sub-phased by extraction risk 8a→8d; **8d is the last
piece** — it lands the two things 8a–8c deferred to it: the **maximal-run
equality** (the closed-form characterization of `replay_bound`, deferred in 8c)
and the **gap-freedom composition** (`advance_head ∘ replay_bound`, the code-level
shadow of `AckedWritesRecoverable`).

**This phase retires nothing.** Kani was fully retired in 7f, and the commit
protocol never had a Kani harness (always too `Vec`/`std`-heavy — CBMC OOM'd).
Verus here is **purely additive** (master plan §4.8): it neither replaces TLA+
(the design gate) nor the proptest (the differential seam) — it closes the gap
*between* them. Both stay. **8d makes no spec / `CLAUDE.md` / `0_kani-rewrite.md`
edits** — those are phase 9, the documentation-only closeout.

`cargo verus verify -p cas --no-default-features`: **58 verified, 0 errors** (was
53 in 8c; the +5 covers `run_len`, the `laid_out` linking invariant, and the three
composition lemmas `lemma_laid_out_mono` / `lemma_run_len_covers` /
`lemma_gap_freedom`). `cargo test -p cas`: green — the crash-injection proptests
`crash_recovery_preserves_acked_state` / `crash_mid_gc_loses_no_data`, the
`wal_replay_scan` / `mount_recovery` fuzz corpora, and **all ten** `mnt1_*` /
`ovl1_*` forgery regressions (incl. `mnt1_forged_wal_seq_max_rejected`) run the
unchanged `mount`/`commit` exec paths (8d adds only ghost spec/proof, which
erases). `cargo test --workspace --exclude kernel`: green. `cd kernel && cargo
build` (rebuilds the user binaries) + the `user/storaged` aarch64 cross-build:
green — the now-two `store.rs` `verus!{}` blocks erase into storaged's no_std build
(the standing phase-7 vstd-with-`alloc` risk, re-cleared). `bash
scripts/boot-test.sh`: **BOOT TEST PASS** — the boot-critical recovery path
end-to-end.

The headline: **the composition theorem discharged formally** — no fall back to
the prose-argument escape hatch the plan reserved (detail §8d Risks). The §4.8
recovery core is now a closed deductive artifact.

---

## 1. The maximal-run equality (the closed-form 8c deferred)

8c proved `replay_bound`'s **totality** (`end_off <= wal.len()` ∀ bytes) and
**termination**, but left the *tight* characterization — that the walk accepts
*exactly* the maximal contiguous seq-run — to 8d, because stating it needs
spec-level byte decoding (doc 66 §4). 8d lands it as a second postcondition:

```
r.count == run_len(wal@, wal_head as int, wal_next_seq)
```

where `run_len` is the recursive spec fn defining that maximal run: from `off` at
`seq`, a record is accepted iff it frames (`frame_at`), continues the sequence,
and passes the content seam (`content_ok_spec`); the run ends at the first that
fails. The `seq == u64::MAX` record is *counted* but ends the run (the sequence
can't advance) — matching `replay_bound`/`mount` exactly (the
`mnt1_forged_wal_seq_max_rejected` corner, doc 66 §3). It is `decreases wal.len() -
off` (each accepted record's `rlen >= WAL_HEADER > 0` shrinks the buffer).

**The reader spec bridge — cheaper than 8c predicted.** `decode_frame` reads
`seq`/`len` via `disk.rs`'s `read_u64_le`/`read_u32_le`, which carried no
`ensures`. 8c predicted the bridge would need a `by (bit_vector)` step (the
`prolly.rs:648` recipe, since vstd's `spec_u32_from_le_bytes` is a closed
subslice spec). It did not: defining the ghost value `spec_u32_le`/`spec_u64_le`
as the **same shift-form expression** as the exec reader makes each reader's
`ensures r == spec_u32_le(buf@, off as int)` *definitional* — the exec body and
the spec body are syntactically identical, so `broadcast use group_slice_axioms`
(bridging `buf[i] == buf@[i]`) closes it with no bit-vector reasoning. These two
`pub(crate) open spec fn`s live in `disk.rs` next to the readers; adding the
`ensures` is additive to the 7f-verified readers (no body change, re-verified
clean).

**`frame_at` — the spec mirror of `decode_frame`.** A `spec fn frame_at(wal:
Seq<u8>, off: int) -> Option<(u64, nat)>` mirroring the magic + `seq` + `len` +
in-bounds parse over `spec_u*_le`; `decode_frame` gains a second `ensures` tying
its exec result to it ∀ bytes. The one corner is the 32-bit-`usize` overflow of
`WAL_HEADER + len`: `decode_frame`'s `checked_add` rejects it, and `frame_at`'s
in-bounds clause `off + WAL_HEADER + len <= wal.len()` already excludes it
(such an `rlen` exceeds any real `wal.len() <= usize::MAX`), so the two agree on
every arch — one `assert(wal@.len() <= usize::MAX)` in the overflow arm.

**The accumulator-invariant proof.** `replay_bound`'s loop carries `count +
run_len(wal@, off as int, seq) == run_len(wal@, wal_head, wal_next_seq)`: each
accepted record unfolds `run_len` once (`1 + run_len` at the next offset/seq), and
each stop point leaves `run_len(wal@, off, seq) == 0`, so `count` equals the total
at every exit. Two structural notes:

- The accumulator is **`invariant_except_break`**, not a plain `invariant`: at the
  seq-exhaustion (`seq == u64::MAX`) break, `count` is bumped past a record whose
  `run_len` tail is not yet zero, so the equation transiently does not hold there.
  The loop `ensures count == total` (proven at every break) states what *does* hold
  at exit. The four "stop" breaks discharge it via `run_len == 0`; the MAX break
  via the pre-bump unfold (a counted MAX record contributes exactly 1).
- A `let wlen = wal.len();` before the loop **materializes `wal@.len() <=
  usize::MAX`** (a real slice length fits usize — a fact Verus only gets from an
  exec `.len()` call), carried as a loop invariant `wlen == wal@.len()`. 8c's loop
  re-called `wal.len()` each iteration, getting this implicitly; 8d hoists it so
  the cast `wal_head as usize` and the `off + rlen` non-overflow stay provable
  without re-deriving it per record.

---

## 2. The gap-freedom composition (the genuinely hard proof — landed formally)

`advance_head` (write path, over a `&[RecMeta]` queue) and `replay_bound`
(recovery path, over raw WAL bytes) operate on **different views**. The
composition relates them through `laid_out`, the **linking invariant**: the byte
region at a record's `off` *is* the record the queue describes — it frames at
`off` with that `seq`, is content-valid, has `seq < u64::MAX` (an honest log never
wraps the 64-bit counter), and is laid out contiguously and seq-continuously into
the next record. `laid_out` is recursive over the suffix from index `k`, so the
coverage proof is a structural induction.

Three lemmas, then the theorem:

- **`lemma_laid_out_mono`** — `laid_out` from `k` carries to any later index `m`
  (each unfold exposes `laid_out` at the next index). So `laid_out(.,0)` gives
  `laid_out(., n_flushed)` for the head `advance_head` picks.
- **`lemma_run_len_covers`** — the crux induction: from a laid-out record `k`,
  `run_len(wal, records[k].off, records[k].seq) >= records.len() - k`. Each step
  unfolds `run_len` once (`frame_at` Some, sequence match, content ok, `seq <
  u64::MAX` ⇒ the `1 + …` step, not the seq-exhaustion stop) and chains
  contiguously into `k+1` by the IH. It is a **lower bound**, not equality: the
  WAL may hold further valid records past the queue, and replay covering *more*
  never drops an acked record — which is all gap-freedom needs.
- **`lemma_gap_freedom`** — the theorem. Its hypotheses after `laid_out` are
  *exactly* `advance_head`'s and `replay_bound`'s `ensures` (the flushed-prefix
  structure + `count == run_len`), passed as parameters/`requires`. Conclusion:

  ```
  forall|i| (0 <= i < records.len() && !records[i].flushed)
      ==> n_flushed <= i < n_flushed + count
  ```

  **every unflushed (acked-but-uncommitted) record lies in the replayed span.**
  With `advance_head` placing the head at the first non-flushed record (everything
  below it flushed) and `count == run_len >= records.len() - n_flushed`
  (coverage), no acked write is left behind — the code-level shadow of
  `AckedWritesRecoverable`'s WAL-replay half.

**The honest framing of the composition.** A `proof fn` cannot call an `exec fn`
(Verus's ghost/exec split), and `laid_out` is a *documented* invariant — not
maintained at one site Verus sees (`mount` *builds* `records` by replaying;
`commit` *consumes* the live queue). So `lemma_gap_freedom` takes the two
functions' postconditions as hypotheses rather than invoking them, and is
conditional on `laid_out`. This is the §4.8 "per-piece contracts compose into the
theorem" shape — the same pattern phase 6 used for the `refcount_sound` system
clause (a conditional theorem whose hypotheses are the proven per-op contracts).
The pieces are each proven unconditionally; the lemma joins them, and the join
fires exactly when `commit`/`mount` run the two in sequence over an honest
(laid-out) queue. The content-coverage half — "flushed ⇒ effects already in the
committed root", the last-write-wins semantics TLA+ abstracts to version numbers —
stays the `CommitProtocol` design gate, deliberately out of scope.

---

## 3. Notes

- **Module location.** 8a–8c parked one `verus!{}` block in `store.rs`; 8d adds the
  composition machinery in a **second** `verus!{}` block after `replay_bound`
  (keeping the data-decision core and the cross-function theorem visually
  separate). The `disk::spec_u32_le`/`spec_u64_le` ghost values are referenced by
  **path** inside `frame_at`, not `use`-imported: a top-level `use` of a `spec fn`
  dangles in the macro-erased plain build (the spec fn is removed), whereas a
  path reference inside `frame_at` erases together with it. (Caught by `cargo
  build -p cas` immediately after the verus pass — the master plan §9 "gated by
  tests, not proofs" discipline; the erased build is one of those gates.)
- **No `by (bit_vector)` anywhere in phase 8.** 8c predicted 8d would need the
  shift-form bridge; the definitional-`spec_u*_le` framing sidesteps it entirely
  (§1). The whole recovery core is index/sequence arithmetic + structural
  induction — Verus's sweet spot.
- **No CI / pinning change.** `cargo verus verify -p cas --no-default-features`
  already runs in the `verus` job (since 7f) with no per-proof filter, so the new
  obligations auto-gate. Verus stays pinned at `0.2026.06.07.cd03505`. The
  `host-tests` `cas` leg (`crash_recovery_preserves_acked_state` /
  `crash_mid_gc_loses_no_data`) stays the differential/regression guard of the
  now-proven recovery core, exactly as `test_store` stayed after phase 6 proved the
  teardown contracts.

---

## 4. The TLA+↔code correspondence (for phase 9's closeout to cite)

Phase 8 lands, in full, the code-level mirror of the `CommitProtocol` recovery
decisions. The map, assembled across 8a–8d:

| `CommitProtocol` (TLA+, design) | `cas::store` (Verus, code) | sub-phase |
|---|---|---|
| `LiveSlot` / `OlderIsA` (pick the higher-generation valid slot) | `pick_survivor` | 8a |
| `Crash` three-outcome safety (a torn write damages only the non-live slot) | `commit_target` (never the live slot) | 8a |
| `CommitPrepare.newHead` (longest contiguous flushed prefix) | `advance_head` | 8b |
| `Recover` (replay contiguous checksummed seq-continuous records) | `replay_bound` (+ `run_len`, the maximal-run equality) | 8c / 8d |
| `AckedWritesRecoverable` — **WAL-replay half** (every unflushed acked record is replayed) | `lemma_gap_freedom` (the `advance_head ∘ replay_bound` composition) | 8d |
| `AckedWritesRecoverable` — **content-coverage half** (flushed ⇒ in the committed root; last-write-wins) | **out of scope — stays the TLA+ design gate** | — |

What stays the other tiers' job (unchanged, master plan §4.8):

- **The content-coverage half of `AckedWritesRecoverable`** — the last-write-wins
  semantics TLA+ abstracts to version numbers — remains the `CommitProtocol`
  design gate; proving it on real bytes would require dragging the chunk store,
  the prolly tree, and `apply_to_overlay` into the proof surface, which is exactly
  the content layer TLA+ exists to abstract.
- **The seam between the two halves** — the two composed against real tree content
  at sampled crash points — stays the crash-injection proptest
  `crash_recovery_preserves_acked_state` and the `wal_replay_scan` /
  `mount_recovery` fuzz corpora, the first-line debugging tier (concrete failing
  inputs).

Phase 8 is the **last proof phase** of the Verus rewrite. Phase 9 is
documentation-only: un-defer the spec §6 Verus row / strike the Kani row, update
`CLAUDE.md`'s tier table + `### Verus` section, and the `0_kani-rewrite.md`
closeout banner. This doc records the correspondence above so phase 9 can cite it.
