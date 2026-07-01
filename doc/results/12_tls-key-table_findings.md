# Findings — Phase 3.5: verified TLS key table + `thread_local!` destructors

Task 3.5 of `doc/plans/2_plan-std-revised.md` (findings **#12**) — the *hardening*
remainder of the TLS work. Phase 3.2 shipped per-thread `TPIDR_EL0` storage but left
three MVP gaps: `thread_local!` macro storage was the single-threaded `no_threads`
version (wrong on spawned threads), no TLS destructors ran, and the per-thread block
leaked on exit. 3.5 closes all three by moving eunomia onto std's **key-based TLS**
(`os.rs`, the motor shape), backed by a new **verified `urt::tls` key table over the
verified `SlotAlloc`**, and by driving destructors + block-free at thread exit. Proven
live: `run bin/stdsmoke tls` → **`STD35 PASS`** (a spawned thread's `thread_local!`
`Drop` destructor runs on exit; the per-thread `Cell` is genuinely per-thread —
`scripts/std-smoke-test.sh`, wired into the CI `on-os` job).

## Decisions (and the alternatives)

- **Key-based `os.rs` storage (the motor shape), not native.** eunomia is **not**
  `target_thread_local` (no ELF `#[thread_local]`), so upstream's only multi-threaded
  `thread_local!` storage is the library key-based `os.rs`. It needs a platform `key`
  backend of five symbols (`Key`/`create`/`get`/`set`/`destroy`) + `racy::LazyKey`;
  eunomia supplies them over the `__eunomia_tls_*` bridge (the seam crate can't be a
  sysroot dep, so the motor `use moto_rt::tls::…` becomes an `extern "Rust"` bridge like
  every other eunomia PAL arm). *Rejected:* making eunomia `target_thread_local` — the
  native ELF-TLS relocation model is a far larger codegen/target-JSON change and is not
  what "key table over `SlotAlloc`" describes.
- **Hybrid: key-based storage (motor) but self-driven exit (hermit).** motor relies on
  its OS to iterate key destructors at thread exit; eunomia has no such mechanism but
  *owns* thread exit. So the trampoline (`sys/thread/eunomia.rs`) and `_start`
  (`sys/pal/eunomia/mod.rs`) explicitly call `run_dtors` → `rt::thread_cleanup` → `free`
  after the thread body, and eunomia's `guard::enable` is the `hermit`/`xous` no-op
  (`{}`). `rt::lang_start_internal` never calls `thread_cleanup` for main, so the
  main-thread calls are additive, not a double free. *Rejected:* the key-based
  `guard/key.rs` deferred-cleanup sentinel dance — it presumes an OS runs key dtors,
  which eunomia does not; self-driving is simpler and matches who owns exit.
- **The verified surface is the key *allocator*; the dtor registry is plain.** A thin
  `verus!{}` `KeyTable` over `SlotAlloc<1>` proves key-uniqueness/range/exhaustion (the
  property the TLS layer stands on); the per-key `[Option<Dtor>; 64]` registry is plain
  sibling bookkeeping (a table of opaque fn-pointers is not a verifiable property),
  guarded by the same lock — the `urt::thread` `Inner { slots: Option<SlotAlloc>, <plain
  fields> }` precedent. This **replaces 3.2's raw `NEXT_SLOT` atomic counter**: both
  `local_pointer!` (CURRENT/ID, `dtor = None`) and every `thread_local!` now draw keys
  from the one verified table.
- **1-based keys.** A key is `slot_index + 1`, so `create` never returns `0` —
  `racy::LazyKey`'s `KEY_SENTVAL == 0` uninitialized sentinel — avoiding its double-create
  dance. `Key k` → `SlotAlloc` slot `k-1` → `TPIDR[k-1]`. A full table maps to `0`, which
  `key/eunomia.rs::create` turns into the `rtabort!("out of TLS keys")` (the `unix.rs`
  posture).
- **Self-initializing global, no bootstrap step.** The `urt::tls` `KeyTable` starts `None`
  and lazy-inits on first `create` (`SlotAlloc::new(0, 64)`), unlike `urt::thread`'s
  cap-gated `configure` — TLS keys need no caps, and the first `create` happens on the
  main thread during `rt::init` (after `bootstrap_init`), so no ordering hazard.
- **Concurrency reuses the certified `SpinLock`; no new model.** `create`/`destroy` need
  only mutual exclusion over the global key state (no wait/wake), so `urt::tls` reuses the
  Loom-certified `lock::SpinLock` — the exact `urt::random`/`urt::Heap` posture. Per-key
  `get`/`set` are lock-free (each thread touches only its own `TPIDR` block). Miri is the
  data-race oracle; no new Loom/Shuttle obligation.
- **Delete the custom `sys/thread_local/eunomia.rs`.** `os.rs` supplies its own
  `LocalPointer`/`local_pointer!` (over `LazyKey::new(None)`), so the 3.2 file — the
  `NEXT_SLOT` counter and TPIDR `mrs` on the std side — is superseded and removed;
  eunomia falls through the storage `cfg_select` to `_ => mod os` like every key-based
  target.

## What shipped

- **Verified key table (`urt/src/tls.rs`, new).** `pub const TLS_KEYS = 64`, `pub type
  Dtor`, the `verus!{}` `KeyTable` over `SlotAlloc<1>` (`wf`/`is_live` closed specs;
  `new`/`create`/`destroy` with the key-uniqueness/frame contracts), and the process-global
  `State { lock: SpinLock, inner: UnsafeCell<Inner{ table: Option<KeyTable>, dtors:
  [Option<Dtor>; 64] }> }` with the plain `create`/`destroy`/`collect_dtors` API. Gated
  `#[cfg(not(any(loom, shuttle)))]` (const-`SpinLock` static, the `random` posture). 4 host
  tests (distinct-in-range-then-exhaust, reuse-after-destroy, sequential-from-zero, the
  global register/collect/free). `urt` **25 → 29 verified**.
- **Seam backend (`eunomia-sys/src/tls.rs`, extended).** Kept the TPIDR block mgmt
  (`init_main`/`init_thread`/`set_tpidr`/`MAIN`); added `tpidr_base()` (`mrs`), the key
  backend `create`/`get`/`set`/`destroy` (1-based, over `urt::tls` + the block), the
  POSIX-round `run_thread_dtors` (re-snapshots the registry each round; runs a key's dtor
  when its per-thread value `addr > 1`), and `free_thread_block` (reclaims a spawned
  block, skips the static `MAIN` — fixes the 3.2 leak). A `const _` couples `TLS_SLOTS ==
  urt::tls::TLS_KEYS`.
- **Seam shims (`eunomia-sys/src/pal.rs`).** `__eunomia_tls_{create,get,set,destroy,
  run_dtors,free_thread}` — one-line delegations beside the existing `_init_main`/`_init_thread`.
- **Vendored std.** `sys/thread_local/mod.rs`: removed eunomia's dedicated storage arm
  (falls to `_ => mod os`), moved eunomia's `guard` to the `hermit`/`xous` no-op group,
  added an eunomia `key` arm (the motor shape). New `sys/thread_local/key/eunomia.rs` (the
  `__eunomia_tls_*` bridge). Deleted `sys/thread_local/eunomia.rs`. Trampoline
  (`sys/thread/eunomia.rs`) and `_start` (`sys/pal/eunomia/mod.rs`) run `run_dtors` +
  `rt::thread_cleanup` (+ `free_thread` on the spawned path) before the kernel terminus.
- **Gate.** `user/stdsmoke` gains a `tls` arm (a `Drop` sentinel `thread_local!` bumping a
  global `AtomicUsize` + a per-thread `Cell`; spawn → touch → join → assert the destructor
  ran and the child's `Cell` is 7 while main's is 0; `STD35 PASS` / `tls-bad` exit 11).
  `scripts/std-smoke-test.sh` drives it and asserts the marker + tripwire.

## Problems hit and how they were solved

- **`TLS_KEYS` unusable in spec context.** A `pub const` declared *outside* `verus!{}` is
  "ignored" by the prover, so the `KeyTable` contracts couldn't mention it. Fixed by moving
  the const *inside* the `verus!{}` block — still an ordinary `const` for the plain state
  below (the `dtors` array length + `collect_dtors` signature).
- **Vendored-std edits need a `target/user` wipe.** As the 3.2 runbook note warns, the
  inner build-std cached its rlib; `rm -rf target/user` before `cd kernel && cargo build`
  forced the `sys/thread_local` reorg (and the deleted file) to take effect.
- **Borrow split in `create`.** Registering the dtor after allocating the key would
  overlap a `table.as_mut()` borrow with `dtors[k] =` on the same `Inner`; structured so
  the `table` borrow ends (the `create()` call returns) before the disjoint `dtors` field
  is touched.

## Verification record

Toolchain `nightly-2026-06-26`; Verus binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Verus (authoritative, cold).**
  - `cargo clean -p urt && cargo verus verify -p urt` → **29 verified, 0 errors** (was 25;
    the `KeyTable` `new`/`create`/`destroy` added 4). Baseline re-derived from the
    pre-change tree via `scripts/verus-baseline.sh urt`: **25**. Every pre-existing
    obligation byte-identical across the change (`slots::alloc_range` rlimit 469496,
    `slots::set` 314877, `slots::alloc` 86590, `slots::lemma_bit_other` 115806,
    `slots::lemma_set_bit` 94353, `slots::new` 36901, `time::utc_ns_at` 223972,
    `time::lemma_decompose` 93080 — all unchanged), so the delta is purely the key table.
    New fns cheap: `create` rlimit **8391**, `destroy` **5035**, `new` **2709** (all below
    `slots::new`). Re-verified 29/0 over the `verusfmt`-formatted tree.
  - `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` → **7 verified, 0
    errors** (unchanged; the seam backend/runner/shims carry no `verus!{}`); loader **30**,
    urt **29** re-verified transitively.
- **Host tests.** `cargo test -p urt` → **46 passed** (42 + the 4 `tls::` tests);
  `cargo test -p eunomia-sys` → 21 passed.
- **Miri.** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p urt
  -j4` → **46 tests, 46 passed** (data-race freedom of `create`/`destroy` under the lock;
  the per-thread `get`/`set` model).
- **Build.** `rm -rf target/user && cd kernel && cargo build` clean (std with the
  `thread_local` reorg + all user binaries + the target-gated `eunomia-sys`/`urt` arms
  cross-build). `eunomia-sys` also cross-builds standalone for
  `aarch64-unknown-none-softfloat`.
- **On-target (QEMU) — the gate.** `scripts/std-smoke-test.sh` → **`STD SMOKE TEST PASS`**
  with the pre-existing `STD2`/`STD32`/`STD33`/`STD34` (no regression) and the new **`STD35
  PASS`** (a spawned thread's `thread_local!` `Drop` destructor ran on exit; the per-thread
  `Cell` genuinely per-thread). Machine `virt,gic-version=3 -cpu cortex-a72`.
- **Formatting.** `cargo fmt --check` clean (root + `user/stdsmoke`); `scripts/verusfmt.sh
  --check` clean (it reformatted the `urt::tls` `verus!{}` interior; urt re-verified 29/0
  over the formatted tree).

## Surface left trusted / unsupported (and why)

- **The `urt::tls` key table is verified; only the seam get/set/run/free stay trusted.**
  The `mrs`/`msr` and per-thread block pointer arithmetic in `eunomia-sys/src/tls.rs` are
  the userspace mirror of the kernel TLS-register marshalling (rev2§6.1d) — folds under
  the 3.1/3.2 TPIDR routing note, host/QEMU-witnessed, never Verus (asm). `run_thread_dtors`
  runs opaque std fn-pointers; `free_thread_block` reclaims a heap box — neither a
  verifiable property, both Miri/QEMU-witnessed.
- **Concurrency reuses the certified `SpinLock`, never Verus.** `create`/`destroy` mutual
  exclusion is an `Acquire`/`Release` protocol (the SeqCst ghost-atomic pin,
  `doc/guidelines/verification.md`); the reused `lock::SpinLock` is already Loom-certified,
  so no new interleaving model — the `urt::random` posture. Miri is the data-race oracle.
- **§11 inverse-leak (the new PAL arms).** `key/eunomia.rs`, `eunomia_sys::tls`, and
  `urt::tls` add zero logic vs `pal/unsupported`: `create` re-establishes "out of TLS
  keys" as a loud `rtabort` (never a bogus key), `destroy` guards `key < TLS_KEYS` at the
  plain boundary (the erased `KeyTable::destroy` precondition), and the `KeyTable` `wf`
  the verified ops require is re-established by construction (`new` gives it, both ops
  preserve it, built no other way — the `urt::thread` precedent).
- **Disclosed MVP bounds.** `TLS_KEYS = 64` shared across all `thread_local!` sites *and*
  `local_pointer!`s (the existing `TLS_SLOTS`); a 65th `create` loudly aborts. Destructor
  rounds bounded at `MAX_DTOR_ROUNDS = 5` (POSIX `_POSIX_THREAD_DESTRUCTOR_ITERATIONS`); a
  value still live after leaks (the same bound POSIX imposes). The main thread's block
  stays the static `.bss` `MAIN` (not freed); spawned blocks are now freed at exit.

## Follow-ups

- **Per-thread untyped/stack reclaim on join** — still lifetime-bounded (`urt::thread`,
  unchanged here); the TLS *block* leak is now fixed, the kernel-object budget is not.
- **ELF-note thread-capable marker** (replacing the shell allowlist) and **same-priority
  scheduling** — 3.2 carry-forwards, untouched.
- **`EUNOMIA_HEAP_BYTES` producer wiring / env producer** — unrelated Phase-2/5
  carry-forwards, untouched.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

- Added the **TLS-key-table routing note (std-port 3.5)** after the 3.4 entropy note: the
  key allocation is verified surface on the urt row (`25 → 29`); the seam get/set/run/free
  are trusted asm shell folding under the 3.1/3.2 TPIDR note; the `os.rs` storage + `guard`
  + `key/eunomia.rs` bridge + `__eunomia_tls_*` shims are upstream/thin. No `external_body`,
  no new seam. §11 host test: `cargo test -p urt` + the QEMU `STD35` smoke.
- Updated the **urt Baseline row** to `29 verified` with the `KeyTable` description + the
  per-fn `rlimit` note, and refreshed the 3.2 TPIDR note (the deleted
  `sys/thread_local/eunomia.rs`; the raw slot counter now the verified key table).

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
