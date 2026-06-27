# Findings 7-1 — Phase-2 GATE (live std smoke) + remaining phase-2 deferred work

Task: the combined **Phase-2 GATE** of the Rust std port
(`doc/plans/1_plan-rust-std-port.md`), plus the two minor cleanups that
phase 2's sub-phases deferred within their own scope. Numbered **7-1** because
the gate row was never allocated a findings number (it sits after 2.4 = findings 7).

## What this closes

Phase 2's four sub-phases each landed their std PAL arm and then **deferred the
live QEMU demonstration** to a single combined gate, because none could build the
demo: there was **no `std` user binary** (every `user/*` binary is `no_std`). The
four findings docs all point here:

- 2.1 (4): *"the live `env::args()` QEMU demo is deferred to the Phase-2 GATE."*
- 2.2 (5): *"the live `Box`/`Vec`/`String`/`Instant`/`SystemTime` QEMU assertion is
  the combined Phase-2 GATE."*
- 2.3 (6): *"the live `println!`/panic-reap assertion is deferred to the combined
  Phase-2 GATE … there is no std `user/*` binary yet (all are `no_std`)."*
- 2.4 (7): *"Combined Phase-2 GATE: assert `println!`/`format!`/`Vec`/`Box`/
  `String`/`Instant`/`SystemTime` live in QEMU once a std binary exists."*

The gate is now a green CI step booting the first live std binary. The two
in-phase-2 cleanups the findings flagged (the `1024` DebugWrite cap literal; the
verusfmt false-positive on two eunomia-sys files) are also done.

## Deliverables

- `user/stdsmoke/` — the first **std** user binary, a dedicated GATE fixture.
- `kernel/build.rs` — cross-builds it (the hello/selftest pattern; not embedded).
- `scripts/std-smoke-test.sh` — the boot harness (QPID + trap + deadline-poll).
- `.github/workflows/ci.yml` — a new `on-os` step running the harness.
- `kcore::sysabi::DEBUG_WRITE_MAX` — the hoisted shared cap const.
- `scripts/verusfmt.sh` — skip-list += the two no-macro eunomia-sys files.

## Decisions (and rejected alternatives)

1. **Dedicated `user/stdsmoke` fixture, not the real `hello`.** The gate needs a
   std binary; rewriting `hello`/`shell` onto std is Phase **5.3**. Rejected
   porting `hello` now (would pull 5.3 forward and churn `run-demo.sh`). The
   fixture mirrors `selftest`/the m1-test program: a permanent, marker-printing
   test subject. The real binaries stay `no_std` until 5.3, which this gate now
   de-risks (the std entry/alloc/stdio/time/exit path is proven live).

2. **`strip = true` (a deviation vs `hello`/`selftest`)** plus `opt-level="s"` +
   `lto=true`. Motivation: the shell loads a child ELF into a `Vec` on its fixed
   `urt::Heap<{1024*1024}>` (1 MiB) via `read_file` (`user/shell/src/runtime.rs`),
   so a fat ELF could exhaust it. **Measured result: the stripped binary is
   50,176 bytes** — the 1 MiB `.bss` heap reservation is `memsz`, not file bytes,
   so it never inflates the image. The "bump the shell heap" contingency in the
   plan was therefore **not needed**; recorded as a 5.3 follow-up for when a
   larger std binary lands.

3. **No `HashMap` / `fill_bytes` / `std::random` in the fixture.** The eunomia
   `sys/random` arm routes to `unsupported`, whose `fill_bytes` **panics**
   (entropy is Phase **3.4**). `hashmap_random_keys` *would* work (it uses
   allocation addresses, not `fill_bytes`), so `HashMap` is technically usable,
   but the gate stays clear of the whole entropy surface to keep the phase
   boundary clean. Documented in the fixture's module doc.

4. **Failure modes exit non-zero (`process::exit(2/3/4)`)**, not just a missing
   `STD2 PASS`. A bad `Vec` sum, a non-monotonic `Instant`, or a pre-2020
   `SystemTime` makes the shell reap `exited(N)`, so a regression fails loudly via
   a distinct verdict — and exercises the overridden `sys::exit` arm too.

5. **Markers: `[stdsmoke]` prefix per feature + `STD2 PASS` green marker.** The
   shared serial console also carries kernel/shell/storaged output; the prefix
   avoids collisions, and `STD2 PASS` is the `…M1 PASS`-style headline. std's own
   panic hook prints lowercase `panicked at …` and the shell reap prints lowercase
   `panicked`, so the harness can treat any **uppercase** `PANIC` as a hard fail
   with no carve-out (no `selftest` runs in this script, unlike spawn-test).

6. **Cap-const home: `kcore::sysabi`.** The `1024` DebugWrite length cap was a
   bare literal in `kernel/src/syscall.rs` mirrored by a bare `1024` in
   `eunomia-sys/src/stdio.rs` (pinned only by `assert_eq!(.., 1024)`). Hoisted to
   `pub const DEBUG_WRITE_MAX: u64 = 1024` in `kcore::sysabi` (the home of
   `NUM_PRIOS`/`NO_PRIO_CEILING` and the `DebugWrite` variant). The kernel now
   cites it; `eunomia-sys`'s `cap_matches_kernel` test asserts its local `usize`
   twin equals the kcore const **through the existing kcore dev-dep** (the
   `encode.rs::constants_match_kcore` pattern). The local `usize` twin is kept —
   eunomia-sys must not take a non-dev dependency on kcore (the userspace/kernel
   decoupling `ipc::sys` keeps), and the chunker needs a `usize`.

7. **verusfmt: skip-list, not reword.** Confirmed empirically (below) that the two
   files trip verusfmt; per the plan's "if it trips → skip-list" rule, and to
   preserve the meaningful `verus!{}` references in their doc comments, both were
   added to `scripts/verusfmt.sh`'s `SKIP` with a new reason category.

## Problems hit (and resolutions)

- **The first-ever live std binary linked and booted clean on the first attempt.**
  Notable, not a problem: the term-for-term PAL shell built across 2.1–2.4 was
  correct as written. `extern crate eunomia_sys;` resolved all the `__eunomia_*`
  symbols under LTO with no `#[used]` shim needed (the `__rust_alloc` pattern held).
  This is the first time `eunomia-sys` itself was cross-compiled and linked.

- **The verusfmt root cause differed from the 2.4 finding's guess.** 2.4 supposed
  the files trip verusfmt because they "mention `verus!{}` only in a comment". The
  actual mechanism: they contain **no `verus!{}` macro block at all**, but
  `git grep -l 'verus!'` (the script's selector) still matches the comment token
  and feeds them to verusfmt, which — finding no macro — reformats the whole file
  as plain Rust (deleting the blank line after the module doc) and so **conflicts
  with `cargo fmt`**. The SKIP-list header now records this third category.

- **`PIPESTATUS` is empty under zsh** (the session shell), so `${PIPESTATUS[0]}`
  after a pipe reports nothing; re-checked exit status via a temp-file redirect +
  `$?`. Harness note only.

## Verification record

| Gate | Command | Result |
|---|---|---|
| **Phase-2 GATE** | `bash scripts/std-smoke-test.sh` | **STD SMOKE TEST PASS** |
| std link (first ever) | `cd kernel && cargo build` | `stdsmoke` links, **50,176 bytes** |
| host suite (CI) | `cargo test --workspace --exclude kernel` | **exit 0**, no failures |
| cap-const pin | `cargo test -p eunomia-sys cap_matches_kernel` | ok |
| kcore proofs (cold) | `cargo clean -p kcore && cargo verus verify -p kcore` | **407 verified, 0 errors** (base 406 → +1 for the new const) |
| formatting | `cargo fmt` + `scripts/verusfmt.sh --check` | **CLEAN** |
| regression | `bash scripts/spawn-test.sh` | **SPAWN TEST PASS** |

The gate's asserted serial-log markers (the success run `run bin/stdsmoke alpha beta`):

```
[stdsmoke] alive
[stdsmoke] argv=["bin/stdsmoke", "alpha", "beta"]
[stdsmoke] vec sum=5050 box=10100 argc=3
[stdsmoke] instant-ok ns=57008
[stdsmoke] systemtime-ok
STD2 PASS
exited(0)
```

The panic run `run bin/stdsmoke panic`:

```
[stdsmoke] alive
[stdsmoke] panicking
thread 'main' (1) panicked at src/main.rs:48:9:
stdsmoke deliberate panic
panicked                              <- shell reap = STATUS_PANIC (2.3 override)
```

Green verdict = `STD2 PASS` ∧ `exited(0)` ∧ argv echo ∧ `systemtime-ok` ∧
`panicked` present, with no `systemtime-bad`/`instant-bad`/`faulted(`/uppercase
`PANIC`.

**Verus count note.** A bare `pub const` inside `verus!{}` is Verus-counted as one
trivial well-formedness item: base **406 → 407**, **0 errors**, no new proof
obligation on existing code (measured by stashing just the `sysabi.rs` edit and
re-verifying). The 406 base matches the phase-2 kcore baseline the plan cites.

## Surface left unsupported / trusted (and why)

- **The std PAL shell is unchanged and stays trusted** — `vendor/rust`'s
  `sys/pal/eunomia` + the `eunomia.rs` arms, the `kernel/`-over-`kcore` posture
  (a submodule fork that by construction never runs the verus gate). This task
  added **no PAL logic**; it only built the first consumer. The §11 thinness +
  inverse-leak review is unchanged (the consolidating audit is 6.2).
- **`fill_bytes`/`std::random` remain `unsupported` (panic)** — entropy is 3.4.
  The fixture avoids them; the prohibition is documented in its module doc.
- **stdout/stderr remain on the `debug-log` path** — the disclosed temporary
  rev2§2.7 deviation (2.3); 5.1 moves them to the console channel.
- **No trusted-base ledger change.** The gate is a test + a fixture binary + a
  script + a CI step; the const-hoist is an internal ABI value (one new
  Verus-counted item, no new seam); the verusfmt edit is tooling. Nothing here
  adds or removes one of the 14 seams.

## Follow-ups

- **5.3** — rewrite the real `hello`, then `shell`, onto std. This gate proves the
  end-to-end std path (entry/argv/env/alloc/stdio/time/exit + STATUS_PANIC reap),
  so 5.3 is now mechanical. When the std binary grows past comfortable headroom
  for the shell's 1 MiB heap, bump `urt::Heap<{1024*1024}>` in
  `user/shell/src/runtime.rs` (the 2.2-anticipated larger `N`); not needed at the
  50 KiB fixture size.
- **5.1** — console stdio; re-point the fixture's output off debug-log if desired.
- **3.4** — entropy/`HashMap`; lift the fixture's `fill_bytes` prohibition and add
  a `HashMap` arm to the gauntlet.
- As later std surface lands (threads 3.x, fs 4.x), the `stdsmoke` gauntlet is the
  natural place to add live assertions.
