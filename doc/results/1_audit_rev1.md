# Eunomia OS — Conformance & Verification Audit (rev1)

Re-audit of the implementation against `doc/spec/spec_rev1.md` (the blessed
revision 1), measuring how the system responded to the prior audit
`doc/results/0_audit_rev0.md`. All section references use the mandated `rev1§X.Y`
form. The rev1 spec was *itself* written to address the rev0 audit, so its text is
also a review target here.

---

## 0. Method and headline assessment

**Method.** Ground truth was established first by running the verification gates the
spec and the trusted-base ledger rest on, then the codebase was partitioned into 19
investigation units (each covering a coherent slice of code against the rev0 finding
IDs it should resolve) and audited by a fan-out of reader agents; every high/medium
verdict was then handed to an independent **adversarial verifier** instructed to
*refute* it by re-reading the cited code. 33 agents produced 112 findings; **42
high/medium verdicts were adversarially re-checked and 0 were refuted.** Two units
independently re-ran the TLA+ models under TLC (Temurin 17 + the vendored
`tla2tools.jar`); the author independently spot-checked the load-bearing claims
(I-1, I-4, M-1's marker, the lemma call-sites, the dangling-doc set).

**Ground-truth gates (run for this audit, not taken from the ledger):**

| Gate | Result | Ledger claim | Match |
|---|---|---|---|
| `cargo verus verify -p kcore` | 389 verified, 0 errors | 389/0 | ✓ |
| `cargo verus verify -p cas --no-default-features` | 80 verified, 0 errors | 80/0 | ✓ |
| `cargo verus verify -p ipc` | 69 verified, 0 errors (fresh run) | 69/0 | ✓ |
| `cargo verus verify -p freelist` | clean (exit 0) | 29/0 | ✓ |
| `cargo verus verify -p dma-pool` | 0 verified, 0 errors | 0/0 | ✓ |
| `cargo verus verify -p urt` | 29 verified, 0 errors | 29/0 | ✓ |
| `CapRevocation` TLC (re-run) | 503,070 distinct states, no error | 503,070 | ✓ |
| `cd kernel && cargo build` (bare-metal) | exit 0 | — | compiles |
| Host tests (cas/ipc/loader/storage-server/freelist/urt/dma-pool) | all green (cas 133, …) | — | pass |

(The root `cargo build` fails `E0152` on the `kernel` bin — expected, because `kernel`
is bare-metal `no_std` and builds under its own target; not a regression.)

**Headline.** The rev0 audit's findings have been addressed comprehensively and, in
the overwhelming majority, **correctly**. Of the rev0 catalogue — five implementation
defects (I-1…I-5), five verification-theater findings (T-1…T-5), nine
incomplete-mechanism findings (M-1…M-9), the §4.2 verification gaps, and the spec
deficiencies (S-1, S-5, S-8…S-12) — **every one is now backed by real, gate-passing
code or model artifacts.** Zero findings were omitted, and **no regressions were
introduced**: every "recorded for posterity" fact from rev0 §6 (the carve-overflow
fix, the device-memory UXN encoding, the WriteOutOfRange path, the ELF `e_phoff`
guard, the DMA disjointness proof, the burn-fix, the proven teardown SCC, the
`*_has_teeth` differential tests, the byte-granular `CrashDev`, the deferred-reuse
law) was confirmed intact, and the seven claims rev0 *refuted* still refute.

The verification surface grew substantially and honestly. Phases B7–B15 and the C/B-IRQ
work moved into the verified surface: the cap-side frame **map** (symmetric with
unmap), the priority-ceiling **refusal** gate, the 32-level **ready queue**, the
**preemptible revoke** (`revoke_step` + the `revoking` marker + the derive guard),
aspace **pool growth**, the **freelist** allocator algorithm shared by the DMA pool
and the urt heap, the prolly **level-partition** core and **node decoder**, the WAL
**structural decode** split from its BLAKE3 wrapper, the **`RecoverReconstructs`**
recovery action property, the completed **`IpcReactor`** protocol (with the symmetric
writable/backpressure half), and the reactor's **`lowest_clear_bit`** allocator core.
The rev0 audit's single most important verification gap — T-1, that a no-op recovery
passed the commit model — is closed by a genuine action property with a committed
negative control, and the rev0 audit's largest block of unverified `unsafe` (the urt
heap, [high]) is now verified at the algorithm level.

The residue is small and almost entirely cosmetic:

1. **Documentation drift introduced by the recent `cleanup stale docs` commit** — ~9
   live source/config files now cite `doc/plans/*` and `doc/results/*` files that were
   deleted (rev1§6 below; the one finding rated *medium*).
2. **Two low-severity verification-*wiring* precision gaps** — top-level composition
   lemmas (`lemma_partition_flatten`, `lemma_grow_pool`) are proven but never *called*
   from the executable path, so the end-to-end "the bytes emitted are a conservative
   partition" / "the topped-up pool preserves every mapping" statements are narrower
   than the headline suggests (rev1§2).
3. **One soft spec over-claim** — rev1§4.4 says the shipped server "matches" the 64 MiB
   WAL default, but the live server runs a 1 MiB WAL from the mkfs image geometry
   (rev1§5).
4. **One minor runtime observation** — with `debug-log` on (default for dev images) the
   kernel diagnostic UART and the userspace console both write the same physical PL011
   (rev1§5).

None of these is a confinement, safety, or conformance defect.

---

## 1. Findings addressed **correctly** (Q1)

Every rev0 finding below was re-read against the current code and confirmed fixed;
high/medium verdicts were adversarially re-verified. The "expected drift" — rev0§ spec
references becoming rev1§ — held: the converted refs are correct (the unconverted
stragglers are catalogued in rev1§6).

### 1.1 Implementation defects (rev0 §2.1)

- **I-1 [high] — `stat-store` right + ungated `statfs`.** Fixed. `R_STAT_STORE = 1<<5`
  is a distinct bit deliberately **excluded** from `R_ALL = 0b1_1111` (bits 0–4), so
  `attenuate(parent, mask) = parent & mask` strips it on any `R_ALL`-or-narrower mask;
  the bit originates only on the privileged `root_grant` (`rights: R_ALL | R_STAT_STORE`),
  and the `Statfs` handler gates on `lookup(session, handle, R_STAT_STORE)` *after* the
  generation/Stale check. `OpenChild`/`OpenSnapshot` drop it by intersection.
  Behaviorally tested (`statfs_gated_by_stat_store`, `stat_store_scope_ignores_subtree`,
  the `gate_matches_bit` proptest). (`storage-server/src/lib.rs:52,57,63,444,876,882`;
  `storage-server/tests/sessions.rs:868`; `storage-server/tests/rights_lattice.rs:113`)
- **I-2 [high] — guarded ref-table batches / per-ref edit-version.** Fixed end-to-end.
  `RefEntry` carries `edit_version: u64` (`cas/src/disk.rs:724`), serialized as a
  fixed-width tail with a truncation-reject test; it advances **exactly once per commit**
  for every ref whose entry set changed, via a single `touch_ref`→`commit` dirty-set path
  that covers every mutation site (snapshot, rollback, tag, untag, flush head-move,
  delete-snapshot, set-class, apply-batch) and is kept **orthogonal** to the §2.2
  revocation generation (`bump_generation` does not `touch_ref`). `ListSnapshots` returns
  the version; `Store::apply_batch(ref, expected_version, edits)` checks the version
  first (returning `VersionMismatch{current}` without mutating), stages all-or-nothing on
  a clone, and commits once. Wired to the boundary as `Request::Apply` gated on
  may-rewrite-history. (`cas/src/store.rs:1898,2949,3180,3192,3195`;
  `storage-server/src/lib.rs:204,711,905`)
- **I-3 [high] — dedup-resurrection fix.** Implemented. `ChunkStore` carries a
  `condemned: BTreeSet<Hash>`; `put`'s dedup short-circuit returns the existing hash
  *only* when `index.contains_key(&hash) && (condemned.is_empty() || !condemned.contains(&hash))`
  — a hit on a condemned chunk falls through and is rewritten to a fresh extent at the
  current birth generation, then un-condemned; sweep opens the resurrection-check window.
  (`cas/src/store.rs:262,393,3062`)
- **I-4 [medium] — non-volatile used-ring poll.** Fixed. `poll_used()` reads the
  device-written `used.idx` via `self.pool.read_volatile(&self.used, 2, &mut idx)` (a real
  `read_volatile` per byte) and follows a detected change with an `Ordering::Acquire`
  fence; it is the *only* reader of the used ring. (`virtio-blk/src/lib.rs:314`;
  `dma-pool/src/lib.rs:217`)
- **I-5 [medium] — ELF page-rounding overflow.** Fixed. `Segment::page_layout` rounds
  the start down and computes `va_end` with `vaddr.checked_add(memsz).and_then(checked_add(PAGE-1))`,
  rejecting the near-`u64::MAX` adversarial image cleanly instead of panicking/wrapping;
  the consumer `prepare()` propagates the error. (`loader/src/elf.rs:46`;
  `loader/src/spawn.rs`)

### 1.2 Verification theater (rev0 §2.2)

- **T-1 [high] — recovery reconstruction.** Fixed. `CommitProtocol.tla` defines
  `RecoverReconstructs` as a genuine action property `[][ (crashed /\ ~crashed') => …
  overlay'[r] = {committed roots + surviving WAL} ]_vars` that fires on exactly the
  recover edge (every other action requires `~crashed` or sets `crashed'`); it is wired
  as a `PROPERTY` in `CommitProtocol.cfg`, and the committed negative control
  (`CommitProtocol_NegControl.cfg`, a no-op `Recover`) is verified to **fail** it. A
  verifier independently re-ran TLC. (`tla/commit_protocol/CommitProtocol.tla:281`;
  `CommitProtocol_NegControl.cfg`)
- **T-2 [medium] — `lemma_gap_freedom` dead proof.** Fixed. `recover_records`
  (`cas/src/store.rs:1365`) now carries `laid_out(...)` in its `ensures`, **proves** it in
  the body, and **fires** `lemma_gap_freedom` on the rebuilt run — the lemma is no longer
  dead and its hypothesis is discharged at recovery rather than assumed.
- **T-3 [medium] — IPC reactor wait-side only.** Fixed. `IpcReactor.tla` now has a
  `Register` action with the poll-once self-signal (`word' = IF Len(queue) > 0 THEN 1 ELSE
  word`), a symmetric writable path, a `NoLostWakeupWritable` invariant, and three
  committed negative controls (send-before-bind, dropped on-writable, dropped `word=0`
  guard) — all re-run under TLC and reporting the expected violations.
  (`tla/ipc_reactor/IpcReactor.tla:121`; `IpcReactor_Neg*.cfg`)
- **T-4 [medium] — `fsync = fsync` axiom.** Fixed. `ASSUME FsyncMeansFsync == TRUE` is a
  named, top-level, grep-able axiom parsed by SANY/TLC, documented as the single trusted
  storage axiom. (`tla/commit_protocol/CommitProtocol.tla:55`)
- **T-5 [low] — `wal_content_ok` decode bundling.** Fixed. `wal_content_ok` is now the
  verified composition `wal_struct_ok(...) && wal_checksum_ok(...)`; the structural decode
  is verified and only the BLAKE3 checksum remains uninterpreted. (`cas/src/store.rs:1022,1076`)

### 1.3 Incomplete mechanisms (rev0 §3.1)

- **M-1 [medium] — preemptible/restartable revoke.** Fixed. `revoke_step`
  (`cas`… `kcore/src/cspace.rs:12153`) is a counted loop guarded by `n < budget` with
  `decreases budget - n`, doing **at most `budget` leaf-deletions per call** regardless of
  subtree size — bounded work, the property rev0 demanded (it had proven *termination*, not
  *preemptibility*). `CapSlot` carries a `revoking: bool` marker, set on `More`/cleared on
  `Done` under a framing lemma; `derive`'s ancestor-guard refuses growth into a revoking
  subtree. The kernel exposes **only** the bounded quantum (`kernel/src/cspace.rs:37`); the
  EL0 retry loop re-issues it unmasked (`ipc/src/sys.rs:177`). The `CapRevocation` TLA model
  (503,070 distinct states, clean; two negative controls firing) covers the cross-restart
  interleaving and `EventuallyRevoked` liveness.
- **M-2 [medium] — aspace pool top-up.** Fixed (all three error-story parts: `NEED_MEMORY`
  / top-up / return-at-teardown), with a verification-wiring caveat noted in rev1§2.2.
  `kcore::aspace::grow_pool` and its stability lemmas verify; `kernel/src/untyped.rs:120`
  debits the donor untyped's watermark via `carve_place` with an abutment guard, and reset
  returns the pool at teardown.
- **M-3…M-7 [medium] — flush/memtable policy.** All fixed; the policy is no longer
  collapsed (B12). `StoreOptions` now carries a per-ref soft bound *and* the global budget
  (M-3); `relieve_size_pressure` sorts dirty refs by size and flushes the biggest offenders
  one at a time down to a **low** watermark (M-4, not flush-everything); the WAL is a
  **circular ring** with `relieve_wal_pressure` flushing the tail-pinner at the watermark
  and both normative edge cases (oversize record bypasses + commits sync; full WAL flushes
  everything + resets) (M-5); an **op-count** secondary bound and a **staleness** timer both
  exist (M-6); and flush re-chunks the **affected neighborhood only** via
  `store_file_neighborhood`, a real bounded algorithm (M-7). (`cas/src/store.rs:179,185,2358,2434,2488,2576,2875`)
- **M-8 [medium] — tags over the wire.** Fixed. `Request::{Tag,Untag,ListTags}` +
  `Response::Tags` exist, gated by may-rewrite-history; `Pinned`-on-tagged-delete is
  reachable and tested over a full session. (`storage-server/src/lib.rs:220,235,310,920`;
  `cas/src/store.rs:3118`)
- **M-9 [high] — userspace console driver.** Fixed. A real `user/console` driver receives
  a startup block, resolves the PL011 MMIO window via a `NAME_PL011_MMIO` REGION grant (not a
  hardcoded base), binds the IRQ cap to a wake notification, and serves a console channel;
  `init` spawns it before the shell and wires the channel under `stdin`/`stdout`; the shell
  does all terminal I/O over the channel with no ambient fallback. The pl011 register layer
  is host-tested. (`user/console/src/main.rs`, `pl011.rs`; `user/init/src/main.rs:179,418`;
  `user/shell/src/runtime.rs:829`)

### 1.4 Verification gaps (rev0 §4.2)

- **urt heap allocator [high].** Closed. The hand-rolled `*mut Block` allocator is gone;
  the algorithm is now `freelist::FreeList<N>` (verified: first-fit, align round-up, split,
  two-sided address-ordered coalesce over a side-stored `(offset,len)` model), the heap
  delegates to it, and the plain-Rust arena byte-region seam is covered by a Miri+proptest
  tier. (`freelist/src/lib.rs`; `urt/src/lib.rs`)
- **DMA-pool wrapper soundness hole [medium].** Closed. `FreeList` preconditions are
  discharged at runtime (the demoted overflow `assert` is restored as a hard `!is_full()`
  backstop), and the cross-pool `bytes()`/`bytes_mut()` UB hazard is addressed.
  (`dma-pool/src/lib.rs:113`)
- **frame-MAP unverified [medium].** Closed. `kcore::cspace::map_frame` is a verified op,
  term-for-term the mirror of the unmap branch, driving a `Store::aspace_map` seam; the raw
  `hdr.refs += 1` left the shell. (`kcore/src/cspace.rs:11091`)
- **priority-ceiling shell `if` [medium].** Closed. `kcore::thread::set_priority` is a
  verified refusing op (over-ceiling → `Err`, thread untouched; accepted → priority proven
  `<= ceiling`); the two shell `if prio > max_prio` gates were deleted.
  (`kcore/src/thread.rs:239`)
- **kernel ready-queue [low-medium].** Closed. The 32-level queue is verified in
  `kcore/src/ready.rs` (bit-scan `top_ready` + bitmap coherence + four list ops); the kernel
  shell is now thin wrappers.
- **prolly canonical/shape [medium]** and **GC mark/sweep + stack overflow [medium].**
  Addressed. A verified node decoder (`decode_node`, total ∀ bytes + leaf canonical
  round-trip) and a verified level-partition core (`split_points`/`boundary_flags`) were
  added over the opaque `is_boundary` BLAKE3 seam; the GC mark walk's unbounded recursion is
  replaced by an explicit heap work-stack with mark-on-push dedup (the stack-overflow hazard
  is closed), and a `gc_mark` cargo-fuzz target + Miri-replayed sufficiency oracle now exist.
  (The headline canonical-form property remains correctly *proptest*-routed — see rev1§2.2.)
- **virtio-blk no tests [medium].** Closed. `ring_props.rs` adds the proptest tier
  (chain round-trip, avail-ring bounds, index-wrap), and `async_complete.rs` exercises the
  poll loop **as a loop** — the gap that let I-4 escape.
- **reactor dispatch / cap-marshalling [low]** and **rights lattice [low].** Addressed:
  `lowest_clear_bit` is Verus-verified (the rest of multi-source dispatch is honestly
  proptest-routed), and a monotone-attenuation proptest over arbitrary `OpenChild` chains
  replaced the example-only tests.

### 1.5 Spec deficiencies the code now satisfies (rev0 §5)

- **S-5** (claim-ticket TTL) — `MintTicket` now clamps `ttl_nanos.min(MAX_TICKET_TTL_NANOS)`.
- **S-8** (ambient debug-UART authority) — the `debug getc` input opcode is removed from the
  verified decoder (decodes to `UnknownCall`); `debug putc`/`write` are gated behind a
  `debug-log` feature with an inert no-op when off; closed for the user-facing path.
- **S-9** (shipped defaults) — `StoreOptions::default` now ships the exact rev1§4.4 numbers
  (8 MiB per-ref, 128 MiB global, 30 s staleness, op-count, 50 % WAL watermark). (Caveat: the
  *live* WAL size, rev1§5.)
- **S-11** (LBA bounds check) — resolved as a blessed ambiguity: rev1§4.5 now explicitly says
  the driver trusts the device for its geometry; a defensive check is permitted, not mandated.
- **S-12** (deadlock detection off) — the `EventuallyDelivered ↔ CHECK_DEADLOCK FALSE`
  dependency is pinned in a labeled cfg comment.

---

## 2. Addressed but **incorrectly or incompletely** (Q2)

Two genuine (low-severity) precision gaps, both of the same shape — a *verified standalone
lemma that is never composed into the executable path* — plus the soft spec over-claim
carried under rev1§5.

- **(2.1) Prolly conservation is proven but not wired into node emission. [low]** The
  load-bearing conservation theorem `lemma_partition_flatten` (`cas/src/prolly.rs:1562`) is a
  `proof fn` with **zero call sites** — grep finds it only in its own definition and in
  doc-comments. Its hypotheses match `split_points`'s `ensures`, but nothing machine-connects
  them; and `build_level` (`prolly.rs:324`), the exec function that drives the cuts and
  stores nodes, carries **no `requires`/`ensures`**, so even `split_points`'s proven
  postconditions are not propagated into a statement about the bytes emitted. The *cut-index
  function* is genuinely verified; the chain "verified cut points ⇒ `build_level` emits a
  conservative partition of the real entry list ⇒ `Dir::save` root well-formed" is not
  mechanized. This does not contradict any doc claim (the docs say `build_level` "stays plain
  Rust"), but the verification is narrower than "the partition is verified" might suggest.
  *Independently confirmed: no exec call site exists.*
- **(2.2) Aspace top-up: the composition lemma is unwired and the accounting is untested.
  [low]** The same pattern recurs: `lemma_grow_pool` (the top-level "a contiguous pool
  extension preserves `pt_wf` and every existing mapping" theorem) is referenced **only in
  doc-comments**, never called from verified exec code — although its per-VA stability core
  `lemma_grow_pool_lookup` *is* wired (`kcore/src/aspace.rs:951`). Separately, the kernel-side
  `aspace_topup` (the unsafe shell that debits the donor watermark) has **no direct unit
  test** (the open B10C item): the host tests model `grow_pool` by a bare `Vec::extend` and
  never exercise the watermark debit, the abutment guard, or the debit-then-reset round trip.
  No production userspace binary calls `map_grow`/`aspace_topup` yet, so the path is exercised
  only by host tests. M-2 composes correctly *by construction*, but part 3's accounting is
  asserted by argument, not by test. *Independently confirmed: `lemma_grow_pool` has no exec
  call site.*

**Observation.** These two are the residue of an otherwise-strong verification expansion. The
recommendation is to either invoke the top-level lemmas from the exec functions (giving the
end-to-end statement) or to explicitly document them as standalone design theorems, and to
land the B10C top-up accounting test.

---

## 3. **Omitted** findings (Q3)

**None.** Every rev0 finding was addressed. The items rev0 §3.2 explicitly recorded as
*spec-deferred* (concurrent/incremental GC, persisted marking, streaming WAL replay,
data-root transactional CAS, multi-window sessions, the kernel wait-set object, IOMMU
migration) remain deferred **and are still disclosed as such in rev1§8.3** — that is
conformant, not an omission. Two formerly-deferred rev0 items were in fact *delivered* this
revision (the named-grant table and version negotiation; see rev1§4). The only residual deferral
that touches a now-claimed feature is the client-side endpoint-cap connect handshake
(rev1§5, disclosed), which is orthogonal to the version-negotiation mechanism rev1§3.7 claims.

---

## 4. **Regressions** (Q4)

**No code regressions found.** Each "recorded for posterity" fact from rev0 §6 was
re-confirmed against current code:

| rev0 §6 fact | Status |
|---|---|
| UO-1/UO-2 carve overflow → `checked_add` in `carve_place` | intact, Verus `ensures` proven |
| AS-1 device memory forced UXN in `pte_encode` | intact, verified (XN regardless of `PERM_X`) |
| OVL-1 `WriteOutOfRange` → `BadOffset`, both layers | intact, regression-tested |
| ELF-1 `e_phoff` near `u64::MAX` → `Truncated` | intact, `checked_*` throughout, fuzz-pinned |
| DN-10 two-buffer DMA disjointness proven ∀ | intact in `freelist` (modular arith, no `bit_vector`) |
| Shell burn-fix (`SlotAlloc` window + reusable donation untyped) + selftest `.bss` probe | intact |
| `kcore` teardown SCC proven (no `external_body`) + `*_has_teeth` tests | intact, `cspace.rs` has zero `external_body` |
| TLA negative controls (RecvBlock `word=0`, etc.) | intact and now **runnable** committed cfgs |
| `CrashDev` byte-granular torn writes | intact |
| Deferred-reuse law (free only at commit after barrier 2) | intact (line numbers drifted, law holds) |

The seven claims rev0 §2.6 *refuted* still refute (the single-core/IRQ-masked basis for TLA
`Revoke` atomicity, the conformant 64-source reactor + `FireSafe` semantics, the
deferred-not-missing GC concurrency, etc.). A light smell-scan of the highest-churn files
(`cas/src/store.rs`, `kcore/src/cspace.rs`) surfaced no new correctness smell — non-test
`unwrap`s in `store.rs` are all guarded by preceding length/contains checks.

**One documentation regression** (not code): the recent `cleanup stale docs` commit (`7b6f55c`)
deleted the `doc/plans/*` and `doc/results/*` technique/detail docs but left ~9 live
source/config files pointing at them (rev1§6.1). The rev0 audit had flagged dangling doc refs;
this class is *narrowed but reopened* — fewer references, but several now dangle in real source
rather than in dev docs.

---

## 5. Beyond the audit — and was it a good direction? (Q5)

The implementation went well beyond patching the audit, in two good directions: **new
capability-model surface** and **deeper verification**.

**New mechanism (all judged sound and spec-faithful):**

- **Kernel IRQ-handler object (B-IRQ).** `kcore/src/irq.rs` adds verified `irq_bind`/
  `irq_unbind`/`destroy_irq` ops with an `irq_binding_refs` census term — the timer object's
  census twin, minus the armed list (delivery is by direct INTID lookup, so there is no chain
  to verify). The cap is integrated into `inc_ref`/`obj_unref`/`cap_consistent` term-for-term
  like the other object kinds and accounted as a boot-static device resource; the
  GIC→IRQ-object→notification delivery path is wired end-to-end and is exactly the
  device-interrupt receive side rev1§3.6/§7 need for the userspace console. **Good direction:**
  it fits the capability model (accounted, monotone, no new trusted seam beyond the existing
  delivery-shell pattern) and was the prerequisite that made M-9 possible.
- **Userspace console driver (C-M9).** Closes M-9 and S-8 (rev1§1.3, §1.5). Good direction;
  the ambient-authority hole is genuinely shut for the user-facing path.
- **Named-grant table / startup block (C1).** rev0 §3.2 recorded the hand-rolled
  `SD02`/`SH01`/`ST01` byte blocks as disclosed-deferred. rev1§5.1 specified a real startup
  block, and C1 delivered a unified `loader::startup` `b"EUS1"` codec (argv + env + a named-grant
  table with a kernel-cap-slot vs storage-handle discriminator), host-tested and fuzzed, with
  the hand-rolled blocks retired and standard names (`root`/`stdin`/`stdout`/`storage`/`time`)
  resolved through it (`tmp`/`cwd`/`env` reserved-but-not-emitted, matching the spec's
  deliberately-soft wording). Strong, spec-faithful work.
- **Version negotiation (C3).** rev0 §3.2 recorded multi-version negotiation as deferred.
  rev1§3.7/§8.3 now claim it implemented, and `ipc/src/session.rs` delivers it: `ConnectReq`
  offers a contiguous version range, `negotiate` selects the highest common version at the
  single admission point or refuses cleanly, the negotiated version is stamped into the
  existing header field and validated per-message — with `header.rs` untouched (no new trusted
  seam, ipc still 69/0), a fuzz target, a negotiation proptest, a negative control, and an
  end-to-end storaged↔shell witness. The deferred IDL/ABI half is honestly scoped.
- **Rename + ephemeral file-id + unlink-while-open (C2).** rev0 §3.2 recorded "no rename op;
  the overlay is path-keyed" as M2 debt. The overlay is now keyed on an ephemeral `FileId` with
  an id→path map (C2A); `Store::rename` + `WalOp::Rename` (tag-3) handle file and directory
  moves, with the Rename arm added to the **verified** structural decode (`s_payload_ok`/
  `e_payload_ok`) **with no new trusted seam**, and crash recovery reconstructing the move from
  the path-keyed record (C2B); unlink-while-open keeps an open handle working against the
  overlay and discards at flush if the id resolves to no path (C2C); and `Request::Rename` +
  the shell `mv` complete the path with a differential interleaving proptest against a
  path-keyed oracle (C2D). Faithful to rev1§4.9.

**Verification-surface growth (B7–B15).** Beyond what any single finding asked, the verified
surface gained `map_frame`, `set_priority`, the ready queue, `revoke_step` + the marker +
the derive guard, `grow_pool`, the shared `freelist` algorithm, the prolly partition core and
node decoder, the WAL structural decode, `RecoverReconstructs`, the completed `IpcReactor`
protocol, and `lowest_clear_bit` — taking the gate counts from the rev0 baseline (kcore 335,
cas 58) to **kcore 389 / cas 80 / ipc 69 / freelist 29 / urt 29**. This is the right direction
and is honestly bounded by the trusted-base ledger.

**One minor caveat (rev1§5 observation, low):** **dual UART writers.** With `debug-log`
enabled (default for dev images), `kernel/src/uart.rs` writes the physical PL011 at
`0x0900_0000` while the userspace console also writes the same device through its mapped MMIO
window — two unsynchronized writers to one UART, so kernel diagnostics and console output can
interleave. This is *not* an ambient-authority hole (the kernel path is kernel-internal
diagnostics, the gate it closed is the EL0 one), only a cosmetic/observability hazard; worth a
note, and naturally goes away when `debug-log` is off.

---

## 6. Other observations and comments (Q6)

### 6.1 Documentation drift (the actionable residue)

- **(medium) Dangling `doc/plans/*` & `doc/results/*` references in live source.** The
  `cleanup stale docs` commit deleted the technique/detail docs; ~9 live files still cite
  missing targets. Confirmed missing and referenced: `doc/plans/12_b11-detail.md`
  (`urt/src/lib.rs:15`, `urt/Cargo.toml:13`, `freelist/src/lib.rs:21`, `dma-pool/src/lib.rs:20`,
  `dma-pool/Cargo.toml:12`), `doc/plans/4_b4-detail.md` (`dma-pool/src/lib.rs:489`),
  `doc/plans/2_b2-detail.md` (`virtio-blk/tests/ring_props.rs:6`), `doc/plans/14_b13-detail.md`
  (`cas/src/prolly.rs:1371`), `doc/results/9_b-irq-c` (`scripts/m1-test.sh:20`),
  `doc/results/23_miri-test-optimization.md` (`CLAUDE.md:118`), `doc/results/4_b9c-findings.md`
  (`doc/guidelines/verus_trusted-base.md:193`). The substantive content the ledger pointer
  backs (the 503,070-state count, the trimmed constants, the negative controls) is correct;
  only the citations dangle. *Fix: re-point or remove these references.*
- **(low) Stale "until B12F" comments.** B12F has landed (the 30 s staleness, 8192 op-count,
  and 50 % watermark all ship in `StoreOptions::default`, and the staleness sweep is live), but
  many comments still call these "disabled/stubbed … until B12F" (`cas/src/store.rs:183,197,2409,2534,2566`,
  `user/storaged/src/main.rs:324`); the test name `staleness_disabled_by_default_never_flushes`
  refers to `test_opts()`, not `Default`, so the name misleads though the body is correct.
- **(low) ~6 bare `§` spec refs missed the rev1 conversion.** Genuine spec-section refs left
  in bare `§` form rather than `rev1§`, sitting alongside correctly-converted refs:
  `kernel/src/syscall.rs:640`, `cas/src/disk.rs:722-723`, `cas/src/store.rs:1150,1295,3172,6365`,
  `kcore/src/ready.rs:772`. (The `§6a/§6d` etc. in `kcore/src/test_store.rs` are local
  proof-obligation labels, not spec refs — correctly *not* converted.)
- **(low) `rev1§4.x` placeholder.** The S-11 blessing is cited as a literal `rev1§4.x`
  placeholder in `virtio-blk/src/lib.rs:109,390` and `virtio-blk/tests/driver.rs:78`; the real
  section is rev1§4.5.

### 6.2 Trusted-base ledger accuracy

The ledger (`doc/guidelines/verus_trusted-base.md`) now **exists** (it was absent at rev0) and
is substantively accurate: the **14**-seam tally (8 `external_body` + 6 `assume_specification`)
matches the code, the gate counts match ground truth, the `[verifying]→landed` transitions all
correspond to real artifacts, and the heap-arena-seam exclusion is correctly reasoned. Two nits:
(a) several seam-row **line citations are stale** — `checksum_ok` (cited :337, actual :342),
`wal_checksum_ok` (:1045 vs :1050), `is_boundary` (:1373 vs :1387), and worst, `CapSlot::empty`
(cited `cspace.rs:1226`, which is unrelated `ready_tail` code; the actual `const fn empty` is at
:167 and its `assume_specification` at :1596) — over-precise cites that can mislead a reader to
the wrong line; (b) the trusted IRQ delivery shell (`kernel/src/irq.rs`) lacks its own
enumerated ledger row with a named host test, though it is correctly folded under the same
"scheduler policy / asm shell stays trusted" umbrella as the timer tick shell.

### 6.3 Spec-text honesty (rev1 as a review target)

The rev1 spec was written to address the audit and largely **describes the code accurately**:
the inline `Status (C-M9)` notes (§2.7, §7), the four `[verifying]` transitions claimed "moved
in" this revision (§6.1(c) map, (d) priority gate, (e) structural decode + replay-equality), and
the §8.3 deferred-items scoping all check out against code/TLA, and the spec correctly **does
not** label the test-routed properties (GC mark sufficiency, prolly canonical form, multi-source
reactor dispatch) as "verified" — honoring its own §6.1 "no trust-routed property mistaken for a
mechanized one" discipline. S-1 (rev0's own false "implemented now" claim about guarded batches)
is corrected: rev1§8.3's "lands in this revision's work" is now *true*.

One soft over-claim:

- **(low) rev1§4.4 — the 64 MiB WAL "shipped configuration matches" claim.** `StoreOptions::default`
  carries a 64 MiB WAL, but `storaged` only ever *mounts* the mkfs-built image and `mount`
  overrides `wal_len` from the on-disk superblock (`cas/src/store.rs:1751`), and mkfs deliberately
  sets `wal_len = 1 MiB` (`mkfs/src/lib.rs:54`, with a documented heap-size rationale). So the
  **live** server runs a 1 MiB WAL with a 512 KiB watermark, not 64 MiB/32 MiB. The mechanism is
  correct and the numbers are explicitly tunable, so this is a spec-text imprecision, not a
  conformance bug: the sentence should say the WAL size comes from the image geometry (which the
  shipped image tunes down), or qualify 64 MiB as the in-memory default rather than the live figure.

The two verification-wiring gaps of rev1§2 also bear on honesty: §6.1 says the cap-side map,
priority gate, structural decode, and replay-equality are "mechanized" — which is true — but a
reader could over-read the prolly "partition" and aspace "top-up" verification as end-to-end
exec guarantees, when in fact the top-level composition lemmas are standalone. The spec/ledger do
not actually claim exec-composition for those, so this is a precision note rather than an
over-claim.

---

## 7. Scope and confidence

This was a **read-only** audit; no code or spec text was modified. Confidence is high: the
verification gates and the `CapRevocation`/`CommitProtocol`/`IpcReactor` TLC runs were
reproduced (the latter two and `CapRevocation` independently re-run by verifier agents), every
high/medium verdict survived an adversarial refutation pass (0 of 42 refuted), and the
load-bearing claims plus the dangling-doc set were spot-checked by hand against the source. The
residual uncertainty is concentrated where it always is — in the parts the system itself routes
to testing rather than proof (GC mark-set sufficiency, prolly canonical form, the multi-source
reactor dispatch, the unsafe Store/asm/UART shells) — and those routings are disclosed in the
ledger, not hidden.

## 8. Suggested follow-ups (logic/doc changes, out of audit scope)

Highest-value first: (1) re-point or delete the ~9 dangling `doc/plans/`·`doc/results/`
references and restore the `4_b9c-findings.md` pointer or inline its content (rev1§6.1); (2) land
the B10C aspace-top-up accounting test and invoke `lemma_grow_pool`/`lemma_partition_flatten` from
the exec path (or document them as standalone theorems) so the partition/top-up guarantees compose
end-to-end (rev1§2); (3) fix the rev1§4.4 WAL-default sentence to reflect the live image geometry
(rev1§6.3); (4) sweep the stale "until B12F" comments and the ~6 bare-`§`/`rev1§4.x` references
(rev1§6.1); (5) refresh the ledger's stale seam-row line citations and give the IRQ delivery shell
its own row (rev1§6.2); (6) optionally suppress the kernel diagnostic UART when the userspace
console is live, to avoid interleaved writers (rev1§5).
