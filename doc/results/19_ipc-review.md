# IPC crate conformance review (`doc/plans/2_ipc.md`)

An independent audit of the IPC crate (§3) against its plan, read on the merged
tree through PR #44 (phase 6). It covers what was built, what was deferred,
whether the deviations are justified, and what remains. This is a critique, not
a self-certification — the implementation is strong, so the review spends most of
its length on the gaps.

## Method

- Mapped the plan's six phases (§6) to the merged history: PRs #38–#43 (phases
  0–5) plus #44 (phase 6, `ipc-phase6-sessions`, HEAD `ed7a1d8`). One PR per
  phase, in order — the phasing was followed exactly.
- Read every IPC source file (`ipc/src/{transport,sync,model,reactor,endpoint,
  wire,session,header,fuzz_support,proofs,sys}.rs`), the TLA+ spec
  (`tla/ipc_reactor/IpcReactor.tla` + `.cfg`), the re-pointed consumer
  (`user/storaged/src/main.rs`), and the CI wiring (`.github/workflows/{ci,fuzz}.yml`).
- Re-ran the tiers on the current tree: `cargo test -p ipc` (16 pass),
  `RUSTFLAGS=--cfg shuttle … --lib` (16 pass), `RUSTFLAGS=--cfg loom … --lib`
  (12 pass). The §5.1 TLA gate is in the `model` job; the fuzz corpus replay is
  in `fuzz.yml`.
- Grepped for production callers of the new surface to separate "library exists"
  from "library is used."

## Verdict

**Phases 0–5 are implemented faithfully and verified well; phase 6 is real but
the thinnest of the six, and the plan's `Status: done` overstates it.** The
verifiable-first architecture (the `Transport`/`ModelTransport` seam, the cfg
sync seam, the TLA-gated reactor) is exactly what the plan asked for and is
executed cleanly. The reactor genuinely hides notification bits. All five
Shuttle harnesses exist, pass, and carry documented negative controls; the TLA+
spec adds the project's first liveness property.

The honest caveats are three, and they compound: (1) the reactor's *raison
d'être* — multiplexing many sources behind one wait — **is never exercised in a
shipping binary** (its sole production consumer, storaged, registers exactly one
source, and the obvious multi-source consumer, the shell, was left hand-rolled);
(2) "sessions/connect" (§4.6/§3.5) shipped as **library-only scaffolding with no
production caller** — the admission *policy* is done, the connect *mechanism*
(endpoint-cap passing, a second live session) is entirely deferred; (3) the
genuinely-new **multi-source dispatch** has the weakest verification of anything
in the crate — TLA and Loom both model a single source, leaving only one
std+shuttle *smoke*. None of this is hidden — the PR and plan note the
deferrals — but "done" should read "admission policy + reactor landed; connect
mechanism, bulk path, and production multiplexing deferred."

## Conformance at a glance

| Plan item | Status | Note |
|-----------|--------|------|
| §3.1 transport seam (`Transport`, `SyscallTransport`, `ModelTransport`) | ✅ done | clean; the `Env`/`Hal` analogue as designed |
| §3.2 sync seam (std/loom/shuttle) | ✅ done | reuses the proven Phase-1 wiring |
| §5.1 `IpcReactor.tla` (gate before reactor) | ✅ done | 3 safety + 1 liveness; sequencing respected (#38 < #40) |
| §4.1 non-blocking `send_nb`/`recv_nb` + null-slot tolerance | ✅ done | harness #3 |
| §4.2 reactor (`register`/`wait`, bind-poll-wait, bits hidden) | ✅ done | harness #1 (Shuttle+Loom); **single-source only in production** |
| §4.3 backpressure: blocking + bounded-retry send | ⚠️ partial | `send_blocking`/`send_retry` done; **no `async send().await`** |
| §4.4 valuable-cap ack | ✅ done | harness #4 (no-loss; no-dup is the kernel's, as scoped) |
| §4.5 postcard codec (module-private, reject trailing) + fuzz | ✅ done | total `decode`, fuzz target + corpus + Miri replay |
| §4.6 sessions/connect | ⚠️ partial | `Admission`/`admit_connect`/codecs done + harness #5; **connect mechanism, endpoint passing, bulk window: deferred, no production caller** |
| §5.2 harnesses #1–#5 | ✅ done | all pass; **no pinned seed** (plan required one) |
| §5.3 Loom fragment | ✅ done | single-source lost-wakeup only |
| §5.4 cargo-fuzz on decoders | ✅ done | `ipc/fuzz`; Header stays Kani-verified |
| §6 phase 6: re-point storaged | ✅ done | reactor-driven; behavior-preserving (boot/spawn tests green) |

## What was done well

- **The seam is the model boundary, exactly as §2/§8 argued.** `ModelTransport`
  (`model.rs`) is a faithful stand-in for `kcore::notification`: OR-accumulate,
  clear-on-receive, word-check-before-block (`model.rs:202`), and a send fires
  the persistent on-readable binding. The production crate pulls neither loom nor
  shuttle and uses no `ipc::sync` — the single-threaded-production invariant (§2)
  is real.
- **Bits are genuinely hidden.** `register(source, signals, key) → wait() →
  (Key, Signals)` (`reactor.rs`); `Key` is opaque; `storaged` dispatches on the
  key and names no bit (`main.rs:179-180`). The §3.6 "wait-set upgrade changes no
  server code" constraint is structurally honored, and the `RegisterErr::Full`
  edge (64-bit word) is handled.
- **The TLA gate was respected and is non-trivial.** `IpcReactor.tla` carries
  `NoLostWakeup`/`NoDrop`/`FifoPerChannel` plus `EventuallyDelivered` under weak
  fairness — the project's first liveness property — with the negative control
  (delete `RecvBlock`'s `word = 0` conjunct) documented in the header.
- **Negative controls are real.** Each harness comment names the mutation that
  breaks it (drop `register`'s poll-once → deadlock; drop `recv_nb`'s
  on-writable signal → hang; bare send-then-destroy → lost cap). They are
  manual bring-up checks, consistent with the kani/TLA discipline.
- **`Admission` is a clean, sound single admission point.** `admit` never grants
  past `budget` (the invariant holds by construction: `granted` only rises by
  `≤ remaining`), `release` uses `saturating_sub` against double-release, and the
  unit tests include a 100-iteration flood (`session.rs:233-239`).
- **The wire `wire`-feature fix** (serde/postcard made optional so alloc-free
  user binaries build — `ipc/Cargo.toml`) is good engineering caught and fixed
  mid-stream.

## Genuine gaps and weaknesses

1. **The reactor's multiplexing has zero production exercise.** Its only
   shipping consumer, `storaged`, registers a single source (`main.rs:174`). The
   *other* server loop — the shell's spawn/reap multiplexer, which already does a
   hand-rolled notification **bit-group scan** over child exit/fault
   (`user/shell/src/main.rs:537`, `sys::notif_wait(EVENT_NOTIF)`) — is precisely
   the `select()`-shaped consumer the reactor was built to absorb, and it was
   **not** re-pointed. So the headline capability runs only in harness #5; no
   binary actually multiplexes through `Reactor`. The plan's framing — "the
   single-session MVP never generated the multiplexing pressure that forces a
   reactor into existence" (§1) — is still literally true post-merge.

2. **"Sessions/connect" is library-only.** `Admission`, `admit_connect`,
   `ConnectReq`, `GrantReply` have **no production caller** (grep: used only by
   harness #5 and unit tests). The actual §3.5 connect — client retypes a
   channel pair, sends *one endpoint cap*, server accepts a *second* live
   session — is entirely deferred (it needs kernel cap-transfer wiring). What
   shipped is the admission arithmetic and a wire vocabulary for a handshake
   nothing performs. Defensible as a scope cut, but it makes phase 6 the
   thinnest deliverable, and the `ConnectErr::{Closed, BadReply, Other}` variants
   are dead (constructed nowhere) — API ahead of mechanism.

3. **The bulk window is notional.** `WindowGrant`/`Admission` account for window
   *bytes*, but no shared frame is granted, no `(window, offset, length)`
   descriptor or doorbell message exists, and nothing reads a window. The quota
   guards a resource that does not yet exist. (§9 scopes multi-window and the
   concurrent bulk path out — fair — but even the single MVP window has no
   mechanism, only an integer.)

4. **No pinned Shuttle seed**, though the plan requires it twice (§5.2 "pinned
   seed"; §7 "pinned seed"). Every harness is `shuttle::check_random(f, 1000)`
   with an entropy seed, so a CI failure is reproducible only from Shuttle's
   printed replay string — lost once logs rotate. There is also no
   `shuttle::replay` corpus, which is acceptable (the convention is "for any bug
   found," and none was) but should be wired so the first failure has somewhere
   to land.

5. **The new multi-source logic is the least-verified code in the crate.**
   `IpcReactor.tla` models one sender, one receiver, and `word ∈ {0,1}` — a
   *single* source/bit. Loom (harness #1) is likewise single-source. The
   multi-bit machinery — bit allocation, the `pending` drain, the `slots` table,
   and the `trailing_zeros()` lowest-bit-first scan (`reactor.rs:139`) — is
   covered only by harness #5, a 3-client std+shuttle **smoke**. "Fairness" is
   asserted as "all three eventually served," with **no starvation / bounded-wait
   property** anywhere; in fact `trailing_zeros` gives a structural lowest-index
   bias that nothing tests against. The plan was explicit that true liveness is
   §5.1's job and #5 is best-effort — but §5.1 never scaled to multi-source, so
   multi-client fairness is established by nothing stronger than a smoke.

6. **No `async send().await`** (§4.3 names it). Only `send_blocking`/`send_retry`
   exist. Justified — urt is single-threaded with no executor — but it is a
   deviation from the literal API, and `lib.rs`'s module doc still advertises
   "Async send/recv" as a responsibility (stale).

7. **Session codecs got no Kani harness**, though §5.4 says "any new *pure* codec
   helper gets a Kani harness in the same module." `ConnectReq::decode` /
   `GrantReply::decode` are exactly that, and `Admission::remaining`'s
   subtraction is sound only by an unproven invariant. They are unit-tested, not
   proven — a small but clear miss against the plan's own rule.

8. **Model fidelity nits.** `recv_nb` ignores `_dests` (`model.rs:154`) — caps
   are never actually placed into destination slots; presence is simulated by the
   mask, so harness #4 tests the mask plumbing, not a cap move (the real move is
   the kernel's, as scoped, but the model is shallower than it looks). All
   channels share one `cap` (no per-channel depth). `timer_arm` is
   `unimplemented!()` (`model.rs:214`), so the §3.6 timer-driven wait timeout has
   no model and no test despite being on the `Transport` trait.

## Are the deviations justified?

- **Yes, and well:** no async (no executor exists); the dynamic connect deferred
  (it is genuinely a kernel cap-transfer change, not userspace); the bulk
  concurrent path deferred (§9, with the §4.8 Loom note owed later); cap no-dup
  left to `CapRevocation` (correct division of labor).
- **Not fully:** (a) the **missing pinned seed** is a stated plan requirement
  dropped silently — it should be restored or the plan amended; (b) the plan's
  **`Status: done`** overstates §4.6 and the "first real consumer proves the API"
  claim — storaged exercises the API's *shape* but not its *multiplexing*, and
  connect is unbuilt. "Done" should be scoped to what shipped.

## What remains to be done

- **The real connect (§3.5/§4.6):** a client `connect()` that funds and passes
  an endpoint cap, and `storaged` accepting a second concurrent session — the
  kernel cap-transfer "Phase 3" the plan says is *unlocked* here but not done.
- **A second production reactor source:** re-point the shell's exit/fault
  multiplexer (`user/shell/src/main.rs:537`) onto `Reactor`, so multi-source
  dispatch — the whole point — runs in a shipping binary, not only in a harness.
- **The bulk window mechanism:** an actual server-granted frame + descriptor /
  doorbell, so `WindowGrant` grants something.
- **Pin the Shuttle seed** and add the `shuttle::replay` corpus hook (§5.2/§7).
- **Close the §5.4 gap:** Kani harnesses for `ConnectReq`/`GrantReply::decode`
  and an `Admission` no-over-grant proof.
- **Multi-source verification or an explicit caveat:** extend `IpcReactor.tla`
  (or the Loom fragment) to ≥2 sources, or document that multi-bit dispatch and
  fairness rest on harness #5 alone; address (or document) the `trailing_zeros`
  scheduling bias.
- **Housekeeping:** remove/wire the dead `ConnectErr` variants; fix the `lib.rs`
  "Async send/recv" doc line; decide whether `timer_arm` should be modeled or
  removed from the MVP trait.

## Recommendations (ranked)

1. **Amend the plan's status** from "done" to a scoped statement (admission
   policy + reactor + harnesses done; connect mechanism, bulk window, and
   production multiplexing deferred). Near-zero cost; restores the project's
   usual honesty (cf. `0_mvp.md`).
2. **Re-point the shell's spawn/reap loop onto `Reactor`.** This is the
   highest-value follow-up: it turns the reactor's multiplexing from
   harness-only into a real consumer and validates the §3.6 "hides bits" claim
   against the exact bit-group scan it was meant to replace.
3. **Pin the Shuttle seed** (plan requirement) and wire the replay corpus.
4. **Add the missing Kani harnesses** for the session codecs + `Admission`
   (§5.4), and either extend TLA/Loom to multi-source or record the caveat.
5. **Tidy the dead/stale surface** (ConnectErr variants, lib.rs doc, timer_arm)
   so the API matches what is actually reachable.

## Bottom line

The verifiable-first machinery is excellent and the per-phase discipline
(one PR, one harness, a negative control each) is exemplary. The reactor and the
codec are production-quality and properly verified. But the crate's two most
ambitious promises — *multiplexing many sources behind one wait* and *sessions/
connect* — are, on the merged tree, a harness and a library with no production
consumer respectively. That is a legitimate MVP stopping point, but it should be
named as such: the IPC crate is **built and verified; not yet load-bearing.**
The single most convincing next step is to make the shell's existing hand-rolled
multiplexer the reactor's second real consumer.
