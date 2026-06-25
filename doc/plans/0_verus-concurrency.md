# Bringing concurrent (and selected sequential) code under Verus

A guideline plus an ordered, phased task schedule for extending Verus
verification across this project's concurrent code, the proptest-routed
shared-state cores, and the selected sequential decoders/predicates that were
previously out of Verus scope. Grounded in Travis Hance's thesis (tokenized
state machines / VerusSync, `doc/plans/hance_thesis.txt`) and the
verus-state-machines reference (`doc/plans/verus-state-machines/`).

Read this alongside the three load-bearing guidelines: `doc/guidelines/verus.md`
(the working discipline + technique catalog), `doc/guidelines/verus_trusted-base.md`
(the seam ledger), and `doc/guidelines/verification.md` (the method dispatcher).
All spec references carry the revision, e.g. "rev2§6".

---

## 1. Approach / guideline

### 1.1 The thesis in one paragraph

This project has already paid the up-front cost of deductive verification on its
sequential cores and found the trade worth it: a Verus proof discharges a property
**for all inputs, all values, all sizes** in a single mechanical-verification run,
where TLC/Loom/Shuttle enumerate only a bounded slice (e.g. `CapIds=5` blows the
15-minute CI budget; IpcReactor's TLC run is bounded to `MaxMsgs 3 / QueueDepth 2`).
Hance's tokenized state machines (VerusSync) extend that same deductive,
unbounded coverage to *concurrent* shared state: you describe the protocol once as
an abstract transition system, discharge its safety invariants with
`#[invariant]`/`#[inductive]` (inductive-invariant reasoning, not state
enumeration), and — where there is a physical atomic — connect the abstract ghost
state to the running code through tracked tokens so the proof is a statement about
the *shipped binary* (erasure, `verus.md` §3). The price is implementation effort
(rewriting code into the transition shape, threading tokens) and a higher proof
ceiling, which the project has decided to accept for properties where the bounded
model-checker is the only current guard. The discipline is *selective*: not every
hard concurrency property is a VerusSync proof (thesis §4), and a faster proof
that proves *less* is a regression (`verus.md` §5).

### 1.2 The technique toolbox under the pin

The pin (binary `0.2026.06.07.cd03505`, `vstd =0.0.0-2026-05-31-0205`,
toolchain `1.95.0`) bundles the full state-machine macro suite. One line each on
when to reach for each tool:

- **`state_machine!{}`** — a *single-threaded* abstract transition system. Reach
  for it to prove a safety predicate holds on all reachable states of a protocol
  *model* (e.g. an abstract twin of a TLA safety arm). It proves the model is
  inductive; it does **not** by itself certify exec code.
- **`tokenized_state_machine!{}`** — `state_machine!` plus ghost tokens (sharding
  strategies) so the abstract state can be split across threads and tied to
  physical atomics. Reach for it only when there is genuine cross-thread sharing
  of a physical atomic to connect tokens to.
- **`atomic_with_ghost!` / `vstd::atomic_ghost` (`AtomicU64`/`AtomicI64`)** —
  fuses one atomic op with one ghost transition step, opening the cell's
  invariant for that single instruction. Reach for it to bind a token-carrying
  atomic to a tokenized SM. **SeqCst-only** (see below).
- **`open_atomic_invariant` / `AtomicInvariant`** — the lower-level invariant a
  single atomic op opens; `atomic_with_ghost!` is sugar over it.
- **`PCell` / `PPtr` (`vstd::cell`, `vstd::simple_pptr`)** — `PointsTo`-permissioned
  interior-mutable cell / pointer. Reach for it when verified code must own and
  hand out raw byte/cell permissions (the mimalloc fungible-memory shape).
- **`RwLock` (`vstd::rwlock`)** — a verified reader/writer lock carrying a
  data invariant. Reach for it when the design genuinely wants a lock with a
  ghost-protected payload.
- **`logatom` (linearizability / logical-atomicity helpers)** — for proving a
  concurrent operation linearizes to one atomic step. Reach for it only for a
  true linearizability obligation.
- **Plain first-order Verus** (`spec fn` `wf` model + `exec fn`
  `requires`/`ensures` + `proof fn` lemmas, `Seq`/`Map`/`Set`, `decreases`,
  `by (bit_vector)`, `by (nonlinear_arith)`) — the default. **Most candidates
  below need only this**, despite their concurrency framing.

**MISSING from the pin — flagged by the feasibility verdicts, with the handling:**

1. **No Relaxed / Acquire / Release atomics and no standalone `fence`.** Every
   `vstd` atomic op is hardcoded `SeqCst` (`reference/vstd/atomic.rs`). A proof
   built on `atomic_with_ghost!` therefore models a **SeqCst machine**. Handling:
   do **not** silently certify Relaxed+fence ship code with a SeqCst proof — that
   is a false statement about the binary (erasure cuts both ways). Either prove
   only the pure part, or, if a SeqCst model is built, label it explicitly as a
   model that does **not** supersede the Loom proof of record. This single gap
   kills the urt seqlock candidates (§4).
2. **`verus_state_machines_macros` is not on crates.io.** `vstd` depends on it
   internally but does not re-export it; the macro crate ships only inside the
   Verus install. Handling: any crate authoring a `state_machine!{}` must add a
   CI-resolvable path/vendored dep into the install dir — solve this build-graph
   problem *before* writing any proof, and treat it as the highest-risk part of
   any SM task.
3. **No way to mint a `PointsToRaw` for a pre-existing `static [u8;N]`.**
   `PointsToRaw` constructors are `empty()`, `allocate()` (std/heap-gated), and
   `PointsTo::into_raw`; `expose_provenance` yields only an `IsExposed` token.
   Handling: a static-arena permission would require a hand-written `external_body`
   fabrication axiom — a *new* trusted seam, net-worse than the existing
   Miri+proptest guard. This kills the urt heap-arena `PointsToRaw` candidate (§4).
4. **`vstd::thread` is std-only**, unusable in the bare-metal userspace runtime.
   Handling: no Verus thread-spawning proofs in `urt`/`ipc` userspace; concurrency
   interleaving stays Loom/Shuttle.

### 1.3 How TLA+ relates

- **Verus replaces TLA where the property is a per-step / inductive safety
  invariant over finite first-order state** — the *unbounded* deductive twin of a
  TLA safety arm (e.g. IpcReactor `NoLostWakeup`/`NoDrop`/`FifoPerChannel` as
  `#[invariant]` over a `Seq` model). When this is done it must **re-route, not
  duplicate**: the TLA safety invariant is retired (or explicitly demoted to a
  smoke tier) and the move recorded, per the "no trust-routed property mistaken
  for mechanized" rule.
- **TLA stays for liveness, fairness, unbounded cross-restart/cross-process
  interleaving, and environment (crash/disk) modeling** — `EventuallyDelivered`,
  `EventuallyRevoked`, the CapRevocation cross-quantum walk, the CommitProtocol
  Crash three-outcome + `FsyncMeansFsync` axiom. Verus has no temporal logic, no
  fairness, no crash model; these are irreducibly TLA-only (thesis §4).
- **TLA invariants that stay become labeled obligations on the Rust.** Where a TLA
  property is liveness-owned but rests on a *per-step* safety fact the code can
  carry (e.g. `ReportMonotone`'s single write-once step), that fact becomes a
  named `ensures` on the relevant op, with the temporal closure left in TLA. The
  ledger routing note records which facts moved and which stayed.

### 1.4 Decision rule — rewrite in Verus vs keep as Loom/Shuttle/TLA

Apply, in order:

1. **Is the property liveness, fairness, or unbounded environment/crash
   interleaving?** → **TLA only.** Verus cannot express it. Stop.
2. **Does the proof require a memory ordering weaker than SeqCst, or a standalone
   fence, to be faithful to the ship code?** → **Loom (certifying) / Shuttle
   (smoke).** The pin cannot model it; a SeqCst Verus proof would certify a
   different binary. Stop.
3. **Is the load-bearing state a physical atomic shared across threads?** → only
   then is `tokenized_state_machine!` + `atomic_with_ghost!` warranted, *and* only
   if step 2 passes. Otherwise the SM machinery buys nothing.
4. **Is the property a per-step / inductive safety invariant over finite
   first-order state (`Seq`/`Map`/`Set`/bitmasks), single-threaded or
   interrupt-masked?** → **plain Verus** (`wf` + `requires`/`ensures` + lemmas).
   This is the foundational posture (`verus.md` §1) and covers the large majority.
5. **Would the rewrite merely re-state an already-verified `ensures` under a
   fancier construct, or refine a struct to itself with no second abstraction?**
   → **do not pursue** (net-negative; thesis §4).

A `state_machine!{}` *model* (no tokens) is warranted only when the
machine-checked TLA↔code bridge is independently judged worth a new artifact and
someone commits to writing the exec-refines-model glue — otherwise it is
additive scaffolding restating facts already proven (see Deferred, §4).

---

## 2. Working procedure (mandatory)

Every task in §3 obeys these rules.

1. **Conform to `doc/guidelines/verus.md`.** The load-bearing constraints, in
   force on every attempt:
   - **The pin moves as one unit.** Confirm `verus --version` prints
     `Version: 0.2026.06.07.cd03505, Toolchain: 1.95.0-...` before trusting any
     result. A toolchain/`vstd`/binary bump is its own PR, never folded into a
     feature change.
   - **The CI gate runs one `cargo verus verify -p <crate>` per crate with no
     per-proof filter.** A new `verus!{}` obligation auto-gates. The gate counts
     verified *items*, not lines; predict the count delta from new fns, not
     changed code.
   - **The `verification results:: N verified, 0 errors` line is the proof of a
     real run.** Its *absence* means a stale-cache false-green. Force a real run
     with `cargo clean -p <crate> && cargo verus verify -p <crate>` (or full
     `cargo clean` for the whole gate); clean when in doubt — especially after
     editing a shared spec/predicate.
   - **Erasure + frame-staleness audit.** `verus!{}` erases to nothing, so the
     host build and aarch64 cross-build link the same `exec` code the proofs run
     against — a green proof is a statement about the shipped binary. But a green
     `cargo build`/`cargo test` is *no evidence* the spec models track the code:
     after any struct/field change to a mirrored type, run `cargo verus verify`
     (the only complete frame audit).
   - **Trusted-base ledger rule.** A seam earns a row only with **both** (a) a
     reason it is a genuine boundary **and** (b) the host test that exercises it
     (with a `_has_teeth` negative control). `external_body`/`assume_specification`
     only at a genuine boundary; no bare in-proof `assume` survives. When ledger
     and code disagree, code is authoritative (re-derive with
     `rg "external_body|assume_specification"`).
   - **Measure every proof change against a freshly re-derived baseline.** There
     is no committed baseline; run `scripts/verus-baseline.sh` on the *pre-change*
     tree (or `git stash`/checkout the merge-base), then on the changed tree, and
     diff. Judge by deterministic `rlimit` on cold (`cargo clean`) runs against
     byte-identical controls; wall-clock is advisory. Keep a perf change only if
     it measurably helps or at least does not regress the crate's `rlimit` total.
   - **Correctness outranks speed.** Never weaken a spec, drop/skip an obligation,
     loosen an `ensures`, or narrow input coverage to make the prover faster.

2. **Every attempt produces a NEW findings doc** in `doc/results/`, named
   `0_verus-findings.md`, `1_verus-findings.md`, … (the directory exists). Each
   findings doc records: **what was attempted**; **the result** (verified-item
   counts, `rlimit` deltas vs the freshly re-derived baseline, what failed and the
   specific obligation/trigger/ordering wall it hit); and **proposed additions to
   `doc/guidelines/verus.md`** (and, if a seam changed, to
   `doc/guidelines/verus_trusted-base.md`). The findings doc is the durable
   artifact even when the code is reverted.

3. **Failed attempts are REVERTED; the findings doc stays.** Code that was
   considered but could not be made to verify is reverted from the tree — but (a)
   the findings doc recording the dead end **stays**, so the next implementer does
   not re-walk it, and (b) any **incidental code improvements** found along the way
   (a tightened proptest, a `_has_teeth` control, a clarified comment, a bug fix in
   plain code) **stay**. State this explicitly in each findings doc: what was
   reverted, what was kept.

4. **A task that changes the trusted base updates the ledger and keeps the tally
   honest.** Adding, removing, or narrowing a seam (any new `external_body`/
   `assume_specification`, or onboarding a new gated crate that establishes a
   Baseline) must update `doc/guidelines/verus_trusted-base.md`: add/adjust the
   row with its boundary reason + host test, and update the 14-seam tally and the
   per-crate Baselines. Onboarding a crate also updates the canonical gate list in
   `CLAUDE.md` and the CI `verus` job.

---

## 3. Task schedule

**Ordering rationale.** The user's importance tiers (1 unverified > 2
loom/shuttle > 3 TLA > 4 already-verified) are balanced against feasibility,
risk, and dependencies. We front-load (a) the **zero-cost documentation/scoping
anchors** that cost nothing and de-risk later work by fixing the routing, (b) a
**cheap pure-arithmetic pilot** that exercises crate-onboarding end-to-end at
near-zero proof risk, and (c) the **low-risk tier-2 promotion** that extends an
already-verified Verus core (`lowest_clear_bit`) — retiring the
toolchain/onboarding risk before any larger or harder task. Harder tier-1
decoders and the tier-3 TLA-conformance labels follow once onboarding is proven.
The genuinely infeasible items (urt seqlock family, heap `PointsToRaw`, the
state-machine NET-NEGATIVEs) are dropped to §4.

> **PILOT (do this first):** Task 1 below
> (`reactor-alloc-bit-verus`) is the recommended low-risk pilot. It extends the
> *already-verified* `lowest_clear_bit` in an *already-gated* crate (`ipc`),
> needs **no** new crate onboarding and **no** state-machine machinery, yet
> exercises the full loop: `by (bit_vector)` bitmask reasoning, an array-coherence
> `forall` invariant, a `decreases` loop, the cold-run baseline measurement, and
> the ledger Baseline bump. It retires the technique risk for the tier-2 dispatch
> work cheaply. (The verdicts pointed at either the urt seqlock or the ipc
> reactor for the pilot; the seqlock is infeasible under the pin — §4 — so the
> reactor is the pilot.)

---

### Task 1 — Extend `lowest_clear_bit` to `alloc_bit` / `register_bound` / pure drain step  **[PILOT]**

- **Tier 2** (proptest-routed dispatch surface) · **Feasibility: partial (yes for the scoped pure core)**
- **Goal:** Convert the proptest-only characterizations (`alloc_bit_is_lowest_clear`,
  `register_sequence_keeps_used_coherent`, `pending_drain_is_lowest_first`) into
  deductive `ensures` on the exec functions. Prove: `alloc_bit` returns a clear
  bit, sets exactly it (`new used == old | (1<<bit)`), `None` iff `used == u64::MAX`;
  `register_bound` sets exactly `mask` in `used` and the matching `slots`, leaves
  `used` unchanged on `Taken`, and maintains slot/used coherence
  (`slots[b].is_some() <==> used bit b set`); a pure `drain_one(pending) -> (u32,u64)`
  helper returns the lowest set bit and clears exactly it.
- **Technique + constructs:** plain Verus. `verus!{}` exec fns with `ensures`;
  reuse the existing `lowest_clear_bit` lemma; `broadcast use
  vstd::std_specs::bits::axiom_u64_trailing_zeros`; `by (bit_vector)` for the
  set/clear identities; a `while`-loop invariant + `decreases bits` (justified by
  `bits & (bits-1) < bits` for `bits != 0`); reuse the `kcore/src/ready.rs`
  `ready_bitmap_coherent` `forall`-over-64-bits pattern for the `[Option<Reg>;64]`
  array invariant.
- **Code refs:** `/Users/mjm/repo/eunomiaos/ipc/src/reactor.rs:185` (`alloc_bit`),
  `:238`/`:243`–`252` (`register_bound` bit-scan), `:266`/`:273` (drain
  `trailing_zeros`), `:59`–`99` (existing `lowest_clear_bit`).
- **TLA/Loom/Shuttle guide:** none for the arithmetic (IpcReactor scope note,
  lines 52–58, states multi-source dispatch is not TLA-modeled). The three
  proptests are the spec oracle and are **kept** as the companion tier; they also
  cover `wait()`'s untouched blocking loop.
- **Dependencies:** none.
- **Phases:**
  1. Verify `alloc_bit` (S): functional `ensures` citing `lowest_clear_bit`.
  2. Verify `register_bound` (M): while-loop invariant + `decreases bits`;
     `used' == old | mask` / unchanged on `Taken`; the 64-bit slot/used coherence
     `forall`.
  3. Extract + verify `drain_one` (S); leave `wait()`'s blocking loop +
     `notif_wait` **outside** `verus!{}` (Loom/Shuttle/TLA-routed). Update the
     ipc Baseline (47 → 47 + new fns) and the ledger routing note that `wait()`'s
     concurrency stays trust-routed.
- **Effort: M · Risk: low.**

---

### Task 2 — Document Admission quota as the verified accounting template

- **Tier 4** (already-verified) · **Feasibility: yes (no code)**
- **Goal:** Record `Admission` (session.rs) as an already-Verus-verified sequential
  core (`well_formed`: granted ≤ budget; `spec_remaining` non-underflowing;
  never-over-grant for all admit/release sequences). It is the template the
  dispatch work (Task 1, Task 6) reuses for `used`-mask accounting. No rewrite.
- **Technique + constructs:** existing `closed spec fn well_formed`,
  `spec_remaining`, `requires`/`ensures`, `final(self)`. Documentation only.
- **Code refs:** `/Users/mjm/repo/eunomiaos/ipc/src/session.rs:339`, `:375`,
  `:404`, `:467`.
- **TLA/Loom/Shuttle guide:** the `fairness_smoke` harness (`ipc/src/model.rs:735`–750)
  quota arm. **Honest correction:** the Verus proof makes the *invariant*
  (granted ≤ budget) redundant with the harness assertion, but the concurrent
  `fairness_smoke` check (exactly `min(budget,N)` grants under N threads) also
  witnesses that the concurrent plumbing calls `admit` atomically under
  interleaving — **keep** that harness arm; only document the invariant overlap.
- **Dependencies:** none. **Effort: S · Risk: low.**

---

### Task 3 — virtqueue `avail_ring_slot` — pure ring index/wrap arithmetic

- **Tier 1** (unverified) · **Feasibility: yes**
- **Goal:** Bring `avail_ring_slot(idx, qsize)` under Verus with `requires
  qsize>0 && qsize<=8` and `ensures result == 4 + (idx % qsize) * 2`,
  `idx % qsize < qsize`, the in-bounds bound, and no `usize`-multiply overflow.
  **Mandatory correction to the candidate spec:** `new()` allocates `6 + 2*n`
  (virtio avail layout: flags/idx/ring[n]/used_event), **not** `4 + 2*qsize`. The
  correct in-bounds `ensures` is `result + 2 <= 6 + 2*qsize` (tightest
  `2 + 2*qsize`). `qsize>0` is a caller precondition (the `u32→u16 .min(8)`
  truncation can yield 0), not provable end-to-end — `new()` is trusted MMIO
  bring-up.
- **Technique + constructs:** plain Verus exec fn `requires`/`ensures`;
  `vstd::arithmetic` modulo lemmas (`lemma_mod_bound`) only if SMT needs them
  (usually `u16 % qsize < qsize` is discharged automatically).
- **Code refs:** `/Users/mjm/repo/eunomiaos/virtio-blk/src/lib.rs:94`, `:182`
  (the `6 + 2*n` allocation), `:297`.
- **TLA/Loom/Shuttle guide:** none. The concurrent device-shared ring stays the
  trusted DMA/hardware seam (rev2§2.5); existing proptests kept.
- **Dependencies:** none, but it is the **virtio-blk crate-onboarding pilot** —
  Phase 1 stands the crate up in the gate (shared prerequisite with Task 7).
- **Phases:**
  1. Gate `virtio-blk`: pin the three versions, `verify=true`/`metadata.verus`,
     confirm the driver (MMIO `unsafe`, fake device, cas adapter) compiles as
     external under `cargo-verus`, add to CI crate list, record a 0-obligation
     Baseline.
  2. Prove `avail_ring_slot`; update the Baseline to the new count.
- **Effort: S · Risk: low.**

---

### Task 4 — storage-server `attenuate()` + rights lattice — monotone delegation

- **Tier 1** (unverified) · **Feasibility: yes**
- **Goal:** Prove `attenuate(p,m) == p & m`; monotone (`attenuate` never grows
  authority: bit set in result ⇒ bit set in `p`); `R_STAT_STORE` (bit 5) stripped
  whenever `mask` bit 5 clear; `attenuate(p, R_ALL)` clears bit 5 for any `p`
  (deny-by-default, `R_ALL = 0b1_1111` omits bit 5).
- **Technique + constructs:** `verus!{}` const/exec fn with `ensures`;
  `by (bit_vector)` for the four u8 identities; `spec fn has_right(bits, R)`.
  Same family as kcore ready-bitmap / ipc `lowest_clear_bit`. No concurrency.
- **Code refs:** `/Users/mjm/repo/eunomiaos/storage-server/src/lib.rs:63`
  (`attenuate`), `:40` (rights bits), `:745`.
- **TLA/Loom/Shuttle guide:** none. Existing host/proptest tests kept.
- **Dependencies:** none, but it is the **storage-server crate-onboarding pilot**
  (shared prerequisite with Task 8). storage-server deps cas (75) + ipc (47),
  re-discharged transitively.
- **Phases:**
  1. Decide gating shape: in-place `verus!{}` island in `storage-server` (mirror
     `cas --no-default-features` to keep serde/BTreeMap out of the verify config)
     **vs.** extract a tiny `no_std` `storage-rights` crate and gate that alone
     (cleaner island, no transitive cas/ipc re-verify drag). Prototype `attenuate`
     + `has_right` + the four lemmas verifying locally.
  2. Wire the gate: Cargo.toml `vstd` + `metadata.verus`, CI line, ledger Baseline
     row (~5 items, 0 added to the 14-seam tally), confirm a cold authoritative
     run; keep existing tests as the companion tier.
- **Effort: S · Risk: low.**

---

### Task 5 — ELF `Segment::page_layout` — total, overflow-safe page geometry

- **Tier 1** (unverified) · **Feasibility: yes**
- **Goal:** Machine-check refuse-not-crash totality (rev2§5.3) for **all** u64
  inputs: `Err` exactly when `vaddr+memsz+(PAGE-1)` overflows u64; on `Ok`,
  `va_start & (PAGE-1)==0`, `va_end & (PAGE-1)==0`, `va_start <= vaddr`,
  `memsz>0 => vaddr < va_end`, `page_offset < PAGE && page_offset == vaddr - va_start`,
  and `pages * PAGE == va_end - va_start`. The producer/consumer hinge: `parse()`
  and `spawn::prepare()` both call it on untrusted images.
- **Technique + constructs:** `verus!{}` exec fn `requires`/`ensures`; a
  `spec fn PageLayout`-invariant predicate; `by (bit_vector)` for the
  mask-alignment / non-underflow facts (extract the recurring `(x & !(PAGE-1))`
  identity into one signature-level lemma, `verus.md` §10); `by (nonlinear_arith)`
  for `pages*PAGE == span`. PAGE is a u64 const (fine for SMT).
- **Code refs:** `/Users/mjm/repo/eunomiaos/loader/src/elf.rs:45`, `:284`;
  `/Users/mjm/repo/eunomiaos/loader/src/spawn.rs:74`.
- **TLA/Loom/Shuttle guide:** none. Unit tests (`page_layout_normal`,
  `page_layout_overflow_boundary_refused`) kept and subsumed.
- **Dependencies:** none, but it is the **loader crate-onboarding** step (shared
  prerequisite with Task 11). Keep the `verus!{}` a small island so loader's
  `std` default does not pull `alloc` into verified code.
- **Phases:**
  1. Gate loader (mirror ipc/cas mixed verified/plain precedent): pin, `vstd`,
     `verify=true`, CI line, Baseline row.
  2. Prove `page_layout` with the bit-vector + nonlinear lemmas. **Sound partial
     fallback** if the mask-to-modular bridge is stubborn: prove everything except
     `pages*PAGE == span` (still strictly better than by-example tests) and keep
     that one clause unit-tested — but attempt the full proof first.
- **Effort: M · Risk: low.**

---

### Task 6 — Reactor dispatch: pure bitmap-coherence invariant (scoped subset of the dispatch SM)

- **Tier 2** (proptest-routed) · **Feasibility: partial**
- **Goal:** Lift the *pure* dispatch arithmetic into verified helpers with a `wf`
  invariant over `used`/`slots`/`pending`: bitmap coherence
  (`slots[bit].is_some() <==> used bit set`), no double-allocation, lowest-clear
  (via the Task-1 `alloc_bit` ensures), the 64-bit ceiling (`Full`), and pending-
  drain lowest-first single-yield ordering. This is the honest, feasible core of
  the proposed dispatch state machine.
- **Technique + constructs:** **plain Verus three-layer pattern, NOT
  `state_machine!`.** The reactor is single-threaded (`&mut self`, holds no
  locks), so tokens are pointless and `state_machine!` would only prove an
  abstract model without certifying exec code. Use a `spec fn wf` + exec
  `requires`/`ensures` + `proof fn` lemmas, exactly as `lowest_clear_bit` and
  `Admission` (Task 2) are built. `axiom_u64_trailing_zeros` + `by (bit_vector)`.
- **Code refs:** `/Users/mjm/repo/eunomiaos/ipc/src/reactor.rs:156`, `:185`,
  `:196`, `:238`, `:266`, `:291`.
- **TLA/Loom/Shuttle guide:** IpcReactor scope note (52–58): dispatch is not
  TLA-modeled. The whole-function `register`/`register_bound`/`wait` contracts and
  the kernel `Transport` seam (`bind`/`notif_signal`/`notif_wait`) stay
  by-construction trusted (`Transport` is outside `verus!{}`; `wait` is an
  unbounded blocking loop). Loom/Shuttle (`model.rs`) keep the concurrent
  wakeup/backpressure; TLA keeps the protocol design. Update the ledger routing
  note recording exactly which dispatch facts moved from proptest-routed to
  Verus-mechanized and which stayed.
- **Dependencies:** Task 1 (reuses its `alloc_bit`/coherence `ensures`).
- **Effort: M · Risk: med.**

---

### Task 7 — virtio-blk `check_capacity` — overflow-safe LBA range refusal

- **Tier 1** (unverified) · **Feasibility: partial**
- **Goal:** Extract the pure arithmetic of `check_capacity(lba, len)` into a free
  `verus!{}` function and prove: totality (no panic/overflow for any `(lba,len)`);
  `Ok` ⇒ `lba + (len/SECTOR) <= capacity` with no wrap; `Err(OutOfRange)` exactly
  when `checked_add` is `None` or `end > capacity`. Keep the generic hardware
  struct (`Mmio`/`DmaBacking` seams, `read_volatile`) **out** of verified scope.
  `capacity` is a struct field read once from trusted MMIO — the property is the
  no-wrap refusal, not the device's honesty.
- **Technique + constructs:** plain Verus exec fn `requires SECTOR>0`/`ensures`;
  `checked_add` modeled via vstd `Option` specs; `spec fn` for the `OutOfRange`
  predicate. **Verdict correction:** the cited `cas::dev::access_range` precedent
  is *not* Verus-verified (it is plain Rust kept honest by Miri+proptest); this is
  net-new verified surface, so effort is M not S.
- **Code refs:** `/Users/mjm/repo/eunomiaos/virtio-blk/src/lib.rs:396`.
- **TLA/Loom/Shuttle guide:** none. Add a companion host-test tier (boundaries
  `0/1/mid/u64::MAX`-near, `_has_teeth` control).
- **Dependencies:** Task 3 (shares the virtio-blk gate).
- **Effort: M · Risk: low.**

---

### Task 8 — storage-server wire decode header/version gate

- **Tier 1** (unverified) · **Feasibility: partial**
- **Goal:** Prove `decode()`'s header+version prefix: `len<3 || buf[..2]!=magic ⇒
  Err(BadHeader)` with body untouched; `len>=3 && magic && !ipc::version_ok(buf[2],
  negotiated) ⇒ Err(Version)` (composing on the **already-verified**
  `ipc::version_ok`, ensures `ok == (h==n)`); header path total (no panic/OOB);
  magic check strictly precedes version. The postcard body decode stays the
  trusted-interpreted seam behind `external_body` (with a host test).
- **Technique + constructs:** `verus!{}` exec fn over `&[u8]` up to the body
  boundary; `Seq<u8>` view of the 3-byte header; `external_body` at
  `postcard::take_from_bytes`; `ensures` referencing the imported
  `ipc::version_ok` (full-pathed, not `use`-imported). Same total-decoder-prefix
  shape as the ipc header codec.
- **Code refs:** `/Users/mjm/repo/eunomiaos/storage-server/src/wire.rs:53`, `:61`;
  `/Users/mjm/repo/eunomiaos/ipc/src/session.rs:452`.
- **TLA/Loom/Shuttle guide:** none. Existing host tests
  (`roundtrip_and_strictness`, `version_is_stamped_and_validated`, wrong-magic-wins
  teeth) kept. The `external_body` postcard boundary adds a ledger row (reason +
  test).
- **Dependencies:** Task 4 (resolves storage-server gating + the
  `#[cfg(feature=serde)]` / feature-set-under-verify decision).
- **Effort: L · Risk: med.**

---

### Task 9 — IpcReactor `FifoPerChannel` + local `NoDrop` as named ensures on the kcore channel ring

- **Tier 3** (TLA-routed) · **Feasibility: partial (labeling, scoped)**
- **Goal:** Add named `spec fn`s and `ensures` making the *already-proven* ring
  FIFO discipline read as the mechanized per-step half of the TLA channel
  invariants: send appends `ring_fifo.push(msg)` / refuses `Full` without dropping;
  recv consumes `ring_fifo.drop_first()` in send order. **Honest scope:** kcore's
  ring holds only the live window (no `nextSend`/`recvd` history), so TLA's
  *global* `NoDrop` counting identity (`nextSend = Len(recvd)+Len(queue)`) stays
  in TLA — only the local per-step refinement is Verus-mechanized.
- **Technique + constructs:** plain Verus. `pub closed spec fn fifo_per_channel` /
  `no_drop_local` keyed per ring via `end_idx_spec(end)` (send) /
  `1-end_idx_spec(end)` (recv) — **must** key the correct ring index or it
  conflates the bidirectional ends. `ensures` on `send`/`recv` discharged by the
  existing `lemma_send_fifo_push`/`lemma_recv_fifo_drop_first`/`lemma_ring_fifo_frame`.
- **Code refs:** `/Users/mjm/repo/eunomiaos/kcore/src/channel.rs:753`, `:836`,
  `:880`; `tla/ipc_reactor/IpcReactor.tla:274`, `:279`.
- **TLA/Loom/Shuttle guide:** IpcReactor `NoDrop`/`FifoPerChannel`. The TLA model
  stays the design oracle for the global/liveness arms; do **not** claim to
  re-route the global counting identity.
- **Dependencies:** none. **Effort: S · Risk: low** (labeling, not new coverage).

---

### Task 10 — CapRevocation `FireSafe` as a binding-slot corollary + report label

- **Tier 3** (TLA-routed) · **Feasibility: partial (corollary + label)**
- **Goal:** Name the rev2§5.1 firing obligation (a non-NULL TCB binding slot
  always names a live cap) explicitly, where it is cheaply entailed. **Verdict
  finding:** the property is already entailed by the verified `caps_consistent`
  invariant (a `Notification(o)` cap in a bind slot requires `notif_wf` ⇒
  `nv.dom().contains(o)` = live), which `revoke_step`/`bind`/`destroy_tcb` already
  maintain; `report_terminal` already discharges the live-firing locally. So this
  is labeling/documentation value, not new safety coverage.
- **Technique + constructs:** `pub open spec fn fire_safe(store)`; a one-line
  `proof fn lemma_fire_safe_from_caps_consistent` (`requires caps_consistent`
  `ensures fire_safe`); add `fire_safe` as a named `ensures` on `report_terminal`.
- **Code refs:** `/Users/mjm/repo/eunomiaos/kcore/src/cspace.rs:163`,
  `/Users/mjm/repo/eunomiaos/kcore/src/thread.rs` (report_terminal),
  `/Users/mjm/repo/eunomiaos/kcore/src/notification.rs:71`;
  `tla/cap_revocation/CapRevocation.tla:388`.
- **TLA/Loom/Shuttle guide:** CapRevocation `FireSafe` (implied by `DeadNowhere`).
  Cross-restart interleaving stays TLA-owned.
- **Phases:** (1) `fire_safe` spec fn + corollary lemma; (2) `ensures` on
  `report_terminal`; (3) **measure-then-decide** whether to surface `fire_safe` on
  `revoke_step`/`destroy_tcb` — a whole-store frame predicate on heavy *consuming*
  callers is the documented establish-vs-consume backfire (`verus.md` §10) and can
  ~double the obligation; keep it on those callers only if the cold `rlimit` total
  does not regress, else keep just the lemma + report label.
- **Dependencies:** `caprevoke-liveparent-ensures-guide` (the named LiveParent
  guide; pursue it or confirm it before Phase 1). **Effort: S · Risk: low.**

---

### Task 11 — ELF `parse()` + le readers — total bounded decoder over arbitrary bytes

- **Tier 1** (unverified) · **Feasibility: yes (verdict: yes, med risk)**
- **Goal:** Total decoder: `parse()` never panics / never reads OOB for any
  `&[u8]`; `Err` on truncation/bad-magic/too-many-segments; on `Ok` every segment
  satisfies `offset+filesz <= bytes.len()`, `nsegments in 1..=MAX_SEGMENTS`, and
  `page_layout().is_ok()` (composes with Task 5); le readers' `Ok` equals the
  little-endian reassembly. Direct twin of cas's verified `decode_node`.
- **Technique + constructs:** `verus!{}` exec fns; `Seq<u8>` view + `subrange`
  bounds reasoning; `spec fn well_formed_image()`; `decreases` on the phnum loop;
  port `u16/u32/u64le` from `from_le_bytes` (alloc-only) to **mask/shift readers
  copied verbatim from cas** (`prolly.rs:772`–830) with their `lemma_u*_le_bytes`
  `by (bit_vector)`; bound the whole phentsize-strided entry up front
  (`ph_end > bytes.len()`, elf.rs:155–158) so each field read is in-bounds.
- **Code refs:** `/Users/mjm/repo/eunomiaos/loader/src/elf.rs:90`, `:111`, `:147`.
- **TLA/Loom/Shuttle guide:** none. Add/confirm the loader fuzz target with
  committed corpus + Miri replay (truncation/bad-magic/too-many-segments/overflow).
- **Dependencies:** Task 5 (shares the loader gate; `page_layout().is_ok()`
  composes on top).
- **Phases:** (2) port + prove the le readers; (3) bring `parse()` under
  `verus!{}` (well-formed spec, `decreases`, loop invariant bounding `n` +
  segment in-bounds, the four `Ok`-clauses); (4) companion fuzz/Miri tier.
  (Phase 1 = the loader gate, done in Task 5.)
- **Effort: L · Risk: med.**

---

### Task 12 — CommitProtocol `AtLeastOneValidSlot` + `GenerationsDistinct` labels on `pick_survivor`/`commit_target`

- **Tier 3** (TLA-routed) · **Feasibility: yes (near-zero-cost label)**
- **Goal:** **Both ensures already exist and verify** (`pick_survivor` store.rs:470:
  `(valid_a && valid_b) ==> ((r is SlotA) <==> gen_a >= gen_b)`; `commit_target`
  store.rs:512: `r != live_slot(sb_in_b)`). The work is a one-line comment on each
  naming the TLA invariant it mechanizes (GenerationsDistinct-determinism;
  AtLeastOneValidSlot-by-construction), keeping the framing that the *global*
  AtLeastOneValidSlot invariant remains TLA-owned and these are local per-call
  witnesses.
- **Technique + constructs:** none new — existing ensures + `live_spec fn`. Pure
  total functions.
- **Code refs:** `/Users/mjm/repo/eunomiaos/cas/src/store.rs:465`, `:497`, `:509`.
- **TLA/Loom/Shuttle guide:** CommitProtocol `AtLeastOneValidSlot` (244),
  `GenerationsDistinct` (247). Confirm `cargo clean -p cas && cargo verus verify
  -p cas --no-default-features` still reads `75 verified, 0 errors` (flat).
- **Dependencies:** none. **Effort: S · Risk: low.** Bundle into a larger
  conformance-doc change, not a standalone PR.

---

### Task 13 — CommitProtocol replay-equality cross-link on `recover_records` (WAL-projection only)

- **Tier 3** (TLA-routed) · **Feasibility: partial**
- **Goal:** Add a cas `spec fn` that is the WAL-byte/queue projection of
  `RecoverReconstructs` (the rebuilt run equals the seq-continuous content-valid
  past-head records, which `run_len`/`laid_out` already characterize) and a thin
  `proof fn` deriving it from `recover_records`'s existing ensures. **Honest
  bound:** do **not** attempt a verbatim `AckedWritesRecoverable`/
  `RecoverReconstructs` over `writeCtr`/`walLog` — that quantifies over global
  acked-write state the verified core does not model and rests on the trusted
  Store-lifetime join + content-coverage axiom (rev2§6.1(e)); it stays TLA +
  by-construction.
- **Technique + constructs:** plain `spec fn`/`proof fn` over `Seq`; reuse
  `run_len`, `laid_out`, `lemma_gap_freedom`, `lemma_run_len_covers`. **Anti-theatre
  requirement:** the new `spec fn` must carry a teeth check (a deliberately-wrong
  off-by-one head bound must *fail* to verify) so it is not a green-proof-of-nothing
  quantifying only over its own producer.
- **Code refs:** `/Users/mjm/repo/eunomiaos/cas/src/store.rs:1147`, `:1210`,
  `:1252`, `:559`.
- **TLA/Loom/Shuttle guide:** CommitProtocol `AckedWritesRecoverable` (261),
  `RecoverReconstructs` (281), `RecoverNoop` control (315). Document that Verus
  proves replay-equality on the byte/queue view; the lift to global recoverability
  stays TLA + by-construction. Keep cas Baseline ≥ 75; re-derive `rlimit`.
- **Dependencies:** none (the candidate's stated dep is the dropped verbatim
  attempt). **Effort: M · Risk: med.**

---

## 4. Deferred / keep-as-is

Tasks judged infeasible under the pin or net-negative; each with the reason and
the fallback. Do **not** schedule these as Verus work.

### Infeasible under the pin (memory-model / construct gaps)

- **`urt-seqlock-tsm` — tokenized SM + `atomic_with_ghost` for the time-page
  seqlock.** *Reason:* `vstd` atomics are SeqCst-only with no standalone fence;
  the ship seqlock is Relaxed data + Acquire/Release fence. A SeqCst Verus proof
  either certifies a *different* binary than ships (false certification under
  erasure) or forces a codegen change to the hot clock-read path; and under SeqCst
  the seqlock is trivially untearable, so the proof would prove the trivial case
  while the actual fence-reordering question is inexpressible. `atomic_with_ghost`
  also opens an invariant for one op only and cannot model the seqlock's
  retroactive cross-read validation. *Fallback:* **keep the Loom proof of record**
  (the only tool modeling the fence edge) and the Shuttle smoke; the legitimate
  Verus surface here (`Sample::utc_ns_at` tick→ns conversion) is *already* verified.

- **`urt-seqlock-writer-transition` — verify a seqlock writer critical section.**
  *Reason:* same SeqCst-only gap, **plus** no production writer exists (`seq` is
  write-once; every writer is `#[cfg(test)]`/loom/shuttle). It proposes verifying
  speculative code that does not exist, and even built it would not subsume the
  Loom harness it claims to retire. *Fallback:* keep loom/shuttle; revisit only if
  rev2§8 deferred clock-setting actually adds a production writer, and even then
  the SeqCst gap blocks Loom removal.

- **`urt-seqlock-retire-harnesses` — delete the loom/shuttle seqlock harnesses.**
  *Reason:* depends on the two infeasible tasks above; a SeqCst proof cannot
  subsume the fence-mediated Loom certification or the unbounded retry-loop
  liveness. *Fallback (partial, allowed):* retire **only** the explicitly
  non-certifying Shuttle harness (and its cfg dep) once a deductive SeqCst-shaped
  artifact exists; **keep Loom**, the native probabilistic tearing test, and the
  Miri pass as mandatory belt-and-suspenders.

- **`urt-heap-arena-pointsto` — narrow the `UnsafeCell<[u8;N]>` arena with
  `PointsToRaw`.** *Reason:* no `vstd` path mints a tracked `PointsToRaw` for a
  pre-existing `static` (constructors are `empty()`/`allocate()`(std)/`into_raw`;
  `expose_provenance` yields only a provenance token). The root permission would
  need a hand-written `external_body` fabrication axiom — a *new* trusted seam,
  Miri-invisible — that is net-worse than today's guard; and `GlobalAlloc`
  signatures cannot carry `Tracked<...>`, so the permission can never reach the
  client. The disjointness/no-double-free properties are already proven by the
  verified `freelist` dependency (off+need ≤ N, alignment). *Fallback:* keep the
  arena in the Miri+proptest tier (status quo, ledger lines 60–63); if hardening
  is wanted, add a `_has_teeth` control to the disjointness proptest.

- **`ipc-reactor-protocol-tsm` — abstract `tokenized_state_machine!` twin of
  IpcReactor.tla.** *Reason:* the macro crate is off-crates.io (fragile CI build
  graph), the unbounded-vs-bounded win is largely theoretical (TLC already
  exhausts the meaningful state space), it duplicates rather than retires the TLA
  safety arm, and it loses the TLA negative controls' CI-runnable teeth.
  *Fallback:* keep IpcReactor.tla as-is. If ever pursued, it must be a non-tokenized
  `state_machine!`, solve the macro dep first, and explicitly retire the TLA safety
  invariants (keeping `EventuallyDelivered`) with guard-stripped failing-`#[inductive]`
  negative controls — a high bar.

### Already satisfied (no work) or net-negative (do not pursue)

- **`reactor-dispatch-tsm` (as a `state_machine!`)** — superseded by Task 6, which
  takes the feasible plain-Verus subset. The SM framing is wrong for a
  single-threaded reactor and would entangle the `Transport` seam.

- **`caprevoke-leaf-first-step-proof`** — **already done**: `revoke_step`
  (cspace.rs:12346) already `ensures cspace_wf` (= LiveParent), descends to a
  childless leaf, and calls `lemma_set_revoking_frames`/`lemma_childless_no_descendant`.
  The proposed `requires (victim childless)` would *weaken* the contract. At most
  a documentary cross-reference comment.

- **`caprevoke-eventuallyrevoked-tla-only`** — **already satisfied**:
  `EventuallyRevoked` is liveness (TLA-only); the write-once `ReportMonotone` slice
  already verifies on `report_terminal`; the routing is already in the ledger
  (line 193). Close as already-recorded; do not edit `report_terminal`'s contract.

- **`commit-committedrootsdurable-barrier`** — **already satisfied**: the
  mechanizable per-step slice IS `advance_head`'s existing everything-popped-flushed
  ensures (store.rs:563), already wired into the commit prepare path. The whole
  invariant quantifies over `durableRoots`/`refRoots`/slot validity not modeled in
  the verified core (the trusted Store seam + `FsyncMeansFsync`). Keep in TLA +
  by-construction.

- **`commit-crash-recover-interleaving-tla-only`** — a scoping/ledger confirmation
  (crash three-outcome + cross-restart Recover stay TLA; `FsyncMeansFsync` stays an
  axiom row). Execute as a one-line routing-note audit alongside Task 12; no proof.

- **`caprevoke-movesemantics-single-owner`** — the faithful system-wide
  single-owner cardinality invariant requires inventing a cap-identity notion plus
  a new global handle-injectivity `wf` clause across all containers (kcore is
  object-census *many*-to-one by design), an XL/high-risk global-invariant rewrite
  with known `rlimit` hazard. *Honest feasible subset (optional, separate L
  task):* a per-op "move conservation" `ensures` on send/recv/bind (source emptied,
  destination filled, no non-empty cap duplicated) with a `_has_teeth` DupOwner
  control — keep global MoveSemantics in TLA.

- **`caprevoke-revokeddead-ghost`** — Rust has no persistent `revoked` set
  (dead ↔ `is_empty_cap`, the complement of live on one field), so the property is
  true by construction; a faithful TLA mirror would add net-new persistent ghost
  state + an inductive proof across three mutators (M/med) to model a hazard the
  representation cannot exhibit. *Fallback:* strengthen the emptied-slot `ensures`,
  document RevokedDead as discharged-by-construction, keep the ReviveRevoked teeth
  in TLA.

- **`ipc-reactor-concurrency-loom-tla-only`** — a confirm-and-close scoping item:
  `EventuallyDelivered` stays TLA, concurrent wakeup/backpressure stays Shuttle,
  multi-source dispatch stays proptest, `lowest_clear_bit` stays the sole Verus
  core. No rewrite; the split is already documented.

- **`mkfs-name-acceptable`** — onboarding `mkfs` into the gate to verify a 3-line
  byte-class predicate *grows* trusted surface (an `external_body` row for the
  OsStr→str step) for negligible assurance over the existing proptests.
  *Fallback:* keep as plain Rust with a strengthened proptest oracle + `_has_teeth`
  control; if ever wanted, verify a `no_std` `&[u8]` predicate *inside* the
  already-gated cas verus block, not via a new mkfs gate.

- **`startup-block-codec-bijection`** — the borrowed-slice arenas (`[&'a [u8]; N]`)
  + prefix-only `PartialEq` round-trip is unprecedented in-repo and the project
  already keeps this exact looping/slice-borrowing decoder shape (cas
  `RefTable::decode`) outside `verus!{}` deliberately. *Fallback:* if any Verus is
  wanted, verify only the pure copying core (integer reassembly + a copying
  `decode` totality with bounded counts), reusing `ipc/src/le_bytes.rs`; leave the
  borrow-relating round-trip in the proptest/Miri/fuzz tier.

- **The `*-NONO` state-machine rewrites** (`channel-ring-fifo-sm`,
  `waiter-queue-timer-ready-list-sm`, `freelist-allocator-sm`,
  `cas-recover-records-replay-sm`, `cas-decode-partition-pure-fns`,
  `aspace-walker-tree-sm`, `untyped-retype-sm`) and
  `revoke-step-quantum-sm-model` — all **net-negative**. These are already-verified
  single-threaded cores whose abstract model IS the existing spec; wrapping them in
  `state_machine!{}` (a non-tokenized model with no second abstraction to refine to,
  no ownership story, no concurrency) adds an indirection layer + refinement glue +
  re-baselined `rlimit` for zero new property, and several (recursive page-table
  tree, existential leaf-set, shared-`qnext` cross-list disjointness, u32 bitmap
  coherence) have no faithful flat-SM encoding. *Fallback:* keep them exactly as
  they are. The `revoke_step` SM model is the only one with conceivable value, and
  only if someone writes the exec-refines-model glue the candidate disclaims; the
  TLA CapRevocation model + the existing loop invariant already discharge that duty.
