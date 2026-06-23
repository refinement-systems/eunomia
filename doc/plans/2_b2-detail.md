# Plan — Part B2 detail: virtio-blk completion-poll correctness + driver test tier (volatile used-ring read, async fake, ring/descriptor proptests, optional LBA pre-check)

Detailed, separately-implementable decomposition of **Phase B2** from
`doc/plans/0_address_audit_rev0.md`. B2 is Wave-1 work: a confirmed driver
correctness hazard (`I-4`), plus the test-coverage gap that let it escape, plus one
small adjacent spec-conformance item the parent plan folds in.

**Closes (from the parent plan):**
- `I-4` [medium] — the used-ring completion poll uses a **non-volatile** load the
  compiler may hoist out of the spin loop, so the loop can never observe the device's
  update (`doc/results/0_audit_rev0.md` §2.1).
- virtio-blk proptest/Loom/Miri gap [medium] — rev1§6's "Miri + proptest — everything"
  baseline is unmet for the driver's ring arithmetic, descriptor-chain construction,
  and `u16` index wrap; the synchronous fake never runs `complete()` *as a loop*, which
  is *why* `I-4` escaped (`audit` §4.2).
- `S-11` [spec→code, optional] — virtio-blk does not bounds-check LBA against device
  capacity, relying on the device to error (`audit` §5).

**Spec target (already blessed in rev1 — B2 only conforms code to it):**
- **rev1§2.5** — the driver is written exclusively against `DmaPool`; the pool mediates
  all CPU access and is "the single place PAs are visible". The real-hardware
  cache-maintenance/barrier debt is disclosed there and is **separate** from the
  compiler-reordering hazard B2 fixes.
- **rev1§3.6** — on the OS the device interrupt binds to a notification ("bind, poll
  once, then wait"); the poll-once completion check B2 factors out is the primitive that
  path reuses. The IRQ wiring itself is **B-IRQ**, not B2.
- **rev1§4.8** — `flush` → `VIRTIO_BLK_T_FLUSH` is the fsync barrier the storage stack
  trusts; B2 must not regress it.
- **rev1§4.x note** (S-11, from Phase A3) — virtio-blk relies on the device as ground
  truth for its own geometry; an optional defensive LBA bound is **permitted, not
  mandated**.

Because Part A is blessed first (the parent plan's hard dependency), **B2 makes no spec
edits** — the rev1 text above is the fixed target. Every citation here is `rev1§`.

**Primary files:** `virtio-blk/src/lib.rs` (the driver: `complete()` :256, the spin
:263, `request()` :223), `virtio-blk/src/fake.rs` (the host fake device), and
`dma-pool/src/lib.rs` (the `DmaPool` wrapper :1250, `read` :1294, `bytes` :1277 — where
the volatile-read primitive lands). Secondary: `virtio-blk/tests/*`,
`virtio-blk/Cargo.toml`, and a confirming-comment touch in `user/storaged/src/main.rs`.

---

## Verification tier & baseline (applies to all sub-phases)

Per rev1§6 routing, virtio-blk is a **device driver**: the baseline is **Miri +
proptest**, and the driver logic is pure sequential ring/descriptor arithmetic over the
fake's shared memory, so **proptest (+ Miri replay)** is the load-bearing tier. Four
honesty notes recorded up front so nothing is silently dropped or over-claimed:

- **No Verus obligation.** The driver is not a CAS/IPC/DMA/kernel *chokepoint* (rev1§6),
  so it is not routed to Verus. The one dma-pool touch (`read_volatile`, B2A) lands in
  the **`DmaPool` wrapper**, which the crate's own module doc already designates the
  trusted PA/backing seam that "stays plain Rust" (`dma-pool/src/lib.rs:18-35`). The
  volatile load *is* a raw-pointer hardware-seam operation — it cannot be Verus logic —
  so it adds no proof obligation. The verified `FreeList<N>` core is untouched; the
  **regression gate `cargo verus verify -p dma-pool` ≥ 26/0 must still hold** (it will —
  no `verus!{}` code changes).
- **No Loom/Shuttle target.** The used ring is a device⇄CPU shared-memory handshake, not
  a multi-thread Rust atomic protocol: the device is not a Rust thread, and the host
  fake is **single-threaded by `SharedMem`'s safety contract**
  (`dma-pool/src/lib.rs:1316-1323` — "test harnesses are single-threaded and never hold
  overlapping slices"). The driver contains no atomics. The volatile-read + acquire-fence
  discipline is asserted **structurally** (the read is `read_volatile`) and exercised
  **behaviourally** (the async fake, B2B); a Loom model would have to fabricate a device
  thread over `UnsafeCell` that the production code never has. B2 records this decision
  rather than adding a no-value harness (same posture as B1's rights-lattice note).
- **Compiler-reordering vs cache-maintenance — distinct hazards.** `I-4` is the
  *compiler* legally hoisting a loop-invariant load; the fix is `read_volatile` + an
  `Acquire` fence. This is **not** the rev1§2.5/§8.1 cache-coherence/DMB debt (cache
  maintenance owed on real hardware). On the QEMU target memory is coherent, so only the
  compiler hazard is live; B2 closes exactly that and leaves the disclosed real-hardware
  barrier debt where rev1§2.5 records it. (The audit makes this separation explicit.)
- **Coordinate with B4 (same crate, no conflict).** B2A adds `DmaPool::read_volatile` as
  a sibling of `read`/`bytes`; **B4** adds the pool-identity/extent soundness guard to
  `bytes`/`bytes_mut` and discharges the `FreeList` wrapper preconditions. B2A writes
  `read_volatile` to route through the *same* `cpu_base().add(buf.offset + off)` shape so
  B4's guard drops into one place uniformly. B2 does **not** add the guard (that is B4's
  finding); the two phases touch adjacent lines, not the same logic.

**Baseline to re-establish at end of B2:** `cargo test -p virtio-blk` green (the 4
existing `tests/driver.rs` cases + the new proptests + the async-completion test);
`cargo test -p dma-pool` green and `cargo verus verify -p dma-pool` ≥ 26/0; a Miri replay
of the new driver tests clean
(`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p virtio-blk --features std
--test driver --test ring_props`). The heavy full-stack `storage_engine_runs_over_virtio`
case stays native-only (it drives interpreted BLAKE3 through the whole `cas` engine —
hours under Miri); it is gated out of the Miri replay, not deleted (honesty: the Miri
target is the *driver*, not the storage engine — that is `cas`'s own Miri sweep).

---

## Design decision 1 — where the volatile read lives, and why not just make `read` volatile *(resolve in B2A)*

The parent plan says "replace the non-volatile used-ring load … with a
`read_volatile`/atomic-acquire read … keep `spin_loop()` as the pause hint." Three
candidate placements; B2A pins the design:

- **Adopted:** a **dedicated `DmaPool::read_volatile(buf, offset, out)`** accessor — a
  byte-wise volatile load — used *only* for the spin-polled field (the used-ring index,
  and any future device-written flag). The driver keeps the no-raw-pointers seam
  (rev1§2.5: "drivers never hold raw pointers into DMA memory"); the volatile load stays
  inside the pool, the one place PAs/raw pointers are sanctioned. The driver pairs it with
  a single `core::sync::atomic::fence(Acquire)` the instant a new index is observed,
  before any status/payload read — so the device's pre-index writes are not reordered
  after the observation. (`urt/src/time.rs:322` already uses `fence(Ordering::Acquire)`
  for the seqlock — same vocabulary, established in the tree.)
- **Rejected — make `read`/`bytes` themselves volatile.** `bytes()` returns a `&[u8]`
  slice (no per-element volatility) used for *bulk* descriptor/header/data copies; making
  every pool access volatile is both wrong (a slice can't express it) and a needless
  pessimisation of the non-polled paths. Only the spin-polled scalar needs volatility.
- **Rejected — raw `read_volatile` in the driver.** Breaks the rev1§2.5 seam (the driver
  would hold a raw pointer into DMA memory) and duplicates the `cpu_base().add(offset)`
  arithmetic B4 wants to guard in one place.

Why a *fence*, and only one: the polled index is the single value that must be re-loaded
each iteration (volatile handles that); once it advances, the **one-shot** status byte and
data payload are read exactly once under the acquire fence, so they need no volatility,
just ordering after the index observation. **Recommendation: adopt the dedicated pool
accessor + acquire fence.**

---

## Design decision 2 — how the fake completes asynchronously so `complete()` runs as a loop *(resolve in B2B)*

The synchronous fake processes the queue **inside** `write32(QUEUE_NOTIFY)`
(`fake.rs:193` → `process_queue`), so by the time `complete()` first reads the used-ring
index it is already advanced: the spin body executes **zero** times. That is precisely why
`I-4` escaped — the loop was never a loop. To exercise it we need the fake to complete
*after* the driver has polled a stale index at least once. The host harness is
**single-threaded** (`SharedMem` contract), and a blocking `complete()` that spins on DMA
memory only touches `self.pool` between iterations — it never re-enters the fake — so
nothing can tick the device from inside a naive blocking spin without threads.

Two faithful single-threaded resolutions; B2B pins the design:

- **Adopted — factor a non-blocking poll-once primitive + a deferred fake the test ticks
  between polls.** Split the driver's completion into:
  - `submit(...)` — everything up to and including the doorbell (build header,
    descriptors, publish the avail ring, `QUEUE_NOTIFY`);
  - `poll_used(&mut self) -> bool` — the **single volatile** used-index read + compare +
    (on advance) the `Acquire` fence; the I-4 fix lives here;
  - `finish()` — advance `last_used`, ack ISR, read the status byte;
  - `complete()` = `while !self.poll_used() { spin_loop() }; self.finish()` (production
    blocking path, behaviour identical to today);
  - `request()` = `submit(); complete()` (the simple synchronous API is unchanged).

  Add a deferred mode to the fake: in deferred mode `QUEUE_NOTIFY` *stages* (sets a
  pending flag) instead of processing, and `device_step()` runs the staged
  `process_queue()` (bumping the used index). The async test then interleaves
  `submit` → `try_complete()` (→ `None`, a stale poll) → `device_step()` →
  `try_complete()` (→ `Some(Ok)`), with sequential `&mut` borrows and **no** `Rc`/thread
  plumbing. `poll_used` (and the `Acquire` fence and `u16` compare) is exercised across a
  real stale→fresh transition.

  This is **forward-useful, not test-only**: rev1§3.6's OS path is exactly
  "wait on the IRQ notification, then poll once" — `poll_used`/`try_complete` is that
  primitive, which **B-IRQ/C-M9 reuse** to retire the busy-spin. So the seam earns its
  keep beyond the test.
- **Rejected — a waiter closure injected into the blocking loop.** `complete_with(|| …)`
  driven by a closure that ticks the fake is more "the exact production loop ran", but the
  closure must reach the fake while `complete()` holds `&mut self`, forcing the fake behind
  `Rc<RefCell<…>>` and the `Mmio` impl into a handle — a heavier refactor for the same
  coverage. Noted as the alternative if a future phase wants the blocking loop itself under
  test; not needed for B2.

A runtime test **cannot** reliably distinguish the volatile from the non-volatile load
(whether the optimiser hoists is build-dependent and won't reproduce under `cargo test`),
so the I-4 fix's guarantee is **structural** (the read is `read_volatile`); the async fake
buys the **coverage** the parent plan asks for — the loop runs as a loop, over a delayed
completion, with the index-wrap and chain logic exercised for real. **Recommendation:
adopt the split + deferred fake.**

---

## Sub-phase B2A — the volatile used-ring poll *(closes I-4)*

The headline correctness fix. Self-contained and mergeable alone: after B2A the spin loop
re-reads the device's used-index every iteration and orders the dependent reads with an
acquire fence. Atomic across two crates by necessity — the driver cannot poll correctly
without the pool primitive.

- **Touches:**
  - `dma-pool/src/lib.rs` — add `DmaPool::read_volatile` next to `read` :1294 (byte-wise
    `read_volatile` over `cpu_base().add(buf.offset + offset)`, the same shape B4 will
    guard); a doc comment stating *why* (non-volatile `read` is hoistable out of a poll
    loop) and the rev1§2.5 cache-debt/compiler-hazard split.
  - `virtio-blk/src/lib.rs` — refactor `complete()` :256 into `poll_used()` (volatile
    read + `core::sync::atomic::fence(Ordering::Acquire)` on advance) and `finish()`
    (advance `last_used`, ack ISR, read status); `complete()` becomes the
    `while !poll_used() { spin_loop() }; finish()` loop; `request()` :223 unchanged in
    behaviour. Keep `core::hint::spin_loop()` as the pause hint (:263).
  - `user/storaged/src/main.rs:138` — **no code change**; add a one-line comment that the
    OS poll observes the device update via the pool's volatile read and that the
    IRQ-driven wait between polls arrives with B-IRQ/C-M9 (rev1§3.6).
- **Depends on:** Part A blessed (rev1§2.5 text). No intra-B2 dependency.
- **Work:**
  1. Pool primitive:
     ```rust
     /// Volatile CPU load of a device-written field — the used-ring index, an
     /// ISR-style flag. Plain `read()`/`bytes()` are non-volatile loads the
     /// optimizer may hoist out of a spin loop (it cannot see the device's
     /// concurrent write), so a poll on them can never observe completion; this
     /// re-reads memory every call. Order the payload the field gates with an
     /// `Acquire` fence on the caller side. (rev1§2.5: the real-hardware
     /// cache-maintenance/barrier debt is separate and tracked there; on the
     /// QEMU target memory is coherent and only the compiler hazard is live.)
     pub fn read_volatile(&self, buf: &DmaBuf, offset: usize, out: &mut [u8]) {
         let base = unsafe { self.backing.cpu_base().add(buf.offset + offset) };
         for (i, b) in out.iter_mut().enumerate() {
             *b = unsafe { base.add(i).read_volatile() };
         }
     }
     ```
  2. Driver poll-once (the fix):
     ```rust
     fn poll_used(&mut self) -> bool {
         let mut idx = [0u8; 2];
         self.pool.read_volatile(&self.used, 2, &mut idx);
         if u16::from_le_bytes(idx) != self.last_used {
             core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
             true
         } else {
             false
         }
     }
     ```
     `complete()` loops on `poll_used`; `finish()` does the post-completion work
     (`last_used.wrapping_add(1)`, the ISR ack at :266-269, the status read at :270-276)
     exactly as today, now reached only after the acquire fence.
- **Acceptance:**
  - The 4 existing `tests/driver.rs` cases stay green (behaviour identical — the
    synchronous fake still completes before the first poll; the loop body runs zero or one
    times as before).
  - `flush()` still maps to `VIRTIO_BLK_T_FLUSH` and the `flush_count` assertion in
    `sector_roundtrip_and_flush` holds (rev1§4.8 not regressed).
  - `cargo verus verify -p dma-pool` ≥ 26/0 and `cargo test -p dma-pool` green (the new
    method is unverified wrapper glue; the `FreeList` proof is untouched).
  - Structural check (the guarantee): the used-index poll reads via `read_volatile`, and
    the status/data reads sit after the `Acquire` fence. (No runtime negative control —
    the hoist is optimiser-dependent; the async-loop coverage is B2B.)
- **Effort/Risk:** S / low. The single correctness change — closes the confirmed driver
  hazard on the spec's QEMU target.

---

## Sub-phase B2B — async fake + driver test tier (proptest + Miri) *(closes the rev1§6 driver-baseline gap)*

The coverage gap that let I-4 through. B2B makes the fake complete asynchronously so
`complete()`'s poll runs as a loop, and adds the proptest tier rev1§6 mandates over the
ring/descriptor/`u16`-wrap arithmetic.

- **Touches:**
  - `virtio-blk/Cargo.toml` — add `proptest = "1"` to `[dev-dependencies]` (matches
    `cas`/`storage-server`/`urt`).
  - `virtio-blk/src/fake.rs` — deferred mode: a `deferred: bool` (or a `pending_steps`
    countdown) field; in deferred mode `write32(QUEUE_NOTIFY)` (:193) sets a pending flag
    instead of calling `process_queue`; `pub fn set_deferred(&mut self, on: bool)` and
    `pub fn device_step(&mut self)` (runs the staged `process_queue` once). Default mode
    is byte-for-byte today's behaviour, so existing tests are unaffected.
  - `virtio-blk/src/lib.rs` — factor the inline avail-ring slot arithmetic (:244) into a
    pure `pub(crate) fn avail_ring_slot(idx: u16, qsize: u16) -> usize { 4 + (idx % qsize)
    as usize * 2 }` so it is directly proptest-addressable and self-documenting (mirrors
    B1B's `attenuate` extraction); make `submit`/`poll_used`/`finish`/`try_complete`
    reachable for the async test (recommend: `submit` and `try_complete` **`pub`** — the
    OS IRQ path uses them, rev1§3.6; `mmio_mut(&mut self) -> &mut M` std/test-gated as a
    test affordance to reach the fake after construction).
  - new `virtio-blk/tests/ring_props.rs` (the proptest tier) and an async-completion test
    (in `tests/driver.rs` or a new `tests/async_complete.rs`).
- **Depends on:** B2A (it tests `poll_used`/`read_volatile` and the loop they form).
- **Work:**
  - **The async-completion test (the I-4 escape, now caught as a loop).** Build the driver
    over a fake in deferred mode; `submit` a request; assert the **first** `try_complete()`
    is `None` (the loop sees a stale index — `poll_used` ran with the old `last_used`);
    `device_step()`; assert the next `try_complete()` is `Some(Ok(()))` and the data round-
    trips. Assert ≥ 1 stale poll occurred, so the test fails if `submit` ever completes
    eagerly. Run the read, write, and flush request shapes (3-desc data chain and 2-desc
    no-data chain).
  - **Property 1 — descriptor-chain round-trip.** proptest over `(req ∈ {read, write},
    lba within capacity, len = k·SECTOR ≤ max_transfer)`: drive the request against the
    fake (synchronous mode) and assert the bytes match a model `Vec<u8>` disk — exercising
    real chain construction (head/data/status flags, `DESC_F_NEXT`/`DESC_F_WRITE`) and the
    status byte. Include `len == 0`/`flush` (the 2-desc chain).
  - **Property 2 — ring arithmetic & `u16` wrap (Miri-cheap, pure).** proptest
    `avail_ring_slot(idx, qsize)` over **all** `idx: u16` × `qsize ∈ 1..=8`: assert
    `4 <= slot` and `slot + 2 <= 6 + 2*qsize as usize` (in-bounds of the avail buffer
    `pool.alloc(6 + 2n, 2)`). Assert `last_used`/`avail_idx` wrap consistency:
    `(0u16..).fold` over `wrapping_add(1)` returns to the start after `1<<16` steps and the
    slot stays in range throughout — proven on the *pure* helper, not via 65536 device ops
    (so it stays cheap under Miri's interpreter).
  - **Property 3 — index wrap behaviourally (native-only, native scale).** A native-scale
    proptest (or a single high-count case) issuing `> queue_size` and across-`u16`-wrap
    counts of small requests, asserting no desync between `avail_idx` and the fake's
    `used_idx`. Gate the high iteration count behind `!cfg!(miri)` (under Miri this case
    drops to the 4-case floor like the others; the wrap *arithmetic* is already covered by
    Property 2).
  - Use the workspace Miri convention for case counts:
    `#![proptest_config(ProptestConfig { cases: if cfg!(miri) { 4 } else { 256 },
    ..ProptestConfig::default() })]` (mirrors `cas/src/file.rs:121-123` and
    `storage-server/tests/rights_lattice.rs:38-40`).
  - **Miri target.** Add virtio-blk to the Miri replay (the fake's `unsafe` `SharedMem`
    slices + the new `read_volatile` are exactly what Miri validates). `storage_engine_…`
    stays native-only (interpreted BLAKE3); record that explicitly so the Miri scope is
    honest (the driver, not the engine).
- **Acceptance:** `cargo test -p virtio-blk` green including `ring_props` and the
  async-completion test; the async test **provably observes a delayed completion** (≥ 1
  stale `try_complete()` before `device_step`), so removing the `device_step` interleave
  makes it spin/fail (negative control on the *test*, in the project's established style);
  ring/descriptor proptests pass at 256 cases natively and 4 under Miri; the Miri replay
  of `driver`+`ring_props` is clean.
- **Effort/Risk:** M / low. Pure test/fake addition behind the host-testable seam; the
  only production change is the `submit`/`try_complete` factoring B2A already set up.

---

## Sub-phase B2C — optional defensive LBA-vs-capacity pre-check *(closes S-11)*

Independent of B2A/B2B (touches only `read_sectors`/`write_sectors`); may land in any
order. rev1§4.x records this as **permitted, not mandated** — the device is ground truth
for its own geometry — so B2C is a small, clearly-marked optional hardening that turns a
device-dependent `DeviceError` into a deterministic local refusal.

- **Touches:** `virtio-blk/src/lib.rs` — `read_sectors` :279 and `write_sectors` :290 (the
  existing `> max_transfer` guards are the natural home for a sibling capacity check); a
  new `VirtioError::OutOfRange` variant (distinct from `TooLarge`, which means "exceeds
  `max_transfer`").
- **Depends on:** Part A blessed (rev1§4.x S-11 note). No intra-B2 dependency.
- **Work:**
  - Add, alongside the `max_transfer` check, a checked capacity bound:
    ```rust
    let end = lba
        .checked_mul((out.len() / SECTOR) as u64)        // sectors in this transfer
        .and_then(|n| lba.checked_add(n))                 // last LBA touched
        .ok_or(VirtioError::OutOfRange)?;
    if end > self.capacity {
        return Err(VirtioError::OutOfRange);
    }
    ```
    (compute against `self.capacity` sectors; use checked arithmetic so an adversarial
    `lba` near `u64::MAX` refuses rather than wraps — the same discipline as I-5/B3).
  - Keep it **optional and self-labelled**: a doc comment that the device remains ground
    truth (rev1§4.x) and this is a defensive local bound, not a correctness dependency.
- **Acceptance (test in `tests/driver.rs`):**
  - A read/write whose last LBA exceeds `capacity_sectors()` → `Err(OutOfRange)` (no device
    round-trip); an in-range transfer at the exact last sector → `Ok`.
  - The existing `blockdev_adapter_handles_unaligned_io` "read past end fails" assertion
    (:70-72) still holds — now via the local check rather than the fake's `ST_IOERR`.
- **Effort/Risk:** S / low. Drop this sub-phase if the defensive bound is judged
  unnecessary — the device error path is already safe (S-11 was a *resolved ambiguity*,
  not a defect).

---

## Execution order

```
B2A  volatile used-ring poll              [the I-4 fix; do first, mergeable alone]
  └─► B2B  async fake + proptest/Miri tier [needs poll_used/try_complete from B2A]
B2C  optional LBA pre-check               [independent; any time, incl. first]
```

- **B2A** is the load-bearing correctness fix and is independently shippable (it fully
  closes `I-4` and establishes the volatile poll in one atomic change across dma-pool +
  virtio-blk).
- **B2B** depends on B2A (it exercises the poll-once primitive and the loop it forms).
- **B2C** is fully independent (only the two sector entry points); sequence wherever
  convenient, or drop it.
- B2A and B2B *may* be reviewed as one change if preferred, but B2A alone is a complete,
  mergeable unit — keep them separable so the correctness fix can land fast.

## Out of scope for B2 (recorded so it is not mistaken for a gap)

- **The IRQ-driven completion path** — binding the device IRQ to a notification and
  *waiting* between polls instead of busy-spinning — is **B-IRQ** (the kernel device-IRQ
  object/syscalls) and **C-M9**. B2 only makes the poll *correct* (volatile + acquire) and
  factors the `poll_used`/`try_complete` primitive (rev1§3.6 "poll once, then wait") that
  the IRQ path will reuse; the bare-metal busy-spin stays the default until B-IRQ lands.
- **The dma-pool wrapper soundness/provenance hole** (cross-pool `DmaBuf`, `FreeList`
  wrapper precondition discharge, the `MAX_FREE_RANGES` runtime backstop) — **Phase B4**.
  B2A writes `read_volatile` in the same `cpu_base().add(buf.offset + off)` shape so B4's
  extent guard drops into `read`/`bytes`/`read_volatile` uniformly; B2 adds no guard.
- **Real-hardware cache maintenance / DMB barriers** — the disclosed rev1§2.5/§8.1 debt;
  B2 closes only the compiler-reordering hazard (volatile + acquire fence), which is the
  live one on the coherent QEMU target. The cache/barrier work is not B2.
- **Multi-in-flight / batched requests** — the MVP is one synchronous in-flight request
  (rev1§2.5); deeper queueing is future work, not a B2 gap.
- **Loom/Shuttle for the driver** — deliberately omitted (the device is not a Rust thread;
  the host fake is single-threaded by contract; the discipline is structural + behavioural)
  per the verification-tier note above.
