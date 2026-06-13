# Plan: wiring in the Loom/Shuttle concurrency tier

**Status:** **proposed.** Phase 0 (the `scratchpad` host proof of both crates)
has landed / is in CI; everything below is the work that follows from it.
**Spec baseline:** `doc/spec/2_spec_rev2.md` — §3.3 (send/recv + backpressure),
§3.5 (the IPC crate), §3.6 (notifications / lost-wakeup), §6 (verification tiers).
**Sibling plan:** `doc/plans/0_kani-rewrite.md` — the *sequential* kernel tier;
this plan is its concurrency counterpart and shares its framing (a verifiable-first
seam, bounded models, a per-PR budget, pinned tool versions).
**Status-quo source:** `doc/results/0_mvp.md` — records that the tier "had no
target and was not exercised."

---

## 1. Background and goal

The verification table (§6) names **Loom / Shuttle** as the concurrency tier,
applied to "userspace servers and the IPC crate," and §3.5 calls the `ipc` crate
"the first serious Loom/Shuttle target." Until the `scratchpad` crate, **there
was no Loom or Shuttle anywhere in the tree** (grep: zero hits). This was not an
oversight so much as a tier with no target: the single-session MVP never
generated the multiplexing pressure that would have forced the async reactor —
the thing §3.5/§3.6 describe — into existence, so `ipc/src/lib.rs` is still
~240 lines of syscall wrappers with `send`/`recv` as `todo!()` stubs, and the
servers are hand-rolled drain-then-wait poll loops (`doc/results/0_mvp.md`,
debt #2). A concurrency-testing tier with nothing concurrent to test stayed dark.

**What these tools actually check, and what they need.** Loom and Shuttle model
**std-style shared-memory threading**: `Arc`, `Mutex`, atomics, `thread::spawn`,
channels. They do it by *substituting* those primitives — `loom::sync::*` /
`shuttle::sync::*` for `std::sync::*` — and then driving the program through many
schedules. For a piece of code to be a target it must therefore be (a) **host-
buildable** (std available under test), (b) written so its sync primitives sit
behind a **swap point** (a `cfg`-selected module, exactly the idiom `scratchpad`
demonstrates), (c) **deterministic** under the model (no real wall-clock, no real
I/O, no real syscalls on the tested path), and (d) **genuinely multi-threaded
shared state** — otherwise there is nothing to interleave.

**The goal of this plan** is to take the tier from "named but dark" to "wired in
and earning its keep," by (1) pointing it at the *one* piece of real, shared-
memory concurrency that exists in the tree today — the time-page seqlock — and
(2) defining the structural contract the IPC crate must meet to be the serious
target the spec always intended, so that when IPC is written it is written
Shuttle-drivable from day one (the concurrency analogue of how `0_kani-rewrite.md`
forced the `Env`/`Hal` seam and the no-int→ptr rule onto `kcore` up front).

**The four-tier picture this completes.** With this plan the system's correctness
machinery partitions cleanly by *what kind of property* each tool can see:

| Tier | Tool | Sees | In this system |
|---|---|---|---|
| Design / protocol | TLA+ / TLC | unbounded transition-system safety, some liveness | commit protocol, cap revocation |
| Sequential impl | Kani / CBMC | all inputs at bounded size; panic/overflow/UB — **no concurrency** | `kcore` (single-core kernel) + host chokepoints |
| **Weak-memory lock-free** | **Loom** | **all interleavings + C11 reorderings, exhaustive at small bound** | **the userspace seqlock; future lock-free fragments** |
| **Async at scale** | **Shuttle** | **randomized thread interleavings (SC), scales to large programs** | **the IPC reactor; userspace servers** |

`0_kani-rewrite.md` §1 already drew the top half of this line — "Concurrency
(irrelevant: the kernel is single-core, non-preemptible — the concurrency tier is
Loom/Shuttle for userspace, §6)." This plan draws the bottom half.

---

## 2. Where concurrency actually lives in this codebase

A precise inventory matters, because the tier's reputation for being unused comes
from looking for concurrency in the wrong places.

- **The kernel is single-core and non-preemptible**, and is bare-metal
  (`aarch64-unknown-none`, not host-buildable). There is no in-kernel thread
  interleaving to model, and Loom/Shuttle could not build it if there were. →
  **Owned by Kani (sequential invariants) and TLA+ (protocol-level concurrency at
  the design abstraction).** Out of scope here, by construction.
- **Userspace processes are single-threaded by construction** — `urt`'s heap
  carries the comment "Single-threaded processes; no concurrent access by
  construction," and the slot allocator and spawn machinery follow suit. *Within*
  one process there is likewise nothing to interleave.

So where is the genuine shared-state concurrency? At exactly two **inter-domain**
seams — the places where independently-scheduled execution contexts touch the
same memory or the same kernel object:

### 2.1 Shared memory across address spaces — the time page (§2.6) — EXISTS NOW

`urt/src/time.rs` is a **seqlock**: a `TimePage` (`seq`, `wall_base_ns`,
`cntvct_base`, `cntfrq`, all atomics) is published by one writer and mapped
read-only into every other process, which reads it concurrently. Today `seq` is
constant zero (write-once at boot), but the reader ships seqlock-shaped on
purpose (deferred clock-setting, §8) and `sample()` (`time.rs:75`) is Boehm's
recipe: acquire-load `seq`, **relaxed** loads of the three data fields, an
**acquire fence** (`time.rs:90`), re-read `seq`, retry on change. The whole
correctness argument is a **memory-ordering** argument — the fence at `:90`
exists precisely so the relaxed data loads cannot be reordered past the `seq`
re-read and let a torn sample slip through.

This is real, lock-free, weak-memory-critical shared-state concurrency, it is
implemented today, and it already has a *hand-rolled, racy* multi-threaded test —
`torn_writes_are_never_observed` (`time.rs:250`): a writer thread tears the page
with the pattern `(k, 2k, 3k+1)` while the main thread samples and asserts no torn
triple. That test is a **probabilistic** catch (50 000 native iterations, hope to
hit the bug) running under SC on x86 (a strong-memory host), plus a Miri pass for
some weak-memory randomization. It is the perfect Loom target and is currently
the *weakest-verified* genuinely-concurrent code in the tree.

### 2.2 Async message passing — the IPC reactor (§3.5/§3.6) — DOES NOT EXIST YET

The spec's designated target. The §3.6 lost-wakeup discipline ("bind, poll once,
then wait"), §3.3 `FULL` backpressure + bounded-retry send, FIFO-per-channel
delivery, and the valuable-cap ack protocol are all *interleaving* properties
over concurrent senders/receivers multiplexed on the notification word. This is
large-state-space, missed-wakeup-class concurrency built on higher-level
primitives (a notification flag, queue head/tail, a wait list) — **not** hand-
rolled relaxed atomics. None of it is implemented (`send`/`recv` are `todo!()`).

### 2.3 storage-server multi-session — FUTURE, downstream of 2.2

`storage-server` is single-threaded today (`Server::handle` is `&mut self`, one
request at a time; the `gc_requested` flag is a plain `bool`). When the IPC
transport binds it to multiple concurrent sessions, that state becomes shared and
becomes a Shuttle target — but only after 2.2 exists.

**Summary map:**

| Seam | Bug class | Implemented? | Tool | Readiness |
|---|---|---|---|---|
| time-page seqlock (2.1) | torn read under **weak memory** | ✅ yes | **Loom** | **ready now** |
| IPC reactor (2.2) | missed wakeup / lost cap / FIFO under interleaving | ❌ stubs | **Shuttle** | after IPC is written |
| storaged sessions (2.3) | shared session/ticket state | ❌ single-thread | Shuttle | after 2.2 |

---

## 3. Which tool — both, divided by bug class (the decision)

The two crates are **not** interchangeable, and the deciding property is the
**memory model**:

- **Loom models the C11/C20 memory model.** It explores not only thread
  interleavings but the *permitted weak-memory reorderings* — a `Relaxed` load
  observing a stale value, a load/store reordered across anything weaker than a
  matching fence. It is **sound and exhaustive** within a bounded number of
  threads/preemptions, which means its state space explodes quickly: models must
  be tiny (2 threads, 1–2 critical sections).
- **Shuttle does not model weak memory.** It is a **randomized scheduler** over
  **sequentially-consistent** atomics. It finds interleaving bugs — missed
  wakeups, lost signals, deadlocks, ordering races — but assumes SC, so it cannot
  see a reordering-induced bug. In exchange it **scales**: it was built to stress
  large concurrent systems where exhaustive search is hopeless, via randomized
  schedules over many iterations with a **replayable seed**.

This dictates the assignment directly:

- **The seqlock (2.1) must be checked by Loom.** Its entire reason to exist is the
  acquire/release/relaxed discipline and the `:90` fence. Run it under Shuttle and
  the relaxed loads behave as SC — Shuttle would **pass a seqlock that is actually
  broken under weak memory** (e.g. with the fence removed). For this target Loom
  is not merely preferable, it is the *only correct* tool of the two.
- **The IPC reactor (2.2) is the natural Shuttle target.** Many tasks, a large
  interleaving space, bugs of the missed-wakeup/backpressure kind, built on
  higher-level primitives where SC is the right abstraction. Loom would
  combinatorially explode on a multi-task reactor; Shuttle is what scales. (Loom
  still gets pointed at any *small* lock-free fragment extracted from the reactor —
  e.g. a notification-word OR-in/clear `compare_exchange` — where weak memory
  re-enters.)

**So: both, because the project has both bug classes, and neither tool covers the
other's.** This is a firmer answer than "pick one." It also yields a
*complementarity* pattern worth using deliberately: on a single small target you
can run **Loom** (exhaustive, weak-memory, tiny bound) *and* **Shuttle**
(randomized, SC, many iterations past Loom's exhaustible bound) as belt-and-
suspenders — which is exactly what `scratchpad` already does, running the same
two-writer test under both. The scratchpad is therefore not just a smoke test;
it is the **template** for the dual-tool idiom this plan rolls out.

---

## 4. The work, phased

Honest framing: this is less a "rewrite" of working code than *finally wiring in a
tier the spec committed to.* Only one piece of existing code gets a real refactor
(`urt::time`, Phase 1); the larger deliverable (Phase 2) is a **forward constraint
on how the IPC crate gets written**, since that crate does not yet exist.

### 4.0 Phase 0 — `scratchpad` (DONE)

Both crates proven to build and run on the host; the `cfg(loom)` sync-swap idiom
and the dual-tool (Loom exhaustive + Shuttle randomized) pattern established on a
toy two-writer slot. This is the reference template every later phase copies.

### 4.1 Phase 1 — Loom on the time-page seqlock (READY NOW; the immediate win)

Turn the *probabilistic* `torn_writes_are_never_observed` into an **exhaustive
Loom proof** that no torn sample is observable under any C11-permitted
interleaving and reordering.

1. **Sync shim.** Replace `time.rs:28`'s `use core::sync::atomic::{…}` with a tiny
   internal module that selects the source by cfg:
   ```rust
   #[cfg(loom)]      use loom::sync::atomic::{AtomicU64, AtomicI64, Ordering, fence};
   #[cfg(not(loom))] use core::sync::atomic::{AtomicU64, AtomicI64, Ordering, fence};
   ```
   The production struct *itself* uses these atomics, so (unlike `scratchpad`)
   the swap lives in the module, not just the test.
2. **Handle the two loom incompatibilities** (the most likely friction, called out
   so the implementer expects them):
   - **Loom atomics have no `const fn new`** and **cannot live in `static`s.** So
     `const fn TimePage::new` (`:62`), the `const _` layout assertions (`:45`),
     the `static TIME_PAGE` pointer cell (`:141`), and the `attach`/`page`/
     `now_utc_ns` placement machinery (`:149–193`) must be `cfg(not(loom))`-gated.
     Under `cfg(loom)` provide a non-const `new` and expose only `sample()` plus a
     test-only writer. This is sound because the page-*location* indirection
     (the static, the `va` cast) is **not** what the seqlock proof is about — the
     proof is about the read/write *protocol* on a `TimePage` the Loom model
     constructs inside `loom::model(...)` and shares via `loom::sync::Arc`.
   - **Atom width/signedness subset.** If loom 0.7's atomic set lacks `AtomicI64`,
     the shim stores `wall_base_ns` through an `AtomicU64` with a bit-cast in the
     loom build only — a mechanical shim detail, not a protocol change.
3. **The Loom model harness** (`#[cfg(all(test, loom))]`): one writer thread doing
   the odd→stagger→even seqlock write with the `(k, 2k, 3k+1)` tearing pattern,
   one reader calling `sample()`, assert the reader never observes a torn triple.
   **Bound it hard** — 1–2 writes — so the state space is finite; that suffices
   because the torn-read invariant is per-critical-section, not cumulative.
4. **Keep the existing proptest** (`time.rs:250`) as the fast native smoke + the
   Miri weak-memory randomized pass. Loom **adds** the exhaustive proof; it does
   not replace proptest. A one-line comment records the tier boundary (proptest =
   breadth/Miri; Loom = the exhaustive ordering proof).
5. **Optional Shuttle breadth-smoke** of the same model — explicitly flagged
   **non-certifying** (SC only; it cannot witness the torn-read class). Useful as a
   second pair of eyes on interleaving, never as the seqlock's proof of record.

**First negative control (do this early, per the `0_kani-rewrite.md` "confirm,
don't assume" discipline):** delete the acquire fence at `time.rs:90` on a scratch
branch and confirm Loom produces a torn-read counterexample. A tool that cannot
fail on the known-bad version is not yet proving anything; this demonstrates the
fence is load-bearing and that Loom earns its place before we trust a green run.

### 4.2 Phase 2 — a Shuttle-ready IPC crate (the real "first serious target"; gated on IPC existing)

The headline target, dependent on the IPC implementation (`0_mvp.md` debt #2).
This plan does **not** implement IPC; it states the **structural contract** the
IPC crate must satisfy to be Shuttle-drivable, so it is written verifiable-first:

- **A `cfg`-swappable `sync` module** (`std` / `loom` / `shuttle`) behind which
  *every* atomic / `Mutex` / `Arc` / channel the reactor uses sits — **no direct
  `std::sync` or `std::thread` reference in library code.** (Phase 1's shim,
  generalized to three backends.)
- **A simulated-kernel seam.** The reactor reaches the kernel's channel rings and
  notification word through a trait, so host tests substitute a deterministic
  in-memory model of those objects (no syscalls, no `ipc::sys`). This is the IPC
  analogue of `kcore`'s `Env`/`Hal` seam — the thing that makes the unit
  host-checkable at all. Shuttle then schedules multiple reactor tasks over the
  model.
- **Determinism rules:** logical clock only (no wall time), no real I/O, bounded
  queues, no nondeterminism the scheduler doesn't own.

**Model-harness catalog** (each a §3.3/§3.5/§3.6 property; Shuttle primary):

1. **No lost wakeup** — the headline §3.6 property. A receiver that polls Empty
   then waits is *always* woken by a sender that subsequently fills the queue,
   under every interleaving of bind / poll / wait / signal. The "bind, poll once,
   then wait" discipline is correct iff this holds.
2. **`FULL` backpressure + retry safety** — a sender blocked on a full queue,
   woken by a draining receiver, always makes progress and **no message is
   dropped** (a dropped message can carry a cap; a lost cap is unacceptable, §3.3).
3. **FIFO-per-channel / no double-delivery** under concurrent senders.
4. **Valuable-cap ack protocol** — an in-flight cap is neither lost nor
   duplicated across a sender/receiver race; move-semantics (§3.4) hold under
   concurrency.
5. **Multi-client fairness/liveness smoke** — the §3.5 session pattern under
   Shuttle's scheduler. Best-effort only: true liveness/fairness stays a TLA/
   argument concern (same boundary `0_kani-rewrite.md` draws for liveness).

**Tool split inside Phase 2:** Shuttle for the reactor (scale, randomized,
replayable); Loom pointed at any *small* extracted lock-free fragment (e.g. the
notification bit-group `compare_exchange`) where weak memory re-enters.

### 4.3 Phase 3 — storage-server multi-session (furthest out; downstream of 4.2)

Once the IPC transport binds storaged to concurrent sessions, Shuttle-check the
request interleavings over the shared session/ticket maps and the `gc_requested`
flag (which becomes a real atomic). Lowest priority; entirely gated on Phase 2.

---

## 5. Conventions

- **The sync-shim cfg-swap is the one idiom.** Every Loom/Shuttle-targeted unit
  imports its sync primitives from a single cfg-selected module; library code
  never names `std::sync`/`std::thread` directly. One place to swap, three
  backends (`std`/`loom`/`shuttle`).
- **Loom/Shuttle add, they do not replace.** proptest and Miri keep everything
  they own (breadth, the conversion arithmetic, Miri's UB/weak-memory
  randomization). The model harnesses are the *exhaustive/scheduled* layer on top.
- **Deterministic harness rules:** no real wall-clock, no real I/O, no real
  syscalls on the tested path; logical clocks and in-memory kernel models only.
- **Shuttle = fixed seed + replay corpus.** CI runs Shuttle with a pinned seed and
  iteration count for reproducibility; every bug Shuttle finds gets its failing
  schedule committed as a `shuttle::replay(...)` regression test — the same
  corpus-replay discipline the fuzz tier uses (`scripts/fuzz.sh`, committed seeds).
- **Pin the tool versions** (`loom = "0.7"`, `shuttle = "0.7"`), upgraded by
  deliberate PRs that re-run the suite — the same policy as the pinned
  cargo-kani 0.67.0.

---

## 6. CI integration

- **Loom job:** `RUSTFLAGS="--cfg loom" cargo test -p urt --lib` (later also
  `-p ipc`), **bounded** to stay inside a time budget — small models plus, if
  needed, `LOOM_MAX_PREEMPTIONS`. Same budget discipline as the `kani` job (each
  model cheap; the suite capped).
- **Shuttle job:** plain `cargo test -p ipc` (Shuttle needs no special cfg — it is
  used via `shuttle::*` directly in tests), with the pinned seed + iteration count.
- **One new `concurrency` job in `.github/workflows/ci.yml`** (or fold the Loom
  cfg into `host-tests`); model harnesses **auto-gate** — running the crate under
  the cfg/flags picks up any new `loom::model`/`shuttle::check_*` test with no
  per-harness filter, the property the `kani` job prizes.
- **Heavy/exhaustive runs go off-PR.** If a deep Loom model (more preemptions, a
  wider bound) outgrows the per-PR budget, it moves to a scheduled weekly workflow
  mirroring `kani-deep.yml` — the per-PR job keeps the cheap bound, the weekly job
  goes deep. Same two-tier structure as the Kani side.
- **The `panic = "abort"` profile is a non-issue:** `cargo test` always builds the
  test profile with unwind regardless of the dev profile's `panic` setting, so
  Loom/Shuttle's internal `catch_unwind` works with no per-crate override —
  already proven by `scratchpad` building green.

---

## 7. Expected findings (to confirm early, not assume)

- The seqlock is *probably* correct (proptest + Miri are clean). Loom's value is
  either a **proof** of that, or — more usefully — a precise counterexample if a
  fence/ordering is subtly wrong. The fence-removal negative control (§4.1) is the
  first thing to run: it must fail, or the harness proves nothing.
- The **IPC lost-wakeup discipline is the most likely real defect site** once
  written — the "bind, poll once, then wait" sequence is exactly the racy idiom
  these tools exist to check, and it has no implementation yet to have been gotten
  right by luck. Build harness #1 (§4.2) first.

---

## 8. Risks and mitigations

- **Loom atomics: non-const, no `static`.** Handled by `cfg(not(loom))`-gating the
  const constructor + static placement machinery and testing the protocol on an
  `Arc`-shared page constructed inside `loom::model` (§4.1.2). The one real
  refactor in this plan; mechanical.
- **Loom state explosion.** Bound every model to ~2 threads / 1–2 critical
  sections; deeper exploration is off-PR (§6). Exhaustiveness at a *tiny* bound is
  the trade Loom is designed for — and is enough for per-critical-section
  invariants like the seqlock's.
- **Shuttle is SC-only.** **Never use Shuttle to certify lock-free / weak-memory
  code** — that is Loom's job. This tier boundary is documented (§3) so a future
  reader does not "verify" a seqlock with the wrong tool and get a false green.
- **The IPC crate does not exist.** Phase 2 is a forward constraint, explicitly
  gated; the guaranteed near-term deliverable is **Phase 1** (a real, exhaustive
  proof of real, shipping code). Phase 2 rides on IPC being written (`0_mvp.md`
  debt #2) and is sequenced after it.
- **Tool churn.** Loom/Shuttle pinned and upgraded by deliberate PRs (§5).

---

## 9. Out of scope

- **The kernel** — single-core, non-preemptible, bare-metal; owned by Kani
  (sequential) and TLA+ (protocol). Loom/Shuttle cannot build or model it.
- **Process-internal single-threaded logic** — the heap, slot allocator, CAS
  arithmetic, wire codecs: nothing to interleave; proptest/Miri/Kani keep them.
- **Liveness and fairness as proofs** — best-effort scheduler smoke only; true
  liveness stays a TLA+/argument concern, as on the Kani side.
- **Implementing the IPC crate itself** — that is `0_mvp.md` debt #2, a separate
  body of work; this plan defines the verifiability contract it must meet and the
  harnesses that ride on it, not the reactor.
