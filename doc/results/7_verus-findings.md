# 7 — virtio-blk `check_capacity` LBA bound under Verus (Task 7)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 7
(virtio-blk `check_capacity` — overflow-safe LBA range refusal; Tier 1, depends on
the Task 3 virtio-blk gate). Outcome: **verified, shipped, first attempt.**
`cargo verus verify -p virtio-blk` now reads **`3 verified, 0 errors`** (was 1).
No reverts; no new trusted seam (tally stays 14); the gated deps (cas 75,
freelist 29, dma-pool 0) are unchanged.

## What was attempted

Mechanize the defensive LBA bound (rev2§4.5) that `VirtioBlk::check_capacity`
(`virtio-blk/src/lib.rs`) applies before every transfer: refuse a request whose
last sector would run past the device's reported `capacity`, and do so with
`checked_add` so an adversarial `lba` near `u64::MAX` *refuses* rather than wraps.
Previously the arithmetic was plain Rust, guarded only by an integration test.

The pure arithmetic was extracted into a free `verus!{}` function with a ghost
predicate oracle:

```rust
pub open spec fn out_of_range(lba: u64, len: usize, capacity: u64) -> bool {
    lba + (len / SECTOR) > capacity
}

pub fn capacity_check(lba: u64, len: usize, capacity: u64) -> (r: Result<(), ()>)
    ensures r is Err <==> out_of_range(lba, len, capacity),
{
    let nsectors = (len / SECTOR) as u64;
    match lba.checked_add(nsectors) {
        None => Err(()),
        Some(end) => if end > capacity { Err(()) } else { Ok(()) },
    }
}
```

The single `ensures` iff captures all three plan obligations at once: totality (a
verified exec fn provably returns, no panic/overflow), `Ok` ⇒ `lba + len/SECTOR <=
capacity` (the `Ok` arm, by Result exhaustiveness), and `Err` exactly when
`checked_add` is `None` or `end > capacity` (the two collapse — an overflowing sum
already exceeds the `u64` `capacity`). The method now delegates and re-labels the
error, keeping the `VirtioError` enum (declared outside `verus!{}`) out of the
verified core:

```rust
fn check_capacity(&self, lba: u64, len: usize) -> Result<(), VirtioError> {
    capacity_check(lba, len, self.capacity).map_err(|()| VirtioError::OutOfRange)
}
```

`SECTOR` (= 512) was moved from a bare module const into the `verus!{}` block (see
finding 2).

## Result

`cargo clean && cargo verus verify -p virtio-blk` — full-session real run (results
line present; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`):

| crate | result |
|---|---|
| cas `--no-default-features` | 75 verified, 0 errors |
| freelist | 29 verified, 0 errors |
| dma-pool | 0 verified, 0 errors |
| **virtio-blk** | **3 verified, 0 errors** (re-verifies cas 75, freelist 29, dma-pool 0 in-session) |

The count rose 1 → 3: `SECTOR` (const, +1) and `capacity_check` (exec fn, +1). The
`out_of_range` `open spec fn` is transparent and carries no standalone obligation,
so it adds 0 (it does not appear in the `--time-expanded` breakdown).

Per-function `rlimit` (`scripts/verus-baseline.sh virtio-blk`, cold):

| obligation | rlimit (pre → post) |
|---|---|
| `avail_ring_slot` (control) | 14022 → 14098 (+76, +0.5%) |
| `capacity_check` (new) | — → 18831 |
| `SECTOR` (new) | — → 2 |

## Findings that mattered

1. **The proof discharged with zero hints.** No modulo lemma, no `as nat` cast in
   the spec, no width-mix workaround — the plan reserved all three as
   contingencies and none was hit. The `(len / SECTOR) as u64` cast is lossless and
   auto-discharged (Verus models `usize` ≤ 64 bits), and the `vstd` `checked_add`
   `Option` spec (`std_specs/num.rs`: `returns (if x+y > MAX { None } else
   { Some((x+y) as $uN) })`) drives the iff directly. This is the same
   `res is Err <==> <overflow>` over `match …checked_add` idiom as loader's
   Task-5 `page_layout` (`loader/src/elf.rs:125`), minus the alignment masks — so a
   strictly simpler proof, confirming the plan's "Risk: low" read.

2. **The divisor const must live inside `verus!{}`.** `len / SECTOR` is total —
   in the spec because Verus's spec division is defined at 0, in the *exec* because
   division by a possibly-zero divisor needs a non-zero proof — *only if the prover
   can see `SECTOR == 512`*. A bare module const outside the block is opaque to the
   prover, so `SECTOR` had to move inside (it stays a crate-root `pub const`, so
   exec callers and the `tests/` crates that use `virtio_blk::SECTOR` are
   unaffected). Same reason storage-server's rights bits live inside its block.

3. **`Result<(), ()>` + `map_err` keeps a non-verified error enum out of the core.**
   `VirtioError` is a rich enum declared outside `verus!{}`; threading it through
   the verified function would drag it (and its derives) into verify scope for no
   benefit. Returning the unit-error `Result<(), ()>` and mapping `()` →
   `VirtioError::OutOfRange` in the thin method is the clean boundary — the proof is
   about the *arithmetic*, the enum is the caller's vocabulary.

4. **A ghost `spec fn` cannot be the test oracle — that is what gives the proptest
   teeth.** `out_of_range` is ghost (erased; not callable from exec test code), so
   the companion proptests cross-check `capacity_check` against an *independent*
   `u128` oracle (`out_of_range_oracle` in `tests/ring_props.rs`) that shares none
   of the production `checked_add` arithmetic. A wrong refusal in either direction
   fails the test; a test that simply re-called the verified function would be a
   green-proof-of-nothing.

5. **An additive change has no byte-identical in-crate control.** §10's perf method
   wants a byte-identical control obligation, but adding sibling declarations
   (`SECTOR`/`out_of_range`/`capacity_check`) enlarges the module's SMT context, so
   `avail_ring_slot`'s `rlimit` shifts +0.5% (14022 → 14098) even though its source
   and obligations are untouched. That near-identical shift is the honest signal
   that the drift is pure context overhead, not a logic regression; the crate total
   rises only by the *new* obligations' cost (`capacity_check` 18831 + `SECTOR` 2),
   which is the cost of the new proof, not a regression of the old one.

## Reverted vs kept

Nothing reverted — the proof succeeded on the first attempt. Kept: the `out_of_range`
spec fn + `capacity_check` exec fn, the `SECTOR`-into-`verus!{}` move, the delegating
`check_capacity` method, the new companion tests (`capacity_check_matches_oracle` and
`capacity_check_high_lba_refuses` proptests, `capacity_check_boundaries_have_teeth`
unit test) in `tests/ring_props.rs`, and the bookkeeping (ledger Baseline row +
LBA-bound routing note, CLAUDE.md gate comment, Cargo.toml `metadata.verus` comment,
CI comment). The existing `lba_past_capacity_refused_locally` integration test is
unchanged — it now exercises the refactored method end-to-end through the driver.

## Proposed guideline additions (`doc/guidelines/verus.md`)

1. **Division by a named const: declare the const inside `verus!{}`.** The exec
   division's no-divide-by-zero obligation is discharged only when the prover sees
   the const's literal value; a bare module const outside the block is opaque. (The
   §6 "rights bits inside the block" note already covers `by (bit_vector)` literals;
   extend it to "any const a spec/exec arithmetic obligation depends on.")
2. **Wrap a non-verified error enum at the boundary, not inside the core.** A pure
   verified helper returning `Result<(), ()>` (or a `bool`) with the rich
   exec-only error mapped on in the thin caller keeps the enum and its derives out
   of verify scope. The proof states the arithmetic; the caller states the policy.
3. **A ghost `spec fn` is not a usable test oracle — and that is the point.** When a
   verified predicate has a companion proptest, compute the oracle *independently*
   (here a `u128` reimplementation) rather than reaching for the `spec fn`; the
   independence is what gives the proptest teeth against the proof.
4. **For a purely additive obligation there is no byte-identical in-crate control.**
   New sibling declarations shift every co-module function's `rlimit` by a small
   context overhead. Read the near-identical shift on an untouched function as
   confirmation of *no logic regression*, and judge the change by the crate total
   rising only by the new obligations' own cost.

## Trusted base

**Tally unchanged at 14.** `capacity_check` adds no `external_body`/
`assume_specification` — it cites only the `vstd` `checked_add` `Option` library
spec, not a project seam, and is otherwise pure `u64` modulo/`checked_add`
reasoning. virtio-blk was already gated (Task 3), so there is no new crate
onboarding: the CI line, `verus-baseline.sh` entry, and CLAUDE.md gate list already
include it (only their descriptive comments were broadened). Baseline row updated:
`-p virtio-blk` → 3 verified, 0 errors; a virtio-blk LBA-bound routing note records
that `capacity` stays a trusted MMIO read and the device-shared ring stays the
trusted DMA seam (rev2§2.5).
