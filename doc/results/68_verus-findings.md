# Verus findings 48 — Phase 9a: proof-hygiene audit + trusted-base ledger

Plan: `doc/plans/3_verus-rewrite.md` (§7 step 8, §8) and
`doc/plans/3_verus-rewrite_closeout-detail.md` (§9a). Prior increment: `67`
(phase 8d — `cas::store` composition, the last *proof* phase). Phase 9 is
**closeout**: it ships no new kernel/chokepoint proofs but certifies the rewrite
did not cheat to get green and produces the one authoritative enumeration of what
remains trusted. 9a is its first sub-phase — the **proof-hygiene audit** (prove
the proofs do not cheat) plus the **trusted-base ledger** (the single source of
truth 9d's `verus.md` and 9e's `CLAUDE.md` "trusted base is exactly …" claim both
cite). It is an audit *with teeth*: it lands real code changes (discharging the
last in-proof `assume`, zeroing every warning), not just prose.

## Result: the suite is green, zero warnings

| Crate | `cargo verus verify` | Warnings |
|---|---|---|
| `kcore` | **316 verified, 0 errors** | 0 |
| `ipc` | **58 verified, 0 errors** | 0 |
| `urt` | **29 verified, 0 errors** | 0 |
| `dma-pool` | **26 verified, 0 errors** | 0 |
| `cas` (`--no-default-features`) | **58 verified, 0 errors** | 0 |
| **total** | **487 verified, 0 errors** | **0** |

`cargo build --workspace --exclude kernel`: clean. `cd kernel && cargo build`
(the aarch64 erasure path): clean apart from the build-std `core v0.0.0` future-
incompat note, which is the pinned nightly's own `core` source, not project code
(non-actionable here — it would move only with a toolchain bump, its own PR per
the pin discipline). `cargo clippy --workspace --exclude kernel`: clean.
`cargo test --workspace --exclude kernel`: green (incl. the two new host tests
below and every kept proptest/fuzz-corpus differential guard).

---

## 1. In-proof `assume` / `admit` — driven to zero

**Before:** exactly one real in-proof assumption in the whole verified surface —
`kcore/src/untyped.rs` `assume(bytes > 0)` in `carve`, just before the
`carve_place` call. (The `adm.admit(...)` hits in `ipc/src/session.rs` are the
`Admission::admit` *method*, not the Verus `admit()` intrinsic — false positives,
excluded.) A bare in-proof `assume`, even commented, is the weakest trusted form;
closeout's bar is that it should not survive.

**Disposition: discharged + boundary-contracted (the `assume` is deleted).** The
fact `carve`'s geometry needs, `bytes > 0`, was triaged per `match` arm:

- **`bytes_for` arms (CSpace / Channel / Aspace).** Each helper adds a non-zero
  struct base to its item count (`size_of::<CSpaceObj>() + n·size_of::<CapSlot>()`,
  etc.), so the byte count is positive — but the helpers live in plain Rust
  (shared with the kernel shell) and their specs were *deliberately empty*. Their
  three `assume_specification`s now carry an **`ensures r > 0` contract** at the
  boundary (`untyped.rs:236-238`), backed by the new host test
  `untyped::tests::bytes_for_positive` (asserts each `bytes_for` is positive for
  `0 / 1 / mid / max` item counts). This is the plan's "external boundary contract
  + host test" form — strictly stronger than a bare caller-side `assume`: the
  assumption is named on the helper signature, not buried in the caller, and it is
  observable.
- **`size_of` arms (Thread / Notification / Timer).** Verus treats these object
  structs as opaque (the `ExTcb` / `ExNotifObj` / `ExTimerObj`
  `external_type_specification` registrations), so it cannot see the fields and
  cannot derive `size_of::<T>() > 0` on its own (confirmed: an inline
  `assert(size_of::<Tcb>() > 0)` fails). The three arms now route through one
  trusted helper `fixed_object_bytes(ty) -> (r: u64) ensures r > 0`
  (`untyped.rs:275`, `#[verifier::external_body]`), host-checked by the new
  `untyped::tests::object_size_positive` (asserts both `size_of::<T>() > 0` and
  the helper for all three kinds). The structs are genuinely non-ZST (each carries
  at least an `ObjHeader`), so the contract is honest.
- **Frame arm (`param * 4096`, `param ∈ [1, 65536]`).** Fully discharged by Verus
  — no contract needed (the existing no-overflow proof already established it).
- **Untyped arm (`checked_next_multiple_of(4096)`).** `param as usize` can
  truncate on a hypothetical 32-bit `usize` (the kernel target is 64-bit, where
  `param != 0 ⇒ round-up ≥ 4096`), so positivity is not free ∀ widths. Discharged
  by a defensive `if b == 0 { return Err(BadArg) }` guard (`untyped.rs`): zero-cost
  on the 64-bit target (unreachable there), and it makes `bytes > 0` provable for
  all widths **with no trusted assumption** — a small robustness improvement, not
  a trust boundary.

Net: the weakest trusted form (a bare in-proof `assume`) is **gone**; what it
papered over is now either fully proven (Frame, Untyped) or a named,
host-tested boundary contract (the six size-helper arms). `grep` confirms no
`assume(` / `admit(` statement survives in any verified crate.

---

## 2. The trusted-base ledger

This is the authoritative enumeration. Verus trusts a fact only through one of
the constructs below; each row names what it assumes, why it is not (or should
not be) a project-code proof, and the host test that exercises the contract.

### 2.1 Trusted functions — `#[verifier::external_body]` (4)

A function whose body Verus does not look inside; the `ensures` (or the empty
post) is taken on trust, discharged at runtime by the cited host test.

| Site | Assumes | Why a boundary, not a proof | Host test |
|---|---|---|---|
| `kcore/untyped.rs:275` `fixed_object_bytes` | the three fixed-size object structs are non-ZST (`r > 0`) | the structs are opaque to Verus (`external_type_specification`); positivity is a layout fact Verus cannot see | `untyped::tests::object_size_positive` (**new, 9a**) |
| `urt/slots.rs:344` `debug_check_free` | nothing (empty post) — runtime double-free guard | `debug_assert!` lowers to `panic!`, forbidden in `verus!{}` exec; the *static* guarantee is `free`'s `!is_free_spec` precondition | `urt::slots::tests::double_free_panics` (`slots.rs:492`) |
| `cas/disk.rs:338` `checksum_ok` | blake3 is a deterministic total function (returns a bool, never panics) | interpreted hashing, out of verification scope (Kani stubbed it identically with `-Z stubbing`); totality needs no collision-freedom | `disk::tests::superblock_roundtrip_and_tearing` + the store crash-recovery proptests |
| `cas/store.rs:570` `wal_content_ok` | blake3 payload checksum + `WalOp::decode_record` are total (`r == content_ok_spec(rec)`) | both out of scope — interpreted hashing + `Vec`-building content decode (TLA+'s abstracted record value); the seam is the standard trusted-fn-with-uninterpreted-spec idiom | `crash_recovery_preserves_acked_state` proptest + the `wal_replay_scan` fuzz corpus |

The first row is **new this phase** (the `assume` discharge, §1); the other three
are confirmed-legitimate categories that predate 9a, each now with its test named
in one place. No `external_body` row lacks both a reason and a test.

### 2.2 Opaque types — `#[verifier::external_type_specification]` (13)

Plain-Rust value/handle types registered so they may appear in spec expressions
(paired with `#[verifier::ext_equal]` for structural `==`, and with
`#[verifier::external_body]` on the three object-struct wrappers so their fields
stay opaque). These are *not* skipped proofs — they are the spec-visibility seam
for types shared with the kernel shell. Confirmed all 13 are such:

- `cspace.rs` (10): `ExSlotId`, `ExObjId`, `ExRights`, `ExChanEnd`, `ExCapKind`,
  `ExCap`, `ExCapSlot`, `ExBinding`, `ExThreadState`, `ExReport`.
- `untyped.rs` (3): `ExTcb`, `ExNotifObj`, `ExTimerObj` (the object structs whose
  positivity §1's `fixed_object_bytes` now contracts).

### 2.3 Trusted library / constructor signatures — `assume_specification` (6)

Tell Verus to trust a signature it cannot or need not see the body of.

| Site | What | Boundary kind |
|---|---|---|
| `aspace.rs:76` `u64::saturating_mul` | std arithmetic | library (sound by std semantics) |
| `cspace.rs:1172` `CapSlot::empty` | a trivial kcore constructor | project constructor (trivial body; `doc/results/28 §1` notes the standalone spec it replaced) |
| `untyped.rs:236-238` `*::bytes_for` ×3 | `ensures r > 0` | **strengthened this phase** (§1) — positivity contract on the real helper, host-tested |
| `untyped.rs:260` `usize::checked_next_multiple_of` | returns `Option` (signature only) | library (vstd has no value spec; the Untyped arm re-checks positivity via the `b == 0` guard, so no value is trusted) |

`external_fn_specification` and bare `#[verifier::external]` (the other escape
hatches the audit looked for): **none** in any verified crate.

### 2.4 The `Store` hardware/scheduler seam — `external_trait_specification` (1 trait)

`kcore/cspace.rs:400` registers the `Store` trait via `ExStore` (with the
`external_trait_extension(StoreSpec)` that adds the seven ghost views). The trait
contract is the rewrite's **irreducible trusted base** (plan §2): the generic
`fn op<S: Store>` operations are verified against these `requires`/`ensures`; the
production kernel impl in the `kernel` crate is bare-metal and not verified
against them. The contracts split into two kinds:

- **Ghost-projection getters/setters** (`slot`/`set_slot`, `obj_refs`/
  `set_obj_refs`, the channel/notif/tcb/timer/cspace accessors). Mechanically
  faithful — a getter projects a field, a setter updates one key and frames the
  other views unchanged. Trusted but trivially so.
- **Effectful hardware/scheduler methods** — the genuine trust. Each is
  host-checked against `ArrayStore` in `kcore/src/test_store.rs` (the
  `make_runnable` precedent, validated by the `check_*` differential guards):
  `make_runnable`, `unqueue_ready` (scheduler ready-queue, modeled as
  state→Runnable / no-op on the views), `tlb_invalidate_page` (appends one
  `(asid,va)` to the TLBI log — the §5e ordering theorem rests on this),
  `barrier_after_map` / `barrier_after_unmap` (pure fences), `aspace_unmap` /
  `aspace_destroy` (shell-owned page-table teardown). These are exactly the
  "trusted base = the `Store` hardware/scheduler seam" the prior phases asserted;
  9a confirms the enumeration is complete and each is `ArrayStore`-checked.

The `host-tests` job's `kcore` leg runs `test_store` (`check_delete` /
`check_destroy_channel` / `check_destroy_tcb` and the seam `check_*`), so the
assumed contracts are executable-checked as differential regression guards.

### 2.5 Uninterpreted ghost models — `uninterp spec fn` (1)

`cas/store.rs:583` `content_ok_spec(rec) -> bool` — the ghost model behind
`wal_content_ok` (§2.1). Uninterpreted because blake3 + the `WalOp` payload decode
are the content seam; the maximal-run spec names "this record is content-valid"
without looking inside the hash. Paired with its `external_body` exec twin, not a
standalone skipped obligation.

**Ledger summary.** The trusted base is exactly: the `Store` hardware/scheduler
seam (§2.4, `ArrayStore`-checked) + 4 trusted functions (§2.1, each host-tested) +
1 uninterpreted content model (§2.5) + the std/constructor signatures and
opaque-type registrations (§2.2/§2.3, library-sound spec-visibility seams). The
audit added one trusted function (`fixed_object_bytes`) and removed the one bare
`assume` — a net move from the weakest trusted form to a tested boundary contract.

---

## 3. Warning-zeroing record

Run and driven to **zero** across every tier the closeout names:

- **`cargo verus verify` (5 crates).** Was **10** warnings, all one category:
  *"using `==>` in `assert forall` does not currently assume the antecedent in the
  body; consider using `implies`"* (`channel.rs` ×1, `cspace.rs` ×4,
  `notification.rs` ×5). Fixed by switching the body-level `==>` to `implies` —
  exactly what the lint suggests. Two shapes: where the consequent was a plain
  conjunction, the outer `==>` became `implies` (the empty/`if`-guarded bodies are
  strictly easier with the antecedent now assumed); where the outer was already
  `implies` and the *inner* `==>` was the effective body (`cspace`'s
  `empty_slots_detached` proofs), the inner antecedent was folded into the
  `implies` LHS (`dom.contains(j) && is_empty_cap(..)` — currying, logically
  identical, same `#[trigger]`). A `matches Some(wn)` binder does **not** propagate
  across an `implies` boundary (`cannot find value wn`), so the two matches-guard
  sites keep the nested `==>` as the `implies` consequent — Verus does not warn on
  a nested `==>` that is the proven goal, only on the top-level forall-body `==>`.
  Re-verified: 316 verified, 0 errors, 0 warnings.
- **`cargo build` host + `cargo test` (erased path).** Was **4** ghost-erasure
  warnings (items used only in `verus!{}` spec/proof, unused once the macro erases
  the ghost code): two unused imports (`notification.rs`, `timer.rs` —
  `#[allow(unused_imports)]` per the `lib.rs` precedent), one spec-only fn
  parameter (`aspace.rs` `pa_of_table`'s `pool_len` — `let _ = pool_len;`, the
  `destroy_notif` idiom), and one model-no-op parameter (`notification.rs`
  `destroy_notif`'s `store` — `let _ = store;`, extending the existing `let _ = n;`).
- **Kernel cross-build (`cd kernel && cargo build`).** Clean apart from the
  build-std `core` toolchain note (§Result) — not project code.
- **`cargo clippy --workspace --exclude kernel`.** Was **34** lints across 6 crates
  (kcore 24, cas 5, dma-pool/ipc/urt/virtio-blk the rest). Clippy is **not a CI
  gate** for this project, and the lints fall into two suppressible classes,
  neither warranting a refactor of verified code (the closeout anti-churn rule):
  (a) `verus!{}` verified-exec idioms Verus reasons about directly —
  `assign_op_pattern` (`x = x + y`), `question_mark` (an explicit `match`),
  `manual_is_multiple_of` / `manual_range_contains` (explicit `% == 0` / bounds),
  `collapsible_match`, `too_many_arguments`, `result_unit_err`,
  `implicit_saturating_sub` (the §7d `.saturating_sub` restructure); and (b) FFI /
  device-driver cosmetics — `len_without_is_empty` (device-size traits where
  `is_empty` is meaningless), `missing_safety_doc` (raw-pointer/MMIO `unsafe`
  documented with prose pre/post comments, not a `# Safety` heading),
  `type_complexity`. Each is suppressed with a justified crate-level `#![allow(..)]`
  (one comment block per crate). The single genuinely-cosmetic doc lint
  (`doc_lazy_continuation`, an aspace doc list reading as markdown) was **fixed**,
  not suppressed.
- **`cargo test --workspace --exclude kernel`.** Green; no warnings.

---

## 4. What this phase did and did not do

9a **certifies** (the green suite above, the trusted-base enumeration) and lands
the two audits-with-teeth changes (the `assume` discharge, the warning zeroing).
It adds **no new verification coverage** and changes no CI `-p` / job (the `verus`
job keeps no per-proof filter, so a new obligation still auto-gates). The two new
host tests (`bytes_for_positive`, `object_size_positive`) are the runtime witnesses
of the new boundary contracts, joining `test_store` and the cas proptest/fuzz
oracles as differential guards of the now-proven code.

The ledger in §2 is the source of truth the remaining sub-phases cite: 9d's
`doc/guidelines/verus.md` trusted-seam section and 9e's `CLAUDE.md` "the trusted
base is exactly the `Store` seam" sentence both ground their claim here rather
than in a remembered summary. The independent spec-to-code conformance re-read is
9b (`doc/results/69`); the final certification run is 9f (`doc/results/70`).
