# Plan — Porting the Rust Standard Library to Eunomia

> Targets `spec rev2`. Supersedes `doc/plans/0_draft-plan-rust-std-port.md`, which
> was written from the spec alone; this revision is grounded in the actual tree
> (`urt`, `kcore`, `ipc`, `loader`, `storage-server`, `vendor/rust`) and corrects
> the draft where code contradicts it.

## What grounding changed

The draft's milestones assumed most of std's hard prerequisites were unbuilt. The
code says otherwise, and three corrections reshape the whole plan:

1. **`urt` already is the PAL spine, and it is verified.** `urt::Heap<N>` is a
   `#[global_allocator]`-ready allocator whose arithmetic delegates to the
   Verus-verified `freelist::FreeList` (`freelist` 30 verified; `urt` 25 verified;
   the arena byte-region is a trusted plain-Rust seam kept honest by Miri+proptest,
   *not* one of the 14 ledger seams). `urt::slots::SlotAlloc` (verified) is the TLS
   key-index allocator. `urt::time` is the rev2§2.6 time page: a Loom-certified
   seqlock read composed with the **Verus-verified** `Sample::utc_ns_at` tick→ns
   conversion (totality + monotonicity). So `GlobalAlloc`, `Instant`, and
   `SystemTime` are **ready, verified-backed**, not new construction.
2. **The draft's kernel-track shrinks to essentially one change.** A `Yield`
   syscall already exists (`kcore/src/sysabi.rs` opcode 2 → `Sys::Yield`, decode
   proven total) — strike it. The userspace console driver (`user/console`) already
   exists and the shell already does all terminal I/O over the console channel
   (`user/shell/src/runtime.rs` `out()` → `chan_send`, rev2§7 / status C-M9) — so
   "move stdio to the console channel" is mostly done at the OS level. The only
   genuine trusted-core touch left is **`TPIDR_EL0` save/restore** for real TLS
   (confirmed absent: `grep -ri tpidr kernel/ kcore/` is empty). Entropy is the one
   genuinely-missing primitive.
3. **The one real upfront verification task is the startup-block decoder.** It
   exists (`loader/src/startup.rs`, versioned EUS1 codec, with a live fuzz target
   at `loader/fuzz/fuzz_targets/startup.rs`) but is **plain Rust** — the ledger says
   "the startup byte codec … stay external plain Rust", and loader's 12 verified
   obligations cover only `elf::parse` + `page_layout`. It is an untrusted-decode
   boundary (rev2§3.7/§2.7) consumed in `_start` before the heap exists, so under
   our discipline it must be **verified on arrival** (Phase 1), not stubbed and
   promoted later.

A fourth correction is structural: `vendor/rust`'s `src/version` reads `1.98.0` but
the tree is a newer nightly carrying the post-#117276 `sys/` reorg — each PAL
surface dispatches by `cfg_select!` in `library/std/src/sys/<module>/mod.rs`, with
`sys/pal/<os>/` reduced to a thin shell. The PAL is therefore **per-module arms**
(`grep -rln motor library/std/src/sys` gives the authoritative file checklist;
`motor` = Motūr-OS-delegates-to-`moto_rt`, the structural twin of
Eunomia-delegates-to-`urt`/`ipc`), **not** one `pal/eunomia/` directory. Confirm the
exact upstream rustc commit and pin the std-build toolchain to it before writing any
file-level task.

---

## Global decisions

Made once; each names a recommendation, the alternative, and the rationale. Open
items needing a human call are consolidated in [Open decisions](#open-decisions).

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
- **Locks use a parker-over-notifications backend, not a futex.** *Recommended over*
  a `urt::futex` module (which would hand Mutex/Condvar/RwLock/Once/Parker to std's
  generic impls "for free" but needs a new address-keyed wait-queue **kernel
  object**). The parker maps 1:1 onto the existing `NotifSignal`/`NotifWait`
  primitive (verified core) and the already-Loom/Shuttle/TLA-validated `ipc`
  reactor, adding **no kernel surface**. Cost: more per-primitive PAL wiring than
  the futex route. Either way the concurrent wakeup is irreducibly Loom/Shuttle —
  never Verus (the version-pinned Verus ghost atomics are SeqCst-only with no
  standalone fence, per `doc/guidelines/verification.md`; a proof would certify a
  different binary).
- **Entropy: a startup-block seed grant**, *recommended over* virtio-rng. The seed
  rides the existing rev2§5.1 named-grant mechanism and its decode is covered by the
  Phase-1 verified startup parser, so it **adds no trusted seam**. virtio-rng is
  cleaner long-term but is a genuinely-new DMA/hardware seam (raises the ledger
  14→15) — recorded as later hardening. Randomness *quality* is not a verification
  property; only the seed decode is.
- **`stderr` → `debug-log` for bring-up, then folded into the `stdout` console
  channel** (no new `NAME_STDERR`). There is no `NAME_STDERR` constant today; folding
  avoids migrating the rev2§5.1 named-grant table. Add `NAME_STDERR` only if a
  separate stream is later required.
- **The MVP heap is the fixed `.bss` arena** (`urt::Heap<N>`, no grow). A `heap`
  named grant for `sbrk`-style growth is a deferred rev2§5.1 convention; the fixed
  arena suffices through Phase 5.
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
- the concurrent wakeup path (Parker/Mutex/Condvar over notifications) — SeqCst-pin
  infeasible in Verus; Loom/Shuttle-of-record + the existing `IpcReactor` TLA model;
- any `virtio-rng` device seam (DMA/hardware, rev2§2.5) — avoided by the seed-grant decision.

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
| Parker (atomic state word + wakeup fence) | **Loom** (certifying — the load-bearing tool only where a raw atomic+fence lives) | No |
| Mutex / Condvar / RwLock / Once (synchronize *through* the Parker) | **Shuttle** (thread-interleaving — Loom's weak-memory modeling adds nothing without their own atomics); **reuse** `tla/ipc_reactor` + its 3 negative controls | No |
| stdio sinks | **trusted-shell** (marshalling over verified `ipc` Admission/reactor) | No |
| fs marshalling | **Verus** (path-component decode) + **cargo-fuzz**; rights lattice + `check_header` already verified | No — path decode is verified surface |
| time (Instant/SystemTime) | **Verus** (`utc_ns_at`, green) + **Loom** (seqlock read, green) | No |
| entropy seed decode | **Verus** + **cargo-fuzz** (rides the startup parser) | No (seed-grant) |
| process exit / abort | **trusted-shell** (already in thread-lifecycle (d)) | No |
| io-error decode | **proptest** (total policy map; Verus if it becomes byte-parsing) | No |

### Trusted-base ledger changes (`doc/guidelines/verus_trusted-base.md`)

The ledger is **14 named seams**. Every row/note must name *both* a reason and a
host test (its §11 admission rule). The port changes it as follows; the tally
**stays 14** under the recommended decisions:

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
- **Conditional seam (14→15) — only if `virtio-rng` is later chosen:** a real
  DMA/hardware row with reason + device bring-up host test. The seed-grant decision
  avoids it.

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
| **alloc (GlobalAlloc)** | `urt::Heap<N>` over verified `freelist` | **ready** | `sys/alloc/eunomia.rs` shim (mandatory). Disclose: single-thread no-lock, `MAX_ALIGN=64` (page-aligned requests → null = clean OOM), frag-cap 1024, dealloc-at-cap leaks |
| **time: SystemTime** | `urt::now_utc_ns` (verified `utc_ns_at`, Loom seqlock) | **ready** | Wire `SystemTime::now`; handle the no-time-grant panic (error path or attach invariant) |
| **time: Instant** | `CNTVCT_EL0` direct (zero-syscall) + verified conversion | **partial** | `sys/time/eunomia.rs` Instant over `cntvct/cntfrq` (trivial) |
| **process: exit/abort** | `ThreadExit(15)` → verified `report_terminal` | **ready** | Override the PAL `exit()`/`abort_internal()` (std owns the panic handler) to `thread_exit(code)`/`thread_exit(STATUS_PANIC = u64::MAX)`. `Command` thin/unsupported |
| **panic/unwind** | panic=abort (no `eh_personality`) | **ready** | Target JSON + build-std flags only |
| **env / args** | `loader::startup::decode` of the slot-0 boot message | **partial** | Verify the decoder (Phase 1); `sys/args`+`sys/env` arms. `env::vars` empty until a producer emits env entries |
| **thread: spawn/join/yield/sleep** | `Retype→Thread` + `ThreadStart(13/18)`; `ThreadExit(15)`; join via `ThreadBind(21,on-exit)`+`NotifWait(12)`+`ReadReport(22)`; **`Yield`=op 2**; sleep `TimerArm(14)`+`NotifWait` | **partial** | `urt` in-process thread-spawn primitive (only one scalar arg `x0` — a boxed `FnOnce` ptr fits); fund stack+guard (rev2§5.3). `urt::spawn` has **zero Verus/host coverage** today — net-new test surface |
| **sync: Mutex/Condvar/RwLock/Parker** | notifications `NotifSignal(11)`/`NotifWait(12)` (verified core) | **partial** | parker backend + committed Loom/Shuttle harness reusing `tla/ipc_reactor`. `notif_wait` has **no timeout** → `*_timeout` needs the unwired timer-as-source path |
| **stdio** | (bring-up) `DebugWrite(1)`; (real) console channel over `ipc` | **partial** out / **missing** stdin | `sys/stdio/eunomia.rs` arm; `stdin` op deliberately unassigned (rev2§7) → must read the console channel. `NAME_STDIN`/`NAME_STDOUT` are **already emitted by init** (both → the console slot) and consumed by the shell, so only the std-side wiring remains, not grant delivery |
| **thread_local / TLS** | `TPIDR_EL0` (absent) + `urt::slots` index alloc (verified) | **missing** | `TPIDR_EL0` save/restore (Phase 3) + `urt` TLS key table + `sys/thread_local` arm |
| **fs: File/read_dir/metadata** | storage-server openat session (handle-relative, component paths) | **partial** | `sys/fs/eunomia.rs` client + path decode; `File=(HandleId, TreePath, client offset)`; 256-byte `MAX_MSG` → client offset loops |
| **hashmap_random_keys** | nothing — **hard blocker** | **missing** | seed-grant + `sys/random/eunomia.rs` (mandatory) |
| **net** | nothing, by design | n/a | none — permanently unsupported |

**fs surface that is `Unsupported` by construction** (rev2§4.9 has none of it):
symlink / hard_link / read_link / canonicalize; permissions / chmod / chown
(authority is the cap rights mask, not mode bits); `accessed` / `created` (no atime,
rev2§4.9); `set_len` / truncate; `create_dir` of empty dirs (creation is a side
effect of `Write`); `DirEntry::ino`, nlink/uid/gid; `current_dir`/`set_current_dir`
as syscalls (handle-relative bookkeeping; no ambient cwd, rev2§4.9). Cross-subtree
rename is `EXDEV` by construction.

**Supportable but deferred (spec allows it; the *wire* does not yet carry it):**
`modified()` / mtime is a **mandatory, server-assigned entry field** in rev2§4.9
(it participates in hashing) — it is *not* forbidden, merely absent from the current
`storage-server` wire protocol. Exposing it is a wire extension (a new `Response`
variant + `PROTO_VERSION` bump), not a spec change. Return `Unsupported` for now and
record it as a deferred wire-protocol task, not a permanent gap. `set_times` stays
unsupported (mtime is server-assigned, not client-set).

---

## Spec & kernel changes (corrected and grounded)

The verification posture is `kernel/`-over-`kcore` throughout. **Net kernel-track
surface: one change (`TPIDR_EL0`)** — plus virtio-rng only if the entropy decision
rejects the seam-free seed grant.

| # | Change | Kind | Where | Trust posture | Still needed? |
|---|---|---|---|---|---|
| 1 | Yield syscall | — | `kcore/src/sysabi.rs` (opcode 2) | verified decode + trusted scheduler shell | **No — already exists; strike** |
| 2 | `TPIDR_EL0` save/restore | trusted-kernel-shell | `kcore/src/thread.rs` `TrapFrame` (outside `verus!{}`) + `kernel/src/exceptions.rs`, `main.rs`, `syscall.rs` | trusted asm shell (rev2§6.1d); ledger *routing note*, no new seam | **Yes — the one genuine kernel-track touch** |
| 3 | Entropy: seed grant (A) | spec-convention + verified-decode | rev2§5.1 table; loader/`eunomia-sys` startup parser | no new seam; decode is Verus+fuzz | **Yes (A preferred)** |
| 3′ | Entropy: virtio-rng (B) | device | new driver crate, DMA seam | **new** DMA/hardware seam (rev2§2.5) | Only if (A) rejected |
| 4 | Console stdio | marshalling only | std PAL over `chan_send`/`chan_recv` resolving the slots from the already-delivered grant table | no new verified logic; driver + shell path + `NAME_STDIN/STDOUT` grants already exist | **Mostly done — only the std-side stdio wiring** |
| 5 | Heap named-grant | spec-convention | rev2§5.1 table + `CapSlot` grant | sbrk-grow folds under Store/aspace seam | Deferred — fixed arena suffices |
| 6 | stderr | spec-convention + marshalling | rev2§5.1 (optional `NAME_STDERR`) or fold into stdout | `debug_write` bring-up; console production | Small — decide sink (folded) |

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
| **2.2** | GlobalAlloc | `sys/alloc/eunomia.rs` over `urt::Heap<N>` (algorithm verified in `freelist`; arena Miri+proptest). Mandatory arm. Disclose MVP bounds | `cargo verus verify -p urt -p freelist` (green, re-cited) + `urt` Miri sweep | **5** |
| **2.3** | stdio (bring-up) + exit terminus | `sys/stdio/eunomia.rs` → `DebugWrite(1)` (len ≤ 1024); `panic_output` same path; **override the PAL `abort_internal()` and `exit()`** to `thread_exit(STATUS_PANIC)` / `thread_exit(code)` (the `motor` template — *not* the `unsupported` `intrinsics::abort()`), preserving the reaper contract for a std binary | boot prints `println!`; a panicking std binary reaps as `STATUS_PANIC` | **6** |
| **2.4** | Time (free) | `Instant` ← `cntvct/cntfrq`; `SystemTime` ← `urt::now_utc_ns` (verified `utc_ns_at`, Loom seqlock); resolve the no-time-grant panic | `cargo verus verify -p urt` (re-cited); `Instant::now`/`SystemTime::now` work | **7** |
| **GATE** | CI smoke | green-boot marker (`…M1 PASS`-style) + kill-cleanly harness (background QPID + trap + deadline-poll, per `CLAUDE.md`) in the on-os CI job; asserts `println!`/`format!`/`Vec`/`Box`/`String`/`Instant`/`SystemTime` | QEMU boot smoke green | — |

### Phase 3 — TLS + threading + locks + entropy + HashMap
*The only kernel-track phase. Parallelizable with Phase 4. Internal order forced:
3.1 → 3.2 → 3.3; 3.4/3.5 independent once 3.1 lands.*

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **3.1** | Real TLS (kernel) | `TPIDR_EL0` save/restore: `tpidr` in `TrapFrame` (272→288 + pad), `mrs/msr` in `exceptions.rs`, grow `sub/add sp,#272`, seed at start; **`offset_of` const-assert** | `cargo clean -p kcore && cargo verus verify -p kcore` re-passes **406/0** (unchanged); host test: 2 threads share an aspace, read distinct TLS markers; ledger routing-note added | **8** |
| **3.2** | spawn/join/yield/sleep | `urt` in-process thread-spawn primitive (`Box<ThreadInit>` ptr → `x0`); `sys/thread/eunomia.rs` (motor template); stack+guard (rev2§5.3); `yield_now` = op 2; sleep = `TimerArm`+`NotifWait` | QEMU spawn smoke + **new host tests** for the `urt::spawn` invariants (bind-before-start, read-report-before-revoke) — currently uncovered | **9** |
| **3.3** | Locks | parker-over-notifications backend (decision above); Mutex/Condvar/RwLock/Once over it; lift the heap single-thread no-lock assumption (lock or per-thread arenas) | **Loom** (certifying) + **Shuttle** (breadth) green, **reusing `tla/ipc_reactor`** + its 3 negative controls — **never Verus** (SeqCst pin) | **10** |
| **3.4** | Entropy + HashMap | startup-block seed grant (decision above); `sys/random/eunomia.rs` (mandatory arm) over the seed; unblock `HashMap` default `RandomState` | seed decode Verus+fuzz (rides 1.2); `HashMap` works under smoke | **11** |
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
| **5.1** | console stdio | move `stdout`/`stdin` off `debug-log` onto the `user/console` channel via `ipc` (reactor poll-once loop = the validated `reactor_no_lost_wakeup` harness; `send_blocking` for write backpressure); keep `debug-log` for panics only. The std PAL resolves the stdin/stdout cspace slots from the startup grant table — `NAME_STDIN/STDOUT` are **already emitted by init and consumed by the shell today**, so no grant-delivery change is needed | Shuttle reactor harness green; QEMU interactive stdio | **16** |
| **5.2** | process/env | `process::exit(code)` → PAL `exit()` → `ThreadExit` `exited(status)`; `abort` → PAL `abort_internal()` → `thread_exit(STATUS_PANIC)` (both overridden in 2.3, re-confirmed here); `Command` thin/unsupported; `temp_dir` → `tmp` grant; `current_dir`/`set_current_dir` handle-relative or unsupported; populate env entries (producer side) | smoke | **17** |
| **5.3** | rewrite a user binary | `hello` on std first (validates entry/argv/alloc/exit/`STATUS_PANIC`), then `shell` (alloc + `SystemTime`/`Instant` + console stdio + args + fs) — **keep spawn/reap on raw `loader::spawn`/`urt::spawn`** (std::process can't model it yet) | extended boot smoke exercising fs + console stdio | **18** |

### Phase 6 — Hardening & forward-port discipline

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **6.1** | on-target test triage | run `libcore`/`liballoc`/`libstd` test subsets on-target via QEMU; triage + record skips (eunomia-only std crates can't host-build — excluded like `kernel`) | QEMU test run; skip log committed | **19** |
| **6.2** | fuzz + PAL audit | grow committed fuzz corpora for the verified decoders; **PAL thin-delegator audit** — diff `sys/pal/eunomia` (+ the `eunomia.rs` arms) vs `pal/unsupported`, confirming zero new logic and that every verified `requires` is re-established or runtime-guarded at the boundary (the §11 inverse-leak rule). *This review is the standing gate for the thinness rule* | cargo-fuzz green; audit recorded | **20** |
| **6.3** | runbook + ledger | forward-port runbook (pinned-nightly bump cadence, the diff surface, regression set); record the deliberate Verus-pin ↔ std-version **decoupling** (std is not Verus-verified; `vendor/rust` never runs the gate); finalize the ledger (new `eunomia-sys` Baseline row; `loader` count update; folding notes for TPIDR/sbrk-grow; **no yield row** — op 2 exists; TLS-key note; entropy decision) | ledger consistent; runbook committed | **21** |

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
phase tables (0-indexed, matching the requested `0_findings.md`, `1_findings.md`, …
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
| 10 | 3.3 parker/locks | 21 | 6.3 forward-port runbook + ledger |

Each findings doc records **everything worth keeping** — err on the side of too
much; there is a consolidation pass at the end. At minimum:

- **Decisions** taken and the alternatives rejected (with the reason), especially
  any of the [Open decisions](#open-decisions) resolved during the task.
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

## Open decisions

Each names a recommended default (already reflected in the plan) so work is never
blocked; record the final call in the relevant task's findings doc.

1. **Sync backend — parker vs futex (task 10).** *Recommended: parker over
   notifications* (no new kernel object; reuses the Loom/Shuttle/TLA-validated
   reactor). Futex hands the whole sync stack to std's generics but needs a new
   address-keyed wait-queue kernel primitive + fresh Loom coverage.
2. **Entropy source (task 11).** *Recommended: startup-block seed grant* (no new
   seam; decode covered by task 3). virtio-rng only if seed quality proves
   insufficient — it raises the ledger 14→15.
3. **stderr sink (tasks 6, 16).** *Recommended: `debug_write` for bring-up, then
   fold into the `stdout` console channel* (no `NAME_STDERR`). Add a separate name
   only if a distinct stream is required.
4. **Heap growth (tasks 5, later).** *Recommended: fixed `.bss` arena for the MVP.*
   The `heap` named grant + sbrk-grow (folds under the Store/aspace seam) is deferred.
5. **`*_timeout` APIs in scope? (task 10).** `notif_wait` has no deadline; `park_timeout`
   / `Condvar::wait_timeout` need the timer-as-source reactor path (`timer_arm` +
   `register_bound`), which has no reactor consumer today. *Recommended: out of scope
   for the initial port; return `Unsupported` or block without timeout, and wire the
   timer source as a follow-up.*
6. **`TPIDR_EL0` timing (task 8).** *Recommended: land it with real threading
   (Phase 3), not hello-world* — single-threaded bring-up uses the global-statics
   `thread_local` fallback, so Phase 2 is not gated on a kernel change.
```
