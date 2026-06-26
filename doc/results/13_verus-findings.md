# 13 — CommitProtocol `RecoverReconstructs` WAL-projection on `recover_records` (Task 13)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 13 (Tier 3,
TLA-routed, feasibility *partial*; Effort M, Risk med). Outcome: **shipped, first
attempt.** `cargo clean -p cas && cargo verus verify -p cas --no-default-features` reads
**`77 verified, 0 errors`** cold (75 → 77: the two new `proof fn`s; the projection
`spec fn` is non-recursive, +0; the new `recover_records` ensures adds no item). No new
trusted seam (tally stays 14); cas Baseline rises `75 → 77`. Sibling of the Task 9/10/12
labeling tasks, but — unlike Task 12 — it introduces a **new `spec fn`**, so it carries
the anti-theatre teeth control the plan requires.

## What was attempted

Name, in Verus, the **WAL-byte/queue projection** of the `CommitProtocol` TLA invariant
`RecoverReconstructs` (`tla/commit_protocol/CommitProtocol.tla:281`): the run
`recover_records` rebuilds from the committed WAL head *is exactly* the maximal
seq-continuous, content-valid post-head record skeleton. That fact is already *proven*
inside `cas/src/store.rs` — the recovery walk discharges `laid_out`, the maximal-run
equality `run_len == records.len() + forged_max`, and the head anchor — but it was not
*named* as the mechanized half of `RecoverReconstructs`. Four edits in
`cas/src/store.rs`, all inside the existing `verus!{}` blocks:

1. **`spec fn recover_reconstructs(wal, records, head, next_seq, forged_max)`** — the
   projection predicate: the conjunction of soundness (`laid_out(wal, records, 0)`,
   head-anchored), maximality (`run_len(wal, head, next_seq) == records.len() +
   forged_max`). Plain transparent `spec fn`, non-recursive → no proof obligation (+0
   verified items), exactly as Task 9/10's `spec fn`s.
2. **`proof fn lemma_recover_reconstructs`** — the thin corollary deriving the projection
   from `recover_records`'s existing `ensures` (each conjunct is a `requires`; empty
   body). The literal "thin `proof fn` deriving it" the plan asks for.
3. A new named **`ensures recover_reconstructs(wal@, r.records@, wal_head, wal_next_seq,
   r.forged_max)` on `recover_records`**, discharged in its existing final `proof {…}`
   block by calling the corollary. This makes the projection a *live* postcondition of
   the actual recovery decision — the rev2§4.5 establish-side label (`verus.md` §10),
   not a dangling lemma.
4. **`proof fn lemma_recover_reconstructs_pins_head`** — the anti-theatre teeth control:
   with `records` non-empty, the projection pins `records[0].off/seq` to its
   `(head, next_seq)` argument, so any differing `(h2, s2)` (e.g. an off-by-one head)
   fails the anchor clause and the predicate is `false`. This stays in the tree as the
   committed witness that the green proof is not vacuous over its own producer.

The honest scope (mirrors Task 12): only the *local per-call* byte/queue projection is
Verus-mechanized. The verbatim global `AckedWritesRecoverable`/`RecoverReconstructs`
(`CommitProtocol.tla:261`/`:281`) quantifies over `writeCtr`/`walLog` — global
acked-write state the verified core does not model — and rests on the trusted
Store-lifetime join + content-coverage axiom (rev2§6.1(e), already a ledger seam row at
`verus_trusted-base.md:248`). It stays TLA-owned + by-construction.

## Result

- `cargo clean -p cas && cargo verus verify -p cas --no-default-features` →
  **`77 verified, 0 errors`** (cold, authoritative — the `verification results::` line was
  present, a real non-cached run; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`). The
  +2 are `lemma_recover_reconstructs` and `lemma_recover_reconstructs_pins_head`; the
  `recover_reconstructs` `spec fn` and the new `recover_records` ensures add no item.
- **`rlimit` (cold, `scripts/verus-baseline.sh cas`, freshly re-derived before and after on
  this branch):** cas total `14,608,236 → 14,474,777` = **−133,459 (−0.91%) — no
  regression** (the small swing is Z3 cold-run nondeterminism; untouched `prolly` codecs
  moved comparably). The establish-side label is genuinely cheap: `recover_records`'s own
  `rlimit` measured `377,942 → 284,385` (it did *not* rise — the new ensures unfolds to the
  three facts the walk already proved). The two new lemmas cost `6,344`
  (`lemma_recover_reconstructs`) + `5,357` (`…_pins_head`) ≈ 11.7k rlimit, negligible.
- `cargo test -p cas` green (lib + `mount_recovery`/`wal_replay_scan` integration + the 10
  `fuzz_regressions` mount-forgery controls, incl. `mnt1_forged_wal_head_rejected` and
  `mnt1_forged_wal_seq_max_rejected`). `cargo fmt -p cas --check` clean. Erasure leaves the
  exec recovery code byte-identical.

## Teeth (anti-theatre), both forms

The plan requires "a deliberately-wrong off-by-one head bound must *fail* to verify." Shown
both ways:

- **Committed, green:** `lemma_recover_reconstructs_pins_head` proves
  `(h2, s2) != (head, next_seq) ==> !recover_reconstructs(wal, records, h2, s2, forged_max)`
  for non-empty `records`. It verifies, and *stays in the tree* as the durable witness that
  the predicate constrains the head it is stated against — a predicate that merely asserted
  "some head exists" could not prove this.
- **Reverted, fails-to-verify (the literal demonstration):** temporarily adding
  `recover_reconstructs(wal@, r.records@, (wal_head + 1) as u64, wal_next_seq, r.forged_max)`
  as a second `ensures` on `recover_records` made Verus report
  `error: postcondition not satisfied` at that clause (`store.rs:1528`), `76 verified,
  1 errors` — the walk proves `records[0].off == wal_head`, contradicting `wal_head + 1`.
  Reverted after capturing the error; the committed artifact is the correct ensures + the
  green pins-head lemma.

Together these refute the "green-proof-of-nothing quantifying only over its own producer"
risk: the projection is discharged by `recover_records`'s real walk proof (not assumed),
and the head bound is provably load-bearing.

## What stayed TLA-owned (no over-claim)

The TLA `CommitProtocol` model is **not** retired or demoted; its 6886-state TLC run and
`CommitProtocol_NegControl.cfg` (the `RecoverNoop` negative control that makes
`RecoverReconstructs` fail) remain the design oracle. Only the local byte/queue projection
moved to a named Verus reading. Staying TLA + by-construction: the *global*
`AckedWritesRecoverable`/`RecoverReconstructs` over `writeCtr`/`walLog`; the Store-lifetime
join (the live `wal_records` queue keeps matching the WAL bytes across write/flush/commit —
the `verus_trusted-base.md:248` seam row); the content-coverage half (flushed ⇒ effects in
the committed root); and the `Crash` three-outcome safety + `Recover` liveness +
`FsyncMeansFsync` axiom.

This also nuances the Task 12 routing note, which flatly called the cross-restart
`Recover`/`RecoverReconstructs` replay-equality "TLA-owned": that is now scoped to the
*global* arm — the local byte/queue projection is Verus-mechanized here.

## Reverted vs kept

- **Reverted:** the off-by-one teeth experiment (the temporary second `ensures` on
  `recover_records`) — its purpose was the fails-to-verify demonstration above, recorded
  here, code reverted.
- **Kept:** the `recover_reconstructs` `spec fn`, the `lemma_recover_reconstructs`
  corollary, the named `ensures` on `recover_records`, the
  `lemma_recover_reconstructs_pins_head` teeth lemma (all in `cas/src/store.rs`); the
  "CommitProtocol `RecoverReconstructs` (WAL-projection) routing note" + cas Baseline
  `75 → 77` in `verus_trusted-base.md`; and this findings doc. No incidental code changes
  were needed.

## Proposed addition to `doc/guidelines/verus.md`

One bullet, appended to the §10 "Labeling an already-proven fact under an invariant name"
guidance, on the case Task 12 did not cover (a *new* `spec fn` projecting a TLA invariant,
not a comment on an existing clause):

> **A projection `spec fn` of a TLA (or otherwise external) invariant must carry a
> pins/teeth control.** When the named predicate is discharged *only* by feeding it its
> sole producer's `ensures` (here `recover_records`), a green proof risks being vacuous —
> it could hold for the wrong reconstruction too. Add a committed `proof fn` proving the
> predicate is *false* for a deliberately-wrong argument (an off-by-one anchor), so the
> green proof demonstrably constrains the input it is stated against. The pins lemma stays
> in the tree; the symmetric "wrong `ensures` fails to verify" experiment is recorded in
> the findings doc and reverted (it cannot be committed — it is red by construction).
