# Verus findings 8 — Phase 3c: untyped `retype_install` into `verus!{}`

Plan: `doc/plans/3_verus-rewrite.md` (§4.2 remainder) and its detailed
decomposition `doc/plans/3_verus-rewrite_phase3-detail.md` (§3c). Prior increments:
`21`…`25` (phase 2 — the cspace/CDT core), `26` (§3a — untyped
`retype_check`/`reset`), `27` (§3b — the `chan_view` ghost-view refactor). This is
the **third** of phase 3's five sub-phases: the §2.5 rights-inheritance theorem +
the channel two-endpoint install, reusing the phase-2c `cdt_insert_child` and the
phase-3b `endpoint_cap_added`.

**Outcome.** `cargo verus verify -p kcore`: **72 verified, 0 errors** (was 60 —
`+retype_install`, `+lemma_local_cap_edit_preserves_cspace_wf`, and the `impl Rights`
block moved into `verus!{}`: its bit consts, `has`, and the now-**verified** `masked`,
previously a standalone `assume_specification`). `cargo test -p kcore`: **18 passed**
(was 17 — `+retype_install_arms`). The aarch64 `kernel` cross-build is unchanged
(ghost code erases; the moved `impl Rights` erases back to the identical plain consts
the kernel already used). **No new `external_body` boundary** — 3c is pure positive
proof. The hard part was contract design + threading the channel arm's two inserts,
not solver time (`retype_install` verifies at rlimit 1, ~10× headroom).

---

## 1. What closed

- **`retype_install`'s functional contract** (`untyped.rs`). The §2.5 rights table
  is now a set of **theorems keyed on `ty`**: `Frame` inherits the untyped's rights;
  `Thread → THREAD_ALL`; a sub-`Untyped` is masked to `READ|WRITE` and so **provably
  never carries `PHYS`** — `(r & (READ|WRITE)) & PHYS == 0` ∀ `r`, via the `masked`
  spec + one `bit_vector` step (the "phys stays off ordinary derivation chains by
  construction" claim, ∀ rather than asserted). The new cap is a CDT child of the
  untyped (`dst.parent == Some(ut_slot)`), the watermark advances to `end - base`,
  and `cspace_wf` is preserved. The **channel arm** installs endpoint B in `dst2`,
  bumps the channel refcount to 2, and accounts both ends (`end_caps == [1, 1]`),
  framing every *other* channel unchanged.
- **The single-slot cap-edit lemma** —
  `lemma_local_cap_edit_preserves_cspace_wf(m0, k, v)` (`cspace.rs`): a `set_slot`
  that keeps `k`'s four CDT links and never turns a non-empty slot empty preserves
  `cspace_wf`. Every structural clause and both acyclicity ranks read only links
  (identical here) and per-slot emptiness (only ever relaxed), so the witnesses
  transfer unchanged. `retype_install`'s **three** `set_slot`s lean on it: the
  untyped's watermark bump (links + emptiness both fixed) and the two detached
  `dst`/`dst2` fills (an empty — hence detached — slot gains a cap with its links
  still null). This is the §3c analog of `derive`'s inline detached-insert block,
  factored once because it recurs.
- **`cdt_insert_child` gains three frame clauses** (`cspace.rs`), all trivially
  discharged by its `set_slot`-only body and purely additive (an extra `ensures` on a
  callee only adds facts, so `derive` — the only other caller — stays green): (i)
  `chan_view()` unchanged — the **front-loaded discovery** (detail §1.1): the channel
  arm calls `cdt_insert_child` *between* `endpoint_cap_added(A)` and `(B)`, so it must
  carry the channel's `chan_view` across the inserts; (ii) the parent's cap unchanged;
  (iii) the old first child's parent **and** cap unchanged. (ii)/(iii) let
  `retype_install` read `ut_slot.cap` (the watermark) and `dst.cap`/`dst.parent`
  (dst is the second insert's old first child) without fighting the `forall`
  caps-unchanged trigger — see §2.
- **`impl Rights` moved into `verus!{}`** (`cspace.rs`). The bit consts
  (`READ`/`WRITE`/`PHYS`/…/`ALL`/`THREAD_ALL`) and `has`/`masked` are now Verus-known,
  so the §2.5 rights theorem can name them. `masked` carries its bit-level `ensures`
  (`out.0 == self.0 & mask`) on the verified method itself; the standalone
  `assume_specification` it previously needed is **gone** — one fewer assumed contract.

### 1.1 Non-channel refs are *unchanged*, not bumped — a precision over the plan text

The detail plan (§3c) lists the channel postconditions as "`obj_refs` of the new
object bumped by one (non-channel), and the channel arm: …". The real body does **not**
bump for a non-channel object: each object's `init` (`CSpaceObj::init`, `Tcb::init`, …)
sets `refs = 1` *before* `retype_install` runs, so the installed `dst` cap is already
counted by that initial ref. The contract therefore states **`refs_view` and
`chan_view` unchanged** on the non-channel path, and `refs_view == old.insert(ch, 2)`
only on the channel path (endpoint B is the genuine second reference). This is the
faithful spec — the `test_store` differential check (`check_retype_install`) pins it
against the real body. (The same "follow the body, not the plan's loose paraphrase"
resolution as doc 26 §1.1's `reset` and doc 27 §1.1's `chan_wf` signature.)

---

## 2. Verus mechanics worth keeping (the untyped-install port template)

Building on doc 26 §2 / doc 27 §2 (full-path spec fns in contracts; `#[allow(unused
_imports)]` for ghost-only imports):

1. **Spec/proof items can't be `use`-imported — only full-path-referenced.** A
   `spec fn`/`proof fn` erases to *nothing* in a normal build, so a module-top
   `use crate::cspace::{cspace_wf, …, lemma_…}` is an **unresolved import** there
   (`E0432`), even under `#[allow(unused_imports)]`. The fix is the doc-26 idiom taken
   to its conclusion: reference `crate::cspace::cspace_wf(…)` / `…::lemma_…(…)` by full
   path *inside* the (erasing) contract/proof. Only real exec/struct items
   (`StoreSpec`, `ChanView`, `ObjId`) may be imported — and import only the ones a
   contract actually *names* (the channel postcondition reads `chan_view()` fields, so
   `ChanView` itself is never written → not imported).

2. **`bit_vector` needs a plain integer, not a datatype field.** `assert((ut_rights.0
   & 3) & 4 == 0) by (bit_vector)` fails — `ut_rights.0` is a field projection of the
   `Rights` datatype, which the bit-vector encoder rejects. Bind it first:
   `let b = ut_rights.0; assert((b & 3u8) & 4u8 == 0u8) by (bit_vector);`. And a const
   bit value needs `by (compute)` to fold: `assert(Rights::READ | Rights::WRITE == 3u8)
   by (compute)` (the `1 << n` const exprs are not auto-evaluated in spec).

3. **Associated consts of an external type must live in `verus!{}` to be named in
   verified code.** `Rights` is `external_type_specification`'d, but its consts lived
   in a plain `impl Rights` outside `verus!{}` — referencing `Rights::THREAD_ALL` from
   verified exec/spec then errors ("not supported"). Moving the `impl` into `verus!{}`
   makes them first-class; it also let `masked` shed its `assume_specification` for a
   verified body. (Erasure makes this invisible to the kernel: the consts come back as
   the same plain Rust.)

4. **Per-slot frame clauses beat re-instantiating a `forall` caps-unchanged
   ensures.** `cdt_insert_child`'s `forall|k| old.dom().contains(k) ==> final[k].cap
   == old[k].cap` is hard to instantiate at a *specific* `k` after the call (the
   trigger is the old-map's `dom().contains(k)`, an opaque post-call snapshot). Rather
   than fight the trigger at each query site, add **direct per-slot `ensures`** for the
   slots a caller actually reads — here the parent's cap and the old first child's
   cap/parent. They are one-liners, trivially provable from the body, and turn three
   would-be instantiation proofs in `retype_install` into free reads. This is the
   construction-side counterpart to doc 25's "quarantine the hard step into a helper".

5. **Thread one ghost snapshot per mutation, assert `=~=` after each.** The channel
   arm is five mutations deep (`set_slot ×3`, `set_obj_refs`, two `cdt_insert_child`,
   two `endpoint_cap_added`); capturing `m0`/`m_u`/`m_a` and asserting
   `store.slot_view() =~= <expr>` after each set_slot keeps the SMT's map model
   concrete, so the `lemma_local_cap_edit` precondition and the later frame reads land
   without search. `end_caps`'s `[0,0] →(A) [1,0] →(B) [1,1]` composes through the two
   `endpoint_cap_added` inserts at the same key (last write wins), read off as
   `end_caps[0] == 1 && end_caps[1] == 1`.

An adversarial multi-agent review (soundness / vacuity / test-teeth / conventions)
ran over the change. It confirmed the proof is sound and non-vacuous and drove three
test/cleanup fixes: the sub-`Untyped` fixture now uses `READ|PHYS` (so
`masked == READ` *differs* from `ALL`, giving the rights table teeth against a
`masked → ALL` mutation that the prior `0xff` fixture — where `0xff & (READ|WRITE) ==
ALL` — could not catch); a second channel exercises the "other channels untouched"
frame; and the dead `ChanView` import was dropped. The "non-deterministic
verification" and "missing `chan_wf` postcondition" claims were investigated and
dismissed (the former unreproducible across 40 runs / 20 seeds / rlimit 1; the latter
correctly 3d's job — see §3).

---

## 3. Scope held (what 3c did *not* touch)

- **`send`/`recv` are 3d**, **`endpoint_cap_dropped`/`bind` are 3e**,
  **`destroy_channel` stays `external_body`** — all remain plain Rust, untouched.
- **`retype_install` does not establish `chan_wf` for the new channel.** It frames
  `end_caps`/refs/the cross-channel view and leaves the structural fields to their
  `Channel::init` (the unverified trusted boundary). Per the detail §3c contract list
  and the §5 exit criterion, `chan_wf` is what 3d's `send`/`recv` *verify against*, not
  what `retype_install` *produces*; adding it here would need a new precondition
  (`retype_install` has no `ring_cap`/`msg_len` domain facts) and is a 3d design call.
- **No `CLAUDE.md` / spec edits** — the phase-3 closeout (moving the untyped + channel
  ops onto the proven list, recording the `signal`/`destroy_channel` residue) lands in
  **3e** per the detail plan; 3a–3d only seed their findings docs.
