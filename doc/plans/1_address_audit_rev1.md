# Plan — Addressing the rev1 Conformance & Verification Audit

Response plan for `doc/results/1_audit_rev1.md`. Unlike rev0, this audit found the
system **conformant**: every rev0 finding is now backed by gate-passing code or model
artifacts, zero findings were omitted, and **no code regressions** were introduced. The
audit's own headline: *"The residue is small and almost entirely cosmetic."*

So this is a **cleanup plan**, not a remediation plan. There is no spec revision 2 and
no new mechanism: rev1 stays the blessed target. The work is (1) documentation drift
introduced by the `cleanup stale docs` commit (`7b6f55c`), (2) two low-severity
verification-*wiring* precision gaps plus one missing accounting test, (3) one soft
spec-text over-claim, and (4) one optional runtime observation. The audit catalogues
all of it in its §8 "Suggested follow-ups", and this plan follows that priority order.

Each phase lists **Closes** (audit §), **Touches**, **Depends on**, **Work**,
**Acceptance**, and **Effort/Risk**. All `rev1§`, finding-ID (I-/T-/M-/S-), and
file:line references are the audit's, re-verified against the working tree while
drafting this plan (drift in a few line numbers is noted inline).

---

## Guiding principles

- **No conformance work is owed.** Every item here is cosmetic, a verification-wiring
  precision gain, a test that hardens an already-correct-by-construction path, or a
  spec sentence made precise. None changes runtime behaviour of the confinement, safety,
  or storage paths. If a phase is descoped, the system remains conformant.
- **Preserve the honesty discipline.** The audit confirmed the spec/ledger correctly
  do **not** label test-routed properties as "verified" (§6.3). Phases B1/B2 may only
  *strengthen* what is claimed verified (by wiring a proven lemma into the exec path) or
  *document* a lemma as standalone — never relabel a proptest-routed property as proven.
- **A fix and its check ship together.** Any phase that touches a Verus surface must
  re-establish the gate at ≥ the current count (and B1/B2 may raise it); any phase that
  touches a host-tested layer adds/updates the test in the same change.
- **Each commit is rustfmt-clean per the workspace split.** Several touched files live
  in the separate `user/*` and `*/fuzz` mini-workspaces (e.g. `user/storaged`), which a
  root `cargo fmt` silently skips — format those via their own manifest (see CLAUDE.md).
- **Doc-drift fixes do not resurrect deleted docs.** The `cleanup stale docs` commit
  deliberately removed the `doc/plans/*`/`doc/results/*` detail docs. The fix is to
  re-point or delete the *dangling references*, and to *inline* the one substantive
  rationale a deleted doc backed — not to recreate the deleted files (see Phase A1's
  decision).

**Baseline to preserve (regression gates).** These are the ground-truth gates the audit
re-ran; any phase that changes them must re-establish them at ≥ the prior numbers:

| Gate | Baseline |
|---|---|
| `cargo verus verify -p kcore` | 389 / 0 |
| `cargo verus verify -p cas --no-default-features` | 80 / 0 |
| `cargo verus verify -p ipc` | 69 / 0 |
| `cargo verus verify -p freelist` | 29 / 0 |
| `cargo verus verify -p urt` | 29 / 0 |
| `cargo verus verify -p dma-pool` | 0 / 0 |
| `CapRevocation` TLC | 503,070 distinct states, no error |
| `CommitProtocol` / `IpcReactor` TLC + committed neg-controls | pass / fail-as-expected |
| `cd kernel && cargo build` (bare-metal) | exit 0 |
| Host tests (cas/ipc/loader/storage-server/freelist/urt/dma-pool) | all green |

The 14-seam trusted base (8 `external_body` + 6 `assume_specification`) is closed:
**no phase here adds a seam.**

---

# Part A — Documentation drift (the actionable residue)

The only finding the audit rated *medium*, plus the low-severity citation sweeps. These
are independent, mechanical, and the highest value-per-effort items (§8 items 1, 4, 5).

### Phase A1 — Re-point or remove the dangling `doc/plans/*` & `doc/results/*` references

- **Closes:** audit §6.1 (medium dangling refs), §4 (the documentation regression).
- **Touches:** `CLAUDE.md`, `dma-pool/Cargo.toml`, `dma-pool/src/lib.rs`,
  `freelist/src/lib.rs`, `urt/Cargo.toml`, `urt/src/lib.rs`,
  `virtio-blk/tests/ring_props.rs`, `scripts/m1-test.sh`, `cas/src/prolly.rs`,
  `.github/workflows/ci.yml`, `doc/guidelines/verus_trusted-base.md`.
- **Depends on:** none.
- **Work.** The `cleanup stale docs` commit deleted the detail docs but left live files
  citing them. Confirmed-dangling references against the working tree (the audit's set,
  plus two it did not list):
  - `doc/plans/12_b11-detail.md` — `urt/src/lib.rs:15`, `urt/Cargo.toml:13`,
    `freelist/src/lib.rs:21`, `dma-pool/src/lib.rs:20`, `dma-pool/Cargo.toml:12`, **and
    `.github/workflows/ci.yml:211`** (not in the audit's list — found while verifying).
  - `doc/plans/4_b4-detail.md` — `dma-pool/src/lib.rs:489`.
  - `doc/plans/2_b2-detail.md` — `virtio-blk/tests/ring_props.rs:6`.
  - `doc/plans/14_b13-detail.md` — `cas/src/prolly.rs:1371` (cited bare as
    `14_b13-detail.md`, no path prefix).
  - `doc/results/9_b-irq-c` — `scripts/m1-test.sh:20`.
  - `doc/results/23_miri-test-optimization.md` — `CLAUDE.md:118`.
  - `doc/results/4_b9c-findings.md` — `doc/guidelines/verus_trusted-base.md:193`.

  **Decision (per audit §8(1)) — two classes:**
  1. **Pure "see X for detail" pointers** (everything except the b9c one): the detail
     they pointed to is gone by design. Rewrite each comment so it stands alone — keep
     the one-clause rationale that is *in* the comment, and **delete the dangling
     `doc/...` pointer** (do not invent a replacement target). Where the design decision
     itself is load-bearing (e.g. the freelist/urt shared-proof rationale in
     `dma-pool` and `urt`), fold the one sentence of "why" into the comment so nothing
     is lost when the pointer is dropped.
  2. **The ledger pointer that backs a substantive claim** —
     `verus_trusted-base.md:193` cites `4_b9c-findings.md` for *why* the `CapRevocation`
     model is trimmed to Threads 1 / QueueDepth 1 (the full-scale liveness tableau
     exhausts heap). **Inline that one-sentence rationale** into the ledger row so the
     503,070-state count keeps its justification, then drop the dead pointer.
- **Acceptance.** `grep -rE 'doc/plans/(12_b11|4_b4|2_b2|14_b13)|doc/results/(9_b-irq-c|23_miri|4_b9c)'`
  over the tree returns only `doc/results/1_audit_rev1.md`, `doc/plans/0_address_audit_rev0.md`,
  and this plan. `cargo build` and `cd kernel && cargo build` still exit 0 (Cargo.toml
  comments only); CI workflow still parses.
- **Effort/Risk:** S / low. Comments, one CI comment, one manifest comment — no code.

### Phase A2 — Sweep the stale "until B12F" comments and the misleading test name

- **Closes:** audit §6.1 (low, stale "until B12F").
- **Touches:** `cas/src/store.rs`, `user/storaged/src/main.rs`.
- **Depends on:** none.
- **Work.** B12F **has landed**: `StoreOptions::default` ships `op_count_bound: 8192`
  and `staleness_ns: 30 * 1_000_000_000` (`cas/src/store.rs:225,230`) and the staleness
  sweep is live. But comments still describe these as disabled/stubbed "until B12F
  ships": `cas/src/store.rs:183,2409,2533,2566,3323,6717` and
  `user/storaged/src/main.rs:324`. Rewrite each to past tense / present state.
  **Subtlety to preserve:** several of these comments are *correct* about `test_opts()`
  (which deliberately disables staleness with `staleness_ns == u64::MAX`) — the bug is
  that they read as if the *production* `Default` is stubbed. Keep the accurate
  "`test_opts` disables staleness" statement; remove only the "until B12F" framing.
  The test `staleness_disabled_by_default_never_flushes` is **correctly bodied** (it
  drives `test_opts()`, not `Default`) but **misleadingly named** — rename to
  `staleness_disabled_under_test_opts_never_flushes` so the name matches the body.
- **Acceptance.** `grep -n 'B12F\|until B12' cas/src/store.rs user/storaged/src/main.rs`
  returns nothing (or only an accurate historical note). `cargo test -p cas` green;
  `cargo fmt --manifest-path user/storaged/Cargo.toml` clean.
- **Effort/Risk:** S / low.

### Phase A3 — Convert the ~6 bare `§` refs and the `rev1§4.x` placeholder

- **Closes:** audit §6.1 (low, bare `§` refs; `rev1§4.x` placeholder).
- **Touches:** `kernel/src/syscall.rs`, `cas/src/disk.rs`, `cas/src/store.rs`,
  `kcore/src/ready.rs`, `virtio-blk/src/lib.rs`, `virtio-blk/tests/driver.rs`.
- **Depends on:** none.
- **Work.** Two mechanical conversions, per the CLAUDE.md rule that every spec ref
  carries its revision:
  - **Bare `§` → `rev1§`** for genuine spec-section refs left unconverted:
    `kernel/src/syscall.rs:640` (`§6.1(d)`), `cas/src/disk.rs:722-723` (`§2.2`),
    `cas/src/store.rs:1150,1295,3172,6365`, `kcore/src/ready.rs:772` (`§1.1`).
    **Do not touch** the `§6a/§6d`-style labels in `kcore/src/test_store.rs` — the audit
    confirms those are local proof-obligation labels, not spec refs.
  - **`rev1§4.x` → `rev1§4.5`** (the real S-11 LBA-bound section):
    `virtio-blk/src/lib.rs:109,390`, `virtio-blk/tests/driver.rs:78`.
- **Acceptance.** No bare `§<digit>` spec ref and no `rev1§4.x` placeholder remain
  outside `test_store.rs`'s proof labels. Crates still build/test.
- **Effort/Risk:** S / low.

### Phase A4 — Refresh the ledger's stale seam-row line citations and add the IRQ-shell row

- **Closes:** audit §6.2 (two nits).
- **Touches:** `doc/guidelines/verus_trusted-base.md`.
- **Depends on:** none.
- **Work.** The ledger's 14-seam tally, gate counts, and reasoning are accurate, but
  four seam-row **line citations drifted** — and the worst points a reader to unrelated
  code:
  - `checksum_ok` — cited `cas/src/disk.rs:337`, audit says actual `:342`.
  - `wal_checksum_ok` — cited `cas/src/store.rs:1045`, actual `:1050`.
  - `is_boundary` — cited `cas/src/prolly.rs:1373`, actual `:1387`.
  - `CapSlot::empty` — cited `kcore/src/cspace.rs:1226`, which is **`set_ready_tail`
    code** (verified in this plan's drafting); the real `const fn empty` is at `:167`
    (its `assume_specification` at `:1596`). The narrative footnote at ledger `:164`
    repeats the wrong `:1226` — fix both.
  Re-derive each line from the current tree at fix time (don't trust these numbers
  blind; they drift) and update. Then **add an enumerated row** for the trusted IRQ
  delivery shell (`kernel/src/irq.rs`) with a named host test, mirroring the timer-tick
  shell row — the audit notes it is correctly *covered* by the "scheduler/asm shell
  stays trusted" umbrella but lacks its own row.
- **Acceptance.** Each cited seam line resolves to the named item (spot-check with
  `sed -n` at fix time). The seam tally is unchanged at 14 (a ledger *row* for the
  already-trusted IRQ shell is not a new seam — note this explicitly in the row).
- **Effort/Risk:** S / low.

---

# Part B — Verification-wiring precision & the missing accounting test

The audit's two genuine (low-severity) Q2 gaps, both the same shape — *a verified
standalone lemma never composed into the executable path* — plus the open B10C test
(§8 item 2). The audit's explicit recommendation: **either invoke the top-level lemmas
from the exec functions (giving the end-to-end statement) or explicitly document them as
standalone design theorems.** This plan picks **invoke where tractable, document
otherwise**, and lands the test unconditionally.

### Phase B1 — Compose `lemma_partition_flatten` into prolly node emission (or document)

- **Closes:** audit §2.1 (prolly conservation proven but not wired).
- **Touches:** `cas/src/prolly.rs`.
- **Depends on:** none. (Verus surface — re-establish cas 80/0, target ≥.)
- **Work.** `lemma_partition_flatten` (`cas/src/prolly.rs:1562`) is a `proof fn` with
  **zero call sites** (confirmed: grep finds it only in its definition and doc-comments
  at `:35,:1444`). Its hypotheses match `split_points`'s `ensures`, but nothing
  machine-connects them, and `build_level` (`prolly.rs:324`) — the exec fn that drives
  the cuts and stores nodes — carries **no `requires`/`ensures`**, so even
  `split_points`'s proven postconditions don't propagate to a statement about the bytes
  emitted.
  - **Preferred:** give `build_level` an `ensures` that the emitted node sequence is a
    conservative partition of the input entry list (concatenation of node contents =
    original entries, in order), discharging it by calling `lemma_partition_flatten` on
    the `split_points` result inside the body. This mechanizes the chain
    "verified cut points ⇒ `build_level` emits a conservative partition ⇒ `Dir::save`
    root well-formed" the audit says is currently un-mechanized.
  - **Fallback (if `build_level`'s exec shape resists a clean `ensures` within a small
    proof budget):** add a doc-comment to `lemma_partition_flatten` and at its
    `build_level` would-be call site explicitly stating it is a **standalone design
    theorem about the cut-index function, not an exec postcondition**, so no reader
    over-reads the headline "the partition is verified". The audit explicitly blesses
    this fallback. Decide by spending a bounded effort on the preferred path first.
- **Acceptance.** Either `lemma_partition_flatten` has ≥1 exec call site and
  `build_level` carries the conservation `ensures` (gate cas ≥ 80/0, ideally higher), or
  it is documented as standalone at both its definition and the exec boundary. Honesty
  check: the spec/ledger wording is **not** changed to claim exec-composition unless the
  preferred path lands.
- **Effort/Risk:** M / low–medium. The lemma is already proven; the work is connecting
  `split_points`'s `ensures` through `build_level`'s exec loop — could need framing
  lemmas about the accumulator. Time-box the preferred path, then take the fallback.

### Phase B2 — Wire `lemma_grow_pool` (or document) and land the B10C top-up accounting test

- **Closes:** audit §2.2 (aspace top-up composition unwired + accounting untested); the
  open B10C item.
- **Touches:** `kcore/src/aspace.rs`, `kcore/src/test_store.rs`, `kernel/src/untyped.rs`
  (the `aspace_topup` shell under test).
- **Depends on:** none. (Verus surface — re-establish kcore 389/0, target ≥.)
- **Work.** Two independent sub-items:
  1. **Composition lemma.** `lemma_grow_pool` (`kcore/src/aspace.rs:880`) — the
     top-level "a contiguous pool extension preserves `pt_wf` and every existing
     mapping" theorem — is referenced only in doc-comments (`:857`) and tests
     (`test_store.rs:6257,6305`), never *called* from verified exec code; its per-VA
     stability core `lemma_grow_pool_lookup` (`aspace.rs:955`) **is** wired. Mirror
     Phase B1: prefer invoking `lemma_grow_pool` from the verified `grow_pool` exec path
     so the end-to-end "topped-up pool preserves every mapping" statement composes;
     fall back to documenting it as a standalone theorem if the exec shape resists.
  2. **Accounting test (B10C).** The kernel-side `aspace_topup` shell (debits the donor
     untyped's watermark via `carve_place` with an abutment guard, then reset returns
     the pool at teardown) has **no direct unit test** — host tests model `grow_pool`
     with a bare `Vec::extend` and never exercise the watermark debit, the abutment
     guard, or the debit-then-reset round trip. Add a host test that drives the real
     shell: assert the donor watermark decreases by exactly the carved amount, that a
     non-abutting carve is refused, and that teardown/reset returns the pool to the
     donor (the debit-then-reset round trip nets to zero). This is the part the audit
     says "M-2 composes correctly *by construction*, but part 3's accounting is asserted
     by argument, not by test."
- **Acceptance.** Either `lemma_grow_pool` has an exec call site (kcore gate ≥ 389/0) or
  it is documented standalone; **and** a new B10C test exercises the watermark
  debit + abutment guard + debit-then-reset round trip and passes. The spec/ledger are
  not relabeled to claim exec-composition unless the lemma is actually wired.
- **Effort/Risk:** M / low–medium. The test is the certain win; the lemma wiring is the
  same time-boxed preferred/fallback call as B1.

---

# Part C — Spec-text honesty

### Phase C1 — Correct the rev1§4.4 64 MiB WAL "shipped configuration matches" claim

- **Closes:** audit §6.3 (low, soft over-claim); the rev1§5 / §0 caveat on the live WAL.
- **Touches:** `doc/spec/spec_rev1.md` (rev1§4.4).
- **Depends on:** none.
- **Work.** rev1§4.4 says the shipped server "matches" the 64 MiB WAL default, but the
  **live** server only ever *mounts* the mkfs image, and `mount` overrides `wal_len`
  from the on-disk superblock; mkfs deliberately sets `wal_len = 1 MiB`
  (`mkfs/src/lib.rs:56`, confirmed). So the live server runs a 1 MiB WAL with a 512 KiB
  watermark, not 64 MiB/32 MiB. The mechanism is correct and the numbers are tunable —
  this is spec-text imprecision, not a conformance bug. Reword §4.4 so the WAL size is
  described as coming from the **image geometry** (which the shipped mkfs image tunes
  down to 1 MiB), and qualify **64 MiB as the in-memory `StoreOptions::default`**, not
  the live figure. Keep `StoreOptions::default`'s 64 MiB unchanged — it is the documented
  default; only the spec sentence is wrong. (This is a *soften-to-match-reality* edit,
  not a conform-the-code edit: the live geometry is the intended behaviour.)
- **Acceptance.** rev1§4.4 no longer asserts the live server runs 64 MiB; it names the
  image-geometry source and the 1 MiB shipped figure. No code change.
- **Effort/Risk:** S / low.

---

# Part D — Optional runtime observation

### Phase D1 — (Optional) Suppress the kernel diagnostic UART when the userspace console is live

- **Closes:** audit §5 (low, dual UART writers); rev1§5 observation.
- **Touches:** `kernel/src/uart.rs` (and a gate the console-spawn path can flip).
- **Depends on:** none.
- **Work.** With `debug-log` enabled (default for dev images), `kernel/src/uart.rs`
  writes the physical PL011 at `0x0900_0000` (confirmed `UART_BASE`) while the userspace
  console driver also writes the same device through its mapped MMIO window — two
  unsynchronized writers, so kernel diagnostics and console output can interleave. The
  audit is explicit this is **not** an ambient-authority hole (the kernel path is
  kernel-internal diagnostics; the EL0 hole M-9/S-8 closed is separate) — only a
  cosmetic/observability hazard that naturally disappears when `debug-log` is off.
  **Recommendation: defer / do last, or skip.** If taken: have the kernel quiesce its
  diagnostic UART writes once it observes the console driver has bound the PL011 region
  (or simply gate kernel UART output off after handoff), so at most one writer is live.
- **Acceptance.** With `debug-log` on, a boot-then-console run shows no interleaving of
  kernel diagnostics into console output after handoff; `debug-log` off is unchanged;
  smoke (`scripts/run-demo.sh`) still reaches `[storaged] store mounted` → `serving`
  with no panic.
- **Effort/Risk:** S–M / low. Purely cosmetic; lowest priority. Acceptable to drop.

---

## Sequencing & effort summary

All phases are independent (no `Depends on` edges) and can land in any order or in
parallel; the ordering below is by the audit's §8 value ranking.

| # | Phase | Closes | Effort | Priority |
|---|---|---|---|---|
| A1 | Re-point/remove dangling doc refs | §6.1 (medium), §4 | S | 1 (highest) |
| B1 | Wire/document `lemma_partition_flatten` | §2.1 | M | 2 |
| B2 | Wire/document `lemma_grow_pool` + B10C test | §2.2 | M | 2 |
| C1 | Fix rev1§4.4 WAL-default sentence | §6.3 | S | 3 |
| A2 | Sweep stale "until B12F" comments + test name | §6.1 | S | 4 |
| A3 | Convert bare `§` / `rev1§4.x` refs | §6.1 | S | 4 |
| A4 | Refresh ledger seam citations + IRQ row | §6.2 | S | 5 |
| D1 | (Optional) single-UART-writer handoff | §5 | S–M | 6 (optional) |

**Total:** five S-effort doc/cosmetic phases, two M-effort verification-wiring phases
(each with a blessed document-only fallback), and one optional runtime phase. The whole
plan is *additive precision* over an already-conformant system — there is no correctness
debt to retire, and dropping any single phase leaves the system conformant.

## Exit criteria for "rev1 audit fully addressed"

1. The dangling-doc grep (A1 acceptance) is clean.
2. B1 and B2 each end in one of {lemma wired into exec, lemma documented standalone};
   the B10C accounting test is committed and green; all Verus gates hold at ≥ baseline.
3. rev1§4.4 no longer over-claims the live WAL size (C1).
4. No "until B12F" framing, no bare `§` spec ref outside proof labels, no `rev1§4.x`
   placeholder remain (A2, A3).
5. The ledger's seam-row citations resolve to their named items and the IRQ delivery
   shell has its own row; the seam tally is still 14 (A4).
6. D1 is consciously done or consciously deferred (recorded either way).

A `doc/results/2_*.md` write-up should record which lemmas were wired vs documented and
the final gate counts, so the next audit can diff against this plan.
