# Eunomia OS — Development Guide

Full design specification: `doc/spec/0_spec_mvp.md`. Read the spec before
touching any component. Section numbers below refer to that document.

---

## Workspace layout

```
kernel/          AArch64 bare-metal microkernel (aarch64-unknown-none) —
                 the architectural shell over kcore (boot, MMU, GIC, sched)
kcore/           Host-buildable kernel object core: cspace/CDT, untyped,
                 channels, notifications, thread/timer objects, aspace data;
                 Verus-verified (§6, doc/plans/3_verus-rewrite.md). no_std,
                 zero deps; the kernel links it, hardware + objects behind the
                 handle/Store seam
ipc/             Async IPC crate — shared by all userspace servers (§3.5)
dma-pool/        DMA buffer pool — the only place PAs are visible (§2.5)
cas/             CAS primitives: chunker, prolly tree, commit protocol (§4)
storage-server/  Userspace storage server process (§4)
virtio-blk/      Virtio-blk driver, written against dma-pool (§2.5)
loader/          ELF loader / program spawner (§5)
user/            Real userspace binaries (init, shell, storaged, …) — own
                 mini-workspaces, built by kernel/build.rs (§5, §7)
mkfs/            Host-side disk image builder; reuses cas crate (§7)
tla/             TLA+ formal specifications (must check before M2)
tools/tla/       Scripts: tla-check.sh (SANY), tla-model-check.sh (TLC)
doc/spec/        Design documents
doc/results/     Implementation and research results.
doc/guidelines/  Additional guidelines
```

---

## Build commands

### Kernel (cross-compiled for AArch64 bare-metal)

```sh
# Build (target aarch64-unknown-none-softfloat and build-std set by
# kernel/.cargo/config.toml; softfloat because trap frames don't save SIMD)
cd kernel && cargo build

# Release build
cd kernel && cargo build --release

# Run in QEMU (uses the runner in kernel/.cargo/config.toml)
cd kernel && cargo run

# Run manually / with GDB stub (attach with gdb-multiarch on :1234).
# gic-version=3 is required (gic.rs drives GICv3 redistributor + ICC_*).
qemu-system-aarch64 -machine virt,gic-version=3 -cpu cortex-a72 -m 256M \
  -nographic -serial mon:stdio \
  -kernel target/aarch64-unknown-none-softfloat/debug/kernel \
  -s -S
```
Note: the cargo target directory is at the workspace root (`target/`), not
under `kernel/`.

### Host crates (storage-server, cas, mkfs, etc.)

```sh
# Build all host crates from workspace root
cargo build -p cas -p storage-server -p mkfs ...

# Run tests for cas (primary proptest target)
cargo test -p cas

# Run with Miri. The proptest suites drop to 4 cases under cfg(miri) —
# blake3 is interpreted (no SIMD), so native-scale case counts would take
# hours; even reduced, this sweep runs ~25 min. Quickest useful UB pass
# (regression tests + every committed fuzz seed, ~30 s for all 3 crates):
#   MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
#     -p cas -p loader -p storage-server \
#     --test fuzz_regressions --test fuzz_corpus
cargo +nightly miri test -p cas
```

### Kani — retired (phase 7f)

**Kani has been retired from the project.** It served as the interim bounded
mechanized tier; every target it covered is now proven *unbounded* in Verus.
The kernel object core migrated in phase 2 (deleting `kcore/src/proofs` and the
off-CI deep-Kani machinery — `scripts/deep-verify.sh`, `kani-deep.yml`, the
`kani_deep`/`kani_contracts` features — it subsumed). The §4.7 host chokepoints
ported per crate across phase 7: `ipc` (7a/7b), `urt` (7c/7d), `dma-pool` (7e),
and `cas` (7f, `doc/results/62`) — each deleting its `proofs.rs` in the same PR.
With `cas` (the last holdout — it ported cleanly, no `Vec`), the **`kani` CI job,
the pinned-`cargo-kani-0.67.0` install dance, and the `#[cfg(kani)]` scaffolding
are all gone**; `cargo kani` is no longer run anywhere. The historical findings
stay recorded across `doc/results/2_kani-findings.md` … `8_kani-findings-7.md`.
(Closeout note: the spec `§6` tier table and the `0_kani-rewrite.md` banner are
the only doc residue, left for phase 7g/9.)

```sh
cargo test -p kcore                                  # kcore host unit tests
```

### Verus (`kcore` + host chokepoints + scratchpad)

**Pinned at `0.2026.06.07.cd03505`** — installed at `/Users/mjm/inst/verus/`;
`vstd` companion pinned at `=0.0.0-2026-05-31-0205` (in `kcore`, `ipc`, `urt`,
`dma-pool`, `cas`, and `scratchpad` `Cargo.toml`). Verus is unstable software:
both the binary and the
`vstd` version must be upgraded together and any upgrade is a deliberate PR.
Code in `verus!{}` blocks erases to plain Rust under a normal `cargo build`/`test`
(the macro drops ghost code), so the aarch64 kernel build and the host crates are
unaffected — confirmed by the kernel cross-build and `cargo test`.

Verus is the **mechanized implementation tier for `kcore`** (plan
`doc/plans/3_verus-rewrite.md`): **unbounded**, functional proofs on the real
handle/`Store` code — the Store seam carries an abstract ghost view so the
generic `fn op<S: Store>` operations are verified once for all stores. Proven
(unbounded, no assumptions): `untyped::carve`/`carve_place` (totality + placement
geometry, phase 0); the **non-recursive** cspace/CDT ops `derive` (monotone
derivation — rights ⊆ source ∀ masks; faithful copy; overflow-free refcount
bump), `cdt_insert_child` (structural splice), and `obj_ref` — these now preserve
the **full `cspace_wf`** (strengthened `cdt_wf` + **parent- and sibling-
acyclicity composition**, the construction-side witnesses; phase 2c,
`doc/results/22_verus-findings.md`); `revoke`/`descend_to_leaf` **termination**
(phase 2b); and **both looping ops `slot_move` and `cdt_unlink`** in full — body
proofs closed. `slot_move` (`doc/results/24_verus-findings.md`): the move is the
identity transposition π=(src dst), and the imperative neighbour-fixups land
exactly the renaming `relabeled(m0, src, dst)` (`lemma_transpose_preserves_cspace_wf`
keeps `cspace_wf`; `lemma_child_on_chain` + a `next_reach`-split loop invariant make
the children-walk re-parent every child), so its `external_body` is **gone**.
`cdt_unlink` (`doc/results/25_verus-findings.md`): the sibling-list *merge* — harder
than the transposition — lands exactly the closed form `unlinked(m0, slot, last)`;
its `cspace_wf` is preserved with the **parent-rank acyclicity witness reused
unchanged** (children move up, the gap shrinks) but the **sibling-rank witness
rescaled** (`lemma_unlink_sib`: a multiplicative band drops the re-parented child
chain into the `prev..next` gap — a constant additive shift provably can't), so its
`external_body` is **gone** too. The strengthened `cdt_wf` adds the reachability
anchors (`siblings_share_parent`/`parent_has_first_child`) that make acyclicity
constructible.

**Phase 3 (untyped remainder + channel, `doc/results/26`…`30`):** the untyped ops
`retype_check`/`retype_install`/`reset` — with the §2.5 rights-inheritance table as
theorems (Frame inherits the untyped's rights; Thread → `THREAD_ALL`; **sub-`Untyped`
masked to `READ|WRITE`, provably never `PHYS`**) — and the channel ops `send`/`recv`
(the §4.3 FIFO `Seq` model: payload length + cap identity + order, caps moved via the
verified `slot_move`, two-pass `recv` atomicity, revocation null-slot tolerance),
`endpoint_cap_added`/`endpoint_cap_dropped` (peer-closed `end_caps` accounting + the
**conditional `refs_view` frame** — held except on the zero-drop that fires
peer-closed), `bind` (the §3.6 binding-refcount delta `bind_refs_post` — the first
`refcount_sound` installment), and `fire` are all proven against `cspace_wf` + the new
`chan_wf`.

**Phase 4 (notification + thread/reports + timer, `doc/results/31`…`35`):** the
notification ops `signal`/`wait`/`remove_waiter`/`destroy_notif` against the FIFO
`waiter_seq` model (so **wake order = block order** is a theorem — `signal` graduates from
its phase-3 `external_body` to a **proven** body); the thread ops `report_terminal` (the
§5.1 **ReportMonotone** — at most one `Running → Exited|Faulted`, terminal absorbing — and
**FireSafe** — a terminal fire reads an empty slot or a live notification, never freed
memory) and `bind`; and the timer ops `arm`/`disarm`/`check_expired`/`destroy_timer`
against the head-only armed-list `timer_wf` (`check_expired`'s multi-fire census tension
resolved by a distinct-notification precondition, `doc/results/35`). The **waiter** and
**armed-timer** refcount deltas are the second/third `refcount_sound` installments after
3e's binding term; the full census stays deferred (the recommended cross-object-teardown
phase after phase 5).

**Phase 5 (aspace §4.5 + sysabi §4.6, `doc/results/36`…`40`):** the sysabi ops
`decode`/`decode_prio` + `untyped::ObjType::from_u64` (total decode — "unknown `nr` is an
error, never a crash"; per-arm length/event/which/prio validation as `ensures`); and the
aspace **page-table walker** against a new `pt_wf` tree-shape model (`pt_lookup`/
`pt_leaf_slot`, the leaf/inner **level partition**, the no-aliasing "page table is a tree,
not a DAG" theorem): `pte_encode` (the §2.5/§4.5 **isolation theorem** ∀ `perms` —
device-never-executable / the AS-1 fix; AP grants EL0-write iff `PERM_W`; PXN always set),
`pte_output_pa` (round-trip), `va_range_ok` (+ user-L1-never-touches-kernel), `range_mapped_in`
(full functional equivalence to the ghost containment + writability, ∀ `(va,len)`),
`map_in` (the two-pass walk-allocate — adds exactly the requested pages or fails atomically;
`AlreadyMapped`/`NeedMemory`; the no-clobber frame), and `unmap_in` (range unmapped; the
outside-range frame; **one TLBI per present-chain page, in ascending order** — the §4.5
effect-ordering theorem, via a seventh `Store`-seam ghost view `tlb_log_view` whose only
mutator is `tlb_invalidate_page`). The first kcore Verus reasoning over **concrete Rust
slices** (`&mut [[u64; 512]]`) and the first **hardware effect-log** on the seam. Phase 5
adds **no `external_body`** — it is the first phase since phase 2 to add zero trusted
residue (the §7-step-5 "delete `proofs/{aspace,sysabi}.rs`" and "`cargo kani -p kcore` is
gone" clauses were already discharged in phase 2). The **frame-mapping `refcount_sound`
term** the aspace mappings contribute passes to the now-unblocked cross-object-teardown
phase.

**Phase 6 (cross-object teardown + the `refcount_sound` census, `doc/results/41`…`56`) — done.**
The teardown cluster — `delete`, `obj_unref`, `destroy_cspace`, `unref_cspace`, `unref_aspace`,
`channel::destroy_channel`, `thread::destroy_tcb` — is **proven, every `external_body` removed**,
closing the cross-module mutual recursion `delete → obj_unref → destroy_{cspace,channel,tcb} →
delete` under the seL4-zombie lexicographic measure `(count_nonempty(slot_view), height)` (6a–6d,
docs 41–54). `revoke` root-survival is the **conditional non-zombie theorem** (6e, doc 55 — the
long-deferred doc 23 §4 gap, under an explicit `!is_homed(slot)` precondition). The **full
`refcount_sound` census** (`refs[o] == slot_refs + binding_refs + waiter_refs + armed_timer_refs
+ frame_map_refs + thread_hold_refs`) is a precondition+postcondition of the whole teardown family,
assembling the binding/waiter/armed-timer/frame-mapping terms phases 3/4/5 landed as deltas. **As a
*system* invariant (6f, doc 56)** it is additionally preserved — in the conditional
`refcount_sound(old) ==> refcount_sound(final)` form, which is purely additive (no caller churn) —
by the ref-touching construction ops `derive` (slot term, via the new `lemma_set_slot_obj_census`),
`channel::bind` / `endpoint_cap_added` (binding term, via `lemma_binding_replace`), and `signal` /
`remove_waiter` / `endpoint_cap_dropped` (the ops already carrying `census_delta_frozen`, bridged by
`lemma_refcount_sound_from_frozen`). The remaining construction ops — notification `wait`, timer
`arm`/`disarm`, channel `send`/`recv`, `thread::bind`, and `untyped::retype_install` — keep their
landed **per-op refs/census delta**, with the system-clause wiring a **recorded follow-on** (the
obstructions are real, not mechanical: `slot_move`'s permutation-neutrality for the cap-move ops,
the thread-on-one-chain invariant for `wait`/the waiter frame, the loop-threaded armed-timer recount
for `arm`/`disarm`, and the *creation off-by-one* for `retype_install`, whose freshly-`init`'d
object is deliberately off-by-one at entry, so the simple implication is false for it). **kcore's
object operations now carry zero `external_body` and zero plain-Rust** — the rewrite's `kcore` goal
(plan §1.2) is met. The **trusted base** is now exactly the `Store` hardware/scheduler seam
(`make_runnable`, `aspace_unmap`/`aspace_destroy`, the TLB hooks) — assumed `ExStore` contracts,
host-checked in `test_store.rs`. (Phase 2 closeout, doc 23, **retracted doc 21 §9's** proposed
revoke-cap-survival fix as unsound — cross-object teardown can empty revoke's own root in the
seL4-zombie case; two `test_store` cases witness it, now `is_homed`'s negative witness.) With the
host chokepoints (§4.7, **phase 7 — done**, 7a–7g), only the commit-protocol recovery core (§4.8,
phase 8) and the spec/`CLAUDE.md`/Kani closeout (phase 9) remain.

**Phase 7 (host chokepoints §4.7, `doc/plans/3_verus-rewrite_phase7-detail.md`) — done (7a–7g).** It
migrates the four host-side §4.7 chokepoint crates (`ipc`, `urt`, `dma-pool`, `cas`) from Kani
(bounded) to Verus (unbounded), one sub-phase/PR per target, deleting the subsumed Kani harness in
the same PR (§5: a property is never unguarded between tiers). **7a (pilot, `doc/results/57`):** the
§3.7 fixed message header `ipc::header` — `decode` totality + accept-iff-`HEADER_SIZE`-length and the
`encode`∘`decode` bijection, ∀ (`lemma_decode_encode`/`lemma_encode_decode`, `by (bit_vector)` over
explicit mask/shift arithmetic — the `to_le_bytes`/`copy_from_slice` form is unspecced by Verus and
`vstd`'s `to_le_bytes` exec wrappers are alloc/`Vec`-only, so the codec is rewritten to keep `vstd`
ghost-only). The real trophy is the **toolchain proof**: `vstd` now rides into the five userspace
binaries (`ipc` is their path-dep) and erases cleanly under the aarch64 cross-build. The two
`check_header_*` Kani harnesses are deleted; the `ipc` session codecs stay on Kani until 7b. The
`verus` CI job runs `cargo verus verify -p {kcore, ipc}` with no per-proof filter, so a new
`verus!{}` obligation auto-gates. `scratchpad` keeps the toolchain-smoke `spec fn min` example.
**7b (`doc/results/58`):** the §4.6 session layer `ipc::session` — the `ConnectReq`/`GrantReply`
codecs as total bijections (same mask/shift + `by (bit_vector)` recipe) and, the real prize,
`Admission` **never over-grants ∀ admit/release sequences** — `granted <= budget` (`well_formed`) is a
modular pre/post-condition of every op, so `remaining()`'s `budget - granted` is non-underflowing for
*all* sequences, vs Kani's hard 3-step unwind. The private `budget`/`granted` are kept off the public
contracts via `closed` accessors (`well_formed`, `spec_remaining`); the `&mut` postconditions use the
new-mut-ref `final(self)` form. `ipc/src/proofs.rs` is **deleted** (the last 5 session harnesses), so
`ipc` is fully off Kani — `-p ipc` drops from the `kani` job. **7c (`doc/results/59`):** the
cspace-slot bitmap free-list `urt::slots` — proven ∀ `cap` and `WORDS` (vs Kani's CAP=4): `alloc`
hands out an in-window slot that was free and is now used (so distinctness is a *corollary* of the
modular contract, not a bounded drain loop); exhaustion is exact; `free` makes a slot free again with
the **double-free `!is_free_spec` precondition** a contract-checked impossibility; `alloc_range` hands
out a contiguous in-window run. The first 7-series port that is not straight-line byte arithmetic: the
`.find().map()` combinators are restructured into invariant-carrying loops, and the packed bitmap
`free[i/64] & (1<<(i%64))` is bridged to a per-slot `is_free_spec` by three `by (bit_vector)` frame
lemmas. The model (`wf`/`is_free_spec`/`spec_base`/`spec_cap`) is `closed` (the 7b opaque-field rule).
`debug_assert!` is forbidden inside `verus!{}` (it lowers to `panic!`), so the runtime double-free
guard the host `double_free_panics` test exercises moves to one `#[verifier::external_body]` helper —
the module's only trusted residue; the static guarantee is `free`'s precondition.
**7d (`doc/results/60`):** the tick→ns wall-clock conversion `urt::time::Sample::utc_ns_at` — **trophy
#1**, the property Kani's own harness recorded as intractable. Its `ensures r == result_spec(cntvct)`
proves **totality** (no panic/overflow ∀ page contents + counter — what Kani's `check_time_conversion_total`
gave bounded) *and* the functional value, and `lemma_utc_ns_at_monotone` proves **monotonicity**
(`c1 ≤ c2 ⇒ utc_ns_at(c1) ≤ utc_ns_at(c2)`) — relating two u128 divisions, the step CBMC could not take
(`doc/results/8` SOLVER note) and proptest only sampled. The crux is the decomposition
`secs·10⁹ + frac_ns == (delta·10⁹)/f`, three lines once `lemma_hoist_over_denominator` (`vstd::arithmetic::div_mod`)
is found — `lemma_fundamental_div_mod` splits `delta`, one `nonlinear_arith` rearranges, hoist discharges
it; monotonicity is then `lemma_mul_inequality` + `lemma_div_is_ordered` over the spec closed form.
`.max(1)`/`.saturating_sub(..)` are restructured into explicit branches (unspecced std combinators, the
7a/7c precedent); the model is `closed` because the spec bodies name the private `NANOS_PER_SEC` (a
`pub open` body may name only public items). `urt/src/proofs.rs` is **deleted** (`check_time_conversion_total`
was the last harness), so `urt` is **fully off Kani** — `-p urt` drops from the `kani` job; only `dma-pool`
(7e) + `cas` (7f) remained there.

**Phase 7e (`doc/results/61`):** the first-fit DMA free-list allocator `dma-pool` — **trophy #2**, the
**DN-10** two-buffer disjointness + alignment round-up Kani could only check at one concrete pair
(symbolic, it OOM'd CaDiCaL). The free-list arithmetic is extracted into a self-contained `FreeList<const N>`
verified inside `verus!{}` (`new`/`alloc`/`free` against a sorted-disjoint-extent `wf` + a `covers` free-set
model); the `DmaPool<B: DmaBacking>` wrapper that touches the trusted PA seam (raw-pointer slices, device
addresses) stays plain Rust — the honest split, since dma-pool *is* "the single place PAs are visible".
`alloc` is proven ∀ size/alignment (in-pool, aligned, the carved region free→used, coverage elsewhere
framed), so **two live buffers are disjoint ∀** is the corollary `lemma_two_allocs_disjoint`; `free`'s
two-sided merge restores coverage. Two exec restructures dodge the named risks: `copy_within` (no Verus
model) → the verified shift helpers `remove_at`/`insert_at` (the `cdt_unlink`/`slot_move` array-splice
reasoning), and the bit-mask round-up `(off+align-1)&!(align-1)` (the OOM term) → the modular
`off + (align-off%align)%align`, so `start%align==0` is pure `vstd::arithmetic` — **no `by (bit_vector)`**.
The heavy splice/merge frame proofs are decomposed into `spinoff_prover` halves (doc 25 §2). `dma-pool/src/proofs.rs`
is **deleted** (its two harnesses were the last), so `dma-pool` is **fully off Kani** — `-p dma-pool` drops from
the `kani` job, leaving only `cas` (7f).

**Phase 7f (`doc/results/62`):** the on-disk superblock chokepoint `cas::disk` — the **last** §4.7 target,
from Kani (symbolic head bytes, `Hash::of` `-Z stubbing`'d) to Verus (unbounded ∀). `validate_geometry_fields`
proves totality + the region-within-device safety invariant (`(r is Ok) <==> geometry_ok`, **overflow-exact**:
the `checked_add` rejections coincide with the `int`-stated clauses wrapping past `u64::MAX >= dev_len`);
`decode_checked_fields` proves **decode totality ∀** buffer bytes (verifying it *is* the theorem — every
fixed-offset read in bounds, every shift/`|` non-overflowing). The 7e split applies to a decoder: the verified
byte-parsing core returns a `Hash`-free `RawSuperblock` (so neither `Hash` nor `Superblock` enters the proof
surface — no `external_type_specification`), and `Superblock::{validate_geometry,decode_checked}` stay thin
plain-Rust delegators (the `Hash::from_bytes` assembly is trivially total). blake3 is the **assumed-total seam** —
one `#[verifier::external_body] checksum_ok` (Verus does not look inside `Hash::of`), exactly the boundary Kani
drew with `-Z stubbing`; totality needs no collision-freedom. The 7a decode recipe is reused: explicit byte
indexing + mask/shift (no `from_le_bytes`/`try_into`/slice `==`), per-byte magic compare, `broadcast use
vstd::slice::group_slice_axioms`. Two gotchas: byte-char literals (`b'E'`) are an "Unsupported constant type"
(use `0x45u8`), and a `const` declared **outside** `verus!{}` is invisible to it — `SB_SIZE`/`WAL_OFF`/`SB_VERSION`/
`CHUNK_HEADER` are moved *into* the block (they erase to the same `pub const`s, so external code is unchanged).
`cas/src/proofs.rs` is **deleted** (its two harnesses were the last in the project); `cas` ports cleanly (no
`Vec`) so it is **not a holdout** — the **entire `kani` CI job, its pinned-cargo-kani install dance, and the
`#[cfg(kani)]` scaffolding are retired**. Every §4.7 chokepoint is now on Verus.

**Phase 7g (`doc/results/63`):** the directory-entry TLV codec `cas::tlv` (§4.9) — the final §4.7 target. The
§7g plan *recommended deferring* this (no Kani harness to delete; the canonical-form oracle is already the
cargo-fuzz target `tlv_entry`), but the project's direction is to **verify unless Verus isn't the right tool**,
with fuzzing filling the gaps — so 7g proves it in Verus at **full exec-level** (the literal `encode(decode(b)) ==
b` over the real `Vec`-building codec, not a spec-only relation). Two ∀ theorems on a new verified core in
`prolly.rs`: `decode_raw` **totality** (no panic ∀ bytes; the opt-TLV `while` loop gets a `decreases`) and the
**canonical-form round-trip** — `decode_raw(b) == Ok((e,k)) ==> canonical_bytes(e) == b[..k]` (the decoder
accepts *only* canonical encodings — the opt-section loop admits at most one record) plus `encode_raw(e)` appends
exactly `canonical_bytes(e)`; composed with `tlv::decode`'s whole-buffer check, `encode(decode(b)) == b`. The 7f
`RawSuperblock` discipline is extended to a **variable-length** type: a `Hash`-free `RawEntry`/`RawContent`
(`Vec<u8>` + `[u8;32]`) so the 32 hash bytes round-trip *inside* the proof — **no Hash axiom, no
`external_type_specification`** — and `encode_entry`/`decode_entry` become thin plain-Rust `Entry ↔ RawEntry`
converters. Entry-level `validate_entry` **stays plain Rust** (it only shrinks the accept set, so it does not bear
on the round-trip — the split that kept the proof tractable). Full exec-level needs `Vec` specs, so `cas`'s vstd
gains the **`alloc`** feature (the chief phase-7 risk — vstd in the userspace cross-build, re-cleared the 7a way);
vstd's `extend_from_slice` (a `cloned` predicate) is replaced by verified push-loop helpers (the 7e move). Gotchas:
the external `FormatError` can't be *constructed* inside `verus!{}` (an `external_type_specification` makes it
opaque — "constructor for an opaque datatype" disallowed), so an in-block `TlvErr` maps 1:1 to it; `content_bytes`
**includes** the content-tag byte (an off-by-one); spec `invariant`s overflow-check usize `+` (restate as
`int`); and a fresh `let end = off + n` needs the bound tied to the exec `buf.len()` (a usize). The fuzz oracle
`tlv_entry` is **kept** as the differential/regression guard of the composed `Entry`-path round-trip. **No Kani
change** (Kani was retired in 7f). With 7g every §4.7 chokepoint is on Verus.

```sh
cargo verus verify -p kcore                     # the kcore proofs (CI-gated)
cargo verus verify -p ipc                       # §4.7 host chokepoints — phase 7a ipc::header + 7b ipc::session (CI-gated)
cargo verus verify -p urt                       # §4.7 host chokepoints — 7c urt::slots free-list + 7d urt::time conversion (CI-gated)
cargo verus verify -p dma-pool                  # §4.7 host chokepoints — 7e dma-pool free-list allocator (CI-gated)
cargo verus verify -p cas --no-default-features  # §4.7 host chokepoints — 7f cas::disk superblock + 7g cas::tlv codec (CI-gated)
cargo verus verify -p scratchpad                # the spec fn min smoke example
```

### TLA+ specs

```sh
# Syntax check
bash tools/tla/tla-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-check.sh tla/cap_revocation/CapRevocation.tla

# Model check (run before M2 and M1 implementations respectively)
bash tools/tla/tla-model-check.sh tla/commit_protocol/CommitProtocol.tla
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla

# CapRevocation.tla carries a SECOND spec (TSpec) for §3.3 channel
# whole-object teardown; check it with its own config (fast, ~1s):
bash tools/tla/tla-model-check.sh tla/cap_revocation/CapRevocation.tla \
  CapRevocation_Teardown.cfg

# IpcReactor.tla — the §3.6 IPC lost-wakeup/backpressure spec (plan
# doc/plans/2_ipc.md §5.1). Unlike the others it carries a liveness property
# (EventuallyDelivered) under weak fairness alongside the safety invariants;
# check before the IPC reactor implementation (~1s):
bash tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla
```

### Fuzzing (cargo-fuzz, host)

Harnesses live in `cas/fuzz`, `storage-server/fuzz`, `loader/fuzz`, `ipc/fuzz`
(each a standalone workspace, excluded from the host workspace). Needs nightly +
`cargo install cargo-fuzz`. See `doc/guidelines/fuzzing.md`; findings in
`doc/results/1_fuzzing-findings.md`. `ipc/fuzz` fuzzes the §3.7 wire codec
(`wire_decode`); its corpus replay is `fuzzing`-gated, so run it with
`cargo test -p ipc --features fuzzing --test fuzz_corpus`.

```sh
scripts/fuzz.sh smoke              # replay committed corpus through every target
scripts/fuzz.sh hunt 300           # time-boxed hunt per target
cargo run -p cas --example gen_cas_corpus   # regenerate that crate's seed corpus
```

The committed corpus is replayed by `cargo test` (`--test fuzz_corpus`); run
that test under Miri with `MIRIFLAGS=-Zmiri-disable-isolation` to UB-check
every seed (the replay reads files, which Miri isolation otherwise blocks).
`cas`/`storage-server`/`ipc` gain a `fuzzing` feature (fuzz-only `fuzz_support`
helpers / `Arbitrary` derives / the `ipc` codec's `DemoMsg`); never enable it in
normal builds.

---

## Milestones and current status

| Milestone | Status | Key deliverable |
|-----------|--------|-----------------|
| **M0** | ✅ Done | Boot, UART, MMU, exception handling |
| **M1** | ✅ Done | Caps + threads + async channels; CDT revoke |
| **M2** | ✅ Done | virtio-blk; CAS + prolly tree; session protocol; mkfs |
| **M3** | ✅ Done | ELF loader; spawn-with-caps; shell |
| **M4** | ✅ Done | Snapshot / rollback demo (MVP) |
| **M5** | ✅ Done | GC + history rewriting |

Both TLA+ models (CapRevocation, CommitProtocol) are complete and
TLC-checked — the M1/M2 formal gates are cleared. The `cas` crate's
chunker + prolly tree + canonical-form proptest suite passes (incl. Miri).

### M2 progress
Done (host-side): the full storage engine in `cas` (`dev.rs` block devices
incl. crash-injection, `disk.rs` on-disk formats, `overlay.rs` memtable,
`store.rs` WAL/flush/A-B-commit/recovery — crash-injection proptest mirrors
the TLA+ AckedWritesRecoverable invariant); `mkfs` builds bootable images
from a host tree (integration-tested); `storage-server/src/lib.rs` has the
transport-agnostic session/handle/ticket layer (7 semantics tests).
dma-pool + virtio-blk are done and host-integrated (the cas engine runs
over the driver over a register-accurate fake device in tests).

### M3 progress
Done: kernel address spaces (aspace.rs, ASID-tagged TTBR0 switching,
shared kernel L1 entries), frame caps with mapping-in-the-cap (§2.5),
map/frame_write/thread_start_as syscalls; `ipc::sys` syscall wrappers;
`loader` is a lib (host-tested ELF64 parser + spawn). Real userspace
binaries live under `user/` (own mini-workspaces, built by
kernel/build.rs into `target/user`, embedded with include_bytes!). The
default boot loads init as a real process; `cargo build --features
m1-test` boots the M1 exit test instead. QEMU prints "M3 SPAWN PASS".
Userspace linker scripts must keep each permission class page-aligned
(one PT_LOAD per class — the loader maps per segment).

### M4: the MVP demo (done)
`bash scripts/run-demo.sh` builds everything, assembles a demo image with
mkfs, and boots the full system: init spawns storaged (virtio-blk over
the MMIO window + DMA region it grants, postcard session protocol,
blocks on a readable→notification binding) and the shell
(ls/cat/write/rm/snap/snaps/rollback/sync/run). `run bin/hello` loads an
ELF from the versioned store and spawns it with an explicit cspace.
cas/storage-server/virtio-blk are no_std+alloc (`urt` provides the
userspace heap). Remaining debt: streaming WAL replay (mount buffers the
whole WAL region — mkfs images use a 1 MiB WAL), IRQ-driven virtio
completion (driver polls), bulk data path (reads are message-bounded).

### M5: GC + history rewriting (done)
On-disk format v2: the superblock references a durable chunk index
(hash → offset/len/birth-generation + free-extent list) written as a
self-verifying frame — mount no longer scans, and the sweep is a pure
metadata edit through the normal A/B flip (crash mid-GC recovers the
previous commit; crash loop + proptest in `cas/src/store.rs`, mark walk
in `cas/src/gc.rs`). Freed extents become allocatable only after the
flip lands. History rewriting: `DeleteSnapshot` (re-points parents;
tagged snapshots refuse deletion), `SetClass`, `Gc`, `Statfs` wire ops
gated on `may-rewrite-history`; post-rewrite trigger + crude 20%-free
watermark arm a GC that storaged drains after replying. Shell built-ins:
`snapdel keep prune gc df` (retention policy is shell-side; `snap` now
takes class `auto`). Remaining debt: the tail high-water mark never
retracts (freed space is reused, the region never visibly shrinks);
first-fit allocator; no concurrent GC (the server is single-threaded, so
mark/sweep run inside one request — the §4.6 incremental machinery
stays deferred).

All MVP milestones (M0–M5) are complete.

### Rev2: the time page (§2.6) — done
Init reads the PL031 once at boot (new kernel boot caps: slot 4 = RTC
frame, slot 5 = init's own aspace), publishes `(seq, wall_base_ns,
cntvct_base, cntfrq)` in a read-only frame funded from its untyped, and
maps it into storaged and the shell (the address travels in the startup
blocks: `SD02`/`SH01`). `urt::time` owns the page ABI, the seqlock
reader (seq is constant zero today; the retry path is host-tested with a
tearing writer thread, incl. under Miri), and the overflow-safe tick→ns
conversion (proptested — the naive `Δ·10⁹` u64 form overflows ~5 min
into uptime at 62.5 MHz). storaged stamps snapshots/mtimes/ticket-TTLs
with UTC ns; the on-disk format is v3 and pre-v3 images are refused with
a distinct version error (`StoreError::UnsupportedVersion`), never
reinterpreted — re-create them with mkfs. Snapshot timestamps are
clamped per-ref strictly monotone (`max(now, predecessor+1)`, §4.7).
QEMU invocations pin `-rtc base=utc,clock=host`. End-to-end proof:
`bash scripts/boot-test.sh` boots the demo, takes two snapshots, and
asserts sane, strictly ordered ISO-8601 timestamps plus a zero-syscall
shell `date`.

### Rev2: thread reports (§5.1) — done
TCBs carry on-exit/on-fault binding slots (real CDT-visible CapSlots —
notification caps move in via `thread_bind`, revoke sees through them)
and a preallocated terminal report record (running → exited(status) |
faulted(cause, far), one transition ever). `thread_exit(status)` (nr 15,
status now recorded) and `read_report` (nr 22) complete the surface;
`bind-reports`/`read-report` rights bits gate them (creator thread caps
carry both — `Rights::THREAD_ALL`). Thread destruction produces no
report (destruction is the parent acting, not the thread dying). The
CapRevocation TLA+ model covers the binding slots (Bind/ThreadExit/
ThreadFault actions; FireSafe + ReportMonotone properties) — TLC-checked.
The §3.3 channel side rides in the same file as a second spec, `TSpec`
(config `CapRevocation_Teardown.cfg`): channel peer-closed bindings are
refcounted, not CDT-visible (the kernel `bind` bumps the notification's
object refcount and leaves the binder's cap in place, unlike the
move-in TCB slots), so whole-object teardown firing safety is a refcount
discipline — modeled with explicit notification objects. Properties:
ChannelFireSafe (every live channel's peer-closed binding names a live
notification, so teardown fires a live object even after the lineage is
revoked), RefCountSound, ReclaimedReleased — TLC-checked (252 states).
Each spec holds the other's variables constant, so TSpec leaves the
799k-state revocation proof untouched. The kernel side already satisfies
this: `cspace::delete` fires `endpoint_cap_dropped` (peer-closed) before
`obj_unref`, and the binding holds a notification refcount, so a revoke
that tears the whole channel down fires each surviving peer's binding
into a still-live object. The runtime witness is M1 EL0 step 6
(`scripts/m1-test.sh`): a channel carved from a sub-untyped, both ends'
peer-closed bound to a notification funded from a *separate* untyped,
revoke the sub-untyped, assert both bindings fired and the notification
outlived the channel.

Userspace half (the shell's reclaim-on-exit loop) is now done. Two kernel
mechanisms the §5.1 spawn design needed land with it: `retype` can carve a
child-sized **sub-untyped** (`OBJ_UNTYPED`, §2.3 page-aligned sub-range,
phys-read stripped) and `untyped_reset` (nr 23) zeroes a carved untyped's
watermark once `revoke` has emptied it (§2.5). The CapRevocation model
already covers both at its abstraction (sub-untyped carve = a `Copy`-style
CDT-child derivation; reset's precondition *is* the modeled `Retype` guard
`Descendants(c) = {}`), so its invariants are undisturbed. `urt::slots` is
a host-tested cspace-slot free-list; `urt::spawn` owns the canonical loop
(`SpawnRec::arm` binds exit/fault before start; `SpawnRec::reap` does
`read_report` strictly before `revoke`+`reset`, asserted — the report
lives in the TCB the revoke kills). The shell carves one persistent event
notification + one reusable donation untyped from its pool (slots 3/4),
spawns each child as a single CDT subtree under the donation, multiplexes
exit/fault on one notification word (the first real §3.6 bit-group scan),
and recycles its slot window. `bash scripts/spawn-test.sh` is the proof
(same genre as the M1 revocation test): `runloop bin/selftest 100`
(slots 56/56, no leak), exit-status propagation, and the fault demo —
`faulted(translation, 0xdead0000)` then a clean re-spawn — with no
BSS-LEAK (retype re-zeroes reused frames). The shell also grants each
child the **time page** (§2.6): init installs a read-only time-frame cap
in shell cspace slot 5, the shell maps a fresh copy into every child's
aspace and passes the VA in the ST01 block (the init→shell grant, one hop
further), and unmaps it before the reap revoke that frees the child
aspace (§2.5 ordering). `spawn-test.sh` step 6 proves it: `run
bin/selftest 253` reads a sane UTC clock (`time-ok`). Scope cut held:
children get stdin/stdout via the console; no storage-session delegation
(that needs the server to accept a second session, §2.4).

### Rev2: the IPC crate (§3) — done
`doc/plans/2_ipc.md` (six phases) built the userspace IPC crate the MVP
deferred (`0_mvp.md` debt). Verifiable-first: the kernel IPC surface sits
behind a `Transport` seam (`SyscallTransport` in production, the
deterministic in-memory `ModelTransport` for harnesses), so the
cross-process races (lost wakeup, backpressure, cap handoff) run under
**Shuttle** (randomized, at scale) and **Loom** (exhaustive, the
lost-wakeup memory-ordering fragment) over the real reactor code — the
concurrency counterpart to Kani on the kernel core. The `IpcReactor` TLA+
spec (safety invariants + the project's first liveness property,
`EventuallyDelivered`) is the design gate, re-checked in CI's `model`
job. Surface: non-blocking `Endpoint::{send_nb,recv_nb}` (§4.1, null-slot
tolerant); the epoll-shaped `Reactor::{register,wait}` that **hides
notification bits** and owns the bind-poll-wait discipline (§4.2/§3.6);
`send_blocking`/`send_retry` over the writable signal (§4.3); the
`send_acked`/`recv_acked` valuable-cap handshake (§4.4); the
module-private postcard `wire` codec behind the `wire` feature (§4.5,
opt-in so alloc-free binaries stay minimal); and `ipc::session` — the
§4.6 admission layer: `Admission` is the single window-quota admission
point (never over-grants), with the fixed `ConnectReq`/`GrantReply`
codecs and the pure `admit_connect` step. Harnesses #1–#5 (FIFO/no-drop,
lost-wakeup, backpressure, cap-ack, multi-client fairness) live in
`ipc/src/model.rs`; the `concurrency` CI job runs them with no per-test
filter, so a new `loom::model`/`shuttle::check_*` auto-gates. The Shuttle
harnesses run under a **pinned seed** (`check_pinned`, a seeded
`RandomScheduler`) so a CI failure reproduces from source, with a
`shuttle_replay_corpus` landing spot for committing a failing schedule as a
`shuttle::replay` regression (the fuzz-corpus discipline; loom-shuttle §5).
The wire decoder is also a cargo-fuzz target (`ipc/fuzz`). **Verus** now verifies
the pure codecs and the quota (migrated off Kani, phase 7a/7b — `ipc/src/proofs.rs`
is deleted): the `Header` (`ipc::header`, §3.7), the §4.6 session codecs
(`ConnectReq`/`GrantReply`) as total bijections, and `Admission`'s
**never-over-grant** invariant ∀ admit/release sequences (review rec 4) — all
unbounded (`doc/results/57`, `58`). The reactor's multi-source dispatch is a
recorded caveat (single-source TLA/Loom; multi-bit rests on harness #5 — see
`IpcReactor.tla`). **`storaged`**
(`user/storaged/src/main.rs`) is the first production consumer: its
drain-then-wait loop is now `Reactor::wait` + `Endpoint` over
`SyscallTransport`, dispatching by opaque key — no notification bit named
in the server, so the §3.6 wait-set upgrade will change no server code.
The **shell** (`user/shell/src/main.rs`) is the first *multi-source*
consumer (review rec 2, `doc/results/19_ipc-review.md`): its spawn/reap
loop multiplexes a child's exit and fault terminations through the reactor
via `Reactor::register_bound` — the entry point for **externally-bound,
edge-triggered** sources (a thread on-exit/on-fault `thread_bind`, a
timer, an IRQ), which (unlike the channel `register`) neither binds nor
self-signals a poll-once. Scope cut: the *dynamic* connect (a client
retyping a channel pair and the server accepting a **second** concurrent
session) needs kernel cap-transfer wiring and stays a follow-up; the
admission protocol and reactor multiplexing it relies on are proven
(harness #5).

### M1 exit criterion (met)
Booting prints `123456M1 PASS` (`bash scripts/m1-test.sh` builds the
`m1-test` feature, boots it, and asserts the full marker line): the
embedded EL0 test program (`kernel/src/user.rs`) retypes untyped into
kernel objects, builds a second thread's cspace explicitly, exchanges a
message + derived cap over a channel with notification-driven waiting,
then revokes the parent cap and verifies both the received copy, a queued
in-flight cap, AND the on-exit binding cap in the second thread's TCB
died; a timer object signals a bound notification; the rebound on-exit
binding delivers the child's death notice and read_report returns
exited(42) (§5.1, the thread-report batch); finally it builds a throwaway
channel from a carved sub-untyped, binds both ends' peer-closed events to
a separately-funded notification, and revokes the sub-untyped — the
runtime witness for §3.3 whole-object teardown (every endpoint's
peer-closed binding fires before reclamation; the notification survives;
the dead endpoint caps then error). The embedded user program is an M1
scaffold, replaced by real binaries at M3 — it must not call into kernel
.text (EL0 execute-never), hence `opt-level = 1` for dev and care with
non-`#[inline(always)]` helpers in user.rs.

### Sequencing rules
- **TLA+ `CapRevocation` model must be checked before M1 implementation.**
- **TLA+ `CommitProtocol` model must be checked before M2 implementation.**
- **TLA+ `IpcReactor` model must be checked before the IPC reactor
  implementation** (the §3.6 lost-wakeup/backpressure protocol; plan
  `doc/plans/2_ipc.md` §5.1 — Phase 0 lands the spec, the reactor is phase 2).
- `cas` crate's proptest canonical-form suite must pass before `mkfs` is used.
- The `storage-server` and `mkfs` can be developed on macOS host in parallel
  with M0–M1 (they are pure userspace Rust, no kernel dependency).
- IOMMU migration (§2.5) must happen before writing the second DMA driver.

---

## Architecture invariants (never violate these)

- **No ambient authority.** Every resource access is via a capability slot or
  a storage handle. No globals, no environment-based auth.
- **Monotone derivation.** Authority can only shrink, never grow (§2.3).
  Attenuation is the only derivation; there is no amplification path.
- **Move semantics for caps** (§3.4). A cap has exactly one owner at all
  times. Senders duplicate first if they want to keep access.
- **Raw hashes are not authority** (§2.4). Storage handles (small integers,
  session-relative) are authority. Hashes are internal addresses and proofs.
- **Event delivery never allocates** (§3.6). Both the notification-bit regime
  and the future wait-set upgrade must satisfy this.
- **DMA only through DmaPool** (§2.5). No raw physical addresses outside the
  `dma-pool` crate. The `phys-read` rights bit enforces this at the kernel
  level; code discipline enforces it in userspace.
- **No kernel allocation that isn't user-accounted** (§2.5, §3.2). Channels,
  address spaces, and wait-sets are created from untyped memory donated by the
  creator; the kernel has no global pool.

---

## Verification tiers (§6)

| Tool | Scope | When |
|------|-------|------|
| TLA+ / TLC | commit protocol, cap revocation | Before respective milestone |
| Kani | **retired** (plan `doc/plans/3_verus-rewrite.md`): the interim bounded tier. Every target migrated to Verus — `kcore` (phase 2) and the §4.7 host chokepoints `ipc` (7a/7b) + `urt` (7c/7d) + `dma-pool` (7e) + `cas` (7f, the last). The `kani` CI job, the pinned-cargo-kani install, and the `#[cfg(kani)]` scaffolding are gone; findings recorded in `doc/results/2…8_kani-findings*.md` | (historical) |
| Verus | **mechanized implementation tier for `kcore`** (plan `doc/plans/3_verus-rewrite.md`): unbounded/functional proofs on the real handle/`Store` code — `untyped::carve` (phase 0); the non-recursive cspace/CDT ops `derive`/`cdt_insert_child`/`obj_ref`, now preserving full `cspace_wf` (parent+sibling acyclicity composition, phase 2c); `revoke`/`descend_to_leaf` termination (phase 2b); `slot_move` and `cdt_unlink` in full (body proofs — `slot_move`'s transposition lands the renaming, `doc/results/24`; `cdt_unlink`'s sibling-list merge lands `unlinked`, parent-rank witness reused / sibling-rank rescaled, `doc/results/25`); **phase 3** the untyped remainder `retype_check`/`retype_install`/`reset` (the §2.5 sub-`Untyped`-never-`PHYS` rights theorem) + the channel ops `send`/`recv`/`endpoint_cap_added`/`endpoint_cap_dropped`/`bind`/`fire` against `chan_wf` + the FIFO `Seq` model (`doc/results/26`…`30`); **phase 4** the notification ops `signal`/`wait`/`remove_waiter`/`destroy_notif` (the `waiter_seq` FIFO model — wake order = block order; `signal` graduates `external_body` → proven), the thread ops `report_terminal` (ReportMonotone + FireSafe) / `bind`, and the timer ops `arm`/`disarm`/`check_expired`/`destroy_timer` (the head-only armed-list `timer_wf`; the waiter + armed-timer `refcount_sound` terms) (`doc/results/31`…`35`); **phase 5** the sysabi `decode`/`decode_prio` + `ObjType::from_u64` and the aspace walker `pte_encode` (the §2.5/§4.5 isolation theorem) / `pte_output_pa` / `va_range_ok` / `range_mapped_in` / `map_in` / `unmap_in` against the `pt_wf` page-table tree model + the TLBI effect-ordering log (`doc/results/36`…`40`) — the first Verus reasoning over concrete Rust slices, **no `external_body`**; **phase 6** the cross-object teardown cluster `delete`/`obj_unref`/`destroy_cspace`/`unref_cspace`/`unref_aspace`/`channel::destroy_channel`/`thread::destroy_tcb` — **all `external_body` removed**, the cross-module recursion closed under the seL4-zombie `(count_nonempty(slot_view), height)` measure — plus `revoke` conditional non-zombie root-survival and the full `refcount_sound` census (a system invariant on the teardown family + the construction ops `derive`/`channel::bind`/`endpoint_cap_added`/`signal`/`remove_waiter`/`endpoint_cap_dropped`; the remaining construction ops keep their landed per-op delta with the system clause a recorded follow-on) (`doc/results/41`…`56`) — **kcore's object operations now carry zero `external_body` and zero plain-Rust**, the trusted base reduced to the `Store` hardware/scheduler seam; **phase 7** the §4.7 host chokepoints port off Kani per target (7a: the `ipc::header` §3.7 message-header bijection, `doc/results/57`; 7b: the §4.6 `ipc::session` codecs + the `Admission` never-over-grant quota ∀ sequences, `doc/results/58` — `ipc` now fully off Kani; 7c: the `urt::slots` bitmap free-list ∀ `cap`/`WORDS` — alloc distinctness/exact-exhaustion, `alloc_range` contiguity, the double-free precondition, via loop restructuring + `by (bit_vector)` frame lemmas, `doc/results/59`; 7d: the `urt::time` tick→ns conversion `utc_ns_at` — totality + **monotonicity** ∀ (the decomposition `secs·10⁹+frac == (delta·10⁹)/f` via `lemma_hoist_over_denominator`; what Kani could not prove), `doc/results/60` — `urt` now fully off Kani; 7e: the `dma-pool` first-fit free-list allocator — two-buffer disjointness + alignment ∀ (the **DN-10** trophy Kani OOM'd on), a `FreeList` extracted to `verus!{}` (the PA seam stays trusted plain Rust), `alloc`/`free` against a sorted-disjoint-extent `wf` + `covers` model, `copy_within`→`remove_at`/`insert_at` shift helpers + modular round-up (no `by (bit_vector)`), `doc/results/61`; 7f: the `cas::disk` on-disk superblock — `validate_geometry` totality + region-within-device ∀ (overflow-exact `<==>`) and `decode_checked` decode-totality ∀, the verified byte-parsing core returning a `Hash`-free `RawSuperblock` with blake3 the `external_body` assumed-total seam, `doc/results/62`; 7g: the `cas::tlv` §4.9 directory-entry TLV codec — **full exec-level** canonical-form round-trip `encode(decode(b))==b` ∀ + decode totality, a `Hash`-free `RawEntry` core (`Vec<u8>`+`[u8;32]`, so hash bytes round-trip in-proof — no Hash axiom), vstd `alloc` enabled, entry-level `validate_entry` left plain Rust, the `tlv_entry` fuzz oracle kept as the composed-path guard, `doc/results/63` — the **last** §4.7 target, so **Kani is fully retired** (job + install + scaffolding gone)). + `scratchpad` smoke | CI `verus` job (`cargo verus verify -p kcore -p ipc -p urt -p dma-pool` + `-p cas --no-default-features`); during the Verus rewrite |
| Loom / Shuttle | IPC crate, userspace servers | During M1+ development |
| Miri + proptest | everything; chunker + prolly tree esp. | Continuous |
| cargo-fuzz | IPC decoder, postcard payloads | From M1 |

The IPC crate (`ipc/`) is the first serious Loom/Shuttle target (§3.5).

**Deviation from the §6 spec table (`doc/plans/0_kani-rewrite.md`).** The spec
assigned cspace/CDT and the allocator to **Verus** ("written in Verus dialect
from day one"); that did not happen — the kernel predated any verification
tooling. **Kani served as the interim mechanized tier for the kernel
implementation**: the object machinery was extracted into the host-buildable
`kcore` crate and the harness suite (plan §4.1–§4.7) re-checked the
CapRevocation TLA+ invariants on the real code (`cargo kani -p kcore`) —
cspace/CDT, untyped, channels, notifications, thread reports, the §2.4
page-table-walker rewrite, and the §2.5 syscall-decode split — plus the
host-side chokepoints (`urt`, `ipc`, `cas`, `dma-pool`). It found and fixed
real defects (a `carve` overflow DoS; a `PERM_DEVICE | PERM_X` executable-MMIO
encoding). Its shape (explicit `wf()` predicates, the handle/`Store` seam, no
int→ptr in the core) is exactly what the Verus port needed — and
`doc/plans/3_verus-rewrite.md` has now made that port the real thing: as of
phase 2, **Verus is the mechanized kernel-core tier** (the spec's original
assignment), so `cargo kani -p kcore` is retired and the kcore harnesses are
deleted. The historical findings/bounds remain recorded:
`doc/results/2_kani-findings.md` … `8_kani-findings-7.md`.

### Continuous integration

`.github/workflows/ci.yml` runs on every PR and push to main:
- **host-tests** — `cargo test --workspace --exclude kernel` (the kernel is
  bare-metal and can't host-build): the `urt` slot-allocator + heap, the
  monotone rights-mask attenuation (`storage-server` sessions), the CAS
  canonical-form proptests, the wire decoders, the ELF parser, etc.
- **model** — reruns the TLA+ proofs (CapRevocation, its §3.3 teardown
  TSpec, CommitProtocol, and the §3.6 `IpcReactor` lost-wakeup/backpressure
  spec) on Linux. `tools/tla/find-tla-tools.sh` honours a pre-set `JAVA` +
  `TLA_TOOLS`, so CI points it at a downloaded `tla2tools.jar`; locally it
  still finds the macOS Toolbox.
- **on-os** — boots the system under QEMU and runs the §5.1 exit criterion
  (`scripts/spawn-test.sh`: the 100× burn loop, status propagation, the
  wild-pointer fault demo + re-spawn, the panic path, the time grant) plus
  the M1 cap-mechanism EL0 test (`scripts/m1-test.sh`).
- **kani** — **removed** (phase 7f). Kani was the interim bounded mechanized
  tier; every target it covered is now proven unbounded in Verus (the `verus`
  job): the `kcore` kernel core (phase 2) and the §4.7 host chokepoints `ipc`
  (7a/7b), `urt` (7c/7d), `dma-pool` (7e), and `cas` (7f — the last; it ported
  cleanly so it is not a holdout). With `cas`'s `proofs.rs` deleted, the job
  had no targets left, so the whole job, the pinned-cargo-kani-0.67.0 install +
  cache, and the cover-vacuity guard were removed (`doc/results/62`). The
  historical findings stay recorded in `doc/results/2…8_kani-findings*.md`.
- **verus** — `cargo verus verify -p kcore -p ipc -p urt -p dma-pool` + `-p cas --no-default-features` (pinned Verus `0.2026.06.07.cd03505`,
  release zip cached): the deductive kernel-core proofs (`untyped::carve`; the
  non-recursive cspace/CDT ops `derive`/`cdt_insert_child`/`obj_ref` preserving
  full `cspace_wf`; `revoke`/`descend_to_leaf` termination; the full body proofs of
  `slot_move` and `cdt_unlink`; the phase-3 untyped `retype_check`/`retype_install`/
  `reset` and channel `send`/`recv`/`endpoint_cap_added`/`endpoint_cap_dropped`/
  `bind`/`fire` against `chan_wf` + the FIFO `Seq` model; the phase-4 notification
  `signal`/`wait`/`remove_waiter`/`destroy_notif`, thread `report_terminal`/`bind`,
  and timer `arm`/`disarm`/`check_expired`/`destroy_timer` against `notif_wf` +
  `timer_wf`); plus the phase-7 §4.7 host chokepoints `ipc::header` (7a — the §3.7
  message-header bijection), `ipc::session` (7b — the §4.6 `ConnectReq`/`GrantReply`
  codec bijections + the `Admission` never-over-grant quota ∀ admit/release sequences),
  `urt::slots` (7c — the bitmap free-list ∀ `cap`/`WORDS`: alloc distinctness +
  exact exhaustion, `alloc_range` contiguity, the double-free precondition), and
  `urt::time` (7d — the tick→ns conversion `utc_ns_at`: totality + monotonicity ∀,
  the decomposition `secs·10⁹+frac == (delta·10⁹)/f` via `lemma_hoist_over_denominator`), and
  `dma-pool` (7e — the first-fit free-list allocator's `FreeList`: `alloc`/`free` against a
  sorted-disjoint-extent `wf` + `covers` model, two-buffer disjointness + alignment ∀, the
  DN-10 trophy; `copy_within`→shift-helper + modular round-up restructures, no `by (bit_vector)`),
  and `cas` (7f — `cas::disk` superblock: `validate_geometry` totality + region-within-device ∀
  with an overflow-exact `<==>`, and `decode_checked` decode-totality ∀ via a verified byte-parsing
  core returning a `Hash`-free `RawSuperblock`, blake3 the `external_body` assumed-total seam;
  7g — `cas::tlv` §4.9 TLV codec: full exec-level canonical-form round-trip `encode(decode(b))==b` ∀
  + decode totality, a `Hash`-free `RawEntry` core (`Vec`+`[u8;32]`), vstd `alloc` enabled, the
  `tlv_entry` fuzz oracle kept; verified `--no-default-features`). With 7g every §4.7 chokepoint is
  on Verus and Kani is retired. No per-proof filter, so a new `verus!{}` obligation gates automatically.
  The `host-tests` job's `kcore` leg now also runs `test_store` — `check_delete`/
  `check_destroy_channel`/`check_destroy_tcb` were the executable check of those ops'
  assumed `external_body` contracts and, now that phase 6 has **proven** their bodies,
  stay as differential regression guards of the (now verified) contracts.
- **concurrency** — the Loom/Shuttle models under `RUSTFLAGS="--cfg loom"` /
  `"--cfg shuttle"` (plan `doc/plans/1_loom-shuttle-rewrite.md` §6):
  `cargo test -p urt -p ipc --lib`. Loom is the certifying exhaustive proof
  (the `urt::time` seqlock; the `ipc` `ModelTransport` rig), Shuttle the
  randomized breadth-smoke. No per-test filter, so a new `loom::model` /
  `shuttle::check_*` test auto-gates.
- **layering** — greps `kcore/src` for the §2.2 violations CBMC can't model
  (`asm!`/`global_asm!`, `as *mut`/`as *const`); kcore uses `.cast()` for
  every pointer-to-pointer conversion.

`.github/workflows/fuzz.yml` is separate (corpus replay per PR; nightly hunt).

---

## Kernel source map (`kernel/src/`)

| File | Responsibility |
|------|---------------|
| `main.rs` | Entry point (`kernel_main`), boot caps, first eret, panic handler |
| `boot.rs` | `_start` assembly: core selection, SP_EL1, BSS zero, → kernel_main |
| `uart.rs` | PL011 UART driver (MMIO at 0x0900_0000); `core::fmt::Write` impl |
| `exceptions.rs` | Vector table; EL0 trap-frame save/restore; EL1 = fatal |
| `mmu.rs` | Identity map: 2 MiB L2 blocks for DRAM, EL0 window at 0x4800_0000 |
| `cspace.rs` | Cap slots, CDT (parent/child/sibling), derive/delete/revoke/move |
| `untyped.rs` | Untyped caps (region+watermark inline), retype, reset |
| `thread.rs` | TCB, TrapFrame layout, ready queues, `maybe_switch` |
| `channel.rs` | Two-ring channels, CDT-visible queue cap slots, event bindings |
| `notification.rs` | Signal word + FIFO waiter queue |
| `timer.rs` | Generic-timer tick (100 Hz), timer objects, CNTVCT helpers |
| `gic.rs` | GICv3 minimal bring-up (vtimer PPI 27), ack/eoi |
| `syscall.rs` | SVC dispatch (x7 = nr); M1 scaffold ABI, not stable |
| `user.rs` | Embedded EL0 test program (M1 exit criterion; removed at M3) |

### QEMU virt memory map (relevant to M0)
```
0x0900_0000  PL011 UART0
0x0800_0000  GICv3 distributor
0x4000_0000  DRAM start (kernel loads here)
```

---

## Storage server conventions

- All state accessed via handles (small integers, session-relative).
- Per-ref overlays; never a single global memtable.
- Flush triggers: explicit sync/snapshot > WAL pressure > size pressure > timer.
- Commit is always: fsync chunks → write new superblock → fsync superblock.
  Nothing is freed on the write path; GC is the only reclamation mechanism.
- Snapshot identity is a per-ref sequence number, never a content hash (§4.7).

---

## IPC wire protocol

- Every message: fixed hand-defined header (proto id, version, opcode, flags,
  body length) + postcard-encoded body (§3.7).
- Capabilities travel in cap slots, never in payloads.
- Storage handles are plain integers in payloads; never raw hashes.
- Message types: boring — no borrowed lifetimes, no serde tricks.
- Decoders reject trailing bytes; they are cargo-fuzz targets.

---

## Style and code conventions

- `no_std` for kernel and userspace process crates; `std` available for cas,
  mkfs, and for host-side testing of any crate.
- No `unsafe` without a comment explaining what invariant it relies on.
- Kernel assembly lives in `global_asm!` blocks in the relevant `.rs` file,
  not in separate `.S` files.
- No comments explaining what code does; only comments explaining *why*
  (hidden constraints, non-obvious invariants, workarounds).
- All system APIs must ship with precise contracts before being called from
  a second crate.
