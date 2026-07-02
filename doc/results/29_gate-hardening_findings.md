# Findings 29 — gate hardening (C6, review finding 18)

Task **C6** of `doc/plans/3_plan-std-correction.md`, acting on finding **18**
(minor) of the independent review (`doc/results/22_std-port-review.md`). Two
gates were honest about *passing* but silent about *coverage*:

- **The Verus gate asserted only exit-0.** The CI `verus` job ran 11 hand-rolled
  `cargo verus verify -p <crate>` steps and checked nothing beyond their exit
  code. A silently-dropped proof obligation — the prover proving *less* while
  still exiting 0 — or a stale-cache false-green (the `CLAUDE.md` trap: a scoped
  re-run over an unchanged `target/` reports *nothing* and exits 0) would keep CI
  green. Nothing pinned the verified counts; the trusted-base ledger's
  `## Baselines` cells were prose only.
- **`scripts/libtest-skips/coretests.skip` read as a placeholder.** It was
  header-comment-only (zero skip entries), but its closing line said "Entries
  below are the triaged skips" — promising entries that did not exist rather than
  declaring the empty list a *verified current state*.

C6 gives the Verus gate the treatment the TLA gate already has
(`TLC_ASSERT_MANIFEST` / `tools/tla/model-manifest.tsv`, "a coverage shrink fails
a PR") and makes the skip list state what its emptiness means. It changes **no**
verified code — the counts are unchanged; the gate merely pins them — so no
obligation moves and the `external_body`/`assume_specification` tally stays 14.

## C6.1 — pin verified counts in CI

### What shipped

- **`tools/verus/verus-manifest.tsv`** (new) — the machine-readable twin of the
  ledger's `## Baselines`, one row per gated crate in CI order, columns
  `crate  flags  expected_verified`:

  | crate | flags | verified |
  |---|---|---|
  | kcore | | 408 |
  | ipc | | 71 |
  | urt | | 30 |
  | freelist | | 30 |
  | le-bytes | | 6 |
  | dma-pool | | 0 |
  | cas | `--no-default-features` | 79 |
  | virtio-blk | | 3 |
  | storage-server | `--no-default-features --lib` | 19 |
  | loader | `--no-default-features` | 30 |
  | eunomia-sys | | 16 |

- **`tools/verus/verus-gate.sh`** (new, executable) — modeled on
  `tools/tla/tla-assert-coverage.sh`. Per row: a COLD
  `cargo clean -p <crate> && cargo verus verify -p <crate> <flags>`, then extract
  the `verification results:: N verified, M errors` line and **fail** if the line
  is absent (stale cache / build failure), if `M != 0`, or if `N !=
  expected_verified` (a `COUNT REGRESSION`); an unpinned (`-`) count is a
  fail-loud misconfiguration. Failures accumulate and are all reported; exit is
  non-zero if any row fails.

- **CI** (`.github/workflows/ci.yml`, `verus` job) — the 11 hand-rolled
  invocations are replaced by `bash tools/verus/verus-gate.sh` (the PATH-setup
  that puts the pinned `cargo-verus` on `$PATH` is kept). The per-crate rationale
  for *what* each crate verifies already lives in the ledger `## Baselines`
  prose, so nothing was lost.

- **`scripts/verus-baseline.sh`** — its hardcoded `ALL_CRATES`/`verus_args_for`
  now derive the crate list and flags from the same manifest (via `awk -F'\t'`),
  so the timing sweep and the gate share one source of truth. This also fixed a
  latent gap: the old `ALL_CRATES` omitted `le-bytes` and `eunomia-sys` (added by
  the std port), so the baseline was silently skipping two gated crates.

- **Ledger** (`doc/guidelines/verus_trusted-base.md`, `## Baselines` intro) — one
  sentence naming `tools/verus/verus-manifest.tsv` as the machine-readable twin
  CI asserts against, which must stay in agreement with the Result cells.

### Decisions (with rejected alternatives)

- **One self-contained driver, not the TLA runner+asserter split.** TLA separates
  `tla-model-check.sh` (runner) from `tla-assert-coverage.sh` (asserter) because
  TLC is invoked many ways; the Verus gate has exactly one shape (cold-verify a
  crate, assert its count), so the plan's single driver is simpler and was
  followed. Text-line parse (`grep`/`sed`) — no `jq` dependency, unlike the
  JSON-based `verus-baseline.sh`.
- **`tail -1` selects the target crate's results line.** A cold verify can emit a
  `verification results::` line per verified crate in the build graph (a cold
  `-p urt` re-verifies its gated deps le-bytes/ipc/freelist too). cargo builds
  the `-p` target last, and a crate can only be verified after its deps compile,
  so the target's line is always the final one. Verified two ways: a stub-`cargo`
  case that emits a dep line (6) before the target line (7) and asserts 7; and
  the real gate, where `dma-pool` (0 obligations, the 30 live in `freelist`)
  correctly asserts 0 rather than freelist's 30.
- **Manual tab-split, not `IFS=$'\t' read`.** Tab is an IFS *whitespace*
  character, so `IFS=$'\t' read -r crate flags expected` collapses the adjacent
  tabs of an empty-flags row (`kcore<tab><tab>408`) into one delimiter and
  mis-shifts the columns (crate=kcore, flags=408, expected=""). The TLA manifest
  never hits this because none of its fields are empty. A stub-`cargo` self-test
  caught it: the driver reported *every* row as unpinned. Fixed by splitting each
  row with parameter expansion in the driver, and with `awk -F'\t'` (which does
  not collapse) in the baseline — noted in both so a future editor does not
  "simplify" back to a whitespace-IFS read.
- **The cold `cargo clean -p <crate>` per row is load-bearing.** It defeats the
  stale-cache false-green: without it, a warm `target/` (CI restores one via
  `Swatinem/rust-cache`) can let `cargo verus verify` exit 0 while re-verifying
  nothing and printing no results line. With the clean, an absent results line is
  itself a hard failure, so a real verification is forced every run.
- **`verus-baseline.sh` reads the manifest** (per the user's scope decision) —
  the faithful mirror of `scripts/tla-baseline.sh` reading
  `model-manifest.tsv`, making the manifest the single source of truth for every
  executable crate list and eliminating the le-bytes/eunomia-sys drift. Rejected:
  a minimal two-line add to the hardcoded list (leaves two lists that can drift
  again — the disease this task treats), and leaving it untouched.

### Gate — commands and result lines

- **Full cold gate** — `bash tools/verus/verus-gate.sh` (verus
  `0.2026.06.07.cd03505`, toolchain 1.95.0):

  ```
  ok: kcore 408 verified, 0 errors (pinned 408)
  ok: ipc 71 verified, 0 errors (pinned 71)
  ok: urt 30 verified, 0 errors (pinned 30)
  ok: freelist 30 verified, 0 errors (pinned 30)
  ok: le-bytes 6 verified, 0 errors (pinned 6)
  ok: dma-pool 0 verified, 0 errors (pinned 0)
  ok: cas 79 verified, 0 errors (pinned 79)
  ok: virtio-blk 3 verified, 0 errors (pinned 3)
  ok: storage-server 19 verified, 0 errors (pinned 19)
  ok: loader 30 verified, 0 errors (pinned 30)
  ok: eunomia-sys 16 verified, 0 errors (pinned 16)
  verus gate: all crates match their pinned verified counts
  ```

  Every `verification results::` line was present (a real cold run, not stale
  cache); exit 0.
- **Anti-vacuity** — a one-row manifest pinning `urt` at 31 (true count 30, so
  only urt re-verifies) →
  `COUNT REGRESSION: urt verified 30 != expected 31 — reconcile
  tools/verus/verus-manifest.tsv with the ## Baselines ledger cell …`, exit 1.
  Reverted (the committed manifest pins 30). A real cold verify of urt, not a
  stub — the mechanism is armed.
- **Baseline still works** — `scripts/verus-baseline.sh le-bytes` cold-verified
  `le-bytes` (6 verified, 0 errors) and produced the `--time-expanded` JSON
  timing; its `ALL_CRATES` now enumerates all 11 gated crates.
- **Agreement** — the manifest's 11 counts equal the ledger `## Baselines` Result
  cells and the counts the full gate printed. `.github/workflows/ci.yml` parses
  (Ruby `YAML.load_file`).

## C6.2 — honest coretests skip list

Re-ran the on-target triage: `bash scripts/libtest-on-target.sh --full --suite
coretests` (whole-suite, applying the empty skip list) →

```
test result: ok. 2590 passed; 0 failed; 5 ignored; 151 filtered out; …
LIBTEST ON-TARGET PASS (full): 1 module runs green, 2590 tests passed, 0 failed, 0 aborts
```

Zero panics/faults in the QEMU log; QEMU cleanly reaped. **Zero coretests skips
is the true state** — the 151 "filtered out" are the `#[should_panic]` tests the
runner's `--exclude-should-panic` drops wholesale, the 5 "ignored" are upstream
`#[ignore]`s; neither is a per-test exclusion this project owns.

`scripts/libtest-skips/coretests.skip`'s closing note was rewritten from "Entries
below are the triaged skips" (a promise of entries that do not exist) to a
statement that the empty list is the verified current state, naming the run that
established it and the meaning of the `filtered out`/`ignored` counts. The shared
boilerplate (the consumed-by paragraph, the `--skip` grammar, the should-panic
`NOTE`, the `Format:` line) stays, since it documents how an entry would be added
if a forward-port ever surfaces one. The file remains plan-token-free (its header
was de-planned in C4) and empty of non-comment lines, so
`scripts/libtest-on-target.sh` consumes it exactly as before — no runner change.

## Surface left trusted

- The gate trusts cargo's build ordering (the `-p` target compiles last) for the
  `tail -1` line selection; validated by the stub dep-line case and the real
  `dma-pool` 0-vs-30 case. No proof, spec, or wire/ABI surface moves — C6 pins
  existing counts and rewrites a comment; the verified tally stays 14.
- The coretests empty-skip claim rests on a whole-suite `--full` run under the
  MVP fixed-heap PAL at one nightly; a forward-port re-runs the triage (the
  header says how), which is the standing discipline, not new trust.

## Follow-ups

- **Pre-existing user-workspace lockfile staleness (out of scope, reverted).** A
  kernel build (`EUNOMIA_BUILD_LIBTESTS=1 cargo build`, or any `user/*` build via
  `kernel/build.rs`) regenerates all ten `user/*/Cargo.lock` to add `le-bytes`
  under `urt` — the C1.3 DRBG-fill change added the `urt → le-bytes` dep and
  updated the root-workspace lock but not the separate `user/*` mini-workspace
  locks (the `CLAUDE.md` separate-workspace trap). These regenerations were
  reverted here to keep the gate-hardening diff focused; they belong in a
  dedicated lockfile-sync commit.
- **CI verus-job wall time.** The per-crate `cargo clean` forces a real recompile
  + re-verify of each crate every run (deps stay warm), where the old job could
  ride a warm `target/`. Watch the first CI run against the `verus` job's
  `timeout-minutes: 20`; bump it if the honest cold gate nears the cap. (The
  local cold gate completed within the run window; kcore's 408-obligation verify
  dominates.)
