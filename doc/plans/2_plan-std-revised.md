# Plan (revised) — Porting the Rust Standard Library to Eunomia

> Targets `spec rev2`. Self-contained and grounded in the **current** tree
> (`urt`, `kcore`, `ipc`, `loader`, `le-bytes`, `eunomia-sys`, `storage-server`,
> `vendor/rust`). This revision supersedes the first plan: **Phases 0–2 are
> implemented and green** (findings `0`–`7`, plus the gate findings `7-1`/`7-2`);
> the text below records what they actually shipped and carries the corrections
> that surfaced from a review of the first plan into the remaining phases (3–6).
> Sub-phase numbers are preserved; nothing new needed a hyphenated insertion this
> round (the `7-1`/`7-2` gate findings are the only such inserts, and they are
> done).

## Status at a glance

| Phase | Scope | State |
|---|---|---|
| **0** | Toolchain, target JSON, build-std, vendor pin, all-unsupported PAL | **DONE** (findings 0, 1) |
| **1** | `eunomia-sys` seam crate + verified `loader::startup::decode` (+ new `le-bytes`) | **DONE** (findings 2, 3) |
| **2** | Hello-world: entry/argv/env, GlobalAlloc, stdio+exit terminus, time; live CI gate | **DONE** (findings 4, 5, 6, 7, 7-1, 7-2) |
| **3** | TLS + threading + locks + entropy + HashMap | **remaining** (kernel-track) |
| **4** | Filesystem (parallel with 3) | **remaining** |
| **5** | Process/env/stdio polish + first std rewrite | **remaining** |
| **6** | Hardening + forward-port discipline | **remaining** |

The single-threaded std runtime is proven live: `user/stdsmoke` boots under QEMU
and exercises `println!`/`format!`/`Vec`/`Box`/`String`/`Instant`/`SystemTime`/
`process::exit`, and a deliberate `panic!` reaps as `STATUS_PANIC`
(`scripts/std-smoke-test.sh` → `STD SMOKE TEST PASS`, wired into the CI `on-os`
job). Everything below Phase 3 is net-new work.

---

## Starting point — what exists now

The port already runs on a userspace runtime and kernel that supply — and verify —
most of std's hard prerequisites, and the first three phases have wired the
single-threaded surface end-to-end.

**`urt` is the verified PAL spine (in use).** `urt::Heap<N>` is the
`#[global_allocator]`-backing allocator whose arithmetic delegates to the
Verus-verified `freelist::FreeList` (`freelist` 30 verified; `urt` 25 verified; the
arena byte-region is a trusted plain-Rust seam kept honest by Miri+proptest, *not*
one of the 14 ledger seams). `urt::slots::SlotAlloc` (verified) is the TLS
key-index allocator. `urt::time` is the rev2§2.6 time page: a Loom-certified seqlock
read composed with the Verus-verified `Sample::utc_ns_at` tick→ns conversion
(totality + monotonicity). `GlobalAlloc`, `Instant`, and `SystemTime` are wired and
verified-backed (Phase 2).

**`eunomia-sys` is the PAL↔OS seam crate (in use, gated).** It carries a
Verus-verified syscall **arg encoder** (`encode.rs`, `encode(Call) → Result<Encoded,
CallError>`, total over all 26 typed calls, 7 verified obligations), a trusted
`svc #0` inline-asm shell + typed wrappers (`syscall.rs`), a named-grant resolver
over the decoded startup block (`grant.rs`), and the Phase-2 std-PAL bridge modules
(`bootstrap.rs`, `heap.rs`, `io_error.rs`, `pal.rs`, `stdio.rs`, all plain Rust /
host-tested). std reaches it through an `extern "Rust"` bridge of `__eunomia_*`
symbols (a consuming std binary does `extern crate eunomia_sys;`), **not** a sysroot
path-dep — the first plan's documented fallback, forced because vstd's
`verus_builtin` cannot build as a rustc-dep-of-std crate.

**`le-bytes` is a new shared verified crate.** Extracted from `cas`/`loader` during
Phase 1.2: read-direction little-endian byte machinery (`read_u{16,32,64}_le` +
their `by (bit_vector)` split lemmas, 6 verified). It carries its own ledger
Baseline row and CI line, and `loader::startup::decode` now reads fixed-width fields
through it.

**The kernel side is nearly complete.** The `Yield` syscall exists
(`kcore/src/sysabi.rs` opcode 2 → `Sys::Yield`, decode proven total; `Sys::Yield`
in `kernel/src/syscall.rs` calls `maybe_switch(frame, preempt_equal=true)`). The
userspace console driver (`user/console`) exists and the shell does terminal I/O
over the console channel, with `NAME_STDIN`/`NAME_STDOUT` grants emitted by init and
consumed by the shell. The only genuinely-missing kernel pieces are **`TPIDR_EL0`
save/restore** for real TLS (`grep -ri tpidr kernel/ kcore/` is still empty) and an
**entropy source** (no random syscall or object anywhere in `kcore`/`kernel`).

**The startup-block decoder is verified.** `loader::startup::decode` (versioned
EUS1 codec, live fuzz target at `loader/fuzz/fuzz_targets/startup.rs` with 99
committed corpus seeds) is now inside `verus!{}` with a total ∀-bytes contract
(`well_formed_startup`), mirroring `elf::parse`. loader rose from 12 → 29 verified.

**The std PAL is per-module, realized.** `vendor/rust`'s `src/version` reads
`1.98.0`, but the tree carries the post-#117276 `sys/` reorg — each PAL surface
dispatches by `cfg_select!` in `library/std/src/sys/<module>/mod.rs`, with
`sys/pal/<os>/` a thin shell. The eunomia arms exist at
`sys/{alloc,args,env,io/error,stdio,time}/eunomia.rs`, `sys/exit.rs` (eunomia arm),
and `sys/pal/eunomia/{mod.rs,common.rs}`. `motor` (Motūr-OS-delegates-to-`moto_rt`)
remains the structural twin for the remaining threading arms. The vendored fork is
pinned to **nightly-2026-06-26** (rustc `bd08c9e7…`), pinned kernel-scoped in
`kernel/rust-toolchain.toml`.

---

## Global decisions

Made once, each with its rationale; deferred upgrades are collected in
[Deferred work](#deferred-work). Decisions marked **(shipped)** were validated by
the Phase 0–2 implementation; the rest govern the remaining phases.

- **`panic = "abort"`, and the panic→`STATUS_PANIC` terminus is a single override
  (shipped).** `panic-strategy=abort` + `-Zbuild-std=…,panic_abort`; no
  `eh_personality`, no `panic_unwind`. In a std binary the application cannot supply
  `#[panic_handler]` — std owns it — so the reaper contract is preserved by
  overriding the **PAL's `abort_internal()`** to `thread_exit(u64::MAX == STATUS_PANIC)`
  and the PAL's `exit()` to `thread_exit(code)`. **No custom panic hook is needed,
  and none is installed.** This is the subtle point the first-plan review got
  backwards: in *this* vendored fork the common single-panic path routes *through*
  `abort_internal`. The chain is `panic!` → `panic_with_hook` (default hook prints
  last-words via `panic_output`) → `rust_panic` → `panic_abort::__rust_start_panic`
  → `__rust_abort()` (`std::rt`) → `process::abort()` → `crate::sys::abort_internal()`
  → the eunomia override. So the one override catches ordinary panic, double-panic
  (`MustAbort`), non-unwinding panic, OOM, and explicit `process::abort()` alike. The
  `unsupported` template's `intrinsics::abort()` (a raw `udf`) would *not* signal
  `STATUS_PANIC`, so the override is mandatory, not inherited. Proven live: the gate
  panics a real std binary and the shell reaps `panicked` (the harness hard-fails on
  `faulted(` or `exited(254|101)`). **Forward-port caveat:** this terminus depends on
  the fork's `__rust_start_panic → __rust_abort → process::abort` routing; a future
  rustc that reverts `panic_abort` to call `intrinsics::abort()` directly would break
  it — re-verify the exact chain in the 6.3 runbook.
- **Softfloat userspace is mandatory and enforced (shipped).** `TrapFrame`
  (`kcore/src/thread.rs`) saves **general-purpose registers only** — `{x[31],
  sp_el0, elr, spsr}` = 272 bytes, no `q0–q31`/`fpsr`/`fpcr`. A context switch
  therefore saves zero SIMD state, so hardware FP/NEON in EL0 would be silently
  corrupted under preemptive multithreading. The target JSON is generated from
  `aarch64-unknown-none-softfloat` (byte-identical but for `"os": "eunomia"`):
  `"abi": "softfloat"`, `"rustc-abi": "softfloat"`, `"features": "+v8a,+strict-align,-neon"`.
  Note the mechanism: `rustc-abi: softfloat` disables FP-register-in-ABI (it
  supersedes an explicit `-fp-armv8` in the feature string, which is the *older*
  knob and is deliberately **not** present), while `-neon` disables SIMD codegen.
  **Standing constraint:** hardware FP/NEON stays off the table until a future phase
  grows `TrapFrame` to save/restore the full V-register file — a far larger,
  alignment- and asm-offset-sensitive change than the single-word `tpidr` bump in
  3.1, and its own phase (grow the frame, fix the exception save/restore offsets,
  then flip the JSON to hardfloat/`+neon`). **Do not** attempt it as a side effect of
  the TPIDR work in 3.1.
- **`OsStr` is bytes** (the `_` default), matching rev2§4.9 byte-equality names —
  no WTF-8, no `os/eunomia/ffi.rs`. (Shipped: the args/env arms use the internal
  `Buf`/`FromInner`, lossless.)
- **Locks use a `sys::futex` backend, emulated in userspace over per-thread
  notifications.** This tree has **no generic parking-based Mutex/Condvar** —
  `mutex/mod.rs`/`condvar/mod.rs` select either the `futex` impl, a platform impl, or
  `no_threads` (panics on contention); the `Parker` backs only `thread::park` and the
  `queue` RwLock/Once, **not** the locks. So `sys::futex` is the *only* primitive that
  lights up the whole stack (Mutex/Condvar/RwLock/Once/Parker) from upstream's
  already-correct impls — we write zero lock logic, just the four futex functions
  (`futex_wait`/`futex_wake`/`futex_wake_all` + the `u32` atomic types). `motor`
  (`sys/pal/motor/mod.rs` = `pub use moto_rt::futex;`) is the exact template. The
  eunomia PAL arm is a one-line `pub use eunomia_sys::futex;`; the emulation — a
  process-global address→waiter table over `NotifSignal`/`NotifWait` with a small
  bootstrap spinlock — lives in the gated crate. The concurrent wakeup is irreducibly
  Loom/Shuttle (reusing the rev2§3.6 word-check-before-wait discipline the
  `IpcReactor` already models) — never Verus (the version-pinned Verus ghost atomics
  are SeqCst-only, per `doc/guidelines/verification.md`; a proof would certify a
  different binary). A later *kernel* futex/wait-set object (rev2§8.3) is an internal
  backend swap inside `eunomia_sys::futex`, invisible to std (see
  [Deferred work](#deferred-work)). `no_threads` locks are the Phase-2 single-threaded
  interim (still in force until 3.3).
- **The MVP heap is the fixed `.bss` arena (shipped)** — one process-global `static
  HEAP: urt::Heap<{heap::HEAP_BYTES}>` (`eunomia-sys/src/pal.rs`), algorithm verified
  in `freelist`, `HEAP_BYTES` = **1 MiB** default (`1<<20`), overridable at compile
  time via `EUNOMIA_HEAP_BYTES` (a `const fn` parse in `heap.rs`). With no demand
  paging / COW / lazy zero-page mapping (all deferred, rev2§8.3), the loader commits
  real frames for the whole `.bss` at spawn — so **`N` is a reservation, not a
  ceiling: max == committed RAM**, bounding concurrency against `-m 256M`. Four
  disclosed bounds bite even early (they are *disclosed limits*, not surprises):
  **(a)** OOM is a **hard abort** (`alloc`→null→`handle_alloc_error`→abort, routed
  through the PAL terminus so it reaps like a panic); **(b)** the
  `FreeList<HEAP_RANGES = 1024>` fragmentation cap is a *second, independent* limit —
  a fragmenting long-lived workload can hit it before `N` is exhausted, and a
  `dealloc` at the cap **leaks**; **(c)** the heap is **single-threaded** today
  (`unsafe impl Sync`, no lock) — **3.2 must add a lock at the first `spawn`**, see
  the ordering fix there; **(d)** an **alignment ceiling**, not to be confused with
  OOM: `MAX_ALIGN = 64` (inclusive), and a `layout.align() > 64` request (over-aligned
  types, page-aligned buffers) returns null → abort *deterministically, even on an
  empty heap* — a fixed capability limit, not a memory-pressure symptom. Mitigate by
  keeping `N` a per-binary const tuned to each program and over-sizing where RAM
  allows. Heap growth is [deferred](#deferred-work).
- **The verified startup decoder lives in `loader` (shipped);** `eunomia-sys` depends
  on it (default-features off) and re-exports its types through `grant.rs`. Reads go
  through the shared `le-bytes` verified readers.
- **`net` is permanently `unsupported`** (non-goal, rev2§8.1). `sys/net/connection`
  has a `_`→unsupported fallback, so eunomia needs **no file**.
- **`process::Command` stays thin/`unsupported`.** Expose a native, capability-rich
  spawn API instead of emulating fork/exec. The shell keeps its spawn/reap on raw
  `loader::spawn`/`urt::spawn` even after moving its allocator/clock/stdio/fs to std.
- **stderr → `debug-log` for bring-up, then a capability-routed `NAME_STDERR` stream
  for production**, resolved as **`NAME_STDERR` if granted, else the `stdout` channel,
  else `debug-log`.** Folding stderr into stdout is rejected: it breaks the rev2§5.1
  stdout/stdin separation (in `a | b` it pipes `a`'s diagnostics into `b`'s stdin).
  `NAME_STDERR` is a new name **id**, not a new grant **kind** (a `CapSlot` like
  `NAME_STDOUT`): no codec, no verified-decoder change. **Panic last-words stay on the
  `debug-log` kernel-diagnostic path** (rev2§7's "kept … for panic reporting"),
  separate from the userspace stderr stream, so a wedged console can't swallow a
  panic. (Shipped in 2.3: stdout/stderr on debug-log; the console move + `NAME_STDERR`
  are 5.1.)
- **Entropy: a startup-block seed grant as the *mechanism*, with a documented,
  explicitly-non-cryptographic seed as the *MVP source*.** The seed rides the
  rev2§5.1 named-grant mechanism and its decode extends the Phase-1 verified startup
  parser, so the *plumbing* adds no trusted seam. But a seed grant only *distributes*
  entropy — the QEMU `virt` machine (`-cpu cortex-a72`) offers init **no good
  source** (the PL031 RTC is explicitly predictable, rev2§2.6; no virtio-rng), so the
  MVP source is **deliberately predictable**, acceptable *only because today's HashDoS
  surface is thin* (the storage server keys directories in sorted prolly trees, not
  `HashMap`s, rev2§4.9; the shell reads trusted interactive input). It **must be
  disclosed loudly** as MVP-only and not-for-cryptography — the failure is silent, and
  randomness *quality* is not a verification property (only the seed decode is). Two
  requirements hold regardless of source: `fill_bytes` is a per-process **DRBG seeded
  by the grant**, never a copy of the seed bytes (std's `fill_bytes` is infallible — a
  finite seed handed back raw repeats/exhausts silently); and a parent spawning
  children draws a **fresh sub-seed per child** from its own DRBG (the classic
  `fork()`-without-reseed trap). The real source is [deferred](#deferred-work). (This
  is the only Phase-2-untouched mandatory arm — `fill_bytes` currently panics; it is
  Phase 3.4.)

---

## Verification discipline (normative)

This section governs the whole port: **verification discipline is upheld at all
times. No unverified code is written with intent to replace it later. Verified code
is written up front. The only unverified code is where existing tools provably cannot
reach.** It routes per `doc/guidelines/verification.md` and extends the trusted-base
ledger (`doc/guidelines/verus_trusted-base.md`) per its own §11 admission rule.

### The resolving principle

The `vendor/rust` PAL — `sys/pal/eunomia/mod.rs` plus the per-module
`sys/<module>/eunomia.rs` arms — is **necessarily a trusted shell**, the exact
posture `kernel/` holds over verified `kcore`: thin, term-for-term dispatch over a
verified core. This does not violate the no-unverified-code constraint because:

1. **The PAL holds zero genuinely-new logic.** Every non-trivial function delegates
   term-for-term to a gated crate (`urt`, `eunomia-sys`, `ipc`, `loader`, `kcore`),
   auditable by inspection against `pal/unsupported`.
2. **The verus gate runs on the project crates, not on `vendor/rust`.** The fork by
   construction never runs `cargo verus verify` — exactly as `kernel/`'s asm context
   switch never does.

The escape hatch — *unverified only where tools provably cannot reach* — is satisfied
**precisely** by these irreducible categories and nothing else:

- **inline asm** — the kernel asm context switch / `TPIDR_EL0` save-restore *and* the
  userspace `svc #0` syscall-trap + register marshalling in `eunomia-sys` (the
  userspace mirror of the kernel-side trusted register marshalling; inherently
  unverifiable, rev2§6.1(d));
- the concurrent wakeup path (the emulated-futex address→waiter dispatch + its
  bootstrap spinlock, over notifications) — SeqCst-pin infeasible in Verus;
  Loom/Shuttle-of-record + the existing `IpcReactor` TLA model;
- any `virtio-rng` device seam (DMA/hardware, rev2§2.5) — *not in the MVP*.

**Everything else is verified on arrival**, never stubbed-then-replaced. The split
inside the syscall layer is realized: the *pure byte-level arg encode* is Verus
(`eunomia-sys/src/encode.rs`, total `encode(Call) → Result`); only the `svc`
instruction and register-file marshalling are the trusted inline-asm shell.

### Where new logic lives

| Sink | What goes there | State |
|---|---|---|
| **`urt`** (25 verified + `freelist` 30) | heap algorithm, `SlotAlloc`, `utc_ns_at` (all done); the **yielding spinlock** (new, Loom); the TLS key-table layer (new, verified surface) | partial |
| **`le-bytes`** (6 verified) | shared LE readers + split lemmas | **done** |
| **`eunomia-sys`** (7 verified) | syscall arg encode (done), io-error map, futex emulation glue; the std-PAL bridge modules | partial |
| **`loader`** (29 verified) | the startup decoder in `verus!{}` (done); entropy-seed grant decode extends it (3.4) | partial |
| **`vendor/rust` PAL** | **nothing but term-for-term delegation** — no arithmetic, no parsing, no business logic | ongoing |

### Per-piece routing

| std logic | Routing | New seam? | State |
|---|---|---|---|
| Startup-block decoder (argv/env/EUS1 grants) | **Verus** (total ∀ bytes) + **cargo-fuzz** | No — verified surface in `loader` | **done** |
| Syscall arg **byte** encode | **Verus** (`encode(Call)→Result`, total) | No — verified surface in `eunomia-sys` | **done** |
| `svc #0` + register-file marshalling | **trusted inline-asm shell** | No — syscall-dispatch shell (d) | **done** |
| GlobalAlloc glue | **Verus** (algorithm in `freelist`) | No | **done** |
| GlobalAlloc arena byte-region + `sbrk` grow | **Miri**+proptest (region); grow **folds under the Store/aspace page-table-join seam (c)** | No | region done; grow deferred |
| Heap thread-safety (the lock) | **Loom** (raw atomic + fence; same tier as the futex bucket lock) | No | **3.2** |
| TLS — `TPIDR_EL0` save/restore | **trusted-shell + ledger routing-note** (asm-context-switch shell (d)) | No — `TcbView` omits the register frame | **3.1** |
| TLS — key table | **Verus** (over verified `SlotAlloc`) | No | **3.5** |
| `sys::futex` — bucket spinlock (raw atomic + fence) | **Loom** (certifying) | No | **3.3** |
| `sys::futex` — address→waiter dispatch over notifications | **Shuttle**; **reuse** `tla/ipc_reactor` + its 3 negative controls | No | **3.3** |
| Mutex / Condvar / RwLock / Once / Parker | **none new** — upstream futex impls over the two rows above | No | **3.3** |
| stdio sinks | **trusted-shell** (marshalling over verified `ipc`) | No | debug-log done; console 5.1 |
| fs marshalling | **Verus** (path-component decode) + **cargo-fuzz**; rights lattice + `check_header` already verified | No | **4.x** |
| time (Instant/SystemTime) | **Verus** (`utc_ns_at`) + **Loom** (seqlock read) | No | **done** |
| entropy seed decode | **Verus** + **cargo-fuzz** (extends the startup parser + corpus) | No; DRBG/quality is *not* verified — only the decode | **3.4** |
| process exit / abort | **trusted-shell** (thread-lifecycle (d)) | No | **done** |
| io-error decode | **proptest** (total policy map) | No | **done** (2.1) |

### Trusted-base ledger changes (`doc/guidelines/verus_trusted-base.md`)

The ledger is **14 named seams**. Every row/note names *both* a reason and a host
test (its §11 admission rule). The tally **stays 14** for the MVP. Applied so far:

- **New verified Baseline row — `eunomia-sys`** (`cargo verus verify -p eunomia-sys`,
  7 verified) + a syscall-marshalling routing note. **Done.** Not a seam — verified
  surface.
- **New verified Baseline row — `le-bytes`** (`cargo verus verify -p le-bytes`, 6
  verified) — an unanticipated but clean extraction of the shared LE readers.
  **Done.** Not a seam.
- **`loader` Baseline row** count rose **12 → 29** (the startup decoder obligations).
  **Done.**
- **Scope note (not a seam):** `vendor/rust` is a submodule fork that by construction
  never runs `cargo verus verify`; the PAL's absence from the gate is the same
  posture as `kernel/` over `kcore`. **Done.**

Still to apply in the remaining phases (all planned, tally stays 14):

  | Folds under | Reason | Host test |
  |---|---|---|
  | `TPIDR_EL0` save/restore → thread-lifecycle / asm-context-switch shell (d) | Register frame is outside `verus!{}`; `TcbView` omits it | Boot two threads sharing an aspace; each reads a distinct TLS marker (3.1) |
  | `sbrk`/heap-grow → Store/aspace page-table-join (c) | Retype+map glue over the already-trusted join | Existing aspace top-up host test (deferred item) |

- The `yield` folding-note is **satisfied already** (op 2 exists; no row).
- **TLS-key-table:** the `urt` key table is verified surface over `SlotAlloc`; the
  per-thread storage block sits over the verified heap. Any irreducible plain-Rust
  pointer step (a TPIDR-base + offset read) folds under (d) as a routing note with a
  host test — not a 15th seam.
- **Conditional seam (14→15) — only if `virtio-rng` is later chosen** as the entropy
  *source* upgrade ([Deferred work](#deferred-work)): a real DMA/hardware row + device
  bring-up host test. The MVP (documented-predictable seed) and the `RNDR`/`-cpu max`
  upgrade both **avoid the seam**.

### What keeps the shell honest

There is **no automated gate** proving PAL thinness, so honesty rests on three
things: (1) a **thinness rule** — the PAL contains zero new logic, enforced by review
of the PAL diff vs `pal/unsupported`, applying the §11 **inverse-leak rule** (the PAL
must re-establish every `eunomia-sys`/`urt` `requires` — alloc bounds, slot capacity,
buffer-belongs-to-pool — at the boundary or runtime-guard it); (2) **host tests** —
each seam and folding-note names one; (3) **the ledger** — a row that cannot name a
reason *and* a host test is a finding.

Because there is no automated gate, the thinness + inverse-leak check is a **per-task
gate**, not an end-of-project sweep: **every PAL-touching task** (the completed 2.x
arms recorded it in their findings; the remaining ones are 3.2, 3.3, 3.4, 3.5, 4.1,
4.3, 5.1, 5.2) reviews its own arm against `pal/unsupported` and records, in that
task's findings doc, that the arm adds zero new logic and re-establishes every
verified `requires` at the boundary. Phase 6.2 is the **consolidating** sweep, not the
first application.

---

## Capability map (std surface → Eunomia)

Three arms are **mandatory** even for hello-world — `sys/alloc`, `sys/random`,
`sys/io/error` (`alloc/mod.rs` and `io/error/mod.rs` have no `_` arm; `random/mod.rs`
has an empty `_ => {}` but `hashmap_random_keys` is imported unconditionally and
resolves to `fill_bytes`). Everything else has a `_`→unsupported and can ship
unsupported first.

| std surface | Backed by | Readiness | Notes / remaining work |
|---|---|---|---|
| **alloc (GlobalAlloc)** | `urt::Heap<1 MiB>` over verified `freelist` | **DONE (2.2)** | `N` is a **reservation** (no demand paging → committed at spawn), per-binary const via `EUNOMIA_HEAP_BYTES`. Disclosed bounds: single-thread no-lock (**lock lands 3.2**), frag-cap 1024 (2nd limit; dealloc-at-cap leaks), **OOM = abort** (routed through the PAL terminus), and an **alignment ceiling** at `MAX_ALIGN=64` (`align>64` → null → abort deterministically even on an empty heap — an unsupported-alignment failure, *not* OOM) |
| **time: SystemTime / Instant** | `urt::now_utc_ns` (verified `utc_ns_at`, Loom seqlock) / `CNTVCT/CNTFRQ` direct | **DONE (2.4)** | `SystemTime` needs the time grant (panics if unattached); `Instant` is zero-syscall |
| **process: exit/abort** | `ThreadExit(15)` → verified `report_terminal` | **DONE (2.3)** | PAL `exit()`→`thread_exit(code)`, `abort_internal()`→`thread_exit(STATUS_PANIC)`; **no panic hook needed** (see Global decisions). `Command` thin/unsupported |
| **panic/unwind** | panic=abort (no `eh_personality`) | **DONE** | Terminus proven live by the gate |
| **env / args** | `loader::startup::decode` of the slot-0 boot message | **DONE (2.1), partial** | args live; `env::vars` **empty until a producer emits env entries (5.2)**; `setenv`/`unsetenv` return `Unsupported` (no shared mutable environ) |
| **thread: spawn/join/yield/sleep** | `Retype→Thread` + **`ThreadStartAs(18)`** (shares the process aspace/cspace); `ThreadExit(15)`; join via `ThreadBind(21,on-exit)`+`NotifWait(12)`+`ReadReport(22)`; **`Yield`=op 2**; sleep **yield-poll MVP** (`now_mono_ns`+`Yield`) | **DONE (3.2, findings #9)** | `urt::thread` in-process primitive; the closure crosses `x0` via the **op-18 7th arg (`x6`)** — `ThreadStart(13)` runs in the identity map, so op 18 + a widened arg vector was needed (a kernel-track change). Real per-thread TLS (`TPIDR_EL0` block, pulled forward from 3.5) makes `set_current` work on >1 thread |
| **sync: Mutex/Condvar/RwLock/Once/Parker** | `sys::futex` emulated over `NotifSignal(11)`/`NotifWait(12)` → upstream futex impls | **3.3** | `eunomia_sys::futex` (4 fns) + Loom/Shuttle reusing `tla/ipc_reactor`; locks come free. Bucket lock **yields**, not pure-spins (priority-inversion mitigation). Timeouts: MVP yield-poll; timer-bit blocking [deferred](#deferred-work) |
| **stdio** | (bring-up) `DebugWrite(1)`; (real) console channel over `ipc` | **DONE out (2.3) / stdin missing** | stdout/stderr on debug-log now; **stdin is EOF until 5.1**; console move + `NAME_STDERR` are **5.1** |
| **thread_local / TLS** | `TPIDR_EL0` + `urt::slots` (verified) | **3.1 / 3.2 / 3.5** | `TPIDR_EL0` save/restore (3.1); the `local_pointer!` current-thread/id path over a `TPIDR_EL0` block (**3.2**, needed for `set_current` on spawned threads); the verified `urt` TLS key table + destructors + `thread_local!` macro storage (**3.5**) |
| **fs: File/read_dir/metadata** | storage-server openat session (handle-relative, component paths) | **4.x** | `sys/fs/eunomia.rs` client + path decode; `File=(HandleId, TreePath, client offset)`; 256-byte `MAX_MSG` → client offset loops |
| **hashmap_random_keys** | nothing — **hard blocker** | **3.4** | seed-grant + `sys/random/eunomia.rs`; `fill_bytes` = per-process **DRBG** over the seed; MVP seed documented-predictable (non-crypto); per-child fresh sub-seeds |
| **net** | nothing, by design | n/a | permanently unsupported |

**fs surface that is `Unsupported` by construction** (rev2§4.9 has none of it):
symlink / hard_link / read_link / canonicalize; permissions / chmod / chown
(authority is the cap rights mask, not mode bits); `accessed` / `created` (no atime,
rev2§4.9); `set_len` / truncate; `create_dir` of empty dirs (creation is a side effect
of `Write`); `DirEntry::ino`, nlink/uid/gid; `current_dir`/`set_current_dir` as
syscalls (handle-relative bookkeeping; no ambient cwd). Cross-subtree rename is
`EXDEV` by construction. `modified()`/mtime is **supportable but deferred** (a
mandatory rev2§4.9 field absent from the current wire protocol; return `Unsupported`
for now, see [Deferred work](#deferred-work)). `set_times` stays unsupported (mtime is
server-assigned).

---

## Spec & kernel changes

The verification posture is `kernel/`-over-`kcore` throughout. **Kernel-track surface:
two changes — `TPIDR_EL0` (3.1) and the `ThreadStartAs` `x6` arg (3.2)** — the entropy
MVP adds no kernel work.

| # | Change | Kind | Where | Trust posture | Still needed? |
|---|---|---|---|---|---|
| 1 | Yield syscall | — | `kcore/src/sysabi.rs` (opcode 2) | verified decode + trusted scheduler shell | **No — exists (op 2); done** |
| 2 | `TPIDR_EL0` save/restore | trusted-kernel-shell | `kcore/src/thread.rs` `TrapFrame` (outside `verus!{}`) + `kernel/src/exceptions.rs`, `main.rs`, `syscall.rs` | trusted asm shell (rev2§6.1d); ledger *routing note*, no new seam | **No — done (3.1, findings #8)** |
| 2a | `ThreadStartAs` 7th arg in `x6` | verified re-proof | `kcore/src/sysabi.rs` `decode` (`[u64;6]→[u64;7]`) + `eunomia-sys/src/encode.rs` (verified); `kernel/src/syscall.rs` dispatch/execute; `ipc::sys`/`eunomia_sys::syscall` marshalling | verified surfaces re-proven (count- & rlimit-neutral); no new seam. Needed because `ThreadStart(13)` runs a fresh TCB in the identity map, so a std thread must use `ThreadStartAs(18)` — which had no arg register free | **No — done (3.2, findings #9)** |
| 3 | Entropy: seed-grant **mechanism** | spec-convention + verified-decode | rev2§5.1 table (a new inline-bytes grant kind → extends the verified decoder); loader/`eunomia-sys` startup parser | no new seam; decode is Verus+fuzz | **Yes (3.4)** |
| 3a | Entropy MVP **source**: documented-predictable | none (init seeds from RTC/`CNTVCT`) | init seed generator (trusted shell) | no new seam; **explicitly non-cryptographic**, disclosed MVP-only | **Yes (3.4)** |
| 4 | Console stdio | marshalling only | std PAL over `chan_send`/`chan_recv` resolving slots from the delivered grant table | no new verified logic; driver + shell path + `NAME_STDIN/STDOUT` grants already exist | **Mostly done — only the std-side stdio wiring (5.1)** |
| 5 | stderr name | spec-convention + marshalling | **add `NAME_STDERR`** to the rev2§5.1 table (new name id, `CapSlot` — no codec change) | `debug_write` bring-up; capability-routed `NAME_STDERR`→stdout→debug-log for production; panic last-words stay on debug-log | **Yes — add the name (5.1)** |

**`TPIDR_EL0` detail (3.1).** `TrapFrame` (`kcore/src/thread.rs`, `repr(C)`, **outside**
`verus!{}`) is `{x:[u64;31], sp_el0, elr, spsr}` = **272 bytes**; `ThreadStart` writes
`elr/sp_el0/spsr/x0` only. The change touches **no Verus obligation** (verified
`TcbView` models no register frame). Edits, all trusted shell: (1) add a `tpidr`
field, growing the struct **272 → 288** with a pad word (280 is not 16-aligned);
(2) `mrs/msr tpidr_el0` in `el0_entry`/`el0_restore` and bump the hand-coded
`sub/add sp,#272` and every `stp` offset in `kernel/src/exceptions.rs` in lockstep;
(3) zero-init at `enter_first_thread` + `ThreadStart`/`ThreadStartAs`. Add an
`offset_of` **const-assert** coupling the asm offsets to the struct (none exists today
— a stale offset silently corrupts `eret`). Re-run `cargo verus verify -p kcore` and
confirm the count is unchanged (currently 407). **Optional refinement (deferred):**
because the kernel touches `TPIDR_EL0` nowhere at EL1 (`grep` is empty), a same-thread
syscall return re-writes the identical value, so strictly only `maybe_switch`'s
context-switch branch needs the `mrs`/`msr`. Keep the uniform per-entry save/restore
for the MVP — it is ~2 cheap system-register ops, simpler and safer than threading a
separate `tpidr` copy through `maybe_switch`'s frame-copy, and it future-proofs
against an EL0 thread rewriting its own TLS base. Restricting to the switch path is an
available later optimization.

The real entropy *source* (`RNDR` / virtio-rng) and the `heap` named grant for
growable heaps are [deferred](#deferred-work).

---

## Phases

Ordered by implementation order. Each **sub-phase is a separately-implementable
task** that produces exactly one findings doc (see
[Findings-doc requirement](#findings-doc-requirement)). Gate notation: the
verification/test that must be green before the task is "done".

> A *real* `cargo verus verify` run ends each crate with a `verification results::
> N verified, 0 errors` line; a re-run over an unchanged `target/` reports *nothing*
> (stale cache). Clean the crate (`cargo clean -p <crate>`) before any gate that
> claims a count, per `CLAUDE.md`.

### Phase 0 — Toolchain & target — **DONE** (findings 0, 1)

Both sub-phases shipped green.

- **0.1** — `targets/aarch64-unknown-eunomia.json` generated from
  `aarch64-unknown-none-softfloat` with one semantic edit (`"os": "eunomia"`);
  softfloat enforced (`abi`/`rustc-abi` = `softfloat`, `features` = `+v8a,+strict-align,-neon`),
  `panic-strategy=abort`. `kernel/build.rs` drives the sub-build with
  `-Zbuild-std=core,compiler_builtins,alloc,std,panic_abort`,
  `-Zbuild-std-features=compiler-builtins-mem` (the mem intrinsics `memcpy/memset/…`
  that std's fmt/io paths emit — the same feature the kernel already links), and the
  newly-required `-Zjson-target-spec`. `__CARGO_TESTS_ONLY_SRC_ROOT` redirects
  build-std at `vendor/rust/library`. The `os` rename rippled additively to 7 source
  sites + 5 user build scripts (`target_os="none"` → `any(target_os="none",
  target_os="eunomia")`) — an open cleanup, not a blocker (a single build.rs-emitted
  `#[cfg(bare_metal)]` alias would centralize it).
- **0.2** — `vendor/rust` pinned to **nightly-2026-06-26** (rustc `bd08c9e7…`, an
  exact compiler↔source match) via a kernel-scoped `kernel/rust-toolchain.toml` (root
  stays default; verus gate stays Rust 1.95.0). **`restricted_std` polarity: eunomia
  was added to the `library/std/build.rs` allowlist** (the "no special requirements"
  branch), so the build does **not** emit `restricted_std` — eunomia gets **full std**
  and no downstream binary needs `#![feature(restricted_std)]` (confirmed:
  `user/stdsmoke` carries no such attribute). `sys/pal/unsupported` copied to
  `sys/pal/eunomia/`; per-module arms stubbed to link all-unsupported (`alloc` shipped
  a null `System`; `io/error`, `random`, `thread_local` handled explicitly because
  their dispatchers lack a usable `_` arm; the rest route via `_`). `library/backtrace`
  nested submodule initialized; CI `on-os` job updated. Gate: `cd kernel && cargo build`
  builds std, a `fn main(){}` links, no_std still boots, `kcore` verus unchanged.

*Carry-forward:* re-confirm the exact nightly↔submodule-commit match in the 6.3
runbook — the submodule has since advanced past the 0.2 pin during later phases while
keeping the same nightly.

### Phase 1 — Seam crate + verified startup decoder — **DONE** (findings 2, 3)

The upfront verification phase shipped green; loader rose 12 → 29 verified, a new
`le-bytes` crate (6 verified) was extracted, and `eunomia-sys` landed (7 verified).

- **1.1** — `eunomia-sys` gated crate (kcore no_std posture). Three trust surfaces:
  (a) **verified** `encode.rs` — `encode(Call) → Result<Encoded, CallError>`, total
  over all 26 typed calls, proving per-register placement matching `kcore::sysabi::decode`
  and inverse-leak *refusal* of out-of-range fields (`Retype ty≥8`, `ChanSend len>256`,
  `ChanBind event>2`, `ThreadStart|ThreadStartAs prio≥32`, `ThreadBind which>1`) — a
  strictly stronger total encoder than the first plan's `requires`-guarded partial
  function; (b) **trusted** `svc #0` inline-asm shell + typed wrappers + the ABI
  constant surface; (c) **plain** `grant.rs` named-grant resolver over a decoded
  `Startup`. Cross-side agreement is a host test (`encode_round_trips_through_kernel_decode`
  against the real `kcore` decoder, kept a dev-dep only). New ledger Baseline row.
  Gate: `cargo verus verify -p eunomia-sys` (7 verified, 0 errors).
- **1.2** — `loader::startup::decode` lifted into `verus!{}` with the total ∀-bytes
  contract `res matches Some(s) ==> well_formed_startup(s, buf@)` (counts within their
  arenas; every borrowed argv/env subrange ⊆ `buf@`, the `elf::seg_ok` twin). Reads
  through the new `le-bytes` verified readers. Public signature unchanged, so callers,
  fuzz target, and corpus are untouched. Gate: `cargo clean -p loader && cargo verus
  verify -p loader` (29 verified) + `cargo fuzz run startup` (42.5M-run/60 s clean) +
  Miri corpus replay.

*Carry-forward (6.2):* the ~40 lines of duplicated trusted `svc` asm in
`eunomia-sys/src/syscall.rs::imp` (copied from `ipc::sys::imp` rather than shared), and
the per-binary `resolve_*` grant helpers still private to `user/shell`/`user/init`,
are not yet consolidated onto the `eunomia-sys` layer — a deliberate blast-radius
deferral.

### Phase 2 — Hello-world, single-threaded ⭐ — **DONE** (findings 4, 5, 6, 7, 7-1, 7-2)

A complete single-threaded std runtime, proven live by a booting CI gate. std reaches
the seam through an `extern "Rust"` bridge of `__eunomia_*` symbols (findings **7-2**
records why: vstd's `verus_builtin` can't build as a rustc-dep-of-std crate, so the
sysroot path-dep was replaced by the plan's documented link-time fallback; a consumer
does `extern crate eunomia_sys;`).

- **2.1** — non-crt0 `_start` → `__eunomia_bootstrap_init()` → `main` →
  `__eunomia_thread_exit`; `sys/args`, `sys/env`, `sys/io/error` arms; the startup
  block decoded by the **verified** 1.2 decoder; io-error map (u8 → `ErrorKind`)
  host-tested. `env::vars` is empty pending a producer (5.2).
- **2.2** — `sys/alloc/eunomia.rs` `impl GlobalAlloc for System` over the single
  process-global `static HEAP: urt::Heap<1 MiB>`; `realloc`/`alloc_zeroed` use
  GlobalAlloc defaults (no in-place grow — a disclosed cost). Disclosed MVP bounds
  (reservation-not-ceiling, OOM=abort, frag-cap 1024, single-thread no-lock, and the
  **alignment ceiling** relabel of finding R9). Gate: `freelist` 30 / `urt` 25 /
  `eunomia-sys` 7 green + `urt` Miri sweep.
- **2.3** — `sys/stdio/eunomia.rs` stdout/stderr → `DebugWrite(1)` chunked at
  `kcore::sysabi::DEBUG_WRITE_MAX` (1024); `panic_output` same path; disclosed as a
  *temporary rev2§2.7 deviation* (replaced for stdout/stdin by the console channel in
  5.1, retained only for panic last-words). **Exit terminus:** PAL `exit()` /
  `abort_internal()` overridden to `thread_exit(code)` / `thread_exit(STATUS_PANIC)`
  with `code as u32 as u64` zero-extension so `exit(-1) ≠ STATUS_PANIC`. Gate: a
  panicking std binary reaps as `STATUS_PANIC`. (This is where the first-plan review's
  "override won't catch a panic" concern was checked and found **moot for this fork** —
  see Global decisions; the added Verus item bumped `kcore` 406 → 407.)
- **2.4** — `Instant` ← `CNTVCT/CNTFRQ` (zero-syscall, no grant); `SystemTime` ←
  `urt::now_utc_ns` (verified `utc_ns_at`, Loom seqlock; panics if the time grant is
  unattached). Gate: `urt` re-cited; both clocks work.
- **GATE (7-1)** — `user/stdsmoke` (cross-built by `kernel/build.rs`) exercises
  `println!`/`format!`/`Vec`/`Box`/`String`/`Instant`/`SystemTime`/`process::exit`,
  and a real `panic!` on argv `panic`. `scripts/std-smoke-test.sh` → `STD SMOKE TEST
  PASS`, hard-failing on `faulted(` / `exited(254|101)`; wired into the CI `on-os` job.

*Carry-forward:* stdin EOF until 5.1; `env::vars` empty until 5.2;
`EUNOMIA_HEAP_BYTES` not yet threaded through `build_user` (every std binary gets the
1 MiB default — producer side is 5.3); `fill_bytes`/`HashMap` unsupported until 3.4.

### Phase 3 — TLS + threading + locks + entropy + HashMap
*The only kernel-track phase. Parallelizable with Phase 4. Internal order forced:
3.1 → 3.2 → 3.3; 3.4/3.5 independent once 3.1 lands.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **3.1** | Real TLS (kernel) | `TPIDR_EL0` save/restore: `tpidr` in `TrapFrame` (272→288 + pad), `mrs/msr` in `exceptions.rs`, grow `sub/add sp,#272`, seed at start; **`offset_of` const-assert**. Keep the save/restore uniform (per-entry) for MVP; note switch-only as a deferred optimization. **Standing-constraint note:** the frame stays GP-only, so hardware FP/NEON in EL0 remains off the table — growing the frame to save the V-register file is its own future phase, not part of this bump | `cargo clean -p kcore && cargo verus verify -p kcore` re-passes **407/0** (unchanged); host test: 2 threads share an aspace, read distinct TLS markers; ledger routing-note added | **8** |
| **3.2** | spawn/join/yield/sleep **+ heap lock** | **(a) Heap lock first — the concurrent-alloc fix.** `thread::spawn` opens simultaneous allocation against the process-global `urt::Heap` (`unsafe impl Sync`, no lock today) — the boxed closure, per-thread TLS blocks (3.5), and any user/std allocation on the child race the parent. Introduce a raw **yielding spinlock** primitive (in `urt`/`eunomia-sys`) and wrap the `Heap` `fl` access; rewrite the `urt::Heap` `Sync` justification from "no concurrent access by construction" to "mutual exclusion by the heap spinlock", and update the `sys/alloc` + findings-5 disclosure. The lock is the correct choice, **not** per-thread arenas: cross-thread frees (a `Box` moved to another thread and dropped) can't locate the owning arena without a global structure. The lock **yields** rather than pure-spins (priority-inversion mitigation, see 3.3). **(b) Thread primitive.** New `urt` in-process thread-spawn primitive (retype-TCB + fund stack/guard per rev2§5.3 + `thread_start`) — **rename to avoid colliding with the existing process-spawn `urt::spawn`** (`SpawnRec`/`arm`/`reap`, a CDT donation subtree). **Shipped mechanism (corrects the plan draft, findings #9):** a std thread must share the process aspace, so it uses **`ThreadStartAs`(18)** — not `ThreadStart(13)`, whose fresh TCB runs in the kernel identity map. `ThreadStartAs` had no free arg register, so **op 18 gained a 7th arg in `x6`** (a small kernel-track verified re-proof, change 2a); the closure `Box<ThreadInit>` then crosses in `x0` and the `sys/thread/eunomia.rs` trampoline is a plain `extern "C" fn(arg)` mirroring `motor.rs`. `yield_now` = op 2; **sleep = yield-poll MVP** (`now_mono_ns`+`Yield`; the child holds no timer cap). Provisioning is **scoped/opt-in** (Q2): a thread-capable binary gets self-aspace(WRITE)/self-cspace/thread-untyped caps + a wider cspace/donation via new `NAME_*` grants; others keep least-authority. **Per-thread TLS pulled forward from 3.5** (a `TPIDR_EL0` `local_pointer!` block) so `set_current` works on >1 thread | **DONE (#9):** **Loom+Shuttle** certify the heap spinlock (`urt::lock`); `urt::thread_layout` host tests (stack-VA budget); QEMU `stdsmoke spawn` → `STD32 PASS` (2 threads, concurrent alloc, distinct per-thread TLS ids); kcore 407 / eunomia-sys 7 / urt 25 re-verify (count- & rlimit-neutral) | **9** |
| **3.3** | Locks | `eunomia_sys::futex` emulated over notifications (the 4 futex fns + types; address→waiter table + bootstrap spinlock, **reusing the 3.2 yielding-spinlock primitive** — no net-new primitive here); add `eunomia` to the five `sys/sync/*/futex.rs` arms + `pub use eunomia_sys::futex;` in the PAL → Mutex/Condvar/RwLock/Once/Parker come free. **Priority-inversion discipline (rev2§5.4):** the scheduler is single-core, strict fixed-priority, round-robin within a level, **no donation**, and `yield` (op 2) provably never drops to a lower level. So (i) the bucket lock's spin loop calls `eunomia_sys::yield_now()` as backoff, not `spin_loop` — this handles same/comparable-priority contention (a same-level holder preempted by the tick runs via round-robin); (ii) **document a safety boundary**: yield-backoff does *not* cure a high-priority spinner waiting on a lower-priority holder (strict priority won't schedule the holder), so the userspace bucket lock must **not be contended across priority levels** until the deferred kernel wait-set lands — flag the tension with the rev2§5.4 "servers run above clients" convention. Scope: only the tiny check-and-enqueue hold is at risk; the long `futex_wait` already blocks in the kernel via `NotifWait`, which deschedules the waiter. Timeouts: MVP **yield-poll** `futex_wait(Some(d))` (`Instant`+`yield_now`, correct but busy → `wait_timeout`/`park_timeout` work); timer-bit blocking is [deferred](#deferred-work) | **Loom** (certifying, the bucket spinlock) + **Shuttle** (breadth, the dispatch) green, **reusing `tla/ipc_reactor`** + its 3 negative controls — **never Verus** (SeqCst pin). Loom mocks yield to its scheduler, so the yield-backoff does not change the obligation | **10** |
| **3.4** | Entropy + HashMap | startup-block seed grant (a new inline-bytes grant kind → extend the 1.2 verified decoder + fuzz corpus); init seeds it MVP-predictable (documented non-crypto); `sys/random/eunomia.rs` (mandatory arm) where `fill_bytes` is a **per-process DRBG** over the seed (zeroize the seed after init), and a parent draws a **fresh sub-seed per child**; define the **no-seed behavior** (recommend loud abort at first use, the `now_utc_ns` precedent — `fill_bytes` is infallible, so the alternative is silent predictability); unblock `HashMap` `RandomState` | seed decode Verus+fuzz (rides 1.2); `HashMap` works under smoke; findings doc records the MVP-predictable disclosure + the deferred real-source path | **11** |
| **3.5** | TLS keys (hardening) | **The `local_pointer!` current-thread/id path + the `TPIDR_EL0` per-thread block + `sys/thread_local/eunomia.rs` shipped early in 3.2** (findings #9 — `set_current` needs per-thread storage the moment `spawn` exists). 3.5 is now the *hardening* remainder: the **verified `urt::tls` key table** over `SlotAlloc` (replacing 3.2's runtime slot-counter), TLS **destructors** (drop-current on thread exit — 3.2 leaks the block), and the `thread_local!`-macro storage (currently single-threaded `no_threads`; needed for `HashMap`'s `RandomState`, 3.4) | `cargo verus verify -p urt` (key-table obligations green); host test | **12** |

### Phase 4 — Filesystem
*Depends ONLY on Phase 2 (allocator + storage connector), not Phase 3 — runs in
parallel with the threading track.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **4.1** | fs client | `sys/fs/eunomia.rs` openat-only client over `storaged`: `File=(HandleId from the root grant, TreePath, client offset)`; build the deferred **client-side** connect handshake (`session.rs` has only the server admit step today); 256-byte `MAX_MSG` → client offset loops | storage host fuzz corpus green; QEMU fs smoke (open/read/write/readdir/rename/remove/sync) | **13** |
| **4.2** | path decode | byte→component-list parser, `OsStr` = bytes. Per rev2§4.9, `.`/`..` are **resolved by the path walk, never stored**: drop `.`, pop on `..`, and **deny any `..` that would pop above the process root handle** (the rev2§2.3/§4.9 "unnameable above the handle" confinement rule) — *resolve*, don't blanket-reject. **Verus**-total + **cargo-fuzz** where it is genuine untrusted byte-parsing; proptest for pure presentation policy | `cargo verus verify` + fuzz green | **14** |
| **4.3** | metadata + stubs | map `len`/`is_file`/`is_dir` (is_symlink always false); errno→`ErrorKind` decision table (11 `ErrorCode` variants; `Stale`/`Pinned` have no clean POSIX analog — documented, not a verification property); stub the `Unsupported` surface listed above. Record that subtree confinement is fuzz/test-routed at dispatch, not proven | host roundtrip tests | **15** |

### Phase 5 — Process / env / stdio polish + first std rewrite

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **5.1** | console stdio + stderr | move `stdout`/`stdin` off `debug-log` onto the `user/console` channel via `ipc` (reactor poll-once loop = the validated `reactor_no_lost_wakeup` harness; `send_blocking` for write backpressure). The std PAL resolves the stdin/stdout cspace slots from the startup grant table — `NAME_STDIN/STDOUT` are **already emitted by init and consumed by the shell today**, so no grant-delivery change is needed. **Add `NAME_STDERR`** (new name id, `CapSlot`, no codec change) and resolve stderr as **`NAME_STDERR` → else stdout channel → else debug-log**; init grants the same console endpoint under both `stdout` and `stderr` for a terminal. **Keep panic last-words on `debug-log`** | Shuttle reactor harness green; QEMU interactive stdio + a piped `a \| b` where `a`'s stderr does not corrupt `b`'s stdin | **16** |
| **5.2** | process/env | `process::exit(code)` → PAL `exit()` (from 2.3); `abort` → `abort_internal()` → `STATUS_PANIC` (from 2.3, re-confirmed); `Command` thin/unsupported; `temp_dir` → `tmp` grant; `current_dir`/`set_current_dir` handle-relative or unsupported; **populate env entries (producer side)** so `env::vars` is non-empty | smoke | **17** |
| **5.3** | rewrite a user binary | `hello` on std first (validates entry/argv/alloc/exit/`STATUS_PANIC`), then `shell` (alloc + `SystemTime`/`Instant` + console stdio + args + fs) — **keep spawn/reap on raw `loader::spawn`/`urt::spawn`** (std::process can't model it yet). Thread `EUNOMIA_HEAP_BYTES` through `kernel/build.rs`'s `build_user` env so each std binary can size its `N` (the shell may want more than the 1 MiB default) | extended boot smoke exercising fs + console stdio | **18** |

### Phase 6 — Hardening & forward-port discipline

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **6.1** | on-target test triage | run `libcore`/`liballoc`/`libstd` test subsets on-target via QEMU; triage + record skips (eunomia-only std crates can't host-build — excluded like `kernel`) | QEMU test run; skip log committed | **19** |
| **6.2** | fuzz + PAL audit | grow committed fuzz corpora for the verified decoders; **PAL thin-delegator audit** — diff `sys/pal/eunomia` (+ the `eunomia.rs` arms) vs `pal/unsupported`, confirming zero new logic and that every verified `requires` is re-established or runtime-guarded at the boundary (the §11 inverse-leak rule). Retire the carry-forward duplication (the copied `svc` asm in `eunomia-sys`; the private `resolve_*` grant helpers in `user/shell`/`user/init`; the `any(none, eunomia)` os-cfg sprawl). *This review is the standing gate for the thinness rule* | cargo-fuzz green; audit recorded | **20** |
| **6.3** | runbook + ledger | forward-port runbook (pinned-nightly bump cadence, the diff surface, regression set). **Re-verify the panic→`STATUS_PANIC` terminus chain** (`panic_abort::__rust_start_panic → __rust_abort → process::abort → abort_internal`) against each nightly bump — it is proven now but version-dependent. **Re-confirm the exact nightly↔`vendor/rust`-commit match** (the 0.2 invariant that drifted). Record the deliberate Verus-pin ↔ std-version **decoupling**; finalize the ledger (the `eunomia-sys`/`le-bytes` Baseline rows and the loader count are in; add the folding notes for TPIDR/sbrk-grow; TLS-key note; entropy decision) | ledger consistent; runbook committed | **21** |

### Dependency & parallelism map

```
Phase 0 ─ 1.1 ─ Phase 2 ─ Phase 3 (kernel track) [3.1 TLS → 3.2 spawn+heap-lock → 3.3 locks; 3.4/3.5]
   │       │        └───── Phase 4 (fs) ── parallel with Phase 3; needs only Phase 2 ─┐
   └── 1.2 (verified decoder, parallel with Phase 0) ── feeds 2.1                       ├─ Phase 5 ─ Phase 6
                                                                                        ┘
   (Phases 0, 1, 2 complete)
```

- **Phase 4 (fs)** depends only on Phase 2 — the fs and threading tracks run
  concurrently, and can start now.
- Within Phase 3, ordering is forced: TLS (3.1) → **spawn+heap-lock (3.2)** → locks
  (3.3). The heap lock is coupled to the first spawn (3.2), not to the futex work
  (3.3) — see the 3.2 rationale.

---

## Findings-doc requirement

Every separately-implementable task above produces exactly one findings document at
`doc/results/<N>_<slug>_findings.md`, where `<N>` is the **Findings** number in the
phase tables (0-indexed) and `<slug>` is a short kebab-case descriptor. Inserted work
that was not a numbered sub-phase takes a **hyphenated** findings number (the `7-1`
gate + `7-2` verus-builtin-build precedent). The mapping (✅ = complete):

| N | Task | N | Task |
|---|---|---|---|
| 0 ✅ | 0.1 target JSON + build-std | 10 | 3.3 futex backend + locks |
| 1 ✅ | 0.2 vendor pin + allowlist + unsupported PAL | 11 | 3.4 entropy + HashMap |
| 2 ✅ | 1.1 `eunomia-sys` + syscall marshalling | 12 | 3.5 TLS key table |
| 3 ✅ | 1.2 verified startup decoder | 13 | 4.1 fs client |
| 4 ✅ | 2.1 entry + argv/env | 14 | 4.2 path decoder |
| 5 ✅ | 2.2 GlobalAlloc | 15 | 4.3 metadata + unsupported stubs |
| 6 ✅ | 2.3 stdio (debug-log) + exit terminus | 16 | 5.1 console stdio |
| 7 ✅ | 2.4 time | 17 | 5.2 process/env polish |
| 7-1 ✅ | Phase-2 gate (std smoke) | 18 | 5.3 rewrite a user binary |
| 7-2 ✅ | verus-builtin build (extern "Rust" bridge) | 19 | 6.1 on-target test triage |
| 8 | 3.1 TPIDR_EL0 | 20 | 6.2 fuzz corpora + PAL audit |
| 9 | 3.2 thread spawn/join + heap lock | 21 | 6.3 forward-port runbook + ledger |

Each findings doc records **everything worth keeping** — err on the side of too much;
there is a consolidation pass at the end. At minimum:

- **Decisions** taken and the alternatives rejected (with the reason), especially any
  deferred-work item settled or advanced during the task.
- **Problems** hit and how they were solved (a `TrapFrame` offset that drifted, a
  Verus trigger that backfired, an `ipc` handshake gap).
- **Verification record:** the exact gate command run and its result line (the
  `N verified, 0 errors` line, the fuzz run, the Loom/Shuttle outcome, the QEMU
  marker), plus any new ledger row/note and its host test.
- **Surface left unsupported or trusted** and *why* it could not be verified with the
  existing tools — the only sanctioned form of unverified code.
- **Follow-ups** discovered (new tasks, deferred work, debts).

Per `CLAUDE.md`, `doc/plans` and `doc/results` are temporary intermediate reports:
they may **not** be referenced from code comments, specs, or guidelines.

---

## Deferred work

Each item is intentionally out of scope for the phases above. It names what it would
build and the planned (MVP) implementation it replaces or upgrades; none blocks any
task.

- **Real entropy source — `RNDR` (`FEAT_RNG`) or virtio-rng.** *Replaces:* the
  documented-predictable MVP seed (task 11). `RNDR` is an EL0 instruction (no syscall,
  no DMA, seam-free) but needs `-cpu max` — the `cortex-a72` model lacks it; virtio-rng
  is cleaner but a new DMA/hardware trusted seam (ledger 14→15), with `RNDR` preferred.
  *Trigger:* the first untrusted input that reaches a `HashMap`'s SipHash keys. The
  swap leaves the `fill_bytes` DRBG and per-child-reseed contract unchanged; only the
  seed bytes' origin moves.

- **Growable heap — the `heap` named grant + `sbrk` via retype/map.** *Replaces:* the
  fixed `.bss` arena `urt::Heap<N>` (task 5). Adds a rev2§5.1 `heap` grant (a donation
  untyped, a name-id + producer-wiring change, no codec change) and grow glue that folds
  under the Store/aspace page-table-join seam (no new seam). *Trigger:* the first
  genuinely input-proportional consumer — `fs::read`/`read_to_string` of large files, or
  storaged at real overlay budgets (rev2§4.4). Until then, oversize the per-binary `N`.
  **Bootstrap-ordering hazard (record before implementing):** the `.bss` arena sidesteps
  this today because the allocator is live from instruction zero (`urt::Heap::new()` in
  `.bss`, mapped+zeroed by the loader), so `_start` can decode-then-build argv/env. If
  the grant supplies the *initial* heap, the allocator can no longer be up before the
  startup block is read — the child must decode the grant table to find the heap untyped,
  yet building argv/env `Vec`s needs the heap. There is **no true circularity** because
  the decoder is no-alloc: `loader::startup::decode` borrows the message buffer
  (`Startup<'_>` slices are verified subranges of `buf@`) and fills the grant table into
  a fixed arena without a heap. So the forced ordering is **recv → decode (no-alloc;
  locate the `heap` grant) → map/init the allocator → only then allocate**. This slots
  into the existing structure with no new plumbing: `eunomia_sys::bootstrap::init()`
  already runs recv → `commit` (no-alloc decode+stash) → `attach_grants()`, where
  `attach_grants()` acts on a located pre-mapped grant (today `urt::time::attach` for
  `NAME_TIME`); the heap-grant map becomes a sibling attach step run before `init()`
  returns — i.e. before `main` and before the first *lazy* `env::args()`/`vars()`
  allocation. **Corollary:** if the grant is used only to *grow* a small `.bss`
  bootstrap arena (sbrk top-up), the hazard does not arise — the grant is consulted only
  on grow, long after argv/env — so this ordering constraint binds only the
  initial-heap variant.

- **Power-efficient timeouts — timer-bit blocking.** *Replaces:* the MVP yield-poll
  inside `futex_wait(addr, expected, Some(d))` (task 10). The parker arms a per-thread
  `kcore::timer` (already verified: absolute-deadline `arm`, re-arm-replace, `disarm`,
  tick expiry) bound to its own notification on a dedicated `TIMER` bit, then
  `notif_wait`s and decodes `WAKE` vs `TIMER`. Wiring, not new kernel work; the
  wake-vs-timeout race (bucket-lock `still_enqueued` check) extends the futex
  Loom/Shuttle harness with a negative control, and the `Duration`→CNTVCT conversion is
  Verus (the `utc_ns_at` inverse). Resolution stays one tick (10 ms). *Trigger:* when
  busy-poll CPU cost matters.

- **Kernel futex / wait-set object (rev2§8.3).** *Replaces:* the userspace
  address→waiter emulation inside `eunomia_sys::futex` (task 10) with a kernel primitive.
  Because std only sees `sys::futex`, this is an internal backend swap — no PAL or std
  change (the futex backend was chosen partly to make this drop-in). **This is also the
  real fix for the cross-priority-level lock-safety boundary in 3.3:** the wait-set
  deschedules the waiter into the kernel, removing it from Runnable so a lower-priority
  holder can run, dissolving the priority inversion that the userspace bucket lock cannot.

- **Full FP/NEON userspace.** *Replaces:* the mandated softfloat ABI (Global
  decisions). Grow `kcore::thread::TrapFrame` to save/restore `q0–q31`/`fpsr`/`fpcr`,
  update every hand-coded save/restore offset in `kernel/src/exceptions.rs` in lockstep
  with a fresh `offset_of` const-assert, then flip the target JSON to hardfloat/`+neon`.
  Its own phase, far larger than the 3.1 `tpidr` bump; *do not* fold it into 3.1.
  *Trigger:* a userspace workload (graphics, numeric) that needs hardware FP/SIMD.

- **mtime on the wire.** *Replaces:* the `Unsupported` stub for
  `File::metadata().modified()` (task 15). mtime is a mandatory rev2§4.9 entry field
  already present in the `cas` entry; exposing it is a storage wire extension — a new
  `Response` variant + `PROTO_VERSION` bump — not a spec change. `set_times` stays
  unsupported (mtime is server-assigned).

- **Bulk-window data plane for fs.** *Replaces:* the client-side offset loops the fs
  client uses because the 256-byte `MAX_MSG` caps every reply (task 13). A shared-memory
  bulk window (rev2§3.1) carries large reads/writes and big directory listings in one
  transfer instead of looping.

- **Tier-3 upstreaming of `aarch64-unknown-eunomia`.** *Replaces:* out-of-tree
  maintenance of the PAL on the vendored fork, and retires the test-only cargo
  mechanisms the build currently leans on (`-Zjson-target-spec`,
  `__CARGO_TESTS_ONLY_SRC_ROOT`) — a target spec in `rustc_target` plus
  `sys/pal/eunomia` in-tree, so the forward-port runbook (task 21) tracks upstream
  directly.
