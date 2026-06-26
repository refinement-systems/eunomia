# The trusted base — ledger

**The trusted base is exactly the seams enumerated below.** This file is the single
source of truth for `CLAUDE.md`'s "the trusted base is exactly …" claim and for
`doc/guidelines/verus.md`'s pointer (its "## The trusted base" section and Part B §11).
It is keyed to the spec's proof boundary, rev2§6.1, and to the four `external_body`
categories of `verus.md` §11. When this ledger and the code disagree, the code is
authoritative — re-derive the table with
`rg "external_body|assume_specification" --type rust` and update here.

A seam earns a row only if it names **both** a reason it is a boundary **and** the host
test that exercises it (the `verus.md` §11 audit rule). A row that cannot name both is a
finding, not a boundary.

## Scope of the verified surface (what is *not* trusted)

For calibration, the mechanized surface these seams bound (the regression baselines, §
"Baselines" below): `kcore`'s cspace/CDT (including the **preemptible revoke walk** — the
bounded `revoke_step`, the revoke-in-progress `revoking` marker on the root, and the `derive`
ancestor-guard that refuses growth into a revoking subtree, all per-step-verified in Verus;
its cross-restart interleaving safety and completion-under-the-guard liveness modeled in the TLA
`CapRevocation` model), untyped retype, channel FIFO, notification
waiter queue, timer armed list, the **IRQ-handler object** (the timer object's census twin —
the verified `irq_bind`/`irq_unbind`/`destroy_irq` ops and the `irq_binding_refs` census term,
*minus* the armed list: delivery is by direct INTID lookup, so there is no chain to verify),
the **32-level ready queue** (its per-level
`ready_chain`/`ready_seq` witnesses, `u32` bitmap-coherence invariant, and the four ops
`top_ready`/`ready_enqueue`/`ready_dequeue`/`ready_unqueue`, integrated through the
`make_runnable`/`unqueue_ready` seams and threaded across `signal`/`fire`/the IPC fast path
/the cspace teardown SCC/`destroy_tcb`; the scheduler *policy* and asm context switch
stay trusted, §6.1(d)), thread report record, the aspace page-table walker (with the
verified pool-growth lemma `grow_pool` — `lemma_grow_pool` + its monotone-widening
helper `lemma_pool_index_widen` and per-VA stability core `lemma_grow_pool_lookup` —
proving a contiguous pool extension preserves `pt_wf` and every existing mapping,
rev2§2.5 "accepts top-ups"), and
`sysabi::decode`; the CAS decode + recovery-decision cores (`pick_survivor`,
`commit_target`, `advance_head`, `decode_frame`, `recover_records` — the recovery walk
that bounds *and* rebuilds the run, proving its `laid_out` linking invariant and its
`replay_bound` maximal-run equality) — the WAL-record structural
decode (`wal_struct_ok`/`e_payload_ok`, the verified half of `wal_content_ok`),
`validate_geometry_fields`, `decode_checked_fields`, the single-entry TLV codec, the
directory **node decoder** — `decode_node`, total ∀ bytes plus the leaf canonical
round-trip (`canonical_leaf_bytes` / `encode_node_leaf`), the last CAS on-disk decoder
in the verified surface (Hash-free, composes on `decode_raw`),
and the directory **level partition core** — `split_points`/`boundary_flags`, proving
`build_level`'s node-cutting is a lossless, ordered partition (`lemma_partition_flatten`:
conservation — no item dropped, duplicated, or reordered) with ≤ `MAX_NODE_ENTRIES`
fanout and boundary discipline (cut only at a boundary or the cap), over the **opaque**
`is_boundary` predicate so the proof never models BLAKE3 (the BLAKE3 split rule is a
trusted seam below)),
and the **gap-freedom composition** (`lemma_gap_freedom` + `lemma_run_len_covers` /
`lemma_laid_out_mono`), *live* — fired by `recover_records` on the rebuilt run, its
`laid_out` premise discharged rather than assumed; the IPC fixed header + window-quota
`Admission` (with the reactor's verified `used`-mask bit-allocator core
`lowest_clear_bit`, the kcore ready-queue-bitmap pattern — Baselines below); the
shared `FreeList` (in the `freelist` crate) — **the
verified allocation algorithm behind both `dma-pool` and the `urt` heap
allocator** (first-fit search, alignment round-up, split, two-sided address-ordered coalesce,
proven over the side-stored `(offset, len)`-extent model); and `urt`'s slot bitmap, seqlock
`utc_ns_at`, and that heap free-list. The `urt` heap's arena byte-region (`UnsafeCell<[u8; N]>`
+ the `base.add(off)` / `(p as usize) - base` seam) is the lone trusted plain-Rust step in
the allocator — the DMA-pool wrapper's posture exactly, kept honest by the Miri+proptest
tier rather than a `verus!{}` construct (so it adds nothing to the seam tally; Baselines below).
The seams below are the irreducible remainder.

GC mark-set **sufficiency** (every object reachable from a live root is in the mark set)
and the mark **walk bound** are, by design, *neither* in the verified surface *nor* a
trusted seam: sufficiency is delivered at the rev2§6 oracle tier — one `LiveOnly`
read-through oracle driven by the `gc_mark` cargo-fuzz target and a randomized proptest,
both Miri-replayed — and the bound is structural (the mark-on-push heap work-stack, native
depth O(1)). Mechanizing reachability would drag `Hash` into the Hash-free recovery core,
so it stays test-routed. Recorded here so a reviewer sees the
property is test-routed, not Verus-mechanized (the rev2§6.1 "no trust-routed property
mistaken for mechanized" discipline).

The IPC reactor's **multi-source dispatch arithmetic** is Verus-mechanized for all
inputs over a single named `wf` **dispatch invariant** (`slots.len() == 64` and the
`coherent` slot/used bijection): the lowest-clear bit allocation (`lowest_clear_bit`,
and `alloc_lowest` which records it — sets exactly that bit, refuses only on a full
word), **both** registration paths proven to preserve `wf` — the channel `register`
path (`register_into` — pick the lowest clear bit, fill exactly that slot, `Full`
(state unchanged) iff the word is exhausted) and the `register_bound` mask path
(`register_bound_into` over the `coherent` invariant — a slot is registered iff its
`used` bit is set; `Taken` leaves state unchanged; an accepted mask sets exactly its
bits, the set bits scanned low-to-high via `bits & (bits - 1)` under the
`lemma_pop_lowest` clear-lowest identity) — and the `pending` drain step (`drain_one`
— the lowest set bit, cleared exactly). These are the deductive, all-inputs twins of
the three reactor proptests (`alloc_bit_is_lowest_clear` over `alloc_lowest`,
`register_sequence_keeps_used_coherent`, `pending_drain_is_lowest_first`), which are
**kept** as the companion oracle tier (Baselines `-p ipc` row).

What stays, by design, *neither* Verus-mechanized *nor* a trusted seam: the channel
`Reactor::register` shell's `Transport::bind`s + poll-once `notif_signal` (the verified
`register_into` computes the lowest-clear allocation and the shell commits it only
*after* every bind succeeds — so a bind failure leaves the reactor untouched — but the
binds and the self-signal themselves are the trust-routed concurrent half), `wait`'s
unbounded **blocking loop** and `Transport::notif_wait` — the concurrent
word-check-before-block half of lost-wakeup safety (`drain_one` verifies the pure
per-iteration drain *step*, not the loop or the block) — and the endpoint
**cap-marshalling** (`cap_slots` ↔ the kernel-ABI `[u32;4]`/`SLOT_NONE`). The
`IpcReactor` TLA model is **single-source by construction** (one on-readable + one
on-writable bit; `tla/ipc_reactor/IpcReactor.tla` scope note), so the multi-bit dispatch
is not TLA-modeled, and the *concurrent* wakeup/backpressure execution is the
Loom/Shuttle harnesses' (`ipc/src/model.rs`), with the TLA model as the protocol-design
oracle. Recorded here so a reviewer reads exactly which dispatch facts are
Verus-mechanized (the pure bit arithmetic + coherence) and which remain trust-routed
(the `Transport` seam, the blocking loop, cap-marshalling) — the same rev2§6.1 "no
trust-routed property mistaken for mechanized" discipline. **No trusted seam added**
(the tally stays 14): the new cores are pure `u64`/array reasoning over `vstd`'s
`axiom_u64_trailing_zeros` and `bit_vector`.

**IPC `Admission` quota routing note.** The `Admission` bulk-window quota
(`ipc/src/session.rs`, rev2§3.5) is Verus-mechanized: `well_formed` (granted ≤ budget)
is a `requires`/`ensures` on every `admit`/`release`, so the never-over-grant accounting
holds for *all* operation sequences by modular composition (the §14 `verus.md`
verified-accounting template the `used`-mask dispatch reuses). This makes the *invariant*
arm of the concurrent `fairness_smoke` harness (`ipc/src/model.rs`, asserting exactly
`min(budget, N)` grants under N client threads) redundant with the proof — but that
harness is **kept**, because it additionally witnesses what Verus does not: that the
concurrent plumbing calls `admit` *atomically* under thread interleaving (the
Shuttle-routed `fairness_smoke_shuttle` arm). The invariant overlaps; the
interleaving-atomicity check does not. No trusted seam, no Baseline change: `Admission`
is already counted in the `-p ipc` `71 verified` total below.

**virtio-blk avail-ring index routing note.** The virtqueue avail-ring slot
arithmetic (`virtio-blk/src/lib.rs`, rev2§2.5) is Verus-mechanized: `avail_ring_slot`
carries an `ensures` fixing the slot offset to `4 + 2*(idx % qsize)` and proving its
two bytes always land inside the `6 + 2*qsize` avail buffer `new()` allocates, ∀ `u16`
idx and qsize `1..=8` — the deductive all-inputs twin of the `avail_ring_slot_in_bounds`
/ `avail_index_wraps_consistently` ring proptests, which are **kept** as the companion
oracle tier (Baselines `-p virtio-blk` row). What stays trust-routed, by design: `qsize`
is the caller's MMIO bring-up precondition (`new()` derives it as `max.min(8) as u16`
after rejecting a zero `QUEUE_NUM_MAX`, on the trusted device-init path), so the
`submit` call site stays external; and the
*device-shared* virtqueue itself — the ring the driver and the device race on — is the
trusted DMA/hardware seam (rev2§2.5), not a Verus or TLA obligation (the ring is not a
Rust thread; the host fake is single-threaded by `SharedMem`'s contract). No trusted
seam added (the tally stays 14): the core is pure `u16`/modulo reasoning, citing no
vstd axiom. **Onboarding note:** virtio-blk is the first gated crate that sits *above*
other gated crates in the dependency graph (it links `dma-pool`→`freelist` and `cas`).
`cargo verus verify -p virtio-blk` therefore re-verifies those deps in-session, and
because `cas` turns on `vstd`'s `alloc` feature the whole session runs under the larger
alloc prelude — see the `freelist` Baseline row for why its two merge proofs carry
alloc-sized `rlimit` budgets.

**virtio-blk LBA-bound routing note.** The defensive capacity check
(`check_capacity`, rev2§4.5) is now Verus-mechanized: its pure arithmetic is the free
`capacity_check(lba, len, capacity)`, whose `ensures` proves the refusal (`OutOfRange`)
fires *exactly* when the last sector `lba + len/SECTOR` exceeds `capacity` — overflow-safe
for *all* `(lba, len, capacity)` via a single `checked_add`, so an adversarial `lba` near
`u64::MAX` refuses rather than wrapping into a valid-looking range. It is the deductive
all-inputs twin of the `lba_past_capacity_refused_locally` integration test and the new
`capacity_check_*` proptests/teeth, all **kept** as the companion oracle tier (Baselines
`-p virtio-blk` row). What stays trust-routed, by design: `capacity` is read once from the
device's MMIO config space on the trusted bring-up path — the verified property is the
no-wrap *refusal* arithmetic, not the device's honesty about its own geometry, which
remains the trusted DMA/hardware seam (rev2§2.5). No trusted seam added (the tally stays
14): the only library fact cited is `vstd`'s `checked_add` `Option` spec, not a project
`external_body`/`assume_specification`.

**IpcReactor `FifoPerChannel` + local `NoDrop` routing note.** The kcore channel ring's
per-step FIFO discipline (`kcore/src/channel.rs`, rev2§3.3) is now Verus-mechanized under
the TLA-invariant names it refines: `send`'s `ensures` carries `fifo_send_appends` (the
sending ring's FIFO `Seq` grows by `Seq::push` at the tail) and `recv`'s carries
`fifo_recv_pops_head` (the drained ring loses its head by `Seq::drop_first`) — the
per-step *local* half of IpcReactor `FifoPerChannel`
(`tla/ipc_reactor/IpcReactor.tla:279`); a refused `send`/`recv` carries
`no_drop_on_refusal` (the store's slot/chan/refs views all unchanged) — the per-step
*local* half of IpcReactor `NoDrop` (`IpcReactor.tla:274`, "Full is the only refusal").
These are `open spec fn` labels over already-proven `ensures` (discharged by the existing
`channel::lemma_send_fifo_push`/`lemma_recv_fifo_drop_first`/`lemma_ring_fifo_frame`), so
they add *no* new coverage — they make the existing per-step facts *read as* the
invariants they mechanize. What stays TLA-owned, by design: the *global* arms the kcore
ring cannot witness because it holds only the live window `[head, head + count)` and keeps
no `recvd`/`nextSend` history — `NoDrop`'s counting identity
`nextSend = |recvd| + |queue|` (`IpcReactor.tla:275`) and `FifoPerChannel`'s global-index
arms `recvd[i] = i` / `queue[i] = |recvd| + i` (`IpcReactor.tla:280-281`). The TLA
`IpcReactor` model is **not** retired or demoted — only the local per-step refinement
moves to Verus. No trusted seam added (the tally stays 14) and no Baseline change: kcore
stays `404 verified` (a non-recursive `spec fn` carries no proof obligation), below.

**CapRevocation `FireSafe` routing note.** The rev2§5.1 firing obligation — a non-NULL TCB
binding slot always names a *live* cap, so a thread-death fire signals a live object or
skips a cleared slot, never freed memory — is now Verus-mechanized as
`cspace::fire_safe(store)` (`kcore/src/cspace.rs`): for every *resident* TCB bind slot,
`cap_notif` `Some(nn)` ⇒ `nn ∈ notif_view.dom()`. It is the whole-store corollary of the
already-verified `caps_consistent` invariant — `lemma_fire_safe_from_caps_consistent`
(`requires caps_consistent`, `ensures fire_safe`) discharges it in one step (a resident
bind slot's `Notification(nn)` cap is non-empty ⇒ `cap_consistent` ⇒ `notif_wf(nn)` ⇒ `nn`
live). `thread::report_terminal` (the firing site) carries it as a named `ensures`
(`caps_consistent(old) ==> fire_safe(final)`, the conditional idiom `signal`/`fire` use for
system invariants), discharged by the corollary plus `lemma_fire_safe_frame` over the
`slot_view`/`bind_slots`/notif-domain frame `set_tcb_report` + `signal` already give. This
names an already-entailed fact; it adds *no* new safety coverage — `revoke_step`/
`destroy_tcb` (which empty bind slots) already `ensure caps_consistent(final)`, so
`fire_safe` is a zero-cost corollary at their call sites and was deliberately **not**
bolted on as a redundant `ensures` (measured flat on `destroy_tcb` — the §10
establish-vs-consume cost is a *consume*-side risk, and these ops *establish*, so the
omission is on cleanliness grounds, not cost). This satisfies the
`caprevoke-liveparent-ensures-guide` dependency by **confirmation**: TLA `LiveParent`
(`tla/cap_revocation/CapRevocation.tla:380`) is mechanized as the `cap_consistent`
Thread-arm (bind slots + cspace resident) + Notification-arm (`notif_wf ⇒ live`) under
`caps_consistent`, which `bind`/`destroy_tcb`/`revoke_step` all require and ensure. What
stays TLA-owned, by design: the *global* cross-restart arm `DeadNowhere` over the whole
`CapIds` space (`CapRevocation.tla:374`, which *implies* `FireSafe`) and the preemptible
revoke walk's `EventuallyRevoked` liveness — the TLA `CapRevocation` model is **not**
retired or demoted. No trusted seam added (the tally stays 14); kcore Baseline rises
`404 → 406` (the two new `proof fn`s; the `fire_safe` `spec fn` is non-recursive, +0),
below.

**CommitProtocol `AtLeastOneValidSlot` + `GenerationsDistinct` routing note.** The two CAS
recovery-decision functions (`cas/src/store.rs`, rev2§4.5) now read as the mechanized
*local per-call* half of the two `CommitProtocol` safety invariants. `pick_survivor`'s
`ensures (valid_a && valid_b) ==> ((r is SlotA) <==> gen_a >= gen_b)` is the per-call
witness of TLA `GenerationsDistinct` (`tla/commit_protocol/CommitProtocol.tla:247`): under
two valid slots the winner is fixed by generation, so `LiveSlot` is deterministic.
`commit_target`'s `ensures r != live_slot(sb_in_b)` is the by-construction witness of TLA
`AtLeastOneValidSlot` (`CommitProtocol.tla:244`, the rev2§4.5 `Crash` three-outcome safety):
a commit never targets the live slot, so a torn write damages only the slot being written.
Both `ensures` **already existed and verified** — this labels them in place (inline comments
on the clauses), adding *no* new coverage. What stays TLA-owned, by design: the *global*
`AtLeastOneValidSlot`/`GenerationsDistinct` invariants as crash-step invariants over the
whole `slotA`/`slotB` × `walLog`/`writeCtr` state the verified pure core does not model
(checked by the 6886-state `CommitProtocol` TLC run + `CommitProtocol_NegControl.cfg`,
below) — and, by the same routing, the `Crash` three-outcome safety, the cross-restart
`Recover`/`RecoverReconstructs` *global* replay-equality (the headline recovery arm; its
local WAL-byte/queue projection is now Verus-mechanized — the Task 13 routing note below),
and the `FsyncMeansFsync` storage axiom (recorded above) all stay TLA-owned +
by-construction; only these two per-call witnesses are Verus-mechanized. The TLA
`CommitProtocol` model is **not** retired or demoted. No trusted seam added (the tally stays
14) and no Baseline change *from Task 12*: cas stayed `75 verified` then (a clause comment
carries no proof obligation); the Task 13 projection below raises it to `77`.

**CommitProtocol `RecoverReconstructs` (WAL-projection) routing note.** The CAS recovery
walk (`cas/src/store.rs::recover_records`, rev2§4.5) now carries a named `ensures` —
`recover_reconstructs(wal@, r.records@, wal_head, wal_next_seq, r.forged_max)` — that reads
as the mechanized *local per-call* half of TLA `RecoverReconstructs`
(`tla/commit_protocol/CommitProtocol.tla:281`): the rebuilt run *is exactly* the maximal
seq-continuous, content-valid post-head record skeleton (sound — every record `laid_out` and
head-anchored — and maximal — the run accounts for the whole `run_len`, with `forged_max` the
lone seq-ceiling record counted past it). The predicate was *already proven* by the walk; the
projection `spec fn` + its corollary `lemma_recover_reconstructs` name it, and the teeth
control `lemma_recover_reconstructs_pins_head` pins the head bound so a deliberately-wrong
(off-by-one) anchor fails it — the green proof is not vacuous over its sole producer (a
temporary `(wal_head + 1)` `ensures` was confirmed to *fail to verify*, then reverted; see
`doc/results/13_verus-findings.md`). What stays TLA-owned + by-construction, by design: the
*global* `AckedWritesRecoverable`/`RecoverReconstructs` over `writeCtr`/`walLog`
(`CommitProtocol.tla:261`/`:281`) — global acked-write state the verified core does not model
— and the **WAL queue ↔ bytes lifetime join** (the seam row below, rev2§6.1(e)); the
6886-state TLC run + `CommitProtocol_NegControl.cfg` stay the design oracle. The TLA
`CommitProtocol` model is **not** retired or demoted. No trusted seam added (the tally stays
14, the lifetime-join row already exists); cas Baseline rises `75 → 77` (the corollary +
teeth `proof fn`s; the `recover_reconstructs` `spec fn` is non-recursive, +0; the new
`recover_records` ensures adds no item), below.

## The seams (14 named constructs + the by-construction category)

Grouped by the `verus.md` §11 category. Each interpreted-hash / size / std-gap seam is a
labeled `ensures`/signature contract, **not** a bare in-proof `assume` (none survive).

### (1) Hardware / scheduler / Store seam — trusted by construction

No `external_body` line: these rest on construction or a boot-setup axiom, not a stored
invariant. They are the spec's rev2§6.1(a–d) `[trusted]` parts:

| Seam | rev2§6.1 | Why trusted |
|---|---|---|
| Physical-region exclusivity | (a) | "No cap references the region" = "the untyped has no immediate CDT child"; that this implies every cap into the carved region is a CDT descendant holds *by construction* (the only frame-creation path records the untyped as parent), because the object seam carries no physical-memory model. |
| Cross-root untyped non-overlap | (b) | Disjointness within one untyped is proven (watermark monotonicity); the *independent* root untypeds' base/size constants live in `unsafe` boot code with no global frame table — their non-overlap and the int→pointer step are a boot-setup axiom. |
| Page-table join | (c) | The cap-side map **and** unmap are both proven over object state (the map record — `map_frame` — is symmetric with the unmap; the derived copy starts unmapped, a map records the entry coordinates on the cap, a delete clears them) and the raw page-table write/clear is proven over page-table memory; what stays trusted is the *join* — that the cap's recorded mapping is the true entry location and that `aspace_map`/`aspace_unmap` truly write/clear it — which lives in the unverified kernel Store. |
| Thread-lifecycle shell | (d) | The spawn-time priority-ceiling gate is a verified refusal in `kcore::thread::set_priority` (over-ceiling → `Err`, thread untouched; accepted → priority proven `<= ceiling`), composing on the already-verified cap-ceiling attenuation. What stays trusted: the "suspended, never rescheduled" state (exception entry, syscall exit, scheduler), the anti-forgery/anti-suppression access control (rights gates + the spawn-time cap-distribution convention), and the exit/read-report syscall dispatch + register marshalling; the asm context switch is inherently unverifiable. |
| IRQ-delivery shell | (c)/(d) | The boot-static `IRQ_TABLE` of `IrqObj` (the device-MMIO-frame precedent, *not* retyped), the INTID→object lookup (the timer `ARMED_HEAD`-resolution analog), the device-IRQ delivery path (mask-on-deliver + the verified `notification::signal`), and the per-IRQ GIC mask/unmask the `IrqBind`/`IrqAck` syscalls drive — the int→ptr shell over the verified `kcore::irq` core (`irq_bind`/`irq_unbind`/`destroy_irq` + the `irq_binding_refs` census, reached through the Store seam). The twin of the timer tick shell, under the same "scheduler/asm shell stays trusted" umbrella, so it is **not** a new seam and the tally stays 14. Host witness: m1-test stage 7 (`scripts/m1-test.sh`) signals a bound PL011 IRQ-handler cap's notification through the real GIC + exception path (the `1234567M1 PASS` regression marker). |
| WAL queue ↔ bytes lifetime join | (c)/(e) | `laid_out` is discharged *at recovery* — `recover_records` rebuilds the run from the on-device bytes and proves it laid out, firing `lemma_gap_freedom`. What stays trusted is the join across the Store's *lifetime*: that the live in-memory `wal_records` queue keeps matching the WAL bytes as `write`/`flush`/`commit` mutate it. Maintaining that as a Store-wide invariant is the larger surface §6.1(e) keeps the commit routine plain Rust over; the full replay-equality invariant remains the `CommitProtocol` model's. |

**Storage durability axiom — "fsync means fsync" (rev2§4.8, §6.1(e)).** Named in the
commit/recovery model as the labeled top-level `ASSUME FsyncMeansFsync` in
`tla/commit_protocol/CommitProtocol.tla`: a completed fsync barrier makes the preceding
writes durable, and a crash never loses durable state. It is **trusted by construction**
(the QEMU/virtio-blk `cache=writeback` + FLUSH config under our control), recorded here as
the storage layer's single **axiom** — *not* a closed seam and *not* a theorem. The model
encodes it operationally (`CommitPrepare` moves `chunkBuf → durableRoots` at barrier 1;
`Crash` leaves `durableRoots` `UNCHANGED`); the `ASSUME` makes the assumption explicit and
grep-able rather than an implicit consequence of the crash semantics, as rev2§4.8 requires.

### (2) Out-of-scope total function — trust *totality + determinism only*

| Construct | Location | Reason | Host test |
|---|---|---|---|
| `checksum_ok` | `cas/src/disk.rs:342` | BLAKE3 superblock-body checksum — interpreted hashing, out of SMT scope; trusted total (inspects buffer, returns bool, no panic). `requires buf@.len()==SB_SIZE` keeps the slicing in bounds. | BLAKE3-justified per rev2§6.1(e); exercised by the superblock-decode fuzz/proptest corpora + Miri replay. |
| `wal_checksum_ok` | `cas/src/store.rs:1111` | BLAKE3 WAL-record checksum (`record_checksum` over `seq‖len‖payload`) — interpreted hashing, out of SMT scope; trusted total (inspects the exact-`rlen` record, returns bool, no panic). `requires off+rlen<=wal@.len()` (from `decode_frame`) keeps the slicing in bounds. Paired with the `uninterp spec fn checksum_ok_spec` twin. **The lone uninterpreted part of the record seam: the `WalOp` structural decode is in the verified surface (`wal_struct_ok`, covering the tag-3 `Rename` arm), so only the checksum is trusted.** | mount/recovery fuzz corpora + Miri replay; `wal_struct_ok_has_teeth` (`cas/src/store.rs:4562`) pins the structural/checksum split (tags 1–3). |
| `is_boundary` | `cas/src/prolly.rs:1457` | BLAKE3 directory split rule (an item is a node boundary iff the low `SPLIT_BITS` bits of `Hash::of(item)` are zero, rev2§4.1) — interpreted hashing, out of SMT scope; trusted **total** (hashes a slice, returns a bool, never panics — `as_bytes()[..8]` is always 8 of the 32 hash bytes). Totality + determinism only, **no injectivity**: the verified partition core (`split_points`, via `boundary_flags`) is proven *around* it — conservation + boundary discipline + ≤ `MAX_NODE_ENTRIES` — for *any* predicate, so the partition is correct regardless of which items boundary. Paired with the `uninterp spec fn is_boundary_spec` twin. | the `canonical_form`/`roundtrip`/`structural_sharing_on_small_edit` proptests + `split_points_*`/`boundary_flags_faithful_to_predicate` unit tests (`cas/src/prolly.rs`) drive `Dir::save` → `build_level` → `split_points`/`is_boundary`; the `tree_node`/`mount_recovery` fuzz corpora replay it. |
| `u64::saturating_mul` | `kcore/src/aspace.rs:76` | vstd specs `saturating_add`/`saturating_sub` but not `_mul`; `va_range_ok` needs it. `returns` mirrors documented std saturating semantics. | std-semantics mirror (the `checked_next_multiple_of` precedent); no dedicated unit test. |
| `usize::checked_next_multiple_of` | `kcore/src/untyped.rs:258` | vstd has no spec yet; the Untyped arm needs only that it returns an `Option`, then re-checks positivity. | positivity re-checked at the call site; signature-only trust. |
| `CapSlot::empty` | `kcore/src/cspace.rs:1595` | plain-Rust `const fn` shared with the kernel shell; the `ensures` state what it builds (empty cap, all four CDT links `None`) so `slot_move`'s final clear verifies. | consumed by the verified `slot_move`; `ensures` pins the construction. |

### (3) Runtime-only guard

| Construct | Location | Reason | Host test |
|---|---|---|---|
| `debug_check_free` | `urt/src/slots.rs:340` | a `debug_assert!` double-free guard; `external_body` so Verus doesn't see the `panic!` lowering (forbidden in exec). The *static* guarantee is `SlotAlloc::free`'s `!is_free_spec` precondition. | `double_free_panics` (urt host test) pins the runtime witness. |

### (4) Opaque layout fact — size positivity

| Construct | Location | Reason | Host test |
|---|---|---|---|
| `ExTcb` | `kcore/src/untyped.rs:246` | `external_type_specification` registering `Tcb` opaque so `size_of` typechecks in the verified `carve`. | `object_size_positive` (`kcore/src/untyped.rs:820`). |
| `ExNotifObj` | `kcore/src/untyped.rs:250` | opaque registration of `NotifObj`. | `object_size_positive`. |
| `ExTimerObj` | `kcore/src/untyped.rs:254` | opaque registration of `TimerObj`. | `object_size_positive`. |
| `fixed_object_bytes` | `kcore/src/untyped.rs:273` | `ensures r > 0`; Verus can't derive `size_of::<Tcb>() > 0` for the opaque types above, so this names the size-positivity fact. | `object_size_positive`. |
| `CSpaceObj::bytes_for` | `kcore/src/untyped.rs:234` | `ensures r > 0`; the per-object size helper lives in plain Rust (shared with the shell); `carve`'s geometry needs only positivity. | `bytes_for_positive` (`kcore/src/untyped.rs:804`). |
| `Channel::bytes_for` | `kcore/src/untyped.rs:235` | `ensures r > 0`; as above. | `bytes_for_positive`. |
| `AspaceObj::bytes_for` | `kcore/src/untyped.rs:236` | `ensures r > 0`; as above. | `bytes_for_positive`. |

**Tally:** 8 `external_body` (4 kcore: `ExTcb`/`ExNotifObj`/`ExTimerObj`/`fixed_object_bytes`;
3 CAS: `checksum_ok`/`wal_checksum_ok`/`is_boundary`; 1 urt: `debug_check_free`) + 6
`assume_specification` (3 `bytes_for` + `saturating_mul` + `checked_next_multiple_of` +
`CapSlot::empty`) = **14**. The `is_boundary` BLAKE3 split rule is the 3rd CAS
interpreted-hash seam, proven *around* by the verified partition core.

> **The `urt` heap arena seam is *not* one of these 14.** Like the DMA-pool wrapper, the
> heap allocator's trusted step — `UnsafeCell<[u8; N]>` interior mutability + `base.add(off)` /
> `(p as usize) - base` — is plain-Rust wrapper code, **not** a `verus!{}` `external_body` /
> `assume_specification` construct. It is kept honest by the Miri+proptest tier (Baselines),
> so it stays outside this tally of **14**. The heap's *algorithm* is verified (the `freelist`
> proof), not trusted; only the byte-region boundary is trusted, exactly as for `dma-pool`.

> **On the `assume_specification` count.** A "three `assume_specification`s" reading
> collapses the three `bytes_for` into one "positivity" category and omits
> `CapSlot::empty` (`cspace.rs:1595`). Ground truth is **6** `assume_specification`
> statements, recorded above.

> **The 10 transparent cspace `external_type_specification` registrations are not seams
> and add 0 to the tally.** `kcore/src/cspace.rs:268-324` defines 10
> `#[verifier::external_type_specification]` + `#[verifier::ext_equal]` wrappers
> (`ExSlotId`, `ExObjId`, `ExRights`, `ExChanEnd`, `ExCapKind`, `ExCap`, `ExCapSlot`,
> `ExBinding`, `ExThreadState`, `ExReport`) that give plain-Rust types structural `==`
> in spec code. Unlike the 3 opaque untyped registrations (`ExTcb`/`ExNotifObj`/`ExTimerObj`
> in `kcore/src/untyped.rs:246-254`), none carry `external_body` and none introduce a
> trusted axiom or opaque size fact — they are transparent Verus scaffolding, erased in a
> normal build. The tally remains **14**.

## Verified surfaces governed by rev2§6.1's `[verifying]` tags

These constructs are in the verified surface (and the TLA models), each mirroring a
rev2§6.1 `[verifying]` tag. Keep these rows and rev2§6.1 in sync with the code.

| Verified construct | rev2§ |
|---|---|
| Cap-side **MAP** bookkeeping behind a verified object op (symmetric with unmap): `cspace::map_frame` + `ref_aspace` driving the `Store::aspace_map` seam, term-for-term the mirror of the delete/unmap branch | §6.1(c) |
| Spawn-time **priority-ceiling gate** as a verified branch of `kcore::thread::set_priority`, which returns `Result`: over-ceiling → `Err` with the thread untouched, accepted → priority proven `<= ceiling`; composes on the already-verified `derive` ceiling attenuation | §6.1(d), §5.4 |
| Per-record **structural decode** split out of `wal_content_ok` (full Verus predicate), verified like the other on-disk decoders | §6.1(e), §3.7 |
| Model **replay-equality** mechanized by the `Recover` action property | §6.1(e), §6 |
| **fsync means fsync** named as a labeled `ASSUME` in the storage model | §4.8, §6.1(e) |

## Baselines (regression gates)

Any phase touching these must re-establish them at ≥ the prior numbers.

| Surface | Command | Result |
|---|---|---|
| kcore object core | `cargo verus verify -p kcore` | 406 verified, 0 errors (includes `thread::destroy_tcb`'s per-phase frame lemmas — `lemma_destroy_tcb_halt_frame` and the cspace/aspace `lemma_destroy_tcb_*_clear_frame` twins — each keying one teardown phase's edit shape to the running cross-object frame; the notification census-delta map lemmas `cspace::lemma_waiter_dequeue_census`/`lemma_waiter_enqueue_census`, keying a one-waiter dequeue/enqueue to the per-object `obj_census` map for `remove_waiter`/`wait`; `cspace::lemma_unlink_merge`, keying `cdt_unlink`'s closing merge case-split (the spliced arena equals the closed-form `unlinked`) to the straight-line splice chain, off the children-walk `next_reach`/`valid_srank` quantifiers; `cspace::lemma_children_walk_peel`, keying the shared per-iteration cursor advance (`cur`→`nn`) in `cdt_unlink`/`slot_move` to a one-step `next_reach` unfold (sibling reachability unchanged for every other node); and the channel post-loop frame lemmas `channel::lemma_{recv,send}_chan_wf`/`lemma_recv_fifo_drop_first`/`lemma_send_fifo_push`, keying each op's `chan_wf`/`ring_fifo` re-establishment to the head/count shift + per-ring-slot facts, off the pass-2 loop's `dests`/`caps` quantifiers; `channel::lemma_ring_fifo_frame`, keying an unchanged ring's `ring_fifo` to its per-position `ring_msg` congruence (shared by `send`/`recv`); the FIFO-label `open spec fn`s `channel::fifo_send_appends`/`fifo_recv_pops_head`/`no_drop_on_refusal`, naming the per-step *local* half of IpcReactor `FifoPerChannel`/`NoDrop` as `ensures` on `send`/`recv` (a non-recursive `spec fn` carries no proof obligation — the count is unchanged; see the routing note above); the CapRevocation `FireSafe` corollary `cspace::lemma_fire_safe_from_caps_consistent` (`caps_consistent ⇒ fire_safe`, the rev2§5.1 firing obligation named where it is cheaply entailed) and its light companion `cspace::lemma_fire_safe_frame` (`fire_safe` carries across the `slot_view`/`bind_slots`/notif-domain frame), the +2 verified items behind the `404 → 406` rise — `fire_safe` itself is a non-recursive `spec fn` (+0) carried as a named `ensures` on `thread::report_terminal` (see the routing note above); and `thread::lemma_running_frame_trans`, folding the four running cross-object frames over two adjacent `destroy_tcb` teardown edges into one composition; the `external_body`/`assume_specification` tally is **14**) |
| CAS decode + recovery cores | `cargo verus verify -p cas --no-default-features` | 79 verified, 0 errors (includes the per-entry codec `decode_raw`/`encode_raw`, each splitting its content section out to the `decode_content`/`encode_content` helpers; the little-endian read machinery — the `u{16,32,64}_le` specs, their `lemma_u{16,32,64}_le_bytes` byte-split identities, and the `read_u{16,32,64}_le` readers — now lives in the shared `le-bytes` crate (its own Baseline row below), cited here by full path (`le_bytes::u*_le`, `le_bytes::read_u*_le`) per `verus.md` §6/§12; cas keeps its cas-only `read_arr32` digest reader and the `push_u{16,32,64}_le` writers, whose `ensures` cite `le_bytes::u*_le` (sound because the shared specs are `open`); the `s_payload_ok`/`e_payload_ok` payload decoders, each dispatching by tag byte to the `{write,unlink,rename}`-arm twins (the tag-3 `Rename` arm included); the directory **level partition core** — `split_points`, `boundary_flags`, the `block_start` spec helper, and the conservation lemmas `flatten_blocks`/`lemma_flatten_covers`/`lemma_partition_flatten` — proven over the opaque `is_boundary` seam, the one trusted construct here; and the **node decoder** `decode_node` total ∀ bytes + leaf canonical round-trip, `encode_node_leaf`, `entries_bytes`/`canonical_leaf_bytes`/`lemma_entries_push`; the **chunk-list codec** in the `cas/src/file.rs` `verus!{}` island — `decode_chunk_list` total ∀ bytes + canonical framing against `chunk_list_bytes` (the on-disk `[MAGIC][count u32][ (32-byte hash, u32 len) × count ]` object the file read path and the rev2§4.6 GC mark walk parse), its encoder `encode_chunk_list`/`encode_chunk_ref`, the layout specs `chunk_ref_bytes`/`chunk_refs_bytes`/`chunk_list_bytes`, and `lemma_chunk_refs_push` — lifting the plain-Rust `chunk_list_entries` off `from_le_bytes`/`try_into`/range-slice onto a `Hash`-free `[u8; 32]` image (totality + framing only, no injectivity, the `decode_node` recipe, `verus.md` §8/§9), citing the shared `le_bytes::read_u32_le` reader plus cas's `read_arr32` (made `pub(crate)` for cross-module reuse alongside `push_arr32`/`push_u32_le`/`fits`/`lemma_cat`/`tlv_err`); the `Hash::from_bytes` wrap stays the thin plain-Rust delegator, so **no trusted seam is added** (tally stays 14); and the **`RecoverReconstructs` WAL-projection** — the `recover_reconstructs` predicate (the local byte/queue reading of TLA `RecoverReconstructs`: the rebuilt run is exactly the maximal seq-continuous content-valid post-head skeleton), surfaced as a live `ensures` on `recover_records` via the corollary `lemma_recover_reconstructs`, with the anti-theatre teeth control `lemma_recover_reconstructs_pins_head` pinning the head bound so an off-by-one anchor fails — see the `RecoverReconstructs` routing note above) |
| shared `le-bytes` read-direction little-endian byte machinery (the `u*_le` specs, their `lemma_u*_le_bytes` split identities, and the `read_u*_le` exec readers, in the `le-bytes` crate) | `cargo verus verify -p le-bytes` | **6 verified, 0 errors** — the three empty-bodied `by (bit_vector)` split identities `lemma_u{16,32,64}_le_bytes` (each bridging a reader's `v = b0 \| b1<<8 \| …` bit-construction form to the `u*_le` shift-extraction spec, stated once per width per `verus.md` §6) plus the three exec readers `read_u{16,32,64}_le` (explicit index/shift — Verus does not spec `from_le_bytes`/`try_into` — each `requires off+N <= buf@.len()` / `ensures` the consumed bytes equal `u*_le(v)`). The `u{16,32,64}_le` specs are `open` (cross-crate visible, so a consumer's encode-side specs cite them by full path) and non-recursive ⇒ carry no obligation (the count is the 3 readers + 3 lemmas). **Read-direction encode-shape only**: ipc's both-direction `reassemble`/`split_bytes` form is deliberately *not* here — it stays in ipc's own `le_bytes` module (see the ipc row). Consumed by cas (under the `vstd[alloc]` prelude) and loader (no-alloc); both contexts verify the same 6 obligations at `rlimit` ≤1.6% of the default ceiling, so **no `rlimit` is sized** (the freelist-style alloc-cost note is added only alongside an `rlimit`; none is needed here). No trusted seam; tally stays 14 |
| IPC header + session codecs + reactor dispatch-arithmetic core | `cargo verus verify -p ipc` | 71 verified, 0 errors (includes the **`Admission`** bulk-window quota accounting core (rev2§3.5) — `well_formed` (granted ≤ budget), the non-underflowing observable `spec_remaining` (= budget − granted), and `new`/`remaining`/`admit`/`release` each carrying `requires self.well_formed()` / `ensures final(self).well_formed()` with the exact `spec_remaining` delta, so the unbounded never-over-grant accounting holds for *all* admit/release sequences by modular composition — the §14 `verus.md` verified-accounting template the reactor `used`-mask dispatch reuses; version negotiation in the connect layer — the `ConnectReq`/`GrantReply` codecs carrying an offered version range and the selected version, the pure `negotiate` highest-common-version selection, the `version_ok` per-message check, and the `VersionRange`/`ConnectReq` constructors, with the four codec bijection lemmas proven over those bytes by the `bit_vector` pattern; the header and session codecs cite the four named width lemmas (`lemma_u{16,32}_le_{reassemble,split_bytes}`) for the little-endian split/reassemble facts — stated once per width rather than as inline `by (bit_vector)` asserts at each field; these live in ipc's own `le_bytes` **module** and are the **both-direction** codec-bijection form (reassemble *and* split — the header/session encode↔decode round-trips need both), deliberately distinct from, and not migrated to, the shared read-direction `le-bytes` crate (cas/loader's gate) whose scope guard excludes them; and the reactor's pure **dispatch arithmetic** over the named `wf` invariant (`slots.len()==64` and the `coherent` slot/used bijection) — `lowest_clear_bit` (lowest-clear-bit correctness, no-double-allocation, the 64-bit structural bound) and `alloc_lowest` which records that allocation (sets exactly the lowest clear bit, `None` iff the word is full); **both** registration paths proven to preserve `wf` — the channel `register` path (`register_into` — pick the lowest clear bit, fill exactly that slot, `Full` (state unchanged) iff the word is full) and the `register_bound` mask path (`register_bound_into` over the `coherent` slot/used bijection — a slot is registered iff its `used` bit is set; `Taken` leaves state unchanged; an accepted mask sets exactly its bits, the set bits scanned low-to-high under the `lemma_pop_lowest` clear-lowest identity `bits & (bits-1) == bits & !(1<<tz)`); and `drain_one` (the `pending` lowest-set-bit drain step) — the deductive all-inputs twins of the three reactor proptests, all pure `u64`/array reasoning over `vstd`'s `axiom_u64_trailing_zeros` + `bit_vector`, the kcore ready-queue-bitmap pattern; **no trusted seam here**, tally stays 14) |
| shared `FreeList` (free-list allocator core + `is_full`/`is_allocated` guard accessors, in the `freelist` crate) | `cargo verus verify -p freelist` | 30 verified, 0 errors (the no_std/no-alloc gate. The count includes the `two_allocs_disjoint_teeth` driver, which performs two real carves and threads `alloc`'s coverage `ensures` into `lemma_two_allocs_disjoint`, discharging that disjointness lemma's premises from code rather than a comment. The two heavy `spinoff_prover` merge proofs carry `rlimit(120)` (`free_insert`) / `rlimit(40)` (`free_both`) — sized so they *also* verify when `freelist` is re-verified as a transitive dep of an `alloc` crate. `virtio-blk` links `cas` (the no_std `blockdev` adapter `storaged` uses), turning on `vstd`'s `alloc` feature, whose larger prelude raises those two proofs' resource cost ~1.4–1.85× when the same `freelist` source is re-verified under it. The no-alloc consumption is byte-identical before/after — `rlimit` is a solver ceiling, not a cost — so this gate's totals are unchanged) |
| DMA-pool wrapper (plain-Rust PA seam; discharges `FreeList`'s preconditions via the `freelist` guards) | `cargo verus verify -p dma-pool` | 0 verified, 0 errors (the 30 obligations live in `freelist`) |
| urt slots + time + heap | `cargo verus verify -p urt` | **25 verified, 0 errors** — urt's *own* surface (slot bitmap + `utc_ns_at`). The heap allocator's *algorithm* is the `freelist` dep it re-checks transitively (**30/0**); the heap *wrapper* is a plain-Rust arena seam (`UnsafeCell<[u8; N]>` + `base.add(off)`), **0 obligations**, kept honest by the Miri+proptest tier (`cargo +nightly miri test -p urt`). Disclosed MVP bounds in that wrapper (test-routed, not Verus-mechanized): `HEAP_RANGES = 1024` fragmentation cap, `MAX_ALIGN = 64`, `dealloc`-at-cap → safe leak (never aborts a free) — see `urt/src/lib.rs` module doc. |
| virtio-blk avail-ring index + LBA-bound arithmetic | `cargo verus verify -p virtio-blk` | **3 verified, 0 errors** — (1) `avail_ring_slot`: the avail-ring slot byte-offset is exactly `4 + 2*(idx % qsize)`, with `idx % qsize < qsize`, `4 <= slot`, and `slot + 2 <= 6 + 2*qsize` so the slot's two bytes always land inside the `6 + 2*qsize` avail buffer `new()` allocates, ∀ `u16` idx and qsize `1..=8`; no `usize` overflow by construction. `qsize > 0` is the caller's trusted MMIO bring-up precondition (`new()` derives it as `max.min(8) as u16` after rejecting a zero `QUEUE_NUM_MAX`), so the `submit` call site stays external. (2) `capacity_check` + `SECTOR`: the defensive LBA bound (`check_capacity`, rev2§4.5) is the free `capacity_check(lba, len, capacity)`, whose `ensures` `r is Err <==> lba + len/SECTOR > capacity` proves `OutOfRange` *exactly* when the last sector exceeds `capacity` (or its `lba + len/SECTOR` sum overflows `u64`, which already exceeds the `u64` `capacity`) — overflow-safe ∀ `(lba, len, capacity)` via one `checked_add` (a `vstd` `Option` library spec, not a project seam), so a near-`u64::MAX` `lba` refuses rather than wrapping into a valid-looking range; `SECTOR` (= 512) is moved into the `verus!{}` block so the prover sees its literal (the totality of `len / SECTOR`). The generic driver, the MMIO `unsafe`, the host fake device, and the no_std cas `blockdev` adapter (`storaged`'s) are all external; the device-shared virtqueue is the trusted DMA/hardware seam (rev2§2.5), and `capacity` itself is a trusted MMIO read — the verified property is the no-wrap *refusal*, not the device's honesty about its geometry. The ring proptests (`avail_ring_slot_in_bounds`, `avail_index_wraps_consistently`) and the LBA companion tier (`capacity_check_matches_oracle`/`capacity_check_high_lba_refuses` proptests, the `capacity_check_boundaries_have_teeth` unit test, and the `lba_past_capacity_refused_locally` integration test) are the kept oracle tier. **No trusted seam here**, tally stays 14: pure `u16`/`u64` modulo + `checked_add` reasoning citing no fabricated axiom. Because virtio-blk links cas, this session pulls `vstd`'s `alloc` feature and re-verifies its gated deps under it — cas (79), freelist (30, see its row), dma-pool (0). |
| storage-server rights lattice (`attenuate` + the rights bits, rev2§2.3) + wire header/version decode prefix (`check_header`, rev2§3.7) | `cargo verus verify -p storage-server --no-default-features --lib` | **19 verified, 0 errors** — covering, in the **rights lattice** (rev2§2.3): the seven `pub const` rights bits, the `has_right` spec reading of the dispatch guards (`bits & R != 0`), `attenuate`'s exec contract, and the two `lemma_attenuate_*` proofs. `attenuate(parent, mask)` is `parent & mask`, mechanized ∀ `u8`: the result equals `parent & mask`, sets no bit absent from `parent` (`r & !parent == 0` — monotone, delegation never grows authority), and clears `R_STAT_STORE` whenever the mask omits bit 5. `lemma_attenuate_monotone` restates monotonicity in the `has_right` reading (an attenuated handle holds a right only if its parent did); `lemma_attenuate_r_all_denies_stat_store` proves the deny-by-default corollary — masking by `R_ALL` (bits 0..=4, which omits bit 5) always clears `R_STAT_STORE`, ∀ parent. And in the **wire-decode header+version prefix** (rev2§3.7, Task 8): the header consts `PROTO_MAGIC`/`PROTO_VERSION`/`MAX_MSG`, the ghost model `spec_check_header`, and the exec `check_header`, mechanized total ∀ `(buf, negotiated)` — `check_header == spec_check_header(buf@, negotiated)`, so it never panics / reads OOB and refuses `BadHeader` exactly on a sub-3-byte buffer or wrong magic, `Version` exactly on a good magic whose stamped version byte is not `negotiated` (composing on the already-verified `ipc::version_ok`, whose `ensures ok == (h == n)` carries the equivalence), else returns the body offset `3`; the magic check structurally precedes the version check (a reordered decoder would disagree with the spec and fail to verify). The session/handle dispatch stays external plain Rust. The **postcard body decode** that follows the prefix stays the trusted interpreted seam, **trust-routed by feature-exclusion, not `external_body`**: `postcard` is an optional serde-gated dependency dropped under the `--no-default-features` verify config (mirroring cas), so the body codec is outside verified compilation entirely — there is nothing to mark `external_body`, and forcing one in would re-enable serde for verify and pull the whole session/handle/postcard dispatch into scope (the opposite of Task 4's island). The `roundtrip_and_strictness`/`version_is_stamped_and_validated` host tests (truncated-body / trailing-bytes / wrong-magic-wins teeth) plus the new always-compiled `check_header_cases` / `magic_strictly_precedes_version_has_teeth` lib tests guard the prefix and the postcard boundary. Like cas the feature-agnostic core verifies in the no_std+alloc variant (`--no-default-features`), and `--lib` skips the placeholder `main.rs` bin (no proofs — storage-server is the first gated crate with a separate bin). The `rights_lattice`/`sessions` proptests + the dispatch fuzz corpora are the kept companion oracle tier. **No trusted seam here**, tally stays 14: pure `u8` bit-mask + slice-prefix reasoning (`by (bit_vector)` / `group_slice_axioms`) over no vstd axioms; the postcard body adds no row because feature-exclusion already routes it out of verify. Because storage-server links cas + ipc, a cold session re-verifies its gated deps under the alloc prelude (cas 79, ipc 71); their `rlimit` totals are byte-identical to their standalone gates. |
| loader ELF page geometry + decoder (`Segment::page_layout`, `parse`, rev2§5/§5.3) | `cargo verus verify -p loader --no-default-features` | **12 verified, 0 errors** — covering `PAGE`/`PAGE_MASK`/`MAX_SEGMENTS`, the `Segment`/`PageLayout`/`ElfError`/`Image` types, the page-geometry lemmas (`lemma_align_down`, `lemma_pages_exact`) and `page_layout`'s exec contract, **plus** the `parse` decoder (the little-endian field readers it calls now come from the shared `le-bytes` crate — see that Baseline row). `page_layout` is mechanized total ∀ `(vaddr, memsz)`: it returns `Err(BadSegment)` *exactly* when the page-up rounding `vaddr + memsz + (PAGE-1)` overflows `u64` (the refuse-not-crash boundary, rev2§5.3), and on `Ok` the geometry is page-aligned at both ends (`va_start & (PAGE-1) == 0`, `va_end & (PAGE-1) == 0`), encloses `vaddr` (`va_start <= vaddr`, and `vaddr < va_end` when `memsz > 0`), the in-page offset is in `[0, PAGE)` (`page_offset == vaddr - va_start`), and the page count is exact (`pages * PAGE == va_end - va_start`). `lemma_align_down` is one symbolic `by (bit_vector)` over an arbitrary mask (the align-down/partition facts hold for every mask); `lemma_pages_exact` routes through the modular world (vstd `low_bits_mask_is_mod` + `sub_mod_noop` + `fundamental_div_mod`) so no subtraction enters `by (bit_vector)` (where only a contiguous low-bit mask would survive it). `parse` is a **total bounded decoder** ∀ `&[u8]` (never panics, never reads OOB, rev2§5.3): it reads each fixed-width field through the shared crate's `le_bytes::read_u{16,32,64}_le` readers (cited by full path), each carrying `requires off+N <= buf@.len()` / `ensures buf@.subrange(off,off+N) == le_bytes::u*_le(v)` (the consumed bytes are exactly the value's little-endian split, by `le-bytes`'s `lemma_u{16,32,64}_le_bytes` `by (bit_vector)` identities — the cas node-decoder reader pattern), and `parse` bounds the whole phentsize-strided program-header entry up front (`ph + phentsize <= len`, `phentsize >= 56`) so every field read is in range; on `Ok` every returned `Image` satisfies `well_formed_image` (`1 <= nsegments <= MAX_SEGMENTS`, and a `decreases` loop maintains the `forall j: seg_ok(segments@[j], bytes@.len())` invariant — each accepted segment's file extent in bounds and `page_layout` overflow-free, composed via `seg_ok` on the Task-5 `page_layout` ensures). The startup byte codec and the target-only `spawn` stay external plain Rust; the `parse`/`page_layout_*` unit tests, the `layout_props` proptest, and the `elf_parse` fuzz target + corpus + Miri replay (`tests/fuzz_corpus.rs`, `tests/fuzz_regressions.rs`) are the kept companion oracle tier. **No trusted seam here**, tally stays 14: pure `u64`/`usize` checked arithmetic + bit/shift reassembly citing no fabricated axiom. Because loader links `ipc` and `le-bytes`, a cold session re-verifies those gated deps (ipc 71, le-bytes 6 under the no-alloc prelude), `rlimit` byte-identical to their standalone gates. |
| TLA+ | `CommitProtocol` (6886 states; the `RecoverReconstructs` replay-equality action property + the committed negative control `CommitProtocol_NegControl.cfg`, which reports the expected violation), `CapRevocation` (stepwise revoke — `RevokeBegin`/`RevokeStep`/`RevokeEnd` over a `revoking` marker, `Copy` derive-guard; 503,070 distinct states with the safety invariants checked at every mid-revoke interleaved state + `EventuallyRevoked` liveness under weak fairness; two committed negative controls — `CapRevocation_NegControl.cfg` reports the `LiveParent` violation under a non-leaf delete, `CapRevocation_NegLiveness.cfg` the `EventuallyRevoked` livelock when the guard is dropped; constants trimmed to Threads 1 / QueueDepth 1 because at the full-scale constants the `EventuallyRevoked` liveness tableau exceeds the default 4 GB heap), `CapRevocation_Teardown` (TSpec, 252 states), `IpcReactor` (the reactor protocol — `Register` + the poll-once self-signal, the symmetric writable/backpressure half, and the 3-state receiver that blocks on the notification *word*, not the queue; the `NoLostWakeupWritable` safety invariant alongside `TypeOK`/`NoLostWakeup`/`NoDrop`/`FifoPerChannel` + `EventuallyDelivered` liveness under weak fairness; **39 distinct states** (59 generated, depth 13) at MaxMsgs 3 / QueueDepth 2; **three committed negative controls** — `IpcReactor_NegControl.cfg` reports `NoLostWakeup` violated when `Register` drops the poll-once self-signal (the send-before-bind hazard), `IpcReactor_NegBackpressure.cfg` reports `NoLostWakeupWritable` violated when `RecvGet` drops the on-writable fire, `IpcReactor_NegLostWakeup.cfg` reports `NoLostWakeup` violated when `RecvBlock` drops the `word = 0` guard; the `CHECK_DEADLOCK FALSE` ↔ `EventuallyDelivered` dependency pinned as a cfg comment. **Single-source by design** — the multi-source dispatch *arithmetic* (the `used`-mask allocation, both registration paths' slot/used coherence, the `pending` drain) is now Verus-mechanized (Baselines `-p ipc` row), while the cap-marshalling is proptest-routed and the live concurrent wakeup/backpressure execution is Loom/Shuttle-routed (`ipc/src/model.rs`); none of it is TLA-mechanized — see the IPC dispatch routing note above) | pass |
| Fuzzing | wire/on-disk/ELF decoders + mount/recovery cargo-fuzz targets + the GC mark-walk target (`gc_mark`), committed corpora + Miri replay | green |

---

*This ledger is the enumerated source of record; the intermediate technique findings and
the Verus-rewrite plan they distilled are not retained in-tree (see
`doc/guidelines/verus.md`).*
