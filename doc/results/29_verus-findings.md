# Verus findings 8 — Phase 3d: channel `send` / `recv` (the FIFO core)

Plan: `doc/plans/3_verus-rewrite.md` (§4.3 channel) and its detailed decomposition
`doc/plans/3_verus-rewrite_phase3-detail.md` (§3d). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26` (§3a — untyped `retype_check`/`reset`), `27`
(§3b — the channel ghost-view enabling refactor). This is the **fourth** of phase 3's
five sub-phases and its **hardest** (the analog of `cdt_unlink`'s merge, doc 25): the
§4.3 FIFO core. It builds only on 3b (`chan_view` + the ring↔arena coupling); 3c
(`retype_install`) is independent and not yet done — doing 3d first is sound, they share
no proof.

**Doc numbering.** 26 = 3a, 27 = 3b, so this (3d) is **28**; when 3c
(`retype_install`) lands it takes **29**. The §3-detail order is 3a/3b/3c/3d/3e; the doc
sequence follows landing order, not plan order.

**Outcome.** `cargo verus verify -p kcore`: **76 verified, 0 errors** (was 60 after 3b:
`+send`, `+recv`, `+cap_is_empty`, the FIFO model `ring_msg`/`ring_fifo`, the four
modular/congruence lemmas `lemma_window_index_distinct`/`lemma_mod_shift_head`/
`lemma_self_mod`/`lemma_ring_msg_eq`, `is_ring_cap_of`). `cargo test -p kcore`: **22
passed** (was 17 — `+send_recv_roundtrip`, `+send_full_and_recv_empty`,
`+recv_nocapslot_atomic`, `+recv_null_slot_tolerance`, `+randomized_fifo_sweep`). The
aarch64 `kernel` cross-build is unchanged (ghost erases; `send`/`recv` are called only
from the `kernel` crate, never from verified `kcore` code, so their new `requires`
constrain nothing at runtime). **No new `external_body` boundary** — `send`/`recv` are
fully proven, not assumed.

---

## 1. What closed

- **`channel::send`** — verified against the §4.3 FIFO model. On `Ok`: `chan_wf`
  preserved; the message lands at the tail (`ring_fifo` of the sending ring grows by
  `Seq::push`, the other ring untouched); the supplied caps **leave the sender's slots
  exactly** (move totality, via the verified `slot_move`); `count` bumped, `head`/`depth`
  fixed. On `Full`/`PeerClosed`: the store is **unchanged** (the read-only-guard frame).
- **`channel::recv`** — the **two-pass** receiver. On `Ok`: `chan_wf` preserved; the head
  message is dequeued (`ring_fifo` of the receiving ring **loses its head**,
  `Seq::drop_first`, other ring untouched); `len == old msg_len[head]`; `count`
  decremented, `head` advanced `(head+1) % depth`. **Atomicity**: `Empty`/`NoCapSlot`
  leave the store unchanged — pass 1 is read-only, so a `NoCapSlot` failure leaves the
  message fully queued. **Null-slot tolerance**: a ring cap emptied in flight by
  revocation is delivered as absent (skipped, mask bit clear) — never a panic, by the
  guarded `unwrap`.
- **The FIFO `Seq` model** (`cspace.rs`): `ring_msg(cv, sv, ring, idx) = (msg_len, the
  four cap contents)` and `ring_fifo(cv, sv, ring) = Seq::new(count, |j| ring_msg(…,
  (head+j) % depth))` — payload bytes abstracted (doc 27), so the model is length + cap
  identity + order. `send` ⇒ `push`, `recv` ⇒ `drop_first`.
- **`chan_wf` gained a ring-cap injectivity clause** (3b deferred it, doc 27 §3): distinct
  ring positions map to distinct arena handles. Load-bearing — it is what lets filling the
  new tail (or emptying the head) leave every other in-window message alone. Plus a
  **`depth <= 0x8000_0000` bound** so `send`'s `(head + count)` stays within `u32`.

All five contracts are also **host-test-checked** against the real bodies
(`test_store.rs`): a FIFO roundtrip with a moved cap, the `Full`/`Empty` guard frames,
`NoCapSlot` atomicity (message survives a failed `recv`), null-slot tolerance (empty a
queued ring cap, then `recv`), and a `randomized_fifo_sweep` (random `send`/`recv`
against a reference `VecDeque`, across wraparound, asserting FIFO order + `chan_wf_exec`
throughout).

---

## 2. Verus mechanics worth keeping

### 2.1 `slot_move` needed **two** frame extensions, not one

The detail plan front-loaded one (§1.1): `slot_move`'s `ensures` had to gain
`final.chan_view() == old.chan_view()`, or a queued-cap move would havoc every channel
cursor. **But the FIFO proof needed a second, deeper frame**: the **`.cap`-content
frame** —

> `forall x ∈ dom, x ≠ src, x ≠ dst: final.slot_view()[x].cap == old.slot_view()[x].cap`.

`slot_move`'s neighbour fixups rewrite `src`'s CDT *link* fields (parent / sibling /
child) but never any `.cap`; yet its original `ensures` pinned only `dst.cap`/`src` empty
and global properties — so a caller could not conclude that an *other* ring slot's
contents survived a move. Both frames are trivially provable additions: the `chan_view`
one chains through `set_slot`'s frame; the `.cap` one falls straight out of
`lemma_generic_relabeled` (already invoked in `slot_move`'s body for the count proof).
**The lesson: a "frame" is not one clause — list every view *and* every field a caller
downstream will read across the call.**

### 2.2 Preconditions do **not** auto-instantiate inside a loop

`send`'s `requires forall|c| … caps@[c] …` (and `recv`'s `dests`) instantiate fine in
straight-line body code but **not inside a `while` loop** — the loop sees only its
invariant, not the function's `requires`. The fix (which recurs for both ops): **restate
the precondition as a loop invariant**, phrased over the immutable snapshots `sv0`/`cv0`
(so it is trivially preserved). A diagnostic that pinned this: the same `assert`
*passed* at the top of the body and *failed* identically inside the loop.

### 2.3 The cap-move loop frame: a 5-clause, non-nested invariant

The move loop's net effect on `slot_view` is captured by five **single** (un-nested)
`forall`s: processed/unprocessed message-slot caps (at the message index), processed/
unprocessed sender (or receiver) caps, and **"every ring slot *not* at the message index
is unchanged"**. Re-establishing each after a `slot_move` uses exactly three disequality
sources, instantiated per clause: **ring-cap injectivity** (`x ≠ dst` at a different
index), the **ring-disjoint precondition** (sender/receiver slots are not ring caps, so
`≠ src`/`≠ dst`), and `slot_move`'s `.cap` frame (§2.1). Splitting into one `forall` per
class — rather than one characterising formula — kept each SMT query first-order (the
doc 25 §3 per-clause discipline, here on the arena frame).

### 2.4 Modular arithmetic quarantined into four tiny lemmas (doc 25 §2 confirmed)

The only nonlinear/modular reasoning lives in four one-line helpers over
`vstd::arithmetic::div_mod`: `lemma_window_index_distinct` (distinct window offsets land
on distinct ring indices — the send-tail-vs-window and recv-head-vs-rest distinctness),
`lemma_mod_shift_head` (`((head+1)%d + j)%d == (head+1+j)%d`, the recv pop's index shift),
`lemma_self_mod` (`x%d == x` for `x < d`, identifying the head index), and the per-message
congruence `lemma_ring_msg_eq` (two `ring_msg`s are equal when their length and four cap
contents agree — `Seq` extensionality over `Cap`). Z3 was reliable once every `%` was
behind one of these. Existentials/`choose` over a modular term still need the explicit
`#![trigger (head + j) % (depth as int)]` (the `in_live_window` idiom, doc 27 §2.1).

### 2.5 Plumbing the now-verified ops into `verus!`

`send`/`recv` returning `Result<…, ChanError>` and forming messages forced **`ChanError`
and the `MSG_*`/`EV_*` consts into `verus!` blocks** (items outside the macro are
"ignored" and uncallable from verified code). Two `Store`/`ExStore` additions: a
`chan_msg_read` spec (frame-only, `&self`; 3b had omitted it as payload-abstracted) and a
free `cap_is_empty(c) -> (r: bool) ensures r == is_empty_cap(c)` exec helper — `Cap::
is_empty` is plain Rust, so verified exec code (`recv`'s passes) cannot call it directly
(doc 26 §2.4, the `matches!(…, CapKind::Empty)` idiom, here packaged).

---

## 3. Scope held (what 3d did *not* touch)

- **Cross-channel ring-slot disjointness** — the *other* clause 3b deferred (doc 27 §3).
  It is **not needed** for 3d's contracts, which are **single-channel** (`chan_wf(…, ch)`
  for the operated channel). It only matters for a global "∀ch chan_wf", which no op
  asserts. Documented residue: it is **trivially preserved** by `send`/`recv` (they never
  reassign a `ring_cap` handle), so it folds in unchanged when a channel-collection
  invariant first needs it.
- **`endpoint_cap_dropped` / `bind`** are 3e; **`destroy_channel` stays `external_body`**
  (the cross-object teardown, phases 4–5); **`notification::signal` stays the assumed
  contract** from 3b (its body proof is phase 4). All remain plain Rust / unchanged.
- **No `CLAUDE.md` / spec edits** — the phase-3 closeout (moving the channel + untyped
  ops onto the proven list, recording the `signal`/`destroy_channel` residue) lands in
  **3e** per the detail plan; 3a–3d only seed their findings docs.
- Payload bytes stay abstracted; `chan_msg_write`/`chan_msg_read` are frame-only. The
  cap-present **mask** `recv` returns is computed but not characterised in the contract
  (it would need bit-vector reasoning); the null-slot guarantee that matters — *no panic*
  — is proven by construction.
