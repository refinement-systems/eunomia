# 9 — IpcReactor `FifoPerChannel` + local `NoDrop` as named ensures on the kcore channel ring (Task 9)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 9 (Tier 3,
TLA-routed; "labeling, not new coverage"; Risk: low). Outcome: **verified, shipped,
first attempt.** `cargo verus verify -p kcore` still reads **`404 verified, 0 errors`**
(unchanged — a non-recursive `spec fn` carries no proof obligation). No reverts; no new
trusted seam (tally stays 14); kcore's own-function `rlimit` total is flat (−0.46%,
within Z3 run-to-run noise — see below).

## What was attempted

The kcore channel `send`/`recv` (`kcore/src/channel.rs`) **already prove**, on the Ok
arm, the per-step FIFO discipline of the message ring — `send` grows the sending ring's
FIFO `Seq` by `Seq::push` at the tail, `recv` pops the receiving ring's head by
`Seq::drop_first`, the peer ring is framed unchanged — and on the Err arm the whole
store is unchanged (all discharged by the existing
`lemma_send_fifo_push`/`lemma_recv_fifo_drop_first`/`lemma_ring_fifo_frame`). These are
exactly the *local per-step half* of the TLA invariants `FifoPerChannel`
(`tla/ipc_reactor/IpcReactor.tla:279`) and `NoDrop` (`:274`) — but they read as bare
`ring_fifo(...) == ...push(...)` expressions, not as the named invariants they mechanize.

The goal was **labeling only**: introduce named `spec fn`s so the contracts *read as*
the mechanized local half of those TLA invariants, and record in the trusted-base ledger
that the *global* arms stay TLA-owned. Three `pub open spec fn`s were added to
`channel.rs` next to `end_idx_spec`, and the matching `ensures` clauses on `send`/`recv`
were rewritten to call them:

```rust
pub open spec fn fifo_send_appends(cv0, sv0, cvf, svf, ring, idx) -> bool {
    cspace::ring_fifo(cvf, svf, ring) == cspace::ring_fifo(cv0, sv0, ring)
        .push(cspace::ring_msg(cvf, svf, ring, idx))
}
pub open spec fn fifo_recv_pops_head(cv0, sv0, cvf, svf, ring) -> bool {
    cspace::ring_fifo(cvf, svf, ring) == cspace::ring_fifo(cv0, sv0, ring).drop_first()
}
pub open spec fn no_drop_on_refusal(sv0, cv0, rv0, svf, cvf, rvf) -> bool {
    svf == sv0 && cvf == cv0 && rvf == rv0   // the three store views unchanged
}
```

- `send` Ok arm: the active-ring push clause → `fifo_send_appends(...)` keyed on
  `end_idx_spec(end)`.
- `recv` Ok arm: the active-ring drop_first clause → `fifo_recv_pops_head(...)` keyed on
  `1 - end_idx_spec(end)`.
- both Err arms: the three-view frame → `no_drop_on_refusal(...)`.

The surrounding `count`/`head`/`depth` and peer-ring-unchanged clauses were kept as-is.
The doc comment on each `spec fn` names the local half **and** states the global arm
stays TLA-owned, so the spec-fn name is never mistaken for a full mechanization (the
rev2§6.1 "no trust-routed property mistaken for mechanized" rule).

## Result

- `cargo clean -p kcore && cargo verus verify -p kcore` → **`404 verified, 0 errors`**
  (cold, authoritative; the results line was present). The verified-item count is
  **unchanged** from the pre-change 404: the three new `open spec fn`s are non-recursive
  and carry no obligation; the named ensures are conjuncts on the already-counted `send`/
  `recv`.
- The named `open` fns discharge with **no proof-body edit**: the existing lemmas already
  establish the bare expressions the `open` fns unfold to (`send` at `channel.rs:1263`,
  `recv` at `:1875`, the Err frame in each Err path), so replacing the inline clause with
  a definitionally-equal `open`-fn call is the same SMT term.
- **`rlimit` (proof-cost) — flat / no regression.** Measured with
  `scripts/verus-baseline.sh kcore` cold before and after, comparing kcore's *own*
  functions (`kcore::*`; the only valid control, because each function's `rlimit` is
  independent of whether the shared `vstd` lemmas were re-verified or cache-served in
  that run):

  | metric | pre | post | delta |
  |---|---|---|---|
  | kcore own-fn `rlimit` total | 150,191,499 | 149,497,179 | **−0.46 %** |
  | `channel::send` | 1,038,980 | 949,664 | −89,316 |
  | `channel::recv` | 1,220,848 | 1,256,869 | +36,021 |

  The −0.46 % total is **Z3 run-to-run nondeterminism, not a real change**: untouched
  functions I never edited swung far more in the same two runs (`cspace::delete`
  −951 k, `lemma_unlink_merge` −443 k, `cspace::lemma_ready_remove_chain` −304 k,
  `release_binding` +406 k, `bind` +202 k). The two functions I actually changed moved
  trivially against ~1 M baselines, well inside that noise band. The three new spec fns
  produce **no** `function-breakdown` entry at all — confirming a non-recursive
  `open spec fn` is invisible to the SMT solver.

  *Measurement caveat for the next implementer:* the whole-crate `rlimit` sum is **not**
  a valid control here. `verus-baseline.sh` does `cargo clean -p kcore` (kcore only, not
  `vstd`), so a run started with a cold `vstd` cache re-verifies and counts ~1091 extra
  `vstd` lemmas (e.g. `EndianNat::to_big_from_big` at 4.5 M `rlimit` each), while a run
  started with `vstd` warm does not. Our pre-run was vstd-cold (`verif 1495`, 2016
  `rlimit` entries) and the post-run vstd-warm (`verif 404`, 354 entries) because the
  authoritative `cargo verus verify` between them cached `vstd`. Comparing those whole-
  crate sums shows a spurious −29 %; only the `kcore::*` subset is apples-to-apples.

## Two honest corrections to the literal Task 9 text

Both are grounded in `doc/guidelines/verus.md`'s anti-theatre / visibility discipline and
were validated before implementation:

1. **`open`, not `closed`.** The task entry suggests `pub closed spec fn`. `open` is the
   right choice for a *clarifying* label: the body stays transparent to external readers,
   the kernel shell, and any future verified caller, so the label is **additive** — it
   names the `ring_fifo.push` / `drop_first` fact without hiding it. `closed` would
   opacify the underlying equality to external consumers, the wrong direction for a label
   whose purpose is to make the contract read as the invariant. (Note: `closed` would
   *also* have discharged here, because the spec fns and `send`/`recv` live in the same
   module and a non-recursive in-module `closed` body unfolds transparently for the
   solver — verus.md "Visibility" §; so the choice is faithfulness, not a discharge
   blocker. The earlier worry that `closed` would force a `reveal` in the proof bodies
   does not apply to this same-module case.) `open` is also `rlimit`-neutral and needs no
   proof-body edit anywhere. Used `open`.

2. **`NoDrop`-local is the Err-frame, not a ring-length fact.** A per-ring
   `ring_fifo().len() ± 1` predicate (one literal reading of the task's "keyed per ring")
   is **vacuous** — it is already implied by the existing `count[ring] ± 1` clause, since
   `ring_fifo(...).len() == cv.count[ring]` by definition (`cspace.rs:1942`). The
   faithful, non-vacuous local reading of `NoDrop` ("Full is the only refusal; no queued
   message is lost") is the **Err arm's refusal-drops-nothing frame**: the three store
   views unchanged. `no_drop_on_refusal` is that predicate.

## What stayed TLA-owned (no over-claim)

The TLA `IpcReactor` model is **not** retired or demoted. Only the *local per-step*
refinement moved to Verus. The *global* arms the kcore ring cannot witness (it holds only
the live window `[head, head + count)`, keeping no `recvd`/`nextSend` history) stay the
TLA design oracle: `NoDrop`'s counting identity `nextSend = |recvd| + |queue|`
(`IpcReactor.tla:275`) and `FifoPerChannel`'s global-index arms `recvd[i] = i` /
`queue[i] = |recvd| + i` (`:280-281`). Recorded in the new ledger routing note.

## Reverted / kept

Nothing reverted — everything verifies. Kept: the three spec fns + the four `ensures`
rewrites in `channel.rs`; the ledger routing note + Baseline-row prose
(`verus_trusted-base.md`, no number change, tally stays 14); the verus.md technique note
(below). No incidental code changes were needed.

## Proposed addition to `doc/guidelines/verus.md` (applied)

A short bullet appended to the visibility section (§ "Visibility: `open`, `closed`, and
the `verus!{}` boundary") on **labeling an already-proven fact under an invariant name**
(a TLA per-step refinement, Part A §1.3): prefer `open` so the named fact stays
transparent — a clarifying label should not encapsulate the thing it names; and beware
the *vacuous* label — a predicate already entailed by another `ensures` clause proves
nothing (the `ring_fifo().len() ± 1` vs `count ± 1` trap), so pick the predicate carrying
genuine, non-derived content. This serves the sibling labeling Tasks 10 and 12.
