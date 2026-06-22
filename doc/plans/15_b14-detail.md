# Plan — Part B14 detail: IPC reactor verification + TLA completion (lift the **bind/register + poll-once self-signal** and the **writable/backpressure half** into the `IpcReactor` TLA model so the model checks the full rev1§3.6 "bind, poll once, then wait" discipline and the rev1§3.3 backpressure its real code *already* runs and Loom/Shuttle-tests — closing T-3's model-lags-code gap with committed runnable negative controls; raise the reactor's **sequential multi-source dispatch** — `used`-mask bit allocation, `pending` drain, `trailing_zeros` lowest-bit scan — and the **endpoint cap-marshalling** above unit+Loom to a verification-grade **proptest** tier, the §4.2 [low] gap, with Verus over the bitmask allocator as a recorded stretch; pin S-12's `CHECK_DEADLOCK FALSE` ↔ `EventuallyDelivered` dependency in the cfg; and correct the §4.3 doc over-claims — the urt Loom-Relaxed docstring and the Loom-vs-Shuttle weight for the atomics-free IPC model — recording in the ledger which half is mechanized-in-TLA, which is proptest-routed, and why)

Detailed, separately-implementable decomposition of **Phase B14** from
`doc/plans/0_address_audit_rev0.md`. B14 is **Wave-4** work. It is **self-contained**: it
depends on nothing else in Part B and nothing depends on it. It is **verification- and
doc-only** — it changes no on-disk bytes, no wire op, no runtime behaviour, and no public
type `storaged`/`shell`/`init` depend on; it closes the gaps between what the IPC layer
*does* and what its *model, tests, and docs claim* it does.

The crisp framing that shapes the whole phase: the audit's IPC findings are almost entirely
**model-/test-/doc-lags-code**, not code bugs. The real reactor (`ipc/src/reactor.rs`)
*already* implements the full rev1§3.6 discipline — `register` binds and **self-signals the
bit** (the poll-once, `reactor.rs:152-154`) so a send-before-bind message is never slept
through, and `wait` drains then blocks only on a clear word; the real backpressure path
*already* exists — `send_nb` returns `Full` not a drop (`endpoint.rs:106-113`), `send_blocking`
loops waiting for `WRITABLE` (`endpoint.rs:148-162`), and `recv_nb` fires the `on_writable`
binding after draining a slot (`model.rs:163-187`). Both are *already* exercised by committed
**Loom and Shuttle** harnesses with verified-to-break negative controls (`model.rs`:
`reactor_no_lost_wakeup` :412-478, `full_backpressure_no_drop` :480-541). What lags is the
**`IpcReactor` TLA model**, which models only the wait-side half on an always-present binding
(T-3); the **sequential dispatch + marshalling**, which has no proptest (§4.2 [low]); and two
**doc over-claims** (§4.3). B14 makes the model, the test tier, and the docs honest about the
code that is already there.

**Closes (from the parent plan).** Verbatim from `doc/results/0_audit_rev0.md`:

- **T-3 [medium, confirmed]** (audit §2, `0_audit_rev0.md:230-241`): "The IPC-reactor TLA spec
  models only the wait-side half of the rev0§3.6 discipline it claims to cover." Its header
  (`IpcReactor.tla:1-39`) claims the full *"bind, poll once, then wait"* discipline and "the
  genuinely-new wakeup protocol," but "It has no bind/register/poll-once-self-signal action —
  `Send` (`:69-79`) treats the on-readable binding as always present — so the **send-before-bind
  hazard and its poll-once mitigation are modeled only in the Loom fragment**, not in TLA+.
  Separately, the title 'lost-wakeup + **backpressure** protocol' is titular: the only
  backpressure modeled is structural (`Send` disabled at `Len(queue) >= QueueDepth`); there is
  no `FULL` return, no writable-signal, and no symmetric writable lost-wakeup." (The wait-side
  lost-wakeup that *is* modeled "is modeled well, with a real negative control.")
- **IPC reactor/endpoint/transport carry no proof [low]** (audit §4.2, `0_audit_rev0.md:474-477`):
  "Verus covers only `header.rs` + `session.rs`. The reactor's sequential dispatch (bit
  allocation, the pending drain, the lowest-bit scan) and the endpoint cap-marshalling are
  Loom/Shuttle + unit-test only."
- **The Loom docstring over-claims Relaxed fidelity [medium]** (audit §4.3, `0_audit_rev0.md:528-532`):
  "The Loom test docstring (`urt/src/time.rs:585-631`) claims Loom enumerates *'every
  C11-permitted interleaving and reordering'*; Loom does **not** faithfully model Relaxed
  atomics. The seqlock's correctness rests on the explicit Acquire fence (which Loom *does*
  model), so the conclusion holds, but the docstring overstates what Loom proves."
- **Loom adds little over Shuttle for the IPC model** (audit §4.3, `0_audit_rev0.md:533-536`):
  "`ipc/` contains **no atomics** at all — the model synchronizes via `crate::sync`
  Mutex/Condvar. Loom's distinctive value (weak-memory atomic reorderings) is moot there; the
  choice is harmless but the 'Loom-certified' framing carries less weight than for the seqlock."
- **S-12** (audit §5, `0_audit_rev0.md:597-601`): "TLA `IpcReactor` disables deadlock detection
  (`CHECK_DEADLOCK FALSE`) — reasonable … but it means a genuine lost-wakeup *deadlock* is
  caught only by the `EventuallyDelivered` liveness property, not by deadlock detection. If that
  `PROPERTY` line were ever dropped, a true deadlock would pass silently. Worth a comment pinning
  the dependency."

The parent-plan B14 **work** line (`0_address_audit_rev0.md:616-631`) and **acceptance**
(`:633-635`) set the targets each sub-phase below conforms to.

---

## Spec target — Part A is blessed; B14 makes no spec edits

Every citation below is `rev1§` against the already-blessed text; B14 changes no spec and flips
no rev1§6.1 `[verifying]` line (the IPC reactor is **not** a §6.1 seam — §6.1(a)–(e) are the
kernel/storage seams; the IPC model lives in the ledger's *Baselines* row, not the proof
boundary). Like B13, B14 conforms model/code/docs to the existing text and records the
mechanized/test-routed split in the ledger (B14C). The load-bearing claims B14 conforms to:

- **rev1§3.3 — Send/receive semantics and backpressure** (`spec_rev1.md:153-161`). "`send` is
  non-blocking and returns `FULL` when the queue is full; messages are never dropped, since a
  dropped message could carry a capability and a lost cap is unacceptable." "Channels expose
  **readability** and **writability** notifications … and a **peer-closed** notification."
  "Delivery is **FIFO per channel**." "Blocking send, bounded-retry send, and async
  `send().await` are userspace library code over the non-blocking primitives plus
  notifications." This is the rev1§3.3 backpressure the *title* claims and B14A's
  Design-decision-1 work makes the model actually check (the `FULL`/writable-signal/symmetric
  lost-wakeup the audit names as absent).
- **rev1§3.6 — Event multiplexing: notifications** (`spec_rev1.md:184-192`). The notification
  object is "a single machine word of signal bits plus a waiter queue. Signalers OR bits in; a
  waiter receives the accumulated word, which then clears. **A signal whose accumulated word is
  still zero conveys nothing and does not wake a queued waiter; the reactor only ever signals a
  nonzero single-bit mask.**" "**The lost-wakeup discipline (bind, poll once, then wait) lives
  in the IPC crate.**" "Its reactor API is epoll-shaped — register a source with a set of
  signals and a key, dispatch in O(1) — implemented over bit groups underneath." This is the
  discipline T-3 says the model claims but does not check, and the dispatch §4.2 says is
  proof-less.
- **rev1§3.7 — Wire protocol and serialization** (`spec_rev1.md:194-200`). "Decoders treat all
  payloads as untrusted, reject trailing bytes, and are fuzz targets on the host (§6)." The
  endpoint cap-marshalling B14B proptests (the `Message.caps` ↔ kernel-ABI `[u32;4]`
  null-tolerant mapping) sits beside the already-verified header/session codecs and the
  `wire_decode` fuzz target — B14B raises the *marshalling glue* to the proptest tier, not the
  codec (already Verus + fuzz).
- **rev1§3.5 — Sessions and the IPC crate** (`spec_rev1.md:180`). "A single **userspace IPC
  crate** … owns the ergonomics: `FULL` handling, async send/receive, the valuable-cap
  acknowledgment protocol, and message serialization. … **This crate is the first Loom/Shuttle
  target (§6).**" The §4.3 Loom-vs-Shuttle note (B14C) clarifies *which* of the two does the
  load-bearing work for an atomics-free crate.
- **rev1§6 — verification tiering** (`spec_rev1.md:393-399`). Two rows bear on B14, and they are
  deliberately *different tiers*:
  - **Concurrency testing | Loom / Shuttle | "userspace servers and the IPC crate."** The
    reactor's *concurrent* wakeup/backpressure protocol is routed to Loom/Shuttle, and it is
    *already* there (`model.rs` harnesses). T-3 is not a missing Loom test — it is a missing
    **TLA+ model** of the same protocol the Loom harness covers (the protocol-model tier the spec
    routes "before implementation" — here, retrofitted to match the code that grew past the
    model).
  - **Baseline | Miri + proptest | "everything."** The reactor's *sequential* dispatch (the
    `used`-mask allocator, the `pending` drain, the lowest-bit scan) and the cap-marshalling are
    single-threaded state-machine logic — by the spec's own routing a **proptest + Miri**
    obligation, which B14B supplies (the §4.2 [low] gap).
  - **Proof-carrying code | Verus | "the IPC crate."** The header + session codecs are already
    there (58/0). The bit allocator is a `u64` bitmask + `trailing_zeros` — *structurally the
    same shape* as the kcore 32-level ready-queue bitmap B8C verified — so Verus over the pure
    allocator is a genuine, recorded **stretch** (Design decision 4), but the floor is proptest
    (it is [low], and the dispatch carries a slot array + Transport I/O that is not SMT-tractable
    cheaply).
- **rev1§6.1 discipline (the honesty rule B14C obeys)** (`spec_rev1.md:411`): "a property routed
  to trust is not mistaken for a mechanized one." B14C's ledger entry must say, in the same
  spirit as B6's GC-sufficiency note and B13's prolly-shape note, exactly which half is
  *mechanized-in-TLA* (the single-source wakeup/backpressure protocol) and which half is
  *proptest-/Loom-/Shuttle-routed* (the multi-source dispatch arithmetic and the live concurrent
  execution), and why the latter is not pulled into TLA (state-space) or Verus ([low] budget;
  Transport I/O).

---

## What is actually true today — the gap is *model/test/doc lags code*, not a code defect

The decomposition that shapes the whole phase. The audit confirms (twice — `0_audit_rev0.md:240-241`
and the negative-control roster `:670-674`) that the protocol is **correctly implemented and
correctly Loom/Shuttle-tested**; what is missing is the *TLA model* of the full protocol, a
*proptest* over the sequential dispatch, and two *doc* corrections. Concretely, the code that
already exists and works:

1. **Bind + poll-once is real and tested.** `Reactor::register` (`reactor.rs:132-156`) allocates
   a bit (`alloc_bit`, `:118-125`, `(!used).trailing_zeros()`), binds each requested signal via
   `Transport::bind`, stores the `Reg` (key + signals), and then **self-signals the bit**
   (`reactor.rs:154`) — the "poll once" that surfaces a message queued *before* the bind so it is
   "not slept through" (`reactor.rs:10-14, 152-154`). `register_bound` (`:174-190`) is the
   edge-triggered twin (thread on-exit/fault, timers, IRQs) that does **not** self-signal (no
   fabricated wakeup for an edge source). The committed harness `reactor_no_lost_wakeup`
   (`model.rs:420-478`, std/loom/shuttle) drives the send-before-bind interleaving; its
   documented negative control (`model.rs:412-419`) — *delete register's self-signal → the
   send-before-bind interleaving deadlocks* — is verified-to-break (`0_audit_rev0.md:672`).
2. **The wait-side word-check is real and tested.** `Reactor::wait` (`reactor.rs:202-216`) drains
   `pending` lowest-bit-first then blocks via `Transport::notif_wait`, which "consumes the
   accumulated word if non-zero, else blocks" (`transport.rs:88-91`; `model.rs:210-220`, the
   `while *word == 0 { … }` guard at `:214` — `kcore::notification` exactly). This is the *only*
   half the TLA model checks today (`RecvBlock`'s `word = 0` conjunct, `IpcReactor.tla:98-107`),
   and it is checked well, with the documented negative control in the model header (`:36-39`).
3. **Backpressure is real and tested.** `send_nb` returns `SendErr::Full`, never a drop
   (`endpoint.rs:106-113`, `transport.rs:69-70`, `model.rs:134-155`); `send_blocking`
   (`endpoint.rs:148-162`) and `send_retry` (`:168-188`) loop on `Full` waiting for a `WRITABLE`
   signal; `recv_nb` fires the `on_writable` binding *after* draining a slot and releasing the
   ring lock (`model.rs:163-187`, the signal at `:177`). The committed harness
   `full_backpressure_no_drop` (`model.rs:488-541`, std/shuttle, capacity 1) drives a blocked
   sender; its documented negative control (`model.rs:480-486`) — *remove recv_nb's on_writable
   signal → the harness hangs* — is verified-to-break (`0_audit_rev0.md:673`).
4. **The sequential dispatch is real but proptest-less.** The `used`/`pending` `u64` masks
   (`reactor.rs:91-102`), `alloc_bit` (`:118-125`), and the lowest-bit `trailing_zeros` drain in
   `wait` (`:202-216`) are exercised only by `fairness_smoke` (`model.rs:626-745`, a few clients)
   and the std-only `reactor_register_bound_dispatch` (`:757-780`, one high/low pair). There is
   **no proptest** over bit-allocation bijectivity, no-double-allocation, drain completeness, or
   lowest-bit ordering — the §4.2 [low] gap. The endpoint `cap_slots` ↔ `Message.caps`
   null-tolerant marshalling (`endpoint.rs:74-85, 122-134`) likewise has only example coverage.

So the honest split B14 delivers, forced by *where each property lives*:

- **Mechanizable-in-TLA (B14A):** the **single-source** wakeup + backpressure *protocol* — bind,
  poll-once, the wait-side and the symmetric writable-side lost-wakeup guards, FIFO, no-drop,
  eventual delivery. This is a protocol-model obligation (rev1§6 TLA row), and the model already
  exists; B14A *completes* it to match the code (T-3) and pins S-12.
- **Proptest-/Loom-/Shuttle-routed (B14B + the existing harnesses):** the **multi-source**
  dispatch arithmetic (`used`-mask allocation, `pending` drain, lowest-bit scan — the model's own
  "Scope limitation" note, `IpcReactor.tla:17-22`, deliberately keeps the TLA model single-bit)
  and the **live concurrent execution** (the Loom/Shuttle harnesses). B14B adds the missing
  **sequential proptest** over the dispatch invariants and the marshalling round-trip; the
  concurrent execution stays Loom/Shuttle.
- **Doc honesty (B14C):** the urt Loom-Relaxed over-claim and the Loom-vs-Shuttle weight, plus
  the ledger record of mechanized-vs-test-routed (rev1§6.1 discipline).

This split is the spine of Design decisions 1–4. The deliverable is **layered**: a headline
must-do (B14A model completion + S-12), a clean must-do (B14B dispatch proptest), and an honest
finishing item (B14C docs + ledger), with one recorded stretch (Verus over the bit allocator).

---

## Primary files (current line numbers)

- `tla/ipc_reactor/IpcReactor.tla` — the model B14A completes:
  - **Header** (`:1-39`): the claim "the lost-wakeup + **backpressure** protocol" (`:1-2`), the
    "bind, poll once, then wait" discipline claim (`:8-11`), the **scope limitation** to one
    source on one bit (`:17-22`, the deliberate single-source boundary B14 keeps), and the
    documented wait-side negative control (`:36-39`).
  - **Vars** (`:47-54`): `nextSend`, `queue`, `recvd`, `word`, `recv`. B14A adds `bound` (the
    binding-present flag) and, if Design decision 1 conforms, `wword` (the on-writable
    notification word) + a sender control state `send`.
  - `Init` (`:56-61`); `Send` (`:69-79`) — the action that "treats the on-readable binding as
    always present," the heart of T-3; `RecvGet` (`:82-87`), `RecvWaitConsume` (`:91-96`),
    `RecvBlock` (`:98-107`, the `word = 0` lost-wakeup guard at `:105`); `Next` (`:109-113`);
    `Spec` with `WF_vars` on the progress actions (`:118-123`).
  - Invariants: `TypeOK` (`:127-133`), `NoLostWakeup` (`:137-138`), `NoDrop` (`:142-143`),
    `FifoPerChannel` (`:147-149`); liveness `EventuallyDelivered` (`:156-157`).
- `tla/ipc_reactor/IpcReactor.cfg` — `SPECIFICATION Spec` (`:1`), `CHECK_DEADLOCK FALSE` (`:5`,
  the S-12 site), `CONSTANTS MaxMsgs = 3 / QueueDepth = 2` (`:7-9`), the four `INVARIANT` lines
  (`:11-14`), `PROPERTY EventuallyDelivered` (`:16`).
- `tla/ipc_reactor/IpcReactor_NegControl.cfg` (and siblings) — **new** committed negative-control
  cfg(s), the CommitProtocol/CapRevocation convention (`CommitProtocol_NegControl.cfg`,
  `CapRevocation_NegControl.cfg` safety, `CapRevocation_NegLiveness.cfg` liveness). TLC admits
  exactly **one `SPECIFICATION` per cfg**, so each broken spec needs its own cfg (Design
  decision 3).
- `ipc/src/reactor.rs` — the dispatch B14B proptests: `Reactor` struct `used`/`pending`/slots
  (`:91-102`), `WORD_BITS = 64` (`:82`), `alloc_bit` (`:118-125`), `register` + self-signal
  (`:132-156`, poll-once at `:154`), `register_bound` (`:174-190`), `bind` (`:192-196`), `wait`
  drain+block (`:202-216`); the lost-wakeup doc (`:1-28`).
- `ipc/src/endpoint.rs` — the marshalling B14B proptests: `Message`/`caps` (`:23-38`,
  null-tolerance `:31-32`), `cap_slots` (`:74-85`), `send_nb`/`recv_nb` (`:106-134`),
  `send_blocking`/`send_retry` (`:148-188`), `send_acked`/`recv_acked` (`:190-209`).
- `ipc/src/transport.rs` — the `Transport` trait seam: `send_nb` `Full`-not-drop (`:69-70`),
  `notif_wait` word-check (`:88-91`), `bind`.
- `ipc/src/model.rs` — the host model + committed harnesses (the proof-of-record for the
  *concurrent* protocol B14A mirrors into TLA): `Notification` word+cv (`:40-43`), `notif_signal`
  (`:201-208`), `notif_wait` guard (`:210-220`, `:214`), `send_nb` (`:134-155`), `recv_nb` +
  on_writable (`:163-187`, `:177`); harnesses `rig_smoke` (`:272-318`), `fifo_no_drop`
  (`:328-409`), `reactor_no_lost_wakeup` (`:412-478`, neg-control comment `:412-419`),
  `full_backpressure_no_drop` (`:480-541`, neg-control comment `:480-486`),
  `valuable_cap_ack_no_loss` (`:552-607`), `fairness_smoke` (`:626-745`),
  `reactor_register_bound_dispatch` (`:757-780`); Shuttle seed/iters (`:236-246`). B14B adds the
  sequential dispatch + marshalling proptests here (or a sibling `#[cfg(test)]` module).
- `ipc/src/sync.rs` — the cfg-selected sync seam (std / `--cfg loom` / `--cfg shuttle`,
  `:1-27`); B14C's Loom-vs-Shuttle note lands in this module's doc or `model.rs`'s.
- `ipc/src/header.rs` (`verus!{}` `:32-185`) and `ipc/src/session.rs` (`verus!{}` `:31-442`,
  `Admission` `:245-329`) — the *already-verified* surface (the `cargo verus verify -p ipc` 58/0
  baseline); B14 does **not** touch these (B14B's stretch, if taken, adds a *new* allocator proof,
  it does not change the codecs).
- `ipc/fuzz/fuzz_targets/wire_decode.rs` + `ipc/src/fuzz_support.rs` + `ipc/tests/fuzz_corpus.rs`
  — the wire-codec fuzz tier (unchanged; B14 is wire-stable — the marshalling proptest is
  additive, no corpus regen).
- `urt/src/time.rs` — the §4.3 doc target: the Loom docstring (`:617-627`) claiming "every
  C11-permitted interleaving *and reordering*" (`:617-622`), the `loom_tests` module (`:628-`),
  the non-certifying-Shuttle label (`:28-30`) that B14C cites as the correct-posture precedent.
- `doc/guidelines/verus_trusted-base.md` — the ledger. B14 adds **no seam** (tally stays **14**)
  and the `cargo verus verify -p ipc` gate stays **58/0** (unless the Verus stretch lands, then
  it rises and the Baselines row records it). B14C **updates the Baselines TLA row** (`:175`,
  the "`IpcReactor` (with a negative control)" entry → the completed actions/properties +
  committed negative-control cfg(s) + recorded state count) and records the dispatch/marshalling
  proptest as a *test-routed* property (the GC-sufficiency-note style, `:64-72`).
- `doc/spec/spec_rev1.md` — **no change** (Part A blessed; B14 has no §6.1 `[verifying]` line to
  flip — the IPC reactor is a Baselines-row component, not a proof-boundary seam).
- `CLAUDE.md` — no change (the `cargo test -p ipc` and Miri sweeps already cover the crate; B14B's
  proptests ride them).

---

## Verification tier & baseline (applies to all sub-phases)

B14 spans three surfaces with different routing (rev1§6): **TLA+ protocol model** (the
`IpcReactor` design gate), the **`cas`/`ipc` baseline proptest tier**, and **doc honesty**. Six
notes up front so nothing is silently dropped or over-claimed:

- **B14 is verification/doc-only — no runtime, wire, or on-disk change.** Unlike B5 (format bump)
  and like B7/B13, B14 changes **zero persistent bytes, zero wire ops, and zero observable
  runtime behaviour**. The reactor/endpoint code is *already* correct (the audit confirms); B14A
  edits a `.tla` model + `.cfg`, B14B *adds* tests, B14C edits doc comments + the ledger. The
  `wire_decode` fuzz corpus and `tests/fuzz_corpus.rs` need no regeneration. No public type
  changes, so the aarch64 cross-build links `storaged`/`shell`/`init` (which pull `ipc`)
  unchanged.
- **The TLA gate is held, then *completed*.** `IpcReactor` passes today (ledger Baselines `:175`,
  "with a negative control"; constants `MaxMsgs = 3`, `QueueDepth = 2`). B14A **adds vars,
  actions, and properties** — the state graph grows (a `bound` flag, optionally a `wword`
  notification + sender state), so B14A **re-runs TLC and records the new exact state count** (the
  ledger entry currently records *no* IpcReactor state figure — B14A supplies it for the first
  time, the CommitProtocol/CapRevocation convention). The four existing invariants + the existing
  liveness property must still pass; the *new* properties pass on the real spec and **fail** under
  the committed negative controls.
- **The Verus gate is held; a stretch may *raise* it.** `cargo verus verify -p ipc` is **58/0**
  today (ledger Baselines `:171`, header + session codecs). B14A/B14C touch **no Verus** → 58/0
  held. **B14B's floor is proptest** (no Verus) → 58/0 held; **if** the recorded Verus stretch
  over the bit allocator lands (Design decision 4), the count goes **above 58** and B14B records
  the new total. No existing proof is weakened; the gate is a floor.
- **No new trusted seam; tally stays 14.** B14 adds no `external_body`/`assume_specification`.
  The reactor dispatch stays plain Rust (proptest-routed), not a verified construct, so it adds
  nothing to the seam tally (`verus_trusted-base.md:131-135`, **14**). If the Verus stretch
  lands, the allocator *algorithm* becomes verified (gate rises) with **no** new trusted seam (a
  pure-bitmask proof, like the kcore ready-queue bitmap — no interpreted primitive). Either way
  the tally is unchanged.
- **The concurrent protocol's guard of record stays Loom/Shuttle; TLA is the *design* check.**
  This must be stated plainly so the ledger is honest (rev1§6.1): the *live, concurrent*
  wakeup/backpressure execution is guarded by the committed `model.rs` Loom/Shuttle harnesses
  (with their verified-to-break negative controls); the **TLA model is the protocol-design
  oracle** — it checks the *abstract* discipline exhaustively over the small state space, the way
  `CommitProtocol`/`CapRevocation` do for storage/caps. B14A makes the TLA model *check what the
  code's harnesses already enforce*; it does not replace them. B14C records this two-tier posture
  (TLA design model + Loom/Shuttle execution + proptest dispatch) so no reviewer reads the TLA
  completion as the sole guard.
- **No Loom/Shuttle rewrite; B14B is proptest (sequential).** The dispatch B14B covers is
  single-threaded state-machine logic; its natural tier is **proptest + Miri** (rev1§6 baseline),
  not a new concurrency harness. `ipc/` has **no atomics** (audit §4.3, `0_audit_rev0.md:533`),
  so Loom adds little over the existing Shuttle harnesses (B14C's note) — B14B adds no Loom/Shuttle
  target, only proptests over the deterministic dispatch + marshalling. The case-count convention
  is `cases: if cfg!(miri) { 4 } else { N }` (the workspace idiom, e.g. `urt/src/time.rs:561-565`).

**Baseline to re-establish at end of B14:**
- `tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla` passes with the completed model
  (the four existing invariants + the existing `EventuallyDelivered` + the new wakeup/backpressure
  properties); **record the new state count** in the ledger Baselines row. The committed
  negative-control cfg(s) each report the **expected** property violation (a short counterexample
  trace), recorded as the runnable negative control(s).
- `cargo verus verify -p ipc` green at **≥ 58/0** (> 58 only if the Verus stretch lands; record
  the new total then).
- `cargo test -p ipc` green: the existing unit tests + std harnesses, plus B14B's new dispatch +
  marshalling proptests.
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p ipc` clean across the new
  proptests (4 cases under Miri) and the committed `wire_decode` corpus replay
  (`tests/fuzz_corpus.rs`) stays clean (wire-stable — no corpus change).
- The Loom and Shuttle harnesses still build and pass under their cfgs
  (`RUSTFLAGS="--cfg loom" cargo test -p ipc`, `RUSTFLAGS="--cfg shuttle" cargo test -p ipc`) —
  B14 adds to the model, it does not perturb the harnesses.
- The aarch64 cross-build links `storaged`/`shell`/`init` unchanged and **QEMU boot stays green**
  (the live witness that the IPC layer the model now fully describes still serves the real stack).

---

## Design decision 1 — the "backpressure" claim: **model the symmetric writable/backpressure half** (conform — the title becomes true and matches the real code's Loom/Shuttle harness), with an honest retitle as the recorded fallback *(resolve in B14A)*

T-3's two halves are (i) the missing bind/poll-once (Design decision 2) and (ii) the *titular*
"backpressure" — the model's header (`IpcReactor.tla:1-2`) calls it "the lost-wakeup +
**backpressure** protocol," but the only backpressure modeled is structural (`Send` disabled at
`Len(queue) >= QueueDepth`, `:70`); there is no `FULL` return, no writable-signal, and **no
symmetric writable lost-wakeup**. The parent plan offers the choice: "Either model real
backpressure … **or** retitle the spec honestly to 'wait-side lost-wakeup' and drop the
'backpressure' claim."

- **Adopted — conform: model the symmetric writable half, making the title honest.** The real
  code *already runs and Loom/Shuttle-tests* the backpressure path (`send_nb`→`Full`,
  `send_blocking` waits on `WRITABLE`, `recv_nb` fires `on_writable`; harness
  `full_backpressure_no_drop`, `model.rs:480-541`, with a verified-to-break negative control).
  The honest move — the project's conform-first ethos (parent plan Guiding principles; B7's
  "make the model check what the title claims") — is to add the missing half to the model so
  "backpressure" is *checked*, not retitled away. Concretely (mirror of the existing wait-side
  machinery):
  - **New var `wword`** (the on-writable notification word, `{0,1}`) and a **sender control
    state** `send \in {"run", "blocked"}` (the sender can now block when full, the symmetric twin
    of `recv`).
  - **`Send` refusal becomes explicit backpressure:** a `send = "run"` sender with a full queue
    does not silently stall — it takes a `SendBlock` step (queue full **and** `wword = 0` → block),
    the symmetric twin of `RecvBlock`. The `wword = 0` conjunct is the **symmetric lost-wakeup
    guard** (the writable analogue of `RecvBlock`'s `word = 0`).
  - **`RecvGet` (drain) fires the on-writable binding:** consuming the head frees a slot, so it
    wakes a `"blocked"` sender (`send' = "run"`, `wword' = 0`) or accumulates (`wword' = 1`) for a
    running one — term-for-term the mirror of `Send`'s readable signal, faithful to
    `recv_nb`'s `on_writable` fire (`model.rs:177`).
  - **`SendWaitConsume`:** a `"run"` sender with a full queue but `wword = 1` consumes the word
    and re-polls (does **not** block) — the writable mirror of `RecvWaitConsume`.
  - **New safety invariant `NoLostWakeupWritable`:** `(send = "blocked") => (Len(queue) =
    QueueDepth /\ wword = 0)` — a blocked sender has no missed writable signal and the queue is
    genuinely full; a blocked sender with free space (`Len(queue) < QueueDepth`) would be exactly
    a lost writable wakeup, the symmetric defect.
  - **Fairness:** add `WF_vars(RecvGet)` already covers drain; add `WF_vars(SendWaitConsume)` so
    accumulated writable signals are consumed; the existing `EventuallyDelivered` must still hold
    (now under genuine two-sided blocking) — confirm TLC liveness passes with the sender able to
    block and be woken.
  - **Header retitle is then *unnecessary but the comment is corrected*:** the header keeps
    "lost-wakeup + backpressure protocol" (now true) and the prose is updated to describe both
    the readable and the writable lost-wakeup guards and their negative controls.
  Decisive reasons: the title becomes *earned* rather than dropped; the model gains a real
  symmetric property with its own negative control (the strongest anti-theater signal,
  `0_audit_rev0.md:670`); and the TLA model finally matches the `full_backpressure_no_drop` Loom
  harness — the two then check the *same* protocol at the design and execution tiers.
- **Recorded fallback — honest retitle (the parent plan's "or").** If the symmetric model blows up
  the state space beyond a tractable TLC run at the chosen constants, or the two-sided liveness
  tableau proves disproportionate for a [medium] item, **retitle**: rename the model to
  "wait-side lost-wakeup protocol," **drop** the "backpressure" word from the header (`:1-2`), and
  add a one-line note that backpressure (the `FULL` return + writable wakeup) is guarded at the
  execution tier by `full_backpressure_no_drop` (Loom/Shuttle) and is a possible future TLA
  extension — the same honest-scoping the model already does for multi-source dispatch
  (`:17-22`). Record *which* path was taken. (Recommended target: conform; the symmetric half is
  a near-mechanical mirror of the existing wait-side actions and the state space at `MaxMsgs = 3`/
  `QueueDepth = 2` is small.)
- **Rejected — leave the title as-is with only structural backpressure.** That is precisely the
  over-claim T-3 names: a header that promises a "backpressure protocol" while the model checks no
  writable signal and no symmetric lost-wakeup. Honesty requires either checking it or not
  claiming it.

**Recommendation: conform — add the symmetric `wword`/`send`/`SendBlock`/`SendWaitConsume`
machinery and the `NoLostWakeupWritable` invariant so "backpressure" is checked, with its own
committed negative control (Design decision 3); fall back to the honest retitle only if the
state space proves intractable, recording which path was taken and the resulting state count.**

---

## Design decision 2 — modeling the **send-before-bind hazard + poll-once mitigation** (the T-3 headline): a `bound` flag + a `Register` action whose poll-once self-signal is the guard *(resolve in B14A)*

T-3's core: the model's `Send` (`IpcReactor.tla:69-79`) "treats the on-readable binding as always
present," so the send-before-bind interleaving — a message enqueued *before* the receiver
registers its binding, whose edge signal therefore goes nowhere — and its poll-once mitigation are
**modeled only in the Loom fragment** (`reactor_no_lost_wakeup`, `model.rs:420-478`), not in TLA.
B14A lifts them into the model.

- **Adopted — a `bound \in {FALSE, TRUE}` flag, a `Register` action that self-signals, and a
  `Send` whose readable signal fires *only when bound*.** Faithful to `reactor.rs:132-156`:
  - **`Init`: `bound = FALSE`** — the receiver has not yet registered its on-readable binding.
  - **`Send` splits on `bound`:** when `bound`, the current behaviour (wake a blocked receiver and
    clear the word, or accumulate `word = 1`); when `~bound`, the message still enqueues (no drop,
    rev1§3.3) but the **edge signal goes nowhere** (`word' = word` UNCHANGED) — exactly the
    send-before-bind hazard (the edge signal of a not-yet-bound source is lost).
  - **`Register` (new action), precondition `~bound`:** sets `bound' = TRUE` and performs the
    **poll-once self-signal** — `IF Len(queue) > 0 THEN word' = 1 ELSE word' = word` — surfacing
    any message queued before the bind so the first `wait` does not sleep through it
    (`reactor.rs:152-154`: "Poll once: surface this source on the first wait, so a message already
    queued before the bind is not slept through").
  - **`RecvBlock` gains `bound` as a precondition:** the receiver cannot block before it has
    registered (it has nothing to block *on*) — `recv` only reaches `"blocked"` via `wait`, which
    follows `register`.
  - The existing `NoLostWakeup` invariant (`:137-138`) and `EventuallyDelivered` (`:156-157`)
    **become the teeth**: without the poll-once, a message sent before bind leaves the receiver
    blocked with `Len(queue) > 0` (NoLostWakeup false) or never delivered (EventuallyDelivered
    false) — which is exactly the negative control (Design decision 3).
  Decisive reasons: this is the minimal, faithful encoding of the real `register`'s three steps
  (bind, store, self-signal); the hazard and its mitigation become a *checked* TLA property rather
  than a Loom-only one (closing T-3's primary clause); and it reuses the existing invariants as
  the oracle (no tautological new property — the poll-once *earns* `NoLostWakeup` against the
  send-before-bind interleaving). The `register_bound` edge-triggered twin (`reactor.rs:174-190`,
  no self-signal) is **out of scope for the model** — it is for externally-bound sources (timers,
  IRQs, thread death) and carries no poll-once by design; the std-only
  `reactor_register_bound_dispatch` test (`model.rs:757-780`) is its guard, recorded as
  test-routed in B14C.
- **Rejected — keep the binding "always present" and add only a property.** Any property over a
  model where the binding is always present cannot express the send-before-bind hazard (there is
  no pre-bind state to reach). The hazard requires the `bound` state var; without it the model
  literally cannot represent the interleaving the audit names.
- **Rejected — model the full multi-source registration (the `used`-mask, multiple bits).** The
  model's own scope note (`:17-22`) deliberately keeps **one source on one bit**; multi-source
  dispatch is routed to B14B's proptest (Design decision 4). Adding multi-bit registration to TLA
  would explode the state space for no gain over the single-source hazard (which is where the
  poll-once correctness lives). Keep the single-source scope; B14A adds `bound` for *that one
  source*.

**Recommendation: add the `bound` flag, the `Register` action with the poll-once self-signal, and
the `~bound` edge-loss branch in `Send`; let the existing `NoLostWakeup`/`EventuallyDelivered`
catch a missing poll-once (the committed negative control, Design decision 3). Keep the
single-source scope; route `register_bound` and multi-source dispatch to B14B.**

---

## Design decision 3 — **committed, runnable negative controls** (the converged project convention), one cfg per broken spec, upgrading the existing documented wait-side control too *(resolve in B14A)*

The existing `IpcReactor` negative control is **documented only** (prose in the header `:36-39`:
delete `RecvBlock`'s `word = 0` conjunct → `NoLostWakeup` reachable-false). The project has since
converged on **committed, CI-runnable** negative-control cfgs — `CommitProtocol_NegControl.cfg`
(B7), `CapRevocation_NegControl.cfg` (safety) + `CapRevocation_NegLiveness.cfg` (liveness, B9) —
and the audit calls TLA negative controls "**the strongest anti-theater signal**"
(`0_audit_rev0.md:670`). B14A now adds *new* load-bearing guards (poll-once, the symmetric
writable signal) that each deserve a runnable control.

- **Adopted — commit a runnable negative-control cfg for each load-bearing guard the completed
  model claims, one `SPECIFICATION` per cfg (the TLC one-spec-per-cfg rule, stated in
  `CapRevocation_NegLiveness.cfg:12-14`).** In `IpcReactor.tla`, add the broken-spec definitions
  (each a `SpecBadX == Init /\ [][NextBadX]_vars` with the matching fairness), the
  `CapRevocation` "Negative controls (committed; the B7 pattern)" idiom:
  1. **Send-before-bind poll-once control** (`IpcReactor_NegControl.cfg`, the T-3 headline):
     `RegisterNoPoll` = `Register` minus the poll-once self-signal (`bound' = TRUE`, `word'`
     UNCHANGED). Under `SpecBadPoll`, `NoLostWakeup` (or `EventuallyDelivered`) **must** be
     violated — a message sent before bind, the receiver registers without surfacing it, then
     blocks with `Len(queue) > 0`. The runnable proof the poll-once is load-bearing; the trace
     mirrors the real-code harness's documented control (`model.rs:412-419`).
  2. **Writable lost-wakeup control** (`IpcReactor_NegBackpressure.cfg`, if Design decision 1
     conforms): `RecvGetNoWritable` = `RecvGet` minus the on-writable fire (drain a slot but never
     signal `wword`). Under `SpecBadWritable`, `NoLostWakeupWritable` (or `EventuallyDelivered`)
     **must** be violated — a blocked sender is never woken though space appeared. Mirrors the
     real-code harness's documented control (`model.rs:480-486`, "removing recv_nb's on_writable
     signal makes this hang").
  3. **Wait-side control, upgraded to runnable** (`IpcReactor_NegLostWakeup.cfg`): `RecvBlockNoGuard`
     = `RecvBlock` minus the `word = 0` conjunct (block without checking the accumulated word).
     Under `SpecBadWait`, `NoLostWakeup` **must** be violated — a blocked receiver holding
     `word = 1`. This makes the *existing* documented control (`:36-39`) a committed, runnable
     artifact, matching its siblings.
  Each cfg carries the same `CONSTANTS`, `CHECK_DEADLOCK FALSE`, and the single `PROPERTY`/
  `INVARIANT` it expects to bite. Decisive reasons: a committed, CI-runnable control is the
  strongest honesty posture (B7 Design decision 1 made exactly this call for the headline storage
  gap, contrasting it with "the IpcReactor header style" of documented-only); a reviewer or a
  future refactor can *run* each cfg and watch the guard bite; and it brings `IpcReactor` to
  parity with the other two TLA models, all of which now ship committed controls.
- **Proportionality latitude (recorded).** If three cfgs prove heavier than a [medium]/[low] item
  warrants, the *minimum* is the **send-before-bind poll-once control committed runnable** (the
  T-3 headline, item 1) plus the others kept documented — but the recommendation is all three
  (they are tiny cfg files, and the writable control only exists if Design decision 1 conforms).
  State which were committed.
- **Rejected — keep all controls documented-only.** That is the lighter status quo the audit and
  B7's precedent moved past; for a phase whose *entire point* is making the model honest about
  its guards, a runnable control is worth the small cfg.

**Recommendation: commit a runnable negative-control cfg per load-bearing guard
(`IpcReactor_NegControl.cfg` send-before-bind, `IpcReactor_NegBackpressure.cfg` writable if
DD1 conforms, `IpcReactor_NegLostWakeup.cfg` the upgraded wait-side), one `SPECIFICATION` each;
record which were committed and the violation each reports.**

---

## Design decision 4 — the B14B verification bar for the **sequential dispatch + marshalling**: a verification-grade **proptest** floor (the parent plan's "at least proptest/Shuttle"), Verus over the bitmask allocator as a recorded stretch *(resolve in B14B)*

The §4.2 finding ([low]): "The reactor's sequential dispatch (bit allocation, the pending drain,
the lowest-bit scan) and the endpoint cap-marshalling are Loom/Shuttle + unit-test only." The
parent plan's work line: "raise the reactor's sequential dispatch … and endpoint cap-marshalling
above unit+Loom — at least a proptest/Shuttle model of the dispatch invariants."

- **Adopted — a verification-grade proptest floor over the deterministic dispatch + marshalling;
  no new concurrency harness.** The dispatch is **single-threaded state-machine logic** (the
  reactor mutates `used`/`pending` under the holder's own thread; concurrency lives in the
  Transport/notification seam, already Loom/Shuttle-tested). Its natural tier is **proptest +
  Miri** (rev1§6 baseline), and `ipc/` has no atomics (audit §4.3) so Loom/Shuttle add little
  over the existing harnesses. B14B adds proptests over:
  - **Bit-allocation invariants** (`alloc_bit`, `reactor.rs:118-125`): over arbitrary
    register/drop sequences, every allocated bit is distinct (no double-allocation — bijection
    between live sources and set `used` bits), `alloc_bit` returns the **lowest** clear bit, and
    allocation refuses cleanly at `WORD_BITS = 64` (the rev1§3.6 structural limit) rather than
    aliasing or panicking.
  - **Pending-drain completeness + lowest-bit ordering** (`wait`, `reactor.rs:202-216`): over
    arbitrary `pending` masks, the drain yields exactly the set bits, each once, in
    `trailing_zeros` (lowest-first) order, and maps each to the registered `(key, signals)` —
    matching the spec's "dispatch in O(1)" epoll shape (rev1§3.6) and the model's documented
    lowest-bit bias (`IpcReactor.tla:19-22`).
  - **Cap-marshalling round-trip** (`cap_slots` ↔ `Message.caps`, `endpoint.rs:74-85, 122-134`):
    over arbitrary `[Option<u32>; 4]` cap arrays, `cap_slots` maps `Some(slot)`/`None` to the
    kernel ABI `[u32;4]`/`SLOT_NONE` and back faithfully, the all-empty case returns `None` (skip
    cap handling), and the receiver's null-tolerance (rev1§3.4, `endpoint.rs:31-32`) holds — a
    `None` where a cap was expected is accepted, never a panic.
  - **Determinism/totality**: the dispatch and marshalling are pure functions of their inputs
    (the canonical-form-style symmetry B13 used) — no order dependence, total over arbitrary
    inputs. All Miri-replayed at `cfg!(miri) { 4 }` cases.
- **Recorded stretch — Verus over the `used`-mask allocator (B13-style latitude).** The bit
  allocator is a `u64` bitmask + `trailing_zeros` — *structurally the same shape* as the kcore
  32-level ready-queue bitmap B8C verified in Verus (ledger `:26-31`). Lifting `alloc_bit`'s
  pure bitmask core into a `verus!{}` proof (lowest-clear-bit correctness, no-double-allocation,
  the `used`-bitmap-coherence invariant) is a genuine, in-scope-for-rev1§6 stretch that would
  **raise** `cargo verus verify -p ipc` above 58/0 with **no new trusted seam** (a pure bitmask,
  no interpreted primitive). **Attempt it only if the pure core extracts cheaply** away from the
  slot array + Transport I/O (which stay plain Rust, the B7/B6 trusted-shell posture); fall back
  to the proptest floor and record which bar was met (the B11/B13 "state which bar is met"
  discipline). Recommended posture: ship the proptest floor unconditionally; take the Verus
  stretch if the allocator core lifts without dragging the `HashMap`/slot-array registration in.
- **Rejected — a new Loom/Shuttle model of the dispatch.** The dispatch is sequential; a
  concurrency harness adds nothing the proptest doesn't, and `ipc/` has no atomics for Loom to
  explore (audit §4.3). The *concurrent* protocol is already Loom/Shuttle-tested
  (`model.rs` harnesses) and TLA-modeled (B14A). Rejected as redundant.
- **Rejected — full Verus over the reactor (registration + dispatch + Transport).** The slot
  array and `Transport` calls are I/O over a trait object — not SMT-tractable within the [low]
  budget, and rev1§6.1's trusted-shell-over-verified-cores posture (B6/B7) keeps such I/O plain.
  Only the *pure bitmask core* is a Verus candidate (the stretch).

**Recommendation: proptest floor over bit-allocation, pending-drain, lowest-bit scan, and
cap-marshalling round-trip (must-do, Miri-replayed); Verus over the pure `used`-mask allocator a
recorded stretch (raises the gate, no new seam) taken only if it extracts cheaply. Record which
bar was met.**

---

## Sub-phase B14A — complete the `IpcReactor` TLA model: bind/register + poll-once, the symmetric writable/backpressure half, committed negative controls, and the S-12 cfg comment *(must-do; the headline; closes T-3 + S-12; TLA-only)*

The headline deliverable. Pure TLA+/cfg — touches no Rust, no Verus, no on-disk/wire bytes. After
B14A the model **checks** the full rev1§3.6 "bind, poll once, then wait" discipline (send-before-
bind hazard + poll-once mitigation, Design decision 2) and the rev1§3.3 backpressure (symmetric
writable lost-wakeup, Design decision 1), each with a committed runnable negative control (Design
decision 3), and the S-12 dependency is pinned in the cfg.

- **Touches:**
  - `tla/ipc_reactor/IpcReactor.tla` — add `bound` (and, if DD1 conforms, `wword` + `send`) to
    `vars` (`:54`) and `Init` (`:56-61`); split `Send` (`:69-79`) on `bound`; add `Register`
    (poll-once, DD2); add `RecvGet`'s on-writable fire + `SendBlock`/`SendWaitConsume` (DD1); add
    `NoLostWakeupWritable` (DD1) to the invariants region after `NoLostWakeup` (`:138`); extend
    `TypeOK` (`:127-133`) for the new vars; add the broken-spec blocks `RegisterNoPoll`/
    `RecvGetNoWritable`/`RecvBlockNoGuard` + `NextBadX`/`SpecBadX` (DD3); update the header
    (`:1-39`) to describe both lost-wakeup guards and to keep "backpressure" *true* (or retitle
    per the DD1 fallback). Leave the existing `RecvWaitConsume`/`RecvBlock`/`NoDrop`/
    `FifoPerChannel`/`EventuallyDelivered` semantics intact (the completion is additive).
  - `tla/ipc_reactor/IpcReactor.cfg` — keep `SPECIFICATION Spec`; add the `NoLostWakeupWritable`
    `INVARIANT`; add an **S-12 comment** above `CHECK_DEADLOCK FALSE` (`:5`) and `PROPERTY
    EventuallyDelivered` (`:16`) pinning the dependency: *deadlock detection is off because an
    all-delivered terminal state is legitimate, so a genuine lost-wakeup deadlock is caught
    **only** by `EventuallyDelivered` — do not drop this PROPERTY line or a true deadlock passes
    silently* (`0_audit_rev0.md:597-601`).
  - `tla/ipc_reactor/IpcReactor_NegControl.cfg`, `…_NegBackpressure.cfg` (if DD1 conforms),
    `…_NegLostWakeup.cfg` — **new** committed negative-control cfgs (DD3), one `SPECIFICATION`
    each, each expecting its property to **fail**.
- **Depends on:** Part A blessed (rev1§3.3/§3.6 text). No intra-B14 dependency — parallel with
  B14B (TLA vs Rust) and B14C (doc).
- **Work:**
  1. Add the `bound` flag + `Register` action with the poll-once self-signal and the `~bound`
     edge-loss branch in `Send` (DD2). Re-run TLC over `IpcReactor.cfg`; the existing invariants +
     `EventuallyDelivered` pass on the real spec.
  2. (DD1 conform) Add `wword` + `send` + `SendBlock`/`SendWaitConsume` + `RecvGet`'s on-writable
     fire + `NoLostWakeupWritable` + the fairness on the new progress actions. Re-run TLC; confirm
     the two-sided liveness still holds. (Or take the DD1 retitle fallback and record it.)
  3. Add the broken-spec definitions and the committed negative-control cfgs (DD3). Run each via
     `tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla <cfg>`; confirm each reports its
     expected property **violated** with a short counterexample. Record each counterexample.
  4. Add the S-12 cfg comment pinning `CHECK_DEADLOCK FALSE` ↔ `EventuallyDelivered`.
  5. **Record the new state count** (the model grows with the new vars/actions) and the negative
     controls in the ledger Baselines TLA row (this is B14C's job, but B14A produces the numbers).
- **Acceptance:**
  - `IpcReactor.cfg`: all four invariants (`TypeOK`/`NoLostWakeup`/`NoDrop`/`FifoPerChannel`),
    the new `NoLostWakeupWritable` (if DD1 conforms), **and** `EventuallyDelivered` pass; state
    count recorded.
  - Each committed negative-control cfg reports its property **violated** with a short trace (the
    runnable proof each guard has teeth) — at minimum the send-before-bind poll-once control.
  - The model header is honest: it describes both the readable and writable lost-wakeup guards and
    their negative controls, and "backpressure" is either *checked* (DD1 conform) or *dropped*
    (DD1 fallback) — not titular.
  - The S-12 dependency is pinned in the cfg as a comment.
  - The send-before-bind hazard and its poll-once mitigation are now modeled **in TLA+**, not only
    in the Loom fragment (T-3's primary clause closed).
- **Effort/Risk:** M / medium. The substance is getting the symmetric backpressure machinery and
  the two-sided liveness right (DD1) and confirming each negative control fails *for the right
  reason*; the `bound`/poll-once half (DD2) is a near-mechanical mirror of the existing wait-side
  actions. Medium because the model completion is the headline T-3 fix and the state space must
  stay tractable (the DD1 retitle fallback de-risks it).

---

## Sub-phase B14B — raise the reactor's sequential dispatch + endpoint cap-marshalling to a verification-grade proptest tier *(must-do; closes the §4.2 [low] gap; Rust testing)*

Add the missing **proptest** coverage over the deterministic multi-source dispatch — the
`used`-mask bit allocation, the `pending` drain, the lowest-bit `trailing_zeros` scan — and the
endpoint cap-marshalling round-trip, the §4.2 [low] gap (today Loom/Shuttle + unit only). This is
the proptest tier rev1§6's baseline row routes the *sequential* logic to; the *concurrent* protocol
stays in the Loom/Shuttle harnesses (B14A's TLA model is its design oracle).

- **Touches:** `ipc/src/model.rs` (or a sibling `#[cfg(test)]` module in `reactor.rs`/
  `endpoint.rs`) — add proptests over `alloc_bit`/`register`/`wait`'s `used`/`pending` masks
  (`reactor.rs:91-216`) and `cap_slots`/`Message.caps` (`endpoint.rs:74-134`). Widen the existing
  std-only `reactor_register_bound_dispatch` (`model.rs:757-780`) and `fairness_smoke`
  (`:626-745`) coverage into property-based form. **No change** to the Loom/Shuttle harnesses, the
  Verus codecs, or any public type.
- **Depends on:** none in B14 (independent of B14A's TLA work and B14C's docs; can land in
  parallel). Part A blessed.
- **Work (DD4 floor):**
  - **Bit-allocation proptest:** over arbitrary register/drop sequences, assert no two live sources
    share a bit (allocation is a bijection onto distinct `used` bits), `alloc_bit` returns the
    lowest clear bit, and the 64th allocation refuses cleanly (no alias, no panic — the rev1§3.6
    structural limit). Assert the `used` mask stays coherent with the live source set (the
    bitmap-coherence invariant).
  - **Pending-drain proptest:** over arbitrary `pending` masks, assert `wait`'s drain yields
    exactly the set bits, each once, in lowest-first order, each mapped to its registered
    `(key, signals)` (the epoll-shaped O(1) dispatch, rev1§3.6).
  - **Cap-marshalling round-trip proptest:** over arbitrary `[Option<u32>; 4]`, assert `cap_slots`
    ↔ ABI `[u32;4]`/`SLOT_NONE` round-trips, the all-empty case returns `None`, and receiver
    null-tolerance holds (a `None` where a cap was expected never panics — rev1§3.4).
  - **Determinism/totality:** dispatch + marshalling are pure functions of inputs, total over
    arbitrary inputs; Miri-replayed at `cfg!(miri) { 4 }` cases (the workspace idiom).
  - **(DD4 stretch, optional)** lift `alloc_bit`'s pure `used`-mask core into a `verus!{}` proof
    (lowest-clear-bit + no-double-alloc + bitmap coherence), the kcore-ready-queue-bitmap pattern,
    **only if** it extracts cleanly from the slot array + Transport I/O. If taken, the gate rises
    above 58/0 with no new seam; record the new total. Else the floor is the proptest tier.
- **Acceptance:**
  - `cargo test -p ipc` green including the new bit-allocation, pending-drain, lowest-bit-scan, and
    cap-marshalling round-trip proptests; the existing unit tests + std harnesses unchanged.
  - `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p ipc` clean over the new
    proptests; `tests/fuzz_corpus.rs` replay unchanged (wire-stable).
  - The Loom/Shuttle harnesses still build + pass under their cfgs (unperturbed).
  - `cargo verus verify -p ipc` ≥ 58/0 (> 58 and recorded **only** if the Verus stretch landed;
    state which bar was met — proptest floor vs Verus allocator).
- **Effort/Risk:** S–M / low. [low] in the parent plan; the dispatch is small deterministic
  bitmask logic and the proptest strategies are straightforward. The only risk is *cost* (keep the
  native case count and source counts modest so the suite stays CI-friendly) and the optional
  Verus stretch's extraction cleanliness (de-risked by the proptest floor shipping regardless).

---

## Sub-phase B14C — doc honesty + ledger reconciliation: the §4.3 Loom over-claims and the mechanized-vs-test-routed record *(finishing; closes the §4.3 doc items; obeys the rev1§6.1 "no trust-routed property mistaken for mechanized" rule)*

Correct the two §4.3 doc over-claims and record, in the one source of truth, exactly which half of
the IPC verification story is *mechanized-in-TLA*, which is *proptest-/Loom-/Shuttle-routed*, and
why — the B6 GC-sufficiency-note / B13 prolly-shape-note discipline applied to the IPC reactor.

- **Touches:**
  - `urt/src/time.rs` — the Loom docstring (`:617-627`). Correct the over-claim "every
    C11-permitted interleaving *and reordering*" (`:617-622`): Loom does **not** faithfully model
    Relaxed atomics; the seqlock's correctness rests on the **explicit Acquire fence** (which Loom
    *does* model and *does* enumerate interleavings around), so the conclusion holds via the fence,
    not via faithful Relaxed reordering. State exactly that (audit §4.3, `0_audit_rev0.md:528-532`).
    The non-certifying-Shuttle label (`:28-30`) is the correct-posture precedent to cite — this is
    a precision fix to an otherwise-correct tool choice, not a tool-defusing.
  - `ipc/src/model.rs` (or `sync.rs:1-27`) module doc — add the **Loom-vs-Shuttle note**: `ipc/`
    has **no atomics** (it synchronizes via `crate::sync` Mutex/Condvar), so Loom's distinctive
    value (weak-memory atomic reorderings) is moot here; **Shuttle** is the load-bearing
    concurrency tool for this crate, and the "Loom-certified" framing carries less weight than for
    the urt seqlock (audit §4.3, `0_audit_rev0.md:533-536`). Harmless choice, honestly scoped.
  - `doc/guidelines/verus_trusted-base.md` — update the **Baselines TLA row** (`:175`): replace
    "`IpcReactor` (with a negative control)" with the completed description — the
    bind/register + poll-once and (if DD1 conformed) the symmetric writable/backpressure actions,
    the new `NoLostWakeupWritable` property, the committed negative-control cfg(s) and the
    violation each reports, and the **recorded state count** (from B14A) — mirroring the detail the
    `CommitProtocol`/`CapRevocation` entries carry. Add a **test-routed note** (the
    GC-sufficiency-note style, ledger `:64-72`): the reactor's **multi-source dispatch** (the
    `used`-mask allocation, `pending` drain, lowest-bit scan) and the **live concurrent execution**
    are delivered at the rev1§6 baseline/concurrency tiers (B14B's proptests + the existing
    Loom/Shuttle harnesses), **not** TLA-mechanized — the TLA model is single-source by design
    (`IpcReactor.tla:17-22`) and the dispatch arithmetic is proptest-routed; record this so a
    reviewer does not read the TLA completion as covering multi-source dispatch (the rev1§6.1 "no
    trust-routed property mistaken for mechanized" discipline). If the DD4 Verus stretch landed,
    record the new `cargo verus verify -p ipc` total in the Baselines row and that the allocator
    *algorithm* is now verified (no new seam).
  - `tla/ipc_reactor/IpcReactor.tla` header — confirm it now matches the ledger (both lost-wakeup
    guards, the committed controls, the single-source scope) — the model-doc/ledger reconciliation.
- **Depends on:** B14A (the final actions/properties, committed-control set, and state count) and
  B14B (the dispatch test-tier description, and the Verus-stretch outcome if taken). The finishing
  item — lands after A and B.
- **Acceptance:**
  - The urt Loom docstring no longer claims faithful Relaxed reordering; it names the Acquire fence
    as the load-bearing modeled construct.
  - The IPC model doc carries the Loom-vs-Shuttle note (Shuttle is the load-bearing tool for the
    atomics-free crate).
  - The ledger Baselines TLA row describes the completed `IpcReactor` (actions, `NoLostWakeupWritable`,
    committed control cfgs, recorded state count) and the test-routed multi-source-dispatch note;
    the tally stays **14** (no new seam) and the `cargo verus verify -p ipc` line reflects 58/0
    (or the raised total if the stretch landed).
  - No claim anywhere reads the multi-source dispatch as TLA-mechanized, or the IPC Loom harness as
    carrying the seqlock's weight; the ledger and the model header agree line-for-line.
- **Effort/Risk:** S / low. Documentation + ledger bookkeeping; the load-bearing care is *honesty*
  (not over-claiming), which the GC-sufficiency note (ledger `:64-72`) and the B13 prolly-shape
  note are the templates for.

---

## Execution order

```
B14A  complete the IpcReactor TLA model (bind/poll-once + symmetric backpressure + committed neg-controls + S-12 comment)   [must-do; headline; TLA-only; T-3 + S-12]
        (models the send-before-bind hazard + poll-once and the writable lost-wakeup the real code already runs; state count recorded)
   │
B14B  sequential dispatch + cap-marshalling proptest tier   [must-do; independent; parallel; §4.2 low]
        (bit-allocation bijection, pending-drain completeness, lowest-bit scan, marshalling round-trip; all Miri-replayed; Verus over the bitmask allocator a recorded stretch)
   │
B14C  doc honesty + ledger reconciliation   [finishing; after A (numbers) and B (test-tier + stretch outcome)]
        (urt Loom-Relaxed docstring fix; Loom-vs-Shuttle note; Baselines TLA row + test-routed multi-source-dispatch note; tally stays 14)
```

- **B14A is the headline** (T-3 [medium] + S-12) and is independently shippable: it completes the
  TLA model to check the full rev1§3.6/§3.3 protocol the code already runs, with committed runnable
  negative controls — a complete, mergeable unit on the model surface alone, parallel with the
  Rust work (the B7A posture).
- **B14B is fully independent** of B14A (Rust tests vs TLA) and independently shippable: it raises
  the sequential dispatch + marshalling to the proptest tier, the §4.2 [low] gap, with the Verus
  allocator a recorded stretch. It must not be skipped just because B14A lands — the dispatch
  arithmetic is its sole proptest guard.
- **B14C last**, once B14A's numbers (actions, committed controls, state count) and B14B's
  test-tier description (and Verus-stretch outcome) are final; its whole job is honesty — the §4.3
  doc corrections and the mechanized-vs-test-routed split recorded so the rev1§6.1 discipline holds.
- Each is a complete, mergeable unit — keep them separable so the headline model fix (B14A) can
  land without waiting on the Rust or doc work, mirroring B7A/B7B/B7C and B13A/B13B/B13C/B13D.

## Out of scope for B14 (recorded so it is not mistaken for a gap)

- **Multi-source / multi-bit TLA modeling.** The `IpcReactor` model is single-source by design
  (`IpcReactor.tla:17-22`: one source on one notification bit) and B14A keeps that scope — adding
  `bound`/poll-once and the writable half for *that one source*. The multi-source dispatch (the
  `used`-mask allocation, `pending` drain, lowest-bit scan) is **proptest-routed** (B14B) and
  recorded as test-routed in the ledger (B14C); extending the TLA model to multiple bits is the
  model's own stated "possible future step" (`:22`), not B14's. This is a *routing*, not a gap.
- **Verus over the full reactor (registration + dispatch + Transport I/O).** The slot array and
  `Transport` trait-object calls are I/O outside the SMT-tractable core, kept plain Rust by the
  rev1§6.1 trusted-shell-over-verified-cores posture (B6/B7). Only the **pure `used`-mask
  allocator** is a Verus candidate, and that is a *recorded stretch* (DD4), not a must-do; if
  deferred, the dispatch is proptest-routed (the floor), recorded in the ledger.
- **Cap move/teardown safety in the IPC model.** Already `CapRevocation.tla`'s
  (`MoveSemantics`/`FireSafe`, the queue-slot CDT visibility); the `IpcReactor` scope note
  (`:13-15`) explicitly owns *only* the wakeup + backpressure protocol. B14 adds no cap-lifecycle
  modeling to `IpcReactor` — the valuable-cap ack protocol stays the `valuable_cap_ack_no_loss`
  Shuttle harness's (`model.rs:552-607`).
- **The kernel notification object.** `kcore::notification` (signal/wait, the waiter queue) is
  already verified in `kcore` (ledger `:24-28`); the `IpcReactor` model and `ipc/src/model.rs`
  *mirror* it faithfully but do not re-verify it. B14 touches no kernel code.
- **On-disk/wire format change / corpus regeneration.** None. B14 changes zero persistent bytes
  and zero wire ops; the header/session/wire codecs and the `wire_decode` fuzz corpus
  (`tests/fuzz_corpus.rs`) are byte-identical and replay unchanged. The cap-marshalling proptest
  (B14B) *tightens* what the `Message.caps` ↔ ABI mapping is checked against; it changes no bytes.
- **A new Loom/Shuttle harness for the dispatch.** The dispatch is sequential and `ipc/` is
  atomics-free (audit §4.3); the concurrent protocol is already Loom/Shuttle-tested (`model.rs`
  harnesses) and TLA-modeled (B14A). B14B's tier is proptest, not a new concurrency harness.
- **rev1 spec edits / a §6.1 `[verifying]` flip.** Part A is blessed; the IPC reactor is **not** a
  §6.1 proof-boundary seam (it lives in the ledger Baselines row), so B14 flips no `[verifying]`
  line and edits no spec text — it conforms the model/tests/docs to the existing rev1§3.3/§3.6/§3.7/§6
  text and records the mechanized/test-routed split in the ledger (the B13 posture).
