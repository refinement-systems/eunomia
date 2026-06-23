# B8C — Ready-queue verification: findings (part 2)

Working notes from the second implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass completes **sub-phase B8C-1**:
the two remaining verified ready-queue ops — `ready_dequeue` (head pop) and `ready_unqueue`
(arbitrary-position splice walk) — plus their two support lemmas and one spec predicate.
With this, **all four ready-queue ops are verified** (the `top_ready` + `ready_enqueue`
pair landed in part 1). Everything is still **standalone (not wired)** — the running
scheduler is untouched, so the change is behaviour-identical and the QEMU boot is unchanged.

Continues `doc/results/1_b8c-findings-1.md`. Branch `b8c-ready-queue`; draft PR #138.
Spec/plan refs: rev1§5.4, rev1§6.1(d), audit §4.2.

---

## 0. Headline

B8C-1 is **done and verified**: `ready_dequeue`, `ready_unqueue`, the clear-bit coherence
lemma `lemma_ready_coherent_after_clear`, the removal wf-sweep lemma `lemma_ready_remove_wf`,
and the `ready_complete_except` predicate. `cargo verus verify -p kcore` rose **362 → 367,
0 errors**; `cargo test -p kcore` is green (94 passed); `cd kernel && cargo build` is green.
No new `external_body` seams and no new `assume_specification`s (the trusted base is
unchanged). The four ops are verified but **not yet wired**, so `make_runnable`/`unqueue_ready`
keep the old seam contracts and the boot is byte-identical.

What is **not** done (later sub-phases, unchanged from part 1's plan): re-contracting the
two seam ops against the verified ops (B8C-2), `signal` + its 3 callers (B8C-3), the
`destroy_tcb` teardown SCC (B8C-4, the genuine risk), the kernel rewiring (B8C-5), and the op
tests + ledger bump (B8C-6). The audit item stays **open** — the running scheduler still uses
the unverified list logic until B8C-5.

The two ops came in **roughly as the plan predicted**: `ready_dequeue` is the `k = 0`
special case and verified first try after the lemma was in place; `ready_unqueue` is the
`disarm` splice-walk **minus the entire census apparatus** and needed only mechanical
trigger/witness fixes (below). No course correction is required — the part-1 plan's
sub-phase decomposition still holds.

---

## 1. What landed (verified, green)

All in `kcore/src/ready.rs` except the one predicate in `kcore/src/cspace.rs`.

### 1.1 `ready_complete_except(rv, tv, t)` — the off-chain-`t` liveness (`cspace.rs`)

A sibling of `ready_complete` with one Runnable thread excepted:

```rust
pub open spec fn ready_complete_except(rv: ReadyView, tv: Map<ObjId, TcbView>, t: ObjId) -> bool {
    forall|x: ObjId| #[trigger] tv.dom().contains(x) && tv[x].state == ThreadState::Runnable
        && x != t
        ==> (tv[x].priority as int) < NUM_PRIOS
            && ready_seq(rv, tv, tv[x].priority as int).contains(x)
}
```

`ready_unqueue` leaves `t` transiently Runnable-and-off-chain, so it cannot preserve full
`ready_complete`; it preserves this `except t` form, and `destroy_tcb` (B8C-4) closes the gap
by halting `t`. This is part-1 technique 7 realized — a **separate** predicate, not a
`ready_wf` conjunct. (The `ready_complete` doc-comment already anticipated it.)

### 1.2 `lemma_ready_coherent_after_clear` — the clear-bit coherence twin

The mirror of `lemma_ready_coherent_after_set` for the level-empties case: precondition
`rvf.bitmap == rv0.bitmap & !(1 << level)` and `ready_seq(rvf, level).len() == 0`; body is the
set-twin with `lemma_clear_bit_self`/`lemma_clear_bit_other` swapped in. The **level-still-
non-empty** removal case keeps the bitmap *unchanged*, so it needs no bit lemma at all — it is
handled inline in `lemma_ready_remove_wf` by transferring `rv0` coherence directly.

### 1.3 `lemma_ready_remove_wf` — the 32-level wf re-establishment for a removal

The removal analogue of `lemma_ready_push_wf`, parameterized by removal position `k` so
**both** ops share it. Takes the op-local facts (the spliced chain `rs0.remove(k)` at `level`
proven via `lemma_ready_remove_chain`, the per-level head/tail/bitmap frame, the tcb frame,
the predecessor's preserved state/priority) and re-derives global `ready_wf`. It establishes
`ready_wf` **only** — not `ready_complete` — and so requires only `ready_wf(rv0)` (not
`ready_complete`), which is what lets the completeness-free `ready_dequeue` call it. It also
**exports the other-levels seq-preservation** (`forall l != level: ready_seq(rvf,l) ==
ready_seq(rv0,l)`), which `ready_unqueue` consumes for its `ready_complete_except` proof.
Bitmap coherence inside it case-splits: clear-lemma when `rs0.remove(k)` is empty, inline
(bitmap unchanged, `rv0` coherence transfers) when it is non-empty.

### 1.4 `ready_dequeue(level) -> Option<ObjId>` — head pop, total

Pops the level head (`k = 0` removal), clears the bit when the level empties, returns the
old head (`None` on an empty level — total, so the trusted `maybe_switch` wrapper carries no
non-emptiness obligation). The dequeued thread stays Runnable and off-chain (the caller sets
it Running), so the contract carries `ready_wf` but **neither** completeness form. Reuses
`lemma_ready_remove_chain`'s `k = 0` arm and `lemma_ready_remove_wf`. Verified at
`rlimit(60)`.

### 1.5 `ready_unqueue(t)` — the arbitrary-position splice walk

Copied **term-for-term from `timer::disarm`** (completeness guarantees `t` is found, so the
fall-off-end is `assert(false)`) but with the **entire census apparatus deleted** — a ready
thread holds no object ref (part-1 §1.1), so there is no `refs` release, no `obj_census`, no
`census_delta_frozen`, no `refcount_sound`. Only `t`'s `qnext` (cleared) and its predecessor's
`qnext` (re-threaded) move. Contract: `ready_wf` + `ready_complete_except(t)` +
`ready_seq(level) == rs0.remove(index_of(t))` + `t`'s preserved fields + a **signal-shaped
frame** (`forall x: tvf[x] != tv0[x] ==> tv0[x] was Runnable at level`) — the shape B8C-4's
`destroy_tcb` will consume to discharge its `dead_tcb_frozen`/`home_views_frozen` step.
Verified at `rlimit(100)`.

---

## 2. Proof techniques (generalizing part 1's list)

These are the new ones from this pass; part 1's 1–10 still apply.

11. **One removal lemma, parameterized by position `k`, serves both pop and splice.**
    `lemma_ready_remove_chain` (part 1) already took an arbitrary `k`; keeping
    `lemma_ready_remove_wf` parameterized by `k` too (rather than a `k = 0` pop lemma + a
    general splice lemma) let `ready_dequeue` (always `k = 0`) and `ready_unqueue` (arbitrary
    `k`) share the entire 32-level wf sweep. The pop is *literally* the splice's head-branch;
    don't special-case it in the proof layer.

12. **Split a re-established invariant by what each caller needs, not by what the op does.**
    `lemma_ready_push_wf` re-establishes `ready_wf` **and** `ready_complete`. For removal the
    two callers diverge — `ready_dequeue` wants neither completeness form, `ready_unqueue`
    wants `_except t` — so `lemma_ready_remove_wf` re-establishes `ready_wf` **only** and
    requires only `ready_wf(rv0)`; each op proves its own completeness obligation on top
    (`ready_dequeue`: none; `ready_unqueue`: `_except t` inline). Folding `ready_complete`
    into the shared lemma would have forced `ready_dequeue` to require a precondition it does
    not have.

13. **Export the lemma's *internal* lemmas-of-passage when a caller's separate obligation
    needs them.** `lemma_ready_remove_wf` proves "other levels' seqs are preserved" internally
    (to assemble `ready_wf`), then **also lists it in `ensures`** because `ready_unqueue`'s
    `ready_complete_except` proof needs `ready_seq(rvf, px) == ready_seq(rv0, px)` for the
    surviving threads at other levels. Cheap to surface (already proven), expensive to
    re-derive at the call site.

14. **Unify a two-case membership proof through the shared invariant, not the two writes.**
    `ready_complete_except` must chart every surviving Runnable `x != t`. The two cases —
    `x` unchanged vs. `x` is `t`'s predecessor — both reduce to "`tv0[x]` is Runnable at its
    level," so `rv0`-`ready_complete` charts *both* (the predecessor is on the chain, hence
    Runnable by the `ready_chain` covenant). Proving `tv0[x].state == Runnable` once, then
    invoking `rv0`-`ready_complete` once, collapses the case split.

15. **`Seq::remove(k)` membership needs an explicit witness index.** `s.remove(k).contains(x)`
    does **not** fall out of `x ∈ s ∧ x != s[k]`. Use `s.remove_ensures(k)` (the index map),
    compute the witness `widx = if j < k { j } else { j - 1 }` where `j = s.index_of(x)`, and
    assert `s.remove(k)[widx] == x` with its range — *then* `contains` fires. (`remove(0) ==
    drop_first()` is the easy special case, dischargeable by `=~=`.)

16. **Carry `ready_wf`/`ready_complete` in the splice-walk loop invariant — the requires don't
    survive into the body.** The `=~=` domain asserts (`rvf.heads.dom() =~= rv0.heads.dom()`,
    via the `by { assert(rv0.heads.dom().contains(level)) }` hint from `ready_enqueue`) need
    `rv0.heads.dom() == Set::new(0..NUM_PRIOS)`, which comes from `ready_wf(rv0)`. Inside a
    `while`, the function's `requires` over `old(store)` is *not* automatically available;
    pin `ready_wf(rv0, tv0)` (and `ready_complete(rv0, tv0)` for the completeness proof) into
    the loop invariant — trivially preserved since `rv0`/`tv0` are ghosts fixed to entry. This
    was the only non-mechanical fix the pass needed.

---

## 3. Contract shapes the later sub-phases consume

Recorded so B8C-2/B8C-4 don't have to re-derive them:

- **B8C-2 (re-contract the seam):** `make_runnable` lifts `ready_enqueue`'s ensures;
  `unqueue_ready` lifts `ready_unqueue`'s — specifically `ready_wf` (require + ensure),
  `ready_complete_except(t)` (ensure), `ready_seq(level) == rs0.remove(index_of(t))`, and the
  signal-shaped tcb frame. `ready_dequeue` is **not** a seam op — only the trusted
  `maybe_switch` calls it (B8C-5), so it stays a plain `kcore::ready::*` call, not a `Store`
  method.
- **B8C-4 (`destroy_tcb`):** the `unqueue_ready` seam now carries `ready_complete_except(t)`,
  not full `ready_complete`. `destroy_tcb` must (a) thread `ready_wf` + `ready_complete`
  through the detach, (b) discharge its detach-step frame from `ready_unqueue`'s
  **signal-shaped frame** (`tvf[x] != tv0[x] ==> tv0[x] Runnable at level` — a *live*
  predecessor's `qnext` move, which `dead_tcb_frozen`/`home_views_frozen` permit since they
  constrain only dead TCBs/residency), and (c) after the unqueue, halt `t` (state → Halted) to
  promote `ready_complete_except(t)` back to full `ready_complete`. The §1.1 census-blindness
  should keep the existing census lemmas untouched — confirm early.

---

## 4. Gate state

| gate | part-1 checkpoint | after B8C-1 |
|---|---|---|
| `cargo verus verify -p kcore` | 362 / 0 | **367 / 0** |
| `cargo test -p kcore` | green (94) | green (94 passed) |
| `cd kernel && cargo build` | green | green |
| QEMU boot | unchanged | unchanged (ops verified, not wired) |

`external_body` seams and `assume_specification`s: **unchanged** (none added). The +5
verified items are `lemma_ready_coherent_after_clear`, `lemma_ready_remove_wf`,
`ready_dequeue`, `ready_unqueue`, and the obligation Verus attributes to the new predicate.
