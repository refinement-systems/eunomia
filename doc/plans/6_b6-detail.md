# Plan — Part B6 detail: GC correctness (resurrection mechanism + bounded mark walk + GC fuzz/sufficiency tier)

Detailed, separately-implementable decomposition of **Phase B6** from
`doc/plans/0_address_audit_rev0.md`. B6 is Wave-2 work: the GC correctness cluster —
the spec's named **dedup-resurrection** mechanism that exists nowhere in the code
(`I-3` [high]), the unbounded recursive mark walk that overflows the native stack on a
deep tree (`audit §4.2` [medium]), and the GC paths that are entirely unfuzzed.

**Closes (from the parent plan):**
- `I-3` [high, confirmed] — rev1§4.6 step 3 mandates the resurrection fix as an
  *always-present* mechanism: *"during sweep, a dedup lookup that hits an unmarked chunk
  is treated as a miss, so the chunk is rewritten under the same hash, replacing the
  index entry. This confines all GC/mutator interaction to one point."* `ChunkStore::put`
  (`cas/src/store.rs:313-341`) does a plain `index.contains_key` (`:315`) with **no
  mark/condemned-set consultation**. It is benign today because GC is fully synchronous
  (`Store::gc`, `:1800-1839`), so no chunk is born between mark and sweep — but the spec's
  named mechanism is absent, the birth-generation "live by fiat" filter (`:1821`,
  `!live.contains(h) && e.birth < epoch`) is consequently vacuous (the code's own comment
  at `:1805-1807` admits it), and the simplification is **not on the recorded
  MVP-simplification list** at `cas/src/store.rs:20-32` (`doc/results/0_audit_rev0.md`
  §2.1, lines 160-173).
- **GC mark/sweep unverified + stack-overflow** [audit §4.2, medium] — `gc::mark`
  (`cas/src/gc.rs:21-54`) recurses on directory children and nested dir-roots with **no
  depth bound**, so a pathologically deep (or adversarial) directory tree overflows the
  native stack — a crash *inside* the storage server, contra rev1§4.8's "detects
  corruption on read." No `requires`/`ensures` on `mark`/sweep; mark-set sufficiency is a
  single hand-built test oracle (`gc.rs:90-121`, genuinely real) rather than a proof
  (`doc/results/0_audit_rev0.md` §4.2, lines 451-457).
- **GC paths unfuzzed** [audit §4.2] — the cas corpus set (`chunker, index_frame,
  mount_recovery, mount_reseal, ref_table, superblock, superblock_fixup, tlv_entry,
  tree_node, wal_replay_scan, wal_replay_scan_fixup`) has **no target that drives the GC
  mark walk over adversarial tree shapes**; `parse_node` is fuzzed (`tree_node`) but the
  *walk* over hostile structure (deep nesting, wide fanout, shared/missing subtrees) is
  not.

**Spec target (already blessed in rev1 — B6 only conforms code to it):**
- **rev1§4.6 "Garbage collection"** — the four-step mechanism (root set / mark / concurrency
  / sweep). Step 3 is *verbatim the resurrection mechanism B6 installs*: "chunks written
  during GC are live by fiat (checkable via birth generation, §4.2). The one subtle hazard
  is **dedup resurrection** — a new flush index-hits a chunk the marker has already
  condemned. The fix: during sweep, a dedup lookup that hits an unmarked chunk is treated
  as a miss, so the chunk is rewritten under the same hash, replacing the index entry. This
  confines all GC/mutator interaction to one point." Step 4 (sweep) keeps "delete index
  entries for unmarked hashes **whose birth generation predates the GC epoch**" — the
  birth-gen filter B6 preserves and documents.
- **rev1§4.2** — the **birth generation** (superblock generation at append time) "makes
  'older than the GC epoch' well-defined, makes the live-by-fiat GC rule checkable, and is
  the hook for incremental GC and birth-time pruning (§4.6)." Already on disk as
  `IndexEntry.birth` (`cas/src/disk.rs:393-397`), so B6 needs **no format change**. Also the
  **deferred-reuse law** ("no extent freed by commit N may be reused until N's second
  barrier has landed … a crash plus a dedup index-hit could resurrect overwritten bytes") —
  the very hazard the put-side check guards once GC is concurrent.
- **rev1§4.8 "Integrity"** — "Every layer self-verifies … The storage server **detects
  corruption on read**." A stack overflow inside the mark walk is an uncontrolled fault, not
  a detect-on-read refusal, so bounding the walk (refuse/complete, never fault) is rev1§4.8
  conformance.
- **rev1§8.3** — concurrent/incremental GC, persisted marking, and streaming WAL replay are
  **deferred** (Phase C4). Crucially, the polarity note: *"If a Bloom filter ever replaces
  the exact mark set, … the resurrection check (§4.6) must not trust Bloom positives, so
  during sweep it must consult the **exact deletion-candidate list** instead."* B6 consults
  exactly that list (the condemned set), making the C4-direction explicit — see Design
  decision 1.
- **rev1§6 / §6.1** — verification routing. GC reachability is a global graph invariant over
  a content-addressed store, not a bounded-arithmetic decode; it is delivered at the
  "everything gets Miri + proptest" oracle tier (strengthened proptest + the new fuzz
  target), with the walk's termination/bound guaranteed *structurally*. See Design
  decision 3.

Because Part A is blessed first (the parent plan's hard dependency), **B6 makes no
normative spec edits** — the rev1 text above is the fixed target, and every citation here
is `rev1§`. The one doc-touch B6 *does* make is to the **recorded MVP-simplification list**
(`cas/src/store.rs:20-32`, in-code, not spec): the audit's standing complaint is that
"GC is synchronous, resurrection fix absent" is *not on that list*. B6 installs the
mechanism **and** records the residual simplification (GC is synchronous; the mechanism is
present but inert until C4) — closing the gap from both ends.

**Primary files:**
- `cas/src/store.rs` — `ChunkStore` struct `:168-186` (the new in-memory `condemned` field),
  `ChunkStore::put` `:313-341` (the condemned-aware dedup branch) and its dedup comment
  `:316-321`, the two `ChunkStore { … }` constructors `:956`/`:1061` (field init),
  `Store::gc` `:1800-1839` (the sweep-window populate/clear; the birth-gen filter `:1821`
  and its admitting comment `:1805-1807`), the MVP-simplification list `:20-32`, the GC tests
  `:2252` (`gc_reclaims_…`), `:2295` (`snapshots_pin_…`), `:2549` (`crash_mid_gc_loses_no_data`).
- `cas/src/gc.rs` — `mark` `:21-54` (the unbounded recursion → work-stack), the mark-set
  sufficiency oracle/test `:69-121` (`LiveOnly` + `mark_set_is_sufficient_to_read_everything`).

Secondary: `cas/fuzz/` (a **new** `gc_mark` target + corpus + `[[bin]]`),
`cas/tests/fuzz_corpus.rs` (a `gc_mark()` replay), `cas/src/file.rs:70` (`chunk_list_entries`
— already total, no change), `cas/src/prolly.rs:433` (`parse_node` — already total/fuzzed,
no change). No on-disk format change, so **no `mkfs`/corpus regeneration** (contrast B5).

---

## Verification tier & baseline (applies to all sub-phases)

Per rev1§6 routing, **cas is a Verus chokepoint with fuzzed decoders**. Five honesty notes
up front so nothing is silently dropped or over-claimed:

- **Format-stable: no `SB_VERSION` bump, no corpus regeneration.** Unlike B5 (which appended
  a fixed-width field to each ref record and bumped `SB_VERSION 3 → 4`), B6 changes **no
  on-disk bytes**. The only on-disk GC hook — `IndexEntry.birth` (`disk.rs:396`) — has
  existed since rev0 and is already persisted in the index frame. The resurrection
  mechanism's state (`condemned`) is an **in-memory, transient** `BTreeSet<Hash>` on
  `ChunkStore`: empty at every commit boundary, populated only inside a `gc()` sweep window,
  cleared after the sweep commit. It is never serialized, so the index-frame codec, the
  `index_frame` fuzz corpus, and the committed mount corpora are all untouched, and no
  `WrongVersion` migration is involved.
- **The Verus gate holds at 58/0 through B6A/B6B and *rises* in B6C.** B6A and B6B touch **no
  `verus!{}` proof**: the `store.rs` verified surface is the recovery decision core
  (`pick_survivor`/`commit_target`/`advance_head`/`replay_bound`, the two `verus! { … }` blocks at
  `:362-774` and `:797-922`); `ChunkStore` (`:168`), `ChunkStore::put` (`:313`), `Store::gc`
  (`:1800`), and all of `gc.rs` sit **outside** those blocks — plain Rust. The new `condemned`
  field, the put branch, and the work-stack mark add no proof obligation, so B6A/B6B re-run verify
  and record **58/0 unchanged**. **B6C is the exception:** it adds a new verified worklist-driver
  core (Design decision 3) that the work-stack mark is refactored to drive, so
  `cargo verus verify -p cas --no-default-features` rises **above 58** — B6C records the new total
  and updates the trusted-base ledger. No existing proof is weakened; the gate is a floor, and B6C
  raises it.
- **GC reachability is delivered at the oracle tier, not by Verus — and that is the honest
  routing.** "Every object reachable from a live root is in the mark set" is a global
  reachability invariant over a content-addressed object graph; mechanizing it in Verus would
  require modeling `parse_node`, `chunk_list_entries`, and the whole store in spec, and would
  drag `Hash` into the verified core — the exact thing the recovery core is deliberately
  structured to avoid (`store.rs:359`, `prolly.rs:592`: `Hash` is kept out so the round-trip
  theorems live on the `[u8;32]`-carrying raw forms). So B6 delivers sufficiency at the
  rev1§6 "everything gets Miri + proptest" tier — a **strong randomized proptest** (B6C) plus
  a **cargo-fuzz target** (B6B), both Miri-replayed — and guarantees the walk's
  termination/bound **structurally** (Design decision 2). This mirrors B4's trusted-seam line
  (FreeList verified, the raw-pointer wrapper Miri+proptest) and B7's posture (mount/commit
  stay plain Rust over verified decision cores). Stated so the test-routed property is not
  mistaken for a mechanized one (the §6.1 discipline).
- **GC stays synchronous — B6 installs the mechanism, it does not turn on concurrency.**
  rev1§8.3 defers concurrent/incremental GC to Phase C4; the broader "GC must be concurrent"
  reading was *refuted* by the audit (rev0§2.6). B6 installs the **single GC/mutator
  interaction point** (the put-side resurrection check) and keeps its sweep-side complement
  (the birth-gen filter) present, so that C4 can enable concurrency with both halves already
  in place and tested. Under B6 both halves are **structurally correct but inert** (no put
  interleaves a synchronous sweep); the new state is exercised by tests that inject the
  interaction directly. This is recorded on the MVP-simplification list (B6A).
- **No Loom/Shuttle.** GC runs synchronously inside a single-authority `Store` (single-
  threaded; the `user/storaged` reactor serializes dispatch); there are no atomics and no
  second mutator for a weak-memory model to witness. The resurrection check is *logical*
  GC/mutator interaction resolved by serialization today, not a memory-ordering protocol; its
  concurrent form (and the persisted-incremental-marking TLA+ model rev1§8.3 calls for) is
  C4's surface. Same posture as B4/B5; the reactor's real concurrency surface is B14.

**Baseline to re-establish at end of B6:**
- `cargo test -p cas` green (existing GC tests `gc_reclaims_…` `:2252`, `snapshots_pin_…`
  `:2295`, `crash_mid_gc_loses_no_data` `:2549`, `mark_set_is_sufficient_to_read_everything`
  `gc.rs:90`, plus the new resurrection tests, the deep-chain regression, and the
  strengthened sufficiency proptest).
- `cargo verus verify -p cas --no-default-features`: **58/0** through B6A/B6B (held), then **> 58/0**
  after B6C adds the verified worklist-driver core (record the new total in the ledger — see above).
- Miri replay clean: the documented sweep grows by the new `gc_mark` corpus, which rides the
  existing `--test fuzz_corpus`:
  `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas -p loader
  -p storage-server --test fuzz_regressions --test fuzz_corpus`.
- The aarch64 build still boots: `cd kernel && cargo build` (`storaged` constructs the `Store`
  and runs GC over its `DmaRegion`-backed device; B6 changes no signatures).

---

## Design decision 1 — installing the resurrection mechanism: consult the *exact condemned set* via an in-memory `ChunkStore` field *(resolve in B6A)*

rev1§4.6 step 3 fixes the mechanism — "a dedup lookup that hits an unmarked chunk is treated
as a miss, so the chunk is rewritten under the same hash, replacing the index entry … confines
all GC/mutator interaction to one point." B6A pins *which set* `put` consults, *where* it lives,
the rewrite semantics, and the disposition of the birth-gen filter.

- **Adopted — a transient `condemned: BTreeSet<Hash>` on `ChunkStore`, populated for the sweep
  window, consulted by `put`.** Add the field to `ChunkStore` (`:168`), init `BTreeSet::new()`
  in both constructors (`:956`/`:1061`). `Store::gc` computes the condemned set as today, copies
  its hashes into `self.chunks.condemned` **before** the sweep removal, performs the sweep,
  `commit()`s, then clears `condemned`. `ChunkStore::put` gains one branch:
  ```rust
  fn put(&mut self, bytes: &[u8]) -> Hash {
      let hash = Hash::of(bytes);
      if self.index.contains_key(&hash) && !self.condemned.contains(&hash) {
          // Dedup (rev1§4.3): a live index hit.
          return hash;
      }
      // Either a true miss, OR a hit on a *condemned* chunk (rev1§4.6 step 3):
      // treat the condemned hit as a miss and rewrite under the same hash at the
      // current birth_gen (>= epoch, so the rewrite is never re-condemned). The
      // condemned set is the exact deletion-candidate list (rev1§8.3 polarity).
      self.condemned.remove(&hash); // resurrected: cancel its condemnation
      /* …existing miss path: alloc fresh extent, write frame at birth_gen,
         index.insert(hash, IndexEntry{ off, len, birth: self.birth_gen }) … */
  }
  ```
  Decisive reasons:
  1. **It is the spec's named mechanism, at the one chokepoint.** Every dedup decision in the
     system flows through `ChunkStore::put`'s `contains_key`; adding the condemned check there
     "confines all GC/mutator interaction to one point" (rev1§4.6) literally. The hot path pays
     only a `!self.condemned.is_empty()` short-circuit when no sweep is in flight (a `BTreeSet`
     `is_empty` is O(1); gate the `.contains` behind it).
  2. **The *exact* condemned set, not the mark set, not a Bloom filter.** rev1§8.3 warns that
     the resurrection check "must consult the exact deletion-candidate list." The condemned set
     (`!live && birth < epoch`, the rows `gc` is about to delete) *is* that list. Consulting the
     full mark `live` set would also be logically correct (a hit on any unmarked chunk), but the
     condemned set is the smaller structure and is the polarity-safe object the C4 incremental-
     marking design is written around — adopting it now makes the C4 direction explicit rather
     than re-litigated.
  3. **Rewrite semantics are crash-safe by the existing rules.** The rewrite allocates a *fresh*
     extent at the current `birth_gen` and replaces the index entry; the **old** condemned extent
     is still freed by the sweep (it is recorded in the sweep's `pending_free`, and the index no
     longer points at it after the replace), so there is no double-reference and no early reuse —
     the rev1§4.2 deferred-reuse law (freed extents reusable only after the next barrier) is
     unchanged. No new atomicity machinery; the sweep still rides the one superblock flip.
- **Adopted — keep the birth-generation "live by fiat" filter; document its current inertness.**
  Leave the `e.birth < epoch` clause in the sweep (`:1821`). It is the **sweep-side complement**
  of the put-side check: under concurrent GC (C4), a chunk written after the epoch was fixed has
  `birth >= epoch` and is therefore never condemned, even though the mark walk (which ran before
  it existed) did not see it. It is spec-mandated (rev1§4.6 step 4) and C4 needs it. Under
  synchronous GC it is **inert** — `gc` does `sync_all()` first, so `epoch = birth_gen` and every
  existing chunk has `birth < epoch` (the audit's vacuity) and no chunk is born during the cycle.
  Removing it would only force its re-addition in C4. **Decision: keep it, refresh the comment at
  `:1805-1807`** to say the filter and the put-side check are *installed and structurally
  correct, load-bearing once C4 makes GC concurrent, inert under today's synchronous cycle* — and
  record that on the MVP-simplification list.
- **Rejected — leave `put` as plain `contains_key` and only *disclose* the simplification.** The
  audit's lower-effort alternative: add "GC is synchronous; the resurrection fix is not
  implemented" to the MVP list and stop. Rejected because the parent plan prefers implementing
  the mechanism, **C4 hard-depends on it being installed** (the resurrection check is the one
  point concurrency relies on), and the mechanism is small — installing it makes the rev1§4.6
  contract real rather than perpetually deferred. (B6A still updates the MVP list, but for the
  *residual* "GC is synchronous" simplification, not for a missing mechanism.)
- **Rejected — consult the full mark `live` set, or a Bloom filter.** The mark set is larger to
  carry through the window, and a Bloom filter is the rev1§8.3 polarity hazard explicitly
  (positives must not be trusted). The exact condemned `BTreeSet` is the correct object.

**Recommendation: add the transient `condemned: BTreeSet<Hash>`; branch `put` on it (exact
deletion-candidate list, gated by `is_empty`); keep and re-document the birth-gen filter; record
the residual synchronous-GC simplification on the MVP list.**

---

## Design decision 2 — bounding the mark walk: an explicit heap work-stack, no artificial depth cap *(resolve in B6B)*

The parent plan offers "an explicit work-stack **or** a checked depth bound that refuses rather
than faults." B6B pins the design.

- **Adopted — convert `gc::mark`'s native recursion to an explicit heap work-stack; no artificial
  depth cap.** Replace the two recursion sites (`NodeRefs::Children` → `mark(child)`,
  `Content::DirRoot(h)` → `mark(h)`) with a `Vec<Hash>` worklist of **nodes to parse**,
  mark-on-push for dedup:
  ```rust
  pub fn mark(store, root, live) -> Result<(), FormatError> {
      let mut stack = alloc::vec::Vec::new();
      if live.insert(*root) { stack.push(*root); }
      while let Some(h) = stack.pop() {
          match parse_node(&store.get(&h).ok_or(MissingNode(h))?)? {
              Children(children) => for c in children { if live.insert(c) { stack.push(c); } },
              Entries(entries)   => for e in entries { match e.content {
                  Inline(_)        => {}
                  ChunkList(ch)    => if live.insert(ch) {            // chunk-list object
                      for (chunk, _) in chunk_list_entries(&store.get(&ch).ok_or(MissingNode(ch))?)? {
                          live.insert(chunk);                          // chunk leaves: mark, never parse
                      }
                  }
                  DirRoot(dr)      => if live.insert(dr) { stack.push(dr); }  // nested dir root
              }},
          }
      }
      Ok(())
  }
  ```
  Decisive reasons:
  1. **It fixes the *actual* fault.** The overflow is native-stack exhaustion from unbounded
     recursion depth. A heap work-stack makes native stack depth **O(1)**; depth becomes heap, which
     is checked-allocation (and on the real `ChunkStore` is bounded by the device — see below).
     rev1§4.8 "detect on read, never fault" is satisfied: a malformed node yields a clean
     `FormatError` (`parse_node`/`chunk_list_entries` are already total — fuzzed as `tree_node`),
     and a well-formed-but-deep tree completes instead of faulting.
  2. **Total work is already bounded by the live set — no artificial cap needed.** `live.insert`
     dedups, and the worklist pushes **only** newly-marked nodes (mark-on-push), so each distinct
     reachable parse-node is pushed at most once and the stack never holds more than the
     distinct-reachable count. A deep `DirRoot` chain of N nodes is O(N) work and O(N) heap — the
     *legitimate* cost of marking N live objects, not a fault. (Content-addressing forbids true
     cycles: a node's hash depends on its children's hashes, so A→B→A is unconstructable; the
     attack is depth/width, which dedup + work-stack absorb.)
  3. **Sufficiency semantics are preserved exactly.** Chunk-list objects are still read inline and
     their chunk leaves marked-not-parsed; dir-roots and internal children are still parsed; the
     same hashes land in `live`. The B6C proptest and the existing `LiveOnly` oracle (`gc.rs:90`)
     hold across the refactor unchanged.
- **Optional defensive backstop (recorded, not adopted by default) — a total-node sanity cap.** A
  `live.len() > CAP → FormatError` guard would make refuse-not-fault airtight even against an
  in-memory store larger than any real device. On a real `ChunkStore` the distinct-object count is
  already bounded by device capacity (you cannot store more objects than fit), and the fuzzer's
  `MemStore` is bounded by input length, so no artificial cap is needed for the audit's
  refuse-not-fault goal; a cap could also mask a legitimately large live set. Offered as future
  hardening if an unbounded backing ever arrives; B6B does not impose it.
- **Rejected — keep recursion, add a `max_depth` parameter that returns `FormatError` past a
  fixed limit.** A smaller diff, but it imposes an *artificial* ceiling on legitimately deep
  directory nesting (any fixed cap can be a real tree's valid depth) and still leaves the work
  unbounded in width — the work-stack removes the native-stack fault *without* capping legitimate
  structure, which is the correct shape for "never fault on well-formed input."

**Recommendation: adopt the heap work-stack with mark-on-push dedup; no artificial depth cap;
record the optional total-node backstop as future hardening.**

---

## Design decision 3 — GC verification posture: strengthened proptest/fuzz oracle for sufficiency, structural argument for the bound *(resolve in B6C)*

The audit names the gap precisely: "mark-set sufficiency is a test oracle … rather than a proof."
B6C pins what "toward a proof" means here.

- **Adopted — reachability/sufficiency at the rev1§6 oracle tier; the bound proven structurally.**
  Two distinct GC properties, routed honestly:
  - **Mark-set sufficiency** ("every object reachable from a live root is in `live`, so it stays
    readable through the mark set alone") — a **global graph invariant**, delivered by the
    `LiveOnly` read-through oracle exercised over (a) a **strong randomized proptest** (B6C:
    realistic tree/snapshot-family shapes) and (b) the **`gc_mark` cargo-fuzz target** (B6B:
    hostile shapes), both Miri-replayed via `fuzz_corpus`. This is strictly more than today's
    single hand-built test, and it is the correct tier: reachability over a content-addressed
    store is not bounded-arithmetic, and a Verus proof would have to model `parse_node`,
    `chunk_list_entries`, and the store in spec, dragging `Hash` into the verified core against
    the recovery core's Hash-free design (`store.rs:359`, `prolly.rs:592`). Same trade-off B4
    made for the DMA raw-pointer seam (Miri+proptest, not Verus) and B7 makes for mount/commit
    orchestration (plain Rust over verified decision cores).
  - **Walk termination + bound** — guaranteed **structurally** by Design decision 2 (mark-on-push
    ⟹ each object pushed at most once ⟹ total work ≤ distinct reachable objects, native depth
    O(1)), and exercised by the B6B fuzz refuse-not-crash oracle. No separate proof needed; the
    structural argument is recorded in `gc.rs`'s module doc and this plan.
- **Optional (recorded, not required) — a Hash-free verified bound core.** The work-stack
  invariant (`pushed ⊆ live`, `|live|` monotone non-decreasing and ≤ a passed-in bound) could be
  lifted into a small `verus!{}` core in the style of `advance_head`/`replay_bound` (pure
  sequence/counter reasoning, no `Hash`), *raising* the verify count. Recommended only if it is
  cheaply extractable; otherwise the structural argument + the fuzz/proptest floor is the bar, and
  B6C **records which bar was met** (the B11/B7 "state the bar" discipline). Default: floor, gate
  held at 58/0.
- **Rejected — pull `gc::mark` into `verus!{}` and prove sufficiency.** Disproportionate: it grows
  the trusted/spec surface (the store model, the decoders, `Hash`) for a property the oracle tier
  covers well — against the crate's verified/trusted boundary and B7's shrink-the-seam direction.

**Recommendation: deliver sufficiency at the strengthened proptest+fuzz oracle tier and the bound
structurally; treat a verified bound core as optional and record the bar met.**

---

## Sub-phase B6A — resurrection mechanism: condemned-aware `put` + sweep window + birth-gen disposition *(closes I-3 [high])*

The headline correctness fix. Self-contained and mergeable alone: after B6A the rev1§4.6 step-3
mechanism is installed at the single GC/mutator interaction point, the birth-gen filter is kept
and honestly documented, and the residual synchronous-GC simplification is on the recorded list —
no change to the bound walk or the fuzz tier yet.

- **Touches:** `cas/src/store.rs`
  - add `condemned: BTreeSet<Hash>` to `ChunkStore` `:168-186` (doc it as the in-memory, transient
    exact deletion-candidate list, rev1§4.6/§8.3; never serialized); init `BTreeSet::new()` in both
    `ChunkStore { … }` constructors (`:956`, `:1061`);
  - branch `ChunkStore::put` `:313-341` per Design decision 1 (the `&& !self.condemned.contains`
    guard on the dedup return, gated by `is_empty`; `self.condemned.remove(&hash)` on the
    rewrite path); rewrite the dedup comment `:316-321` to describe the now-present mechanism
    instead of asserting the hazard "cannot arise";
  - in `Store::gc` `:1800-1839`: after computing `condemned` (`:1817-1823`), insert its hashes into
    `self.chunks.condemned` before the removal loop (`:1825`); after `self.commit()?` (`:1833`),
    `self.chunks.condemned.clear()`; **keep** the birth-gen filter `:1821`; refresh the admitting
    comment `:1805-1807` to the "installed but inert under synchronous GC, load-bearing in C4"
    framing;
  - extend the **MVP-simplification list** `:20-32` with a line: *"GC is synchronous (rev1§8.3
    defers concurrency to Phase C4). The rev1§4.6 step-3 dedup-resurrection check and the
    birth-generation live-by-fiat filter are installed and structurally correct, but inert under
    the synchronous cycle (no flush interleaves a sweep); they become load-bearing when C4 makes
    GC concurrent."*
- **Depends on:** Part A blessed (rev1§4.6/§4.2/§8.3). No intra-B6 dependency.
- **Work:** the field + put branch + gc window + comment/MVP-list edits as above. No format change,
  no new atomicity (the sweep rides the existing flip). Because synchronous `gc()` interleaves no
  put, add a minimal **test seam** so the mechanism is exercisable in isolation — either a unit test
  directly on `ChunkStore` (`#[cfg(test)]` access to `condemned`/`put`), or a small test-only helper
  on `Store` that opens a condemned window, applies a `put`, and closes it. Prefer the
  `ChunkStore`-level unit test (no production seam needed).
- **Acceptance (tests in `cas/src/store.rs` `mod tests`):**
  - **Condemn-then-rewrite (the I-3 witness).** Put content C → index entry at extent E1, birth B1.
    Insert C's hash into `condemned` (simulating a sweep that marked without C). `put(C)` again →
    the index entry now points at a **fresh** extent E2 ≠ E1 with `birth == current birth_gen`
    (≥ epoch), C is **removed from `condemned`**, and C reads back correctly. Pre-B6A this dedup'd
    onto the condemned entry (the resurrection bug).
  - **Live dedup unaffected.** With `condemned` empty (or not containing C), a re-`put(C)` dedups
    (index unchanged, no new extent) — the rev1§4.3 fast path is preserved; the `is_empty`
    short-circuit means no behavioural change outside a sweep window.
  - **Birth-gen filter / inertness.** `gc()` still reclaims superseded roots and reuses space
    (`gc_reclaims_…` `:2252` stays green); a snapshot still pins data across GC (`snapshots_pin_…`
    `:2295` stays green). A unit assertion that, after a synchronous `gc()` returns,
    `chunks.condemned.is_empty()` (the window is closed).
  - **Crash-safety unchanged.** `crash_mid_gc_loses_no_data` `:2549` stays green (the condemned set
    is in-memory and rebuilt each cycle, so a crash mid-sweep recovers the previous commit exactly
    as before — no new durable state).
  - `cargo verus verify -p cas --no-default-features` = **58/0** (plain Rust; record it); `cd kernel
    && cargo build` still boots.
- **Effort/Risk:** M / medium. The mechanism is small, but it sits at the chunk-store dedup
  chokepoint; correctness of the put branch (rewrite vs dedup vs miss) and the sweep-window lifetime
  is the substance. Medium because it touches the one routine every write deduplicates through.

---

## Sub-phase B6B — bounded mark walk + GC fuzz tier *(closes the stack-overflow + GC-unfuzzed findings)*

The refuse-not-fault fix. Independent of B6A (touches `gc::mark` + the fuzz infra, not
`put`/sweep) — may land in either order. After B6B a pathologically deep or adversarial tree shape
completes or refuses cleanly instead of overflowing the native stack, and the GC mark walk is in
the directly-fuzzed surface for the first time.

- **Touches:**
  - `cas/src/gc.rs` — rewrite `mark` `:21-54` as the heap work-stack (Design decision 2),
    preserving the chunk-list-inline / dir-root-and-children-parse semantics; add a one-paragraph
    module-doc note that native stack depth is now O(1) and total work is bounded by the distinct
    reachable set (the structural bound argument for Design decision 3);
  - `cas/fuzz/fuzz_targets/gc_mark.rs` — a **new** target + a `[[bin]]` in `cas/fuzz/Cargo.toml`:
    interpret the input as a **recipe** that builds a `MemStore` of tree nodes (a sequence of
    node specs — leaf entries with inline/chunk-list/dir-root content, and internal nodes
    referencing previously-built hashes — allowing deep chains, wide fanout, shared subtrees, and
    dangling/missing children), pick the last-built node as the root, run `mark`. Oracle: it must
    **never panic or overflow**; on `Ok`, the mark set is **sufficient** (read every reachable
    entry through a `LiveOnly` view, the `gc.rs:72` oracle, and assert success); on `Err`, it is a
    clean `FormatError`. Seed `cas/fuzz/corpus/gc_mark/` from a corpus-gen path (include a
    deep-chain seed and a wide-fanout seed);
  - `cas/tests/fuzz_corpus.rs` — a `gc_mark()` replay test (mirrors `tree_node` `:46`), so the new
    corpus rides the documented `--test fuzz_corpus` Miri sweep;
  - `cas/src/file.rs:70` (`chunk_list_entries`), `cas/src/prolly.rs:433` (`parse_node`) — **no
    change** (already total; the walk relies on their existing totality).
- **Depends on:** Part A blessed (rev1§4.8). Independent of B6A.
- **Work:** the work-stack refactor; the recipe-builder fuzz harness + corpus + replay; a
  deep-chain regression test (build a `DirRoot` chain ~100k deep in a `MemStore`, assert `mark`
  returns `Ok` without overflow — pre-B6B this overflowed the native stack; promote any
  fuzz-discovered crash to `cas/tests/fuzz_regressions.rs`, where the cas decoder regressions
  already live).
- **Acceptance:**
  - **Deep-tree refuse-not-crash.** The deep-chain regression test marks a ~100k-deep tree without
    overflow (`Ok`, mark set sufficient). An adversarial `gc_mark` input that is malformed yields a
    clean `FormatError`, never a panic/overflow.
  - **Sufficiency preserved.** The existing `mark_set_is_sufficient_to_read_everything` `gc.rs:90`
    passes verbatim across the refactor.
  - **Fuzz/Miri.** The `gc_mark` corpus replays clean under
    `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --test fuzz_corpus`; the
    target builds and runs under `cargo +nightly fuzz run gc_mark` (smoke).
  - `cargo verus verify -p cas --no-default-features` = **58/0** (gc.rs is plain Rust).
- **Effort/Risk:** S–M / low–medium. The work-stack is a mechanical refactor that must preserve
  the mark semantics exactly; the recipe-builder fuzz harness (constructing adversarial shapes in a
  content-addressed store) is the new work.

---

## Sub-phase B6C — mark-set sufficiency: strengthen the oracle toward a proof + record the verification posture *(closes the GC-unverified finding)*

The verification deliverable. Depends on B6B (it strengthens the sufficiency oracle on the
refactored work-stack mark and shares the `LiveOnly` read-through helper). After B6C the audit's
"mark-set sufficiency is a single test oracle rather than a proof" is closed by a strong,
randomized, Miri-replayed property plus an explicit, honest statement of the verified-vs-tested
line for GC.

- **Touches:**
  - `cas/src/gc.rs` `mod tests` — promote the single `mark_set_is_sufficient_to_read_everything`
    `:90-121` to a **randomized proptest**: a generator producing varied tree/snapshot-family
    shapes (random depth, fanout, inline vs chunked files, structural sharing across snapshot
    families via repeated `tree::put` over shared roots), then `mark` from the root and assert via
    `LiveOnly` that **every reachable entry reads correctly through the mark set alone**, and
    (where the incremental build leaves superseded roots) that `live.len() < store.len()` (the
    pruning property). Keep the original hand-built case as a named regression. Use the workspace
    Miri case-count convention (`cases: if cfg!(miri) { 4 } else { 256 }`, mirroring
    `cas/src/file.rs:121-123`);
  - factor the `LiveOnly` read-through oracle into a shared test helper so the B6B fuzz target and
    this proptest assert the *same* sufficiency property (one oracle, two drivers);
  - record the GC verification posture: a short note in `gc.rs`'s module doc (sufficiency =
    proptest+fuzz oracle tier per rev1§6; termination/bound = structural via the work-stack) and a
    one-line entry in the trusted-base ledger (`doc/guidelines/verus_trusted-base.md`) under the
    GC/oracle posture, so a reviewer sees the property is test-routed, not Verus-mechanized — the
    §6.1 "no property routed to trust is mistaken for mechanized" discipline.
- **Depends on:** B6B (the refactored mark + the shared oracle helper). Not structurally dependent
  on B6A.
- **Work:** the proptest generator + oracle; the shared helper; the posture note + ledger line.
  **Optional** (Design decision 3): a Hash-free verified bound core for the work-stack invariant; if
  pursued, re-run verify and record the **new** total (> 58) in the ledger; if not, record that the
  bound is delivered structurally + by the B6B fuzz, and the gate stays 58/0.
- **Acceptance:**
  - **Sufficiency proptest** passes at 256 cases native / 4 under Miri; the original hand-built
    case survives as a named regression; the B6B fuzz target and this proptest share one oracle.
  - **Posture recorded.** `gc.rs` module doc + the trusted-base ledger state the GC verified-vs-
    tested boundary; the audit's "test oracle rather than a proof" finding is closed as *a
    strengthened, randomized, Miri-replayed oracle + a structural bound argument* (the honest §6
    tier), with the bar met explicitly recorded.
  - `cargo verus verify -p cas --no-default-features` = **58/0** (or the recorded higher total if
    the optional bound core was pursued); Miri replay clean.
- **Effort/Risk:** S–M / low.

---

## Execution order

```
B6A  resurrection mechanism (ChunkStore::put condemned-aware)    [I-3 high; the headline fix; independent]
B6B  bounded mark walk + gc_mark fuzz tier                       [stack-overflow + unfuzzed; independent of B6A]
  └─► B6C  mark-set sufficiency proptest + verification posture  [needs B6B's refactored mark + shared oracle]
```

- **B6A** is the high-severity I-3 fix and is independently shippable: it installs the rev1§4.6
  step-3 mechanism at the single GC/mutator interaction point, keeps and documents the birth-gen
  filter, and records the residual synchronous-GC simplification — a complete, mergeable unit whose
  new state is inert in production and fully tested in isolation until C4 enables concurrency.
- **B6B** is independent of B6A (different surfaces: `gc::mark` + fuzz infra vs `put`/sweep) and is
  independently shippable: the work-stack refactor + the first GC fuzz target close the
  stack-overflow and GC-unfuzzed findings.
- **B6C** depends on B6B (it strengthens the sufficiency oracle on the refactored mark and shares
  the read-through helper) and closes the GC-unverified finding.
- B6A and B6B may be reviewed together, but each alone is a complete, mergeable unit — keep them
  separable so the high-severity correctness fix (B6A) can land without waiting on the mark/fuzz
  work (same posture as B4A/B4B, B5A/B5B/B5C).

## Out of scope for B6 (recorded so it is not mistaken for a gap)

- **Concurrent / incremental GC, persisted marking, streaming WAL replay.** rev1§8.3 defers these
  to **Phase C4**, which hard-depends on B6 (the resurrection mechanism installed and the
  birth-gen filter present). B6 installs the single interaction point and keeps GC synchronous; it
  does **not** turn on concurrency. The persisted-incremental-marking protocol "worth its own
  TLA+ model" (rev1§8.3) is C4's, not B6's.
- **A Bloom-filter mark set.** rev1§8.3 future work; B6 uses the exact `BTreeSet` mark set and the
  exact condemned (deletion-candidate) set — the polarity-safe choice the spec mandates for the
  resurrection check. Switching to a Bloom approximation, with the "never trust positives"
  discipline, is deferred with the rest of the incremental-GC family.
- **TLA / commit-protocol changes.** B6 changes no commit *sequencing* — the sweep (and any
  resurrection rewrite) rides the existing two-barrier superblock flip (rev1§4.2), exactly as B5's
  guarded batch did. The `CommitProtocol` model and its proofs are **B7's** surface and are
  untouched; B6 adds no TLA obligation. GC's crash-safety ("a crash anywhere inside GC recovers the
  previous commit") is the existing flip's property, exercised by `crash_mid_gc_loses_no_data`.
- **Verus over the Hash-carrying mark walk.** Design decision 3: reachability is delivered at the
  proptest+fuzz oracle tier and the bound structurally; pulling `mark` into `verus!{}` would grow
  the trusted/spec surface and drag `Hash` into the verified core, against the recovery core's
  Hash-free design and B7's shrink-the-seam direction. The optional Hash-free *bound* core is
  recorded as future tightening, not a B6 obligation; the gate is held at 58/0, not raised.
- **On-disk format change / `SB_VERSION` bump / corpus regeneration.** None. The only on-disk GC
  hook (`IndexEntry.birth`) already exists; the resurrection state (`condemned`) is in-memory and
  transient. Contrast B5, which bumped the format and regenerated corpora — B6 does neither.
- **GC scheduling policy — watermark / event-driven / floor triggers, sweep-I/O throttling**
  (rev1§4.6 "Policy"/"Rules"). These are **server-side orchestration** (the `storaged` reactor
  deciding *when* to run GC and at what I/O priority), not the cas *mechanism* the audit flags. B6
  closes the correctness mechanism (resurrection) and the safety (bounded walk); the triggers ride
  the existing client-`Gc` and maintenance paths and are out of this phase.
- **Loom/Shuttle for GC.** Deliberately omitted (synchronous GC, single-authority `Store`, no
  atomics, no second mutator) per the verification-tier note; the resurrection check is logical
  GC/mutator interaction resolved by serialization today. Its concurrent form is C4; the reactor's
  concurrency surface is B14.
