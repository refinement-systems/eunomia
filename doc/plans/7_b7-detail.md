# Plan — Part B7 detail: storage commit/recovery verification (replay-equality action property + fsync axiom + shrink the WAL content seam + tie the verified decisions to the running code)

Detailed, separately-implementable decomposition of **Phase B7** from
`doc/plans/0_address_audit_rev0.md`. B7 is Wave-2 work and the parent plan's
**single most important verification gap**: the storage commit/recovery surface claims
mechanized replay-equality, but the TLA+ model checks only the durable substrate (a no-op
`Recover` passes every invariant), the fsync axiom is encoded operationally rather than
named, a true-but-disconnected lemma sits on an undischarged hypothesis with zero call
sites, and one `external_body` seam over-trusts a *structural* decode that the other
on-disk decoders verify. B7 is **verification-only** — it changes no on-disk bytes, no wire
op, no runtime behaviour; it closes the theater by making the claimed proofs real.

**Closes (from the parent plan):**
- `T-1` [high] — **the headline gap.** The TLA+ headline invariant `AckedWritesRecoverable`
  (`CommitProtocol.tla:250-254`) constrains only the durable substrate (`LiveSlot.refRoots`,
  `walLog`, `walHead`) and **never references `overlay`** — the recovered memtable. So a
  `Recover` that reconstructs `overlay` *incorrectly* (or not at all) passes all five
  configured invariants: replay-*equality* is asserted operationally inside the `Recover`
  action body (`:192-202`) but **checked nowhere**. The audit confirmed a no-op `Recover`
  passes the full invariant suite empirically. B7A adds a `Recover`-step **action property**
  that relates `overlay'` to `(durableRoots, walLog, LiveSlot)`, plus a committed negative
  control that makes a no-op `Recover` produce a counterexample.
- `T-2` [medium] — `lemma_gap_freedom` (`store.rs:910-946`) proves a true statement
  (every unflushed record lies in the replayed span) with **zero call sites**, on an
  **undischarged** `laid_out` hypothesis (`:831`) — the authors' own comment admits it is
  "a documented invariant, not enforced at one site Verus sees" (`:816-819`). A proof that
  fires nowhere and rests on an unestablished premise is a dead proof. B7C either discharges
  `laid_out` so the lemma becomes live (tied to the running recovery decisions) or retires
  it cleanly — **no dead proofs either way** (parent open decision #4).
- `T-4` [medium] — rev1§4.8's single trusted axiom, **"fsync means fsync,"** is encoded only
  *operationally* in the model (barrier 1 moves `chunkBuf → durableRoots`; `Crash` leaves
  `durableRoots` `UNCHANGED`) and is **not named**. rev1§4.8 requires it "stated explicitly
  as a labeled `ASSUME` axiom in the TLA+ model." B7A adds the labeled `ASSUME`.
- `T-5` [low] — `wal_content_ok` (`store.rs:637-645`, `external_body`) trusts **both**
  interpreted BLAKE3 (legitimately trusted, the same seam as `checksum_ok`) **and** the pure
  bounded **structural decode** (`WalOp::decode_record → decode_payload`, `disk.rs:618/:545`)
  in one opaque box. The structural half is exactly the kind of total bounded decode the
  other on-disk decoders verify (`decode_checked_fields`, `decode_frame`). B7B splits the
  structural acceptance out and brings it into the verified/total surface, **shrinking the
  trusted seam to BLAKE3 only** (rev1§3.7/§6.1(e)).
- **mount/commit orchestration unverified** [audit §4.2, medium] — `mount` (`store.rs:1031`)
  and `commit` (`:1737`) are plain Rust that *call* the verified decision cores
  (`pick_survivor`, `commit_target`, `advance_head`, `replay_bound`, `decode_frame`) but
  expose **no `requires`/`ensures` boundary** tying the running sequencing to the proved
  decisions. B7C adds the orchestration-boundary contract (the home `lemma_gap_freedom`
  fires through, T-2's other half).

**Spec target (already blessed in rev1 — B7 only conforms code/model to it):**
- **rev1§4.5 "Crash recovery"** (`spec_rev1.md:268-280`) — "Replay the WAL from the recorded
  head to rebuild per-ref overlay state for acknowledged-but-unflushed writes; discard
  checksum-failing tail records." Recovery is "total over arbitrary device contents." This is
  the replay-equality T-1 mechanizes and the totality T-5's verified structural decode
  extends.
- **rev1§4.8 "Integrity"** (`:331`) — "The single trusted axiom is that **fsync means
  fsync** … stated explicitly as a labeled `ASSUME` axiom in the TLA+ model." T-4 makes the
  text true. "Every layer self-verifies … detects corruption on read" — the structural decode
  T-5 verifies is part of that self-verification.
- **rev1§6 "Verification"** and **rev1§6.1(e) "Storage-recovery content coverage"**
  (`:419`) — already written by Part A with the four `[verifying]` parts, two of them B7's:
  "each record's **structural decode**, split out of its hash wrapper and verified like the
  other on-disk decoders (§3.7); and the model's **replay-equality**, mechanized by the
  recovery-step action property (§6) that relates the reconstructed overlay to the durable
  roots and surviving WAL — earlier the model constrained only the durable substrate, so a
  no-op recovery passed, and the new property must fail that case." rev1§6.1(e) also fixes
  what **stays trusted**: "per-record content acceptance on the real code; the **BLAKE3 hash
  and superblock checksum** … the only part of the record seam left uninterpreted once the
  structural decode is split off; and the durability axiom — **fsync means fsync** … named as
  a labeled `ASSUME`." And it pins the deliberate non-goal: "The commit routine itself stays
  plain Rust over the verified decisions, so the full replay-equality invariant is mechanized
  nowhere on the real code and remains the model's alone."

Because Part A is blessed first (the parent plan's hard dependency), **B7 makes no normative
spec edits** — the rev1 text above is the fixed target, and every citation here is `rev1§`.
The only doc-touches B7 *does* make are the sanctioned **"flip your own `[verifying]` status
line" edits** (parent plan A4): each sub-phase flips its rev1§6.1(e) `[verifying]` part to
mechanized and updates the matching trusted-base-ledger row + TLA baseline. No prose claim
changes; only the trusted/verified tag moves, exactly as A4 set up the per-phase hooks.

**Primary files:**
- `tla/commit_protocol/CommitProtocol.tla` — the `Recover` action `:192-202`,
  `AckedWritesRecoverable` `:250-254`, the `overlay` var `:52`, `Crash`'s `UNCHANGED …
  durableRoots` `:186` and `CommitPrepare`'s `durableRoots' = durableRoots \cup chunkBuf`
  `:132` (the operational fsync encoding T-4 names), `Next` `:204-210`, `Spec` `:212`.
- `tla/commit_protocol/CommitProtocol.cfg` — the `INVARIANT` block `:11-15` (add a `PROPERTY`
  line); a **new** `CommitProtocol_NegControl.cfg` for the negative control.
- `cas/src/store.rs` — the gap-freedom composition block `:802-948` (`laid_out` `:831`,
  `lemma_laid_out_mono` `:853`, `lemma_run_len_covers` `:873`, `lemma_gap_freedom` `:910`, and
  its admitting comment `:816-819`); the WAL content seam `wal_content_ok` `:637-645` +
  `content_ok_spec` `:650`; `run_len` `:662`, `frame_at` `:559`, `decode_frame` `:587`,
  `replay_bound` `:718`, `advance_head` `:497`; the plain-Rust `mount` replay loop
  `:1172-1217` (the `decode_record(...).expect(...)` at `:1182`) and `commit`'s `advance_head`
  call `:1766`.
- `cas/src/disk.rs` — `WalOp::decode_record` `:618-635` (framing + checksum + structural
  decode), `WalOp::decode_payload` `:545-590` (the structural half T-5 verifies/splits),
  `checksum_ok` `:339-346` (the BLAKE3 superblock seam, the model for the WAL-side split),
  `record_checksum`/`encode_record` `:603`.
- `doc/guidelines/verus_trusted-base.md` — the `wal_content_ok` row `:58` and `checksum_ok`
  row `:57`, the `[verifying]` transition table `:96-102`, the Baselines table `:104-116`
  (CAS `58/0` `:111`, TLA `CommitProtocol (6886 states)` `:115`).
- `doc/spec/spec_rev1.md` — the rev1§6.1(e) `[verifying]` markers `:419` (flip B7's two parts).

Secondary: `tools/tla/tla-model-check.sh` (the TLC runner; no change — the new cfg rides it),
`cas/tests/*` (the crash-injection proptest that backstops T-2's retire-path, if taken).

---

## Verification tier & baseline (applies to all sub-phases)

B7 spans two verification surfaces with different routing (rev1§6): **TLA+ model checking**
(the `CommitProtocol` design gate) and the **`cas` Verus chokepoint**. Six honesty notes up
front so nothing is silently dropped or over-claimed:

- **The model and the code are deliberately separate, and `(e)` says so.** rev1§6.1(e) is
  explicit: "The commit routine itself stays plain Rust over the verified decisions, so the
  full replay-equality invariant is mechanized nowhere on the real code and remains the
  model's alone." So B7 does **not** verify `mount`/`commit` end-to-end in Verus (they do
  device I/O, `Vec`/`BTreeMap` building — outside the SMT-tractable core). T-1 mechanizes
  replay-equality in the **TLA+ model**; T-2 + the orchestration contract tie the **verified
  decision cores** to the running code via a thin `requires`/`ensures` boundary; the two are
  complementary halves of "the recovery code is tied to the proved decisions," not one
  redundant proof. This is the same trusted-shell-over-verified-cores posture B6 took for the
  mark walk and B4 for the DMA raw-pointer wrapper.
- **The Verus gate is held, then *rises*.** `cargo verus verify -p cas --no-default-features`
  is **58/0** today (ledger `:111`). B7A touches **no Verus** (TLA only) → 58/0 held.
  **B7B raises it**: the split adds a verified total structural-decode predicate (Design
  decision 3), so the count goes **above 58** — B7B records the new total and updates the
  ledger. **B7C** either *raises* it again (discharging `laid_out` makes `lemma_gap_freedom`
  +`lemma_run_len_covers`+`lemma_laid_out_mono` live, reachable proofs that now contribute)
  **or holds at the B7B total** if the lemma is retired (the three dead proofs are removed,
  not weakened). No existing proof is ever weakened; the gate is a floor.
- **The TLA+ baseline is held, then a property is added and a negative control committed.**
  `CommitProtocol` checks **6886 states** today (ledger `:115`). Adding a `PROPERTY` checks
  transitions over the *same* reachable state graph, so the state count stays ~6886 (B7A
  re-runs and records the exact figure); the property is checked over every step, the no-op
  `Recover` does not arise in the real `Next`, so it passes. The **negative control** is a
  second, deliberately-broken spec (`SpecNeg`, a no-op `Recover`) checked by
  `CommitProtocol_NegControl.cfg`, where the new property **must report a violation** (a short
  counterexample trace) — the runnable proof that the property has teeth. This mirrors
  `CapRevocation_Teardown.cfg`'s committed-second-spec style and the parent plan's
  "negative control, in the project's established style."
- **No on-disk format change, no wire change, no corpus regeneration.** Unlike B5 (which
  bumped `SB_VERSION` and appended a ref-record field) and like B6, B7 changes **zero
  persistent bytes** and **zero wire ops**. The WAL record encoding (`encode_record`,
  `disk.rs:603`) is byte-identical before and after T-5 — the split is a *refactor of which
  predicate the verifier trusts*, not a layout change. The committed mount/recovery corpora
  (`mount_recovery`, `mount_reseal`, `wal_replay_scan`, `wal_replay_scan_fixup`) stay valid
  and keep exercising the same bytes; T-5 strengthens what the verifier proves *about* those
  bytes, so the fuzz/Miri tier is unchanged in inputs and stronger in oracle.
- **fsync stays trusted — T-4 names it, it does not prove it.** "fsync means fsync" is a
  statement about real hardware (QEMU/virtio-blk `cache=writeback` + FLUSH honored), so it is
  an **axiom**, not a theorem. T-4's deliverable is to make the trusted assumption a *named,
  grep-able, TLC-acknowledged* top-level `ASSUME` — converting the silent operational encoding
  into an explicit one. It is recorded on the trusted-base ledger as an axiom, never as a
  closed seam.
- **No Loom/Shuttle, no new proptest machinery.** The commit protocol's crash-atomicity is
  the existing two-barrier superblock flip (rev1§4.2), already exercised by the
  crash-injection proptest (`crash_recovery_preserves_acked_state`) and modeled by
  `CommitProtocol`. B7 adds no atomics and no second mutator; the only new *test* artifact is
  the negative-control cfg (T-1) and — only if T-2 takes the retire path — an explicit note
  that the in-code gap-freedom guarantee rests on the existing crash proptest + TLA, no new
  proptest needed.

**Baseline to re-establish at end of B7:**
- `tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla` passes with the new
  `PROPERTY` (record the state count — expected ~6886/unchanged); the negative-control run
  `tools/tla/tla-model-check.sh … CommitProtocol_NegControl.cfg` reports the expected
  property violation (a counterexample), recorded as the negative control.
- `cargo verus verify -p cas --no-default-features` ≥ **58/0**, **> 58** after B7B (record the
  new total in the ledger), held-or-higher after B7C per its branch.
- `cargo test -p cas` green (the crash-injection + WAL-replay tests unchanged; T-5's structural
  predicate is exercised by the existing mount/recovery corpora and any unit added for the
  split).
- Miri replay clean over the unchanged sweep:
  `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas -p loader
  -p storage-server --test fuzz_regressions --test fuzz_corpus`.
- The aarch64 build still boots: `cd kernel && cargo build` (B7 changes no signatures
  `storaged` depends on; `mount`/`commit` keep their public types).

---

## Design decision 1 — the replay-equality property: a *semantic* `Recover`-step action property + a committed negative-control spec *(resolve in B7A)*

T-1 needs `overlay'` after `Recover` to be *checked* against durable state, and the check
must have teeth (a no-op `Recover` must fail it). B7A pins the property's phrasing and the
negative control's form.

- **Adopted — a semantic action property `RecoverReconstructs`, phrased over `writeCtr`/
  `walLog`/`LiveSlot`, not a mirror of the `Recover` action body.** Add to
  `CommitProtocol.tla`:
  ```tla
  \* rev1§6/§4.5: replay-EQUALITY. After any Recover step, the rebuilt overlay
  \* is EXACTLY the acked writes past the committed head not already covered by
  \* the committed root — characterized semantically (over writeCtr + walLog),
  \* independently of how Recover computes the set. The earlier suite constrained
  \* only durable state, so a no-op Recover passed; this is the property that
  \* fails it (see CommitProtocol_NegControl.cfg).
  RecoverReconstructs ==
      [][ (crashed /\ ~crashed') =>
            \A r \in Refs :
              overlay'[r] = { v \in 1..writeCtr[r] :
                                /\ v > LiveSlot.refRoots[r]
                                /\ \E i \in (LiveSlot.walHead + 1)..Len(walLog) :
                                       walLog[i] = <<r, v>> } ]_vars
  ```
  and list it under `PROPERTY` in `CommitProtocol.cfg`. Decisive reasons:
  1. **It is genuinely a check, not a tautology.** The `Recover` action body
     (`:194-199`) builds the set by *projecting WAL indices* (`{walLog[i][2] : i \in {…}}`);
     `RecoverReconstructs` characterizes it *semantically* (the set of versions `v` that are
     acked, past the committed root, and have a surviving WAL record past the head). The two
     phrasings coincide **iff replay is correct** — so the property catches an off-by-one in
     the `walHead + 1` bound, a dropped `> refRoots[r]` idempotence filter, a wrong ref
     selection, or a future regression in `Recover`. A property that merely re-stated the
     action body would be vacuous; this one is the independent oracle.
  2. **It is an action property, the right shape — and the project has the precedent.**
     Replay-equality is about a *step* (the crash→recovered transition), so it is `[][P]_vars`
     with primed `overlay'`/`crashed'`, exactly the shape of `CapRevocation`'s
     `ReportMonotone` (`CapRevocation.tla:294-296`), which is also listed as a `PROPERTY`.
     `(crashed /\ ~crashed')` selects precisely the `Recover` steps.
  3. **`LiveSlot` is stable across `Recover`.** `Recover` leaves `slotA`/`slotB` `UNCHANGED`
     (`:201`), so `LiveSlot` evaluated in the post-state equals its pre-state value — the
     property may name `LiveSlot` directly without priming.
- **Adopted — a committed, runnable negative control: a second `SpecNeg` with a broken
  `Recover`, checked by `CommitProtocol_NegControl.cfg`.** In the same `.tla` file add:
  ```tla
  \* NEGATIVE CONTROL (rev1§6 discipline): a Recover that rebuilds nothing.
  \* Under SpecNeg, RecoverReconstructs MUST be violated — the runnable proof
  \* that the property has teeth. Checked by CommitProtocol_NegControl.cfg.
  RecoverNoop ==
      /\ crashed
      /\ overlay' = [r \in Refs |-> {}]
      /\ crashed' = FALSE
      /\ UNCHANGED <<slotA, slotB, walLog, durableRoots, chunkBuf,
                     pendingRoot, pendingSB, commitPhase, writeCtr>>
  NextNeg == \/ \E r \in Refs : Write(r)  \/ \E r \in Refs : Flush(r)
             \/ CommitPrepare \/ CommitFinish \/ Crash \/ RecoverNoop
  SpecNeg == Init /\ [][NextNeg]_vars
  ```
  with `CommitProtocol_NegControl.cfg` = `SPECIFICATION SpecNeg` + `PROPERTY
  RecoverReconstructs` (+ the same `CHECK_DEADLOCK FALSE`). TLC reports a property violation
  with a short trace (write → flush nothing / crash with an unflushed acked write → recover to
  empty overlay ≠ the required set). Decisive reasons: a **committed, CI-runnable** negative
  control is the strongest honesty posture for the headline gap — a reviewer (or a future
  refactor) can *run* `CommitProtocol_NegControl.cfg` and see the property bite, exactly as
  `CapRevocation_Teardown.cfg` is a committed second spec. It also documents *which* mutation
  the property defends against.
- **Rejected — a tautological property that mirrors the `Recover` action body** (`overlay' =
  <the same set-builder Recover uses>`). Trivially true against the real `Recover` and false
  against the no-op, so it would *appear* to satisfy T-1 — but it checks only that two copies
  of the same expression agree, catching no real reconstruction bug. The semantic phrasing is
  what gives the property independent value.
- **Rejected — a documented-only negative control (the IpcReactor header style).** IpcReactor
  documents its lost-wakeup negative control in prose (remove the `word = 0` guard → property
  fails) without a committed broken cfg. Acceptable and lighter, but for B7's *headline*
  verification gap a committed, runnable control is worth the one extra small file — it makes
  "the property has teeth" a checkable artifact, not a claim. (Recorded so the heavier choice
  is deliberate.)

**Recommendation: add the semantic `RecoverReconstructs` action property to the main cfg, and
commit the `SpecNeg`/`RecoverNoop` negative control as `CommitProtocol_NegControl.cfg`.**

---

## Design decision 2 — the fsync axiom: a labeled top-level `ASSUME` naming the durability assumption *(resolve in B7A)*

rev1§4.8 requires "fsync means fsync … stated explicitly as a labeled `ASSUME` axiom in the
TLA+ model." Today it is encoded only operationally (`CommitPrepare`: `durableRoots' =
durableRoots \cup chunkBuf` at barrier 1, `:132`; `Crash`: `UNCHANGED … durableRoots`,
`:186`). B7A names it.

- **Adopted — a labeled, commented top-level `ASSUME FsyncMeansFsync`.** Add near the top of
  `CommitProtocol.tla` (after the `EXTENDS`/`CONSTANTS`, before `Init`):
  ```tla
  \* rev1§4.8 — THE single trusted storage-layer axiom, named (not derived).
  \* "fsync means fsync": a completed fsync barrier makes the preceding writes
  \* durable, and a crash never loses durable state. The model ENCODES this in
  \* CommitPrepare (chunkBuf -> durableRoots at barrier 1) and Crash (UNCHANGED
  \* durableRoots); this ASSUME makes the assumption an explicit, grep-able
  \* top-level axiom rather than an implicit consequence of the crash semantics.
  \* It rests on the QEMU/virtio-blk cache=writeback + FLUSH config under our
  \* control (rev1§4.8), NOT on any proof — it is the irreducible trusted base of
  \* the storage recovery argument, recorded as such in the trusted-base ledger.
  ASSUME FsyncMeansFsync == TRUE
  ```
  Decisive reasons: TLA's `ASSUME name == expr` is a named top-level assumption TLC evaluates
  at the constant level; `TRUE` is the honest body for an axiom about *the real world* (there
  is nothing in the constant universe to check — the content is the trusted modeling choice
  the comment documents). The value of the line is that the trusted axiom is now **named in
  the model**, satisfying rev1§4.8 literally and giving the ledger a concrete construct to
  cite. This is the labeled-`ASSUME` idiom rev1§4.8 asks for.
- **Rejected — encode durability as a checkable action property** (e.g. `DurableSurvivesCrash
  == [][ durableRoots' \supseteq durableRoots \/ … ]_vars`). That would only check the model
  *against itself* (that `Crash` indeed leaves `durableRoots` unchanged) — turning a trusted
  axiom into a self-consistency check, which misrepresents it as derived. fsync durability is
  trusted by construction; an `ASSUME` is the correct, honest construct, distinct from the
  *checked* properties (`RecoverReconstructs`, the invariant suite).
- **Rejected — leave it operational and only document it in a comment.** That is the status quo
  the audit (T-4) flags: rev1§4.8 specifically requires a *labeled `ASSUME`*, not prose. A bare
  comment is not grep-able as an axiom and not acknowledged by the tool.

**Recommendation: add the labeled `ASSUME FsyncMeansFsync` with the durability comment; record
it as an axiom (not a closed seam) in the trusted-base ledger.**

---

## Design decision 3 — shrinking the WAL content seam: split the structural decode out of `wal_content_ok`, leaving only BLAKE3 trusted *(resolve in B7B)*

`wal_content_ok` (`store.rs:637-645`, `external_body`) wraps `WalOp::decode_record(...)
.is_some()` — which folds together **framing** (already verified via `decode_frame`/
`frame_at`), the **BLAKE3 record checksum** (legitimately trusted), and the **structural
payload decode** (`decode_payload`, `disk.rs:545-590` — bounded `Reader` reads, total by
`FormatError` discipline). T-5 splits the structural half out and verifies it like the other
on-disk decoders, leaving BLAKE3 the only uninterpreted part.

- **Adopted — refactor `content_ok_spec` into `struct_ok_spec ∧ checksum_ok_spec`, verify
  the structural predicate total, and reduce the `external_body` seam to the BLAKE3 compare
  alone.** Concretely:
  1. Define a **verified, total** `wal_payload_struct_ok(rec: &[u8]) -> bool` (in `verus!{}`,
     the `decode_frame`/`decode_checked_fields` style) that returns whether the record's
     payload region structurally decodes — proven panic-free and in-bounds **∀ bytes** (every
     `Reader::take(n)?` bounds-checked, no arithmetic overflow), mirroring how
     `decode_checked_fields` *is* its own totality theorem. The exec body is the structural
     part of `decode_payload` (tag dispatch + bounded length-prefixed reads), returning a
     bool rather than building the `Vec`s — the decode that *builds* `WalOp` stays the
     plain-Rust applier's job in `mount` (it needs the owned `Vec`s; rev1§6.1(e) keeps content
     acceptance on the real code).
  2. Keep `wal_content_ok` `external_body` but shrink its body and `ensures` to **only the
     BLAKE3 record checksum** — `record_checksum(seq, len, payload) == buf[16..48]` — and make
     its spec twin `checksum_ok_spec` an `uninterp spec fn` over the slice (the lone BLAKE3
     abstraction, exactly parallel to `checksum_ok`/the superblock seam).
  3. Redefine `content_ok_spec(rec) == wal_payload_struct_ok_spec(rec) /\ checksum_ok_spec(rec)`
     — where `wal_payload_struct_ok_spec` is the *verified* predicate's spec (interpreted, no
     longer trusted) and `checksum_ok_spec` is the uninterp BLAKE3 twin. `run_len`/`frame_at`/
     `replay_bound`/`laid_out` keep referencing `content_ok_spec` **unchanged** (interface
     stable), so the maximal-run reasoning and B7C's `laid_out` work are unaffected by the
     internal split.
  Decisive reasons:
  1. **It shrinks the trusted surface to exactly BLAKE3**, which is what rev1§6.1(e) now
     promises ("the only part of the record seam left uninterpreted once the structural decode
     is split off") and the ledger's `wal_content_ok` row pre-commits ("shrinking this seam to
     BLAKE3-only"). The structural decode joins `decode_frame`/`decode_checked_fields` in the
     verified surface.
  2. **The interface stays stable**, so B7B is independent of B7C: `content_ok_spec` keeps its
     name and meaning ("this record is content-valid"); only its *internal definition* refines
     from one uninterp predicate to `verified-struct ∧ uninterp-blake3`. The closed-form
     `run_len` and the `laid_out` invariant that name `content_ok_spec` need no edit.
  3. **No on-disk or behavioural change.** `wal_content_ok`'s observable result is identical
     (`decode_record().is_some()` ⇔ framing ∧ checksum ∧ struct-ok); the refactor only changes
     which conjunct the verifier trusts vs. proves.
- **State-the-bar fallback (recorded, B11/B7 discipline).** If proving `decode_payload`'s
  structural totality in `verus!{}` proves disproportionate (the `Reader`/`Vec` abstraction
  needs more vstd specs than the [low] budget warrants), the acceptable floor is: keep the
  structural decode a **plain-Rust total function** (it already is, by `FormatError`
  discipline, and is fuzzed via `wal_replay_scan`) and shrink `wal_content_ok`'s
  `external_body`/`ensures` to **BLAKE3 only** with `content_ok_spec = struct_ok_spec(total
  decoder) ∧ checksum_ok_spec`, recording in the ledger that the structural half is delivered
  at the **total-decoder + fuzz/Miri** tier and BLAKE3 is the lone uninterp seam. Either way
  **the trusted seam shrinks to BLAKE3** — the audit's actual finding — and B7B records which
  bar (full Verus vs. total+fuzz) was met. Recommended target: the full Verus predicate (it is
  the same shape as `decode_frame`, already in the crate), accepting the floor only if the
  `Vec`-free structural walk does not extract cheaply.
- **Rejected — leave the combined seam and only re-document it.** The status quo over-trusts a
  bounded structural decode that the crate verifies elsewhere; the audit ([T-5]) and the
  pre-committed ledger row both call for the split. Re-documenting without splitting leaves the
  trusted surface larger than it needs to be.

**Recommendation: split the structural decode into a verified total predicate, reduce
`wal_content_ok` to the BLAKE3 compare, redefine `content_ok_spec` as the conjunction
(interface stable); record the bar met and the new verify total.**

---

## Design decision 4 — tying the verified decisions to the running code: discharge `laid_out` (make `lemma_gap_freedom` live) or retire it; add the mount/commit orchestration contract *(resolve in B7C)*

`lemma_gap_freedom` is true, has **zero call sites**, and rests on the **undischarged**
`laid_out` hypothesis (the comment at `:816-819` admits it). The parent plan's open decision
#4: "discharge `laid_out` to make the lemma live, vs delete it … attempt discharge first;
delete only if disproportionate — no dead proofs either way."

- **Adopted — attempt discharge via a self-contained verified composition core that `mount`/
  `commit` call through a thin `requires`/`ensures` boundary; retire cleanly if
  disproportionate.** The discharge path:
  1. **Establish `laid_out` where the records are produced, inside the verified core — not in
     plain-Rust `mount`.** `replay_bound` (`:718`) already walks exactly the accepted records
     over the raw WAL bytes; strengthen it (or add a companion verified lemma keyed to its
     walk) to also **establish `laid_out(wal@, <the accepted records>, 0)`** — every accepted
     record frames (`frame_at` Some via `decode_frame`'s ensures), is content-valid
     (`wal_content_ok`'s ensures → `content_ok_spec`), has `seq < u64::MAX` (the loop stops at
     the boundary), and chains contiguously/seq-continuously into the next (the loop's `off +=
     rlen`, `seq += 1`). This converts `laid_out` from an *assumed* invariant into one
     *produced* by the verified walk — the "one site Verus sees."
  2. **Add a verified composition wrapper** (e.g. `gap_free_recovery_core`) with
     `requires`/`ensures` that takes `replay_bound`'s laid-out evidence + `advance_head`'s
     flushed-prefix structure and **invokes `lemma_gap_freedom`**, exposing a single
     `ensures`: every unflushed record lies in the replayed span. Plain-Rust `mount`/`commit`
     call this wrapper at the orchestration boundary — the contract the audit's "mount/commit
     glue" item asks for ("at minimum `requires`/`ensures` on the orchestration boundary"). The
     lemma now **fires** (live), tying the proved gap-freedom decision to the running recovery
     sequencing, with `laid_out` discharged inside the core rather than asserted by hand.
  3. `lemma_run_len_covers` (`:873`) and `lemma_laid_out_mono` (`:853`) become **live support
     lemmas** of the discharge (already reachable from `lemma_gap_freedom`), so the whole
     `:823-948` block is wired in — the verify count *rises* by the now-reachable proofs.
  - **The boundary is honest about what stays trusted.** Per rev1§6.1(e), `mount`/`commit`
    themselves stay plain Rust over the verified decisions — the device I/O, the `Vec`/`BTreeMap`
    building, the applier's `decode_record` are not pulled into Verus. The wrapper verifies the
    *decision* (gap-freedom over a laid-out queue), not the I/O around it; the join between the
    in-memory `RecMeta` queue and the on-device bytes is the same trusted-Store seam §6.1(c)/(e)
    already names. The deliverable is "the running recovery code is tied to the proved
    decisions," not "mount is verified."
- **Retire fallback (recorded; parent open decision #4).** If discharging `laid_out` across the
  plain-Rust replay loop proves disproportionate — because the `RecMeta` queue is built by I/O
  code Verus cannot see end-to-end without a Store-level verified invariant (a much larger
  surface than B7's budget) — then **remove** `lemma_gap_freedom`, `lemma_run_len_covers`,
  `lemma_laid_out_mono`, and `laid_out` entirely, and record in the gap-freedom comment block
  (`:802-821`) that the in-code gap-freedom guarantee rests on **(a)** the TLA+
  `RecoverReconstructs`/`AckedWritesRecoverable` model (B7A) and **(b)** the crash-injection +
  WAL-replay proptest/fuzz tier — **no dead proof left in tree.** The verify count then *falls*
  by the removed proofs (still 0 errors; the gate's 58/0 floor is a verified-or-removed count,
  and the ledger records the delta with rationale). The mount/commit orchestration contract
  (step 2's wrapper `ensures`, minus the now-absent lemma) is still added — tying
  `advance_head`/`replay_bound`'s *individual* ensures to the call sites — so "glue" is closed
  either way.
- **Rejected — keep `lemma_gap_freedom` as-is (true, dead, undischarged).** That is precisely
  the theater T-2 names: a proof that fires nowhere on a premise nothing establishes. The plan's
  "no dead proofs" rule forbids leaving it.

**Recommendation: attempt the discharge (strengthen `replay_bound` to emit `laid_out` evidence,
add the composition wrapper, make the lemma live and the orchestration contract real); retire
the lemma + its supports cleanly only if the discharge is disproportionate — recording which
path was taken and the resulting verify count.**

---

## Sub-phase B7A — TLA+ model: replay-equality action property + fsync axiom *(closes T-1 [high], T-4 [medium])*

The headline deliverable, and the single most important item in B7. Pure TLA+ — touches no
Rust, no Verus, no on-disk bytes. Self-contained and mergeable alone: after B7A the model
*checks* replay-equality (with a committed negative control proving the check bites) and names
the fsync axiom.

- **Touches:**
  - `tla/commit_protocol/CommitProtocol.tla` — add `ASSUME FsyncMeansFsync` (Design decision 2)
    near the top; add `RecoverReconstructs` (Design decision 1) in the properties region after
    `AckedWritesRecoverable` `:254`; add the negative-control `RecoverNoop`/`NextNeg`/`SpecNeg`
    block (Design decision 1). Leave the existing `Recover` action, `Next`, `Spec`, and the five
    invariants **unchanged** — the property is additive.
  - `tla/commit_protocol/CommitProtocol.cfg` — add `PROPERTY RecoverReconstructs` under the
    `INVARIANT` block `:11-15`.
  - `tla/commit_protocol/CommitProtocol_NegControl.cfg` — **new**: `SPECIFICATION SpecNeg`,
    `CHECK_DEADLOCK FALSE`, the same `CONSTANTS`, `PROPERTY RecoverReconstructs` (expected to
    **fail** with a counterexample).
- **Depends on:** Part A blessed (rev1§4.5/§4.8/§6/§6.1(e) text). No intra-B7 dependency —
  parallel with B7B/B7C (different surface entirely).
- **Work:**
  1. The `ASSUME FsyncMeansFsync` axiom + its documenting comment (T-4). Verify TLC accepts it
     (named constant-level assumption, body `TRUE`).
  2. The `RecoverReconstructs` action property (T-1), semantically phrased over
     `writeCtr`/`walLog`/`LiveSlot` (Design decision 1). Re-run TLC over `CommitProtocol.cfg`;
     it passes (the real `Recover` reconstructs correctly) at ~6886 states — **record the exact
     state count**.
  3. The negative-control spec + cfg. Run `tools/tla/tla-model-check.sh
     tla/commit_protocol/CommitProtocol.tla tla/commit_protocol/CommitProtocol_NegControl.cfg`;
     confirm TLC reports `RecoverReconstructs` **violated** with a short trace (an unflushed
     acked write recovered to an empty overlay). Record the counterexample as the negative
     control.
  4. Flip the rev1§6.1(e) `[verifying]` part for **replay-equality** (and **fsync `ASSUME`**)
     to mechanized/named (`spec_rev1.md:419`); update the trusted-base-ledger `[verifying]`
     rows for T-1 and T-4 (`verus_trusted-base.md:101-102`) and add the `FsyncMeansFsync` axiom
     to the trusted-base enumeration as an **axiom** (not a closed seam); update the TLA
     baseline line `:115` with the recorded state count + the negative-control note.
- **Acceptance:**
  - `CommitProtocol.cfg`: all five invariants **and** `RecoverReconstructs` pass; state count
    recorded (~6886, unchanged — the property does not grow the state graph).
  - `CommitProtocol_NegControl.cfg`: `RecoverReconstructs` **fails** with a counterexample (the
    negative control bites) — the runnable proof the property has teeth.
  - `ASSUME FsyncMeansFsync` present, labeled, commented as the durability axiom; TLC accepts
    the model with it.
  - rev1§6.1(e) and the ledger record T-1/T-4 as delivered; the fsync axiom is enumerated as a
    trusted axiom; the TLA baseline reflects the new property + negative control.
- **Effort/Risk:** M / medium. The property phrasing (semantic, not tautological) and verifying
  the negative control actually fails are the substance; the fsync `ASSUME` is small. Medium
  because getting `RecoverReconstructs` to have genuine teeth — and confirming the no-op control
  fails for the *right* reason — is the headline verification gap the whole phase exists to
  close.

---

## Sub-phase B7B — shrink the WAL content seam to BLAKE3-only *(closes T-5 [low])*

The seam-tightening deliverable. Independent of B7A (Rust/Verus vs. TLA) and of B7C (interface
stable, Design decision 3). After B7B the WAL record's structural decode is in the verified/
total surface and `wal_content_ok`'s `external_body` trusts **only** the BLAKE3 checksum — the
trusted surface shrinks to exactly the interpreted-hash primitive.

- **Touches:**
  - `cas/src/store.rs` — split `content_ok_spec` `:650` into `wal_payload_struct_ok_spec`
    (verified) `∧` `checksum_ok_spec` (uninterp BLAKE3 twin); shrink `wal_content_ok` `:637-645`
    `external_body`/`ensures` to the BLAKE3 compare; add the verified total
    `wal_payload_struct_ok` predicate (Design decision 3). `run_len` `:662`, `frame_at` `:559`,
    `replay_bound` `:718`, `laid_out` `:831` keep naming `content_ok_spec` unchanged.
  - `cas/src/disk.rs` — factor the structural part of `decode_payload` `:545-590` into a
    reusable bool-returning total walk (`store.rs`'s verified predicate calls it, or mirrors
    it in `verus!{}`), keeping `decode_payload`/`decode_record` byte-identical in behaviour for
    the plain-Rust applier; `checksum_ok` `:339-346` is the precedent to cite (the superblock
    BLAKE3 seam, same shape).
  - `doc/guidelines/verus_trusted-base.md` — rewrite the `wal_content_ok` row `:58` to
    **BLAKE3-only** (drop "and `WalOp` structural decode"); move the structural-decode
    construct into the verified-surface list `:22-23`; flip the T-5 `[verifying]` row `:100`;
    record the new `cargo verus verify -p cas` total `:111`.
  - `doc/spec/spec_rev1.md` — flip the rev1§6.1(e) `[verifying]` part for **structural decode**
    to mechanized `:419`.
- **Depends on:** Part A blessed (rev1§3.7/§6.1(e)). Independent of B7A; interface-stable w.r.t.
  B7C (coordinate the `content_ok_spec` definition if both land together).
- **Work:** the verified total structural predicate (or the recorded total+fuzz floor, Design
  decision 3); the `external_body` shrink to BLAKE3; the `content_ok_spec` conjunction; confirm
  `run_len`/`replay_bound`/`laid_out` still verify unchanged over the refined `content_ok_spec`.
  Add a focused unit/regression that the structural predicate accepts a well-formed record and
  rejects a structurally-malformed one (the existing `wal_replay_scan` corpus already exercises
  the path; no new corpus needed — no byte change).
- **Acceptance:**
  - `wal_content_ok`'s `external_body` body + `ensures` reference **only** the BLAKE3 checksum;
    `content_ok_spec = wal_payload_struct_ok_spec ∧ checksum_ok_spec` with the structural half
    verified (or recorded at the total+fuzz floor with the bar stated).
  - `cargo verus verify -p cas --no-default-features` **> 58/0** (record the new total); the
    maximal-run / `replay_bound` proofs verify unchanged over the refined spec.
  - `cargo test -p cas` green; the `wal_replay_scan` corpus replays clean under Miri (unchanged
    inputs, stronger oracle).
  - Ledger `wal_content_ok` row is BLAKE3-only; rev1§6.1(e) records the structural decode as
    mechanized; the bar met (full Verus vs. total+fuzz) is stated.
- **Effort/Risk:** S–M / low–medium. [low] in the parent plan; the risk is whether the
  `Vec`-free structural walk extracts into `verus!{}` cheaply — hence the stated-bar fallback.
  No format/behaviour change keeps the blast radius small.

---

## Sub-phase B7C — tie the verified recovery decisions to the running code: discharge-or-retire `lemma_gap_freedom` + mount/commit orchestration contract *(closes T-2 [medium] + the mount/commit glue [medium])*

The "connect the cores to the code" deliverable, and the resolution of parent open decision #4.
Independent of B7A; sequence after B7B so the `laid_out`/`content_ok_spec` reasoning is over the
final (split) seam shape (the same posture as B6B→B6C). After B7C there is **no dead proof** in
the gap-freedom block — `lemma_gap_freedom` is either live (discharged, firing at a real
orchestration boundary) or gone — and the running `mount`/`commit` carry a `requires`/`ensures`
boundary tying them to the verified `advance_head`/`replay_bound`/`run_len` decisions.

- **Touches:**
  - `cas/src/store.rs` — **discharge path:** strengthen `replay_bound` `:718` (or a companion
    verified lemma) to establish `laid_out(wal@, <accepted records>, 0)` over its walk; add the
    verified composition wrapper (`gap_free_recovery_core` or similar) that invokes
    `lemma_gap_freedom` `:910` under discharged hypotheses; call it from the `mount` replay
    boundary `:1172-1217` and/or the `commit` `advance_head` boundary `:1766` as a thin
    `requires`/`ensures` contract; update the gap-freedom comment block `:802-821` to say the
    invariant is now *produced* by `replay_bound`, not assumed. **Retire path (fallback):**
    remove `lemma_gap_freedom`, `lemma_run_len_covers` `:873`, `lemma_laid_out_mono` `:853`,
    `laid_out` `:831`; rewrite the comment block to rest the in-code guarantee on the TLA model
    (B7A) + the crash/replay proptest; keep the orchestration contract over the individual
    `advance_head`/`replay_bound` ensures.
  - `doc/guidelines/verus_trusted-base.md` — record T-2's resolution (lemma live vs. retired)
    and the resulting `cargo verus verify -p cas` total `:111`; note the mount/commit
    orchestration boundary now carries the decision contract.
- **Depends on:** Part A blessed; **B7B** (interface-stable, but the `laid_out`/`content_ok_spec`
  reasoning is cleaner and final over the split seam — recommend after B7B). Independent of B7A.
- **Work:** Design decision 4 — attempt the discharge first (strengthen `replay_bound`'s
  ensures to emit laid-out evidence; the composition wrapper; the call-site contract). If
  disproportionate, take the clean retire path. Either way, add the orchestration-boundary
  `requires`/`ensures` so the running recovery sequencing is tied to the proved decisions, and
  record which path was taken.
- **Acceptance:**
  - **No dead proof.** `lemma_gap_freedom` is either **live** (≥ 1 call site, `laid_out`
    discharged) or **absent** — never true-but-unreachable on an unestablished premise. `cargo
    verus verify -p cas --no-default-features` ≥ the B7B total (higher if discharged — the
    now-reachable lemmas count; equal-or-lower-but-still-0-errors if retired), recorded with
    rationale.
  - **Orchestration tied.** The `mount`/`commit` boundary carries a `requires`/`ensures`
    contract over `advance_head`/`replay_bound`/`run_len` so the running recovery code is bound
    to the verified decisions (the §4.2 glue item), with the I/O/applier staying the trusted
    plain-Rust shell rev1§6.1(e) sanctions.
  - `cargo test -p cas` green (the crash-injection proptest backstops the retire path); Miri
    replay clean.
  - The ledger records T-2's resolution and the final verify total.
- **Effort/Risk:** M–L / medium. The discharge is real proof engineering (carrying `laid_out`
  through `replay_bound`'s walk and composing the lemma at a thin verified boundary over
  plain-Rust I/O); the clean retire path is the bounded fallback that still closes "no dead
  proofs" + the glue contract. The judgment call (discharge vs. retire) is the substance.

---

## Execution order

```
B7A  replay-equality action property + fsync ASSUME (TLA only)   [T-1 high; the headline; independent]
B7B  shrink the WAL content seam to BLAKE3-only (Verus)          [T-5 low; independent of B7A]
  └─► B7C  discharge-or-retire lemma_gap_freedom + glue          [T-2 + glue; cleaner over B7B's split seam]
```

- **B7A** is the high-severity T-1 fix and is independently shippable: it mechanizes
  replay-equality in the TLA+ model with a committed, runnable negative control, and names the
  fsync axiom — a complete, mergeable unit on the model surface alone, parallel with all the
  Rust work.
- **B7B** is independent of B7A (Verus vs. TLA) and independently shippable: the seam split
  shrinks the trusted surface to BLAKE3 with no format or behaviour change, raising the verify
  count. Interface-stable w.r.t. B7C.
- **B7C** depends on Part A and is recommended after **B7B** (the `laid_out`/`content_ok_spec`
  reasoning is over the final split seam, same posture as B6B→B6C); it closes T-2 and the
  mount/commit glue, resolving open decision #4 with no dead proof either way. Independent of
  B7A.
- B7A may be reviewed alongside B7B/B7C, but each is a complete, mergeable unit — keep them
  separable so the headline model fix (B7A) can land without waiting on the Verus seam work,
  mirroring B5A/B5B/B5C and B6A/B6B/B6C.

## Out of scope for B7 (recorded so it is not mistaken for a gap)

- **Verifying `mount`/`commit` end-to-end in Verus.** rev1§6.1(e) is explicit that "the commit
  routine itself stays plain Rust over the verified decisions, so the full replay-equality
  invariant is mechanized nowhere on the real code and remains the model's alone." B7 tightens
  the *boundary contract* (B7C) and the *model* (B7A); it does **not** pull the device I/O,
  `Vec`/`BTreeMap` building, or the applier into the verified core. The join between the
  in-memory queue and the on-device bytes stays the trusted-Store seam §6.1(c)/(e) names.
- **Proving BLAKE3 / the superblock checksum.** `checksum_ok` (`disk.rs:339`) and the shrunk
  `wal_content_ok` (B7B) stay `external_body` over interpreted hashing — the irreducible
  trusted primitive rev1§6.1(e) keeps. T-5 shrinks the seam *to* BLAKE3, it does not eliminate
  it; a round-trip/injectivity proof over the hash is not in scope (the ledger records totality
  + determinism only, as today).
- **On-disk format change / `SB_VERSION` bump / corpus regeneration.** None. B7 changes zero
  persistent bytes and zero wire ops; the WAL record encoding is byte-identical. Contrast B5
  (format v4, regenerated corpora); like B6, B7 is format-stable. The committed mount/recovery
  corpora stay valid and keep exercising the same bytes.
- **Concurrent-GC / persisted-marking TLA+ model.** The `CommitProtocol` model is the
  *commit/recovery* protocol; the persisted-incremental-marking model rev1§8.3 calls for is
  **Phase C4**'s, not B7's. B7 adds the `Recover` action property and the fsync axiom to the
  existing model; it adds no GC modeling.
- **Backpressure / IPC-reactor TLA work (T-3, S-12).** That is the `IpcReactor` model's
  surface — **Phase B14**. B7 touches only `CommitProtocol`.
- **The kernel verified-surface work (MAP, priority-ceiling, ready-queue — the other two
  `[verifying]` parts of §6.1).** Those are §6.1(c)/(d) and **Phase B8**; B7 owns only the
  §6.1(e) storage-recovery parts (replay-equality + structural decode + fsync axiom).
- **New crash-injection machinery.** B7 adds no new proptest infrastructure: T-1's atomicity is
  the existing two-barrier flip (already modeled + proptested), and T-2's retire-path guarantee
  rests on the *existing* `crash_recovery_preserves_acked_state` proptest + the WAL-replay
  fuzz corpora. The only new test artifact is the TLA negative-control cfg (B7A).
