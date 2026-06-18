# Eunomia OS — Design Document

Eunomia is an experimental operating system built around three commitments:

1. **Capability-based access to all resources.** No ambient authority; a process's reach is exactly the contents of its capability space plus its storage sessions.
2. **Deduplicated, versioned storage.** Content-addressed chunks under a canonical prolly tree, with snapshots, rollback, and history rewriting as first-class, cheap operations.
3. **Verification where it pays.** No fully verified stack is attempted; instead, a tiered policy applies the strongest affordable tool to each component, with the highest-value protocols modeled before implementation.

**Implementation language:** Rust + assembly.
**Development environment:** macOS on Apple Silicon (M1).
**Target (MVP):** virtualized ARM64 (QEMU `virt` machine). Real hardware deferred.

**Terminology note.** "Capability" ("cap") is used throughout as the conceptual term for transferable, unforgeable authority. Kernel caps are physically slots in cspaces. Storage caps are physically **handles** in per-session tables held by the storage server (§2.4); phrases like "a snapshot cap" mean "a handle denoting a snapshot" — raw hashes are never a form of authority.

---

## 1. Architecture

Eunomia is an seL4-style microkernel system. The kernel's object vocabulary is deliberately minimal:

- **Untyped memory** — regions of physical memory that can be retyped into other kernel objects or frames.
- **Address-space objects** — page-table trees created from donated untyped (§2.5), into which frames are mapped.
- **Threads** — schedulable execution contexts bound to an address space and a cspace, each carrying fixed exit/fault binding slots and a preallocated terminal-report record (§5.1, §5.3).
- **IPC channels** — asynchronous message endpoints (§3).
- **Capability spaces (cspaces)** — per-process tables of capability slots.
- **IRQ handlers** — caps granting the right to receive and acknowledge an interrupt.
- **Notifications** — a machine word of signal bits plus a waiter queue; the event-delivery primitive (§3.6).
- **Timers** — caps to program a deadline that signals a bound notification (backed by the ARM generic timer).

Everything else is a userspace server holding caps: the storage server (which owns the virtio-blk cap), the program loader, the shell, and any future drivers. This split is not just aesthetic — it is what makes the verification strategy (§6) tractable. The storage stack, the most complex component in the system, is ordinary userspace Rust that can be developed and tested on the macOS host with Miri, proptest, Loom, and Shuttle, completely decoupled from the kernel.

At boot the kernel constructs exactly one process, **init**, whose cspace holds all unallocated untyped memory and all device resources (MMIO frames, IRQ caps). Every grant in the running system flows from init; there is no other source of authority.

### Design influences

seL4 (capability mechanics, untyped retype model), Zircon (async channel IPC, pragmatic object model), KeyKOS/EROS (lazy generation-based revocation, adapted for storage caps), git/Dolt/Noms (prolly trees, content addressing), ZFS (birth-time reclamation, end-to-end checksumming), E/CapTP/Cap'n Proto (live-ref vs. sturdy-ref distinction behind the session/handle model).

---

## 2. Capabilities

### 2.1 Addressing

Kernel capabilities are addressed as indices into a per-process cspace. Caps are transferred over IPC channels (§3.4) and inherited at spawn time: a parent constructs the child's initial cspace explicitly. There is no other way to acquire kernel authority.

### 2.2 Revocation — hybrid model

Two revocation regimes, matched to where each kind of cap lives:

**Kernel caps (memory, threads, channels, cspaces, IRQs): seL4-style capability derivation tree (CDT).** Every copy, mint, or derivation records a parent–child edge, threaded through the cap slots themselves. `revoke(cap)` eagerly deletes all descendants, with the kernel walk made preemptible/restartable since it is unbounded. This is required, not optional: retyping untyped memory is only sound if the kernel can establish that no outstanding caps reference the region, and revoke is how exclusivity is proven. Capabilities queued in in-flight messages occupy real, CDT-visible slots owned by the channel (§3.4), so **revocation sees through queues** — the guarantee is unconditional, with no "except messages in flight" caveat. Kernel caps never touch disk, so the CDT's pointer-web nature poses no persistence problem.

**Storage caps (handles held against the storage server): EROS-style versioning plus session scoping.**

- **Mass revocation of a ref** is O(1) regardless of how many handles exist: each ref carries a **generation counter** in the ref table; every handle records the generation at grant time; bumping the counter (on revoke-all or ref destruction) lazily invalidates every outstanding handle on next use. The counter is plain data and persists through the normal commit path for free.
- **Per-grantee revocation** is first-class: delete one handle, or kill one session, and exactly that grantee's access dies (§2.4). The session is the interposition point — the membrane pattern comes built in rather than bolted on.
- **Snapshot handles** denote immutable data. Revoking one (deleting the handle or its session) cuts off future access but cannot claw back what was already read — holding a snapshot handle is morally equivalent to holding a copy of whatever was read through it. This is accepted by design.

For kernel caps, selective revocation finer than the CDT provides remains a userspace pattern: interpose a forwarding process at grant time and revoke by killing it. Revocability of that kind must be anticipated when authority is granted.

### 2.3 Attenuation and derivation lattice

All derivations are **monotone**: authority can only shrink, never grow. This is the invariant that makes "what can this process touch?" answerable by inspecting its cspace and enumerating its storage sessions (§2.4) — a two-step audit, but a complete and trustworthy one.

| Cap kind | Derivations allowed |
|---|---|
| Untyped / memory | sub-range (page-aligned) + rights mask |
| Channel, thread, cspace | rights mask only |
| Storage snapshot handle | subtree |
| Storage ref handle | subtree + rights mask |

**Thread rights bits:** `bind-reports` (configure the on-exit/on-fault slots, §5.1), `read-report` (read the terminal record; later also the debugger's register access — deferred, §8), `manage` (suspend; resume is the deferred §5.3 addition). The §5.4 maximum-controlled-priority ceiling is a value on the cap, not a bit, and attenuates the same monotone way — realized as a `u8` on the thread cap and **Verus-verified** (`kcore::cspace::derive` proves `child.max_prio ≤ parent.max_prio` ∀, exactly like the rights mask; D-B1, doc 70). Kill is deliberately not on the list: destruction is not a thread operation but a property of resource ancestry (§2.2) — a supervisor holding an attenuated thread cap can observe and suspend, while only the funder, sitting above the thread's memory in the CDT, can destroy. Handing a child's main thread cap to a third party, attenuated as desired, *is* the supervision grant — revocable like any other cap.

**Subtree caps are a headline feature.** A handle rooted at a directory denotes an interior node of the prolly tree (internally, a hash; externally, always a handle). Because the wire protocol is handle-relative (§2.4), the holder physically cannot name anything outside the subtree — confinement by unreachability, not by checked policy. Subtree handles on refs work by server-side path resolution with merged commits upward. This subsumes most uses of chroot/jails/bind-mounts.

**Ref rights bits:** `read`, `write`, `may-snapshot`, `may-rewrite-history`, `stat-store`. Snapshot and rewrite are separate bits because history rewriting is destructive enough to deserve its own. `stat-store` is the odd one out, deliberately: it gates **store-global observation** — `statfs(handle)` today, and any future global observable (GC counters, index occupancy, compaction stats) folds under this one bit rather than earning bits of its own. It is the single right whose meaning ignores the subtree its handle denotes: a handle attenuated to one directory but carrying `stat-store` still observes the whole store's space accounting. Monotone derivation holds — the bit strips like any other, is enumerable via `enumerate-session`, and dies with a generation bump — but its *scope* does not shrink with the subtree, documented here so nobody assumes it does. The default posture is deny: delegation helpers strip it, init grants it only to the shell and maintenance holders, and `statfs` without it returns `NoRights`. Ordinary confined programs have no claim on the number — "how much space may I use?" is a policy question, answered by your parent like every other world-question (§5.2), not by reading the store's vital signs. The recorded upgrade: when disk-space quotas arrive (§8), unprivileged `statfs(handle)` returns quota-relative numbers — your budget, your usage, information the holder is entitled to by construction — and `stat-store` narrows to the global tier it was defined as from day one.

**Limits of confinement, recorded honestly.** Confinement by unreachability is a statement about *naming*; it does not extend to what sharing one physical store makes observable. The inventory: free-space accounting is a covert channel between any two clients that can read it (bandwidth bounded by commit rate — the counter moves only when commits land); dedup is an existence oracle (write content C, `sync`, compare space before and after — no drop means someone already stored C: deterministic content fingerprinting across tenants); and timing leaks survive any rights regime — ENOSPC arrival correlates clients, one client's GC (including the post-rewrite trigger, so one client's *prune*) is every client's latency spike until GC goes incremental, and allocator/WAL contention is measurable in principle. `stat-store` exists to break the precision instruments: without readable space counters, the dedup oracle degrades to timing inference against a flush path dominated by two fsync barriers — a large bandwidth cut, not an elimination. The honest statement: rights-gating removes byte-precise observation; only disjoint stores make confined clients mutually unobservable. And a conceptual admission worth naming: `statfs` is checked policy gating an operation that names no tree node — the headline mechanism does not cover global observables, and the bit is the system admitting that enforceably.

**Byte-range caps within files are explicitly excluded** (for memory caps, the MMU provides page-granular ranges in hardware). CDC chunk boundaries give no structural help for arbitrary byte ranges, truncation semantics have no clean answer, and the use cases are thin. A program that wants to share a file header copies it into a fresh object.

### 2.4 Storage capabilities at the boundary: sessions, handles, tickets

A storage cap at the IPC boundary is a **small integer handle, meaningful only relative to the session channel it arrived on** — exactly a file descriptor. The server keeps, per session, a table:

```
handle → (kind: snapshot | ref, target, subtree root, rights, generation-at-grant)
```

Unforgeability comes from the kernel guaranteeing channel identity (move semantics, §3.4, means a channel cap has exactly one holder); the integers themselves carry no authority, so leaking them is harmless. The kernel knows nothing of storage caps — the handle table is plain Rust, host-testable, and adds zero kernel surface.

**The wire protocol is handle-relative.** Operations take the form `read(handle, path, range)`, `open_child(handle, name) → handle`, `close(handle)`, `write(handle, path, offset, …)`, and so on. **Raw hashes never appear as request parameters.** Hashes are internal addresses and integrity proof, not authority; knowing a root hash (from a log line, an audit trail, a ref listing) confers nothing. *(This supersedes earlier phrasing of a snapshot cap "being" a hash — that is true only inside the server.)*

**Delegation along spawn:** the parent funds a fresh channel pair (§3.5) and asks the server, over its own session, to open a new session on the offered endpoint pre-populated with specified handles — attenuated in the same breath (sub-subtree, reduced rights) — then bestows the retained endpoint on the child. One round trip, and the funding rule is uniform: the child's session costs the parent's memory, never the server's.

**Peer-to-peer transfer:** Alice asks the server to mint a **claim ticket** for a handle — a one-shot, short-TTL sparse token. She sends the ticket bytes to Bob; Bob redeems it on his own session and the handle (with its recorded attenuation and generation) materializes in his table. The ticket is the system's *only* bearer-token mechanism, deliberately narrow: one-shot redemption plus expiry bounds the exposure window, and the durable representation of authority never leaves the handle/session regime. (This is CapTP's live-ref vs. sturdy-ref distinction; persistent sturdy refs are explicitly deferred — if a future use case demands authority that survives reboot outside the boot-time grant path, it will be designed atop the ticket mechanism, not smuggled in as the default representation.)

**Audit:** `enumerate-session` is a first-class right, letting a supervisor dump exactly what a session can touch.

**Cleanup:** when a client dies, its client-funded session channel (§3.5) is destroyed with it; teardown signals the server's peer-closed binding (§3.3), and the server drops the whole session table and revokes the session's bulk windows (§2.5, §3.1), reclaiming the memory. No leaked server state, no finalizer protocols.

### 2.5 Memory: frames, mappings, and DMA

**Frames and address spaces.** Frames are retyped from untyped (4 KiB and larger contiguous sizes; contiguity comes free from retype). An address-space object is created from donated untyped, **pool-at-creation**: the kernel draws intermediate page tables from the aspace's own pool, returns `NEED_MEMORY` when the pool is exhausted, and accepts top-ups — one error path, a trivial allocator, and teardown returns the pool with the object. This is the channel-queue pattern (§3.2) applied again; it deviates from seL4's explicit page-table objects but not from seL4's principle (no kernel allocation that isn't user-accounted). What is given up — per-table caps and revocation of individual intermediate tables — is something nothing in this design wants.

**Mapping state lives in the frame cap: one mapping per cap copy, and deleting or revoking the cap unmaps it.** This single rule makes shared memory obey the same revocation story as everything else. The bulk-IPC path (§3.1) is: derive a frame cap copy (attenuated to read-only if desired), send it (§3.4), receiver maps it — and revoking the parent cap unmaps every sharer everywhere, with no special machinery.

**Grant direction: the party whose liveness matters must be the CDT ancestor of any shared mapping.** Revocation flows strictly downward, and death revokes what the dead process funded — so a received (descendant) frame cap can vanish at any instant, by the granter's deliberate revoke or as a side effect of the granter's own teardown, turning the receiver's next load or store into a fault; and faults suspend (§5.3). A server that mapped client-granted memory could therefore be wedged by any client — maliciously, or by simply crashing mid-request, since parent cleanup revoking the dead client's untyped unmaps the server's view under it. The direction for client–server bulk transfer (§3.1) is therefore fixed: **bulk windows are funded from the server's untyped**; the server derives a per-session cap (rights-masked as appropriate), sends it (§3.4), and the client maps. Deleting a child cap never propagates upward, so a client can destroy only its own view — the server's mapping is unconditionally valid for the session's lifetime, and session teardown revokes the session's window caps and reclaims the memory (window lifecycle: §3.1). Accounting lands on the server, where it belongs: per-session window quotas are §3.3's "fairness is the server's problem," enforced at session setup. The direction also tracks trust: a client that maps server memory accepts that the server's death faults it — but it already depends on that server for every byte it stores, so nothing new is trusted; the reverse would have the server extending liveness-trust to every client, which is precisely what a server cannot do. General form: the ancestor of a shared mapping is the party whose liveness dominates the other's (storaged funds the buffers the block driver maps); where neither dominates, the common supervisor funds.

**DMA.** Virtqueue descriptors carry guest-physical addresses, and **DMA does not go through the MMU**: whoever programs a DMA device can, by construction, touch any physical memory in the machine. MVP stance, stated plainly: **a DMA-capable driver is inside the memory-isolation TCB** — its CPU is confined by the MMU; its device is confined by nothing. (For MVP this is a small concession: the only DMA driver feeds the storage server, which already holds every byte of the data.) Mechanism: a distinct `phys-read` rights bit on frame caps gates `frame_paddr(cap) → u64`. Init grants it only to the holder of the **DmaPool crate** — the single place in the system where physical addresses may appear. The crate hands out buffers labeled with opaque "device addresses"; drivers are written against it from day one and never see a PA. The driver owns a bounded, persistent DMA pool and copies to/from the storage server's shared-memory buffers; zero-copy granting is deferred (§8). Kernel "validation" of DMA regions without an IOMMU is rejected as security theater: either hardware enforces, or the driver is trusted — there is no third stance.

**Committed upgrade shape: the IO-space object.** `io_map(frame, iospace, iova)` mirrors `map`; pool-at-creation; mapping-in-the-cap, so revoking a frame cap unmaps it from IO spaces too — DMA revocation under the one revocation story. Drivers then see only IOVAs; `phys-read` is retired by init simply ceasing to grant it; QEMU's virtio-iommu (later SMMUv3 on real hardware) slots in behind the same interface, and the DmaPool crate swaps backends without driver changes. The irreducible later work, recorded so it is budgeted: a virtio-iommu control-plane driver (the IOMMU is itself a virtio device — bootstrap knot solved by identity-mapping its own control queues; shares the virtio-queue crate with virtio-blk) and an ownership decision (a userspace iommu-server acting as the DMA-authority broker is the architectural fit; deferred). Because the MVP DMA pool is mapped once at driver startup, the steady state needs zero IOMMU operations per request — the hot-path objection belongs to zero-copy, deferred with it. Enabling the IOMMU in QEMU is machine-wide for all virtio devices, so the scheduling rule is: **migrate before writing the second DMA driver** (naturally alongside the real-hardware push).

**Real-hardware debt, logged:** QEMU DMA is host memcpy and therefore cache-coherent; cache-maintenance operations are omitted in MVP and owed — alongside SMP and PSCI — when real hardware arrives.

### 2.6 Time

**Monotonic time** comes from the ARM generic timer: CNTVCT is readable from EL0, giving every process a zero-syscall monotonic clock; the kernel timer object (§3.6) programs deadline interrupts for timeouts and the userspace flush timer (§4.4).

**Wall-clock time** uses a one-shot boot read, not a driver: init (which holds all device caps) reads the PL031 RTC once at boot and publishes `(seq, wall_base, cntvct_base, cntfrq)` in a read-only shared frame mapped into every process — a **time page**, vDSO-style. The frame is funded from init's untyped (the §2.5 grant rule in its degenerate, correct form: the supervisor whose liveness dominates everyone's funds the mapping everyone shares) and arrives through the startup block under the standard name `"time"` (§5.1) — no magic addresses. The RTC's one-second granularity puts ±1 s of absolute error on `wall_base`, accepted rather than polled away: retention rules are denominated in hours, not boot latency. `wall_base` is pinned as **UTC nanoseconds since the Unix epoch, signed 64-bit**; wall time = `wall_base + (CNTVCT − cntvct_base) · 10⁹ / cntfrq`, computable by anyone with zero syscalls and zero IPC, and the same representation is what snapshot rows store (§4.7). `seq` is a sequence word, today constant — the page is write-once at boot — one integer spent now (§4.2's birth-generation move) so that deferred clock setting and drift correction (§8) become a seqlock discipline rather than a flag-day retrofit of a page mapped everywhere; the reader protocol ships seqlock-shaped from day one, because the field without the discipline buys nothing. Pre-time-page images, whose snapshot rows hold raw CNTVCT ticks, are not migrated: they are re-created with `mkfs`, and the on-disk format version bump makes the stance mechanical — tick and nanosecond fields are structurally identical, so old images are *refused with a version error*, never silently misread as dates in 1970. All stored time is UTC; timezones are presentation, owned by the shell. Boot-relative timestamps were rejected once retention policies existed: "older than 30 days" must order across reboots.

One storage-server rule: **snapshot timestamps are clamped non-decreasing per ref**, so RTC misbehavior can never disorder the snapshot log. Setting the clock, drift correction, and NTP are deferred (no networking anyway, §9).

---

## 3. IPC

Asynchronous channels, Zircon-style, rather than synchronous rendezvous. Rationale: the userspace is Rust-centric, and async channels compose naturally with Rust async servers; the kernel pays modest extra complexity (message queuing) in exchange for a much friendlier userspace programming model.

### 3.1 Message format

Small inline payload + shared memory for bulk. A queue slot is fixed-size: small header, **256-byte inline payload, 4 capability slots** (MVP values; format constants, not ABI promises). Anything bigger travels through a per-session **bulk window** — a shared-memory region established at session setup via a **server-granted** frame cap (grant direction and revocation semantics: §2.5; concurrent-access discipline: §4.8) — with the channel message acting as a doorbell/descriptor naming `(window, offset, length)`. The bulk path is mandatory anyway — file contents through the storage server must not be copied through kernel messages — so inline messages need only carry control traffic, and per-message kernel work stays bounded.

**Window lifecycle.** Window size is a connect-time parameter (§3.5), granted under the server's per-session quota; zero is legal — a control-only session (a registry connection, say) carries no window at all. MVP grants exactly one window per session, so the descriptor's window field is always 0 — an integer spent now so the upgrade is an extension, not a migration (the §4.2 birth-generation move). The committed upgrade is **grow-only multi-window**: the server may grant additional windows on a live session, descriptors address them by index, and reclamation remains session-teardown-only — revoke the window list, return the memory — deleting the shrink problem (quiescing live descriptors against a disappearing window) rather than solving it. Descriptors are validated against the named window's static extent before any access. Sizing pressure is modest by construction: a window need only cover the bandwidth–delay product of an IPC-plus-memcpy pipeline on one machine — a few maximal chunks — and large writes gain nothing from large windows, because the engine backpressures upstream anyway (overlay bounds and WAL watermarks, §4.4). Implementation note: per-session windows may be page-aligned sub-range derivations (§2.3) of a single arena frame retyped once at server startup, making per-session allocation plain userspace arena management — the server's untyped never fragments, and a window grant (now or in the multi-window future) is just another sub-range.

### 3.2 Queue memory

Queue memory comes from untyped at channel creation: the creator retypes untyped into the channel object, and capacity = donated bytes / slot size. Depth is a per-channel, creator-chosen parameter with an explicit, capability-controlled cost. No kernel-global pool, no shared exhaustible resource.

### 3.3 Send/receive semantics and backpressure

- `send` is non-blocking and returns `FULL` when the queue is full; messages are never dropped (a dropped message could carry a capability — a lost cap is unacceptable).
- Channels expose **readability** and **writability** notifications ("signal me when a message / space appears") and a **peer-closed** notification (raised when the other endpoint is destroyed — required for session cleanup, §2.4). **Teardown always signals:** deleting one endpoint fires the surviving peer's binding, and destroying the whole object at once — its backing untyped revoked, the normal case when a session's funder dies (§3.5) — fires every endpoint's binding before reclamation; the bound notification is a separate object and outlives the channel if separately funded. Afterward a dead endpoint cap yields error returns. Queue memory is touched only through syscalls, so channel death is always delivered as an event, never as a fault.
- Delivery is **FIFO per channel**, with no kernel-side priorities or fairness across messages. Fairness across clients is the server's problem, solved by the session pattern (§3.5).
- On receive, transferred caps are installed into the receiver's cspace; if the receiver lacks free slots, **the receive fails and the message stays queued** — the receiver makes room and retries. Receive-side exhaustion is the receiver's own resource problem, handled locally.
- Blocking send, bounded-retry send, and async `send().await` are userspace library code built on the non-blocking primitives plus notifications. The kernel provides mechanism; policy lives in the Rust async runtime.

Waiting on *sets* of these signals is the job of the event mechanism (§3.6).

### 3.4 Capability transfer: move semantics

A cap leaves the sender's cspace at send time, occupies a slot in the message while queued, and lands in the receiver's cspace at receive time. **At every instant a capability has exactly one owner** — sender, queue slot, or receiver, never two. A sender that wants to keep access duplicates first: a deliberate, auditable act.

Consequences, all load-bearing:

- **Queue slots are real, CDT-visible capability slots** owned by the channel (allocated from the memory donated at creation). Revocation therefore finds and deletes in-flight caps like any other descendants — no special case in the revoke logic, no caveat in its specification (§2.2, §6).
- **Receivers must tolerate null cap slots** — revocation may have emptied them in flight (and senders can lie regardless).
- **Channel destruction destroys queued caps** with ordinary CDT cleanup: the sender moved them out, nobody holds them, they are gone — like cash in a shredded envelope. The implied discipline (don't destroy channels with valuable caps queued) is handled in userspace by a small ack protocol for valuable-cap handoffs. No kernel reverse-path / bounce-back mechanism (a Mach-style tar pit).
- At most 4 caps per message; the limit is structural (preallocated slot layout), not policed.

### 3.5 Sessions and the IPC crate

Servers publish a connection endpoint; **the client funds the session**: it retypes a channel pair from its own untyped (§3.2) and sends one endpoint in the connect request, together with a requested bulk-window size (§3.1) that the server grants — or refuses — under its per-session quota at this single admission point. A connect therefore consumes the connector's memory for queues and the server's for the window, each side funding what the other must not (§2.5); anonymous connects cannot drain a server, because queue memory is the connector's and window memory is bounded by the quota the server itself enforces at admission. Each client buys exactly the queue depth it wants — §3.2's cost model applied at the trust boundary — and the per-client channel remains where per-client queue accounting and fairness happen. Client funding is safe here precisely where §2.5 forbids it for mappings, and one criterion decides both directions — **fund by failure mode**: queue memory is touched only through syscalls, so a client-funded channel's death reaches the server as error returns plus the peer-closed signal (§3.3), an event handled in code; mapped memory is touched by loads and stores, so a mapping's death arrives as a fault (§5.3). What dies as an event may be funded by the untrusted side; what dies as a fault must be funded by the side that cannot afford to fault.

A single **userspace IPC crate** used by every server owns the ergonomics: `FULL` handling, async send/recv, the valuable-cap ack protocol, and message (de)serialization (§3.7). The kernel primitive stays primitive; the ergonomics are solved exactly once. This crate is the first serious Loom/Shuttle target (§6).

Interrupts are delivered to userspace drivers as events through the same mechanism (§3.6).

### 3.6 Event multiplexing: notifications now, wait-sets later

**MVP kernel primitive: the notification object** — a single machine word of signal bits plus a waiter queue. Signalers OR bits in; a waiter receives the accumulated word, which clears. Each channel endpoint carries **fixed binding slots**, configured by the endpoint's holder: on-readable, on-writable, and on-peer-closed each bind to a (notification cap, bit) pair. IRQ handlers bind identically (seL4 precedent); timer objects bind identically (providing wait timeouts and the userspace flush timer, §4.4); **threads bind identically** — on-exit and on-fault slots in the TCB deliver death notices (§5.1, §5.3), with the report record preallocated in the thread object so the no-allocation rule below holds even for dying threads; notifications are also directly signalable from userspace (executor self-wakeup). One trivial object type, three pointer-sized slots per endpoint, **zero allocation on any event path** — and seL4's verification of this object is good evidence it is cheap in a Verus budget. The lost-wakeup discipline (bind, poll once, then wait) lives in the IPC crate.

The structural limit is the word: at most 64 distinguishable sources per waiting thread. Beyond that, bits identify *groups* and a wakeup costs an O(group) scan — `select()`, not `epoll`. Acceptable at MVP scale; the storage server (one channel per session) is the component that will outgrow it.

**The IPC crate hides this shape from day one.** Its reactor API is epoll-shaped — `register(source, signals, key)`-style with O(1)-dispatch semantics — implemented over bit groups underneath. No server ever sees bits, so the kernel upgrade below changes no server code.

**Committed upgrade path: the wait-set object** — a Zircon-style port adapted to a heap-less kernel. A wait-set is created from donated untyped; capacity = registration slots; `register(waitset, object, signals, key)` consumes a slot, which doubles as an intrusive node in the object's observer list — **the registration is the packet** (the epoll-epitem move). When signals fire, the node links itself onto the wait-set's ready list: no allocation on event arrival, and since registrations are one-shot (disarmed on delivery, re-armed explicitly), each node is on the ready list at most once, so **overflow is impossible by construction**. Dequeue delivers (key, observed signals) in FIFO order; multiple workers waiting on one wait-set get natural packet distribution (relevant when SMP arrives). Accepted costs, recorded so the work is budgeted rather than discovered: every waitable object grows a dynamic observer list in place of fixed slots, and teardown gains real invariants — destroying a channel must unlink its registrations from all referencing wait-sets, destroying a wait-set must unlink all its registrations from their objects, both walks preemptible (like revoke) and correct against concurrent signal delivery. This is a second CDT-like intrusive pointer web with its own lifecycle proofs — Verus/Kani candidates, accepted deliberately: correctness over speed, and over being locked into a select-shaped kernel API.

**Hard rule under both regimes: event delivery never allocates.** Fixed binding slots now; registration-as-packet later.

### 3.7 Wire protocol and serialization

Every message begins with a **fixed, hand-defined header**: protocol id, version, opcode, flags, body length. Versions are negotiated once at session establishment; an unknown opcode yields an error reply, never a crash; a breaking change is a new version number, and a server may speak several concurrently. The header layout itself never migrates — it is the layer that makes every other layer migratable.

**Bodies are postcard-encoded via serde** (`no_std`-first, compact, deterministic), behind an encode/decode trait that is **module-private to the IPC crate** — servers and clients construct and consume plain message types and cannot reach the serializer; no ad-hoc encoding, no pre-encoded byte blobs smuggled through. Message types are kept deliberately **boring**: no borrowed lifetimes, no `#[serde(flatten)]`, no untagged enums, no non-string-keyed maps — the subset that maps 1:1 onto any IDL's type system. Capabilities never appear in payloads (they travel in cap slots, §3.4) and storage handles are plain integers, so the format needs no exotic types. Decoders treat all payloads as untrusted and reject trailing bytes; they are cargo-fuzz targets on the host (§6).

**The IDL is the recorded path to non-Rust userspace, not an MVP feature.** Nothing persistent speaks postcard — on-disk formats (TLV entries, superblock, WAL, index) are hand-defined and canonical — so adopting an IDL later migrates no data: write schemas mirroring the message types (a line-for-line translation, ideally making the IDL the source of truth and generating the Rust types), implement a second trait backend, bump protocol versions; old and new clients coexist per-session. Foreign-language support is mostly *not* a serialization problem anyway — it needs a stable syscall ABI, the startup-block layout (§5.1), and protocol semantics — so the IDL lands as part of a deliberate future "public ABI" milestone alongside syscall stabilization (§8).

---

## 4. Storage

### 4.1 Structure

- **Chunking:** FastCDC (gear hash), target chunk size ~16–64 KiB.
- **Addressing:** BLAKE3 over chunk content; hash = address (internally — never authority at the boundary, §2.4).
- **Aggregation:** nested per-directory prolly trees (Merkle search trees): each directory is its own tree keyed by entry name, referencing child directories by root hash (§4.9). Node split boundaries are a function of the hash at the boundary key, so tree shape is **history-independent (canonical)**: the same logical contents always produce the same tree, regardless of edit order. Canonical form is what makes structural sharing, dedup, and diffing work across histories — and it is the property that makes this layer pleasant to specify formally. (Entry schema and encoding: §4.9.)
- **Ref table:** a small tree in the CAS, committed through the superblock like everything else, holding three kinds of entries: **refs** (named branch heads: `name → (root hash, generation counter)`), the **snapshot log** (§4.7), and **tags** (§4.7).

**Persistence model:** processes are ephemeral; storage is persistent. This is deliberate (orthogonal persistence was considered and rejected — §8). Durability and versioning happen at semantic boundaries (commits, snapshots), per-branch, under user control. Processes are cheap, disposable, and reconstruct in-memory state from canonical persistent state at startup — crash-only software as a default property.

### 4.2 On-disk layout

1. **Two superblock slots (A/B)** at fixed locations, one block each. A superblock holds: magic, format version, monotonically increasing **generation number**, ref-table root hash, WAL head position, references to the durable chunk index and free-extent list, and a **checksum over the whole superblock**.
2. **Write-ahead log (WAL)** region — replay buffer only; never the commit mechanism.
3. **Chunk store** — append-friendly region of write-once chunks and tree nodes, plus a **durable index** mapping hash → (offset, length, **birth generation**) — superblock-referenced and self-verifying, committed through the same flip as everything else (format v2; the rebuild-by-scan shortcut of the first implementation was structurally incompatible with reclamation, since a scan cannot represent holes and GC exists to make holes). The birth generation (superblock generation at append time) is reserved from day one: it makes "older than the GC epoch" well-defined, makes the live-by-fiat rule checkable, and is the hook for incremental GC and birth-time pruning later (§4.6). Index format changes are migrations; the field costs one integer now.
4. **Free-space accounting** — a durable free-extent list, superblock-referenced and committed like the index.

The generation-checksummed A/B superblock flip, preceded by an fsync barrier, is the **single atomicity mechanism for the entire system**: writes, snapshots, ref updates, GC results, and history rewrites all commit through it. One mechanism to implement, one to verify.

**Index commitment rules,** learned at first implementation and now normative:

- *Self-reference:* the index frame records the free list, yet placing the frame consumes free space — resolved by an upper-bound size estimate plus explicit padding so the frame fills its extent exactly.
- *The deferred-reuse invariant:* **no extent freed by commit N may be reused until N's barrier-2 has landed.** Otherwise a crash plus a dedup index-hit can resurrect overwritten bytes. This is the superblock-alternation rule revealed as the general law of all freed space — superseded index frames and swept extents alike — and it is stated here as a named invariant of the commit protocol, not an index detail.
- *No wedging:* index frames must be placeable in freed extents anywhere in the chunk region, never tail-only — tail-only placement deadlocks a store whose tail is exhausted even while GC has freed plenty (the first implementation had exactly this bug; a test caught it).

### 4.3 Mutation path

Writes never touch the tree directly.

1. **Memtable.** Each write lands in a per-ref in-memory overlay (§4.4) keyed by (file id, offset range), also recording creates/deletes/renames; per-file overlays are interval maps, since reads consult the overlay first and fall through to the immutable tree — an LSM read path whose bottom level is the prolly tree.
2. **WAL (durability before flush).** If a write must survive a crash before the next flush, its record is appended to the WAL and the WAL is fsynced before acknowledgment. Per-record checksums allow torn tails to be discarded safely on replay.
3. **Flush** (triggers and scheduling: §4.4) — turning one ref's frozen overlay into immutable structure:
   - Freeze that ref's overlay; open a fresh one so the flush doesn't block writers.
   - For each dirty file, **re-chunk the affected neighborhood only**: back up one chunk before the first dirty byte, run the chunker forward, and stop when an emitted boundary coincides with an existing one (CDC self-synchronization guarantees this within a few chunks). A 200-byte edit in a 1 GiB file yields ~2–4 new chunks.
   - Hash new chunks; index hit → dedup (reuse), miss → append to chunk store. Chunks are never modified in place — this write-once discipline is the root of crash safety.
   - Path-copy upward through the prolly tree to a new root hash. Batching in the memtable means many dirty files in one directory rewrite that directory node once.
   - Update the ref table (another small path copy) to point the ref at the new root. Nothing on disk references any of this yet; a crash here leaves only unreachable garbage.
4. **Commit.**
   - **Barrier 1:** fsync the chunk store and index appends. No superblock may mention chunks that aren't durable.
   - Build the new superblock: generation+1, new ref-table root, WAL head advanced past the **contiguous prefix of records whose effects are now flushed** (the head is pinned by the oldest unflushed record — see §4.4; this is also how the WAL truncates), fresh checksum.
   - Write it to the **older** slot (always alternate; never overwrite the current latest commit).
   - **Barrier 2:** fsync the superblock. Only now is the commit real and acknowledgeable.
   - Nothing is freed on the write path, ever. Reclamation is GC's job exclusively; this separation keeps the write path simple enough to verify.

A commit may carry any number of freshly flushed ref roots: batching across refs happens at the commit, not in the memtable.

### 4.4 Memtable and flush policy

**Per-ref overlays under a global byte budget, charged to sessions.** Per-ref (rather than one global memtable) keeps snapshot latency independent of other refs' traffic, makes "who is consuming the buffer" a queryable per-session fact, and keeps each freeze small. The global budget exists because memory is finite; per-ref soft quotas under it exist for containment — both are numbers in one allocator, not separate mechanisms. (For MVP with a single `main` ref this degenerates to a global memtable; the API is per-ref from day one because freeze granularity is real surgery to retrofit.)

All `rw` handles to a ref share its overlay; write ordering is server arrival order, **last-write-wins, no multi-operation transactions** (a compare-and-set-on-root commit is a possible later extension, recorded in §8 as deferred).

**Bounds** are denominated in bytes of dirty overlay (the unit that governs memory, recovery-replay time, and read-path overhead alike), with an operation-count secondary bound so metadata storms can't hide under a small byte count. On hitting a bound: **backpressure, not eviction** — the write gets `FULL`/blocks at the IPC layer while a flush runs. (There is no eviction; the only way overlay leaves memory is by becoming tree.)

**Flush triggers, in priority order:**

1. **Explicit:** `sync` or `snapshot` on a ref flushes that ref synchronously. A snapshot must name a tree hash, so snapshotting *forces* a flush of that ref — non-negotiable semantics. **Rollback** with a dirty overlay flushes too — into the *abandoned* pre-rollback root, keeping the WAL coherent (the records' effects become committed tree, so the head can advance), and only then re-points the ref; the abandoned root is ordinary garbage for the next GC. (Discarding the overlay would strand acknowledged WAL records; refusing would make rollback unusable under background writes.)
2. **WAL pressure.** The WAL is one global sequential region whose head can only advance past records whose effects are flushed; the tail is therefore **pinned by the oldest unflushed record**, and an idle ref with one ancient dirty byte can pin the whole log. Rule: when WAL usage crosses a watermark, **flush the ref pinning the tail**, repeat until comfortable. The server tracks per-ref oldest-WAL-position as the flush scheduler's sort key. (This is checkpoint scheduling by oldest-dirty-LSN, as in Postgres/InnoDB.) Two edge cases are normative, not implementation discretion: a record larger than the entire WAL region bypasses the log and commits synchronously — full flush plus superblock flip — before acknowledgment; a full WAL flushes everything and resets the log.
3. **Size pressure:** per-ref quota or global watermark crossed → flush the biggest offenders. Low/high watermarks: start flushing at the low mark so writers rarely hit `FULL` at the high one.
4. **Timer:** a staleness bound so a quietly dirty ref eventually becomes committed tree.

**Defaults** (tunable; the mechanisms above are the fixed part): per-ref soft bound 8 MiB, global budget 128 MiB, WAL 64 MiB with flush-the-pinner at 50%, timer 30 s. The tension the numbers balance: frequent flushes amplify writes (the same directory spine path-copied repeatedly, each superseded root instant garbage); rare flushes cost memory, recovery-replay time, and dedup misses within the unflushed window. Coalescing wins comfortably at MVP scale.

### 4.5 Crash recovery

Read both superblock slots; discard any failing checksum (a torn superblock write can only damage the slot being written — the other is a complete older commit); take the survivor with the higher generation. Its ref table defines reality. **The checksum detects torn writes; it authenticates nothing** — there is no secret in it, and a checksum-valid superblock proves a complete write, not a write by this system. Mount therefore validates every geometry field of the surviving superblock — index and free-list locations, WAL region, chunk tail — against the one ground truth it holds, the actual device length, with checked arithmetic, at a single chokepoint immediately after checksum verification; only past that point may downstream code size allocations or reads from superblock fields. (Untrusted fields must never vouch for each other: a length gated only by `chunk_tail` is untrusted data validated against untrusted data.) Replay the WAL from the recorded head to rebuild per-ref overlay state for acknowledged-but-unflushed writes; discard checksum-failing tail records (never acknowledged, so safe). Unreferenced chunks from interrupted flushes are invisible and reclaimed by the next GC.

There is **no repair logic** — no fsck. Either a commit completed (its superblock checksums and wins on generation) or it didn't (its garbage is unreachable). And recovery is **total over arbitrary device contents**, sealed or not: mount returns mounted-or-refused on any byte string presented as a device — never a panic, never an unbounded allocation, never a read past the device's end. This is the third instance of one rule the spec states for bulk-window descriptors (§3.1) and wire decoding (§4.8 / the fuzz mandate, §6): a length from untrusted input is validated against ground truth before it is used.

### 4.6 Garbage collection

**Mark-and-sweep from live roots, periodic and concurrent.** Refcounting is rejected: structural sharing makes count maintenance a write-amplification disaster with its own crash-consistency problem, and history rewriting — a headline feature — would turn each lineage drop into a cascading decrement walk over millions of nodes. Mark-and-sweep pays only at reclamation time.

**Mechanism:**

1. **Root set:** every ref, snapshot, and tag target in the current committed ref table, plus any roots committed while GC runs.
2. **Mark:** walk from each root, accumulating reachable hashes; prune already-marked subtrees (structural sharing makes the walk cheap across snapshot families). Mark state is an **exact in-memory hash set for MVP** — at MVP scale (gigabytes, ~50 bytes per live chunk entry) this is trivially affordable, and an aborted GC (crash, restart) is **safe by construction**: reclamation happens only at the sweep commit, so losing mark progress loses liveness (reclamation work), never data. In-memory marking is a bet that mark time ≪ server uptime, true by orders of magnitude at MVP scale. Future pressure valves, recorded for later: external-memory marking (sorted runs) for *size*; persisted incremental marking for *restart-survival* — a second TLA+-worthy protocol, deferred until mark time approaches uptime. If a Bloom filter ever replaces the exact set, note the **polarity hazard**: the resurrection check below must not trust Bloom positives (a condemned chunk falsely reading "marked/live" would be reused) — during sweep, consult the sweep's exact deletion-candidate list instead.
3. **Concurrency:** chunks written during GC are live by fiat (checkable via birth generation, §4.2). The one subtle hazard is **dedup resurrection** — a new flush index-hits a chunk the marker has already condemned. Fix: during sweep, a dedup lookup that hits an unmarked chunk is treated as a miss (the chunk is rewritten under the same hash, replacing the index entry). Cheap, local, and it confines all GC/mutator interaction to one point.
4. **Sweep:** delete index entries for unmarked hashes whose birth generation predates the GC epoch; return extents to the allocator; commit the updated index/allocation state via the normal superblock flip. A crash mid-sweep loses reclamation work, never data.

**Policy.** Garbage arrives from three sources; one trigger per source plus a floor:

1. **Steady drip — superseded roots.** Every flush makes the ref's previous root garbage (unless pinned by a snapshot) plus the path-copied spine; rate ∝ flush frequency × tree depth. Trigger: **space watermarks** — below ~20% free, schedule GC; below ~10%, run with elevated I/O priority. Watermarks are primary because they tie the only real cost (running out of disk) to the only remedy. One rule keeps them from thrashing on a store that is simply *full of live data*, where every request would re-trigger a futile cycle: the watermark **re-arms only after the generation advances past the last completed GC** — no new commits, no new garbage, no new cycle.
2. **Cliffs — history rewriting.** A retention pass dropping a month of snapshots creates an enormous unreachable mass in one commit. Trigger: **event-driven** — any `may-rewrite-history` operation (and snapshot deletion generally) sets a GC-requested flag, so the foreground op stays O(small) exactly as promised above while reclamation follows promptly. (Matches user psychology: someone who just pruned is watching `df`.)
3. **The floor:** a periodic trigger (daily, or every N generations) so a lightly-used system still converges; cheap insurance against trigger-logic bugs.

**Rules:** at most one GC at a time — a trigger arriving mid-cycle coalesces into "run again after this one." The sweep is the I/O-heavy phase and the only one taxing the write path (every dedup lookup pays the resurrection check); throttle sweep I/O behind foreground traffic by default. The mark phase is comparatively gentle (reads, heavily pruned by sharing). **MVP scope:** manual `gc` command + the post-rewrite trigger + a crude watermark; exact in-memory mark; no Bloom, no spill, no persistence.

**Rights.** Client-triggered GC requires `may-rewrite-history` on a ref-root handle — reclamation is history rewriting's other half, and the pair travels together. Server-initiated triggers (watermark, post-rewrite, the periodic floor) are the server acting on its own authority and involve no client right. A GC reply reports its own effect (objects and bytes freed) under `may-rewrite-history` alone: maintenance operations may report what they did without `stat-store` (§2.3) — the rights stay independent, with no implication lattice; init simply grants the pair together to maintenance holders.

**History rewriting** is, at the storage layer, merely editing the root set: "forget snapshots on `main` older than 30 days" = one small ref-table edit + commit. GC asynchronously reclaims whatever became unreachable.

### 4.7 Snapshots, tags, and retention

**A snapshot's identity is a stable, server-assigned ID** (per-ref sequence number) — never its content hash and never a hash over its metadata. Two reasons this is structural rather than aesthetic. First, hash-as-identity à la git embeds parentage into identity, so rewriting any snapshot would re-identify every descendant — git's rebase tax, paid on a nightly retention schedule. With row identity, metadata is *editable*: fix a message, re-point a parent after a prune, change a retention class, all without touching anything else. Second, canonical trees make content hash unusable as identity anyway: snapshotting unchanged content twice yields the same root — genuinely different events sharing a root is normal here, so event identity and content identity must come apart.

A snapshot is a **row in the snapshot log** (stored in the ref table, committed via the superblock):

```
(snapshot id, root hash, timestamp, provenance, parent?, message?, retention class)
```

- **Timestamp:** server-assigned at snapshot time, stored as UTC nanoseconds (§2.6); client-supplied times are not accepted. Assignment is clamped per-ref monotone: `ts = max(now, predecessor_ts + 1)`, so a host clock regressing between boots can never make a child snapshot "older" than its parent. The clamp protects exactly what retention needs — per-ref strict order — and is honest about what it cannot fix: a wildly wrong RTC makes "older than 30 days" wrong in absolute terms regardless. (Mechanism: the time page, §2.6.)
- **Provenance:** filled in by the server — which session created it, via which trigger (explicit call, timer policy, pre-rewrite safety snapshot). Non-interactive snapshots identify themselves with no prose: `#412, 2026-06-10 03:00, auto/timer, session=backupd`.
- **Parent:** advisory, single-parent, nullable — "the ref's previous head at snapshot time." Not needed for diff (prolly-tree structural diff works between any two roots regardless of lineage); it buys only presentation (log view, undo chain) and may be freely re-pointed by history rewriting (prune #40, and #41's parent becomes #39 — a one-row edit). No merge commits, no DAG; if merging ever arrives, multi-parent rows are a backward-compatible schema extension.
- **Message:** optional, default empty, never prompted for.
- **Retention class:** `keep` (immune to automatic pruning) | `auto` (subject to the ref's retention policy) | `ephemeral` (first to go). The interactive "choose what survives" flow is: mark survivors `keep`, run the policy — a pure ref-table edit followed by ordinary GC.

Editing rows is privileged uniformly: deletion, parent re-pointing, message and class changes all require `may-rewrite-history` on a ref-root handle (§2.3) — row surgery *is* history rewriting, whichever field it touches. Two deletion semantics are pinned. Deleting a snapshot a tag points at **fails with `Pinned`** — delete the tag first if you mean it; the alternative, cascading to the tag, would quietly expand a row deletion's authority into tag destruction. And retention classes govern *automatic* pruning only: an explicit `may-rewrite-history` deletion of a `keep`-class snapshot succeeds — `keep` is protection from policy, not from its owner — while policy tools (the shell's `prune`, the retention daemon) skip `keep` rows.

**Retention policy itself is a userspace daemon** holding a `may-rewrite-history` handle, expressing rules over timestamps and classes (keep hourly for a day, daily for a month, …). The server stores fields; it does not interpret policy.

**Guarded ref-table batches.** A retention pass is read-then-act — enumerate the log, compute a prune set, issue deletions — and another session may snapshot or edit between the read and the act; individual edits are server-serialized, but the batch can act on a stale view. The remedy is a conditional batch. Each ref carries an **edit version**: a counter advancing on every committed mutation of the ref's entries (head moves, snapshot rows, tags) — distinct from the §2.2 revocation generation and never to be conflated with it, since bumping *that* is mass revocation. Enumerate operations return the edit version; `guarded_apply(handle, expected_version, edits)` applies all-or-nothing within one commit iff the version still matches, else fails carrying the current version and the caller re-reads. The counter is plain data through the normal commit path, the check is one comparison in the single authority over the ref table, and it is the difference between a retention daemon one can reason about and one with a permanent advisory asterisk. Compare-and-set transactions on *data* roots remain deferred (§8); this is only the ref-table half, pulled forward because the retention daemon — the first second client — needs it.

**Tags** name the few snapshots worth remembering: ref-table entries mapping `name → snapshot ID` (not root hash, so they survive metadata edits), acting as `keep`-strength pins. The trichotomy: **refs** name lines of development, **snapshot IDs** name events, **tags** name memorable events.

### 4.8 Integrity

Every layer self-verifies: chunks (hash = address), tree nodes (hashed), the superblock (checksummed). The storage server detects any corruption on read. The single trusted axiom is that **fsync means fsync** — on the MVP target this is a QEMU/virtio-blk configuration under our control (`cache=writeback` with FLUSH honored), and it is stated explicitly as an axiom in the TLA+ model.

Bulk windows (§3.1) add a concurrency discipline: the window stays mapped writable in the client while the server works, so a client can race its own request. Two rules confine the race. **Single-fetch:** the server moves a request's bytes across the boundary exactly once — copy in (or out) and never re-read window memory expecting stability. **Hash-what-you-stored:** content hashes are computed over the server's private copy, never over the window, so a racing writer can corrupt only its own payload — the hash-is-address invariant cannot be broken from outside. The write path already copies into the overlay (§4.3), so both rules cost nothing; they are written down so that "hash straight from the window and skip the copy" is recognized as the vulnerability it is rather than the optimization it looks like.

### 4.9 Tree schema, entry encoding, and namespace model

**Nested directory trees, not a flat path keyspace.** Each directory is its own prolly tree keyed by entry name; entries reference child directories by their root hash. Directory moves are O(depth) — detach a hash, reattach it — and the subtree-handle story (§2.3) is literally true: the holder holds a node and cannot name anything above or beside it. Diff stays cheap and recursive: equal root hashes ⇒ identical subtrees, skip; unequal ⇒ diff the two entry lists (each itself a prolly tree, so large directories diff with equal-node skipping) and recurse only into changed children — O(changes × depth). A directory move diffs as one entry removed plus one added with an identical content hash, which is also the signal for cheap rename detection; identical directories anywhere in any snapshots share their subtree hash outright. Costs accepted: no single global key order (whole-store enumeration is a recursive walk), and balance is per-directory, so resolution depth is actual nesting depth.

**Names** are 1–255 uninterpreted bytes, excluding NUL and 0x2F (purely for display/interop sanity), with `.` and `..` reserved as path syntax — resolved by shells and path-walks, never stored. Identity is bytewise; ordering is memcmp. No case folding, no Unicode normalization — canonical form *requires* byte-equality, since any coarser equality makes the stored bytes depend on insertion history. UTF-8 is convention enforced by tooling (shell, `mkfs`), as is the MVP printable-ASCII restriction: tooling restrictions can loosen freely, while format-level restrictions are migrations. The wire protocol takes **component lists** (`open(handle, ["etc","config"])`); `/` is shell presentation, not a format concept.

**Entry encoding: deterministic TLV.** Mandatory fields (type, size, mtime, content reference), then optional fields as (tag, length, value) triples, **sorted by tag, absent fields contributing zero bytes** — exactly one encoding per logical entry, so canonical form survives extension and new tags never perturb old entries' hashes. MVP defines a single optional tag: a flags word containing the **advisory-executable** bit. A hard cap on total optional bytes per entry (a few KiB) keeps directory nodes directory-shaped regardless of future tags (media types, xattr-like data). No user-facing xattr API in MVP.

An entry: `name → (type: file | dir, flags, size, mtime, content: inline bytes | chunk-list hash | child-directory root hash)`.

- **Small-file inlining:** content ≤ 512 bytes lives inline in the entry. The rule is a pure function of content, preserving canonical form; reading a small file costs no I/O beyond the directory node already fetched.
- **mtime** is server-assigned; there is no atime. Honest cost: metadata participates in hashing, so "same contents ⇒ same tree" is strictly "same contents *and metadata*"; chunk-level dedup is unaffected, and node sharing within a snapshot lineage survives since mtimes there change only when content does.
- **Execute is not a storage right.** The storage server only ever serves reads, and "read in order to execute" is indistinguishable from read. Enforceable execution authority is possession of process-construction caps or access to a spawner (§5). The executable flag is a type hint — PATH lookup, completion — with zero security semantics, and is documented as advisory.
- **No hard links:** identity-sharing links require persistent inode indirection, which is incompatible with canonical trees (§8); dedup already delivers the storage benefit. **Symlinks deferred:** absolute targets are meaningless without a global root, and subtree-relative resolution is complexity the MVP doesn't need.

**File identity at runtime.** The persistent format is purely path-keyed; the "file id" in the memtable keying (§4.3) is an **ephemeral, server-runtime ID** assigned per open file. The overlay keys on it; an ID → current-path map updates O(1) per rename regardless of how much dirty state exists; open handles therefore follow renames. IDs never touch disk. Unlink-while-open: the open handle keeps working against the overlay, but if at flush time the ID resolves to no path, the data is discarded — which is what unlink means here. Rename across refs is a copy with new lineage; rename targeting outside a subtree handle is unnameable and thus denied by construction.

**Namespace model.** There is no global root; a process's namespace is the set of subtree handles it holds. Every storage operation is `openat`-shaped — relative to an explicitly named handle — and no ambient-root operation will ever be added "for convenience." Namespace composition (Plan 9-style union/bind of several handles into one view) is deferred; MVP programs simply receive several handles (§5.1).

---

## 5. Process model

Processes are ephemeral. A process is created by a spawn operation that takes: an ELF image (typically read via a snapshot handle on a storage session), an explicitly constructed initial cspace (and, typically, a pre-populated storage session minted per §2.4), and scheduling parameters. There is no fork. Long-lived services that want durability persist state through the storage server like everyone else.

(Possible future extension, explicitly out of scope for MVP: a userspace supervisor that periodically serializes a cooperating process's heap and cap manifest into the versioned store — "poor man's persistent process," with the rollback unit being an ordinary storage commit.)

### 5.1 Spawn convention and the startup block

A spawned process finds its world in a **startup block**: the first message waiting on a bootstrap channel in cspace slot 0, containing argv and env (byte-string vectors) and a **named-grant table**. Table entries carry a discriminator for the two kinds of names: kernel caps resolve to cspace slots; storage grants resolve to handle numbers on the process's storage session channel (itself in a well-known slot), pre-populated by the parent per §2.4. MVP implements the table as ad-hoc per-binary config blocks; that is acceptable exactly until the public-ABI milestone (§3.7, §8), at which point the table format becomes load-bearing for non-Rust userspace and must be designed for real — recorded so the debt has an owner and a due date.

Standard names: `"root"` (the process's subtree), `"stdin"` / `"stdout"` (deliberately split, so shell pipelines wire A's stdout to B's stdin with neither aware; an interactive console is simply the same channel granted under both names), `"tmp"`, `"storage"` (a connector to the storage server, when granted), and `"time"` (the read-only time-page frame, §2.6). `"cwd"` is reserved; whether the shell passes it or folds it into how it constructs `"root"` is a shell-level choice.

**There is no kernel process object; the thread is the unit of report.** A "process" is a loader/runtime convention — an aspace, a cspace, a main thread, and the untyped that funds them — and the kernel's share of its lifecycle lives entirely in the thread object. Each TCB carries two fixed binding slots, configured by the holder of the thread cap exactly as channel endpoints are (§3.6): **on-exit** and **on-fault**, each a (notification cap, bit) pair, CDT-visible like queue slots so revocation sees through them. Alongside sits a preallocated **terminal report record** — `running | exited(status word) | faulted(cause, faulting address)` — at most one terminal report per thread (suspend-on-fault means no second fault), so event delivery never allocates (§3.6). Two syscalls complete the design: `thread_exit(status)`, the only voluntary stop, recorded by the kernel so a child can neither lie about nor forget its own death; and `read_report(thread cap)`. Spawn therefore returns a **process record**, plain data whose kernel-visible heart is the main thread cap; process status is the main thread's status, by convention. The canonical parent loop: bind on-exit/on-fault to a notification, start the thread, wait, `read_report`, revoke the donated untyped — whole-child teardown by resource ancestry (§2.2); kill was never a thread operation — and reuse the slots. Because bindings are configured by the cap holder and a child holds no cap to its own threads, a child cannot silence or forge its own death notice. Orphans do not exist, because there is no process lifetime independent of funding: parent death revokes the parent's untyped and the entire descendant tree with it; "adoption" is a supervisor having been granted thread caps before the parent died, and a binding whose notification has died is a slot the revoke already cleared — signaling nothing is a no-op. Exit status persists in the TCB until the parent reclaims the thread, and the TCB is parent-funded memory: the reaping incentive is the parent's own budget, and there is no global zombie table to leak.

### 5.2 Service discovery

Discovery bottoms out in the spawn tree, not in a global name table: **parents are the registry** (the Genode discipline — discovery as recursive delegation). Every child's world is whatever its parent put in its startup block, so "where is the storage server?" is always answered by whoever spawned you, and sandboxing a child is simply not granting a name. MVP mechanism: **static wiring** — init holds the storage server's connector cap and bakes it into the shell's startup block; no registry process exists, and init is the only binder.

The broker *protocol* is defined now so a real registry can drop in later without changing clients: a registry is any process speaking `lookup(name) → connector cap` and `register(name, connector cap)` over a channel, where the returned cap is the service's accept endpoint — the broker never proxies traffic (clients connect per §3.5, funding the session pair themselves; the service merely accepts the offered endpoint), keeping it out of the data path and out of the TCB for everything except introductions. Registry channels are attenuable like any other authority: lookup-only rights, name-subset views (this child's registry resolves only `"storage"` and `"log"`). Registration authority is a broker's entire security story; in MVP it is vacuous. Dynamic registration is deferred (§8).

### 5.3 Faults

A faulting thread is **suspended, not destroyed**: the kernel fills the thread's terminal report record — `faulted(cause, faulting address)`; the registers are already saved by the exception path — and fires the TCB's on-fault binding (§5.1, §3.6). Faults and exits are one design literally: two bits on one mechanism, one record, one `read_report`. Supervision delegation is one grant: hand a supervisor an attenuated thread cap (§2.3) and it rebinds on-fault to its own notification — the same move a future pager will use. MVP parents respond only by killing, and that is the correct semantics rather than a compromise: with no swap, no lazy loading (the loader maps programs fully), and fixed-size stacks with unmapped guard regions, there is nothing a handler could legitimately repair — every fault is a bug. That sentence is a **design obligation on every protocol, not an observation about current code**: no process may be put in a position where a peer it does not already trust for liveness can induce its fault — under suspend-not-destroy, an inducible fault is a wedge. The grant-direction rule (§2.5) is this obligation applied to shared memory; any future mechanism that lets one process affect another's address space must re-establish it. Because the exception path already saves the suspended register state, "handler repairs the mapping and resumes" is a pure later addition (one resume syscall, no redesign), and demand paging, copy-on-write, and mmap-style lazy mapping of CAS chunks (a page-cache server sharing immutable chunk pages read-only across every process mapping the same content) are enabled by — not prebuilt into — this shape (§8).

### 5.4 Scheduling

Single-core, **strict fixed-priority preemptive scheduling: 32 levels, round-robin within a level** on a periodic 10 ms tick; idle is WFI. Priority is *authority*: spawn sets a thread's priority bounded by a maximum carried in the spawner's own thread cap (seL4's maximum-controlled-priority pattern), so the priority lattice is monotone like every other derivation (§2.3). **Verification boundary (D-B1, doc 70):** the cap-carried ceiling and its monotone attenuation through `derive` are Verus-verified, and spawn enforces `prio ≤ cap.max_prio` (the ceiling is stamped at retype = the retyper's priority). The one residual seam: priority is a `kcore` `Tcb` field *outside* the verified Store view, so the spawn-time *write* of the gated priority into the TCB is performed by the unverified shell; a machine-checked `ensures tcb.priority ≤ ceiling` (priority into the Store seam + a verified `set_priority`) is the recorded follow-on.

Documented wart, deliberately unsolved: with async channels there is no priority inheritance — a server processes a high-priority client's request at the server's own priority. The MVP answer is convention (servers run above their clients); the real answers (donation, MCS-style budgets) are evaluated at SMP time, not before (§8). The scheduler should be the most boring code in the kernel.

---

## 6. Verification

Tiered policy, strongest affordable tool per component:

| Tier | Tool | Applied to | Notes |
|---|---|---|---|
| Protocol models | **TLA+** | (a) storage commit/recovery protocol, (b) kernel cap revocation | **Before implementation.** (a) State = (slot A, slot B, chunk-store set, WAL, per-ref flushed/unflushed status); the model covers **partial flushes** — one ref's new root committed while another's overlay is unflushed — with invariant: *after any crash, recovered state = committed roots + replay of all WAL records not covered by the committed head*. (b) The revocation model **includes channel queue slots**, so "revoke destroys all descendants" is checked unconditionally, in-flight caps included. |
| Proof-carrying code | ~~**Verus**~~ — deferred | cspace/CDT operations; kernel allocator | **Deviation, recorded (`doc/plans/0_kani-rewrite.md`).** The plan was to write these in Verus dialect "from day one"; that did not happen — the kernel predated the tooling. Kani (below) is now the mechanized tier for the kernel implementation. The rewrite's shape — a host-buildable `kcore`, explicit `wf()` predicates, the `Env`/`Hal` hardware seam, no integer→pointer casts in the core — is exactly what a later Verus port would need, so Verus is **preserved as an option, not foreclosed**. |
| Bounded model checking | **Kani** | The kernel object core (`kcore`), re-checking the CapRevocation TLA+ invariants on the *real* implementation at TLC-scale bounds: cspace/CDT (revoke through queues + TCB slots), untyped retype, channels, notifications, thread reports, the page-table walker, and syscall decode — plus the host-side chokepoints (`urt` time/slots, the `ipc` header, `cas` TLV/superblock, `dma-pool`). | Pinned cargo-kani 0.67.0; CI job `kani`. Found and fixed real defects (carve overflow; an executable-MMIO encoding). Findings and per-harness bounds: `doc/results/2_kani-findings.md` … `8_kani-findings-7.md`. Out of scope by construction (owned by the rows above/below): concurrency, unbounded proofs, and anything behind inline asm / MMIO / the boot path. |
| Concurrency testing | **Loom / Shuttle** | userspace servers and the IPC crate | These tools model std-style primitives only — one more reason complexity lives in userspace. |
| Adversarial input | **cargo-fuzz** | wire/on-disk decoders, ELF loader, mount/recovery | Canonical-form oracle where formats promise one encoding: decode-then-re-encode must reproduce the input bytes (acceptance of a non-canonical encoding silently breaks hash-is-identity, panic or not). Checksum-re-sealing harness variants so coverage reaches past integrity gates — a clean run that dies at the checksum proves the checksum, not the code behind it. Mount is total over arbitrary device contents (§4.5). Allocations bounded by remaining input; corpora committed; every crash artifact promoted to a unit regression. |
| Baseline | **Miri + proptest** | everything; chunker and prolly tree especially | Round-trip and canonical-form properties are ideal proptest targets: same contents ⇒ same tree, regardless of edit order. |

All system APIs ship with precise contracts. Since no verified Rust compiler exists (no CompCert analogue), end-to-end guarantees are explicitly best-effort; the tiering concentrates effort where the system's correctness actually pivots: the commit protocol and the cap machinery.

---

## 7. Toolchain and development environment

**No LLVM/clang fork.** Rust cross-compiles to bare-metal aarch64 with a custom target JSON plus `-Zbuild-std`. Stock clang already accepts `--target aarch64-unknown-none-elf`; a C toolchain port (libc + OS triple) is deferred indefinitely, since MVP userspace is pure Rust.

**Virtual machine:** QEMU `-machine virt`, `-accel hvf` for near-native speed on the M1; TCG retained as fallback (deterministic, better single-stepping). Device set: PL011 UART, GICv3, ARM generic timer, virtio-mmio block device. Debugging via QEMU's gdbstub.

**Host-side image tooling (new deliverable):** a `mkfs`/populate tool, running on macOS and reusing the storage crates, that builds the initial disk image (superblocks, ref table, an initial snapshot of a directory tree containing the demo programs). Without it, nothing is "in the versioned store" for the demo to load. Part of the build system; feeds M2/M3.

**Console:** the kernel keeps a minimal debug-print path to the UART (needed from M0 onward); the user-facing console is a userspace UART driver holding the IRQ and MMIO caps, and "console cap" in the demo means a channel to it.

**Userspace tooling for MVP:** a command-line shell with built-ins for the demo operations (run, snapshot, rollback, ls/cat through a handle).

---

## 8. Rejected alternatives (recorded for posterity)

- **Orthogonal persistence (KeyKOS/EROS-style whole-system checkpointing).** Rejected for MVP and likely permanently. Its recovery unit (the periodic global checkpoint) is the wrong granularity for a system whose headline features are semantic, per-branch versioning; it makes bugs durable and defeats crash-only recovery; and its cross-cutting kernel complexity (prepare/deprepare, consistent cuts including in-flight IPC) is precisely what the microkernel split is meant to evict. The prolly-tree commit machinery would have to be built anyway, on top of it.
- **Kernel badge mechanism for server-minted caps.** The session pattern already gives every client a private channel, so the channel *is* the identifier; badges would add CDT-entangled mint logic and a nasty badge-recycling problem to the verified kernel core to solve a problem the IPC design doesn't have.
- **Sealed bearer tokens as the durable representation of storage authority.** Data-as-authority dissolves confinement: every information leak becomes an authority leak, and "what can this process touch?" stops being answerable. Claim tickets (§2.4) are the deliberate, one-shot, short-TTL exception; durable sturdy refs are deferred.
- **Copy and reference (late-binding) semantics for caps in flight.** Copy doubles ownership and entangles both sides' resource failures; late binding resolves authority at receive time, opening a TOCTOU gap and requiring dangling-reference machinery. Move keeps ownership singular at every instant (§3.4).
- **Client-granted bulk-transfer buffers.** The obvious direction — the client owns its transfer buffer and grants the server a mapping — hands every client a wedge-the-server primitive: revoking the grant (or just dying mid-request, since parent cleanup revokes a dead child's untyped) unmaps the server's view under it, and the resulting server fault suspends (§5.3). Grant direction is fixed the other way (§2.5).
- **A kernel copy-between-aspaces primitive for bulk data.** Solves the same liveness problem by having no shared mapping to lose, but reintroduces per-byte kernel work — the cost §3.1 exists to avoid — plus a long-running copy loop that must be made preemptible and restartable inside the kernel. The grant-direction rule gets the same safety for free.
- **A kernel process object.** Its defining feature would be dynamic membership — threads joining, leaving, and dying independently through the CDT — which requires an observer list with unlink-on-destruction invariants correct against concurrent teardown: the wait-set's pointer-web costs (§3.6), paid one object early, immediately before the cspace/CDT verification pass, and for thin value — the parent necessarily holds every thread cap it created, aspace and cspace are already independent first-class objects, and the kernel would be maintaining an abstraction it never otherwise consults. Threads carry the reports instead (§5.1); "process" is a loader/runtime convention.
- **Kernel-synthesized fault/exit messages into a channel.** The natural translation of seL4's fault endpoint to async channels, and unimplementable under this spec's own rules: a fault report can neither block (it originates on the exception path) nor be dropped (§3.3) nor allocate (§3.6), so the queue-full case has no answer short of pre-reserved per-thread queue slots — allocation machinery on the event path. Fixed TCB binding slots plus a preallocated report record deliver the same information with none of it.
- **Global memtable.** Makes snapshot latency hostage to unrelated refs' traffic and recreates a shared exhaustible resource; per-ref overlays with a global budget get the batching anyway, at the commit (§4.4).
- **Hash-based snapshot identity (git-style commit objects).** Embeds parentage into identity, so routine history rewriting would churn every descendant's identity (§4.7).
- **Persistent inodes and hard links.** Inode numbers depend on allocation order, so identical logical states reached through different histories would encode — and hash — differently: fundamentally incompatible with canonical trees. Dedup already provides hard links' storage benefit; their aliasing semantics are not missed. Runtime file identity is provided by ephemeral server-side IDs instead (§4.9).
- **Flat path-keyed store (one global tree over full paths).** Directory renames become O(subtree) key rewrites and subtree caps degenerate into checked key-range policy; nested per-directory trees give O(depth) moves and confinement by unreachability (§4.9).
- **Normalizing or case-folding name equality.** Any equality coarser than byte equality makes stored bytes depend on insertion history, breaking canonical form (§4.9).
- **Refcounting GC.** See §4.6.
- **Synchronous rendezvous IPC.** Simpler kernel, but a worse fit for Rust async userspace.
- **Byte-range file caps.** See §2.3.
- **LLVM fork.** Unnecessary; see §7.
- **Deferred (not rejected):** transactional commits on *data* roots via compare-and-set (§4.4) — the ref-table half, generation-guarded batches, is pulled forward instead (§4.7); persistent sturdy refs (§2.4); persisted incremental GC marking (§4.6); symlinks and a user-facing xattr API (§4.9); Plan 9-style namespace composition (§4.9); dynamic service registration / a broker process (§5.2); the kernel wait-set object — a committed upgrade path with the design recorded, not an open choice (§3.6); likewise the IO-space object / virtio-iommu migration (§2.5); resumable fault handling, demand paging, CoW, and lazy CAS-chunk mapping via a page-cache server (§5.3); debugger access to suspended threads — register read/write as additional thread rights (§2.3); zero-copy DMA granting, and with it per-request bulk-buffer granting — exact-fit transient grants are the right shape only when the granted pages *are* the payload rather than a staging window (§2.5, §3.1); IDL-based wire encoding and a stable public syscall ABI as one future "non-Rust userspace" milestone (§3.7); per-ref/per-session disk-space quotas, and with them the quota-scoped `statfs` views that give unprivileged holders their own numbers (§2.3); clock setting, drift correction, NTP (§2.6); priority donation / MCS-style budgets (§5.4).

## 9. Non-goals (MVP)

- POSIX compatibility (inspiration only, never adherence)
- Graphics
- Networking
- SMP (PSCI makes later addition straightforward)
- Real hardware

---

## 10. MVP definition and milestones

**MVP demo script:** boot QEMU → kernel brings up init → init spawns the storage server (holding the virtio-blk caps) and a shell (holding a console channel and a storage session pre-populated with a snapshot handle) → shell loads and runs a program out of the versioned store → take a snapshot, modify a file, roll back, show the old contents.

| Milestone | Deliverable | Exit criterion |
|---|---|---|
| **M0** | Boot, UART, MMU, exception handling | Kernel prints over PL011, takes and reports a synchronous exception, runs with MMU on |
| **M1** | Caps + threads + async channels; notification + timer objects with channel/IRQ bindings; CDT revoke; untyped retype | Two userspace threads exchange a message and a cap via notification-driven waiting; revoke verifiably destroys descendants **including a cap queued in an in-flight message** |
| **M2** | Userspace virtio-blk driver (written against the DmaPool crate, §2.5); CAS + prolly tree + commit protocol; session/handle protocol; host-side `mkfs`/populate tool | Storage server passes proptest canonical-form suite on host; crash-injection tests recover correctly in QEMU; TLA+ model (incl. partial-flush invariant) checked **before** this milestone's implementation begins |
| **M3** | ELF loading, spawn-with-caps, shell | Shell connects a storage session and spawns a program from a snapshot handle, with an explicitly constructed cspace |
| **M4** | Snapshot / rollback demo | Full MVP demo script runs end to end |
| **M5** | GC + history rewriting | Dropping snapshots via a ref-table edit triggers GC; manual `gc` and the watermark trigger reclaim space; a crash mid-GC recovers with no data loss |

M4 is the MVP demo; M5 completes the headline feature set (cheap history rewriting is a stated commitment, and it isn't real until reclamation works).

Orderings: the TLA+ commit-protocol model is a prerequisite of M2 and should be done early (it is small); the storage server and the `mkfs` tool can be developed against a file-backed block device on macOS in parallel with M0–M1, since both are pure userspace Rust.