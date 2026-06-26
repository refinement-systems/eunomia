# Verus-discipline cleanup — implementation plan

This plan collects the surviving findings from the Verus-discipline audit of the
verified surface (`kcore`, `cas`, `ipc`, `loader`, `freelist`, `urt`, `dma-pool`,
`virtio-blk`, `storage-server`) and orders them for implementation. The ordering
principle is foundational-and-low-risk-first: trusted-base hygiene and the shared
`le-bytes` crate (which several later items depend on) land before per-crate
trigger/`rlimit` tuning, and all proof-performance work — every `rlimit` walk-down,
`spinoff_prover` re-check, trigger projection, and `opaque` re-measurement — comes
last because it is measured-and-reversible and must be judged against a freshly
re-derived cold baseline per `verus.md` §10/§13. Every task that touches `verus!{}`
code (or relocates an obligation) carries an explicit *no-weakening verification*
step: re-run the relevant `cargo verus verify -p <crate>` **cold** (`cargo clean`
first, per `verus.md` Part A's stale-cache rule — a present `verification results::`
line means a real run), confirm verified-count ≥ the trusted-base ledger Baseline and
`0 errors`; and for perf items, capture cold `rlimit` before/after against a
byte-identical control (`--time-expanded --output-json`,
`times-ms.smt.smt-run-module-times[].function-breakdown[]`), keeping the change only
if it is flat-or-better. No committed baseline exists (`target/verus-baseline/` is
gitignored), so every before-number is re-derived from the base of the work, never a
saved one.

Three cross-cutting rules govern the whole plan. **Correctness outranks checker speed**
(`verus.md` §10): never weaken a spec, drop an obligation, loosen an `ensures`, or
narrow input coverage to make the prover faster. **Relocation nets to zero**: the
`le-bytes` extraction moves verified obligations between crates but drops none — the
new crate's Baseline row plus the decremented consumer rows must sum to the prior
total. **Record new findings as you go**: implementing any task may surface something
this audit did not — a latent bug, a ledger fact stale beyond the Phase 1 anchors, a
measured `rlimit` regression that invalidates a task's premise, or a dead-end not
foreseen here. Record each such discovery in a fresh numbered report
`doc/results/N_verus-findings.md` (the next unused `N`; the directory is currently empty
and the historical series the ledger still cites reached `13`, so begin at `14`), and if
it changes a trusted seam or a Baseline reflect it in
`doc/guidelines/verus_trusted-base.md` per that file's "code is authoritative" rule.
These `doc/results` reports are temporary intermediate records per CLAUDE.md and must not
be referenced from comments, specs, or guidelines.

---

## Do-not-touch / already-idiomatic

The audit confirmed a large set of code as textbook applications of the rubric. The
implementation must **not** re-decompose, re-key, "simplify", or sweep these. They are
recorded here so the trigger / `rlimit` / dead-lemma passes skip the tempting-but-wrong
fixes.

- **`le-bytes` scope guard.** Scope the shared crate to the cas/loader read-direction
  *encode-shape* only. Do **not** fold in `ipc/src/le_bytes.rs:19-54` (both-direction
  `reassemble`/`split_bytes` — the header/session bijection needs both facts; merging
  to one weaker lemma drops coverage per §10), `cas/src/disk.rs:261-314`
  (`spec_u*_le` value-decode — definitional `ensures`, no `bit_vector` bridge; folding
  it onto the `Seq` encode form adds a redundant SAT query per §5/§6), or any
  `virtio-blk` byte codec (`virtio-blk/src/lib.rs:312-385` — the intentional
  device-DMA seam, §11). Add both-direction / value-decode families *additively* only
  if a second consumer ever needs them, never as a merge.

- **Trigger shapes that must stay whole-element** (§10's projection advice is scoped to
  neighbour-relating tuple/struct foralls; §6's mask-equal preference is conditioned on
  a `& mask` call site): the cspace waiter/ready/timer chains over scalar `ObjId`
  (`kcore/src/cspace.rs:1979-2001,2974-2989,3336-3349`), the single-key `tcb_view`/
  `timer_view` frames (`kcore/src/ready.rs:524-536,818-828`,
  `kcore/src/timer.rs:126-134`), loader `seg_ok` foralls (`loader/src/elf.rs:330-334,
  384`), `split_points` over `usize` (`cas/src/prolly.rs:1526-1556`), and
  `channel::lemma_mask_set_bit` (`kcore/src/channel.rs:164-181`, boolean-equiv over the
  named `mask_bit` predicate, not a raw `& mask`).

- **Honestly-documented standalone theorems — keep, do not "fix" as dead lemmas**:
  `lemma_from_u64_roundtrip` (`kcore/src/untyped.rs:96-115`), `pte_output_pa`
  (`kcore/src/aspace.rs:224-235`, the §8 decode half), `lemma_partition_flatten`
  (`cas/src/prolly.rs:1639-1650`, conservation design theorem with `build_level` the
  trusted applier), `lemma_recover_reconstructs_pins_head` (`cas/src/store.rs:1657-1671`,
  the §15.6 caller-less teeth control). Deleting any drops a guarantee or §8 coverage;
  fabricating an exec wrapper grows the trusted base.

- **Audited-clean trusted seams — confirm, do not churn**: `CapSlot::empty`
  (`kcore/src/cspace.rs:1595`), `u64::saturating_mul` (`kcore/src/aspace.rs:76`), the
  TLBV effect-log append+frame seam, the `is_boundary`/`checksum_ok` uninterp+
  `external_body` twins, dma-pool's `free` precondition re-establishment
  (`dma-pool/src/lib.rs:112-141`), virtio-blk's `avail_ring_slot` qsize seam discharged
  by the `new()` clamp, loader's `prepare` re-running `page_layout`
  (`loader/src/spawn.rs:45-105`), and ipc's no-seam status (tally stays 14). Use these
  as the model when fixing the genuine `urt` finding below; do not relocate dma-pool's
  hard-assert order or bolt redundant `ensures` onto producers.

- **Idiomatic proof shapes to preserve unchanged**: the §3 census/frame discipline and
  clause-labeled `wf` re-establishment (do not split a closed `wf` into sub-predicates),
  §4 termination measures, the §8 codec accept-iff / two-direction / byte-indexing
  recipes and deliberate totality-only scope boundaries, §5 modular round-up and
  overflow-iff (do not "optimize" to a bit-mask), the §6 packed-bitmap and align/modular
  split, the §14 `Admission` quota, §15.6 local-half labels, the §2 end-index partition /
  FIFO `Seq` / selector idiom, and the *deliberately documented* `spinoff_prover`/
  `rlimit` sites (`cdt_unlink`, timer-chain, `is_thread_cap_for`). The documented spinoff/
  `rlimit` *re-measurement* candidates are split out into Phase 6 below; those protected
  documented sites are not in scope for blind removal.

---

## Phase 1 — Trusted-base hygiene

Doc-only and additive-host-test corrections to the ledger and the seams it audits. Low
risk, high audit value, no proof obligation touched (except the one `urt` correctness
finding, which only *adds* a runtime check or corrects a comment). Land first so the
ledger is an accurate instrument before any obligation moves.

### 1.1 — Correct the `urt` heap `dealloc` double-free comment (drop the "promote to hard assert" option)

- **Guideline:** §11 (inverse leak; a runtime guard demoted to a `requires` needs a
  runtime backstop) + CLAUDE.md comment discipline.
- **Locations:** `urt/src/lib.rs:146-172` (`dealloc`; `debug_assert!(fl.is_allocated(off,
  need))` before `fl.free`), comment at `urt/src/lib.rs:163-166`;
  `dma-pool/src/lib.rs:136-140` (hard `assert!`); `freelist/src/lib.rs:1133-1194`
  (`FreeList::free` derives merge geometry from its `!covers` precondition).
- **Change:** Comment-only. **Drop the "promote to hard `assert!`" alternative entirely**
  — a hard assert in `GlobalAlloc::dealloc` would abort/double-panic on a drop-unwind
  path and violate the stated invariant "a heap must never abort a dealloc"
  (`urt/src/lib.rs:30-31,154`). dma-pool's hard assert is justified by an asymmetry urt
  does not share (a `Copy` `DmaBuf` is forgeable across pools; urt's dealloc receives a
  pointer its own allocator handed out). Rewrite `urt/src/lib.rs:163-166` so it no longer
  claims "the same line dma-pool draws": state the asymmetry honestly — the heap dealloc
  rests on in-process trust plus core/freelist correctness in release, the `is_allocated`
  guard being a debug-build witness only (`debug_assert!` so a dealloc never aborts).
  Optionally note (and confirm against ledger row `urt`) that the boundary also relies on
  `off+need<=N` and arena membership holding by construction from the matching alloc
  round-trip.
- **Expected effect:** audit-hygiene (comment correctness). No behaviour change.
- **No-weakening check:** byte-identical SMT (comment-only); no verify run needed beyond
  confirming `cargo build -p urt` is clean.
- **Effort:** S.

### 1.2 — Re-derive stale ledger file:line anchors and the `gc.rs` inlined count

- **Guideline:** §11 (the ledger is keyed to file:line; code is authoritative) +
  CLAUDE.md (describe what is).
- **Locations:** `verus_trusted-base.md:289-291,306-310` (drifted anchors);
  `cas/src/gc.rs:40` (stale `58/0` count).
- **Change:** Re-derive **all** seam/host-test anchors wholesale from a fresh
  `rg 'external_body|assume_specification|uninterp spec fn'` over `cas/`, `kcore/`, `urt/`
  plus a grep of each cited host-test fn name — not only the named four. Confirmed
  corrections: `is_boundary` `prolly.rs:1386→1457`; `wal_checksum_ok` `store.rs:1047→1111`
  (the Location anchor is for `wal_checksum_ok` itself — do **not** re-point it to 867,
  the `checksum_ok_spec` twin the ledger never line-anchors); `wal_struct_ok_has_teeth`
  `store.rs:4373→4562`; `object_size_positive` `untyped.rs:759→820`; `bytes_for_positive`
  `untyped.rs:743→804`; and the smaller live drift the same sweep surfaces — `checksum_ok`
  `disk.rs:341→342`, `ExTcb` `untyped.rs:244→246`, `ExNotifObj` `:248→250`, `ExTimerObj`
  `:252→254`, `fixed_object_bytes` `:272→273`. In `cas/src/gc.rs:40` **drop** the concrete
  count ("no Verus obligation is added, so the gate is unchanged") rather than re-pinning a
  number that will re-drift; it may reference the Baselines row instead.
- **Expected effect:** audit-hygiene. Zero code/SMT/coverage change.
- **No-weakening check:** doc-only; no verify run.
- **Effort:** S.

### 1.3 — Add a ledger note that the 10 transparent cspace `external_type_specification` registrations are non-seams

- **Guideline:** §9 (`external_type_specification`) + §11 (every `external_*` reconciles
  as seam or noted non-seam; tally honesty).
- **Locations:** `kcore/src/cspace.rs:268-324` (10 `ext_equal` transparent registrations,
  no `external_body`); `kcore/src/untyped.rs:243-254` (the 3 `Ex*` with `external_body`, in
  the tally); `verus_trusted-base.md:314-330` (Tally line + existing disclaimers).
- **Change:** Add one short blockquote note after the **Tally: … = 14** line (alongside the
  urt-arena / postcard disclaimers): the 10 transparent cspace
  `external_type_specification` + `ext_equal` registrations carry no `external_body`
  (unlike the 3 untyped opaque ones), introduce no trusted fact, and add 0 to the tally.
- **Expected effect:** audit-hygiene. Doc-only.
- **No-weakening check:** doc-only; no verify run.
- **Effort:** S.

### 1.4 — Give `lemma_two_allocs_disjoint` mechanical teeth (preferred) or an honest standalone-theorem label

- **Guideline:** §11 (a lemma whose `requires` is only documented, not code-discharged, is
  a finding) + §15.6 (a sanctioned standalone corollary must be framed as such).
- **Locations:** `freelist/src/lib.rs:1238-1264` (`lemma_two_allocs_disjoint`, zero
  proof-fn callers); `dma-pool/src/lib.rs:533-536`, `urt/src/lib.rs:358-363` (the runtime
  wrapper-corollary proptests).
- **Change:** **Do not delete** (the property is real and load-bearing in three crates'
  docs). Prefer **(a)**: add a small caller (proof or test) performing two real `alloc`
  calls and feeding their actual postconditions into the lemma — feasible since alloc #2's
  `old(self)` *is* the post-alloc-#1 state, so both premises derive from real `ensures`;
  this alone catches a spec/`ensures` drift mechanically, which the existing runtime
  proptests do not. Fallback **(b)**: label it an honest standalone corollary in its doc
  comment ("premises restate alloc's ensures, verified in isolation, not threaded from a
  call site").
- **Expected effect:** stronger-guarantee (option a) or audit-hygiene (option b).
- **No-weakening check:** option (a) adds a live caller/test — re-run
  `cargo clean -p freelist && cargo verus verify -p freelist`, confirm ≥ 29 verified, 0
  errors. Option (b) is comment-only.
- **Effort:** S.

### 1.5 — Fix `lemma_user_va_l1_index`'s false "(consumed by walk_alloc)" comment

- **Guideline:** §11 (dead lemma) + CLAUDE.md (comments describe what is).
- **Locations:** `kcore/src/aspace.rs:302-315` (lemma + doc comment; zero call sites,
  `walk_alloc` uses `lemma_va_indices` instead; property host-tested by
  `user_va_never_touches_kernel_l1`).
- **Change:** At minimum, correct the false consumer claim. Prefer **(a)** wiring it into a
  live obligation — have `walk_alloc`/`map_in` carry an `l1_index>=2` `ensures`, turning the
  dead theorem into a mechanized guarantee. Fallback **(b)**: state honestly it is a
  standalone pin of the rev2§2.5 isolation property with no current proof consumer. **Do not
  delete** (it pins a genuine property).
- **Expected effect:** audit-hygiene (b) or stronger-guarantee (a).
- **No-weakening check:** option (a) touches `verus!{}` — `cargo clean -p kcore && cargo
  verus verify -p kcore`, confirm ≥ 406 verified, 0 errors, and cold `rlimit` of
  `walk_alloc`/`map_in` flat-or-acceptable vs byte-identical control. Option (b) is
  comment-only.
- **Effort:** S.

---

## Phase 2 — Shared `le-bytes` crate

Foundational dedup that later codec work depends on. Land the crate, measure its
alloc-prelude cost, then migrate consumers. The hard constraint: the standalone no-alloc
gate stays byte-identical and the relocation nets to zero.

### 2.1 — Create the `le-bytes` workspace member (read-direction encode-shape only)

- **Guideline:** §6 (extract the recurring `bit_vector` identity) + §7 + Part A (`verify =
  true` dep re-verified transitively) + §13.
- **Locations to consolidate:** `cas/src/prolly.rs:648-661,721-767,772-830` and
  `loader/src/elf.rs:200-219,221-256,258-316` (byte-identical modulo doc comments +
  rustfmt). Precedent: `freelist/Cargo.toml`.
- **Change:** Create workspace member `le-bytes` (`vstd` `default-features=false`, `verify
  = true`, `no_std` + no alloc, modeled on `freelist/Cargo.toml`) exporting **only** the
  genuinely duplicated read-direction machinery: `pub open spec fn u{16,32,64}_le`,
  empty-bodied `pub proof fn lemma_u{16,32,64}_le_bytes`, and `pub fn read_u{16,32,64}_le`.
  **Drop `read_arr32` from the shared crate** — it is cas-only (`cas/src/prolly.rs:832`,
  three call sites, no loader twin); leave it in cas. Consumers will cite specs/lemmas by
  full path from inside proof blocks (never a top-level `use`, per §6/§12) so plain `cargo
  build` still erases the proof helpers.
- **Expected effect:** dedup (foundational; no consumer migration yet).
- **No-weakening check:** `cargo clean -p le-bytes && cargo verus verify -p le-bytes` ends
  with a `verification results::` line, 0 errors; record per-fn cold `rlimit` for the six
  obligations as the standalone-gate baseline.
- **Effort:** M.
- **Depends on:** —

### 2.2 — Measure the `le-bytes` alloc-prelude cost; size `rlimit` to the worst context only if needed

- **Guideline:** §10 (the `rlimit` budget must cover every re-verification context) + §15.5/
  Part A (cargo feature-unifies `vstd` globally per invocation) + the freelist alloc-cost
  ledger note (`verus_trusted-base.md:354`).
- **Locations:** `cas/Cargo.toml:43` (`vstd` `alloc`), `loader/Cargo.toml:17`,
  `ipc/Cargo.toml:40`, `freelist/Cargo.toml:23`.
- **Change:** Before migrating consumers, cold-verify `le-bytes` standalone (no-alloc) AND
  transitively under the alloc prelude: `cargo clean && cargo verus verify -p virtio-blk --
  --time-expanded --output-json` (pulls `cas → vstd[alloc]`). Compare each `le-bytes`
  obligation's `rlimit` in no-alloc vs alloc context. The lemmas are empty-bodied fixed-width
  `by (bit_vector)` SAT queries largely insulated from the prelude, so the blowup is expected
  far milder than freelist's `spinoff_prover` merge proofs — possibly zero. **Only if** a cold
  alloc-context run exceeds the default, size that obligation's `rlimit` to the worst context
  (bisect to smallest-passing + margin) and add a one-line `le-bytes` Baseline routing note
  mirroring freelist. Do not add an `rlimit` speculatively.
- **Expected effect:** perf-`rlimit` (ceiling sizing only; SMT work and coverage unchanged).
- **No-weakening check:** sizing an `rlimit` ceiling cannot change a passing proof's work;
  confirm the standalone no-alloc consumption is byte-identical, and any added ceiling passes
  cold under the alloc context (named CI error if too low, never a silent gap).
- **Effort:** S.
- **Depends on:** 2.1.

### 2.3 — Migrate cas, loader, and ipc to `le-bytes`; delete local encode-shape copies

- **Guideline:** §6 + §12 (full-path cite verified-only helpers, no top-level `use`) + Part A.
- **Locations:** `cas/src/prolly.rs:648-830`, `loader/src/elf.rs:200-316`, and ipc's
  equivalent helpers (ledger row `-p ipc`: `lemma_u{16,32}_le_{reassemble,split_bytes}`).
- **Change:** Add a `le-bytes` dependency to cas, loader, and ipc; delete each crate's local
  `u*_le` / `lemma_u*_le_bytes` / `read_u*_le` (and cas's `read_arr32`, which the shared crate
  exports — loader carries no 32-byte field, so do not claim to delete a `read_arr32` from
  loader). Replace references with `crate::`-qualified full-path citations from inside proof
  blocks. cas's own encode-side spec fns (`content_bytes`, `opt_bytes`, `canonical_bytes`,
  `canonical_leaf_bytes`) and writers (`push_u*_le`) must then cite the shared `u*_le` spec by
  full path — sound because the specs stay `open`. **For ipc:** fold its `reassemble`/
  `split_bytes` helpers into the shared crate **only if** their shape matches; if it differs
  enough (it is the both-direction form the `le-bytes` scope guard excludes), state that
  exclusion explicitly so a third drifting copy is a recorded decision, not an oversight.
- **Expected effect:** dedup. The drift hazard between hand-kept copies is removed.
- **No-weakening check:** re-verify cas (`--no-default-features`), loader
  (`--no-default-features`), and ipc **cold**; confirm each ends with a `verification
  results::` line and 0 errors. Verify `le-bytes` both standalone (no-alloc) and transitively
  under an alloc consumer, confirming relocated-obligation `rlimit` is byte-identical across
  both preludes. **Update the ledger:** add a `le-bytes` Baseline row = the relocated item
  count, and decrement the cas/loader/ipc rows so the totals net to zero (pure relocation — no
  obligation dropped, no `ensures` loosened, no input coverage narrowed).
- **Effort:** M.
- **Depends on:** 2.1, 2.2.

---

## Phase 3 — Codec accept-iff hardening

Coverage-strengthening (additive teeth) and one design spike. Sequenced after Phase 2
because the chunk-list lift reuses the new byte-indexed readers.

### 3.1 — Cross-check the `wal_struct_ok` / `WalOp::decode_record` faithfulness join (fix the checksum-masking trap)

- **Guideline:** §11 (faithful mirror + teeth; the inverse leak).
- **Locations:** `cas/src/store.rs:1994-1995` (`WalOp::decode_record(...).expect()`),
  `cas/src/store.rs:1136-1144` (`wal_content_ok`), `cas/src/disk.rs:669-686` (plain-Rust
  `WalOp::decode_record`), `cas/src/store.rs:4562-4636` (`wal_struct_ok_has_teeth`).
- **Change:** Strengthen `wal_struct_ok_has_teeth` (or add a sibling) to cross-check the
  decode/`content_ok_spec` join directly. **Positive direction** (sound as-is): for each
  record built via `encode_record(...)` (real checksum), assert
  `WalOp::decode_record(&rec).is_some()` alongside the existing `wal_content_ok(&rec,..)`.
  **Negative direction** (corrected — a `framed(...)` record carries a zero placeholder
  checksum, so `decode_record` rejects at the checksum gate `disk.rs:681` *before* the
  structural `decode_payload` `disk.rs:684`, making `is_none()` pass *vacuously*): for each
  structurally-malformed payload (bad tag, trailing bytes, truncated path component, Rename
  second-path/mtime truncation, empty payload), either (a) splice in a valid checksum via
  `disk::record_checksum(seq,len,payload)` so `decode_record` reaches and rejects at
  `decode_payload`, or (b) assert `WalOp::decode_payload(payload).is_err()` (the structural
  half) for every payload `!wal_struct_ok` rejects and `.is_ok()` for every accepted one.
  Keep the existing checksum-half teeth (the `bad_cksum` case).
- **Expected effect:** audit-hygiene (additive test coverage; no spec, `ensures`, or input
  coverage touched).
- **No-weakening check:** `cargo test -p cas` green; the negative-direction assertions must
  *reach the structural decoder* (not pass vacuously at the checksum gate).
- **Effort:** S.

### 3.2 — Lift `file.rs::chunk_list_entries` toward the §8 byte-indexing / §9 Hash-free recipe (design spike)

- **Guideline:** §8 (index bytes, build `[u8;N]` element-wise, accept-iff totality) + §9
  (Hash-free image) + rev2§6.1(e).
- **Locations:** `cas/src/file.rs:70-86` (`chunk_list_entries`, plain Rust using
  `from_le_bytes` / `try_into().unwrap()` / range-slice — exactly the §8-forbidden forms);
  `cas/src/file.rs:17-33,200-206` (`store_file`/`store_file_neighborhood` encoders).
- **Change:** Design spike, not a mechanical move. Lift the integer/framing half into an
  always-compiled `verus!{}` island returning a Hash-free image — reuse the byte-indexed
  readers (post-`le-bytes`), prove a total accept-iff over `Seq<u8>` (`len>=5 && [0]==MAGIC &&
  len==5+count*36`), carrying the `[u8;32]` digests as raw bytes (the §9 `RawContent` shape
  `decode_node` uses); the `Hash::from_bytes` wrap stays the thin delegator. A strictly smaller
  first step is the §8 form-only rewrite (byte-index, build `[u8;32]` element-wise),
  behaviour-identical and a drop-in for a later proof. Separately, factor the `5+count*36`
  stride shared between `store_file`/`store_file_neighborhood`/`chunk_list_entries` into one
  place so encode and decode reference one layout. Prove totality + framing only (no
  injectivity — as `decode_node` does over the opaque `is_boundary`).
- **Expected effect:** stronger-guarantee (totality/accept-iff over all adversarial inputs).
- **No-weakening check:** `cargo clean -p cas && cargo verus verify -p cas
  --no-default-features` — the new decode/encode obligations are additive, so confirm ≥ 77
  verified (rising by the new fn count), 0 errors, and the rest of the no-default-features
  surface's `rlimit` is not regressed (byte-identical control).
- **Effort:** M.
- **Depends on:** 2.1 (the byte-indexed readers).

### 3.3 — Record `startup::decode` totality as a declined mechanization candidate (do not auto-pursue)

- **Guideline:** §8 (totality / accept-iff) + §9 + §2 (fixed-array arena).
- **Locations:** `loader/src/startup.rs:268-305` (`decode`, total over arbitrary bytes, no
  `verus!{}`), `:229-259` (`Reader`); `loader/src/elf.rs:343-468` (the verified sibling
  `parse`).
- **Change:** Record only — decide consciously rather than by omission. `startup::decode` is a
  structural sibling of the verified `elf::parse` (same `checked_add`/`get` `Reader`, same
  fixed `MAX_GRANTS`/`MAX_ARGV`/`MAX_ENV` arena, same rev2§2.7 refuse-not-crash contract). It
  is **not** trusted-provenance input — the code is treated as untrusted-input wire decoding
  (it carries a `startup` fuzz target *and* the `decode_is_total` proptest), so per §8 it is a
  genuine adversarial-decode surface whose proof-less oracle tier the ledger
  (`trusted-base.md:359`) accepted as the status quo. **Recommend NOT auto-scheduling**:
  mechanizing it is a large new proof surface (argv/env borrow lifetimes into `buf`, a `Reader`
  rewrite). Schedule only if the parent decides the `_start`-input refuse-not-crash floor
  warrants a deductive twin beyond the existing oracle tier. **Do not strip** the never-fires
  `.ok()?` guards on `push_grant`/`push_argv`/`push_env` — deleting an unverified guard needs a
  licensing proof this file lacks.
- **Expected effect:** none (recording).
- **No-weakening check:** n/a (no code change).
- **Effort:** L if pursued; the recording itself is S.

---

## Phase 4 — Host-oracle teeth completion

Additive host-test coverage completing the test files' own uniform discipline. No
`verus!{}` or spec change; risk confined to host tests.

### 4.1 — Add a `fire_safe_exec` mirror + `_has_teeth` control in `test_store.rs`

- **Guideline:** §11 (the mirror must reject a malformed shape) + §15.6 (a fresh projection
  of a model invariant needs a committed teeth control).
- **Locations:** `kcore/src/test_store.rs:2459` (`report_terminal_firesafe_empty_slot`, the
  only `fire_safe` test); `kcore/src/cspace.rs:5576` (`fire_safe` spec);
  `kcore/src/test_store.rs:930,3379` (`caps_consistent_exec` + `_has_teeth`, the established
  pattern).
- **Change:** Add `fire_safe_exec(st)` (for every resident TCB bind slot, the cap is empty OR
  names a notification present in `st.notifs`) and a `fire_safe_exec_has_teeth` test (a bind
  slot holding `Notification(nn)` with `nn` absent from `notifs` must be rejected; a
  well-formed shape accepted), host-checked across `report_terminal`. This is the only verified
  whole-store invariant lacking the `{_exec, _has_teeth}` pair every other one carries; the
  teeth shape (resident in-arena slot, absent `nn`) is distinct from
  `caps_consistent_exec_has_teeth`'s out-of-arena Thread arm, so it adds discriminating
  coverage.
- **Expected effect:** audit-hygiene (additive; no proof obligation or `ensures` touched).
- **No-weakening check:** `cargo test -p kcore` green; confirm `fire_safe_exec_has_teeth`
  *rejects* the malformed shape (a vacuously-passing teeth test is a finding).
- **Effort:** S.

### 4.2 — Extract the hand-rolled cycle-bounded chain-walk idiom in `test_store.rs` into one helper

- **Guideline:** §11 (bound any chain-walking mirror against a cyclic fixture — cap by
  `nodes.len()+1`) + §13 (deduplicating an identical block).
- **Locations:** `kcore/src/test_store.rs:564` (`waiter_count_exec`), `:707` (`notif_wf_exec`),
  `:741` (`timer_wf_exec`, redundant `seen` dup-check), `:789` (`ready_seq_exec`). **Exclude**
  `:503` (`no_cycle` — walks `SlotId` with an `n`-cap over all starts, returns bool; a
  structurally different shape).
- **Change:** Extract one generic `walk_chain<Id, F: Fn(Id)->Option<Id>>(start, cap, next) ->
  Option<Vec<Id>>` returning `None` on overrun (`steps > cap`), `Some(nodes)` otherwise.
  Reroute the three homogeneous TCB/timer walks plus the waiter count: `notif_wf_exec` (cap =
  `tcbs.len()+1`, then run its per-node validation over the returned `Vec`), `timer_wf_exec`
  (cap = `timers.len()+1`; **the returned `Vec` IS the `seen` set** — reuse it for the
  completeness sweep at lines 764-768 rather than dropping it; only the now-redundant inline
  `seen.contains` dup-check is removed, since None-on-overrun already rejects cycles),
  `ready_seq_exec` (cap = `tcbs.len()+1`), and `waiter_count_exec` (use the `Vec` len for the
  count, preserving partial/None overrun semantics so `obj_census_exec` is unchanged on the
  well-formed precondition). Each caller passes its own `collection.len()+1` cap.
- **Expected effect:** dedup (host-test clarity).
- **No-weakening check:** `cargo test -p kcore` green; keep `notif_wf_exec_has_teeth` (3128),
  `timer_wf_exec_has_teeth` (5162), `ready_wf_exec_has_teeth` (5929) green (the teeth tests
  re-validate the consolidated walk rejects cycles) and confirm a valid fixture is not
  spuriously rejected.
- **Effort:** M.

### 4.3 — Exercise `map_frame`'s `Err (NeedMemory)` arm in a host differential test

- **Guideline:** §11 (the mirror must be faithful; exercise both branches).
- **Locations:** `kcore/src/test_store.rs:198` (`aspace_map` always `Ok`),
  `:6142` (the `map_in_need_memory` precedent); `kcore/src/cspace.rs` (`map_frame` Err arm:
  store unchanged on `NeedMemory`).
- **Change:** Optionally add a second `Store` impl (or flag) whose `aspace_map` returns
  `Err(NeedMemory)` and a host test driving `map_frame` through it to assert the store-unchanged
  frame on the Err arm — mirroring how the aspace `map_in` tests already drive
  `MapError::NeedMemory`. Leave the genuinely-effectless no-op seams as documented. Low-priority
  hardening; the verified `ensures` already covers all inputs.
- **Expected effect:** audit-hygiene (additive host coverage; verified Err-arm `ensures`
  unchanged).
- **No-weakening check:** `cargo test -p kcore` green; the new test asserts the unchanged frame.
- **Effort:** S.

---

## Phase 5 — Trigger economy sweep

Trigger-annotation changes, each measured cold and kept only if flat-or-better (one is a
confirmed perf win, the rest are clarity/uniformity unless `rlimit` moves). All are
annotation-only: a non-firing projection trigger fails re-verification immediately, so no
silent weakening is possible.

### 5.1 — Project the neighbour-relating freelist sortedness re-proofs onto `(.0,.1)`

- **Guideline:** §10 (projection over whole-aggregate trigger for sortedness/adjacency;
  mirror the target conjunct verbatim) + §13.
- **Locations:** `freelist/src/lib.rs:678` (`split_wf`), `:819` (`free_insert`), `:929`
  (`free_replace`), `:1033` (`free_both`); target conjunct at `:82-85` (already projects via
  `#![trigger self.free@[k].0, self.free@[k].1]`).
- **Change:** Change the four sortedness asserts from `#![trigger new.free@[k]]` to
  `#![trigger new.free@[k].0, new.free@[k].1]`, matching the conjunct at line 82. This is the
  exact §10 sortedness/adjacency shape where a whole-aggregate trigger self-perpetuates a
  matching loop. Highest-priority trigger item (the bodies relate element `k` and neighbour
  `k+1`).
- **Expected effect:** perf-`rlimit` (a measurable drop is the realistic expectation since this
  is the neighbour-relating case).
- **No-weakening check:** `cargo clean -p freelist && cargo verus verify -p freelist --
  --time-expanded --output-json` before AND after; diff per-fn `rlimit` for
  `split_wf`/`free_insert`/`free_replace`/`free_both` against a byte-identical control (only the
  four trigger annotations differ); confirm ≥ 29 verified, 0 errors. Keep regardless (trigger
  uniformity is a clarity win even if flat).
- **Effort:** S.

### 5.2 — Project the in-bounds freelist re-proofs and `is_allocated` loop invariant onto `(.0,.1)`

- **Guideline:** §10 (mirror the target conjunct's trigger; keep trigger shape uniform across
  siblings).
- **Locations:** `freelist/src/lib.rs:703` (`split_wf`), `:840` (`free_insert`), `:942`
  (`free_replace`), `:1056` (`free_both`), `:308` (`is_allocated` loop invariant); target
  in-bounds conjunct `:80-81`.
- **Change:** Change these five whole-aggregate triggers (`#![trigger new.free@[k]]` /
  `#![trigger self.free@[j]]`) to `#![trigger ...[k].0, ...[k].1]` matching the in-bounds
  conjunct and sibling projecting asserts. **Note (correcting the original framing):** this is
  **not** a flat clarity-only tidy — measurement showed a deterministic cold `rlimit` win
  (crate function-`rlimit` total ~215.1M → ~150.2M, ~30%, driven by `free_insert` ~110.9M →
  ~43.2M, ~61%). Treat it as a measured perf win and fold it in alongside 5.1. Re-confirm the
  before-number on the merged tree.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** as 5.1 — cold before/after with only the five trigger annotations
  differing; confirm ≥ 29 verified, 0 errors and the crate-total `rlimit` does not regress.
- **Effort:** S.

### 5.3 — Project the flushed-only `RecMeta` foralls in `cas/store.rs` onto `records@[k].flushed`

- **Guideline:** §10 (projection over whole-aggregate; uniform shape) + §13.
- **Locations:** `cas/src/store.rs:573,588` (`advance_head`), `:1344,1351,1359`
  (`lemma_gap_freedom`), `:1519,1544,1556` (`recover_records`).
- **Change:** Change the flushed-only `RecMeta` foralls (bodies read only `.flushed`) to
  `#![trigger records@[k].flushed]` (and `records[j].flushed` in `lemma_gap_freedom`). These do
  not relate same-shape neighbours (the neighbour relation lives in `rec_ok`/`laid_out`), so the
  matching-loop hazard is largely absent and the win may be small — the real basis is cross-talk
  reduction with the `records@[k]`/`records@[k+1]` foralls. Measure-and-keep-if-helps.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** `cargo clean -p cas && cargo verus verify -p cas --no-default-features
  -- --time-expanded --output-json` before/after; diff `advance_head`, `lemma_gap_freedom`,
  `recover_records` (only the trigger annotations change); confirm ≥ 77 verified, 0 errors.
  **Keep only if** each obligation's `rlimit` is flat-or-better AND the crate total does not
  regress; revert otherwise.
- **Effort:** S.

### 5.4 — Restate `timer_wf` via the deterministic `timer_seq` selector to match `ready_wf`'s idiom

- **Guideline:** §10 (eliminate a bare existential with a deterministic selector trigger
  anchor) + §2 (choose-defined order needs a uniqueness lemma).
- **Locations:** `kcore/src/cspace.rs:3362` (`timer_wf`, bare `exists`), `:3307-3318`
  (`ready_wf`, the migrated sibling), `:3369` (`timer_seq` via `choose`);
  `kcore/src/timer.rs:151,356-357,756` (per-op manual witness-surfacing asserts).
- **Change:** Restate `timer_wf` as `timer_chain(tmv, head, timer_seq(tmv,head)) &&
  timer_complete(tmv, timer_seq(tmv,head))`, making `timer_seq` the explicit selector so the
  per-op witness-surfacing asserts can drop. The `timer_complete` conjunct must travel into the
  selector body so `timer_seq`'s `choose` still pins the unique completed chain. Equivalent under
  `lemma_timer_chain_unique`. Note the establishment sites (`timer.rs:247,406`) may each need an
  inserted `lemma_timer_chain_unique(...)` call before the `timer_wf` assert (mirroring
  `lemma_ready_inv_frame_fields` at `cspace.rs:5301-5309`).
- **Expected effect:** clarity (consistency win; modest magnitude).
- **No-weakening check:** `cargo clean -p kcore && cargo verus verify -p kcore --
  --time-expanded --output-json` before/after; measure the *sum* of `rlimit` across
  `disarm`/`arm`/`check_expired` (the delta to keep flat-or-better is the sum, not any single
  op), holding the spec text and `lemma_timer_chain_unique` byte-identical except the body swap;
  confirm ≥ 406 verified, 0 errors. Keep only if flat-or-better.
- **Effort:** M.

### 5.5 — (Dropped) ipc `coherent` projection — do not pursue

- **Verdict:** drop. `coherent` (`ipc/src/reactor.rs:208`) is a single-index forall (body reads
  only `.is_some()`, no `slots[b±1]`), so §10's matching-loop perf rationale does not apply, and
  the two re-establishment proofs (`:259,357`) are already uniform on the whole-element form
  (`(#[trigger] out@[b]).is_some()`). Switching the spec to `slots[b].is_some()` is lateral at
  best and risks a method-call trigger mismatch on already-passing, already-uniform code.
  Recorded so the sweep skips it.

---

## Phase 6 — `rlimit` right-sizing and `spinoff_prover` / `opaque` re-checks

Pure proof-performance tuning, deferred to last because it is measured-and-reversible and
must be judged against a freshly re-derived cold baseline (the freelist items additionally
gated on Phase 5 having shrunk their contexts). Every item: `rlimit` is a ceiling not work,
`spinoff_prover` is a scheduling hint, `opaque`/`reveal` is a perf lever — none change a
proven obligation, and a too-low/too-aggressive choice surfaces as a named CI error, never a
silent gap. Judge `spinoff` and removal effects by the **crate total** (a function-level
attribute ripples module-wide), and keep each cap passing in **every** re-verification
context (the alloc-prelude worst case where it applies).

### 6.1 — Walk down the seven `ready.rs` `rlimit` budgets after decomposition

- **Guideline:** §10 (bisect to smallest-passing + margin; drop if default suffices) + §13.
- **Locations:** `kcore/src/ready.rs:137,184` (`rlimit(100)` coherence lemmas), `:229,370`
  (`rlimit(150)` push/remove wf), `:493` (`rlimit(40)` enqueue), `:664` (`rlimit(60)` dequeue),
  `:778` (`rlimit(100)` unqueue).
- **Change:** Run `cargo clean -p kcore && cargo verus verify -p kcore -- --time-expanded
  --output-json` for per-fn `rlimit` consumption, then bisect each cap down to smallest-passing +
  small margin (drop any that verify at default). The coherence lemmas are spun off and the wf
  sweeps delegate to them, so the bodies carry small isolated contexts now. kcore is a leaf, so
  the standalone cold measurement is the worst context.
- **Expected effect:** perf-`rlimit` (ceiling change only).
- **No-weakening check:** lowering a cap cannot change consumption — confirm the whole-crate
  per-fn `rlimit` consumption is byte-identical before/after (any delta means something other
  than caps changed), ≥ 406 verified, 0 errors, full `-p kcore` cold run green.
- **Effort:** S.

### 6.2 — Re-check redundant `spinoff_prover` on already-extracted freelist covers/wf leaf lemmas

- **Guideline:** §10 (`spinoff_prover` redundant after a clean extraction; the existential-set
  frame is the legitimate exception) + §13.
- **Locations:** `freelist/src/lib.rs:648` (`split_wf`), `:716` (`split_covers`), `:853,955,1070`
  (`free_covers_insert/replace/both`); **do not touch** `:288,349,1132`
  (`is_allocated`/`alloc`/`free` exec dispatchers — large host/loop contexts, the legitimate
  use).
- **Change:** Drop `spinoff_prover` from the already-extracted leaf lemmas one at a time (start
  with `split_wf`/`split_covers`, which are pure structural/sortedness lemmas with no existential
  frame — the more promising). The covers halves carry a heavy choose-witness frame across an
  index/shift correspondence (`j→j+1`, remove-shift) — the named legitimate spinoff use-case, so
  expect removal to regress them. Keep `spinoff` wherever removal regresses either context or
  pushes a body past budget.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** measure per-fn `rlimit` AND crate total under **both** the standalone
  no-alloc freelist gate and the alloc-prelude context (`cargo verus verify -p virtio-blk`, which
  re-checks freelist under `vstd[alloc]`), since `free_insert`/`free_both` budgets are sized for
  that ~1.4–1.85× worst case. Keep a removal only if both contexts' lemma `rlimit` and crate
  total do not regress; confirm ≥ 29 verified, 0 errors. If `spinoff` is removed where an
  `rlimit` cap sits, re-tighten the cap (6.5).
- **Effort:** M.

### 6.3 — Re-measure `destroy_tcb` `spinoff_prover` + `rlimit(24)`

- **Guideline:** §10 (spinoff redundant after extraction; walk `rlimit` down) + §13 (spinoff
  correct when reasoning genuinely entangled).
- **Locations:** `kcore/src/thread.rs:814-815` (`spinoff_prover` + `rlimit(24)`),
  `:501,600,689` (extracted per-phase frame lemmas), `:473` (`lemma_running_frame_trans`).
- **Change:** On a cold `-p kcore --time-expanded` run: (a) try removing `spinoff_prover` and
  confirm `destroy_tcb` still discharges; (b) independently bisect `rlimit(24)` down. The body
  still threads four running frames across many edges plus an inline detach-phase proof
  (`:922-972`), so entanglement may remain — measure-then-decide. Update the justification
  comment to present-tense whichever survives.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** removing `spinoff` merges this body into the module's shared SMT batch,
  so judge (a) by the kcore crate/module total (and confirm `destroy_tcb` still discharges); for
  (b) measure `destroy_tcb`'s own `rlimit`. Keep each annotation only if removal/reduction
  regresses; confirm ≥ 406 verified, 0 errors.
- **Effort:** M.

### 6.4 — Re-measure `notification.rs` `signal` `rlimit(50)` and `remove_waiter` `spinoff` + `rlimit(25)`

- **Guideline:** §10 (walk `rlimit` down; spinoff redundant after extraction) + §13.
- **Locations:** `kcore/src/notification.rs:69-70` (`spinoff_prover` + `rlimit(50)` on `signal`),
  `:719-720` (`spinoff_prover` + `rlimit(25)` on `remove_waiter`), `:964`
  (`lemma_waiter_dequeue_census` call).
- **Change:** `signal` keeps its wake-path census inline as a justified §10 dead-end (the comment
  at `:379-383` documents why extraction backfires), so its `spinoff` is correct — bisect only the
  numeric `rlimit(50)` down (keep `spinoff`). `remove_waiter`'s census IS extracted
  (`lemma_waiter_dequeue_census`), so measure its `rlimit` with and without `spinoff` and bisect
  `rlimit(25)`; drop `spinoff` only if the residual splice-walk loop body verifies within budget
  without it.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** for `signal`, measure inside its spinoff and bisect; for `remove_waiter`,
  profile twice (with/without `spinoff`) comparing both the function `rlimit` and the kcore
  crate-total (spinoff removal shifts cost into the module query). Keep all `ensures` and the loop
  invariant byte-identical; confirm ≥ 406 verified, 0 errors.
- **Effort:** S.

### 6.5 — Walk down freelist merge-proof `rlimit` budgets (120/40/20) under the alloc-prelude worst context

- **Guideline:** §10 (walk `rlimit` down; budget must cover the worst context) + the freelist
  ledger row.
- **Locations:** `freelist/src/lib.rs:796` (`free_insert` `rlimit(120)`), `:898` (`free_replace`
  `rlimit(20)`), `:1007` (`free_both` `rlimit(40)`).
- **Change:** Strip all three caps and re-derive cold `rlimit`. Per the ledger
  (`trusted-base.md:354`), only `free_insert(120)` and `free_both(40)` are documented as
  alloc-sized; `free_replace(20)` is likely already near its no-alloc floor — verify rather than
  assume. Re-bisect each cap to smallest-passing under the **worst (alloc)** context + modest
  margin. **Do not lower any cap below its alloc-prelude consumption.** Update the ledger row to
  reflect the final three caps including `free_replace`.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** measure each cap's consumption in two cold sessions: standalone (`cargo
  clean -p freelist && cargo verus verify -p freelist`, no-alloc) AND `cargo verus verify -p
  virtio-blk` (freelist under `vstd[alloc]`, worst context). Set each new cap = ceil(worst-context
  consumption) + margin. Confirm the no-alloc consumption is byte-identical (cap-only change), ≥
  29 verified, 0 errors in both sessions.
- **Effort:** M.
- **Depends on:** 5.1, 6.2 (the context-shrinking changes must land first so the re-bisect targets
  the new, smaller contexts).

### 6.6 — Re-measure `spinoff_prover` + `rlimit(60)` on the two cspace ready-frame lemmas

- **Guideline:** §10 (spinoff redundant after clean extraction; walk `rlimit` down) + §13.
- **Locations:** `kcore/src/cspace.rs:5231-5232` (`lemma_ready_inv_frame_offchain`),
  `:5280-5281` (`lemma_ready_inv_frame_fields`). These are the only cspace `spinoff` sites
  carrying both `spinoff` and `rlimit(60)` **without** a documented rationale (unlike the
  deliberately-documented `cdt_unlink`/timer-chain/`is_thread_cap_for` sites).
- **Change:** On a cold `-p kcore --time-expanded` run: (a) drop `spinoff_prover` from each and
  confirm it still passes (fresh small context); these reason over `NUM_PRIOS`-quantified
  `ready_seq` term families with a forall-implies `requires` — the entangled existential-set frame
  §10 says `spinoff` *suits*, so the drop may be a no-op, in which case (b) bisect `rlimit(60)`
  down to smallest-passing + margin, or drop the annotation if it verifies at default.
- **Expected effect:** perf-`rlimit`.
- **No-weakening check:** read per-fn `rlimit` for both lemmas plus the kcore crate-total; hold a
  few neighbouring untouched cspace proofs (`lemma_ready_inv_frame` `:5212`,
  `lemma_timer_push_head_chain` `:3631`) as byte-identical controls and confirm their `rlimit` is
  unchanged. Judge net effect by the crate total (removing a function ripples module-wide); ≥ 406
  verified, 0 errors. Revert any regression.
- **Effort:** S.

### 6.7 — Measure whether `#[verifier::opaque]` on `content_ok_spec` helps or hurts (likely keep)

- **Guideline:** §10 (opaque earns its keep only on recursive specs) + §13 (opaque on a
  non-recursive spec is typically net-negative — but the recursive-shield case is the exception).
- **Locations:** `cas/src/store.rs:873-876` (`#[verifier::opaque] content_ok_spec`), `:1142`
  (`reveal` in `wal_content_ok`), `:858-861` (recursive `s_payload_ok`/`s_path`).
- **Change:** `content_ok_spec` is non-recursive, but its body transitively references the
  recursive structural family and its consumers `run_len`/`laid_out` are themselves recursive — so
  the opaque shields a recursive structural decode (the case where the non-recursive-opaque rule
  may not apply). Compare crate `rlimit` with the opaque vs removed (and the `reveal` at 1142
  dropped). If removing is flat-or-better, drop it per §13; if it regresses the recursive consumers
  (the likely outcome), **keep** the opaque (the existing comment at `:870-872` already states the
  shielding rationale, so this is largely confirm-and-leave).
- **Expected effect:** none (likely) or perf-`rlimit`.
- **No-weakening check:** `cargo clean -p cas && cargo verus verify -p cas --no-default-features --
  --time-expanded --output-json` for both trees; measure per-fn `rlimit` for `run_len` (`:1157`),
  `laid_out` (`:1220`), `recover_records` (`:1506`), `lemma_recover_reconstructs`/
  `lemma_gap_freedom`, plus the crate total. Keep the opaque (expected) unless removal is
  flat-or-better on every listed consumer AND the crate stays at 77 verified, 0 errors.
- **Effort:** S.

---

## Phase 7 — Clarity nits

Low-value, low-risk cosmetic and code-quality items. Whitespace and trivially-total-cast
changes are SMT-neutral (no measurement); the unverified-code census/guard additions are
test-only.

### 7.1 — Re-indent the eight `ready_view` frame conjuncts in cspace `ensures` blocks

- **Guideline:** §3 (the per-view frame line is the grep-able completeness checklist) +
  CLAUDE.md formatting.
- **Locations:** `kcore/src/cspace.rs:9295,9351,9774,10060,10453,11219,11315,11530` (the
  `final(store).ready_view()==old(store).ready_view()` lines indented 12 spaces; siblings are 8).
- **Change:** Re-indent the eight lines to 8 spaces to match siblings. Inside `verus!{}`, so plain
  `cargo fmt` does not re-flow them. Pure whitespace.
- **Expected effect:** clarity.
- **No-weakening check:** SMT obligation byte-identical (Verus ignores this whitespace); a cold
  `-p kcore` run confirms 406 verified, 0 errors. No `rlimit` measurement needed.
- **Effort:** S.

### 7.2 — Optionally drop the redundant `& 0xFF` before the total `as u8` narrowing in `decode_prio`

- **Guideline:** §5 (narrowing casts `as u8` carry no obligation — they are total).
- **Locations:** `kcore/src/sysabi.rs:92` (`(raw & 0xFF) as u8`), `:85-87` (the comment).
- **Change:** Optionally simplify to `let prio = raw as u8;` and trim the comment. The subsequent
  `< NUM_PRIOS` check is unchanged. Behaviour-identical; the mask arguably documents intent — do
  not churn if the team prefers it explicit.
- **Expected effect:** clarity.
- **No-weakening check:** `raw as u8` and `(raw & 0xFF) as u8` are bit-identical for all `u64`; a
  cold `-p kcore` run confirms 406 verified, 0 errors, count/`rlimit` neutral.
- **Effort:** S.

### 7.3 — Add a recomputable-from-state byte-census debug check in `overlay.rs`

- **Guideline:** §3 (a clamp hides a census mismatch — code-quality nudge for unverified code).
- **Locations:** `cas/src/overlay.rs:226-229,281-283,451-454` (`saturating_sub` reaps),
  `:212,321` (`(bytes + delta).max(0)` writes), `:476-525` (the existing `#[cfg(test)]
  check_invariants` helper, 8 call sites).
- **Change:** Make the byte census recomputable-from-state (sum over `by_id` writes) **in the
  existing `cfg(test)` `check_invariants` helper** (which checks structural invariants but not the
  bytes census) rather than a runtime `debug_assert` on the hot write path. This catches a latent
  miscount the `saturating_sub`/`.max(0)` clamps would otherwise absorb. **Do not** move overlay
  into Verus (correctly test-routed).
- **Expected effect:** stronger-guarantee (independent census check; removes no behaviour).
- **No-weakening check:** `cargo test -p cas` green (the 8 existing call sites now also assert the
  census).
- **Effort:** S.

### 7.4 — Add a length-stability `debug_assert!` in `CrashDev::crash`'s replayed-write slicing (drop the clamp/skip option)

- **Guideline:** §3 (keep-total / refuse-not-panic posture for the crash oracle) + §11
  (runtime-only-guard idiom) + rev2§4.8.
- **Locations:** `cas/src/dev.rs:203-230` (`crash`), `:216-218,223-225,228-229` (the
  `copy_from_slice` slices over recorded `off`/`keep`).
- **Change:** **Drop the clamp/skip alternative** — silently dropping an out-of-range recorded
  write would mute a torn-write/false-crash signal in the `CommitProtocol` fuzz oracle, and §10
  counsels removing a never-firing guard, not adding one. Add **only** a `debug_assert!` at the
  start of `crash` (or before the `copy_from_slice` calls) asserting the relied-upon
  length-stability invariant (e.g. `debug_assert_eq!(self.durable.len(),
  self.current.borrow().len())` and `off + data.len() <= disk.len()`), with a one-line comment
  stating pending writes were bounds-checked against `current.len()` at `write()` time and
  `durable`/`current` are allocated once and never resized. Compiles out in release; surfaces the
  non-local invariant under test/fuzz.
- **Expected effect:** clarity (surfaces a non-local invariant; removes/loosens nothing).
- **No-weakening check:** `cargo test -p cas` green; the assert documents the as-is invariant
  (CLAUDE.md discipline).
- **Effort:** S.

### 7.5 — Measure-then-maybe-drop the two empty-body assert-forall hint blocks in urt `slots.rs` `alloc_range`

- **Guideline:** §10 (prune hints a bloated context forced; the lift-then-prune posture).
- **Locations:** `urt/src/slots.rs:272-279` (two empty-body `assert forall ... by {}` blocks
  before the marking loop).
- **Change:** These are *intermediate* (surfacing facts for the subsequent `while m` marking loop's
  invariant), not the §10 terminal dead-end. Try deleting both and re-verify cold; if `slots.rs`
  still verifies, drop them for clarity. If verification fails they are load-bearing surfacing
  steps and must stay. Low value; droppable if effort is scarce.
- **Expected effect:** clarity.
- **No-weakening check:** `cargo clean -p urt && cargo verus verify -p urt` — confirm ≥ 25
  verified, 0 errors after deletion; restore on any failure (removing a redundant hint cannot
  weaken any `ensures`; verification fails loudly otherwise).
- **Effort:** S.

### 7.6 — (Dropped) virtio-blk `avail_ring_slot` derivable `4<=slot` clause — do not remove

- **Verdict:** drop. `4 <= slot` (`virtio-blk/src/lib.rs:122-126`) is entailed by the
  slot-equality clause, but removing a stated `ensures` is the forbidden "loosen an `ensures`"
  direction with **zero** offsetting gain (the candidate is SMT-neutral and count-neutral by its
  own admission). §3 favors the opposite — a directly-usable per-key bound saves every call site
  re-deriving it from the nonlinear product `4+(idx%qsize)*2` (which §5 warns Z3 is flaky on).
  Recorded so the sweep keeps the clause.

---

## Phase 8 — Bit-identity dedup (u64) — scoped cross-crate evaluation

A narrow, measurement-gated cross-crate item. The realistic dedup is small; most reactor
sites are distinct identities, not the shared single-bit shape. `by (bit_vector)` is
width-fixed (§6), so a shared u64 module cannot cover the kcore u32 sites — those are a
separate strength edit, not dedup.

### 8.1 — Fold the genuinely-shared u64 single-bit identities (≈2 reactor sites) into the urt mask-equal shape

- **Guideline:** §6 (extract a recurring `by (bit_vector)` identity; mask-equal form preferred)
  + §13.
- **Locations:** `urt/src/slots.rs:394-413` (`lemma_set_bit`/`lemma_bit_other` — u64, mask-equal,
  the model; **do not churn**); `ipc/src/reactor.rs:307,317` (the genuine shared target — single-bit
  OR-set self/other over `u==used|(1<<g)`); kcore u32 boolean-equiv lemmas
  (`kcore/src/ready.rs:45-88`).
- **Change:** Of reactor.rs's 18 `by (bit_vector)` sites, most are distinct: lines 76/87/93/131/136
  are trailing-zeros axiom bridges (keep as one tiny bridge lemma, §6 — not the urt shape); line 338
  (`lemma_pop_lowest`) is already an extracted empty-bodied lemma; lines 397–502 are the
  `register_bound_into` whole-mask OR-in + `bits&(bits-1)` drain identities carrying local
  construction hyps (their intersection with urt's pure single-bit lemma is small, so each still
  needs its construction bridge — extraction may net more lines or regress). **Only lines 307/317**
  (single-bit OR-set over `u==used|(1<<g)`) plausibly route through a pure mask-equal u64 lemma plus
  a one-line construction bridge. Do this only if measurement shows no ipc crate-total `rlimit`
  regression. Treat the kcore mask-equal migration as a **separate u32-only strength edit** (not
  dedup; §6 width-fixing forbids sharing urt's u64 lemma) and confirm callers
  `ready.rs:162-212` still verify against the stronger mask-equal `ensures`. **Do not churn urt**
  (the model).
- **Expected effect:** dedup (narrow — ≈2 sites, not 18).
- **No-weakening check:** `cargo clean -p ipc && cargo verus verify -p ipc -- --time-expanded
  --output-json` before/after; judge by the ipc crate-total `rlimit` (inserting any proof fn
  perturbs neighbours), watching `register_bound_into`/`register_into`; hold the touched fns'
  `ensures` byte-identical; confirm ≥ 71 verified, 0 errors. Keep only if the crate total does not
  regress; revert otherwise.
- **Effort:** M.

---

*Implementation order: Phases 1–4 are independent of each other except where noted and may
proceed in parallel; Phase 2 must precede the codec-lift in 3.2; Phases 5–8 are
proof-performance work and should land after the foundational phases, with 6.5 gated on 5.1
and 6.2. Every phase re-establishes the trusted-base Baselines at ≥ the prior numbers.*
