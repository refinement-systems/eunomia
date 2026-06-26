# Eunomia OS — Development Guide

Full design specification: `doc/spec/spec_rev2.md`. Read the spec before
touching any component. Section numbers below refer to that document.
All spec references must contain the revision number, like "rev2§6" or "rev2§3.1".

The trusted base is exactly the seams enumerated in
`doc/guidelines/verus_trusted-base.md` (the ledger), kept honest by
`doc/guidelines/verus.md`.

---

## Workspace layout

```
kernel/          AArch64 bare-metal microkernel (aarch64-unknown-none) —
                 the architectural shell over kcore (boot, MMU, GIC, sched)
kcore/           Host-buildable kernel object core: cspace/CDT, untyped,
                 channels, notifications, thread/timer objects, aspace data;
                 Verus-verified (rev2§6, doc/guidelines/verus.md). no_std,
                 zero deps; the kernel links it, hardware + objects behind the
                 handle/Store seam
ipc/             Async IPC crate — shared by all userspace servers (rev2§3.5)
dma-pool/        DMA buffer pool — the only place PAs are visible (rev2§2.5)
cas/             CAS primitives: chunker, prolly tree, commit protocol (rev2§4)
storage-server/  Userspace storage server process (rev2§4)
virtio-blk/      Virtio-blk driver, written against dma-pool (rev2§2.5)
loader/          ELF loader / program spawner (rev2§5)
user/            Real userspace binaries (init, shell, storaged, …) — own
                 mini-workspaces, built by kernel/build.rs (rev2§5, rev2§7)
mkfs/            Host-side disk image builder; reuses cas crate (rev2§7)
tla/             TLA+ formal specifications
tools/tla/       Scripts: tla-check.sh (SANY), tla-model-check.sh (TLC)
doc/spec/        Design documents
doc/results/     Implementation and research results.
doc/guidelines/  Additional guidelines
vendor/verus/    Vendored Verus prover (git submodule, fork pinned to the release
                 in doc/guidelines/verus.md) — in-tree reading copy of vstd and
                 the state-machine macro examples; not built by cargo
```

---

## Comment and documentation discipline

Comments and documentation (doc/spec and doc/guidelines) describe what is, not what was, or what was removed.
Comments may in exceptional cases document paths not taken and rationale for not taking it, if the existing implementation is surprising.
Comments may reference doc/spec and doc/guidelines, nothing else.
Documents in doc/plans and doc/results are considered temporary intermediate reports, and may not be referenced in comments, or in specs and guidelines.

---

## Build commands

### Kernel (cross-compiled for AArch64 bare-metal)

The `user/*` binaries build `std` via build-std from the **vendored fork**
(`vendor/rust`), so a fresh clone needs that submodule plus its `library/backtrace`
nested submodule (std includes it unconditionally via `#[path]`) before the kernel
build will succeed — `-Zbuild-std` is redirected there by `kernel/build.rs`
(`__CARGO_TESTS_ONLY_SRC_ROOT`), not at rustup's `rust-src`. Pull only those (the
fork's other nested submodules — `llvm-project`, `cargo`, … — are huge and
unneeded):

```sh
git submodule update --init vendor/rust
git -C vendor/rust submodule update --init library/backtrace
```

The std-build nightly is pinned by `kernel/rust-toolchain.toml` to match the
`vendor/rust` commit exactly (rustup auto-installs it with `rust-src`); the pin is
kernel-scoped so it never perturbs the `verus` (Rust 1.95.0) or host toolchains.

```sh
# Build (target aarch64-unknown-none-softfloat and build-std set by
# kernel/.cargo/config.toml; softfloat because trap frames don't save SIMD)
cd kernel && cargo build

# Release build
cd kernel && cargo build --release

# Run in QEMU (uses the runner in kernel/.cargo/config.toml)
cd kernel && cargo run

# Run manually / with GDB stub (attach with gdb-multiarch on :1234).
# gic-version=3 is required (gic.rs drives GICv3 redistributor + ICC_*).
qemu-system-aarch64 -machine virt,gic-version=3 -cpu cortex-a72 -m 256M \
  -nographic -serial mon:stdio \
  -kernel target/aarch64-unknown-none-softfloat/debug/kernel \
  -s -S
```
Note: the cargo target directory is at the workspace root (`target/`), not
under `kernel/`.

#### Running the QEMU smoke non-interactively (and killing it cleanly)

`scripts/run-demo.sh` builds + boots the full stack (mkfs image → virtio-blk →
storaged → mount → shell) and `exec`s QEMU. It is interactive by default but
reads scripted commands on stdin. The recurring trap: **QEMU must be killed by
the harness, or it runs forever** (it sits at the shell waiting for more stdin
after your piped commands hit EOF), and the usual one-liners don't kill it:

- `timeout`/`gtimeout` are **not installed** on this machine (no GNU coreutils).
- `perl -e 'alarm N; exec @ARGV' bash scripts/run-demo.sh` does **not** work:
  `alarm` survives `exec` into bash but `run-demo.sh` `fork`s QEMU as a child,
  and the timer is not inherited — at N seconds only `bash` dies while the
  orphaned `qemu-system-aarch64` keeps running (this is what hung for 14 min).

Reliable pattern — a Perl parent that puts the script in its own process group
and signals the **whole group** on timeout (kill `-pid`):

```sh
printf 'write docs/smoke hello\nsync\ncat docs/smoke\nls docs\ndf\n' | \
perl -MPOSIX=setsid -e '
  my $t = shift @ARGV;
  defined(my $pid = fork) or die "fork: $!";
  if ($pid == 0) { setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!"; }
  local $SIG{ALRM} = sub { kill "TERM", -$pid; sleep 2; kill "KILL", -$pid; exit 124; };
  alarm $t; waitpid($pid, 0); exit($? >> 8);
' 90 bash scripts/run-demo.sh 2>&1 | tail -60
```

The boot is green when the log shows `[storaged] store mounted` → `serving`,
your shell commands echo their results (e.g. `cat` returns what `write` stored),
and there is no panic/`Corrupt`/`unwrap` trace. If a run is ever orphaned,
`pkill -f qemu-system-aarch64` cleans it up.

### Host crates (storage-server, cas, mkfs, etc.)

```sh
# Build all host crates from workspace root
cargo build -p cas -p storage-server -p mkfs ...

# Run tests for cas (primary proptest target)
cargo test -p cas

# Run with Miri. Under cfg(miri) the proptests drop to 4 cases AND cap their
# op streams (blake3 is interpreted — no SIMD — so native-scale work would take
# hours). cas is the heavy crate: every store/tree/file test drives interpreted
# blake3. Miri is a SINGLE-THREADED interpreter — inside one `cargo miri test`
# process even `--test-threads=N` runs on one core, so don't expect it to help;
# cross-process parallelism is the lever (nextest, below). Single-core serial is
# ~an hour (sum of all tests); nextest -j4 runs ~12 min here — now throughput-
# bound across the 4 cores, not gated by one pole (the cfg(miri) op/size caps
# flattened the long tail). Driving it lower means capping more of the remaining
# ~100-180 s tests (the crash-recovery family, chunk-boundary proptests, the
# gc_mark corpus). Because it is long-running, NEVER
# pipe it into `tail` (or any
# buffering filter): `tail` emits nothing until the command exits, so the log
# stays empty for the whole run and you cannot tell progress from a hang — this
# has wasted time before. Instead redirect to a file you can inspect mid-run, or
# run it in the background and watch the live log / check the `miri` PID's CPU
# with `ps` to confirm it is progressing. Quickest useful UB pass
# (regression tests + every committed fuzz seed, ~30 s for all 3 crates):
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
#     -p cas -p loader -p storage-server \
#     --test fuzz_regressions --test fuzz_corpus
# Full cas sweep (canonical, parallel). -Zmiri-disable-isolation is REQUIRED:
# proptest's failure-persistence calls current_dir(), which Miri's isolation
# blocks (getcwd unsupported) — every other crate's line below already passes
# it. nextest runs one process per test, so -j4 fans the suite across the 4
# performance cores (use -j8 to also use the efficiency cores). One-time setup:
# `cargo install cargo-nextest --locked`.
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p cas -j4
# Serial fallback (no nextest, single-core), same required flag:
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas
# The DMA-pool wrapper (the one place PAs are visible) joins the sweep:
# it has no fuzz corpus, so its proptests run as the crate's lib tests —
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p dma-pool -j4
# The urt heap allocator wrapper joins (same posture as dma-pool: no
# fuzz corpus, so its proptests run as the crate's lib tests — randomized
# alloc/dealloc/realloc, exhaustion, and the fragmentation-cap leak path). The
# fragmentation-cap proptest fully carves a ~2050-block heap, so it caps Miri at
# one case (the rest stay at 4); no blake3, so the sweep is still quick —
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p urt -j4
```

### Verus verification — the deductive-verification gate

The discipline lives in `doc/guidelines/verus.md` (Part A); the trusted base it
gates is `doc/guidelines/verus_trusted-base.md`. `cargo verus verify` runs the
prover. Verus has **no crates.io binary** and is pinned as one unit (binary
`0.2026.06.07.cd03505`, its `vstd` companion, and Rust toolchain `1.95.0` — see
`.github/workflows/ci.yml`'s `verus` job); a local install just unzips the
matching release and puts its `cargo-verus`/`verus`/`z3` directory on `PATH`.
Confirm the binary matches the pin before trusting any result:

```sh
verus --version   # must print Version: 0.2026.06.07.cd03505, Toolchain: 1.95.0-...
```

**From a fresh build (after `cargo clean`).** `cargo clean` wipes the workspace
`target/`, so the next run re-verifies every obligation from scratch — this is
the authoritative, no-stale-cache run, exactly what the CI job does. Verify each
gated crate, one `-p` per crate, **no per-proof filter** (a new `verus!{}`
obligation auto-gates):

```sh
cargo clean
cargo verus verify -p kcore
cargo verus verify -p ipc
cargo verus verify -p urt
cargo verus verify -p freelist
cargo verus verify -p dma-pool
cargo verus verify -p cas --no-default-features   # cas is Vec-heavy; the
                                                  # feature-agnostic codecs verify
                                                  # in the no_std+alloc variant
cargo verus verify -p virtio-blk                  # avail_ring_slot index/wrap +
                                                  # check_capacity LBA-bound
                                                  # arithmetic; re-verifies its
                                                  # gated deps (cas pulls
                                                  # vstd[alloc])
cargo verus verify -p storage-server --no-default-features --lib
                                                  # rights lattice (attenuate
                                                  # monotone / deny-by-default) +
                                                  # wire header/version decode
                                                  # prefix (check_header, total ∀
                                                  # bytes; postcard body stays the
                                                  # trusted seam by feature-
                                                  # exclusion); no_std+alloc variant
                                                  # like cas, --lib skips the
                                                  # placeholder bin
cargo verus verify -p loader --no-default-features # ELF page_layout (total,
                                                  # overflow-safe ∀ vaddr,memsz)
                                                  # + parse total bounded decoder
                                                  # ∀ &[u8] (le readers + every
                                                  # accepted Image well-formed);
                                                  # no_std core, re-verifies ipc
```

A real run ends each crate with a `verification results:: N verified, 0 errors`
line; expected counts per crate are in the trusted-base ledger.

**Making sure the cache is clean before another verification.** Verus caches
verification per build, so re-running over an unchanged `target/` (or a
`--verify-function`/`--verify-only-module`-scoped run) can exit 0 **from stale
cache without re-verifying** — a false green. The tell is the *missing*
`verification results::` line: present == a real run, absent == cached. To force
an authoritative re-verify of one crate, clean just that crate first; for the
whole gate, `cargo clean` first as above:

```sh
cargo clean -p kcore && cargo verus verify -p kcore   # results line present == real run
```

This bites hardest after editing a shared spec/predicate, where a scoped recheck
of the edited function alone reports nothing. When in doubt, clean.

**Profiling proof time.** Forward `--time-expanded` (with `--output-json` for a
machine-readable form) to the prover for a per-module / per-function timing
breakdown — the JSON's `times-ms.smt.smt-run-module-times[].function-breakdown[]`
ranks each function's SMT `time`/`rlimit`, the data you need to find the
expensive obligation behind a slow crate or an rlimit blowup:

```sh
cargo clean -p kcore   # a real run; a cached one reports no timing at all
cargo verus verify -p kcore -- --time-expanded --output-json
```

`scripts/verus-baseline.sh` automates this across the whole gate: it cleans then
verifies each crate, captures the per-crate JSON, and prints a summary table plus
the slowest functions (`scripts/verus-baseline.sh [crate...]`, output under
`target/verus-baseline/`). Run it before a proof-perf change to establish a
baseline, and after to see what moved.

**Performance discipline — measure every proof change, correctness first.**
Verified code is held to a proof-checking-cost budget, but **correctness and
thoroughness always outrank checker speed**: never weaken a spec, drop or skip an
obligation, loosen an `ensures`, or narrow input coverage to make the prover
faster — a slower proof that proves *more* is correct, a faster one that proves
*less* is a regression. Within that bound, every change touching `verus!{}` code
measures its effect on verification cost with the profiling tools above. There is
no committed baseline (`target/verus-baseline/` is local and gitignored), so
re-derive the before-number freshly from the base of your work rather than
trusting a saved one: run `scripts/verus-baseline.sh` on the pre-change tree — or
temporarily rewind (`git stash` your edits, or check out the base / merge-base
commit), measure, and return — then re-run on your changed tree and diff. A merge
or rebase moves that base, so re-establish the before-number on the merged code;
never compare against numbers from a stale tree. Judge by deterministic `rlimit`
on cold (`cargo clean`) runs against byte-identical controls, per
`doc/guidelines/verus.md` §10 — wall-clock ms is noisy and machine-dependent, so
advisory only. Keep a perf-motivated change only if it measurably helps (or at
least does not regress the crate's `rlimit` total), and revert measured
regressions; extraction around quantified/existential predicates is a known
backfire. The technique itself — decomposition into tightly-keyed lemmas,
projection triggers over whole-aggregate triggers, extracting recurring
`by (bit_vector)` identities, `rlimit` right-sizing, and the bounded dead-ends —
lives in `doc/guidelines/verus.md` §10 and the §13 decision map; reach for those
before guessing.

### Formatting — run `cargo fmt` before every commit

The tree is kept rustfmt-clean **per change**: run `cargo fmt` before committing
and stage the result, so each commit's diff is only its own work. There are no
longer periodic "rustfmt" sweep commits — those existed because earlier changes
skipped fmt; don't bring them back.

The catch is the workspace split (`Cargo.toml`): a plain `cargo fmt` at the root
formats every **root-workspace** member (`cas`, `kcore`, `kernel`, `ipc`,
`storage-server`, `mkfs`, `dma-pool`, `freelist`, `virtio-blk`, `loader`, `urt`)
but **silently skips** the separate workspaces — the `user/*` binaries
(`storaged`, `init`, `shell`, …, their own mini-workspaces) and the `*/fuzz`
crates (`cas/fuzz`, `storage-server/fuzz`, `loader/fuzz`, `ipc/fuzz`, excluded so
a plain build never pulls libfuzzer). If your change touches one of those, format
it via its own manifest, e.g.:

```sh
cargo fmt --manifest-path user/storaged/Cargo.toml
cargo fmt --manifest-path cas/fuzz/Cargo.toml
```

(The trap this avoids: editing `user/storaged` and running only the root
`cargo fmt` leaves it untouched, so the next person to fmt that workspace drags
your file's pre-existing reformatting into their diff.)

#### Verus code — also run `verusfmt` before committing

`cargo fmt` does **not** format code inside the `verus!{}` macro (rustfmt skips
macro interiors), so the spec/proof code is left untouched by it. `verusfmt`
(pinned to **0.7.2** here) is the complement: run `scripts/verusfmt.sh` before
committing any change that touches `verus!{}` code. It is a complement, not a
replacement — it formats only the macro interior (`--verus-only`) and then runs
`cargo fmt`, which stays the authority for every out-of-macro line. The two have
disjoint domains, so the pair is a deterministic fixed point, and `verusfmt` is
layout-only — it does not change what Verus proves (the verification gate was
re-run green over the formatted tree).

```sh
scripts/verusfmt.sh            # format Verus interiors in place, then cargo fmt
scripts/verusfmt.sh --check    # gate: verify formatting (pairs with cargo fmt --check)
```

The authoritative formatting gate is still `cargo fmt --check`;
`scripts/verusfmt.sh --check` adds the macro-interior half.

Four files are **excluded** by the script — `verusfmt` 0.7.2 mishandles them, so
they keep their hand formatting and `cargo fmt` alone owns them:

- `cas/src/disk.rs`, `cas/src/prolly.rs` — verusfmt's parser rejects the
  half-open range index `x[..n]`.
- `cas/src/store.rs`, `kcore/src/aspace.rs` — files with several `verus!{}`
  blocks: verusfmt wrongly indents the plain-Rust comments **between** the
  blocks.

If a new file gains either trait (an `x[..n]` index, or comments between multiple
`verus!{}` blocks that verusfmt re-indents), add it to the script's skip list.
No `user/*` binary or `*/fuzz` crate contains `verus!{}`, so the separate-
workspace caveat above does not apply to verusfmt.
