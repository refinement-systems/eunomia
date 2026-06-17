# Verus findings 34 — Phase 6d-final-thread-body-2: `destroy_tcb`'s body proven, the cross-module SCC closed — **phase 6d complete**

Plan: `doc/plans/3_verus-rewrite.md` (§4.1) and `doc/plans/3_verus-rewrite_phase6-detail.md`
(§2 "6d", the *6d-final-thread-body-2* residue named in doc 53 §3). Prior increment: `53`
(6d-final-thread-body — the `dead_tcb_frozen` cross-object frame threaded through the whole
teardown call graph, with `destroy_tcb`'s body **drafted but deferred** on a `remove_waiter`
field-frame + an rlimit decomposition). This increment **closes that body**: `destroy_tcb`
loses its `external_body` attribute and its real teardown is verified, closing the last
cross-module recursion cycle of the rewrite.

`cargo verus verify -p kcore`: **305 verified, 0 errors** (was 295 after doc 53's baseline —
the body plus nine new foundation lemmas). `cargo test -p kcore`: **82 passed** (unchanged —
`destroy_tcb_structural`/`check_destroy_tcb` now differentially check a *proven* contract). The
aarch64 `kernel` cross-build is unchanged (every change is ghost, a contract, or a
behavior-equivalent body reorder; confirmed `cd kernel && cargo build`).

**Phase 6d is complete.** `delete`, `destroy_channel`, and `destroy_tcb` are now all proven
bodies; **the kernel object-core teardown family carries zero `external_body`** (the only
`external_body` left in `kcore` is the pre-existing non-object-op residue outside 6d scope).
The cross-module mutual recursion `delete → obj_unref → destroy_{cspace,channel,tcb} → delete`
— the seL4-zombie cluster the whole rewrite was reaching for — terminates as a Verus theorem.

---

## 1. What landed

- **`destroy_tcb`'s body, proven** (`thread.rs`). `#[verifier::external_body]` is gone; the real
  teardown — **detach** (`unqueue_ready` / `remove_waiter`) → **halt** (clear `qnext`/`wait_notif`,
  set `Halted`; report untouched) → **bind-slot `delete`s** → **clear-before-unref** cspace/aspace
  releases — is verified against the full doc-53 contract (`cspace_wf`/`refcount_sound`/
  `caps_consistent`/`end_caps_sound`/`census_dom_complete`/the `count_nonempty` non-increase/the
  residency + channel-skeleton frames/the except-`t` `dead_tcb_frozen`).

- **The cross-module SCC closed.** Un-`external_body`-ing `destroy_tcb` makes the cycle
  `destroy_tcb → unref_cspace → destroy_cspace → delete → obj_unref → destroy_tcb` visible, so
  `unref_cspace` joins the SCC. Both gained the shared lexicographic measure
  `decreases (count_nonempty(slot_view), height)` with **`destroy_tcb = 3`, `unref_cspace = 2`**
  (above `destroy_cspace = 1` / `delete = 0`, below/equal `obj_unref = 4` / `destroy_channel = 3`).
  The count-dropping edge stays `delete → obj_unref` (delete empties its slot first); every
  count-flat edge drops the height tag.

- **`remove_waiter`'s field-frame** (`notification.rs`, doc 53 §3 blocker 1). Its present-branch
  `ensures` now exposes that the splice preserves `t`'s **non-queue** TCB fields
  (`cspace`/`aspace`/`state`/`report`/`retval`/`bind_bits`/`bind_slots`) — it writes only `t`'s
  `qnext`/`wait_notif`. `destroy_tcb` reads this across the BlockedNotif detach to keep `t`'s
  holds (for `unref_cspace`/`unref_aspace`'s resident-wf) and report/state/bind-slots intact.

- **The clear-before-unref census lemmas** (`cspace.rs`, doc 53 §3's two named lemmas).
  `lemma_census_after_hold_clear` (cspace) + `lemma_census_after_hold_clear_aspace`: clearing a
  halted, queue-detached thread's hold (`Some(x) → None`, every other field fixed) drops the
  census by exactly one at `x` and nowhere else, producing **`census_off_by_one(s1, x)`** (and
  `census_dom_complete(s1)`) — the precise window `unref_cspace`/`unref_aspace` consume. The five
  non-thread-hold terms are framed (`lemma_thread_hold_*_drop` for the dropping term;
  `lemma_waiter_refs_frame_dequeued` for the waiter term, since `cspace`/`aspace` is a field
  `waiter_chain` never reads and `t` is off every chain).

- **Five supporting lemmas** (`cspace.rs`): `lemma_no_live_thread_cap_from_dead` (a `refs[t]==0`
  object is designated by no live cap — discharges the halt-clear frame's "no live `Thread(t)`
  cap"); `lemma_census_frame_thread_halt` (the halt edit freezes the census — `t` off every chain
  in both states, holds fixed); `lemma_refcount_sound_from_census_eq` + `lemma_sysinv_frame_equal_views`
  (carry the four system invariants across census-fixed / all-views-equal edits); and the
  **except-`t` composition trio** `lemma_dead_tcb_frozen_{to_except, except_single_t, except_trans}`
  — the `_trans` analog restricted to `x != t`, since `destroy_tcb` rewrites its own subject so the
  *full* frame cannot hold across its body, only the except-`t` one `obj_unref`'s Thread arm needs.

---

## 2. Findings worth keeping

- **Clear-before-unref is the load-bearing ordering, and it is behavior-equivalent.** The release
  must clear `tcb[t].cspace` *then* call `unref_cspace` (not the reverse): the clear drops the
  census at `cs` first, opening the `census_off_by_one(·, cs)` window the unref's `-1` then closes,
  so `refcount_sound` is only ever transiently false in the one direction the unref's contract
  expects. The runtime effect is identical — `destroy_cspace` fires on `cs` and never reads
  `tcb[t]` — so the reorder of the old `external_body` body (which unref'd then cleared) is sound.
  The halt step must also **clear `wait_notif`**: that is what makes `t` itself satisfy
  `dead_tcb_frozen_at`'s antecedent (`refs[t]==0 ∧ wait_notif is None`), so the frame fixes `t`'s
  own `report`/`state`/`qnext` across the recursive `delete`/`unref_cspace` calls.

- **The off-chain frame needs the `dequeued` form, not the simple disjunct.** A thread can be
  `BlockedNotif`-on-`wn` yet *absent* from `wn`'s queue (the `remove_waiter` absent path leaves it
  so). The simple `wait_notif is None || state != BlockedNotif` off-chain test fails that state, so
  `lemma_census_frame_thread_halt` keys on the full `forall ws. waiter_chain(·,o,ws) ⟹ !ws.contains(t)`
  (via `lemma_waiter_refs_frame_dequeued`) instead. `destroy_tcb` discharges it per detach branch:
  Runnable/no-wait → first/second disjunct; BlockedNotif-spliced → cleared `wait_notif`;
  BlockedNotif-absent → the *third* disjunct (`Some(wn)` + `notif_wf(wn)` + `!waiter_seq(wn).contains(t)`,
  the latter two read off `remove_waiter`'s ensures).

- **Map-`insert` dom-preservation needs the key-in-domain witness, explicitly.** Three
  `set_tcb_*(t, …)` re-inserts leave the tcb domain unchanged *only because* `t` is already in it;
  Verus would not discharge `s1.tcb_view().dom() == s0.tcb_view().dom()` until handed
  `assert(s0.tcb_view().dom().contains(t))` + an `=~=` extensionality nudge. The lesson generalizes:
  after a sequence of same-key inserts, assert the key is in-domain before claiming dom equality.

- **The rlimit decomposition is what keeps CI cross-platform-robust (doc 53 §3 blocker 2).** The
  doc-53 monolithic body attempt hit the solver limit; this increment keeps the body's inline proof
  to lemma calls + small single-state asserts, pushing every multi-term census/frame argument into a
  named `proof fn` (the doc-25 §2 discipline). Two concrete instances: the five
  `remove_waiter`-present-branch field-frame asserts had to be **single-key** (`tvf[t].X == tv0[t].X`),
  not domain `forall`s — a `forall` blew the already-hot splice-loop's rlimit (doc 53's exact
  failure mode); and the `caps_consistent`-across-equal-views step had to be a **per-slot
  `cap_consistent` forall** (the `unref_aspace`-body pattern), since a bare `assert(caps_consistent)`
  chokes on `notif_wf`/`timer_chain`'s existential/`choose` triggers. Result: **305 verified, no
  rlimit warning** — no `rlimit` bump or `spinoff_prover` needed.

- **`cap_consistent`'s Timer arm reads `timer_head_view`.** An "all object views equal" frame must
  include `timer_head_view` (not just `timer_view`) or the Timer-cap consistency clause's
  `timer_wf` (which threads the head) fails — the silent omission that cost one verify cycle.

---

## 3. Coverage of the cross-module recursion path

The exhaustive guarantee is the Verus proof: the SCC's `decreases` is checked over all members'
real bodies for **all** store shapes, so the deep case (a TCB whose bound cspace holds a channel
holding a queued cspace cap, etc.) is covered unconditionally. The host `destroy_tcb_structural` /
`check_destroy_tcb` differentially check the now-proven contract against the real `ArrayStore`
body on the non-recursive shape (a Runnable thread with two notification bind caps); the recursive
teardown is additionally exercised at runtime by the on-os integration tests (`scripts/spawn-test.sh`
reap loop, `scripts/m1-test.sh` revocation). A hand-built deep-recursion host fixture (the plan's
optional Step 4) was **not** added — constructing a valid store satisfying all six teardown
preconditions (resident-wf bound cspace, census-sound refs, …) is fragile, and the proof already
covers the path exhaustively; recorded here rather than added.

---

## 4. Doc / CLAUDE.md

No `CLAUDE.md`/spec edit this increment — per the doc-30 §3 / doc-53 §4 convention the sub-phase
closeout edits ride **phase 6f** (the system-invariant-on-construction-ops + documentation
closeout). The milestone is now true and will be recorded there: **the kcore teardown family
carries zero `external_body`**, and `delete`/`revoke`(survival)/`obj_unref`/`destroy_cspace`/
`unref_cspace`/`unref_aspace`/`channel::destroy_channel`/`thread::destroy_tcb` move onto the proven
list — retiring the phases-2…5 trusted-residue note. `cargo verus verify -p kcore` runs with no
per-proof filter, so `destroy_tcb`'s body, the `unref_cspace` SCC measure, the `remove_waiter`
field-frame, and the nine new foundation lemmas all auto-gate; `host-tests` reruns
`destroy_tcb_structural`/`check_destroy_tcb` against the proven contract.

**Doc-numbering note.** Plan §6-detail budgeted 6d as a single findings doc (44); it instead spans
docs **44–54** (the sub-phase "turned out harder than expected", doc 53 §3) — the
`dead_tcb_frozen` cross-object frame and this body-closure being the two hardest steps. The
master-plan §7 renumber (teardown = phase 6; host chokepoints → 7; commit core → 8; closeout → 9)
is unchanged and is recorded in the 6f closeout. **Next: 6e** (revoke root-survival, the
conditional non-zombie theorem) **then 6f** (the system invariant + closeout).
