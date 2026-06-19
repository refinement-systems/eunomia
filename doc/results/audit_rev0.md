# Eunomia OS — Conformance & Verification Audit (rev0)

Audit of the implementation against `doc/spec/spec_rev0.md` (the blessed
revision 0). All section references below use the mandated `rev0§X.Y` form.

---

## 0. Method and headline assessment

**Method.** The codebase was partitioned into 28 units (20 component readers
covering every crate against its owning spec sections, plus 8 verification-theater
specialists covering the Verus escape hatches, the three TLA+ specs, the
concurrency-tool fit, and the test suites). Each unit produced structured findings
across the five requested categories. Every falsifiable high/medium claim was then
handed to an independent **adversarial verifier** instructed to *refute* it by
re-reading the cited code and spec. Where tools were available the specialists ran
them: `cargo verus verify` per crate and TLC on the commit/reactor models (no JRE
was available for the larger `CapRevocation` run). The author independently
spot-checked every Verus escape hatch, the three TLA+ specs, the verus guideline,
and the seqlock orderings.

Of 22 adversarially-checked claims, **15 were confirmed and 7 refuted**; the
refuted ones are listed in rev0§2.6 below so the reader can see the adversarial
pass did real work.

**Headline.** The system is a faithful, honestly-verified MVP of the spec, and the
verification is *not* theater at the seam level. The kernel object core verifies
clean (`cargo verus verify -p kcore` → 335 verified, 0 errors) and contains exactly
four `#[verifier::external_body]` attributes, all of them legitimate trusted-base
boundaries (opaque `size_of`, interpreted BLAKE3, a debug-only `panic!` guard) each
paired with a host test, exactly as the verus guideline's discipline demands. The
TLA+ models are non-vacuous and carry deliberate *negative controls*. The
crash-recovery test harness models torn writes at byte granularity. This is a
codebase whose verification claims are, with the specific exceptions catalogued
below, backed by real proofs.

The genuine deficiencies cluster in five places:

1. **One real authority bug** — the `stat-store` right does not exist, so `statfs`
   is readable by any handle (rev0§3, high).
2. **A storage feature the spec says is "implemented now" is absent** — guarded
   ref-table batches / per-ref edit-version (rev0§3, high; rev0§5 records the spec's
   own false claim).
3. **The rev0§4.4 flush/memtable policy and the rev0§4.6 GC concurrency machinery
   are heavily simplified** — much of it acknowledged in-code as MVP debt, but some
   not on any disclosed-simplification list.
4. **One genuine piece of TLA+ theater** — the commit model's headline invariant
   checks that recovery's *ingredients* are durable, not that recovery
   *reconstructs* the right state; a no-op `Recover` passes all invariants
   (empirically confirmed by re-running TLC). The spec's replay-**equality**
   property is therefore mechanized nowhere (rev0§2, high).
5. **Pervasive documentation drift** — 0 of 1165 `§` references use the mandated
   `rev0§` form, `doc/plans/` and `doc/results/` are empty yet referenced by 40
   files, and comments carry heavy historical baggage. These were addressed in this
   pass (rev0§1).

---

## 1. Cosmetic issues (addressed in this pass)

### 1.1 Spec-reference form — the mandated `rev0§` rule is violated everywhere

`CLAUDE.md` requires *"All spec references must contain the revision number, like
`rev0§6` or `rev0§3.1`."* Across the whole tree there are **1165 `§` references and
zero in the `rev0§` form** — not in any `.rs`, `.tla`, `.md`, or `Cargo.toml`. The
worst concentrations: `kcore/src/cspace.rs` (367), `kcore/src/test_store.rs` (99),
`kcore/src/channel.rs` (84), `kcore/src/thread.rs` (66).

These were converted to `rev0§` for genuine spec references (see rev0§7, "Actions
taken"). Note that a substantial fraction of bare `§` references in
`ipc/`, `kcore/timer.rs`, and `cas/` denote **plan-document** sections
(`doc/plans/2_ipc.md §4.2`, `plan §6d`, `plan §7f`) interleaved with genuine spec
references; those are dev-doc baggage (rev0§1.3), not spec references, and must not
be mechanically rev0-prefixed.

### 1.2 Dangling development-document references

`doc/plans/` and `doc/results/` **exist but are empty directories**, yet:

- **24 files** reference `doc/plans/*` (e.g. `doc/plans/3_verus-rewrite.md`,
  `doc/plans/2_ipc.md`, `doc/plans/3_verus-rewrite_phase7-detail.md`).
- **16 files** reference `doc/results/*` (e.g. `doc/results/68_verus-findings.md`,
  `doc/results/1_fuzzing-findings.md`, `doc/results/35`).
- `doc/guidelines/verus.md` references the entire `doc/results/21…67_verus-findings.md`
  series **and** the trusted-base ledger `doc/results/68_verus-findings.md`, which it
  calls *"the source of truth for CLAUDE.md's 'the trusted base is exactly …' claim"*
  — a claim that is itself **not present** in the current `CLAUDE.md`.

Every one of these references is dangling. The trusted-base ledger being absent
means the enumerated trusted base cannot be cross-checked against its source of
record; this audit reconstructs it from the code instead (rev0§4.1). This audit
document is, accordingly, the first artifact actually written to `doc/results/`.

### 1.3 Historical baggage in comments

Comments pervasively narrate past development rather than describing the current
state and why. Categories observed (with representative locations):

- **Stale claims contradicting the current code.** The most serious: a large set of
  comments still describe `delete`, `destroy_tcb`, `destroy_channel`, `signal`,
  `cdt_unlink`, `slot_move` as `external_body`/assumed, when **all six now have
  fully proven Verus bodies** (the only `external_body` attributes left in `kcore`
  are the four in `untyped.rs`). Examples: `kcore/src/cspace.rs:8781,8847`
  ("`delete` is **opaque** here (`external_body`)"); `kcore/src/thread.rs:390-399,445`
  (`destroy_tcb` header "carries an `external_body` contract … not a Verus body
  proof"); ~18 sites in `kcore/src/test_store.rs`. A reader trusting these comments
  would mis-map the trusted boundary.
- **Wrong spec-section citations.** `kcore/src/sysabi.rs:103,117,134` attribute
  syscall-decode validation to "rev0§4.6" (which is Garbage Collection); the
  relevant sections are rev0§3.7/rev0§3.1. `kernel/src/user.rs:1` cites "rev0§10"
  (the spec ends at rev0§8). `ipc/src/session.rs:49` cites a nonexistent "rev0§9".
- **Stale tool labels.** `ipc/src/wire.rs:3` calls the header "Kani-verified"; it is
  now Verus-verified. `kernel/src/aspace.rs:5` claims "Kani verifies it"; the
  verified target is the array-backed `kcore::aspace` walker. Kani was retired
  (rev0§8.4) but is cited as live justification in escape-hatch comments
  (`cas/src/disk.rs:333`, `kcore/src/untyped.rs:231,272,296`).
- **Plan-phase / dev-doc narration.** "phase 6d", "plan §8b", "the 7f/7g split",
  "F-70-9", "D-A1/D-A3", "(the 799k-state proof)", "is gone", "no longer", "used
  to", "previously", "the old walker", milestone tags ("(M5)", "becomes the on-OS
  server at M3"). Heaviest in `kcore/src/cspace.rs` (~329 lines),
  `kcore/src/test_store.rs`, `cas/src/store.rs`, `kcore/src/aspace.rs`,
  `kcore/src/timer.rs`.

The substantive history embedded in this baggage — which is worth keeping — is
preserved in rev0§6 below before being removed from the comments.

---

## 2. Incorrect implementation or verification

Reported only; no ad-hoc corrections were made to logic. Status reflects the
adversarial verification pass.

### 2.1 Confirmed implementation defects

**(I-1) `stat-store` right does not exist; `statfs` is ungated — deny-by-default
violated. [high, confirmed]** rev0§2.3 mandates `stat-store` as a distinct ref
right gating store-global observation, with an explicit deny-by-default posture
(*"`statfs` without it is refused"*, *"delegation helpers strip it"*, *"The default
posture is deny"*). The rights set
(`storage-server/src/lib.rs:38-45`, `R_ALL=0b1_1111`) defines
read/write/snapshot/rewrite-history/enumerate but **no stat-store bit**, and the
`Statfs` handler (`storage-server/src/lib.rs:617-621`) gates only on
`lookup(session, handle, 0)` — any live handle, even one with zero rights, reads
whole-store space accounting. This is a real confinement leak: the spec calls
free-space accounting a covert channel (rev0§2.3, "Limits of confinement") and
`stat-store` is precisely the right that is supposed to gate it.

**(I-2) Guarded ref-table batches / per-ref edit-version are entirely absent.
[high, confirmed]** rev0§4.7 mandates an **edit version** per ref (distinct from the
rev0§2.2 revocation generation) that advances on every committed ref-entry mutation,
is returned by enumerate ops, and gates an all-or-nothing
`apply(handle, expected_version, edits)` batch — the documented remedy for the
retention read-then-act race. None of it exists: `cas/src/disk.rs` `RefEntry`
carries only `{root, generation, next_snap_id}`, `SnapInfo`/`ListSnapshots` return
no version, and the storage-server `Request` enum has no guarded-batch variant. The
retention race the spec calls out is therefore unguarded. (rev0§8.3 claims this is
"implemented now" — see rev0§5, S-11, for the spec error.)

**(I-3) Dedup-resurrection fix is not implemented; correctness rests on synchronous
GC. [high, confirmed]** rev0§4.6 step 3 mandates the resurrection fix as an
always-present mechanism: *"during sweep, a dedup lookup that hits an unmarked chunk
is treated as a miss … This confines all GC/mutator interaction to one point."*
`ChunkStore::put` (`cas/src/store.rs:271-298`) does a plain `index.contains_key`
with no mark/condemned-set consultation. This is **benign today** because GC is
fully synchronous (`Store::gc`, `cas/src/store.rs:1641-1680`) so no chunk can be
born between mark and sweep — but the spec's named mechanism is absent, the
birth-generation "live by fiat" filter at `cas/src/store.rs:1662` is consequently
vacuous (the code's own comment at `:1647-1648` admits it), and this simplification
is **not on the recorded MVP-simplification list** at `cas/src/store.rs:20-32`.
(The broader "GC must be concurrent" reading was *refuted* — see rev0§2.6 — because
rev0§2.1 and rev0§8.3 explicitly defer concurrency. The defect is the missing
*mechanism*, not the missing concurrency.)

**(I-4) virtio-blk used-ring completion poll uses a non-volatile read. [medium,
confirmed]** `complete()` (`virtio-blk/src/lib.rs:256-264`) polls the device-DMA'd
used ring through `DmaPool::read → bytes()` (`dma-pool/src/lib.rs:1287-1306`), a
plain non-volatile load, with only `core::hint::spin_loop()` (not a barrier) in the
loop. The load is loop-invariant to the compiler and may legally be hoisted out of
the spin, so the loop can never observe the device's update. This is a genuine
correctness hazard on the spec's QEMU target, *not* covered by the rev0§8.1/rev0§2.5
cache-maintenance debt (that debt is about cache coherence, not compiler
reordering). A `read_volatile`/atomic-acquire load is required.

**(I-5) ELF segment VA page-rounding can overflow/underflow on adversarial images.
[medium, confirmed]** `loader/src/spawn.rs:57-59` rounds an untrusted segment VA
with unchecked arithmetic (`va_end = (vaddr + memsz + 4095) & !4095`, then
`pages = (va_end - va_start)/PAGE`), while `loader/src/elf.rs:124` only rejects
`vaddr + memsz` overflowing `u64` (it permits `== u64::MAX`). A crafted image with
`vaddr + memsz` within 4095 of `u64::MAX` overflows the `+4095`: a debug-build abort
(overflow-checks on, `panic=abort`) or a release-build wrap underflowing the page
count. `elf::parse` is a cargo-fuzz target and is total, but its consumer
`prepare()` (aarch64-only, unfuzzed) is where the unchecked math lives.

### 2.2 Confirmed verification defects (theater)

**(T-1) The commit model's headline invariant does not check that recovery
reconstructs state; a no-op `Recover` passes. [high, confirmed — empirically]**
rev0§6 names the centerpiece of the TLA+ tier: *"after any crash, the recovered
state equals the committed roots plus a replay of all WAL records not covered by the
committed head."* The model's `AckedWritesRecoverable`
(`tla/commit_protocol/CommitProtocol.tla:250-254`) asserts only that, for every
acked version, *either* it is covered by the live root *or* a WAL record survives
past the head — a **durability-of-ingredients** property of the on-disk substrate.
It never references the `overlay` variable that `Recover` (`:192-202`) reconstructs,
and no other invariant constrains `overlay` beyond a `TypeOK` subset bound. The
specialist confirmed this empirically: **replacing `Recover`'s WAL-replay with an
empty-overlay no-op still passes all five invariants (5358 states)**. So the spec's
replay-**equality** property — which rev0§6.1(e) explicitly says "remains the TLA+
model's alone" — is in fact mechanized **nowhere**: the Verus surface over the real
WAL code proves only structural/maximal-run framing (content uninterpreted via
`wal_content_ok`), and the TLA model proves only ingredient-durability, not
reconstruction. This is the single most important verification gap in the storage
stack, and unlike the seam-level gaps it is *not* disclosed. (Reconstruction
correctness is expressible only as a step/action property relating `overlay'` to
`(refRoots, walLog)` across `Recover`, which the model omits.)

**(T-2) `lemma_gap_freedom` — the code-level shadow of the recovery invariant — is a
dead proof on an undischarged hypothesis. [medium, confirmed]** `cas/src/store.rs:843-879`
proves that every unflushed WAL record lies in the replayed span, presented as
"the code-level shadow of the TLA+ `AckedWritesRecoverable`." But it is a `proof fn`
with **zero call sites**, and its load-bearing hypothesis `laid_out(wal, records, 0)`
(`:764-782`) is, per the authors' own comment at `:749-754`, *"a documented
invariant, not enforced at one site Verus sees."* `mount()` builds `records` by
replaying and `commit()` consumes them, but nothing discharges `laid_out`. So the
lemma proves a true statement that is never connected to the running code — the
in-code AckedWritesRecoverable guarantee actually rests on the crash-injection
proptest plus TLA+, not on this lemma.

**(T-3) The IPC-reactor TLA spec models only the wait-side half of the rev0§3.6
discipline it claims to cover. [medium, confirmed]** `tla/ipc_reactor/IpcReactor.tla`
headers (`:8-15`) claim it models the full *"bind, poll once, then wait"* discipline
and "the genuinely-new wakeup protocol." It has no bind/register/poll-once-self-signal
action — `Send` (`:72-82`) treats the on-readable binding as always present — so the
**send-before-bind hazard and its poll-once mitigation are modeled only in the Loom
fragment**, not in TLA+. Separately, the title "lost-wakeup + **backpressure**
protocol" is titular: the only backpressure modeled is structural (`Send` disabled
at `Len(queue) >= QueueDepth`); there is no `FULL` return, no writable-signal, and
no symmetric writable lost-wakeup (`tla/ipc_reactor/IpcReactor.tla:2,15,72-74`). The
header over-claims its own coverage. (The wait-side lost-wakeup that *is* modeled is
modeled well, with a real negative control — see rev0§6.)

**(T-4) `fsync = fsync` is not stated as an explicit axiom in the commit model.
[medium, confirmed]** rev0§4.8 mandates that the single trusted storage axiom be
*"stated explicitly as an axiom in the TLA+ model."* `CommitProtocol.tla` contains
no `AXIOM`/`ASSUME`; the assumption is encoded only operationally (via the
`durableRoots`/`Crash` semantics) and mentioned in prose. The one trusted-base
assumption of the storage layer is therefore not surfaced as a labeled axiom.

**(T-5) `wal_content_ok` / `checksum_ok` fold verifiable decode logic into the
BLAKE3 trusted seam. [low, noted]** `cas/src/store.rs:570-578` is `external_body`
covering *both* the BLAKE3 payload checksum (legitimately interpreted, out of scope
per rev0§6.1(e)) *and* `WalOp::decode_record` → `decode_payload`
(`cas/src/disk.rs:531-566`), a pure bounded length-prefixed structural decode that
Verus could verify exactly as `decode_frame`/`decode_checked_fields` are. The same
shape recurs at `cas/src/disk.rs:338` (`checksum_ok`). These are spec-sanctioned
seams and the bundling is honest in intent, but a slightly tighter seam (split the
content decode out, leave only BLAKE3 trusted) would shrink the trusted surface.

### 2.3 Wrong spec-section citations (documentation correctness)

Several comments cite the wrong spec section as the *normative basis* for code
behavior (distinct from the dev-doc baggage of rev0§1.3): `kcore/src/sysabi.rs:103,117,134`
cite rev0§4.6 (GC) for syscall-decode/message-length validation (should be
rev0§3.7/rev0§3.1); `kernel/src/user.rs:1` cites rev0§10 (nonexistent);
`ipc/src/session.rs:49` cites rev0§9 (nonexistent). These mislead a reader trying to
trace code to its requirement.

### 2.4 (no further confirmed incorrect-implementation findings)

The remaining `incorrect_impl`-tagged observations either resolved to documentation
issues (rev0§2.3) or were refuted (rev0§2.6).

### 2.5 (reserved)

### 2.6 Claims examined and refuted by adversarial verification

These were raised by a finder and then *refuted* on re-reading; recorded so they are
not mistaken for open issues:

- **Reactor caps at 64 sources → spec divergence.** *Refuted.* rev0§3.6 frames
  bit-groups as the mechanism *beyond* the 64-source word-width limit and routes the
  `>64` lift to the rev0§8 kernel event object ("changes no server code"). The
  one-bit-per-source reactor with an epoll-shaped API is conformant; the scaling
  mechanism is deferred, not missing. (The comments *claiming* "implemented over bit
  groups" are nonetheless inaccurate — counted under rev0§1.3 baggage.)
- **Prolly split keys on the full entry, not the key.** *Refuted.* rev0§4.1 "a
  function of the hash at the boundary key" identifies *which* item governs the
  split, not key-only hashing; rev0§4.9 line 330 states verbatim that metadata
  participates in hashing. Hashing the full encoded entry is correct.
- **GC is synchronous → violates "periodic and concurrent".** *Refuted.* rev0§2.1
  ("until GC is incremental") and rev0§8.3 (defers persisted/incremental GC) make a
  blocking GC the accepted present state. (The missing *dedup-resurrection
  mechanism* is a separate, real finding — I-3.)
- **stdin/stdout split and the named-grant table are absent.** *Refuted as a
  divergence.* rev0§8.3 explicitly defers "a real named-grant-table format". The
  absence is real (see rev0§3, M-9) but disclosed-deferred, not a violation.
- **Shell does not delegate a storage session to children.** *Refuted.* rev0§5.1
  lists `storage` as granted *"when granted"* (conditional), and rev0§4 namespaces
  are whatever handles a process receives — no mandate to delegate.
- **TLA `Revoke` atomicity is theater.** *Refuted.* The kernel is single-core and
  non-preemptible with IRQs masked at EL1 (`kcore/src/lib.rs:20-21`), so the walk
  genuinely runs atomically with respect to all other kernel operations; modeling
  `Revoke` as one step is faithful. (This very fact *confirms* a different
  finding — M-1, that the implementation is non-preemptible while rev0§2.2 requires a
  preemptible walk.)
- **TLA `FireSafe` constrains nothing.** *Refuted.* `FireSafe` is exactly what
  catches a `Revoke` that purges cspaces/queues but fails to null TCB binding slots;
  deleting `tla/cap_revocation/CapRevocation.tla:210-212` makes it fail.

---

## 3. Incomplete implementation (in the spec, not in the code)

Distinguishing **(a)** features the spec itself defers in rev0§8.3 (disclosed debt)
from **(b)** mandated mechanisms simply absent.

### 3.1 Mandated mechanisms absent or materially simplified

**(M-1) Revoke is not preemptible/restartable. [medium, confirmed]** rev0§2.2 states
the unbounded descendant-deletion walk *"is preemptible and restartable."*
`kcore::cspace::revoke` (`kcore/src/cspace.rs:10070`) is a straight-line
run-to-completion `while` loop with no preemption point or restart entry, run with
IRQs masked / non-preemptibly (`kcore/src/cspace.rs:13`,
`kernel/src/exceptions.rs:7`). A large subtree therefore monopolizes the CPU with
interrupts masked, defeating the preemptive scheduler (rev0§5.4) and making revoke a
source of unbounded interrupt latency. The Verus proof establishes *termination*,
which is not *preemptibility*.

**(M-2) Aspace pool top-up is not implemented. [medium, confirmed]** rev0§2.5's
pool-at-creation aspace *"accepts top-ups,"* but `AspaceObj.pool_pages` is set once
at retype (`kernel/src/aspace.rs:62-69`) and never grown, and the syscall table has
no top-up variant — an exhausted pool returns `NEED_MEMORY` permanently. The
three-part error story (NEED_MEMORY / top-up / return-at-teardown) is implemented as
two parts.

**(M-3 … M-6) The rev0§4.4 flush/memtable policy is largely collapsed to a single
global budget.** rev0§4.4 calls its mechanisms *fixed* (only the numbers tunable),
yet:
- **No per-ref soft bound** — `StoreOptions` exposes only one global
  `overlay_budget`; one ref can consume the whole budget, defeating the per-ref
  containment the spec attributes to per-ref quotas (`cas/src/store.rs:110,1352-1357`).
  [medium, confirmed]
- **Size pressure flushes everything**, not "the biggest offenders" at a low
  watermark; there is one hard threshold → `sync_all()` (flush every ref + commit),
  with no low/high watermark split (`cas/src/store.rs:1352-1357`; the code calls this
  "collapsed to the simplest correct policy"). [medium, confirmed]
- **WAL-pressure flush-the-pinner and the 50 % watermark are absent** — flushing
  triggers only at a completely full WAL and then flushes all refs; there is no
  intermediate watermark and no per-ref oldest-WAL-position sort key
  (`cas/src/store.rs:1325-1339`). [medium, confirmed]
- **No operation-count secondary bound and no timer/staleness trigger** exist at all
  (rev0§4.4 mandates both).

These are partly acknowledged as MVP debt in `cas/src/store.rs:20-32`, but the
spec's framing ("the mechanisms above are fixed") makes them conformance gaps, not
tuning choices.

**(M-7) Flush re-chunks whole dirty files**, not the rev0§4.3 "affected neighborhood
only" path (`cas` overlay flush) — acknowledged MVP perf simplification, but it
changes the write-amplification behavior the spec describes.

**(M-8) Tags are not exposed over the wire. [medium, confirmed]** rev0§4.7 tags
(name → snapshot-ID pins, editable under `may-rewrite-history`) exist only via the
in-process `Store::tag()` backdoor (`cas/src/store.rs:1226`, used by tests); the
storage-server `Request` enum (`storage-server/src/lib.rs:92-123`) has no
create/delete/list-tag op, so the `Pinned`-on-tagged-delete semantics
(rev0§4.7) are unreachable over a session.

**(M-9) The console is kernel debug-syscall I/O, not a userspace UART driver.
[high, confirmed]** rev0§7 requires the user-facing console to be a userspace UART
driver holding the PL011 IRQ/MMIO caps, with the "console cap" a channel to it. No
such driver binary exists in `user/`; the PL011 driver lives in the kernel
(`kernel/src/uart.rs`) and the shell does all console I/O through kernel debug
syscalls (`sys::debug_getc/putc/write`, `user/shell/src/main.rs:751,772,87`) — the
path the kernel itself labels *"scaffold … until the userspace UART driver exists
(rev0§7)."*

### 3.2 Spec-deferred (disclosed in rev0§8.3) — present as gaps but not violations

- **Named-grant table / argv / env / standard names** (`root/stdin/stdout/tmp/storage`,
  the deliberately-split stdin/stdout) are unimplemented; init and shell hand-roll
  fixed-layout byte blocks ("SD02"/"SH01"/"ST01") carrying only magic + mode + time-VA
  (`user/*/src/main.rs`). Only `time` is delivered as a standard grant. rev0§8.3 defers
  "a real named-grant-table format". [confirmed-deferred]
- **Ephemeral file-id indirection and rename** (rev0§4.9): no rename op exists in
  `cas`; the overlay is path-keyed. M2 debt.
- **Multi-version wire negotiation** (rev0§3.7): the protocol is at version 2 but
  both peers ship from one tree; no negotiation is implemented.
- **Concurrent/incremental GC, persisted marking, streaming WAL replay** (rev0§8.3):
  all deferred; the synchronous GC and whole-region WAL replay are the present state.

---

## 4. Incomplete verification

Only platform-specific assembly is legitimately out of scope (plus the Rust
compiler, external libraries, and the verification tools themselves). The following
is in-scope project logic that is *unverified* or covered by a *weaker tool than
appropriate*.

### 4.1 What *is* verified (the trusted base, reconstructed)

For calibration, the verification that genuinely holds:

- **`kcore` object core** — `cargo verus verify -p kcore` → 335 verified, 0 errors.
  cspace/CDT, untyped retype (`carve_place` total ∀), channel FIFO, notification
  waiter queue, timer armed list, thread report record, the aspace page-table walker,
  and `sysabi::decode` all carry real `requires`/`ensures`/`decreases`. The teardown
  SCC (`delete`/`destroy_channel`/`destroy_tcb`/`signal`/`cdt_unlink`/`slot_move`) is
  proven, not assumed. The **only** trusted seams left are the four `untyped.rs`
  `external_body` items (three opaque `Ex*` type registrations + `fixed_object_bytes`,
  all `size_of`-positivity facts host-tested by `object_size_positive`) and three
  `assume_specification`s (`bytes_for` positivity; `saturating_mul` and
  `checked_next_multiple_of`, genuine vstd gaps). All four rev0§6.1(a–e) trusted
  seams match the code.
- **CAS decode + recovery-decision cores** — `cargo verus verify -p cas
  --no-default-features` → 58 verified, 0 errors. `pick_survivor`, `commit_target`,
  `advance_head`, `decode_frame`, `replay_bound`, `validate_geometry_fields`,
  `decode_checked_fields`, and the single-entry TLV codec (`decode_raw`/`encode_raw`
  with a canonical accept-set theorem) are proven total. Two `external_body` seams
  (`checksum_ok`, `wal_content_ok`), both BLAKE3-justified per rev0§6.1(e).
- **IPC header + session codecs** — 58 verified; the fixed header codec and the
  window-quota `Admission` are proven.
- **DMA-pool `FreeList`** — `cargo verus verify -p dma-pool` → 26 verified, 0 errors;
  two-buffer disjointness proven ∀ sizes/alignments. No escape hatches.
- **`urt` slots + time** — proven; `slots` allocator bitmap and the seqlock
  conversion `utc_ns_at` (unbounded totality + monotonicity). One escape hatch
  (`debug_check_free`, a `debug_assert!` guard — legitimate).
- **TLA+** — `CommitProtocol` (6886 states, passes, with the rev0§2.2 caveat above),
  `CapRevocation`/`CapRevocation_Teardown` (~799k states per a recorded run; not
  re-runnable here), `IpcReactor` (passes, with a real negative control).
- **Fuzzing** — wire/on-disk/ELF decoders and mount/recovery have cargo-fuzz targets
  with committed corpora (10 cas targets; 581 `request_dispatch` files) and Miri
  replay; decoder oracles use the spec's canonical decode-then-reencode form.

### 4.2 In-scope logic that is unverified but verifiable

rev0§6 routes "the CAS layer", "the IPC crate", "the userspace runtime", and "the
DMA pool" to the Verus tier, and "everything" to the Miri+proptest baseline. The
following falls short of that routing:

- **Prolly-tree shape and the canonical-form property are unverified. [medium]**
  The Verus surface in `cas/src/prolly.rs` covers *only* the single-entry TLV codec.
  `is_boundary` (split rule), `build_level`, `Dir::save`, `load_node` — the actual
  canonical-tree machinery — are plain Rust. The central rev0§4.1 claim *"the same
  logical contents always produce the same tree, regardless of edit order"* is
  carried by proptest/Miri only. This matches rev0§6's "the chunker and prolly tree
  especially" baseline routing, but the headline canonical property deserves a proof,
  not just sampling.
- **GC mark/sweep is unverified and the mark walk can overflow the stack. [medium]**
  `gc::mark` (`cas/src/gc.rs:21-55`) recurses on directory children with **no depth
  bound**, so a pathologically deep (or adversarial) directory tree overflows the
  stack — a crash inside the storage server, contra rev0§4.8's "detect corruption on
  read rather than fault." GC paths are not fuzzed. No `requires`/`ensures` on
  `mark`/`sweep`; mark-set sufficiency is a test oracle (`gc.rs:69`, genuinely real)
  rather than a proof.
- **`mount()`/`commit()` orchestration is unverified plain Rust. [medium]** Comments
  claim recovery totality is "proven ∀" (`cas/src/disk.rs:86-88`), and the *decision
  cores* are; but `Store::mount` (`cas/src/store.rs:957-1136`) — which sequences the
  validated reads, WAL replay, and overlay rebuild — is unverified glue. Combined
  with T-1/T-2, no mechanized artifact ties the running recovery code to
  replay-equality.
- **DMA-pool public wrapper is unverified glue with a soundness hole. [medium]**
  Only `FreeList<N>` is verified; the type drivers actually use, `DmaPool<B>`
  (`dma-pool/src/lib.rs:1260-1307`), calls `FreeList::free`/`alloc` **without
  discharging their preconditions** (`spec_nfree() < N`, `off+n <= len`, …), and the
  runtime `assert!(nfree < MAX_FREE_RANGES)` overflow guard was demoted to a Verus
  precondition with no runtime backstop in the wrapper. `bytes()/bytes_mut()`
  (`:1287-1306`) build raw slices `from_raw_parts(cpu_base().add(buf.offset), buf.len)`
  with no check that `buf` originated from this pool — a `DmaBuf` (Copy, private
  fields) from a larger pool used against a smaller pool's `bytes()` is out-of-bounds
  UB.
- **IPC reactor/endpoint/transport carry no proof. [low]** Verus covers only
  `header.rs` + `session.rs`. The reactor's sequential dispatch (bit allocation, the
  pending drain, the lowest-bit scan) and the endpoint cap-marshalling are
  Loom/Shuttle + unit-test only.
- **Kernel ready-queue list logic is unverified shell, though structurally identical
  to verified `kcore` code. [low–medium]** `enqueue`/`dequeue`/`unqueue_ready`/`top_ready`
  (`kernel/src/thread.rs:79-140`) are intrusive-linked-list manipulations over
  `READY[prio]` + a bitmap — the *same shape* as the notification waiter queue and
  timer armed list that `kcore` *does* verify. rev0§6.1(d) routes "the scheduler" to
  trust, so this is by-design, but the list surgery (as opposed to the asm context
  switch) is verifiable and was left in the shell.
- **The frame-MAP operation is unverified while the symmetric unmap is verified.
  [medium]** rev0§6.1(c) claims "the cap-side unmap is proven over object state" —
  and it is (`cspace::delete`'s frame-unmap drives `aspace_unmap`/`unref_aspace`
  through the verified census). But the matching MAP side lives entirely in the
  unverified syscall shell (`kernel/src/syscall.rs:475-512`): it sets
  `mapping: Some((asp, va))` and does a raw `(*asp_ptr).hdr.refs += 1`. The asymmetry
  is real and the rev0§6.1(c) wording quietly only claims the unmap half.
- **The spawn-time priority-ceiling *check* is an unverified shell `if`. [medium]**
  rev0§5.4 says *"the spawn-time check that a thread's priority does not exceed its
  ceiling **are verified**."* The cap-carried ceiling and its monotone attenuation
  *are* verified (`cspace::derive`), but the decision to refuse a start when
  `prio > max_prio` is a plain `if` in the syscall shell
  (`kernel/src/syscall.rs:425,560`), not a verified gate. The spec over-claims the
  verified surface here.
- **The `urt` heap allocator is wholly unverified. [high]** `urt` is in the Verus
  verify set and its `slots`/`time` modules are proven, but the crate's actual
  `GlobalAlloc` — `Heap<N>::alloc/dealloc` (`urt/src/lib.rs:48-159`): first-fit
  free-list traversal, alignment padding, block splitting, two-sided
  address-ordered coalescing over raw `*mut Block` — is heavy `unsafe` pointer
  arithmetic with only two happy-path tests, no Miri target, no proptest, no proof.
  This is the largest single block of unverified `unsafe` in a "verified" crate.
- **virtio-blk has no proptest/Loom/Miri. [medium]** rev0§6's "Miri + proptest —
  everything" baseline is unmet for the driver's ring arithmetic, descriptor-chain
  construction, and `u16` index wrap; only 4 hand-written example tests exist, and
  the `fake` device completes synchronously so `complete()`'s poll loop is never
  exercised *as a loop* (which is also why I-4 escaped the tests).
- **storage-server rights lattice has only example tests. [low]** No proptest over
  monotone attenuation across arbitrary derivation chains; no Loom/Shuttle target,
  though rev0§6 routes userspace servers to concurrency testing.
- **`mkfs` directory-walk and the user binaries** have no automated tests
  (one happy-path integration test for `mkfs`; the five user binaries are validated
  only by QEMU boot output). `loader::prepare`'s page-rounding (the I-5 site) has no
  host model.

### 4.3 Suboptimal-tool / tool-fit observations

- **The seqlock tool choice is *correct*, and worth recording as a positive.** The
  `urt/src/time.rs` seqlock uses Acquire/Release + Relaxed data loads + an Acquire
  fence — orderings that **Shuttle cannot witness a tear under** (it models all
  orderings as SeqCst). The project correctly makes **Loom the certifying tier** and
  explicitly labels the **Shuttle harness "non-certifying"** (`urt/src/time.rs:28-30,
  644-679`), exactly the rev0-audit-rule-(f) call. This is the opposite of
  tool-defusing.
- **Caveat: the Loom docstring over-claims Relaxed fidelity. [medium]** The Loom test
  docstring (`urt/src/time.rs:585-631`) claims Loom enumerates *"every C11-permitted
  interleaving and reordering"*; Loom does **not** faithfully model Relaxed atomics.
  The seqlock's correctness rests on the explicit Acquire fence (which Loom *does*
  model), so the conclusion holds, but the docstring overstates what Loom proves.
- **Loom adds little over Shuttle for the IPC model.** `ipc/` contains **no atomics**
  at all — the model synchronizes via `crate::sync` Mutex/Condvar. Loom's distinctive
  value (weak-memory atomic reorderings) is moot there; the choice is harmless but the
  "Loom-certified" framing carries less weight than for the seqlock.

---

## 5. Spec deficiencies — where the code is better, or resolves an ambiguity, or the
spec is wrong

- **(S-1) Spec self-contradiction: rev0§8.3 claims guarded ref-table batches are
  "implemented now"; they are not** (see I-2). The spec text is wrong, independent of
  the implementation gap.
- **(S-2) Channel retype takes `depth` directly, inverting rev0§3.2's "bytes / slot
  size".** `kcore/src/untyped.rs:401-406` takes a creator-chosen `depth` and computes
  bytes via `Channel::bytes_for(depth)` — cleaner than "donate bytes, derive depth by
  truncating division", and makes the creator's intent explicit. The code resolves an
  underspecification in the better direction.
- **(S-3) Wall-time formula hardened beyond the spec.** rev0§2.6 gives
  `wall_base + (now-cntvct_base)·1e9/cntfrq` with no guidance on `cntfrq==0` or a
  regressing counter; `urt/src/time.rs` floors `cntfrq` to 1 and saturates a
  below-baseline delta to 0. Sensible robustness the spec omits.
- **(S-4) Window quota is a server-total pool, not a per-session cap.** rev0§3.1/§2.5
  call it a "per-session window quota"; `ipc/src/session.rs` `Admission` decrements
  one server-wide budget across all sessions. This satisfies the load-bearing
  anti-drain property (total granted ≤ budget, proven ∀) but enforces no per-session
  maximum — one connect can claim the whole budget. The spec wording is ambiguous;
  the code picks the global interpretation.
- **(S-5) Claim-ticket TTL is caller-chosen with no server clamp.** rev0§2.4 says
  "short-TTL"; `MintTicket` (`storage-server/src/lib.rs:537-552`) uses the client's
  `ttl_nanos` verbatim with no upper bound, so a holder can mint an effectively
  unbounded ticket — weakening "bound the exposure window." The spec doesn't pin
  "short"; the code trusts the caller.
- **(S-6) Mount validates index-frame fields *past* the "single chokepoint".**
  rev0§4.5 says geometry validation is "at a single chokepoint"; the chokepoint
  (`validate_geometry`) covers the superblock, while index-frame `ilen` and each
  entry's `(off,len)` are validated in additional gates (`cas/src/store.rs:1033-1059`).
  This is *correct* layering (each derived field checked against the now-trusted
  chunk-tail, honoring "untrusted fields must never vouch for each other") — the
  spec's literal "single chokepoint" wording is the imprecise part.
- **(S-7) Syscall ABI borrows rev0§3.7's "unknown opcode → error, never crash".**
  The spec has no section defining the syscall ABI; `sysabi.rs` reasonably applies the
  rev0§3.7 wire-protocol discipline to syscall numbers. A spec gap the code fills (the
  comment's rev0§4.6 citation is wrong — see rev0§2.3).
- **(S-8) Debug UART syscalls are ambient authority.** rev0§2 forbids ambient
  authority and rev0§7 sanctions only a *debug* UART path; `DebugPutc/Write/Getc`
  (`kernel/src/syscall.rs:178-195,597-600`) are unconditional for any EL0 thread.
  These are documented M1 scaffolds, but until M-9 (userspace console) lands they are
  a standing ambient-authority hole the spec does not bless for the *user-facing*
  path.
- **(S-9) Shipped defaults diverge from every rev0§4.4 number.** `StoreOptions::default`
  uses 16 MiB WAL / 8 MiB budget (mkfs overrides to 1 MiB WAL) vs the spec's
  64 MiB / 128 MiB / 8 MiB-per-ref. The numbers are explicitly tunable, so this is
  soft, but no shipped default matches the spec, and the single budget conflates the
  spec's per-ref and global figures.
- **(S-10) `mkfs` and `format` panic on an undersized device** instead of the clean
  `Result`/`ExitCode::FAILURE` path that exists (`mkfs/src/main.rs:74` →
  `cas/src/store.rs:903` `assert!`). rev0§4.5 makes refuse-not-panic the discipline
  for *mount* over arbitrary contents; `format` has no analogous contract, so this is
  a spec gap the code falls into rather than a violation.
- **(S-11) virtio-blk does not bounds-check LBA against device capacity**
  (`virtio-blk/src/lib.rs:279-298`), relying on the device to error. Safe (the device
  is ground truth for its geometry and the DMA buffer is fixed-size), and the spec
  doesn't require a pre-check; recorded as a resolved ambiguity.
- **(S-12) TLA `IpcReactor` disables deadlock detection** (`CHECK_DEADLOCK FALSE`) —
  reasonable (an all-delivered terminal state is legitimate), but it means a genuine
  lost-wakeup *deadlock* is caught only by the `EventuallyDelivered` liveness property,
  not by deadlock detection. If that `PROPERTY` line were ever dropped, a true
  deadlock would pass silently. Worth a comment pinning the dependency.

---

## 6. Recorded for posterity (development history extracted from comments)

Per the audit instruction, the substantive history embedded in the comments being
cleaned (rev0§1.3) is preserved here so nothing is lost. Most of this is also the
evidence that the verification is real.

**Verification migration (Kani → Verus).** The kernel, the IPC header/session
codecs, `urt::slots`, `urt::time::utc_ns_at`, and the DMA-pool free-list were all
first mechanized under **Kani (CBMC)** with bounded harnesses, then ported to
unbounded **Verus** proofs; the Kani tier and its CI job were retired (matching
rev0§8.4). The port's enabling move was the "arena rewrite": an earlier intrusive
`*mut` object graph plus a separate `Env`/`Hal` seam were unified into the
handle-based `Store` trait (`ObjId`/`SlotId` indices, no raw pointers in `kcore`),
which is what made the core first-order and verifiable.

**Specific defects found and fixed during development** (worth keeping as regression
lore):
- **UO-1/UO-2** — carve overflow on the `param` argument in `kcore` retype; fixed
  with checked math in `carve_place`.
- **AS-1** — the prior kernel page-table walker honored `PERM_X` on device memory;
  `pte_encode` now forces device memory non-executable (UXN), verified.
- **OVL-1** — a write-out-of-range overlay bug; `StoreError::WriteOutOfRange` →
  storage-server `BadOffset`, regression-tested both layers.
- **ELF-1** — `e_phoff` near `u64::MAX` panicked in phoff arithmetic despite the
  "no panics" contract; found by the `elf_parse` fuzz target, now returns
  `Truncated`, pinned by `loader/tests/fuzz_regressions.rs`. (Note: the *consumer*
  `spawn::prepare` has the analogous unfixed hazard — finding I-5.)
- **DN-10** — the two-buffer DMA disjointness proof bit-blasted CaDiCaL to OOM under
  Kani; re-expressed in modular arithmetic (`off + (align - off%align)%align`
  instead of the bit-mask round-up) so it proves ∀ sizes/alignments in Verus without
  `by (bit_vector)`.
- The shell's spawn/reap "burn fix" — an earlier shell leaked cspace slots across
  spawns; the current `SlotAlloc` window + single reusable donation untyped +
  revoke/reset returns slots and memory clean per child. `selftest`'s `.bss` probe
  (write `0xA5`, re-probe next spawn) exists specifically to catch a kernel that
  fails to re-zero frames on untyped reuse.

**Proof-structure facts worth keeping:**
- `kcore` teardown is a mutually-recursive SCC (`delete → obj_unref →
  destroy_cspace/destroy_channel/destroy_tcb → delete`) closed under a shared
  lexicographic `decreases (count_nonempty(slot_view), height)` measure; it was kept
  `external_body` until the whole SCC could be flipped to proven bodies in one step
  (piecemeal destructor proofs are unsound).
- `destroy_tcb` needs `#[verifier::spinoff_prover] + rlimit(30)` because surfacing
  the rev0§5.4 `max_prio` ceiling field destabilized Z3 resource accounting
  (Linux-vs-macOS rlimit differences).
- The maximum-controlled-priority ceiling is the `u8` in `CapKind::Thread(o, max_prio)`;
  `derive` proves the derived ceiling `== min(parent, requested)`; `0xFF` is the
  no-reduction sentinel.
- `revoke`'s contract previously sat behind a `requires !is_homed` that was **false
  for every real `Sys::CapRevoke` target** — a vacuously-true guarantee over real
  inputs; the precondition was dropped and descendant-deletion + `cspace_wf` are now
  unconditional. (A textbook instance of the vacuous-premise hazard this audit
  watches for — caught and fixed in development.)
- The deferred-reuse law (rev0§4.2) is implemented via `ChunkStore.pending_free` vs
  `free`: freed extents migrate to `free` only at `commit()` *after* barrier 2
  (`cas/src/store.rs:1607,1613`). Index-frame placement is first-fit over committed-free
  extents then tail (not tail-only) — satisfies rev0§4.2 "no wedging".
- `CrashDev` models torn writes at *byte* granularity (arbitrary prefix), stronger
  than real sector atomicity; each superblock slot is a separate write call so one
  torn write cannot damage both — faithful to rev0§4.5.
- The `*_has_teeth` tests (cspace_wf/chan_wf/notif_wf/timer_wf/refcount_sound/…) each
  mutate exactly one invariant clause and assert the executable mirror *rejects* it,
  with a positive control — so the differential seam tests are demonstrably
  non-vacuous.
- TLA negative controls (the strongest anti-theater signal): deleting `RecvBlock`'s
  `word = 0` guard makes `NoLostWakeup` reachable-false; on real code, removing
  `register`'s poll-once self-signal deadlocks the send-before-bind harness, and
  removing `recv_nb`'s on-writable signal hangs the backpressure harness — each
  verified to break a check.
- A recorded TLC run reported `CapRevocation` Spec at ~799k states (not re-runnable in
  the audit environment); `CommitProtocol` re-ran here at 6886 states.

---

## 7. Actions taken in this pass

- **Wrote this audit** to `doc/results/audit_rev0.md` (the first file in the
  previously-empty `doc/results/`).
- **Cosmetic cleanup** (comment-only; no logic touched) — see the accompanying diff:
  genuine spec references converted to the mandated `rev0§` form; the factually-wrong
  comments fixed (stale `external_body` claims on now-proven bodies; wrong
  spec-section citations rev0§4.6→rev0§3.7 and the nonexistent rev0§9/rev0§10;
  stale "Kani-verified"/"Kani verifies it" labels); and historical/development-doc
  baggage reworded to describe current state. The substantive history removed is
  preserved in rev0§6 above.

## 8. Suggested follow-ups (not done here — logic changes, out of audit scope)

Highest-value first: (1) add the `stat-store` right and gate `statfs` (I-1);
(2) implement guarded ref-table batches or correct rev0§8.3's "implemented now"
claim (I-2/S-1); (3) make the WAL used-ring poll volatile (I-4) and bound the ELF
page-rounding (I-5); (4) add a TLA `Recover`-reconstruction action property so
replay-**equality** is actually checked (T-1), and label the fsync axiom (T-4);
(5) bound the GC mark recursion and fuzz GC (rev0§4.2 stack-overflow); (6) verify or
Miri-cover the `urt` heap allocator and the DMA-pool wrapper (rev0§4.2); (7) restore
the per-ref/watermark flush mechanisms or move them onto a disclosed-simplification
list (M-3…M-6); (8) populate or remove the dangling `doc/plans/` & `doc/results/`
references and the absent trusted-base ledger.
