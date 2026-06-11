# Eunomia MVP Retrospective

This document reviews the MVP against `doc/spec/0_spec_mvp.md`: what was
built, what was simplified or not built, where the spec turned out to be
underspecified, and what should come next. Section references (§) are to
the spec.

The headline: **all six milestones (M0–M5) shipped and their exit
criteria are met.** The demo script of §10 runs end to end — QEMU boots
the kernel, init constructs the system from its boot capabilities,
storaged mounts the versioned store over userspace virtio-blk, the shell
loads and spawns a program out of a snapshot, and snapshot / modify /
rollback / delete-snapshot / GC all work from the console. Both TLA+
models were written and model-checked before their implementations, as
the sequencing rules demanded. The system is ~11 kLoC of Rust (kernel
3.3k, storage engine 4.2k, storage server 1.3k, drivers/loader/runtime
~2.2k) plus ~430 lines of TLA+.

---

## 1. What was built, against the spec

| Spec area | Status |
|---|---|
| §1 microkernel object model | ✅ untyped, aspaces (pool-at-creation), threads, channels, cspaces, notifications, timers; IRQ delivery exists for the vtimer |
| §2.1–2.3 caps: CDT revoke, move semantics, monotone derivation | ✅ incl. revocation through in-flight queue slots (M1 exit test witnesses it live) |
| §2.2/2.4 storage caps: sessions, handles, generations, tickets | ✅ host-tested; subtree confinement by unreachability; O(1) mass revocation; one-shot TTL tickets; enumerate-session |
| §2.5 frames, mapping-in-the-cap, DmaPool, `phys-read` | ✅ one DMA driver, PAs only inside dma-pool |
| §2.6 time | ⚠️ CNTVCT readable from EL0; **no PL031 read, no time page** — timestamps are raw ticks (see §3 below) |
| §3 async channels, notifications, bindings, backpressure | ✅ kernel side; ⚠️ the *userspace* async story is thin (see below) |
| §3.7 wire protocol | ✅ fixed header + postcard bodies, strict decoders; version bumped to 2 for M5 ops |
| §4 storage: CDC chunking, BLAKE3, canonical prolly trees, TLV entries, inlining, WAL, A/B commit, recovery | ✅ proptested (canonical form, crash injection) + Miri |
| §4.6 GC + history rewriting (M5) | ✅ mark-and-sweep from committed roots; snapshot deletion with parent re-pointing; retention classes; manual / post-rewrite / watermark triggers; crash-anywhere-in-GC loses nothing (power-cut injection at every write/fsync) |
| §4.7 snapshot rows, server-assigned identity/provenance, tags as pins | ✅ |
| §5 spawn-with-caps, ELF loader, no fork | ✅ explicit cspace construction; startup via bootstrap channel |
| §5.3 faults | ⚠️ faulting threads suspend, never destroyed — but no fault *report* to a parent (no process cap, below) |
| §5.4 scheduling | ✅ 32 fixed priorities, round-robin in level, maximum-controlled-priority ceiling |
| §6 verification | ⚠️ TLA+/TLC ✅, Miri+proptest ✅; **Verus, Kani, Loom/Shuttle, cargo-fuzz did not happen** (see §3) |
| §7 toolchain, mkfs, demo shell | ✅ no LLVM fork; mkfs reuses the engine byte-for-byte |

One deliberate post-spec format change: M5 made the chunk index and
free-extent list durable (**on-disk format v2**, superblock-referenced,
self-verifying). §4.2 always listed the index and free-space accounting
as layout components; the M2 implementation had taken a shortcut
(rebuild-by-scan at mount) that turned out to be structurally
incompatible with reclamation — a scan cannot represent holes, and GC
exists to make holes. §4.2's "index format changes are migrations"
escape hatch was used exactly as intended.

## 2. What could not be demonstrated honestly (and isn't)

Nothing in the demo is faked, but two spec promises are visibly absent
at the console:

- **Bulk data path (§3.1).** The spec calls shared-memory bulk transfer
  "mandatory anyway"; it does not exist. Every byte of file content
  crosses the storage session inside 256-byte channel messages (reads in
  160-byte slices). `run bin/hello` works because the binary is 11 KiB.
  This is the single largest gap between the spec's architecture and the
  running system, and the first thing post-MVP work should fix.
- **Wall-clock time (§2.6).** Snapshot rows carry CNTVCT ticks, not UTC.
  The non-decreasing-per-ref clamp is implemented, so ordering is sound,
  but `snaps` cannot print a date. The time-page design in the spec is
  complete and was simply not reached; nothing about it looks wrong.

## 3. Simplifications and recorded debt

**Userspace async / the IPC crate (§3.5–3.6).** The spec envisioned an
IPC crate owning `FULL` handling, async send/recv, an epoll-shaped
reactor over notification bits, and the valuable-cap ack protocol — the
"first serious Loom/Shuttle target." What exists is ~240 lines of
syscall wrappers; servers are hand-rolled drain-then-wait poll loops
(storaged does use the readable→notification binding correctly). The
single-session MVP never generated the multiplexing pressure that would
have forced the reactor into existence. Consequence: the Loom/Shuttle
verification tier had no target and was not exercised.

**Verification tiers (§6).** The spec's own warning — "Verus cannot be
retrofitted — these components are written in Verus dialect from day
one" — was not heeded: cspace/CDT and the allocator are plain Rust with
asserts. That tier is now a *rewrite*, not an annotation pass, and the
spec was right to predict it. Kani was never set up. Decoders were
written fuzz-shaped (strict, no trailing bytes, no panics on arbitrary
input — and unit-tested for that) but actual cargo-fuzz harnesses don't
exist. The tiers that were applied (TLA+ first; proptest + crash
injection + Miri continuously) carried the project, see §5.

**Process lifecycle (§5.1, §5.3).** Spawn does not return a process cap;
exit and fault reports to the parent do not exist. The demo's `run`
waits for the child on an ordinary channel, and a faulting thread
suspends with a UART diagnostic but nobody is told. Channel peer-closed
events exist, so session *cleanup* has its mechanism, but supervision
does not. (The shell also burns its spawn slots: one `run` per boot.)

**Storage engine (§4.3–4.4).** The WAL is linear, not circular — when
full, everything flushes and the log resets; the flush-the-pinner
scheduler and the staleness timer are unimplemented (with one ref and
one client, every trigger degenerates to "flush the only overlay"
anyway). Mount buffers the whole WAL region for replay (mkfs images use
a 1 MiB WAL accordingly). Flush rebuilds whole dirty files instead of
re-chunking the affected neighborhood. Per-ref overlay quotas under the
global budget exist in API shape only.

**Storage server / sessions.** init wires exactly one session
(shell→storaged) at boot; the §3.5 connect-endpoint protocol and any
second concurrent session have never run on the OS (multi-session
semantics are host-tested). The §5.2 registry protocol is defined but
vacuous, as the spec intended for MVP.

**Driver (§2.5).** virtio-blk polls for completion. The IRQ-to-
notification machinery the spec describes exists and is exercised by the
timer; the driver just doesn't use it yet.

**GC (§4.6).** The server is single-threaded, so the GC cycle is
synchronous within one request — mark and sweep cannot interleave with
mutations. The birth-generation epoch check is implemented as specified,
and the dedup-resurrection guard falls out structurally (sweep removes
index entries before any subsequent put, so a re-put of condemned
content is a miss and rewrites the chunk). The spec's concurrent-GC
machinery therefore sits unexercised until the server grows real
concurrency. The allocator is first-fit over a flat extent list, and the
tail high-water mark never retracts — freed space is fully reusable, but
the occupied region never visibly shrinks.

## 4. Where the spec was underspecified

Decisions the implementation had to invent; recorded so the next
revision of the spec can pin them down.

1. **The durable-index commit story (§4.2).** The spec names the index
   and free-space accounting as layout items but says nothing about how
   they commit. Three sub-problems surfaced:
   - *Self-reference:* the index frame records the free list, but
     placing the frame consumes free space — resolved with an
     upper-bound size estimate plus explicit padding so the frame fills
     its extent exactly.
   - *Reuse timing:* extents freed by a sweep (and superseded index
     frames) must not be reused until the freeing commit's barrier-2
     lands, or a crash plus a dedup index-hit can resurrect overwritten
     bytes. This is the superblock-alternation rule generalized to all
     freed space; the spec states it only for superblocks.
   - *Wedge hazard:* tail-only index placement deadlocks a store whose
     tail is exhausted even when GC has freed plenty (the first
     implementation had exactly this bug; a test caught it).
2. **Rights for maintenance operations.** Who may run `gc`? Read space
   statistics? The §2.3 rights table covers data operations only. Chosen:
   GC and snapshot-row edits require `may-rewrite-history` on a ref-root
   handle; `statfs` needs any live handle. Defensible, but invented.
3. **Deleting a tagged snapshot.** Tags are "keep-strength pins" (§4.7)
   — does deletion fail or cascade to the tag? Chosen: refuse
   (`Pinned`); delete the tag first if you mean it. The alternative is
   quietly authority-expanding.
4. **Deleting a `keep`-class snapshot manually.** Inferred from §4.7
   that classes govern *automatic* pruning only, so an explicit
   `may-rewrite-history` deletion succeeds; the shell's `prune` skips
   `keep` rows. The spec never says this outright.
5. **Watermark re-arm.** "Below ~20% free, schedule GC" thrashes if the
   store is simply *full of live data* — every request would re-trigger
   a futile cycle. Added: re-arm only after the generation advances past
   the last completed GC. Any real implementation needs this rule; the
   spec should state it.
6. **Rollback with a dirty overlay.** Pending writes at rollback time:
   discard, refuse, or flush? Chosen: flush into the abandoned
   pre-rollback root (keeping the WAL coherent), then re-point. The
   abandoned root becomes garbage for the next GC.
7. **WAL edge cases (§4.4).** A record larger than the whole WAL region
   (bypass the log, commit synchronously before acking) and WAL-full
   handling (flush everything, reset) — both unspecified.
8. **The startup block (§5.1).** The named-grant table is specified
   abstractly; the MVP uses ad-hoc per-binary config blocks. Fine until
   the "public ABI" milestone, at which point the table format is
   load-bearing and must be designed for real.

## 5. What worked — process notes worth keeping

- **TLA+ before implementation paid for itself twice.** Once as design
  pressure (queue slots as CDT-visible cap owners came out of making
  revocation checkable *unconditionally*), and once as a test oracle:
  the crash-injection proptest in `cas/src/store.rs` is the model's
  AckedWritesRecoverable invariant transcribed against real bytes. M5
  then shipped a format change and a GC *under* that proptest — the
  invariant held throughout the rework, which is exactly the regression
  story you want from a model.
- **One atomicity mechanism (§4.2) was the right bet.** GC needed zero
  new crash-safety machinery: the sweep is a metadata edit that rides
  the same A/B flip as everything else, and "crash mid-GC loses
  reclamation work, never data" fell out of the architecture rather
  than being engineered.
- **Canonical form is a gift to testing.** "Same contents ⇒ same root
  hash, regardless of edit order" turns deep structural properties into
  one-line proptest assertions, and made dedup/GC interactions (two
  snapshots sharing a root; deleting one) trivially checkable.
- **Host-first storage development (§1's stated goal) worked.** The
  entire engine was built and debugged on macOS against fake and
  crash-injecting block devices; the no_std port for the on-OS build
  came late and cheap. The register-accurate fake virtio device let
  even the driver be host-tested.

## 6. Future work

Roughly ordered by leverage; items marked *(deferred in spec §8)* were
explicitly deferred rather than missed.

1. **Shared-memory bulk path + IRQ-driven virtio.** The mandatory-anyway
   data path (§3.1): frame-cap-backed shared buffers with channel
   messages as doorbells, and the block driver completing on its
   interrupt binding instead of polling. Unblocks real file sizes and
   makes the storage stack's performance story honest.
2. **The real IPC crate.** Async send/recv over the notification
   bindings, the epoll-shaped reactor (so the §3.6 wait-set kernel
   upgrade later changes no server code), the valuable-cap ack protocol
   — then point Loom/Shuttle at it, finally exercising that
   verification tier. This is also the prerequisite for storaged serving
   multiple sessions concurrently on the OS.
3. **Process caps with exit/fault reports (§5.1/§5.3).** The fault path
   already suspends-not-destroys, so this is the delivery protocol plus
   a process-cap object — and it unlocks supervision trees and a
   `wait`-capable shell.
4. **Time page (§2.6).** One PL031 read at boot, one shared read-only
   frame, and snapshot rows gain UTC. Small and overdue.
5. **Verification debt (§6).** Rewrite cspace/CDT and the kernel
   allocator in Verus dialect (the spec's day-one warning, now a real
   rewrite); add cargo-fuzz targets for the wire and on-disk decoders;
   Kani for kernel data-structure invariants.
6. **Storage engine hardening.** Streaming WAL replay; circular WAL with
   flush-the-pinner scheduling and the staleness timer (§4.4);
   neighborhood re-chunking on flush (§4.3); per-ref quotas. On the GC
   side: incremental/persisted marking when mark time grows (§4.6 names
   this as a second TLA+-worthy protocol *(deferred in spec)*), and
   compaction so the tail high-water mark can retract.
7. **A retention daemon (§4.7).** The spec's design — a userspace
   process holding a `may-rewrite-history` handle, expressing
   keep-hourly/daily rules over timestamps and classes — is also the
   natural first *second client* of the storage server, forcing
   multi-session, fairness, and discovery questions that the
   single-client MVP never asked.
8. **Spec-deferred features, in their stated order** *(all §8)*:
   compare-and-set-on-root transactional commits; sturdy refs atop
   tickets; Plan 9-style namespace composition; dynamic service
   registration; the wait-set object; the IO-space object / virtio-iommu
   migration (**must precede the second DMA driver** — the spec's
   scheduling rule stands); demand paging / CoW / page-cache server;
   IDL-based wire encoding + stable syscall ABI as the "non-Rust
   userspace" milestone; symlinks/xattrs; priority donation or MCS
   budgets at SMP time; SMP/PSCI and real hardware (which also calls in
   the logged cache-maintenance debt).

The MVP's bet — that capability discipline, canonical storage, and
selective formal methods could be carried from a design document to a
running system without the design buckling — held. The places where
reality pushed back (durable index, watermark thrash, the bulk path's
absence) are now recorded above, which is what a spec's first contact
with implementation is for.
