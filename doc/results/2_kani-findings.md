# Kani verification findings

Bounded-model-checking results for the kernel object core (`kcore`), per the
rewrite plan `doc/plans/0_kani-rewrite.md` (§6 verification tier). Each harness
lives in `kcore/src/proofs/` under `#[cfg(kani)]` and is run by `cargo kani -p
kcore` (CI job `kani`, pinned cargo-kani **0.67.0**).

## Standing caveat (read before trusting any row below)

**Every result here is bounded.** Kani/CBMC proves a property over *all* inputs
and interleavings only within the stated scope: slot counts, queue depths,
op-sequence length, and loop-unwinding bounds. The bounds mirror the
TLC-checked `tla/cap_revocation/CapRevocation.tla` configuration (`CapIds=4`,
`Procs=2`, one channel of `QueueDepth=2`, `Threads=2`, `Notifs=2`) — the
model-checking tradition both tools share holds that the interesting
interleavings of this state space manifest at small scope, and TLC found the
design sound at exactly this scope. Bounds are recorded per-harness in
`kcore/src/proofs/bounds.rs`; scaling them up is a one-line change.

What is **out of scope by construction** (owned by other tiers, per plan §1):
unbounded proofs (revoke termination for arbitrary trees — TLA+ + code review);
concurrency (kernel is single-core, non-preemptible — Loom/Shuttle own
userspace); liveness/fairness; and anything behind inline asm, MMIO, or the
boot path (QEMU on-OS suites own those).

## Design notes (pinned current behavior, not bugs)

These are facts a harness must encode to pass; they are recorded so a future
reader does not mistake the encoding for an oversight. None is a defect under
the current spec; each is revisited only if the spec changes.

- **DN-1 — refcount can reach zero without a destroy call.** Three sites drop
  an object refcount with a bare `-= 1` rather than through `obj_unref`:
  `notification::signal`'s waiter release, `notification::remove_waiter`, and
  `channel::destroy_channel`'s binding release. Reaching zero this way does not
  trigger type-specific teardown (which is a no-op / disarm-only for the
  affected types, and the backing memory returns to the donor untyped via
  `revoke` regardless). Consequence for verification: the census invariant is
  stated as `hdr.refs == census`, which still holds at `0 == 0`; a stricter
  "object destroyed exactly when refs hits zero" assertion would produce
  spurious counterexamples and is deliberately **not** used. (Plan §7 item 4
  adjacent.)
- **DN-4 — `delete`'s recursive teardown is a CBMC tractability wall.**
  `delete` dispatches through `obj_unref`, whose `match` is on a cap kind
  read from slot memory; CBMC does not constant-fold it, so every arm is
  explored — including `destroy_cspace`/`destroy_channel`, which loop over
  (symbolic) slot counts and recurse back into `delete`. Deleting a
  **notification** cap stays tractable (its `destroy_notif` has no loops/
  recursion): `check_revoke` (≈193 s) and the concrete `check_delete_reparent`
  (≈2.5 s) both verify. But a single delete of a frame, channel, or cspace
  cap — or a nondet CDT shape layered on a delete — unrolls the recursive
  teardown into a formula that blows past the CI budget (many minutes). So
  the frame-unmap / peer-closed-fire-order / container-teardown behaviours
  are **not** Kani proofs; they are covered by the TLC-checked `CapRevocation`
  TSpec, the `delete` source order, and the QEMU suites (`m1-test.sh` step 6,
  `spawn-test.sh` reclaim loop) — see `kcore/src/proofs/teardown.rs`. Lifting
  the wall (e.g. `-Z stubbing` the `destroy_*` recursion, or a function
  contract on `obj_unref`) is deferred future work, not an unsound bound.

- **DN-3 — the CDT is a forest, not a single tree.** The kernel installs
  several parentless root caps directly (the boot caps in `kernel/src/main.rs`:
  the untyped, device/RTC frames, the init aspace), so there is no unique
  root. `cdt_unlink` of a root that has children re-parents them to the null
  parent, leaving them roots that still share sibling links. Consequence for
  verification: `cdt_wf` asserts the forest invariants (double-linked sibling
  lists, parent/first-child back-pointers, empty⇒detached, acyclicity, links
  in-universe) but deliberately does **not** assert "roots have no siblings".
  That property holds of freshly-built shapes — only `cdt_insert_child` makes
  siblings, always under a non-null parent — but `cdt_unlink` does not
  preserve it, and it is not a structural-integrity property. Asserting it
  would produce a counterexample (unlink a multi-child root) that is not a
  real defect. (Surfaced while writing `check_cdt_unlink`, plan §4.1.)

- **DN-2 — fire-before-reclaim ordering is end-state-unobservable in general.**
  `cspace::delete` fires `endpoint_cap_dropped` (peer-closed) strictly before
  `obj_unref` (source order: `kernel/src/cspace.rs`), satisfying the TSpec
  `ChannelFireSafe`/ordering obligation. But because `destroy_notif` is a no-op
  and harness memory is never freed, a pure post-state check cannot distinguish
  the two orderings when no environment-visible event fires. The ordering is
  therefore proven *observably* on a representative world (a blocked waiter ⇒ a
  `make_runnable` event, a mapped frame ⇒ an `aspace_unmap` event; the unified
  ghost event log's order is the witness); the universal claim rests on source
  order plus that representative proof, stated here and in the harness doc
  comment.

## Findings

The §4.1 CDT/teardown suite found no defects. The §4.2 untyped/retype suite
found two (UO-1, UO-2) — both carve-arithmetic overflows predicted by plan
§7.1, confirmed and fixed alongside the harness; recorded in
`doc/results/3_kani-findings-2.md`. The §4.3 channel suite found no defects;
its notes (DN-5, the DN-4 refinement, and the harness-cost lesson) are in
`doc/results/4_kani-findings-3.md`. Every Kani-found bug gets a minimized
regression harness kept forever (like a fuzz seed), a fix PR, and a row in the
relevant findings file.

| ID | Date | Harness | Bounds | Severity | Description | Status | Fix PR |
|----|------|---------|--------|----------|-------------|--------|--------|
| —  | —    | —       | —      | —        | (no §4.1/§4.3 defects; §4.2 in `3_kani-findings-2.md`) | — | — |

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Recorded so a regression in solver cost is visible. Measured on the dev
machine (cargo-kani 0.67.0); CI runners differ but the ratios hold.

| Harness | Bounds | Time |
|---------|--------|------|
| `check_cdt_insert_child` | `POOL_SLOTS=4` | ~76 s |
| `check_cdt_unlink` | `POOL_SLOTS=4` | ~101 s |
| `check_slot_move` | `POOL_SLOTS=4` | ~114 s |
| `check_derive_*` (×3) | 2 slots | <2 s each |
| negatives (×4) | minimal | <2 s each |
| `check_revoke` | `World` (28 slots) | ~193 s |
| `check_delete_reparent` | concrete 3-node | ~3 s |
| `check_cdt_transition_system` | bare pool, K=2 | ~131 s (K=3 ≈ 297 s) |

A 6-slot pool put `check_cdt_insert_child` at ~387 s (over budget); 4 slots —
which is exactly TLA `CapIds` — brings the nondet-shape harnesses well under
the cap (plan §3, §9). The `cdt_wf` membership check is the `O(n²)` cost
driver, so the bound is load-bearing.
