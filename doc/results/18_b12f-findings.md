# B12F findings — recommended defaults (S-9) + refuse-not-panic `format` contract (S-10)

**Phase:** B12F (`doc/plans/13_b12-detail.md`), the **finishing** sub-phase of B12. It closes the
last two flush-policy audit items, both of which B12A–B12E left as stub/placeholder behavior:

- **S-9 (soft) — shipped defaults diverge from every rev1§4.4 number.** `StoreOptions::default()`
  still carried the pre-B12 stub magnitudes (8 MiB budget / 16 MiB WAL) with the new B12A–D bounds
  *stubbed non-triggering* (`op_count_bound = u64::MAX`, `staleness_ns = u64::MAX`). B12A
  deliberately stubbed them so the reshape changed no behavior, leaving B12F to ship the rev1§4.4
  recommended-defaults table that "the storage server's shipped configuration matches."
- **S-10 (spec-gap) — `format`/`mkfs` panic on an undersized device.** `Store::format` aborted
  through `assert!(dev.len() > chunk_off + 4096, "device too small")` instead of the clean
  `Result`/`ExitCode::FAILURE` path rev1§4.5 requires: "Mount is total over arbitrary device
  *contents*; `format` is total over arbitrary device *geometry* … refused with an error, never a
  panic."

B12F is **format-stable**, adds **no Verus** (cas gate held at **65/0**), and its only API
addition is one `StoreError` variant. The blast radius is `cas` + `mkfs`; `user/storaged` and the
storage-server/virtio-blk/loader crates need **no edit**.

**Decisions exercised (from the plan):**
- **Design decision 2 — proptest + Miri, no new Verus chokepoint.** The geometry check and the
  defaults are plain-Rust policy below the Verus line; the cas gate holds at 65/0.
- **Design decision 1 — format-stable.** No on-disk format touched; no `SB_VERSION` bump; no corpus
  regen (the committed Miri fuzz sweep stays clean).
- **The mkfs `wal_len` choice (plan B12F):** keep the deliberate 1 MiB batch-tool tune, documented
  as a tune (not drift), because the recommended 64 MiB WAL cannot lay out within the default
  64 MiB image and the on-OS server has a 3 MiB heap (finding 3 below).

---

## What landed

### S-10 — refuse-not-panic `format` (`cas/src/store.rs`, `mkfs/`)

- **`StoreError::DeviceTooSmall`** + its `Display` arm (`"device too small for the requested
  geometry"`), citing rev1§4.5.
- **`const MIN_CHUNK_REGION: u64 = 4096`** — the minimal chunk region a fresh device must hold
  beyond the WAL for the initial ref-table object + durable index frame (the old `assert!`'s slack,
  now named and documented).
- **`format` validates geometry before writing anything**, with `checked_add` so a hostile
  `wal_len` near `u64::MAX` cannot wrap into a false pass:
  ```rust
  let chunk_off = WAL_OFF.checked_add(opts.wal_len).ok_or(StoreError::DeviceTooSmall)?;
  let min_dev   = chunk_off.checked_add(MIN_CHUNK_REGION).ok_or(StoreError::DeviceTooSmall)?;
  if dev.len() <= min_dev { return Err(StoreError::DeviceTooSmall); }
  ```
  The threshold is byte-identical to the old `assert!` (strict `>`, so refuse on `<=`), so a device
  that formatted before still formats and one that panicked before now refuses.
- **No `mkfs` plumbing change for the failure path** — `mkfs/src/main.rs`'s `Store::format(dev,
  opts)?` already propagates `Err` to `main()`'s `ExitCode::FAILURE`. Once `format` returns `Err`
  instead of asserting, the clean exit is reached with a tidy message.

### S-9 — recommended defaults (`cas/src/store.rs` `StoreOptions::default()`)

| field | old (stub) | new (rev1§4.4) |
|---|---|---|
| `wal_len` | 16 MiB | **64 MiB** |
| `global_budget` (high watermark) | 8 MiB | **128 MiB** |
| `per_ref_budget` (soft bound) | 8 MiB | **8 MiB** |
| `size_low_watermark` | 8 MiB | **96 MiB** (`global_budget * 3/4`) |
| `wal_watermark` | 16 MiB | **32 MiB** (`wal_len / 2`, flush-the-pinner at 50 %) |
| `staleness_ns` | `u64::MAX` | **30 s** (`30_000_000_000`) |
| `op_count_bound` | `u64::MAX` | **8192** (documented: rev1§4.4 gives no number) |

`storaged` mounts with `StoreOptions::default()`, so it inherits these production memory budgets
with **no storaged edit** — it *is* "the storage server's shipped configuration" the spec names
(`mount` overrides only `wal_len` from the on-disk superblock; the budgets come from the opts).

### S-9 — keeping non-production builders non-triggering

Exactly **two** builders needed pinning (the rest are immune — see the blast-radius findings):

- **`test_opts()` (the shared cas fixture)** — pinned `op_count_bound: u64::MAX, staleness_ns:
  u64::MAX`, making its existing doc-promise ("preserves the pre-B12 flush behavior") literally true
  again. This immunizes every cas-internal test; the bound-specific tests already override their own
  field.
- **`mkfs/src/main.rs`** — kept `wal_len: 1 MiB` (documented as a deliberate batch tune) and pinned
  `op_count_bound: u64::MAX, staleness_ns: u64::MAX`: those per-write *triggers* are
  long-running-server memtable mechanisms, meaningless for a single-shot image build that snapshots
  at the end, and staleness keyed off arbitrary historical host file mtimes would inject
  nondeterministic intermediate commits. The byte budgets keep their defaults (they bound peak
  populate memory, deterministically by content).

The `//!` MVP-disclosure block is unchanged: it is already down to oversized-bypass / allocator /
GC (B12C/E retired the M-5/M-7 entries), and B12F retires none.

## Verification (all green, run locally)

| Check | Result |
|---|---|
| `cargo test -p cas` | **89 lib** (88 prior + 1 new format test) + 9 fuzz_corpus + 10 fuzz_regressions — all pass |
| `cargo test -p mkfs` | **2** pass — the existing image round-trip + the new undersized-image clean-`FAILURE` test |
| `cargo test -p storage-server` | **19** pass — default flip inert (small mtimes / few ops) |
| `cargo test -p virtio-blk` | **4** pass |
| `cargo verus verify -p cas --no-default-features` | **65 verified, 0 errors** — unchanged |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --lib format_refuses_undersized_device_without_panic` | **1 passed** — no UB in the checked-arithmetic geometry path |
| `… miri test -p cas --test fuzz_regressions --test fuzz_corpus` | clean — codec/mount/GC corpora unaffected (B12F is format-stable) |
| aarch64 cross-build (`cd kernel && cargo build`) | clean — `storaged` links the new defaults |
| `scripts/run-demo.sh` (QEMU, mkfs image + virtio-blk) | green: `[storaged] store mounted → serving`, a `write/sync/cat` round-trip, no panic |

New tests:
- **`cas/src/store.rs` `format_refuses_undersized_device_without_panic`** — `format` returns
  `Err(DeviceTooSmall)` for devices `<= floor` (0, 8 KiB, `floor-1`, `floor`) *and* for the
  `wal_len = u64::MAX` overflow edge, while `floor+1` formats `Ok`. The Miri UB witness for the
  geometry arithmetic.
- **`mkfs/tests/image.rs` `refuses_undersized_image_cleanly`** — invoking the mkfs binary
  (`CARGO_BIN_EXE_mkfs`) on a 0 MiB image exits **code 1** (clean `ExitCode::FAILURE`), not a
  panic/abort (101 / signal), and prints `mkfs: device too small for the requested geometry`.

## Key findings

1. **WAL replay never auto-flushes, so the default flip is inert for every mount-only site.**
   `mount`'s replay loop applies records via `apply_to_overlay` + `account_op` directly
   (`store.rs:~1707`), *not* `log_then_apply`, so the new op-count/staleness triggers cannot fire
   during recovery. This is what makes the `mount_reseal`/`mount_recovery` fuzz targets,
   `cas/tests/fuzz_corpus.rs`, `mkfs/tests/image.rs`, and storaged's *mount* immune to the S-9
   change — only the live *write* path (`storaged`'s request handling) is subject to the new bounds,
   which is the intended production behavior.

2. **Staleness keys off the per-op `mtime`, not test wall-clock — the audit's first read
   over-flagged the blast radius.** `relieve_staleness` is called inside `log_then_apply` with
   `now = op.mtime()`. Every cas and storage-server test passes tiny mtimes (0/1/2/3, ≤ a few
   thousand ns) and ≤ ~120 ops per ref, so neither the 30 s staleness bound nor the 8192 op-count
   bound can trip regardless of how long the test process runs. The genuine exposures were exactly
   the keystone fixture (`test_opts`) and the one tool that uses *real* host file mtimes (`mkfs`).
   Confirmed empirically: the storage-server suite (incl. the GC-churn `watermark_arms_gc…` test,
   40×10 KB writes to one ref) and virtio-blk pass unchanged with **no pin**.

3. **`mkfs` keeps a 1 MiB WAL because the recommended 64 MiB cannot lay out in the default 64 MiB
   image — a forced tune, not drift.** `chunk_off = WAL_OFF + wal_len`; a 64 MiB WAL in a 64 MiB
   image leaves no room for the chunk region (the new geometry check would itself refuse it). Plus
   the on-OS server has a **3 MiB heap** (`user/storaged`'s `urt::Heap<{3 * 1024 * 1024}>`) and
   recovery buffers the whole WAL region, so a 64 MiB WAL is infeasible there too. The 1 MiB tune is
   the spec-permitted "numbers are tunable," now documented as deliberate at the override site.

4. **`storaged`'s effective WAL watermark is moot for the mkfs image — pre-existing, recorded as
   future tuning.** `wal_watermark` is an absolute byte count carried in `StoreOptions` and
   inherited unchanged at mount (only `wal_len` is overridden from the superblock). So storaged's
   configured 32 MiB watermark (50 % of the *default* 64 MiB WAL) exceeds the mkfs image's tuned
   1 MiB WAL, and for that image WAL-pressure degrades to the normative full-WAL-reset fallback
   rather than 50 %-flush-the-pinner. This is **not a B12F regression** — the pre-B12 default
   watermark (16 MiB) also exceeded a 1 MiB image WAL. Making the watermark WAL-relative (a fraction,
   or recomputed from `sb.wal_len` at mount) is a B12C-level refinement; recorded in *Out of scope*.

5. **The byte budgets are nominal ceilings the 3 MiB storaged heap would hit first — also
   pre-existing.** The 128 MiB global / 8 MiB per-ref budgets are far above storaged's heap, so a
   pathological workload would exhaust the heap before reaching the budget. This was already true
   pre-B12F (per-ref was 8 MiB then too); B12F only raises the *global* ceiling, which the per-ref
   bound shadows for any single hot ref. Not introduced here; recorded.

6. **`format`'s deeper writes were already panic-free, so S-10 is a single early refusal.**
   `BlockDev::{read,write}` bounds-check via `access_range` and return `DevError::OutOfRange` past
   the device end (no panic); `ChunkStore::put`/`write_index_frame` return `NoSpace`. The `assert!`
   was the *only* panic in `format`, so converting it to a checked `Err` before any write makes the
   whole path total over geometry — and a device that clears the floor but somehow overruns later
   still surfaces a `DevError`, never a panic.

## Ledger

No edit to `doc/guidelines/verus_trusted-base.md`: B12F adds no verified surface and no new trusted
seam (the geometry check is plain Rust; the verified `validate_geometry_fields` mount chokepoint is
untouched and could not be reused at format time — `chunk_tail`/`index_off` are `0` before the index
frame is written). The cas baseline (`cargo verus verify -p cas --no-default-features` → **65/0**,
ledger line 158) is held unchanged, consistent with B12A–E. B12F is format-stable, so the fuzz
corpora are untouched and need no regeneration.

## Out of scope for B12F (recorded so it is not mistaken for a gap)

- **A WAL-relative `wal_watermark`** (finding 4) — storing the watermark as a fraction of `wal_len`,
  or recomputing it from `sb.wal_len` at mount, so storaged's effective WAL watermark tracks the
  *mounted* WAL rather than the default. A B12C-level refinement; harmless and pre-existing as-is.
- **Sizing storaged's heap to the global budget** (finding 5) — the 3 MiB heap vs. 128 MiB ceiling
  is a server-provisioning concern, not a flush-policy one.
- **An async `FULL` backpressure return, a verified ring-arithmetic core, an armed-timer staleness
  trigger** — the rest of B12's recorded future work (Design decisions 2/4/5). The cas gate stays
  65/0.
- **B12 is complete with B12F.** M-3…M-7 + S-9 + S-10 are all closed across B12A–B12F; the rev1§4.4
  flush/memtable policy is conformant and proptest-/crash-injection-/Miri-routed.
