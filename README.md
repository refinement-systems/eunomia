# Eunomia OS

An experimental operating system built around three commitments:

1. **Capability-based access to everything.** No ambient authority: a
   process can touch exactly what is in its capability space plus its
   storage sessions — nothing else. Authority only ever shrinks as it is
   delegated, so "what can this process reach?" has a complete answer.
2. **Deduplicated, versioned storage as the filesystem.** Content-addressed
   chunks under canonical (history-independent) prolly trees. Snapshots,
   rollback, history rewriting, and garbage collection are first-class,
   cheap operations — `git`-like semantics at the filesystem layer.
3. **Verification where it pays.** The commit protocol and capability
   revocation were modeled in TLA+ and model-checked *before*
   implementation; the storage engine is hammered with crash-injection
   proptests and Miri; decoders are fuzz-shaped and strict.

The system is an seL4-style microkernel for AArch64 (QEMU `virt`): the
kernel knows about untyped memory, address spaces, threads, async IPC
channels, notifications, and capability spaces with a derivation tree.
Everything else — the storage server, the virtio-blk driver, the ELF
loader, the shell — is unprivileged userspace Rust holding capabilities.
The full design rationale lives in [`doc/spec/0_spec_mvp.md`](doc/spec/0_spec_mvp.md);
the post-MVP retrospective in [`doc/retrospective/0_mvp.md`](doc/retrospective/0_mvp.md).

All MVP milestones (M0–M5) are complete: boot → capabilities/IPC →
storage stack → real processes → snapshot/rollback demo → GC + history
rewriting.

## Prerequisites

- **Rust nightly** with the `rust-src` component (the kernel
  cross-compiles with `-Zbuild-std` for `aarch64-unknown-none-softfloat`):
  `rustup default nightly && rustup component add rust-src`
- **QEMU** ≥ 7.x with `qemu-system-aarch64`
- (optional) **Java** for the TLA+ model checker, **`cargo +nightly miri`**
  for the UB checks

Everything runs on a stock macOS or Linux host; no cross-toolchain or
LLVM fork is needed.

## Running the demo

```sh
bash scripts/run-demo.sh
```

This builds the host tools and the kernel (which embeds the userspace
binaries), assembles a 64 MiB versioned disk image with `mkfs`, and boots
the full system in QEMU. You land in a shell served by the storage
server over IPC:

```
Eunomia shell - type help
eunomia> help
ls cat write rm sync run
snap snaps rollback snapdel keep prune gc df help
```

Exit QEMU with `Ctrl-A x`. The script is interactive by default; pipe
commands on stdin for scripted runs.

Plain `cd kernel && cargo run` boots the kernel without a disk;
`cargo build --features m1-test` (in `kernel/`) boots the M1 capability
exit test instead of init (see below).

## Demo tour

### The versioned filesystem

Every file lives in a content-addressed store under a canonical prolly
tree. The `main` ref is your mutable branch; snapshots are immutable,
numbered rows pinning old roots.

```
eunomia> cat hello.txt
Hello from the versioned store!
eunomia> snap before-my-edits          # snapshot the ref -> #2
snapshot #2
eunomia> write hello.txt something-else
ok
eunomia> cat hello.txt
something-else
eunomia> rollback 2                    # ref head -> snapshot 2's root
ok
eunomia> cat hello.txt
Hello from the versioned store!
```

Rollback is a ref-table edit — O(1) regardless of how much changed,
because the old tree was never destroyed; the head just points at it
again.

History rewriting and reclamation (M5):

```
eunomia> snaps
#1  keep [mkfs] initial image
#2  auto [session=1] before-my-edits
eunomia> df
chunk region: 14681 used / 66037415 free of 66052096 bytes
eunomia> snapdel 2                     # drop one snapshot (a row edit)
ok
eunomia> gc                            # mark-and-sweep from live roots
gc: freed 9 objects / 1640 bytes, 8 live
eunomia> df
chunk region: 13046 used / 66039050 free of 66052096 bytes
```

Deleting a snapshot is a tiny metadata edit; the newly unreachable mass
is reclaimed by GC, which the server also triggers itself after any
history-rewriting operation (watch for `[storaged] gc: ...` lines) and
when free space crosses a watermark. `prune <n>` applies a shell-side
retention policy: keep the newest *n* `auto` snapshots; `keep <id>`
promotes a snapshot out of `prune`'s reach. A crash at *any* point
inside GC recovers the previous commit with nothing lost — reclamation
work is the only thing a crash can forfeit (this is tested by power-cut
injection at every write/fsync inside the GC cycle).

Programs are data in the same store, versioned like everything else:

```
eunomia> run bin/hello                 # load an ELF out of the store, spawn it
loaded 11312 bytes from the store
[hello] child alive in its own aspace
child replied: hello-ok
```

### Capabilities

`run` is itself the kernel-capability demo: the shell retypes a piece of
its own untyped memory into a channel, constructs the child's capability
space explicitly (the child gets the bootstrap channel and nothing
else), and starts the thread. There is no fork, no inherited file
descriptors, no ambient namespace — a process's world is exactly what
its parent put in its cspace.

Storage authority works the same way at a different layer. The shell
holds *handle 0*: a session-relative handle to the root of the `main`
ref with full rights. Handles attenuate monotonically (subtree + rights
mask, e.g. read-only on `/pub` — the holder physically cannot *name*
anything outside that subtree), can be passed to children at spawn, and
are revocable en masse in O(1) by bumping the ref's generation.

The fine-grained capability mechanics are exercised by the test suites
rather than shell built-ins:

- `cargo test -p storage-server` — handle relativity (the same integer
  means nothing in another session), subtree confinement by
  unreachability, monotone attenuation, O(1) mass revocation with lazy
  staleness, one-shot claim tickets with TTL, session audit/cleanup, and
  the `may-rewrite-history` gate on snapshot deletion and GC.
- `cd kernel && cargo build --features m1-test && cargo run` — boots an
  EL0 program that retypes untyped into kernel objects, builds a second
  thread's cspace, sends a capability through a channel, then **revokes
  the parent capability and verifies that the copy queued inside an
  in-flight message died with it** (prints `M1 PASS`). Revocation seeing
  through message queues is checked unconditionally in the TLA+ model
  and witnessed live here.

## Testing and verification

```sh
cargo test -p cas                  # storage engine: canonical-form proptests,
                                   #   crash-injection (power cut + torn writes),
                                   #   GC reclamation/pinning/crash suite
cargo test -p storage-server       # session/handle/capability semantics
cargo test -p mkfs                 # image build + remount integration
cargo test --workspace             # everything host-testable
cargo +nightly miri test -p cas    # UB check (slow)

bash tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla
```

The two TLA+ models cover the system's highest-value protocols: the
A/B-superblock commit/recovery protocol (including partial flushes —
the crash-injection proptest in `cas/src/store.rs` mirrors its headline
invariant against real bytes) and capability revocation including caps
queued in in-flight messages.

## Repository layout

```
kernel/          AArch64 bare-metal microkernel (boot, MMU, cspaces/CDT,
                 threads, channels, notifications, timers, syscalls)
ipc/             Syscall wrappers shared by userspace
cas/             The storage engine: chunker, prolly trees, WAL,
                 A/B commit, crash recovery, GC (host-testable, no_std)
storage-server/  Sessions, handles, tickets, wire protocol (no_std lib)
virtio-blk/      Userspace virtio-mmio block driver
dma-pool/        The only place physical addresses exist
loader/          ELF64 parser + spawn-with-caps
user/            On-OS binaries: init, storaged, shell, hello
mkfs/            Host tool: build a bootable versioned disk image
shell/           Host-side placeholder (the real shell is user/shell)
tla/             TLA+ models; tools/tla/ has the check scripts
doc/spec/        The design document — read this first
doc/retrospective/  What happened when the spec met reality
scripts/         run-demo.sh
```
