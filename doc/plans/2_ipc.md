# Plan: implementing the IPC crate (§3) — Shuttle-verifiable from day one

**Status:** **proposed.**
**Spec baseline:** `doc/spec/2_spec_rev2.md` §3 (§3.1 message format … §3.7 wire
protocol), with §2.3/§2.5 (caps, fund-by-failure-mode) and §4.8 (bulk-window
concurrency) as context.
**Verification baseline:** `doc/plans/1_loom-shuttle-rewrite.md` §4.2 — the
"Shuttle-ready IPC crate" structural contract **this plan fulfils** — and its
Phase-1 seqlock seam (the *proven* cfg(loom)/cfg(shuttle) template). Framing
mirrors `doc/plans/0_kani-rewrite.md`: a verifiable-first seam (the `Env`/`Hal`
analogue), and TLA+ checking the *design* while Shuttle re-checks the *real code*.
**Status quo:** `ipc/` is the fixed header (`header.rs`, done + Kani-verified) and
raw syscall wrappers (`sys.rs`); `send`/`recv` in `lib.rs` are `todo!()`. The
reactor, backpressure, valuable-cap ack, and serialization do not exist
(`doc/results/0_mvp.md` debt #2).

---

## 1. Background and goal

§3.5 calls the userspace IPC crate "the first serious Loom/Shuttle target (§6)."
It was never built: the single-session MVP never generated the multiplexing
pressure that forces a reactor into existence, so every server is a hand-rolled
drain-then-wait poll loop (`user/storaged/src/main.rs`: a `chan_recv` loop plus a
bare `notif_wait`). The consequence recorded in `0_mvp.md` is that **the
Loom/Shuttle tier had no target and was never exercised** — until Phase 1 of the
loom-shuttle plan pointed it at the time-page seqlock as a warm-up.

**Goal.** Implement §3 — non-blocking send/recv, the epoll-shaped reactor, async
+ bounded-retry send over backpressure, the valuable-cap ack protocol, postcard
serialization, and the session/connect path — **written verifiable-first so the
Shuttle tier (`1_loom-shuttle-rewrite.md` §4.2) drives it from the first commit,
not after a rewrite.** The §3.6 promise that the future wait-set kernel object
"changes no server code" becomes a hard architectural constraint: the reactor API
hides notification bits from day one.

This is the concurrency counterpart to the storage stack's formal work: the same
"design in TLA+, re-check the real code mechanically" loop, with **Shuttle** as
the mechanized tier (where Kani sits for the sequential kernel).

---

## 2. The concurrency actually under test (the framing that makes this work)

A precise model of *where the races are* is the single most important design
decision, and the easy mistake is to look for threads inside the crate.

- **Each Eunomia process is single-threaded** (`urt`: "Single-threaded processes;
  no concurrent access by construction"). The IPC crate's reactor, inside one
  process, has **no internal thread interleaving**.
- The genuine concurrency is **cross-process**: a *sender* process and a
  *receiver* process (or several clients and a server) race through **shared
  kernel objects** — the channel's queue ring and the notification word. The
  headline bug class, the lost wakeup, is precisely such a race: the receiver
  reads the queue Empty, and before it blocks in `notif_wait`, the sender enqueues
  + signals; if the wait then misses the accumulated signal, the receiver sleeps
  forever.

So the reactor itself is **sync-primitive-free in production** (one thread); the
raced, shared state lives in the **kernel objects**. This dictates the
architecture: put the kernel objects behind a **transport seam**, give that seam a
deterministic in-memory model, and let Shuttle schedule the communicating
processes as its "threads" over the model's shared state. Shuttle threads =
processes; the shared state = the simulated channel + notification; the crate's
real send/recv/reactor code runs unchanged inside each thread.

(This is *also* why §4.2's "no `std::sync`/`std::thread` in library code" rule is
easy to honour here: the production crate has no shared atomics to hide. The
cfg-swappable sync seam wraps the **model's** shared state and the **harness's**
thread-spawning, not the reactor.)

---

## 3. Target architecture (verifiable-first)

### 3.1 The transport seam — the IPC `Env`/`Hal`

A trait abstracting exactly the kernel IPC surface the reactor needs, 1:1 with
`sys.rs` and §3.3/§3.6:

```
trait Transport {
    fn send_nb(&self, ch: Chan, msg: &WireMsg, caps: &[CapRef]) -> Result<(), SendErr>; // Full
    fn recv_nb(&self, ch: Chan, buf: &mut WireMsg, dests: &mut [Slot]) -> Result<Recv, RecvErr>; // Empty | NoSlot
    fn bind(&self, ch: Chan, ev: Event, notif: Notif, bits: u64) -> Result<(), Err>; // READABLE|WRITABLE|PEER_CLOSED
    fn notif_signal(&self, n: Notif, bits: u64);
    fn notif_wait(&self, n: Notif) -> u64;        // accumulated word, which clears (§3.6)
    fn timer_arm(&self, t: Timer, n: Notif, bits: u64, delta: u64);
}
```

- **Production:** `SyscallTransport` — a zero-cost shim over `ipc::sys`
  (`chan_send`/`chan_recv`/`chan_bind`/`notif_signal`/`notif_wait`/`timer_arm`).
- **Test/model:** `ModelTransport` — a deterministic in-memory kernel: a bounded
  FIFO queue ring (capacity = the §3.2 donated-bytes/slot count), a notification
  word with the §3.6 semantics (signalers **OR** bits in; a waiter receives the
  whole accumulated word, which **clears**; FIFO waiter queue), and a peer-closed
  flag that fires bindings on teardown (§3.3). Its shared state is built on
  `ipc::sync` and shared across Shuttle/Loom threads via `ipc::sync::Arc`.

The seam is the IPC analogue of `kcore`'s `Env`/`Hal` split: the thing that makes
the unit host-checkable at all.

### 3.2 The sync seam (std / loom / shuttle), generalizing Phase 1

`ipc::sync` re-exports `{Arc, Mutex, atomic::*, thread}` from `std` (default host
test), `loom` (`cfg(loom)`), or `shuttle` (`cfg(shuttle)`), exactly as
`urt::time` now does for its atomics. Reuse the proven Phase-1 wiring verbatim:
`[target.'cfg(loom)'.dependencies]` / `[target.'cfg(shuttle)'.dependencies]` (off
the normal `no_std` build), the `cfg(loom)`/`cfg(shuttle)` `check-cfg` lint
entries, and the spin-hint seam. The production `no_std` crate pulls **neither**
loom nor shuttle and uses **no** `ipc::sync` (single-threaded) — the seam is
compiled only for the model and harnesses.

### 3.3 Determinism rules (so the scheduler owns all nondeterminism)

Logical time only (the timer is a schedulable model event, never wall-clock); no
real I/O or syscalls on the model path; bounded queues; message bodies are fixed
test vectors. The only nondeterminism is the thread schedule Shuttle/Loom control.

---

## 4. The IPC API (what every server sees; §3.3–§3.7)

Layered so each layer is independently Shuttle-testable over `ModelTransport`.

### 4.1 Non-blocking primitives (§3.3, §3.4)
`send_nb` → `Ok | Full` (never drops — a dropped message can carry a cap);
`recv_nb` → `Ok(msg) | Empty | NoCspaceSlot` (on slot exhaustion the message
**stays queued**, receiver retries). Caps travel in the message's 4 slots, and
receivers **tolerate null slots** (revocation may empty them in flight, §3.4).

### 4.2 The reactor (§3.6) — the lost-wakeup core
An epoll-shaped `register(source, signals, key)` / `wait() -> (key, signals)` API
implemented over notification **bit-groups** (O(group) scan now; the wait-set
object is an O(1) drop-in later — *the API never changes*, §3.6). The reactor
**owns the "bind, poll once, then wait" discipline** — the one place the
lost-wakeup obligation lives, proven by harness #1. No server ever sees a bit.

### 4.3 Async + bounded-retry send over backpressure (§3.3)
`send().await`, blocking send, and bounded-retry send are library code over the
non-blocking primitive plus the reactor's **writability** signal: on `Full`,
register for writable, wait, retry — **no message dropped, sender always
eventually progresses when the receiver drains** (harness #2).

### 4.4 The valuable-cap ack protocol (§3.4)
A small userspace handshake for valuable cap handoffs, so a cap in flight is
neither lost to channel destruction nor duplicated across a sender/receiver race
— move-semantics (exactly one owner: sender, queue slot, or receiver) preserved
under concurrency (harness #4). No kernel reverse-path (§3.4 forbids a Mach tar
pit); this is pure userspace protocol.

### 4.5 Serialization (§3.7)
A **module-private** `encode`/`decode` trait over **postcard**, behind which
servers construct/consume plain, **boring** message types (no borrowed lifetimes,
no `flatten`, no untagged enums, no non-string-keyed maps). Reuse the done,
byte-stable `Header` (`header.rs`). Decoders treat payloads as untrusted and
**reject trailing bytes** — cargo-fuzz targets (§5.4).

### 4.6 Sessions and connect (§3.5)
`connect`: the **client funds** the channel pair (retypes it from its own untyped,
§3.2) and sends one endpoint plus a requested **bulk-window size**; the server
grants or refuses under its per-session quota at this single admission point. The
per-client channel is where per-client queue accounting and fairness live. The
bulk path (§3.1): the message is a doorbell/descriptor naming `(window, offset,
length)` into a server-granted shared frame; MVP grants one window (descriptor
window field always 0). The window's **concurrent-access discipline is §4.8** —
shared *memory* (touched by loads/stores), a second, Loom-relevant surface scoped
below.

---

## 5. Verification — four tiers wired from the first commit

### 5.1 TLA+ (design tier) — a new spec, checked *before* the reactor is built
`tla/ipc_reactor/IpcReactor.tla` (+ `.cfg`), in the house style of
`CapRevocation`/`CommitProtocol`. Model: a sender, a receiver, a **bounded queue**,
and a **notification word** (bits; OR-accumulate; clear-on-receive — faithful to
`kcore::notification`'s "wait checks the word before sleeping"), with the
send/poll/wait-consume/block actions. The framing is **both safety and liveness**
(the safety invariants gate and port to the §5.2 Shuttle harness; the liveness
property is the TLC-only extra, in the spirit of the safety-only
CapRevocation/CommitProtocol house style):
- **NoLostWakeup** (safety invariant): a blocked receiver has nothing pending —
  never `blocked ∧ queue-non-empty ∧ word-zero`. A lost wakeup is exactly that
  bad state; a negative control (dropping the wait's word-check) makes TLC report
  it. This is the gate, and it ports to Shuttle.
- **NoDrop** (safety): every offered message is received or still queued — `Full`
  is the only refusal, never a silent drop.
- **FifoPerChannel** (safety): receive order = send order.
- **EventuallyDelivered** (liveness, weak fairness): every offered message is
  *eventually* received — the property Shuttle's bounded randomized search
  **cannot** establish and TLC can. The project's first fairness/liveness spec.

It deliberately **does not re-model cap move/teardown** — `CapRevocation.tla`
already proves "queue slots are CDT-visible, revocation deletes in-flight caps,
teardown fires every peer-closed binding" (its `MoveSemantics`/`FireSafe`). The
new spec owns only the genuinely-new **wakeup + backpressure** protocol.

**New sequencing rule (mirrors the M1/M2 gates):** *the `IpcReactor` TLA+ model
must be TLC-checked before the reactor implementation (§4.2) lands.*

### 5.2 Shuttle (primary implementation tier) — the §4.2 catalog over `ModelTransport`
The five harnesses, each re-checking a §3.3/§3.5/§3.6 property on the **real
reactor code**: (1) no lost wakeup, (2) `Full` backpressure + retry, no drop,
(3) FIFO / no double-delivery under concurrent senders, (4) valuable-cap ack
(no lost/dup cap), (5) multi-client fairness/liveness *smoke* (best-effort; true
liveness stays §5.1's job). Pinned seed + a committed `shuttle::replay` corpus for
any bug found (the §5 convention of the loom-shuttle plan; the fuzz-corpus
discipline).

### 5.3 Loom (weak-memory fragment) — exhaustive, tiny
Point Loom at the one ordering-sensitive fragment: the **poll-then-wait** sequence
against the notification word (the lost-wakeup race is a memory-ordering question
at root, the same class as the seqlock). Shuttle (SC, scale) and Loom (weak
memory, exhaustive at a tiny bound) are complementary here exactly as the
loom-shuttle plan §3 argues.

### 5.4 cargo-fuzz + Kani (sequential chokepoints)
The postcard body **decoders** become cargo-fuzz targets (`ipc/fuzz`, reject
trailing bytes, §3.7) alongside the existing CAS/loader/storage corpora. The
fixed `Header` codec stays Kani-verified (`ipc/src/proofs.rs`, done); any new
*pure* codec helper gets a Kani harness in the same module.

---

## 6. Phasing (each layer ships with its harness)

0. **Scaffold + TLA+.** Write and TLC-check `IpcReactor.tla`; land the `Transport`
   seam, the `ipc::sync` seam (Phase-1 wiring), and `ModelTransport`. No behavior
   yet — the rig that makes everything below verifiable.
1. **Non-blocking send/recv** over the seam + the **FIFO / no-drop** Shuttle
   harness (#3).
2. **The reactor** (`register`/`wait`, bind-poll-wait) + the **lost-wakeup**
   Shuttle (#1) and Loom (§5.3) harnesses — the headline, build first after the
   rig (it is the likeliest real defect, `0_mvp.md`/loom-shuttle §7).
3. **Async / bounded-retry send + backpressure** + the **`Full`+retry** harness (#2).
4. **Valuable-cap ack protocol** + its harness (#4).
5. **Serialization** (the module-private postcard trait) + the fuzz targets.
6. **Sessions / connect**, then **re-point `storaged`** off its hand-rolled
   drain-then-wait loop onto the reactor (the first real consumer; proves the API
   hides bits) + the fairness smoke (#5). storaged serving a second concurrent
   session is the §4.3-of-loom-shuttle "Phase 3" target, unlocked here.

---

## 7. CI integration

- **Shuttle:** the existing `concurrency` job gains `RUSTFLAGS="--cfg shuttle"
  cargo test -p ipc` (no per-test filter → new harnesses auto-gate, the property
  the kani job prizes); pinned seed.
- **Loom:** the same job's loom step extends to `-p ipc`.
- **TLA+:** `IpcReactor.tla` joins the `model` job (`tla-model-check.sh`) next to
  CapRevocation + CommitProtocol.
- **Fuzz:** the postcard decoder targets join `fuzz.yml` (per-PR corpus replay +
  nightly hunt) and the Miri seed-replay sweep.
- The `panic = "abort"` profile is a non-issue (test profile always unwinds; §6 of
  loom-shuttle, proven by the seqlock run).

---

## 8. Risks and mitigations

- **Mis-modelling the concurrency.** The races are cross-process through kernel
  objects, not intra-crate threads (§2). Mitigation: the `Transport`/`ModelTransport`
  seam *is* the model boundary; get the notification semantics (OR-accumulate,
  clear-on-receive, FIFO waiters) byte-faithful to `kcore::notification`.
- **Lost wakeup is the likeliest real bug** (loom-shuttle §7). Mitigation: build
  harness #1 + the Loom fragment immediately after the rig (phase 2), and gate the
  reactor impl on the §5.1 TLA+ check.
- **API leaking the bit shape** would force a server rewrite at the wait-set
  upgrade (§3.6). Mitigation: `register(source, signals, key)` is the only event
  surface; bit-groups are private; a grep-style guard (no server names a bit) like
  the kcore layering check.
- **The bulk window is a second concurrency surface** (shared *memory*, §4.8,
  Loom-relevant). Mitigation: MVP keeps reads message-bounded with the descriptor
  in place (`0_mvp.md`), and the concurrent bulk-window access discipline is
  scoped to a follow-up with its own Loom note — not on the reactor's critical
  path.
- **Shuttle is SC-only** (it can't witness a weak-memory wakeup miss). Mitigation:
  Loom owns the ordering fragment (§5.3) and TLC owns liveness (§5.1); Shuttle is
  the interleaving-at-scale tier, never the sole proof of the ordering.

---

## 9. Out of scope

- **The wait-set kernel object** (§3.6 committed upgrade) — the reactor API already
  hides it; a kernel-side change for later, changing no server code.
- **The IDL / non-Rust userspace** (§3.7) — postcard now; the schema/second-backend
  migration is a deliberate future "public ABI" milestone.
- **Multi-window bulk** (§3.1 grow-only upgrade) and the full concurrent bulk-data
  path (§4.8) — MVP is one window, message-bounded reads.
- **Kernel channel/notification changes** — `kcore` owns those; `CapRevocation.tla`
  already models the cap-move/teardown safety this crate relies on.
