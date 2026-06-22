# B14B findings — reactor dispatch + cap-marshalling proptest tier (+ Verus allocator proof)

**Phase:** B14B of `doc/plans/15_b14-detail.md` (IPC reactor verification + TLA completion), the
must-do, independent Rust-testing sub-phase. Closes audit **§4.2 [low]**
(`doc/results/0_audit_rev0.md:474-477` — "the reactor's sequential dispatch (bit allocation, the
pending drain, the lowest-bit scan) and the endpoint cap-marshalling are Loom/Shuttle +
unit-test only"). Resolves the plan's **Design decision 4** (the verification-grade proptest
floor) **and lands its recorded Verus stretch**.

B14B is **purely additive** — it adds tests and one verified pure function, and changes no
runtime behaviour, no wire op, no on-disk byte, and no public type. The *concurrent*
wakeup/backpressure protocol stays TLA-modeled (B14A) and Loom/Shuttle-tested (`ipc/src/model.rs`
harnesses); B14B covers the **single-threaded state-machine logic** those tiers leave to the
rev1§6 baseline (proptest + Miri).

## The bar met

Both bars: the **proptest floor shipped** *and* the **Verus stretch landed**. `cargo verus
verify -p ipc` rose from **58/0 to 62/0** — the pure `used`-mask allocator core is now verified —
with **no new trusted seam** (the trusted-seam tally stays **14**). This is the strongest of the
DD4 outcomes (the B11/B13 "state which bar is met" discipline: here, the higher one).

## What landed

### Proptest floor (rev1§6 baseline tier) — 6 new property tests

Placed in `#[cfg(all(test, not(loom), not(shuttle)))]` sibling modules so they reach the private
allocator/marshalling internals and stay off the loom/shuttle model builds (the
`urt/src/time.rs` idiom). All under the workspace Miri config block
(`cases: if cfg!(miri) { 4 } else { 256 }` + the `failure_persistence` Miri guard). `ipc/` is
atomics-free (audit §4.3), so the dispatch is proptest-routed, not a new concurrency harness.

- **`reactor.rs` `mod proptests`** (driven over `ModelTransport`):
  - `alloc_bit_is_lowest_clear` — over an arbitrary `used: u64`, `alloc_bit` returns the **lowest
    clear** bit (characterized, not compared to its own impl: the bit was clear, every lower bit
    was set, exactly that bit flips) and refuses (`None`) **only** when the word is full.
  - `register_sequence_keeps_used_coherent` — over an arbitrary `register`/`register_bound`
    sequence (0..80 ops), `used` stays equal to the union of all claimed bits (the
    bitmap-coherence invariant — a bijection onto distinct bits, no double-allocation),
    `slots[bit].is_some()` iff the bit is used, every `register` takes the lowest clear bit, a
    `register_bound` of an already-used mask is `Taken` (leaving `used` untouched), and a
    `register` past the ceiling is `Full`.
  - `pending_drain_is_lowest_first` — over an arbitrary signaled mask, `wait` yields exactly the
    *registered* set bits, each once, in `trailing_zeros` (lowest-first) order, mapping each to
    its `(key, signals)`; signaled-but-**unregistered** bits are silently skipped, never returned
    and never blocking (the `M ⊆ S`, `U ⊆ !S` disjoint-subset construction keeps `wait` from
    sleeping). This is the epoll-shaped O(1) dispatch (rev1§3.6).
  - `alloc_exhausts_at_word_bits` (deterministic, `not(miri)`) — exactly 64 `register`s succeed on
    distinct bits, the 65th refuses `Full`, no alias, no panic (the proptest reaches this
    probabilistically; this pins the boundary).
- **`endpoint.rs` `mod tests`**:
  - `cap_slots_round_trips` — over arbitrary `[Option<u32>; 4]` (slot indices `< SLOT_NONE`),
    all-empty ⇒ `None` (skip cap handling), else `Some(slots)` with
    `slots[i] == caps[i].unwrap_or(SLOT_NONE)` that decodes back to `caps` exactly; `cap_slots` is
    a pure function of `caps`.
  - `cap_present_mask_round_trips` — end-to-end through `Endpoint` + `ModelTransport`: after a
    send/recv, the receiver's `caps[i].is_some()` matches `sent[i].is_some()` and `None` lands
    where no cap arrived — null-slot tolerance, never a panic (rev1§3.4).

### Verus stretch — the verified `used`-mask allocator core (`reactor.rs`)

Extracted the pure core `lowest_clear_bit(used: u64) -> Option<u32>` (the
`(!used).trailing_zeros()` body) into a `verus!{}` block; `alloc_bit` (plain Rust, the
trusted-shell over the slot array) calls it and records the allocation (`used |= 1 << bit`). The
contract proven:

```
r is None        ==> used == u64::MAX                       // refuses only when full
r matches Some(bit) ==> bit < 64                            // structural bound
                     && used & (1u64 << bit) == 0           // the bit was clear — no double-alloc
                     && forall j < bit: used & (1u64 << j) != 0   // it is the *lowest* clear bit
```

The proof mirrors the B8C kcore ready-queue bitmap (`kcore/src/ready.rs`'s `leading_zeros`
bit-scan), swapping u32/leading-zeros/highest-set for u64/trailing-zeros/lowest-clear. It rests on
`vstd::std_specs::bits::axiom_u64_trailing_zeros` (the `i == 0 <==> tz == 64`, the bit-at-`tz`-set,
and the lower-bits-clear facts) plus `by(bit_vector)` to bridge the `(!used >> k) & 1` form the
axiom speaks to the `used & (1 << k)` form the allocator's `used |= 1 << bit` speaks. It verified
on the first run (with only auto-trigger *notes*, since silenced by an explicit
`#![trigger used & (1u64 << j)]`).

## Key findings

1. **The stretch extracted cleanly** — the allocator's pure core is exactly one `u64 →
   Option<u32>` function with no slot-array or `Transport` entanglement, so it lifted into Verus
   without dragging the trusted shell in (the DD4 precondition). `alloc_bit`'s only remaining
   plain-Rust work is the side effect (`used |= 1 << bit`) the verified core's contract justifies.
2. **No new trusted seam.** The proof adds no `external_body`/`assume_specification` to the
   project; `u64::trailing_zeros`'s spec is **vstd's** `assume_specification` (the trusted verified
   library), exactly as kcore's verified `leading_zeros` scan already relies on
   `axiom_u32_leading_zeros`. Tally stays 14.
3. **The proptest captures the same lowest-clear / no-double-alloc property the Verus contract
   proves** — `register_sequence_keeps_used_coherent` exercises it over multi-step register
   sequences (where the Verus contract is per-call), and the two agree. The proptest also covers
   what Verus does not: the `pending` drain ordering, the multi-source `slots ↔ used` coherence
   across a sequence, and the cap-marshalling round-trip (Verus stays on the pure bitmask core, the
   B6/B7 trusted-shell posture).
4. **The existing harnesses are unperturbed.** The Loom (12 tests) and Shuttle (17 tests)
   harnesses still build + pass — the new proptest modules are correctly excluded by the
   `not(loom)/not(shuttle)` gate, and the `proptest` dev-dep compiles harmlessly under both cfgs.
   `alloc_bit`'s refactor to call `lowest_clear_bit` is behaviour-identical (all prior tests pass).

## Verification

| Check | Result |
|---|---|
| `cargo test -p ipc` | **23 passed, 0 failed** (17 prior + 6 new proptests/unit) |
| `cargo verus verify -p ipc` | **62 verified, 0 errors** (was 58/0; the verified allocator core; **no new seam**, tally 14) |
| `cargo build -p ipc` (normal, `verus!` erases) | clean |
| `RUSTFLAGS="--cfg loom" cargo test -p ipc` | **12 passed** (new modules excluded) |
| `RUSTFLAGS="--cfg shuttle" cargo test -p ipc` | **17 passed** (new modules excluded) |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p ipc` | **22 passed, 0 failed**, no UB (19s; the `not(miri)` boundary test skipped, proptests at 4 cases) |
| `… cargo +nightly miri test -p ipc --features fuzzing --test fuzz_corpus` | **1 passed** — the `wire_decode` corpus replays unchanged (wire-stable) |
| `cargo fmt` | clean; rustfmt leaves the `verus!{}` block intact |

## Numbers for the B14C ledger update (recorded here; B14B does not edit the ledger)

When B14C reconciles `doc/guidelines/verus_trusted-base.md`:
- the `cargo verus verify -p ipc` Baselines figure rises **58/0 → 62/0**; record that the reactor's
  **`used`-mask allocator algorithm** (`lowest_clear_bit`: lowest-clear-bit correctness,
  no-double-allocation, the 64-bit structural bound) is now Verus-verified, with **no new trusted
  seam** (a pure bitmask over `axiom_u64_trailing_zeros`, the kcore-ready-queue-bitmap pattern) —
  tally stays **14**;
- the reactor's **multi-source dispatch** beyond that pure core (the `pending` drain, the
  lowest-bit scan over the slot array) and the endpoint **cap-marshalling** are **proptest-routed**
  (the 6 tests above), not TLA-mechanized — the TLA model is single-source by design
  (`IpcReactor.tla` scope note). This is the test-routed note (the GC-sufficiency-note style) B14C
  records so a reviewer does not read the TLA completion *or* the allocator proof as covering the
  full multi-source dispatch.

## Out of scope (recorded so it is not mistaken for a gap)

- **Verus over the full reactor (registration + dispatch + `Transport` I/O).** The slot array and
  `Transport` trait-object calls stay plain Rust (the rev1§6.1 trusted-shell-over-verified-cores
  posture, B6/B7); only the pure `used`-mask bit-scan was an SMT-tractable candidate, and that is
  what landed.
- **A new Loom/Shuttle harness for the dispatch.** The dispatch is sequential and `ipc/` is
  atomics-free (audit §4.3); the concurrent protocol is already Loom/Shuttle-tested and
  TLA-modeled. B14B's tier is proptest.
- **Wire/on-disk format or corpus change.** None — B14B is wire-stable; the cap-marshalling
  proptest *tightens what the mapping is checked against*, it changes no bytes, and the
  `wire_decode` fuzz corpus replays unchanged.
- **The §4.3 doc over-claims + the ledger reconciliation.** The urt Loom-Relaxed docstring fix,
  the Loom-vs-Shuttle note for the atomics-free IPC crate, and the Baselines-row update (folding
  the numbers above) are **B14C's** scope.
