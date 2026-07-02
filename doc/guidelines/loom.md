# Loom and Shuttle

Loom and Shuttle are the **concurrency-interleaving tier**: they exercise the
*actual code's* thread interleavings, the class of bug — a lost wakeup, a torn
seqlock read, a non-exclusive lock — that a single-threaded property test never
schedules and a design-level model (`tla.md`) proves only *above* the code. rev2§6
sites this tier at the userspace servers and the IPC crate; in practice it also
certifies the userspace-runtime concurrency primitives (a yielding spinlock, a
time-page seqlock, a futex table) once in-process threads exist.

The two tools are **not interchangeable**, and the distinction is the whole game:

- **Loom is certifying.** It enumerates interleavings *exhaustively* and models
  the `Acquire`/`Release`/`SeqCst` fences a protocol actually uses. A Loom-green
  model is a proof — over the fence semantics it models — that no interleaving
  violates the asserted invariant.
- **Shuttle is a non-certifying breadth-smoke.** It drives a *randomized*
  scheduler and reinterprets every ordering as SeqCst (it prints a one-time
  warning saying so), so it cannot witness a weak-memory defect. It is a second
  scheduler's sanity pass — deadlock, retry-loop, and logic smoke — and the
  reproducibility harness for a schedule worth pinning.

Which *shape* of claim lands on this tier rather than on Verus is decided in
`verification.md` (the routing dispatcher) and pinned down by `verus.md` §15.5:
the version-pinned Verus ghost atomics are **SeqCst-only with no standalone
fence**, so a structure that ships `Relaxed` data behind an `Acquire`/`Release`
fence — the seqlock shape — *cannot* be proven faithfully in Verus (a SeqCst
proof would certify a different binary than ships, and under SeqCst the structure
is trivially correct, dodging the real reordering question). That shape is
irreducibly Loom-certifying. This note does not re-argue the routing; it teaches
the *how* — how to wire a module for the models, author a model with teeth, and
keep the search bounded — exhaustively.

---

# Part A — working discipline

## What each tool models, and does not

Scope every claim to what the tool actually checks, and say so in the model's own
docstring (below). The dividing lines:

| The code synchronizes through… | Load-bearing tool | Why |
|---|---|---|
| A raw atomic with an explicit `Acquire`/`Release` fence over `Relaxed` data (seqlock, hand-rolled lock) | **Loom** (certifying) | Loom models the fence and enumerates interleavings around it; this is the shape Verus cannot reach under the SeqCst-only pin (`verus.md` §15.5). |
| A genuine SeqCst atomic protocol | **Verus** tokenized state machine (`verus.md` §15) | When the shared state *is* SeqCst, the deductive path applies and gives an unbounded proof. |
| Only `Mutex`/`Condvar` (no atomics) | **Shuttle** (interleavings) | There is no weak memory to model, so Loom's fence modeling buys nothing; the risk is a scheduling interleaving (lost wakeup, deadlock), which the randomized scheduler hunts. |

Two hard limits set the honest scope of a Loom result:

- **Loom does not faithfully reorder `Relaxed` atomics.** A structure that is
  correct *via an explicit fence* (data fields `Relaxed`, ordered by a
  `fence(Release)`/`fence(Acquire)` pair) is proven correct **via the modeled
  fence**, not over every C11-permitted reordering. The claim must say this — a
  Loom-green seqlock is "no torn read under any interleaving *around the fence*",
  not "under every weak-memory execution".
- **Shuttle collapses all orderings to SeqCst.** It therefore *cannot* witness a
  torn read or any weak-memory defect — under SeqCst the seqlock cannot tear. Run
  it for interleaving/deadlock breadth and reproducibility, never as the
  correctness authority for a fence-ordered structure.

Name the actually-load-bearing tool, not the strongest-sounding one.

## Running the models

Both tools are selected by a `RUSTFLAGS` **cfg**, not a Cargo feature, because
they substitute into *library* code (the field atomics), not just test code — a
feature would leak into the dependency graph of a normal build. The paired
invocations, one per tool:

```sh
# Loom — the certifying proof. Exhaustive; no seed.
RUSTFLAGS="--cfg loom" cargo test -p urt -p ipc --lib

# Shuttle — the non-certifying breadth-smoke. Randomized; seeded for repro.
RUSTFLAGS="--cfg shuttle" cargo test -p urt -p ipc --lib
```

CI runs both in one `concurrency` job on the stable host toolchain, **with no
per-test filter** — a newly added `loom::model(...)` or `shuttle::check_random`
test in a covered crate auto-gates, exactly the property the deductive gate also
prizes. The job carries a wall-clock `timeout-minutes` cap as *margin*, not as a
tuning lever (see "Bound the search" below).

## The dependency posture

loom and shuttle are declared as **real `cfg`-gated dependencies**, not
`[dev-dependencies]`, precisely because they must replace the atomics inside
library code when their model is being built:

```toml
# Only pulled when the model is being built (RUSTFLAGS="--cfg loom|shuttle").
# A normal or aarch64 build never sees them. Pinned so an upgrade is a
# deliberate PR, exactly like the verified-toolchain pins.
[target.'cfg(loom)'.dependencies]
loom = "0.7"

[target.'cfg(shuttle)'.dependencies]
shuttle = "0.7"
```

Two obligations come with this:

- **Declare every model cfg to `check-cfg`.** Because the cfgs are set by
  `RUSTFLAGS` and unknown to Cargo, a normal build warns on them unless the crate
  teaches them:
  ```toml
  [lints.rust]
  unexpected_cfgs = { level = "warn", check-cfg = [
      "cfg(loom)", "cfg(shuttle)", "cfg(some_neg_control)"] }
  ```
  Every negative-control cfg (below) is declared here too.
- **They never reach the shipped binary.** A `cfg(loom)`/`cfg(shuttle)`
  dependency is compiled only under its flag, so the userspace cross-build and the
  aarch64 kernel build pull neither. This keeps the concurrency tier off the
  trusted dependency surface.

---

# Part B — technique distilled

## 1. The cfg seam — one module, three worlds

Factor the "which sync primitives" choice into a single seam so every model-aware
module imports from one place, and the production crate imports **none** of it.
The seam picks the instrumented primitive under each model cfg and the real one
otherwise:

```rust
// The concurrency seam: std by default, loom/shuttle under their cfgs.
// Compiled only for the model/harnesses (`#[cfg(any(test, loom, shuttle))]`
// at the crate root); the production no_std path uses none of it.
#[cfg(loom)]
pub use loom::sync::{Arc, Condvar, Mutex};
#[cfg(shuttle)]
pub use shuttle::sync::{Arc, Condvar, Mutex};
#[cfg(all(not(loom), not(shuttle)))]
pub use std::sync::{Arc, Condvar, Mutex};
```

One subtlety worth copying: re-export `thread` under a **separate `test` gate**.
A `ModelTransport`-style library that spawns nothing itself still needs
`thread::spawn` in its *harnesses*; gating the re-export on `test` keeps the
non-test library build from tripping an unused-import warning:

```rust
#[cfg(all(test, loom))]
pub use loom::thread;
#[cfg(all(test, shuttle))]
pub use shuttle::thread;
#[cfg(all(test, not(loom), not(shuttle)))]
pub use std::thread;
```

For a single atomic the seam is the same shape inline:

```rust
#[cfg(all(not(loom), not(shuttle)))]
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU32, Ordering};
#[cfg(shuttle)]
use shuttle::sync::atomic::{AtomicU32, Ordering};
```

## 2. The `const fn` split for statics

A model build's `AtomicU32::new` is **not `const`**. A primitive that lives in a
process-global `static` (a `.bss` lock the loader zeroes with the RW segment)
needs a `const fn new()` on the real build; keep that, and give the model builds a
plain `fn` with an identical body:

```rust
// Real build: const, so an all-zero `SpinLock` is the unlocked `.bss` state.
#[cfg(all(not(loom), not(shuttle)))]
pub const fn new() -> SpinLock {
    SpinLock { locked: AtomicU32::new(UNLOCKED) }
}

// Model builds: identical body, but `AtomicU32::new` is not const here.
#[cfg(any(loom, shuttle))]
pub fn new() -> SpinLock {
    SpinLock { locked: AtomicU32::new(UNLOCKED) }
}
```

## 3. The backoff must yield to the model scheduler

An acquire loop that pure-spins is **opaque** to the models: a raw `spin_loop`
blows Loom's branch budget (it looks like unbounded progress with no scheduling
point) and never preempts under Shuttle, so the contended interleaving is never
explored. The backoff therefore rides a four-way seam — yield to the model
scheduler under each model, issue the target's `Yield` syscall on hardware, and
keep the CPU hint only on a host non-model build:

```rust
#[cfg(loom)]
fn backoff() { loom::thread::yield_now(); }
#[cfg(shuttle)]
fn backoff() { shuttle::thread::yield_now(); }
#[cfg(all(not(loom), not(shuttle), bare_metal))]
fn backoff() { ipc::sys::yield_now(); }   // rev2§5.4 priority-inversion mitigation
#[cfg(all(not(loom), not(shuttle), not(bare_metal)))]
fn backoff() { core::hint::spin_loop(); }

pub fn lock(&self) -> Guard<'_> {
    while self
        .locked
        .compare_exchange_weak(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        backoff();
    }
    Guard { lock: self }
}
```

The ordering pairing here is the fence the Loom model certifies: `Acquire` on the
winning CAS pairs with the `Release` store in `Guard::drop`, so the critical
section cannot leak past either edge. The failure ordering is `Relaxed` — a failed
CAS synchronizes with nothing.

## 4. Instrumented cells — Loom has one, Shuttle does not

Loom ships an instrumented `loom::cell::UnsafeCell` whose `.with`/`.with_mut`
accessors are a *data-race oracle*: it flags an unsynchronized access directly,
independent of the invariant assertion. Use it in a Loom model so a broken lock
fails two ways. Shuttle has no such cell, so hand-roll the minimal `Sync` wrapper:

```rust
// Loom model: the instrumented cell double-checks the race.
let data = loom::sync::Arc::new(loom::cell::UnsafeCell::new(0u32));
// ... under the guard:
data.with_mut(|p| unsafe { *p += 1 });   // SAFETY: guard is sole live accessor

// Shuttle twin: no instrumented cell, so wrap a plain one.
struct Cell(core::cell::UnsafeCell<u32>);
// SAFETY: exclusivity is provided by the primitive under test.
unsafe impl Sync for Cell {}
```

## 5. Author the model with a tiny state space

Loom's cost is exponential in the schedule, so the model must witness the defect
with the **fewest** threads and steps that can express it. The invariants these
protocols rest on are *per-critical-section*, not cumulative, so one contended
episode is the whole proof: **two threads, one shared object, one write**. A
seqlock torn-read model in full — one writer bumping to a single new epoch, one
reader sampling, the invariant `b == 2·a` broken by any torn mix:

```rust
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    #[test]
    fn no_torn_sample_under_any_interleaving() {
        loom::model(|| {
            // Initial epoch: (a, b) = (0, 0), invariant b == 2·a holds.
            let page = Arc::new(TimePage::new(0, 0));

            let writer = {
                let page = Arc::clone(&page);
                thread::spawn(move || {
                    // One seqlock write to the next epoch: (a, b) = (1, 2).
                    page.seq.fetch_add(1, Ordering::Relaxed);   // odd: writer in
                    fence(Ordering::Release);
                    page.a.store(1, Ordering::Relaxed);
                    page.b.store(2, Ordering::Relaxed);
                    page.seq.fetch_add(1, Ordering::Release);   // even: writer out
                })
            };

            // Both epochs (0,0) and (1,2) satisfy b == 2·a; any torn mix breaks it.
            let s = page.sample();   // reader: fence(Acquire) between the seq reads
            assert_eq!(s.b, 2 * s.a, "torn sample: {s:?}");

            writer.join().unwrap();
        });
    }
}
```

Bounding is a *correctness* discipline, not only a speed one: never widen a model
to the point that it no longer terminates (below). Widen coverage by adding a
*second small model* for a distinct interleaving, not by growing one model's
thread count.

## 6. The honest-scope docstring is mandatory

Every model states, in its own words, exactly what it proves — because the
tool's guarantee is narrower than "correct". A Loom model over a fenced structure
must record that it holds *via the fence*, not over every reordering; the Shuttle
twin must record that it *cannot* witness the weak-memory class it looks like it
covers:

```text
/// Loom proof of the seqlock: under every interleaving of one writer's
/// odd→stagger→even update and one reader's sample() — enumerated *around the
/// explicit Acquire/Release fence* — the reader never observes a torn triple.
///
/// Honest scope. NOT a proof over every C11-permitted reordering: Loom does not
/// faithfully model Relaxed reordering, and the data fields are Relaxed. The
/// seqlock's correctness rests on the fence(Release)/fence(Acquire) pair, which
/// is exactly what Loom models — so the conclusion holds VIA THE FENCE.
```

```text
/// Shuttle breadth-smoke of the same model. NON-CERTIFYING: Shuttle models only
/// SeqCst and reinterprets the Relaxed/Acquire/Release as SeqCst (it warns once),
/// so it CANNOT witness a torn read. A randomized-scheduler sanity pass; the Loom
/// model is the proof of record.
```

## 7. The negative control — teeth

Per the `verification.md` teeth principle, a model earns its keep only when a
**deliberately broken variant is confirmed to fail**. A green model whose broken
twin also passes proves nothing. Two patterns are in use:

- **A compiled negative-control cfg.** Introduce a `--cfg <name>_neg_control`
  that swaps in a broken operation (e.g. the wait's word-check hoisted *outside*
  the bucket lock, reopening the lost-wakeup window), selected by a nested `cfg`
  inside the harness so the same model body runs both ways:
  ```rust
  let waiter = thread::spawn(move || {
      #[cfg(not(park_neg_control))]
      table.wait_block(&word, 0);
      #[cfg(park_neg_control)]
      table.wait_block_neg(&word, 0);   // word-check outside the lock: racy
      assert_eq!(word.load(Ordering::Relaxed), 1,
                 "woke but the word was never set (lost wakeup)");
  });
  ```
  Then confirm the control actually fails, and keep the command near the model:
  ```sh
  # Must DEADLOCK / fail — the harness has teeth.
  RUSTFLAGS="--cfg loom --cfg park_neg_control" cargo test -p urt --lib
  ```
- **A run-manually red control.** For a lock model, deleting the `lock()` call
  makes both the invariant assertion *and* Loom's instrumented-cell race detector
  fire. Documented in the model's docstring as the control to run by hand when the
  proof is edited.

## 8. Module gating tiers — which modules compile under a model build

A module's presence under `--cfg loom`/`--cfg shuttle` is not uniform; it depends
on what the module needs and whether it carries its own model. Three tiers recur:

| Tier | Gate | When |
|---|---|---|
| Fully portable | *(ungated)* | A pure primitive (the spinlock) that compiles everywhere, including both model builds — it *is* the thing being modeled. |
| Off the model builds | `#[cfg(not(any(loom, shuttle)))]` | A module holding a process-global `static` over the *const* `new()` those builds drop, with **no interleaving model of its own** because it only reuses an already-modeled primitive. |
| Target-or-model only | `#[cfg(any(test, bare_metal))]` | A module whose primitive is absent on the plain no_std host — e.g. a parker that is a kernel notification on target and a `Mutex`+`Condvar` under the models — so it exists for the target and the harnesses but nowhere else. |

Match the gate to the reason, and record the reason: the middle tier is *narrower*
than the portable primitive (the model can't construct its const static) and
*wider* than the target-or-model module (it still builds for plain host tests,
Miri, and Verus).

## 9. Reusing a certified primitive adds no new obligation

Once a primitive is Loom-certified, a module that only *uses* it — wrapping some
state behind that lock and asserting `unsafe impl Sync` — **inherits** the
certification and gets no model of its own. Adding a redundant Loom model for the
wrapper would only re-prove the primitive. State this explicitly so the wrapper is
not mistaken for an un-checked concurrency site: its data-race oracle is **Miri**
(over the plain host build), not a second interleaving model. A new Loom
obligation appears only when a module introduces a *new* concurrent protocol
(a new atomic handshake), not when it reuses one.

## 10. Shuttle reproducibility and the replay corpus

Loom is exhaustive, so it needs no seed — a green run covers the whole (bounded)
schedule space. Shuttle is randomized, so pin its seed and iteration count; a
bare `check_random` reseeds from entropy and a failure reproduces only from the
schedule that run happened to print. A seeded runner makes every CI run explore
the same schedules and a failure reproduce from source:

```rust
#[cfg(shuttle)]
const SHUTTLE_SEED: u64 = 0x1C_5EED;
#[cfg(shuttle)]
const SHUTTLE_ITERS: usize = 1000;
#[cfg(shuttle)]
fn check_pinned<F: Fn() + Send + Sync + 'static>(f: F) {
    use shuttle::scheduler::RandomScheduler;
    use shuttle::Runner;
    let scheduler = RandomScheduler::new_from_seed(SHUTTLE_SEED, SHUTTLE_ITERS);
    Runner::new(scheduler, Default::default()).run(f);
}
```

Treat `SHUTTLE_ITERS` like a pinned tool version — bump it *deliberately* to widen
coverage. Alongside the seed, keep a **replay corpus**: the fuzz-corpus discipline
applied to interleavings. When Shuttle finds a failing schedule it prints an
encoded replay string; paste it as a `(harness, schedule)` entry and it becomes a
deterministic regression pinning that exact interleaving, independent of the seed:

```rust
#[cfg(shuttle)]
#[test]
fn shuttle_replay_corpus() {
    // Empty until the first bug — the designated place for a pinned schedule to
    // land. The empty slice keeps the `shuttle::replay` plumbing type-checked.
    let corpus: &[(fn(), &str)] = &[
        // ((|| reactor_no_lost_wakeup()) as fn(), "…encoded schedule…"),
    ];
    for &(harness, schedule) in corpus {
        shuttle::replay(harness, schedule);
    }
}
```

## 11. Bound the search — the runaway guard

A Loom model that loses its bound — an unbounded spin with no scheduling point, or
a thread/step count that grows the schedule space past what terminates — does not
fail fast; it **explodes** the branch search and hangs. The CI wall-clock cap is
*margin* to catch such a runaway, not a lever to accommodate a too-large model.
When a model is slow or unbounded, bound it at the source:

- keep the state minimal (§5) — two threads, one write;
- cap the exploration with `LOOM_MAX_PREEMPTIONS` rather than by raising the
  timeout;
- ensure the acquire loops yield to the scheduler (§3) so a contended spin is a
  finite set of scheduling points, not an infinite regress.

Scope deliberately across the pair, too: **not every harness runs on both tools**.
A FIFO / no-double-delivery property is a pure interleaving check with no
weak-memory content — Shuttle's tier, with no Loom variant. A lost-wakeup fragment
is the weak-memory poll-then-wait sequence — Loom's tier, at a one-message bound.
Give each harness the tool whose guarantee its claim actually needs.

---

*This guideline distills the technique; the live code is authoritative and this
note is code-independent by design — it is not updated for every refactor. The
tier assignment is rev2§6; the problem-shape → tool routing is `verification.md`;
and the SeqCst-only pin that sends the fence-ordered seqlock shape here rather
than to a deductive proof is `verus.md` §15.5. When a snippet here and the live
code disagree, the code wins.*
