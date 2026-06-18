# Plan: the Verus rewrite — deductive verification for the kernel core (and where it pays elsewhere)

**Status:** **proposed.** The spec (§6) named **Verus** for the cspace/CDT
operations and the kernel allocator "written in Verus dialect from day one." That
never happened: the kernel predated the tooling, and `doc/plans/0_kani-rewrite.md`
substituted **Kani** (bounded model checking on a host-extracted `kcore`) as the
mechanized kernel tier. Kani delivered real value (it found a `carve` overflow DoS
and an executable-MMIO encoding; it re-checks the CapRevocation TLA+ invariants on
real code). But it bought that value at three structural costs the findings docs
record: it proves only at **TLC-scale bounds** (4–6 slots, depth 2, K≤4 ops), it
**cannot prove termination** of the revoke/delete recursion (left as a
`debug_assert` + TLA argument; the `-Z function-contracts` spike, DN-14
`doc/results/18_kani-findings-15.md`, confirmed Kani contracts cannot even *name*
`delete`'s write set), and its composition harnesses **OOM** (DN-12 forced the
exhaustive multi-op CDT replay off-CI into `scripts/deep-verify.sh`). Verus now
builds and verifies on the host (the `scratchpad` crate, pinned
`0.2026.06.07.cd03505` / `vstd =0.0.0-2026-05-31-0205`; `verus!{}` erases to
ordinary Rust so the aarch64 build is unaffected — proven by `scratchpad` building
under plain `cargo build`).

This plan is the deliberate debt paydown the original spec called for. **It is
merciless by direction:** wherever Verus is the better tool it replaces what is
there now — including deleting Kani harnesses, the `kcore` proof scaffolding, and
the off-CI deep-Kani machinery they subsume, and including a data-model rewrite of
`kcore` that the churn is worth. It is **not** Verus-everywhere: §2 states plainly
where Verus is *not* the best tool (concurrency, adversarial bytes, the asm shell)
and those tiers stay. "Best tool for the job" is the test, applied honestly.

**Baselines:** spec `doc/spec/2_spec_rev2.md` §6 (the tier table this updates);
`doc/plans/0_kani-rewrite.md` (the `kcore` extraction + the TLA→property mapping
this inherits and extends); the CapRevocation / CommitProtocol / IpcReactor TLA+
models (the design tier, unchanged — Verus checks *code*, TLA checks *design*).

---

## 1. Background and goal

### 1.1 What Verus is, in one paragraph

Verus is a deductive verifier for a subset of Rust: you annotate `exec` (compiled)
functions with `requires`/`ensures` contracts and loop `invariant`s, write
`spec` (ghost, pure, total) functions as the mathematical model, and `proof`
functions/lemmas to discharge the hard steps; an SMT solver (Z3) proves each
function modularly against its contract. Crucially for this codebase it gives,
out of the box, three things Kani does not:

- **Unbounded proofs.** Properties hold for *all* slot counts, tree shapes, queue
  depths, and op sequences — not the bounded scope CBMC unwinds. The CapRevocation
  invariants stop being "checked at 4–6 slots" and become "proven for all N."
- **Termination.** A `decreases` clause proves recursion/loops terminate for every
  input. Revoke, `delete`, `obj_unref`, and `destroy_*` recursion — today a
  `debug_assert` + prose TLA argument (Kani's explicit gap) — become theorems.
- **Modular, compositional scaling.** Each function is verified against its
  contract independently, so whole-core coverage does not blow up the way CBMC's
  whole-state-space search did (DN-12). Composition is free: a caller reasons from
  callees' `ensures`, not by re-exploring their bodies.

It also keeps Kani's wins: panic/overflow/bounds freedom (Verus proves arithmetic
non-overflow for all inputs as a matter of course), and "checking the property on
the real code, not a transcribed model."

### 1.2 The goal

Make Verus the **mechanized implementation tier for `kcore`**, proving the full
CapRevocation invariant set (and its TSpec teardown half) plus the
implementation-only properties (refcount soundness, structural well-formedness,
move totality, termination, overflow freedom) as **unbounded** theorems on the
real kernel object code; and apply Verus to the **host-side chokepoints** where an
unbounded functional proof beats a bounded one (allocator disjointness, the
tick→ns conversion, the wire/TLV/superblock codecs, the commit-protocol recovery
core). Retire the Kani machinery these subsume. Leave concurrency, adversarial
input, and the unverifiable asm shell on the tiers that are actually best for them.

---

## 2. Where Verus is the best tool — and where it is not

The whole plan turns on this table. "Incumbent" is what verifies it today.

| Code | Incumbent | Best tool | Verdict |
|---|---|---|---|
| `kcore` cspace/CDT, untyped, channel, notification, thread/reports | Kani (bounded) | **Verus** | **Migrate.** Unbounded + termination + functional specs; this is the spec's original Verus assignment. |
| `kcore` aspace walker + PTE encode | Kani (bounded) | **Verus** | **Migrate.** Pure index/bit arithmetic and a partial-map model — Verus's sweet spot; isolation invariants proven for all VAs, not sampled. |
| `kcore` syscall decode/validate | Kani (bounded) | **Verus** | **Migrate.** Totality + length/slot validation as `ensures`. |
| `urt::time` tick→ns; `urt::slots` allocator; `dma-pool` disjointness; `ipc` header codec; `cas` TLV + superblock geometry | Kani (supplementary) + proptest/fuzz | **Verus** for the functional core | **Migrate the proof obligation.** Overflow-freedom and disjointness for *all* inputs; proptest/fuzz stay as differential/regression coverage. |
| Storage commit/recovery (`cas::store` A/B flip, WAL replay) — functional core | TLA+ + crash-injection proptest | **Verus** *complements* TLA+ | **Add (mandatory, scoped).** Verus on the real recovery function closes the model-to-code gap TLA+ leaves; std/`Vec` weight bounds the scope to an extracted pure function (§4.8), but the proof is required — the commit protocol is a correctness pivot (spec §6). TLA+ stays the design tier. |
| Cross-process IPC races (reactor lost-wakeup/backpressure); `urt::time` seqlock *interleavings* | TLA+ IpcReactor + Loom + Shuttle | **Loom/Shuttle + TLA+** | **Keep.** Verus *can* do concurrency (tokenized state machines, `vstd::atomic`) but it is a research-grade lift, and the TLA+/Loom/Shuttle coverage is already strong and proven. Not worth overriding. Honest call: Verus is **not** better here today. |
| Adversarial bytes: wire/on-disk decoders, ELF loader, mount over arbitrary device contents | cargo-fuzz + proptest | **cargo-fuzz** | **Keep.** Verus proves decode *totality* and *canonical form* (and will, §4.7), but differential coverage, the checksum-reseal harnesses, and corpus regressions are fuzzing's job. Complementary, not replaced. |
| `blake3`, FastCDC gear loop | Miri/proptest | **neither** | **Out of scope.** Crypto/perf inner loops; interpreted hashing is what makes even Miri slow. Stub with an injective-on-small-inputs ghost where a proof needs a hash (same axiom Kani used). |
| Kernel shell: boot, MMU/TLB asm, GIC, MMIO, scheduler statics, the one PA→pointer site | (none — TCB) | **(none)** | **Stays the trusted base.** Inherently unverifiable; the whole `kcore` split exists to keep this surface small. Verus changes nothing here. |

The shape of the decision: **Verus replaces Kani entirely** (Kani's niche —
bounded BMC of the kernel core — is strictly dominated by Verus once the data
model is Verus-native, §3). Verus **complements** TLA+ and cargo-fuzz (different
questions: design vs. code; totality-proof vs. differential-coverage). Verus
**does not touch** the concurrency tier or the asm shell.

---

## 3. The enabling rewrite: from intrusive pointers to an arena

This is the centerpiece and the largest churn. It must come first, because it is
what makes everything in §4.1–§4.6 tractable.

### 3.1 Why the current shape fights Verus

`kcore` today is an **intrusive raw-pointer graph**: the CDT threads
`parent`/`first_child`/`next_sib`/`prev_sib` as `*mut CapSlot` through the slots,
and `CapKind` embeds `*mut` object pointers (`*mut Channel`, `*mut Tcb`, …); the
`Env` seam passes `*mut Tcb`/`*mut AspaceObj` (`kcore/src/env.rs`). Kani tolerates
this only because the `0_kani-rewrite.md` §2.2 rules forbid int→ptr and feed CBMC
caller-allocated, provenance-carrying pointers — and even then the proofs build
worlds out of *shape builders* (`proofs/world.rs`, `proofs/ghost.rs`) rather than
`kani::any()` on pointers.

Verifying a cyclic, doubly-linked, intrusive pointer web in Verus means carrying a
`PointsTo` permission token for every node and threading the permission map through
every traversal — for a graph with back-pointers (sibling `prev`, child→parent)
this is research-grade proof engineering, the single hardest thing to do in Verus.
Doing it would spend the whole budget fighting the representation instead of
proving the algorithm.

### 3.2 The move: arena-indexed objects, links as indices

Rewrite `kcore`'s data model so **there are no raw pointers in the verified core**:

- **Slots in a fixed arena.** `struct CSpace { slots: [CapSlot; N] }` (or a
  `vstd`-friendly bounded vector); CDT links become `Option<SlotId>` (a
  `u32`/index newtype), not `*mut`. `parent`/`first_child`/`next_sib`/`prev_sib`
  are indices into the arena.
- **Objects in typed arenas.** Channels, TCBs, notifications, aspaces, timers live
  in per-type arenas addressed by an `ObjId` index; `CapKind` carries `ObjId`s,
  not `*mut`. Refcounts live in the arena slot.
- **The `Env` seam passes indices**, not pointers (`make_runnable(ThreadId)`,
  `aspace_unmap(AspaceId, …)`); the ghost impl still logs effects/order for the
  teardown-ordering proofs.
- **The kernel shell** keeps the *only* address arithmetic: it backs each arena
  with donated untyped memory and maps `ObjId → address` at the one sanctioned
  site (replacing today's `start as *mut T` in `retype`). `kcore` never sees an
  address.

Now the entire core is **pure functions over arrays + ghost `Seq`/`Map`/`Set`**.
The CDT well-formedness predicate (`cdt_wf`) is a `spec fn` over the slot array;
`revoke` is a recursion over a subtree whose `decreases` measure is the number of
live descendants (a finite arena quantity); refcount soundness is a `spec fn`
recount over the arenas. This is the representation Verus is *built* for.

### 3.3 This also pays off independently

Index-based capability tables are a known-good kernel design (no provenance
hazard, no int→ptr fragility — the very thing `0_kani-rewrite.md` §2.2 spent rules
avoiding; trivially relocatable/serializable arenas; bounds-checked by
construction). The rewrite is not Verus tax — it is better kernel code that
happens to also be verifiable. It **removes** the pointer-oriented Kani
scaffolding wholesale: `proofs/world.rs`, the pointer-logging in `proofs/ghost.rs`,
and the `#[cfg(kani)]` `ghost_destroy_*` routing hooks in `env.rs` (DN-4 workarounds
for CBMC recursion limits — moot once `decreases` proves the recursion directly).

### 3.4 Cost, stated plainly

This touches `kcore`'s every module and the kernel shell's object placement
(`untyped::retype`, the arena backing, the `Env` impl). It is the deepest change
in the plan and the chief risk (§9). It is sequenced (§7) so the kernel keeps
booting at every step: the arena model is introduced behind the existing API
shape, one object type at a time, with the on-OS tests (`spawn-test.sh`,
`m1-test.sh`) green throughout.

---

## 4. What Verus verifies, component by component

Properties are stated as the Verus obligation. Each inherits the TLA→implementation
mapping from `0_kani-rewrite.md` §3 and **strengthens it**: where Kani asserted
"`wf` preserved at bound N," Verus proves "`wf` preserved for all N, the operation
meets its full functional postcondition, and it terminates." `wf` predicates
become `spec fn`; harness "shape builders" become `requires wf(state)`.

### 4.1 cspace / CDT (`kcore::cspace`) — the centerpiece

`spec fn cdt_wf(cs)` (the executable `TypeOK`, now total and unbounded): sibling
list doubly consistent; `first_child`/`parent` agreement; empty slots fully
detached; **acyclic** (proven via a rank/`decreases` measure, not a bounded walk);
`spec fn refcount_sound(world)`: every object's `refs` equals the recount over all
designating slots (cspace + channel-queue + TCB-bind) + bindings + waiters + armed
timers + frame mappings.

| Operation | Verus `ensures` (functional postcondition + invariant + termination) |
|---|---|
| `cdt_insert_child` | `cdt_wf` preserved; new node is first child; prior children unchanged (as a `Seq` equality on the child list) |
| `cdt_unlink` | `cdt_wf` preserved; children re-parented one level **in order** (`Seq` splice equality); detached slot fully nulled |
| `slot_move` | `cdt_wf` preserved; dst inherits exact CDT position incl. children's `parent` and the `first_child` fixup; src empty; **refcounts unchanged** (move totality — the `MoveSemantics` residue) |
| `derive` | `dst.rights ⊆ src.rights` for **all** masks (monotone derivation — the load-bearing security theorem, now ∀ not sampled); refuses Untyped/occupied dst; fresh Frame copy unmapped; `refs+1` |
| `delete` | `cdt_wf` preserved; children survive re-parented; peer-closed fires **before** unref (TSpec ordering, via the ghost `Env` log); mapped-frame delete unmaps+unrefs; last-ref delete destroys; **terminates** (`decreases` on subtree) |
| `revoke` | post: subtree empty (`LiveParent` re-established) for **all** tree shapes; every descendant's object correctly unref'd/destroyed; **queue slots and TCB-bind slots emptied** (the "sees through queues" guarantee, on a world with a derived cap parked in a channel ring and a TCB bind slot); revoked cap survives; **terminates unconditionally** (the headline gain over Kani/`debug_assert`) |
| `destroy_cspace` | resident deletion total; recursion through nested containers **terminates** (the seL4-zombie debt becomes a proven bound, not a pinned bounded behavior) |
| `obj_unref` | refcount decrement sound; destroys exactly at zero; dispatch to the right destructor (functional, replacing the DN-4 ghost-routing hooks) |

This single module subsumes `proofs/cdt.rs`, `proofs/teardown.rs`,
`proofs/transition.rs`, and the off-CI `proofs/exhaustive.rs` "mini-TLC" replay —
the last entirely, because an unbounded proof is what that enumeration was
approximating (DN-12).

### 4.2 untyped (`kcore::untyped`)

The cleanest early win (pure `u64` arithmetic — pilot candidate, §7).

- `carve(base, size, watermark, ty, param)`: **never overflows/panics for any
  inputs** under the explicit `requires base + size` no-wrap precondition
  (including adversarial `param` from user register `a[2]`); result aligned per
  type; `[start,end) ⊆ [base+wm', base+size)`; successive carves disjoint;
  watermark strictly monotone — all as `ensures`, all ∀.
- `retype`: new cap is a CDT child of the untyped; channel retype installs both
  endpoints with `refs == 2`; rights inheritance table proven (Frame inherits;
  sub-Untyped masked to `READ|WRITE`, **never** PHYS — the §2.5 by-construction
  claim as a theorem; Thread → `THREAD_ALL`).
- `reset`: refuses while children exist (the `Retype` guard / `untyped_reset`
  precondition as a `requires`); zeroes watermark otherwise.

### 4.3 channel (`kcore::channel`)

`spec fn chan_wf`: `count[r] ≤ depth`; `head[r] < depth`; slots outside the live
window empty; `end_caps` consistent with a ghost cap census.

- `send`/`recv` against a ghost `Seq` (the FIFO model): payload + cap identity
  delivered in order, indices in bounds, for **all** op sequences at **any** depth
  (Kani checked depth 2–3).
- Move: caps leave sender slots exactly on success; untouched on `Full`/`PeerClosed`.
- `recv` atomicity: `NoCapSlot` leaves the message fully queued (no partial cap
  install, payload intact).
- Null-slot tolerance: revocation-emptied queue slots delivered as absent (mask
  bit clear), no panic.
- `peer_closed` / `destroy_channel`: last cap fires the other end's binding once;
  teardown deletes every queued cap and releases every binding ref
  (`ReclaimedReleased`); fire-before-unref ordering (`ChannelFireSafe`).

### 4.4 notification + thread/reports (`kcore::notification`, `kcore::thread`)

- `signal`/`wait`: signal ORs bits; waiter wake delivers the whole word and clears
  it; no-waiter signal accumulates; `wait` on nonzero consumes without blocking —
  the exact `ModelTransport`/TLA semantics, now on the kernel code.
- Waiter queue: wake order = block order (ghost `Seq` from the `Env`/`Sched` log);
  `remove_waiter` unlinks head/middle/tail with correct tail fixup; refcounts
  exact through block/wake/remove.
- `Report` transition: at most one `Running → Exited|Faulted`; terminal states
  absorbing — proven **total** over any op sequence (`ReportMonotone`).
- `FireSafe`: on-exit/on-fault firing only ever reads an empty slot or a live
  notification (a revoke racing death emptied the slot ⇒ no-op, never freed-memory
  touch).
- Thread teardown: dying thread unlinked from its notification, ref released; **no
  report** on destruction (destruction is the parent acting, §5.1).

### 4.5 aspace + PTE (`kcore::aspace`)

Pure bit/index arithmetic and a partial-map ghost — Verus excels here.

- `pte_encode`: AF/PXN unconditional; W⇒`AP_EL0_RW`, ¬W⇒RO; ¬X⇒UXN; device ⇒
  non-exec + `SH_NONE` + device attr; address bits round-trip; **no perms
  combination yields an EL1-writable-EL0-visible or EL0-executable-kernel page**
  (the isolation theorem, ∀ perms).
- `map` against a ghost `Map<va_page, (pa_page, perms)>`: adds exactly the
  requested pages or fails atomically; `AlreadyMapped` on any overlap; no nonzero
  PTE overwritten.
- `va_bounds`: ∀ `(va, pages)` the mapping is confined to `[USER_VA_BASE,
  USER_VA_END)`; user L1 indices never touch the two shared kernel entries.
- `range_mapped(va, len, w)` ⇔ ghost containment (+ writability) for **all**
  `va,len` including `len==0` and `va.checked_add(len)` overflow edges — the
  predicate the syscall layer trusts before dereferencing user pointers, so it
  gets full functional equivalence.
- `unmap` clears exactly the mapped pages; ghost `Env` records one TLBI per cleared
  page (the effect-ordering proof the asm seam exists for).
- pool accounting: `pool_used ≤ pool_pages` always; `NeedMemory` exactly at
  exhaustion; tables zeroed at allocation.

### 4.6 syscall decode (`kcore::sysabi`)

- `decode(nr, args)` **never panics** for any `u64⁸`; unknown nr ⇒ error, never UB.
- length validated ≤ `MSG_PAYLOAD` **before** any `as u16` cast; slot indices <
  cspace size before use; `ObjType::from_u64` total.

### 4.7 Host chokepoints (where unbounded beats bounded)

Verus takes over the *proof obligation* (proptest/fuzz stay as differential and
regression coverage).

| Target | Verus `ensures` |
|---|---|
| `urt::time` tick→ns | no overflow for **all** `(Δticks, cntfrq)` in the hardware envelope; monotone in Δ (the naive `Δ·10⁹` overflow becomes a theorem, not a probabilistic proptest hit). The seqlock *value* logic only; the *interleaving* stays Loom (§2). |
| `urt::slots` | free-list never double-allocates, never loses a slot — for all alloc/free sequences (Kani: small bounds). |
| `dma-pool` | handed-out buffers pairwise disjoint and in-pool; device-addr↔buffer mapping bijective — ∀. |
| `ipc` header codec | fixed-header `decode` total over all byte strings; trailing-byte rejection; `encode∘decode = id`. |
| `cas::tlv` | deterministic TLV: `decode`-then-re-`encode` reproduces input for all accepting inputs (the canonical-form oracle as a theorem); non-canonical rejected. |
| `cas::disk` superblock | the §4.5 mount chokepoint: every geometry field validated vs. a symbolic device length with checked arithmetic only — "no untrusted field vouches for another" as a proven dataflow; parse total over arbitrary bytes. *(Scope flag: `disk.rs` is `Vec`-heavy; if the `vstd::Vec` port proves disproportionate, this one target may stay on Kani — the single explicit place Kani earns a stay of execution, §5.)* |

### 4.8 Storage commit protocol (mandatory; scoped complement to TLA+)

`cas::store` (1634 lines, std/`Vec`/`Box`-heavy) is not a wholesale Verus target.
But the **recovery core** — "recovered state = committed roots + replay of WAL
records not covered by the committed head," the `AckedWritesRecoverable` invariant
TLA+ checks and `cas/src/store.rs`'s crash-injection proptest samples — **is**
proven in Verus on an *extracted pure decision function* (pick-survivor-superblock,
replay-bound computation), where Verus closes the model-to-code gap TLA+ cannot
reach. This is **additive** (TLA+ remains the design gate; proptest/fuzz stay) and
deliberately narrow — but it is **required, not optional**: cleanly extracting that
pure function from `store.rs` is part of the work, not a precondition for doing it.
The commit protocol is, with the cap machinery, where the system's correctness
actually pivots (spec §6), so it does not get a weaker tier than the kernel core.

---

## 5. What is removed or demoted (the merciless part)

When a Verus phase lands and its proofs are green in CI, the machinery it subsumes
is **deleted**, not left to rot:

- **The `kcore` Kani harness suite** — `kcore/src/proofs/{cdt,channel,notification,
  thread,aspace,untyped,sysabi,transition,teardown,exhaustive,world,ghost,wf,
  contracts,stubs,bounds,mod}.rs` go as each is superseded. `wf` predicates are not
  lost — they move into the `kcore` modules as `spec fn` (the verified form).
- **The deep-Kani off-CI tier** — `scripts/deep-verify.sh`'s `kani` and `contracts`
  paths, the `kani_deep` and `kani_contracts` cargo features, and `kani-deep.yml`.
  Verus's unbounded proof is exactly what "widened bounds" and the function-contracts
  spike were reaching for; both are moot.
- **The `kani` CI job's `kcore` leg** — `cargo kani -p kcore` (and `-Z stubbing`)
  is removed once §4.1–§4.6 are Verus-proven. The host-leg (`-p urt -p ipc
  -p dma-pool`, `-p cas`) is removed per-target as §4.7 lands — **except** a
  possible `cas::disk` superblock holdout (§4.7 scope flag). If that holdout is also
  ported, **Kani is retired from the project entirely** and `CLAUDE.md`/CI lose the
  job; the pinned-`0.67.0` install dance goes with it.
- **`scratchpad`** graduates: it stops being a toy `spec fn min` and becomes either
  the first real proof module's home or is removed once `kcore` carries its own
  `verus!{}` blocks.

What is explicitly **kept** (per §2): TLA+ (design tier, all three models);
Loom/Shuttle (concurrency); cargo-fuzz (adversarial bytes + checksum-reseal +
corpora); Miri + proptest (baseline, canonical-form, differential). These answer
questions Verus does not.

---

## 6. Verus techniques this plan relies on (grounding the feasibility)

- **`spec`/`proof`/`exec` split.** `wf`/`refcount_sound`/the FIFO and partial-map
  models are `spec fn`; operations are `exec fn` with `requires`/`ensures`; hard
  lemmas (acyclicity preserved, refcount recount equals stored) are `proof fn`.
- **`decreases`** on revoke/delete/`obj_unref`/`destroy_*` (measure: live
  descendant count / subtree size in the arena) — the termination proofs Kani
  could not give.
- **Ghost `Seq`/`Map`/`Set`** (`vstd`) as the mathematical models: the channel
  FIFO is a `Seq`, the page table a `Map<VA,…>`, the CDT child list a `Seq`,
  refcount census a `Map<ObjId, nat>`.
- **Loop `invariant`s + automatic overflow proof** for the carve arithmetic, the
  map two-pass walk, the tick→ns conversion.
- **The arena model (§3)** is what keeps all of the above first-order: no
  `PointsTo` permission threading, because there are no raw pointers in the core.
- **`verus!{}` erasure**: ghost/spec/proof code compiles to nothing, so the
  aarch64 `kernel` build links the same `exec` code it does today (verified by
  `scratchpad` building under plain `cargo build`). The verified `kcore` still
  cross-compiles unchanged.

---

## 7. Phasing (each phase ships proofs + deletes the Kani it subsumes)

0. **Toolchain + pilot.** Stand up the `verus` CI job (§8) on a real module:
   port `kcore::untyped::carve` + geometry (§4.2) to `verus!{}` — self-contained
   pure arithmetic, the cleanest end-to-end proof of the workflow. Delete
   `proofs/untyped.rs`'s carve harnesses. Proves the pin, the CI install, and the
   cross-build all hold before any deep change.
1. **The arena data-model rewrite (§3).** Introduce `SlotId`/`ObjId` indices and
   typed arenas behind the existing API shape; move the kernel shell's object
   placement onto arena-backed untyped; switch `Env` to indices. No new proofs yet
   — the on-OS tests (`spawn-test.sh`, `m1-test.sh`) stay green. This is the
   enabling refactor; it lands before any CDT proof.
2. **cspace / CDT (§4.1).** The centerpiece: `cdt_wf`, `refcount_sound`, the op
   contracts, and the revoke/delete **termination** proofs. Delete
   `proofs/{cdt,transition,teardown,exhaustive,world,ghost}.rs` and the DN-4
   `ghost_destroy_*` env hooks.
3. **untyped retype + reset (§4.2 remainder), channel (§4.3).** Delete
   `proofs/{untyped,channel}.rs`.
4. **notification + thread/reports (§4.4).** Delete `proofs/{notification,thread}.rs`.
5. **aspace + PTE (§4.5), sysabi (§4.6).** Delete `proofs/{aspace,sysabi}.rs`.
   At this point `cargo kani -p kcore` is gone.
6. **Host chokepoints (§4.7).** Port per target; delete the matching Kani host
   harnesses (`urt/dma-pool/ipc/cas` `proofs.rs`). Decide the `cas::disk` holdout.
7. **Commit-protocol recovery core (§4.8).** Extract the pure recovery decision
   function from `cas::store` and prove `AckedWritesRecoverable` on it (additive to
   TLA+). **Mandatory** — the commit protocol is a correctness pivot (spec §6), so
   it gets the same mechanized tier as the kernel core.
8. **Closeout.** Update spec §6, `CLAUDE.md`, and `0_kani-rewrite.md` (a closeout
   note pointing here); retire Kani CI/scripts if step 6 left no holdout.

Each phase is a PR with proofs green in CI before the deletion lands in the same PR
(the property is never unguarded between tiers).

---

## 8. CI integration + version pinning

- **New `verus` job** in `ci.yml`: `cargo verus verify -p kcore` (plus the host
  crates as §4.7 lands). No per-harness filter — a new `verus!{}` obligation
  auto-gates, the discipline the `kani`/`concurrency` jobs already use.
- **Pinning.** Verus binary `0.2026.06.07.cd03505` + `vstd =0.0.0-2026-05-31-0205`
  are pinned in `CLAUDE.md`/`scratchpad` today; the job installs that exact build.
  Upgrades are deliberate PRs that re-run the suite (the cargo-kani-0.67.0
  discipline). **Risk:** Verus has no crates.io binary — CI fetches the release
  artifact (a tarball/`z3` bundle), slower and more fragile than `cargo install`;
  cache it like the Kani backend (§9).
- **Cross-build guard stays.** The `layering` grep that forbids asm/`as *mut` in
  `kcore` keeps its meaning — and the arena rewrite (§3) makes the `as *mut` half
  of it trivially satisfiable (there are none left in the core).

---

## 9. Risks and mitigations

- **The arena rewrite is deep (chief risk).** It touches every `kcore` module and
  the kernel shell's placement. Mitigation: it is its own phase (§7 phase 1), lands
  behind the unchanged API shape, and is gated by the on-OS tests, not by new
  proofs — so a regression shows up as a boot failure immediately, before any
  proof work builds on it.
- **Verus proof-engineering labor exceeds Kani harness-writing.** Invariants and
  lemmas are real work; the CDT acyclicity/refcount lemmas are the hardest.
  Mitigation: the arena model removes the worst of it (no permission threading);
  phase order does the pure/easy wins first (carve) to bank the workflow; the TLA+
  models already supply the invariants to encode.
- **Toolchain immaturity / unstable pin / CI install friction.** Verus is unstable
  software; `vstd` and the binary must move together. Mitigation: hard pin
  (already in `CLAUDE.md`), cache the artifact, treat upgrades as their own PRs.
- **`Vec`/`std` weight in storage (`store.rs`, `disk.rs`).** Verus supports
  `vstd::Vec` but porting std-heavy code is costly. Mitigation: §4.8 is scoped to
  an *extracted pure function*, not the whole module; the `cas::disk` superblock is
  flagged as the one allowed Kani holdout (§5).
- **Losing Kani's counterexample traces.** Kani prints a concrete failing trace;
  Verus prints a failed assertion + SMT context. Mitigation: keep proptest/fuzz
  (they produce concrete inputs) as the first-line debugging tier; Verus failures
  are localized to one function by construction.
- **Double-maintenance window.** Between a phase's proof landing and the Kani
  deletion, both exist. Mitigation: delete in the *same* PR once green (§7), so the
  window is one review, not one release.

---

## 10. Out of scope / non-goals

- **A verified compiler / end-to-end guarantee.** No CompCert analogue exists;
  Verus proves the source against its spec, not the binary. Stated in spec §6;
  unchanged.
- **Concurrency in Verus.** The tokenized-state-machine route is not attempted;
  Loom/Shuttle/TLA+ keep the IPC and seqlock concurrency (§2).
- **The asm shell.** Boot/MMU/GIC/MMIO/scheduler statics stay the trusted base.
- **Crypto/perf inner loops** (`blake3`, FastCDC).
- **Rewriting `store.rs`/`disk.rs` wholesale** — only the extracted recovery core
  (§4.8, mandatory) and the superblock chokepoint (§4.7) are in scope; the rest of
  those modules stays on proptest/fuzz/TLA+.

---

## 11. Spec & doc updates on landing

- **Spec `2_spec_rev2.md` §6:** un-defer the Verus row (it becomes the kernel
  implementation tier — its original assignment); narrow the Kani row to whatever
  holdout remains, or strike it if Kani is retired (§5).
- **`CLAUDE.md`:** the verification-tiers table and the `### Verus` / `### Kani`
  sections track the new division; the layering/CI sections track the `verus` job.
- **`0_kani-rewrite.md`:** a closeout banner — "superseded for the kernel core by
  `3_verus-rewrite.md`; Kani served as the interim mechanized tier and found
  DN-1…DN-14, recorded in `doc/results/2…8_kani-findings*.md`." The findings docs
  stay as the historical record of what the interim tier caught.
