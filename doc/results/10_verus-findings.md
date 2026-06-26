# 10 — CapRevocation `FireSafe` as a binding-slot corollary + report label (Task 10)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 10 (Tier 3,
TLA-routed; "corollary + label", **Effort S, Risk low**). Outcome: **verified, shipped,
first attempt.** `cargo clean -p kcore && cargo verus verify -p kcore` reads
**`406 verified, 0 errors`** (cold, authoritative; results line present), up `404 → 406`
from the two new `proof fn`s. No reverts of shipped code (one measurement experiment was
run then reverted, see below); no new trusted seam (tally stays 14); the real
attributable `rlimit` cost is ~113 k (0.08 % of the kcore own-fn total).

## What was attempted

The rev2§5.1 *firing obligation* — a non-NULL TCB binding slot always names a live cap, so
a thread-death fire (`thread::report_terminal` → `notification::signal`) signals a live
object or skips a cleared slot, never freed memory — lived only as (a) the TLA `FireSafe`
invariant (`tla/cap_revocation/CapRevocation.tla:388`) and (b) an unnamed prose paragraph
in `report_terminal`'s doc comment (`kcore/src/thread.rs:138-147`). It is already
*entailed* by the verified `caps_consistent` system invariant: a resident bind slot's
`Notification(nn)` cap is non-empty, so `caps_consistent` ⇒ `cap_consistent` ⇒
`notif_wf(nn)` ⇒ `notif_view.dom().contains(nn)` (live). The task names that entailment.

**Phase 1 — `fire_safe` + corollary lemma** (`kcore/src/cspace.rs`, next to
`caps_consistent`):

```rust
pub open spec fn fire_safe<S: Store>(store: &S) -> bool {
    forall|t: ObjId, k: int| #![trigger store.tcb_view()[t].bind_slots[k]]
        store.tcb_view().dom().contains(t) && 0 <= k < store.tcb_view()[t].bind_slots.len()
            && store.slot_view().dom().contains(store.tcb_view()[t].bind_slots[k])
            ==> (cap_notif(store.slot_view()[store.tcb_view()[t].bind_slots[k]].cap)
                    matches Some(nn) ==> store.notif_view().dom().contains(nn))
}
pub proof fn lemma_fire_safe_from_caps_consistent<S: Store>(store: &S)
    requires caps_consistent(store), ensures fire_safe(store) { /* instantiate per slot */ }
```

The implication form (`cap_notif Some ⇒ live`) is faithful to TLA's `= NULL \/ ∈ live` (a
bind slot holds only an empty or a notification cap), conditioned on slot *residency* so it
is entailed by `caps_consistent` alone (no extra TCB-residency invariant needed).

**Phase 2 — named `ensures` on `report_terminal`** (the firing site), in the conditional
idiom `signal`/`fire` use for system invariants:

```rust
ensures cspace::caps_consistent(old(store)) ==> cspace::fire_safe(final(store)),
```

Discharged by **Route B** — the corollary on `old`, then a *light* frame lemma
(`lemma_fire_safe_frame`, read-set = slot caps + `bind_slots` + notif domain) carrying
`fire_safe(old)` to `fire_safe(final)`. This is far cheaper than **Route A** (re-deriving
`caps_consistent(final)` across `signal` by replicating `fire`'s ~50-line
`lemma_caps_consistent_frame` discharge). One inline `assert` was needed: the notif-domain
equality across `signal` rests on `n` being resident (in scope only inside the
`if let Notification(n)` block), so it is established there (and holds trivially on the
no-fire path), surviving the branch merge. The three other frame facts (slot_view eq, tcb
dom eq, per-thread `bind_slots` eq) auto-composed across `set_tcb_report` + `signal`.

**Dependency confirmed (not a new doc).** `caprevoke-liveparent-ensures-guide` does not
exist as a doc; it is satisfied by confirming TLA `LiveParent` (`CapRevocation.tla:380`) is
mechanized as the `cap_consistent` Thread-arm (bind slots + cspace resident) +
Notification-arm (`notif_wf ⇒ live`) under `caps_consistent`, which
`bind`/`destroy_tcb`/`revoke_step` all require and ensure. Recorded in the ledger routing
note.

## Result

- `cargo clean -p kcore && cargo verus verify -p kcore` → **`406 verified, 0 errors`**
  (cold, results line present). The `+2` are `lemma_fire_safe_from_caps_consistent`
  (22,383 rlimit) and `lemma_fire_safe_frame` (20,375); `fire_safe` is a non-recursive
  `spec fn` (+0). `cargo build -p kcore` + `cargo test -p kcore` → 110 passed, 0 failed
  (the `report_terminal` host tests + randomized sweeps; erasure leaves exec unchanged).
- **`rlimit` (proof cost) — no real regression.** Cold `scripts/verus-baseline.sh kcore`
  before/after, comparing the kcore own-fn (`kcore::*`) subset (the only valid control —
  doc 9 §"measurement caveat"):

  | metric | pre | post | delta |
  |---|---|---|---|
  | kcore own-fn `rlimit` total | 149,497,179 | 153,410,234 | +2.62 % (see below) |
  | `thread::report_terminal` | 125,416 | 195,907 | +70,491 |
  | `cspace::lemma_fire_safe_from_caps_consistent` | — | 22,383 | new |
  | `cspace::lemma_fire_safe_frame` | — | 20,375 | new |

  The +2.62 % total is **almost entirely Z3 nondeterminism on untouched code**:
  `cspace::delete` alone swung +3,387,130 (3.53M → 6.92M) on byte-identical source between
  the two runs (the doc-9 pattern — there it swung −951 k), while the heavy hitters I did
  not touch were bit-stable (`signal` +0, `destroy_tcb` +0, `remove_waiter` +0,
  `cdt_unlink` +0). The **real attributable cost is ~113 k** (the two lemmas + the
  `report_terminal` frame proof), 0.08 % of the total; `report_terminal` grew +56 %
  relatively but +70 k absolutely — trivial against the 20–25M heavy hitters, and the price
  of a genuine fire-safety preservation proof, not a label over an already-implied clause.

## Phase 3 — measure-then-decide: NOT added to `revoke_step`/`destroy_tcb`

**Decision: omit.** Both already `ensure caps_consistent(final)`, so `fire_safe(final)` is
a zero-cost corollary at any call site via `lemma_fire_safe_from_caps_consistent` — an
explicit `ensures fire_safe(final)` would add proof surface (and a future-edit obligation
on the crate's heaviest ops) for **no new derivable fact**.

The measurement (run, then reverted) refutes the *cost* concern the plan flagged: adding
`ensures cspace::fire_safe(final(store))` + a one-line corollary discharge to `destroy_tcb`
left its cold `rlimit` **flat** (24,609,374 → 22,037,728 — *down*, inside the `delete` ±3M
noise band), and the crate still read `406 verified, 0 errors`. The §10
establish-vs-consume "~doubling" is a *consume*-side risk; `destroy_tcb`/`revoke_step`
*establish* `fire_safe` (the cheap side), so cost is not the deciding factor — **redundancy
is**. (`revoke_step` would additionally require restructuring its multi-arm `match` tail to
land a single discharge, more churn for the same redundant label.) This matches the plan's
default ("keep just the lemma + report label") and keeps the change minimal (Effort S). The
finding generalized the §10 establish-vs-consume note in `verus.md` (below).

## What stayed TLA-owned (no over-claim)

The TLA `CapRevocation` model is **not** retired or demoted. Only the local per-step
corollary moved to Verus. The *global* cross-restart arm stays the design oracle:
`DeadNowhere` over the whole `CapIds` space (`CapRevocation.tla:374`, which *implies*
`FireSafe`) and the preemptible revoke walk's `EventuallyRevoked` liveness. The
`report_terminal` doc-comment prose at `thread.rs:140` ("FireSafe — discharged by the body
verifying") now reads as a named `ensures`, not just a comment.

## Reverted / kept

- **Kept (shipped):** `cspace::fire_safe` + `lemma_fire_safe_from_caps_consistent` +
  `lemma_fire_safe_frame` (`kcore/src/cspace.rs`); the conditional `fire_safe` `ensures` +
  its frame proof on `report_terminal` (`kcore/src/thread.rs`); the ledger FireSafe routing
  note + Baseline row `404 → 406` (tally stays 14); the `verus.md` technique note.
- **Reverted:** the Phase-3 measurement experiment (the `fire_safe` `ensures` + discharge
  temporarily added to `destroy_tcb` to measure cost). No incidental code changes were
  needed; nothing else reverted.

## Proposed additions to `doc/guidelines/verus.md` (applied)

A §10 paragraph on **naming a whole-store predicate as an `ensures` on a function that
*establishes* it**: the establish side is the *cheap* side (only the corollary call, not a
consumer's re-unfold — measured flat on `destroy_tcb`), so on a producer the reason to omit
the label is **redundancy** (the op already ensures the stronger invariant `Q`, from which a
`Q ⇒ P` corollary gives `P` free at every call site), not cost. Reserve the explicit label
for the one operation the property is *about* (the firing site), and discharge it across the
op's own body with a *light* frame lemma (slot caps + `bind_slots` + notif domain), never a
re-run of the heavy `caps_consistent` frame. Serves the sibling labeling Task 12 and any
future "name an entailed invariant on the op it bites" work.
