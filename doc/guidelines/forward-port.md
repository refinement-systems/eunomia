# Forward-port runbook — the vendored Rust fork

Eunomia's `std` runs on a **vendored fork of the Rust standard library**
(`vendor/rust`, a git submodule) that carries a custom `eunomia` platform-abstraction
layer (PAL). Upstream Rust moves; this fork must be re-based onto newer nightlies over
its lifetime. This runbook is the standing discipline for that re-base: it names the
**two independent version pins**, the **complete diff surface** to re-check on every
bump, the **version-dependent invariants** that break *silently* if upstream reroutes
them (chiefly the panic→`STATUS_PANIC` terminus), and the **regression set** that
proves a re-base is clean.

The PAL's verification posture is the `kernel/`-over-`kcore` posture: the fork is a
**trusted term-for-term shell** that by construction never runs `cargo verus verify`,
delegating every non-trivial function to a gated crate (`urt`, `eunomia-sys`, `loader`,
`ipc`). The audit that keeps that shell honest is the PAL thin-delegator review in
`doc/guidelines/verus.md` (§11 inverse-leak rule) against
`doc/guidelines/verus_trusted-base.md` (the ledger). This runbook is the *maintenance*
complement: what to re-verify when the shell's upstream substrate changes.

> Scope discipline (per the project comment/doc rules): this guideline references only
> `rev2§…` (the spec, `doc/spec/spec_rev2.md`) and other `doc/guidelines`, and describes
> concrete source paths. It does not cite `doc/plans` or `doc/results` (temporary
> intermediate reports).

---

## 1. The two pins, deliberately decoupled

Two upstream toolchains are pinned **independently**. A bump to one does **not** move
the other — this decoupling is intentional, so that following upstream `std` never
perturbs the deductive-verification gate, and vice-versa.

**(A) The `std` / build-std nightly** — what compiles the vendored `std`, `core`,
`alloc`, and every `user/*` binary:

- `kernel/rust-toolchain.toml` → `channel = "nightly-YYYY-MM-DD"` + `rust-src`. Scoped
  to `kernel/` on purpose: a root-level pin would drag the Verus gate and the host test
  suite onto this nightly. The `user/*` sub-builds inherit it via `RUSTUP_TOOLCHAIN`
  (they are spawned from `kernel/build.rs` with a cwd outside `kernel/`, so they do not
  discover this file by walk).
- `.github/workflows/ci.yml`, the `on-os` job toolchain, must name the **same** date.
- The `vendor/rust` submodule commit is a patch stack whose **base nightly** must equal
  that date (see §2).

**(B) The Verus gate** — what proves the project crates (`kcore`, `ipc`, `urt`,
`freelist`, `le-bytes`, `dma-pool`, `cas`, `virtio-blk`, `storage-server`, `loader`,
`eunomia-sys`):

- `.github/workflows/ci.yml`, the `verus` job env: `VERUS_VERSION` (the release-binary
  version, e.g. `0.2026.06.07.cd03505`) + `VERUS_TOOLCHAIN` (the Rust the prover uses,
  e.g. `1.95.0`).
- The `vendor/verus` submodule pins the matching prover source (`release/rolling/<VERUS_VERSION>`).
- The `vstd` companion is pinned per-crate in the `Cargo.toml` deps.
- Confirm a local install with `verus --version` — it must print exactly the pinned
  `Version:` and `Toolchain:` (see `doc/guidelines/verus.md`).

**Decoupling consequence.** A `std` nightly bump (A) does not touch the Verus gate (B):
the verified crates are host-built on their own toolchain, do not link the vendored
`std`, and their proof obligations are unaffected. Conversely a Verus bump (B) — a rare,
deliberate act — does not move the `std` nightly. Bump each on its own cadence; never
assume one implies the other.

---

## 2. The nightly ↔ commit invariant

The superproject pins `vendor/rust` at a **commit**, not a branch — the eunomia patch
stack layered on an upstream `std`. Three statements must name the **same base nightly**:

1. `vendor/rust`'s base — the upstream `library/std/src/version` reading and the nightly
   its patches were rebased onto (recorded in the `kernel/rust-toolchain.toml` header
   comment as `nightly-YYYY-MM-DD == rustc commit <hash>`).
2. `kernel/rust-toolchain.toml` `channel`.
3. `.github/workflows/ci.yml` `on-os` toolchain.

The `vendor/rust` **commit advances** as eunomia patches land (each std-port phase that
edits a PAL arm bumps it); the **base nightly stays fixed** between deliberate bumps.
The failure this guards is the one that has bitten before: the submodule commit drifting
past the pin while the three nightly statements fall out of lockstep. On every bump,
re-assert all three name the same date, and update the header comment's `rustc commit`
hash to match.

---

## 3. The diff surface — what to re-check on every bump

Everything below is eunomia-specific carry that upstream can move, rename, or reshuffle.
A bump re-bases the patch stack and re-checks each item.

### 3.1 The PAL proper
- `vendor/rust/library/std/src/sys/pal/eunomia/mod.rs` — `_start` (the `ENTRY(_start)`
  entry: TLS-init → bootstrap → `lang_start` main → dtors → `thread_exit`), and the
  `extern "Rust"` declarations of the `__eunomia_*` seam symbols it imports.
- `vendor/rust/library/std/src/sys/pal/eunomia/common.rs` — `init`/`cleanup`/`unsupported`
  and **`abort_internal`** (§4).
- `vendor/rust/library/std/src/sys/pal/eunomia/futex.rs` — the futex backend surfaced as
  `crate::sys::futex`.
- The PAL selector arm: `vendor/rust/library/std/src/sys/pal/mod.rs`
  (`target_os = "eunomia" => { mod eunomia; pub use self::eunomia::*; }`).

### 3.2 The per-module `eunomia.rs` arms
`vendor/rust/library/std/src/sys/<module>/eunomia.rs` for:
`alloc`, `args`, `env`, `fs`, `io/error`, `paths`, `random`, `stdio`, `thread`, `time`,
and `thread_local/key`. Plus the **`exit` arm**, which lives inline in the shared
`sys/exit.rs` (`target_os = "eunomia"` block), not a separate file.

### 3.3 The dispatchers that gained an eunomia arm
Upstream reshuffles these `cfg_select!` / `cfg_if` blocks; each must still route eunomia:
- the module `mod.rs` for every arm in §3.2, and
- the five sync dispatchers `sys/sync/{condvar,mutex,once,rwlock,thread_parking}/mod.rs`,
  which all route eunomia to the futex backend (Mutex/Condvar/RwLock/Once/Parker then
  come free from upstream's futex impls).

### 3.4 The `restricted_std` allowlist
`vendor/rust/library/std/build.rs` — the `|| target_os == "eunomia"` clause in the big
allowlist `if`. Being on this list is what lets build-std compile **full std** for the
JSON target without emitting `cargo:rustc-cfg=restricted_std`. Upstream edits this list
often; if the clause is dropped, downstream binaries fail to build (or would need
`#![feature(restricted_std)]`, which eunomia deliberately does **not** carry).

### 3.5 The `extern "Rust"` `__eunomia_*` bridge (the std ↔ seam contract)
std cannot take `eunomia-sys` as a sysroot dependency (its verified deps pull `vstd`,
whose `verus_builtin` is not buildable as a `rustc-dep-of-std` crate), so the seam is a
link-time bridge: `std` declares each symbol as an undefined `extern "Rust"` fn, and
`eunomia-sys/src/pal.rs` provides the matching `#[no_mangle] extern "Rust"` shim (each a
one-line delegation). A consuming binary does `extern crate eunomia_sys;` to pull the
definitions in. **The two sides must stay in exact lockstep** — a symbol std declares
with no shim is a link error; a signature skew is undefined behavior. The current
contract (all shims defined in `eunomia-sys/src/pal.rs`):

- **entry / lifecycle:** `__eunomia_bootstrap_init`, `__eunomia_argv`, `__eunomia_env`,
  `__eunomia_thread_exit`
- **alloc:** `__eunomia_alloc`, `__eunomia_dealloc`
- **TLS:** `__eunomia_tls_{init_main,init_thread,create,get,set,destroy,run_dtors,free_thread}`
- **thread:** `__eunomia_thread_{spawn,join,yield,sleep}`
- **futex:** `__eunomia_futex_{wait,wake,wake_all}`
- **stdio:** `__eunomia_stdio_write`, `__eunomia_stdout_write`, `__eunomia_stderr_write`,
  `__eunomia_stdin_read`
- **time:** `__eunomia_mono_ns`, `__eunomia_wall_ns`
- **random:** `__eunomia_fill_bytes`
- **io-error:** `__eunomia_io_classify`, `__eunomia_io_message`
- **fs:** `__eunomia_fs_{read,write,stat,metadata,rename,unlink,sync,readdir_open,readdir_next,readdir_close}`

On a bump, diff the set std declares (across `sys/pal/eunomia/mod.rs` and the `eunomia.rs`
arms) against the shims in `eunomia-sys/src/pal.rs`; a mismatch on either side is a defect.

### 3.6 The build wiring — `kernel/build.rs`
The eunomia target is built out-of-tree via cargo's test-only build-std mechanisms:
- `-Zjson-target-spec` + `--target <abs path>/targets/aarch64-unknown-eunomia.json` — the
  custom-spec build (JSON at repo-root `targets/`, resolved as `root.join("targets")`).
- `-Zbuild-std=core,compiler_builtins,alloc,std,panic_abort`
  + `-Zbuild-std-features=compiler-builtins-mem` (the `memcpy`/`memset`/… intrinsics std's
  fmt/io paths emit).
- `__CARGO_TESTS_ONLY_SRC_ROOT = vendor/rust/library` — redirects build-std's `std` source
  to the vendored fork (rustup's `rust-src` is *not* used for the source; the toolchain is
  used only for the compiler).
- The **libtest variant** (`build_user_test`) adds `,test` to the build-std set and
  `-Zpanic-abort-tests` (the test/bench profile is panic=unwind, so a `test`-linked binary
  needs this to build under panic=abort).

**The build-std cache trap (the top forward-port hazard).** `-Zbuild-std` fingerprints the
*toolchain*, not the `__CARGO_TESTS_ONLY_SRC_ROOT`-redirected source — so an edit to the
vendored `library/std/src` **silently caches the old std and never rebuilds it**. Symptom:
after editing e.g. `sys/stdio/eunomia.rs`, stdout still "works" via the stale code path
while a changed path (stdin, a new arm) quietly misbehaves, and nothing recompiles.
`kernel/build.rs` closes this with a `rerun-if-changed` on `vendor/rust/library/std/src`
plus `build_std_is_stale`, which wipes the per-binary build-std cache (`target/user`) so
the next build recompiles std from current source. **Edits to the vendored `core`/`alloc`
(outside `std/src`) are *not* tracked** — after one, `rm -rf target/user` by hand. Any new
generated/vendored input a build depends on must be wired into `build.rs` the same way (a
`rerun-if-changed` and, where a downstream cache ignores it, an explicit invalidation),
not worked around with a manual clean.

### 3.7 The target JSON
`targets/aarch64-unknown-eunomia.json` — generated from `aarch64-unknown-none-softfloat`
with one semantic edit (`"os": "eunomia"`). The upstream-coupled fields to re-verify on a
bump (rustc can rename or newly-require spec keys across nightlies):
- `"abi": "softfloat"` **and** `"rustc-abi": "softfloat"` — FP-register-in-ABI off. These
  are the fields most likely to drift or be renamed; `rustc-abi` supersedes an explicit
  `-fp-armv8` in the feature string (the older knob, deliberately absent).
- `"features": "+v8a,+strict-align,-neon"` — SIMD codegen off (`-neon`), strict alignment.
- `"panic-strategy": "abort"` — wires in `panic_abort` (the terminus, §4).
- `"llvm-target": "aarch64-unknown-none"`, `"data-layout"`, `"max-atomic-width": 128`.

Softfloat is **mandatory, not a preference**: `kcore::thread::TrapFrame` saves
general-purpose registers only (no `q0–q31`/`fpsr`/`fpcr`), so hardware FP/NEON in EL0
would be silently corrupted under preemption. Do **not** flip these to hardfloat/`+neon`
during a bump — that is its own future change gated on growing `TrapFrame` (rev2§ deferred
work), not a forward-port task.

---

## 4. The panic → `STATUS_PANIC` terminus — re-verify chain

This is the **marquee version-dependent invariant** and the one most likely to break
*silently* on a bump, because eunomia rides an **unmodified upstream chain** with no arm
of its own in the middle hops. In a std binary the application cannot supply
`#[panic_handler]` (std owns it), so the rev2§5.1 reaper contract is preserved by
overriding the **PAL's `abort_internal()`**, which the whole panic/OOM/`process::abort()`
path funnels through. The chain (each hop's file is a re-verify point):

1. `panic!` → **`panic_with_hook`** — `library/std/src/panicking.rs` (default hook prints
   last-words via `panic_output` → the eunomia stdio arm → `debug-log`).
2. → **`rust_panic`** — `library/std/src/panicking.rs`.
3. → **`__rust_start_panic`** — `library/panic_abort/src/lib.rs`
   (`#[rustc_std_internal_symbol]`; **unmodified upstream** — eunomia adds no arm here).
   It calls an `extern "Rust"` `__rust_abort` "defined in std::rt".
4. → **`__rust_abort`** — `library/std/src/rt.rs` (`#[rustc_std_internal_symbol]`), which
   calls `crate::process::abort()`.
5. → **`process::abort`** — `library/std/src/process.rs`, which calls
   `crate::sys::abort_internal()`.
6. → **`crate::sys::abort_internal`** resolves to the eunomia PAL: `sys/mod.rs`'s
   `pub use pal::*;` glob + the `sys/pal/mod.rs` eunomia arm.
7. → **eunomia `abort_internal`** — `sys/pal/eunomia/common.rs`, which calls
   `__eunomia_thread_exit(u64::MAX)`.
8. → the `eunomia-sys` shim → the kernel `ThreadExit` terminus; the parent reaper reads
   `STATUS_PANIC`.

So the **one** `abort_internal` override catches ordinary panic, double-panic, non-unwinding
panic, OOM (`handle_alloc_error`), and explicit `process::abort()` alike — **no custom panic
hook is needed and none is installed.**

**Why it is fragile.** The `unsupported` PAL template's `abort_internal` is a raw
`intrinsics::abort()` (a `udf`) that would **not** signal `STATUS_PANIC`. The override is
therefore mandatory, and the routing that reaches it (hops 3–5) is upstream code eunomia
does not own. On a bump, re-verify:
- `panic_abort::__rust_start_panic` still resolves `__rust_abort` via the `extern "Rust"` +
  `#[rustc_std_internal_symbol]` mechanism (a future rustc that reverts `panic_abort` to
  call `intrinsics::abort()` **directly** would bypass the override and break the terminus
  with no error).
- `std::rt::__rust_abort` still exists and still calls `process::abort()` (an upstream
  rename/move of `__rust_abort` breaks the chain silently).
- `process::abort` still routes through `crate::sys::abort_internal`.
- The eunomia `abort_internal` still passes `u64::MAX`.

**The clean-exit arm is separate and must not collide.** `process::exit(code)` routes
through the `sys/exit.rs` eunomia arm → `__eunomia_thread_exit(code as u32 as u64)`. The
`code as u32 as u64` **zero-extend** (not sign-extend) is load-bearing: `-1i32 as u64 ==
u64::MAX` would collide with the reserved panic sentinel, so a `process::exit(-1)` would
reap as a crash; zero-extension keeps the top half clear so no 32-bit exit code reaches the
all-ones sentinel — that value is reachable only via `abort_internal`. The `_start` orderly
return (`sys/pal/eunomia/mod.rs`) zero-extends in lockstep.

---

## 5. The `STATUS_PANIC == u64::MAX` lockstep invariant

Because std cannot depend on the seam crate, the sentinel value is **duplicated** across
four homes that must agree, each guarded by a compile-time assert where it can be:
- Canonical: `eunomia-sys/src/syscall.rs` — `pub const STATUS_PANIC: u64 = u64::MAX;`
  + `const _: () = assert!(STATUS_PANIC == u64::MAX);`.
- Bridge copy: `ipc/src/sys.rs` — same const + assert.
- Std-side literals (no symbolic const, since std cannot import the crate): the `u64::MAX`
  in `sys/pal/eunomia/common.rs::abort_internal`, and the comment reference in
  `sys/exit.rs`.

On a bump (or any edit to the reaper contract), re-confirm all four agree. This is the same
class the committed cross-bridge lockstep guards cover (the `STATUS_PANIC` and `io_error::Kind`
duplications between the seam crates and the std-side literals): keep the guards and their
teeth in place.

---

## 6. The regression set

The gate that proves a re-base is clean is the CI matrix (`.github/workflows/ci.yml`). A
bump is not done until it is green.

**The `verus` job** — `cargo verus verify -p <crate>` across the 11 gated crates.
**Not** moved by a `std` nightly bump (the verified crates do not link the vendored std),
so a clean run over an unchanged tree needs no re-derivation; it is listed here as the
gate that must stay green. Each crate ends a real run with a
`verification results:: N verified, 0 errors` line; the authoritative per-crate counts and
the tally-of-14 trusted seams are the ledger's Baselines section
(`doc/guidelines/verus_trusted-base.md`). Clean the crate (`cargo clean -p <crate>`, or a
full `cargo clean`) before any run that claims a count — a re-run over an unchanged
`target/` reports *nothing* from stale cache (a false green; see `doc/guidelines/verus.md`).

**The `on-os` job** (QEMU, `-machine virt,gic-version=3 -cpu cortex-a72`) — the live
end-to-end gate that exercises the forward-ported PAL:
- `scripts/m1-test.sh` — cap-mechanism / CDT / revoke / reports / teardown (its stage 8 is
  the two-thread distinct-`TPIDR_EL0` TLS witness).
- `scripts/spawn-test.sh` — spawn/reclaim burn loop + fault / panic / time demo.
- `scripts/std-smoke-test.sh` — the std runtime smoke: `println!`/`format!`/`Vec`/`Box`/
  `String`/`Instant`/`SystemTime`/`process::exit`, **and a real `panic!` that must reap as
  `STATUS_PANIC`** — this is the live proof of the §4 terminus (the harness hard-fails on
  `faulted(` or `exited(254|101)`).
- `scripts/fs-smoke-test.sh` — File / read / write / read_dir / rename / remove / sync.
- `scripts/libtest-on-target.sh --ci` — the on-target `coretests`/`alloctests` subset
  (built with the §3.6 libtest variant); committed skip lists live under
  `scripts/libtest-skips/`.

**Fuzzing** — the committed corpora + Miri replay for the verified decoders (wire, on-disk,
ELF, startup, path). Re-run after any decode-surface change.

**Local QEMU note.** When running the `on-os` scripts by hand, QEMU must be killed by the
harness or it runs forever (it waits at the shell after piped stdin hits EOF); `timeout`
is not installed on the dev machine. Use the process-group-kill pattern in the top-level
development guide (`CLAUDE.md`); `pkill -f qemu-system-aarch64` cleans up an orphan.

---

## 7. The bump procedure

1. **Re-base the patch stack.** Rebase the eunomia patches (§3) onto the target upstream
   nightly in `vendor/rust`; resolve conflicts arm-by-arm.
2. **Align the three nightly statements** (§2): `kernel/rust-toolchain.toml` `channel`, the
   `on-os` CI toolchain, and the `vendor/rust` base — all the same date; update the
   toolchain-header `rustc commit` hash.
3. **Re-check every diff-surface item** (§3): PAL proper, per-module arms, the dispatchers
   (esp. that each `cfg_select!` still routes eunomia), the `restricted_std` allowlist, the
   `__eunomia_*` bridge lockstep, the build-std flags, and the target JSON fields.
4. **Re-verify the panic terminus** (§4) at the source level — each hop still resolves —
   and the `STATUS_PANIC` lockstep (§5).
5. **Run the regression set** (§6) green: `verus` job, `on-os` job (the std-smoke panic
   assertion is the live terminus proof), fuzzing.
6. **Leave Verus put** — a `std` nightly bump does not touch the Verus pin (§1); bump it
   only as a deliberate, separate act, and only then re-derive the ledger Baselines.

**Deferred escape hatch.** Tier-3 upstreaming of `aarch64-unknown-eunomia` (a target spec
in `rustc_target` + `sys/pal/eunomia` in-tree) would retire the test-only cargo mechanisms
this build leans on (`-Zjson-target-spec`, `__CARGO_TESTS_ONLY_SRC_ROOT`) and let this
runbook track upstream directly instead of maintaining an out-of-tree patch stack. Until
then, the out-of-tree fork + this runbook are the mechanism.
