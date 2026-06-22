# B14C findings — IPC verification doc honesty + ledger reconciliation

**Phase:** B14C of `doc/plans/15_b14-detail.md` (IPC reactor verification + TLA completion), the
**finishing** sub-phase. Closes the two audit **§4.3** doc over-claims
(`doc/results/0_audit_rev0.md:528-536`) and records, in the one source of truth, the
mechanized-vs-test-routed split for the IPC layer (the rev1§6.1 "no trust-routed property
mistaken for mechanized" discipline). It lands after **B14A** (PR #159 — the completed TLA model,
`22_b14a-findings.md`) and **B14B** (PR #160 — the dispatch/marshalling proptests + the verified
allocator core, `23_b14b-findings.md`), folding their final numbers into the ledger.

B14C is **documentation + ledger only**: it changes no Rust logic, no TLA model, no wire/on-disk
bytes, and no public type. The `IpcReactor` TLA gate (39 states), the `cargo verus verify -p ipc`
gate (62/0), the trusted-seam tally (14), the Loom/Shuttle harnesses, and the QEMU smoke are all
untouched — B14C makes the prose around them honest, it does not move them.

## Audit items closed

- **§4.3 — the Loom docstring over-claims Relaxed fidelity [medium]** (`0_audit_rev0.md:528-532`).
  The `urt/src/time.rs` `loom_tests` docstring claimed a proof over *"every C11-permitted
  interleaving **and reordering**."* Loom does **not** faithfully model Relaxed-atomic reordering;
  the seqlock's correctness rests on the explicit `fence(Release)`/`fence(Acquire)` pair (the data
  fields are loaded/stored `Relaxed`), which Loom *does* model and enumerate interleavings around.
  Fixed: the docstring now states the honest scope — the conclusion holds **via the fence**, not
  via faithful Relaxed reordering — and cites the non-certifying Shuttle note below it as the
  correct-posture precedent. A precision fix to an otherwise-correct tool choice, not a
  tool-defusing.
- **§4.3 — Loom adds little over Shuttle for the IPC model [medium]** (`0_audit_rev0.md:533-536`).
  `ipc/` contains **no atomics** — it synchronizes via `crate::sync` `Mutex`/`Condvar` — so Loom's
  distinctive value (weak-memory atomic reorderings) is moot for this crate. Recorded: a new
  Loom-vs-Shuttle note in `ipc/src/model.rs`'s module doc names **Shuttle** as the load-bearing
  concurrency tool for these harnesses, and states that the "Loom-certified" framing carries less
  weight than for the `urt` seqlock (where Loom models the load-bearing Acquire fence). Harmless
  choice, honestly scoped.
- **rev1§6.1 record — mechanized vs test-routed** (the discipline, `spec_rev1.md:411`). The ledger
  now records, in the GC-sufficiency-note style, that the reactor's **multi-source dispatch** and
  the endpoint **cap-marshalling** are *neither* TLA-mechanized *nor* a trusted seam — they are
  proptest-routed (B14B) and Loom/Shuttle-routed (`model.rs`), with only the pure `lowest_clear_bit`
  bit-scan core Verus-mechanized — so no reviewer reads the B14A TLA completion or the allocator
  proof as covering the full multi-source dispatch.

## What landed (doc/ledger only)

1. **`urt/src/time.rs`** — `loom_tests` docstring rewritten. Drops the "every C11-permitted
   interleaving *and reordering*" claim; adds an **"Honest scope (audit §4.3)"** paragraph naming
   the `fence(Release)`/`fence(Acquire)` pair as the load-bearing modeled construct and stating
   Loom does not faithfully model Relaxed reordering. No code change (docstring only).
2. **`ipc/src/model.rs`** — module-doc **"Loom vs Shuttle for this crate (audit §4.3, honest
   scope)"** paragraph added: `ipc/` is atomics-free → Shuttle is the load-bearing tool; the Loom
   variant is harmless but carries less weight than the `urt` seqlock; the *sequential* dispatch is
   a third, lower tier still (proptest-routed, `reactor.rs`'s `mod proptests`). Complements — does
   not duplicate — the existing sequential-dispatch note at `reactor.rs:283-290`.
3. **`doc/guidelines/verus_trusted-base.md`** (the ledger) — four reconciliations + one drive-by:
   - **Verus Baselines row** relabelled "IPC header + session codecs **+ reactor bit-allocator
     core**" and the figure raised **58 → 62 verified, 0 errors**, recording the +4: the verified
     `used`-mask allocator core `lowest_clear_bit` (lowest-clear-bit correctness,
     no-double-allocation, the 64-bit structural bound), a pure `u64` bitmask over `vstd`'s
     `axiom_u64_trailing_zeros`, the kcore ready-queue-bitmap pattern; **no new trusted seam**.
   - **Scope-of-verified-surface prose** notes the reactor's verified `lowest_clear_bit` core
     joined the surface in B14B (detail deferred to the Baselines row).
   - **TLA Baselines row** — the `IpcReactor` entry expanded from "(with a negative control)" to the
     completed description matching the `CommitProtocol`/`CapRevocation` detail level: the
     bind/register + poll-once, the symmetric writable/backpressure half, the 3-state receiver, the
     new `NoLostWakeupWritable` invariant, **39 distinct states** (59 generated, depth 13), the
     **three committed negative-control cfgs** and the violation each reports, and the S-12 cfg-comment
     pin — ending with the single-source-by-design routing pointer.
   - **Test-routed note** (GC-sufficiency-note style) added recording the multi-source-dispatch /
     cap-marshalling routing (above).
   - **Drive-by tally-consistency fix** — the urt-arena-seam note still read "not one of these
     **13**" / "unchanged at **13**" (stale from B11C; the tally was bumped to 14 in B13B and both
     the section heading and the Tally line already say 14). Corrected both to **14** so the file
     is internally consistent with the "tally stays 14" claim B14C reaffirms.
4. **`tla/ipc_reactor/IpcReactor.tla` header** — **confirm-only, no edit.** B14A already rewrote the
   header (`:1-85`) to describe both lost-wakeup guards, the 3-state receiver, the three committed
   controls, and the single-source scope. It agrees line-for-line with the reconciled ledger row;
   no change was needed.

## Verification (the figures B14C records, re-run to confirm honest)

| Check | Result |
|---|---|
| `cargo verus verify -p ipc` | **62 verified, 0 errors** (confirms the raised Baselines figure) |
| `tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla` (positive) | **39 distinct states** (59 generated, depth 13); all invariants + `EventuallyDelivered` pass — **no error** |
| `… IpcReactor_NegControl.cfg` | **`NoLostWakeup` violated** (10 distinct states) — the poll-once control bites |
| `… IpcReactor_NegBackpressure.cfg` | **`NoLostWakeupWritable` violated** (21 distinct states) — the writable control bites |
| `… IpcReactor_NegLostWakeup.cfg` | **`NoLostWakeup` violated** (9 distinct states) — the wait-side control bites |
| `cargo build -p ipc -p urt` | clean (the docstring/module-doc edits compile) |
| `cargo test -p ipc --no-run` | clean (compiles `model.rs` under the test cfg, where the new module-doc note lives) |

Every figure written into the ledger was re-derived here, not copied — the Verus 62/0, the TLA 39
distinct states, and the three negative-control violations all match the recorded text exactly.

## Key findings

1. **The IpcReactor header was already honest — B14A did the model-doc work as it landed.** B14C's
   "confirm the header matches the ledger" step found nothing to change: the B14A rewrite already
   names both lost-wakeup guards, the committed controls, and the single-source scope. The
   reconciliation was therefore one-directional — fold B14A/B14B numbers *into* the ledger — rather
   than a two-way sync.
2. **Two existing in-code notes meant the §4.3 fixes had to be placed to complement, not
   duplicate.** `reactor.rs:283-290` (B14B) already says the *sequential dispatch* is atomics-free
   and proptest-routed. The §4.3 finding is about the *concurrent harnesses'* Loom-vs-Shuttle
   weight, so the canonical note went into `model.rs`'s module doc (where the harnesses live) and
   explicitly distinguishes the three tiers: Shuttle (concurrent, load-bearing) > Loom (concurrent,
   harmless-but-lighter here) > proptest (sequential dispatch).
3. **The drive-by tally fix was a genuine stale reference, not a B14 artifact.** The "13" in the
   urt-arena-seam note predates B13B's 13→14 bump; reaffirming "tally stays 14" in the same file
   made the contradiction glaring, so it was corrected (and recorded here as a drive-by).
4. **The two-tier posture is now explicit in the single source of truth.** A reviewer reading the
   ledger sees: the *protocol* (single-source wakeup + backpressure) is TLA-design-mechanized (39
   states, three runnable controls) **and** Loom/Shuttle-executed; the *multi-source dispatch
   arithmetic* is proptest-routed with only its pure bit-scan core Verus-verified; the
   cap-marshalling is proptest-routed. Nothing reads as mechanized that is not.

## Out of scope (recorded so it is not mistaken for a gap)

- **Any code / TLA / Verus change.** B14C is doc + ledger only. The gates it records (Verus 62/0,
  TLA 39 states, tally 14) were established by B14A/B14B and are merely re-confirmed here.
- **rev1 spec edits / a §6.1 `[verifying]` flip.** Part A is blessed; the IPC reactor is a
  Baselines-row component, not a §6.1 proof-boundary seam, so B14 flips no `[verifying]` line. The
  verified `lowest_clear_bit` core is a *new* verified construct with **no** trusted seam, so it
  does not enter the `[verifying]` transition table either.
- **Multi-source / multi-bit TLA modeling.** The model is single-source by design; the multi-source
  dispatch is proptest-routed (B14B) and now recorded as such in the ledger. Extending the TLA model
  to multiple bits is the model's own stated future step, not B14's. A routing, not a gap.
