# Findings — Phase 2.4: std time (Instant + SystemTime)

Task 2.4 of `doc/plans/1_plan-rust-std-port.md` — the last brick of the single-threaded
hello-world phase. `sys/time/mod.rs` had no eunomia arm, so `Instant::now()` /
`SystemTime::now()` fell to the `_ => unsupported` arm whose `now()` is
`panic!("time not implemented")`. This wires `Instant` to the AArch64 virtual counter
(`CNTVCT_EL0`/`CNTFRQ_EL0`, **no time grant**) and `SystemTime` to the rev2§2.6 time page
(`urt::now_utc_ns`), reusing the **Verus-verified** `Sample::utc_ns_at` conversion for both.
No new `verus!{}` obligation and no new trusted seam — the tally stays **14**; `urt`
re-cites **25/0** unchanged.

## What shipped

- **`urt`** (the verified time core; extend):
  - `src/time.rs` — `now_mono_ns() -> i64` (the `Instant` basis): reuses the verified
    `Sample::utc_ns_at` over a **zero wall/counter base**
    (`Sample { wall_base_ns: 0, cntvct_base: 0, cntfrq: cntfrq() }.utc_ns_at(cntvct())`), so
    the result is `clamp_i64(cntvct·1e9/cntfrq)` = ns since the counter epoch. Total +
    monotone inherited from `lemma_utc_ns_at_monotone` (cntvct is hardware-monotone, cntfrq
    a constant register, so consecutive calls sample the same `Sample`). `aarch64 +
    none/eunomia`-gated, like `now_utc_ns`/`cntvct`/`cntfrq`. Independent of the time *page*
    → needs no `attach`/`"time"` grant.
  - `src/time.rs` `tests` — a new `zero_base_conversion_is_non_negative` proptest pinning
    the `Instant` invariant (the zero-base conversion is ≥0 ∀ counter/frequency — the
    precondition the PAL's `max(0)`→`from_nanos` relies on). urt host suite 22→23.
- **`eunomia-sys`** (the seam; extend):
  - `src/pal.rs` — two `#[no_mangle] extern "Rust"` shims, `__eunomia_mono_ns` →
    `urt::time::now_mono_ns`, `__eunomia_wall_ns` → `urt::time::now_utc_ns` (one-line
    delegations; disassembly: `bl <fn>; ret`).
  - `src/bootstrap.rs` — `init()` now calls a target-gated `attach_grants()` after `commit`:
    resolves `grant::time_va(startup())` and `urt::time::attach(va)` when the `"time"` grant
    is present. cfg-split so the host build (no `urt` dep) stays byte-identical.
- **`vendor/rust`** std PAL (the trusted term-for-term shell):
  - `sys/time/eunomia.rs` (new) — `Instant`/`SystemTime` over the two extern symbols; bodies
    are the `unsupported.rs` template verbatim except the two `now()`s, which wrap the seam's
    `i64` ns into `Duration` with a `ns.max(0)` guard (the §11 inverse-leak re-establishment
    of `Duration::from_nanos`'s `u64` domain). All other methods (`checked_*`, `sub_time`,
    `UNIX_EPOCH`, `MAX`/`MIN`) copied unchanged.
  - `sys/time/mod.rs` — a `target_os = "eunomia"` arm (`mod eunomia; use eunomia as imp;`)
    after `motor`, before `sgx`.
- **Ledger** (`doc/guidelines/verus_trusted-base.md`): a std-port-2.4 paragraph appended to
  the eunomia-sys routing note — time is trusted shell over verified `urt::time`; no new
  seam, tally stays 14.

## Decisions (and rejected alternatives)

- **`Instant` reads the counter directly (no page, no grant); `SystemTime` reads the page
  (grant).** The split is load-bearing for Phase 3.3: the futex yield-poll timeouts
  (`wait_timeout`/`park_timeout`) call `Instant` in processes that may hold no `"time"`
  grant, so `Instant` must never depend on it. `CNTVCT`/`CNTFRQ` are EL0-readable with no
  syscall and no IPC.
  - *Rejected:* backing `Instant` with `now_utc_ns` too — it would panic without a `"time"`
    grant, breaking timeout measurement in ungranted processes.
- **Reuse the verified `utc_ns_at` with a zero base for `Instant`, rather than dividing in
  the PAL.** `clamp_i64(cntvct·1e9/cntfrq)` is exactly `utc_ns_at` with
  `wall_base = cntvct_base = 0`; reusing it inherits **totality** (the ~5-minute `u64`
  overflow at 62.5 MHz is already handled by `utc_ns_at`'s seconds/remainder decomposition)
  **and monotonicity** from the proof, and keeps zero arithmetic in the trusted shell.
  - *Rejected:* raw `delta·1e9/cntfrq` in the PAL — the overflow-prone arithmetic the
    thinness rule forbids in the shell.
- **The conversion lives in `urt` (`now_mono_ns`), not `eunomia-sys`.** Per the plan's
  "Where new logic lives" table, time conversion is urt's domain; `now_mono_ns` is the
  monotonic counterpart of `now_utc_ns` and makes the seam shim a one-liner. It is a
  **non-`verus!{}` exec fn** (`aarch64`-gated out of the Verus host build), so urt's own
  count stays **25** — the gate is genuinely re-run on changed urt and reports 25/0.
- **Resolve the no-time-grant panic by wiring the attach, not by suppressing it.**
  `bootstrap::init` attaches the time page when the `NAME_TIME` grant is present (so
  `SystemTime` works); an ungranted process keeps urt's loud `now_utc_ns` panic
  (`"time page not attached"`) — the documented "asking for wall time without the grant is
  mis-wired, not degraded" posture (`urt/src/time.rs`).
  - *Rejected:* returning `Err`/`UNIX_EPOCH` on no grant — std's `SystemTime::now()` is
    infallible, and silent-wrong wall time is worse than a loud abort. `Instant` still works
    without the grant, so timeout code is unaffected.
- **The §11 guard (`max(0)`) lives in the PAL arm, where `Duration` is the concern.** The
  seam returns raw `i64` ns; the arm re-establishes `Duration::from_nanos`'s `u64`
  non-negativity at the boundary. `Instant`'s value is ≥0 by construction (pinned by the new
  urt proptest); `SystemTime`'s is ≥0 for the post-1970 MVP RTC.

## Problems hit and how they were solved

- **The plain `cd kernel && cargo build` did not re-exercise the std arm.** It finished in
  ~7 s recompiling only the kernel crate (the build-std graph was cached, and the no_std
  `user/*` binaries don't link std), so it was not a real compile check of
  `sys/time/eunomia.rs`. Resolved with the throwaway **std canary** (fresh `--target-dir`):
  a `fn main()` calling `Instant::now()`/`.elapsed()` + `SystemTime::now()
  .duration_since(UNIX_EPOCH)` (and `checked_add`), `extern crate eunomia_sys;`, built for
  `aarch64-unknown-eunomia` via the `kernel/build.rs` flags — recompiled `std v0.0.0` from
  scratch with the new arm and linked clean. Symbol/disassembly checks
  (`llvm-nm`/`llvm-objdump`): `__eunomia_mono_ns`/`__eunomia_wall_ns` defined (`T`), **no
  undefined `__eunomia` symbols**, `_start` present (`T`), and each shim disassembles to `bl
  <_…urt..time..now_{mono,utc}_ns>; ret` — a pure delegation.
- **`verusfmt.sh --check` flags `eunomia-sys/src/bootstrap.rs` and `io_error.rs` — a
  pre-existing false-positive, not from this change.** `verusfmt.sh` selects files by
  `git grep -l 'verus!'`, which matches the literal `verus!{}` token in those files' **doc
  comments** even though neither has a real `verus!{}` block; verusfmt 0.7.2 then reports
  them "not formatted". Proven pre-existing: `io_error.rs` is untouched on this branch yet
  flagged, and `verusfmt --verus-only --check` fails on `main`'s copies of *both*. The
  authoritative gate `cargo fmt --check` is clean for both touched crates. (Same known issue
  the 2.2/2.3 findings recorded; CI gates neither `cargo fmt` nor `verusfmt`.)

## Verification record

Toolchain `nightly-2026-06-26` (== `vendor/rust` `bd08c9e7…`) for the cross-build; Verus
binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Verus (authoritative, cold)**
  - `cargo clean -p urt && cargo verus verify -p urt` → **25 verified, 0 errors** (the
    plan's re-cite; `now_mono_ns` is non-`verus!{}`, count unchanged).
  - `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` → **7 verified, 0
    errors** (own count unchanged; the `pal` shims + the bootstrap attach add no `verus!{}`).
- **urt Miri sweep** — `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run
  -p urt -j4` → **23 tests: 23 passed, 0 skipped** (incl. the new
  `zero_base_conversion_is_non_negative` under Miri).
- **Host tests** — `cargo test -p urt` → **23 passed**; `cargo test -p eunomia-sys` → **21
  passed** (unchanged; the bootstrap attach is target-gated, not host-exercised).
- **Throwaway std build/link/symbol canary** (removed after) — see Problems; `std v0.0.0`
  recompiled + linked clean, symbols/disassembly as above.
- **Kernel build** — `cd kernel && cargo build` clean.
- **No-regression boot** — `scripts/spawn-test.sh` (under the `CLAUDE.md` Perl group-kill
  backstop) → **SPAWN TEST PASS**: runloop 100/100 slots reclaimed, exit(42)/exit(7)
  propagated, fault demo `faulted(translation, 0xdead0000)` then clean re-spawn, panic demo
  `panicked` (reserved status) not exited(254), **time grant `time-ok`** then clean
  re-spawn, no BSS-LEAK / no unexpected PANIC. (The `time-ok` step exercises the no_std time
  path — unregressed; the std `Instant`/`SystemTime` live run is the combined GATE / 5.3.)
- **Formatting** — `cargo fmt -p urt -p eunomia-sys -- --check` clean (the authority);
  `verusfmt.sh --check` shows only the pre-existing `bootstrap.rs`/`io_error.rs`
  false-positive (see Problems); `vendor/rust` keeps upstream rustfmt style.

The **live** `Instant`/`SystemTime` QEMU assertion is the combined **Phase-2 GATE** (now
that 2.4 wires time) + the std `hello` at 5.3 — there is no std `user/*` binary yet (all are
no_std), so a live `Instant::now()`/`SystemTime::now()` run can only be observed once a std
binary is spawned. This matches how 2.1/2.2/2.3 deferred their live demos. No
`.github/workflows/ci.yml` edit belongs to 2.4.

## Surface left trusted / unsupported (and why)

- **The `sys/time/eunomia.rs` arm + the two `pal.rs` shims** — the trusted term-for-term
  shell, the `kernel/`-over-`kcore` posture (`vendor/rust` is a submodule fork that by
  construction never runs the gate). Each `now()` delegates to the verified seam
  (`urt::time::now_mono_ns`/`now_utc_ns` over the verified `utc_ns_at`). The §11 inverse-leak
  check: the arm re-establishes `Duration::from_nanos`'s `u64` non-negativity via `ns.max(0)`;
  the shims re-establish nothing (`now_mono_ns` is total + non-negative by construction;
  `now_utc_ns` is total where attached, panicking by design where not).
- **`now_mono_ns` reuses the verified conversion but is itself plain exec** (two `mrs`
  register reads + a `Sample` literal) — the same trusted register-read posture as
  `cntvct`/`cntfrq`/`now_utc_ns` (rev2§2.6; the `CNTKCTL_EL1.EL0VCTEN` enable is the
  kernel's). **No new seam:** it adds no `verus!{}` and the conversion it calls is already
  verified; the new urt proptest pins its non-negativity.
- **The no-time-grant panic is intentional, not a gap.** A process calling
  `SystemTime::now()` without a `"time"` grant is mis-wired (the urt posture);
  `bootstrap::init` attaches when granted, and `Instant` needs no grant.

## Follow-ups

- **Combined Phase-2 GATE:** assert `println!`/`format!`/`Vec`/`Box`/`String`/`Instant`/
  `SystemTime` live in QEMU once a std binary exists; **5.3** rewrites `hello` on std and
  observes the live `Instant`/`SystemTime` + the `STATUS_PANIC` reap.
- **Pre-existing verusfmt false-positive** on files that mention `verus!{}` only in a
  comment (`bootstrap.rs`, `io_error.rs`): a small follow-up could add them to
  `scripts/verusfmt.sh`'s skip list (or reword the comments to avoid the literal token, the
  2.3 `stdio.rs` tactic). Out of scope for 2.4; `cargo fmt` is the authority and is clean.
- **Phase 3.3 futex timeouts** depend on the grant-free `Instant` landed here
  (`wait_timeout`/`park_timeout` via `Instant` + `yield_now`).

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
