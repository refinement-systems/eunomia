# B12C findings — circular WAL ring + flush-the-pinner at the 50% watermark

**Phase:** B12C (`doc/plans/13_b12-detail.md`), the third sub-phase of B12, which conforms the
`cas` flush/memtable policy to rev1§4.4's *mandatory* triggers and bounds (Open Decision 1 /
Phase A2 resolved **mandatory**). B12A laid the per-ref accounting substrate (incl.
`oldest_wal_pos`); B12B closed size pressure (M-4); B12C closes the **WAL-pressure** trigger
(rev1§4.4 trigger 2) — the long pole:

- **M-5 — WAL-pressure flush-the-pinner and the 50% watermark were absent.** The MVP WAL was
  *linear*: the head could reclaim space only by resetting to 0 when *everything* flushed
  (`wal_tail + reclen > wal_len → sync_all()`). Flush-the-pinner is meaningless without space
  reclaim, so the per-ref WAL scheduler was collapsed to flush-everything-on-full. B12C makes the
  WAL a **circular ring** whose head advances past a partial flushed prefix, driven by **flushing
  the ref pinning the tail** when usage crosses a watermark — so flushing one ref actually frees
  reusable WAL space.

It is **format-stable** and adds **no Verus** (cas gate held at **65/0**). It makes **no
`StoreOptions` API change** — the `wal_watermark` field already existed (stubbed `= wal_len`)
since B12A — so it has **no caller blast radius**: `mkfs`, `storaged`, `storage-server`,
`virtio-blk` are untouched.

**Decisions exercised (from the plan):**
- **Design decision 3 — the WAL becomes a circular ring with an advancing `wal_head`** (vs. linear
  reset-only). The persistent `Superblock::wal_head` already exists and is already advanced
  partially by the Verus-verified `advance_head`; B12C is a *runtime* change: ring write (with a
  split across the wrap), ring replay at mount, and flush-the-pinner space reclaim.
- **Design decision 1 — format-stable.** Every datum is runtime scheduler state; nothing new is
  persisted. No `SB_VERSION` bump, no corpus regen.
- **Design decision 2 — proptest + crash-injection + Miri, no new Verus chokepoint.** The flush
  policy is a scheduler over the already-verified codec/recovery substrate (B5/B7, untouched). The
  cas gate holds at 65/0.
- **Design decision 4 — backpressure is a synchronous blocking flush, not a `FULL` return.**

## The key design discovery: rotation-at-mount keeps the verified cores untouched

The hard constraint is that `recover_records` is **Verus-verified**, walks records **contiguously**
from `wal_head`, and must not be modified (cas gate 65/0). A ring's live records can physically
wrap (straddle the buffer end), which a contiguous walk cannot follow. The resolution:

- **At mount, rotate the in-memory WAL buffer by `wal_head` (`wal.rotate_left(wal_head)`)** so the
  live window becomes contiguous starting at rotated offset 0. Then call the **unchanged**
  `recover_records(&wal, 0, wal_next_seq)`. A straddling record is contiguous after rotation; the
  walk stops naturally at the genuine torn tail (or the stale region). The returned record offsets
  are *rotated* → mapped back to physical ring positions via `(wal_head + rotated) % wal_len` for
  the live `wal_records` / accounting / `wal_tail` (the persisted head is already physical). The
  applier decodes from the *rotated* buffer at the rotated offset, so a straddling record reads
  contiguously.
- This is safe and total: `validate_geometry` (run before replay) guarantees `wal_head <= wal_len`,
  so `rotate_left` cannot panic — mount stays total over arbitrary device contents (rev1§4.5).
- **Why it's sound without re-verifying:** `advance_head` only *selects* a record's `off` (it copies
  `records[i].off`); it never adds, compares, or orders offsets, so feeding it physical (wrapped,
  non-monotonic) offsets is fine. The verified `recover_records`/`lemma_gap_freedom` reason over the
  *rotated/linearized view* — exactly the contiguous run they require — and the physical-offset
  remap is plain-Rust applier glue, outside any `verus!{}` block. No verified function changed; the
  cas gate is held at 65/0.

This is strictly better than the rejected alternatives: end-padding (a gap that breaks the
rotation-then-contiguous-walk and is indistinguishable from a torn tail at mount) and mount-time
WAL re-linearization (an extra non-atomic WAL rewrite with its own crash-safety story).

## What landed (all in `cas/src/store.rs`)

- **`wal_usage(&self) -> u64`** — live ring usage: `0` when the queue is empty, else
  `(wal_tail + wal_len - sb.wal_head) % wal_len`, with the disambiguation `raw == 0 && !empty ⟹
  wal_len` (a tail wrapped exactly onto the head is *full*, not empty).
- **`wal_write(&mut self, off, rec)`** — appends a record at ring offset `off`, splitting the device
  write across the buffer end when the record wraps, then a **single** `flush()` after both halves.
  The single fsync is load-bearing for crash-safety (a flush *between* halves would make an
  ack-less half-record durable that a later same-offset write could partially overlay). Handles a
  header that itself straddles the wrap.
- **`relieve_wal_pressure(&mut self, incoming)`** — flushes the ref **pinning the tail** (the front
  of `wal_records`, whose oldest record sits at `wal_head`) + `commit()` until `usage + incoming`
  fits **and** `usage < wal_watermark`. `commit`'s verified `advance_head` advances `wal_head` past
  the now-flushed contiguous prefix, reclaiming that span. The single-ref case degenerates to
  flush-everything-and-reset (the normative full-WAL edge). Replaces the `wal_tail + reclen >
  wal_len → sync_all()` block in `log_then_apply`.
- **`log_then_apply`** — calls `relieve_wal_pressure(reclen)` *before* the write, reads `rec_off`
  only *after* relief (relief's `commit` can move `wal_tail`), then a ring `wal_write` and
  `wal_tail = (rec_off + reclen) % wal_len`. The oversized-bypass edge case is unchanged.
- **`mount` WAL replay** — `rotate_left(wal_head)` → `recover_records(.., 0, ..)` → physical-offset
  remap (above).
- **Stale-comment fixes:** the cross-commit head-monotonicity note on `advance_head` (now false
  under the ring — offsets wrap; reworded to explain `advance_head` only *selects* an offset) and
  the `RefAcct::oldest_wal_pos` doc (B12C picks the pinner by `front()`, **not** by sorting on this
  field — sorting is wrong across the wrap; the field stays the per-ref position datum tests assert
  against).
- **MVP-disclosure retired:** the `//!` "WAL is linear, not circular" bullet is removed (the ring
  now exists). The oversized-bypass bullet stays (a normative rev1§4.4 edge case, not a
  simplification).

## Verification (all green, run locally)

| Check | Result |
|---|---|
| `cargo test -p cas` | **81 lib** (74 prior + 7 new) + 9 fuzz_corpus + 10 fuzz_regressions — all pass |
| `cargo test -p mkfs -p storage-server -p virtio-blk` | all pass (no API change → no-op) |
| `cargo verus verify -p cas --no-default-features` | **65 verified, 0 errors** — unchanged |
| `MIRIFLAGS=… miri test -p cas --lib -- wal_ ring_ oversized_ crash_recovery_survives_wal` | **12 passed, 0 failed** — no UB in the ring/offset arithmetic (the two new proptests are capped under `cfg(miri)` — small device + short op stream — so the sweep stays bounded; the full untargeted lib sweep otherwise balloons because each random-ring case allocates a fresh device Miri byte-tracks) |
| `MIRIFLAGS=… miri test -p cas --test fuzz_regressions --test fuzz_corpus` | clean (9 + 10) — mount-over-arbitrary-bytes now runs through the rotation; still total + UB-free (B12C is format-stable, corpora unaffected) |
| aarch64 cross-build (`cd kernel && cargo build`) | clean (pre-existing kcore unused-import warnings only) — `storaged` links the reshaped flush path |
| `scripts/run-demo.sh` (QEMU, mkfs image + virtio-blk) | green: `[storaged] store mounted → serving`, then a live `write docs/b12c … / sync / cat docs/b12c → circular-wal-works` round-trip through the new ring write + flush path |

New tests (`cas/src/store.rs mod tests`):
- `wal_pressure_flushes_pinner_keeps_newer_dirty` — M-5 headline: WAL pressure flushes the
  tail-pinner (overlay → committed tree, head advances, span reclaimed) and the newer ref stays
  dirty (not flush-everything).
- `ring_wrap_front_pinner_reclaim_and_remount` — across a ring **wrap**, the victim is the front of
  the queue (the record at `wal_head`), **not** the ref with the smallest `oldest_wal_pos` (those
  differ after a wrap); flushing the front advances the head; a straddling record and all values
  survive remount.
- `ring_exactly_full_reports_full_then_relieves` — an exactly-full ring (tail wraps onto head)
  reports `wal_len` not `0`; the next write relieves (single ref ⇒ flush-everything-and-reset).
- `wal_record_header_straddles_wrap` — a record whose 48-byte **header** straddles the wrap is
  split mid-header on write and reassembled by the mount-time rotation.
- `oversized_write_while_ring_nonempty` — the normative oversized-bypass edge case while the ring
  already holds live records: bypass commits synchronously, the ring tail stays consistent, all
  survives remount.
- `wal_ring_invariants_hold_across_random_wraps` (proptest, 256/4) — across random multi-ref streams
  on a small ring (so it wraps repeatedly and flush-the-pinner fires), after every op: `wal_head ==
  front().off`, `wal_usage == ` the independent sum of live record spans, and `usage <= wal_len`
  (relief always makes room).
- `crash_recovery_survives_wal_wrap` (crash proptest, 64/4) — the circular-WAL flush-the-pinner path
  (selective flush, partial head advance, **split writes across the wrap**, crash possibly landing
  between the two halves) preserves all-acked-survives: every acked write recovers from durable
  state.

## Key findings

1. **The "partial head advance" the plan attributes to B12C already existed.** `advance_head`
   (B7/B8) already pops the *contiguous flushed prefix* and sets the head to the first non-flushed
   record. B12A/B12B already rode it. The genuinely new B12C work is therefore narrower than the
   plan's framing: **ring write** (wrap `wal_tail` instead of only resetting to 0 on full), **ring
   replay** (read across the wrap at mount), and the **flush-the-pinner watermark trigger**. The
   head-advance machinery is reused unchanged.

2. **The tail-pinner must be chosen by `front()` of the WAL queue, not by min `oldest_wal_pos` —
   and this only matters across a wrap.** After a wrap a newer ref can sit at a *lower* physical
   offset than the tail-pinner, so min-by-`oldest_wal_pos` would pick the wrong ref; flushing it
   would not advance the head (`advance_head` pops only the contiguous *prefix*, which starts at the
   actual pinner), so usage would not drop and relief would stall. `wal_records.front()` is provably
   the record at `wal_head`, so it is the correct, wrap-safe victim. The
   `ring_wrap_front_pinner_reclaim_and_remount` test constructs exactly the disagreeing state and
   guards against a future "sort by `oldest_wal_pos`" regression. (The B12A `oldest_wal_pos` doc
   claimed it was the runtime sort key; corrected.)

3. **Even at the default-stubbed `wal_watermark = wal_len`, the ring is a strict improvement.** With
   the stub, the watermark trigger never fires (usage is always `< wal_len` except when exactly
   full), so relief fires only on a genuine won't-fit — the same point the old `sync_all` fired. But
   the *action* differs: instead of flush-everything-and-reset, it flushes the front pinner and
   wraps, reclaiming only that span and reusing freed low offsets. B12F sets the recommended 50%
   watermark to make the trigger proactive. So existing tests (which inherit the stub) stay green
   while gaining the ring.

4. **Crash-safety rests on the same WAL invariant, now across the wrap.** The split-across-wrap
   write uses a *single* fsync after both halves, so a torn straddling tail record is never acked;
   the per-record blake3 checksum (over `seq || len || payload`) plus seq-continuity reject it on
   replay, and stale bytes in the reused low region (records the head advanced past, with lower
   seqs) cannot forge a seq-continuous record. The crash proptest tunes the device fail point so it
   can land between the two halves. B12C changes *which* refs flush and *when* (and lets the head
   advance past a partial prefix across the wrap), not the "head advances only past flushed records"
   invariant — which `CommitProtocol`'s `AckedWritesRecoverable` + the B7 `Recover` property already
   model, and the crash proptest witnesses.

5. **The chunk region, not the WAL, is the capacity limit under heavy flushing.** Frequent
   flush-the-pinner without GC (synchronous-GC MVP) accumulates dead chunks fast; the randomized
   ring test fills a 1 MiB device's chunk region long before the WAL invariants could fail. The test
   uses a 64 MiB device and stops cleanly on `NoSpace` — chunk capacity is not what B12C is about
   (the WAL ring is), and it is exactly the pressure incremental GC (Phase C4) relieves.

## Ledger

No edit to `doc/guidelines/verus_trusted-base.md`: B12C adds no verified surface and no new trusted
seam, and the cas baseline (`cargo verus verify -p cas --no-default-features` → **65/0**, ledger
line 158) is held unchanged — consistent with B12A/B12B, which also left the ledger untouched (the
flush policy is plain-Rust scheduler code below the Verus line, test-routed per Design decision 2).

## Out of scope for B12C (the rest of B12, recorded so it is not mistaken for a gap)

B12C closes M-5 only. Remaining: **B12D** staleness-timer trigger (M-6 timer half — consumes the
B12A `oldest_dirty_ns`); **B12E** neighborhood-only re-chunk on flush (M-7, independent — retires
the remaining "whole dirty files" `//!` bullet); **B12F** the rev1§4.4 recommended defaults
(S-9, incl. the 50% `wal_watermark` and 64 MiB WAL) + the refuse-not-panic `format`/`mkfs` contract
(S-10). A true asynchronous `FULL` backpressure return / background flush is rev1-deferred future
work (Design decision 4); a verified circular-ring-arithmetic core remains optional future hardening
(Design decision 2, recorded like `freelist` — additive if ever taken, weakening nothing); streaming
WAL replay (vs. buffering the live window for the rotation) is Phase C4. The cas gate is held at 65/0.
