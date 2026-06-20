# Plan — Addressing the rev0 Conformance & Verification Audit

Response plan for `doc/results/audit_rev0.md`. The audit found the system to be a
faithful, honestly-verified MVP with a small cluster of genuine deficiencies. This
plan turns every audit finding into a separately-implementable phase, ordered:

1. **Part A — Spec revision 1.** Bless `spec_rev1.md`, fixing the spec problems
   (false claims, ambiguities, gaps, verification over-claims, doc drift). Everything
   downstream keys off the blessed target, so this comes first.
2. **Part B — Code & verification remediation.** Highest-severity correctness and
   verification-theater findings first, sequenced by dependency.
3. **Part C — Spec-deferred gaps.** The rev0§8.3 disclosed-deferred items, last —
   except the **named-grant table**, pulled forward because the userspace console
   (M-9) depends on it.

Each phase lists: **Closes** (audit finding IDs), **Spec** (rev1§ touched),
**Touches** (crates/files), **Depends on**, **Work**, **Acceptance**, and
**Effort/Risk**. Finding IDs (I-*, T-*, M-*, S-*) and `§` references are the
audit's.

---

## Guiding principles

- **Spec rev1 is the new blessed target.** Where the audit found the *spec* wrong
  (false claim, ambiguity, gap, imprecise wording), rev1 edits the text. Where the
  audit found *code/verification* falling short of a *correct* spec claim, rev1 keeps
  the claim as the target and a Part-B phase brings the code into conformance —
  **unless** this plan decides to descope that work, in which case rev1 *softens* the
  claim to match reality. Every over-claim below carries an explicit
  **conform-or-soften** decision.
- **No logic change lands without its verification tier.** Per rev1§6 routing:
  decoders get fuzz targets, CAS/IPC/DMA/kernel chokepoints get Verus, servers get
  Loom/Shuttle, everything gets Miri+proptest. A fix and its proof/test ship together.
- **Phases are independently shippable behind existing seams** (handle/Store seam in
  `kcore`, the IPC crate, the DMA pool). Findings are grouped by subsystem to minimize
  cross-phase churn.
- **Honesty discipline is preserved.** Do not defuse tools to make checks pass;
  disclose every simplification on a recorded list; keep the trusted base enumerated
  in one ledger (Phase A5).
- **Reference-form migration.** New and amended sections become `rev1§`; code touched
  by a phase updates its `§` citations to `rev1§` for the sections that phase changes.
  A mechanical sweep migrates the rest (Phase A5).

**Baseline to preserve (regression gates).** `cargo verus verify -p kcore` → 335/0;
`-p cas --no-default-features` → 58/0; `-p dma-pool` → 26/0; the four `kcore`
`external_body` seams and three `assume_specification`s; TLC on `CommitProtocol`
(6886 states) and `IpcReactor`; the committed fuzz corpora + Miri replay. Any phase
that changes these must re-establish them at ≥ the prior numbers.

---

# Part A — Spec Revision 1

**Deliverable:** `doc/spec/spec_rev1.md` (copy of rev0 with the edits below; revision
banner bumped), `CLAUDE.md` updated to point at it and to require the `rev1§`
reference form, `doc/guidelines/verus.md` updated. Phases A1–A5 are edits to one
document and can be drafted together, but are separated by theme for review.

### Phase A1 — Truth-in-spec: correct false claims and verification over-claims

- **Goal:** the spec asserts nothing that is false at blessing time.
- **Closes:** S-1; the spec half of T-1/T-4; the rev0§5.4 and rev0§6.1(c)
  over-claims; the wrong-citation homes from audit §2.3.
- **Spec (rev1):**
  - **rev0§8.3** (S-1): the "transactional commits … the ref-table half — generation-
    guarded batches — is **implemented now**" clause is false. Reword to "specified;
    landing in this revision's work" (the implementation is Phase B5). Do **not** leave
    a present-tense "implemented now".
  - **rev0§5.4 / rev0§6.1(d)** (priority-ceiling check over-claim): the text says the
    spawn-time check that priority ≤ ceiling "are verified," but it is an unverified
    shell `if` (`kernel/src/syscall.rs:424,559`). **Decision: conform** (Phase B8 moves
    the gate into `kcore`). rev1 keeps the claim; add a forward note that the gate is
    verified as of the Phase-B8 work. (Soften only if B8 is descoped.)
  - **rev0§6.1(c)** (map/unmap asymmetry): wording quietly claims only the *unmap* half
    is proven; MAP is unverified shell (`kernel/src/syscall.rs:504`). **Decision:
    conform** (Phase B8 verifies MAP). rev1 strengthens the clause to claim the
    symmetric guarantee once B8 lands.
  - **rev0§6 / rev0§6.1(e)** (replay-equality): the prose is accurate ("remains the
    TLA+ model's alone"), but the TLA model does not actually check it (T-1). rev1
    keeps the wording and adds one sentence naming the `Recover`-reconstruction action
    property that Phase B7 introduces, so the claim becomes mechanized rather than
    aspirational.
  - **rev0§4.8** (T-4): keep the "stated explicitly as an axiom in the TLA+ model"
    requirement; Phase B7 makes it true. No text change beyond cross-referencing the
    new `ASSUME`.
  - **Wrong-citation homes** (audit §2.3): the nonexistent rev0§9/§10 and the
    rev0§4.6-for-syscall-decode mis-citations are comment fixes already done in the
    audit pass, but their *correct* homes need to exist — the syscall-ABI home is
    created in Phase A3 (S-7).
- **Depends on:** none.
- **Acceptance:** no present-tense claim in rev1 contradicts the code at blessing
  time, except claims explicitly tagged "conformed by Phase Bx" with that phase in
  this plan.
- **Effort/Risk:** S / low.

### Phase A2 — Resolve ambiguities and underspecifications

- **Goal:** every spec ambiguity the audit flagged has one decided meaning.
- **Closes:** S-2, S-3, S-4, S-5 (spec half), S-6, S-9 (spec half), the
  "fixed mechanisms" framing behind M-3…M-7.
- **Spec (rev1):**
  - **rev0§3.2** (S-2): adopt the code's better form — channel retype takes a
    creator-chosen `depth`, bytes derived as `Channel::bytes_for(depth)`. Replace
    "donate bytes, derive depth by truncating division" with the depth-first form.
  - **rev0§2.6** (S-3): record the robustness the code adds — `cntfrq` floored to 1,
    a below-baseline delta saturated to 0 — as normative (so it is not "drift").
  - **rev0§3.1 / rev0§2.5 / rev0§3.5** (S-4): the "per-session window quota" is
    enforced today as a **server-total budget admitted per session** (anti-drain holds:
    Σ granted ≤ budget, proven). **Decision:** rev1 specifies it as a server-total
    budget with an **optional per-session clamp**; the load-bearing property is the
    total bound. Phase B-storage may add the clamp (small); spec no longer implies a
    mandatory per-session maximum.
  - **rev0§2.4** (S-5): "short-TTL" is unenforceable as written. **Decision:** rev1
    adds a **server-imposed maximum TTL** (claim tickets are clamped to a server
    bound); Phase B5 implements the clamp. This makes "bound the exposure window"
    real.
  - **rev0§4.5** (S-6): replace the literal "single chokepoint" with "a single
    chokepoint for the superblock's own geometry, after which each *derived* field is
    checked with checked arithmetic against the now-trusted chunk tail" — describing
    the code's correct layered validation (`cas/src/store.rs:1033-1059`).
  - **rev0§4.4 + rev0§4.4 "Defaults"** (S-9 / M-3…M-7 framing): **Decision required —
    see Open Decisions.** Recommended: keep the *mechanisms* (per-ref soft bound,
    low/high watermarks, WAL flush-the-pinner, op-count secondary bound, staleness
    timer) **mandatory** (Phase B12 implements them) and relax only the *numbers* to a
    "recommended defaults" table that the shipped `StoreOptions::default` must match
    (Phase B12 aligns the code). The alternative — formally disclosing the collapsed
    single-budget policy as accepted MVP simplification — is the lower-effort path and
    is acceptable if B12 is descoped; rev1 must pick one and say so.
- **Depends on:** none.
- **Acceptance:** each S-item above resolves to one normative statement; the M-3…M-7
  conformance status (gap vs disclosed-simplification) is unambiguous in rev1.
- **Effort/Risk:** S–M / low.

### Phase A3 — Fill spec gaps (new normative sections)

- **Goal:** code behavior the audit found *correct but spec-less* gets a home.
- **Closes:** S-7, S-8, S-10, S-11.
- **Spec (rev1):**
  - **New rev1§2.7 "The syscall boundary"** (S-7): define the syscall opcode space and
    the decode discipline — unknown opcode → error (never crash), message-length and
    field validation against ground truth — explicitly generalizing the rev0§3.7 wire
    discipline to syscall numbers. This is the normative home `sysabi.rs` lacked and
    the correct target for the comments mis-citing rev0§4.6.
  - **rev0§7 (amended)** (S-8): scope the kernel debug-UART syscalls
    (`DebugPutc/Write/Getc`) as a **sanctioned M1 scaffold** for the kernel's own
    debug path, explicitly *not* a user-facing authority; state the end-state — the
    user-facing console is the userspace UART driver (M-9), and the debug syscalls are
    gated/removed for EL0 once it lands. This converts a standing ambient-authority
    hole into a disclosed, time-boxed carve-out.
  - **rev0§4.5 (amended)** (S-10): add a `format`/`mkfs` contract mirroring mount's
    "refuse, never panic" — `format` over an undersized or over-constrained device
    geometry returns an error, not a panic (the clean `Result`/`ExitCode::FAILURE`
    path). Phase B12 implements it.
  - **rev0§4.x note** (S-11): record that virtio-blk relies on the device as ground
    truth for its own geometry (no pre-check required); an optional defensive LBA
    bound is permitted, not mandated.
- **Depends on:** none.
- **Acceptance:** `sysabi.rs`, the debug-UART path, and `format` each cite a real
  rev1 section; no code behavior the audit blessed is orphaned.
- **Effort/Risk:** M / low.

### Phase A4 — Reconcile the verification-scope (rev1§6 / rev1§6.1 proof boundary)

- **Goal:** rev1§6.1's trusted/verified boundary describes the *target* state this
  plan delivers, with each seam tagged verified-or-trusted and, if verified, by which
  phase.
- **Closes:** the rev1§6 wording dependencies of T-1…T-5 and the Phase-B kernel/CAS
  verification work; sets up the per-phase "update rev1§6.1 on completion" hooks.
- **Spec (rev1):** rewrite rev1§6.1 as a checklist keyed to the trusted-base ledger
  (Phase A5). For each seam state: trusted-by-construction (a–e as today, adjusted),
  or verified-as-of-Phase-Bx. Specifically: (c) becomes symmetric (MAP+unmap, B8);
  (e) gains the TLA `Recover` action property (B7); add the fsync `ASSUME` (B7); note
  the tightened BLAKE3 seam (T-5, B7); note the priority-ceiling gate verified (B8).
- **Depends on:** the *decisions* of Phases B7, B8 (not their completion — rev1 states
  the target; each phase flips its own line from "target" to "delivered").
- **Acceptance:** rev1§6.1 and the trusted-base ledger agree line-for-line; no seam is
  both "trusted" in one place and "verified" in another.
- **Effort/Risk:** M / low.

### Phase A5 — Documentation infrastructure & the trusted-base ledger

- **Goal:** kill the dangling-reference debt and give "the trusted base is exactly …"
  a single source of truth.
- **Closes:** audit §1.2 (24 files → `doc/plans/*`, 16 → `doc/results/*`, all
  dangling), the absent trusted-base ledger, audit §8 follow-up (8); establishes the
  `rev1§` reference rule.
- **Work:**
  - Recreate the **trusted-base ledger** (the absent `doc/guidelines/verus_trusted-base.md`
    successor) from audit §4.1 — the four `kcore` `external_body` items + three
    `assume_specification`s, the two CAS BLAKE3 seams, the `urt` debug-assert hatch —
    as the authoritative enumeration `verus.md` and CLAUDE.md point at.
  - Triage the dangling `doc/plans/*` / `doc/results/*` references: either restore the
    referenced artifact or update the citation. (This plan file is itself the first
    `doc/plans/` artifact; the audit is the first `doc/results/` artifact.)
  - Update `doc/guidelines/verus.md` (its "source of truth for CLAUDE.md's trusted-base
    claim" pointer) and add the trusted-base claim back to `CLAUDE.md`.
  - Update `CLAUDE.md`: spec pointer → `spec_rev1.md`; reference-form rule → `rev1§`.
  - Migrate genuine spec `§` references → `rev1§` (mechanical, sparing the dev-doc/plan
    `§` baggage the audit §1.1 warns against).
- **Depends on:** A1–A4 (ledger reflects the rev1 boundary).
- **Acceptance:** no dangling `doc/plans/*` or `doc/results/*` reference remains; the
  trusted-base ledger exists and matches rev1§6.1; `rg '§'` shows genuine spec refs in
  `rev1§` form.
- **Effort/Risk:** M / low.

---

# Part B — Code & Verification remediation

Ordered by recommended execution (severity + dependency). **Wave** tags in the
execution summary show what parallelizes. Every Part-B phase assumes Part A is
blessed (it defines the target each conforms to).

### Phase B1 — Storage-server authority: the `stat-store` right + statfs gate

- **Closes:** I-1 [high]; storage-server rights-lattice proptest (audit §4.2 [low]);
  S-5 TTL clamp (small, adjacent).
- **Spec:** rev1§2.3 (rights set), rev1§2.4 (ticket TTL).
- **Touches:** `storage-server/src/lib.rs` (`R_ALL` at :44, `Statfs` handler at :616,
  delegation/attenuation helpers, `MintTicket` at :537).
- **Depends on:** A1/A2 (rights set + TTL decision).
- **Work:**
  - Add a `stat-store` bit to the rights set (widen `R_ALL`); gate the `Statfs`
    handler on it (deny-by-default — a zero-rights handle must be refused).
  - Ensure delegation/attenuation helpers **strip** `stat-store` by default and that
    its scope ignores the subtree (per rev1§2.3) while still stripping/dying on
    generation bump like any right.
  - Init grants `stat-store` only to the shell and maintenance holders (rev1§2.3).
  - **S-5:** clamp `MintTicket` `ttl_nanos` to a server maximum.
  - **Test tier:** proptest over monotone attenuation across arbitrary derivation
    chains (no chain ever *gains* a right; `stat-store` strips correctly); regression
    test that `statfs` without the bit is refused.
- **Acceptance:** `statfs` is refused on any handle lacking `stat-store`; attenuation
  proptest passes; ticket TTL is bounded.
- **Effort/Risk:** S / low. High value (closes the one real authority bug).

### Phase B2 — virtio-blk: completion-poll correctness + driver test tier

- **Closes:** I-4 [medium]; virtio-blk proptest/Loom/Miri gap (audit §4.2 [medium]);
  S-11 (optional pre-check).
- **Spec:** rev1§2.5 (DMA), rev1§4.x (S-11 note).
- **Touches:** `virtio-blk/src/lib.rs` (`complete()` at :256, spin at :263),
  `dma-pool/src/lib.rs` (`bytes()` at :1277).
- **Depends on:** none (independent of B4, though both touch dma-pool).
- **Work:**
  - Replace the non-volatile used-ring load in `complete()` with a
    `read_volatile`/atomic-acquire read so the spin loop can observe the device update;
    keep `spin_loop()` as the pause hint. (The load is currently loop-invariant and
    legally hoistable — a real hazard on the QEMU target.)
  - Add the **driver test tier** rev1§6 requires: proptest over ring arithmetic,
    descriptor-chain construction, and `u16` index wrap; a Miri target. Make the
    `fake` device able to complete **asynchronously** so `complete()`'s poll executes
    *as a loop* (the gap that let I-4 escape).
  - **S-11 (optional):** defensive LBA-vs-capacity pre-check.
- **Acceptance:** poll loop provably observes a delayed completion in the async-fake
  test; ring/descriptor proptests pass under Miri.
- **Effort/Risk:** S (fix) + M (tests) / low.

### Phase B3 — Loader / ELF page-rounding hardening

- **Closes:** I-5 [medium]; `loader::prepare` host model + fuzz gap (audit §4.2).
- **Spec:** rev1§5 (spawn), rev1§3.7 decode discipline.
- **Touches:** `loader/src/spawn.rs` (`prepare`, `va_end` at :58), `loader/src/elf.rs`
  (the `vaddr+memsz` check at :124).
- **Depends on:** none.
- **Work:**
  - Use checked arithmetic for the VA page-rounding (`va_end = (vaddr+memsz+PAGE-1) &
    !(PAGE-1)` overflows on `vaddr+memsz` within `PAGE-1` of `u64::MAX`; the page-count
    subtraction can underflow in release). Reject adversarial segments cleanly.
  - Tighten `elf.rs` so the rounding consumer can't be handed a region that passes
    `parse` but overflows `prepare` (currently `== u64::MAX` is permitted).
  - Add a **host model + cargo-fuzz target** for `prepare`'s rounding (today aarch64-
    only and unfuzzed); promote any crash to `loader/tests/fuzz_regressions.rs` (where
    ELF-1 already lives).
- **Acceptance:** fuzzing `prepare` over adversarial images yields refuse-not-crash;
  Miri replay clean.
- **Effort/Risk:** S / low.

### Phase B4 — DMA-pool wrapper: soundness + verification

- **Closes:** DMA-pool public-wrapper soundness hole + unverified glue (audit §4.2
  [medium]; UB hazard → treat as high).
- **Spec:** rev1§2.5 (DMA), rev1§6.1 (DMA-pool seam).
- **Touches:** `dma-pool/src/lib.rs` (`DmaPool<B>` :1260, `bytes/bytes_mut`
  :1277/:1284, the `FreeList::free/alloc` call sites).
- **Depends on:** none (coordinate with B2; same crate).
- **Work:**
  - Close the **provenance hole**: `bytes()/bytes_mut()` build raw slices
    `from_raw_parts(cpu_base().add(buf.offset), buf.len)` with no check that `buf`
    came from this pool — a `DmaBuf` (Copy, private fields) from a larger pool used
    against a smaller pool is OOB UB. Add a pool-identity/extent check (tagged
    `DmaBuf`, or validate `offset+len` against this pool's arena).
  - Restore the runtime backstop for the `MAX_FREE_RANGES` overflow that was demoted
    to a Verus-only precondition; discharge `FreeList::free/alloc` preconditions
    (`spec_nfree() < N`, `off+n <= len`) in the wrapper or add runtime checks.
  - Extend Verus (or Miri+proptest) coverage from `FreeList<N>` to the `DmaPool<B>`
    wrapper drivers actually use.
- **Acceptance:** a cross-pool `DmaBuf` is rejected (test); wrapper preconditions
  discharged or runtime-guarded; `cargo verus verify -p dma-pool` ≥ 26/0.
- **Effort/Risk:** M / medium (touches the only place PAs are visible).

### Phase B5 — Storage protocol: guarded ref-table batches + tags over the wire

- **Closes:** I-2 [high] / S-1; M-8 [medium].
- **Spec:** rev1§4.7 (edit version, guarded batch, tags), rev1§8.3 (S-1 reword).
- **Touches:** `cas/src/disk.rs` (`RefEntry` :609 — add `edit_version`; encode/decode
  at :650/:689), `cas/src/store.rs` (`tag` :1225; bump edit-version on every committed
  ref-entry mutation), `storage-server/src/lib.rs` (`Request` enum :91; enumerate
  replies; new guarded-batch and tag ops).
- **Depends on:** A1/A2 (S-1, rev0§4.7 semantics).
- **Work:**
  - **Per-ref edit version (I-2):** add `edit_version: u64` to `RefEntry`, advancing on
    every committed mutation of the ref's entries (head moves, snapshot rows, tags),
    distinct from the rev1§2.2 revocation `generation`. Return it from
    `SnapInfo`/`ListSnapshots`/enumerate.
  - **Guarded batch (I-2):** add a `Request::Apply { handle, expected_version, edits }`
    variant applied all-or-nothing within one commit iff the version matches, else
    failing with the current version so the caller re-reads. This is the documented
    remedy for the retention read-then-act race.
  - **Tags over the wire (M-8):** add create/delete/list-tag ops to `Request`; route
    them to `Store::tag()` (today an in-process backdoor used only by tests); enforce
    the `Pinned`-on-tagged-snapshot-delete semantics (rev1§4.7) over a session.
  - **Test tier:** crash-injection + proptest that a stale `expected_version` is
    rejected and a matching one applies atomically; tag pin/unpin round-trips.
  - **Verus:** keep the ref-table TLV codec proofs (`decode_raw/encode_raw`) extended
    to the new field; the on-disk format change is a decoder, so it stays in the
    verified+fuzzed surface (rev1§3.7/§6).
- **Acceptance:** guarded batch demonstrably closes the read-then-act race in a test;
  tags reachable over a session; rev1§8.3 no longer claims "implemented now" falsely
  (now actually implemented); CAS decode verify ≥ 58/0.
- **Effort/Risk:** M–L / medium (on-disk format addition → migration discipline).

### Phase B6 — GC correctness: resurrection mechanism + bounded mark + fuzz

- **Closes:** I-3 [high]; GC mark/sweep unverified + stack-overflow (audit §4.2
  [medium]); GC paths unfuzzed.
- **Spec:** rev1§4.6 (resurrection fix as always-present mechanism), rev1§4.8
  (detect-on-read, never fault).
- **Touches:** `cas/src/store.rs` (`ChunkStore::put` :271/`contains_key` :273; `gc`
  :1640; the birth-generation "live by fiat" filter :1662 and its admitting comment
  :1647), `cas/src/gc.rs` (`mark` :21 — unbounded recursion).
- **Depends on:** none (independent of concurrent-GC, which stays deferred — Phase C4).
- **Work:**
  - **Install the resurrection mechanism (I-3):** during sweep, a dedup lookup that
    hits an unmarked/condemned chunk is treated as a miss, so the chunk is rewritten
    under the same hash, replacing the index entry — confining all GC/mutator
    interaction to one point. `put` must consult the mark/condemned set during sweep.
    This is the spec's named mechanism; install it even though it is benign under
    today's synchronous GC, and either remove the now-vacuous birth-generation filter
    or make it meaningful. **Not on the recorded MVP-simplification list today** — so
    either implement (preferred) or add it to that list with rationale.
  - **Bound the mark walk:** `gc::mark` recurses on directory children with no depth
    bound → stack overflow (a crash *inside* the storage server) on adversarial/deep
    trees, contra rev1§4.8. Convert to an explicit work-stack or impose a checked depth
    bound that refuses rather than faults.
  - **Fuzz + verify:** add a cargo-fuzz target over GC mark/sweep on adversarial tree
    shapes; add `requires/ensures` (or at minimum strengthen the existing mark-set
    sufficiency oracle at `gc.rs:85`) toward a proof of mark-set sufficiency.
- **Acceptance:** deep-tree fuzz input yields refuse-not-crash; resurrection mechanism
  exercised by a test that condemns-then-rewrites a hash; mark-set sufficiency oracle
  still green.
- **Effort/Risk:** M / medium.

### Phase B7 — Storage commit/recovery verification: close the theater

- **Closes:** T-1 [high], T-2 [medium], T-4 [medium], T-5 [low]; `mount()/commit()`
  orchestration unverified (audit §4.2 [medium]).
- **Spec:** rev1§6 / rev1§6.1(e) (replay-equality, now mechanized), rev1§4.8 (fsync
  axiom).
- **Touches:** `tla/commit_protocol/CommitProtocol.tla` (`Recover` :192,
  `AckedWritesRecoverable`, the `overlay` var, no `AXIOM` today), `cas/src/store.rs`
  (`lemma_gap_freedom`/`laid_out` :763; `mount` :957, `commit`),
  `cas/src/disk.rs` (`decode_record`/`decode_payload` :531, `checksum_ok` :338),
  `tools/tla/*`.
- **Depends on:** A1/A4 (rev1§6.1 wording). Independent of B5/B6.
- **Work:**
  - **T-1 — the headline gap.** Add a `Recover`-reconstruction **action property**
    relating `overlay'` to `(durableRoots, walLog)` across `Recover`, so replay-
    *equality* is actually checked. The current `AckedWritesRecoverable` constrains
    only the durable substrate and never references `overlay` — a no-op `Recover`
    passes all five invariants (confirmed empirically at 5358 states). The new property
    must **fail** under a no-op `Recover` (a negative control, in the project's
    established style). Re-run TLC; record the state count.
  - **T-4 — label the axiom.** Add an explicit `ASSUME`/`AXIOM` in `CommitProtocol.tla`
    stating `fsync = fsync` (the one trusted storage-layer assumption), per rev1§4.8,
    instead of encoding it only operationally.
  - **T-2 — connect or retire `lemma_gap_freedom`.** It proves a true statement with
    zero call sites on an undischarged `laid_out` hypothesis (the authors' own comment
    at :748 admits it is "not enforced at one site Verus sees"). Either discharge
    `laid_out` at the `mount`/`commit` site that builds `records`, wiring the lemma to
    the running code, or remove it and document that the in-code guarantee rests on the
    crash-injection proptest + TLA (no dead proofs).
  - **T-5 — tighten the BLAKE3 seam.** `wal_content_ok`/`checksum_ok` are `external_body`
    over *both* interpreted BLAKE3 (legitimately trusted) *and* the pure bounded
    structural decode (`decode_record → decode_payload`). Split the content decode out
    and verify it like `decode_frame`/`decode_checked_fields`, leaving only BLAKE3
    trusted — shrinking the trusted surface.
  - **mount/commit glue (§4.2):** connect the verified decision cores (`pick_survivor`,
    `commit_target`, `advance_head`, `replay_bound`) to the sequencing in `mount`/
    `commit` so the running recovery code is tied to the proved decisions (at minimum
    `requires/ensures` on the orchestration boundary).
- **Acceptance:** TLC fails on a no-op `Recover` and passes on the real one; the fsync
  `ASSUME` is present; `lemma_gap_freedom` is either live (discharged) or gone; the
  BLAKE3 seam covers only BLAKE3; `cargo verus verify -p cas --no-default-features`
  ≥ 58/0; update the trusted-base ledger + rev1§6.1.
- **Effort/Risk:** M–L / medium. This is the single most important verification gap.

### Phase B8 — Kernel: extend the verified surface into the syscall shell

- **Closes:** frame-MAP unverified vs verified unmap (audit §4.2 [medium]); spawn-time
  priority-ceiling *check* unverified (audit §4.2 [medium]); ready-queue list logic
  unverified (audit §4.2 [low–medium]). Conforms rev1§5.4 and rev1§6.1(c).
- **Spec:** rev1§5.4, rev1§6.1(c)/(d).
- **Touches:** `kernel/src/syscall.rs` (raw map `mapping: Some` :504 + `refs += 1`;
  priority gate `if prio > max_prio` :424,:559), `kernel/src/thread.rs` (ready-queue
  `enqueue` :79 / `top_ready` :106 / `dequeue` / `unqueue_ready`), `kcore` (new verified
  entry points).
- **Depends on:** A4 (target wording). Independent of B9/B10.
- **Work:**
  - **MAP side:** move the frame-map bookkeeping (set `mapping`, bump refs) behind a
    `kcore` operation verified over object state, symmetric to the already-verified
    `cspace::delete` unmap path that drives `aspace_unmap`/`unref_aspace`. Then rev1§6.1(c)
    can claim the symmetric guarantee.
  - **Priority-ceiling gate:** move the `prio > max_prio` refusal into `kcore` as a
    verified gate (the cap-carried ceiling and its monotone attenuation are already
    verified in `cspace::derive`), satisfying rev1§5.4's "the spawn-time check … are
    verified."
  - **Ready-queue list surgery:** move `enqueue/dequeue/unqueue_ready/top_ready` (an
    intrusive linked list + priority bitmap, *the same shape* as the verified
    notification waiter queue and timer armed list) into `kcore` with
    `requires/ensures`. The asm context switch stays trusted (rev1§6.1(d)).
- **Acceptance:** `cargo verus verify -p kcore` ≥ 335/0 (higher, with the new entry
  points); the three shell sites now call verified `kcore` code; trusted-base ledger +
  rev1§6.1 updated to flip these from trusted to verified.
- **Effort/Risk:** M / medium (proof engineering, but patterns exist in `kcore`).

### Phase B9 — Kernel: preemptible / restartable revoke

- **Closes:** M-1 [medium].
- **Spec:** rev1§2.2 (preemptible, restartable walk), rev1§5.4 (preemptive scheduler).
- **Touches:** `kcore/src/cspace.rs` (`revoke` :10046 — straight-line run-to-completion
  `while`), `kernel/src/exceptions.rs`, the scheduler tick path. The kernel currently
  runs non-preemptibly with IRQs masked at EL1 (audit §2.6 confirms this), so the walk
  monopolizes the CPU — unbounded interrupt latency.
- **Depends on:** none structurally, but the largest kernel change; sequence after B8
  so the verified-surface work isn't churned by it.
- **Work:**
  - Introduce a preemption point / restart entry in the descendant-deletion walk so a
    large subtree yields and resumes, bounding interrupt latency and not defeating the
    preemptive scheduler. The Verus proof currently establishes *termination*, not
    *preemptibility* — preserve termination while adding a re-entrant restart state.
  - Decide the mechanism: either a bounded per-tick work quantum with a persisted
    cursor, or a kernel-side continuation re-armed on the next tick. Re-verify the
    walk's well-formedness and termination across restart.
- **Acceptance:** a synthetic deep CDT subtree revoke does not block timer interrupts
  beyond one quantum; `kcore` revoke proofs (descendant-deletion + `cspace_wf`) still
  pass across the restart refactor.
- **Effort/Risk:** L / high (touches the kernel's non-preemption assumption; the
  hardest kernel item — schedule with care).

### Phase B10 — Kernel: aspace pool top-up

- **Closes:** M-2 [medium].
- **Spec:** rev1§2.5 (pool-at-creation "accepts top-ups"; the three-part error story).
- **Touches:** `kernel/src/aspace.rs` (`AspaceObj.pool_pages` set-once at retype
  :62-69), the syscall table (no top-up variant), `kcore` aspace data.
- **Depends on:** none. Self-contained.
- **Work:** add a top-up syscall variant that grows an aspace's intermediate-page-table
  pool from donated untyped; implement the full three-part story (NEED_MEMORY →
  top-up → return-at-teardown) rather than today's two parts (an exhausted pool returns
  NEED_MEMORY permanently). Honor accounting (top-up funded by the caller's untyped).
- **Acceptance:** an aspace that exhausts its pool can be topped up and continue
  mapping; teardown returns the (grown) pool; decode discipline for the new opcode
  per rev1§2.7.
- **Effort/Risk:** M / medium.

### Phase B-IRQ — Kernel IRQ-handler cap, GIC SPI routing, and IRQ syscalls

- **Closes:** the rev0§1 "IRQ handlers" kernel object — **mandated but entirely
  absent**. The audit folds this into M-9; it is in fact M-9's long pole and a
  prerequisite for it. Also enables interrupt-driven drivers generally (e.g.,
  retiring the virtio-blk poll, cf. B2/I-4 — bonus, not a dependency).
- **Spec:** rev1§1 (IRQ-handler object in the kernel object set), rev1§3.6 ("IRQ
  handlers bind identically" to a notification), rev1§2.7 (new IRQ syscalls' decode
  discipline).
- **Touches:** `kcore/src/cspace.rs` (new `CapKind::Irq` variant → re-verify
  `cspace_wf`, `derive`, the teardown SCC, `refcount_sound`), `kernel/src/gic.rs`
  (distributor SPI enable/route — today only the redistributor vtimer PPI),
  `kernel/src/exceptions.rs` (`handle_el0_irq` non-timer branch), `kernel/src/syscall.rs`
  (new IRQ ops), `kernel/src/main.rs` (grant init the PL011 MMIO frame + IRQ-handler
  cap to delegate).
- **Depends on:** none structurally. Sequence in the kernel wave, after B8, so it does
  not churn the freshly-verified surface.
- **Verification finding that motivates this phase** (from the IRQ-path investigation):
  - *The delivery primitive exists and is verified.* The ARM vtimer interrupt
    (PPI 27) is taken in `handle_el0_irq` → `timer::check_expired` → signals the timer
    object's **bound notification**; the arm/disarm + binding refcount census is
    verified in `kcore::timer`. "Hardware interrupt → userspace notification" already
    works end-to-end — it is just hardwired to the timer. This de-risks M-9's design.
  - *Device MMIO is already a frame cap.* init already holds device-MMIO frame caps
    (virtio-mmio at `0x0a00_0000`, the RTC region at `0x0901_0000`) and `storaged`
    already maps device MMIO from one; granting the PL011 region (`0x0900_0000`) is a
    small addition reusing this mechanism.
  - *But the general device-IRQ path is unbuilt.* Concretely missing: (1) no
    `CapKind::Irq` — the rev0§1 IRQ-handler object does not exist (`gic.rs` explicitly
    defers it: "Userspace IRQ-handler caps … are introduced by the userspace drivers");
    (2) `gic::init` enables only the redistributor PPI for the vtimer — no
    `GICD_ISENABLER`/`GICD_IROUTER` SPI enable+route (the PL011 RX is SPI 1 → INTID 33
    on QEMU virt); (3) `handle_el0_irq`'s non-timer branch just EOIs and **drops** the
    interrupt — it neither signals a bound notification nor masks the line; (4) no
    `IrqBind`/`IrqAck` syscalls (only `TimerArm`/`ChanBind`/`ThreadBind` exist);
    (5) corollary: every current driver **polls** (virtio-blk's used-ring spin, I-4),
    so there is no device-interrupt code path to copy beyond the timer.
- **Work:**
  - Add `CapKind::Irq(intid)` (held by init, derived/attenuated like other caps) and
    re-establish the `kcore` proofs over the widened cap set — the new binding
    participates in `refcount_sound` exactly as the timer's `notif` binding does, which
    is the proof template.
  - Extend `gic::init` to enable + affinity-route device SPIs; add per-IRQ
    enable/mask/EOI helpers.
  - Rework `handle_el0_irq`: on a bound device INTID, **signal the bound notification
    `(notif, bits)` and mask the source** (do not EOI), so a level-triggered line does
    not storm before the driver services it.
  - Add `IrqBind(irq_cap, notif, bits)` and `IrqAck(irq_cap)` syscalls (the seL4
    pattern: ack unmasks/EOIs after servicing), gated by the IRQ-handler cap; decode
    per rev1§2.7.
  - Grant init the PL011 MMIO frame + the PL011 IRQ-handler cap so it can delegate both
    to the console driver (C-M9).
  - **Test/verify tier:** `kcore` re-verifies at ≥ the new total with the widened cap;
    a QEMU integration test that a bound device IRQ wakes a waiting EL0 thread via its
    notification, which then acks to re-arm.
- **Acceptance:** a userspace thread binds a device IRQ to a notification, blocks on
  it, is woken by the hardware interrupt, and acks to re-arm; `cargo verus verify -p
  kcore` green at ≥ the new total; trusted-base ledger + rev1§6.1 record the new
  verified object.
- **Effort/Risk:** L / high (new *verified* kernel object + GIC work; the console
  track's long pole).

### Phase B11 — `urt` heap allocator verification

- **Closes:** the `urt` `GlobalAlloc` wholly unverified (audit §4.2 [high]) — the
  largest single block of unverified `unsafe` in a "verified" crate.
- **Spec:** rev1§6 (Verus tier covers the userspace runtime).
- **Touches:** `urt/src/lib.rs` (`Heap<N>` :48, `alloc` :88, `dealloc` :132 —
  first-fit free-list traversal, alignment padding, block splitting, two-sided
  address-ordered coalescing over raw `*mut Block`).
- **Depends on:** none. Independent. High value.
- **Work:** bring the allocator up to the crate's own verification bar. Preferred:
  Verus proofs over the free-list invariants (no overlap, coalescing preserves
  ordering, split conserves bytes, alignment correctness). Minimum acceptable: a Miri
  target + proptest exercising the unsafe pointer arithmetic across adversarial
  alloc/free/realloc sequences (today only two happy-path tests). State which bar is
  met and record it in the trusted-base ledger.
- **Acceptance:** allocator covered by Verus or Miri+proptest; no UB under Miri across
  randomized sequences; ledger updated.
- **Effort/Risk:** M–L / medium.

### Phase B12 — Memtable / flush-policy conformance + format contract

- **Closes:** M-3…M-6 [medium], M-7; S-9 (code half), S-10 (code half).
- **Spec:** rev1§4.4 (mechanisms + recommended defaults — per the A2 decision),
  rev1§4.5 (format contract).
- **Touches:** `cas/src/store.rs` (`StoreOptions`/`overlay_budget` :110; size-pressure
  :1352; WAL-pressure :1325; `format` :903 `assert!`), `mkfs/src/main.rs` :74.
- **Depends on:** **A2 decision** (mechanisms mandatory vs disclosed-simplification).
  If A2 chose disclosure, this phase shrinks to documentation + S-10.
- **Work (if A2 keeps mechanisms mandatory):**
  - **M-3:** per-ref soft bound under the global budget (so one ref can't consume the
    whole budget).
  - **M-4:** low/high watermark split for size pressure (flush biggest offenders at the
    low watermark) instead of one hard threshold → `sync_all()`.
  - **M-5:** WAL-pressure flush-the-pinner at the 50% watermark with a per-ref
    oldest-WAL-position sort key, instead of flush-everything only at a full WAL.
  - **M-6:** operation-count secondary bound + timer/staleness trigger.
  - **M-7:** re-chunk the affected neighborhood only (rev1§4.3) instead of whole dirty
    files (changes write-amplification toward the spec's model).
  - **S-9:** align `StoreOptions::default` (and mkfs overrides) to the rev1
    recommended-defaults table.
  - **S-10:** `format`/`mkfs` return a clean `Result`/`ExitCode::FAILURE` on undersized
    device instead of `assert!`-panicking.
- **Acceptance:** per-ref containment demonstrated (one ref cannot starve another);
  WAL/size watermarks exercised by tests; format refuses-not-panics on a tiny device;
  shipped defaults match rev1. If disclosed instead: the simplification list at
  `cas/src/store.rs:20-32` covers all of M-3…M-7 and rev1§4.4 reflects it.
- **Effort/Risk:** M–L / medium (or S if disclosure path).

### Phase B13 — Prolly-tree canonical-form verification

- **Closes:** prolly-tree shape / canonical-form unverified (audit §4.2 [medium]).
- **Spec:** rev1§4.1 (history-independent canonical form), rev1§6 (chunker/prolly
  baseline).
- **Touches:** `cas/src/prolly.rs` (`is_boundary`, `build_level`, `Dir::save`,
  `load_node` — today plain Rust; only the single-entry TLV codec is verified).
- **Depends on:** none.
- **Work:** prove (Verus) or, if a full proof is out of reach, substantially
  strengthen the proptest, the central rev1§4.1 property: the same logical contents
  produce the same tree regardless of edit order. The split rule (`is_boundary`) and
  level construction are the load-bearing pieces; the headline property deserves more
  than sampling.
- **Acceptance:** canonical-form property proven, or a documented strong proptest
  (many shapes × edit orders) with the decode-then-reencode oracle, both green under
  Miri.
- **Effort/Risk:** M–L / medium (Verus over tree shape is non-trivial).

### Phase B14 — IPC reactor verification + TLA completion

- **Closes:** T-3 [medium]; IPC reactor/endpoint no proof (audit §4.2 [low]); Loom
  docstring over-claim + Loom-vs-Shuttle note (audit §4.3); S-12.
- **Spec:** rev1§3.6 (bind, poll once, then wait), rev1§3.3 (backpressure).
- **Touches:** `tla/ipc_reactor/IpcReactor.tla` (`Send` :69 treats binding as always
  present; no bind/register/poll-once action; titular "backpressure"),
  `ipc/src/` (reactor dispatch, endpoint cap-marshalling), `urt/src/time.rs:585-631`
  (Loom docstring), the IpcReactor `.cfg`.
- **Depends on:** none.
- **Work:**
  - **T-3:** add a bind/register + poll-once-self-signal action to the TLA model so the
    **send-before-bind hazard and its poll-once mitigation** are modeled in TLA+, not
    only in the Loom fragment. Either model real backpressure (a `FULL` return, a
    writable-signal, the symmetric writable lost-wakeup) or retitle the spec honestly
    to "wait-side lost-wakeup" and drop the "backpressure" claim. Keep the existing
    wait-side negative control.
  - **Reactor proof/testing:** raise the reactor's sequential dispatch (bit allocation,
    pending drain, lowest-bit scan) and endpoint cap-marshalling above unit+Loom — at
    least a proptest/Shuttle model of the dispatch invariants.
  - **Audit §4.3 doc fixes:** correct the Loom docstring's "every C11-permitted
    interleaving and reordering" over-claim (Loom does not faithfully model Relaxed;
    the seqlock conclusion holds via the explicit Acquire fence — say exactly that);
    note that Loom adds little over Shuttle for the atomics-free IPC model.
  - **S-12:** add a comment in the IpcReactor `.cfg`/spec pinning the
    `CHECK_DEADLOCK FALSE` ↔ `EventuallyDelivered` dependency (dropping the liveness
    PROPERTY would let a true deadlock pass silently).
- **Acceptance:** removing the new poll-once self-signal makes the TLA model reachable-
  deadlock (negative control); reactor dispatch invariants covered; docstrings match
  what the tools actually prove.
- **Effort/Risk:** M / medium.

### Phase B15 — Baseline test backfill

- **Closes:** mkfs directory-walk + user-binary test gaps (audit §4.2 [low]).
- **Spec:** rev1§6 (Miri+proptest baseline — everything).
- **Touches:** `mkfs/` (one happy-path test today), `user/*` (validated only by QEMU
  boot output). (loader::prepare host model is in B3.)
- **Depends on:** none; can trail the others or fold into subsystem phases.
- **Work:** add proptest/unit coverage for the mkfs directory walk; add host-testable
  logic tests for the user binaries' non-I/O logic where feasible; keep the QEMU boot
  smoke as the integration gate.
- **Acceptance:** mkfs walk has proptest coverage; user-binary logic has at least
  smoke-level host tests beyond boot output.
- **Effort/Risk:** S–M / low.

---

# Part C — Spec-deferred gaps (rev0§8.3)

The disclosed-deferred items from audit §3.2. Last by default — **except the
named-grant table (C1), pulled forward** because the high-severity userspace console
(M-9) depends on it. The debug-UART scaffold (rev1§7, Phase A3/S-8) keeps the system
usable until M-9 lands, so there is no correctness emergency forcing the console
ahead of Part B.

### Phase C1 — Named-grant table / argv / env / standard names  *(pulled forward)*

- **Closes:** audit §3.2 named-grant-table gap [confirmed-deferred]; unblocks M-9;
  cleans up the hand-rolled startup blocks.
- **Spec:** rev1§5.1 (startup block, named-grant table, standard names), rev1§8.3
  (mark delivered).
- **Touches:** `loader/`, `user/init`, `user/shell`, `user/storaged` (today hand-roll
  fixed-layout byte blocks "SD02"/"SH01"/"ST01" carrying only magic+mode+time-VA; only
  `time` is delivered as a standard grant).
- **Depends on:** A3 (rev1§5.1 format). Foundational; schedule before M-9.
- **Work:** define and implement a real named-grant-table format in the startup block:
  a discriminated table mapping names → (cspace slot | storage handle), carrying argv
  and env. Deliver the standard names (`root`, `tmp`, `storage`, `time`, and —
  deliberately split — `stdin`/`stdout`). Replace the hand-rolled blocks in init/shell/
  storaged.
- **Acceptance:** init/shell/storaged consume the named-grant table (no magic byte
  blocks); standard names resolve; QEMU boot still green.
- **Effort/Risk:** M–L / medium.

### Phase C-M9 — Userspace console UART driver  *(high; depends on B-IRQ + C1)*

- **Closes:** M-9 [high]; resolves S-8 (retires the user-facing ambient debug path).
- **Spec:** rev1§7 (userspace UART driver holding PL011 IRQ/MMIO caps; "console cap" =
  a channel to it), rev1§3.6 (IRQ → notification delivery).
- **Touches:** new `user/console` driver binary; `kernel/src/uart.rs` (the in-kernel
  PL011 path — to be demoted to debug-only); `user/shell` (console I/O via the console
  channel instead of `sys::debug_getc/putc/write`); init (grant PL011 IRQ + MMIO caps
  to the driver, wire the console channel into the shell's startup block under
  `stdin`/`stdout`).
- **Depends on:** **C1** (deliver the console cap under standard names) **and B-IRQ**
  (the kernel device-IRQ→notification path). The IRQ-path investigation confirmed this
  is a hard prerequisite, not a wire-up: a console driver needs RX interrupts (polling
  a console for input is not viable the way the block driver polls a completion), and
  today device IRQs reach no one — only the timer is wired, and all drivers poll. The
  delivery primitive is proven by the timer and device MMIO is already a frame cap (see
  B-IRQ), so the design is de-risked, but the kernel IRQ object/syscalls must be built
  first (B-IRQ).
- **Work:**
  - Write a userspace PL011 driver holding the IRQ-handler cap + MMIO frame cap (both
    delegated by init, granted in B-IRQ), delivering RX as events via `IrqBind`/`IrqAck`
    (rev1§3.6) and accepting TX over its channel.
  - Make the "console cap" a channel to the driver; init grants it to the shell under
    `stdin`/`stdout` (an interactive console is the same channel under both names,
    rev1§5.1).
  - Move the shell off `sys::debug_*`; demote the kernel UART to the debug-only scaffold
    rev1§7 now sanctions, and gate/remove the EL0 debug-UART syscalls (closing S-8 for
    the user-facing path).
- **Acceptance:** the shell does all console I/O through the userspace driver channel;
  no EL0 path uses the kernel debug-UART syscalls; QEMU interactive boot works.
- **Effort/Risk:** L / high (the heavy kernel lifting is in B-IRQ; this phase is the
  driver + shell rewiring on top of it).

### Phase C2 — Ephemeral file-id indirection + rename

- **Closes:** audit §3.2 file-id/rename gap (rev0§4.9). M2 debt.
- **Spec:** rev1§4.3 (memtable file-id keying), rev1§4.9 (runtime file identity,
  rename, unlink-while-open).
- **Touches:** `cas/src/store.rs` (overlay keying — today path-keyed), the
  storage-server `Request` enum (no rename op).
- **Depends on:** none among audit items; does not unblock other phases (so it stays
  late). Coordinate with B5 (both touch the storage-server protocol surface).
- **Work:** introduce ephemeral server-runtime file IDs with an ID→current-path map
  (O(1) per rename); key the overlay on file id; add a rename op; implement
  unlink-while-open (open handle keeps working; data discarded at flush if the ID
  resolves to no path); deny cross-subtree-handle rename targets (unnameable).
- **Acceptance:** rename is O(1) over dirty state; open handles follow renames; unlink-
  while-open behaves per rev1§4.9; proptest over rename/unlink interleavings.
- **Effort/Risk:** M–L / medium.

### Phase C3 — Multi-version wire negotiation

- **Closes:** audit §3.2 multi-version negotiation gap (rev0§3.7). Protocol is at
  version 2 but both peers ship from one tree; no negotiation.
- **Spec:** rev1§3.7 (versions negotiated once at session establishment; a server may
  speak several concurrently).
- **Touches:** `ipc/` (session establishment / `wire.rs` header), the servers'
  connect handlers.
- **Depends on:** none. Low urgency until a second concurrent version exists.
- **Work:** implement version negotiation at session establishment so a server can
  speak several versions concurrently; keep the fixed header (the never-migrating
  layer) untouched. This is mostly mechanism for the future "non-Rust userspace"
  effort (rev1§8.3) and can stay minimal now (negotiate, even if only one version is
  offered).
- **Acceptance:** a session negotiates a version explicitly; an unsupported version is
  refused cleanly (rev1§3.7 discipline); header codec proofs unchanged.
- **Effort/Risk:** S–M / low.

### Phase C4 — Concurrent / incremental GC, persisted marking, streaming WAL replay

- **Closes:** audit §3.2 concurrent-GC family (rev0§8.3). The synchronous GC and
  whole-region WAL replay are the accepted present state (audit §2.6 refuted the
  "must be concurrent" reading).
- **Spec:** rev1§4.6 (concurrency), rev1§8.3 (persisted incremental marking — flagged
  as worth its own TLA+ model).
- **Touches:** `cas/src/gc.rs`, `cas/src/store.rs` (GC + mount/replay), a new TLA+
  model for incremental marking.
- **Depends on:** **B6** (the resurrection mechanism must already be installed — it is
  the one point of GC/mutator interaction that concurrency relies on; B6 makes the
  birth-generation "live by fiat" filter non-vacuous). Genuinely future work; lowest
  priority.
- **Work:** make GC concurrent/incremental with the mutator; persist mark progress for
  restart survival; stream WAL replay instead of whole-region. Model the persisted-
  incremental-marking protocol in TLA+ first (rev1§8.3 calls for it), noting the
  Bloom-filter polarity hazard (the resurrection check must consult the exact
  deletion-candidate list, never trust Bloom positives).
- **Acceptance:** GC runs concurrently with writes without resurrecting condemned
  chunks (the B6 mechanism now load-bearing); TLA+ model for incremental marking
  passes with a negative control; mark progress survives restart.
- **Effort/Risk:** L / high. Defer until mark time approaches uptime (rev1§8.3).

---

# Execution order & dependency map

Recommended sequence as parallelizable waves. Within a wave, phases are independent.

```
Part A (spec rev1) ── A1,A2,A3 ── A4 ── A5      [bless spec_rev1.md FIRST]
        │
        ▼
Wave 1  B1  B2  B3  B4        quick high-value correctness (parallel)
Wave 2  B5  B6  B7  B11       high-severity storage + verification clusters (parallel)
Wave 3  B8 ── B9             kernel verified-surface, then preemptible revoke
        B10                  aspace top-up (parallel with B8/B9)
        B8 ──► B-IRQ        kernel IRQ subsystem (after B8; console track's long pole)
Wave 4  B12  B13  B14         medium verification/conformance (parallel; B12 gated by A2)
Wave 5  C1 ─┐
        B-IRQ ┴► C-M9        named-grant table + IRQ subsystem, then userspace console
Wave 6  B15  C2  C3           backfill + deferred (parallel)
Wave 7  C4                   concurrent/incremental GC (depends on B6)
```

**Hard dependencies:**
- All of Part B/C depend on **Part A** (the blessed target).
- **B12** is gated by the **A2** mechanisms-vs-disclosure decision.
- **C-M9** depends on **both C1** (console cap under standard names) **and B-IRQ** (the
  kernel device-IRQ→notification path). Confirmed by the IRQ investigation — a console
  needs RX interrupts, and no device-IRQ path exists today (only the timer; all drivers
  poll).
- **B-IRQ** is sequenced after **B8** to avoid churning the freshly-verified kernel
  surface; otherwise independent and can run anytime in the kernel wave.
- **C4** depends on **B6** (resurrection mechanism installed).
- **B9** is sequenced after **B8** to avoid churning the verified surface; not a hard dep.

**Severity rollup:** high → B1, B5, B6, B7, B11, B-IRQ, C-M9 (+ the DMA-pool UB hazard
in B4); medium → B2, B3, B4, B8, B9, B10, B12, B13; low → B14 (partly), B15. B-IRQ is
high because it is a mandated rev0§1 object that is entirely absent.

---

# Open decisions requiring sign-off

1. **A2 / B12 — flush-policy mechanisms (M-3…M-7).** Keep the rev0§4.4 mechanisms
   *mandatory* and implement them (Phase B12, more work, full conformance), **or**
   formally disclose the collapsed single-budget policy as accepted MVP simplification
   (doc-only, plus S-10). *Recommendation:* keep mechanisms mandatory, relax only the
   numbers — the spec's "the mechanisms above are fixed" framing makes the collapse a
   conformance gap, not a tuning choice.
2. **A1 / B8 — priority-ceiling and MAP verification.** Conform (verify both in B8,
   keep the rev1§5.4/§6.1(c) claims) vs soften the spec. *Recommendation:* conform —
   both are cheap relative to the existing `kcore` proof surface.
3. **C-M9 ordering.** Two prerequisites, with different latitude. **B-IRQ is
   unavoidable** — the IRQ investigation confirmed there is no device-IRQ→notification
   path today, so it must be built regardless (the only open choice there is *when* in
   the kernel wave). **C1 is optional-but-recommended**: build the named-grant table
   first (clean console under standard names), **or** wire M-9 with the existing
   hand-rolled startup blocks to decouple it from C1. *Recommendation:* do C1 first —
   it also retires the "SD02/SH01/ST01" hand-rolling the audit flags. Schedule B-IRQ
   in the kernel wave so it is ready when the console track begins.
4. **B7 / T-2 — `lemma_gap_freedom`.** Discharge `laid_out` to make the lemma live, vs
   delete it and rest the in-code guarantee on proptest+TLA. *Recommendation:* attempt
   discharge first; delete only if it proves disproportionate — no dead proofs either
   way.
5. **B11 — `urt` heap bar.** Full Verus proof vs Miri+proptest floor. *Recommendation:*
   target Verus (it's in the verify set); accept the Miri+proptest floor only if the
   pointer-graph proof is disproportionate, and record which bar was met in the ledger.
