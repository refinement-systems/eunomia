# Eunomia OS — Development Guide

Full design specification: `doc/spec/0_spec_mvp.md`. Read the spec before
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

### Kani (host chokepoints)

cargo-kani is **pinned at 0.67.0** (CI installs that exact version; upgrades
are deliberate PRs that re-run the suite). Install it via `cargo install
--locked kani-verifier --version 0.67.0` (then `cargo kani setup`): the
crate is **`kani-verifier`**, which ships the `cargo-kani`/`kani` binaries —
there is **no `cargo-kani` crate on crates.io** (installing that name fails
with "could not find cargo-kani in cargo-registry crates-io"). Run from the
repo root — never inside `kernel/`, whose `.cargo/config.toml` forces the
bare-metal target Kani can't drive.

**Kani no longer covers the kernel object core.** The Verus rewrite
(`doc/plans/3_verus-rewrite.md`, phase 2) migrated the `kcore` cspace/CDT
proofs to deductive verification (see **Verus** below), deleting
`kcore/src/proofs` and the off-CI deep-Kani machinery (`scripts/deep-verify.sh`,
`kani-deep.yml`, the `kani_deep`/`kani_contracts` features) it subsumed; `cargo
kani -p kcore` is no longer run. Kani is retained for the host-side §4.7
chokepoints (`urt`, `ipc`, `cas`, `dma-pool`), which keep their own
`#[cfg(kani)]` harnesses until their Verus ports land (plan phase 6). The
historical kcore findings/bounds remain recorded across
`doc/results/2_kani-findings.md` … `8_kani-findings-7.md`.

```sh
cargo kani -p urt -p ipc -p dma-pool                 # urt time/slots, ipc header, dma-pool
cargo kani -p cas -Z stubbing                        # cas superblock (blake3 stubbed)
cargo test -p kcore                                  # kcore host unit tests
```

A harness that does not terminate (CBMC blow-up — e.g. symbolic `u128`
division, a large symbolic free-list, or `Vec`-parsing) must be bounded,
made concrete, or scoped to another tier and documented — never left to hang.

### Verus (`kcore` + scratchpad)

**Pinned at `0.2026.06.07.cd03505`** — installed at `/Users/mjm/inst/verus/`;
`vstd` companion pinned at `=0.0.0-2026-05-31-0205` (in `kcore/Cargo.toml` and
`scratchpad/Cargo.toml`). Verus is unstable software: both the binary and the
`vstd` version must be upgraded together and any upgrade is a deliberate PR.
Code in `verus!{}` blocks erases to plain Rust under a normal `cargo build`/`test`
(the macro drops ghost code), so the aarch64 kernel build and the host crates are
unaffected — confirmed by the kernel cross-build and `cargo test`.

Verus is the **mechanized implementation tier for `kcore`** (plan
`doc/plans/3_verus-rewrite.md`): **unbounded**, terminating, functional proofs on
the real handle/`Store` code — the Store seam carries an abstract ghost view so
the generic `fn op<S: Store>` operations are verified once for all stores. Proven
so far: `untyped::carve`/`carve_place` (totality + placement geometry, phase 0);
the cspace/CDT operation contracts (`cdt_wf`/`refcount_sound` preservation,
monotone derivation ∀ masks, the revoke/delete **termination** Kani could not
give — phase 2). The `verus` CI job runs `cargo verus verify -p kcore` with no
per-proof filter, so a new `verus!{}` obligation auto-gates. `scratchpad` keeps
the toolchain-smoke `spec fn min` example.

```sh
cargo verus verify -p kcore        # the kcore proofs (CI-gated)
cargo verus verify -p scratchpad   # the spec fn min smoke example
```

### TLA+ specs

```sh
# Syntax check
bash tools/tla/tla-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-check.sh tla/cap_revocation/CapRevocation.tla

# Model check (run before M2 and M1 implementations respectively)
bash tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla

# CapRevocation.tla carries a SECOND spec (TSpec) for §3.3 channel
# whole-object teardown; check it with its own config (fast, ~1s):
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla \
  CapRevocation_Teardown.cfg

# IpcReactor.tla — the §3.6 IPC lost-wakeup/backpressure spec (plan
# doc/plans/2_ipc.md §5.1). Unlike the others it carries a liveness property
# (EventuallyDelivered) under weak fairness alongside the safety invariants;
# check before the IPC reactor implementation (~1s):
bash tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla
```

### Fuzzing (cargo-fuzz, host)

Harnesses live in `cas/fuzz`, `storage-server/fuzz`, `loader/fuzz`, `ipc/fuzz`
(each a standalone workspace, excluded from the host workspace). Needs nightly +
`cargo install cargo-fuzz`. See `doc/guidelines/fuzzing.md`; findings in
`doc/results/1_fuzzing-findings.md`. `ipc/fuzz` fuzzes the §3.7 wire codec
(`wire_decode`); its corpus replay is `fuzzing`-gated, so run it with
`cargo test -p ipc --features fuzzing --test fuzz_corpus`.

```sh
scripts/fuzz.sh smoke              # replay committed corpus through every target
scripts/fuzz.sh hunt 300           # time-boxed hunt per target
cargo run -p cas --example gen_cas_corpus   # regenerate that crate's seed corpus
```

The committed corpus is replayed by `cargo test` (`--test fuzz_corpus`); run
that test under Miri with `MIRIFLAGS=-Zmiri-disable-isolation` to UB-check
every seed (the replay reads files, which Miri isolation otherwise blocks).
`cas`/`storage-server`/`ipc` gain a `fuzzing` feature (fuzz-only `fuzz_support`
helpers / `Arbitrary` derives / the `ipc` codec's `DemoMsg`); never enable it in
normal builds.

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

### Rev2: thread reports (§5.1) — done
TCBs carry on-exit/on-fault binding slots (real CDT-visible CapSlots —
notification caps move in via `thread_bind`, revoke sees through them)
and a preallocated terminal report record (running → exited(status) |
faulted(cause, far), one transition ever). `thread_exit(status)` (nr 15,
status now recorded) and `read_report` (nr 22) complete the surface;
`bind-reports`/`read-report` rights bits gate them (creator thread caps
carry both — `Rights::THREAD_ALL`). Thread destruction produces no
report (destruction is the parent acting, not the thread dying). The
CapRevocation TLA+ model covers the binding slots (Bind/ThreadExit/
ThreadFault actions; FireSafe + ReportMonotone properties) — TLC-checked.
The §3.3 channel side rides in the same file as a second spec, `TSpec`
(config `CapRevocation_Teardown.cfg`): channel peer-closed bindings are
refcounted, not CDT-visible (the kernel `bind` bumps the notification's
object refcount and leaves the binder's cap in place, unlike the
move-in TCB slots), so whole-object teardown firing safety is a refcount
discipline — modeled with explicit notification objects. Properties:
ChannelFireSafe (every live channel's peer-closed binding names a live
notification, so teardown fires a live object even after the lineage is
revoked), RefCountSound, ReclaimedReleased — TLC-checked (252 states).
Each spec holds the other's variables constant, so TSpec leaves the
799k-state revocation proof untouched. The kernel side already satisfies
this: `cspace::delete` fires `endpoint_cap_dropped` (peer-closed) before
`obj_unref`, and the binding holds a notification refcount, so a revoke
that tears the whole channel down fires each surviving peer's binding
into a still-live object. The runtime witness is M1 EL0 step 6
(`scripts/m1-test.sh`): a channel carved from a sub-untyped, both ends'
peer-closed bound to a notification funded from a *separate* untyped,
revoke the sub-untyped, assert both bindings fired and the notification
outlived the channel.

Userspace half (the shell's reclaim-on-exit loop) is now done. Two kernel
mechanisms the §5.1 spawn design needed land with it: `retype` can carve a
child-sized **sub-untyped** (`OBJ_UNTYPED`, §2.3 page-aligned sub-range,
phys-read stripped) and `untyped_reset` (nr 23) zeroes a carved untyped's
watermark once `revoke` has emptied it (§2.5). The CapRevocation model
already covers both at its abstraction (sub-untyped carve = a `Copy`-style
CDT-child derivation; reset's precondition *is* the modeled `Retype` guard
`Descendants(c) = {}`), so its invariants are undisturbed. `urt::slots` is
a host-tested cspace-slot free-list; `urt::spawn` owns the canonical loop
(`SpawnRec::arm` binds exit/fault before start; `SpawnRec::reap` does
`read_report` strictly before `revoke`+`reset`, asserted — the report
lives in the TCB the revoke kills). The shell carves one persistent event
notification + one reusable donation untyped from its pool (slots 3/4),
spawns each child as a single CDT subtree under the donation, multiplexes
exit/fault on one notification word (the first real §3.6 bit-group scan),
and recycles its slot window. `bash scripts/spawn-test.sh` is the proof
(same genre as the M1 revocation test): `runloop bin/selftest 100`
(slots 56/56, no leak), exit-status propagation, and the fault demo —
`faulted(translation, 0xdead0000)` then a clean re-spawn — with no
BSS-LEAK (retype re-zeroes reused frames). The shell also grants each
child the **time page** (§2.6): init installs a read-only time-frame cap
in shell cspace slot 5, the shell maps a fresh copy into every child's
aspace and passes the VA in the ST01 block (the init→shell grant, one hop
further), and unmaps it before the reap revoke that frees the child
aspace (§2.5 ordering). `spawn-test.sh` step 6 proves it: `run
bin/selftest 253` reads a sane UTC clock (`time-ok`). Scope cut held:
children get stdin/stdout via the console; no storage-session delegation
(that needs the server to accept a second session, §2.4).

### Rev2: the IPC crate (§3) — done
`doc/plans/2_ipc.md` (six phases) built the userspace IPC crate the MVP
deferred (`0_mvp.md` debt). Verifiable-first: the kernel IPC surface sits
behind a `Transport` seam (`SyscallTransport` in production, the
deterministic in-memory `ModelTransport` for harnesses), so the
cross-process races (lost wakeup, backpressure, cap handoff) run under
**Shuttle** (randomized, at scale) and **Loom** (exhaustive, the
lost-wakeup memory-ordering fragment) over the real reactor code — the
concurrency counterpart to Kani on the kernel core. The `IpcReactor` TLA+
spec (safety invariants + the project's first liveness property,
`EventuallyDelivered`) is the design gate, re-checked in CI's `model`
job. Surface: non-blocking `Endpoint::{send_nb,recv_nb}` (§4.1, null-slot
tolerant); the epoll-shaped `Reactor::{register,wait}` that **hides
notification bits** and owns the bind-poll-wait discipline (§4.2/§3.6);
`send_blocking`/`send_retry` over the writable signal (§4.3); the
`send_acked`/`recv_acked` valuable-cap handshake (§4.4); the
module-private postcard `wire` codec behind the `wire` feature (§4.5,
opt-in so alloc-free binaries stay minimal); and `ipc::session` — the
§4.6 admission layer: `Admission` is the single window-quota admission
point (never over-grants), with the fixed `ConnectReq`/`GrantReply`
codecs and the pure `admit_connect` step. Harnesses #1–#5 (FIFO/no-drop,
lost-wakeup, backpressure, cap-ack, multi-client fairness) live in
`ipc/src/model.rs`; the `concurrency` CI job runs them with no per-test
filter, so a new `loom::model`/`shuttle::check_*` auto-gates. The Shuttle
harnesses run under a **pinned seed** (`check_pinned`, a seeded
`RandomScheduler`) so a CI failure reproduces from source, with a
`shuttle_replay_corpus` landing spot for committing a failing schedule as a
`shuttle::replay` regression (the fuzz-corpus discipline; loom-shuttle §5).
The wire decoder is also a cargo-fuzz target (`ipc/fuzz`). Kani
(`ipc/src/proofs.rs`) verifies the pure codecs and the quota — the `Header`,
the §4.6 session codecs (`ConnectReq`/`GrantReply`), and `Admission`'s
no-over-grant invariant (review rec 4); the reactor's multi-source dispatch is a
recorded caveat (single-source TLA/Loom; multi-bit rests on harness #5 — see
`IpcReactor.tla`). **`storaged`**
(`user/storaged/src/main.rs`) is the first production consumer: its
drain-then-wait loop is now `Reactor::wait` + `Endpoint` over
`SyscallTransport`, dispatching by opaque key — no notification bit named
in the server, so the §3.6 wait-set upgrade will change no server code.
The **shell** (`user/shell/src/main.rs`) is the first *multi-source*
consumer (review rec 2, `doc/results/19_ipc-review.md`): its spawn/reap
loop multiplexes a child's exit and fault terminations through the reactor
via `Reactor::register_bound` — the entry point for **externally-bound,
edge-triggered** sources (a thread on-exit/on-fault `thread_bind`, a
timer, an IRQ), which (unlike the channel `register`) neither binds nor
self-signals a poll-once. Scope cut: the *dynamic* connect (a client
retyping a channel pair and the server accepting a **second** concurrent
session) needs kernel cap-transfer wiring and stays a follow-up; the
admission protocol and reactor multiplexing it relies on are proven
(harness #5).

### M1 exit criterion (met)
Booting prints `123456M1 PASS` (`bash scripts/m1-test.sh` builds the
`m1-test` feature, boots it, and asserts the full marker line): the
embedded EL0 test program (`kernel/src/user.rs`) retypes untyped into
kernel objects, builds a second thread's cspace explicitly, exchanges a
message + derived cap over a channel with notification-driven waiting,
then revokes the parent cap and verifies both the received copy, a queued
in-flight cap, AND the on-exit binding cap in the second thread's TCB
died; a timer object signals a bound notification; the rebound on-exit
binding delivers the child's death notice and read_report returns
exited(42) (§5.1, the thread-report batch); finally it builds a throwaway
channel from a carved sub-untyped, binds both ends' peer-closed events to
a separately-funded notification, and revokes the sub-untyped — the
runtime witness for §3.3 whole-object teardown (every endpoint's
peer-closed binding fires before reclamation; the notification survives;
the dead endpoint caps then error). The embedded user program is an M1
scaffold, replaced by real binaries at M3 — it must not call into kernel
.text (EL0 execute-never), hence `opt-level = 1` for dev and care with
non-`#[inline(always)]` helpers in user.rs.

### Sequencing rules
- **TLA+ `CapRevocation` model must be checked before M1 implementation.**
- **TLA+ `CommitProtocol` model must be checked before M2 implementation.**
- **TLA+ `IpcReactor` model must be checked before the IPC reactor
  implementation** (the §3.6 lost-wakeup/backpressure protocol; plan
  `doc/plans/2_ipc.md` §5.1 — Phase 0 lands the spec, the reactor is phase 2).
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
| Kani | host chokepoints (`urt`, `ipc`, `cas`, `dma-pool`); the `kcore` kernel-core harnesses were migrated to Verus (plan `doc/plans/3_verus-rewrite.md` phase 2) | During kernel development |
| Verus | **mechanized implementation tier for `kcore`** (plan `doc/plans/3_verus-rewrite.md`): unbounded/terminating/functional proofs on the real handle/`Store` code — `untyped::carve` (phase 0), cspace/CDT contracts + termination (phase 2); migrating the rest as phases land. + `scratchpad` smoke | CI `verus` job (`cargo verus verify -p kcore`); during the Verus rewrite |
| Loom / Shuttle | IPC crate, userspace servers | During M1+ development |
| Miri + proptest | everything; chunker + prolly tree esp. | Continuous |
| cargo-fuzz | IPC decoder, postcard payloads | From M1 |

The IPC crate (`ipc/`) is the first serious Loom/Shuttle target (§3.5).

**Deviation from the §6 spec table (`doc/plans/0_kani-rewrite.md`).** The spec
assigned cspace/CDT and the allocator to **Verus** ("written in Verus dialect
from day one"); that did not happen — the kernel predated any verification
tooling. **Kani served as the interim mechanized tier for the kernel
implementation**: the object machinery was extracted into the host-buildable
`kcore` crate and the harness suite (plan §4.1–§4.7) re-checked the
CapRevocation TLA+ invariants on the real code (`cargo kani -p kcore`) —
cspace/CDT, untyped, channels, notifications, thread reports, the §2.4
page-table-walker rewrite, and the §2.5 syscall-decode split — plus the
host-side chokepoints (`urt`, `ipc`, `cas`, `dma-pool`). It found and fixed
real defects (a `carve` overflow DoS; a `PERM_DEVICE | PERM_X` executable-MMIO
encoding). Its shape (explicit `wf()` predicates, the handle/`Store` seam, no
int→ptr in the core) is exactly what the Verus port needed — and
`doc/plans/3_verus-rewrite.md` has now made that port the real thing: as of
phase 2, **Verus is the mechanized kernel-core tier** (the spec's original
assignment), so `cargo kani -p kcore` is retired and the kcore harnesses are
deleted. The historical findings/bounds remain recorded:
`doc/results/2_kani-findings.md` … `8_kani-findings-7.md`.

### Continuous integration

`.github/workflows/ci.yml` runs on every PR and push to main:
- **host-tests** — `cargo test --workspace --exclude kernel` (the kernel is
  bare-metal and can't host-build): the `urt` slot-allocator + heap, the
  monotone rights-mask attenuation (`storage-server` sessions), the CAS
  canonical-form proptests, the wire decoders, the ELF parser, etc.
- **model** — reruns the TLA+ proofs (CapRevocation, its §3.3 teardown
  TSpec, CommitProtocol, and the §3.6 `IpcReactor` lost-wakeup/backpressure
  spec) on Linux. `tools/tla/find-tla-tools.sh` honours a pre-set `JAVA` +
  `TLA_TOOLS`, so CI points it at a downloaded `tla2tools.jar`; locally it
  still finds the macOS Toolbox.
- **on-os** — boots the system under QEMU and runs the §5.1 exit criterion
  (`scripts/spawn-test.sh`: the 100× burn loop, status propagation, the
  wild-pointer fault demo + re-spawn, the panic path, the time grant) plus
  the M1 cap-mechanism EL0 test (`scripts/m1-test.sh`).
- **kani** — `cargo kani -p urt -p ipc -p dma-pool` and `-p cas -Z stubbing`
  (pinned cargo-kani 0.67.0, cached with its CBMC backend): the §4.7 host
  chokepoints. The `kcore` kernel-core leg was migrated to Verus (the `verus`
  job; plan phase 2). No `--harness` filter, so a new harness gates automatically.
- **verus** — `cargo verus verify -p kcore` (pinned Verus `0.2026.06.07.cd03505`,
  release zip cached): the deductive kernel-core proofs (`untyped::carve` +
  cspace/CDT contracts/termination). No per-proof filter, so a new `verus!{}`
  obligation gates automatically.
- **concurrency** — the Loom/Shuttle models under `RUSTFLAGS="--cfg loom"` /
  `"--cfg shuttle"` (plan `doc/plans/1_loom-shuttle-rewrite.md` §6):
  `cargo test -p urt -p ipc --lib`. Loom is the certifying exhaustive proof
  (the `urt::time` seqlock; the `ipc` `ModelTransport` rig), Shuttle the
  randomized breadth-smoke. No per-test filter, so a new `loom::model` /
  `shuttle::check_*` test auto-gates.
- **layering** — greps `kcore/src` for the §2.2 violations CBMC can't model
  (`asm!`/`global_asm!`, `as *mut`/`as *const`); kcore uses `.cast()` for
  every pointer-to-pointer conversion.

`.github/workflows/fuzz.yml` is separate (corpus replay per PR; nightly hunt).

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
