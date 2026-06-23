# Plan — Part B15 detail: baseline test backfill (bring the **mkfs directory-walk** and the **user-binary non-I/O logic** up to the rev1§6 "Miri + proptest — everything" baseline — the two surfaces the audit names as having *no automated tests*: `mkfs` has one happy-path integration test (+ B12's S-10 refusal test), and the five `user/*` binaries are validated only by QEMU boot output. B15 makes the *load-bearing pure logic* host-testable behind a thin seam — the `populate` directory walk and its name-acceptance rule, and the shell/storaged/init parsing & formatting helpers — proptests and golden vectors them, and keeps the QEMU boot smoke as the integration gate for the syscall-bound wiring that cannot be host-tested. Verification/test-only: no spec edit, no `rev1§6.1` seam, no Verus, no TLA, no on-disk/wire change; the kcore/CAS/ipc/dma-pool/freelist/urt gates and the three TLA models are held by not touching them.)

Detailed, separately-implementable decomposition of **Phase B15** from
`doc/plans/0_address_audit_rev0.md` (parent-plan B15 at `:638-650`). B15 is **Wave-6**
work. It is **self-contained**: it depends on nothing else in Part B/C and nothing
depends on it (parent plan `:644` "Depends on: none; can trail the others or fold into
subsystem phases"). It is **test-only** — it changes no on-disk bytes, no wire op, no
runtime behaviour, and no public type any other crate consumes; it closes the gap between
the rev1§6 baseline ("Miri + proptest — *everything*") and the two surfaces that today
have effectively none.

The framing that shapes the whole phase: like B14's IPC findings, the B15 gaps are
**coverage gaps, not code defects**. The audit found `mkfs` and the user binaries
*correct* (the QEMU smoke is green end-to-end), but **untested at the unit/property
tier**. So B15 adds tests; it does not fix bugs. Its real engineering content is
*making the load-bearing logic reachable from a host test* — both `mkfs::populate` and
every interesting `user/*` function are buried in places a host `cargo test` cannot reach
today (a `bin`-only crate; `#![no_std] #![no_main]` binaries that only link for
aarch64). B15's design decisions are almost entirely about that reachability seam, chosen
to add the *least* structure that makes the logic testable while leaving the syscall-bound
I/O on the QEMU boot gate where it belongs.

**Closes (from the parent plan).** Verbatim from `doc/results/0_audit_rev0.md` §4.2
(`:514-517`):

> **`mkfs` directory-walk and the user binaries** have no automated tests (one happy-path
> integration test for `mkfs`; the five user binaries are validated only by QEMU boot
> output). `loader::prepare`'s page-rounding (the I-5 site) has no host model.

Two scope notes on that sentence, both load-bearing for B15's boundary:

- **`loader::prepare`'s page-rounding is *not* B15.** The audit bundles it into the same
  bullet, but the parent plan routes it to **Phase B3** (`0_address_audit_rev0.md:644`,
  "loader::prepare host model is in B3"; and B3's detail `doc/plans/3_b3-detail.md`). B15
  does **not** touch `loader`. Recorded here so the `loader` half of the audit bullet is
  not mistaken for a B15 gap.
- **`mkfs`'s S-10 refusal is already done (B12).** The "one happy-path integration test"
  the audit saw has since gained a *second* test — `refuses_undersized_image_cleanly`
  (`mkfs/tests/image.rs:55-81`, the S-10 / rev1§4.5 format-contract test, landed in
  **B12**). B15 does **not** redo S-10; it adds the **directory-walk** coverage the audit
  asks for (the `populate` recursion, name-acceptance rule, ordering, and skip discipline),
  which neither existing `mkfs` test exercises as logic.

The parent-plan B15 **work** line (`0_address_audit_rev0.md:645-647`) and **acceptance**
(`:648-650`) set the targets: "add proptest/unit coverage for the mkfs directory walk;
add host-testable logic tests for the user binaries' non-I/O logic **where feasible**;
keep the QEMU boot smoke as the integration gate." The "where feasible" is doing real
work — it scopes B15 down from "test the user binaries" (most of which is syscall I/O that
*cannot* be host-tested without mocking the kernel, explicitly out of scope) to "test the
pure logic that *can* be lifted host-side," which is concentrated in the shell.

---

## Spec target — Part A is blessed; B15 makes no spec edits

Every citation below is `rev1§` against the already-blessed text. B15 changes no spec and
flips **no** `rev1§6.1` `[verifying]` line — `mkfs` and the `user/*` binaries are **not**
proof-boundary seams (rev1§6.1(a)–(e) are the kernel/storage seams; the verified surface
is `kcore`/`cas`/`ipc`/`dma-pool`/`freelist`/`urt`, per `verus_trusted-base.md`). `mkfs`
and the user binaries sit entirely in the rev1§6 **Baseline** tier. The load-bearing
claims B15 conforms to:

- **rev1§6 — the Baseline tier** (`spec_rev1.md:393-399`, the table row at `:399`):
  **"Baseline | Miri + proptest | everything, the chunker and prolly tree especially."**
  *Everything* includes `mkfs` and the user binaries; today they are the conspicuous
  unmet corner of "everything." B15 supplies the proptest/unit tier the row requires for
  the **pure, host-reachable** logic, and records (Design decision 3) that the
  **syscall-bound** logic stays on the QEMU integration gate — the honest split, since you
  cannot Miri+proptest code whose every step is a kernel syscall without standing up a
  kernel mock (out of scope, parent plan `:647` "keep the QEMU boot smoke as the
  integration gate").
- **rev1§6 Baseline prose** (`spec_rev1.md:405`): "Round-trip and canonical-form
  properties are the natural proptest targets: the same contents produce the same tree,
  regardless of edit order." This is the **mkfs directory-walk oracle** B15A adopts — the
  same host directory tree, built into an image, mounts to the same logical contents
  regardless of the order the source files were created in (the `sort_by_key` at
  `mkfs/src/main.rs:31` + the prolly tree's history-independent canonical form, rev1§4.1,
  whose own verification is B13). mkfs is the *producer* whose output that property
  governs; B15A pins that mkfs faithfully maps the host tree into it.
- **rev1§7 — mkfs and the userspace binaries** (`spec_rev1.md`, the rev1§7 "tooling /
  bring-up" scope; `mkfs/src/main.rs:1` cites rev1§7). mkfs "builds the initial disk
  image … byte-for-byte the same on-disk format the storage server mounts." B15A tests
  that the *content mapping* (host FS → store) is faithful; it does not re-test the CAS
  format itself (that is `cas`'s own verified + fuzzed surface).
- **rev1§2.7 — the syscall/decode boundary** (the S-7 home created in Phase A3): "unknown
  opcode → error (never crash), message-length and field validation against ground truth."
  The startup/config-block parsers in `storaged`/`init`/`shell`/`selftest` (the hand-rolled
  "SD02"/"SH01"/"ST01" blocks) are *decoders of an untrusted-shaped message*, and the
  rev1§2.7 discipline is **refuse-not-crash** on a short/garbage block. B15C pins exactly
  that for the config-block parse (a panic there is a boot failure) — the decode-discipline
  floor, the same posture the wire decoders get from fuzzing.
- **rev1§4.9 — printable-name convention** (`mkfs/src/main.rs:38-40` cites it): "Tooling
  enforces the printable-ASCII convention; the format itself only excludes NUL and '/'."
  B15A's name-acceptance proptest pins that mkfs's filter matches this rule
  (`0x20..0x7F`, reject '/', reject non-UTF-8) and that a rejected name is *skipped*, never
  fatal.
- **rev1§6.1 honesty discipline** (`spec_rev1.md:411`, the rule the ledger obeys): "a
  property routed to trust is not mistaken for a mechanized one." B15's analogue: a
  property routed to the **QEMU integration gate** (the syscall-bound wiring) must not be
  mistaken for a host-unit-tested one. Design decision 3 records the split explicitly, in
  the spirit of B6's GC-sufficiency note and B14's dispatch note in the ledger.

---

## What is actually true today — the gap is *no host tests for reachable logic*, not a defect

The inventory that shapes the phase. Two surfaces, very different in how much pure logic
they hold.

### mkfs — one untested walk, two integration tests that don't exercise it as logic

`mkfs/src/main.rs` is a **`bin`-only crate** (`mkfs/Cargo.toml`: just `[dependencies] cas`;
no lib target). Its logic:

- **`populate`** (`:24-58`) — the recursive directory walk. The load-bearing, untested
  piece. It: reads `read_dir` into a Vec and **sorts by `file_name`** (`:30-31`, the
  determinism/canonical-order hinge); for each entry, **accepts the name** iff it is UTF-8
  (`:34-37`) **and** printable-ASCII `0x20..0x7F` with no `'/'` (`:40-43`, rev1§4.9) — else
  `eprintln` + `continue` (skip, *before* the prefix push); pushes the name onto `prefix`
  (`:45`), recurses on a directory (`:46-47`), `store.write`s a regular file and bumps
  `count` (`:48-51`), or `eprintln`-skips a non-regular file (`:52-54`); then `prefix.pop`
  (`:55`). Returns the regular-file `count`.
- **`mtime_nanos`** (`:16-22`) — a pure `Metadata → u64` helper (epoch-relative, `0` on
  failure).
- **`run`** (`:60-112`) — argument parsing, the `StoreOptions` tuning for the batch tool
  (`:81-86`), `format`/`create_ref`/`populate`/`snapshot`, the success line. `main`
  (`:114-122`) maps `run`'s `Result` to `ExitCode` (the S-10 clean-exit path).

The two existing tests (`mkfs/tests/image.rs`) both **spawn the binary** (`CARGO_BIN_EXE_mkfs`)
and assert at the *image* level: `built_image_mounts_and_matches_source` (`:8-53`, one
two-file tree: a small file + a 1.5 MB file in a subdir, mount + read-back + snapshot
checks) and `refuses_undersized_image_cleanly` (`:55-81`, B12's S-10 test). Neither varies
the *tree shape* or hits the **skip branches** (non-UTF-8 name, non-printable name,
non-regular file), the **ordering** invariant, or **deep nesting**. The walk's correctness
is, today, asserted by exactly one hand-built two-entry example. That is the §4.2 gap.

`cas` already exposes everything an in-process test needs: `MemDev` (`cas/src/dev.rs:58`,
in-memory — no disk file), `FileDev` (`:105`), `Store::format`/`mount`/`write`/`read`
(used by the existing test). mkfs is **not** in the CLAUDE.md Miri sweep (which targets
`-p cas -p loader -p storage-server`, `-p dma-pool`, `-p urt`).

### user/* — pure logic concentrated in the shell; the rest is syscall I/O

All five binaries are `#![no_std] #![no_main]`, each its own mini-workspace
(`user/*/Cargo.toml` has `[workspace]`), built by `kernel/build.rs` for aarch64. They have
**zero** `#[cfg(test)]`/`#[test]` today. `ipc::sys` has a host-stub branch
(`ipc/src/sys.rs:109`, `#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]`),
so the crates *link* on the host — the obstruction to host tests is the `#![no_main]` +
`_start`/`#[global_allocator]`/`#[panic_handler]` items, which don't belong in a host test
harness (Design decision 2 resolves this). The pure-logic inventory, richest first:

- **`shell`** (`user/shell/src/main.rs`) — by far the most host-testable logic:
  - **`civil_from_days`** (`:118-129`) — Howard Hinnant's civil-from-days; pure
    `u64 → (y,m,d)`. The single most logic-dense, most-worth-a-golden-vector function in
    all of `user/*`.
  - the **date/time formatter** `out_utc` (`:137-155`) + `out_num`/`out_num_pad`
    (`:90-114`) + `out_hex` (`:394-403`) — pure *computation* today coupled to output via
    `out()` → `sys::debug_write` (`:86-88`); B15B's small refactor (Design decision 2)
    splits the formatting from the writing so the bytes are assertable.
  - **`parse_path`** (`:176-181`) — `&[u8] → Vec<Vec<u8>>`, splits on `'/'`, drops empties.
    Pure.
  - **`parse_u64`** (`:379-391`) — decimal parse with reject-on-nondigit and empty→None.
    Pure.
  - **`fault_class`** (`:409-422`) — ESR_EL1 → a fault-name string (rev1§5.3 classification).
    Pure.
  - the **prune retention policy** in `cmd_prune` (`:350-377`) — the *selection* arithmetic
    (`candidates = rows.filter(class != 0)`, `excess = len.saturating_sub(n)`,
    `&candidates[..excess]`, `:355-358`) is pure and worth a property; it is currently
    welded to the IPC `request` calls and must be extracted to a pure
    `fn prune_victims(rows, keep_n) -> Vec<u64>` to be testable.
- **`storaged`** (`user/storaged/src/main.rs`) — almost all syscall I/O; the one pure piece
  is the **SD02 config-block parse** (`:117-121`): the magic + length check (`len < 44 ||
  &buf[..4] != b"SD02"`, `:117`) and the five-`u64` little-endian field extraction (the
  `rd` closure, `:120-121`). A decode of an untrusted-shaped message (rev1§2.7).
- **`init`** (`user/init/src/main.rs`) — almost all syscall wiring; the host-testable pieces
  are the **startup-block construction** (SD02 at `:168-174`, SH01 at `:235-237` — the
  inverse of storaged's/shell's parse) and the **RTC sanity rule** (`secs < RTC_MIN_SANE_SECS
  || cntfrq == 0`, `:87`). The block construct/parse pair is a nice round-trip property
  shared with storaged (B15C).
- **`selftest`** (`user/selftest/src/main.rs`) — the **ST01 startup-block parse** (mode +
  time-VA, `:78-88`). Otherwise it *is* a test subject (its modes exercise the shell's
  reclaim loop under QEMU); its `_start` is inherently boot-only.
- **`hello`** (`user/hello/src/main.rs`) — a pure I/O smoke subject (read block, reply,
  exit); **no** host-testable pure logic. Stays on the QEMU gate.

So the honest split B15 delivers:

- **mkfs (B15A):** the directory walk + name-acceptance rule, lifted behind a `lib` seam
  and proptested with a mount-equality / determinism / skip-discipline oracle.
- **shell (B15B):** the date math + parsers + fault classifier + prune policy, host-tested
  with golden vectors and properties (the prize — most of `user/*`'s testable logic).
- **storaged/init/selftest startup parsing (B15C):** the SD02/SH01/ST01 block
  encode/decode, host-tested for round-trip + **refuse-not-crash** on short/garbage blocks
  (the rev1§2.7 decode-discipline floor) — where feasible.
- **everything else stays the QEMU boot gate (Design decision 3):** the syscall wiring, the
  MMIO probe loop, `hello`, selftest's fault/panic/bss-leak modes.

---

## Primary files (current line numbers)

- `mkfs/src/main.rs` — B15A's subject: `populate` (`:24-58`; name filter `:34-43`, sort
  `:30-31`, dir/file/other branches `:46-54`, `count`), `mtime_nanos` (`:16-22`), `run`
  (`:60-112`; `StoreOptions` tuning `:81-86`), `main` (`:114-122`).
- `mkfs/Cargo.toml` — B15A adds a `[lib]` target (or a `src/lib.rs` + a thin `[[bin]]`) and a
  `[dev-dependencies] proptest`; today `cas`-only, no lib.
- `mkfs/tests/image.rs` — the two existing integration tests (`built_image_mounts_and_matches_source`
  `:8-53`, `refuses_undersized_image_cleanly` `:55-81`); B15A adds a walk proptest here (or a
  sibling `tests/walk.rs` / a `#[cfg(test)]` module in the new lib).
- `cas/src/dev.rs` — `MemDev` (`:58-79`, the in-memory device B15A's walk proptest uses so no
  per-case disk file), `FileDev` (`:105`), `BlockDev` (`:38`).
- `user/shell/src/main.rs` — B15B's subject: `civil_from_days` (`:118-129`), `out_utc`
  (`:137-155`), `out_num`/`out_num_pad` (`:90-114`), `out_hex` (`:394-403`), `parse_path`
  (`:176-181`), `parse_u64` (`:379-391`), `fault_class` (`:409-422`), `cmd_prune` selection
  (`:350-377`), `out` (`:86-88`, the `sys::debug_write` sink the formatters are split from).
- `user/storaged/src/main.rs` — B15C: the SD02 parse (`:117-121`).
- `user/init/src/main.rs` — B15C: SD02 construct (`:168-174`), SH01 construct (`:235-237`),
  RTC sanity (`:87`).
- `user/selftest/src/main.rs` — B15C (optional): ST01 parse (`:78-88`).
- `user/{shell,storaged,init,selftest}/Cargo.toml` — each gets the `#[cfg(not(test))]`
  gating (Design decision 2); no manifest change needed unless a `[dev-dependencies]`
  (proptest) is added for the shell.
- `ipc/src/sys.rs` — the host-stub branch (`:109`) that lets these crates link host-side;
  **unchanged** (B15 relies on it, does not touch it).
- `doc/guidelines/verus_trusted-base.md` — the ledger. B15 adds **no seam** (tally stays
  **14**) and changes **no Verus/TLA gate**. It **may** add `mkfs` + the user-binary host
  tests to the **Baselines** table as test-routed gates (Design decision 3) — recorded the
  way the GC-sufficiency note (`:66-74`) and the IPC-dispatch note (`:76-90`) record
  test-routed properties, so a future change re-runs them. No `[verifying]` line to flip.
- `doc/spec/spec_rev1.md` — **no change** (Part A blessed; B15 is a Baseline-tier
  conformance, no proof boundary).
- `CLAUDE.md` — **no change** required, but B15 should note (and may add) that the user-binary
  host tests run via their own manifests (`cargo test --manifest-path user/shell/Cargo.toml`)
  and that `cargo test -p mkfs` now covers the walk — the same workspace-split caveat
  CLAUDE.md already documents for `cargo fmt`.

---

## Verification tier & baseline (applies to all sub-phases)

B15 is **entirely** rev1§6 **Baseline** tier (Miri + proptest / unit). Five notes up front
so nothing is silently dropped or over-claimed:

- **B15 is test-only — no runtime, wire, on-disk, or public-type change.** It adds tests
  and the *minimum* structure to reach the tested code (a `lib` seam in mkfs; `#[cfg(not(test))]`
  gating + a format/policy extraction in the user binaries). The on-disk format, the wire
  protocol, the startup-block layouts, and every `_start` are byte-for-byte unchanged, so the
  aarch64 cross-build and the QEMU boot are unaffected by construction.
- **No Verus, no TLA, no new seam.** mkfs and the user binaries are outside the verified
  surface (`verus_trusted-base.md` "Scope"). B15 adds no `external_body`/`assume_specification`
  (tally stays **14**), no `verus!{}` construct, no `.tla` model. The kcore (389/0), cas
  (80/0), ipc (62/0), freelist (29/0), dma-pool (0/0), urt (29/0) gates and the three TLA
  models (`CommitProtocol`, `CapRevocation`, `IpcReactor`) are held **by not touching them** —
  B15 builds and runs them as the regression check, it does not modify them.
- **The Miri-routing decision is explicit, per surface.** The mkfs walk and the user-binary
  logic are **plain Rust with no `unsafe`** (the one `unsafe` in `user/*` is the time-page
  attach / the selftest bss probe — both syscall/boot-only, not host-testable), so Miri's
  *UB-finding* value is low; the value is **proptest coverage of the logic**. Therefore: the
  pure host tests run as ordinary `cargo test` and use the workspace Miri case-count idiom
  (`cases: if cfg!(miri) { 4 } else { N }`, e.g. `cas/src/overlay.rs:201-205`,
  `urt/src/time.rs:561-565`) so they *can* be Miri-replayed, but B15 does **not** add mkfs or
  the user mini-workspaces to the standing CLAUDE.md Miri sweep (which is scoped to the
  `unsafe`-heavy crates). The mkfs walk's CAS write/mount path is already Miri-swept under
  `-p cas`. This routing is recorded (Design decision 3), not silently chosen.
- **No Loom/Shuttle.** mkfs and the user binaries' tested logic is single-threaded and
  atomics-free; concurrency is the IPC reactor's surface (Phase B14) and the kernel's. B15
  adds no concurrency harness (the B1 precedent — record the decision rather than add a
  no-value harness).
- **The `cargo fmt` workspace-split trap applies.** Any `user/*` file B15 touches must be
  formatted via its own manifest (`cargo fmt --manifest-path user/shell/Cargo.toml`, etc.);
  the root `cargo fmt` silently skips the user mini-workspaces (CLAUDE.md "Formatting"). mkfs
  is a root-workspace member, so the root `cargo fmt` covers it.

**Baseline to re-establish at end of B15:**

- `cargo test -p mkfs` green: the two existing image tests **plus** B15A's walk proptest(s)
  and name-acceptance unit/proptest.
- `cargo test --manifest-path user/shell/Cargo.toml` green: B15B's new host tests (the bin
  crate now builds a host test harness under `cfg(test)`, Design decision 2). Likewise
  `--manifest-path user/storaged/Cargo.toml` and `user/init/Cargo.toml` (and
  `user/selftest/Cargo.toml` if B15C covers it) for B15C.
- The aarch64 cross-build still links every `user/*` binary (the `#[cfg(not(test))]` gating
  leaves the non-test build identical) and **`scripts/run-demo.sh` boots green** — the
  unchanged integration gate (`[storaged] store mounted` → `serving`, shell commands echo,
  no panic/`Corrupt`), run under the CLAUDE.md timeout-harness pattern.
- The verified-surface gates (kcore/cas/ipc/freelist/dma-pool/urt Verus counts, the three TLA
  models, the fuzz corpora + Miri replay) unchanged — B15 touches none of them.

---

## Design decision 1 — making the mkfs walk host-testable: **bin → bin+lib**, an extracted **`name_acceptable`** predicate, and an **in-memory-tree** proptest with the mount-equality oracle *(resolve in B15A)*

`populate` lives in a `bin`-only crate, so no host test can call it; and it interleaves the
*pure walk* (name rule, ordering, recursion structure) with *real-FS reads* and *`Store`
writes*. Three things must be decided: how to reach it, what to assert, and how to vary the
input cheaply.

- **Adopted — convert `mkfs` to a `bin`+`lib` package; extract the name rule; proptest the
  walk in-process over generated temp-dir trees with a mount-equality oracle, and proptest
  the name rule + ordering over an in-memory tree model (Miri-able).**
  1. **`bin`+`lib` split.** Add `mkfs/src/lib.rs` exposing `pub fn populate(...)`,
     `pub fn mtime_nanos(...)`, and the new `pub fn name_acceptable(name: &OsStr) ->
     Option<&str>` (factored out of the inline `:34-43` filter); `mkfs/src/main.rs` becomes
     a thin `fn main() -> ExitCode` over `mkfs::run`. This is the B1 precedent ("factor the
     attenuation arithmetic into a pure `pub fn` so the … core is unit/proptest-addressable",
     `1_b1-detail.md:168-172`) and the standard, low-risk way to give a host tool a test
     surface. No behaviour change — the bin is the same bytes.
  2. **`name_acceptable` unit + proptest (Miri-able).** A pure `&OsStr → Option<&str>`,
     testable with no FS: golden cases at the boundary (`0x1F` reject, `0x20` space accept,
     `0x7E '~'` accept, `0x7F` DEL reject, control chars reject, embedded `'/'` reject,
     a non-UTF-8 `OsStr` reject on Unix via `OsStrExt::from_bytes`) and a proptest that the
     predicate accepts a byte string **iff** every byte is in `0x20..0x7F` and none is `'/'`
     (the rev1§4.9 rule, `mkfs/src/main.rs:38-43`). Runs under Miri (no I/O).
  3. **Walk proptest with the mount-equality oracle (in-process; native, optionally
     Miri).** Generate an arbitrary small directory tree as an **in-memory model** (a
     recursive `enum Node { Dir(BTreeMap<name, Node>), File(Vec<u8>) }`, names drawn to
     include accepted *and* rejected forms, depth- and breadth-bounded for proptest speed),
     **materialize it** into a fresh temp dir, run `mkfs::populate` against a
     **`MemDev`-backed `Store`** (in-memory — no per-case disk file), then mount and assert
     the oracle:
     - **content fidelity:** for every model file at path `P` whose every path-component is
       `name_acceptable`, `store.read(b"main", P) == Some(contents)`;
     - **skip discipline:** a file/dir with a rejected name (and any descendant under it) is
       **absent** from the store (the `continue`-before-push skips the whole subtree); a
       non-regular entry (a Unix symlink/FIFO, created where the harness can) is absent and
       non-fatal;
     - **count:** the returned `count` equals the number of accepted regular files (every
       accepted ancestor directory) in the model;
     - **no panic / total:** `populate` returns `Ok` for *every* generated tree (adversarial
       names included) — refuse-not-crash, the rev1§4.9 "tooling enforces the convention …
       skips" discipline.
  4. **Determinism / canonical-order property (the rev1§6 prose, `spec_rev1.md:405`).**
     Materialize the **same logical model** into **two** temp dirs created in different file-
     creation orders; build an image from each; assert the two mounts have **identical
     logical contents** (read-back of every accepted path is equal). The `sort_by_key`
     (`:31`) + the prolly canonical form (rev1§4.1) make mkfs's output a function of the
     *logical* tree, not the host `read_dir` order — this property pins mkfs's half of that
     (B13 owns the prolly half). Compare *logical contents*, not raw image bytes (the
     snapshot row carries a wall-clock timestamp from `SystemTime::now`, so images are not
     byte-identical across runs — see Rejected, below).
  - **Miri routing for the walk proptest:** the walk has no `unsafe`; its CAS write/mount
    path is already Miri-swept under `-p cas`. So the walk proptest is routed **native**
    (real `read_dir` over a temp tree; `cases: if cfg!(miri) { … }` is still applied for
    portability, but mkfs is **not** added to the standing Miri sweep). The Miri-able tier
    for mkfs is the `name_acceptable` proptest (item 2). Recorded in Design decision 3.
- **Rejected — drive the walk by spawning the binary per proptest case.** The existing tests
  `Command::new(CARGO_BIN_EXE_mkfs)`; 256 subprocess spawns + image builds per proptest is
  far too slow and cannot run under Miri at all. The `bin`+`lib` split makes the walk callable
  in-process, which is the enabler. (The two existing *example* integration tests keep
  spawning the binary — they also cover the `main`/`ExitCode` path; B15A leaves them.)
- **Rejected — assert byte-identical images for the determinism property.** The snapshot
  timestamp (`SystemTime::now`, `mkfs/src/main.rs:93-95`) makes the image non-deterministic
  across runs; the meaningful, stable property is **logical-content** equality, which is what
  rev1§4.1/§6 actually promise. (A byte-identical check would require threading a fixed clock
  through `run`, more surgery than a [low] item warrants.)
- **Rejected — a fully abstract `Source` trait so the walk reads no real FS.** Decoupling
  `populate` from `std::fs` behind a trait would make the *entire* walk Miri-able with no temp
  dirs, but it is disproportionate surgery for a [low] backfill and would change `populate`'s
  signature (a public-API churn for the one in-tree caller). The in-memory-model-→-temp-dir
  approach gets the same coverage with a `read_dir` the walk already uses; the pure
  `name_acceptable` already gives the Miri-able core.

**Recommendation: bin→bin+lib; extract `name_acceptable` (Miri-able unit + proptest); a
native walk proptest over generated temp-dir trees against a `MemDev` `Store` with the
content/skip/count/total oracle; a determinism property over two creation-orders comparing
logical contents. Record mkfs's Miri routing (name rule = Miri-able; walk = native +
cas-Miri-covered).**

---

## Design decision 2 — making the `user/*` non-I/O logic host-testable: `#[cfg(not(test))]`-gate the bare-metal items so `cargo test` builds a host harness, and split formatting/policy from I/O *(resolve in B15B/B15C)*

A `#![no_std] #![no_main]` binary that only declares `_start` cannot be built by a host
`cargo test` (no `main`, the bare-metal `#[global_allocator]`/`#[panic_handler]` collide with
std's). And the formatters (`out_utc` etc.) write straight to `sys::debug_write`, so their
*output* isn't assertable even once the crate links. Both must be resolved minimally.

- **Adopted — gate the bare-metal items behind `#[cfg(not(test))]` so the crate compiles as
  a normal host test harness under `cargo test`; and split the *computation* from the *write*
  for the logic worth asserting.**
  1. **`cfg(not(test))` gating** (the standard idiom for unit-testing a `no_main` binary).
     In each user binary B15 tests, change the crate attributes to
     `#![cfg_attr(not(test), no_std)]` / `#![cfg_attr(not(test), no_main)]` and gate
     `_start`, the `#[global_allocator]`, and the `#[panic_handler]` with `#[cfg(not(test))]`.
     Under `cfg(test)` (host) std and the default test `main` take over, the pure fns are
     reachable, and a `#[cfg(test)] mod tests { … }` runs. The **aarch64 build is byte-for-byte
     unchanged** (it is always `cfg(not(test))`). `ipc::sys`'s host stub (`sys.rs:109`) makes
     the crate link; the tests call only pure fns, never a `sys::*` with real effect.
  2. **Format/computation split** (shell, B15B). Refactor the date path so the *bytes* are
     assertable without a syscall: introduce a pure core that formats into a buffer —
     e.g. `fn fmt_utc(ns: u64, out: &mut impl core::fmt::Write)` or
     `fn fmt_utc(ns: u64) -> heapless/ArrayString`-style fixed buffer — and have `out_utc`
     (`:137-155`) call it then `out()` the bytes. `out_num`/`out_num_pad`/`out_hex` get the
     same treatment if convenient. `civil_from_days` is **already** a pure return value
     (`:118-129`) — test it directly with golden vectors, no refactor.
  3. **Policy extraction** (shell prune, B15B). Extract the retention selection from
     `cmd_prune` (`:355-358`) into a pure `fn prune_victims(rows: &[SnapRow], keep_n: u64) ->
     Vec<u64>` (filter `class != 0`, take all but the newest `keep_n`); `cmd_prune` keeps the
     IPC loop over its result. The pure fn is proptest-able.
  4. **Parse extraction** (storaged/init, B15C). Extract the SD02 parse from storaged's
     `_start` (`:117-121`) into a pure `fn parse_config(buf: &[u8]) -> Option<Config>` (magic
     + length + five LE fields), and the SD02/SH01 *construction* in init into pure builders;
     test the construct↔parse round-trip and the **reject** path (short/garbage → `None`,
     never a panic — the rev1§2.7 discipline).
- **Rejected — a separate host-only test crate that `path = "../shell/src/..."`-includes the
  logic.** Brittle (`include!`/path games), and it would not exercise the *real* module the
  binary compiles. The `cfg(not(test))` gate tests the actual code.
- **Rejected — mock `ipc::sys` to host-test the I/O paths (`request`, `recv_blocking`, the
  spawn loop, the MMIO probe).** Standing up a kernel/transport mock to drive `cmd_run`'s
  spawn/reap, storaged's serve loop, or the virtio probe is a large harness for a [low]
  backfill, and it would test the *mock*, not the kernel. That logic is correctly validated by
  the QEMU boot smoke (Design decision 3). B15 tests only logic that is genuinely
  *independent* of the syscall layer.
- **Rejected — convert each user binary to bin+lib (the mkfs DD1 move).** For the user
  binaries the `cfg(not(test))` gate is *less* churn than a lib split (no second target, no
  manifest `[lib]`, the bare-metal binary stays one file) and avoids the host-can't-link-the-
  `no_main`-bin problem a bin+lib package would reintroduce. (mkfs differs: it is already a
  std bin, so a lib split there is the clean move and gives `tests/` a crate to import.)

**Recommendation: `#[cfg(not(test))]`-gate the bare-metal items in each user binary B15
tests; split the shell's date formatting into a buffer-formatting core and extract
`prune_victims`; extract `parse_config`/the block builders in storaged/init. Test only
syscall-independent logic; leave the I/O on the QEMU gate. Format every touched `user/*`
file via its own manifest.**

---

## Design decision 3 — the QEMU boot smoke stays the integration gate; record the host-vs-boot split honestly (the rev1§6.1 discipline) *(resolve across B15)*

The parent plan is explicit: "keep the QEMU boot smoke as the integration gate"
(`0_address_audit_rev0.md:647`). The honesty rule (rev1§6.1, `spec_rev1.md:411`) requires
that a property guarded *only* by the boot smoke is not presented as host-unit-tested.

- **Adopted — name, per surface, what is host-tested and what stays the boot gate; add a
  short ledger note.** After B15:
  - **Host-tested (proptest/unit, Miri-able where noted):** the mkfs name rule + directory
    walk (B15A); the shell's date math/formatting, parsers, fault classifier, prune policy
    (B15B); the SD02/SH01/ST01 block encode/decode + reject discipline (B15C).
  - **QEMU-boot-gated (not host-tested, by design):** every `sys::*` interaction — init's
    wiring, storaged's mount + serve loop + the virtio-MMIO probe (`:128-150`), the shell's
    spawn/reap loop and `request`/IPC paths, selftest's fault/panic/bss-leak modes, `hello`.
    These are validated by `scripts/run-demo.sh` (the CLAUDE.md timeout-harness pattern).
  - **Ledger touch (optional, recommended).** Add a **Baselines** row (or extend the existing
    table, `verus_trusted-base.md:181-194`) recording `cargo test -p mkfs` and the user-binary
    host tests as test-routed baseline gates, in the same spirit as the GC-sufficiency note
    (`:66-74`) and the IPC-dispatch note (`:76-90`) — so a reviewer sees these are
    *test-routed, not mechanized*, and a future change re-runs them. No seam, no gate-count
    change.
- **Rejected — claim the user binaries are now "tested" without the split.** That would
  over-state coverage: most of each binary is syscall I/O still resting on the boot smoke.
  The split must be explicit.

**Recommendation: record the host-tested-vs-boot-gated split (per Design decision 3's lists),
add the light Baselines ledger note for `mkfs` + the user host tests, and keep
`scripts/run-demo.sh` green as the unchanged integration gate.**

---

## Sub-phase B15A — mkfs directory-walk coverage *(must-do; closes the §4.2 mkfs half; the headline)*

The headline deliverable. Make `populate` host-reachable (bin→bin+lib, Design decision 1),
extract `name_acceptable`, and proptest the walk against the mount-equality / skip / count /
determinism oracle. Touches only `mkfs/` and its tests; no behaviour change to the shipped
tool.

- **Touches:**
  - `mkfs/src/lib.rs` — **new**: `pub fn populate`, `pub fn mtime_nanos`, `pub fn run`, and
    the extracted `pub fn name_acceptable(name: &OsStr) -> Option<&str>` (lifted from the
    inline `:34-43` filter; `populate` calls it).
  - `mkfs/src/main.rs` — reduced to a thin `fn main() -> ExitCode { … mkfs::run() … }`.
  - `mkfs/Cargo.toml` — add the `[lib]`/`src/lib.rs` target and `[dev-dependencies] proptest =
    "1"` (cas/urt already carry proptest as the workspace precedent).
  - `mkfs/tests/walk.rs` — **new** (or a `#[cfg(test)]` module in `lib.rs`): the
    `name_acceptable` unit+proptest and the walk + determinism proptests.
  - `mkfs/tests/image.rs` — **unchanged** (the two existing example/`ExitCode` integration
    tests stay).
- **Depends on:** Part A blessed (rev1§4.9, rev1§6, rev1§7 text). No intra-B15 dependency.
- **Work:**
  1. Split bin→bin+lib; extract `name_acceptable` (no behaviour change — verify the bin's
     output is identical via the existing `image.rs` tests).
  2. `name_acceptable` unit golden cases + proptest (Design decision 1, item 2); Miri-able.
  3. The in-memory-tree generator + materializer; the walk proptest against a `MemDev`
     `Store` with the content/skip/count/total oracle (item 3); use `cases: if cfg!(miri)
     { 4 } else { 256 }`.
  4. The determinism property over two creation-orders comparing logical contents (item 4).
  5. A **negative control** for the oracle (the project's anti-theater habit): a deliberately
     broken expectation (e.g. assert an accepted file is *absent*) must make the walk proptest
     fail — confirming the oracle has teeth.
- **Acceptance:**
  - `cargo test -p mkfs` green: the two existing image tests + `name_acceptable` (unit +
    proptest, 256/4) + the walk proptest (content/skip/count/total) + the determinism
    property, all passing.
  - Adversarial generated trees (non-UTF-8 names, control-char names, `'/'`-bearing names,
    non-regular entries, deep nesting, empty dirs, empty/large files) yield **refuse-not-
    crash** and the correct logical mount.
  - The broken-oracle negative control fails (oracle has teeth).
  - The shipped binary is byte-for-byte unchanged (the bin+lib split is behaviour-preserving;
    `image.rs` still green).
- **Effort/Risk:** S–M / low. The substance is the tree generator + materializer and getting
  the skip-subtree semantics right in the oracle; the bin→lib split is mechanical.

---

## Sub-phase B15B — shell non-I/O logic host tests *(must-do "where feasible"; the prize; closes most of the §4.2 user-binary half)*

The shell holds the lion's share of host-testable `user/*` logic. Gate the bare-metal items
(Design decision 2), split the date formatting and the prune policy from their I/O, and
golden-vector / proptest the pure cores.

- **Touches:** `user/shell/src/main.rs` — the `#[cfg(not(test))]` gating on
  `no_std`/`no_main`/`_start`/`#[global_allocator]`/`#[panic_handler]`; the `fmt_utc`
  buffer-formatting split (over `out_utc` `:137-155` + `out_num*`/`out_hex`); the
  `prune_victims` extraction (over `cmd_prune` `:355-358`); a `#[cfg(test)] mod tests`.
  `user/shell/Cargo.toml` — add `[dev-dependencies] proptest` if a property test is used.
  **No** change to any I/O path, the dispatch table, or `_start`'s behaviour.
- **Depends on:** none in B15 (independent of B15A/B15C). Part A blessed (rev1§5.3 fault
  classification, rev1§2.6 time, rev1§4.7 retention policy is shell-side).
- **Work:**
  - **`civil_from_days`** (`:118-129`): golden vectors (epoch `0` → 1970-01-01; known dates
    incl. leap-year boundaries 2000-02-29, 2100-03-01; a far-future date) **and** an inverse
    property (days → (y,m,d) → days round-trips via a reference computation) over a proptest
    range.
  - **`fmt_utc`** (the split core): golden ISO-8601 strings for known nanosecond inputs
    (matching the `2026-06-11T12:34:56.123456789Z` form, full nanosecond precision per `:131-136`);
    a property that the output is always the fixed `YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ` shape and
    parses back to the input seconds.
  - **`parse_path`** (`:176-181`): unit + property — splits on `'/'`, drops empty components
    (`"//a///b/"` → `["a","b"]`, `""`/`"/"` → `[]`); round-trip against a reference join where
    well-defined.
  - **`parse_u64`** (`:379-391`): property against a reference parse for all-digit inputs;
    reject non-digit / empty → `None`; note (and test) it does **not** guard overflow (pins
    current behaviour — a forward note, since this is shell input, not a wire decoder).
  - **`fault_class`** (`:409-422`): golden ESR_EL1 values for each EC/DFSC branch (translation,
    permission, access-flag, address-size, the `abort`/`exception` fallbacks).
  - **`prune_victims`** (extracted): property — never selects a `class == 0` (keep) row; the
    surviving non-keep count is `min(candidates, keep_n)`; selects the **oldest** excess
    (matching `&candidates[..excess]`); `keep_n >= candidates` → empty.
- **Acceptance:**
  - `cargo test --manifest-path user/shell/Cargo.toml` green with the new `mod tests`.
  - The aarch64 build still links `ushell` unchanged; `scripts/run-demo.sh` still green
    (`date`, `prune`, `cat`, fault demo all behave as before — the splits are
    behaviour-preserving).
  - Date golden vectors and the round-trip property pass; the fault table and parsers pass.
  - The file is `cargo fmt`'d via `user/shell/Cargo.toml`.
- **Effort/Risk:** S–M / low. The `cfg(not(test))` gate and the small format/policy splits
  are mechanical; the value is the golden vectors for `civil_from_days`/`fmt_utc` (the most
  logic-dense, currently-untested code in `user/*`).

---

## Sub-phase B15C — storaged/init/selftest startup-block parsing host tests *(where feasible; the rev1§2.7 decode-discipline floor)*

The hand-rolled SD02/SH01/ST01 startup blocks are decoders of an untrusted-shaped message
(rev1§2.7): a short or garbage block must be **refused, never panic** (a panic here is a boot
failure). Extract the parse/construct, test the round-trip and the reject path. Small, shared
theme; "where feasible."

- **Touches:** `user/storaged/src/main.rs` (extract `parse_config` from `:117-121`; gate the
  bare-metal items), `user/init/src/main.rs` (extract the SD02/SH01 builders from
  `:168-174`/`:235-237`; the RTC sanity predicate from `:87`), optionally
  `user/selftest/src/main.rs` (the ST01 parse `:78-88`). `#[cfg(test)] mod tests` in each.
- **Depends on:** none in B15. Part A blessed (rev1§2.7 decode discipline; rev1§5.1 startup
  block; rev1§2.6 RTC).
- **Work:**
  - **SD02 round-trip + reject:** init's builder ↔ storaged's `parse_config` round-trips the
    five fields (mmio_va, dma_va, dma_pa, dma_len, time_va); a too-short buffer
    (`len < 44`), a wrong magic, and an empty buffer each return `None`/error **without
    panicking** (rev1§2.7). Property over arbitrary `&[u8]`: `parse_config` is **total** (never
    panics) — the decode-discipline floor.
  - **SH01 round-trip + reject:** init's SH01 builder ↔ the shell's parse (`shell:_start`
    `:743-748`, `&boot[..4] == b"SH01"` + 8-byte time-VA); short/garbage rejected, the shell's
    `blen >= 12` guard pinned.
  - **ST01 (optional):** selftest's parse (`:78-88`) — mode + optional time-VA; a `len < 5`
    block → mode `0`, a `len < 13` block → no time attach (current behaviour pinned).
  - **RTC sanity:** init's `RTC_MIN_SANE_SECS`/`cntfrq == 0` predicate (`:87`) as a pure
    `fn rtc_sane(secs, cntfrq) -> bool` — golden cases around the 2020 threshold and `cntfrq
    == 0`.
- **Acceptance:**
  - `cargo test --manifest-path user/storaged/Cargo.toml` and `… user/init/Cargo.toml` (and
    `user/selftest/Cargo.toml` if covered) green.
  - `parse_config`/the block parsers are **total** over arbitrary bytes (proptest: no panic);
    the construct↔parse round-trips; the aarch64 builds link unchanged and boot green.
  - Touched files `cargo fmt`'d via their own manifests.
- **Effort/Risk:** S / low. Mostly the extraction; the value is pinning refuse-not-crash on
  the startup blocks before **Phase C1** replaces the hand-rolled "SD02/SH01/ST01" blocks with
  the named-grant table (these tests document current behaviour at the C1 boundary).

---

## Execution order

```
B15A  mkfs walk coverage          [bin→lib + name rule + walk/determinism proptests]
B15B  shell non-I/O logic         [independent; the prize]
B15C  storaged/init startup parse [independent; the rev1§2.7 floor]
```

All three sub-phases are **mutually independent** (different files, different crates) and
can land in any order or in parallel. None depends on any other B15 sub-phase, and B15 as a
whole depends only on Part A being blessed (the rev1§ text it conforms to). B15B and B15C
share the Design-decision-2 `cfg(not(test))` mechanism but apply it to different crates, so
they do not serialize. B15A is listed first only because it is the parent plan's headline
("add proptest/unit coverage for the mkfs directory walk").

## Out of scope for B15 (recorded so it is not mistaken for a gap)

- **`loader::prepare`'s page-rounding host model + fuzz** — the *other half* of the audit's
  §4.2 bullet (`0_audit_rev0.md:516-517`). Routed to **Phase B3** (`doc/plans/3_b3-detail.md`);
  B15 does not touch `loader`.
- **The mkfs S-10 / `format` refusal contract** — already landed in **B12**
  (`mkfs/tests/image.rs:55-81`, `refuses_undersized_image_cleanly`). B15 does not redo it.
- **Host-testing the user binaries' syscall I/O** — init's wiring, storaged's mount/serve/
  virtio-probe, the shell's spawn/reap + `request` IPC, `hello`, selftest's fault/panic/bss
  modes. These rest on the **QEMU boot smoke** (`scripts/run-demo.sh`), the integration gate
  the parent plan keeps (Design decision 3); standing up a kernel mock to unit-test them is
  out of scope for a [low] backfill.
- **The named-grant table / standard-name startup format** that replaces the hand-rolled
  "SD02/SH01/ST01" blocks — that is **Phase C1** (pulled forward for the M-9 console). B15C's
  block-parse tests pin *current* behaviour at the C1 boundary; C1 reworks the format.
- **Verus/TLA/Loom for `mkfs` or `user/*`** — none are verified-surface or concurrency
  surfaces; B15 adds no mechanized proof, no model, and no concurrency harness (Design
  decisions in the tier note). The tally stays **14** and every Verus/TLA gate is held by
  not touching it.
- **Adding `mkfs`/`user/*` to the standing CLAUDE.md Miri sweep** — the walk and the
  user-binary logic are `unsafe`-free, so the sweep (scoped to the `unsafe`-heavy crates)
  is not extended; the `name_acceptable` proptest is Miri-able and the walk's CAS path is
  already covered under `-p cas` (Design decision 3's routing).
