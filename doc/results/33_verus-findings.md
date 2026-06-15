# Verus findings 13 — Phase 4c: notification `remove_waiter`, the mid-queue unlink

Plan: `doc/plans/3_verus-rewrite.md` (§4.4) and its decomposition
`doc/plans/3_verus-rewrite_phase4-detail.md` (§4c). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31` (phase 4a — the notification/TCB/timer ghost-view refactor), `32` (phase 4b —
`signal`/`wait`/`destroy_notif`, the waiter-queue FIFO core). This is the **third** of
phase 4's five sub-phases: it proves `remove_waiter`, the thread-teardown path that
unlinks a blocked waiter from a notification's queue — the last unproven notification
op. With it the waiter queue is **fully verified on all three shapes**: push (`wait`),
pop (`signal`), and now splice (`remove_waiter`), so **"wake order = block order"** is
closed.

**Outcome.** `cargo verus verify -p kcore`: **101 verified, 0 errors** (was 98 after 4b;
`+3` = the new `lemma_remove_chain`, plus `remove_waiter` graduating from plain Rust
outside the `verus!{}` block to a proven body, and the spec-fn accounting). `cargo test
-p kcore`: **32 passed** (was 31 — `+remove_waiter_unlink`). The aarch64 `kernel`
cross-build is unchanged (ghost erasure; confirmed `cd kernel && cargo build`).

---

## 1. What closed

- **`remove_waiter` is proven** (`notification.rs`), moved into the `verus!{}` block. The
  contract splits on whether `t` is queued on `n` (`waiter_seq(n).contains(t)`):
  - **present** ⇒ `t` is spliced out (`waiter_seq` loses exactly the `t` element,
    `Seq::remove(index_of(t))` — the FIFO order of the rest preserved), `t`'s `qnext`/
    `wait_notif` are cleared, and the queued ref is released (`refs[n] -= 1`);
  - **absent** ⇒ the store is unchanged;
  - `notif_wf(n)` preserved, `slot_view`/`chan_view`/`timer_view` framed, either way.
  The release `-1` is the **second installment of `refcount_sound`'s waiter term**
  (after 4b's pop-release), discharged by the same `wait_head is Some ⇒ refs > 0`
  precondition `signal` carries.
- **`lemma_remove_chain`** (`cspace.rs`), the splice analog of `lemma_drop_first_chain`:
  given the imperative link fixups for removing `ws0[k]`, it concludes
  `waiter_chain(nvf, tvf, n, ws0.remove(k))`. `lemma_drop_first_chain` is exactly its
  `k == 0` head-pop special case; the extra work is the predecessor `qnext` re-thread
  (the boundary `ws0[k-1] → ws0[k+1]`) and the tail-drop branch (`k == len-1` ⇒ tail
  becomes the predecessor). Mid-`remove_waiter` it is composed with the 4b
  `lemma_waiter_chain_unique`, so the result is a `waiter_seq` **equality**, not mere
  existence.

## 2. Design decisions / mechanics worth keeping

- **Lighter than `cdt_unlink`, as the detail plan predicted.** The waiter queue is
  singly-linked with no re-parenting, so the removal is a plain `Seq::remove` — no rank
  rescale (`cdt_unlink` needed `lemma_unlink_sib`'s multiplicative band, doc 25) and **no
  modular arithmetic** (`send`/`recv`'s four `%`-lemmas, doc 29). `lemma_remove_chain` is
  pure index bookkeeping over the three regions of the spliced sequence (`i < k-1`
  non-predecessor, `i == k-1` predecessor re-thread, `i ≥ k` shifted), each reducing to a
  source-chain clause at the index-shift `ii = if i < k { i } else { i + 1 }`.
- **The walk is read-only, so its only invariant burden is "store == old".** Unlike
  `cdt_unlink`/`slot_move`, `remove_waiter`'s loop never writes (the writes are all in the
  found branch, which `return`s), so the loop invariant pins all seven views to the entry
  state and the body's continue path preserves it for free. The decreases is a `ghost mut`
  position counter `k` with `decreases ws0.len() - k` — the **first `ghost mut` in the
  codebase** (the existing loops use rank-based `decreases`); it verified cleanly, so the
  rank fallback the plan held in reserve was not needed.
- **A loop severs pre-loop ghost↔`old(store)` links — re-pin them in the invariant.**
  The sharp lesson of this sub-phase. The found-path `return` lives *inside* the loop, and
  a Verus loop body assumes **only** the invariant: the pre-loop bindings `nv0 ==
  old(store).notif_view()` / `tv0 == old(store).tcb_view()` are **not** carried in. The
  symptom was pathological — `assert(store.tcb_view().dom() =~= tv0.dom())` passed while
  the *syntactically equal* postcondition `... == old(store).tcb_view().dom()` failed,
  because Verus no longer knew `tv0 == old(store).tcb_view()`. Fix: add `nv0 ==
  old(store).notif_view()`, `tv0 == old(store).tcb_view()`, `ws0 == waiter_seq(nv0, tv0,
  n)` to the loop invariant. (`signal`/`wait` never hit this — they have no loop. The
  cspace looping ops return *after* their loops, where the linkage re-derives differently.)
- **Branchy mutations: assert dom-preservation inside each arm, not at the merge.** The
  `match prev` (head vs. predecessor write) and `if tail_is_t` (tail fixup) make the
  post-state views conditional. The predecessor key is a match-bound `p` that is out of
  scope at the merged point, so a single post-merge `dom` assert can't see it is resident.
  Asserting `store.tcb_view().dom() =~= tv0.dom()` *inside* the `Some(p)` arm (where
  `p == ws0[k-1] ∈ tv0.dom()` is in scope) lets the fact path-merge — the standard Verus
  idiom for branchy `&mut` edits, recorded here because it is non-obvious.
- **`ObjId`/`Option<ObjId>` exec `==` is external; compare the `.0` tag.** The original
  plain-Rust body used `c == t` and `tail == Some(t)`; inside `verus!{}` those resolve to
  the ignored `ObjId::eq`. Rewritten as `c.0 == t.0` and a `match` on the tag (the
  `cspace.rs:4096` pattern), with `assert(t == ws0[k])` bridging the tag test back to the
  structural chain element. The erased exec behaviour is identical to the prior body.

## 3. Doc-numbering note

Phase 4 produces docs 31–35 ("findings 11–15"), numbered in landing order (the doc-29/30
convention). This is 4c, the third to land, so doc 33 / findings 13.

## 4. What's next (4d)

`report_terminal` (ReportMonotone + FireSafe) + `thread::bind` — the §4.4 thread/report
obligations, now that `signal` (4b) is proven. ReportMonotone rests on `signal` not
touching a halted/faulted thread's report; FireSafe states the local "cap-in-slot ⇒
object live" fact (the first cspace-slot down payment on `refcount_sound`, scoped per the
3e per-op-delta precedent). `thread::bind` is the direct analog of `channel::bind` (3e).
