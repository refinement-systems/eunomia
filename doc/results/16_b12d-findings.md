# B12D findings — staleness-timer flush trigger

**Phase:** B12D (`doc/plans/13_b12-detail.md`), the fourth sub-phase of B12, which conforms the
`cas` flush/memtable policy to rev1§4.4's *mandatory* triggers and bounds (Open Decision 1 /
Phase A2 resolved **mandatory**). B12A laid the per-ref accounting substrate (incl.
`oldest_dirty_ns`); B12B closed size pressure (M-4); B12C closed WAL pressure (M-5). B12D closes
the **staleness-timer** trigger (rev1§4.4 trigger 4) — the lowest-priority one, the last of the
four:

- **M-6 (timer half) — no timer/staleness trigger existed at all.** `StoreOptions::staleness_ns`
  was carried by B12A but consumed nowhere and stubbed to `u64::MAX`. A ref dirtied once and never
  touched again sat in overlay indefinitely (until size/WAL pressure or an explicit
  `sync`/`snapshot`). B12D wires the bound in: a ref whose **oldest-dirty age** exceeds
  `staleness_ns` is flushed to committed tree, so "a quietly dirty ref eventually becomes committed
  tree." (M-6's op-count half landed in B12A; this is the timer half.)

It is **format-stable** and adds **no Verus** (cas gate held at **65/0**). It makes **no
`StoreOptions` API change** — the `staleness_ns` field already existed (stubbed `= u64::MAX`) since
B12A — so the blast radius is `cas` + the `storaged` reactor loop only; `mkfs`,
`storage-server`/lib, and `virtio-blk` are untouched.

**Decisions exercised (from the plan):**
- **Design decision 5 — the timer fires opportunistically, not via an armed kernel timer.** The
  single-threaded request-driven server has no background thread, so the staleness sweep runs at
  the points the server already runs: the write path (`log_then_apply`, lowest priority) and the
  storage server's **reactor-idle** point. The B-IRQ timer-object route (arming a kernel timer for
  a fully-idle, never-polled server) is recorded as the future upgrade, not built.
- **Design decision 1 — format-stable.** `oldest_dirty_ns` is runtime scheduler state,
  reconstructed at mount from WAL replay (the B12A substrate); nothing new is persisted. No
  `SB_VERSION` bump, no corpus regen.
- **Design decision 2 — proptest + Miri, no new Verus chokepoint.** The flush policy is a scheduler
  over the already-verified codec/recovery substrate (B5/B7, untouched). The cas gate holds at 65/0.
- **Design decision 4 — backpressure is a synchronous blocking flush, not a `FULL` return.**

## The key design discovery: there is no internal clock to inject — the seam already exists

The plan's Design decision 5 anticipated needing "an injectable clock seam for the test" and
referenced "the store's clamped-monotone per-ref time source (`store.rs:1700-1705`)." Exploration
found that framing imprecise in a way that *simplified* the work: **the store has no internal
clock at all.** Time enters the store only as the caller-provided `mtime`/`now: u64` parameter on
`write`/`unlink`/`snapshot` — sourced in the shipped server by `storaged`'s `now_utc()` →
`urt::time::now_utc_ns()` (rev1§2.6 time page), and threaded into each `WalOp` as its `mtime`. The
per-ref `oldest_dirty_ns` is just `op.mtime()` captured on the first dirty op after a flush
(`account_op`, `get_or_insert`).

So the "injectable clock" is the `now` argument itself. `relieve_staleness(now)` takes the clock
as a parameter; the write-path call passes the incoming op's mtime, the server's reactor-idle call
passes `now_utc()`, and tests pass plain integers. No synthetic-clock plumbing, no new field, no
trait — deterministic and Miri-safe by construction (no wall-clock sleeps). This is why B12D is the
S–M / low-effort sub-phase the plan rated it.

## What landed

**`cas/src/store.rs`:**
- **`relieve_staleness(&mut self, now: u64)`** — the sweep. Early-returns when `staleness_ns ==
  u64::MAX` (the disabled stub, keeping the hot write path free of the `acct` walk), else collects
  every ref whose `oldest_dirty_ns` is `Some(t)` with `now.saturating_sub(t) > staleness_ns`,
  `flush_ref`s each, and folds the lot into a **single** `commit` — mirroring `relieve_size_pressure`.
  `saturating_sub` makes a non-monotone clock a no-op rather than a spurious flush. Selective, not
  flush-everything: only overdue refs flush; fresher dirty refs stay in overlay.
- **`flush_stale(&mut self, now: u64)` (pub)** — the server's opportunistic-sweep entry point
  (delegates to `relieve_staleness`). Called at request boundaries / reactor idle.
- **`log_then_apply`** — appends `self.relieve_staleness(op.mtime())?` *after* `relieve_size_pressure`,
  the lowest of the four triggers (WAL → per-ref → size → staleness). The incoming op's mtime is the
  clock, so the write path itself bounds staleness without a timer.

**`user/storaged/src/main.rs`:**
- A `server.store().flush_stale(now_utc())` sweep at the **top of the dispatch loop**, just before
  `reactor.wait()`. That is the reactor-idle point: the inner drain loop has just emptied the
  request ring (broke on `Empty`), so the server is about to park. Best-effort (a flush failure is
  non-fatal — the next write still logs durably). `Server::store()` already exposed `&mut Store`, so
  `storage-server`/lib needed no change.

No `//!` MVP-disclosure bullet was retired: the staleness timer was an *absent* mechanism (M-6),
not a disclosed simplification (unlike B12C's "WAL is linear" or B12E's "whole-file re-chunk"
bullets).

## Verification (all green, run locally)

| Check | Result |
|---|---|
| `cargo test -p cas` | **85 lib** (81 prior + 4 new) + 9 fuzz_corpus + 10 fuzz_regressions — all pass |
| `cargo test -p mkfs -p storage-server -p virtio-blk` | all pass (no API change → no-op) |
| `cargo verus verify -p cas --no-default-features` | **65 verified, 0 errors** — unchanged |
| `MIRIFLAGS=-Zmiri-disable-isolation miri test -p cas --lib -- staleness_` | **4 passed, 0 failed** (54 s) — no UB in the sweep/accounting arithmetic; the proptest is capped at 4 cases under `cfg(miri)` |
| aarch64 cross-build (`cd kernel && cargo build`) | clean (pre-existing kcore unused-import warnings only) — `storaged` links the reactor-idle `flush_stale` |
| `scripts/run-demo.sh` (QEMU, mkfs image + virtio-blk) | green: `[storaged] store mounted → serving`, then a live `write docs/b12d … / sync / cat docs/b12d → staleness-timer-works` round-trip through the reshaped serve loop |

New tests (`cas/src/store.rs mod tests`), under a `stale_opts(staleness_ns)` fixture that disables
every other trigger so the staleness sweep is observed in isolation:
- `staleness_flushes_quietly_dirty_ref_keeps_fresh_ref_dirty` — M-6 headline: the explicit
  `flush_stale` sweep flushes an overdue ref to committed tree (read-backable) while a ref dirtied
  within the bound stays dirty (selective, not flush-everything).
- `staleness_fires_on_next_write_request` — the write-path trigger: a write whose mtime leaves an
  older quiet ref past the bound flushes that ref, while the just-written ref stays dirty.
- `staleness_disabled_by_default_never_flushes` — guards that the default-stubbed
  `staleness_ns = u64::MAX` (which every shipped/B12A-C fixture inherits until B12F) never fires.
- `staleness_sweep_leaves_no_overdue_ref` (proptest, 256/4) — across random multi-ref streams under
  a monotone-increasing clock, after `flush_stale(now)` **no ref left dirty is overdue** (the sweep
  is total over the dirty set) and refs within the bound stay dirty.

## Key findings

1. **The trigger keys on *oldest*-dirty, so it bounds the maximum staleness of any dirty byte — not
   "time since last touch."** `oldest_dirty_ns` is set on the first dirty op after a flush and never
   moves forward until the ref flushes (B12A's `get_or_insert`). So a ref written-to continuously
   since `t0` still flushes once `now − t0 > staleness_ns`, capping how long *any* unflushed byte
   sits. This is the correct reading of "a staleness bound" and is what the proptest invariant
   asserts. (A "time since last write" reading would let a steadily-appended ref evade the bound
   forever.)

2. **Disabled by default; B12D installs the mechanism, B12F ships the figure.** Like B12C's ring at
   the stubbed `wal_watermark = wal_len`, the staleness sweep is inert at the default
   `staleness_ns = u64::MAX` — the early return makes it a literal no-op on the hot path. The
   recommended 30 s default (rev1§4.4) is S-9 / B12F's job. So every existing test (and the shipped
   server today) is behavior-unchanged while gaining the mechanism; the `staleness_disabled_*` test
   pins that.

3. **The write-path scan and the reactor-idle sweep cover disjoint cases; together they realize
   "eventually."** A *write-busy* server flushes its quiet refs via the `log_then_apply` scan (using
   each op's mtime). A *quiet or read-only* server flushes them via the reactor-idle sweep, which
   fires exactly when the request ring drains and the server is about to block. The MVP "eventually"
   = "by the next write or reactor wake." A truly-idle, never-polled server that must still flush on
   a wall-clock deadline is the recorded armed-timer upgrade (Design decision 5) — out of scope.

4. **The crash-injection proptest was deliberately *not* extended.** B12D adds no new selective-flush
   *durability* path: `relieve_staleness` calls the same `flush_ref` + `commit` that B12A's per-ref
   bound and B12B's size pressure already drive through `crash_recovery_*`. Staleness changes only
   *when* an existing flush fires, not *how* it persists, so the all-acked-survives invariant
   (`CommitProtocol`'s `AckedWritesRecoverable` + the B7 `Recover` property) is already witnessed.
   This matches the plan's Design decision 2 / Out-of-scope note.

5. **Heavy flush-without-GC fills the chunk region, not the WAL — the same B12C finding.** The
   staleness proptest, when staleness flushes fire often, accumulates dead chunks fast (synchronous-GC
   MVP). It uses a 16 MiB device and treats a `NoSpace` as a clean early end (the invariant still
   holds on the state reached), exactly as B12C's randomized ring test did. Chunk capacity is what
   incremental GC (Phase C4) relieves; it is orthogonal to the staleness trigger under test.

## Ledger

No edit to `doc/guidelines/verus_trusted-base.md`: B12D adds no verified surface and no new trusted
seam, and the cas baseline (`cargo verus verify -p cas --no-default-features` → **65/0**, ledger
line 158) is held unchanged — consistent with B12A/B12B/B12C, which also left the ledger untouched
(the flush policy is plain-Rust scheduler code below the Verus line, test-routed per Design
decision 2).

## Out of scope for B12D (the rest of B12, recorded so it is not mistaken for a gap)

B12D closes the M-6 timer half only. Remaining: **B12E** neighborhood-only re-chunk on flush (M-7,
independent — retires the remaining "whole dirty files" `//!` bullet); **B12F** the rev1§4.4
recommended defaults (S-9, incl. the 30 s `staleness_ns`, 50% `wal_watermark`, 8 MiB per-ref / 128
MiB global / 64 MiB WAL) + the refuse-not-panic `format`/`mkfs` contract (S-10). The
armed-timer-notification staleness trigger (a B-IRQ kernel timer object for a fully-idle,
never-polled server) is the recorded upgrade beyond the opportunistic MVP sweep (Design decision
5). A true asynchronous `FULL` backpressure return / background flush is rev1-deferred future work
(Design decision 4). The cas gate is held at 65/0.
