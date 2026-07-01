# Findings 19 — Phase 6.1: on-target library-test triage

Task: **std-port 6.1** (`doc/plans/2_plan-std-revised.md`, Findings #19) — run subsets of
the upstream Rust `coretests` (libcore) and `alloctests` (liballoc) unit-test suites
on-target against the eunomia PAL under QEMU, triage the results, and commit a skip log.
The gate is a live CI step (`LIBTEST ON-TARGET PASS`) plus the committed
`scripts/libtest-skips/*.skip` files. This is the first hardening task: a PAL regression
(or a forward-port to a newer nightly) is now caught by the *real* library tests, not
only the bespoke `stdsmoke` fixture.

**Headline:** the full upstream suites run green on-target under QEMU —
**coretests 2590 passed / 0 failed / 5 ignored**, **alloctests 758 passed / 0 failed /
25 ignored** (skipping 2 heap/alignment-bound tests) — **3348 tests, 0 failed, 0 aborts**.

## What shipped

- **`user/coretests/`, `user/alloctests/`** — thin mini-crates whose `[[test]]` target
  compiles the *vendored* upstream suite in place (`path` → `vendor/rust/library/…/tests/lib.rs`);
  no test logic is copied. Each carries the suite's own dev-deps (`rand`/`rand_xorshift`)
  and an `eunomia-sys` dep.
- **`vendor/rust/library/{coretests,alloctests}/tests/lib.rs`** — one cfg-gated line each,
  `#[cfg(target_os = "eunomia")] extern crate eunomia_sys;`, forcing the PAL↔seam bridge
  into the link. Inert for host/`x.py` builds.
- **`kernel/build.rs`** — `build_user_test` (`cargo test --no-run … -Zbuild-std=…,test
  -Zpanic-abort-tests`, parses the JSON `executable`, copies it to a stable name). The
  suites build only under `EUNOMIA_BUILD_LIBTESTS` and are **embedded into the shell**.
  Also added `eunomia-sys/src` to `rerun-if-changed` (a latent gap — see Problems).
- **`user/shell`** — embeds the two suite ELFs (`libtests` cfg) and spawns them from
  `.rodata` on `run bin/{coretests,alloctests}`, bypassing the store; `THREAD_CAPABLE +=
  {coretests, alloctests}`; `DONATION_BYTES` 16→48 MiB; shell heap 4→8 MiB; libtest
  children opt out of env inheritance (`inherit_env`) to save startup-block budget.
- **`urt`** — `MAX_ALIGN` 64→128 (the AArch64 cache line): cache-line-padded std
  structures — notably `std::sync::mpsc`, which every libtest run allocates — no longer
  hit the alignment ceiling. A change to the **trusted arena seam** (Miri+proptest), not a
  Verus obligation.
- **`scripts/libtest-on-target.sh`** + **`scripts/libtest-skips/{coretests,alloctests}.skip`**
  — the QEMU runner (whole-suite for `--full`, per-module for `--ci`, reap-synchronized,
  paced input, continue-on-failure triage) and the committed skip lists.
- **`.github/workflows/ci.yml`** — a new `on-os` step running the `--ci` curated subset.

## Decisions (and rejected alternatives)

1. **Build the harness with `-Zbuild-std=…,test` + `-Zpanic-abort-tests`.** The target is
   `panic=abort`; without `-Zpanic-abort-tests` cargo builds the test profile as
   `panic=unwind`, so build-std produces a *second* `core` (the unwind variant) and the
   non-sysroot deps (`serde_core`/`verus_builtin`, via `eunomia-sys`) link the wrong one — a
   duplicate-lang-item `E0152`. **Rejected:** dropping the `eunomia-sys` dep tree (it is
   needed — the bridge delegates to `urt`/`loader`/`ipc`).

2. **Run in-process (`--force-run-in-process`), exclude `#[should_panic]`
   (`--exclude-should-panic`).** libtest's default `SpawnPrimary` (subprocess per test) is
   impossible with no process spawn. Under panic=abort `catch_unwind` cannot catch, so a
   `#[should_panic]` test would abort the run — `--exclude-should-panic` drops them
   wholesale. Both flags need `-Zunstable-options`, accepted because build-std compiles
   libtest with the nightly (its `build.rs` sets `enable_unstable_features`) — **no
   `RUSTC_BOOTSTRAP` needed**. `--test-threads=1` is unnecessary (`available_parallelism()`
   = 1 ⇒ serial by default, which also makes the `test <NAME> ...`-before-run line the
   culprit oracle).

3. **Embed the suites in the shell, spawn from `.rodata`, bypass the store.** The MVP fs
   read path (`storage-server`) returns the **whole file** per `Request::Read` and slices
   it, so a 256-byte-`MAX_MSG` client loop reconstructs a multi-MiB file ~thousands of
   times — O(n²) plus a storaged OOM (3 MiB heap), paid *again* per module invocation.
   Embedding makes `run bin/coretests …` instant and store-free. **Rejected:** a storaged
   read cache + heap bump (removes the OOM but not the ~9800 per-load IPC round-trips, and
   still reloads per module); the deferred fs bulk-window data plane (out of scope).

4. **`--full` runs each suite whole (one child); `--ci` runs per-module (few children).**
   Repeated console-child spawn/reap wedges the shell's console *input* after a few dozen
   iterations (Problems), so a 50-module per-module `--full` sweep is unreliable; running
   the entire suite in one process (one spawn) sidesteps it and finishes in ~16 s. `--ci`
   (10 modules) stays under the wedge threshold and keeps per-module isolation for the
   fast regression gate. A single failing test aborts a whole-suite run (panic=abort,
   in-process); the last `test <NAME> ...` line names it for the skip list.

5. **`MAX_ALIGN` 64→128 in `urt`.** Instrumentation showed the first post-`running N tests`
   allocation is `sz=512 al=128` — `std::sync::mpsc`'s cache-line-padded channel block,
   which libtest itself creates. 128 is the AArch64 cache line, so this is the principled
   cap (any std program using `mpsc`/`CachePadded` benefits). The `Heap::alloc` GlobalAlloc
   impl is the trusted arena seam (Miri+proptest per `urt`'s module doc), *not* a Verus
   obligation, so no verified count moves. A page-aligned (4096) request is still refused.
   **Rejected:** raising to 256 to also pass `vec::overaligned_allocations` — that test's
   `align(256)` is beyond the MVP arena's principled bound, so it is a documented skip, not
   a reason to grow the arena further.

6. **libtest children opt out of env inheritance (`inherit_env=false`).** The 256-byte
   startup block can't hold two `--skip` filters *and* the 3 base flags *and* the grants
   *and* the inherited env. core/alloc tests read no env vars, so dropping the ~38 env bytes
   for embedded children keeps the block in budget. **Rejected:** raising `MAX_BLOCK` (the
   plan's sanctioned escalation) — it touches the verified `loader::startup` decoder + fuzz
   corpus, a bigger blast radius than a shell-local opt-out; kept as a follow-up if more
   skips are ever needed.

## Problems hit and how they were solved

Each was confirmed empirically (instrumentation / serial logs), not guessed.

- **`E0152` (two `core`s).** First build died in `serde_core`/`verus_builtin`. Cause:
  libtest wants `panic=unwind` → a second unwind-core → the eunomia-sys tree linked the
  wrong one. Fixed by `-Zpanic-abort-tests`. *Runbook:* a libtest binary on this target
  needs `-Zbuild-std=…,test` **and** `-Zpanic-abort-tests`.

- **storaged OOM + O(n²) file service.** `run bin/coretests` (2.4 MiB) aborted storaged
  with `memory allocation of 2361792 bytes failed`. `Request::Read` reconstructs the
  *entire* file per call. Solved by embedding + spawn-from-memory (Decision 3).

- **init failed to spawn the shell at 32 MiB heap; child spawn (`BadElf`) at 32 MiB heap.**
  Two memory walls, both from init's/shell's fixed donation budgets: a 32 MiB shell heap
  overran init's 127 MiB boot untyped; a 32 MiB *child* heap made the libtest child's
  segments overrun the 48 MiB shell donation (`spawn::prepare` failed → the shell's
  `RunErr::BadElf`). Resolved by keeping the shell heap at 8 MiB and the child heap at
  16 MiB — libtest frees each test's resources before the next, so the live set is bounded
  (one test), not cumulative, and 16 MiB survives a 2590-test whole-suite run.

- **`sz=512 al=128` allocation failure.** With a confirmed 16 MiB child heap, a 512-byte
  alloc failed at `running N tests`, before any test. Instrumenting `__eunomia_alloc` to
  print `(size, align)` on null showed `al=128` > `MAX_ALIGN=64` (`std::sync::mpsc`). Fixed
  by `MAX_ALIGN=128` (Decision 5).

- **`eunomia-sys/src` was not in `kernel/build.rs`'s `rerun-if-changed`.** The alloc
  instrumentation appeared inert — because editing `eunomia-sys/src` did not retrigger
  `build.rs`, so the user binaries (which all link `eunomia-sys`) kept a stale copy. Added
  it to the list. *Runbook:* a pure `eunomia-sys` edit now rebuilds the user binaries.

- **Console input wedged after ~27–46 child spawns.** A per-module `--full` sweep lost a
  command after N heavy-output thread-capable spawns (the shell reached the prompt but
  stopped reading stdin). `runloop selftest 100` survives 100 minimal spawns, so the
  trigger is the thread-capable/heavy-output test children — a per-spawn resource churn.
  Sidestepped by running whole-suite for `--full` (Decision 4); the reap-synchronized
  runner (key the next command on the child *reap*, not the `test result:` line) removed a
  separate command-pacing race.

- **Long commands silently dropped (`--skip` filters).** A 133-char command was never
  echoed: a large burst written to the FIFO overflows QEMU's PL011 RX FIFO faster than the
  32-byte-buffer console driver drains it, dropping bytes. Fixed by pacing the runner's
  input (16-byte chunks with brief pauses).

- **Startup block overflow with 2 skips.** Once delivered, the 2-skip alloctests command
  hit `error: startup block rejected` (> 256 bytes). Fixed by the env opt-out (Decision 6)
  plus a shortened skip substring.

- **Two alloctests tests exceed MVP bounds (documented skips).** `sort::*` `correct_1k_*`
  sort 1000 elements of a ~10 KB element type (≈10 MB per test) — beyond the 16 MiB fixed
  `.bss` heap; `vec::overaligned_allocations` uses `#[repr(align(256))]` — beyond
  `MAX_ALIGN=128`. Both are in `scripts/libtest-skips/alloctests.skip` with reasons.

## Verification record

| Gate | Command | Result |
|---|---|---|
| suite build (target) | `cargo test --no-run … -Zbuild-std=…,test -Zpanic-abort-tests` | **exit 0**; coretests 2.4 MiB / alloctests 1.6 MiB ELF |
| **full sweep** | `bash scripts/libtest-on-target.sh --full` | **LIBTEST ON-TARGET PASS**: coretests **2590 passed / 0 failed / 5 ignored**, alloctests **758 passed / 0 failed / 25 ignored** — **3348 tests, 0 failed, 0 aborts** |
| **CI subset (the gate)** | `bash scripts/libtest-on-target.sh --ci` | **PASS**: 10 modules, **2139 tests passed, 0 failed** |
| urt host tests | `cargo test -p urt` | **46 passed, 0 failed** (MAX_ALIGN=128 over-align cap + proptest to 128) |
| host suite (changed) | `cargo test -p urt -p eunomia-sys` | **all ok, 0 failed** |
| urt proofs (cold) | `cargo clean -p urt && cargo verus verify -p urt` | **29 verified, 0 errors** (unchanged; freelist 30) |
| urt Miri (UB) | `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p urt -j4` | **all passed, no UB** |
| formatting | `cargo fmt --check` | **clean** |

Green verdict = each suite's child `exited(0)` after a `test result: ok`, with no
`panicked`/`faulted(`/`memory allocation of`/`error:` in the run, and the runner's `FAILS`
list empty.

## Surface left unsupported / trusted (and why)

- **`libstd`'s inline `#[cfg(test)]` unit tests are skipped by construction** — std's
  `_start` is `#[cfg(not(test))]`, so a `--test` build of std has no entry point. The
  external `coretests`/`alloctests` crates link a normally-built std (so `_start` is
  present) and are the tractable populations; `library/std/tests/*.rs` exercises mostly
  Unsupported surface (fs/process/env) and was not pursued.
- **`#[should_panic]` tests are excluded** (`--exclude-should-panic`) — a structural limit
  of a panic=abort test binary without subprocess isolation.
- **Two alloctests skips** (`scripts/libtest-skips/alloctests.skip`): `sort::` (heap-bound,
  10 MB stress tests vs the 16 MiB fixed arena) and `vec::overaligned_allocations`
  (`align 256` > `MAX_ALIGN=128`). Both are disclosed MVP bounds, tunable via
  `EUNOMIA_HEAP_BYTES` / a larger arena; the small-input sort correctness is also covered by
  coretests `slice` sorting.
- **`MAX_ALIGN = 128` remains a trusted arena-seam bound**, kept honest by the `urt` Miri +
  proptest (now covering alignments to 128; `over_alignment_returns_null` asserts 128
  succeeds and 256 is a clean OOM).
- **Multi-MiB binaries are loaded by embedding, not the store** — the MVP fs read path is
  O(n²) for large files; the bulk-window data plane (rev2§3.1) is the deferred proper fix.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

No new seam and no Baseline count moves. `MAX_ALIGN` 64→128 is inside the already-trusted
`urt::Heap` arena seam (Miri+proptest, explicitly *not* one of the 14) — it adds no
`external_body`/`assume_specification` and changes no verified obligation (the `urt` Verus
count is **29, unchanged**). Everything else — the two mini-crates, `build_user_test`, the
shell embed/env-opt-out, the runner, the skip files, the CI step — is test/tooling
infrastructure, the same genre as the 7-1 gate. **Tally stays 14.**

## Follow-ups

- **Console-child spawn/reap churn** wedges console input after ~27–46 heavy-output
  thread-capable spawns. `--full` sidesteps it (whole-suite = one spawn); the underlying
  per-spawn resource churn (likely a console/cap census edge, cf. findings 16-1) is worth a
  dedicated kernel/shell investigation — it would let per-module `--full` sweeps run.
- **`MAX_BLOCK` / bulk-window fs** (both deferred): raising the 256-byte startup block would
  remove the `--skip`-budget squeeze (currently mitigated by the libtest env opt-out); the
  fs bulk-window would let multi-MiB binaries load from the store, retiring the shell embed.
- **Heap-bound skips**: `sort::`'s large-input tests re-enable with a larger
  `EUNOMIA_HEAP_BYTES` (bounded by the shell donation); `vec::overaligned_allocations`
  re-enables only if the arena's `MAX_ALIGN` is raised past 128 (not warranted by real code).
- The `--ci` subset is a fast regression gate; the `--full` sweep is the local exhaustive
  pass. New modules become runnable by appending to the runner's lists.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
