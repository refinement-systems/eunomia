# Eunomia OS

This work is licensed under a [CC0 1.0 Universal](https://creativecommons.org/publicdomain/zero/1.0) license.

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

All MVP milestones (M0–M5) are complete: boot → capabilities/IPC →
storage stack → real processes → snapshot/rollback demo → GC + history
rewriting.

## Prerequisites

- **Rust nightly** with the `rust-src` component (the kernel
  cross-compiles with `-Zbuild-std` for `aarch64-unknown-none-softfloat`):
  `rustup default nightly && rustup component add rust-src`
- **QEMU** ≥ 7.x with `qemu-system-aarch64`
- (optional) **Java** for the TLA+ model checker, **`cargo +nightly miri`**
  for the UB checks, and **Verus** (pinned binary `0.2026.06.07.cd03505`
  with Rust toolchain `1.95.0`) for the deductive-verification gate —
  unzip the matching release and put its `cargo-verus` on `PATH`

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
cargo test --workspace --exclude kernel   # everything host-testable
cargo +nightly miri test -p cas    # UB check (slow)

bash tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla
```

The two TLA+ models cover the system's highest-value protocols: the
A/B-superblock commit/recovery protocol (including partial flushes —
the crash-injection proptest in `cas/src/store.rs` mirrors its headline
invariant against real bytes) and capability revocation including caps
queued in in-flight messages.

### Deductive verification (Verus)

The `kcore` object model and the host chokepoints (`ipc`, `urt`,
`freelist`, `dma-pool`, and the `cas` superblock codecs) are proven with
Verus for all inputs. Verus ships no crates.io binary; install the pinned
release (see Prerequisites) and check it matches before trusting a result:

```sh
verus --version   # Version: 0.2026.06.07.cd03505, Toolchain: 1.95.0-...
```

Run the full gate from a clean build — `cargo clean` first so nothing is
served from stale verification cache, then one `cargo verus verify` per
crate (this is exactly what CI does):

```sh
cargo clean
cargo verus verify -p kcore
cargo verus verify -p ipc
cargo verus verify -p urt
cargo verus verify -p freelist
cargo verus verify -p dma-pool
cargo verus verify -p cas --no-default-features   # feature-agnostic codecs
```

Verus caches verification per build, so a re-run over an unchanged
`target/` can exit 0 **without re-verifying** — a stale-cache false green.
A real run prints a `verification results:: N verified, 0 errors` line per
crate; if that line is missing, the result came from cache. Before
re-verifying, clean the relevant crate first (`cargo clean -p kcore`), or
`cargo clean` for the whole gate. See `doc/guidelines/verus.md` for the
full discipline and `doc/guidelines/verus_trusted-base.md` for the
trusted base and expected per-crate counts.

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
scripts/         run-demo.sh
```

## Licensing

Project-owned code in this repository is licensed under the 0BSD license; see
[`LICENSE.0BSD`](LICENSE.0BSD) for the full text. Project-owned Markdown
documentation is licensed under the CC0 1.0 Universal public-domain dedication;
see [`LICENSE.CC0`](LICENSE.CC0) for the full text.

Third-party dependencies and vendored external code retain their own licenses.
The complete combined build is therefore not simply 0BSD: the 0BSD license
applies only to this project's own code, and dependency or vendored components
remain governed by their respective license terms. Eunomia-owned crates reported
as `N/A` by `cargo license` are covered by the project-code statement above, not
listed as third-party dependencies here.

### Third-party Rust dependency licenses

| License expression | Crates |
| --- | --- |
| `(Apache-2.0 OR MIT) AND Unicode-3.0` | `unicode-ident` |
| `Apache-2.0` | `shuttle` |
| `Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR CC0-1.0` | `blake3` |
| `Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT` | `linux-raw-sys`, `rustix`, `wasi`, `wasip2`, `wasip3`, `wasm-encoder`, `wasm-metadata`, `wasmparser`, `wit-bindgen`, `wit-bindgen-core`, `wit-bindgen-rust`, `wit-bindgen-rust-macro`, `wit-component`, `wit-parser` |
| `Apache-2.0 OR BSD-2-Clause OR MIT` | `zerocopy`, `zerocopy-derive` |
| `Apache-2.0 OR CC0-1.0 OR MIT-0` | `constant_time_eq` |
| `Apache-2.0 OR LGPL-2.1-or-later OR MIT` | `r-efi` |
| `Apache-2.0 OR MIT` | `anyhow`, `arrayvec`, `autocfg`, `bit-set`, `bit-vec`, `bitflags`, `cc`, `cfg-if`, `cobs`, `cpufeatures`, `embedded-io`, `equivalent`, `errno`, `fastrand`, `find-msvc-tools`, `fnv`, `generator`, `getrandom`, `hashbrown`, `heck`, `hex`, `id-arena`, `indexmap`, `itoa`, `lazy_static`, `leb128fmt`, `libc`, `log`, `num-traits`, `once_cell`, `pin-project-lite`, `postcard`, `ppv-lite86`, `prettyplease`, `proc-macro2`, `proptest`, `quick-error`, `quote`, `rand`, `rand_chacha`, `rand_core`, `rand_pcg`, `rand_xorshift`, `regex-automata`, `regex-syntax`, `rustversion`, `rusty-fork`, `scoped-tls`, `semver`, `serde`, `serde_core`, `serde_derive`, `serde_json`, `shlex`, `smallvec`, `syn`, `tempfile`, `thiserror`, `thiserror-impl`, `thread_local`, `unarray`, `unicode-xid`, `verus_prettyplease`, `verus_syn`, `wait-timeout`, `windows-link`, `windows-result`, `windows-sys` |
| `BSD-2-Clause` | `arrayref` |
| `MIT` | `assoc`, `bitvec`, `funty`, `loom`, `matchers`, `nu-ansi-term`, `owo-colors`, `radium`, `sharded-slab`, `synstructure`, `tap`, `tracing`, `tracing-core`, `tracing-log`, `tracing-subscriber`, `valuable`, `verus_builtin`, `verus_builtin_macros`, `verus_state_machines_macros`, `vstd`, `wyz`, `zmij` |
| `MIT OR Apache-2.0` | `arbitrary`, `derive_arbitrary`, `jobserver` |
| `(MIT OR Apache-2.0) AND NCSA` | `libfuzzer-sys` |
| `MIT OR Unlicense` | `aho-corasick`, `memchr` |
| `Zlib` | `foldhash` |
