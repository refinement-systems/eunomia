# Miri test-suite performance — findings & optimization options

*Investigation date: 2026-06-22. No code changed; this is analysis only.*

## TL;DR

* The Miri sweep is dominated by **one crate, `cas`**, and within it by the
  **interpreted BLAKE3 hashing** that every store/tree/file test drives. The
  other Miri crates are noise by comparison (`dma-pool` ≈ 27 s, `urt` ≈ 40 s of
  interpretation).
* The whole sweep runs on **a single CPU core**. Miri is a single-threaded
  interpreter: inside one `cargo miri test` process, even libtest's
  `--test-threads=N` schedules the test threads cooperatively on **one** OS
  core. Measured directly — throughout a multi-test run there is exactly **one
  `miri` process pegged at ~95 % CPU** while the machine's other 7 logical cores
  (4 performance + 4 efficiency) sit idle.
* **A single test, `store::tests::size_pressure_holds_total_below_high_watermark`,
  is ~12.6 minutes by itself** — because it (and two siblings) lacks the
  `cfg(miri)` op-stream cap that the other store proptests already use. Fixing
  that is a few lines and is the prerequisite for parallelism to help (a single
  test = a single process = a single core, so it caps the achievable wall-clock).
* Three levers, in order of payoff-to-effort:
  1. **Cap the three uncapped proptests** (tiny code change) — removes the
     ~12.6-min long pole. Do this first.
  2. **Parallelism across processes** (no code change) — shard the `cas` run
     into N concurrent `cargo miri test` processes (or use `cargo miri nextest
     run -jN`). On 4 performance cores this is ~3–4× on the bottleneck — but only
     after lever 1, because it cannot beat the slowest single test.
  3. **Cut the interpreted-BLAKE3 cost** (small code change behind the existing
     `cas/src/hash.rs` seam) — a `#[cfg(miri)]` cheap 256-bit hash instead of
     `blake3::hash`. Miri's job here is UB detection in the *surrounding* Rust,
     not validating BLAKE3's bytes; this makes *every* hashing test cheap.
* Combined, a 50-minute sweep should drop to **a few minutes**.

The repo's documentation is stale: `CLAUDE.md` says the `cas` sweep is "~25
min". It is now > 50 min, because B12/B13 added more `cas` proptests and
deterministic store/snapshot/gc tests — every one of them hashes.

---

## 1. What actually runs under Miri

Miri is **not** in CI (`.github/workflows/ci.yml` only mentions it in a
comment). It is a **local, manual** sweep, documented in `CLAUDE.md` →
"Run with Miri". The documented invocations:

```
cargo +nightly miri test -p cas         # the heavy one
cargo +nightly miri test -p dma-pool    # fast
cargo +nightly miri test -p urt         # fast
# quick UB pass (regressions + every committed fuzz seed):
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
  -p cas -p loader -p storage-server --test fuzz_regressions --test fuzz_corpus
```

Run sequentially, the per-crate totals add up to the >50 min the user reports.

### `cas` test inventory (the bottleneck)

`cargo +nightly miri test -p cas` builds and runs **125 tests** across three
binaries (lib + `fuzz_corpus` + `fuzz_regressions`; no doctests). By module:

| module           | tests | hashes? |
|------------------|------:|---------|
| `store::tests`   |  50   | **yes** — format/write/mount/snapshot/gc all hash |
| `prolly::tests`  |  25   | **yes** — every tree node is BLAKE3-addressed |
| `chunk::tests`   |   8   | no (content-defined chunking, no hashing) |
| `file::tests`    |   6   | **yes** — chunk + hash |
| `disk::tests`    |   5   | mostly byte-codec, light |
| `gc::tests`      |   4   | **yes** — builds + marks trees |
| `tree::tests`    |   3   | **yes** |
| `overlay::tests` |   3   | no |
| `hash::tests`    |   2   | yes (tiny inputs) |
| integration      |  ~16  | corpus/regression replays — deliberately cheap under Miri |

Of these, **15 `proptest!` blocks (≈19 property-test fns)** in `cas/src`. Under
`cfg(miri)` each is capped to **4 cases** (some to 1), down from 256–1024 native
— this reduction already exists. But 4 cases of a store proptest still means 4×
(format a store → write a stream of ops → mount/snapshot/gc), and each of those
operations hashes many chunks/nodes with interpreted BLAKE3.

### The single cost chokepoint

`cas/src/hash.rs`:

```rust
pub fn of(data: &[u8]) -> Self {
    Hash(*blake3::hash(data).as_bytes())
}
```

`blake3` is pulled with `default-features = false, features = ["pure"]`
(`cas/Cargo.toml`) — a pure-Rust, no-SIMD build. Native that's fine; **under
Miri it is interpreted instruction-by-instruction**, with no SIMD to fall back
on, so it is *the* dominant cost of every test that touches the store, tree, or
file layers. The code comments already acknowledge this ("blake3 is interpreted
hashing", "whole-image hashing under Miri is too slow").

### Mitigations already in place (so the report is honest about them)

* proptest `cases` dropped to 4 (or 1) under `cfg(miri)` — 14 sites.
* op-stream / device-size / entry-count caps under `cfg(miri)` (e.g.
  `store.rs` `max_ops = 16`, `dev_bytes = 2<<20`; `prolly.rs` entry counts
  64 vs 320/1024).
* whole-image hashing **skipped** under Miri: `cas/tests/fuzz_corpus.rs::mount_recovery`
  returns early; `storage-server/tests/fuzz_corpus.rs::request_dispatch` decodes
  but skips the hashing dispatch; a few tests carry `#[cfg_attr(miri, ignore)]`.
* corpus replays under Miri are decode-only (the expensive path is gated off),
  so the 588 storage-server / 103 loader / 41 cas corpus files are **not** the
  bottleneck.

The remaining cost is intrinsic: the store/tree/file **lib** proptests and
deterministic unit tests must hash to exercise their logic, and they hash a lot.

---

## 2. Measurements (this machine: 8 logical cores, 4 performance; miri 2026-06-09)

Whole-crate runs (warm build cache):

| run | tests | Miri execution time |
|-----|------:|---------------------|
| `cargo miri test -p dma-pool` | 18 | **≈ 27 s** |
| `cargo miri test -p urt`      | 22 | **≈ 40 s** |

The fast crates confirm the thesis: no hashing → tens of seconds.

### Per-test cost of the heavy `cas` proptests

Eight of the heaviest `cas` tests, each run as its **own** `cargo miri test`
process (so each gets a core), launched **concurrently** (8 procs over 4
performance + 4 efficiency cores, so these are *upper bounds* — under core
contention — not dedicated-core figures):

| test | wall-clock | notes |
|------|----------:|-------|
| `store::tests::size_pressure_holds_total_below_high_watermark` | **757 s (12.6 min)** | **uncapped op stream** — see below |
| `file::tests::file_roundtrip` | 199 s | chunk + hash a ≤16 KiB file |
| `store::tests::crash_recovery_survives_wal_wrap` | 188 s | op cap 12 |
| `store::tests::wal_ring_invariants_hold_across_random_wraps` | 142 s | op cap 16 |
| `store::tests::crash_recovery_survives_size_pressure_flush` | 138 s | |
| `prolly::tests::build_level_fires_multi_level_and_roundtrips` | 83 s | multi-level tree |
| `prolly::tests::roundtrip` | 81 s | 64-entry tree round-trip |
| `prolly::tests::canonical_form_deep` | 5 s | already `#[cfg_attr(miri, ignore)]` (effectively free) |

Two headline facts:

* **One test dominates everything.** `size_pressure_holds_total_below_high_watermark`
  ran **>12 minutes** — and it kept the only running core to itself for most of
  that, so 757 s is close to its true single-core cost. The other seven combined
  are ~830 s of (contended) time. A single test is responsible for a large slice
  of the whole `cas` sweep.

* **The cause is a missing one-line cap.** Unlike its siblings, this proptest
  iterates the **full** op stream under Miri:

  ```rust
  // store.rs ~3852: ops in vec((0usize..4, 1usize..600), 1..120)
  for (i, (ri, len)) in ops.iter().enumerate() {          // NO .take(max_ops)
      let data = vec![0x5Au8; *len];                       // up to 600 bytes
      store.write(...).unwrap();                           // chunk + BLAKE3-hash
      ...
  }
  ```

  So Miri interprets up to **120 writes × up to 600 bytes, hashed, × 4 cases**.
  Its siblings `wal_ring_invariants_hold_across_random_wraps` (cap **16**) and
  `crash_recovery_survives_wal_wrap` (cap **12**) already use
  `let max_ops = if cfg!(miri) { N } else { ops.len() };`. **Two other proptests
  share the same omission:** `per_ref_overlay_never_exceeds_soft_bound`
  (store.rs ~3095, up to 80 × 400 B) and `staleness_sweep_leaves_no_overdue_ref`
  (store.rs ~5533, up to 40 × 600 B).

### Single-core, confirmed directly

Throughout a multi-test `cargo miri test` run there was **exactly one `miri`
process pegged at ~95 % CPU**, despite `--test-threads=4`. Miri is a
single-threaded interpreter; libtest's test threads are interpreted cooperatively
on one OS core, so the whole sweep uses 1 of 8 cores. The earlier serial sample
(8 tests, one process) was killed at **23 min without finishing** — entirely
consistent with the per-test costs above summing on a single core.

### The catch: the long pole gates the parallelism win

Process-level parallelism (§3 Option 1) cannot get the `cas` wall-clock below the
**slowest single test**, because one test = one process = one core and cannot be
split further (Amdahl's law). With `size_pressure` at ~12.6 min, **no amount of
sharding gets `cas` under ~12 min** until that test is capped. Hence the revised
ordering in §4: **cap the uncapped proptests first**, *then* parallelize.

---

## 3. Optimization options

Four options below. The recommended order is in §4 (it is **not** the numeric
order — Option 4, capping the long pole, comes first because it gates the others;
Options 1 and 2 then stack on top).

### Option 1 — Parallelize across processes (no code change). Biggest no-risk win.

Miri can't use multiple cores *within* a process, but each `cargo miri test`
**process** gets its own core. Two ways:

* **1a. Manual sharding of `cas`.** Split the `cas` tests into N filtered
  invocations run concurrently — e.g. by test-name prefix
  (`store::tests::a`–`l`, `store::tests::m`–`z`, `prolly::`, everything-else) —
  and launch them as background jobs, one per performance core. With 4 cores
  this is ~3–4× on the bottleneck. Zero new dependencies; just a shell wrapper.
  Caveat: shards must be roughly balanced — after Option 4 caps `size_pressure`,
  the next long poles are `file::file_roundtrip` (~199 s) and
  `store::crash_recovery_survives_wal_wrap` (~188 s); keep those in separate
  shards. (Per-test costs are in §2.)

* **1b. `cargo miri nextest run -jN`.** nextest runs **one process per test**
  with `-j`-way parallelism, which under Miri means real N-core scaling for
  free, no manual sharding. Requires installing `cargo-nextest` (not currently
  installed). Caveats: nextest does not run doctests (cas has none, so this is
  moot here) and miri+nextest wants recent versions of both (we're on a
  2026-06 nightly, fine).

* **1c. Run the crates concurrently.** Even just launching the `cas`,
  `dma-pool`, `urt`, `loader`, `storage-server` invocations in parallel instead
  of sequentially reclaims the ~1–2 min tail of the fast crates. Small, but
  free.

**Recommended:** 1b if you're willing to add `cargo-nextest`; otherwise 1a.
Either turns the `cas` wall-clock from "sum of all tests" into "sum of the
slowest shard".

### Option 2 — Remove interpreted BLAKE3 under Miri (small code change). Biggest per-process win.

Behind the existing single chokepoint `cas/src/hash.rs::Hash::of`, add a
`#[cfg(miri)]` path that uses a **cheap, non-cryptographic 256-bit hash** (a
simple multiply-xor / FxHash-style mix over the input) instead of `blake3::hash`:

```rust
pub fn of(data: &[u8]) -> Self {
    #[cfg(miri)]
    { Hash(cheap_256bit_mix(data)) }   // UB-checking doesn't need real BLAKE3
    #[cfg(not(miri))]
    { Hash(*blake3::hash(data).as_bytes()) }
}
```

Rationale: Miri exists here to find **UB in the surrounding Rust** (aliasing,
OOB, overflow, uninit). It does *not* need BLAKE3's exact output — that is pinned
natively by `hash::tests::matches_blake3_reference` and by the whole native test
suite. A cheap hash preserves the only property the store logic relies on under
test (distinct inputs → distinct content addresses, with overwhelming
probability), while deleting the cost that dominates every store/tree/file test.

* Affects **only the Miri build** — native, release, the aarch64 cross-build,
  and `cargo verus verify` are all `cfg(not(miri))` and unchanged. BLAKE3 stays
  the real seam everywhere that matters; `disk.rs`/`store.rs` already model it as
  an opaque/`external_body` trusted seam in the Verus proofs, so semantics there
  are unaffected.
* Required follow-ups: gate `hash::tests::matches_blake3_reference` with
  `#[cfg_attr(miri, ignore)]` (its hard-coded vector won't match the cheap hash),
  and sanity-check that no test asserts a *specific* BLAKE3 value (none seen —
  the proptests assert structural/round-trip invariants, not literal hashes).
* Residual risk: a hash collision could change store behavior, but a decent
  256-bit mix makes that astronomically unlikely over the tiny Miri inputs; and
  collisions, if they ever surfaced, would be a Miri-only test artifact, not a
  product bug.

This is the single biggest lever for the wall-clock *inside* one process, and it
stacks with the sharding in Option 1.

### Option 3 — MIRIFLAGS tuning (no code change). Modest, measure before adopting.

* `MIRIFLAGS=-Zmiri-tree-borrows` — Tree Borrows is often faster than the default
  Stacked Borrows on pointer-heavy code (and is the direction Miri is heading).
  Worth A/B-timing; it may speed up the store code's `Vec`/slice churn.
* `-Zmiri-disable-stacked-borrows` / `-Zmiri-disable-validation` — large speedups
  **but they remove the aliasing/validity checks that are a primary reason to run
  Miri at all.** Only appropriate as a separate, clearly-labelled "fast smoke"
  tier, never as the certifying run. Document, don't default.
* Keep `-Zmiri-disable-isolation` scoped to the runs that need it (already the
  case); it's not itself a cost driver here.

### Option 4 — Cap the three uncapped proptests (tiny code change). DO THIS FIRST — it removes the long pole.

Promoted to the top of the list by the measurements: three store proptests omit
the `cfg(miri)` op-stream cap their siblings already use, and one of them
(`size_pressure_holds_total_below_high_watermark`) is **a >12-minute single
test** — the Amdahl ceiling that caps how far Option 1's parallelism can help.

Apply the established pattern (copied verbatim from
`wal_ring_invariants_hold_across_random_wraps` / `crash_recovery_survives_wal_wrap`):

```rust
let max_ops = if cfg!(miri) { 16 } else { ops.len() };
for ... in ops.iter().take(max_ops) ... { ... }
```

to:

* `store::tests::size_pressure_holds_total_below_high_watermark` (store.rs ~3852) — the long pole;
* `store::tests::per_ref_overlay_never_exceeds_soft_bound` (store.rs ~3095);
* `store::tests::staleness_sweep_leaves_no_overdue_ref` (store.rs ~5533).

This is the **highest payoff-to-risk** change in the document: a few lines, no
loss of UB coverage (the deterministic store unit tests already exercise the same
size-pressure / staleness / per-ref code paths under Miri; the proptest only
needs *enough* ops under Miri to drive the watermark transition, which a cap of
~16 still does), and it likely turns a ~13-minute test into a ~20–30-second one
like its capped siblings. Until it lands, Option 1's sharding cannot get `cas`
under ~12 min.

Optionally, the same pass can drop `cfg(miri)` `cases` from 4 → 2 on the heaviest
proptests, but that is secondary to capping the op streams (capping cuts the work
*per case*, which is where the cost is).

### Compile time (orthogonal)

Miri builds its own sysroot (`build-std`) the first time and caches it under
`target/miri/`. Don't `cargo clean` between runs; the recurring cost is
interpretation, not compilation (the test binaries rebuilt in ~16 s here from
warm cache).

---

## 4. Recommended plan

1. **First, cheapest, biggest single jump (Option 4):** add the `cfg(miri)`
   op-cap to the three uncapped proptests (`size_pressure_holds_total_below_high_watermark`,
   `per_ref_overlay_never_exceeds_soft_bound`, `staleness_sweep_leaves_no_overdue_ref`).
   A few lines, no coverage loss. This removes the ~12.6-min long pole — on its
   own it should knock a large chunk off the sweep, and it is the prerequisite
   for parallelism to pay off.
2. **Then parallelize (Option 1):** shard `cas` across the 4 performance cores —
   1a (shell wrapper, ~4 concurrent filtered `cargo miri test` runs) or 1b (add
   `cargo-nextest`, `cargo miri nextest run -j4`) — and launch the fast crates
   concurrently (1c). With the long pole gone, sharding now actually scales:
   expected **→ ~6–10 min**, no further source changes.
3. **Then, the across-the-board per-process lever (Option 2):** add the
   `#[cfg(miri)]` cheap hash behind `hash.rs`, with the `matches_blake3_reference`
   ignore. This makes *every* hashing test cheap (not just the capped ones).
   Combined with 1–2, target **a few minutes** for the whole sweep.
4. Optionally A/B `-Zmiri-tree-borrows` (Option 3) and record whichever is
   faster as the default `MIRIFLAGS`.
5. Update `CLAUDE.md`: replace the stale "~25 min" figure, document the
   sharded/nextest invocation as the canonical way to run the Miri sweep, and
   note the single-core fact so future contributors don't expect `--test-threads`
   to help.

## Appendix — how this was measured

* `cargo +nightly miri test -p {dma-pool,urt}` timed end-to-end (warm cache):
  27 s / 40 s.
* Per-test costs: each heavy `cas` test run as its own
  `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --lib
  <test>` process, eight launched concurrently and individually wall-clock-timed
  (so the figures are upper bounds under core contention). Numbers in §2.
* Single-core confirmed via `ps` (exactly one `miri` proc at ~95 % CPU) during a
  `--test-threads=4` run; the serial 8-test sample was killed at 23 min unfinished.
* Inventory via `cargo miri test -p cas -- --list` (125 tests) and `grep` over
  `cas/src` for `proptest!` / `cfg!(miri)` / `.take(max_ops)` sites.
* Machine: macOS, 8 logical CPUs (4 performance + 4 efficiency), miri 0.1.0
  (2026-06-09 nightly).

---

## 5. Implementation outcome (2026-06-22)

The analysis above is no longer "analysis only" — Options 4 and 1 were
implemented, plus an extension. Two corrections to the analysis surfaced during
implementation; both are recorded here so the document stays honest.

### What landed

* **Option 4 (cap the 3 named proptests):** `size_pressure_holds_total_below_high_watermark`,
  `per_ref_overlay_never_exceeds_soft_bound`, `staleness_sweep_leaves_no_overdue_ref`
  in `cas/src/store.rs` got the `let max_ops = if cfg!(miri) { 16 } else { ops.len() }`
  + `.take(max_ops)` cap. The three together drop from ~12.6 min (`size_pressure`
  alone) to **80 s** total under Miri; native still runs full case counts.
* **Option 1 (parallelize):** chose **1b — cargo-nextest**
  (`cargo install cargo-nextest --locked`). `cargo +nightly miri nextest run -p
  cas -j4` runs one process per test (nextest auto-selects its `default-miri`
  profile) → genuine multi-core (user 2683 s over 765 s wall ≈ 3.5 cores).
  CLAUDE.md now documents this as the canonical sweep.
* **Extension — 6 additional poles** (see correction 2) capped the same way.

### Correction 1 — the bare `cargo miri test -p cas` line was broken

`cargo +nightly miri test -p cas` **with isolation enabled fails immediately**
under proptest 1.11: its failure-persistence path calls `std::env::current_dir()`
(`getcwd`), which Miri's isolation refuses. Every cas proptest aborts. The
canonical invocation **requires `MIRIFLAGS=-Zmiri-disable-isolation`** (which the
§Appendix per-test runs already used, and which every other crate's documented
line already passed — `cas` was the lone omission). Fixed in CLAUDE.md. Note
isolation governs only host-resource access, not the aliasing/validity checks
that are Miri's point, so coverage is unaffected.

### Correction 2 — `size_pressure` was not the only long pole

§2's per-test table sampled 8 tests and concluded `size_pressure` (757 s) was
*the* Amdahl ceiling. The first full `nextest` sweep (which runs **every** test)
showed it was not — six heavier tests the sample missed dominated:

| test | before | type |
|------|-------:|------|
| `store::ring_wrap_front_pinner_reclaim_and_remount` | 505 s | `#[test]` |
| `gc::check_recipe_handles_recipes` | 480 s | `#[test]` |
| `file::neighborhood_matches_whole_file` | 280 s | proptest |
| `store::gc_reclaims_superseded_roots_and_reuses_space` | 225 s | `#[test]` |
| `gc::mark_set_sufficient_over_random_trees` | 173 s | proptest |
| `file::file_roundtrip` | 165 s | proptest |

All six were capped under `cfg(miri)` with the established knobs (WAL 16→2 KiB;
churn 10→4 iters × 20→4 KB; sweep `0..=255`→`0..=31` + deep chain 300→48 — note
`step_by(16)` would alias the `b % 6` opcode to {0,2,4} and was *not* used;
max file 16→4 KiB; entries 16→6, content 2→0.5 KiB). Native keeps full ranges.

### Measured result

| stage | cas wall-clock (nextest -j4) |
|-------|------------------------------|
| serial single-core (sum of all) | ~69 min |
| after Option 4 only | ~21 min |
| after all 9 caps | **~12 min** (765 s measured, then `check_recipe` trimmed 185→45 s) |

All **122 cas tests pass** (3 `#[cfg_attr(miri, ignore)]`). dma-pool 11.9 s, urt
17.1 s under nextest. No Verus impact (all edits are `#[cfg(test)]`/`#[cfg(miri)]`).

The TL;DR's "a few minutes" is **not** reached: after the caps the sweep is
**throughput-bound**, not pole-gated (longest test 185→~45 s ≪ the 765 s wall),
across a flat tier of ~100–180 s tests the caps didn't touch — the crash-recovery
family, chunk-boundary proptests (`chunk::shared_suffix_boundaries_agree...`
155 s), `store::per_ref_soft_bound...` (138 s), and the `gc_mark` corpus replay
(137 s, *not* the decode-only cheap path §1 assumed). Getting to a few minutes
means capping that tier too, or adopting Option 2 (the `#[cfg(miri)]` cheap hash)
which cuts every hashing test at once — neither was in this change's scope.
