# 3 — virtio-blk `avail_ring_slot` under Verus + crate onboarding (Task 3)

Date: 2026-06-25. Attempt against `doc/plans/0_verus-concurrency.md` Task 3
(virtqueue `avail_ring_slot` — pure ring index/wrap arithmetic; Tier 1, the
virtio-blk crate-onboarding pilot). Outcome: **verified, shipped**.
`cargo verus verify -p virtio-blk` is a new gate line reading **`1 verified, 0
errors`**. No reverts. One *necessary, expected* collateral change: the shared
`freelist` crate's two heavy merge proofs get larger `rlimit` ceilings (consumption
unchanged) so they still verify when re-checked under the `vstd[alloc]` prelude
this session pulls in — see "The onboarding finding" below.

## What was attempted

Bring `avail_ring_slot(idx, qsize)` (`virtio-blk/src/lib.rs`) under Verus and, in
doing so, onboard `virtio-blk` into the verification gate for the first time. The
function computes the byte offset of `avail.ring[idx % qsize]` within the avail
buffer; `new()` allocates that buffer as `6 + 2*qsize` bytes
(`pool.alloc(6 + 2 * n, 2)`, the virtio avail layout flags/idx/ring[n]/used_event).

Contract added (plain Verus, no helper lemmas):

```rust
pub fn avail_ring_slot(idx: u16, qsize: u16) -> (slot: usize)
    requires qsize > 0, qsize <= 8,
    ensures
        idx % qsize < qsize,
        slot == 4 + (idx % qsize) * 2,
        4 <= slot,
        slot + 2 <= 6 + 2 * qsize,
{ 4 + (idx % qsize) as usize * 2 }
```

The last two clauses are exactly the two assertions of the kept companion
proptest `avail_ring_slot_in_bounds` (`tests/ring_props.rs`), now mechanized ∀
`u16` idx and qsize `1..=8`.

## Result

`cargo clean && <full gate>` — every crate real-run (results line present;
prover `0.2026.06.07.cd03505` / toolchain `1.95.0`):

| crate | result |
|---|---|
| kcore | 404 verified, 0 errors |
| ipc | 68 verified, 0 errors |
| urt | 25 verified, 0 errors (freelist dep 29) |
| freelist | 29 verified, 0 errors (no-alloc gate) |
| dma-pool | 0 verified, 0 errors |
| cas `--no-default-features` | 75 verified, 0 errors |
| **virtio-blk** | **1 verified, 0 errors** (re-verifies cas 75, freelist 29, dma-pool 0 in-session) |

Notes that turned out to matter:

- **No modulo lemma needed.** The plan allowed reaching for
  `vstd::arithmetic::div_mod::lemma_mod_bound` if SMT stalled; it did not.
  `qsize > 0` discharges `idx % qsize < qsize`, the cast/`*2`/`+4` no-overflow,
  and the in-bounds bound automatically. Pure first-order, zero vstd axioms cited.
- **No `#[verifier::external]` needed.** Everything outside the new `verus!{}`
  block — the generic `VirtioBlk<M, B>` driver, the `Mmio` trait, the host fake
  device, the no_std `cas::dev::BlockDev` adapter — is externalized by default and
  compiles clean under `cargo-verus`. The onboarding-contingency the plan reserved
  (`#[verifier::external]` on a module that trips the frontend) was not hit.
- **`qsize > 0` stays a caller precondition.** `submit` (the only non-test call
  site) is external code, so the precondition is not checked there — correct, since
  `new()`'s `u32→u16 .min(8)` can truncate to 0 and `new()` is trusted MMIO
  bring-up. The proof is about the arithmetic, not the device-init path.

## The onboarding finding (the real content of this task)

The plan called crate onboarding "the highest-risk part," and the risk landed —
but not where expected (it expected a macro-dep / frontend problem; the frontend
was clean). **virtio-blk is the first gated crate that sits *above* other gated
crates in the dependency graph** (`virtio-blk → dma-pool → freelist`, and
`virtio-blk → cas`). Two consequences fell out:

1. **`cargo verus verify -p virtio-blk` re-verifies the transitive gated deps
   in-session.** Unlike a leaf crate, the run prints `Checking freelist … / cas …
   / dma-pool …` and re-checks their obligations (cas 75, freelist 29, dma-pool 0)
   before virtio-blk's own. (`-p dma-pool` *imports* freelist as pre-verified — "0
   verified" — because freelist is its *direct* dep already built under a matching
   config; for virtio-blk the deps are rebuilt, so they re-verify. `--exclude`
   cannot pare this down: it requires `--workspace`.)

2. **`cas` turns on `vstd`'s `alloc` feature for the whole session.**
   `cas/Cargo.toml` has `vstd = { …, features = ["alloc"] }`. Cargo feature
   unification is global per invocation: one consumer requesting `vstd[alloc]`
   turns it on for the *single* shared `vstd` build, including the no-alloc
   `freelist` re-verified alongside. Under the larger alloc prelude (vstd itself
   goes 1495→1533 verified items), freelist's two `spinoff_prover` merge proofs
   consume ~1.4–1.85× more Z3 resource and **blew their `rlimit` budgets**:

   ```
   freelist::free_both   no-alloc 31.4M → alloc 58.2M   (budget was rlimit(15))
   freelist::free_insert no-alloc 110.9M → alloc 154.7M (budget was rlimit(50))
   ```

   Isolating the cause: enabling `vstd[alloc]` on `freelist` *standalone*
   reproduces the exact `27 verified, 2 errors` on the same two functions — so it
   is the `alloc` feature, not the multi-crate session, not nondeterminism.

   **This is unavoidable.** `cas` is a non-optional dep of virtio-blk: the
   `blockdev` adapter (`virtio_blk::blockdev::VirtioBlockDev`) is used by **`storaged`
   in its no_std OS build** (`user/storaged/src/main.rs:31,223`,
   `virtio-blk = { default-features = false }`), so `blockdev`/`cas` cannot be made
   std-only or optional without breaking the OS build. Any virtio-blk verify build
   carries `vstd[alloc]`.

**Fix (minimal, sound):** raise the two budgets to cover the alloc context —
`free_insert` `rlimit(50)→rlimit(120)`, `free_both` `rlimit(15)→rlimit(40)`. An
`rlimit` is a *solver ceiling, not a cost*: re-measuring freelist's no-alloc gate
after the bump gives byte-identical consumption (`free_both` 31,387,180,
`free_insert` 110,940,697 — unchanged to the unit), so the `-p freelist` Baseline
totals do not move and no other crate regresses (urt re-checks freelist no-alloc;
still 29/0). The budgets now satisfy both the standalone no-alloc gate and the
alloc re-verification. The freelist Baseline row and a virtio-blk routing note
record why.

Alternatives considered and rejected: making `cas`/`blockdev` an optional
std-only feature (breaks the no_std `storaged` build); verifying `freelist` itself
under `vstd[alloc]` permanently (ripples into dma-pool/urt baselines for no gain).

## Reverted vs kept

Nothing reverted — the proof succeeded. Kept: the virtio-blk gate (Cargo.toml
`vstd` + `metadata.verus` + lints), the `verus!{}` contract on `avail_ring_slot`,
the freelist budget bump, and the bookkeeping (CI line, CLAUDE.md gate list,
`verus-baseline.sh` `ALL_CRATES`, ledger Baseline rows + routing note). The two
ring proptests are unchanged (the kept companion oracle tier).

## Proposed guideline additions (`doc/guidelines/verus.md`)

Onboarding a gated crate that sits **above** other gated crates is structurally
different from a leaf and deserves a note:

1. **`-p <upper>` re-verifies its transitive gated deps in-session** (a leaf
   imports them; an upper crate rebuilds and re-checks them). Budget the wall-clock
   accordingly, and expect the deps' counts to appear in the run.
2. **vstd feature unification is global per `cargo` invocation.** If the upper
   crate (or *any* crate in its graph — here `cas`) requests `vstd`'s `alloc`
   feature, every co-verified no-alloc dep is re-verified under the *larger alloc
   prelude*. `rlimit` is deterministic only for byte-identical SMT input, and the
   alloc prelude is not byte-identical to the no-alloc one — same proof, richer
   context, higher cost (~1.4–1.85× on freelist's heavy merge proofs here).
   **Size a shared crate's heavy `spinoff_prover` proofs' `rlimit` budgets to
   cover the alloc context, not just its own no-alloc gate.** Raising the ceiling
   does not change no-alloc consumption (verify with `--time-expanded`), so it is a
   regression-free hardening, not a perf change.
3. **Corollary to the plan's "verify the minimal feature set" recipe:** it only
   keeps `std`/test scaffolding out of scope. A dep that is part of the *no_std
   core* (here `cas`, via the `blockdev` adapter `storaged` links) is in the verify
   graph regardless, and brings its vstd features with it.

## Trusted base

**Tally unchanged at 14.** virtio-blk adds no `external_body`/`assume_specification`
— `avail_ring_slot` is pure `u16`/modulo reasoning citing no vstd axiom. The
freelist budget change is an `rlimit` ceiling, not a seam. New Baseline row:
`-p virtio-blk` → 1 verified, 0 errors; freelist row annotated with the
alloc-context budget rationale; CLAUDE.md gate list + CI `verus` job +
`verus-baseline.sh` updated to include virtio-blk.
