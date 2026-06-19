# Eunomia OS — Development Guide

Full design specification: `doc/spec/3_spec_rev3.md`. Read the spec before
touching any component. Section numbers below refer to that document.

---

## Workspace layout

```
kernel/          AArch64 bare-metal microkernel (aarch64-unknown-none) —
                 the architectural shell over kcore (boot, MMU, GIC, sched)
kcore/           Host-buildable kernel object core: cspace/CDT, untyped,
                 channels, notifications, thread/timer objects, aspace data;
                 Verus-verified (§6, doc/plans/3_verus-rewrite.md). no_std,
                 zero deps; the kernel links it, hardware + objects behind the
                 handle/Store seam
ipc/             Async IPC crate — shared by all userspace servers (§3.5)
dma-pool/        DMA buffer pool — the only place PAs are visible (§2.5)
cas/             CAS primitives: chunker, prolly tree, commit protocol (§4)
storage-server/  Userspace storage server process (§4)
virtio-blk/      Virtio-blk driver, written against dma-pool (§2.5)
loader/          ELF loader / program spawner (§5)
user/            Real userspace binaries (init, shell, storaged, …) — own
                 mini-workspaces, built by kernel/build.rs (§5, §7)
mkfs/            Host-side disk image builder; reuses cas crate (§7)
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

### Host crates (storage-server, cas, mkfs, etc.)

```sh
# Build all host crates from workspace root
cargo build -p cas -p storage-server -p mkfs ...

# Run tests for cas (primary proptest target)
cargo test -p cas

# Run with Miri. The proptest suites drop to 4 cases under cfg(miri) —
# blake3 is interpreted (no SIMD), so native-scale case counts would take
# hours; even reduced, this sweep runs ~25 min. Quickest useful UB pass
# (regression tests + every committed fuzz seed, ~30 s for all 3 crates):
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
#     -p cas -p loader -p storage-server \
#     --test fuzz_regressions --test fuzz_corpus
cargo +nightly miri test -p cas
```
