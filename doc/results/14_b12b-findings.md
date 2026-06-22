# B12B findings — size-pressure low/high watermarks + flush-the-biggest-offenders

**Phase:** B12B (`doc/plans/13_b12-detail.md`), the second sub-phase of B12, which conforms the
`cas` flush/memtable policy to rev1§4.4's *mandatory* triggers and bounds (Open Decision 1 /
Phase A2 resolved **mandatory**). B12A laid the per-ref-soft-bound-under-a-global-budget shape
and the per-ref accounting substrate; B12B closes the **size-pressure** trigger (rev1§4.4
trigger 3):

- **M-4 — size pressure flushed everything.** The MVP collapsed size pressure to a single hard
  threshold (`total > overlay_budget → sync_all()` — flush every ref + commit), "the simplest
  correct policy". B12B replaces it with the spec's two-watermark, selective policy: start
  flushing the **biggest offenders** at a **low watermark**, leaving small refs dirty, so writers
  rarely reach the high watermark (`global_budget`).

It is **format-stable** and adds **no Verus** (cas gate held at **65/0**). Unlike B12A it makes
**no `StoreOptions` API change** (no new fields, no renames), so it has **no caller blast
radius** — `mkfs`, `storaged`, `storage-server`, `virtio-blk` are untouched.

**Decisions exercised (from the plan):**
- **Design decision 4 — backpressure is a synchronous blocking flush, not a `FULL` return.**
  Crossing the low watermark flushes the biggest offenders inline (overlay → committed tree);
  the write proceeds once pressure is relieved. No eviction (overlay leaves memory only by
  becoming tree), no `FULL` protocol surface a single-threaded server can never need (the async
  `FULL` reply is recorded future work).
- **Design decision 2 — proptest + crash-injection + Miri, no new Verus chokepoint.** The size
  policy is a scheduler over the already-verified codec/recovery substrate (B5/B7, untouched).

## What landed (all in `cas/src/store.rs`)

- **`relieve_size_pressure(&mut self)`** replaces the `total > global_budget → sync_all()` block
  in `log_then_apply`. When total dirty overlay bytes cross `size_low_watermark`, it sorts the
  dirty refs by `Overlay::bytes()` **descending** (ties by ref name, for determinism) and
  `flush_ref`s them — biggest first — until total is back at or below the low watermark **or only
  one ref remains**; then a **single `commit`** folds in every flush this round (which rides the
  Verus-verified `advance_head` partial-head-advance). Small refs stay dirty — it is never
  `sync_all`.
- **`test_opts` watermark fix.** The shared test fixture set `global_budget: 32 KiB` but inherited
  `size_low_watermark = 8 MiB` from `Default`, leaving low > high. B12B pins
  `size_low_watermark: 32 KiB` so the fixture honors the rev1§4.4 invariant
  `size_low_watermark <= global_budget` and preserves the pre-B12 size trigger threshold (now
  realized selectively). With `wal_len = 8 KiB`, the WAL-full path still binds first in the
  fixture, so existing single-ref tests are unaffected.
- **No `//!` MVP-disclosure edit.** M-4 had no `//!` bullet (only the inline "collapsed to the
  simplest correct policy" comment, now replaced); the M-5/M-7 `//!` entries are retired later by
  B12C/B12E.

Two design notes worth recording (also in the code doc-comment):

1. **The high watermark (`global_budget`) is not separately enforced in code after B12B.** In the
   single-threaded synchronous model the low-watermark flush runs to completion inline and keeps
   `total <= size_low_watermark < global_budget`, so the high watermark is never crossed by normal
   traffic. `global_budget` defines where an *async* server would return `FULL` (Design decision
   4, future work); it stays the documented high watermark in `Default`/tests and gets the
   rev1§4.4 recommended 128 MiB in B12F.
2. **The "one ref remains" guard is the M-3/M-4 division of labor.** Size pressure flushes the
   biggest offenders but never empties the store — a single ref larger than the low watermark is
   contained instead by the **per-ref soft bound** (B12A, M-3), which with B12F's defaults
   (`per_ref_budget 8 MiB < global 128 MiB`) fires long before global size pressure. Per-ref bound
   contains one ref; size pressure sheds across refs.

## Verification (all green, run locally)

| Check | Result |
|---|---|
| `cargo test -p cas` | **74 lib** (70 prior + 4 new) + integration/fuzz-regression — all pass |
| `cargo test -p mkfs -p storage-server -p virtio-blk` | all pass (no API change → no-op) |
| `cargo verus verify -p cas --no-default-features` | **65 verified, 0 errors** — unchanged |
| `MIRIFLAGS=… miri test -p cas --lib -- size_pressure low_watermark` | **4 passed, 0 failed** (~23 min) — clean, no UB in the sort/sum/flush arithmetic |
| `MIRIFLAGS=… miri test -p cas --test fuzz_regressions --test fuzz_corpus` | clean — corpora unaffected (B12B is format-stable; no codec/format touched) |
| `cd kernel && cargo build` (aarch64 cross + user binaries) | clean (pre-existing kcore warnings only) |
| `scripts/run-demo.sh` (QEMU, mkfs image + virtio-blk) | green: `[storaged] virtio-blk up → store mounted → serving`, Eunomia shell up |

New tests (`cas/src/store.rs mod tests`):
- `size_pressure_flushes_biggest_offenders_keeps_small_dirty` — M-4 headline: three refs of
  unequal size cross the low watermark; the biggest flushes (overlay → tree, read-backable) while
  the two smaller refs stay dirty (contrast the old `sync_all` that emptied all three).
- `low_watermark_shields_high_watermark_under_steady_writes` — steady ~1 KiB round-robin writes
  across four refs keep total at or below the low watermark and never reach the high watermark.
- `size_pressure_holds_total_below_high_watermark` (proptest, 256/4) — containment invariant under
  arbitrary multi-ref interleavings: total stays `< global_budget` (and `<= low watermark` except
  the documented one-ref guard).
- `crash_recovery_survives_size_pressure_flush` (crash proptest, 64/4) — the size-pressure
  `flush_ref + commit` (partial head advance) preserves all-acked-survives: multi-ref writes cross
  a tight low watermark mid-stream so the crash point lands inside a *partial* flush (some refs
  flushed, some dirty), and every acked write recovers.

## Key findings

1. **A dedicated multi-ref crash test, not an extension of the single-ref one.** The plan's
   execution-order note says to "extend `crash_recovery_preserves_acked_state`'s op set". That test
   writes only to one ref (`main`), and its maintenance ops (`snapshot`/`gc`/`apply_batch`) are
   hardcoded to it — but the **"one ref remains" guard means size pressure never flushes a lone
   ref**, so a single-ref workload structurally cannot witness "some refs flushed, some not". B12B
   therefore adds a *separate* multi-ref crash proptest that does, leaving the load-bearing
   single-ref witness untouched. This satisfies the plan's intent (re-witness all-acked-survives
   across the new selective-flush path) without destabilizing the existing test.

2. **The behavior change is real but masked in the shared fixture, so the suite stayed green.**
   The old size-pressure path called `sync_all` (flush *everything*); the new one flushes
   biggest-offenders-only and leaves the smallest ref(s) dirty. In `test_opts` this is invisible
   because `wal_len = 8 KiB` is far below the 32 KiB budget, so the WAL-full trigger binds first;
   the size-pressure path is exercised only by the new tests, which pass tight, WAL-ample opts.
   All 70 prior cas tests pass unchanged.

3. **The new path is crash-safe for the same reason B12A's was: it rides `commit`'s verified
   partial-head-advance.** `relieve_size_pressure` does `flush_ref(...)×n` then **one** `commit`,
   which pops only the contiguous flushed prefix via `advance_head` (the B7-verified decision) and
   resets `wal_tail` only when the queue fully drains. So a crash inside a partial size-pressure
   flush leaves no acked-but-unflushed record stranded behind the head — exactly the invariant the
   new multi-ref crash proptest witnesses. B12B changes *which* refs flush and *when*, not the WAL
   invariant.

4. **`global_budget` becomes a documented-but-unenforced ceiling under the synchronous model.**
   Because the low-watermark flush always relieves pressure inline, `total` never reaches
   `global_budget`, so there is no code that acts on the high watermark — it is the boundary an
   async server would turn into a `FULL` reply (Out of scope / future work). The field stays live
   as the high watermark for `Default`, the tests, and B12F's recommended numbers.

## Out of scope for B12B (the rest of B12, recorded so it is not mistaken for a gap)

B12B closes M-4 only. The remaining sub-phases: **B12C** circular WAL ring + flush-the-pinner at
the 50% watermark (M-5, the long pole — consumes the B12A `oldest_wal_pos` accounting and starts
advancing `Superblock::wal_head` partially); **B12D** staleness-timer trigger (M-6 timer half —
consumes `oldest_dirty_ns`); **B12E** neighborhood-only re-chunk on flush (M-7, independent);
**B12F** the rev1§4.4 recommended defaults (S-9) + the refuse-not-panic `format`/`mkfs` contract
(S-10). A true asynchronous `FULL` backpressure return / background flush is rev1-deferred future
work (Design decision 4); a verified circular-ring core remains optional future hardening
(Design decision 2). The cas gate is held at 65/0.
