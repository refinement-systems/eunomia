# Findings 21 — Forward-port runbook + ledger finalization (Phase 6.3)

The last sub-phase of the std-library port (`doc/plans/2_plan-std-revised.md`,
Phase 6.3). Two deliverables plus this record: a durable **forward-port runbook**
(`doc/guidelines/forward-port.md`) and a **finalized trusted-base ledger**
(`doc/guidelines/verus_trusted-base.md`). This is a documentation + audit task —
**no source/code touched, no new Verus obligations, tally stays 14.**

## What shipped

- **`doc/guidelines/forward-port.md`** (new, durable guideline) — the standing
  discipline for re-basing the vendored Rust fork (`vendor/rust`) onto newer
  nightlies. Seven sections: (1) the two independent pins (std nightly vs. Verus
  gate) and their deliberate decoupling; (2) the nightly↔commit invariant; (3) the
  complete diff surface; (4) the panic→`STATUS_PANIC` terminus re-verify chain; (5)
  the `STATUS_PANIC == u64::MAX` lockstep invariant; (6) the regression set (the CI
  `verus` + `on-os` jobs, fuzzing); (7) the ordered bump procedure.
- **`doc/guidelines/verus_trusted-base.md`** (one-sentence edit) — recorded the
  sbrk/heap-grow deferral in the Phase 2.2 heap paragraph (see Decisions).

## Decisions

- **Runbook lives in `doc/guidelines/`, not `doc/results/`.** The plan's exit
  criterion is "runbook committed" as a *standing* forward-port discipline — the
  maintenance sibling of `verus.md`'s verification discipline. That belongs in the
  durable guidelines, not a temporary intermediate report. Consequence enforced: the
  runbook restates content rather than citing findings docs, since a guideline may
  reference only `rev2§…` (spec) and other `doc/guidelines`, never `doc/plans`/
  `doc/results`. *Confirmed with the user.*

- **The "sbrk/heap-grow folding note" the plan names is deliberately NOT added; the
  deferral is recorded instead.** The plan's ledger-finalization bullet lists a
  folding note for `sbrk`/heap-grow (folding under the page-table-join seam), but the
  heap is a fixed compile-time `.bss` reservation (`static HEAP: urt::Heap<N>`) with
  **no growth path** — a growable heap is Deferred work, and the plan's own table
  tags that row "(deferred item)". The ledger describes what *is* trusted; a folding
  note for absent code would be a phantom row. Resolution: one sentence in the Phase
  2.2 heap paragraph stating the fixed reservation is the entire mechanism, there is
  no `sbrk` path, and the page-table-join note is authored only if/when growth lands.
  *Confirmed with the user.*

- **The ledger needed no count reconciliation — it was already self-consistent.**
  The audit expected to fix stale per-note counts (e.g. a routing note reads "kcore
  407", the Baseline reads 408). It found no contradiction: the per-note numbers are
  **dated phase-snapshot deltas**, and the Baseline rows explicitly tie back to them
  — the kcore Baseline literally reads "the crate's rise to **408** (from the **407**
  of the `ThreadStartAs x6` re-proof recorded in the TPIDR routing note)", and the
  eunomia-sys (`7 → 16`), urt (`25 → 29`), and loader (`29 → 30`) rows each name their
  prior snapshot. So the notes and Baselines form a consistent delta chain; a
  "clarifier that per-note counts are historical" would be redundant. No edit made
  beyond the deferral sentence.

- **One incidental discipline fix during the audit.** The ledger carried a single
  stray `see doc/results/13_verus-findings.md` citation in the `RecoverReconstructs`
  note — a `doc/results` reference from within a guideline, which `CLAUDE.md` forbids
  ("may not be referenced ... in specs and guidelines"). Removed the citation clause,
  keeping the substantive teeth-control statement (the `(wal_head + 1)` `ensures` was
  confirmed to fail-to-verify, then reverted). Surgical, in service of "ledger
  consistent"; it was the only such reference in the file.

- **The three notes the plan says to "add" were already in the ledger.** TPIDR_EL0
  (added by Phase 3.1 / findings #8), TLS-key-table (3.5 / #12), and entropy-seed
  (3.4 / #11) routing notes are all present, each naming a reason + a §11 host test.
  The plan text was written before those phases landed their own notes; 6.3's ledger
  work is therefore an audit, not authoring.

- **Verus pin ↔ std nightly decoupling recorded as §1 of the runbook.** The two
  toolchains move independently: the verified crates are host-built on the Verus
  toolchain, do not link the vendored std, and their obligations are unaffected by a
  std nightly bump; conversely a Verus bump does not move the std nightly. This is the
  "decoupling" the plan asks to record.

## Problems hit

- **The plan text is stale relative to the finalized tree.** The plan cites kcore
  407 / loader 29 / eunomia-sys 7; the authoritative Baselines are kcore 408 / loader
  30 / eunomia-sys 16 (later phases: findings 16-1's `lemma_set_slot_end_cap` +1 on
  kcore, the entropy `KIND_SEED` arm +1 on loader, the 4.2 path resolver +9 on
  eunomia-sys). The runbook and this doc use the ledger's current numbers, not the
  plan's, and point readers at the ledger Baselines as the source of record rather
  than restating counts that will drift again.

- **The build-std cache trap is the single most dangerous forward-port hazard**, so
  it is called out explicitly in the runbook (§3.6). `-Zbuild-std` fingerprints the
  *toolchain*, not the `__CARGO_TESTS_ONLY_SRC_ROOT`-redirected source, so a vendored
  `std/src` edit silently caches the old std and never rebuilds — the class of bug
  that has cost time repeatedly (a stdio arm "works" via stale code while a changed
  path misbehaves). `kernel/build.rs`'s `rerun-if-changed` + `build_std_is_stale`
  cache-wipe closes it for `std/src`; edits to vendored `core`/`alloc` still need a
  manual `rm -rf target/user`.

- **Assumed paths were slightly off** and corrected against the tree: the target JSON
  is at repo-root `targets/`, not `kernel/targets/`; there is no `std-smoke-spawn`
  script (the std smoke is `scripts/std-smoke-test.sh`).

## Verification record

Gate for this task (per the plan): *"ledger consistent; runbook committed."* No
`cargo verus verify` re-run is required — no obligations were touched. The cheap,
in-scope confirmations were all run this session:

- `verus --version` → `Version: 0.2026.06.07.cd03505`, matching the CI `verus` job
  pin — the decoupled Verus gate is intact.
- Panic-chain anchors all resolve as documented: `__rust_start_panic`
  (`vendor/rust/library/panic_abort/src/lib.rs:33`), `__rust_abort`
  (`.../std/src/rt.rs:32`), `rust_panic` / `panic_with_hook`
  (`.../std/src/panicking.rs:886`/`:777`), `process::abort`
  (`.../std/src/process.rs:2637`), the eunomia PAL selector
  (`.../sys/pal/mod.rs:27-29`), and `abort_internal` → `__eunomia_thread_exit(u64::MAX)`
  (`.../sys/pal/eunomia/common.rs:19-31`). The clean-exit arm
  (`.../sys/exit.rs:85-96`) and `_start` (`.../sys/pal/eunomia/mod.rs:37-70`) both
  zero-extend `code as u32 as u64`.
- `STATUS_PANIC == u64::MAX` present with its `const _` assert in all seam homes:
  `eunomia-sys/src/syscall.rs:83`/`:89` and `ipc/src/sys.rs:411`/`:416`, plus the
  std-side literals in `common.rs`/`sys/exit.rs`.
- `restricted_std` allowlist clause present: `vendor/rust/library/std/build.rs:37`
  (`|| target_os == "eunomia"`).
- The `__eunomia_*` bridge contract cross-checked: every symbol std declares matches a
  `#[no_mangle] extern "Rust"` shim in `eunomia-sys/src/pal.rs` (39 shims).
- Pins confirmed live: `vendor/rust` @ `39ceb263`, base nightly-2026-06-26 == rustc
  `bd08c9e7…`; `kernel/rust-toolchain.toml` channel == `nightly-2026-06-26`;
  `vendor/verus` @ `cd035058`. All runbook-referenced scripts and the target JSON
  exist.
- Ledger re-read end-to-end: tally **14**; the three std-port routing notes present
  with reason + §11 host test; Baselines authoritative and internally reconciled.

Out of scope (CI-owned, expensive, unaffected by doc edits): the full `verus` gate and
the QEMU `on-os` jobs are green on the current tree (6.1/6.2 landed). The panic
terminus is re-verified at the **source-chain** level here — drift detection is the
point of the runbook — and is asserted *live* on every push by
`scripts/std-smoke-test.sh` in the `on-os` job (which hard-fails if a panic does not
reap as `STATUS_PANIC`).

## Surface left trusted / unverified — and why

The panic terminus (runbook §4) rides an **unmodified upstream chain** (`panic_abort`
→ `std::rt` → `process::abort`) with no eunomia arm in the middle hops. It is
**trusted by inspection**, re-checked per bump, and asserted live in CI — the only
sanctioned form of unverified code here (it is inline-with, and downstream of, the
trusted `kernel/`-over-`kcore` PAL-shell posture; there is no obligation to route to a
prover, only a drift-detection duty this runbook now encodes). This adds no ledger
seam — it is a maintenance invariant, not a new trusted construct.

## Follow-ups

- **Growable-heap folding note** — author the page-table-join folding note (and its
  §11 host test) if/when the `heap` named grant + `sbrk`/retype-map top-up lands; until
  then the fixed `.bss` reservation is the whole mechanism (recorded in the ledger).
- **Tier-3 upstreaming of `aarch64-unknown-eunomia`** — would retire the test-only
  build mechanisms (`-Zjson-target-spec`, `__CARGO_TESTS_ONLY_SRC_ROOT`) and let the
  runbook track upstream directly instead of maintaining an out-of-tree patch stack
  (runbook §7).
- **Unchanged deferrals** (not this task): real entropy source (`RNDR`/virtio-rng),
  power-efficient futex timeouts (timer-bit blocking), kernel futex/wait-set object,
  full FP/NEON userspace, mtime-on-the-wire, and the fs bulk-window data plane — each
  documented as Deferred work with its trigger.

*Per `CLAUDE.md`, `doc/plans` and `doc/results` are temporary intermediate reports and
are not referenced from code, specs, or guidelines.*
