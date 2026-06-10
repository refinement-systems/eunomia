# Eunomia OS — Development Guide

Full design specification: `doc/spec/0_spec_mvp.md`. Read the spec before
touching any component. Section numbers below refer to that document.

---

## Workspace layout

```
kernel/          AArch64 bare-metal microkernel (aarch64-unknown-none)
ipc/             Async IPC crate — shared by all userspace servers (§3.5)
dma-pool/        DMA buffer pool — the only place PAs are visible (§2.5)
cas/             CAS primitives: chunker, prolly tree, commit protocol (§4)
storage-server/  Userspace storage server process (§4)
virtio-blk/      Virtio-blk driver, written against dma-pool (§2.5)
loader/          ELF loader / program spawner (§5)
shell/           Command-line shell with built-ins for the demo (§7)
mkfs/            Host-side disk image builder; reuses cas crate (§7)
tla/             TLA+ formal specifications (must check before M2)
tools/tla/       Scripts: tla-check.sh (SANY), tla-model-check.sh (TLC)
doc/spec/        Design documents
```

---

## Build commands

### Kernel (cross-compiled for AArch64 bare-metal)

```sh
# Build (target and build-std set by kernel/.cargo/config.toml)
cd kernel && cargo build

# Release build
cd kernel && cargo build --release

# Run in QEMU (uses the runner in kernel/.cargo/config.toml)
cd kernel && cargo run

# Run with GDB stub (attach with gdb-multiarch on :1234)
qemu-system-aarch64 -machine virt -cpu cortex-a72 -m 256M -nographic \
  -serial mon:stdio -kernel target/aarch64-unknown-none/debug/kernel \
  -s -S
```

### Host crates (storage-server, cas, mkfs, etc.)

```sh
# Build all host crates from workspace root
cargo build -p cas -p storage-server -p mkfs ...

# Run tests for cas (primary proptest target)
cargo test -p cas

# Run with Miri
cargo +nightly miri test -p cas
```

### TLA+ specs

```sh
# Syntax check
bash tools/tla/tla-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-check.sh tla/cap_revocation/CapRevocation.tla

# Model check (run before M2 and M1 implementations respectively)
bash tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla
```

---

## Milestones and current status

| Milestone | Status | Key deliverable |
|-----------|--------|-----------------|
| **M0** | 🚧 In progress | Boot, UART, MMU, exception handling |
| **M1** | 🔲 Not started | Caps + threads + async channels; CDT revoke |
| **M2** | 🔲 Not started | virtio-blk; CAS + prolly tree; session protocol; mkfs |
| **M3** | 🔲 Not started | ELF loader; spawn-with-caps; shell |
| **M4** | 🔲 Not started | Snapshot / rollback demo (MVP) |
| **M5** | 🔲 Not started | GC + history rewriting |

### M0 exit criterion
Boot QEMU → kernel prints over PL011 → MMU enabled → synchronous exception
triggered, caught, and reported → system halts with ESR/ELR printed.

### Sequencing rules
- **TLA+ `CapRevocation` model must be checked before M1 implementation.**
- **TLA+ `CommitProtocol` model must be checked before M2 implementation.**
- `cas` crate's proptest canonical-form suite must pass before `mkfs` is used.
- The `storage-server` and `mkfs` can be developed on macOS host in parallel
  with M0–M1 (they are pure userspace Rust, no kernel dependency).
- IOMMU migration (§2.5) must happen before writing the second DMA driver.

---

## Architecture invariants (never violate these)

- **No ambient authority.** Every resource access is via a capability slot or
  a storage handle. No globals, no environment-based auth.
- **Monotone derivation.** Authority can only shrink, never grow (§2.3).
  Attenuation is the only derivation; there is no amplification path.
- **Move semantics for caps** (§3.4). A cap has exactly one owner at all
  times. Senders duplicate first if they want to keep access.
- **Raw hashes are not authority** (§2.4). Storage handles (small integers,
  session-relative) are authority. Hashes are internal addresses and proofs.
- **Event delivery never allocates** (§3.6). Both the notification-bit regime
  and the future wait-set upgrade must satisfy this.
- **DMA only through DmaPool** (§2.5). No raw physical addresses outside the
  `dma-pool` crate. The `phys-read` rights bit enforces this at the kernel
  level; code discipline enforces it in userspace.
- **No kernel allocation that isn't user-accounted** (§2.5, §3.2). Channels,
  address spaces, and wait-sets are created from untyped memory donated by the
  creator; the kernel has no global pool.

---

## Verification tiers (§6)

| Tool | Scope | When |
|------|-------|------|
| TLA+ / TLC | commit protocol, cap revocation | Before respective milestone |
| Verus | cspace/CDT ops, kernel allocator | Written in Verus dialect from day one |
| Kani | kernel data-structure invariants | During kernel development |
| Loom / Shuttle | IPC crate, userspace servers | During M1+ development |
| Miri + proptest | everything; chunker + prolly tree esp. | Continuous |
| cargo-fuzz | IPC decoder, postcard payloads | From M1 |

The IPC crate (`ipc/`) is the first serious Loom/Shuttle target (§3.5).

---

## Kernel source map (`kernel/src/`)

| File | Responsibility |
|------|---------------|
| `main.rs` | Entry point (`kernel_main`), panic handler |
| `boot.rs` | `_start` assembly: core selection, SP_EL1, BSS zero, → kernel_main |
| `uart.rs` | PL011 UART driver (MMIO at 0x0900_0000); `core::fmt::Write` impl |
| `exceptions.rs` | AArch64 exception vector table + EL1 handlers |
| `mmu.rs` | Identity-map MMU setup: MAIR/TCR/TTBR0, enable SCTLR_EL1.M |

M1 additions: `cspace.rs`, `untyped.rs`, `thread.rs`, `channel.rs`,
`notification.rs`, `timer.rs`, `syscall.rs`.

### QEMU virt memory map (relevant to M0)
```
0x0900_0000  PL011 UART0
0x0800_0000  GICv3 distributor
0x4000_0000  DRAM start (kernel loads here)
```

---

## Storage server conventions

- All state accessed via handles (small integers, session-relative).
- Per-ref overlays; never a single global memtable.
- Flush triggers: explicit sync/snapshot > WAL pressure > size pressure > timer.
- Commit is always: fsync chunks → write new superblock → fsync superblock.
  Nothing is freed on the write path; GC is the only reclamation mechanism.
- Snapshot identity is a per-ref sequence number, never a content hash (§4.7).

---

## IPC wire protocol

- Every message: fixed hand-defined header (proto id, version, opcode, flags,
  body length) + postcard-encoded body (§3.7).
- Capabilities travel in cap slots, never in payloads.
- Storage handles are plain integers in payloads; never raw hashes.
- Message types: boring — no borrowed lifetimes, no serde tricks.
- Decoders reject trailing bytes; they are cargo-fuzz targets.

---

## Style and code conventions

- `no_std` for kernel and userspace process crates; `std` available for cas,
  mkfs, and for host-side testing of any crate.
- No `unsafe` without a comment explaining what invariant it relies on.
- Kernel assembly lives in `global_asm!` blocks in the relevant `.rs` file,
  not in separate `.S` files.
- No comments explaining what code does; only comments explaining *why*
  (hidden constraints, non-obvious invariants, workarounds).
- All system APIs must ship with precise contracts before being called from
  a second crate.
