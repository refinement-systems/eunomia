# Findings — Phase 3.3: `sys::futex` backend + upstream locks

Task 3.3 of `doc/plans/2_plan-std-revised.md` (findings **#10**). Lights the whole
std sync stack — `Mutex`/`Condvar`/`RwLock`/`Once`/`Parker` — by supplying the one
primitive upstream needs, `sys::futex`, emulated in userspace over kernel
notifications and reusing the 3.2 yielding spinlock. We wrote **zero lock logic**;
upstream's already-correct futex impls come free. Proven live: `run bin/stdsmoke sync`
→ `STD33 PASS` (two std threads ping-pong a counter under a real `Mutex`+`Condvar`,
`scripts/std-smoke-test.sh`, wired into the CI `on-os` job).

## Decisions (and the alternatives)

- **The futex table lives in `urt` (`urt::futex`); the std-facing glue lives in
  `eunomia-sys`.** The table reuses `urt::lock::SpinLock` (the 3.2 bucket lock,
  already Loom/Shuttle-certified) and `urt`'s existing Loom/Shuttle harness + Cargo
  deps — exactly as `urt::thread`/`urt::lock` do — so the CI `concurrency` job (`-p
  urt`) covers the new model unchanged. `eunomia-sys/src/futex.rs` is a thin bridge
  (like `eunomia-sys/src/thread.rs` over `urt::thread`) and `pal.rs` exposes the
  `__eunomia_futex_*` seam symbols. *Rejected:* the whole table in `eunomia-sys` —
  it is a `no_std` verified crate without the Loom/Shuttle deps, and the SpinLock
  lives in `urt`.
- **Forced correction to the plan text.** The plan (`:146`/`:356`) described the
  eunomia PAL arm as motor's one-line `pub use eunomia_sys::futex;`. That is **not
  achievable** — per finding 7-2, `eunomia-sys` cannot be a `rustc-dep-of-std`
  sysroot crate (vstd/`verus_builtin`). So the arm is a real
  `sys/pal/eunomia/futex.rs` calling `__eunomia_futex_*` `extern "Rust"` symbols —
  the established `sys/thread/eunomia.rs` pattern. No choice; resolved during
  planning.
- **Per-thread park notification, per-pool-slot (not per-instance).** The kernel
  notif delivers its whole ORed word to *one* FIFO waiter and clears it, so a shared
  notif cannot wake a *specific* enqueued waiter — each waiter parks on its **own**
  notif. It is carved per thread-pool slot (reused across spawn/join), **not** per
  spawn: a per-instance notif would leak a cspace slot + untyped every spawn/join
  cycle. `SLOTS_PER_THREAD` 5→6 (`park_notif` at `base+5`), `WORKING_SLOTS` →
  `6*16+1 = 97` (the `+1` = the main thread's own park-notif, carved lazily from the
  thread-untyped), `THREAD_UNTYPED_BYTES` → `PER_THREAD_BYTES*(MAX_THREADS+1)`. All
  ceilings hold (`97 ≤ SlotAlloc<2>` cap 128; child working range `4..101 ≤ 128`);
  producer side is comment-only (the consts auto-propagate).
- **A running thread finds its own park-notif by SP→slot mapping** (`slot_of_sp`,
  the inverse of the host-tested `stack_region`), not by threading a slot index
  through `ThreadInit` — self-contained in `urt`, no std-seam plumbing, and reuses
  the already-load-bearing stack geometry. The main thread's SP lies above
  `STACK_TOP`, outside every pool region → resolves to the main notif.
- **Timed `futex_wait(Some(d))` = yield-poll MVP** (the process holds no timer cap).
  The only two timeout callers (condvar `wait_timeout`, `park_timeout`) both mutate
  the futex word *before* the paired wake, so a non-enqueued waiter that polls
  `load() != expected` against a `now_mono_ns` deadline is correct (confirmed by
  reading the two consumers). Untimed waits block on the notif (no busy-wait).
  Disclosed; timer-bit blocking is deferred.
- **No new TLA spec; a new Loom/Shuttle negative control instead.** The futex's
  recheck-word-under-lock-before-blocking over an accumulate-and-clear notif is a
  refinement of the same no-lost-wakeup class `tla/ipc_reactor` already model-checks,
  so it is reused for design-level corroboration (the plan's stated reuse). But the
  reactor's 3 controls exercise the IPC poll-once / on-writable / ack paths, **not**
  the futex recheck path, so the concrete teeth are a **new** `--cfg
  futex_neg_control` variant (word-check moved outside the bucket lock) that
  deadlocks under Loom/Shuttle. Confirmed: it aborts under `--cfg loom`.
- **The kernel untyped watermark aligns to `{16, 4096}` (page), not 128 KiB.**
  Verified against `kcore/src/untyped.rs` `carve_place` — so the main park-notif can
  be carved lazily in any order (after some pool sub-untypeds) with only sub-page
  waste. The planning pass had over-worried a 128 KiB-alignment fragility; the `+1`
  block (128 KiB for one tiny notif) is comfortably sufficient.

## What shipped

- **`urt` — the table + provisioning.** New `urt/src/futex.rs`: a `.bss` bucket
  `FutexTable` (`SpinLock` + `UnsafeCell<[Entry; MAX_THREADS+1]>`, the `urt::thread`
  idiom), `futex_wait`/`futex_wake`/`futex_wake_all`, a cfg-selected park seam
  (target: `ipc::sys::notif_{wait,signal}` + `current_park_notif`; model: an
  `Arc<Parker{word: Mutex<u32>, cv: Condvar}>` — the `ipc::model::ModelTransport`
  shape), and the `futex_no_lost_wakeup` Loom/Shuttle/std triad + the negative
  control. `urt/src/thread.rs`: `Slot.park_notif` (retyped per spawn from the slot
  sub-untyped), a lazily-carved `Inner.main_park`, `current_park_notif()` (SP→slot),
  the `THREAD_UNTYPED_BYTES` bump. `urt/src/thread_layout.rs`: `SLOTS_PER_THREAD`/
  `WORKING_SLOTS` bump + `slot_of_sp` + host tests. `lib.rs`: `pub mod futex` gated
  `any(test, <target>)` (not fully portable like `lock` — the park primitive is
  either the notif or a std/loom/shuttle parker). Cargo `check-cfg` += `futex_neg_control`.
- **`eunomia-sys` — the bridge.** New `src/futex.rs` (delegates to `urt::futex`),
  `pub mod futex` in `lib.rs`, and three `#[no_mangle] __eunomia_futex_{wait,wake,
  wake_all}` shims in `pal.rs`.
- **Vendored std.** New `sys/pal/eunomia/futex.rs` (`Futex`/`SmallFutex`/`Primitive`/
  `SmallPrimitive` + the three fns via the `__eunomia_futex_*` externs, marshalling
  `Option<Duration>`→nanos with the motor `None`/overflow → `u64::MAX` convention);
  `pub mod futex` in `sys/pal/eunomia/mod.rs`; `target_os = "eunomia"` added beside
  `motor` in the futex `cfg_select!` arm of all five dispatchers
  (`sys/sync/{mutex,condvar,rwlock,once,thread_parking}/mod.rs`). No `sys/futex/mod.rs`
  exists and no sync `futex.rs` was edited.
- **Producer + fixture.** `user/shell/src/runtime.rs` comment-only (the bumped consts
  auto-propagate; `THREAD_CHILD_CSPACE_SLOTS=128`/`DONATION_BYTES=16 MiB` already
  suffice). `user/stdsmoke/src/main.rs` `sync` arm (two threads ping-pong a counter
  under `Mutex`+`Condvar`, `STD33 PASS`). `scripts/std-smoke-test.sh` extended.

## Problems hit and how they were solved

- **The futex module must compile on the no_std host build (verus, plain `cargo
  build`) but has no valid park primitive there.** Solved by gating `pub mod futex`
  to `any(test, <target>)` — the harness needs it under `test`/loom/shuttle, the real
  build under `<target>`, and it is absent (nothing references it) elsewhere. This is
  why it is *not* `unconditional like lock` as the plan sketched — `lock` is fully
  portable, `futex`'s park is not.
- **A `wake_all` two-waiter test with a "drain until empty" waker loses a wakeup.**
  The first draft looped `wake_all` then `dequeue_one` to "confirm both parked" — but
  `dequeue_one` removes a waiter *without* signaling it. Corrected to the real waker
  shape: a single `store; wake_all`. A waiter enqueued by then is drained+signaled;
  one that enqueues later locks the bucket after `wake_all` released it, so its
  under-lock word-check sees the store and it returns without parking. No loop, no
  lost wakeup.
- **Pre-existing verusfmt gate red on `main`.** `scripts/verusfmt.sh --check` flagged
  `eunomia-sys/src/tls.rs` and `urt/src/thread.rs` — both mention `` `verus!{}` ``
  only in a doc comment (no real block), so `git grep -l 'verus!'` selects them and
  verusfmt reformats their plain-Rust layout against `cargo fmt`. This is the exact
  trait the SKIP-list header documents (like `bootstrap.rs`/`io_error.rs`); confirmed
  pre-existing on pristine `HEAD`. Applied the documented remedy: added both to the
  script's SKIP list.

## Verification record

Toolchain `nightly-2026-06-26`; Verus binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Verus (authoritative, cold).**
  - `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` → **7 verified,
    0 errors** (unchanged; the new `futex.rs`/`pal.rs` shims carry no `verus!{}`). ipc
    71, loader 29 re-verified transitively.
  - `cargo clean -p urt && cargo verus verify -p urt` → **25 verified, 0 errors**
    (unchanged; `futex`/`thread`/`thread_layout` carry no `verus!{}`). freelist 30.
- **Loom (certifying) / Shuttle (breadth).** `RUSTFLAGS="--cfg loom" cargo test -p
  urt --lib` and `--cfg shuttle` → the full urt suite green, incl.
  `futex::tests::futex_no_lost_wakeup_{loom,shuttle}` alongside the pre-existing
  lock/time models. The negative control `RUSTFLAGS="--cfg loom --cfg
  futex_neg_control"` **fails (deadlock detected)** — the harness has teeth.
- **Host tests.** `cargo test -p urt` → **32 passed** (incl. `futex_no_lost_wakeup_std`,
  `wake_all_wakes_every_waiter`, and the `slot_of_sp` inverse tests).
- **urt Miri.** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run
  -p urt -j4` → **32 tests, 32 passed** (arena soundness under the lock unchanged; the
  new futex host tests run under Miri's thread model).
- **On-target (QEMU) — the gate.** `scripts/std-smoke-test.sh` → **`STD SMOKE TEST
  PASS`** with the pre-existing `STD2`/`STD32 PASS` and the new **`STD33 PASS`** (two
  std threads block/wake on a real `Mutex`+`Condvar` over `sys::futex`), no
  panic/fault. Machine `virt,gic-version=3 -cpu cortex-a72`.
- **Formatting.** `cargo fmt --check` clean (root + `user/stdsmoke` + `user/shell`);
  `scripts/verusfmt.sh --check` clean (after the SKIP-list fix above).

## Surface left trusted / unsupported (and why)

- **The address→waiter dispatch + the bucket lock → Loom/Shuttle-certifying, never
  Verus.** An `Acquire`/`Release` protocol over a notification; the version-pinned
  Verus ghost atomics are SeqCst-only, so a proof would certify a different binary
  (`doc/guidelines/verification.md`). Folding note, not a new seam — same category as
  the 3.2 heap spinlock it reuses.
- **The per-thread park-notif + the notif-based park/unpark asm-adjacent shell** —
  object provisioning over the verified `kcore::notification` + untyped retype (the
  join-notif precedent), plus the `svc`-based `notif_wait`/`notif_signal` (the
  trusted syscall shell, rev2§6.1(d)). No new seam; extends the 3.2 thread-lifecycle
  routing note.
- **§11 inverse-leak (the new PAL arm).** `sys/pal/eunomia/futex.rs`, `eunomia_sys::
  futex`, and `urt::futex` add zero logic vs `pal/unsupported`: `futex_wait` degrades
  to a yield-poll (never a bogus syscall) when the park-notif is unavailable, and
  `current_park_notif` clamps to the configured slot allocator. The timeout marshal
  reserves `u64::MAX` as the "no timeout" sentinel and re-maps a saturating finite
  timeout off it.
- **Disclosed MVP bounds.** `futex_wait(Some(d))` is a busy yield-poll (no timer cap);
  the per-thread park-notif is leaked with its pool slot on the lifetime-bounded
  spawn count (as the join notif); the bucket lock's cross-priority-level caveat
  (rev2§5.4) is bounded to the tiny check-and-enqueue hold — the long wait blocks in
  the kernel via `notif_wait`.

## Follow-ups

- **Timer-bit blocking `futex_wait(Some(d))`** (replace the yield-poll — needs a
  per-thread timer cap; the deferred-work item in the plan).
- **A kernel futex / wait-set object (rev2§8.3)** — an internal backend swap inside
  `urt::futex`/`eunomia_sys::futex`, invisible to std; also the real fix for the
  cross-priority-level bucket-lock boundary.
- **A committed Shuttle replay-corpus entry** for `futex_no_lost_wakeup` (the
  `ipc::model` `shuttle_replay_corpus` pattern), once a schedule is worth pinning.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

- Added the **Futex-backend routing note (std-port 3.3)** after the 3.2 heap-spinlock
  note: the address→waiter dispatch + bucket lock is a Loom/Shuttle folding note
  (host test = the `urt::futex` model + the QEMU `STD33` smoke), same category as the
  3.2 heap spinlock; the per-thread park-notif is object provisioning over the
  verified notification/retype (the join-notif precedent); the std futex arm + bridge
  are thin delegation. No `external_body`, no new seam.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is
not referenced from code, specs, or guidelines.
