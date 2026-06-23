# Eunomia OS — Development Guide

Full design specification: `doc/spec/spec_rev1.md`. Read the spec before
touching any component. Section numbers below refer to that document.
All spec references must contain the revision number, like "rev1§6" or "rev1§3.1".

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
                 Verus-verified (rev1§6, doc/guidelines/verus.md). no_std,
                 zero deps; the kernel links it, hardware + objects behind the
                 handle/Store seam
ipc/             Async IPC crate — shared by all userspace servers (rev1§3.5)
dma-pool/        DMA buffer pool — the only place PAs are visible (rev1§2.5)
cas/             CAS primitives: chunker, prolly tree, commit protocol (rev1§4)
storage-server/  Userspace storage server process (rev1§4)
virtio-blk/      Virtio-blk driver, written against dma-pool (rev1§2.5)
loader/          ELF loader / program spawner (rev1§5)
user/            Real userspace binaries (init, shell, storaged, …) — own
                 mini-workspaces, built by kernel/build.rs (rev1§5, rev1§7)
mkfs/            Host-side disk image builder; reuses cas crate (rev1§7)
tla/             TLA+ formal specifications (must check before M2)
tools/tla/       Scripts: tla-check.sh (SANY), tla-model-check.sh (TLC)
doc/spec/        Design documents
doc/results/     Implementation and research results.
doc/guidelines/  Additional guidelines
```

---

## Build commands

### Kernel (cross-compiled for AArch64 bare-metal)

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
# The DMA-pool wrapper (the one place PAs are visible) joins the sweep as of
# B4C: it has no fuzz corpus, so its proptests run as the crate's lib tests —
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p dma-pool -j4
# The urt heap allocator wrapper joins as of B11C (same posture as dma-pool: no
# fuzz corpus, so its proptests run as the crate's lib tests — randomized
# alloc/dealloc/realloc, exhaustion, and the fragmentation-cap leak path). The
# fragmentation-cap proptest fully carves a ~2050-block heap, so it caps Miri at
# one case (the rest stay at 4); no blake3, so the sweep is still quick —
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p urt -j4
```

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
