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

### TLA+ specs

```sh
# Syntax check
bash tools/tla/tla-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-check.sh tla/cap_revocation/CapRevocation.tla

# Model check (run before M2 and M1 implementations respectively)
bash tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla
```

### Fuzzing (cargo-fuzz, host)

Harnesses live in `cas/fuzz`, `storage-server/fuzz`, `loader/fuzz` (each a
standalone workspace, excluded from the host workspace). Needs nightly +
`cargo install cargo-fuzz`. See `doc/guidelines/fuzzing.md`; findings in
`doc/results/1_fuzzing-findings.md`.

```sh
scripts/fuzz.sh smoke              # replay committed corpus through every target
scripts/fuzz.sh hunt 300           # time-boxed hunt per target
cargo run -p cas --example gen_cas_corpus   # regenerate that crate's seed corpus
```

The committed corpus is replayed by `cargo test` (`--test fuzz_corpus`); run
that test under Miri with `MIRIFLAGS=-Zmiri-disable-isolation` to UB-check
every seed (the replay reads files, which Miri isolation otherwise blocks).
`cas`/`storage-server` gain a `fuzzing` feature (fuzz-only `fuzz_support`
helpers / `Arbitrary` derives); never enable it in normal builds.

---

## Milestones and current status

| Milestone | Status | Key deliverable |
|-----------|--------|-----------------|
| **M0** | ✅ Done | Boot, UART, MMU, exception handling |
| **M1** | ✅ Done | Caps + threads + async channels; CDT revoke |
| **M2** | ✅ Done | virtio-blk; CAS + prolly tree; session protocol; mkfs |
| **M3** | ✅ Done | ELF loader; spawn-with-caps; shell |
| **M4** | ✅ Done | Snapshot / rollback demo (MVP) |
| **M5** | ✅ Done | GC + history rewriting |

Both TLA+ models (CapRevocation, CommitProtocol) are complete and
TLC-checked — the M1/M2 formal gates are cleared. The `cas` crate's
chunker + prolly tree + canonical-form proptest suite passes (incl. Miri).

### M2 progress
Done (host-side): the full storage engine in `cas` (`dev.rs` block devices
incl. crash-injection, `disk.rs` on-disk formats, `overlay.rs` memtable,
`store.rs` WAL/flush/A-B-commit/recovery — crash-injection proptest mirrors
the TLA+ AckedWritesRecoverable invariant); `mkfs` builds bootable images
from a host tree (integration-tested); `storage-server/src/lib.rs` has the
transport-agnostic session/handle/ticket layer (7 semantics tests).
dma-pool + virtio-blk are done and host-integrated (the cas engine runs
over the driver over a register-accurate fake device in tests).

### M3 progress
Done: kernel address spaces (aspace.rs, ASID-tagged TTBR0 switching,
shared kernel L1 entries), frame caps with mapping-in-the-cap (§2.5),
map/frame_write/thread_start_as syscalls; `ipc::sys` syscall wrappers;
`loader` is a lib (host-tested ELF64 parser + spawn). Real userspace
binaries live under `user/` (own mini-workspaces, built by
kernel/build.rs into `target/user`, embedded with include_bytes!). The
default boot loads init as a real process; `cargo build --features
m1-test` boots the M1 exit test instead. QEMU prints "M3 SPAWN PASS".
Userspace linker scripts must keep each permission class page-aligned
(one PT_LOAD per class — the loader maps per segment).

### M4: the MVP demo (done)
`bash scripts/run-demo.sh` builds everything, assembles a demo image with
mkfs, and boots the full system: init spawns storaged (virtio-blk over
the MMIO window + DMA region it grants, postcard session protocol,
blocks on a readable→notification binding) and the shell
(ls/cat/write/rm/snap/snaps/rollback/sync/run). `run bin/hello` loads an
ELF from the versioned store and spawns it with an explicit cspace.
cas/storage-server/virtio-blk are no_std+alloc (`urt` provides the
userspace heap). Remaining debt: streaming WAL replay (mount buffers the
whole WAL region — mkfs images use a 1 MiB WAL), IRQ-driven virtio
completion (driver polls), bulk data path (reads are message-bounded).

### M5: GC + history rewriting (done)
On-disk format v2: the superblock references a durable chunk index
(hash → offset/len/birth-generation + free-extent list) written as a
self-verifying frame — mount no longer scans, and the sweep is a pure
metadata edit through the normal A/B flip (crash mid-GC recovers the
previous commit; crash loop + proptest in `cas/src/store.rs`, mark walk
in `cas/src/gc.rs`). Freed extents become allocatable only after the
flip lands. History rewriting: `DeleteSnapshot` (re-points parents;
tagged snapshots refuse deletion), `SetClass`, `Gc`, `Statfs` wire ops
gated on `may-rewrite-history`; post-rewrite trigger + crude 20%-free
watermark arm a GC that storaged drains after replying. Shell built-ins:
`snapdel keep prune gc df` (retention policy is shell-side; `snap` now
takes class `auto`). Remaining debt: the tail high-water mark never
retracts (freed space is reused, the region never visibly shrinks);
first-fit allocator; no concurrent GC (the server is single-threaded, so
mark/sweep run inside one request — the §4.6 incremental machinery
stays deferred).

All MVP milestones (M0–M5) are complete.

### Rev2: the time page (§2.6) — done
Init reads the PL031 once at boot (new kernel boot caps: slot 4 = RTC
frame, slot 5 = init's own aspace), publishes `(seq, wall_base_ns,
cntvct_base, cntfrq)` in a read-only frame funded from its untyped, and
maps it into storaged and the shell (the address travels in the startup
blocks: `SD02`/`SH01`). `urt::time` owns the page ABI, the seqlock
reader (seq is constant zero today; the retry path is host-tested with a
tearing writer thread, incl. under Miri), and the overflow-safe tick→ns
conversion (proptested — the naive `Δ·10⁹` u64 form overflows ~5 min
into uptime at 62.5 MHz). storaged stamps snapshots/mtimes/ticket-TTLs
with UTC ns; the on-disk format is v3 and pre-v3 images are refused with
a distinct version error (`StoreError::UnsupportedVersion`), never
reinterpreted — re-create them with mkfs. Snapshot timestamps are
clamped per-ref strictly monotone (`max(now, predecessor+1)`, §4.7).
QEMU invocations pin `-rtc base=utc,clock=host`. End-to-end proof:
`bash scripts/boot-test.sh` boots the demo, takes two snapshots, and
asserts sane, strictly ordered ISO-8601 timestamps plus a zero-syscall
shell `date`.

### M1 exit criterion (met)
Booting prints `12345M1 PASS`: the embedded EL0 test program
(`kernel/src/user.rs`) retypes untyped into kernel objects, builds a second
thread's cspace explicitly, exchanges a message + derived cap over a
channel with notification-driven waiting, then revokes the parent cap and
verifies both the received copy, a queued in-flight cap, AND the on-exit
binding cap in the second thread's TCB died; a timer object signals a
bound notification; the rebound on-exit binding delivers the child's
death notice and read_report returns exited(42) (§5.1, the thread-report
batch). The embedded user program is an M1
scaffold, replaced by real binaries at M3 — it must not call into kernel
.text (EL0 execute-never), hence `opt-level = 1` for dev and care with
non-`#[inline(always)]` helpers in user.rs.

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
| `main.rs` | Entry point (`kernel_main`), boot caps, first eret, panic handler |
| `boot.rs` | `_start` assembly: core selection, SP_EL1, BSS zero, → kernel_main |
| `uart.rs` | PL011 UART driver (MMIO at 0x0900_0000); `core::fmt::Write` impl |
| `exceptions.rs` | Vector table; EL0 trap-frame save/restore; EL1 = fatal |
| `mmu.rs` | Identity map: 2 MiB L2 blocks for DRAM, EL0 window at 0x4800_0000 |
| `cspace.rs` | Cap slots, CDT (parent/child/sibling), derive/delete/revoke/move |
| `untyped.rs` | Untyped caps (region+watermark inline), retype, reset |
| `thread.rs` | TCB, TrapFrame layout, ready queues, `maybe_switch` |
| `channel.rs` | Two-ring channels, CDT-visible queue cap slots, event bindings |
| `notification.rs` | Signal word + FIFO waiter queue |
| `timer.rs` | Generic-timer tick (100 Hz), timer objects, CNTVCT helpers |
| `gic.rs` | GICv3 minimal bring-up (vtimer PPI 27), ack/eoi |
| `syscall.rs` | SVC dispatch (x7 = nr); M1 scaffold ABI, not stable |
| `user.rs` | Embedded EL0 test program (M1 exit criterion; removed at M3) |

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
