# Kani verification findings — part 11 (housekeeping: naming + CI budget)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `12_kani-findings-10.md`.
This part implements recommendation #5 of the conformance review
(`9_kani-review.md`) — the last, "cosmetic, neither urgent" item: the findings
filename drift and the aggregate `kani` CI job approaching its budget. **No proof
code, no harness, no defect** — a tidy-up record so the next reader inherits a
consistent tree and an enforced budget. The standing caveat and design notes
(DN-1…DN-13) of the earlier parts apply unchanged.

## Filename convention (made explicit)

Findings files are `doc/results/N_kani-findings-P.md`, where:

- **`N`** is the shared `doc/results/` sequence number — one counter across *all*
  results docs, so the kani files interleave with non-kani ones (`0_mvp.md`,
  `1_fuzzing-findings.md`, `9_kani-review.md`). That is why `N` does not equal
  the part number; it is not a drift to "fix," it is the shared index.
- **`P`** is the kani-findings part. `§4.1` is part 1. The canonical first file
  `2_kani-findings.md` *is* part 1 and omits the `-P` suffix — it is the
  standing-caveat + design-note (DN) index every later part links back to. Every
  other file carries `-P` equal to its part number.

The review flagged one file that broke this: `6_kani-findings_6.md` (part 5) used
an underscore separator and the trailing number `6` instead of `-5`. **Renamed
to `6_kani-findings-5.md`** (and its two live cross-references —
`2_kani-findings.md`'s findings index and `7_kani-findings-6.md`'s back-link —
updated). With that, the trailing `-P` equals the part number for every file:

| File | Part | | File | Part |
|---|---|---|---|---|
| `2_kani-findings.md` | 1 (§4.1) | | `7_kani-findings-6.md` | 6 (§4.6) |
| `3_kani-findings-2.md` | 2 (§4.2) | | `8_kani-findings-7.md` | 7 (§4.7) |
| `4_kani-findings-3.md` | 3 (§4.3) | | `10_kani-findings-8.md` | 8 |
| `5_kani-findings-4.md` | 4 (§4.4) | | `11_kani-findings-9.md` | 9 |
| `6_kani-findings-5.md` | 5 (§4.5) | | `12_kani-findings-10.md` | 10 |
| | | | `13_kani-findings-11.md` | 11 (this) |

`9_kani-review.md` is left verbatim: it is a dated audit snapshot that quotes the
old `6_kani-findings_6.md` *as its example of the drift*; rewriting it would make
the example self-contradictory. It is the one intentionally-preserved mention of
the old name.

## CI budget audit

Budget (plan §8): **each harness ≤ ~5 min solver time, the whole job ≤ ~30 min.**
Summing the per-harness times recorded in parts 1–10 (dev machine, cargo-kani
0.67.0; CI runners differ but ratios hold):

| Group | Σ time | Dominant harness |
|---|---|---|
| §4.1 CDT + teardown (`kcore`) | ~610 s | `check_revoke` ~193 s |
| §8 transition (`kcore`) | ~475 s | `check_cdt_transition_system` ~315 s |
| §4.3 channel (`kcore`) | ~212 s | `check_ring_fifo` ~142 s |
| §4.5 aspace (`kcore`) | ~30 s | `check_unmap_exact` ~10.5 s |
| §4.4 notif/thread (`kcore`) | ~26 s | `check_waiter_fifo` ~11.6 s |
| §4.2 untyped + §4.6 sysabi (`kcore`) | ~5 s | — |
| §4.7 host (`urt`/`ipc`/`dma-pool`/`cas`) | ~30 s | — |
| **Total** | **~1380 s ≈ 23 min** | |

So the suite is **under the 30-min budget, with ~7 min of headroom** — exactly
the "approaching, not over" the review noted. `kcore` is ~22 of the 23 minutes;
the host crates are ~30 s combined.

**The one per-harness overage:** `check_cdt_transition_system` is ~315 s ≈ 5.25 min,
just over the per-harness 5-min target. This is the *known* cost of raising its
op-sequence to K=3 in rec. #2 (`10_kani-findings-8.md`); it verifies, and the
revert lever (`const K = 2`, ~131 s) is one line, left as the user merged it.

### What this part does about it

- **Enforce the budget.** The `kani` job had **no `timeout-minutes`**, so a
  runaway harness would have burned the GitHub-Actions 6-hour default. Added
  **`timeout-minutes: 45`** — a runaway guard with margin above the 30-min target
  (CI runners are slower than the dev machine, and a cache-miss
  `cargo install kani-verifier` adds minutes that count against the job clock).
  The documented *target* stays 30 min; 45 is the hard cap that turns "the job
  should finish in 30 min" into "the job cannot silently run for hours."
- **Document the escape valve, defer the work.** When the suite next outgrows the
  budget, split `kcore` into a CI matrix by `--harness` group so the expensive
  harnesses (`check_cdt_transition_system`, `check_revoke`, `check_delete_step`,
  `check_ring_fifo`, the nondet-shape CDT trio) run in parallel jobs. The cost:
  it trades away the current "no `--harness` filter ⇒ a newly-added harness
  auto-gates" property, so it would need a companion check that every harness is
  assigned to a group. Because `kcore` is ~95 % of wall-clock, *nothing short of
  intra-`kcore` splitting reduces it* — a crate-level matrix would not help — so
  for a "not urgent" item the proportionate action is to enforce + document, not
  to restructure CI now.

## Status of recommendation #5

✅ Done. The one drifting filename is renamed and the convention is written down
so it is not reintroduced; the CI budget is now enforced by a `timeout-minutes`
guard with the growth path documented. This closes the review's recommendation
list (#1 DN-4, #2 transition, #3 `cover!`, #4 dma, #5 this).
