# Findings — Phase 3.2: in-process `std::thread` (spawn/join/yield/sleep) + heap lock

Task 3.2 of `doc/plans/2_plan-std-revised.md` (findings **#9**). Lights up real
threading for the std port: `std::thread::spawn`/`join`/`yield_now`/`sleep` work in a
thread-capable process, two threads allocate concurrently on the one process heap
(serialized by a new Loom-certified spinlock), each with real per-thread TLS. Proven
live: `run bin/stdsmoke spawn` → `STD32 PASS` (`scripts/std-smoke-test.sh`, wired into
the CI `on-os` job).

Two facts verified against the tree contradicted the plan's written mechanism and were
resolved with the user before implementation; a third surfaced mid-implementation.

## Decisions (and the alternatives)

- **Q1 = C — `ThreadStartAs` (op 18) gains a 7th arg in `x6`.** The plan said the
  closure crosses via `x0` using `ThreadStart` (op 13). It can't: a fresh TCB started
  by `ThreadStart` has `aspace = None`, which `activate_aspace` resolves to the
  **kernel identity map** (`kernel/src/thread.rs:189-197`, `kcore/src/thread.rs`
  `Tcb::empty`) — the process heap is unmapped there. Sharing the aspace needs
  `ThreadStartAs`, whose six decoded args (`a[0..5]`) are all spent and which carried
  no arg. The hardware `x6` register is free (the kernel `dispatch` read only
  `x[0..5]`+`x[7]`), so the fix widens the arg vector 6→7 for op 18: the closure
  crosses in `x0` and the std arm mirrors `motor.rs` line-for-line — **no naked
  trampoline, no stack-slot handoff.** This is a **kernel-track + verified re-proof**
  change (`kcore::sysabi::decode` and `eunomia-sys::encode` widened), so 3.2 is a
  *second* kernel-track touch (the plan's "3.1 is the one genuine kernel-track touch",
  `:384`, is superseded — corrected in the plan). *Rejected:* a stack-slot naked-stub
  handoff (userspace-only, no kernel change) — the user chose the cleaner motor-faithful
  register path over avoiding the small verified re-proof; and a spawn-lock + global-slot
  notification handshake (adds a serializing concurrent protocol, its own Loom obligation).
- **Q2 = 2 — scoped/opt-in thread-capability (rev2§2.3 least-authority).** A std thread
  needs the process to hold caps to its own aspace (WRITE, to map thread stacks — `map`
  requires `Rights::WRITE`), its own cspace (to name in `ThreadStartAs`), and an untyped
  to retype the per-thread objects — none provisioned today. Only a **marked** binary is
  provisioned; every other keeps the minimal footprint. *Rejected:* uniform provisioning
  (every child gets self-remap + retype authority) — simpler but a blanket authority bump;
  and a loader-pre-carved fixed thread pool (tightest authority but a bigger loader change
  and fixed thread count). The **marker** is an MVP shell-side allowlist
  (`THREAD_CAPABLE` in `user/shell/src/runtime.rs`) — the plan's sanctioned fallback,
  since `loader::elf::parse` (verified) extracts only PT_LOAD, so an ELF-note marker that
  travels in the binary is a noted upgrade that avoids touching the verified parser.
- **Per-thread TLS pulled forward from 3.5 (user-approved mid-task).** Discovered on the
  first live run: `std::thread::spawn` → `ThreadInit::init` → `set_current`, which needs
  **per-thread** storage for the current-thread handle/id. eunomia's `thread_local` routed
  to `no_threads` (process-*global*), so the second thread's `set_current` found `CURRENT`
  already set → `fatal runtime error: current thread handle already set during thread
  spawn`. This is 3.5's planned work, but 3.2's gate needs it. The user chose to pull the
  **minimal** slice forward: a `TPIDR_EL0`-based `sys/thread_local/eunomia.rs`
  `local_pointer!` (per-`local_pointer!` site claims a slot index at first access; every
  thread reads `[TPIDR + slot]`), + block setup in `_start` (main) and the `sys/thread`
  trampoline (spawned). The `thread_local!` *macro* storage stays single-threaded
  `no_threads` (no user `thread_local!` runs multi-threaded yet — `HashMap`'s is 3.4); the
  verified `urt` key table + destructors remain **3.5**.
- **`sleep` = yield-poll MVP** (not `TimerArm`, which needs a timer cap the process
  doesn't hold): `urt::time::now_mono_ns` + `yield_now`. Disclosed busy-wait; timer-bit
  blocking is a follow-up.
- **Spawned-thread priority = a fixed low `THREAD_PRIO = 1`** (MVP). Must be `<=` the
  process's own priority (the rev2§5.4 ceiling stamped at retype); threads time-slice
  among themselves and run when the main thread blocks (e.g. at `join`). Same-level
  time-slicing with the main thread (passing the process's actual priority) is a follow-up.

## What shipped

- **Kernel-track (Q1 = C).** `kcore/src/sysabi.rs` `decode(nr, a: [u64;6]) → [u64;7]`,
  `Sys::ThreadStartAs` gains `arg: a[6]` (verified, count-neutral). `kernel/src/syscall.rs`
  `dispatch` reads `x[6]`; the op-18 execute arm sets `frame.x[0] = arg`.
  `eunomia-sys/src/encode.rs` (verified) `Encoded` gains `a6`, `Call::ThreadStartAs` gains
  `arg`, placed in `a6` — the faithful `decode`-inverse preserved (the "field-for-field
  mirror"), pinned by the host round-trip oracle. `eunomia-sys/src/syscall.rs` +
  `ipc/src/sys.rs` marshal `x6` (`ipc` via a dedicated `syscall7` used only by op 18;
  `loader::spawn::start` passes `arg = 0` — a process main thread gets argv via the
  startup block, not `x0`).
- **Heap lock.** New `urt/src/lock.rs` — a yielding `SpinLock` (raw `AtomicU32`,
  Acquire/Release), cfg-selected atomic seam + a 4-way backoff seam (loom/shuttle yield;
  target `Yield` syscall; host `spin_loop`), const `new()` on non-model builds (preserves
  the `.bss` static). Integrated into `urt::Heap` (`lib.rs`): a `lock` field guards the
  `fl` critical section in `alloc`/`dealloc`; the `unsafe impl Sync` justification rewrote
  from "no concurrent access by construction" to "mutual exclusion by the heap spinlock".
  Disclosures updated (`sys/alloc/eunomia.rs`, `eunomia-sys/pal.rs`).
- **urt thread primitive.** New `urt/src/thread.rs` (`configure`/`spawn`/`join`/
  `yield_now`/`sleep`) + `urt/src/thread_layout.rs` (pure, host-tested stack geometry). A
  reuse pool of `MAX_THREADS = 16` slots, each lazily carving a persistent per-thread
  sub-untyped; stacks at fixed VAs below the main stack within one L3 table (no topup);
  bind-before-start / read-report-before-revoke (mirroring `urt::spawn`).
- **eunomia-sys bridge + grants.** New `eunomia-sys/src/thread.rs` (delegates to
  `urt::thread`) + `tls.rs` (TPIDR block mgmt). `pal.rs` `__eunomia_thread_{spawn,join,
  yield,sleep}` + `__eunomia_tls_init_{main,thread}` shims. `grant.rs` `thread_caps`
  resolver; `bootstrap.rs` `configure_threads` + `init_main` TLS. `loader/src/startup.rs`
  four new `NAME_*` CapSlot ids (no codec change).
- **Vendored std.** New `sys/thread/eunomia.rs` (Thread/join/yield/sleep, plain
  `extern "C" fn(arg)` trampoline) + `sys/thread_local/eunomia.rs` (TPIDR `local_pointer!`);
  `sys/thread/mod.rs` + `sys/thread_local/mod.rs` route eunomia; `sys/pal/eunomia/mod.rs`
  `_start` inits main TLS first.
- **Producer + fixture.** `user/shell/src/runtime.rs` scoped provisioning (self-cap
  installs, `CHILD_CSPACE_SLOTS` 8→128, `DONATION_BYTES` 4→16 MiB) + `main.rs`
  `build_child_block` grants. `user/stdsmoke/src/main.rs` `spawn` arm (2 threads,
  concurrent alloc, distinct-TLS-id check). `scripts/std-smoke-test.sh` extended.

## Problems hit and how they were solved

- **`ThreadStart` runs in the identity map, not the parent aspace.** The crux above; the
  fix is Q1 = C (`ThreadStartAs` + `x6`). Verified `activate_aspace(None) → kernel_ttbr0()`
  and that ThreadStartAs already spends `a[0..5]`, so `x6` (unused by any opcode) is the
  vehicle.
- **`set_current` aborts without per-thread TLS.** Confirmed empirically (`fatal runtime
  error: current thread handle already set`); solved by pulling minimal TPIDR-based TLS
  forward (user-approved). Distinct thread ids in the smoke now witness the storage is
  genuinely per-thread.
- **build-std cache staleness.** After adding the `sys/thread`/`sys/thread_local` arms, the
  first live run still hit `unsupported.rs` (`Unsupported`): the inner build-std reused its
  cached std rlib despite `rerun-if-changed=vendor/rust/library/std/src` re-running
  `build.rs`. Fixed by `rm -rf target/user && cargo clean -p kernel` to force a fresh
  build-std. **Recorded for the runbook: a vendored-std edit needs a `target/user` wipe to
  take effect reliably.**
- **Priority ceiling.** `set_priority` accepts iff `prio <= ceiling` (= the process's own
  priority at retype); a spawned thread runs at a fixed low `THREAD_PRIO = 1`.

## Verification record

Toolchain `nightly-2026-06-26`; Verus binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Verus (authoritative, cold).**
  - `cargo clean -p kcore && cargo verus verify -p kcore` → **407 verified, 0 errors**
    (unchanged — the `[u64;6]→[u64;7]` + passthrough field add no obligation).
  - `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` → **7 verified, 0
    errors** (unchanged; the new `thread`/`tls` modules carry no `verus!{}`). ipc 71,
    loader 29 re-verified transitively.
  - `cargo verus verify -p urt` → **25 verified, 0 errors** (unchanged; `lock`/`thread`/
    `thread_layout` carry no `verus!{}`).
  - Host round-trip oracle `encode_round_trips_through_kernel_decode` green — the
    cross-side agreement guard for the widened `ThreadStartAs`.
- **Perf (rlimit, deterministic, cold).** kcore `times-ms.smt.rlimit-run` before (git
  stash) `147,029,752` → after `147,033,115` = **+3,363 (+0.00%)** — the widening is
  rlimit-neutral (measured per `doc/guidelines/verus.md` §10).
- **Loom (certifying) / Shuttle (breadth).** `RUSTFLAGS="--cfg loom" cargo test -p urt
  --lib` and `--cfg shuttle` → the heap-lock mutual-exclusion model green (`lock::loom_tests`
  / `lock::shuttle_tests`), alongside the pre-existing seqlock models.
- **urt Miri.** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p urt
  -j4` → **28 tests, 28 passed** (the heap-arena proptests are sound under the lock).
- **Host tests.** `eunomia-sys` round-trip/grant + `urt::thread_layout` (stack-VA
  non-overlap / guard / L3 budget / 16-alignment) + `user/shell` `build_child_block_emits_
  thread_grants` — all green.
- **On-target (QEMU) — the gate.** `scripts/std-smoke-test.sh` → **`STD SMOKE TEST PASS`**
  with `STD2 PASS` (single-threaded, no regression), **`STD32 PASS`** (two threads
  spawned/joined, concurrent heap alloc under the lock, distinct per-thread TLS ids), and
  the std panic reaped as `STATUS_PANIC`. Machine `virt,gic-version=3 -cpu cortex-a72`.
- **Formatting.** `cargo fmt --check` clean; `scripts/verusfmt.sh --check` clean (it
  reformatted `encode.rs`; eunomia-sys re-verified 7/0 over the formatted tree).

## Surface left trusted / unsupported (and why)

- **The heap spinlock + its concurrent-alloc use → Loom-certifying, never Verus.** An
  Acquire/Release protocol; the version-pinned Verus ghost atomics are SeqCst-only, so a
  proof would certify a different binary (`doc/guidelines/verification.md`). Same tier as
  the 3.3 futex bucket lock, which reuses this primitive. Folding note, not a new seam.
- **The `TPIDR_EL0` TLS asm shell (`eunomia-sys::tls` `msr`, `sys/thread_local::eunomia`
  `mrs`) + the thread trampoline / `svc`-based thread primitive** — the userspace mirror
  of the trusted register marshalling (rev2§6.1(d)); it extends the 3.1 TPIDR
  thread-lifecycle routing note. No new seam. On-target witnesses: the QEMU spawn smoke +
  the distinct-TLS-id check.
- **`ThreadStartAs`'s `arg` stays verified, not trusted:** the widened `decode`/`encode`
  are re-proven (count- and rlimit-neutral), adding no `external_body`.
- **§11 inverse-leak (the new PAL arms).** `sys/thread/eunomia.rs` and
  `eunomia_sys::thread` re-establish every `requires` at the boundary — `spawn` returns
  `Err` when unconfigured or over `MAX_THREADS`/stack-size rather than pushing an
  out-of-range value into a syscall; `configure` clamps the slot count to `SlotAlloc`'s
  `WORDS*64` cap; `from_raw_os_error` re-surfaces the negative `ERR_*` faithfully. Both
  arms add zero new logic vs `pal/unsupported`.
- **Disclosed MVP bounds.** `MAX_THREADS = 16` (stack-VA budget within one L3 table);
  per-thread stack fixed at `STACK_PAGES*PAGE` = 64 KiB (a larger `Builder::stack_size` is
  refused); the per-thread sub-untyped is **not reclaimed to the process untyped** on
  join (bounded by lifetime spawn count, like `urt::spawn`); the per-thread **TLS block is
  leaked** on thread exit (bounded); spawned threads run at a fixed low priority; `sleep`
  is a busy yield-poll; the thread-capable **marker is a shell allowlist**.

## Follow-ups (and what 3.5 now is)

- **3.5 shrinks to hardening:** the verified `urt` TLS key table over `SlotAlloc`, TLS
  destructors (drop_current on thread exit), and `thread_local!`-macro storage (for
  `HashMap`'s `RandomState`, 3.4). The `local_pointer!` current-thread/id path is done here.
- **TLS-block free on thread exit** (currently leaked); per-thread untyped reclaim on join
  (currently lifetime-bounded).
- **Same-priority scheduling** (pass the process priority so threads time-slice with main);
  **timer-bit blocking `sleep`** (needs a timer cap); the **ELF-note thread-capable marker**
  (replacing the allowlist).
- **Runbook note:** a `vendor/rust` std edit needs a `target/user` wipe for build-std to
  pick it up.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

- Heap-spinlock folding note (Loom-certifying, host test = the `urt::lock` model + the QEMU
  simultaneous-alloc smoke) — same category as the 3.3 futex lock.
- The TPIDR-TLS asm shell folds under the existing 3.1 thread-lifecycle / asm-context-switch
  routing note (extended to the userspace TLS-block setup); no new seam.
- Q1 = C widens two *verified* surfaces (re-proven, count/rlimit-neutral), not trusted ones.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
