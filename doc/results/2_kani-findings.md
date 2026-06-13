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
- **DN-4 — `delete`'s recursive teardown CBMC wall — RESOLVED (`-Z stubbing`).**
  `delete` dispatches through `obj_unref`, whose `match` is on a cap kind read
  from slot memory; CBMC does not constant-fold it, so when a `delete` is the
  *top-level* entry every arm is explored — including `destroy_cspace` /
  `destroy_channel` / `destroy_tcb`, which loop over slot counts and recurse
  back into `delete`. This bites even a concrete `Frame` cap: the discriminant
  is symbolic once stored to and reloaded from a slot, so the recursion unrolls
  into a formula that never finishes unwinding (`check_delete_frame` without
  stubs did not complete in 420 s). The original entry deferred frame-unmap,
  fire-order, and container teardown to the TSpec + QEMU as a result.

  These are now **real Kani proofs**, split so each piece is tractable and the
  two compose (see `kcore/src/proofs/teardown.rs`):
  - **Teardown bodies, by direct call** (entry is the destructor, no top-level
    `obj_unref` → no recursion blowup): `check_destroy_cspace` (≈2.2 s, a dying
    cspace deletes every resident), alongside the already-existing
    `check_destroy_channel` (§4.3) and `check_thread_teardown` (§4.4) — the same
    structural pattern.
  - **`delete`/`obj_unref` dispatch, with the recursive arms stubbed** to
    no-ops (`teardown::stub`, enabled by `-Z stubbing` on the kcore CI run):
    `check_delete_frame` (≈9.1 s — the §2.5 mapped-frame `aspace_unmap` +
    `unref_aspace`, whose logic lives in `delete` itself and calls none of the
    stubbed destructors) and `check_delete_cspace` (≈1.1 s — the
    `CapKind::CSpace => destroy_cspace` routing + refcount-to-zero). Stubbing
    removes only the recursion CBMC cannot prune cheaply; the bodies it would
    re-derive are the direct proofs above. Generic-fn stubbing was confirmed
    working under cargo-kani 0.67.0; enabling `-Z stubbing` did not regress the
    non-stubbed harnesses (`check_revoke`, `check_delete_reparent` re-verified).

  Fire-before-reclaim on a real `delete` remains `check_teardown_fire_safe`
  (§4.3, TSpec `ChannelFireSafe`; DN-2 for the universal claim). The residual,
  stated honestly: **deeply nested** container teardown (a container whose
  resident is itself a live container → multi-level recursion) stays TSpec +
  QEMU-covered (`spawn-test.sh` reclaim loop); the proofs here cover one level
  of recursion with leaf (notification) residents.

  *Routing now witnessed (review-2 rec. 3, `15_kani-findings-12.md`):* the
  stubs are no longer silent — each records a `GhostEvent` for the destructor
  arm `obj_unref` dispatched to, so `check_delete_cspace` and the new
  `check_delete_channel`/`check_delete_tcb` analogs *assert* the dispatch
  reached the right teardown (the way `check_delete_frame` already witnesses its
  `AspaceUnmap`), closing the one-level dispatch's last source-only seam.

- **DN-12 — destructive ops don't fit a *nondet multi-step* transition
  harness (post-DN-4).** Closing DN-4 made *single concrete* `delete`s
  tractable, but **not** K independent nondet deletes: putting `delete`/`revoke`
  in the K-step `check_cdt_transition_system` sequence OOMs CBMC (the 4-op
  alphabet at K=2 is ~9.3 M SAT vars; even 3-op OOMs at K=2, verifies only at
  K=1), and CBMC emits spurious unwinding-assertion failures because it can't
  bound the `cdt_unlink`/`slot_move` walks without `cdt_wf` as an *assumption*
  (an exhaustive plain-Rust replay of all length-2 sequences confirmed the
  invariants actually hold — no real bug). The sound resolution: `delete` is
  checked *inductively* — one op over a nondet asserted-wf shape
  (`check_delete_step`, generalizing `check_delete_reparent` to all shapes);
  `revoke`'s symbolic-tree walk OOMs even inductively, so it stays the concrete
  `check_revoke`. The additive `derive`/`slot_move` sequence rose to K=3. Full
  write-up: `doc/results/10_kani-findings-8.md`.

- **DN-13 — `kani::cover!` is informational, not a gate.** The nondet harnesses
  carry `kani::cover!` reachability checkpoints (rec. #3) so an over-constraining
  `assume` can't make a proof vacuous. But cargo-kani 0.67 does **not** fail a
  run when a cover is unreachable — it only lowers the `N of M cover properties
  satisfied` tally (the check shows `Status: UNSATISFIABLE`). So the CI kani job
  post-checks each run's log and fails if any `N != M`. 15 harnesses / 41 covers,
  all satisfied; no vacuity bug. (Also: never put `matches!` inside `cover!` — it
  spawns a spurious unreachable sub-cover; use `==`.) Full write-up:
  `doc/results/11_kani-findings-9.md`.

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
`doc/results/4_kani-findings-3.md`. The §4.4 notification + thread-report suite
found no defects; its notes (DN-6 and the DN-4 reappearance in `destroy_tcb`)
are in `doc/results/5_kani-findings-4.md`. The §4.5 aspace suite (the §2.4
walker rewrite) found one — AS-1, a `PERM_DEVICE | PERM_X` executable-MMIO gap,
confirmed and fixed alongside the harness; its notes (DN-7, the QEMU gate) are
in `doc/results/6_kani-findings-5.md`. The §4.6 syscall-decode suite (the §2.5
split into `kcore::sysabi`) found no defects — it makes the existing argument
validations checkable; its notes (DN-8 the 6-register ABI, DN-9 the benign
decode-first error precedence) are in `doc/results/7_kani-findings-6.md`. The
§4.7 host-side suite (`urt`/`ipc`/`cas`/`dma-pool` — tier 2) found no defects;
it adds the new verified `ipc::header` codec and maps where Kani pays vs. where
proptest/fuzz/Loom own the property (DN-10 the tractability limits — symbolic
u128 division, the dma free-list and tlv `Vec`-parse OOMs; DN-11 the
`cas::hash` stub axiom), in `doc/results/8_kani-findings-7.md`. Every
Kani-found bug gets a minimized regression harness kept forever (like a fuzz
seed), a fix PR, and a row in the relevant findings file.

| ID | Date | Harness | Bounds | Severity | Description | Status | Fix PR |
|----|------|---------|--------|----------|-------------|--------|--------|
| —  | —    | —       | —      | —        | (no §4.1/§4.3/§4.4 defects; §4.2 in `3_kani-findings-2.md`) | — | — |

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
| `check_destroy_cspace` | `World`, 2 notif residents | ~2 s |
| `check_delete_frame` | `World`, stubbed destructors | ~9 s |
| `check_delete_cspace` | `World`, stubbed destructors | ~1 s |
| `check_cdt_transition_system` | bare pool, K=3 (derive/move) | ~315 s |
| `check_delete_step` | nondet shape, `POOL_SLOTS=4` | ~160 s |

A 6-slot pool put `check_cdt_insert_child` at ~387 s (over budget); 4 slots —
which is exactly TLA `CapIds` — brings the nondet-shape harnesses well under
the cap (plan §3, §9). The `cdt_wf` membership check is the `O(n²)` cost
driver, so the bound is load-bearing.
