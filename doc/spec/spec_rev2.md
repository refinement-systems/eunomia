# Eunomia OS — Design Document (rev2)

Eunomia is an experimental operating system built around three commitments:

1. **Capability-based access to all resources.** No ambient authority; a process's reach is exactly the contents of its capability space plus its storage sessions.
2. **Deduplicated, versioned storage.** Content-addressed chunks under a canonical prolly tree, with snapshots, rollback, and history rewriting as first-class operations.
3. **Tiered verification.** No fully verified stack is attempted; instead a tiered policy applies the strongest affordable tool to each component, with the highest-value protocols modeled before implementation.

**Implementation language:** Rust and assembly.
**Development environment:** macOS on Apple Silicon (M1).
**Target platform:** virtualized ARM64 (QEMU `virt` machine).

**Terminology.** A *capability* ("cap") is transferable, unforgeable authority. Kernel caps are slots in capability spaces (§2). Storage caps are *handles* in per-session tables held by the storage server (§2.4); a phrase like "a snapshot cap" means "a handle denoting a snapshot." Raw content hashes are never a form of authority.

---

## 1. Architecture

Eunomia is an seL4-style microkernel. The kernel provides a minimal set of object types:

- **Untyped memory** — regions of physical memory that can be *retyped* into other kernel objects or into frames. Retyping is the sole means of creating kernel objects, so all kernel memory is accounted to whoever donated the untyped.
- **Address-space objects** — page-table trees, created from donated untyped, into which frames are mapped (§2.5).
- **Threads** — schedulable execution contexts, each bound to an address space and a capability space.
- **IPC channels** — asynchronous message endpoints (§3).
- **Capability spaces (cspaces)** — per-process tables of capability slots.
- **IRQ handlers** — caps granting the right to receive and acknowledge an interrupt.
- **Notifications** — a machine word of signal bits plus a waiter queue; the event-delivery primitive (§3.6).
- **Timers** — caps to program a deadline that signals a bound notification, backed by the ARM generic timer.

Everything else is a userspace server holding caps: the storage server (which owns the virtio-blk cap), the program loader, the shell, and any drivers. Keeping the object set small is what makes the verification strategy (§6) tractable, and it places the most complex component — the storage stack — in ordinary userspace Rust that can be developed and tested on the macOS host (with Miri, proptest, Loom, and Shuttle), decoupled from the kernel.

At boot the kernel constructs one process, **init**, whose cspace holds all unallocated untyped memory and all device resources (MMIO frames, IRQ caps). Every grant in the running system derives from init; there is no other source of authority.

**Design influences:** seL4 (capability mechanics, untyped retype model), Zircon (asynchronous channel IPC, object model), KeyKOS/EROS (generation-based revocation, adapted for storage caps), git/Dolt/Noms (prolly trees, content addressing), ZFS (birth-time reclamation, end-to-end checksumming), and E/CapTP/Cap'n Proto (the live-ref vs. sturdy-ref distinction behind the session/handle model).

---

## 2. Capabilities

### 2.1 Addressing

Kernel capabilities are addressed as indices into a per-process cspace. Caps are transferred over IPC channels (§3.4) and inherited at spawn time, when a parent constructs the child's initial cspace explicitly. There is no other way to acquire kernel authority.

### 2.2 Revocation

Two revocation regimes, matched to where each kind of cap lives.

**Kernel caps** (memory, threads, channels, cspaces, IRQs) use an seL4-style **capability derivation tree (CDT)**: every copy, mint, or derivation records a parent–child edge, threaded through the cap slots themselves. Revoking a cap eagerly deletes all of its descendants; because that walk is unbounded, it is preemptible and restartable. Restartable is surfaced at the syscall boundary (§2.7) as a bounded per-call quantum returning a retry status until the subtree is empty; a revoke-in-progress marker on the root refuses derivation into the subtree, so the walk terminates even under concurrent derivation. Revocation is what makes retyping sound: untyped memory may be retyped only once the kernel can establish that no outstanding cap references the region, and revoke is how exclusivity is proven.

The kernel operationalizes "no outstanding cap references the region" structurally, as *the untyped has no immediate CDT child*. That this implies the real property — every cap into the carved physical region is a CDT descendant of the untyped — holds by construction: the only path that creates a frame records the untyped as the new cap's CDT parent. This is a trusted-base bridge rather than a stored invariant, because the kernel's object seam carries no physical-memory model (§6.1).

Capabilities queued in in-flight messages occupy real, CDT-visible slots owned by the channel (§3.4), so revocation reaches caps in flight: the descendant-deletion guarantee has no "except messages in flight" exception. (Whether the *revoked cap itself* survives the walk is a separate question, treated under reclamation in §2.5.) Kernel caps never touch disk, so the CDT's pointer structure poses no persistence problem.

**Storage caps** (handles held against the storage server) use EROS-style generation versioning plus session scoping:

- **Mass revocation** of a ref is O(1) regardless of how many handles exist. Each ref carries a **generation counter**; every handle records the generation at grant time; bumping the counter (on revoke-all or ref destruction) invalidates every outstanding handle on next use. The counter is plain data and persists through the normal commit path.
- **Per-grantee revocation** is first-class: delete one handle, or destroy one session, and exactly that grantee's access ends (§2.4). The session is the interposition point.
- **Snapshot handles** denote immutable data. Revoking one ends future access but cannot reclaim what was already read; holding a snapshot handle is equivalent to holding a copy of whatever was read through it. This is by design.

For kernel caps, selective revocation finer than the CDT provides is a userspace pattern: interpose a forwarding process at grant time and revoke by destroying it. Such revocability must be anticipated when authority is granted.

### 2.3 Attenuation and the derivation lattice

All derivations are **monotone**: authority can only shrink, never grow. This is what makes "what can this process touch?" answerable by inspecting its cspace and enumerating its storage sessions (§2.4).

| Cap kind | Derivations allowed |
|---|---|
| Untyped / memory | page-aligned sub-range; rights fixed to `read`/`write` |
| Channel, thread, cspace | rights mask |
| Storage snapshot handle | subtree |
| Storage ref handle | subtree + rights mask |

**Untyped rights are not deriver-chosen.** Ordinary derivation refuses untyped caps; the only way to subdivide an untyped is to retype it, which pins the child's rights to its parent's `read`/`write` bits and clears `phys-read`. Physical-address authority therefore never flows down an ordinary derivation chain. Because `read` and `write` are the only meaningful rights on an untyped, a caller-supplied mask would have nothing to attenuate, so none is offered. Page-aligned range containment is enforced on every carve.

**Thread rights:** `bind-reports` (configure the on-exit/on-fault slots, §5.1), `read-report` (read the terminal record), and `manage` (suspend). A thread cap also carries a **maximum-controlled-priority** ceiling — a value, not a bit — that attenuates monotonically: a derived thread cap's ceiling is the minimum of the parent's ceiling and any lower ceiling the deriver requests, so a supervisor can hand out a thread cap with a strictly lower ceiling. Destruction is deliberately not a thread right: a thread is destroyed by destroying the resources that fund it (§2.2), so a supervisor holding an attenuated thread cap can observe and suspend a thread, while only the party that funds the thread's memory can destroy it. Handing a child's main-thread cap to a third party, attenuated as desired, is the supervision grant — revocable like any other cap.

**Subtree caps.** A handle rooted at a directory denotes an interior node of the prolly tree (internally a hash, externally always a handle). Because the wire protocol is handle-relative (§2.4), the holder cannot name anything outside the subtree — confinement by unreachability rather than by checked policy. Subtree handles on refs are resolved server-side, with commits merged upward. This subsumes most uses of chroot, jails, and bind-mounts.

**Ref rights:** `read`, `write`, `may-snapshot`, `may-rewrite-history`, `stat-store`. Snapshot and history-rewrite are separate bits because history rewriting is destructive. `stat-store` gates **store-global observation** — `statfs(handle)` today, and any future global observable (GC counters, index occupancy, compaction statistics) — and is the one right whose meaning ignores the subtree its handle denotes: a handle attenuated to a single directory but carrying `stat-store` still observes the whole store's space accounting. It strips, enumerates, and dies with a generation bump like any other right, but its *scope* does not shrink with the subtree. The default posture is deny: delegation helpers strip it, init grants it only to the shell and to maintenance holders, and `statfs` without it is refused. "How much space may I use?" is otherwise a policy question, answered by a process's parent (§5.2), not by reading the store's counters.

**Limits of confinement.** Confinement by unreachability is a property of *naming*; it does not extend to what sharing one physical store makes observable. The residual channels are: free-space accounting is a covert channel between any two clients that can read it (bandwidth bounded by commit rate); dedup is an existence oracle (write content C, `sync`, and compare free space before and after — no drop means C was already stored); and timing leaks survive any rights regime (ENOSPC arrival correlates clients, one client's GC is every client's latency spike until GC is incremental, and allocator/WAL contention is measurable in principle). `stat-store` removes byte-precise observation — without readable counters the dedup oracle degrades to timing inference against a flush path dominated by two fsync barriers — but only disjoint stores make confined clients mutually unobservable.

**Byte-range caps within files are excluded.** (For memory caps, the MMU already provides page-granular ranges in hardware.) Chunk boundaries give no structural help for arbitrary byte ranges, truncation semantics have no clean answer, and the use cases are thin. A program that wants to share a file header copies it into a fresh object.

### 2.4 Storage capabilities at the boundary: sessions, handles, and tickets

A storage cap at the IPC boundary is a small integer **handle**, meaningful only relative to the session channel it arrived on — exactly a file descriptor. Per session, the server keeps a table:

```
handle → (kind: snapshot | ref, target, subtree root, rights, generation-at-grant)
```

Unforgeability comes from the kernel guaranteeing channel identity: move semantics (§3.4) mean a channel cap has exactly one holder. The integers themselves carry no authority, so leaking them is harmless. The kernel knows nothing of storage caps; the handle table is plain Rust, host-testable, and adds no kernel surface.

**The wire protocol is handle-relative.** Operations take the form `read(handle, path, range)`, `open_child(handle, name) → handle`, `write(handle, path, offset, …)`, `close(handle)`, and so on. **Raw hashes never appear as request parameters**: hashes are internal addresses and integrity proofs, not authority, and knowing a root hash (from a log line, an audit trail, a ref listing) confers nothing.

**Delegation along spawn.** The parent funds a fresh channel pair (§3.5) and asks the server, over its own session, to open a new session on the offered endpoint, pre-populated with specified handles and attenuated in the same request (sub-subtree, reduced rights); it then gives the retained endpoint to the child. One round trip, and the funding rule is uniform: the child's session costs the parent's memory, never the server's.

**Peer-to-peer transfer.** A holder asks the server to mint a **claim ticket** for a handle: a one-shot token whose time-to-live (TTL) the caller requests but the server clamps to a server-imposed maximum, so no ticket can outlive that bound. The holder sends the ticket bytes to a peer, who redeems it on its own session, materializing the handle (with its recorded attenuation and generation) in its table. The ticket is the only bearer-token mechanism in the system, and deliberately narrow — one-shot redemption plus the server-bounded expiry bound the exposure window, and the durable representation of authority never leaves the handle/session regime.

**Audit.** `enumerate-session` is a first-class right, letting a supervisor dump exactly what a session can touch.

**Cleanup.** When a client dies, its client-funded session channel (§3.5) is destroyed with it; teardown signals the server's peer-closed binding (§3.3), and the server drops the session table and revokes the session's bulk windows (§2.5), reclaiming the memory. There is no leaked server state and no finalizer protocol.

### 2.5 Memory: frames, mappings, and DMA

**Frames and address spaces.** Frames are retyped from untyped, at 4 KiB and larger contiguous sizes (contiguity is free from retype). An address-space object is created from donated untyped and is **pool-at-creation**: the kernel draws intermediate page tables from the aspace's own pool, returns `NEED_MEMORY` when the pool is exhausted, accepts top-ups, and returns the pool with the object at teardown. This gives one error path and a trivial allocator. It departs from seL4's explicit per-page-table objects but not from seL4's principle that the kernel performs no allocation that is not user-accounted; what it gives up — per-table caps and revocation of individual intermediate tables — nothing in this design needs.

**Mapping state lives in the frame cap: one mapping per cap copy, and deleting or revoking the cap unmaps it.** This makes shared memory obey the same revocation story as everything else. The bulk-IPC path (§3.1) is: derive a frame-cap copy (attenuated to read-only if desired), send it (§3.4), and the receiver maps it; revoking the parent cap unmaps every sharer with no special machinery. The cap-side bookkeeping is verified — a derived copy starts unmapped, and deletion drives an unmap at the cap's recorded coordinates, with the matching map-time recording joining the verified surface this revision (§6.1) — while the actual clearing of page-table entries is verified separately over raw page-table memory; the correspondence between the cap's recorded mapping and the true entry is a trusted seam (§6.1).

**Root survival across revoke.** Reclaim and grant patterns rely on the granter retaining its parent cap after a revoke empties the descendants — the §5.1 reuse of a revoked untyped, or a server reclaiming a session window. Revoke deletes only *descendants* of its target, so the target itself can empty only through the cross-object teardown that fires when a revoked descendant holds the **last** capability to an object that *homes* the target (a cspace whose resident, a channel whose ring slot, or a thread whose binding slot *is* that cap). Survival is therefore guaranteed for any root whose homing object keeps a live reference outside the revoked subtree — exactly the shape the donation and reuse patterns build, since a reclaimable grant always keeps a reference to the funder outside the granted subtree. The only case that self-empties is a root whose homing object's last reference is itself inside the revoked subtree.

**Grant direction: the party whose liveness matters must be the CDT ancestor of any shared mapping.** Revocation flows downward, and a process's death revokes what it funded — so a received (descendant) frame cap can vanish at any instant, by the granter's deliberate revoke or as a side effect of the granter's teardown, turning the receiver's next load or store into a fault, and faults suspend (§5.3). A server that mapped client-granted memory could thus be wedged by any client, maliciously or by simply crashing mid-request. The direction for client–server bulk transfer is therefore fixed: **bulk windows are funded from the server's untyped**; the server derives a per-session cap (rights-masked as appropriate), sends it, and the client maps. Deleting a child cap never propagates upward, so a client can destroy only its own view; the server's mapping is valid for the session's lifetime, and session teardown revokes the window caps and reclaims the memory. Accounting lands on the server, where the server's total window budget is enforced at session setup (§3.5). The direction also tracks trust: a client that maps server memory already depends on that server for every byte it stores, so it trusts nothing new; the reverse would have the server extend liveness-trust to every client, which a server cannot do. In general the ancestor of a shared mapping is the party whose liveness dominates the other's; where neither dominates, the common supervisor funds.

**DMA.** Virtqueue descriptors carry guest-physical addresses, and **DMA does not go through the MMU**: whoever programs a DMA device can touch any physical memory in the machine. The consequence: **a DMA-capable driver is inside the memory-isolation TCB** — its CPU is confined by the MMU, its device is confined by nothing. (The only DMA driver feeds the storage server, which already holds every byte of the data it stores.) The mechanism that contains physical addresses is a distinct `phys-read` right on frame caps, which gates the operation returning a frame's physical address. Init grants `phys-read` only to the holder of the **DMA-pool crate** — the single place in the system where physical addresses appear. The crate hands out buffers labeled with opaque device addresses; drivers are written against it and never see a physical address. A driver owns a bounded, persistent DMA pool and copies between it and the storage server's shared-memory buffers. Kernel validation of DMA regions without an IOMMU would be ineffective — either hardware enforces isolation or the driver is trusted — so it is not attempted.

### 2.6 Time

**Monotonic time** comes from the ARM generic timer. The counter register is readable from EL0, giving every process a zero-syscall monotonic clock; the kernel timer object (§3.6) programs deadline interrupts for timeouts and the storage flush timer (§4.4). Deadlines are evaluated on the periodic scheduler tick (§5.4), so deadline resolution is one tick; the specification does not otherwise quantify timing resolution.

**Wall-clock time** uses a one-shot boot read rather than a driver. Init (which holds all device caps) reads the PL031 RTC once at boot and publishes `(seq, wall_base, cntvct_base, cntfrq)` in a read-only shared frame mapped into every process — a **time page**, in the style of a vDSO. The frame is funded from init's untyped and arrives in the startup block under the name `time` (§5.1). The RTC's one-second granularity puts ±1 s of absolute error on `wall_base`; retention rules are denominated in hours, so this is accepted rather than polled away. `wall_base` is UTC nanoseconds since the Unix epoch as a signed 64-bit integer; wall time is `wall_base + (now − cntvct_base) · 10⁹ / cntfrq`, computable by anyone with no syscall and no IPC, and is the representation snapshot rows store (§4.7). The computation is made total by two normative rules: `cntfrq` is floored to 1 before the division, so a zero frequency cannot divide; and the elapsed delta `now − cntvct_base` saturates to 0 when `now` precedes `cntvct_base`, so a counter reading below the recorded base contributes no time rather than underflowing. `seq` is a sequence word, reserved now so that deferred clock setting and drift correction (§8) become a seqlock discipline rather than a retrofit; readers follow the seqlock protocol from the start. All stored time is UTC; time zones are presentation, owned by the shell. Boot-relative timestamps are not used, because "older than 30 days" must order across reboots.

One storage rule follows: **snapshot timestamps are clamped non-decreasing per ref**, so RTC misbehavior cannot disorder the snapshot log.

### 2.7 The syscall boundary

Every kernel operation in this section is invoked from userspace the same way: a **syscall** — a trap from EL0 into the kernel carrying an **opcode** (the syscall number) that selects the operation, and a fixed set of argument words (capability-slot indices and inline scalars). The opcode space is small and closed — one number per kernel operation across the object set (§1), IPC (§3), and the thread lifecycle (§5) — and a syscall is the only way to invoke a kernel operation; reading the timer counter (§2.6) or a mapped frame is a hardware affordance, not an operation, and needs none.

The boundary applies to syscall numbers the same untrusted-decode discipline the wire protocol applies to IPC message opcodes (§3.7):

- **An unrecognized opcode returns an error, never a crash.** A syscall number outside the defined space is refused with an error code; it neither panics the kernel nor faults the caller.
- **Every argument is validated against ground truth before use.** A slot index is checked against the calling cspace's actual extent, a count or length against its fixed structural bound, before it indexes a table or sizes a copy — each field checked against a quantity the kernel already holds, never against another caller-supplied field.
- **Decode is total over arbitrary arguments.** Any register contents yield an operation or an error, never undefined behavior, an out-of-bounds access, or an unbounded allocation.

The syscall decoder is in the verified surface (§6); the thin dispatch and register marshalling onto the operation it selects is trusted shell, the same posture every object setter takes (§6.1). The debug-print scaffold of §7 occupies opcodes in this space as a disclosed, temporary exception to the capability model of §2 — an ambient operation that no capability gates, retained only until the userspace console driver replaces it. *(Status, C-M9: the userspace console driver has landed. The ambient `debug getc` input opcode is removed — its decoder arm is gone and the number now decodes to `UnknownCall` — and the two `debug putc`/`write` output opcodes are gated behind a `debug-log` build feature as a kernel-diagnostic path; no EL0 user-facing path uses them, closing the §2 exception for the console.)*

---

## 3. IPC

Channels are **asynchronous**, in the style of Zircon, rather than synchronous rendezvous. The userspace is Rust-centric, and asynchronous channels compose naturally with Rust async servers; the kernel pays modest extra complexity (message queuing) for a friendlier userspace model.

### 3.1 Message format

A small inline payload plus shared memory for bulk. A queue slot is fixed-size: a small header, a 256-byte inline payload, and 4 capability slots (format constants, not ABI promises). Anything larger travels through a per-session **bulk window** — a shared-memory region established at session setup via a server-granted frame cap (grant direction and revocation: §2.5; concurrency discipline: §4.8) — with the channel message acting as a descriptor naming `(window, offset, length)`. The bulk path is mandatory regardless, since file contents must not be copied through kernel messages, so inline messages carry only control traffic and per-message kernel work stays bounded.

**Window lifecycle.** Window size is a connect-time parameter, admitted against the server's total window budget (§3.5); zero is legal, giving a control-only session (for example a registry connection) no window at all. Each session has exactly one window, so the descriptor's window field is always 0; the field is reserved so that a later multi-window extension (§8) is an addition rather than a migration. Descriptors are validated against the named window's static extent before any access. A window need only cover the bandwidth–delay product of an IPC-plus-memcpy pipeline on one machine — a few maximal chunks — and large writes gain nothing from larger windows, because the engine backpressures upstream anyway (§4.4). A window may be implemented as a page-aligned sub-range derivation (§2.3) of a single arena frame retyped once at server startup, making per-session allocation ordinary arena management.

### 3.2 Queue memory

Queue memory comes from untyped at channel creation: the creator chooses the channel's **depth** (its number of queue slots) and retypes enough untyped to fund it, the byte cost being a fixed function of the depth and the fixed slot size (§3.1). Depth is a per-channel parameter with an explicit, capability-controlled cost. There is no kernel-global pool and no shared exhaustible resource.

### 3.3 Send/receive semantics and backpressure

- `send` is non-blocking and returns `FULL` when the queue is full; messages are never dropped, since a dropped message could carry a capability and a lost cap is unacceptable.
- Channels expose **readability** and **writability** notifications ("signal me when a message or space appears") and a **peer-closed** notification, raised when the other endpoint is destroyed (required for session cleanup, §2.4). Teardown always signals: deleting one endpoint fires the surviving peer's binding, and destroying the whole object at once — its backing untyped revoked, the normal case when a session's funder dies — fires every endpoint's binding before reclamation. The bound notification is a separate object and outlives the channel if separately funded. A dead endpoint thereafter yields error returns. Queue memory is touched only through syscalls, so channel death is always delivered as an event, never as a fault.
- Delivery is **FIFO per channel**, with no kernel-side priorities or fairness across messages. Fairness across clients is the server's problem, solved by the session pattern (§3.5).
- On receive, transferred caps are installed into the receiver's cspace; if the receiver lacks free slots, the receive fails and the message stays queued, and the receiver makes room and retries. Receive-side exhaustion is the receiver's own resource problem.
- Blocking send, bounded-retry send, and async `send().await` are userspace library code over the non-blocking primitives plus notifications. The kernel provides mechanism; policy lives in the Rust async runtime.

Waiting on *sets* of these signals is the job of the event mechanism (§3.6).

### 3.4 Capability transfer: move semantics

A cap leaves the sender's cspace at send time, occupies a slot in the message while queued, and lands in the receiver's cspace at receive time. **At every instant a capability has exactly one owner** — sender, queue slot, or receiver, never two. A sender that wants to keep access duplicates first, a deliberate and auditable act. Both halves of the move are verified: send empties the source slots and pushes the cap onto the queue, and receive installs each arriving cap into the destination slot the caller named and empties the dequeued slot.

Consequences, all load-bearing:

- **Queue slots are real, CDT-visible capability slots** owned by the channel (allocated from the memory donated at creation). Revocation therefore finds and deletes in-flight caps like any other descendants — no special case in the revoke logic, no caveat in its specification. A queued cap is a descendant because the send path inherits the parent CDT edge into the ring slot.
- **Receivers must tolerate null cap slots:** revocation may have emptied a slot in flight, and senders can lie regardless.
- **Channel destruction destroys queued caps** through ordinary CDT cleanup: the sender moved them out, nobody holds them, they are gone. The matching discipline — do not destroy a channel with valuable caps queued — is handled in userspace by a small acknowledgment protocol for valuable-cap handoffs. There is no kernel bounce-back mechanism.
- At most 4 caps per message; the limit is structural (the preallocated slot layout), not policed.

### 3.5 Sessions and the IPC crate

Servers publish a connection endpoint, and **the client funds the session**: it retypes a channel pair from its own untyped (§3.2) and sends one endpoint in the connect request, together with a requested bulk-window size (§3.1) that the server grants or refuses against its **total window budget** at this single admission point: the sum of all granted windows never exceeds that budget — the load-bearing anti-drain bound — and a server may additionally clamp the per-session size, though the total bound is what holds. A connect therefore consumes the connector's memory for queues and the server's for the window, each side funding what the other must not (§2.5). Anonymous connects cannot drain a server: queue memory is the connector's, and window memory is bounded in total by the budget the server enforces at admission. Each client buys exactly the queue depth it wants, and the per-client channel is where per-client queue accounting and fairness happen.

Client funding is safe here precisely where §2.5 forbids it for mappings, and one criterion decides both directions — **fund by failure mode**. Queue memory is touched only through syscalls, so a client-funded channel's death reaches the server as error returns plus the peer-closed signal (§3.3), an event handled in code. Mapped memory is touched by loads and stores, so a mapping's death arrives as a fault (§5.3). What dies as an event may be funded by the untrusted side; what dies as a fault must be funded by the side that cannot afford to fault.

A single **userspace IPC crate**, used by every server, owns the ergonomics: `FULL` handling, async send/receive, the valuable-cap acknowledgment protocol, and message serialization (§3.7). The kernel primitive stays primitive; the ergonomics are solved once. This crate is the first Loom/Shuttle target (§6).

Interrupts are delivered to userspace drivers as events through the same mechanism (§3.6).

### 3.6 Event multiplexing: notifications

The kernel event primitive is the **notification object**: a single machine word of signal bits plus a waiter queue. Signalers OR bits in; a waiter receives the accumulated word, which then clears. A signal whose accumulated word is still zero conveys nothing and does not wake a queued waiter; the reactor only ever signals a nonzero single-bit mask.

Each channel endpoint carries **fixed binding slots**, configured by the endpoint's holder: on-readable, on-writable, and on-peer-closed, each binding to a (notification cap, bit) pair. IRQ handlers bind identically (seL4 precedent); timer objects bind identically (providing wait timeouts and the storage flush timer, §4.4); and threads bind identically — on-exit and on-fault slots in the thread object deliver death notices (§5.1, §5.3), with the report record preallocated so delivery never allocates even for a dying thread. Notifications are also signalable directly from userspace, for executor self-wakeup. One object type, three pointer-sized slots per endpoint, and no allocation on any event path. The lost-wakeup discipline (bind, poll once, then wait) lives in the IPC crate.

The structural limit is the word width: at most 64 distinguishable sources per waiting thread. Beyond that, bits identify *groups*, and a wakeup costs an O(group) scan — `select`, not `epoll`. The storage server (one channel per session) is the component that will outgrow this. The IPC crate hides this shape: its reactor API is epoll-shaped — register a source with a set of signals and a key, dispatch in O(1) — implemented over bit groups underneath, so no server sees bits and a later kernel event object (§8) changes no server code.

**Event delivery never allocates.** This is a hard rule of the design.

### 3.7 Wire protocol and serialization

Every message begins with a **fixed, hand-defined header**: protocol id, version, opcode, flags, and body length. Versions are negotiated once at session establishment; an unknown opcode yields an error reply, never a crash; a breaking change is a new version number, and a server may speak several concurrently. The header layout never migrates — it is the layer that makes every other layer migratable.

**Bodies are postcard-encoded via serde** (`no_std`-first, compact, deterministic), behind an encode/decode trait that is module-private to the IPC crate: servers and clients construct and consume plain message types and cannot reach the serializer, so there is no ad-hoc encoding and no pre-encoded byte blobs. Message types are kept deliberately plain — no borrowed lifetimes, no flattened or untagged enums, no non-string-keyed maps — the subset that maps onto any IDL's type system. Capabilities never appear in payloads (they travel in cap slots, §3.4) and storage handles are plain integers, so the format needs no exotic types. Decoders treat all payloads as untrusted, reject trailing bytes, and are fuzz targets on the host (§6).

Nothing persistent speaks postcard — on-disk formats are hand-defined and canonical — so the message format and the storage format evolve independently.

---

## 4. Storage

### 4.1 Structure

- **Chunking:** FastCDC (a gear-hash content-defined chunker), target chunk size ~16–64 KiB.
- **Addressing:** BLAKE3 over chunk content; the hash is the address (internally — never authority at the boundary, §2.4).
- **Aggregation:** nested per-directory **prolly trees** (Merkle search trees). Each directory is its own tree keyed by entry name, referencing child directories by root hash (§4.9). Node split boundaries are a function of the hash at the boundary key, so tree shape is **history-independent (canonical)**: the same logical contents always produce the same tree, regardless of edit order. Canonical form is what makes structural sharing, dedup, and diffing work across histories, and it is what makes this layer tractable to specify formally.
- **Ref table:** a small tree in the CAS, committed through the superblock, holding three kinds of entry: **refs** (named branch heads, `name → (root hash, generation counter)`), the **snapshot log** (§4.7), and **tags** (§4.7).

**Persistence model.** Processes are ephemeral; storage is persistent. Durability and versioning happen at semantic boundaries (commits, snapshots), per branch, under user control. Processes are disposable and reconstruct in-memory state from canonical persistent state at startup — crash-only by default.

### 4.2 On-disk layout

1. **Two superblock slots (A/B)** at fixed locations, one block each. A superblock holds: magic, format version, a monotonically increasing **generation number**, the ref-table root hash, the WAL head position, references to the durable chunk index and free-extent list, and a **checksum over the whole superblock**.
2. **Write-ahead log (WAL)** region — a replay buffer only, never the commit mechanism.
3. **Chunk store** — an append-friendly region of write-once chunks and tree nodes, plus a **durable index** mapping hash → (offset, length, **birth generation**). The index is superblock-referenced, self-verifying, and committed through the same flip as everything else; it is not rebuilt by scanning at mount, because a scan cannot represent the holes that reclamation creates. The birth generation (the superblock generation at append time) makes "older than the GC epoch" well-defined, makes the live-by-fiat GC rule checkable, and is the hook for incremental GC and birth-time pruning (§4.6).
4. **Free-space accounting** — a durable free-extent list, superblock-referenced and committed like the index.

The generation-checksummed A/B superblock flip, preceded by an fsync barrier, is the **single atomicity mechanism for the entire system**: writes, snapshots, ref updates, GC results, and history rewrites all commit through it.

**Commit invariants:**

- *Self-reference:* the index frame records the free list, yet placing the frame consumes free space — resolved by an upper-bound size estimate plus explicit padding, so the frame fills its extent exactly.
- *Deferred reuse:* **no extent freed by commit N may be reused until N's second barrier has landed.** Otherwise a crash plus a dedup index-hit could resurrect overwritten bytes. This is the general law behind the superblock-alternation rule and applies to all freed space — superseded index frames and swept extents alike.
- *No wedging:* index frames must be placeable in freed extents anywhere in the chunk region, never tail-only; tail-only placement deadlocks a store whose tail is exhausted even while GC has freed space elsewhere.

### 4.3 Mutation path

Writes never touch the tree directly.

1. **Memtable.** Each write lands in a per-ref in-memory **overlay** (§4.4) keyed by (file id, offset range), also recording creates, deletes, and renames; per-file overlays are interval maps. Reads consult the overlay first and fall through to the immutable tree — an LSM read path whose bottom level is the prolly tree.
2. **WAL.** If a write must survive a crash before the next flush, its record is appended to the WAL and the WAL is fsynced before acknowledgment. Per-record checksums let torn tails be discarded safely on replay.
3. **Flush** (triggers and scheduling: §4.4) turns one ref's frozen overlay into immutable structure:
   - Freeze that ref's overlay and open a fresh one, so the flush does not block writers.
   - For each dirty file, **re-chunk the affected neighborhood only**: back up one chunk before the first dirty byte, run the chunker forward, and stop when an emitted boundary coincides with an existing one (CDC self-synchronization guarantees this within a few chunks). A 200-byte edit in a 1 GiB file yields ~2–4 new chunks.
   - Hash new chunks; an index hit dedups (reuse), a miss appends to the chunk store. Chunks are never modified in place — this write-once discipline is the root of crash safety.
   - Path-copy upward through the prolly tree to a new root hash. Because the memtable batches, many dirty files in one directory rewrite that directory node once.
   - Update the ref table (another small path copy) to point the ref at the new root. Nothing on disk references any of this yet; a crash here leaves only unreachable garbage.
4. **Commit.**
   - **Barrier 1:** fsync the chunk-store and index appends. No superblock may mention chunks that are not durable.
   - Build the new superblock: generation + 1, new ref-table root, WAL head advanced past the contiguous prefix of records whose effects are now flushed, fresh checksum.
   - Write it to the **older** slot (always alternate; never overwrite the current latest commit).
   - **Barrier 2:** fsync the superblock. Only now is the commit real and acknowledgeable.
   - Nothing is freed on the write path; reclamation is GC's job alone, which keeps the write path simple.

A single commit may carry any number of freshly flushed ref roots: batching across refs happens at the commit, not in the memtable.

### 4.4 Memtable and flush policy

**Per-ref overlays under a global byte budget, charged to sessions.** Per-ref (rather than one global memtable) keeps snapshot latency independent of other refs' traffic, makes "who is consuming the buffer" a per-session fact, and keeps each freeze small. The global budget exists because memory is finite; per-ref soft quotas under it provide containment. With a single `main` ref this degenerates to a global memtable, but the API is per-ref because freeze granularity is hard to retrofit.

All read-write handles to a ref share its overlay; write ordering is server arrival order, **last-write-wins, with no multi-operation transactions**.

**Bounds** are denominated in bytes of dirty overlay — the unit that governs memory, recovery-replay time, and read-path overhead — with an operation-count secondary bound so that metadata storms cannot hide under a small byte count. On hitting a bound the response is **backpressure, not eviction**: the write gets `FULL` or blocks at the IPC layer while a flush runs. There is no eviction; overlay leaves memory only by becoming tree.

**Flush triggers, in priority order:**

1. **Explicit.** `sync` or `snapshot` on a ref flushes that ref synchronously. A snapshot must name a tree hash, so snapshotting forces a flush of that ref. **Rollback** with a dirty overlay also flushes — into the *abandoned* pre-rollback root, so the WAL stays coherent (the records' effects become committed tree and the head can advance) — and only then re-points the ref; the abandoned root is ordinary garbage for the next GC. (Discarding the overlay would strand acknowledged WAL records; refusing would make rollback unusable under background writes.)
2. **WAL pressure.** The WAL is one global sequential region whose head can advance only past records whose effects are flushed, so the tail is pinned by the oldest unflushed record, and an idle ref with one ancient dirty byte can pin the whole log. When WAL usage crosses a watermark, **flush the ref pinning the tail**, and repeat until comfortable. The server tracks per-ref oldest-WAL-position as the scheduler's sort key. Two edge cases are normative: a record larger than the entire WAL region bypasses the log and commits synchronously before acknowledgment; a full WAL flushes everything and resets the log.
3. **Size pressure.** A per-ref quota or global watermark crossed flushes the biggest offenders. Start flushing at a low watermark, so writers rarely hit `FULL` at the high one.
4. **Timer.** A staleness bound, so a quietly dirty ref eventually becomes committed tree.

**Recommended defaults.** The triggers and bounds above are mandatory mechanisms; the numbers below are recommended defaults — the in-memory `StoreOptions::default`, which a store may tune — not part of the mechanism: per-ref soft bound 8 MiB, global budget 128 MiB, WAL 64 MiB with flush-the-pinner at 50%, timer 30 s. The shipped server adopts these in-memory defaults with one exception: the **WAL size is not an in-memory choice**. A mounted store reads its WAL region from the on-disk superblock — image geometry — so the live size is whatever the image was built with, and the shipped `mkfs` image deliberately tunes the WAL down to 1 MiB (recovery buffers the whole region against a few-MiB server heap, and a 64 MiB WAL would not even lay out within the default 64 MiB image). One consequence: the flush-the-pinner watermark keeps its 32 MiB in-memory default rather than re-deriving from the 1 MiB live ring, so on the shipped image WAL-pressure flushing is bounded by the full-WAL reset (above) rather than the 50% trigger. The tension the numbers balance: frequent flushes amplify writes (the same directory spine path-copied repeatedly, each superseded root instant garbage); rare flushes cost memory, recovery-replay time, and dedup misses within the unflushed window.

### 4.5 Crash recovery

Read both superblock slots and discard any with a failing checksum: a torn superblock write can damage only the slot being written, so the other is a complete older commit. Take the surviving slot with the higher generation; its ref table defines reality. Two checksum-valid slots of *equal* generation are impossible for commits made by this system (the protocol keeps generations distinct) but can be constructed by a forger; the tie resolves deterministically to **slot A**. This is safe because the survivor — whichever slot — is then validated before use (below), so a forged tie can neither panic, over-read, nor allocate unboundedly; it is no worse than the single-forged-slot case.

**The checksum detects torn writes; it authenticates nothing** — there is no secret in it, and a checksum-valid superblock proves a complete write, not a write by this system. Mount therefore validates the geometry in layers. First, at a single chokepoint immediately after checksum verification, it validates the superblock's own geometry fields — index and free-list locations, WAL region, chunk tail — against the one ground truth it holds, the device length, with checked arithmetic; this establishes the chunk tail and the rest of the superblock geometry as trusted. Then each field *derived* from that geometry — read from a structure the now-trusted superblock points at — is checked with checked arithmetic against the now-trusted chunk tail before it sizes an allocation or a read. (Untrusted fields must never vouch for each other: a length gated only by another untrusted field is untrusted data validated against untrusted data.)

Replay the WAL from the recorded head to rebuild per-ref overlay state for acknowledged-but-unflushed writes; discard checksum-failing tail records, which were never acknowledged. Unreferenced chunks from interrupted flushes are invisible and reclaimed by the next GC.

There is **no repair logic** — no fsck. Either a commit completed (its superblock checksums and wins on generation) or it did not (its garbage is unreachable). Recovery is **total over arbitrary device contents**: mount returns mounted-or-refused for any byte string presented as a device, never a panic, never an unbounded allocation, never a read past the device's end. This is the same rule applied at the syscall boundary (§2.7), to bulk-window descriptors (§3.1), and to wire decoding (§3.7): a length from untrusted input is validated against ground truth before use.

**Initialization obeys the same discipline, in the other direction.** Creating a fresh store — `format`, and the host-side `mkfs` that wraps it (§7) — validates the requested geometry against the device before writing anything. A device too small to hold the fixed superblock slots, WAL, and a minimal chunk region, or a geometry that cannot be laid out within the device, is refused with an error, never a panic: `format` returns a clean error result, and the host tool exits with a failure status. Mount is total over arbitrary device *contents*; `format` is total over arbitrary device *geometry* — together they keep opening a device panic-free whether it is read or initialized.

**The block driver trusts the device for its own geometry.** Mount's ground truth is the device length (above); at runtime the block driver likewise treats the device's reported capacity as ground truth for which logical block addresses are valid. It need not pre-check a requested address against capacity — an out-of-range request is the device's to reject, and the driver's fixed-size DMA buffer (§2.5) bounds every transfer regardless. A defensive address-versus-capacity check in the driver is permitted but not mandated; nothing in the design depends on one.

### 4.6 Garbage collection

**Mark-and-sweep from live roots, periodic and concurrent.** Refcounting is not used: structural sharing makes count maintenance a write-amplification problem with its own crash-consistency burden, and history rewriting would turn each lineage drop into a cascading decrement over millions of nodes. Mark-and-sweep pays only at reclamation time.

**Mechanism:**

1. **Root set:** every ref, snapshot, and tag target in the current committed ref table, plus any roots committed while GC runs.
2. **Mark:** walk from each root, accumulating reachable hashes and pruning already-marked subtrees (structural sharing makes the walk cheap across snapshot families). Mark state is an exact in-memory hash set. An aborted GC (crash, restart) is safe by construction: reclamation happens only at the sweep commit, so losing mark progress loses reclamation work, never data.
3. **Concurrency:** chunks written during GC are live by fiat (checkable via birth generation, §4.2). The one subtle hazard is **dedup resurrection** — a new flush index-hits a chunk the marker has already condemned. The fix: during sweep, a dedup lookup that hits an unmarked chunk is treated as a miss, so the chunk is rewritten under the same hash, replacing the index entry. This confines all GC/mutator interaction to one point.
4. **Sweep:** delete index entries for unmarked hashes whose birth generation predates the GC epoch, return their extents to the allocator, and commit the updated index and allocation state via the normal superblock flip. A crash mid-sweep loses reclamation work, never data.

**Policy.** Garbage arrives from three sources, each with a trigger, plus a floor:

1. **Superseded roots.** Every flush makes the ref's previous root garbage (unless pinned by a snapshot) plus the path-copied spine. Trigger: **space watermarks** — below ~20% free, schedule GC; below ~10%, run with elevated I/O priority. To avoid thrashing on a store that is simply full of live data, the watermark re-arms only after the generation advances past the last completed GC.
2. **History rewriting.** A retention pass dropping a month of snapshots creates a large unreachable mass in one commit. Trigger: **event-driven** — any history-rewrite operation (snapshot deletion included) sets a GC-requested flag, so the foreground operation stays O(small) and reclamation follows promptly.
3. **Floor:** a periodic trigger (daily, or every N generations), so a lightly used system still converges; cheap insurance against trigger-logic bugs.

**Rules:** at most one GC at a time — a trigger arriving mid-cycle coalesces into "run again after this one." The sweep is the I/O-heavy phase and the only one taxing the write path (every dedup lookup pays the resurrection check), so sweep I/O is throttled behind foreground traffic by default. The mark phase is comparatively gentle (reads, heavily pruned by sharing).

**Rights.** Client-triggered GC requires `may-rewrite-history` on a ref-root handle: reclamation is history rewriting's other half. Server-initiated triggers (watermark, post-rewrite, floor) are the server acting on its own authority and involve no client right. A GC reply reports its own effect (objects and bytes freed) under `may-rewrite-history` alone; maintenance operations may report what they did without `stat-store` (§2.3).

**History rewriting** is, at the storage layer, editing the root set: "forget snapshots on `main` older than 30 days" is one small ref-table edit plus a commit, and GC asynchronously reclaims whatever became unreachable.

### 4.7 Snapshots, tags, and retention

**A snapshot's identity is a stable, server-assigned ID** (a per-ref sequence number) — never its content hash and never a hash over its metadata. Two reasons. First, hash-as-identity (git-style) embeds parentage into identity, so rewriting any snapshot would re-identify every descendant; with row identity, metadata is editable (fix a message, re-point a parent after a prune, change a retention class) without touching anything else. Second, canonical trees make content hash unusable as identity anyway: snapshotting unchanged content twice yields the same root, so event identity and content identity must come apart.

A snapshot is a **row in the snapshot log** (stored in the ref table, committed via the superblock):

```
(snapshot id, root hash, timestamp, provenance, parent?, message?, retention class)
```

- **Timestamp:** server-assigned at snapshot time, stored as UTC nanoseconds (§2.6); client-supplied times are not accepted. Assignment is clamped per-ref monotone (`ts = max(now, predecessor_ts + 1)`), so a host clock regressing between boots cannot make a child snapshot older than its parent. The clamp protects per-ref order; it cannot fix a wildly wrong RTC making "older than 30 days" wrong in absolute terms.
- **Provenance:** filled in by the server — which session created the snapshot, via which trigger (explicit call, timer policy, pre-rewrite safety snapshot).
- **Parent:** advisory, single-parent, nullable — "the ref's previous head at snapshot time." It is not needed for diff (prolly-tree structural diff works between any two roots regardless of lineage); it serves presentation (log view, undo chain) and may be re-pointed by history rewriting. There are no merge commits and no DAG; multi-parent rows would be a backward-compatible schema extension if merging were ever added.
- **Message:** optional, default empty, never prompted for.
- **Retention class:** `keep` (immune to automatic pruning), `auto` (subject to the ref's retention policy), or `ephemeral` (first to go). The "choose what survives" flow is: mark survivors `keep`, then run the policy — a pure ref-table edit followed by ordinary GC.

Editing rows is uniformly privileged: deletion, parent re-pointing, and message and class changes all require `may-rewrite-history` on a ref-root handle (§2.3) — row surgery *is* history rewriting, whichever field it touches. Two deletion semantics are fixed. Deleting a snapshot a tag points at **fails with `Pinned`** — delete the tag first if intended — because cascading would quietly expand a row deletion into tag destruction. And retention classes govern *automatic* pruning only: an explicit `may-rewrite-history` deletion of a `keep`-class snapshot succeeds (`keep` is protection from policy, not from its owner), while policy tools skip `keep` rows.

**Retention policy is a userspace daemon** holding a `may-rewrite-history` handle and expressing rules over timestamps and classes (keep hourly for a day, daily for a month, and so on). The server stores fields; it does not interpret policy.

**Guarded ref-table batches.** A retention pass is read-then-act — enumerate the log, compute a prune set, issue deletions — and another session may snapshot or edit between the read and the act. The remedy is a conditional batch. Each ref carries an **edit version**: a counter advancing on every committed mutation of the ref's entries (head moves, snapshot rows, tags), distinct from the §2.2 revocation generation. Enumerate operations return the edit version; a guarded batch apply (`handle`, `expected_version`, `edits`) applies all-or-nothing within one commit if the version still matches, else fails carrying the current version so the caller re-reads. The counter is plain data through the normal commit path, and the check is one comparison in the single authority over the ref table.

**Tags** name the few snapshots worth remembering: ref-table entries mapping `name → snapshot ID` (not root hash, so they survive metadata edits), acting as `keep`-strength pins. The trichotomy: **refs** name lines of development, **snapshot IDs** name events, **tags** name memorable events.

### 4.8 Integrity

Every layer self-verifies: chunks (hash = address), tree nodes (hashed), the superblock (checksummed). The storage server detects corruption on read. The single trusted axiom is that **fsync means fsync** — on the target this is a QEMU/virtio-blk configuration under our control (`cache=writeback` with FLUSH honored), and it is stated explicitly as a labeled `ASSUME` axiom in the TLA+ model.

Bulk windows (§3.1) add a concurrency discipline, since the window stays mapped writable in the client while the server works and a client can race its own request. Two rules confine the race. **Single-fetch:** the server moves a request's bytes across the boundary exactly once and never re-reads window memory expecting stability. **Hash-what-you-stored:** content hashes are computed over the server's private copy, never over the window, so a racing writer can corrupt only its own payload and the hash-is-address invariant cannot be broken from outside. The write path already copies into the overlay (§4.3), so both rules are free.

### 4.9 Tree schema, entry encoding, and namespace

**Nested directory trees, not a flat path keyspace.** Each directory is its own prolly tree keyed by entry name, with entries referencing child directories by root hash. Directory moves are O(depth) — detach a hash, reattach it — and the subtree-handle property (§2.3) holds literally: the holder holds a node and cannot name anything above or beside it. Diff is cheap and recursive: equal root hashes mean identical subtrees (skip), unequal means diff the two entry lists (each itself a prolly tree, so large directories diff with equal-node skipping) and recurse only into changed children — O(changes × depth). A directory move diffs as one entry removed plus one added with an identical content hash, which is also the signal for cheap rename detection. The costs: no single global key order (whole-store enumeration is a recursive walk), and balance is per-directory, so resolution depth is actual nesting depth.

**Names** are 1–255 uninterpreted bytes, excluding NUL and `/` (for display and interop sanity), with `.` and `..` reserved as path syntax — resolved by shells and path walks, never stored. Identity is bytewise and ordering is memcmp; there is no case folding and no Unicode normalization, because any equality coarser than byte equality would make the stored bytes depend on insertion history and break canonical form. UTF-8 is a convention enforced by tooling, as is the printable-ASCII restriction; tooling restrictions can loosen freely, while format-level restrictions are migrations. The wire protocol takes **component lists** (`open(handle, ["etc","config"])`); `/` is shell presentation, not a format concept.

**Entry encoding: deterministic TLV.** Mandatory fields (type, size, mtime, content reference), then optional fields as (tag, length, value) triples, sorted by tag, with absent fields contributing zero bytes — exactly one encoding per logical entry, so canonical form survives extension and new tags never perturb old entries' hashes. A single optional tag is defined: a flags word containing the **advisory-executable** bit. A hard cap on total optional bytes per entry (a few KiB) keeps directory nodes directory-shaped regardless of future tags.

An entry: `name → (type: file | dir, flags, size, mtime, content: inline bytes | chunk-list hash | child-directory root hash)`.

- **Small-file inlining:** content ≤ 512 bytes lives inline in the entry. The rule is a pure function of content, preserving canonical form; reading a small file costs no I/O beyond the directory node already fetched.
- **mtime** is server-assigned; there is no atime. Because metadata participates in hashing, "same contents produce the same tree" is precisely "same contents *and metadata*"; chunk-level dedup is unaffected, and node sharing within a snapshot lineage survives since mtimes there change only when content does.
- **Execute is not a storage right.** The storage server only serves reads, and "read in order to execute" is indistinguishable from read. Execution authority is possession of process-construction caps or access to a spawner (§5). The executable flag is a type hint (PATH lookup, completion) with no security semantics, and is documented as advisory.

**File identity at runtime.** The persistent format is purely path-keyed; the "file id" in the memtable keying (§4.3) is an ephemeral, server-runtime ID assigned per open file. The overlay keys on it; an ID → current-path map updates O(1) per rename regardless of how much dirty state exists, so open handles follow renames. IDs never touch disk. Unlink-while-open: the open handle keeps working against the overlay, but if at flush time the ID resolves to no path, the data is discarded — which is what unlink means here. Rename across refs is a copy with new lineage; rename targeting outside a subtree handle is unnameable and therefore denied.

**Namespace model.** There is no global root; a process's namespace is the set of subtree handles it holds. Every storage operation is `openat`-shaped — relative to an explicitly named handle — and no ambient-root operation exists. A process simply receives several handles at spawn (§5.1).

---

## 5. Process model

Processes are ephemeral. A process is created by a spawn operation that takes an ELF image (typically read via a snapshot handle on a storage session), an explicitly constructed initial cspace (and typically a pre-populated storage session, §2.4), and scheduling parameters. There is no fork. Long-lived services that want durability persist state through the storage server like everyone else.

### 5.1 Spawn convention and the startup block

A spawned process finds its world in a **startup block**: the first message waiting on a bootstrap channel in cspace slot 0, containing argv and env (byte-string vectors) and a **named-grant table**. Table entries carry a discriminator over grant kinds. The two authority-bearing kinds are the literal pair — kernel caps resolve to cspace slots, and storage grants resolve to handle numbers on the process's storage session channel (itself in a well-known slot), pre-populated by the parent (§2.4). Two further kinds carry **no new authority** and are additive: a **pre-mapped region** (a virtual address the parent already mapped read-only before start — the time page travels this way, §2.6 — so only the address, not a cap, crosses), and an **inline value** carried by-value in the block itself (the per-run entropy seed, below).

Standard names: `root` (the process's subtree), `stdin` and `stdout` (deliberately split, so a shell pipeline wires one process's `stdout` to another's `stdin` with neither aware; an interactive console is the same channel granted under both names), `stderr` (kept distinct from `stdout` so diagnostics never enter a pipeline's data; a terminal grants the same console channel under both names; when ungranted, a process's `stderr` falls back to its `stdout` channel, else the debug log), `tmp`, `storage` (a connector to the storage server, when granted), `time` (the read-only time page, §2.6), and `random-seed` (a per-run entropy seed the parent draws from its own generator and hands each child as an inline value; the child seeds a process-local generator from it, and a parent draws a fresh seed for every child so siblings never share a stream. QEMU `virt` offers no hardware entropy source (§2.6), so the seed init derives is deliberately predictable and non-cryptographic — an MVP whose only property the kernel/loader mechanizes is the *decode* of the inline bytes; the quality of the seed itself is out of scope until a real source, §8). `cwd` is reserved; whether the shell passes it or folds it into how it constructs `root` is a shell-level choice.

**There is no kernel process object; the thread is the unit of report.** A "process" is a loader/runtime convention — an aspace, a cspace, a main thread, and the untyped that funds them — and the kernel's share of its lifecycle lives entirely in the thread object. Each thread carries two fixed binding slots, configured by the holder of the thread cap exactly as channel endpoints are (§3.6): **on-exit** and **on-fault**, each a (notification cap, bit) pair, CDT-visible like queue slots so revocation sees through them. Alongside them sits a preallocated **terminal report record** — `running | exited(status) | faulted(cause, faulting address)` — at most one terminal report per thread (suspend-on-fault means no second fault), so event delivery never allocates. Two syscalls complete the design: a thread-exit operation (the only voluntary stop), recorded by the kernel so a child can neither lie about nor forget its own death, and a read-report operation. The report record's properties — first-write-wins (absorbing) and survival of the thread object's destruction — are verified; the thin syscall dispatch and register marshalling around them are trusted (§6.1).

Spawn returns a **process record**, plain data whose kernel-visible heart is the main-thread cap; process status is the main thread's status, by convention. The canonical parent loop: bind on-exit and on-fault to a notification, start the thread, wait, read the report, and revoke the donated untyped — whole-child teardown by resource ancestry (§2.2). Because bindings are configured by the cap holder and a child holds no cap to its own threads, a child cannot silence or forge its own death notice. (That a child cannot is enforced by the rights gates on the binding operations and the spawn-time cap-distribution convention, which sit in the trusted shell rather than in the verified core; the rights bits themselves and their monotone attenuation are verified — §6.1.) Orphans do not exist, because there is no process lifetime independent of funding: parent death revokes the parent's untyped and the entire descendant tree with it. Exit status persists in the thread object until the parent reclaims it, and that memory is parent-funded, so the reaping incentive is the parent's own budget and there is no global zombie table.

### 5.2 Service discovery

Discovery bottoms out in the spawn tree, not a global name table: **parents are the registry** (discovery as recursive delegation). Every child's world is whatever its parent put in its startup block, so "where is the storage server?" is answered by whoever spawned you, and sandboxing a child is simply not granting a name. The current mechanism is **static wiring**: init holds the storage server's connector cap and bakes it into the shell's startup block; there is no registry process, and init is the only binder.

The broker *protocol* is defined so that a registry can be added later without changing clients: a registry is any process speaking `lookup(name) → connector cap` and `register(name, connector cap)` over a channel, where the returned cap is the service's accept endpoint. The broker never proxies traffic — clients connect per §3.5, funding the session pair themselves, and the service merely accepts the offered endpoint — so it stays out of the data path. Registry channels are attenuable like any other authority (lookup-only rights, name-subset views).

### 5.3 Faults

A faulting thread is **suspended, not destroyed**: the kernel fills the thread's terminal report record — `faulted(cause, faulting address)`, the registers already saved by the exception path — and fires the thread's on-fault binding (§5.1, §3.6). Faults and exits are one mechanism: two cases of one record and one read-report. The at-most-one-terminal-report property is verified; the "suspended, never rescheduled" behavior is realized in the trusted scheduler shell (§6.1). Supervision delegation is one grant: hand a supervisor an attenuated thread cap (§2.3) and it rebinds on-fault to its own notification.

A parent responds to a fault by destroying the child, and that is the correct semantics rather than a limitation: with no swap, no lazy loading (the loader maps programs fully), and fixed-size stacks with unmapped guard regions, there is nothing a handler could legitimately repair — every fault is a bug. This implies a **design obligation on every protocol**: no process may be put in a position where a peer it does not already trust for its liveness can induce its fault, because under suspend-not-destroy an inducible fault is a wedge. The grant-direction rule (§2.5) is this obligation applied to shared memory; any future mechanism that lets one process affect another's address space must re-establish it.

### 5.4 Scheduling

Single-core, **strict fixed-priority preemptive scheduling: 32 levels, round-robin within a level**, on a periodic 10 ms tick; the idle path is WFI. Priority is authority: spawn sets a thread's priority bounded by a maximum carried in the spawner's own thread cap (seL4's maximum-controlled-priority pattern), so the priority lattice is monotone like every other derivation (§2.3). The cap-carried ceiling and its monotone attenuation are verified; this revision's verification work moves the spawn-time check that a thread's priority does not exceed its ceiling into the verified surface alongside them (§6.1). The one raw priority write behind the object seam is trusted, the same posture as every other setter (§6.1).

A documented limitation, deliberately unsolved: with asynchronous channels there is no priority inheritance — a server processes a high-priority client's request at the server's own priority. The answer for now is convention (servers run above their clients); donation and budget-based schemes are evaluated when SMP arrives (§8).

---

## 6. Verification

A tiered policy applies the strongest affordable tool to each component.

| Tier | Tool | Applied to |
|---|---|---|
| Protocol models | **TLA+** | the storage commit/recovery protocol, and kernel cap revocation — both modeled before implementation |
| Proof-carrying code | **Verus** | the kernel object core (cspace/CDT, untyped retype, channels, notifications, timers, thread reports, the page-table walker, syscall decode) and the host-side chokepoints (the IPC crate, the userspace runtime, the DMA pool, and the CAS layer) |
| Concurrency testing | **Loom / Shuttle** | userspace servers and the IPC crate |
| Adversarial input | **cargo-fuzz** | wire and on-disk decoders, the ELF loader, and mount/recovery |
| Baseline | **Miri + proptest** | everything, the chunker and prolly tree especially |

**TLA+.** The storage model's state is the two superblock slots, the chunk-store set, the WAL, and per-ref flushed/unflushed status; it covers partial flushes (one ref's new root committed while another's overlay is unflushed) with the invariant: after any crash, the recovered state equals the committed roots plus a replay of all WAL records not covered by the committed head. This equality is checked as an **action property over the recovery step** — relating the reconstructed overlay to the durable roots and the surviving WAL — so the model verifies that recovery *reconstructs* the committed state, not merely that its ingredients remain durable. This revision's verification work adds that action property; without it the model's invariants constrain only the durable substrate, and a no-op recovery satisfies them — so the property carries a negative control, a no-op recovery that must fail it. The single durability assumption — **fsync means fsync** (§4.8) — is a labeled `ASSUME` in the model rather than an encoding left implicit in the crash semantics. The revocation model includes channel queue slots, so "revoke destroys all descendants" is checked with in-flight caps included.

**Verus.** The kernel object core and the host chokepoints verify without bound. The verified surface over the storage layer covers WAL framing, maximal-run replay coverage (every unflushed record lies in the replayed span), and A/B slot selection (commit writes the non-live slot; recovery's tie-break matches the model). This revision splits each record's **structural decode** out of its hash wrapper and verifies it like the other on-disk decoders (§3.7), leaving only the BLAKE3 hash and the superblock checksum — the content-integrity primitives — uninterpreted; the commit routine itself is plain Rust over those verified decisions, so the *global* replay-equality invariant over the acked-write/WAL-log state remains the TLA+ model's, with only the *local* per-call projection of the recovery walk mechanized on the real recovery code (§6.1).

**Baseline.** Round-trip and canonical-form properties are the natural proptest targets: the same contents produce the same tree, regardless of edit order. The canonical-form oracle for decoders is decode-then-re-encode reproducing the input bytes, since accepting a non-canonical encoding would silently break hash-is-identity. Mount is total over arbitrary device contents (§4.5); committed corpora and every promoted crash artifact become regression tests.

Since no verified Rust compiler exists, end-to-end guarantees are best-effort; the tiering concentrates effort where the system's correctness pivots — the commit protocol and the cap machinery.

### 6.1 Verified vs. trusted: the proof boundary

Where Verus proves a property it proves it in full: rights-mask monotonicity is universally quantified, not sampled; the revoke walk structurally forces every transitive descendant gone, and its *preemptibility* is now mechanized too — bounded per-step safety in Verus (each `revoke_step` quantum preserves every cspace invariant, deletes only empties, and makes progress, yielding no live descendant on completion), with the cross-restart interleaving and completion-under-the-derive-guard modeled in the TLA `CapRevocation` model (the leaf-first deletion order checked at every preemption point, the marked walk proven to terminate); the channel FIFO and the notification waiter queue are faithful deterministic encodings; mount geometry validation is total over arbitrary bytes. But several spec safety properties are delivered only up to trusted-base seams — the same discipline §4.8 states for "fsync means fsync," collected here so that a property routed to trust is not mistaken for a mechanized one.

The boundary is not fixed, and each seam below is tagged accordingly. A **[trusted]** seam is permanently outside the verified surface: it rests on construction or on a labeled axiom, not on a stored-object invariant, and is the irreducible remainder. A **[verifying]** seam is trusted at this revision's blessing but is being drawn into the verified surface by this revision's verification work; its line here reads as trusted until that work lands and as mechanized after, and each names the property that closes it. A mixed seam tags its parts. This revision moves four such parts in: the cap-side map (c), the priority-ceiling gate (d), the per-record structural decode (e), and the model's replay-equality (e).

- **(a) Physical-region exclusivity. [trusted]** "No outstanding cap references the region" is operationalized as "the untyped has no immediate CDT child." That this implies every cap into the carved region is a CDT descendant of the untyped holds by construction — the only frame-creation path records the untyped as the cap's parent — rather than as a stored invariant, because the object seam carries no physical-memory model (§2.2).
- **(b) Cross-root untyped non-overlap. [trusted]** Disjointness within one untyped is proven (watermark monotonicity), and sub-untyped and sibling disjointness follow from parent containment. The disjointness of the *independent* root untypeds set up by the boot shell is a boot-setup axiom: their base/size constants live in `unsafe` code with no global frame table, so their non-overlap and the integer-to-pointer step are trusted (§2.5).
- **(c) Cap-to-page-table correspondence, mapping, and clearing.** The cap-side unmap is proven over object state — deletion drives an unmap at the coordinates the cap records. The matching cap-side map is now mechanized: this revision moved the map-time bookkeeping — a derived copy starting unmapped, and mapping recording the entry's coordinates in the cap — behind the same kind of verified object operation (the `map_frame` op, term-for-term the mirror of the unmap branch, driving the page-table write through the same kind of store seam), making the cap-side guarantee symmetric where only the unmap half was mechanized. The real writing and clearing of page-table entries is proven separately over raw page-table memory. What stays **[trusted]** is the join — that the cap's recorded mapping is the true entry location and that map and unmap truly write and clear it — which lives in the unverified kernel store (§2.5).
- **(d) Thread-lifecycle shell.** The kernel core mechanizes the report record's mutation (first-write-wins, absorbing), its sole mutator, and its survival of object destruction. The spawn-time priority-ceiling gate — the refusal to start a thread whose requested priority exceeds its thread cap's ceiling (§5.4) — is now mechanized: this revision moved the gate out of the syscall shell into a verified kernel operation (the refusing `set_priority` op — an over-ceiling request returns `Err` and leaves the thread untouched, an accepted one writes a priority proven `<= ceiling`), joining the cap-carried ceiling and its monotone attenuation, which are already proven. What stays **[trusted]**: the "suspended, never rescheduled" state (exception entry, syscall exit, scheduler), the anti-forgery and anti-suppression access control (the rights gates plus the spawn-time cap-distribution convention), and the exit/read-report syscall dispatch and register marshalling (§5.1, §5.3).
- **(e) Storage-recovery content coverage.** The verified surface over the WAL and superblock covers the in-bounds, totality, and maximal-run structural properties. Both parts flagged this revision are now mechanized: each record's **structural decode** is split out of its hash wrapper and verified like the other on-disk decoders (§3.7); and the model's **replay-equality** is mechanized this revision by the recovery-step action property (§6) that relates the reconstructed overlay to the durable roots and surviving WAL — earlier the model constrained only the durable substrate, so a no-op recovery passed, and the new property fails that case, demonstrated by a committed negative control. The commit routine itself stays plain Rust over the verified decisions, so the *global* replay-equality invariant — over the acked-write/WAL-log state, resting on the trusted Store-lifetime join, content coverage, and fsync-means-fsync — remains the model's; what is now mechanized on the real code is the *local* per-call WAL-byte/queue projection of the recovery walk (the rebuilt run equals the maximal seq-continuous content-valid post-head skeleton), named as an `ensures` on the recovery routine and pinned by a committed off-by-one negative control. What stays **[trusted]**: per-record content acceptance on the real code; the **BLAKE3 hash and superblock checksum** as content-integrity primitives — the only part of the record seam left uninterpreted once the structural decode is split off; and the durability axiom — **fsync means fsync** (§4.8), named as a labeled `ASSUME` in the storage model rather than left implicit in the crash semantics.

---

## 7. Toolchain and development environment

**No LLVM/clang fork.** Rust cross-compiles to bare-metal aarch64 with a custom target JSON plus `-Zbuild-std`; stock clang already accepts `--target aarch64-unknown-none-elf`. A C toolchain port is not needed, since userspace is pure Rust.

**Virtual machine:** QEMU `-machine virt`, with `-accel hvf` for near-native speed on the M1 and TCG retained as a deterministic fallback for single-stepping. Device set: PL011 UART, GICv3, ARM generic timer, virtio-mmio block device. Debugging is via QEMU's gdbstub.

**Host-side image tooling:** a `mkfs`/populate tool that runs on macOS, reuses the storage crates, and builds the initial disk image (superblocks, ref table, an initial snapshot of a directory tree containing the demo programs).

**Console.** The user-facing console is a userspace UART driver holding the PL011 IRQ and MMIO caps; a "console cap" is a channel to that driver, and a shell does all terminal I/O over it, wired under `stdin`/`stdout` (§5.1). This is the sanctioned path, and what every other component assumes.

The kernel separately retains a **minimal debug-print path to the UART**, exposed to EL0 as a few **debug syscalls** — put a character, write a buffer, read a character. These are a deliberate **early-bring-up scaffold for the kernel's own diagnostics**, not a user-facing authority: they are ambient, callable by any EL0 thread outside the capability model of §2, and that ambient reach is why they are scaffold and not architecture. They exist so the system is observable and minimally interactive before the userspace console and its prerequisites are built — the device-interrupt-to-notification path a receive side needs (§3.6) and the named-grant table that delivers the console cap under standard names (§5.1). The carve-out is time-boxed: once the userspace console driver lands, the debug syscalls are gated off for EL0 — kept, if at all, only for kernel-internal panic reporting — closing the ambient-authority hole. Until then, a shell's use of them is a disclosed, temporary deviation from §2, not a blessed user-facing path.

*Status (C-M9).* The userspace console driver (`user/console`) is implemented, and the shell does all terminal I/O over the console channel wired under `stdin`/`stdout`. The time-box has therefore expired and the exit condition is met: the ambient `getc` (input) syscall is **removed**, and `putc`/`write` (output) are **gated behind a `debug-log` build feature** (default-on for dev images) used only by pre-console server diagnostics and panic reporting — the "kept, if at all, only for kernel-internal" clause. The kernel UART is now the kernel-internal panic/fault/boot path only. This closes the ambient-authority hole for the user-facing path (audit S-8).

**Userspace tooling:** a command-line shell with built-ins for the core operations (run, snapshot, rollback, list and read through a handle).

---

## 8. Considered but not in the current spec

This section collects everything deliberately outside the current system: goals not pursued, alternatives evaluated and rejected, designs deferred to later work, and approaches that earlier revisions or implementations used and have since been replaced.

### 8.1 Non-goals

- POSIX compatibility (inspiration only).
- Graphics.
- Networking.
- SMP (PSCI makes later addition straightforward).
- Real hardware. The target is the QEMU `virt` machine; with it, DMA is host memcpy and therefore cache-coherent, so cache-maintenance operations are omitted and owed — alongside SMP and PSCI — when real hardware arrives.

### 8.2 Rejected alternatives

- **Orthogonal persistence (KeyKOS/EROS-style whole-system checkpointing).** Its recovery unit — the periodic global checkpoint — is the wrong granularity for a system whose headline features are semantic, per-branch versioning; it makes bugs durable, defeats crash-only recovery, and carries cross-cutting kernel complexity (prepare/deprepare, consistent cuts including in-flight IPC) that the microkernel split is meant to avoid. The prolly-tree commit machinery would have to be built on top of it anyway.
- **Kernel badge mechanism for server-minted caps.** The session pattern already gives every client a private channel, so the channel is the identifier; badges would add CDT-entangled mint logic and a badge-recycling problem to the verified core to solve a problem the IPC design does not have.
- **Sealed bearer tokens as the durable representation of storage authority.** Data-as-authority dissolves confinement: every information leak becomes an authority leak, and "what can this process touch?" stops being answerable. Claim tickets (§2.4) are the deliberate, one-shot, short-TTL exception.
- **Copy or late-binding semantics for caps in flight.** Copy doubles ownership and entangles both sides' resource failures; late binding resolves authority at receive time, opening a TOCTOU gap and requiring dangling-reference machinery. Move keeps ownership singular at every instant (§3.4).
- **Client-granted bulk-transfer buffers.** Having the client own the transfer buffer and grant the server a mapping hands every client a way to wedge the server: revoking the grant (or dying mid-request, since parent cleanup revokes a dead child's untyped) unmaps the server's view, and the resulting fault suspends it. Grant direction is fixed the other way (§2.5).
- **A kernel copy-between-aspaces primitive for bulk data.** It avoids the shared mapping but reintroduces per-byte kernel work plus a long-running, preemptible/restartable copy loop in the kernel. The grant-direction rule gets the same safety without it.
- **A kernel process object.** Its defining feature would be dynamic membership — threads joining, leaving, and dying independently through the CDT — which requires an observer list with unlink-on-destruction invariants correct against concurrent teardown, for thin value: the parent necessarily holds every thread cap it created, aspace and cspace are already first-class objects, and the kernel would maintain an abstraction it never otherwise consults. Threads carry the reports instead (§5.1).
- **Kernel-synthesized fault/exit messages into a channel.** The natural translation of seL4's fault endpoint to asynchronous channels is unimplementable under this spec's own rules: a fault report can neither block (it originates on the exception path) nor be dropped (§3.3) nor allocate (§3.6), so the queue-full case has no answer short of pre-reserved per-thread queue slots. Fixed thread binding slots plus a preallocated report record deliver the same information with none of that.
- **Global memtable.** Makes snapshot latency hostage to unrelated refs' traffic and recreates a shared exhaustible resource; per-ref overlays under a global budget get the batching anyway, at the commit (§4.4).
- **Hash-based snapshot identity (git-style commit objects).** Embeds parentage into identity, so routine history rewriting would churn every descendant's identity (§4.7).
- **Persistent inodes and hard links.** Inode numbers depend on allocation order, so identical logical states reached through different histories would encode and hash differently — incompatible with canonical trees. Dedup already provides the storage benefit; runtime file identity is provided by ephemeral server-side IDs instead (§4.9).
- **Flat path-keyed store (one global tree over full paths).** Directory renames become O(subtree) key rewrites and subtree caps degenerate into checked key-range policy; nested per-directory trees give O(depth) moves and confinement by unreachability (§4.9).
- **Normalizing or case-folding name equality.** Any equality coarser than byte equality makes stored bytes depend on insertion history, breaking canonical form (§4.9).
- **Refcounting garbage collection.** Structural sharing makes count maintenance a write-amplification problem with its own crash-consistency burden, and history rewriting would cascade decrements over millions of nodes (§4.6).
- **Synchronous rendezvous IPC.** A simpler kernel, but a worse fit for Rust async userspace (§3).
- **Byte-range file caps.** Chunk boundaries give no structural help, truncation has no clean semantics, and the use cases are thin (§2.3).
- **An LLVM fork.** Unnecessary; Rust cross-compiles to bare-metal aarch64 directly (§7).

### 8.3 Deferred / future work

These are intended later and have a recorded design or shape, but are not part of the current system.

- **Transactional commits on data roots** via compare-and-set on the root (§4.4). The ref-table half — generation-guarded batches — is specified (§4.7) and lands in this revision's work; the data-root half is deferred.
- **Persistent sturdy refs:** authority that survives reboot outside the boot-time grant path. It would be built atop the claim-ticket mechanism (§2.4), not as the default representation.
- **Multi-window sessions (grow-only):** the server may grant additional windows on a live session, descriptors address them by index, and reclamation stays session-teardown-only — revoke the window list, return the memory. The descriptor's window-index field is reserved now (§3.1).
- **The kernel wait-set object:** a Zircon-style port adapted to a heap-less kernel. A wait-set is created from donated untyped (capacity = registration slots); registering an object consumes a slot that doubles as an intrusive node in the object's observer list (the registration is the packet). When signals fire, the node links onto the wait-set's ready list — no allocation on event arrival — and since registrations are one-shot (disarmed on delivery, re-armed explicitly), each node is on the ready list at most once, so overflow is impossible by construction. Dequeue delivers (key, observed signals) in FIFO order. Accepted costs: every waitable object grows a dynamic observer list in place of fixed slots, and teardown gains real invariants (destroying a channel must unlink its registrations from all wait-sets, and vice versa), both walks preemptible and correct against concurrent signal delivery — a second intrusive pointer web with its own lifecycle proofs. The IPC crate's reactor API (§3.6) already hides this shape, so the upgrade changes no server code.
- **The IO-space object and IOMMU migration.** An IO-mapping operation mirroring frame mapping, pool-at-creation, with mapping state in the cap so revoking a frame cap unmaps it from IO spaces too — DMA revocation under the one revocation story. Drivers would then see only IOVAs, and the `phys-read` right would be retired simply by init ceasing to grant it; QEMU's virtio-iommu (later SMMUv3 on real hardware) slots in behind the same interface, and the DMA-pool crate swaps backends without driver changes. The irreducible later work: a virtio-iommu control-plane driver (the IOMMU is itself a virtio device, bootstrapped by identity-mapping its own control queues) and an ownership decision (a userspace iommu-server as the DMA-authority broker is the architectural fit). Because the DMA pool is mapped once at driver startup, the steady state needs no IOMMU operations per request — the hot-path concern belongs to zero-copy, deferred with it. Enabling the IOMMU in QEMU is machine-wide, so the rule is: migrate before writing the second DMA driver.
- **Zero-copy DMA granting, and with it per-request bulk-buffer granting.** Exact-fit transient grants are the right shape only when the granted pages *are* the payload rather than a staging window (§2.5, §3.1).
- **Resumable fault handling, demand paging, copy-on-write, and lazy CAS-chunk mapping** via a page-cache server that shares immutable chunk pages read-only across every process mapping the same content. Because the exception path already saves suspended register state, "handler repairs the mapping and resumes" is a pure addition (one resume syscall, no redesign), enabled by — not prebuilt into — the suspend-not-destroy shape (§5.3).
- **Debugger access to suspended threads:** register read/write as additional thread rights (§2.3).
- **Persisted incremental GC marking** for restart survival, and **external-memory marking** (sorted runs) for size — a second protocol worth a TLA+ model, deferred until mark time approaches uptime. If a Bloom filter ever replaces the exact mark set, note the polarity hazard: the resurrection check (§4.6) must not trust Bloom positives, so during sweep it must consult the exact deletion-candidate list instead.
- **Symlinks and a user-facing xattr API.** Symlinks: absolute targets are meaningless without a global root, and subtree-relative resolution is complexity not currently needed. Xattrs: the TLV entry format already reserves optional tags for them (§4.9).
- **Plan 9-style namespace composition:** union/bind of several subtree handles into one view (§4.9).
- **Dynamic service registration / a broker process:** the broker protocol is already defined (§5.2); a real registry implementing it is deferred.
- **IDL-based wire encoding and a stable public syscall ABI**, grouped as one future "non-Rust userspace" effort. An IDL migrates no data (nothing persistent speaks the wire format): write schemas mirroring the message types, add a second codec backend, and bump protocol versions so old and new clients coexist per session. The **version-negotiation mechanism** for that last clause is implemented as of C3 (`ipc/src/session.rs`): the connect request offers a contiguous version range, the server selects the highest common version at the single admission point or refuses cleanly, and the negotiated version is stamped into the existing header field and validated per-message — so what remains deferred under this item is the IDL second codec backend and the stable public ABI, not the negotiation that lets versions coexist. Foreign-language support also needs a stable syscall ABI and a real named-grant-table format (§5.1), so it lands as one deliberate public-ABI effort (§3.7).
- **Per-ref and per-session disk-space quotas**, and with them quota-scoped `statfs` views: an unprivileged holder would then read quota-relative numbers (its own budget and usage) without `stat-store`, which would narrow to the global tier it already denotes (§2.3).
- **Clock setting, drift correction, and NTP.** The time page's `seq` field and seqlock-shaped reader protocol are in place now so these become a seqlock discipline rather than a retrofit (§2.6).
- **Priority donation / MCS-style scheduling budgets**, to address the absence of priority inheritance across asynchronous channels; evaluated when SMP arrives (§5.4).
- **A userspace persistent-process supervisor** ("poor man's persistent process"): periodically serialize a cooperating process's heap and cap manifest into the versioned store, with the rollback unit an ordinary storage commit (§5).

### 8.4 Superseded approaches (development history)

- **Kani (bounded model checking).** An interim mechanized tier covered the kernel core and the host chokepoints under bounded model checking; it found and fixed real defects (a carve overflow, an executable-MMIO encoding). Every target it covered is now proven without bound in Verus, so the Kani tier has been retired. (The kernel also predated the Verus tooling and was first mechanized under Kani before the Verus port; the port's shape — a host-buildable object core, explicit well-formedness predicates, a hardware seam, and no integer-to-pointer casts in the core — is what made it possible.)
- **Rebuild-the-index-by-scan.** An earlier on-disk format rebuilt the chunk index by scanning at mount. This is structurally incompatible with reclamation, because a scan cannot represent holes, so the index is now committed through the superblock like everything else (§4.2).
- **Tail-only index placement.** Placing index frames only at the chunk-region tail deadlocks a store whose tail is exhausted even while GC has freed space elsewhere; index frames are now placeable in any freed extent (§4.2).
- **Prose-only priority monotonicity.** The maximum-controlled-priority ceiling was once described but not proven; the cap-carried ceiling and its monotone attenuation are now verified (§5.4, §6.1).
