# The trusted base — ledger

**The trusted base is exactly the seams enumerated below.** This file is the single
source of truth for `CLAUDE.md`'s "the trusted base is exactly …" claim and for
`doc/guidelines/verus.md`'s pointer (its "## The trusted base" section and Part B §11).
It is keyed to the spec's proof boundary, rev1§6.1, and to the four `external_body`
categories of `verus.md` §11. When this ledger and the code disagree, the code is
authoritative — re-derive the table with
`rg "external_body|assume_specification" --type rust` and update here.

A seam earns a row only if it names **both** a reason it is a boundary **and** the host
test that exercises it (the `verus.md` §11 audit rule). A row that cannot name both is a
finding, not a boundary.

## Scope of the verified surface (what is *not* trusted)

For calibration, the mechanized surface these seams bound (the regression baselines, §
"Baselines" below): `kcore`'s cspace/CDT, untyped retype, channel FIFO, notification
waiter queue, timer armed list, thread report record, the aspace page-table walker, and
`sysabi::decode`; the CAS decode + recovery-decision cores (`pick_survivor`,
`commit_target`, `advance_head`, `decode_frame`, `recover_records` — the recovery walk
that bounds *and* rebuilds the run, proving its `laid_out` linking invariant (B7C, T-2;
folds in the former `replay_bound` maximal-run equality) — the WAL-record structural
decode (`wal_struct_ok`/`e_payload_ok`, the verified half of `wal_content_ok`),
`validate_geometry_fields`, `decode_checked_fields`, the single-entry TLV codec), and the
**gap-freedom composition** (`lemma_gap_freedom` + `lemma_run_len_covers` /
`lemma_laid_out_mono`), now *live* — fired by `recover_records` on the rebuilt run, its
`laid_out` premise discharged rather than assumed; the IPC fixed header + window-quota
`Admission`; the DMA-pool `FreeList`; and `urt`'s slot bitmap and seqlock `utc_ns_at`. The
seams below are the irreducible remainder.

GC mark-set **sufficiency** (every object reachable from a live root is in the mark set)
and the mark **walk bound** are, by design, *neither* in the verified surface *nor* a
trusted seam: sufficiency is delivered at the rev1§6 oracle tier — one `LiveOnly`
read-through oracle driven by the `gc_mark` cargo-fuzz target and a randomized proptest,
both Miri-replayed — and the bound is structural (the mark-on-push heap work-stack, native
depth O(1)). Mechanizing reachability would drag `Hash` into the Hash-free recovery core,
so it stays test-routed (B6 Design decision 3). Recorded here so a reviewer sees the
property is test-routed, not Verus-mechanized (the rev1§6.1 "no trust-routed property
mistaken for mechanized" discipline); this routing leaves the gate unchanged (65/0 after B7C).

## The seams (13 named constructs + the by-construction category)

Grouped by the `verus.md` §11 category. Each interpreted-hash / size / std-gap seam is a
labeled `ensures`/signature contract, **not** a bare in-proof `assume` (none survive).

### (1) Hardware / scheduler / Store seam — trusted by construction

No `external_body` line: these rest on construction or a boot-setup axiom, not a stored
invariant. They are the spec's rev1§6.1(a–d) `[trusted]` parts:

| Seam | rev1§6.1 | Why trusted |
|---|---|---|
| Physical-region exclusivity | (a) | "No cap references the region" = "the untyped has no immediate CDT child"; that this implies every cap into the carved region is a CDT descendant holds *by construction* (the only frame-creation path records the untyped as parent), because the object seam carries no physical-memory model. |
| Cross-root untyped non-overlap | (b) | Disjointness within one untyped is proven (watermark monotonicity); the *independent* root untypeds' base/size constants live in `unsafe` boot code with no global frame table — their non-overlap and the int→pointer step are a boot-setup axiom. |
| Page-table join | (c) | The cap-side map **and** unmap are both proven over object state (B8A landed the map record — `map_frame` — symmetric with the unmap; the derived copy starts unmapped, a map records the entry coordinates on the cap, a delete clears them) and the raw page-table write/clear is proven over page-table memory; what stays trusted is the *join* — that the cap's recorded mapping is the true entry location and that `aspace_map`/`aspace_unmap` truly write/clear it — which lives in the unverified kernel Store. |
| Thread-lifecycle shell | (d) | B8B landed the spawn-time priority-ceiling gate as a verified refusal in `kcore::thread::set_priority` (over-ceiling → `Err`, thread untouched; accepted → priority proven `<= ceiling`), composing on the already-verified cap-ceiling attenuation. What stays trusted: the "suspended, never rescheduled" state (exception entry, syscall exit, scheduler), the anti-forgery/anti-suppression access control (rights gates + the spawn-time cap-distribution convention), and the exit/read-report syscall dispatch + register marshalling; the asm context switch is inherently unverifiable. |
| WAL queue ↔ bytes lifetime join | (c)/(e) | B7C discharges `laid_out` *at recovery* — `recover_records` rebuilds the run from the on-device bytes and proves it laid out, firing `lemma_gap_freedom`. What stays trusted is the join across the Store's *lifetime*: that the live in-memory `wal_records` queue keeps matching the WAL bytes as `write`/`flush`/`commit` mutate it. Maintaining that as a Store-wide invariant is the larger surface §6.1(e) keeps the commit routine plain Rust over; the full replay-equality invariant remains the `CommitProtocol` model's. |

**Storage durability axiom — "fsync means fsync" (rev1§4.8, §6.1(e)).** Named in the
commit/recovery model as the labeled top-level `ASSUME FsyncMeansFsync` in
`tla/commit_protocol/CommitProtocol.tla`: a completed fsync barrier makes the preceding
writes durable, and a crash never loses durable state. It is **trusted by construction**
(the QEMU/virtio-blk `cache=writeback` + FLUSH config under our control), recorded here as
the storage layer's single **axiom** — *not* a closed seam and *not* a theorem. The model
encodes it operationally (`CommitPrepare` moves `chunkBuf → durableRoots` at barrier 1;
`Crash` leaves `durableRoots` `UNCHANGED`); the `ASSUME` makes the assumption explicit and
grep-able rather than an implicit consequence of the crash semantics, as rev1§4.8 requires.

### (2) Out-of-scope total function — trust *totality + determinism only*

| Construct | Location | Reason | Host test |
|---|---|---|---|
| `checksum_ok` | `cas/src/disk.rs:337` | BLAKE3 superblock-body checksum — interpreted hashing, out of SMT scope; trusted total (inspects buffer, returns bool, no panic). `requires buf@.len()==SB_SIZE` keeps the slicing in bounds. | BLAKE3-justified per rev1§6.1(e); exercised by the superblock-decode fuzz/proptest corpora + Miri replay. |
| `wal_checksum_ok` | `cas/src/store.rs:927` | BLAKE3 WAL-record checksum (`record_checksum` over `seq‖len‖payload`) — interpreted hashing, out of SMT scope; trusted total (inspects the exact-`rlen` record, returns bool, no panic). `requires off+rlen<=wal@.len()` (from `decode_frame`) keeps the slicing in bounds. Paired with the `uninterp spec fn checksum_ok_spec` twin. **The lone uninterpreted part of the record seam after B7B (T-5) split the `WalOp` structural decode into the verified surface (`wal_struct_ok`).** | mount/recovery fuzz corpora + Miri replay; `wal_struct_ok_has_teeth` (`cas/src/store.rs:2445`) pins the structural/checksum split. |
| `u64::saturating_mul` | `kcore/src/aspace.rs:76` | vstd specs `saturating_add`/`saturating_sub` but not `_mul`; `va_range_ok` needs it. `returns` mirrors documented std saturating semantics. | std-semantics mirror (the `checked_next_multiple_of` precedent); no dedicated unit test. |
| `usize::checked_next_multiple_of` | `kcore/src/untyped.rs:258` | vstd has no spec yet; the Untyped arm needs only that it returns an `Option`, then re-checks positivity. | positivity re-checked at the call site; signature-only trust. |
| `CapSlot::empty` | `kcore/src/cspace.rs:1226` | plain-Rust `const fn` shared with the kernel shell; the `ensures` state what it builds (empty cap, all four CDT links `None`) so `slot_move`'s final clear verifies. | consumed by the verified `slot_move`; `ensures` pins the construction. |

### (3) Runtime-only guard

| Construct | Location | Reason | Host test |
|---|---|---|---|
| `debug_check_free` | `urt/src/slots.rs:340` | a `debug_assert!` double-free guard; `external_body` so Verus doesn't see the `panic!` lowering (forbidden in exec). The *static* guarantee is `SlotAlloc::free`'s `!is_free_spec` precondition. | `double_free_panics` (urt host test) pins the runtime witness. |

### (4) Opaque layout fact — size positivity

| Construct | Location | Reason | Host test |
|---|---|---|---|
| `ExTcb` | `kcore/src/untyped.rs:244` | `external_type_specification` registering `Tcb` opaque so `size_of` typechecks in the verified `carve`. | `object_size_positive` (`kcore/src/untyped.rs:759`). |
| `ExNotifObj` | `kcore/src/untyped.rs:248` | opaque registration of `NotifObj`. | `object_size_positive`. |
| `ExTimerObj` | `kcore/src/untyped.rs:252` | opaque registration of `TimerObj`. | `object_size_positive`. |
| `fixed_object_bytes` | `kcore/src/untyped.rs:272` | `ensures r > 0`; Verus can't derive `size_of::<Tcb>() > 0` for the opaque types above, so this names the size-positivity fact. | `object_size_positive`. |
| `CSpaceObj::bytes_for` | `kcore/src/untyped.rs:234` | `ensures r > 0`; the per-object size helper lives in plain Rust (shared with the shell); `carve`'s geometry needs only positivity. | `bytes_for_positive` (`kcore/src/untyped.rs:743`). |
| `Channel::bytes_for` | `kcore/src/untyped.rs:235` | `ensures r > 0`; as above. | `bytes_for_positive`. |
| `AspaceObj::bytes_for` | `kcore/src/untyped.rs:236` | `ensures r > 0`; as above. | `bytes_for_positive`. |

**Tally:** 7 `external_body` (4 kcore: `ExTcb`/`ExNotifObj`/`ExTimerObj`/`fixed_object_bytes`;
2 CAS: `checksum_ok`/`wal_checksum_ok`; 1 urt: `debug_check_free`) + 6 `assume_specification`
(3 `bytes_for` + `saturating_mul` + `checked_next_multiple_of` + `CapSlot::empty`) = **13**.

> **Reconciliation with audit §4.1.** The audit (`doc/results/0_audit_rev0.md` §4.1)
> says "three `assume_specification`s." That count collapses the three `bytes_for` into
> one "positivity" category and omits `CapSlot::empty` (`cspace.rs:1226`). Ground truth is
> **6** `assume_specification` statements, recorded above.

## `[verifying]` — seams moving into the verified surface this revision

Mirroring rev1§6.1's `[verifying]` tags; each line reads as trusted at blessing and as
mechanized after its phase lands. Update the affected rows above (and rev1§6.1) when each
phase completes.

| Transition | rev1§ | Closing phase |
|---|---|---|
| Cap-side **MAP** bookkeeping moved behind a verified object op (symmetric with unmap) | §6.1(c) | **B8A** — landed ✓ (verified `cspace::map_frame` + `ref_aspace` driving a new `Store::aspace_map` seam, term-for-term the mirror of the delete/unmap branch; gate 335→342) |
| Spawn-time **priority-ceiling gate** moved from the syscall shell into a verified op | §6.1(d), §5.4 | **B8B** — landed ✓ (the refusal is now a verified branch of `kcore::thread::set_priority`, which returns `Result`: over-ceiling → `Err` with the thread untouched, accepted → priority proven `<= ceiling`; the two shell `if prio > max_prio` gates deleted; composes on the already-verified `derive` ceiling attenuation; a refactor of an existing verified op, so the gate stays 342) |
| Per-record **structural decode** split out of `wal_content_ok`, verified like the other on-disk decoders | §6.1(e), §3.7 | **B7B** — landed ✓ (T-5; full Verus predicate, gate 58→64) |
| Model **replay-equality** mechanized by the `Recover` action property | §6.1(e), §6 | **B7A** — landed ✓ (T-1) |
| **fsync means fsync** named as a labeled `ASSUME` in the storage model | §4.8, §6.1(e) | **B7A** — landed ✓ (T-4) |

## Baselines (regression gates)

Any phase touching these must re-establish them at ≥ the prior numbers.

| Surface | Command | Result |
|---|---|---|
| kcore object core | `cargo verus verify -p kcore` | 342 verified, 0 errors |
| CAS decode + recovery cores | `cargo verus verify -p cas --no-default-features` | 65 verified, 0 errors |
| IPC header + session codecs | `cargo verus verify -p ipc` | 58 verified, 0 errors |
| DMA-pool `FreeList` (core + `is_full`/`is_allocated` wrapper-guard accessors) | `cargo verus verify -p dma-pool` | 29 verified, 0 errors |
| urt slots + time | `cargo verus verify -p urt` | verified (slot bitmap + `utc_ns_at`) |
| TLA+ | `CommitProtocol` (6886 states; the `RecoverReconstructs` replay-equality action property + the committed negative control `CommitProtocol_NegControl.cfg`, which reports the expected violation), `CapRevocation`/`_Teardown` (~799k, recorded run), `IpcReactor` (with a negative control) | pass |
| Fuzzing | wire/on-disk/ELF decoders + mount/recovery cargo-fuzz targets + the GC mark-walk target (`gc_mark`), committed corpora + Miri replay | green |

---

*This ledger is the enumerated source of record; the historical dated technique findings
(`21…67_verus-findings.md`) and the Verus-rewrite plan it distilled are not retained
in-tree (see `doc/guidelines/verus.md`).*
