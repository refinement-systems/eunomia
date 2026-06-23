# Plan — Part B5 detail: guarded ref-table batches + tags over the wire (per-ref edit version on-disk, conditional `Apply` batch, tag create/delete/list ops, crash-injection + proptest tier)

Detailed, separately-implementable decomposition of **Phase B5** from
`doc/plans/0_address_audit_rev0.md`. B5 is Wave-2 work: the high-severity storage
correctness cluster — the retention read-then-act race (`I-2` / `S-1`) and the
tag mechanism that exists in `Store` but never reaches a client (`M-8`).

**Closes (from the parent plan):**
- `I-2` [high] / `S-1` — there is **no conditional/guarded ref-table batch**, so a
  retention pass (enumerate → compute prune set → delete) races a concurrent snapshot
  or edit between the read and the act; and rev1§8.3 names the remedy as "specified …
  and lands in this revision's work" with no implementation behind it
  (`doc/results/0_audit_rev0.md` §2.1, §5).
- `M-8` [medium] — `Store::tag()` (`cas/src/store.rs:1247`) is reachable only in-process
  (tests and the maintenance path); there is **no wire op** to create, delete, or list a
  tag, so the rev1§4.7 tag trichotomy (refs / snapshot IDs / tags) is unusable over a
  session (`audit` §4.2).

**Spec target (already blessed in rev1 — B5 only conforms code to it):**
- **rev1§4.7 "Guarded ref-table batches"** — *verbatim the mechanism B5 builds*: "Each
  ref carries an **edit version**: a counter advancing on every committed mutation of the
  ref's entries (head moves, snapshot rows, tags), distinct from the §2.2 revocation
  generation. Enumerate operations return the edit version; a guarded batch apply
  (`handle`, `expected_version`, `edits`) applies all-or-nothing within one commit if the
  version still matches, else fails carrying the current version so the caller re-reads.
  The counter is plain data through the normal commit path, and the check is one
  comparison in the single authority over the ref table."
- **rev1§4.7 "Tags"** — "ref-table entries mapping `name → snapshot ID` (not root hash,
  so they survive metadata edits), acting as `keep`-strength pins." Row surgery
  (including snapshot deletion) "all require `may-rewrite-history` on a ref-root handle
  (§2.3)." **"Deleting a snapshot a tag points at fails with `Pinned`."**
- **rev1§2.2** — the **revocation generation** is a *separate* per-ref counter, bumped on
  revoke-all/destruction to invalidate outstanding handles; "plain data [that] persists
  through the normal commit path." B5's edit version must be **orthogonal** to it.
- **rev1§4.2** — "The generation-checksummed A/B superblock flip, preceded by an fsync
  barrier, is the **single atomicity mechanism for the entire system**." The edit version
  and the guarded batch ride this flip; B5 introduces no new atomicity machinery.
- **rev1§8.3** — already reworded in Phase A1 to "the ref-table half — generation-guarded
  batches — is specified (§4.7) and **lands in this revision's work**"; B5 is that work,
  so the S-1 clause becomes backed by code without a further edit.

Because Part A is blessed first (the parent plan's hard dependency), **B5 makes no
normative spec edits** — the rev1 text above is the fixed target, and every citation here
is `rev1§`. The only candidate doc-touch is the optional tense flip of the §8.3 status
clause once B5 merges (the per-phase "flip your own status line" discipline the parent
plan uses for B7/B8 in §6.1); the current future-progressive wording stays accurate
either way, so B5 leaves it to the merging change.

**Primary files:**
- `cas/src/disk.rs` — `RefEntry` `:656-662` (the new field), `RefTable::encode`
  `:689-723` / `RefTable::decode` `:725-789` (the plain-Rust codec), `SB_VERSION` `:139`
  / `WrongVersion` `:232,:366` (the migration knob).
- `cas/src/store.rs` — `commit()` `:1604-1660` (the dirty-ref bump), the ref-table
  mutators (`snapshot` `:1194`, `rollback` `:1231`, `delete_snapshot` `:1741`, set-class
  at `:1784`, `tag` `:1247`, the new `untag`/`tags`/`apply_batch`), `Store` struct
  `:884-897`, the crash tests `:1959-2458`.
- `storage-server/src/lib.rs` — `Request` `:109-199`, `Response` `:220-241`, `ErrorCode`
  `:245-260`, the `ListSnapshots` `:602-618` / `Snapshot` `:585-601` / `DeleteSnapshot`
  handlers and `lookup` `:395-422`.

Secondary: `cas/fuzz/*` (a **new** `ref_table` target + corpus), `cas/tests/fuzz_corpus.rs`,
`storage-server/fuzz/fuzz_targets/structured_request.rs`, `storage-server/tests/*`,
`mkfs/` (inherits format v4 via the cas crate; image fixtures regenerated), and the
`cas`/`storage-server` `Cargo.toml`s.

---

## Verification tier & baseline (applies to all sub-phases)

B5 spans two crates with different routing (rev1§6). **`cas` is a Verus chokepoint with
fuzzed decoders; `storage-server` is a userspace server (Miri + proptest).** Five honesty
notes up front so nothing is silently dropped or over-claimed — one of them corrects the
parent plan:

- **The ref-table codec is plain Rust + fuzz/proptest, NOT Verus — and is not even
  directly fuzzed today.** The parent plan says "keep the ref-table TLV codec proofs
  (`decode_raw`/`encode_raw`) extended to the new field." This conflates two unrelated
  codecs: `decode_raw`/`encode_raw` (`cas/src/prolly.rs:855/:915`) are the **rev1§4.9
  directory-entry** TLV codec, Verus-proven (canonical round-trip ∀); the **ref-table**
  codec is `RefTable::encode`/`decode` (`disk.rs:689-789`), which sits **outside** the
  `verus!{}` block (it closes at `disk.rs:380`) — plain Rust with `FormatError` decode
  discipline (rejects trailing bytes, bad retention-class). It carries **no Verus proof
  and no dedicated fuzz target** (the cas corpus set is `chunker, index_frame,
  mount_recovery, mount_reseal, superblock, superblock_fixup, tlv_entry, tree_node,
  wal_replay_scan, wal_replay_scan_fixup` — none for the ref table; it is reached only
  indirectly through `mount_recovery`). So B5A cannot "keep a proof it never had." The
  honest discharge of the parent plan's intent ("the on-disk format change is a decoder,
  so it stays in the … fuzzed surface, rev1§3.7/§6") is to **add** the ref-table decoder
  to the directly-fuzzed surface: a new `ref_table` cargo-fuzz target + committed corpus +
  corpus replay + proptest + Miri. This is strictly more coverage than today, and it is
  recorded as the deliberate reinterpretation.
- **The Verus gate is held, not raised. `cargo verus verify -p cas --no-default-features`
  must stay ≥ 58/0.** B5 touches no `verus!{}` proof: the `RefEntry` field add and the
  `commit()` bump are plain Rust; the only `verus!{}` edit is bumping the `SB_VERSION`
  literal `3 → 4` (`disk.rs:139`, declared inside the block so the geometry spec can name
  it), which changes no proof obligation — `decode_checked_fields` is total over arbitrary
  bytes for any version literal and its `version != SB_VERSION` refusal (`:366`) keeps
  working. B5A re-runs verify and records 58/0 unchanged.
- **No new atomicity mechanism — the batch rides the one superblock flip.** rev1§4.2 makes
  the A/B flip "the single atomicity mechanism for the entire system." The edit version is
  "plain data through the normal commit path" (rev1§4.7) and the guarded batch is
  validate-all-then-one-`commit()`. Its all-or-nothing property under power loss is
  therefore the **existing** two-barrier commit's property, not a new one. B5 adds no TLA
  obligation (the `CommitProtocol` model and its proofs are B7's surface and are untouched
  — B5 changes no commit *sequencing*, only the payload that flows through it). The
  guarantee is exercised by **extending the existing crash-injection proptest**
  (`store.rs:2217`) with the batch op, not by new crash machinery.
- **Two version axes, moved independently.** The **on-disk** format genuinely changes
  (a fixed-width field is appended to each ref record, which an old reader cannot skip),
  so `SB_VERSION` bumps `3 → 4` and mount of an older image is **refused cleanly** via the
  existing `SbError::WrongVersion` path (`disk.rs:366`) — refuse-not-mis-decode (rev1§4.5).
  The **wire** protocol only *grows*: new `Request`/`Response` variants appended to
  postcard enums keep every existing variant's discriminant stable, so the storage wire
  header stays `[0x45,0x51,0x02]` (`storage-server/src/wire.rs:16`) and the committed
  `request_dispatch` corpus stays valid (the same posture B1 took — B1 kept the header
  fixed because it added no opcode; B5 adds opcodes but append-only). Design decision 1
  pins this and weighs the bump-the-wire alternative.
- **No Loom/Shuttle.** The storage server's `Server::handle` processes one request at a
  time (the `user/storaged` reactor serializes dispatch) and the ref table has a single
  authority (`Store`, single-threaded); there are no atomics and no second mutator for a
  weak-memory model to witness. The guarded-batch CAS-on-version is *logical*
  concurrency-control between sessions, resolved by the single-authority serialization,
  not a memory-ordering protocol. Same posture as B1's rights-lattice note; the server's
  real concurrency surface (the IPC reactor) is Phase B14.

**Baseline to re-establish at end of B5:**
- `cargo test -p cas` and `cargo test -p storage-server` green (including the extended
  crash proptests and the new `ref_table` corpus replay).
- `cargo verus verify -p cas --no-default-features` = **58/0** (held — see above).
- Miri replay clean: the documented sweep grows by the new `ref_table` corpus, which rides
  the existing `--test fuzz_corpus`:
  `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas -p loader
  -p storage-server --test fuzz_regressions --test fuzz_corpus`.
- The aarch64 build still boots: `cd kernel && cargo build` (init/shell/storaged go
  through the storage wire; `storaged` constructs the `Store` over its `DmaRegion`).
- Any committed format-v3 disk-image fixture and the v3 mount corpora are regenerated to
  v4 (Design decision 1 / B5A); the runtime crash-injection proptests build images via
  `format`/`commit`, so they pick up v4 automatically.

---

## Design decision 1 — versioning the format change: bump the on-disk version, keep the wire version *(resolve in B5A)*

Adding `edit_version: u64` to `RefEntry` changes the persistent ref-table encoding and
adds new wire ops. These are two different formats with two different migration stories;
B5A pins both.

- **Adopted (on-disk) — fixed-width append + `SB_VERSION 3 → 4`, refuse-old.** Append the
  8-byte `edit_version` to each ref record (after `next_snap_id`, `disk.rs:697-698`) and
  bump `SB_VERSION` (`disk.rs:139`) to 4. A format-v3 image then fails the version check at
  `decode_checked_fields` (`disk.rs:366`) and mount returns `SbError::WrongVersion(3)` —
  the clean refuse-not-panic path rev1§4.5 already guarantees, with no risk of a v3 ref
  record being mis-read as a (shorter) v4 one. Decisive reasons:
  1. **A fixed-width record has no skip-the-unknown-field affordance.** Unlike the
     rev1§4.9 directory-entry TLV (sorted optional tags, where a new tag is
     backward-compatible by absence), `RefEntry` is a packed fixed layout
     (`root(32) | generation(8) | next_snap_id(8)`); a v3 reader handed a v4 record would
     desync every subsequent record. The version gate is the only safe boundary.
  2. **The mechanism already exists.** `SbError::WrongVersion` (`disk.rs:232`) and the
     `version != SB_VERSION` refusal (`:366`) are verified-total; bumping the literal
     reuses them and the totality proof is independent of the literal's value (so 58/0
     holds — see the verification-tier note).
  3. **No real v3 images to migrate.** `mkfs` rebuilds images from one tree (the parent
     plan's C3 note: both peers ship from one tree), so there is no deployed v3 corpus to
     forward-migrate; refuse-old is sufficient and honest. `mkfs` inherits v4 with no logic
     change (it commits through the cas crate).
- **Adopted (wire) — append `Request`/`Response` variants, keep header `[0x45,0x51,0x02]`.**
  `Request`/`Response`/`ErrorCode` are postcard enums
  (`#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]`,
  `storage-server/src/lib.rs:107,202,219,244`); postcard tags variants by declaration
  order, so **appending** `Apply`, `Tag`, `Untag`, `ListTags` (and the new replies) at the
  *end* leaves every existing op's discriminant — and thus the committed `request_dispatch`
  corpus — unchanged. The storage wire header (`wire.rs:16`) stays at version 2. A peer
  built from an older tree that receives a new-variant message fails postcard decode
  cleanly (unknown variant → `Body` error → refused, never a crash) — the rev1§3.7 decode
  discipline, identical in effect to a header-version tripwire while preserving the corpus.
- **Rejected (wire) — bump the storage wire version `2 → 3`.** The header comment notes
  "v2 carries the history-rewrite opcodes," so there is precedent for bumping when opcodes
  were added. But that bump predated (or bundled) the committed corpus; bumping now would
  make every `request_dispatch` corpus file `BadHeader` at `wire.rs:39`, gutting the
  corpus's dispatch coverage unless every file's first three bytes are re-encoded `0x02 →
  0x03` (a rewrite of committed fuzz history, including the minimized regressions). Since
  version *negotiation* is deferred (Phase C3) and both peers ship from one tree, the
  version byte buys nothing operationally today that postcard's clean unknown-variant
  rejection does not already provide. **Recommendation: keep `0x02`** (matches B1's stated
  preference to preserve the corpus) and rely on append-compatibility; revisit the bump
  when C3 introduces real negotiation. Noted so the omission is a decision, not an
  oversight.

**Recommendation: bump `SB_VERSION 3 → 4` (refuse-old at mount); keep the wire header at
`0x02` (append postcard variants).**

---

## Design decision 2 — where the edit-version bump lives: once per commit per changed ref, via a dirty-set *(resolve in B5A)*

rev1§4.7 says the counter advances "on every committed mutation of the ref's entries (head
moves, snapshot rows, tags)" and "the check is one comparison in the single authority over
the ref table." Two placements; B5A pins the design.

- **Adopted — a `dirty_refs: BTreeSet<Vec<u8>>` on `Store`, drained-and-bumped at the top
  of `commit()`.** Each ref-table mutator records the ref it touched in `dirty_refs`; the
  first thing `commit()` does (before `self.table.encode()` at `store.rs:1605`) is, for
  each name in `dirty_refs`, `self.table.refs.get_mut(name).edit_version += 1`, then clear
  the set. Decisive reasons:
  1. **"Once per commit per changed ref," exactly.** A single commit may batch many edits
     across many refs (rev1§4.3: "A single commit may carry any number of freshly flushed
     ref roots"), and the guarded batch (B5B) applies *several* edits to *one* ref in one
     commit. A per-mutator `+= 1` would over-count the batch (N edits → N bumps); the
     dirty-set collapses them to the one bump §4.7 mandates ("a counter advancing on every
     committed mutation" = one tick per committed entry-set change, the unit the
     `expected_version` compares against).
  2. **Centralizes the rule in the single authority.** The bump lives in the one routine
     that owns the ref table, matching "the single authority over the ref table." Mutators
     only declare *what changed*; `commit()` decides the version arithmetic.
  3. **No-op commits don't advance the counter.** A `sync` with nothing dirty leaves
     `dirty_refs` empty → no bump → `expected_version` stays valid across pure flushes that
     changed no entry. Correct: a flush that does *not* move the head is not an entry-set
     mutation.
  - **What marks a ref dirty:** `snapshot` (row add + `next_snap_id`), `rollback`
    (head move), `delete_snapshot`, set-class (`store.rs:1784`), and B5C's `tag`/`untag`;
    **and the write/flush head-move path** — when a flush re-points a ref's root, that ref
    is marked dirty, so a concurrent writer's commit between a retention daemon's read and
    its `Apply` advances the version and invalidates the batch (the precise race I-2 names).
  - **What does NOT mark it dirty:** `bump_generation` (`store.rs:1435`). Revocation bumps
    the **§2.2 generation**, not the **§4.7 edit version** — they are orthogonal counters
    in the same `RefEntry`. A revoke does not mutate the ref's entry-set (head/rows/tags);
    it invalidates handles. Keeping them independent is load-bearing: a retention batch
    must not be spuriously rejected because someone revoked an unrelated handle, and a
    revoke must not be defeated by an edit-version match.
- **Rejected — `edit_version += 1` inline in each mutator.** Simpler to read at one site
  but over-counts the batch and scatters the arithmetic across six call sites that must all
  agree to bump exactly once; the dirty-set is the single point that makes "one tick per
  committed change" structural.

**Recommendation: adopt the `dirty_refs` set bumped in `commit()`; keep `edit_version`
strictly orthogonal to the `generation` (revocation) counter.**

---

## Design decision 3 — the guarded batch: a `Store::apply_batch` over a row+tag edit vocabulary, validate-all-then-one-commit *(resolve in B5B)*

rev1§4.7 fixes the shape — `apply (handle, expected_version, edits)`, all-or-nothing in one
commit, mismatch "fails carrying the current version." B5B pins the `edits` vocabulary, the
atomicity discipline, and the reply.

- **Adopted — `Store::apply_batch(ref_name, expected_version, edits) -> Result<u64,
  StoreError>` is the single authority; the wire handler is a thin marshaller.** The
  method, in `cas` (the "single authority over the ref table"): (1) reads the ref's current
  `edit_version`; (2) if `!= expected_version`, returns `Err(StoreError::VersionMismatch {
  current })` **without mutating or committing**; (3) **validates every edit against the
  current table first** (snapshot exists, tag→snapshot exists, the rev1§4.7 `Pinned` rule
  for any snapshot-deletion in the batch, retention-class in range) — any invalid edit
  fails the **whole** batch with no mutation; (4) applies all edits to `self.table`, marks
  the ref dirty (Design decision 2 → one version bump), and calls `commit()` **once**;
  returns the new `edit_version`. Decisive reasons:
  1. **All-or-nothing is structural.** Validate-before-mutate makes a partial in-memory
     apply impossible, and the single `commit()` makes the persisted result atomic via the
     one superblock flip (rev1§4.2) — a crash mid-flip leaves the pre-batch superblock
     winning on generation, so no partial batch is ever observable (exercised by the B5B
     crash proptest).
  2. **The check lives where §4.7 puts it.** "One comparison in the single authority over
     the ref table" is literally step (2), in `Store`, not in the server shell.
  3. **The mismatch carries data.** `VersionMismatch { current }` propagates to a
     **data-carrying reply** (`Response::VersionMismatch { edit_version }`), not a bare
     `ErrorCode`, so "fails carrying the current version so the caller re-reads" is honored.
- **Adopted — `edits` is the row+tag-surgery vocabulary, not data writes or head moves.**
  Define `RefEdit` (in `cas`, shared by wire and store):
  `DeleteSnapshot { id }`, `SetClass { id, class }`, `SetParent { id, parent }`,
  `SetMessage { id, message }`, `CreateTag { name, snap_id }`, `DeleteTag { name }` —
  exactly the mutations a retention daemon issues ("mark survivors `keep`, then run the
  policy," rev1§4.7), each already gated by `may-rewrite-history`. **Excluded:** data
  writes (rev1§4.4 is explicit: "no multi-operation transactions" in the memtable; writes
  stay last-write-wins) and `Rollback`/head-moves (a flush-bearing operation with its own
  semantics; the *counter* advances on head moves so a concurrent one invalidates the
  batch, but a head-move is not itself a batchable *edit* in B5). This keeps B5 squarely
  inside the blessed rev1§4.7 and out of the **deferred** rev1§8.3 data-root CAS half ("the
  data-root half is deferred").
- **Adopted — the reply set.** Append to `Response` (`storage-server/src/lib.rs:220`):
  `Applied { edit_version: u64 }` (success, the post-batch version) and
  `VersionMismatch { edit_version: u64 }` (the stale-`expected_version` case, carrying the
  current version). Invalid-edit failures reuse the existing `ErrorCode`
  (`Pinned`/`NoSuchSnapshot`/`Denied`/`Stale`/`BadPath`) via `Response::Err`, with the new
  `StoreError::VersionMismatch` mapped to the `VersionMismatch` reply (not an `ErrorCode`,
  since it carries the version).
- **Rejected — a free-form "apply this opaque ref-table delta" op.** Maximally general but
  unbounded and un-validatable as a unit (the server could not enforce `Pinned`/rights
  per-edit before commit); the typed `RefEdit` enum keeps each edit individually authorized
  and validated, and keeps the wire decoder total (rev1§3.7).
- **Rejected — applying each edit through its own existing handler + commit.** Would
  re-use `delete_snapshot`/`set_class` as-is but commits *per edit* (N flips, non-atomic)
  and re-checks the version only once at the top — defeating all-or-nothing. The batch must
  be one staged apply + one commit.

**Recommendation: adopt `Store::apply_batch` with the typed `RefEdit` row+tag vocabulary,
validate-all-then-one-commit, and the data-carrying `VersionMismatch` reply.**

---

## Sub-phase B5A — per-ref edit version (on-disk format v4): the field, the commit bump, the codec, and the ref-table fuzz tier *(closes the I-2 mechanism prerequisite)*

The foundation. Self-contained and mergeable alone: after B5A every ref carries a
correctly-advancing `edit_version`, persisted across mount, observable via `ListSnapshots`,
and the ref-table decoder is in the directly-fuzzed surface — but no guarded batch or tag
wire op yet (the version is inert until B5B/B5C consume it, and is fully tested in
isolation). Atomic by necessity: the field, its bump, and its codec must land together or
the format is inconsistent.

- **Touches:**
  - `cas/src/disk.rs` — add `pub edit_version: u64` to `RefEntry` `:656-662` (doc it as the
    rev1§4.7 edit version, "distinct from `generation` (§2.2 revocation) and from the
    superblock generation (§4.2)"); append the 8-LE field in `RefTable::encode` `:697-698`
    and read it in `RefTable::decode` `:725-789` (one `r.take_u64()?` after `next_snap_id`,
    keeping the trailing-byte/`FormatError` discipline); bump `SB_VERSION 3 → 4` `:139`.
  - `cas/src/store.rs` — add `dirty_refs: BTreeSet<Vec<u8>>` to `Store` `:884-897`; a
    private `fn touch_ref(&mut self, name: &[u8])`; the drain-and-bump loop at the top of
    `commit()` `:1605`; call `touch_ref` from `snapshot` `:1194`, `rollback` `:1231`,
    `delete_snapshot` `:1741`, the set-class site `:1784`, and the write/flush head-move
    path; **do not** call it from `bump_generation` `:1435`. Initialize `edit_version: 0`
    wherever a `RefEntry` is created (ref creation / `format`).
  - `storage-server/src/lib.rs` — change `Response::Snapshots(Vec<SnapInfo>)` `:233` to a
    struct variant `Snapshots { snaps: Vec<SnapInfo>, edit_version: u64 }`; the
    `ListSnapshots` handler `:602-618` reads the ref's current `edit_version` and returns it
    alongside the rows (so a daemon's enumerate and its later `expected_version` come from
    one atomic read). (Replies are server→client and are **not** in the `request_dispatch`
    corpus, so this changes no committed fuzz input.)
  - `cas/fuzz/` — a **new** `ref_table` target (`fuzz_targets/ref_table.rs` + a `[[bin]]`
    in `cas/fuzz/Cargo.toml`): `RefTable::decode(data)`; on `Ok(t)`, assert
    `RefTable::decode(t.encode()) == Ok(t)` (canonical round-trip) and that a
    deliberately-non-minimal re-encode is rejected (matching the `tlv_entry`/`tree_node`
    oracle style). Seed `cas/fuzz/corpus/ref_table/` from a corpus-gen path.
  - `cas/tests/fuzz_corpus.rs` — a `ref_table()` replay test (mirrors `tlv_entry` `:34`),
    so the new corpus rides the documented `--test fuzz_corpus` Miri sweep.
  - **Regenerate** the committed v3 mount corpora (`mount_recovery`, `mount_reseal`) and any
    committed disk-image fixture to format v4, so they still exercise the deep mount path
    instead of being refused at the version check; keep one v3 input as an explicit
    `WrongVersion` refuse-case if a regression slot is wanted. (The runtime crash proptests
    build images via `format`/`commit`, so they need no corpus change.)
  - `mkfs/` — no code change; confirm it emits v4 (it commits through the cas crate) and
    regenerate any golden image it produces for tests.
- **Depends on:** Part A blessed (rev1§4.7/§4.2 text). No intra-B5 dependency.
- **Work:**
  1. The field + codec (fixed-width append; the migration knob):
     ```rust
     pub struct RefEntry {
         pub root: Hash,
         /// Storage-cap revocation generation (rev1§2.2) — not the superblock
         /// generation, and not the edit version below.
         pub generation: u64,
         pub next_snap_id: u64,
         /// rev1§4.7 edit version: advances once per committed mutation of this
         /// ref's entries (head moves, snapshot rows, tags). Plain data through
         /// the normal commit path; the guarded-batch CAS compares against it.
         /// Orthogonal to `generation` (revocation).
         pub edit_version: u64,
     }
     ```
     `encode`: `out.extend_from_slice(&e.edit_version.to_le_bytes());` after `next_snap_id`.
     `decode`: `let edit_version = r.take_u64()?;` after `next_snap_id`, keeping the
     existing `FormatError`/trailing-byte checks. Bump `SB_VERSION` to 4.
  2. The bump (Design decision 2):
     ```rust
     // top of commit(), before self.table.encode():
     for name in core::mem::take(&mut self.dirty_refs) {
         if let Some(e) = self.table.refs.get_mut(&name) {
             e.edit_version += 1;
         }
     }
     ```
     and a one-line `self.dirty_refs.insert(name.to_vec());` in each entry-set mutator.
- **Acceptance (tests in `cas/src/store.rs` `mod tests` + `storage-server/tests/`):**
  - **Monotone, per-mutation, persisted.** A fresh ref has `edit_version == 0`; one
    `snapshot` → 1; one `rollback` → 2; one `delete_snapshot` → 3; a write+flush that moves
    the head → +1; remount and read back the same value (persistence through `commit`).
  - **One bump per commit, not per edit.** (Anticipates B5B but tests the bump rule now:)
    a commit that stages two edits on one ref bumps `edit_version` by exactly 1.
  - **Orthogonal to generation.** `bump_generation` (revoke) advances `generation` and
    leaves `edit_version` unchanged; a `snapshot` advances `edit_version` and leaves
    `generation` unchanged.
  - **No-op sync.** A `Sync` with nothing dirty leaves `edit_version` unchanged.
  - **`ListSnapshots` surfaces it.** The reply carries the ref's current `edit_version`,
    equal to the value a subsequent direct read returns.
  - **Codec / format.** `RefTable::decode(encode())` round-trips with the field; a truncated
    record (missing the 8 version bytes) → `FormatError`; mounting a hand-built v3 superblock
    → `Err(SbError::WrongVersion(3))` (refuse-not-panic). The new `ref_table` corpus replays
    clean under Miri.
  - `cargo verus verify -p cas --no-default-features` = **58/0** (record it); `cd kernel &&
    cargo build` still boots.
- **Effort/Risk:** M / medium. The on-disk format change is the risk (migration discipline
  + corpus regeneration); the field and bump are small. Medium because it touches the
  persistent format at the one place reality is defined (the ref table).

---

## Sub-phase B5B — guarded ref-table batches: `Store::apply_batch` + `Request::Apply` + crash/proptest tier *(closes I-2 / S-1)*

The headline race fix. Depends on B5A (needs the `edit_version` to compare). After B5B a
retention pass is conditional: it enumerates (getting the version), computes a prune set,
and `Apply`s it; a concurrent snapshot/edit/write between read and act has advanced the
version, so the batch is rejected with the current version and the caller re-reads —
closing the read-then-act window I-2 names.

- **Touches:**
  - `cas/src/store.rs` — add `pub enum RefEdit { … }` and
    `pub fn apply_batch(&mut self, ref_name, expected_version, edits) -> Result<u64,
    StoreError>` (Design decision 3); add `StoreError::VersionMismatch { current: u64 }`
    (next to `Pinned` `:65`). `apply_batch` reuses the existing validators: the `Pinned`
    check from `delete_snapshot` `:1748-1754`, the snapshot-existence check, the
    retention-class range; it stages all edits, `touch_ref`s once, and `commit()`s once.
  - `storage-server/src/lib.rs` — append `Request::Apply { handle: HandleId,
    expected_version: u64, edits: Vec<RefEdit> }` `:199`; append `Response::Applied {
    edit_version: u64 }` and `Response::VersionMismatch { edit_version: u64 }` `:241`; a
    handler that `lookup`s the handle with the `may-rewrite-history` right (the same
    `rewrite_target`/right the `Gc`/`DeleteSnapshot` handlers use — confirm the constant)
    and a `HandleTarget::Ref`, then marshals into `store.apply_batch`, mapping
    `VersionMismatch` to the reply and other `StoreError`s through the existing `ErrorCode`
    map (`:872`).
  - `storage-server/fuzz/fuzz_targets/structured_request.rs` — add `Apply` (with a small
    `RefEdit` vector) to the structured generator so dispatch of the new op is fuzzed; seed
    one `request_dispatch` corpus file for it (append-only, so existing seeds stay valid).
  - `cas/src/store.rs` crash tests `:2217-2327` — extend `crash_recovery_preserves_acked_state`'s
    op set with an `Apply` operation.
- **Depends on:** B5A (the `edit_version` field + bump + `dirty_refs`).
- **Work:**
  - `apply_batch` per Design decision 3 (version check → validate-all → stage-all →
    `touch_ref` once → `commit` once → return new version). Mismatch returns
    `Err(VersionMismatch { current })` with **no** mutation/commit.
  - The server handler: deny-by-default on the right; `HandleTarget::Ref` only; thin marshal.
- **Acceptance:**
  - **The race is closed (the I-2 test).** A two-session test: session X enumerates
    (`edit_version = v`); session Y snapshots (advancing the ref to `v+1`); X's `Apply
    { expected_version: v, edits }` → `Response::VersionMismatch { edit_version: v+1 }` with
    **no** edit applied; X re-reads and re-`Apply`s at `v+1` → `Response::Applied
    { edit_version: v+2 }`, edits now applied. This is the documented remedy demonstrably
    closing the read-then-act race.
  - **All-or-nothing.** A batch whose third edit is invalid (deletes a `Pinned`/tagged
    snapshot, or a nonexistent id) → `Err(Pinned)`/`Err(NoSuchSnapshot)` with **none** of
    the batch applied and `edit_version` unchanged (no commit).
  - **Atomic under crash.** The extended `crash_recovery_preserves_acked_state` proptest
    (64 cases native / 4 Miri) with `Apply` in the op set: after any crash point, the batch
    is observed **all-or-nothing** — an acked `Applied` survives whole, an un-acked one
    leaves the pre-batch state whole; never a partial batch (the invariant rides the
    existing two-barrier commit, no new machinery).
  - **Fuzz/Miri.** `structured_request` dispatches `Apply` without panic; the
    `request_dispatch` corpus (header `0x02`, existing seeds) still decodes and dispatches;
    Miri replay clean.
  - `cargo verus verify -p cas --no-default-features` = 58/0 (apply_batch is plain Rust).
- **Effort/Risk:** M / medium. The logic is small and rides existing commit/validators;
  the crash-atomicity argument and the two-session race test are the substance.

---

## Sub-phase B5C — tags over the wire: create / delete / list + `Pinned` enforcement over a session *(closes M-8)*

Independent of B5B (both depend only on B5A; both append to the same enums without
conflict — coordinate the variant order). After B5C the rev1§4.7 tag trichotomy is usable
over a session: a client with `may-rewrite-history` can name memorable snapshots, and a
tagged snapshot cannot be deleted out from under its tag.

- **Touches:**
  - `cas/src/store.rs` — `tag` `:1247` already exists (create); add
    `pub fn untag(&mut self, name) -> Result<(), StoreError>` and
    `pub fn tags(&self) -> impl Iterator<Item = (&[u8], &[u8], u64)>` (over
    `self.table.tags`, `disk.rs:684`); `tag`/`untag` `touch_ref` the affected ref (tags are
    a §4.7 entry-set mutation → advance the edit version). The `Pinned` rule is **already
    enforced** in `delete_snapshot` `:1748-1754` and mapped at the server (`lib.rs:872`),
    so B5C does not re-implement it — it only ensures the new tag ops keep it correct
    (creating a tag pins; deleting the tag unpins; then the snapshot deletes).
  - `storage-server/src/lib.rs` — append `Request::Tag { handle, name, snap_id }`,
    `Request::Untag { handle, name }`, `Request::ListTags { handle }` `:199`; append
    `Response::Tags(Vec<(Vec<u8>, Vec<u8>, u64)>)` `:241`; handlers gated by
    `may-rewrite-history` for `Tag`/`Untag` (row surgery is uniformly privileged, rev1§4.7)
    and `R_READ`/`R_ENUMERATE` for `ListTags`, each `HandleTarget::Ref`, routing to
    `store.tag`/`untag`/`tags`. `NoSuchSnapshot` from `tag` maps through the existing
    `ErrorCode`.
  - `storage-server/fuzz/fuzz_targets/structured_request.rs` — add `Tag`/`Untag`/`ListTags`
    to the generator; append corpus seeds.
- **Depends on:** B5A (tag ops advance the edit version). Independent of B5B.
- **Work:** the three handlers + `untag`/`tags` in `Store`; confirm the `may-rewrite-history`
  right constant (the one `delete_snapshot`/`Gc` already require) and apply it deny-by-default.
- **Acceptance (tests in `storage-server/tests/`, extending the session tests, and a Store
  proptest):**
  - **Round-trip over a session.** `Tag{name, snap_id}` → `Ok`; `ListTags` → contains
    `(name, ref, snap_id)`; `Untag{name}` → `Ok`; `ListTags` → absent.
  - **Pinned enforcement over the wire.** Tag a snapshot, then `DeleteSnapshot` it →
    `Err(Pinned)`; `Untag`, then `DeleteSnapshot` → `Ok` (the rev1§4.7 "delete the tag
    first" flow, end-to-end). Tagging survives a metadata edit (set-message/set-class on the
    snapshot) — the tag points at the **ID**, not a hash (rev1§4.7).
  - **Rights.** `Tag`/`Untag` without `may-rewrite-history` → `Err(Denied)`; with it → `Ok`.
    `Tag` of a nonexistent snapshot → `Err(NoSuchSnapshot)`.
  - **Edit version.** A `Tag`/`Untag` advances the ref's `edit_version` (so a concurrent
    guarded batch sees the change); covered by a proptest interleaving tag ops with `Apply`.
  - **Pin/unpin round-trip proptest** (Store-level, `cas/src/store.rs` `mod tests`,
    extending `delete_snapshot_repoints_parents_and_respects_tag_pins` `:2119`): random
    tag/untag/delete interleavings never delete a pinned snapshot and never strand a tag on
    a deleted snapshot. Miri case-count convention (`cases: if cfg!(miri) { 4 } else { 256 }`).
  - `cargo verus verify -p cas --no-default-features` = 58/0; Miri replay clean.
- **Effort/Risk:** S–M / low–medium. Mostly wire-up over an already-enforced `Pinned`
  rule; the new `Store` methods are small map operations.

---

## Execution order

```
B5A  per-ref edit version + format v4 + ref_table fuzz tier   [foundation; on-disk format; do first]
  ├─► B5B  guarded batch (Store::apply_batch + Request::Apply) [needs edit_version; the I-2 fix]
  └─► B5C  tags over the wire + Pinned enforcement            [needs edit_version; independent of B5B]
```

- **B5A** is the load-bearing foundation and is independently shippable: it adds the
  edit-version field, its commit bump, the format-v4 migration, and brings the ref-table
  decoder into the fuzzed surface — a complete, mergeable unit whose new state is inert and
  fully tested in isolation until B5B/B5C consume it.
- **B5B** depends on B5A (it compares `expected_version` against the field and rides the
  bump). It is the high-severity I-2/S-1 fix and is mergeable once B5A lands.
- **B5C** depends on B5A (tag ops advance the edit version) and is **independent of B5B**;
  the two append disjoint variants to `Request`/`Response` — pick a variant order so they
  don't collide, then they can land in either order.
- B5B and B5C may be reviewed together, but each is a complete unit; keep them separable so
  the high-severity race fix (B5B) can land without waiting on the tag wire-up.

## Out of scope for B5 (recorded so it is not mistaken for a gap)

- **Data-root transactional commits (compare-and-set on a data root).** rev1§8.3 keeps "the
  data-root half … deferred"; B5 implements only the **ref-table** half (rev1§4.7). The
  `RefEdit` vocabulary is row+tag surgery only; data writes stay last-write-wins (rev1§4.4
  "no multi-operation transactions"). Adding writes/head-moves to the batch is future work,
  not a B5 gap.
- **Verus over the ref-table codec.** The ref-table codec is the plain-Rust, decode-
  disciplined boundary (not the §4.9 directory-entry codec, which *is* verified as
  `decode_raw`/`encode_raw`). B5 brings it under **fuzz + proptest + Miri** (a new
  `ref_table` target), the rev1§3.7/§6 tier for a decoder of trusted-but-corruptible
  on-disk bytes; pulling `RefTable::decode` into `verus!{}` is a possible later tightening
  (cf. B7's shrink-the-seam direction for the WAL/superblock decoders), not a B5 obligation.
  The `cargo verus verify -p cas` gate is held at 58/0, not raised.
- **TLA / commit-protocol changes.** B5 changes no commit *sequencing* — the edit version
  and the batch are payload through the existing flip (rev1§4.2). The `CommitProtocol`
  model and its proofs are B7's surface and are untouched; B5 adds no TLA obligation. The
  batch's crash-atomicity is the existing two-barrier commit's property, exercised by
  extending the crash proptest.
- **Wire version negotiation.** Keeping the header at `0x02` (append-only variants) relies
  on postcard discriminant stability and the rev1§3.7 unknown-variant refusal; real
  multi-version negotiation is **Phase C3**. B5 records the decision (Design decision 1) so
  the un-bumped version is a choice, not an oversight.
- **Per-session edit-version clamps / quotas.** The edit version is a per-ref content
  counter, not an authority; per-session views and quotas are rev1§8.3 future work, as for
  `statfs` (cf. B1's out-of-scope note).
- **Rename / ephemeral file-id indirection.** The other storage-protocol surface change the
  audit flags is **Phase C2** (rev1§4.9 runtime file identity); it coordinates with B5 on
  the `Request` enum but is a distinct phase. B5 touches only ref-table entries, not the
  overlay file-id keying.
- **Loom/Shuttle for the server.** Deliberately omitted (single-authority ref table,
  serialized dispatch, no atomics) per the verification-tier note; the guarded-batch CAS is
  logical concurrency control resolved by serialization, not a memory-ordering protocol.
  The reactor's concurrency surface is Phase B14.
