# Findings 20 — Phase 6.2: fuzz corpora + PAL thin-delegator audit

Task: **std-port 6.2** (`doc/plans/2_plan-std-revised.md`, Findings #20) — the second
hardening task. Four deliverables: (1) grow the committed fuzz corpora for the verified
decoders (gate: *cargo-fuzz green*); (2) the **PAL thin-delegator audit**, the *standing
gate for the thinness rule* — diff the eunomia PAL arms vs `pal/unsupported`, confirming
zero new logic and that every verified `requires` is re-established/runtime-guarded at
the seam (the §11 inverse-leak rule); (3) retire the copied `svc` inline-asm in
`eunomia-sys`; (4) retire the surviving private grant-resolver duplicate.

**Headline:** the trusted userspace `svc` asm now has a single home (`ipc::sys::imp`,
reused by `eunomia-sys` instead of copied); the `GrantKind` projections have a single
home (`loader::startup`, reused by `eunomia_sys::grant` and the no_std `console`/
`storaged` drivers); the `path` verified decoder gained its missing corpus generator
(15 → 29 curated seeds) and `ipc/connect_decode` grew 7 → 12; and the PAL audit found
every arm a term-for-term delegator with its inverse-leak obligations discharged
seam-side. **Ledger tally stays 14**; no verified count moved.

## Scope decisions taken with the user

- **os-cfg sprawl → deferred with a documented follow-up.** The
  `any(target_os="none", target_os="eunomia")` sprawl is ~40 `#[cfg]` sites across 4
  *verified* crates (loader, ipc, eunomia-sys, urt) plus 5 user `build.rs` string
  checks. The plan's `bare_metal` cfg alias would add a `build.rs` to each of the 4
  crates (none has one today) and force a re-verify of all four — high blast radius for a
  purely cosmetic dedup the plan itself calls "an open cleanup, not a blocker." Recorded
  as a concrete follow-up (census below) rather than implemented in 6.2.
- **grant-resolver dedup → via `loader`.** See Decision 2.

## What shipped

- **svc-asm dedup (`ipc/src/sys.rs`, `eunomia-sys/src/syscall.rs`).** `ipc::sys::imp`
  (both the real-asm and host-stub variants) is now `pub`; `eunomia-sys/src/syscall.rs`
  deletes its copied ~40-line target `imp` and, on the target arm, re-uses ipc's shims
  (`use ipc::sys::imp::{syscall7 as syscall, syscall2, syscall3}` — eunomia-sys's 8-arg
  form ≡ ipc's `syscall7`). It keeps a local host stub for its off-target tests (`ipc` is
  a *target-only* dep, and `syscall.rs` compiles on host). The verified `encode` still
  gates every argument; only the raw register marshalling is trusted, now defined once.
- **grant-resolver dedup (`loader/src/startup.rs`, `eunomia-sys/src/grant.rs`,
  `user/console`, `user/storaged`).** The pure `GrantKind` projections (`region`,
  `cap_slot`, `storage_handle`, `region_va`) moved to `loader::startup` (with the type),
  plus a `grant_projections` unit test. `eunomia_sys::grant` re-exports them
  (`pub use loader::startup::{…}`) and keeps its named-role layer, so
  `eunomia_sys::grant::region` (etc.) — the PAL/bootstrap path — is unchanged. The two
  no_std drivers dropped their byte-identical private `region()` and call
  `startup::region`; `storaged`'s now test-only `GrantKind` import is `#[cfg(test)]`-scoped.
- **fuzz corpora.** New `eunomia-sys/examples/gen_eunomia_sys_corpus.rs` (the only
  verified decoder that lacked a generator), growing `path` 15 → 29 curated seeds across
  every equivalence class + the length/depth boundaries. `ipc/examples/gen_ipc_corpus.rs`
  enriched `connect_decode` 7 → 12 (window/version boundaries + the low-side length
  rejection + grant/refusal trailing-byte edges). Both grown by *curated generator
  seeds*, not committed libFuzzer output (Decision 4).
- **PAL audit + ledger.** The audit is recorded below. The `eunomia-sys`
  syscall-marshalling routing note in `doc/guidelines/verus_trusted-base.md` was updated
  to point the trusted `svc` asm at its new single home (`ipc::sys::imp`); tally stays 14.

## Decisions (and rejected alternatives)

1. **The shared `svc` asm lives in `ipc`, not `eunomia-sys`.** Findings #2 imagined
   "consolidate ipc/urt onto eunomia-sys's syscall layer", but the dependency graph
   forbids it: `eunomia-sys` depends on `ipc` (under the target cfg), `ipc` has no reverse
   edge. So `ipc` is the lower crate and the natural home; `eunomia-sys` re-uses it.
   *Rejected:* a new shared asm crate (needless third crate for ~40 lines); reversing the
   dep (a cycle). Exposing `ipc::sys::imp` as `pub` is the minimal move — the shims are
   `#[inline(always)]`, so cross-crate inlining keeps the emitted code identical.
2. **`GrantKind` projections move to `loader::startup` (the "via loader" option).** The
   projections are structural readers of `GrantKind`, which lives in `loader::startup`;
   the no_std `console`/`storaged` drivers already depend on `loader`, so they share one
   definition with no new deps. *Rejected:* adding an `eunomia-sys` dependency to the two
   minimal drivers (pulls the whole PAL tree — urt/ipc/storage-server — into a console
   and a storage server); leaving the duplicate (the plan's spirit is one home).
3. **The plan's literal `resolve_*` target was already satisfied.** `user/shell`'s
   `resolve_seed` and `user/init`'s helpers were removed in 5.3 when both became
   std/producer binaries bootstrapped through `eunomia_sys`. 6.2 consolidated the only
   surviving duplicate — the byte-identical `region()` in `console`/`storaged` that
   `grant.rs`'s own doc names as a consolidation target.
4. **Fuzz-corpus growth for verified *total* decoders is via curated generator seeds,
   not committed libFuzzer output.** A 60 s `hunt` on `path` ran 19.3M execs with
   coverage flat at **136** the entire time — libFuzzer only accumulated *feature-count*
   variations (edge-hit multiplicities), no new code coverage, because the curated seeds
   already exercise every branch (the differential oracle has the same branch structure
   as the verified `resolve`). Committing ~80 random-ish inputs would be repo bloat that
   reads as "more coverage" while adding none. So the growth is the enriched, documented,
   coverage-complete generator seeds; the hunt is retained as *evidence of completeness*,
   not as a source of committed inputs. *Rejected:* committing the hunt's in-place corpus
   (bloat); `cargo fuzz cmin` (would delete the curated named seeds, replacing them with
   opaque SHA1 minimizations — losing the documented intent). This is the plan's
   sanctioned fallback, and for a verified total decoder it is the *superior* primary.
5. **Left the mature corpora (loader 100/83/20, storage 586) untouched.** They were
   fuzzer-grown when their decoders were new and the search still found coverage; a
   re-hunt now yields the same flat-coverage bloat Decision 4 describes. The scheduled
   `fuzz.yml` `hunt` job (06:00 UTC) already re-exercises them nightly.

## The PAL thin-delegator audit (the standing thinness gate)

Scope: `vendor/rust/library/std/src/sys/pal/eunomia/{mod,common,futex}.rs`, `sys/exit.rs`'s
eunomia arm, and the 12 per-module arms (`sys/{alloc,args,env,fs,io/error,paths,random,
stdio,thread,thread_local/key,time}/eunomia.rs`). `net`/`process`/`pipe`/`fd` have no
eunomia arm (fall through to `unsupported`).

**Every arm is a term-for-term delegator over the `__eunomia_*` bridge** (all 36 shims
defined `#[no_mangle] pub extern "Rust"` in `eunomia-sys/src/pal.rs`, each a one-line
forward into `urt`/`ipc`/`loader`/`eunomia_sys`). No arm holds protocol/arithmetic/parsing
logic beyond the std-side bookkeeping std *requires* of any PAL:

- **Pure marshalling (zero new logic):** `alloc`, `args`, `random`, `stdio`, `io/error`,
  `thread_local/key`, `futex` (only a `u64::MAX` no-timeout sentinel guard), `time` (only
  the `ns.max(0)` inverse-leak guard, otherwise identical to `unsupported`), and the
  `pal/eunomia/{mod,common}.rs` + `exit.rs` process-entry/exit shell (only the
  `code as u32 as u64` zero-extend guard).
- **Necessary std-side bookkeeping (not protocol logic):** `fs/eunomia.rs` (the largest,
  603 lines) holds `File`/`ReadDir`/`FileAttr` cursor bookkeeping, `File::open`
  precondition emulation, and `parse_listing` (the readdir buffer decoder); `env`'s
  `split_kv` (first-`=` split); the `thread` trampoline (`Box<ThreadInit>` round-trip in
  the required motor/hermit shape); `io/error`'s `decode_error_kind` (`u8` → `ErrorKind`
  table). All of this is the shape std demands of *any* target's PAL, not eunomia logic.

**Inverse-leak re-establishment (§11).** Every verified `requires` reachable through the
seam is discharged, almost all *seam-side* in `eunomia-sys/src/pal.rs` (so the vendored
PAL arm carries none of it):

| Verified `requires` (crate) | Re-established at | How |
|---|---|---|
| `urt::Heap::alloc` — none (total) | `pal.rs` `__eunomia_alloc` | vacuous; GlobalAlloc's non-zero layout also defended by `size.max(1)` |
| `urt::tls::KeyTable::create/destroy` (`key != 0` on the std side) | `thread_local/key/eunomia.rs` | `rtabort!("out of TLS keys")` when the seam returns 0 |
| `urt::time` monotonicity + `Duration` domain | `time/eunomia.rs` | `ns.max(0)` before `Duration::from_nanos` (called out as the §11 guard) |
| kernel `DebugWrite` length cap (`ERR_FAULT`) | `eunomia-sys/src/stdio.rs` | chunk to `DEBUG_WRITE_MAX` |
| entropy "seed attached" | `pal.rs` `__eunomia_fill_bytes` | loud abort if unseeded |
| `"time"` grant attached | `pal.rs` `__eunomia_wall_ns` | panic if absent |
| thread stack-size bound | `thread/eunomia.rs` | `Builder::stack_size` over the min → seam `ERR_ARG` |

**Structurally-forced cross-bridge duplications (review-kept).** std reaches the seam
only through the `__eunomia_*` symbols — it *cannot* import `eunomia_sys` types (its
verified deps pull `vstd`, unbuildable as a `rustc-dep-of-std` crate; findings #7-2). So
three constants/layouts are necessarily mirrored on both sides and kept honest by review,
not by a shared definition: the `STATUS_PANIC = u64::MAX` sentinel (`pal/eunomia/{mod,
common}.rs`, `exit.rs` vs `eunomia_sys::syscall::STATUS_PANIC`); the `io_error::Kind`
`#[repr(u8)]` discriminants (`io/error/eunomia.rs::decode_error_kind` vs
`eunomia_sys::io_error`); and the readdir wire layout (`fs/eunomia.rs::parse_listing` vs
`eunomia_sys::fs`). Listed as follow-ups. This is a **property of the bridge**, not a
thinness violation — the alternative (a type dependency) is the one findings #7-2 proved
impossible.

**Verdict:** the PAL is the `kernel/`-over-`kcore` posture — thin, term-for-term
delegation over the verified core, adding no `verus!{}` obligation and no seam.

## Problems hit and how they were solved

- **`storaged` `GrantKind` unused-import warning.** After removing its private `region()`,
  `GrantKind` was used only by the `#[cfg(test)]` tests, so the target (non-test) build
  warned. Fixed by `#[cfg(test)] use loader::startup::GrantKind;` (the test module's
  `use super::*` re-exports it). `console`'s `GrantKind` was used *only* by its `region()`,
  so its import was dropped entirely (`use loader::startup;`).
- **The `hunt`-as-growth trap.** The first instinct — run `hunt` and commit what libFuzzer
  adds — produces coverage-flat bloat for verified total decoders (Decision 4). Caught by
  watching `cov:` stay at 136 across 19.3M execs; reverted the ~80 SHA1-named files and
  kept the curated generator seeds.

## Verification record

| Gate | Command | Result |
|---|---|---|
| verus — ipc (cold) | `cargo clean -p ipc && cargo verus verify -p ipc` | **71 verified, 0 errors** (unchanged) |
| verus — loader (cold) | `cargo clean -p loader && cargo verus verify -p loader --no-default-features` | **30 verified, 0 errors** (unchanged) |
| verus — eunomia-sys (cold) | `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` | **16 verified, 0 errors** (unchanged) |
| host tests | `cargo test -p ipc -p eunomia-sys -p loader` | **all pass** (`grant_projections`, `resolvers_read_each_named_grant` incl.) |
| storaged host tests | `cargo test --manifest-path user/storaged/Cargo.toml` | **6 pass** (`parse_config` suite) |
| fuzz — smoke (all crates) | `scripts/fuzz.sh smoke` | **green** — every target builds + replays its corpus |
| fuzz — replay tests | `cargo test -p cas -p storage-server -p loader -p eunomia-sys --test fuzz_corpus --test fuzz_regressions` + `cargo test -p ipc --features fuzzing --test fuzz_corpus` | **all pass** |
| fuzz — path completeness | `cargo +nightly fuzz run path -- -max_total_time=60` | 19.3M execs, **cov 136 stable** (curated seeds coverage-complete) |
| target build | `cd kernel && cargo build` | **builds** (console/storaged/std binaries via the shared svc shim) |
| QEMU boot smoke | `scripts/run-demo.sh` (perl process-group harness) | `[storaged] store mounted` → `serving`; shell commands echo; no panic |
| std smoke | `scripts/std-smoke-test.sh` | **STD SMOKE TEST PASS** |
| formatting | `cargo fmt --check` + `scripts/verusfmt.sh --check` | **clean** |

Corpus growth: `eunomia-sys/path` 15 → 29; `ipc/connect_decode` 7 → 12; mature corpora
(loader 100/83/20, storage 586, cas 42) unchanged by design.

## Surface left unsupported / trusted (and why)

- **The `svc #0` inline asm (now `ipc::sys::imp`).** rev2§6.1(d) register marshalling —
  inherently unverifiable, the userspace mirror of the kernel's trusted marshalling. 6.2
  did not add trust; it *reduced* it (one asm definition instead of two). Folds under the
  existing thread-lifecycle shell seam; no new seam.
- **The readdir/io-error/`STATUS_PANIC` cross-bridge mirrors.** Trusted-by-review because
  the `__eunomia_*` bridge forbids a shared type (findings #7-2); documented above and as
  follow-ups.
- **The mature corpora were not fuzzer-grown further** — a re-hunt yields no new coverage
  (Decision 4/5); the nightly `fuzz.yml hunt` job covers ongoing search.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

No new seam, no Baseline count moves. The `eunomia-sys` syscall-marshalling routing note
was updated to point the trusted `svc` asm at its single home (`ipc::sys::imp`, reused by
`eunomia-sys`) instead of the retired copy — a location correction, not a tally change.
The grant-projection move is plain Rust outside `verus!{}` (loader stays 30; note 3.4's
entropy-seed decode had raised loader 29 → 30 and 4.2's path resolver raised eunomia-sys
7 → 16 before this task — both unchanged here). The fuzz
generators + corpora are test/tooling infrastructure. **Tally stays 14.**

## Follow-ups

- **Deferred: the `bare_metal` cfg alias for the os-cfg sprawl.** Census (repo, excluding
  `vendor`/`target`): ~40 `#[cfg(any(target_os="none", target_os="eunomia"))]` sites —
  `eunomia-sys` (lib.rs, pal.rs, thread/random/futex/fs/tls/stdio/bootstrap, and
  `console.rs`'s 11), `urt` (lib.rs, lock.rs, time.rs, futex.rs), `loader` (lib.rs),
  `ipc` (sys.rs) — plus 5 user `build.rs` `TARGET`/`CARGO_CFG_TARGET_OS` string checks
  (init, selftest, storaged, console, shell). Proposed: one `build.rs` per crate emitting
  `cargo:rustc-cfg=bare_metal` (+ `rustc-check-cfg`) when `CARGO_CFG_TARGET_OS` ∈
  {none, eunomia}, replacing the `any(...)` attributes; a shared build-script helper for
  the 5 user scripts. None of the 4 crates has a `build.rs` today, so this adds four and
  re-verifies all four — deferred as cosmetic per the user decision.
- **Cross-bridge lockstep drift points.** `STATUS_PANIC`, the `io_error::Kind`
  discriminants, and the readdir wire layout are mirrored across the `__eunomia_*` bridge
  by review. A lightweight guard (e.g. a seam-exported constant the PAL asserts against,
  where the bridge allows a scalar) could harden the first two; the readdir layout is the
  one genuine wire-format twin (a shared spec comment exists; a shared codec would need
  the bridge to carry structured data instead of `Vec<u8>`).
- **cas verified-codec corpora** stay at their curated hand seeds; the nightly hunt job
  covers ongoing search. A per-codec generator enrichment (the `path`/`connect_decode`
  pattern) is a low-priority tidy-up.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
