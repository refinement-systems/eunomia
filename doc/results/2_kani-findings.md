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

None yet — harnesses land per plan phases 3–4. Every Kani-found bug gets a
minimized regression harness kept forever (like a fuzz seed), a fix PR, and a
row here.

| ID | Date | Harness | Bounds | Severity | Description | Status | Fix PR |
|----|------|---------|--------|----------|-------------|--------|--------|
| —  | —    | —       | —      | —        | (no findings yet) | — | — |
