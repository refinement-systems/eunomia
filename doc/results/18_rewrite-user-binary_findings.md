# Findings #18 — Phase 5.3: rewrite the real `hello` and `shell` on std

Targets `doc/plans/2_plan-std-revised.md` phase **5.3**. Converts the first
**non-fixture** user programs from hand-rolled `no_std` onto the std runtime:
`user/hello` (trivial validation) and `user/shell` (the interactive REPL). Spawn/reap
stays on raw `loader::spawn`/`urt::spawn` (std::process cannot model capability spawn).
`EUNOMIA_HEAP_BYTES` is threaded so the shell can size its heap above 1 MiB.

**Gate met:** `scripts/std-smoke-test.sh` → `STD SMOKE TEST PASS`, now driven end to end
through the *std* shell (its REPL over the console, `std::fs` file built-ins, a
versioned-store admin op over the shared session, `SystemTime` `date`, and spawning the
real std `hello`), plus the new `STD53 PASS` marker.

---

## What shipped

- **`user/hello` → std** (`user/hello/{Cargo.toml,src/main.rs,link.ld}`). Modeled on
  `user/stdsmoke`: `extern crate eunomia_sys;` + `fn main()`, no `_start`/`#[panic_handler]`/
  boot-ack. Validates entry/argv (`env::args`), alloc (`Vec`/`String`/`format!`), inherited
  env (`env::var("TERM")`), monotonic `Instant`, a clean `exit(0)` (`STD53 PASS`), and a
  `panic` arm that reaps as `STATUS_PANIC`. The vestigial no_std boot-channel `hello-ok`
  ack was dropped — nothing read it (the shell reaps children via the reactor, not an ack).
- **`user/shell` → std** (`user/shell/{Cargo.toml,src/main.rs,src/runtime.rs,src/tests.rs}`).
  A std binary: std owns `_start`/alloc/panic; `eunomia_sys` at bootstrap supplies
  argv/env, the time page, the DRBG seed, the storaged session, and stdio over the console.
  `_start` → `pub fn run()` (called by a `#[cfg(not(test))] fn main`), dropping the by-hand
  block decode, grant resolution, and storaged connect handshake; it keeps only the
  spawn-object carve + the REPL. File built-ins (`ls`/`cat`/`write`/`rm`/`mv`, child-ELF
  load) ride `std::fs`; the versioned-store admin ops (`snap`/`snaps`/`rollback`/`snapdel`/
  `keep`/`prune`/`gc`/`df`/`sync`) ride the newly-public `eunomia_sys::fs::request`; `date`
  uses `SystemTime`; stdio uses std `stdin`/`stdout`. Spawn/reap (`Spawner`, per-child
  provisioning, `build_child_block`) is unchanged on raw `ipc`/`urt`/`loader`.
- **`eunomia-sys/src/fs.rs`**: the private session round-trip `request` is now `pub` — the
  admin escape hatch for a client that delegates its whole session to the crate.
- **`kernel/build.rs`**: the shell's `build_user` now passes `EUNOMIA_HEAP_BYTES=4194304`
  (4 MiB) so its `System` heap (the `option_env!` in `eunomia-sys/src/heap.rs`) clears the
  1 MiB default — it loads whole child ELFs into that heap on `run`.
- **`scripts/std-smoke-test.sh`** / **`scripts/run-demo.sh`**: hello copied into the image;
  new 5.3 arms (`STD53`, shell `std::fs` + admin + `date`); run-demo comments updated.

---

## Decisions (and rejected alternatives)

### 1. `std::fs` for the shell's file ops, over ONE shared storaged session (user-chosen)

The user chose (`AskUserQuestion`) to route the shell's plain file built-ins through
`std::fs` (dogfooding the 4.x fs client, sharing the verified `eunomia_sys::path`
resolver) rather than keeping them on raw IPC. The shell **must** keep raw IPC for the
versioned-store admin ops (`snap`/`gc`/`df`/… — `std::fs` cannot express them).

**Realized as a single shared session, not two.** `eunomia_sys::fs` connects the shell's
slot-1 session at bootstrap (via the `NAME_STORAGE` grant init already emits); both the
`std::fs` file ops and the shell's raw admin `Request`s ride it. This needed **no change
to `user/storaged` and no change to `user/init`**, verified against the storaged session
model:
- `ipc::connect` (what `eunomia_sys::fs` uses) is byte-identical to the shell's old
  hand-rolled `ConnectReq`/`GrantReply` — storaged cannot tell who drove the handshake.
- storaged serves slot 1 (its key-0 session) uniformly (`admit_connect` → `wire` requests),
  is offset-stateless (Read/Write carry explicit offsets), and a strictly-sequential
  client (one round-trip at a time) cannot desync it by alternating `std::fs` and admin
  requests.
- The one invariant — **connect exactly once, never re-issue a `ConnectReq` on slot 1**
  (storaged never re-inspects `TAG_REQ` on key 0) — holds by construction: the shell
  drops its own connect and `eunomia_sys::fs` never re-attaches.

*Rejected — two sessions* (a separate fs-session for `std::fs` + the raw session for
admin): would need init to grant a second session, storaged to multiplex a third session,
and two parallel storage code paths in the shell. Strictly more surface for no gain over
the shared session.

### 2. Host tests preserved via `#[cfg(not(test))]` gating

The shell's pure logic (date math, formatters, parsers, `prune_victims`,
`build_child_block`) keeps its host proptest suite. The old `#![cfg_attr(not(test),
no_std)]` dual-mode is replaced by a plain **std crate** whose target-only surface is
`cfg`-gated: `#[cfg(not(test))] extern crate eunomia_sys;`, `#[cfg(not(test))] mod
runtime;`, `#[cfg(not(test))] fn main() { runtime::run() }`. Under host `cargo test` the
runtime + PAL bridge are excluded, the harness supplies `main`, and the pure logic + tests
build against host std. Result: **28/28 host tests pass**, unchanged coverage minus the
three now-deleted grant-resolver tests (the resolvers moved into `eunomia_sys`/std).

### 3. Minimal-diff refactor of `runtime.rs`

The intricate spawn machinery (`Spawner`/`spawn_inner`/`SpawnRec`/reactor dispatch/per-child
provisioning) is preserved verbatim except one line (`STDOUT_SLOT` → `CONSOLE_SLOT`). The
I/O helper *functions* (`out`/`out_num`/`out_hex`/`out_utc`/`diag`/`request`/`root_handle`)
kept their names but swapped bodies onto std/eunomia-sys — so every admin command handler
(`report`/`cmd_snaps`/`cmd_gc`/`cmd_df`/`cmd_prune` and the `snap`/`rollback`/… dispatch
arms) stays byte-for-byte. `out` → std `stdout` + explicit `flush` (line-buffered; the
prompt and per-keystroke echo carry no newline). `request` → `eunomia_sys::fs::request(…)
.unwrap_or(Internal)`. `root_handle` → const `0` (init's convention).

### 4. `CONSOLE_SLOT` hardcoded to init's convention (slot 6)

The shell donates its console cap to children by *slot number* (`spawn_inner`). As a std
binary its own stdio is resolved by `eunomia_sys::console` (whose module is private), so
the donation source is a hardcoded `CONSOLE_SLOT = 6` — consistent with the other
already-hardcoded init-installed slots (`POOL`=2, `SH_TIME`=5, `SHELL_FS_SESSION_SLOT`=7),
a shell↔init co-designed cspace. *Follow-up:* a small `eunomia_sys::console::out_slot()`
accessor would decouple this from the layout.

### 5. `parse_path` retained as the host-tested reference

`std::fs` now owns path splitting on the target (via the verified `eunomia_sys::path`
resolver), so the shell's `parse_path` is orphaned there. It is kept as the host-tested
reference for the rev2§4.9 path model (`#[cfg_attr(not(test), allow(dead_code))]`); a
follow-up shares the verified resolver with it.

---

## Problems hit

- **Path bytes → `Path`.** There is no `os/eunomia` byte-`OsStr` extension (unlike `unix`),
  so the shell converts command path bytes with `core::str::from_utf8` (paths are UTF-8 in
  practice; a non-UTF-8 path is refused cleanly — a disclosed MVP limit). Directory entry
  names print via `OsStr::as_encoded_bytes()` (lossless on eunomia, where `OsStr` is bytes).
- **`std::fs::write` truncates; `File::truncate` is `Unsupported`.** `std::fs::write` does
  `File::create` (open with `truncate`), and the vendored eunomia `File::open` **emulates**
  open-time truncate by unlinking the existing file (rev2§4.9 has no `set_len`), so
  `write` works and now *replaces* content (a re-`write` no longer overlays the old tail —
  a small, more-correct semantics change from the raw offset-0 write).
- **Smoke greps vs the shell's keystroke echo.** The REPL echoes typed commands, so a
  naive `wait_for '<content>'` matches the echo, not the command output. Fixed with
  line-anchored greps (`^hello-53`, `^s53.txt`) that only match the output lines (echo
  lines start with the `eunomia> ` prompt), and distinctive output prefixes (`chunk
  region:`, `[hello] …`, `T[0-9][0-9]:…`) for the rest.
- **Wrong-toolchain manual build red herring.** Invoking the shell's cross-build by hand
  (without the kernel-pinned toolchain and `__CARGO_TESTS_ONLY_SRC_ROOT`) builds against
  pristine std and fails with missing-`__eunomia_*` errors — *not* a code problem. The real
  `cd kernel && cargo build` (which sets both) compiles it cleanly. Only reproduce user
  builds through `kernel/build.rs`.

---

## Verification record

- **Cross-build (the gate that also cross-builds every user binary):**
  `cd kernel && cargo build` → `Finished` clean (the only warning is the pre-existing
  vendored `core v0.0.0` future-incompat note); `ushell` (83 KB) + `hello` (54 KB) fresh.
  Forcing a shell/hello recompile surfaced **no** warnings (dead-code/unused clean).
- **Host tests:** `cargo test --manifest-path user/shell/Cargo.toml` → **28 passed, 0
  failed** (pure logic + `build_child_block` producer; eunomia-sys builds as a host dep).
- **Verus:** `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` →
  `verification results:: 16 verified, 0 errors` (a real run — `results::` line present).
  Making `request` `pub` is invisible to the gate: `fs.rs` is `#![cfg(target_os=eunomia/
  none)]`, outside the host verus graph. Count unchanged (16).
- **QEMU boot smoke — the gate:** `scripts/std-smoke-test.sh` → **`STD SMOKE TEST PASS`**,
  all markers green including the new **`STD53 PASS`** and the shell-on-std line
  (write/cat/ls over `std::fs`, `df` Statfs over the shared session, `SystemTime` `date`,
  REPL over the console). `run bin/hello` reaps `exited(0)`; `run bin/hello panic` reaps
  `panicked` (no `exited(254|101)`, no `faulted(`). No orphaned QEMU. Run under the
  `CLAUDE.md` process-group timeout wrapper.
- **Formatting:** `cargo fmt --check` (root) + `--manifest-path user/{shell,hello}` all
  clean. No `verus!{}` interior was touched, so `verusfmt` is not implicated.

---

## Surface left trusted / unsupported (and why)

- **The PAL is untouched by 5.3.** `vendor/rust`'s eunomia PAL gains nothing; the shell is
  a std *consumer*. The one gated-crate change — `eunomia_sys::fs::request` becoming
  `pub` — is a visibility change on an existing thin marshalling delegate (encode → send →
  recv → decode over the session `attach` connected), not new logic. Per the plan's
  per-task **thinness + inverse-leak** self-check: `request` re-establishes the session
  invariant it always did (a dead/absent session → clean `Internal` error, never a bogus
  round-trip), and the shared-session **handshake-once** invariant is re-established at the
  boundary by the shell dropping its own connect. No new trusted seam; the ledger tally is
  unchanged.
- **Shell paths must be UTF-8** on the target (no byte-`OsStr` construction API); non-UTF-8
  path arguments are refused, not crashed. `std::fs` `truncate`/`set_len`, symlink/perms,
  etc. remain `Unsupported` by construction (findings #15), unaffected here.

---

## Follow-ups (deferred, not blockers)

- Mutable environment / an `export` built-in (would flip `setenv`/`unsetenv` to supported).
- A concrete `a | b` pipeline test that `a`'s stderr does not corrupt `b`'s stdin
  (the rev2§5.1 separation; the plumbing exists since 5.1).
- Share the verified `eunomia_sys::path` resolver with the shell's host-tested `parse_path`
  (kept separate here so the host tests stay buildable).
- A small `eunomia_sys::console::out_slot()` accessor to replace the hardcoded
  `CONSOLE_SLOT` in the shell's child-donation path.
- The `write` semantics change (now truncating) is intentional and better, but worth a
  line in any user-facing shell docs if they exist.
