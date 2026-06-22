# B14A findings — completed IpcReactor TLA+ model (bind/poll-once + symmetric backpressure)

**Phase:** B14A of `doc/plans/15_b14-detail.md` (IPC reactor verification + TLA completion),
the headline, TLA-only sub-phase. Closes audit **T-3** (`doc/results/0_audit_rev0.md:230-241` —
the `IpcReactor` model checked only the wait-side half on an always-present binding) and pins
**S-12** (`:597-601` — the `CHECK_DEADLOCK FALSE` ↔ `EventuallyDelivered` dependency). Resolves
the plan's Design decisions 1 (conform — model the writable/backpressure half), 2 (the
bind/register + poll-once `bound` flag), and 3 (committed, runnable negative controls).

B14A completes `tla/ipc_reactor/IpcReactor.tla` so the model **checks** the full rev1§3.6
"bind, poll once, then wait" discipline and the rev1§3.3 backpressure the real `ipc/` code
already runs and already Loom/Shuttle-tests (`reactor.rs`, `endpoint.rs`, `model.rs`). It is
**verification/doc-only**: it edits a `.tla` model + `.cfg` and adds three negative-control
`.cfg`s — **no Rust, no Verus, no on-disk/wire bytes, no runtime behaviour**. The Verus gate
(`cargo verus verify -p ipc` = 58/0) and the trusted-seam tally (**14**) are untouched; the
`IpcReactor` TLA gate is held and **completed** (positive model: **39 distinct states**, all
five invariants + `EventuallyDelivered` pass).

## Design decisions exercised

- **DD2 (the T-3 headline — bind/register + poll-once).** New `bound \in BOOLEAN` flag
  (`Init: bound = FALSE`); a `Register` action that binds and performs the **poll-once
  self-signal** (`word' = IF Len(queue) > 0 THEN 1 ELSE word`, faithful to `reactor.rs:152-154`);
  and a `Send` that fires the on-readable binding **only when bound** (when `~bound` the message
  still enqueues — no drop — but the edge signal is lost, `word' = word` UNCHANGED). The
  send-before-bind hazard and its poll-once mitigation are now modeled **in TLA+**, not only in
  the `reactor_no_lost_wakeup` Loom/Shuttle fragment.
- **DD1 (conform — the symmetric writable/backpressure half).** Adopted the conform path (not
  the retitle fallback). New `wword \in {0,1}` (on-writable notification) and `send \in
  {"run","blocked"}` (sender control state); `SendBlock`/`SendWaitConsume` as the writable
  mirrors of `RecvBlock`/`RecvWake`; `RecvGet` fires the on-writable binding on each drain
  (faithful to `recv_nb`'s `on_writable` signal, `model.rs:177`); and the new safety invariant
  `NoLostWakeupWritable`. "Backpressure" in the title is now **checked**, not titular. The
  two-sided liveness held under weak fairness, so the retitle fallback was **not** needed.
- **DD3 (committed, runnable negative controls).** Three new `.cfg`s, one `SPECIFICATION` per
  cfg (the TLC one-spec-per-cfg rule, the `CommitProtocol`/`CapRevocation` convention), each
  reporting its property **violated** with a short trace. This upgrades the previously
  *documented-only* wait-side control to a runnable artifact and brings `IpcReactor` to parity
  with the other two TLA models.

## Key design realization — the 3-state receiver (a deviation from the plan's action sketch)

The plan's DD2 sketch (keep `RecvGet` firing freely on `Len(queue) > 0`, add `bound`, and have
`RecvBlock` merely gain a `bound` precondition) **cannot make the poll-once load-bearing**, and
this had to change. If the receiver may drain a queued message merely because the queue is
non-empty — and weak fairness on `RecvGet` then *forces* it — a missing poll-once never strands
delivery: the message is drained regardless of whether the wakeup was lost. The negative control
would report no violation (it would be theatre).

The faithful fix mirrors the real reactor loop `register -> loop { wait(); while recv_nb() {..} }`:
the receiver blocks on the **notification word, not the queue**, and can only drain *after*
`wait()` returns. This needs a three-valued `recv \in {"poll","blocked","drain"}`:
`"poll"` = inside `wait()`; `"blocked"` = `wait()` slept; `"drain"` = `wait()` returned, running
the `recv_nb` loop. `RecvBlock` blocks iff `word = 0` (**regardless of queue length** — `wait()`
only checks the word), and `RecvGet` is gated on `recv = "drain"`. This makes the poll-once
genuinely load-bearing — and it subsumes the old `RecvWaitConsume` (an empty-queue spurious
wakeup is now `RecvWake -> RecvDone`). This is a faithful refinement of the model the plan's
prose itself demanded (its stated teeth, "blocked with `Len(queue) > 0`", are unreachable in a
free-`RecvGet` model). Recorded here as the headline B14A finding.

## What landed (all in `tla/ipc_reactor/`)

1. **`IpcReactor.tla` — completed model.** Vars `bound`/`wword`/`send` added and `recv` widened
   to three states; `Init` extended. Actions: `Register` (poll-once), `Send` (split on `bound`),
   `RecvWake`, `RecvBlock` (the `word = 0` wait-side guard), `RecvGet` (drain + on-writable fire),
   `RecvDone`, `SendBlock` (the `wword = 0` writable guard), `SendWaitConsume`. `Spec` adds
   `WF_vars` on the six progress actions (not the two blocking actions). `TypeOK` extended;
   new invariant **`NoLostWakeupWritable == (send = "blocked") => (Len(queue) = QueueDepth /\
   wword = 0)`**; `NoLostWakeup`/`NoDrop`/`FifoPerChannel`/`EventuallyDelivered` kept. The header
   (`:1-90`) was rewritten to describe both lost-wakeup guards, the 3-state discipline, the
   committed controls, and to keep the single-source scope note.
2. **Three broken-spec blocks** in the same file (the `SpecBadX == Init /\ [][NextBadX]_vars`
   pattern, no fairness — all three are safety violations): `RegisterNoPoll` (Register minus the
   self-signal) → `SpecBadPoll`; `RecvGetNoWritable` (RecvGet minus the on-writable fire) →
   `SpecBadWritable`; `RecvBlockNoGuard` (RecvBlock minus the `word = 0` conjunct) → `SpecBadWait`.
3. **`IpcReactor.cfg`** — new `INVARIANT NoLostWakeupWritable` and the **S-12 comment** above
   `CHECK_DEADLOCK FALSE` / `PROPERTY EventuallyDelivered` pinning the dependency (deadlock
   detection is off because all-delivered is a legitimate terminal state, so a genuine
   lost-wakeup deadlock is caught **only** by `EventuallyDelivered` — do not drop that line).
4. **`IpcReactor_NegControl.cfg`** (`SpecBadPoll`, the T-3 headline), **`…_NegBackpressure.cfg`**
   (`SpecBadWritable`), **`…_NegLostWakeup.cfg`** (`SpecBadWait`) — each with a comment header
   stating what is broken, which property MUST be violated, why, and that the real `Spec` passes.

## Verification

| Check | Result |
|---|---|
| `tools/tla/tla-check.sh tla/ipc_reactor/IpcReactor.tla` (SANY) | clean |
| `tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla` (positive, `IpcReactor.cfg`) | **39 distinct states** (59 generated, depth 13); `TypeOK`/`NoLostWakeup`/`NoLostWakeupWritable`/`NoDrop`/`FifoPerChannel` **and** `EventuallyDelivered` all pass — **no error** |
| `… IpcReactor_NegControl.cfg` (poll-once) | **`NoLostWakeup` VIOLATED**, depth 4 — `Send`(before bind) → `RegisterNoPoll` → `RecvBlock`: `recv="blocked"`, `queue=<<1>>`, `word=0` |
| `… IpcReactor_NegBackpressure.cfg` (writable) | **`NoLostWakeupWritable` VIOLATED**, depth 7 — `Register` → `Send` → `Send` → `RecvWake` → `SendBlock` → `RecvGetNoWritable`: `send="blocked"`, `queue=<<2>>` (Len 1 < QueueDepth 2), `wword=0` |
| `… IpcReactor_NegLostWakeup.cfg` (wait-side) | **`NoLostWakeup` VIOLATED**, depth 4 — `Register` → `Send` → `RecvBlockNoGuard`: `recv="blocked"`, `word=1`, `queue=<<1>>` |

No Rust/Verus/build/QEMU runs: B14A touches no Rust, so the `cargo verus verify -p ipc` 58/0
gate, `cargo test -p ipc`, the Loom/Shuttle harnesses, the aarch64 cross-build, and the QEMU
smoke are all unaffected by this change (constants unchanged: `MaxMsgs = 3`, `QueueDepth = 2`).

## Key findings

1. **The poll-once is only load-bearing if the drain is gated behind the wakeup.** The central
   modeling insight (see above): a model where `RecvGet` fires freely on `Len(queue) > 0` cannot
   express the send-before-bind defect, because the queued message is always delivered. The
   3-state receiver (block on the *word*, drain only after `wait()` returns) is what gives both
   the readable poll-once control and the existing wait-side control real teeth.
2. **`RecvBlock`'s guard is on the word, not the queue — and that is the faithful change.** The
   original model blocked only when `Len(queue) = 0 /\ word = 0`; that coupling silently relied
   on "a queued message under a bound sender always set the word," which breaks exactly at the
   send-before-bind hazard. `wait()` blocks on the notification, so the model must too. The
   `NoLostWakeup` invariant — unchanged in form — still holds on the positive model precisely
   *because* the protocol re-establishes that coupling (the poll-once + the bound-Send fire).
3. **The writable half is a term-for-term mirror, and the two-sided liveness held under WF.**
   `wword`/`send`/`SendBlock`/`SendWaitConsume` mirror `word`/`recv`/`RecvBlock`/`RecvWake`;
   `RecvGet` firing on-writable mirrors `Send` firing on-readable. `EventuallyDelivered` passed
   with weak fairness on the six progress actions (a blocked sender is woken by `RecvGet`'s
   writable fire; a blocked receiver by `Send`/`Register`), so DD1's retitle fallback was not
   needed. The state space stayed tiny (39 states) at the existing constants.
4. **`SendWaitConsume` is required for positive liveness, not just symmetry.** Without it, a
   running sender that hits a full queue while `wword = 1` (a previously-accumulated writable
   signal) can neither `Send` (full), `SendBlock` (guarded by `wword = 0`), nor consume the
   stale signal — a real deadlock that fails `EventuallyDelivered`. It is the writable mirror of
   the old `RecvWaitConsume`'s role.
5. **All three controls bite as safety violations (no fairness needed).** Each reaches a
   concrete reachable bad state at shallow depth (≤ 7), so the cfgs need no `PROPERTY`/fairness
   — just the `INVARIANT` and `CHECK_DEADLOCK FALSE`, matching `CapRevocation_NegControl.cfg`'s
   safety posture. The traces match the real-code harnesses' documented negative controls
   (delete `register`'s self-signal → send-before-bind deadlock; remove `recv_nb`'s `on_writable`
   → blocked sender hangs).

## Numbers for the B14C ledger update (recorded here; B14A does not edit the ledger)

When B14C reconciles `doc/guidelines/verus_trusted-base.md`, the Baselines TLA row entry
"`IpcReactor` (with a negative control)" should record:
- the completed actions (bind/register + poll-once; the symmetric writable/backpressure half;
  the 3-state receiver) and the new `NoLostWakeupWritable` property;
- the **positive state count: 39 distinct states** (59 generated, depth 13), all five invariants
  + `EventuallyDelivered` passing;
- the **three committed negative-control cfgs** and the violation each reports
  (`IpcReactor_NegControl.cfg` → `NoLostWakeup`; `…_NegBackpressure.cfg` →
  `NoLostWakeupWritable`; `…_NegLostWakeup.cfg` → `NoLostWakeup`);
- the tally stays **14** (no new seam) and `cargo verus verify -p ipc` stays **58/0** (B14A
  touches no Verus).

The §4.3 doc over-claims (urt Loom-Relaxed docstring; the Loom-vs-Shuttle weight for the
atomics-free IPC crate) and the test-routed multi-source-dispatch note are **B14C's** scope.

## Out of scope (recorded so it is not mistaken for a gap)

- **Multi-source / multi-bit dispatch.** The model is single-source by design (one on-readable
  bit, one on-writable bit; `IpcReactor.tla` scope note). The `used`-mask allocation, the
  `pending` drain, and the `trailing_zeros` lowest-bit scan are **proptest-routed** (B14B), not
  modeled here. This is a routing, not a gap.
- **The `register_bound` edge-triggered twin** (`reactor.rs:174-190`, no poll-once). Out of
  scope for the model — it is for externally-bound sources (timers, IRQs, thread death) and
  carries no poll-once by design; its guard of record is the std-only
  `reactor_register_bound_dispatch` test, recorded as test-routed in B14C.
- **The live concurrent execution.** The TLA model is the protocol-**design** oracle over the
  small state space; the live, concurrent wakeup/backpressure execution stays guarded by the
  committed `model.rs` Loom/Shuttle harnesses with their verified-to-break negative controls.
  B14A makes the model *check what those harnesses already enforce*; it does not replace them.
- **Cap move/teardown safety.** Already `CapRevocation.tla`'s (`MoveSemantics`/`FireSafe`); the
  valuable-cap ack protocol stays the `valuable_cap_ack_no_loss` Shuttle harness's. B14A adds no
  cap-lifecycle modeling to `IpcReactor`.
