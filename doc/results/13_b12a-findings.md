# B12A findings — per-ref accounting substrate + `StoreOptions` reshape + per-ref soft bound

**Phase:** B12A (foundation sub-phase of B12, `doc/plans/13_b12-detail.md`). B12 conforms the
`cas` flush/memtable policy to rev1§4.4's *mandatory* triggers and bounds (Open Decision 1 /
Phase A2 resolved **mandatory**); the code had "collapsed to the simplest correct policy" — one
global `overlay_budget`, flush-everything on pressure. B12A turns that single global budget into
the spec's **per-ref-soft-bound-under-a-global-budget** shape and lays the per-ref accounting
substrate every later sub-phase reads. On its own it closes:

- **M-3 — no per-ref soft bound.** One hot ref could consume the whole global budget; now a ref
  self-flushes at its per-ref byte quota, so it cannot.
- **M-6 (op-count half) — no operation-count secondary bound.** A metadata storm whose dirty
  *bytes* stay small now flushes once it crosses an op-count bound.

It is **format-stable** and adds **no Verus**: production behavior is unchanged except the
mechanism, which is exercised only by tight-bound test opts (the rev1§4.4 *recommended numbers*
— S-9 — ship in B12F).

**Decisions exercised (from the plan):**
- **Design decision 1 — all accounting is in-memory, reconstructed at mount from WAL replay.**
  No `SB_VERSION` bump, no corpus regen. The one persistent datum a circular ring needs
  (`Superblock::wal_head`) already exists and is untouched here (B12C starts advancing it
  partially).
- **Design decision 2 — proptest + crash-injection + Miri, no new Verus chokepoint.** The flush
  policy is a scheduler over an already-verified substrate (B5 codecs / B7 recovery cores,
  untouched). The cas verify gate holds at **65/0**.
- **Design decision 4 — backpressure is a synchronous blocking flush, not a `FULL` return.**
  Crossing a per-ref bound flushes that ref inline (overlay → committed tree), then the write
  proceeds; no eviction, no protocol surface a single-threaded server can never return.

## What landed (all in `cas/src/store.rs` unless noted)

- **`StoreOptions` reshape.** `overlay_budget` → `global_budget`; five new `pub` fields, each
  doc-commented with its rev1§4.4 citation and consuming sub-phase: `per_ref_budget`,
  `op_count_bound` (consumed **here**), `size_low_watermark` (B12B), `wal_watermark` (B12C),
  `staleness_ns` (B12D). `Default` keeps the pre-B12 magnitudes (16 MiB WAL / 8 MiB budget) and
  **stubs the new bounds to non-triggering values** (`per_ref_budget = global_budget`,
  `op_count_bound = u64::MAX`, watermarks = the budgets they sit under, `staleness_ns = u64::MAX`),
  so the reshape changes no behavior. Derives unchanged.
- **`RefAcct` substrate.** A private `#[derive(Debug, Default)] struct RefAcct { op_count: u64,
  oldest_wal_pos: Option<u64>, oldest_dirty_ns: Option<u64> }` and a parallel `Store` field
  `acct: BTreeMap<Vec<u8>, RefAcct>` keyed like `overlays` (dirty bytes already live in
  `Overlay::bytes()`). Initialized empty in both `Store` constructions (`format`, `mount`).
- **`account_op(&self, op, wal_pos)`** — `entry(ref).or_default()`, `op_count += 1`,
  `oldest_wal_pos.get_or_insert(wal_pos)`, `oldest_dirty_ns.get_or_insert(op.mtime())`. Called on
  the live write path (`log_then_apply`) and again, identically, in the `mount` WAL-replay loop —
  so a remounted store recomputes the substrate. New `WalOp::mtime()` accessor in `disk.rs`
  (mirrors `ref_name()`).
- **Per-ref soft bound** in `log_then_apply`: after `account_op`, if the written ref's overlay
  bytes exceed `per_ref_budget` **or** its `op_count` exceeds `op_count_bound`, `flush_ref(&r) +
  commit()`. `flush_ref` now drops the ref's `acct` entry alongside its overlay (reset on flush).
  The pre-existing global size-pressure check is renamed to `global_budget` (still
  flush-everything — B12B refines it).
- **Caller updates (the breaking-rename blast radius).** Spread-form callers (`mkfs`, `storaged`,
  several storage-server tests) needed no change; seven full-literal sites
  (`cas` `test_opts`/tests/example, `storage-server` tests + fuzz target, `virtio-blk` test)
  were converted to keep their explicit `wal_len`/`chunker`/`global_budget` and inherit the new
  fields via `..StoreOptions::default()`.
- **Tests** (`cas/src/store.rs mod tests`): two deterministic + one proptest for the bounds, one
  remount round-trip, plus the crash-injection extension (below).

## Verification (all green, run locally)

| Check | Result |
|---|---|
| `cargo test -p cas` | **70 lib** (66 prior + 4 new) + 9 + 10 integration — all pass |
| `cargo test -p mkfs -p storage-server -p virtio-blk` | all pass (caller updates) |
| `cargo build -p cas --examples` / `storage-server/fuzz cargo +nightly check` | clean |
| `cargo verus verify -p cas --no-default-features` | **65 verified, 0 errors** — unchanged |
| `MIRIFLAGS=… miri test -p cas` (4 B12A tests + crash) | **5 passed, 0 failed, 366 s — clean** (no UB in accounting/soft-bound arithmetic) |
| `MIRIFLAGS=… miri test -p cas --test fuzz_regressions --test fuzz_corpus` | **clean** — corpora unaffected (B12A is format-stable) |
| `scripts/run-demo.sh` (QEMU, mkfs image + virtio-blk disk) | green: `[storaged] virtio-blk up → store mounted → serving`, shell up |
| `cd kernel && cargo build` (aarch64 cross + user binaries) | clean (pre-existing kcore warnings only) |

New / changed tests:
- `per_ref_soft_bound_flushes_hot_ref_keeps_quiet_ref_dirty` — M-3 headline: a hot ref driven
  past a tight `per_ref_budget` self-flushes (overlay never exceeds the bound; flushed data is
  read-backable from the tree), while a quiet ref under its bound stays dirty (no eviction).
- `per_ref_overlay_never_exceeds_soft_bound` (proptest, 256/4) — the M-3 invariant under
  arbitrary multi-ref write interleavings: no ref's overlay ever exceeds the soft bound.
- `op_count_bound_flushes_a_metadata_storm_under_the_byte_bound` — M-6 op-count half: nine 1-byte
  ops flush the ref via op-count while its 9 dirty bytes stay far under `per_ref_budget`.
- `per_ref_accounting_is_reconstructed_on_remount` — Design decision 1: a remount replays the WAL
  and recomputes bytes / op-count / oldest-WAL-position / oldest-dirty-timestamp **identical** to
  the pre-remount live state.
- `crash_recovery_preserves_acked_state` (extended) — now formats/mounts with `crash_opts`
  (`op_count_bound = 4`) so the per-ref soft-bound auto-flush (`flush_ref + commit`) fires
  mid-stream; the crash point can land inside the selective flush, re-witnessing
  all-acked-survives across the new path.

## Key findings

1. **Loose defaults make the reshape behavior-neutral; the mechanism is real but ships disarmed.**
   M-3/M-6 are closed by the *mechanism existing and being demonstrated* (the tight-opt tests),
   not by tightening the shipped numbers — those are the rev1§4.4 recommended defaults (S-9),
   deferred to B12F. So `StoreOptions::default()` keeps `per_ref_budget = global_budget` and
   `op_count_bound = u64::MAX`, and all 66 prior cas tests pass unchanged. This is the
   minimum-blast-radius reading of "the build stays green before B12B refines it."

2. **The dirty timestamp is the op's own `mtime`, not a separate clock — which is *why* remount
   reconstruction is exact.** `account_op` derives `oldest_dirty_ns` from `op.mtime()` (the
   server-assigned UTC-nanos already carried in every `Write`/`Unlink` and persisted in the WAL
   record). So the live write path and the replay path feed `account_op` the identical value, and
   the round-trip test asserts byte-exact equality of all four accounting data — no clock seam, no
   drift. (B12D's staleness check will compare this timestamp against an injected `now`.)

3. **The per-ref soft-bound flush rides `commit`'s existing partial-head-advance — no new WAL
   machinery in B12A.** `flush_ref` marks the ref's records flushed; `commit` pops the *contiguous
   flushed prefix* via the Verus-verified `advance_head` and only resets `wal_tail` when the queue
   fully drains. With the single-ref crash test this drains fully; with multiple dirty refs the
   head advances partially and conservatively (WAL space not fully reclaimed until B12C's ring).
   So B12A's selective flush is correct on the *existing* substrate; B12C adds reclaim, not
   correctness.

4. **A crash inside the auto-flush is the *in-flight* write the model already tolerates.** The
   write is acked at its WAL fsync — *before* `apply_to_overlay` and the soft-bound
   `flush_ref + commit`. If the auto-flush/commit then fails (device fail-point), `write` returns
   `Err`, the proptest records the op as the single ambiguous in-flight mutation, and recovery
   either replays the durable WAL record (got == written) or sees the prior value — both branches
   the `matches_model || matches_inflight` assertion accepts. The verified `advance_head` +
   `recover_records` guarantee no acked-but-unflushed record is stranded behind the head, so
   all-acked still survives. `op_count_bound = 4` fires the flush every 4 ops, giving dense
   crash coverage of the new path.

5. **mkfs and storaged are semantically untouched — the demo is the live witness.** Both
   construct options via spread/`default()`, so they pick up the new fields transparently;
   `scripts/run-demo.sh` builds a 64 MiB image with `mkfs` and boots with a virtio-blk disk, and
   `storaged` reports `virtio-blk up → store mounted → serving` with the IPC-served shell up. The
   bare `cd kernel && cargo run` attaches *no* disk (README), so its `[storaged] FATAL: no
   virtio-blk device` is expected and pre-existing — not a regression.

## Out of scope for B12A (the rest of B12, recorded so it is not mistaken for a gap)

The new option fields `size_low_watermark` / `wal_watermark` / `staleness_ns` are carried as the
substrate but **stubbed** — their mechanisms land later: **B12B** size-pressure low/high
watermarks + flush-the-biggest-offenders (M-4); **B12C** circular WAL ring + flush-the-pinner at
the 50% watermark (M-5, starts advancing `Superblock::wal_head` partially); **B12D**
staleness-timer trigger (M-6 timer half); **B12E** neighborhood-only re-chunk on flush (M-7,
independent); **B12F** the rev1§4.4 recommended defaults (S-9) + the refuse-not-panic `format`
contract (S-10). The MVP-disclosure block in `store.rs` is left intact (the M-5/M-7 entries it
discloses are retired by B12C/B12E). The cas gate is held at 65/0 — a verified circular-ring core
remains optional future hardening (Design decision 2).
