# Phase 5 detail: aspace + PTE (§4.5) + sysabi (§4.6) (Verus rewrite)

**Status:** proposed. Detailed, step-by-step decomposition of **phase 5** of
`doc/plans/3_verus-rewrite.md` (§4.5 + §4.6 + §7 step 5), written *before* any code so
the implementation does not repeat phase 2's mid-flight splits — the same treatment
`3_verus-rewrite_phase3-detail.md` and `3_verus-rewrite_phase4-detail.md` gave their
phases, which then landed cleanly (phase 3 across docs 26…30; phase 4 across docs
31…35).

**Baselines:** `3_verus-rewrite.md` (§4.5 aspace + PTE, §4.6 sysabi, §7 phasing);
`3_verus-rewrite_phase3-detail.md` and `3_verus-rewrite_phase4-detail.md` (the
structure this mirrors); `doc/results/21…25` (phase 2 — cspace/CDT, the acyclicity-rank
and linked-list-merge mechanics), `26…30` (phase 3 — the ghost-view enabling refactor +
FIFO core), `31…35` (phase 4 — the waiter/timer list models and the
`make_runnable`/`signal` Store-contract discipline this is the direct analog of, and the
phase-4 closeout `35` whose recommendations this acts on); the current `kcore` source as
of `main` `01ff371` (phase-4e merge). Current Verus baseline: **120 verified, 0 errors**;
`cargo test -p kcore`: **43 passed** (doc 35).

---

## 0. Purpose, and the phase-2/3/4 lessons it acts on

Phase 2's terse §7 entry became a five-PR scramble because the structural-`wf`
strengthening, the looping-op ghost machinery, and the cross-object-teardown
entanglement were discovered mid-implementation. Phases 3 and 4 front-loaded that
discovery into a detail plan and went smoothly. This document does the same for phase 5:
the one genuinely new proof model (the page-table partial-map), the first use of Verus
slice/array reasoning in kcore, the two settled design forks, the minimal Store-seam
touch, and the cross-object scope-out are **all decided here**, so each sub-phase PR is a
known quantity going in.

**Three master-plan-line facts, resolved up front** (the phase-4-detail §0 discipline):

- **"Delete `proofs/{aspace,sysabi}.rs`" is already discharged.** There is no
  `kcore/src/proofs/` directory — it was deleted wholesale in the Kani→Verus migration
  (CLAUDE.md; plan phase 2 closeout). The "delete the Kani it subsumes" discipline is
  already satisfied for these modules; phase 5 has nothing to delete.
- **"`cargo kani -p kcore` is gone" is already discharged.** The kcore Kani leg was
  retired in phase 2; the `kani` CI job covers only the host chokepoints (`urt`/`ipc`/
  `dma-pool`/`cas`) today. Phase 5 changes nothing there (those are phase 6).
- **The substantive work is the §4.5 + §4.6 ports**, the two remaining un-ported kcore
  object-machinery modules. Unlike phase 4 (which had to *fold in* the orphan `timer`
  module §7 never assigned a phase), phase 5 has **no orphan to fold**: `aspace` and
  `sysabi` are exactly §7-step-5's assignment, and `id.rs` (the handle newtypes) carries
  no operations. After phase 5, the only un-ported kcore code is the cross-object
  teardown residue (§1.5).

### Two design forks, decided here

- **Page-table representation — keep the slices.** The aspace walker is verified on its
  existing `&mut [u64;512]` / `&mut [[u64;512]]` signature, modelling the tables via
  `.view()` Seqs plus a `pt_lookup`/`pt_wf` spec (§1.2). The tables are **shell-owned and
  passed in**, not handle-reached object state, so the `Store`-view machinery (which
  exists to solve the aliasing of shared object state) buys nothing here — lifting them
  into the seam would be a wide `KernelStore` refactor for idiom consistency only.
  Rejected.
- **The §4.5 TLBI-ordering proof — in scope (5e), with a fallback.** A minimal ghost
  effect-log on the `Store`/`ExStore` seam makes "one TLBI per cleared page, in order" a
  real `unmap_in` postcondition (§2.5e). If the log's frame interaction with the slice
  mutation proves disproportionate, fall back to a structural host-test + a page-table-
  only postcondition and record the deferral — the doc-35 `check_expired` "attempt full,
  fall back" precedent.

### Discipline carried from phases 2–4 (applies to every sub-phase below)

- **One PR per sub-phase.** It merges only when green.
- **`cargo verus verify -p kcore` green before merge** — the CI `verus` job runs with no
  per-proof filter, so a new `verus!{}` obligation auto-gates (`3_verus-rewrite.md` §8).
  Baseline: **120 verified, 0 errors** (doc 35).
- **`cargo test -p kcore` green** — the `test_store`/host-test harness is the executable
  check of every `external_body` contract against its real body, and (new in phase 5) of
  the aspace walker's contracts against concrete arrays. Baseline: **43 passed** (doc 35).
- **The aarch64 `kernel` cross-build is unaffected** — `verus!{}` erases ghost code, so
  the erased `exec` body is byte-identical to today's plain Rust (`3_verus-rewrite.md`
  §6). Confirm with `cd kernel && cargo build` per sub-phase.
- **A `doc/results/N_verus-findings.md` increment per sub-phase**, recording what closed
  and the Verus-mechanics findings worth keeping (the doc-26…35 cadence). Phase 4
  produced docs 31–35 ("findings 11–15"); phase 5 produces **docs 36–40** ("findings
  16–20"), one per sub-phase, **numbered in landing order** (the doc-29/30 convention).

---

## 1. Dependency analysis — why this order

Phase 5 touches `sysabi.rs` (1 fn + a helper), `untyped.rs` (`ObjType::from_u64`),
`aspace.rs` (6 fns + 2 internal walkers), `cspace.rs` (the `ExStore` spec — three
effect-hook contracts + one ghost effect-log view), and `test_store.rs` /a new
`aspace::tests` (the first executable aspace-walker checks + the effect log). The
structure differs from phases 3 and 4 in three ways, all stated so the sub-phase shapes
are expected, not surprises.

### 1.1 Phase 5 *has* a clean island, and needs *no* wide enabling refactor

Phase 3 opened with **3a** (a `slot_view`-only confidence-builder) then did the wide
`chan_view` refactor (3b); phase 4 had **no** clean island so opened *directly* with the
refactor (4a). Phase 5 is the easy case on both axes:

- **The clean island exists:** `sysabi::decode` is pure data — no Store, no slices, no
  recursion. It re-banks the workflow before the slice-reasoning novelty (the 3a role).
  Phase 5 opens with it (**5a**).
- **There is no wide enabling refactor.** The page tables are concrete slices, not a
  `Store` view, so there is no six-view→seven-view accessor churn. The new model lands
  not in a standalone refactor PR but **with its first consumer** — the read-only
  `range_mapped_in` (**5c**) — exactly where the simplest proof exercises it. The only
  `Store`-seam change is the three effect-hook contracts (5d/5e), far smaller than
  3b/4a's accessor sweep.

### 1.2 The page-table partial-map is the one new proof model

Every kcore Verus proof to date (phases 2–4) reasons over `Map`/`Seq` ghost *views*
resolved through the `Store` seam; **none has touched a concrete Rust slice.** The aspace
walker operates on `&mut [u64;512]` (the L1 table) and `&mut [[u64;512]]` (the table
pool). So phase 5 introduces, for the first time in this codebase:

- **Verus slice/array reasoning** — `.view()` (a `Seq<u64>` / `Seq<Seq<u64>>`), indexed
  reads/writes, the `&mut [T]` no-aliasing reasoning. A flagged new surface (§3); a
  one-line slice-mutation spike at the top of 5c de-risks it before the real proof.
- **The page-table partial-map model** (the §4.5 ghost `Map<va_page,(pa,perms)>`, the
  analog of phase 3's FIFO `Seq` and phase 4's `waiter_seq`/`timer_seq`):
  - `spec fn pool_index_spec(pool_base, pool_len, desc) -> Option<int>` — mirrors
    `pool_index` (`aspace.rs:116`).
  - `spec fn pt_lookup(l1: Seq<u64>, pool: Seq<Seq<u64>>, pool_base, va) -> Option<u64>`
    — the spec walk: follow `l1[l1_index] → l2 → l3`, returning the leaf PTE if every
    level is a present table descriptor, else `None` (the spec analog of `lookup`
    `aspace.rs:189` + the `pool[l3][e]` read). Conceptually `pt_map(va) := pt_lookup(va)`
    gives the §4.5 `Map<va_page, pte>`; the pointwise form is what the proofs use (the
    per-node idiom, doc 27 §3 / doc 29 §1).
  - `spec fn pt_wf(l1, pool, pool_base, pool_used, pool_len) -> bool` — the table-pool
    well-formedness (the `chan_wf`/`notif_wf`/`timer_wf` analog), carrying **(a) pool
    accounting** (`pool_used ≤ pool_len`; used tables within `pool[0..pool_used]`),
    **(b) closure** (every table descriptor resolves via `pool_index_spec` to `idx <
    pool_used`), and **(c) the tree shape / no-aliasing invariant** — distinct present
    table descriptors point to **distinct** pool indices (the page table is a tree, not a
    DAG). (c) is the load-bearing invariant: it is what makes a leaf write *local* (so
    `map_in` writing `va`'s leaf cannot perturb `va'≠va`'s lookup unless they genuinely
    share the same L3 table entry). It is the phase-5 analog of CDT acyclicity (phase 2)
    and the ring-window coupling (3d).

This model + its frame lemmas are the new spec work and the chief design risk; they are
**designed in 5c** (with `range_mapped_in`) before any `map_in` proof, exactly as 3b
settled the channel coupling before 3d and 4a settled the waiter model before 4b.

### 1.3 No termination obligation — the first such phase since phase 2's revoke

The aspace walk is **fixed 3-level depth** (`walk_alloc` is straight-line, no recursion)
and the `map_in`/`unmap_in` loops are bounded `for i in 0..pages`, so Verus discharges
their termination automatically. The only explicit `decreases` is `range_mapped_in`'s
one `while page < end` loop (`decreases end - page`). So phase 5 carries **no termination
theorem** of the revoke/delete kind — a genuine simplification worth stating, since
termination was a recurring theme in phases 2/4 (the waiter and armed-list ranks). The
proof effort concentrates entirely in the §1.2 model and its frame lemmas.

### 1.4 The scheduler/hardware seam needs three minimal effect-hook contracts

`map_in` calls `store.barrier_after_map()` (`aspace.rs:236`); `unmap_in` calls
`store.tlb_invalidate_page(asid, va)` and `store.barrier_after_unmap()`
(`aspace.rs:256,259`). These three `Store` methods are **uncontracted** in `ExStore`
today (the comment at `cspace.rs:442` — "only the methods cspace/CDT calls are
contracted; the rest of the ~70-method seam is left unconstrained") — confirmed: only
`make_runnable` (`cspace.rs:963`) carries a contract. To call them from verified
`map_in`/`unmap_in`, phase 5 adds their `ExStore` entries (the 4a `make_runnable`
precedent). Because none of the three takes the table slices, Verus already knows they
cannot perturb `l1`/`pool` (separate `&mut` borrows) — so the **page-table postconditions
are independent of them regardless of contract strength.** Their contracts matter only
for the *effect-ordering* theorem (the TLBI log, 5e); for `barrier_after_map` (5d) a
trivial `ensures` (object views unchanged) suffices.

### 1.5 Cross-object teardown and the full `refcount_sound` census are OUT of phase 5

Phase 5 verifies the aspace **walker** (`map_in`/`unmap_in`/`range_mapped_in`/
`pte_encode`/`va_range_ok`) — the pure machinery the kernel shell calls directly. It does
**not** touch:

- **`delete`/`revoke` bodies** (`external_body` / plain Rust) — they recurse cross-object
  and call `aspace_unmap`/`aspace_destroy` (`cspace.rs:4815-4816`).
- **`unref_aspace`/`aspace_destroy`/`aspace_unmap`** (`cspace.rs:238`, `store.rs:122-124`)
  — the aspace **refcount/teardown term**: the per-cap-copy mapping refcount and the
  last-reference aspace destruction. These are part of `refcount_sound`'s **frame-mapping
  term** and the seL4-zombie recursion, deferred wholesale.
- **`obj_unref`/`destroy_cspace`/`unref_cspace`/`destroy_channel`/`destroy_tcb`** — the
  rest of the deferred teardown residue, unchanged from phase 4.

**The recommended dedicated cross-object-teardown phase** (closing all of the above
bodies, the seL4-zombie recursion measure, and the full `refcount_sound` census — of
which phases 3/4 landed the binding, waiter, and armed-timer terms) was recommended by
phase-4 detail §1.4 / doc 35 §4 to follow phase 5, *because it can only be attempted once
aspace is ported.* Phase 5 ports exactly the aspace machinery that phase unblocks. Phase
5 therefore **reaffirms** that phase as the next one and adds zero teardown work itself.

### 1.6 Resulting order

```
5a  sysabi decode + ObjType::from_u64    (pure data — the clean island; the 3a analog)
5b  pte_encode / pte_output_pa / va_range_ok   (pure bit/arith — the §2.5 isolation theorem)
5c  range_mapped_in + the pt_lookup/pt_wf model (read-only walk; the model lands here)
5d  map_in                                (the two-pass walk-alloc — the hard sub-phase)
5e  unmap_in + the TLBI effect-ordering log + closeout
```

`pte_output_pa` and `va_range_ok` ride with `pte_encode` in 5b (all pure, no slices);
they are the read-only corollaries `range_mapped_in`/`map_in` build on. The effect-hook
contracts land where their caller is proven: `barrier_after_map` in 5d, the
`tlb_invalidate_page`/`barrier_after_unmap` ghost-log in 5e.

---

## 2. The sub-phases

Each carries: scope · specs/contracts landed · key lemmas/risks · test additions · the
"done =" gate.

### 5a — sysabi `decode` + `ObjType::from_u64` (the confidence-builder; the 3a analog)

Pure data, no Store, no slices, no recursion — the cleanest possible Verus win, and the
clean island phase 4 lacked. First PR, to re-bank the workflow on the slice-free module
before the aspace novelty.

- **Scope.** Move `sysabi::decode` (`sysabi.rs:81`) and `decode_prio` (`:71`) into
  `verus!{}`; port `untyped::ObjType::from_u64` (`untyped.rs:62`, currently a plain-Rust
  impl wedged between two `verus!{}` blocks) into a `verus!{}` block (or attach an
  `assume_specification`) so `decode` can call it in verified code.
- **`ObjType::from_u64` contract.** Total over all `u64`; `Some(ty)` iff `v` is a valid
  discriminant; the round-trip `from_u64(ty as u64) == Some(ty)` (aligns with the
  existing `spec_align`/retype reasoning, `untyped.rs:167`). The §4.6 "`ObjType::from_u64`
  total" obligation.
- **`decode_prio` contract.** `ensures` on `Ok`: `prio < NUM_PRIOS`; the `(raw & 0xFF) as
  u8` cast is exact (mask < 256), proven automatically.
- **`decode` contract — the §3.7/§4.6 obligations.**
  - **Total**: for any `(nr, a) : (u64, [u64;6])` it returns `Ok(Sys)` or `Err(SysError)`,
    **never panics, never overflows, never UB** — free in Verus once in the block (the
    "unknown `nr` is an error, never a crash" claim as a theorem, not a review convention).
  - Per-arm validation as `ensures`: `ChanSend.len ≤ MSG_PAYLOAD` (the cap at `sysabi.rs:96`
    that precedes `channel::send`'s `as u16` truncation — it discharges send's existing
    `data.len() ≤ MSG_PAYLOAD` precondition, `channel.rs:362`); `ChanBind.event < 3`;
    `ThreadBind.which < 2`; `prio < NUM_PRIOS` for both `ThreadStart`/`ThreadStartAs`;
    unknown `nr ⇒ Err(UnknownCall)`; bad discriminant `⇒ Err(BadObjType)`.
  - The narrowing casts (`a[1] as usize`, the prio path, the length compare) are proven
    in range by the preceding guards — the §4.6 "length validated ≤ `MSG_PAYLOAD` before
    any `as u16`; slot/event/which/prio bounded before use" as checked facts.
- **Mechanics.** No lemmas. The only subtlety is the casts, which the guards already
  bound; `ObjType::from_u64` must become spec-visible (the one cross-module touch).
- **Tests.** The existing `#[cfg(test)] mod tests` in `sysabi.rs` (`known_calls_decode`,
  `validation_rejects`, `prio_is_masked_then_bounded`) stays as the differential check.
  **No `external_body`** — `decode`/`from_u64` are fully proven, so nothing is assumed and
  no `test_store` contract check is needed.
- **Done =** verus green + `cargo test -p kcore` + `cd kernel && cargo build`. Findings
  doc **36**.

### 5b — `pte_encode` / `pte_output_pa` / `va_range_ok` (the §2.5 isolation theorem)

Pure bit/index arithmetic — no slices mutated, no Store. The load-bearing §4.5 security
property, given its own sub-phase.

- **Scope.** Move `pte_encode` (`aspace.rs:133`), `pte_output_pa` (`:143`), and
  `va_range_ok` (`:102`) into `verus!{}`. The L1/L2/L3 index helpers (`:89-97`) ride with
  them (they are needed by the spec walk in 5c).
- **`pte_encode` contract — the isolation theorem, ∀ `(pa, perms)`.**
  - **AF and PXN unconditional** (user pages are never EL1-executable);
  - `PERM_W` set ⇒ `AP == AP_EL0_RW` (`0b01`), else `AP_EL0_RO` (`0b11`) — so the
    `(pte >> 6) & 0b11 == 0b01` writability test in `range_mapped_in` aligns (the 5c
    bridge);
  - `PERM_DEVICE` ⇒ `UXN` set (non-exec) + `SH_NONE` + `ATTR_DEVICE`, **even when
    `PERM_X` is set** — the AS-1 finding (the old kernel walker honoured `PERM_X` on
    device memory; the comment at `aspace.rs:131`), now "device is never executable" as a
    theorem;
  - `¬PERM_X` (and ¬device) ⇒ `UXN` set;
  - the output-address field `== pa & ADDR_MASK` (bits [47:12]); the control bits never
    collide with the address field (disjoint masks).
  - **The security corollary:** no `perms` yields a descriptor that is EL1-writable or
    EL0-kernel-executable — `PXN` always set, `UXN` set whenever device-or-¬X, and `AP`
    never grants EL0 write without `PERM_W`. (The §4.5 "no perms combination yields an
    EL1-writable-EL0-visible or EL0-executable-kernel page.")
- **`pte_output_pa` round-trip.** `pte_output_pa(pte_encode(pa, perms)) == pa &
  ADDR_MASK` (the address field round-trips; the low 12 bits are intentionally dropped).
- **`va_range_ok` / va_bounds corollary.** `va_range_ok(va, pages) ⇒ va % PAGE == 0 ∧ va
  ≥ USER_VA_BASE ∧ va + pages*PAGE ≤ USER_VA_END` (the `saturating_add`/`saturating_mul`
  are exact under the bound). **The user-L1 corollary:** any `va` with `va_range_ok` has
  `l1_index(va) ≥ l1_index(USER_VA_BASE)` (= 2 for `USER_VA_BASE = 0x8000_0000`), so a
  user mapping **never touches the two shared kernel L1 entries** (indices 0/1) — the
  §4.5 "user L1 indices never touch the two shared kernel entries" as a theorem. Stated
  here; consumed by 5d's `walk_alloc` reasoning.
- **Mechanics.** Pure bit-vector reasoning; quarantine the awkward mask/shift goals into
  one-line `assert(...) by (bit_vector)` helpers (the doc-25 §2 / doc-29 §2.4 "isolate the
  hard step; decomposition beats an rlimit bump" discipline — Verus's `bit_vector` solver
  is reliable on isolated goals). The `saturating_*` semantics in `va_range_ok` are
  modelled with the `vstd` saturating specs.
- **Tests.** The **first executable aspace tests** — a new `#[cfg(test)] mod tests` in
  `aspace.rs` (no `ArrayStore` needed; pure functions): `pte_encode` arms (W vs RO; device
  ignores `PERM_X` — the AS-1 regression; ¬X sets UXN) asserting the *named* bit constants
  (`aspace.rs:40-52`); `pte_output_pa` round-trip; `va_range_ok` boundaries (below base,
  at `USER_VA_END`, the `saturating` overflow edge); the `l1_index ≥ 2` corollary.
- **Done =** verus green + test + cross-build. Findings doc **37**.

### 5c — `range_mapped_in` + the page-table partial-map model (the new spec machinery)

The model lands with its first (read-only) consumer — the structural difference from
3b/4a, where the model needed a standalone refactor because it was a `Store` view. Here
the tables are concrete slices, so `pt_lookup`/`pt_wf` are `spec fn`s over `Seq`s that
land naturally with `range_mapped_in` (which *is* "is `va` in the ghost map, writably?").

- **Scope.** Move `range_mapped_in` (`aspace.rs:265`) and the read-only walker `lookup`
  (`:189`) + `pool_index` (`:116`) into `verus!{}`; define `pool_index_spec`, `pt_lookup`,
  and `pt_wf` (§1.2). **Open with a one-line slice-mutation/`.view()` spike** to settle
  the Verus slice idiom before the real proof (the §3 new-surface mitigation).
- **The model** (§1.2): `pool_index_spec`, `pt_lookup`, `pt_wf` (pool accounting +
  closure + the tree-shape/no-aliasing invariant). Settle the head/leaf-read coupling and
  the `Seq` shapes here so 5d/5e build on a settled representation (the 3b→3d, 4a→4b
  discipline). Defer to 5d/5e any extra `pt_wf` clause an op pays for (the doc-27 §3 /
  doc-29 §1 "add the clause when the op needs it" discipline).
- **`range_mapped_in` contract — full functional equivalence, ∀ `(va, len, write)`** (the
  predicate the syscall layer trusts before dereferencing user pointers, `syscall.rs:86,94`):
  - `len == 0 ⇒` result `== (USER_VA_BASE ≤ va < USER_VA_END)`;
  - `va.checked_add(len)` overflow, or the range escaping `[USER_VA_BASE, USER_VA_END)`
    `⇒ false`;
  - else result `⇔ ∀ page ∈ [va & !(PAGE-1), end) : pt_lookup(page) is Some ∧ ≠ 0 ∧
    (write ⇒ (pte >> 6) & 0b11 == 0b01)` — i.e. exactly the ghost containment (+
    writability). The `while page < end` loop gets `decreases end - page` and a loop
    invariant that the walked prefix is fully mapped (+ writable). Includes the `len == 0`
    and `va + len` overflow edges (the §4.5 "for all `va,len` including `len==0` and the
    `va.checked_add(len)` overflow edges").
- **Mechanics.** Read-only walk (no mutation, no `Store`) — the easiest of the three
  slice ops, deliberately first to exercise the model and the Verus slice surface. The
  writability bit-extract connects to 5b's `pte_encode` AP theorem.
- **Tests.** `range_mapped_in` over hand-built `[[u64;512]]` arrays: a fully mapped RW
  range, an RO range with `write=true` (rejected), a range with a hole, the `len==0` /
  overflow / below-base edges. (Reuse the 5b `aspace::tests` module.)
- **Done =** verus green + test + cross-build. Findings doc **38**.

### 5d — `map_in` (the two-pass walk-alloc — the hard sub-phase; the 3d/4b analog)

The §4.5 centerpiece and phase 5's hardest sub-phase; the analog of the channel FIFO
(3d) and the waiter queue (4b). Isolate it so the difficulty is contained. Budget the
most time.

- **Scope.** Move `map_in` (`aspace.rs:212`), `walk_alloc` (`:164`), and `alloc_table`
  (`:152`) into `verus!{}`.
- **The one Store-seam touch:** add `barrier_after_map` to the `ExStore` spec (a trivial
  `ensures` framing the object views unchanged — `map_in` asserts nothing about hardware
  effects beyond ordering, which is 5e's job). Because it does not take the slices, the
  page-table postcondition is independent of it (§1.4).
- **`map_in` contract against the ghost model.**
  - `¬va_range_ok ⇒ Err(BadVa)`, tables + `pool_used` unchanged;
  - **atomicity:** any page already mapped ⇒ `Err(AlreadyMapped)` with **no leaf written**
    for any page in the range (pass 1, `aspace.rs:226-231`, rejects before pass 2 writes);
    pool exhaustion in pass 1 ⇒ `Err(NeedMemory)`;
  - **the two-pass theorem (the §4.5 `check_map_model`):** because pass 1 walked and
    allocated the tables along the *whole* range, pass 2's `walk_alloc` for every
    `va+i*PAGE` is a pure lookup (all tables present), so **pass 2 allocates nothing and
    cannot return `NeedMemory`** — stated as: after pass 1, every `walk_alloc(va+i*PAGE)`
    succeeds without touching `pool_used`;
  - on `Ok`: ∀ `i < pages`, `pt_lookup(va + i*PAGE) == Some(pte_encode(pa + i*PAGE,
    perms))` (**adds exactly the requested pages**); every **other** VA's `pt_lookup`
    unchanged (**no nonzero PTE overwritten** — the frame); `pt_wf` preserved; `*pool_used`
    only grows (monotone);
  - **pool accounting:** `pool_used ≤ pool_len` maintained throughout (the §4.5 "`pool_used
    ≤ pool_pages` always; `NeedMemory` exactly at exhaustion; tables zeroed at
    allocation" — `alloc_table` writes `[0u64;512]`, so a fresh table contributes no
    spurious mapping).
- **Key lemmas / risk — the chief risk of phase 5.**
  - **The tree-shape frame lemma** (the load-bearing one): under `pt_wf`, allocating a
    fresh table (index `pool_used`, then `++`) and linking it via a table descriptor
    preserves `pt_wf` and changes **no existing** `pt_lookup(va')`; and writing a leaf at
    `(l3, e)` changes only the `pt_lookup(va)` whose walk lands at `(l3, e)`. This is the
    §4.5 analog of CDT `lemma_local_cap_edit_preserves_cspace_wf` (`cspace.rs:2316`) and
    the ring-window coupling (3d) — expect bespoke lemmas.
  - **Two-pass coupling:** relate pass-1's allocations to pass-2's lookups (pass 2 sees the
    tables pass 1 built; the `*pool_used` high-water mark only grew).
  - **No `decreases`** for the structure (§1.3) — `walk_alloc` is straight-line, the
    passes are bounded `for i in 0..pages`.
  - Quarantine the index/offset arithmetic (`l1_index`/`l2_index`/`l3_index` shifts,
    `pa_of_table`'s `idx*PAGE`, the `va + i*PAGE`) into one-line `bit_vector` /
    `nonlinear_arith` helpers (doc-25 §2).
- **Tests.** `map_in` over hand-built arrays: single page; multi-page; `AlreadyMapped`
  (overlap rejected atomically — assert **no partial write**); `NeedMemory` at pool
  exhaustion; the post-map `pt_lookup` / `range_mapped_in` round-trip; a small randomized
  map sweep (the `randomized_sweep` cadence, `test_store.rs:573`). A tiny local `Store`
  stub — or `ArrayStore` — supplies the effect hooks.
- **Done =** verus green + test + cross-build. Findings doc **39**.

### 5e — `unmap_in` + the TLBI/barrier effect-ordering log + phase-5 closeout

The read-mostly counterpart (leaf-clear, no allocation, no tree growth — lighter than
5d), the decided effect-ordering proof, and the documentation closeout (the 3e/4e analog).

- **Scope.** Move `unmap_in` (`aspace.rs:243`) into `verus!{}`; add the minimal ghost
  effect-log to the seam; write the closeout.
- **The effect-ordering ghost log (the in-scope §4.5 "the asm seam exists for this"
  obligation).** Add a ghost view to `ExStore` — `spec fn tlb_log(&self) -> Seq<(u16,
  u64)>` (or a small effect-enum `Seq` covering the per-page TLBIs + the barrier markers)
  — and contracts:
  - `tlb_invalidate_page(asid, va)`: `ensures tlb_log() == old.tlb_log().push((asid,
    va))`; frames all object views (and the 5d `barrier_after_map`'s clauses) unchanged;
  - `barrier_after_unmap` / `barrier_after_map`: append a barrier marker (or frame the log
    unchanged — settle the minimal shape here); frame the object views unchanged;
  - `ArrayStore` gains a real `Vec<(u16,u64)>` log behind these (replacing the no-ops at
    `test_store.rs:300-307`) so `check_unmap` host-verifies the contract — the
    `delete`-contract / `check_signal_frame` discipline.
- **`unmap_in` contract.**
  - on return: ∀ `i < pages`, a page at `va+i*PAGE` that **was** present is now
    `pt_lookup == None`/`0` (**clears exactly the mapped pages**); pages with no L3 table
    are skipped (no spurious clear); every VA **outside** the range unchanged (`pt_lookup`
    framed); `pt_wf` preserved (clearing a leaf keeps the tree shape — tables are *not*
    freed, only leaves zeroed, so pool accounting / closure / no-aliasing all hold);
  - **the effect-ordering theorem:** `tlb_log()` grows by exactly one `(asid, va+i*PAGE)`
    per **cleared** page, **in ascending `i` order**, followed by the unmap barrier — the
    §4.5 "one TLBI per cleared page." The `for` loop invariant tracks `tlb_log() == old ++
    (the cleared prefix's TLBIs)`.
  - **Fallback (the doc-35 `check_expired` discipline):** if the log's frame interaction
    with the leaf-clear mutation proves disproportionate, fall back to a structural
    host-test of the ordering + a page-table-only `unmap_in` postcondition, and **record
    the deferral** — *attempt full, fall back*, recorded, not silently dropped.
- **Key lemmas / risk.** The log-frame interacting with the leaf-clear mutation (two
  distinct mutable targets — `store` and `pool` — in one loop); reuse 5c's `pt_lookup`
  frame facts for the clear-is-local step. Lighter than 5d (no allocation, no tree
  growth).
- **Tests.** `check_unmap`: clear a mapped range (assert pages gone + the TLBI log equals
  the expected `(asid, va)` sequence, in order); unmap of an unmapped range (no TLBIs, no
  panic); a partial overlap; the map→unmap→`range_mapped_in` round-trip.
- **Closeout.**
  - Write `doc/results/40_verus-findings.md` (what closed in 5a–5e; the `pt_lookup`/
    `pt_wf` tree-shape model; the first-slice-reasoning mechanics + the `bit_vector`-
    isolation findings; the effect-log shape; the no-termination-obligation note; the
    doc-numbering note).
  - Update `CLAUDE.md`'s `### Verus` section + the §6 verification-tier table: move sysabi
    `decode` (+ `ObjType::from_u64`) and aspace `pte_encode`/`pte_output_pa`/`va_range_ok`/
    `range_mapped_in`/`map_in`/`unmap_in` onto the **proven** list (with the PTE isolation
    theorem, the `range_mapped` functional-equivalence, and the TLBI-ordering effect
    theorem among them); note that aspace + sysabi add **no `external_body`** (phase 5 is
    the first phase since phase 2 to add zero trusted residue — 3e left
    `destroy_channel`/`signal`, 4e left `destroy_tcb`); record that the §7-step-5 clauses
    "delete `proofs/{aspace,sysabi}.rs`" and "`cargo kani -p kcore` is gone" were already
    discharged (§0); **reaffirm the recommended cross-object-teardown phase as the next
    one** (now unblocked — aspace's walker is ported, §1.5). No spec-doc edit — that is the
    phase-8 closeout (doc 30 §3).
- **Done =** verus green + test + cross-build. Findings doc **40**.

---

## 3. Risks & mitigations (phase-2/3/4-informed)

- **Verus slice/array reasoning is a new surface for kcore (5c/5d).** Every prior phase
  reasoned over `Map`/`Seq` *views*; the aspace walker is the first verified code over
  raw `&mut [[u64;512]]`. The realistic repeat of the 3d-FIFO / 4b-waiter "new model"
  surprise, here in the representation rather than the algorithm. Mitigation: a one-line
  `.view()`/slice-mutation spike opens 5c before the real proof; the model (`pt_lookup`/
  `pt_wf`) is designed in 5c before any `map_in` proof; sysabi (5a) banks the workflow
  first on a slice-free module.
- **The tree-shape no-aliasing invariant balloons (5d).** The page-table-as-tree frame
  lemma is the load-bearing proof, the analog of CDT acyclicity. Mitigation: it is its own
  sub-phase, isolated; index/offset arithmetic is quarantined into `bit_vector`/
  `nonlinear_arith` one-liners (doc-25 §2); `pt_wf`'s clauses are added only as an op pays
  for them (doc-27 §3).
- **The TLBI ghost-log frame-interacts with the slice mutation (5e).** Two mutable targets
  in one loop. Mitigation: the decided fallback (structural host-test + page-table-only
  postcondition, recorded) — the doc-35 `check_expired` "attempt full, fall back"
  precedent; reuse 5c's `pt_lookup` frame facts.
- **`ObjType::from_u64`'s cross-`verus!{}`-block move (5a).** It currently sits in a
  plain-Rust impl between two `verus!{}` blocks (`untyped.rs:62`). Mitigation: a small,
  contained move into a `verus!{}` block (or an `assume_specification`), gated by phase-3's
  retype proofs (which already consume `ObjType`) staying green.
- **Scope creep into the cross-object teardown / the aspace refcount term.** Mitigation:
  declared a non-goal in §1.5 and §4, and the recommended dedicated teardown phase gives
  it an explicit home (now unblocked) so it is not silently absorbed into phase 5.

---

## 4. Out of scope (phase-5 non-goals)

- **The cross-object teardown** — `delete`/`revoke`/`obj_unref`/`destroy_cspace`/
  `destroy_channel`/`destroy_tcb` bodies, `unref_cspace`/`unref_aspace`/`aspace_destroy`/
  `aspace_unmap`, the seL4-zombie recursion measure, and the **full `refcount_sound`
  census** (including the **frame-mapping term** the aspace mappings contribute) — the
  recommended dedicated phase **after** phase 5 (doc 35 §4 / phase-4 detail §1.4), now
  unblocked by the aspace-walker port. Phase 5 adds no teardown work.
- **Lifting the page tables into the `Store` seam** — decided against (§0); the walker is
  verified on its slice signature.
- **The host chokepoints (phase 6, §4.7), the commit-protocol recovery core (phase 7,
  §4.8), and the phase-8 spec/`CLAUDE.md`/Kani closeout.**
- **Re-opening `notification::signal`'s / the phase-3/4 callers' contracts** — phase 5
  touches no object op; the effect-hook contracts it adds are new, not amendments.

---

## 5. Exit criterion for phase 5

`cargo verus verify -p kcore` proves:

- **sysabi** `decode` (total over `(u64, [u64;6])` — the §3.7 "unknown `nr` is an error,
  never a crash"; the §4.6 length/event/which/prio validation as `ensures`) and
  `ObjType::from_u64` (total);
- **aspace** `pte_encode` (the §2.5/§4.5 isolation theorem ∀ `perms`, including
  device-never-executable / the AS-1 fix), `pte_output_pa` (round-trip), `va_range_ok`
  (+ the user-L1-never-touches-kernel-entries corollary), `range_mapped_in` (full
  functional equivalence to the `pt_lookup` ghost containment + writability, ∀ `(va,len)`
  incl `len==0` and `va+len` overflow), `map_in` (adds exactly the requested pages or
  fails atomically; `AlreadyMapped`/`NeedMemory`; the two-pass totality; pool accounting;
  the no-overwrite frame), and `unmap_in` (clears exactly the mapped pages; one TLBI per
  cleared page in order — the effect-ordering theorem — *or* the recorded fallback);

all against the new `pt_wf` tree-shape/pool-accounting invariant; with **no termination
obligation** (fixed-depth walk + bounded loops; `range_mapped_in`'s one `while` aside);
the only `Store`-seam change is the three minimal effect-hook contracts + the TLBI
ghost-log; **aspace and sysabi carry no `external_body`** (fully proven — phase 5 adds no
trusted residue, the first phase since phase 2 to do so); the aarch64 `kernel` build and
`cargo test -p kcore` are green (the first executable aspace-walker host tests land);
`doc/results/40` and `CLAUDE.md` record the new division, note the already-discharged
§7-step-5 clauses, and reaffirm the cross-object-teardown phase as the next, now-unblocked
phase. The cross-object teardown and the full `refcount_sound` census pass forward to that
dedicated phase unchanged.
