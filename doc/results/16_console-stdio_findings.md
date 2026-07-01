# Findings 16 — Console stdio + `NAME_STDERR` (std-port 5.1)

Moves the std PAL's `stdout`/`stdin`/`stderr` off the bring-up kernel debug-log
(the 2.3 deviation) onto the userspace `user/console` channel (rev2§5.1), adds the
`NAME_STDERR` name id so stderr is a stream distinct from stdout, and keeps panic
last-words on the debug-log (rev2§7 C-M9). A booting CI gate exercises interactive
console stdio end to end (`STD51 PASS`: a std binary reads a stdin line over the console,
echoes it to stdout, and writes an independent stderr line).

## What shipped

1. **`loader/src/startup.rs`** — `pub const NAME_STDERR: u8 = 12;` (the free 12–15 gap).
   A `CapSlot`-kind name *value*, decoded through the existing `KIND_CAP_SLOT` arm → **no**
   decoder/encoder/`well_formed_startup`/Verus change (the name byte is opaque to the
   decoder; dispatch is on the kind tag). The `stdin`/`stdout` doc comments updated (now
   live, not "reserved").
2. **`eunomia-sys/src/grant.rs`** — re-export `NAME_STDERR`; add `stderr_slot()`.
3. **`eunomia-sys/src/console.rs` (new)** — the console client. Pure helpers (`resolve`
   with the stderr fallback, the 256-byte `chunks` splitter, the `Carry` read-remainder
   buffer) are host-visible and host-tested; the syscall/atomic-touching entry points
   (`attach`, `stdout_write`, `stderr_write`, `stdin_read`) are target-gated like
   `crate::stdio`/`crate::fs`. Writes chunk at `MSG_PAYLOAD` (256) and yield-poll on
   `ERR_FULL`; reads block by yield-polling `chan_recv` and carry any remainder.
4. **`eunomia-sys/src/{lib.rs,bootstrap.rs,pal.rs}`** — `mod console;`; `console::attach(s)`
   in `attach_grants()`; bridge symbols `__eunomia_{stdout_write,stderr_write,stdin_read}`.
   The existing `__eunomia_stdio_write` now serves only the panic/debug-log path.
5. **`vendor/rust`'s `sys/stdio/eunomia.rs`** — `Stdout`/`Stderr` write to the console
   symbols; `Stdin` is a real blocking `read` over `__eunomia_stdin_read` (the EOF stub and
   its overrides removed — `io::Read` defaults now apply); `STDIN_BUF_SIZE` raised
   `0 → DEFAULT_BUF_SIZE`; `PanicWriter` stays on the debug-log.
6. **`user/init/src/main.rs`** — `build_shell_block` grants `NAME_STDERR` on the same
   console endpoint as stdin/stdout (the terminal case); test extended.
7. **`user/shell/src/{runtime.rs,main.rs,tests.rs}`** — the shell donates its console
   endpoint to a *console-capable* child (`CONSOLE_CAPABLE = [b"bin/stdio"]`) via a
   best-effort `cap_copy` + `cap_install`, mirroring the fs-session donation;
   `build_child_block` gains a `console_slot` param that emits the `stdin`/`stdout` grants.
8. **`user/stdio/` (new binary)** — a dedicated std console demonstrator (mirroring
   `bin/stdfs`): reads a stdin line, echoes it, writes a stderr diagnostic, prints
   `STD51 PASS`.
9. **`kernel/build.rs`** — builds `user/stdio`; **plus** a build-std cache-invalidation fix
   (see "The build-std staleness trap" below).
10. **`scripts/std-smoke-test.sh`** — copies `bin/stdio`, drives it **as the last arm**
    (see the census-bug confinement below), asserting the echo + stderr line + `STD51 PASS`.

## Decisions & rejected alternatives

- **Raw yield-poll, not `ipc::Reactor`/`send_blocking`** (a refinement of the plan's 5.1
  row, cf. finding #9's "corrects the plan draft"). A reactor needs a wake-notification cap
  the std process is not granted, and using one would require a grant-delivery change the
  plan rules out. Raw yield-poll is the shipped precedent (the shell's `out()`/`Stdin::getc`,
  `eunomia-sys/src/fs.rs`) and the sanctioned MVP pattern; the console channel is a single
  kernel-serialized syscall per op, so there is **no** new userspace concurrency obligation.
  Power-efficient reactor/timer blocking is a deferred upgrade.
- **Children get `NAME_STDIN` + `NAME_STDOUT` only; stderr via the fallback.** The stderr
  resolution (`NAME_STDERR` → else the stdout channel) routes a terminal child's stderr to
  the stdout channel automatically. This also *avoids* a `MAX_GRANTS` bump: 2 console grants
  keep a would-be thread+console child at ≤ 8. The explicit `NAME_STDERR` grant is exercised
  by init→shell and the `console::resolve` unit tests.
- **Contained donation to a dedicated last-run demonstrator, NOT every child** (forced by
  the kernel `cap_copy` census bug below; the user chose this over a kernel fix for 5.1).
  Only `bin/stdio` (a `CONSOLE_CAPABLE` allowlist of one) inherits the shell's console, and
  it runs last so its post-reap console-wedge is harmless. A dedicated binary (not a
  `stdsmoke` arm) keeps the console concern — and the shell-console donation — off the
  thread/lock fixture, exactly as `bin/stdfs` is kept separate for fs.
- **Interactive echo demo; the concurrent `piped a | b` separation test deferred to 5.3.**
  The shell has no pipeline support and isn't itself on std until 5.3. 5.1 ships the
  `NAME_STDERR` separation *mechanism*; the QEMU gate shows stdin, stdout, and stderr all
  route over the console.

## The `cap_copy` endpoint-census bug (detailed — the reason donation is contained)

**Symptom.** With *every* std child donated the shell's console (the original "route all"
design), the shell's console **input** died immediately after the *first* console child was
reaped: the shell's own stdout still printed (the prompt appeared), but no subsequent
command was ever read or echoed, so the run wedged at the second command.

**Diagnosis (from on-target instrumentation).** After the reap, the console driver's
`chan_send` to the shell returned **`ERR_CLOSED`**, i.e. the kernel believed the shell's
console **end had zero live caps** (`end_caps[B] == 0`) — yet the shell's own `chan_recv`
on that same slot returned `ERR_EMPTY` (a *live* cap). So one donate→reap cycle
net-decremented `end_caps` for the shell's end below the true live-cap count, spuriously
firing peer-closed against the shell's own console.

**Root cause.** The rev2§3.3 per-endpoint cap census (`Channel::end_caps[2]`, used to fire
`EV_PEER_CLOSED` when the *last* cap of an end is deleted) is maintained by
`kcore::channel::endpoint_cap_added`/`_dropped`. But `endpoint_cap_added` is called **only
from the retype path** (`kcore/src/untyped.rs`, when a channel is first created) — **never
from `cap_copy`** (`kernel/src/syscall.rs::Sys::CapCopy` → `cspace::derive`). `derive`
happily produces a second `Channel(ch, end)` cap and bumps the *object* refcount
(`obj_refs`), but it does **not** bump `end_caps[end]`. So:

- `cap_copy(shell_console, scratch)` → a 2nd live end-B cap, but `end_caps[B]` stays `1`.
- reap deletes the child's copy → `endpoint_cap_dropped` → `end_caps[B]` `1 → 0` → fires
  peer-closed, even though the shell's cap is still live.

The console driver's next send to the (falsely-closed) end returns `ERR_CLOSED`, and the
shell's console is dead. The pre-existing storage-session donation `cap_copy`s a channel
cap too and hits the *same* census undercount — it only never surfaces because storaged
never *sends* to the delegated session after the child reaps (it only replies to requests),
so it never observes the false close, and the shell's own storage session is a different
channel.

**Why no userspace workaround exists.** `end_caps` can only be *incremented* by
`endpoint_cap_added` (retype) — `cap_copy` won't, and there is no syscall to bump it
directly. So a userspace holder cannot restore the count the reap wrongly cleared. "Route
all std-child stdio via the console" is therefore infeasible without a kernel change.

**Containment shipped.** Donate the console only to `bin/stdio`, run it last. The false
peer-close then lands at the very end of the run, after every assertion, so it is
harmless; all other children stay on the debug-log (their markers unaffected).

**The proper fix (kernel-track follow-up).** Make `cspace::derive` maintain the census:
when the derived cap is a `CapKind::Channel(ch, end)`, call
`channel::endpoint_cap_added(store, ch, end)` after installing the slot (the inverse of the
`delete`-path `endpoint_cap_dropped` at `cspace.rs:13006`). This re-establishes the
`end_caps_sound` invariant that `cap_copy` currently violates at runtime — a genuine latent
correctness bug, not just a 5.1 blocker. It is verified-kernel surface, so it needs the
`end_caps_sound`/census obligations threaded through `derive`'s `ensures` (non-trivial
proof work) and a `cargo verus verify -p kcore` re-verification; hence deferred out of this
userspace phase. Once landed, the `CONSOLE_CAPABLE` allowlist can widen to "every child"
(true foreground-terminal inheritance) with no other change. A host test worth adding with
it: copy a channel end cap, delete the copy, assert the peer is **not** closed while the
original lives.

## The build-std staleness trap (why the fix took so long — and the build.rs fix)

Editing the vendored std PAL (`vendor/rust/library/std/src/sys/stdio/eunomia.rs`) did **not**
rebuild std: `-Zbuild-std` fingerprints the *toolchain*, not the
`__CARGO_TESTS_ONLY_SRC_ROOT`-redirected source, so it silently cached and re-linked a
**stale** std. The `rerun-if-changed` on `std/src` correctly reran `build.rs` (and the user
binaries recompiled), but they linked the old std. The failure mode was a debugging trap:
`stdout` still "worked" (the *old* arm's `Stdout::write` → `__eunomia_stdio_write` →
debug-log, which reaches the same UART), while `stdin` read EOF (the old EOF stub) and
`__eunomia_stdin_read` was never called — making it look like a grant/resolution bug when
the new arm simply wasn't compiled. Confirmed empirically: touching the vendor source and
rebuilding left the `libstd-*.rlib` mtime unchanged; only `rm -rf target/user` forced a
rebuild.

**Fix (`kernel/build.rs`):** `build_std_is_stale` compares the newest mtime under
`vendor/rust/library/std/src` against the built `libstd-*.rlib`; when the source is newer
(a real edit / submodule bump), it `rm -rf target/user` so the `build_user` calls recompile
std from current source. Steady-state builds pay only a cheap tree walk. Edits to the
vendored `core`/`alloc` (outside `std/src`) are not tracked — a `CLAUDE.md` note calls out
the manual `rm -rf target/user` for that rare case. Verified: a vendor-std edit now rebuilds
`libstd`; an unrelated build does not.

## Verification record

- **Host tests.** `cargo test -p eunomia-sys` — the 7 `console::tests` green
  (`cap_matches_kernel` pins `CONSOLE_MSG_MAX == kcore::channel::MSG_PAYLOAD`; the `resolve`
  fallback cases; the `chunks` splitter; the `Carry` prefix/remainder). Shell host tests — 30
  green incl. `build_child_block_emits_console_grants` and `…_within_max_grants`. Init — 5
  green incl. the extended `shell_block_carries_named_grants` (NAME_STDERR).
- **Verus (cold).** `cargo clean -p loader && cargo verus verify -p loader
  --no-default-features` ⇒ **30 verified, 0 errors** (unchanged — `NAME_STDERR` is outside
  `verus!{}`; confirmed count-neutral by stashing the change and re-verifying the base). The
  plan doc's "29" was stale; the ledger records 30 (the 3.4 `KIND_SEED` arm). `cargo clean -p
  eunomia-sys && cargo verus verify -p eunomia-sys` ⇒ **16 verified, 0 errors** (unchanged;
  `console.rs` holds no `verus!{}`). No `verus!{}` code touched ⇒ no `verusfmt`.
- **Cross-build + format.** `cd kernel && cargo build` green (builds `bin/stdio`); `cargo fmt
  --check` clean over the root + the `user/{stdsmoke,shell,init,stdio}` mini-workspaces.
- **QEMU gate.** `scripts/std-smoke-test.sh` → `STD SMOKE TEST PASS`, including the new
  `STD51 PASS` (a stdin line echoed over the console + an independent stderr line), with every
  prior marker (`STD2/STD32/STD33/STD34/STD35`, argv, both time arms, the std panic reap)
  still green.

## Trusted / unverified surface (and why)

The console client is a **trusted marshalling shell** — the `sys/stdio` posture, marshalling
over the verified `ipc` channel syscalls. Its only non-delegation logic is the write chunking
(to `MSG_PAYLOAD`) and the read-remainder carry, both pure and host-tested. The channel is
kernel-serialized, so — unlike the futex path — there is **no** userspace concurrency
obligation to Loom/Shuttle. **No new trusted-base seam; the ledger tally stays 14.** Per the
per-task thinness gate, the PAL/seam stdio arm adds zero new logic beyond passing a valid
slice to a channel syscall and re-establishes nothing beyond that (the chunker re-establishes
the `MSG_PAYLOAD` length precondition; `chan_recv`'s 256-byte-buffer precondition is met by
the fixed `scratch`/`Carry` buffers).

## Follow-ups

- **Kernel: `cap_copy`/`derive` must maintain the endpoint census** (detailed above). The
  real fix; unblocks true "every child inherits the console" and fixes a latent
  channel-cap-donation correctness bug. Kernel-track + Verus.
- **Concurrent `a | b` pipeline + its stderr/stdin separation test → 5.3** (shell on std +
  pipeline support). 5.1 ships the `NAME_STDERR` separation mechanism it relies on.
- **Power-efficient console I/O** — reactor + timer-bit blocking (parallel to the deferred
  futex timer-bit). Needs a wake-notification grant to the std process.
- **Shell's own stdio onto std → 5.3** (the shell keeps its raw runtime console path today).
- **stdin line-ending policy** — `bin/stdio` reads until `\n`; a real interactive shell (5.3)
  owns any `\r`→`\n` translation.
