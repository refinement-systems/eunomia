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
# The DMA-pool wrapper (the one place PAs are visible) joins the sweep as of
# B4C: it has no fuzz corpus, so its proptests run as the crate's lib tests —
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p dma-pool
```
