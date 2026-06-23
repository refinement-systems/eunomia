# Plan — Part B12 detail: memtable / flush-policy conformance + the refuse-not-panic format contract (per-ref accounting under a global budget, low/high size watermarks with flush-the-biggest-offenders, a circular WAL with flush-the-pinner at a 50% watermark, an operation-count secondary bound and a staleness timer, neighborhood-only re-chunking, the recommended-defaults table, and a `format` that returns an error instead of panicking — bringing the collapsed single-budget MVP up to rev1§4.4's *mandatory* mechanisms; the proptest + crash-injection + Miri tier, **format-stable**, no Verus change)

Detailed, separately-implementable decomposition of **Phase B12** from
`doc/plans/0_address_audit_rev0.md`. B12 is **Wave-4** work. Its one hard dependency was the
**A2 decision** (mechanisms mandatory vs. disclosed-simplification); that decision is now
**resolved in the blessed spec** (see *Spec target*), so B12 is the *full implementation*
path, not the doc-only shrink. Otherwise B12 is self-contained — it depends on nothing else
in Part B and nothing depends on it, except **C4** (concurrent/incremental GC), which is
already gated on B6, not B12.

It closes the largest cluster of *conformance* gaps the audit found in a subsystem the spec
calls fully-specified: the rev0§4.4 flush/memtable policy, which the code "collapsed to the
simplest correct policy" — one global budget, flush-everything on pressure — while the spec
frames its mechanisms as *fixed*.

**Closes (from the parent plan).** Verbatim from `doc/results/0_audit_rev0.md`:

- **(M-3 … M-6) The rev0§4.4 flush/memtable policy is largely collapsed to a single global
  budget [medium, confirmed]** (audit §3.1, lines 337–357). rev0§4.4 "calls its mechanisms
  *fixed* (only the numbers tunable), yet":
  - **M-3 — no per-ref soft bound.** "`StoreOptions` exposes only one global `overlay_budget`;
    one ref can consume the whole budget, defeating the per-ref containment the spec attributes
    to per-ref quotas (`cas/src/store.rs:110,1352-1357`)."
  - **M-4 — size pressure flushes everything.** "not 'the biggest offenders' at a low
    watermark; there is one hard threshold → `sync_all()` (flush every ref + commit), with no
    low/high watermark split (`cas/src/store.rs:1352-1357`; the code calls this 'collapsed to
    the simplest correct policy')."
  - **M-5 — WAL-pressure flush-the-pinner and the 50 % watermark are absent.** "flushing
    triggers only at a completely full WAL and then flushes all refs; there is no intermediate
    watermark and no per-ref oldest-WAL-position sort key (`cas/src/store.rs:1325-1339`)."
  - **M-6 — no operation-count secondary bound and no timer/staleness trigger** "exist at all
    (rev0§4.4 mandates both)."
  - The audit's own framing: "These are partly acknowledged as MVP debt in
    `cas/src/store.rs:20-32`, but the spec's framing ('the mechanisms above are fixed') makes
    them conformance gaps, not tuning choices."
- **(M-7) Flush re-chunks whole dirty files** [medium] (audit §3.1, lines 359–361): "not the
  rev0§4.3 'affected neighborhood only' path (`cas` overlay flush) — acknowledged MVP perf
  simplification, but it changes the write-amplification behavior the spec describes."
- **(S-9) Shipped defaults diverge from every rev0§4.4 number** [soft] (audit §5, lines
  583–587): "`StoreOptions::default` uses 16 MiB WAL / 8 MiB budget (mkfs overrides to 1 MiB
  WAL) vs the spec's 64 MiB / 128 MiB / 8 MiB-per-ref. The numbers are explicitly tunable, so
  this is soft, but no shipped default matches the spec, and the single budget conflates the
  spec's per-ref and global figures."
- **(S-10) `mkfs` and `format` panic on an undersized device** [spec-gap] (audit §5, lines
  588–592): "instead of the clean `Result`/`ExitCode::FAILURE` path that exists
  (`mkfs/src/main.rs:74` → `cas/src/store.rs:903` `assert!`). rev0§4.5 makes refuse-not-panic
  the discipline for *mount* over arbitrary contents; `format` has no analogous contract, so
  this is a spec gap the code falls into rather than a violation." (The §4.5 contract is now
  added in rev1 by Phase A3/S-10; B12 implements it.)

The audit's follow-up action (7) (line 699) names exactly this phase: "restore the
**per-ref/watermark flush mechanisms** or move them onto a disclosed-simplification list
(M-3…M-6)" — and A2 chose *restore*.

---

## Spec target — A2 resolved *mandatory*; B12 conforms code to the blessed rev1 text

Part A is blessed first (the parent plan's hard dependency), so **B12 makes no spec edits** —
the rev1 text below is the fixed target and every citation here is `rev1§`. The decisive fact
that shapes this entire phase: **Open Decision 1 / Phase A2 was resolved in favour of
mandatory mechanisms.** rev1§4.4's closing paragraph (spec line 266) states verbatim:

> **The triggers and bounds above are mandatory mechanisms; the numbers below are recommended
> defaults — which the storage server's shipped configuration matches and which a store may
> tune — not part of the mechanism**: per-ref soft bound 8 MiB, global budget 128 MiB, WAL 64
> MiB with flush-the-pinner at 50%, timer 30 s.

So the four triggers and the two bounds are *architecture*, not a tuning choice the code may
collapse. The blessed normative text B12 conforms to:

- **rev1§4.4 — bounds.** "Per-ref overlays under a global byte budget, charged to sessions …
  The global budget exists because memory is finite; per-ref soft quotas under it provide
  containment." (M-3.) "**Bounds** are denominated in bytes of dirty overlay … with an
  **operation-count secondary bound** so that metadata storms cannot hide under a small byte
  count." (M-6, op-count half.) "On hitting a bound the response is **backpressure, not
  eviction**: the write gets `FULL` or blocks at the IPC layer while a flush runs."
- **rev1§4.4 — flush triggers, in priority order:**
  1. **Explicit** — `sync`/`snapshot`/`rollback` flush that ref synchronously. *Already
     implemented* (`store.rs` `sync`/`snapshot`/`rollback`); B12 leaves it.
  2. **WAL pressure** — "When WAL usage crosses a watermark, **flush the ref pinning the
     tail**, and repeat until comfortable. The server tracks **per-ref oldest-WAL-position** as
     the scheduler's sort key. Two edge cases are normative: a record larger than the entire
     WAL region bypasses the log and commits synchronously before acknowledgment; a full WAL
     flushes everything and resets the log." (M-5.)
  3. **Size pressure** — "A per-ref quota or global watermark crossed flushes the **biggest
     offenders**. Start flushing at a **low watermark**, so writers rarely hit `FULL` at the
     high one." (M-4.)
  4. **Timer** — "A staleness bound, so a quietly dirty ref eventually becomes committed
     tree." (M-6, timer half.)
- **rev1§4.3 — mutation path, step 3 (flush).** "For each dirty file, **re-chunk the affected
  neighborhood only**: back up one chunk before the first dirty byte, run the chunker forward,
  and stop when an emitted boundary coincides with an existing one (CDC self-synchronization
  guarantees this within a few chunks). A 200-byte edit in a 1 GiB file yields ~2–4 new
  chunks." (M-7.)
- **rev1§4.5 — initialization contract.** "Creating a fresh store — `format`, and the
  host-side `mkfs` that wraps it (§7) — validates the requested geometry against the device
  before writing anything. A device too small … or a geometry that cannot be laid out within
  the device, **is refused with an error, never a panic**: `format` returns a clean error
  result, and the host tool exits with a failure status. Mount is total over arbitrary device
  *contents*; `format` is total over arbitrary device *geometry*." (S-10.)
- **rev1§6 routing.** The flush policy is server/store *policy* — not a decoder (rev1§3.7), not
  an on-disk codec, not a recovery decision core. Its tier is therefore the "everything gets
  Miri + proptest" baseline plus the storage layer's established **crash-injection proptest**,
  *not* a new Verus chokepoint (Design decision 2).

**Recommended-defaults table (the S-9 target), rev1§4.4 line 266:** per-ref soft bound **8
MiB**, global budget **128 MiB**, WAL **64 MiB** with flush-the-pinner at **50 %**, staleness
timer **30 s**. The shipped storage-server config must match these; a store may tune them.

---

## Primary files (current line numbers — the audit/parent-plan citations predate code drift)

- `cas/src/store.rs` — the whole of the flush mechanism:
  - `StoreOptions` struct + `Default` (`:155-171`): today `{ wal_len: 16 MiB, chunker,
    overlay_budget: 8 MiB }` — the **single global budget** to be reshaped (M-3/M-4/S-9).
  - The **MVP-simplification disclosure** doc-comment block (`:20-32`): a `//!` bulleted list
    that today discloses whole-file re-chunk (M-7), the linear-not-circular WAL "flush
    everything and reset" (M-5), the oversized-write bypass, the first-fit allocator
    high-water, and synchronous GC. B12 **retires the M-5 and M-7 entries** (the simplifications
    they disclose are removed) and **leaves** the oversized-bypass, allocator, and GC entries
    (those remain accepted MVP posture; the oversized-bypass is in fact a *normative* edge case
    rev1§4.4 keeps).
  - **Size-pressure** (`:1923-1928`, inside `log_then_apply`): `let total = Σ overlay.bytes();
    if total > overlay_budget { sync_all()? }` — the single hard threshold → flush-everything,
    self-described "collapsed to the simplest correct policy" (M-4).
  - **WAL-pressure** (`:1905-1910`, inside `log_then_apply`): `if wal_tail + rec.len() >
    wal_len { sync_all()?; debug_assert_eq!(wal_tail, 0); }` — flush everything, reset to 0
    (M-5).
  - `format` (`:1396-1449`): the `assert!(dev.len() > chunk_off + 4096, "device too small")` at
    **`:1398`** — the panic to replace with a checked `Result` (S-10). Note `format` already
    *returns* `Result<Store<D>, StoreError>`; only the geometry check panics.
  - `flush_ref` (`:2108-2137`): re-chunks each dirty file by reading the **whole** old content,
    applying the overlay, and calling `make_file_entry` over the entire content (M-7).
  - `touch_ref` (`:1681-1683`) / `tag` (`:1766-1775`): the per-ref dirty-set hook (`dirty_refs`,
    bumped once per ref per commit by B5's `edit_version`); B12 hangs per-ref *accounting* off
    the same set.
  - The crash-injection proptest `crash_recovery_preserves_acked_state` (`~:2217`, the B5/B6
    extension point): B12 extends its op set with the *selective/partial* flush paths.
  - `Overlay::bytes()` already exists (used by the size-pressure sum); per-ref byte accounting
    is therefore already half-built — B12 adds per-ref op-count and per-ref oldest-WAL-position.
  - The `Superblock` already carries **`wal_head`** (set `0` in `format`, `:1430`); rev1§4.3's
    commit step already speaks of "WAL head advanced past the contiguous prefix" — so the
    circular WAL (B12C) needs **no on-disk format change** (Design decision 1).
- `mkfs/src/main.rs` (`:60-112`): `run()` returns `Result`, `main()` maps it to
  `ExitCode::{SUCCESS,FAILURE}` (`:104-111`); `Store::format(dev, opts)?` (`:77`) today
  *panics* through the `assert!` rather than returning `Err`, so the clean `ExitCode::FAILURE`
  path is unreachable for an undersized image (S-10). `wal_len` is overridden to 1 MiB
  (`:73-76`) — the S-9 mkfs divergence.
- `user/storaged` (the shipped storage server) — constructs the runtime `StoreOptions` it
  mounts with; its config is the one rev1§4.4 says "the storage server's shipped configuration
  matches" (S-9).
- `doc/guidelines/verus_trusted-base.md` — the ledger (Baselines table `:151-164`). B12 adds
  **no** verified surface and **no** new trusted seam (Design decision 2); the only ledger
  touch is recording that the flush policy moved from "collapsed MVP, partly disclosed" to
  "conformant, proptest+crash-injection-routed" and that the **M-5/M-7 disclosures are
  retired**. The cas gate (`cargo verus verify -p cas --no-default-features`, **65/0**, line
  158) is held unchanged.
- `CLAUDE.md` — no change (the cas Miri sweep already names `cas`; B12's new proptests ride it).

---

## Verification tier & baseline (applies to all sub-phases)

The flush policy is **plain-Rust scheduler/policy code outside any `verus!{}` block** — like
B6's GC mechanism and unlike B5's on-disk codec. Five honesty notes up front so nothing is
silently dropped or over-claimed:

- **B12 is *format-stable* — no `SB_VERSION` bump, no corpus regen (the B6 posture, not B5's).**
  Every datum B12 introduces — per-ref overlay bytes, per-ref op-count, per-ref
  oldest-WAL-position, per-ref oldest-dirty timestamp, the ring head/tail — is **runtime
  scheduler state**, reconstructible from WAL replay at mount (Design decision 1). Nothing is
  persisted that a recovering store must read back, so the on-disk record/superblock formats are
  untouched and the fuzz corpora (`cas/fuzz`) need no regeneration. The one place this could
  have forced a format change — a persistent WAL head for the circular ring — is *already*
  present (`Superblock::wal_head`, `store.rs:1430`); B12 only starts *advancing* it past a
  partial prefix instead of always resetting it to 0.
- **No new Verus, and the cas gate holds at ≥ 65/0 (Design decision 2).** The flush policy is
  not a chokepoint under rev1§6's routing: it decodes nothing (the WAL/superblock decoders it
  feeds are *already* in the verified surface and untouched), and it makes no recovery decision
  (the `pick_survivor`/`commit_target`/`advance_head`/`replay_bound` cores B7 verified are
  untouched). B12 adds policy *on top of* an already-verified-and-tested substrate. The cas
  verify total stays **65/0** — record it unchanged in the ledger; if any new `verus!{}`
  appears (it should not — see Design decision 2's optional ring-core note), record the new
  total instead.
- **The load-bearing *safety* invariant is unchanged and already covered; B12 changes flush
  *scheduling*.** The property that must never break — "the WAL head advances only past records
  whose effects are flushed, so every acknowledged write survives a crash" — is the rev1§4.4
  WAL invariant, modeled in `CommitProtocol.tla` (`AckedWritesRecoverable` + the B7 `Recover`
  action property) and exercised by `crash_recovery_preserves_acked_state`. B12 does **not**
  touch that invariant; it changes *which* refs flush and *when* (per-ref bound, low watermark,
  flush-the-pinner, timer) and, in B12C, lets the head advance past a **partial contiguous
  prefix** instead of resetting wholesale. The new obligation is therefore: **selective/partial
  flush still preserves all-acked-survives** — which is exactly what the crash proptest is
  extended to witness (each of B12A/B/C adds its selective-flush path to the op set).
- **The tier is proptest + crash-injection + Miri — the storage-layer convention, not fuzz.**
  The policy consumes no untrusted wire/disk bytes (its inputs are the in-process write stream
  and the clock), so the rev1§3.7 "decoders are fuzz targets" routing does not apply (the fuzz
  targets stay on the codecs/mount/GC, untouched). Property tests use the workspace case-count
  convention `cases: if cfg!(miri) { 4 } else { 256 }` (mirroring `cas/src/file.rs:121-123`);
  the crash-injection extensions use the established `64 native / 4 Miri` count
  (`crash_recovery_preserves_acked_state`, B5B). No Loom/Shuttle: the storage server's dispatch
  is serialized and atomic-free (the B5/B6 note), and B12 adds no concurrency — the flush is
  synchronous (Design decision 4).
- **Backpressure is realized as a *synchronous flush that blocks the write* — the `FULL` return
  is recorded as future async work (Design decision 4).** rev1§4.4's "gets `FULL` or blocks at
  the IPC layer" admits both; for the single-threaded in-process Store the natural and
  spec-permitted realization is "blocks while a flush runs" (the flush makes overlay into tree
  synchronously, then the write proceeds). A genuine `FULL` protocol return belongs with
  async/multi-threaded flush and is out of scope (recorded, not a gap).

**Baseline to re-establish at end of B12:**
- `cargo verus verify -p cas --no-default-features` green at **65/0** unchanged (plain-Rust
  policy; no proof added or weakened).
- `cargo test -p cas` green: the rewritten size/WAL-pressure paths, the new per-ref-accounting
  / watermark / flush-the-pinner / staleness / neighborhood-re-chunk proptests, and the
  extended `crash_recovery_preserves_acked_state` (selective/partial flush in the op set).
- `cargo test -p mkfs` green including a **new undersized-device test** asserting clean
  `ExitCode::FAILURE`, not a panic (S-10).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas` clean across the new
  flush-policy proptests (4 cases under Miri) — the ring arithmetic and per-ref accounting are
  index/offset arithmetic Miri validates; and the committed `--test fuzz_regressions --test
  fuzz_corpus` sweep stays clean (format/codec/GC corpora unaffected, B12 being format-stable).
- The aarch64 cross-build links `storaged` against the reshaped `StoreOptions` and **QEMU boot
  stays green** — the live witness that the new flush policy serves the real storage server on
  the boot path (the same load-bearing acceptance B11B used for the heap).

---

## Design decision 1 — where the new accounting lives: in-memory runtime state vs. persisted on-disk *(pin in B12A; resolves the format-change question)*

Everything B12 adds is *scheduler* state; the question is whether any of it must survive a
crash on disk.

- **Adopted — all of it is in-memory, reconstructed at mount from WAL replay; B12 is
  format-stable.** Per-ref overlay bytes, op-count, oldest-WAL-position, and oldest-dirty
  timestamp are all *functions of the currently-unflushed records*, which mount already replays
  (rev1§4.5: "Replay the WAL from the recorded head to rebuild per-ref overlay state"). So the
  server recomputes them as it replays — they are derived, never stored. Decisive consequences:
  1. **No `SB_VERSION` bump, no corpus regen** (contrast B5, which added an on-disk
     `edit_version` and bumped `SB_VERSION 3→4`). The record and superblock decoders — the
     verified+fuzzed surface — are untouched.
  2. **The one datum that *is* persistent already exists.** The circular ring (B12C) needs a
     durable WAL head so recovery knows where the live window starts; `Superblock::wal_head` is
     already written by `format` (`:1430`) and already described by rev1§4.3's commit step
     ("WAL head advanced past the contiguous prefix"). The current code simply always resets it
     to 0; B12C starts advancing it partially. **No new field.**
- **Rejected — persist per-ref accounting (e.g. an oldest-WAL-position column in `RefEntry`).**
  It is redundant (replay reconstructs it), it would force a format bump and corpus regen for
  zero recovery benefit, and it would put scheduler hints into the verified on-disk codec — the
  opposite of the B7 shrink-the-trusted-surface direction. **Rejected.**

**Recommendation: keep every accounting datum in-memory and reconstruct it during WAL replay;
B12 changes no on-disk format and regenerates no corpus. The only persistent state it exercises
is the already-present `Superblock::wal_head`.**

---

## Design decision 2 — the verification tier: proptest + crash-injection + Miri, *not* a new Verus chokepoint *(pin in B12A)*

The parent plan's acceptance for B12 is test-tier ("per-ref containment demonstrated … WAL/size
watermarks exercised by tests; format refuses-not-panics"), and rev1§6's routing puts policy
below the Verus line. B12 commits to that and says why, so the choice is a decision, not an
omission.

- **Adopted — proptest + the storage-layer crash-injection proptest + Miri; cas gate held at
  65/0.** The flush policy is a *scheduler over an already-verified substrate*. The two things
  that *could* be verified — the on-disk codecs and the recovery decision cores — already are
  (B5/B7), and B12 does not touch them. What B12 adds is admission/eviction *timing*, whose only
  hard obligation (all-acked-survives under selective/partial flush) is a *crash-recovery*
  property the project verifies by **crash-injection proptest + TLA**, not by Verus over the
  policy code. So B12 routes:
  - **Per-ref containment, watermark selection, flush-the-pinner choice, op-count/staleness
    triggers** → proptest with a reference model (the property style of `cas/src/file.rs`).
  - **Selective/partial flush preserves all-acked-survives** → extend
    `crash_recovery_preserves_acked_state` (the B5B pattern: add the new flush path to the op
    set; 64 native / 4 Miri).
  - **No UB in the ring/accounting arithmetic** → the cas Miri leg.
- **The circular-WAL ring arithmetic is the one part with a Verus *temptation*, declined for
  MVP and recorded as optional future hardening.** The ring's `usage = (tail − head) mod
  wal_len`, the wrap on write, and "is this record's effects in the flushed contiguous prefix"
  are exactly the modular-arithmetic shape that off-by-ones live in — the kind of thing a small
  `verus!{}` core (à la `freelist`) could pin. B12 covers it with a **reference-model proptest**
  (a `Vec`-backed ring oracle the real ring must match across randomized write/flush/wrap
  sequences) under Miri, which is proportionate for a scheduler; a verified ring core is
  recorded in *Out of scope* as future hardening, exactly as B11 recorded `free_or_coalesce`.
  **If** that core is later judged worth it, it is additive (the gate *rises*, nothing
  weakens).
- **Rejected — a `verus!{}` proof of the flush policy as a whole.** Flush orchestration is I/O
  sequencing over the device (`flush_ref` → `commit` → `sync`), not first-order arithmetic; it
  is the same kind of orchestration B7 connected to verified *cores* via `requires/ensures`
  rather than proving end-to-end, and B12 adds no new core worth that boundary. **Rejected** —
  it would not shrink the trusted surface and would be disproportionate to a medium conformance
  finding.

**Recommendation: proptest + crash-injection + Miri; hold the cas gate at 65/0; cover the ring
arithmetic with a reference-model proptest and record a verified ring core as optional future
hardening.**

---

## Design decision 3 — the WAL becomes a circular ring (the heart of M-5) vs. stays linear-reset-only *(pin in B12C)*

M-5's "flush the ref pinning the tail … and repeat until comfortable" only *relieves* WAL
pressure if flushing one ref **frees WAL space**. Today the WAL is linear: the head can advance
only by resetting to 0 when *everything* flushes (`store.rs:1905-1910` + the MVP disclosure
`:24-27`). Flush-the-pinner is meaningless without a way to reclaim the space of the flushed
(oldest) records while later records stay live.

- **Adopted — a circular ring over `[0, wal_len)` with an advancing `wal_head`.** `wal_head` is
  the start of the oldest live (unflushed-prefix) record; `wal_tail` is the next write position;
  both wrap. `usage = (wal_tail − wal_head) mod wal_len`. Writing wraps `wal_tail`; a write that
  would overrun `wal_head` is the genuine full-WAL case. Flushing the tail-pinning ref lets
  `wal_head` advance past the now-flushed **contiguous prefix** (up to the next still-unflushed
  ref's oldest record), reclaiming exactly that span — the rev1§4.4 mechanism, realized. The
  persistent `Superblock::wal_head` already exists (Design decision 1), so this is a *runtime*
  change: ring write, ring replay (read from `wal_head` around to `wal_tail`, torn-tail
  checksum discipline preserved across the wrap), and partial head advance on commit.
  - **The two normative edge cases stay (rev1§4.4):** a record larger than the whole WAL region
    **bypasses the log and commits synchronously before ack** (already implemented + on the MVP
    list `:28-29` — kept, and it is in fact normative, not a simplification); a **completely
    full WAL flushes everything and resets** (kept as the fallback when even flushing the pinner
    cannot free enough — e.g. one ref pins the entire ring).
- **Rejected — keep the WAL linear, add selective flush without space reclaim.** Flushing the
  pinner without advancing the head past its span frees nothing until the next full reset, so the
  50% watermark trigger would fire repeatedly with no effect — it would *look* like
  flush-the-pinner while behaving like flush-everything-soon. This fails M-5's intent. **Rejected.**
- **Rejected — compact the WAL (rewrite to drop flushed records) instead of a ring.** Rewriting
  the live records to close the hole is O(WAL) I/O per flush and re-fsyncs the whole region —
  far worse than advancing a head pointer over a ring. **Rejected.**

**Recommendation: make the WAL a circular ring with an advancing `wal_head` (no format change —
the field exists); keep the oversized-bypass and full-WAL-reset edge cases; cover the wrap with
the reference-model proptest of Design decision 2 and a replay-across-the-wrap crash test.**

---

## Design decision 4 — backpressure shape: synchronous flush that blocks the write vs. a `FULL` protocol return *(pin in B12A/B12B)*

rev1§4.4: "On hitting a bound the response is **backpressure, not eviction**: the write gets
`FULL` or blocks at the IPC layer while a flush runs."

- **Adopted (MVP) — the flush runs synchronously inside the write; the write blocks, then
  proceeds.** For the single-threaded in-process Store, crossing a bound triggers a synchronous
  flush (the pinner, the biggest offenders, or the per-ref offender), which turns overlay into
  tree and relieves the pressure; the write then completes. This *is* "blocks at the IPC layer
  while a flush runs" — the IPC reply is simply delayed by the flush. No `FULL` is returned
  because the synchronous flush always relieves the bound (single-threaded, no competing
  writer), so the write never has to be refused. This is also why there is "**no eviction**":
  overlay leaves memory only by becoming tree (the flush), never by being dropped.
- **The one genuinely-refusing case stays a hard error, not silent eviction:** a per-write
  payload that alone exceeds the global budget even with everything else flushed is refused
  (`StoreError`), the analogue of the oversized-WAL-record bypass — recorded, tiny.
- **Rejected (for B12) — a `FULL` reply the caller must retry.** That belongs with
  *asynchronous* flush (flush on a background thread, refuse new writes meanwhile) or
  multi-threaded contention, neither of which exists in the MVP server. Implementing a `FULL`
  the synchronous server can never actually return would be dead protocol surface. **Deferred**
  to the async-flush future work (Out of scope), recorded so the synchronous choice is a
  decision.

**Recommendation: realize backpressure as a synchronous blocking flush (no `FULL` reply);
refuse only the pathological single-write-exceeds-budget case as a hard error; record the
async `FULL` return as future work.**

---

## Design decision 5 — where the staleness timer fires in a request-driven single-threaded server *(pin in B12D)*

rev1§4.4 trigger 4: "A staleness bound, so a quietly dirty ref eventually becomes committed
tree." The server has no background thread, so "timer" needs a firing point.

- **Adopted — opportunistic checks at request boundaries and at reactor idle, against a
  per-ref oldest-dirty timestamp.** B12A already tracks each ref's oldest-dirty UTC-nanos
  timestamp (the store already has a clamped-monotone per-ref time source — `store.rs:1700-1705`
  feeds snapshot timestamps). On each request the server checks whether any ref's oldest-dirty
  age exceeds `staleness_ns` (30 s) and, if so, flushes it before/after serving the request;
  and when the IPC reactor (rev1§3.6) would otherwise block waiting for work, it performs the
  same sweep. This needs no background thread and no new kernel facility — it is a cheap scan of
  the (small) dirty-ref set keyed by a timestamp already maintained.
- **Rejected (for B12) — an armed timer notification (the B-IRQ/timer object).** B-IRQ landed
  the hardware-timer→notification path and the storage server *could* arm a timer to wake on
  staleness. But that wires a kernel timer object into the storage server purely for a flush
  hint, when an opportunistic scan at the points the server already runs is sufficient for the
  MVP's "quietly dirty ref eventually flushes" guarantee. **Deferred** — recorded as the natural
  upgrade if a truly idle server (no requests, reactor parked indefinitely) must still flush on
  a wall-clock deadline; for the MVP, "eventually" = "by the next request or reactor wake" is
  the accepted reading.
- **Test note.** The staleness path is tested with a **synthetic/injectable clock** (advance
  time in the proptest), not wall-clock sleeps, so the test is deterministic and Miri-safe.

**Recommendation: opportunistic staleness scan at request boundaries and reactor idle against
an injectable clock; record the armed-timer-notification upgrade as future work for the
fully-idle case.**

---

## Sub-phase B12A — per-ref accounting substrate + `StoreOptions` reshape + per-ref soft bound *(foundation; closes M-3 and the op-count half of M-6)*

The substrate every later sub-phase reads from. It turns the single global budget into the
spec's per-ref-soft-bound-under-a-global-budget shape and installs the two *bounds* (bytes +
op-count). Resolves Design decisions 1, 2, 4.

- **Touches:** `cas/src/store.rs`:
  - **`StoreOptions` reshape** (`:155-171`): rename `overlay_budget` → `global_budget`; add
    `per_ref_budget`, `op_count_bound`, `size_low_watermark` (the fraction/threshold B12B
    consumes, stubbed at `global_budget` until then), `wal_watermark` (consumed by B12C, stub),
    and `staleness_ns` (consumed by B12D, stub). This is a **breaking API change** — every
    caller (`mkfs/src/main.rs`, `user/storaged`, the cas tests) updates; mechanical. Keep all
    new fields `pub` and documented with their `rev1§4.4` citation.
  - **Per-ref accounting** alongside `overlays: BTreeMap<ref, Overlay>`: a parallel per-ref
    record `{ op_count: u64, oldest_wal_pos: Option<u64>, oldest_dirty_ns: Option<u64> }`
    (bytes already available via `Overlay::bytes()`), updated where `touch_ref` (`:1681-1683`)
    and the write path already run, and **reset when that ref flushes** (its overlay becomes
    tree). Reconstructed during WAL replay (Design decision 1).
  - **Per-ref soft bound** in `log_then_apply`: after applying a write/op to ref `R`, if
    `overlays[R].bytes() > opts.per_ref_budget` **or** `op_count[R] > opts.op_count_bound`,
    flush just `R` (`flush_ref(R)` + `commit`, advancing the WAL head past `R`'s now-flushed
    records — the partial-advance machinery is B12C; until then this rides the existing
    flush-and-reset, which is correct if conservative). This is the M-3 containment: a hot ref
    self-flushes at its soft bound, so it cannot consume the whole global budget; and the
    op-count bound (M-6) catches a metadata storm whose bytes stay small.
- **Depends on:** Part A blessed (rev1§4.4 mandatory-mechanisms framing). No intra-B12 dep —
  this is the root.
- **Work:** the reshape + accounting + per-ref-bound flush above; update the size-pressure sum
  (`:1923-1928`) to compare against `global_budget` (renamed) so the build stays green before
  B12B refines it; thread the accounting through WAL replay so a remounted store recomputes it.
- **Acceptance (tests in `cas/src/store.rs` `mod tests`):**
  - **Per-ref containment (M-3).** A proptest with N refs, one written far past `per_ref_budget`:
    after each op, **no ref's overlay exceeds `per_ref_budget` + one write**, and global usage
    cannot reach `global_budget` from a single ref alone. The hot ref demonstrably self-flushes;
    quiet refs stay dirty.
  - **Op-count bound (M-6, op-count half).** A metadata-storm proptest (many tiny ops on one
    ref) flushes that ref via `op_count` **while its byte count stays well under
    `per_ref_budget`** — the byte-only bound would have missed it.
  - **Accounting survives remount.** Replay reconstructs per-ref bytes/op-count/oldest-positions
    identical to the pre-crash live state (a small round-trip test).
  - **Crash-injection.** `crash_recovery_preserves_acked_state` extended with the
    per-ref-soft-bound flush in its op set: after any crash point, all acked writes recover
    (64 native / 4 Miri).
  - `cargo verus verify -p cas --no-default-features` still **65/0**.
- **Effort/Risk:** M / medium. The reshape touches every `StoreOptions` caller and the
  accounting is the shared substrate, so it is the widest-blast-radius sub-phase even though
  each piece is small.

---

## Sub-phase B12B — size-pressure low/high watermarks + flush-the-biggest-offenders *(closes M-4)*

Replaces the single `total > overlay_budget → sync_all()` threshold with the spec's
two-watermark, selective-flush policy.

- **Touches:** `cas/src/store.rs` size-pressure (`:1923-1928`).
- **Depends on:** B12A (per-ref byte accounting + the `global_budget`/`size_low_watermark`
  fields).
- **Work:** rewrite the size-pressure check as:
  1. compute `total = Σ overlays[*].bytes()` (already cheap);
  2. if `total > size_low_watermark`: flush the **biggest offenders** — sort dirty refs by
     `bytes()` descending and `flush_ref` them until `total ≤ size_low_watermark` (or one ref
     remains) — *not* `sync_all`; small refs stay dirty;
  3. the **high watermark** is `global_budget`: crossing it is the backpressure point — the
     write blocks while the low-watermark flush above runs to completion (Design decision 4);
     it is "rarely hit" precisely because flushing starts at the low watermark.
  Set `size_low_watermark` default to a fraction below `global_budget` (recommend
  `global_budget * 3 / 4`); `global_budget` itself is the high watermark.
- **Acceptance (tests in `cas/src/store.rs` `mod tests`):**
  - **Selective, biggest-first (M-4).** A proptest with several refs of unequal size crossing
    the low watermark flushes the **largest** ref(s) and leaves the small ones dirty — asserted
    by inspecting which overlays remain non-empty — versus the old behavior that emptied all of
    them.
  - **Low watermark shields the high one.** Under steady writes, `total` oscillates around the
    low watermark and the high watermark (FULL/backpressure) is reached only by a single
    over-budget write (the Design-decision-4 hard-error case), not by normal traffic.
  - **Crash-injection.** The extended `crash_recovery_preserves_acked_state` op set now includes
    a partial size-pressure flush (some refs flushed, some not) → all-acked recovers.
  - cas gate **65/0**.
- **Effort/Risk:** M / medium.

---

## Sub-phase B12C — circular WAL ring + WAL-pressure flush-the-pinner at the 50% watermark *(closes M-5; the long pole)*

Turns the linear flush-everything-and-reset WAL into a ring whose head advances past a partial
flushed prefix, driven by flush-the-tail-pinner at a 50% watermark. Resolves Design decision 3.

- **Touches:** `cas/src/store.rs` — the WAL write path (`wal_tail` handling, `:1905-1910`), the
  per-ref `oldest_wal_pos` accounting (from B12A) as the sort key, `commit`'s head advance, and
  the WAL replay in `mount` (read from `wal_head` around the ring). `Superblock::wal_head`
  (`:1430`) already exists.
- **Depends on:** B12A (per-ref `oldest_wal_pos`). Independent of B12B/B12D.
- **Work:**
  - **Ring representation.** `wal_head` = start of the oldest live record; `wal_tail` = next
    write position; both wrap mod `wal_len`. `usage = (wal_tail − wal_head) mod wal_len`. A
    record write that would overrun `wal_head` is the full-WAL case.
  - **Flush-the-pinner at the watermark.** When `usage` crosses `wal_watermark` (50% of
    `wal_len`), find the ref whose `oldest_wal_pos == wal_head` (the tail-pinner), `flush_ref`
    it, advance `wal_head` past the now-flushed **contiguous prefix** (stopping at the next
    still-unflushed ref's oldest record), and repeat until `usage` is comfortable.
  - **Normative edge cases preserved (rev1§4.4):** the oversized record (> whole WAL region)
    keeps its synchronous-bypass-before-ack path; a genuinely full WAL (flushing the pinner
    cannot free enough — e.g. one ref pins the whole ring) keeps the flush-everything-and-reset
    fallback.
  - **Replay across the wrap.** `mount` reads records from `wal_head` forward around the ring to
    `wal_tail`, preserving the per-record-checksum torn-tail discipline (rev1§4.5) across the
    wrap boundary, and reconstructs per-ref accounting (Design decision 1).
  - **Disclosure retire.** Remove the MVP-list "WAL is linear, not circular … flush-the-pinner
    scheduler arrives with real multi-ref traffic" entry (`:24-27`) — it now exists.
- **Acceptance (tests in `cas/src/store.rs` `mod tests`):**
  - **Flush-the-pinner (M-5).** A multi-ref proptest where one idle ref holds an ancient record
    at `wal_head` while active refs fill the ring: crossing 50% flushes **the pinner** and
    advances `wal_head`, freeing its span, **without** flushing the active refs — versus the old
    flush-everything. The per-ref `oldest_wal_pos` sort key is asserted to pick the tail-pinner.
  - **Ring oracle (Design decision 2).** A reference-model proptest: a `Vec`-backed ring oracle
    and the real ring agree on `usage`, head/tail positions, and which records are live across
    randomized write/flush/wrap sequences — the wrap arithmetic guard.
  - **Replay across the wrap.** A crash test where live records straddle the wrap point recovers
    every acked write (extends `crash_recovery_preserves_acked_state`); the oversized-bypass and
    full-WAL-reset edge cases keep their existing assertions.
  - cas gate **65/0**; cas Miri leg clean over the ring arithmetic.
- **Effort/Risk:** L / medium–high. The ring + replay-across-the-wrap is the riskiest change in
  B12; it is de-risked by being **format-stable** (the head field exists) and by the ring oracle
  + crash proptest. Sequence it carefully and lean on the crash test as the load-bearing gate.

---

## Sub-phase B12D — staleness-timer trigger *(closes the timer half of M-6)*

Adds the fourth, lowest-priority flush trigger: a quietly dirty ref eventually becomes tree.
Resolves Design decision 5.

- **Touches:** `cas/src/store.rs` — per-ref `oldest_dirty_ns` (from B12A); a check at the
  request entry points (`log_then_apply` and the read/op handlers) and at the storage-server
  reactor's idle point; an injectable clock seam for the test.
- **Depends on:** B12A (per-ref `oldest_dirty_ns` + the clamped-monotone time source at
  `store.rs:1700-1705`). Independent of B12B/B12C.
- **Work:** on each request and at reactor idle, scan the dirty-ref set for any ref whose
  `now − oldest_dirty_ns > staleness_ns` (30 s default) and `flush_ref` it. Use the existing
  monotone-clamped clock; thread an injectable `now` for deterministic tests (no wall-clock
  sleeps). Priority-ordered *below* WAL/size pressure (it only fires when nothing more urgent
  did).
- **Acceptance (tests in `cas/src/store.rs` `mod tests`):**
  - **Staleness flush (M-6, timer half).** With an injected clock: a ref dirtied then left idle
    past `staleness_ns` is flushed on the next request (or simulated reactor idle); a ref within
    the bound is left dirty. Deterministic, Miri-safe.
  - cas gate **65/0**.
- **Effort/Risk:** S–M / low.

---

## Sub-phase B12E — neighborhood-only re-chunk on flush *(closes M-7; independent)*

Replaces whole-file re-chunking with the rev1§4.3 bounded neighborhood pass. Orthogonal to the
budget mechanisms — touches the flush's chunk-splicing, not its scheduling.

- **Touches:** `cas/src/store.rs` `flush_ref` (`:2108-2137`) and `make_file_entry` /
  `cas/src/file.rs` (the chunker invocation + chunk-list splice).
- **Depends on:** none in B12 (independent of A–D); can land in parallel.
- **Work:** for each dirty file, instead of reading the whole old content and re-chunking it
  all:
  1. read the existing file entry's chunk-boundary list (already in the tree entry) and the
     overlay's dirty-interval map to find the **first dirty byte**;
  2. **back up one chunk** before it, run the chunker forward from that boundary, and **stop
     when an emitted boundary coincides with an existing chunk boundary** (CDC self-sync
     guarantees this within a few chunks);
  3. **splice** the freshly-hashed chunks between the unchanged prefix and suffix chunk runs,
     reusing the untouched chunks (no re-hash).
  The result is ~2–4 new chunks for a small edit in a large file, per rev1§4.3.
  - **Disclosure retire.** Remove the MVP-list "Flush rebuilds whole dirty files instead of
    re-chunking only the affected neighborhood" entry (`:21-23`).
- **Acceptance (tests in `cas/src/store.rs` / `cas/src/file.rs` `mod tests`):**
  - **Canonical-form oracle (the correctness guard).** A proptest: for an arbitrary edit to a
    large file, the neighborhood-re-chunk root hash **equals** the whole-file-re-chunk root hash
    (history-independent canonical form, rev1§4.1) — neighborhood re-chunk is behavior-preserving,
    only cheaper. This is the load-bearing test (it is what makes M-7 "no semantic difference").
  - **Write-amplification (M-7).** The same proptest asserts the count of **newly-hashed**
    chunks is bounded (O(edit) — a few), not O(file size), for a small edit in a large file.
  - **Miri clean** over the splice arithmetic; cas gate **65/0**.
- **Effort/Risk:** M–L / medium. The chunk-splice correctness is the risk; the canonical-form
  oracle (shared in spirit with B13) is the guard. Adjacent to but distinct from B13's
  prolly-shape proof — B12E proves *this flush path* matches the canonical form, B13 proves the
  canonical form itself.

---

## Sub-phase B12F — recommended defaults (S-9) + the refuse-not-panic format contract (S-10) *(the finishing + orthogonal items)*

Aligns the shipped defaults to the rev1§4.4 table and makes `format`/`mkfs` refuse an
undersized device cleanly.

- **Touches:** `cas/src/store.rs` `StoreOptions::default` (`:163-171`) and `format` (`:1396-1449`,
  the `assert!` at `:1398`); `mkfs/src/main.rs` (`:60-112`, the `wal_len` override `:73-76` and
  the `format(…)?` call `:77`); `user/storaged` config.
- **Depends on:** B12A–B12D for **S-9** (every option field must exist before its default can be
  set); **S-10 is independent** and may land any time.
- **Work:**
  - **S-9 — defaults.** Set `StoreOptions::default` to the rev1§4.4 recommended numbers: `wal_len
    64 MiB`, `per_ref_budget 8 MiB`, `global_budget 128 MiB`, `wal_watermark = 50%`,
    `staleness_ns = 30 s`, and a documented `op_count_bound` default (the spec gives no number —
    pick a defensible default, e.g. tied to a few thousand ops, and note it is a recommended
    default a store may tune). Make `storaged`'s shipped runtime config match (it is "the
    storage server's shipped configuration" the spec names). `mkfs`'s 1 MiB `wal_len` override
    is a **format-time** parameter (the WAL size is baked into the image's superblock) — either
    raise it to the recommended 64 MiB so shipped images match, **or** keep the tuned-small
    value with an explicit comment that it is a deliberate batch-tool tune for replay-memory
    (the spec permits tuning); recommend **matching 64 MiB** unless the host replay-memory
    concern in the existing `:73-76` comment is judged real, in which case keep the override and
    document it as deliberate, not drift.
  - **S-10 — format contract.** Replace the `assert!(dev.len() > chunk_off + 4096, …)` (`:1398`)
    with a **checked geometry validation** returning a `StoreError` (a new
    `StoreError::DeviceTooSmall`/`BadGeometry` variant): validate, with checked arithmetic, that
    the device holds the two fixed superblock slots + the WAL region + a minimal chunk region and
    that the geometry lays out within the device — refuse with `Err`, never panic (rev1§4.5).
    `format` already returns `Result`, and `mkfs`'s `Store::format(dev, opts)?` (`:77`) already
    propagates `Err` to `main()`'s `ExitCode::FAILURE` (`:104-111`) — so once `format` returns
    `Err` instead of asserting, the clean failure path is reached with **no further mkfs
    change** beyond a tidy error message. Cite rev1§4.5 at the new check.
- **Acceptance:**
  - **S-9.** A test asserts `StoreOptions::default()` equals the rev1§4.4 table values; a
    `storaged` config test (or a comment-anchored assertion) confirms the shipped server matches;
    the mkfs `wal_len` choice is either 64 MiB or carries the documented-tune comment.
  - **S-10.** A `format` unit test over an undersized `FileDev` returns `Err`, **not** a panic;
    an `mkfs` test (or `assert_cmd`-style invocation) over a tiny image exits **`ExitCode::FAILURE`
    cleanly** with the error message, no panic/abort.
  - cas gate **65/0**; `cargo test -p cas` and `-p mkfs` green.
- **Effort/Risk:** S / low. S-9 is mechanical; S-10 is small (the `Result` plumbing already
  exists — only the geometry check changes from `assert!` to checked-`Err`).

---

## Execution order

```
B12A  per-ref accounting + StoreOptions reshape + per-ref soft bound + op-count bound   [foundation; do first]
        (M-3, M-6 op-count half)
   ├─► B12B  size-pressure low/high watermarks + flush-biggest-offenders   (M-4)
   ├─► B12C  circular WAL ring + flush-the-pinner at 50% watermark         (M-5)  [long pole]
   └─► B12D  staleness-timer trigger                                       (M-6 timer half)
B12E  neighborhood-only re-chunk on flush   (M-7)        [independent of A–D; parallel]
B12F  recommended defaults (S-9) + refuse-not-panic format (S-10)
        S-10 independent (parallel); S-9 after A–D land their option fields
```

- **B12A is foundational and the widest blast radius** (the `StoreOptions` reshape touches every
  caller; the per-ref accounting is the shared substrate). It lands the per-ref soft bound (M-3)
  and the op-count bound (M-6 half) on its own.
- **B12B, B12C, B12D each depend only on B12A** and are mutually independent — they read the
  per-ref accounting (bytes, oldest-WAL-position, oldest-dirty-timestamp) B12A maintains and add
  one trigger each. B12C is the long pole (the ring).
- **B12E (M-7) is fully independent** — it changes the flush's chunk-splicing, not its
  scheduling — and can land in parallel with the whole A–D chain.
- **B12F** splits: **S-10 (format contract) is independent** and may land any time; **S-9
  (defaults) lands last** because it sets the default for every option field A–D introduce.
- **The crash-injection proptest is extended incrementally:** B12A adds the per-ref-soft-bound
  flush to its op set, B12B the partial size-pressure flush, B12C the ring/partial-head-advance
  + replay-across-the-wrap — so the all-acked-survives invariant is re-witnessed as each
  selective-flush path appears (Design decision 2).

## Out of scope for B12 (recorded so it is not mistaken for a gap)

- **Concurrent / incremental GC, persisted incremental marking, and streaming WAL replay** —
  rev1§8.3-deferred, gated on B6, scheduled as **Phase C4**. B12 keeps GC synchronous; the
  circular ring (B12C) changes the *live window* but mount still buffers that window for replay
  (streaming replay is C4). The MVP-list GC and "modest WAL keeps replay memory bounded" notes
  stay.
- **A true asynchronous `FULL` backpressure return / multi-threaded flush** — the MVP realizes
  backpressure as a synchronous blocking flush (Design decision 4); a `FULL` reply the caller
  retries belongs with async flush and is future work.
- **A `verus!{}` proof of the flush policy or a verified circular-ring core** — the tier is
  proptest + crash-injection + Miri (Design decision 2); the cas gate holds at 65/0. A verified
  ring-arithmetic core (mirroring `freelist`) is recorded as optional future hardening — additive
  if ever taken, weakening nothing.
- **An armed timer-notification staleness trigger** — B12D fires the staleness scan
  opportunistically (request boundaries + reactor idle); arming a kernel timer object (the B-IRQ
  path) for a fully-idle, never-polled server is the recorded upgrade (Design decision 5).
- **Persisting any scheduler accounting on disk / an `SB_VERSION` bump / corpus regen** — B12 is
  format-stable; all accounting is reconstructed at mount from WAL replay (Design decision 1).
- **Per-session (vs per-ref) budget charging** — the per-session admission budget and anti-drain
  bound landed in **B1** (S-4); B12 charges per-ref *under* the global budget, leaving B1's
  session-total accounting as the "charged to sessions" hook rev1§4.4 names.
- **Rename / ephemeral file-id-keyed overlays** — **Phase C2**. B12's neighborhood re-chunk
  (B12E) works on the existing path-keyed overlay; file-id keying is C2's concern.
- **The prolly-tree canonical-form *proof*** — **Phase B13**. B12E *uses* the canonical form as
  its correctness oracle (neighborhood re-chunk == whole-file re-chunk root hash); B13 *proves*
  the canonical form itself. Distinct, complementary.
- **TLA changes** — `CommitProtocol`'s `AckedWritesRecoverable` + the B7 `Recover` property
  already cover "head advances only past flushed records"; B12C's partial head advance is a
  refinement of the *same* invariant, guarded by the extended crash-injection proptest. A small
  per-ref-flush TLA action is recorded as optional **only if** the existing model is judged not
  to cover partial-prefix advance — the crash proptest is the primary guard either way.
