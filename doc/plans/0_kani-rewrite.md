# Plan: rewriting the kernel for Kani verification

**Status:** proposed, not started.
**Spec baseline:** `doc/spec/2_spec_rev2.md` (all § references below).
**Model baseline:** `tla/cap_revocation/CapRevocation.tla` (Spec + TSpec),
`tla/commit_protocol/CommitProtocol.tla` — both TLC-checked.

---

## 1. Background and goal

The verification tier table (§6) names **Kani** for kernel data-structure
invariants. The kernel was implemented without it: the only Kani artifact in
the tree is the toolchain smoke proof in `scratchpad/` (`cargo kani -p
scratchpad`, currently green on cargo-kani 0.67.0). Meanwhile the kernel's
object machinery — the CDT, untyped retype, channel rings, notification
queues, thread reports, address spaces — was written as raw-pointer code
inside the `kernel` crate, which **cannot be built by Kani at all**:

- `kernel/.cargo/config.toml` forces `aarch64-unknown-none-softfloat` with
  `build-std`; Kani compiles for the host and drives CBMC, which has no
  notion of either.
- `asm!`/`global_asm!` appears in 9 of 15 kernel source files (boot,
  exceptions, MMU, GIC, timer, aspace TLB maintenance, …). CBMC cannot
  interpret inline assembly.
- Objects are placed by **integer→pointer casts** (`start as *mut T` in
  `untyped.rs::retype`, PA-derived table pointers throughout `aspace.rs`).
  CBMC's memory model is object/offset-based; int-to-pointer casts are at
  best fragile and at worst unsound under Kani — they must not appear in
  verified code.
- Hardware side effects (TLBI, barriers, MMIO, the scheduler's `static mut`
  ready queues) are entangled with the pure data-structure logic Kani is
  supposed to check (e.g. `cspace::delete` → `aspace::unmap` → `tlbi vae1`;
  `notification::signal` → `thread::enqueue` → `static mut READY`).

This plan restructures the kernel so the data-structure core becomes a
host-buildable crate that Kani verifies exhaustively at small bounds, and
defines the complete catalog of invariants, properties, and behaviors to
verify — derived from the spec and from the already-checked TLA+ models,
which serve as the property source of truth.

**The framing that makes this coherent:** TLC checked the *design* of cap
revocation and the commit protocol as finite transition systems (CapIds=4,
Procs=2, QueueDepth=2; Refs=2, MaxWrites=2). Kani's job is to re-check the
same properties **against the real implementation code**, at comparable
bounds, plus the implementation-only properties the TLA abstraction cannot
see: refcount soundness, pointer-link well-formedness, arithmetic
overflow/panic freedom, and memory safety of the `unsafe` blocks. TLA+
remains the design tier; Kani becomes the implementation tier for the
kernel; proptest/fuzz/Miri keep everything they own today.

### What Kani gives us (and doesn't)

| Gives | Doesn't give |
|---|---|
| Exhaustive proof over **all** inputs/interleavings within stated bounds (slot counts, queue depths, op-sequence length, loop unwinding) | Unbounded proofs (revoke termination for arbitrary trees stays a `debug_assert` + TLA argument) |
| Panic freedom, arithmetic overflow, slice/pointer bounds, UB in `unsafe` blocks (within CBMC's model) | Concurrency (irrelevant: the kernel is single-core, non-preemptible — the concurrency tier is Loom/Shuttle for userspace, §6) |
| Checking the TLA invariants on the real code, not a hand-transcribed model | Liveness/fairness properties (TLC's `ReportMonotone` is a safety property and ports; true liveness does not) |
| Counterexample traces for every failure | Verification of inline asm, MMIO, or the boot path — these stay outside the verified core by construction |

### Relation to the §6 Verus row

§6 assigns cspace/CDT and the kernel allocator to **Verus**, "written in
Verus dialect from day one." That did not happen — the code predates any
verification tooling, which is the misunderstanding this plan corrects. Per
direction, **Kani is now the mechanized tier for the kernel
implementation**. The rewrite below (explicit `wf()` predicates, pre/post
contracts already present as comments, hardware seams, no int→ptr in the
core) is also exactly the shape a later Verus port would need, so the Verus
option is preserved, not foreclosed. When this plan lands, the §6 table and
`CLAUDE.md` are updated to record the deviation.

---

## 2. Target architecture: the `kcore` extraction

### 2.1 New crate

A new workspace member, **`kcore/`** (`no_std`, zero dependencies),
containing the architecture-independent kernel object machinery. It builds
for both `aarch64-unknown-none-softfloat` (as a dependency of `kernel`,
unchanged semantics) and the host (for `cargo kani -p kcore` and ordinary
`cargo test`). The `kernel` crate keeps everything architectural and
becomes a thin shell: boot, exception vectors, MMU bring-up, GIC, UART,
the scheduler's ready queues and context switch, syscall entry, and the
single place where physical addresses become pointers.

What moves into `kcore` (current → new home):

| Today | Moves | Stays in `kernel` |
|---|---|---|
| `cspace.rs` | All of it: `Rights`, `Cap`, `CapSlot`, CDT ops (`cdt_insert_child`, `cdt_unlink`, `slot_move`, `derive`, `delete`, `revoke`), refcounting, cspace init/teardown | — |
| `untyped.rs` | Retype/reset logic, **split** (§2.3 below): pure carve arithmetic + object dispatch over caller-supplied pointers | The PA→pointer conversion at the carve boundary |
| `channel.rs` | All of it: rings, send/recv, bindings, `endpoint_cap_dropped`, `destroy_channel` | — |
| `notification.rs` | All of it: word, waiter queue, `signal`/`wait`/`remove_waiter` | — |
| `thread.rs` | `Tcb`, `Report`, `ThreadState`, `TrapFrame` (plain data), report transitions, bind-slot logic, waiter-queue links | Ready queues, `maybe_switch`, `CURRENT`, the idle WFI loop |
| `aspace.rs` | Table-walk and PTE logic, **rewritten** over a pool-index view (§2.4 below); PTE encode/decode as pure functions | TLBI/DSB sequences, ASID allocation's `tlbi vmalle1`, the PA of the shared kernel L1 |
| `timer.rs` | Armed-timer list and deadline/binding logic | CNTVCT/CNTP register access, GIC ack/eoi |
| `syscall.rs` | Decode + validation layer, **split** (§2.5 below) | SVC entry, trap-frame plumbing, execution against live objects |

### 2.2 Layering rules (the contract that keeps `kcore` verifiable)

These are absolute; CI greps for violations (same spirit as the
no-`unsafe`-without-comment rule):

1. **No `asm!`, no `global_asm!`, no MMIO addresses, no register access**
   in `kcore`.
2. **No integer→pointer casts** in `kcore`. Every raw pointer entering
   `kcore` is produced by the caller (the kernel shell from PAs at the one
   sanctioned boundary; Kani harnesses and host tests from ordinary Rust
   allocations — so CBMC sees only provenance-carrying pointers).
3. **Hardware effects behind a `Hal` trait** with associated no-op ghost
   implementations for host/Kani builds:
   ```rust
   pub trait Hal {
       fn tlb_invalidate_page(asid: u16, va: u64);
       fn barrier_after_map();
       fn barrier_after_unmap();
   }
   ```
   The kernel implements it with the existing `tlbi`/`dsb` sequences; the
   Kani impl records calls into ghost state so harnesses can *assert*
   "unmap invalidated exactly the pages it cleared" instead of merely
   skipping the asm.
4. **Scheduler hook behind a trait** (`fn make_runnable(t: *mut Tcb)`),
   because `notification::signal` must wake waiters without `kcore`
   knowing about ready queues. The Kani impl appends to a ghost list;
   harnesses assert FIFO wake order against it.
5. `kcore` keeps the existing contract comments (pre/post on every
   `unsafe fn`) and **adds executable forms of them**: each module gains a
   `#[cfg(any(kani, test))] mod wf` with the well-formedness predicates of
   §4, used as assumptions and assertions by harnesses and as
   `debug_assert!` hooks in stress tests.

### 2.3 The untyped/retype split

`retype` today computes placement (`(base + watermark + align - 1) &
!(align - 1)`, bounds checks, watermark bump) and immediately writes the
object through `start as *mut T`. Split:

- **`kcore::untyped::carve(base, size, watermark, ty, param) ->
  Result<Carve, RetypeError>`** — pure `u64` arithmetic returning
  `{ start, end, bytes }`. Fully verified: no overflow for *any* input
  (including adversarial `param`, which arrives raw from user register
  `a[2]`, `syscall.rs:147–166`), alignment of `start`, `[start, end) ⊆
  [base+watermark', base+size)`, disjointness from all previously carved
  ranges, watermark monotonicity.
- **`kernel`**: converts `carve.start` to pointers (the one int→ptr site,
  with its invariant comment) and calls the `kcore` object initializers
  (`CSpaceObj::init`, `Channel::init`, …), which take pointers and are
  verified separately with harness-allocated memory.
- CDT insertion of the new cap, rights inheritance (frame inherits,
  thread gets `THREAD_ALL`, sub-untyped strips `PHYS` — §2.5's
  no-PA-on-ordinary-chains argument), and the channel two-endpoint dance
  stay in `kcore` and are verified.

### 2.4 The aspace rewrite (the deepest change)

`aspace.rs` walks page tables by masking PAs out of descriptors and
casting them to pointers (`l3_slot`, `l3_lookup`) — unverifiable as
written, and also the code where a logic bug is literally a
memory-isolation hole. Rewrite the walk to operate on the table pool as an
**indexed slice**:

- An aspace's tables live, as today, contiguously after the header
  (pool-at-creation, §2.5 of the spec). The walker addresses them as
  `tables: &mut [[u64; 512]]` with **pool indices**, not PAs: a table
  descriptor's payload is converted index↔PA only at the `kernel`
  boundary (`pa_of_table(pool_base, idx)` / `table_of_pa`), keeping the
  on-hardware descriptor format byte-identical. The L1 special case (two
  shared kernel entries, copied at init) is handled by the shell passing
  the two entry values in as opaque `u64`s.
- **PTE encode/decode become pure functions** (`pte_encode(pa, perms) ->
  u64`, `pte_decode`), verified for: AF always set, `PXN` always set
  (user memory is never EL1-executable), `PERM_DEVICE ⇒ UXN` (device
  never executable — currently *documented* on `PERM_DEVICE` but not
  enforced by `map`'s bit assembly; the harness will settle whether
  that's a real gap, see §7), EL0 AP bits never grant kernel-RW-only
  encodings, address bits round-trip.
- `map`/`unmap`/`range_mapped` are then verified as a functional model:
  see §4.5.

This is the riskiest phase (it touches translation-table code the whole
system stands on), which is why it is sequenced after the CDT work and
gated on the full QEMU suite (§6).

### 2.5 The syscall split

`syscall.rs` (633 lines) mixes register decoding, argument validation,
capability lookup/rights checks, and execution. Split decode+validate into
a pure layer (`kcore::sysabi` or a host-compilable `kernel` module):
`fn decode(nr: u64, args: [u64; 7]) -> Result<Sys, SysError>` — total over
all register values (Kani: no panic, no overflow for any `u64⁷`), plus the
per-call validation predicates (slot index in range, `ObjType::from_u64`
totality, message length ≤ `MSG_PAYLOAD` before the `as u16` truncation in
`channel::send`, VA alignment checks). Execution stays in the kernel and
consumes the typed `Sys` value. This makes "no user-controlled value
reaches kernel arithmetic unvalidated" a checked property instead of a
review convention.

### 2.6 Alternatives considered and rejected

- **Verify a hand-written model of the kernel instead of the kernel.**
  Rejected: verifies the model, not the code — a re-run of the original
  misunderstanding. The TLA+ layer already *is* the model tier.
- **Index-arena rewrite of the CDT** (slots as `u32` indices into a
  static table — the maximally Kani-friendly shape). Rejected: a static
  kernel slot pool violates "no kernel allocation that isn't
  user-accounted" (§3.2/§2.5); cap slots must live inside user-donated
  memory (cspaces, channel rings, TCBs). The pointer-threaded CDT stays;
  harnesses build it over harness-owned arrays, which CBMC handles.
- **cfg-gate the existing `kernel` crate for host builds** instead of
  extracting a crate. Rejected: the forced target + `build-std` live in
  `kernel/.cargo/config.toml` (directory-scoped), `global_asm!` modules
  can't be compiled out without gutting the crate, and the cfg seam set
  *is* the `kcore` boundary — better expressed as a crate boundary the
  type system enforces.
- **Wait for Verus instead** (per the original §6 table). Out of scope by
  direction; see §1.

---

## 3. Property sources: from TLA+ to Kani

The CapRevocation model's invariants translate to implementation
properties as follows. The model's notion of "place" (cspaces, queue
positions, TCB binding slots) maps to physical `CapSlot`s; "exactly one
owner" is structural in the implementation (a cap *is* the contents of one
slot), so its checkable residue is **refcount soundness** plus **move
totality** (a move empties the source and fills the destination —
`slot_move`'s contract). The mapping:

| TLA property (TLC-checked) | Implementation property (Kani) |
|---|---|
| `TypeOK` | `wf()` structural predicates (§4.1) |
| `MoveSemantics` (one owner per cap) | `slot_move`/`send`/`recv` contracts: src emptied ⇔ dst filled, refcounts unchanged across a move; no op duplicates a cap without `derive`/`obj_ref` |
| `DeadNowhere` (deleted caps purged everywhere) | `RefCountSound` (§4.1): `refs(obj)` = caps in slots (cspace + queue + TCB bind) + bindings + waiters + mappings; object destroyed exactly at zero |
| `LiveParent` (revoke complete, queues included) | post-`revoke(s)`: `s.first_child == null` and no slot in the world has an ancestor path through a deleted slot; queue slots and TCB bind slots verifiably emptied |
| `FireSafe` | non-null binding ⇒ bound notification's refcount includes the binding ⇒ live at fire time |
| `RevokedDead` | ghost-state harness variant: revoked slots stay empty until an explicit reuse op |
| `ReportMonotone` | `Report` transition function: `Running → Exited\|Faulted` once; terminal states absorbing (`thread.rs` already guards; the harness proves the guard total) |
| TSpec `RefCountSound` | same as `DeadNowhere` row — notification refcounts vs. (caps + channel bindings + TCB bind slots + waiters + armed timers) |
| TSpec `ChannelFireSafe` | whole-object teardown ordering: `delete` fires `endpoint_cap_dropped` (peer-closed) **before** `obj_unref`; binding's refcount keeps the notification alive through the firing |
| TSpec `ReclaimedReleased` | `destroy_channel` releases every binding ref and deletes every queued cap (no leaked refcount, no orphaned cap) |

`CommitProtocol` is host-side and stays primarily TLA+ + the
crash-injection proptest in `cas/src/store.rs` (which already mirrors
`AckedWritesRecoverable`). Kani's storage-side role is narrower —
§4.7.

**Bounds policy:** mirror the TLC configurations. `CapRevocation.cfg`
checks 4 cap ids, 2 procs, 1 channel of depth 2, 2 threads, 2
notifications; Kani harnesses use the same scale (cspaces of 4–6 slots,
channel depth 2, op sequences of 4–6 steps). The justification is
inherited from the model-checking tradition both tools share: the
interesting interleavings of this state space manifest at small scope,
and TLC found the design sound at exactly this scope. Bounds are recorded
per-harness in one place (`kcore/src/proofs/bounds.rs`) so scaling them up
is a one-line change when CI budget allows.

---

## 4. Harness catalog

Harnesses live in `kcore/src/proofs/` under `#[cfg(kani)]`, one module per
subject, named `check_<module>_<property>`. Two genres:

- **Contract harnesses** — arbitrary (assumed-`wf`) state, one operation,
  assert `wf` + the op's postcondition. State comes from *shape builders*:
  nondeterministic construction of small structures (e.g. a CDT over N
  slots from a nondet parent array, assumed acyclic and consistent) — not
  `kani::any()` on pointers.
- **Transition-system harnesses** — start from the real `Init` state
  (fresh cspaces, one untyped), run K nondeterministically chosen ops from
  the same action alphabet as the TLA model (retype, derive, move, send,
  recv, bind, thread_exit, thread_fault, delete, revoke, reset), assert
  all invariants after every step. This is the direct re-check of the TLC
  result on real code, and it exercises the *compositions* contract
  harnesses miss.

### 4.1 cspace / CDT (`kcore::cspace`) — the centerpiece

Well-formedness `cdt_wf(world)` (the executable `TypeOK`):
- sibling list doubly consistent: `s.next_sib.prev_sib == s` and dually;
  `parent.first_child.prev_sib == null`;
- `s.first_child != null ⇒ first_child.parent == s`; every child reachable
  from `parent.first_child` via sibling links;
- empty slots fully detached (all four links null);
- no cycles (bounded walk ≤ world size);
- **`RefCountSound`**: for every object, `hdr.refs` equals the recount
  over all slots designating it + channel/TCB bindings + blocked waiters
  + armed timers + frame `mapping` aspace refs.

| Harness | Property |
|---|---|
| `check_cdt_insert_child` | preserves `cdt_wf`; child is first child; previous children intact |
| `check_cdt_unlink` | preserves `cdt_wf`; children re-parented one level up **in order**; detached slot fully nulled |
| `check_slot_move` | preserves `cdt_wf`; dst inherits exact CDT position incl. children's parent pointers and the `parent.first_child == src` fixup; src empty; refcounts unchanged |
| `check_derive_monotone` | `dst.rights ⊆ src.rights` for **all** masks (§2.3 monotone derivation — the load-bearing security property); refuses Untyped and occupied dst; fresh Frame copy starts unmapped (§2.5 one-mapping-per-copy); refcount +1 |
| `check_delete` | preserves `cdt_wf`; children survive re-parented; peer-closed fires before unref on channel caps (TSpec ordering); mapped-frame delete unmaps and unrefs the aspace; last-ref delete destroys |
| `check_revoke` | post: no descendants (`LiveParent` re-established); every deleted cap's object correctly unref'd/destroyed; **queue slots and TCB bind slots emptied** (the §2.2 "sees through queues" guarantee, asserted on a world where a derived cap is parked in a channel ring and another in a TCB bind slot); the revoked cap itself survives; terminates within the bound |
| `check_destroy_cspace` | a dying cspace deletes all residents; recursion through nested containers bounded (the known seL4-zombie-cap debt — the harness pins the *current* bounded behavior so the future fix is observable) |
| `check_cdt_transition_system` | the K-step nondet-op harness over all invariants above + `RevokedDead` ghost |

### 4.2 untyped (`kcore::untyped`)

| Harness | Property |
|---|---|
| `check_carve_no_overflow` | `carve()` never panics/overflows for **any** `(base, size, watermark, ty, param)` with the stated input invariant `base + size` doesn't wrap (made explicit, since boot constructs it); in particular adversarial `param` (today: `(param as usize).next_multiple_of(4096)` at `untyped.rs:130` panics for `param` near `u64::MAX`, and `param * 4096` for frames is guarded only by `param ≤ 1<<16` — see §7) |
| `check_carve_geometry` | result aligned per type; `[start,end) ⊆ [base+wm, base+size)`; successive carves disjoint; watermark strictly monotone |
| `check_retype_cdt` | new cap is CDT child of the untyped; channel retype installs both endpoints with correct end refcounts (`endpoint_cap_added` × 2, `refs == 2`) |
| `check_retype_rights` | rights inheritance table: Frame inherits parent rights (PHYS flows only from boot caps); sub-Untyped masked to `READ\|WRITE` (never PHYS — §2.5's by-construction claim, now a proof); Thread gets `THREAD_ALL`; others `ALL` |
| `check_reset` | refuses while children exist (the TLA `Retype` guard / `untyped_reset` precondition); resets watermark to 0 otherwise |

### 4.3 channel (`kcore::channel`)

`chan_wf`: `count[r] ≤ depth`, `head[r] < depth`, all cap slots outside
the live window empty, `end_caps` consistent with a ghost cap census.

| Harness | Property |
|---|---|
| `check_ring_fifo` | send/recv against a ghost `VecDeque` model: payload bytes and cap identity delivered FIFO; indices never out of bounds for any op sequence at depth 2–3 |
| `check_send_move` | caps leave sender slots exactly when send succeeds; on `Full`/`PeerClosed` sender slots untouched |
| `check_recv_atomic` | `NoCapSlot` failure leaves the message **fully queued** (§3.3: the receiver retries) — no partial cap installation, payload intact |
| `check_recv_null_tolerant` | revocation-emptied queue slots delivered as absent caps (mask bit clear), no panic (§3.4 null-slot rule) |
| `check_peer_closed` | last cap of an end fires the *other* end's binding, once; send into closed peer errors |
| `check_bind_refcounts` | bind/rebind/unbind keep notification refcounts exact (rebind releases old) |
| `check_destroy_channel` | TSpec mirror: all queued caps deleted (their objects unref'd), all binding refs released — `ReclaimedReleased` |
| `check_teardown_fire_safe` | TSpec `ChannelFireSafe` end-to-end: channel + peer-closed bindings to a separately-funded notification; delete all endpoint caps in nondet order; assert each surviving peer's binding fired into a live notification and the notification outlives the channel (the M1 EL0 step-6 scenario, as a proof) |

### 4.4 notification + thread reports (`kcore::notification`, `kcore::thread`)

| Harness | Property |
|---|---|
| `check_signal_wait` | signal ORs bits; waiter wake delivers the whole word and clears it; no-waiter signal accumulates; `wait` on nonzero word consumes without blocking |
| `check_waiter_fifo` | wake order = block order (ghost list from the `Sched` trait, §2.2 rule 4); waiter refcounts exact through block/wake/`remove_waiter` |
| `check_remove_waiter` | unlinks head/middle/tail correctly (incl. tail pointer fixup), nulls the TCB's links, releases the ref; absent thread is a no-op |
| `check_report_monotone` | TLA `ReportMonotone` on the real transition fn: any op sequence (`exit`, `fault`, spurious repeats) yields at most one `Running →` transition; terminal absorbing |
| `check_bind_fire_safe` | TLA `FireSafe`: on-exit/on-fault firing only ever reads a slot that is empty or holds a live notification (a revoke racing the death emptied the slot ⇒ firing is a no-op, never a touch of freed memory) |
| `check_thread_teardown` | dying thread blocked on a notification is unlinked and its ref released; destruction produces **no report** (§5.1: destruction is the parent acting) |

### 4.5 aspace (`kcore::aspace`, post-rewrite)

| Harness | Property |
|---|---|
| `check_pte_encode` | pure-function properties from §2.4: AF, PXN unconditional; W⇒`AP_EL0_RW`, ¬W⇒RO; ¬X⇒UXN; device ⇒ non-executable + `SH_NONE` + device attr; address bits round-trip; no perms combination yields an EL1-writable-EL0-visible or EL0-executable-kernel page |
| `check_map_model` | against a ghost partial map `va_page → (pa_page, perms)`: map adds exactly the requested pages or fails atomically — **the current two-pass map is not atomic on `NeedMemory`** (first pass allocates tables as a side effect; a mid-loop pool exhaustion in pass 2 cannot happen since pass 1 walked the same range — the harness proves this implication, which is the actual atomicity argument) |
| `check_map_no_silent_remap` | `AlreadyMapped` on any overlap; no PTE overwritten while nonzero |
| `check_va_bounds` | for all `(va, pages)`: mapping confined to `[USER_VA_BASE, USER_VA_END)`; L1 indices for user VAs never touch the two shared kernel entries (the isolation-critical index arithmetic) |
| `check_unmap_exact` | unmap clears exactly the mapped pages, ghost-Hal records a TLBI per cleared page (§2.2 rule 3) |
| `check_range_mapped` | `range_mapped(va, len, w)` ⇔ ghost-model containment (+ writability) — for all `va, len` incl. the `len == 0` and overflow edges (`va.checked_add(len)`); this is the predicate the syscall layer trusts before dereferencing user pointers, so it gets the strongest treatment |
| `check_pool_accounting` | `pool_used ≤ pool_pages` always; `NeedMemory` exactly at exhaustion; tables zeroed at allocation |

### 4.6 syscall decode (`kcore::sysabi`, post-split)

| Harness | Property |
|---|---|
| `check_decode_total` | `decode(nr, args)` never panics for any `u64⁸`; unknown nr ⇒ error, never UB (§3.7's "unknown opcode yields an error, never a crash", applied to the syscall ABI) |
| `check_validate_lengths` | message length validated ≤ `MSG_PAYLOAD` **before** the `as u16` cast in `send`; slot indices < cspace size before use; `ObjType::from_u64` totality |

### 4.7 Host-side targets (tier 2 — after the kernel core)

These crates already build on host; no extraction needed, only harnesses.
Kani is **supplementary** here (proptest/fuzz remain primary per §6); it is
applied only where exhaustiveness at small bounds buys something a fuzzer
can't promise:

| Target | Property |
|---|---|
| `urt::time` | tick→ns conversion: no overflow for all `(Δticks, cntfrq)` in the hardware envelope, monotone in Δ (the naive `Δ·10⁹` overflow that proptest currently catches probabilistically becomes a proof); seqlock reader never returns torn values under a nondet writer-step model |
| `urt::slots` | free-list never double-allocates, never loses a slot (alloc/free sequences at small bounds) |
| `ipc` wire header | fixed-header decode total over all byte strings; trailing-byte rejection; encode∘decode = id |
| `cas::tlv` | deterministic TLV: decode-then-re-encode reproduces input for all accepting inputs ≤ N bytes (the §6 canonical-form oracle, as proof at small N); non-canonical encodings rejected |
| `cas::disk` superblock | the §4.5 mount chokepoint: geometry validation of all fields vs. a nondet device length, checked arithmetic only — "no untrusted field vouches for another" becomes a checked dataflow at the chokepoint; parse total over arbitrary superblock bytes |
| `dma-pool` | handed-out buffers disjoint, in-pool; device-address↔buffer mapping bijective |

`blake3` and the FastCDC gear loop are out of Kani scope (interpreted
hashing is what makes Miri slow; CBMC fares worse). Where a harness needs
hashes, stub `cas::hash` (`-Z stubbing`) with an injective-on-small-inputs
ghost function — the standard collision-freedom axiom, stated explicitly
in the harness doc comment.

---

## 5. Harness engineering conventions

- **One property per harness.** Solver time scales viciously with formula
  size; many small harnesses beat one omnibus (and CI can parallelize and
  attribute regressions).
- **Bounds in one module** (`proofs/bounds.rs`): world sizes, queue depths,
  op-sequence length K, `#[kani::unwind]` values derived from them. Every
  unwind bound is `bound + 1` of a stated structural bound, never a magic
  number.
- **Ghost state over assertion spaghetti:** transition harnesses carry an
  explicit abstract state (live set, parent map, refcount census — the TLA
  variables, in Rust) and assert the abstraction function matches after
  every step. This keeps the TLA↔Kani correspondence reviewable
  property-by-property.
- **Negative harnesses** (`#[kani::should_panic]`) for every
  `debug_assert!` contract (e.g. `slot_move` on a non-empty dst), so the
  contracts are tested as unreachable-in-wf-states, not merely present.
- **Stubbing** (`-Z stubbing`) only for: hashing (above), and nothing in
  `kcore` — the `Hal`/`Sched` traits make kernel-side stubbing unnecessary
  by construction. Each stub carries a comment stating the axiom it
  introduces.
- **Loop contracts / function contracts** (`-Z function-contracts`): not
  load-bearing in phase 1–4 (unstable surface); adopt selectively later
  for `revoke`'s walk if unwinding costs bite. Pin the cargo-kani version
  in CI (currently 0.67.0) and in `rust-toolchain`-adjacent docs;
  upgrades are deliberate PRs that re-run the full suite.
- Every Kani-found bug gets: a minimized regression harness (kept
  forever, like fuzz seeds → `--test fuzz_regressions`), a fix PR, and an
  entry in `doc/results/2_kani-findings.md`.

---

## 6. Phasing

Each phase is a PR series with an exit criterion; later phases depend on
earlier ones. The on-OS test suites (`m1-test.sh`, `spawn-test.sh`,
`boot-test.sh`, `run-demo.sh`) are the no-regression gate for every phase
that touches the kernel — they are what proves the extraction preserved
behavior.

| Phase | Work | Exit criterion | Size |
|---|---|---|---|
| **0 — plumbing** | `kani` CI job (pinned cargo-kani, runs the scratchpad proof); `doc/results/2_kani-findings.md` skeleton; bounds module conventions | CI runs `cargo kani` green on every PR | S |
| **1 — pure functions, no extraction** | Harnesses for code that already host-builds: `urt::time`, `urt::slots`, `ipc` header codec; extract-and-verify `carve()` (pure math can move ahead of the full untyped move) and `pte_encode` | §4.7 rows 1–3 + `check_carve_*` + `check_pte_encode` proven; first findings triaged | M |
| **2 — `kcore` extraction** | Create `kcore`; move cspace/channel/notification/thread-report/untyped/timer logic behind the §2.2 seams; `kernel` becomes the shell; **zero intended behavior change** | Kernel boots; all four QEMU suites green; host `cargo test -p kcore` runs the existing contract comments as `wf`-based unit tests; CI `host-tests` includes kcore | L |
| **3 — CDT suite** | §4.1 + §4.2 harnesses, contract + transition-system genres | All CapRevocation-mirror properties proven at TLC bounds; findings fixed or filed | L |
| **4 — channel/notification/thread suite** | §4.3 + §4.4 harnesses incl. the TSpec teardown mirrors | TSpec-mirror properties proven; M1 step-6 scenario exists as proof | M |
| **5 — aspace rewrite + suite** | §2.4 rewrite; §4.5 harnesses | QEMU suites green on the rewritten walker; §4.5 properties proven | L |
| **6 — syscall split + suite** | §2.5 split; §4.6 harnesses | decode totality + validation properties proven | M |
| **7 — storage-side chokepoints** | §4.7 rows 4–6 (`cas::tlv`, superblock chokepoint, `dma-pool`) | properties proven; stubbing axioms documented | M |
| **8 — docs closeout** | Update spec §6 table (Kani as kernel implementation tier, Verus deviation recorded), `CLAUDE.md` build/verify commands, scratchpad demoted/retired | docs merged | S |

Sequencing rationale: phase 1 produces value (and likely real findings,
§7) before any risky surgery, and builds harness fluency on easy targets;
phase 2 is pure refactor with the strongest regression net; phases 3–4
mirror the TLA models that already define "correct"; phase 5 is isolated
because it's the only phase that *rewrites* rather than relocates logic.

---

## 7. Expected findings (to confirm early, not assume)

Writing the first harnesses should immediately interrogate these — found
by inspection while planning, listed so phase 1/3 aims at them; none are
confirmed bugs until a counterexample or proof says so:

1. **`untyped.rs:130`** — `(param as usize).next_multiple_of(4096)` panics
   for `param > usize::MAX − 4095`. `param` is raw user input
   (`syscall.rs:166`, register `a[2]`). A kernel panic from any
   untyped-holder would be a user-triggerable DoS. Likewise the unchecked
   `base + watermark + align − 1` (`untyped.rs:135`) and `base + size`
   rely on an *implicit* "untyped ranges don't wrap" invariant —
   `check_carve_no_overflow` either proves the guard set sufficient or
   produces the counterexample.
2. **`aspace.rs::map`** — nothing enforces the "`PERM_DEVICE` is never
   executable" doc comment; `PERM_DEVICE | PERM_X` would encode an
   executable device mapping. Possibly unreachable via current callers —
   `check_pte_encode` decides whether the invariant is real or aspirational.
3. **`channel.rs::send`** — `data.len() as u16` truncates; safety
   currently rests on a caller-side length check living in another file.
   The §2.5 syscall split turns this cross-layer precondition into a
   checked one.
4. **Refcount census** — `RefCountSound` is exactly the kind of invariant
   that drifts when a new reference-holding edge is added (armed timers
   and frame mappings are recent); the census harness makes any future
   drift a CI failure instead of a use-after-free.

---

## 8. CI integration

Extend `.github/workflows/ci.yml` with a fourth job:

- **kani** — `cargo kani -p kcore` (and `-p urt -p ipc …` as phases land),
  pinned cargo-kani version installed via `cargo install --locked
  cargo-kani --version <pin>` + `cargo kani setup` (cached); runs on every
  PR and push to main, parallel to `host-tests` / `model` / `on-os`.
- Budget: each harness ≤ ~5 min solver time, whole job ≤ ~30 min. A
  harness that outgrows the budget is split or has its bound documented
  and reduced — never silently skipped (the §6 no-silent-caps discipline).
- The harness list in CI is `--harness`-explicit-free (run all), so adding
  a harness automatically gates.

Local commands (added to `CLAUDE.md` at phase 8):

```sh
cargo kani -p kcore                      # full kernel-core proof suite
cargo kani -p kcore --harness check_cdt_ # one module's harnesses
cargo test -p kcore                      # wf predicates as plain tests
```

---

## 9. Risks and mitigations

| Risk | Mitigation |
|---|---|
| CBMC blow-up on pointer-heavy CDT harnesses | TLC-scale bounds (§3); one property per harness; shape builders that assume `wf` instead of constructing via long op prefixes; contract harnesses carry the load, the transition harness is the small-K integration check |
| The aspace rewrite breaks translation on real boots | Phase-isolated; full QEMU suite gate; descriptor format byte-identical by construction (only the *walker's* addressing changes); GDB-stub diffing of generated tables against the old code on a fixed map sequence before merging |
| `kcore`/`kernel` drift (verified code vs. shipped code) | There is no second copy — `kcore` *is* the kernel's object machinery; the §2.2 layering rules are grep-enforced in CI |
| Kani/unstable-feature churn (`-Z stubbing`, contracts) | Version pinned; minimal unstable surface (stubbing only host-side, no function contracts until needed); upgrades are dedicated PRs |
| Bounded proofs oversold as total | Every harness doc-comment states its bounds; `doc/results` findings file carries the standing caveat; unbounded arguments (revoke termination, full-scale behavior) remain owned by TLA+ + code review, stated in spec §6's updated table |
| Extraction subtly changes codegen for the kernel target (inlining across the new crate boundary) | `#[inline]` parity where measured to matter; the M1 user.rs EL0-execute-never constraint (`opt-level = 1` note in `CLAUDE.md`) re-checked; QEMU suites as the behavioral gate |

---

## 10. Out of scope

- **Concurrency proofs** — the kernel is single-core and non-preemptible
  (IRQs masked at EL1); when the revoke walk becomes preemptible (tracked
  M2 debt) its restartability argument lands first in the TLA model
  (LiveParent at every step), then as new Kani harnesses over the
  partial-walk states. Loom/Shuttle keep the userspace/`ipc` concurrency
  tier (§6).
- **Boot, exception vectors, context switch, GIC/UART/MMU bring-up** —
  inline asm and MMIO; outside CBMC's model by construction. They stay
  small, reviewed, and exercised by the on-OS suites.
- **The storage engine's data path** (chunker, prolly tree, store
  commit/recovery) — owned by proptest + crash-injection + fuzz + Miri +
  the CommitProtocol TLA model; Kani touches only the §4.7 chokepoints.
- **Verus adoption** — deliberately preserved as an option by this
  rewrite's shape, not pursued here.
