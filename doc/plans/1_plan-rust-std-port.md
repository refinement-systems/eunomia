# Plan — Porting the Rust Standard Library to Eunomia

> Targets `spec rev2`. Self-contained and grounded in the current tree (`urt`,
> `kcore`, `ipc`, `loader`, `storage-server`, `vendor/rust`).

## Starting point — what exists, what's missing

The port builds on a userspace runtime and kernel that already supply — and verify —
most of std's hard prerequisites.

**`urt` is the verified PAL spine.** `urt::Heap<N>` is a `#[global_allocator]`-ready
allocator whose arithmetic delegates to the Verus-verified `freelist::FreeList`
(`freelist` 30 verified; `urt` 25 verified; the arena byte-region is a trusted
plain-Rust seam kept honest by Miri+proptest, *not* one of the 14 ledger seams).
`urt::slots::SlotAlloc` (verified) is the TLS key-index allocator. `urt::time` is the
rev2§2.6 time page: a Loom-certified seqlock read composed with the Verus-verified
`Sample::utc_ns_at` tick→ns conversion (totality + monotonicity). So `GlobalAlloc`,
`Instant`, and `SystemTime` are verified-backed and ready to wire.

**The kernel side is nearly complete.** A `Yield` syscall exists
(`kcore/src/sysabi.rs` opcode 2 → `Sys::Yield`, decode proven total). The userspace
console driver (`user/console`) exists, and the shell already does all terminal I/O
over the console channel (`user/shell/src/runtime.rs` `out()` → `chan_send`, rev2§7 /
status C-M9), with `NAME_STDIN`/`NAME_STDOUT` grants emitted by init and consumed by
the shell. The only genuinely-missing kernel pieces are **`TPIDR_EL0` save/restore**
for real TLS (`grep -ri tpidr kernel/ kcore/` is empty) and an **entropy source** (no
random syscall or object exists anywhere in `kcore`/`kernel`).

**The startup-block decoder is the one piece of upfront verification.** It exists
(`loader/src/startup.rs`, versioned EUS1 codec, with a live fuzz target at
`loader/fuzz/fuzz_targets/startup.rs`) but is plain Rust — loader's 12 verified
obligations cover only `elf::parse` + `page_layout`. It is an untrusted-decode
boundary (rev2§3.7/§2.7) consumed in `_start` before the heap exists, so the
verification discipline requires it be verified before any PAL code leans on it
(Phase 1).

**The std PAL is per-module, not one directory.** `vendor/rust`'s `src/version` reads
`1.98.0`, but the tree carries the post-#117276 `sys/` reorg: each PAL surface
dispatches by `cfg_select!` in `library/std/src/sys/<module>/mod.rs`, with
`sys/pal/<os>/` reduced to a thin shell. The PAL is therefore **per-module arms** —
`grep -rln motor library/std/src/sys` is the authoritative file checklist, and `motor`
(Motūr-OS-delegates-to-`moto_rt`) is the structural twin of Eunomia-delegates-to-`urt`/
`ipc`. Confirm the exact upstream rustc commit and pin the std-build toolchain to it
before writing any file-level task.

---

## Global decisions

Made once, each with its rationale; deferred upgrades are collected in
[Deferred work](#deferred-work).

- **`panic = "abort"`.** Free here: `personality/mod.rs`'s `_` arm is a no-op (no
  `eh_personality`), so eunomia needs no personality file and no `panic_unwind`.
  Build with `panic-strategy=abort` + `-Zbuild-std=…,panic_abort`. **In a std binary
  the application cannot supply `#[panic_handler]`** — std owns it — so the
  `STATUS_PANIC = u64::MAX` reaper contract is preserved by overriding the **PAL's
  `abort_internal()`** (the `panic`→`process::abort`→`__rust_start_panic` terminus
  under panic=abort) **and the PAL's `exit()`** to call `thread_exit(STATUS_PANIC)` /
  `thread_exit(code)`, mirroring `sys/pal/motor/mod.rs`. The `unsupported` template
  wires `abort_internal` to `core::intrinsics::abort()`, which would **not** signal
  `STATUS_PANIC`, so these two overrides are mandatory, not inherited. The parent
  reaper (`urt::spawn::reap`) distinguishes a crash from `exit(0)` by that status.
- **`OsStr` is bytes** (the `_` default), matching rev2§4.9 byte-equality names —
  no WTF-8, no `os/eunomia/ffi.rs`.
- **Locks use a `sys::futex` backend, emulated in userspace over per-thread
  notifications.** This tree has **no generic parking-based Mutex/Condvar** —
  `mutex/mod.rs` and `condvar/mod.rs` select either the `futex` impl, a
  platform-specific impl, or `no_threads` (panics on contention); the `Parker` backs
  only `thread::park` and the `queue` RwLock/Once, **not** the locks. So `sys::futex`
  is the *only* primitive that lights up the whole stack
  (Mutex/Condvar/RwLock/Once/Parker) from upstream's already-correct impls — we write
  zero lock logic, just the four futex functions
  (`futex_wait`/`futex_wake`/`futex_wake_all` + the `u32` atomic types). (A parker-only
  backend would instead force hand-writing Mutex+Condvar as a bespoke platform backend
  — the sgx/xous pattern — more code, and discarded the day a futex lands.) `motor`
  (`sys/pal/motor/mod.rs` = `pub use moto_rt::futex;`, present in all five futex arms)
  is the exact template: a from-scratch OS delegating `sys::futex` to its runtime
  crate. The eunomia PAL arm is a one-line `pub use eunomia_sys::futex;`; the emulation
  — a process-global address→waiter table over `NotifSignal`/`NotifWait`, with a small
  bootstrap spinlock — lives in the gated crate. A later *kernel* futex/wait-set object
  (rev2§8.3) is an internal backend swap inside `eunomia_sys::futex`, invisible to std
  (see [Deferred work](#deferred-work)). The concurrent wakeup is irreducibly
  Loom/Shuttle (reusing the rev2§3.6 word-check-before-wait discipline the `IpcReactor`
  already models) — never Verus (the version-pinned Verus ghost atomics are SeqCst-only
  with no standalone fence, per `doc/guidelines/verification.md`; a proof would certify
  a different binary). `no_threads` locks are the Phase-2 single-threaded interim.
- **Entropy: a startup-block seed grant as the *mechanism*, with a documented,
  explicitly-non-cryptographic seed as the *MVP source*.** The seed rides the
  rev2§5.1 named-grant mechanism and its decode is covered by the Phase-1 verified
  startup parser, so the *plumbing* **adds no trusted seam**. But a seed grant only
  *distributes* entropy — it verifies the pipe, not the water — and the QEMU `virt`
  machine as currently configured (`-cpu cortex-a72`) offers init **no good source**:
  the PL031 RTC is explicitly predictable (rev2§2.6) and there is no virtio-rng. So the
  MVP source is **deliberately predictable**, and that is acceptable *only because
  today's HashDoS surface is thin* — the storage server keys directories in sorted
  prolly trees, not `HashMap`s (rev2§4.9), and the shell reads trusted interactive
  input, so no untrusted external input currently reaches a `HashMap`'s SipHash keys.
  It **must be disclosed loudly** as MVP-only and not-for-cryptography, because the
  failure is silent (predictable keys look identical to good keys, and the gate cannot
  catch it — randomness *quality* is not a verification property; only the seed decode
  is). The real entropy source is [deferred](#deferred-work). Two requirements hold
  regardless of source: `fill_bytes` is a per-process **DRBG seeded by the grant**,
  never a copy of the seed bytes (std's `fill_bytes` is infallible — a finite seed
  handed back raw repeats/exhausts silently); and a parent spawning children draws a
  **fresh sub-seed per child** from its own DRBG, never copies its own seed (the
  classic `fork()`-without-reseed trap).
- **`stderr` → `debug-log` for bring-up, then a capability-routed `NAME_STDERR`
  stream for production**, resolved as **`NAME_STDERR` if granted, else fall back to
  the `stdout` channel, else `debug-log`.** Folding stderr into stdout is rejected:
  it would break the very separation rev2§5.1 splits stdout/stdin *for* — in `a | b`
  it pipes `a`'s diagnostics into `b`'s stdin. Adding `NAME_STDERR` is cheap because
  it is a new name **id**, not a new grant **kind** (a `CapSlot` like `NAME_STDOUT`):
  it touches **no codec and no verified decoder** — just a constant, init pushing it,
  and the PAL resolving a third slot. The fallback rule keeps "just works on a
  terminal" (init grants the same console endpoint under both `stdout` and `stderr`,
  the rev2§5.1 "same channel under both names" pattern) while allowing a shell to wire
  `2>` / pipelines separately. **Panic last-words stay on the `debug-log`
  kernel-diagnostic path** (rev2§7's "kept … for panic reporting" clause), separate
  from the userspace stderr stream, so a wedged console can't swallow a panic.
- **The MVP heap is the fixed `.bss` arena** (`urt::Heap<N>`, no grow), chosen
  because the allocation algorithm is *verified* (`freelist`) and growth adds
  sbrk/retype/map glue. Understand what `N` costs: with no demand paging / COW / lazy
  zero-page mapping (rev2§5; all deferred in rev2§8.3), the loader commits real frames
  for the whole `.bss` at spawn — so **`N` is a reservation, not a ceiling: max ==
  committed RAM**, used or not, bounding concurrency against the `-m 256M` machine.
  Three caveats bite even early, so they are *disclosed bounds*, not surprises:
  **(a)** OOM is a **hard abort** (`alloc`→null→`handle_alloc_error`→abort), not a
  graceful `Err` — any input-proportional allocation can abort on large input, and
  `N` is fixed at compile time (route that abort through the PAL terminus so it reaps
  like a panic, not a raw fault); **(b)** the `FreeList<HEAP_RANGES = 1024>`
  fragmentation cap is a *second, independent* limit — a fragmenting long-lived
  workload can hit it before `N` is exhausted, and a `dealloc` at the cap **leaks**;
  **(c)** the heap is **single-threaded** (`unsafe impl Sync`, no lock) — Phase 3 must
  add a lock or per-thread arenas (and per-thread multiplies the reservation). Mitigate
  by keeping `N` a per-binary const tuned to each program's workload and over-sizing
  generously where RAM allows (cheap at MVP scale). Note std raises the baseline:
  **moving the shell to std (5.3) needs a larger `N` than its current `no_std` 1 MiB.**
  Heap growth is [deferred](#deferred-work).
- **The verified startup decoder lives in `loader`** (least churn, reuses the live
  fuzz target + corpus); `eunomia-sys` depends on `loader::startup`.
- **`net` is permanently `unsupported`** (non-goal, rev2§8.1). `sys/net/connection`
  has a `_`→unsupported fallback, so eunomia needs **no file** — stated here so it is
  not read as an unfilled hole.
- **`process::Command` stays thin/`unsupported`.** Expose a native, capability-rich
  spawn API instead of emulating fork/exec. The shell keeps its spawn/reap on raw
  `loader::spawn`/`urt::spawn` even after moving its allocator/clock/stdio/fs to std.

---

## Verification discipline (normative)

This section governs the whole port: **verification discipline is upheld at all
times. No unverified code is written with intent to replace it later. Verified code
is written up front. The only unverified code is where existing tools provably
cannot reach.** It routes per `doc/guidelines/verification.md` and extends the
trusted-base ledger (`doc/guidelines/verus_trusted-base.md`) per its own §11
admission rule.

### The resolving principle

The `vendor/rust` PAL — `sys/pal/eunomia/mod.rs` plus the per-module
`sys/<module>/eunomia.rs` `cfg_select!` arms — is **necessarily a trusted shell**,
and that is the *exact* posture `kernel/` holds over the verified `kcore`: thin,
term-for-term dispatch and marshalling over a verified core. This does not violate
the no-unverified-code constraint because:

1. **The PAL holds zero genuinely-new logic.** Every non-trivial function delegates
   term-for-term to a gated crate (`urt`, `eunomia-sys`, `ipc`, `kcore`). It is
   auditable by inspection against `pal/unsupported`.
2. **The verus gate runs on the project crates, not on `vendor/rust`.**
   `vendor/rust` is a submodule fork that *by construction never runs
   `cargo verus verify`* — exactly as `kernel/`'s asm context switch never does.

The constraint's escape hatch — *unverified only where tools provably cannot reach*
— is then satisfied **precisely**, by these irreducible categories and nothing else:

- **inline asm** — both the kernel asm context switch / `TPIDR_EL0` save-restore
  *and* the userspace `svc #0` syscall-trap + register-marshalling wrappers in
  `eunomia-sys` (the userspace mirror of the kernel-side trusted register
  marshalling; inherently unverifiable, rev2§6.1(d));
- the concurrent wakeup path (the emulated-futex address→waiter dispatch + its
  bootstrap spinlock, over notifications) — SeqCst-pin infeasible in Verus;
  Loom/Shuttle-of-record + the existing `IpcReactor` TLA model;
- any `virtio-rng` device seam (DMA/hardware, rev2§2.5) — *not in the MVP*; the
  documented-predictable seed and the `RNDR`/`-cpu max` upgrade both avoid it.

**Everything else is verified on arrival**, never stubbed-then-replaced. Note the
split inside the syscall layer: the *pure byte-level arg encode/decode* is Verus
(`eunomia-sys`, verified surface); only the `svc` instruction and the
register-file marshalling around it are the trusted inline-asm shell above.

### Where new logic lives

| Sink | What goes there |
|---|---|
| **`urt`** (extend; 25 verified + `freelist` 30 transitively) | heap algorithm (done), slot/TLS-key index allocation (`SlotAlloc`, done), time conversion (`utc_ns_at`, done), the TLS key-table layer (new, verified surface) |
| **`eunomia-sys`** (NEW gated crate; joins the verus gate) | syscall arg encode/decode marshalling, io-error mapping; depends on the verified `loader::startup` decoder and `le-bytes` readers |
| **`loader`** (extend) | the startup-block decoder lifted into `verus!{}` (Phase 1) |
| **`vendor/rust` PAL** | **nothing but term-for-term delegation** — no arithmetic, no parsing, no business logic |

### Per-piece routing

| std logic | Routing | New seam? |
|---|---|---|
| Startup-block decoder (argv/env/EUS1 grants) | **Verus** (total ∀ bytes, mirror `elf::parse`) + **cargo-fuzz** (live corpus) | No — verified surface in `loader` |
| Syscall arg **byte** encode/decode | **Verus** (`sysabi::decode` precedent) | No — verified surface in `eunomia-sys` |
| `svc #0` + register-file marshalling | **trusted inline-asm shell** | No — folds under syscall-dispatch shell (d) |
| GlobalAlloc glue | **Verus** (algorithm in `freelist`, green) | No |
| GlobalAlloc arena byte-region + `sbrk` grow | **Miri**+proptest (region, green); grow **folds under the Store/aspace page-table-join seam (c)** | No — region is *not* one of the 14 (ledger scope note) |
| TLS — `TPIDR_EL0` save/restore | **trusted-shell + ledger routing-note** (under asm-context-switch shell (d)) | No — touches no Verus obligation (`TcbView` omits the register frame) |
| TLS — key table | **Verus** (over the verified `SlotAlloc`) | No — verified surface; the per-thread block uses the verified heap |
| `sys::futex` emulation — bucket spinlock (raw atomic + fence) | **Loom** (certifying — the load-bearing tool only where a raw atomic+fence lives) | No |
| `sys::futex` emulation — address→waiter dispatch over notifications | **Shuttle** (thread-interleaving); **reuse** `tla/ipc_reactor` + its 3 negative controls | No |
| Mutex / Condvar / RwLock / Once / Parker (upstream futex impls) | **none new** — verified by upstream over `sys::futex`; covered transitively by the two rows above | No — we write no lock logic |
| stdio sinks | **trusted-shell** (marshalling over verified `ipc` Admission/reactor) | No |
| fs marshalling | **Verus** (path-component decode) + **cargo-fuzz**; rights lattice + `check_header` already verified | No — path decode is verified surface |
| time (Instant/SystemTime) | **Verus** (`utc_ns_at`, green) + **Loom** (seqlock read, green) | No |
| entropy seed decode (likely a new inline-bytes grant kind) | **Verus** + **cargo-fuzz** (extends the task-3 startup-parser obligation + corpus) | No (seed-grant); the DRBG/quality is *not* verified — only the decode is |
| process exit / abort | **trusted-shell** (already in thread-lifecycle (d)) | No |
| io-error decode | **proptest** (total policy map; Verus if it becomes byte-parsing) | No |

### Trusted-base ledger changes (`doc/guidelines/verus_trusted-base.md`)

The ledger is **14 named seams**. Every row/note must name *both* a reason and a
host test (its §11 admission rule). The port changes it as follows; the tally
**stays 14** for the MVP:

- **New verified Baseline row — `eunomia-sys`** (`cargo verus verify -p eunomia-sys`):
  syscall marshalling + io-error map; paired cargo-fuzz where any byte-decode appears.
  Plus the startup decoder lands as new verified obligations on the existing **`loader`
  Baseline row** (count rises from 12). Not seams — verified surface.
- **New scope note (not a seam):** `vendor/rust` is a submodule fork that by
  construction never runs `cargo verus verify`; the PAL's absence from the gate is
  the same posture as `kernel/` over `kcore`. (Prevents a reviewer reading the
  missing gate as a violation.)
- **Three folding routing-notes (tally stays 14 — the IRQ-delivery-shell precedent):**

  | Folds under | Reason | Host test |
  |---|---|---|
  | `TPIDR_EL0` save/restore → thread-lifecycle / asm-context-switch shell (d) | Register frame is outside `verus!{}`; `TcbView` omits it, so no obligation moves | Boot two threads sharing an aspace; each reads a distinct TLS marker (m1-test style) |
  | `yield` → scheduler-stays-trusted (d) | Thin shell over the trusted scheduler (op 2 already exists) | QEMU boot exercising `thread::yield_now` / spin-before-park |
  | `sbrk`/heap-grow → Store/aspace page-table-join (c) | Retype+map glue over the already-trusted join | Existing aspace top-up host test (rev2§2.5 "accepts top-ups") |

- **New TLS-key-table consideration:** the `urt` key table is verified surface over
  `SlotAlloc`; the per-thread storage block sits over the verified heap. If any
  irreducible plain-Rust pointer step remains (a TPIDR-base + offset read), it folds
  under (d) as a routing note with a host test — not a 15th seam.
- **Conditional seam (14→15) — only if `virtio-rng` is later chosen** as the entropy
  *source* upgrade ([Deferred work](#deferred-work)): a real DMA/hardware row with
  reason + device bring-up host test. The MVP (documented-predictable seed) and the
  `RNDR`/`-cpu max` upgrade both **avoid the seam** (`RNDR` is an EL0 instruction, no
  DMA); only virtio-rng raises the tally.

### What keeps the shell honest

There is **no automated gate** proving PAL thinness, so honesty rests on three
things: (1) a **thinness rule** — the PAL contains zero new logic, enforced by review
of the PAL diff vs `pal/unsupported`, applying the §11 **inverse-leak rule** (the PAL
must re-establish every `eunomia-sys`/`urt` `requires` — alloc bounds, slot capacity,
buffer-belongs-to-pool — at the boundary or runtime-guard it); (2) **host tests** —
each seam and folding-note names one; (3) **the ledger** — a row that cannot name a
reason *and* a host test is a finding.

Because there is no automated gate, the thinness + inverse-leak check is a
**per-task gate**, not an end-of-project sweep: **every PAL-touching task** (2.1, 2.2,
2.3, 2.4, 3.2, 3.5, 4.1, 4.3, 5.1, 5.2) reviews its own arm against `pal/unsupported`
and records, in that task's findings doc, that the arm adds zero new logic and
re-establishes every verified `requires` at the boundary. Phase 6.2 is then the
**consolidating** sweep over the whole PAL, not the first time the rule is applied —
so unverified logic cannot accumulate across phases unchecked.

---

## Capability map (std surface → Eunomia)

Three arms are **mandatory** even for hello-world — `sys/alloc`, `sys/random`,
`sys/io/error`. `alloc/mod.rs` and `io/error/mod.rs` have **no `_` arm** at all, so a
missing eunomia arm is a compile error directly. `random/mod.rs` *does* have an empty
`_ => {}` arm, but is equally mandatory for a subtler reason: `hashmap_random_keys`
is imported **unconditionally** (`hash/random.rs`) and resolves to `fill_bytes`,
which the empty `_` arm does not provide — so the link still fails without an arm.
Everything else has a `_`→unsupported and can ship unsupported first.

| std surface | Backed by | Readiness | New work |
|---|---|---|---|
| **alloc (GlobalAlloc)** | `urt::Heap<N>` over verified `freelist` | **ready** | `sys/alloc/eunomia.rs` shim (mandatory). `N` is a **reservation** (no demand paging → committed at spawn), per-binary const. Disclose: single-thread no-lock, `MAX_ALIGN=64` (page-aligned requests → null = clean OOM), frag-cap 1024 (2nd limit; dealloc-at-cap leaks), **OOM = abort not `Err`** (route through the PAL terminus) |
| **time: SystemTime** | `urt::now_utc_ns` (verified `utc_ns_at`, Loom seqlock) | **ready** | Wire `SystemTime::now`; handle the no-time-grant panic (error path or attach invariant) |
| **time: Instant** | `CNTVCT_EL0` direct (zero-syscall) + verified conversion | **partial** | `sys/time/eunomia.rs` Instant over `cntvct/cntfrq` (trivial) |
| **process: exit/abort** | `ThreadExit(15)` → verified `report_terminal` | **ready** | Override the PAL `exit()`/`abort_internal()` (std owns the panic handler) to `thread_exit(code)`/`thread_exit(STATUS_PANIC = u64::MAX)`. `Command` thin/unsupported |
| **panic/unwind** | panic=abort (no `eh_personality`) | **ready** | Target JSON + build-std flags only |
| **env / args** | `loader::startup::decode` of the slot-0 boot message | **partial** | Verify the decoder (Phase 1); `sys/args`+`sys/env` arms. `env::vars` empty until a producer emits env entries |
| **thread: spawn/join/yield/sleep** | `Retype→Thread` + `ThreadStart(13/18)`; `ThreadExit(15)`; join via `ThreadBind(21,on-exit)`+`NotifWait(12)`+`ReadReport(22)`; **`Yield`=op 2**; sleep `TimerArm(14)`+`NotifWait` | **partial** | `urt` in-process thread-spawn primitive (only one scalar arg `x0` — a boxed `FnOnce` ptr fits); fund stack+guard (rev2§5.3). `urt::spawn` has **zero Verus/host coverage** today — net-new test surface |
| **sync: Mutex/Condvar/RwLock/Once/Parker** | `sys::futex` emulated over `NotifSignal(11)`/`NotifWait(12)` (verified core) → upstream's futex lock impls | **partial** | `eunomia_sys::futex` (4 fns) + committed Loom/Shuttle harness reusing `tla/ipc_reactor`; the locks come free from std. Timeouts: MVP yield-poll (`Instant`+`yield_now` → `*_timeout` work, busy); power-efficient timer-bit blocking is [deferred](#deferred-work) |
| **stdio** | (bring-up) `DebugWrite(1)`; (real) console channel over `ipc` | **partial** out / **missing** stdin | `sys/stdio/eunomia.rs` arm; `stdin` op deliberately unassigned (rev2§7) → must read the console channel. `NAME_STDIN`/`NAME_STDOUT` are **already emitted by init** (both → the console slot) and consumed by the shell, so only the std-side wiring remains, not grant delivery. stderr: **add `NAME_STDERR`** (new name id, no codec change), resolve `NAME_STDERR` → stdout → debug-log; panic last-words stay on debug-log |
| **thread_local / TLS** | `TPIDR_EL0` (absent) + `urt::slots` index alloc (verified) | **missing** | `TPIDR_EL0` save/restore (Phase 3) + `urt` TLS key table + `sys/thread_local` arm |
| **fs: File/read_dir/metadata** | storage-server openat session (handle-relative, component paths) | **partial** | `sys/fs/eunomia.rs` client + path decode; `File=(HandleId, TreePath, client offset)`; 256-byte `MAX_MSG` → client offset loops |
| **hashmap_random_keys** | nothing — **hard blocker** | **missing** | seed-grant + `sys/random/eunomia.rs` (mandatory). `fill_bytes` = per-process **DRBG** over the seed (not a raw copy); MVP seed is documented-predictable (non-crypto); per-child fresh sub-seeds |
| **net** | nothing, by design | n/a | none — permanently unsupported |

**fs surface that is `Unsupported` by construction** (rev2§4.9 has none of it):
symlink / hard_link / read_link / canonicalize; permissions / chmod / chown
(authority is the cap rights mask, not mode bits); `accessed` / `created` (no atime,
rev2§4.9); `set_len` / truncate; `create_dir` of empty dirs (creation is a side
effect of `Write`); `DirEntry::ino`, nlink/uid/gid; `current_dir`/`set_current_dir`
as syscalls (handle-relative bookkeeping; no ambient cwd, rev2§4.9). Cross-subtree
rename is `EXDEV` by construction.

`modified()` / mtime is **supportable but deferred** — a mandatory rev2§4.9 entry
field absent from the current wire protocol; return `Unsupported` for now and see
[Deferred work](#deferred-work). `set_times` stays unsupported (mtime is
server-assigned, not client-set).

---

## Spec & kernel changes

The verification posture is `kernel/`-over-`kcore` throughout. **Net kernel-track
surface: one change (`TPIDR_EL0`)** — the entropy MVP adds no kernel work (the
seed-grant mechanism is a spec-convention/decoder change and the MVP source is
documented-predictable init code); a real entropy *source* is [deferred](#deferred-work).

| # | Change | Kind | Where | Trust posture | Still needed? |
|---|---|---|---|---|---|
| 1 | Yield syscall | — | `kcore/src/sysabi.rs` (opcode 2) | verified decode + trusted scheduler shell | **No — already exists (op 2); no change** |
| 2 | `TPIDR_EL0` save/restore | trusted-kernel-shell | `kcore/src/thread.rs` `TrapFrame` (outside `verus!{}`) + `kernel/src/exceptions.rs`, `main.rs`, `syscall.rs` | trusted asm shell (rev2§6.1d); ledger *routing note*, no new seam | **Yes — the one genuine kernel-track touch** |
| 3 | Entropy: seed-grant **mechanism** | spec-convention + verified-decode | rev2§5.1 table (likely a new inline-bytes grant kind → touches the verified decoder, task 3); loader/`eunomia-sys` startup parser | no new seam; decode is Verus+fuzz | **Yes — the delivery mechanism** |
| 3a | Entropy MVP **source**: documented-predictable | none (init seeds from RTC/`CNTVCT`) | init seed generator (trusted shell) | no new seam; **explicitly non-cryptographic**, disclosed MVP-only | **Yes — MVP default** |
| 4 | Console stdio | marshalling only | std PAL over `chan_send`/`chan_recv` resolving the slots from the already-delivered grant table | no new verified logic; driver + shell path + `NAME_STDIN/STDOUT` grants already exist | **Mostly done — only the std-side stdio wiring** |
| 5 | stderr name | spec-convention + marshalling | **add `NAME_STDERR`** to the rev2§5.1 table (new name id, `CapSlot` — **no codec/decoder change**) | `debug_write` bring-up; capability-routed `NAME_STDERR`→stdout→debug-log fallback for production; panic last-words stay on debug-log | **Yes — add the name (folding rejected: breaks pipeline separation)** |

The real entropy *source* (`RNDR` / virtio-rng) and the `heap` named grant for
growable heaps are [deferred](#deferred-work).

**`TPIDR_EL0` detail.** `TrapFrame` (`kcore/src/thread.rs`, `repr(C)`, **outside**
`verus!{}`) is `{x:[u64;31], sp_el0, elr, spsr}` = **272 bytes**; `ThreadStart`
writes `elr/sp_el0/spsr/x0` only. The change touches **no Verus obligation** (the
verified `TcbView` models no register frame). Edits, all trusted shell:
(1) add a `tpidr` field, growing the struct **272 → 288** with a pad word (280 is
not 16-aligned); (2) `mrs/msr tpidr_el0` in `el0_entry`/`el0_restore` and bump the
hand-coded `sub/add sp,#272` and every `stp` offset in `kernel/src/exceptions.rs` in
lockstep; (3) zero-init at `enter_first_thread` + `ThreadStart`/`ThreadStartAs`.
Add an `offset_of` **const-assert** coupling the asm offsets to the struct (none
exists today — a stale offset silently corrupts `eret`). Re-run
`cargo verus verify -p kcore` and confirm the count is unchanged (it should be).

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

### Phase 0 — Toolchain & target
*No findings dependency; Phase 1.2 may run in parallel.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **0.1** | The target exists; build-std carries std | `aarch64-unknown-eunomia.json` from `aarch64-unknown-none` (`os=eunomia`, `panic-strategy=abort`, softfloat inherited); edit the single point in `kernel/build.rs` → `-Zbuild-std=core,compiler_builtins,alloc,std,panic_abort` (`build.rs` scrubs `RUSTFLAGS`, so thread custom flags through it) | `core`+`alloc` build for the target; a `no_std` binary boots in QEMU and prints via `debug-log` | **0** |
| **0.2** | std knows the target | Confirm the exact upstream rustc commit for `vendor/rust`; pin the std-build toolchain to it; add `\|\| target_os == "eunomia"` to the `library/std/build.rs` `restricted_std` allowlist; copy `sys/pal/unsupported` → `sys/pal/eunomia/mod.rs`. `grep -rln motor library/std/src/sys` for the per-module arm checklist | `std` compiles all-unsupported; `fn main(){}` links | **1** |

### Phase 1 — Seam crate + verified startup decoder
*The upfront verification phase. 1.2 has no Phase-0 dependency — start it immediately.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **1.1** | The PAL↔OS seam | `eunomia-sys` gated crate: raw `svc #0` wrappers (trusted asm shell), named-grant lookup, **Verus-verified** syscall arg marshalling (the `sysabi::decode` precedent). New ledger Baseline row + `external_body` audit + verusfmt/`cargo fmt` posture | `cargo verus verify -p eunomia-sys` (results line present, 0 errors) | **2** |
| **1.2** | **The one real upfront proof** | Lift `loader::startup::decode` into `verus!{}` with a total ∀-bytes contract mirroring `elf::parse`'s `well_formed_image`: never panics / reads OOB; `ngrants ≤ MAX_GRANTS`, `nargv ≤ MAX_ARGV`, `nenv ≤ MAX_ENV`; every borrowed argv/env subrange ⊆ `buf@`. Replace the hand-rolled `Reader` with the verified `le_bytes` readers. Verified **on arrival**, not stubbed-then-promoted | `cargo clean -p loader && cargo verus verify -p loader` (count rises from 12) + `cargo fuzz run startup` + `--test fuzz_corpus`/`fuzz_regressions` under Miri | **3** |

### Phase 2 — Hello-world, single-threaded ⭐
*Depends on Phase 1. Clears the GlobalAlloc blocker; TLS deferred to the
global-statics fallback. Time is pulled in here — it is free.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **2.1** | Entry + argv/env | non-crt0 `_start`→`lang_start`→`main` reads the slot-0 bootstrap channel's first message, calls the **verified** decoder (1.2); `sys/args`, `sys/env`, and `sys/io/error` (mandatory) arms; io-error map proptested | links; `env::args` visible in QEMU | **4** |
| **2.2** | GlobalAlloc | `sys/alloc/eunomia.rs` over `urt::Heap<N>` (algorithm verified in `freelist`; arena Miri+proptest). Mandatory arm. Pick a per-binary `N` (reservation, committed at spawn). Disclose MVP bounds (reservation-not-ceiling, OOM=abort, frag-cap 1024 leak, single-thread); confirm `handle_alloc_error`'s abort routes through the PAL terminus (reaps like a panic, not a raw fault) | `cargo verus verify -p urt -p freelist` (green, re-cited) + `urt` Miri sweep | **5** |
| **2.3** | stdio (bring-up) + exit terminus | `sys/stdio/eunomia.rs` stdout/stderr → `DebugWrite(1)` (len ≤ 1024); `panic_output` same path. **Disclose** this EL0 use of the debug-log path as a *temporary §2 deviation* (the pre-console-shell precedent, rev2§7) — replaced for stdout/stdin by the console channel in 5.1, retained only for panic last-words. **Override the PAL `abort_internal()` and `exit()`** to `thread_exit(STATUS_PANIC)` / `thread_exit(code)` (the `motor` template — *not* the `unsupported` `intrinsics::abort()`), preserving the reaper contract for a std binary | boot prints `println!`; a panicking std binary reaps as `STATUS_PANIC` | **6** |
| **2.4** | Time (free) | `Instant` ← `cntvct/cntfrq`; `SystemTime` ← `urt::now_utc_ns` (verified `utc_ns_at`, Loom seqlock); resolve the no-time-grant panic | `cargo verus verify -p urt` (re-cited); `Instant::now`/`SystemTime::now` work | **7** |
| **GATE** | CI smoke | green-boot marker (`…M1 PASS`-style) + kill-cleanly harness (background QPID + trap + deadline-poll, per `CLAUDE.md`) in the on-os CI job; asserts `println!`/`format!`/`Vec`/`Box`/`String`/`Instant`/`SystemTime` | QEMU boot smoke green | — |

### Phase 3 — TLS + threading + locks + entropy + HashMap
*The only kernel-track phase. Parallelizable with Phase 4. Internal order forced:
3.1 → 3.2 → 3.3; 3.4/3.5 independent once 3.1 lands.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **3.1** | Real TLS (kernel) | `TPIDR_EL0` save/restore: `tpidr` in `TrapFrame` (272→288 + pad), `mrs/msr` in `exceptions.rs`, grow `sub/add sp,#272`, seed at start; **`offset_of` const-assert** | `cargo clean -p kcore && cargo verus verify -p kcore` re-passes **406/0** (unchanged); host test: 2 threads share an aspace, read distinct TLS markers; ledger routing-note added | **8** |
| **3.2** | spawn/join/yield/sleep | `urt` in-process thread-spawn primitive (`Box<ThreadInit>` ptr → `x0`); `sys/thread/eunomia.rs` (motor template); stack+guard (rev2§5.3); `yield_now` = op 2; sleep = `TimerArm`+`NotifWait` | QEMU spawn smoke + **new host tests** for the `urt::spawn` invariants (bind-before-start, read-report-before-revoke) — currently uncovered | **9** |
| **3.3** | Locks | `eunomia_sys::futex` emulated over notifications (the 4 futex fns + types; address→waiter table + bootstrap spinlock); add `eunomia` to the five `sys/sync/*/futex.rs` arms + `pub use eunomia_sys::futex;` in the PAL → Mutex/Condvar/RwLock/Once/Parker come free; lift the heap single-thread no-lock assumption (lock or per-thread arenas). Timeouts: MVP **yield-poll** `futex_wait(Some(d))` (`Instant`+`yield_now`, correct but busy → `wait_timeout`/`park_timeout` work); timer-bit blocking is [deferred](#deferred-work) | **Loom** (certifying, the bucket spinlock) + **Shuttle** (breadth, the dispatch) green, **reusing `tla/ipc_reactor`** + its 3 negative controls — **never Verus** (SeqCst pin) | **10** |
| **3.4** | Entropy + HashMap | startup-block seed grant (likely a new inline-bytes grant kind → extend the task-3 verified decoder + fuzz corpus); init seeds it MVP-predictable (documented non-crypto); `sys/random/eunomia.rs` (mandatory arm) where `fill_bytes` is a **per-process DRBG** over the seed (zeroize the seed after init), and a parent draws a **fresh sub-seed per child**; define the **no-seed behavior** (recommend loud abort at first use, the `now_utc_ns` precedent — `fill_bytes` is infallible, so the alternative is silent predictability); unblock `HashMap` `RandomState` | seed decode Verus+fuzz (rides 1.2); `HashMap` works under smoke; findings doc records the MVP-predictable disclosure + the deferred real-source path | **11** |
| **3.5** | TLS keys | `urt::tls` key table over the verified `SlotAlloc` + per-thread block over the verified heap; `sys/thread_local` arm | `cargo verus verify -p urt` (key-table obligations green); host test | **12** |

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
| **5.1** | console stdio + stderr | move `stdout`/`stdin` off `debug-log` onto the `user/console` channel via `ipc` (reactor poll-once loop = the validated `reactor_no_lost_wakeup` harness; `send_blocking` for write backpressure). The std PAL resolves the stdin/stdout cspace slots from the startup grant table — `NAME_STDIN/STDOUT` are **already emitted by init and consumed by the shell today**, so no grant-delivery change is needed. **Add `NAME_STDERR`** (new name id, `CapSlot`, no codec change) and resolve stderr as **`NAME_STDERR` → else stdout channel → else debug-log**; init grants the same console endpoint under both `stdout` and `stderr` for a terminal. **Keep panic last-words on `debug-log`** (kernel-diagnostic, separate from the userspace stderr stream) | Shuttle reactor harness green; QEMU interactive stdio + a piped `a \| b` where `a`'s stderr does not corrupt `b`'s stdin | **16** |
| **5.2** | process/env | `process::exit(code)` → PAL `exit()` → `ThreadExit` `exited(status)`; `abort` → PAL `abort_internal()` → `thread_exit(STATUS_PANIC)` (both overridden in 2.3, re-confirmed here); `Command` thin/unsupported; `temp_dir` → `tmp` grant; `current_dir`/`set_current_dir` handle-relative or unsupported; populate env entries (producer side) | smoke | **17** |
| **5.3** | rewrite a user binary | `hello` on std first (validates entry/argv/alloc/exit/`STATUS_PANIC`), then `shell` (alloc + `SystemTime`/`Instant` + console stdio + args + fs) — **keep spawn/reap on raw `loader::spawn`/`urt::spawn`** (std::process can't model it yet) | extended boot smoke exercising fs + console stdio | **18** |

### Phase 6 — Hardening & forward-port discipline

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **6.1** | on-target test triage | run `libcore`/`liballoc`/`libstd` test subsets on-target via QEMU; triage + record skips (eunomia-only std crates can't host-build — excluded like `kernel`) | QEMU test run; skip log committed | **19** |
| **6.2** | fuzz + PAL audit | grow committed fuzz corpora for the verified decoders; **PAL thin-delegator audit** — diff `sys/pal/eunomia` (+ the `eunomia.rs` arms) vs `pal/unsupported`, confirming zero new logic and that every verified `requires` is re-established or runtime-guarded at the boundary (the §11 inverse-leak rule). *This review is the standing gate for the thinness rule* | cargo-fuzz green; audit recorded | **20** |
| **6.3** | runbook + ledger | forward-port runbook (pinned-nightly bump cadence, the diff surface, regression set); record the deliberate Verus-pin ↔ std-version **decoupling** (std is not Verus-verified; `vendor/rust` never runs the gate); finalize the ledger (new `eunomia-sys` Baseline row; `loader` count update; folding notes for TPIDR/sbrk-grow; no yield row — op 2 exists; TLS-key note; entropy decision) | ledger consistent; runbook committed | **21** |

### Dependency & parallelism map

```
Phase 0 ─ 1.1 ─ Phase 2 ─ Phase 3 (kernel track) [3.1 TLS → 3.2 spawn → 3.3 locks; 3.4/3.5]
   │       │        └───── Phase 4 (fs) ── parallel with Phase 3; needs only Phase 2 ─┐
   └── 1.2 (verified decoder, parallel with Phase 0) ── feeds 2.1                       ├─ Phase 5 ─ Phase 6
                                                                                        ┘
```

- **1.2** (the upfront proof) starts immediately, parallel with Phase 0.
- **Phase 4 (fs)** depends only on Phase 2 — the fs and threading tracks run concurrently.
- Within Phase 3, ordering is forced: TLS (3.1) → spawn (3.2) → locks (3.3).

---

## Findings-doc requirement

Every separately-implementable task above produces exactly one findings document at
`doc/results/<N>_<slug>_findings.md`, where `<N>` is the **Findings** number in the
phase tables (0-indexed, matching the `0_findings.md`, `1_findings.md`, …
convention) and `<slug>` is a short kebab-case descriptor (the `13_verus-findings.md`
precedent). The mapping:

| N | Task | N | Task |
|---|---|---|---|
| 0 | 0.1 target JSON + build-std | 11 | 3.4 entropy + HashMap |
| 1 | 0.2 vendor pin + allowlist + unsupported PAL | 12 | 3.5 TLS key table |
| 2 | 1.1 `eunomia-sys` + syscall marshalling | 13 | 4.1 fs client |
| 3 | 1.2 verified startup decoder | 14 | 4.2 path decoder |
| 4 | 2.1 entry + argv/env | 15 | 4.3 metadata + unsupported stubs |
| 5 | 2.2 GlobalAlloc | 16 | 5.1 console stdio |
| 6 | 2.3 stdio (debug-log) | 17 | 5.2 process/env polish |
| 7 | 2.4 time | 18 | 5.3 rewrite a user binary |
| 8 | 3.1 TPIDR_EL0 | 19 | 6.1 on-target test triage |
| 9 | 3.2 thread spawn/join | 20 | 6.2 fuzz corpora + PAL audit |
| 10 | 3.3 futex backend + locks | 21 | 6.3 forward-port runbook + ledger |

Each findings doc records **everything worth keeping** — err on the side of too
much; there is a consolidation pass at the end. At minimum:

- **Decisions** taken and the alternatives rejected (with the reason), especially
  any deferred-work item ([Deferred work](#deferred-work)) settled or advanced during
  the task.
- **Problems** hit and how they were solved (e.g. a `TrapFrame` offset that
  drifted, a Verus trigger that backfired, an `ipc` handshake gap).
- **Verification record:** the exact gate command run and its result line (the
  `N verified, 0 errors` line, the fuzz run, the Loom/Shuttle outcome, the QEMU
  marker), plus any new ledger row/note and its host test.
- **Surface left unsupported or trusted** and *why* it could not be verified with
  the existing tools — the only sanctioned form of unverified code.
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
  *Trigger:* the first untrusted input that reaches a `HashMap`'s SipHash keys — until
  then the predictable seed is acceptable. The swap leaves the `fill_bytes` DRBG and
  per-child-reseed contract unchanged; only the seed bytes' origin moves.

- **Growable heap — the `heap` named grant + `sbrk` via retype/map.** *Replaces:* the
  fixed `.bss` arena `urt::Heap<N>` (task 5). Adds a rev2§5.1 `heap` grant (a donation
  untyped, a name-id + producer-wiring change, no codec change) and grow glue that folds
  under the Store/aspace page-table-join seam (no new seam). *Trigger:* the first
  genuinely input-proportional consumer — `fs::read`/`read_to_string` of large files, or
  storaged at real overlay budgets (rev2§4.4, where a few-MiB fixed heap fights the
  8 MiB/ref · 128 MiB-global defaults). Until then, oversize the per-binary `N`.

- **Power-efficient timeouts — timer-bit blocking.** *Replaces:* the MVP yield-poll
  inside `futex_wait(addr, expected, Some(d))` (task 10). The parker arms a per-thread
  `kcore::timer` (already verified: absolute-deadline `arm`, re-arm-replace, `disarm`,
  tick expiry) bound to its own notification on a dedicated `TIMER` bit, then
  `notif_wait`s and decodes `WAKE` vs `TIMER`. Wiring, not new kernel work; the
  wake-vs-timeout race (bucket-lock `still_enqueued` check — else a `futex_wake` consumes
  a departed waiter = lost wakeup) extends the futex Loom/Shuttle harness with a negative
  control, and the `Duration`→CNTVCT conversion is Verus (the `utc_ns_at` inverse).
  Resolution stays one tick (10 ms). *Trigger:* when busy-poll CPU cost matters.

- **mtime on the wire.** *Replaces:* the `Unsupported` stub for
  `File::metadata().modified()` (task 15). mtime is a mandatory rev2§4.9 entry field
  already present in the `cas` entry; exposing it is a storage wire extension — a new
  `Response` variant + `PROTO_VERSION` bump — not a spec change. `set_times` stays
  unsupported (mtime is server-assigned).

- **Bulk-window data plane for fs.** *Replaces:* the client-side offset loops the fs
  client uses because the 256-byte `MAX_MSG` caps every reply (task 13). A shared-memory
  bulk window (rev2§3.1) carries large reads/writes and big directory listings in one
  transfer instead of looping.

- **Kernel futex / wait-set object (rev2§8.3).** *Replaces:* the userspace
  address→waiter emulation inside `eunomia_sys::futex` (task 10) with a kernel primitive.
  Because the std side only sees `sys::futex`, this is an internal backend swap — no PAL
  or std change. The futex backend was chosen partly to make this drop-in.

- **Tier-3 upstreaming of `aarch64-unknown-eunomia`.** *Replaces:* out-of-tree
  maintenance of the PAL on the vendored fork — a target spec in `rustc_target` plus
  `sys/pal/eunomia` in-tree, so the forward-port runbook (task 21) tracks upstream
  directly.
