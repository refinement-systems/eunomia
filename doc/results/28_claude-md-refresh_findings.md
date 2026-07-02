# Findings 28 — `CLAUDE.md` crate-list refresh (C5, review finding 17)

Task **C5** of `doc/plans/3_plan-std-correction.md`, acting on finding **17**
(minor) of the independent review (`doc/results/22_std-port-review.md`) plus
one gap the review missed: `CLAUDE.md`'s three crate/command enumerations had
drifted from the actual `Cargo.toml` workspace and CI verus job.

## The problem (finding 17 + the missed gap)

Finding 17 observed that the std-port effort added `eunomia-sys`, `le-bytes`,
and `eunomia-sys/fuzz` to the workspace but never updated `CLAUDE.md`'s
fmt-caveat sentence, which lists which root-workspace members a plain
`cargo fmt` formats and which separate `*/fuzz` workspaces it silently skips
— exactly the trap the caveat exists to warn about. Separately (not named by
finding 17), `CLAUDE.md`'s illustrative from-clean Verus gate command block
was missing the two `cargo verus verify -p ...` invocations the real CI verus
job runs for `le-bytes` and `eunomia-sys`, so a reader following `CLAUDE.md`
literally would run an incomplete gate.

## Scope decision — `scratchpad` left out

While verifying the lists against the tree, `scratchpad` (`Cargo.toml:17`)
turned up as a third root-workspace member missing from the fmt-caveat
sentence. It is not named by finding 17 or by the plan's C5.1 deliverable
text — the review's only mention of `scratchpad` is a housekeeping note
("pre-existing, not this effort's artifact"), never checked against this
sentence. Confirmed with the user before implementing: **C5.1 stays scoped to
exactly what finding 17 and the plan name** (`eunomia-sys`, `le-bytes`,
`eunomia-sys/fuzz`); `scratchpad`'s absence from the fmt-caveat sentence is
left as-is. Recorded here explicitly so it reads as a considered, deliberate
scope decision rather than a second missed gap. `scratchpad` also does not
appear in the CI verus job's `-p` list (`git grep -n scratchpad
.github/workflows/ci.yml` → no hits), so the Verus gate block needs no
`scratchpad` line either.

## What changed

Three edits to `CLAUDE.md`, all inside existing sections:

- **fmt-caveat root-workspace list** (`CLAUDE.md:325-328`): added
  `` `eunomia-sys` `` and `` `le-bytes` ``.
- **excluded-fuzz-workspace list** (`CLAUDE.md:329-331`): added
  `` `eunomia-sys/fuzz` ``.
- **Verus gate command block** (`CLAUDE.md:216-255`): inserted
  `cargo verus verify -p le-bytes` (between `freelist` and `dma-pool`) and
  `cargo verus verify -p eunomia-sys` (after `loader`, at the end), each with
  a one-line purpose annotation in the block's existing wrapped-`#`-comment
  house style, positioned to match the real CI job's order exactly. Wording
  is based on the ledger's own descriptions
  (`doc/guidelines/verus_trusted-base.md:666` for `le-bytes`, `:674` for
  `eunomia-sys`) so the annotations stay consistent with the source of
  record.

No code, spec, or proof changes — this is a documentation-only pass.

## Verification record

- **`Cargo.toml` diff check.** Root `Cargo.toml` `members`:
  `kernel, kcore, ipc, freelist, le-bytes, dma-pool, cas, storage-server,
  virtio-blk, loader, mkfs, urt, eunomia-sys, scratchpad`. The fmt-caveat
  sentence now lists every one of these except `scratchpad` (the deliberate
  scope decision above) — exact match otherwise. `exclude`: `cas/fuzz,
  storage-server/fuzz, loader/fuzz, ipc/fuzz, eunomia-sys/fuzz` — the
  fuzz-exclusion sentence now matches this list exactly, no gaps.
- **CI verus job diff check.** `.github/workflows/ci.yml`, job `verus`, step
  "Verify kcore + host chokepoints" runs, in order: `kcore` (:342), `ipc`
  (:348), `urt` (:349), `freelist` (:354), `le-bytes` (:360), `dma-pool`
  (:361), `cas --no-default-features` (:367), `virtio-blk` (:378),
  `storage-server --no-default-features --lib` (:391), `loader
  --no-default-features` (:403), `eunomia-sys` (:414) — 11 invocations. The
  `CLAUDE.md` Verus gate block now lists the same 11 crates in the same
  order, with the same flags on `cas`/`storage-server`/`loader` and no flags
  on `le-bytes`/`eunomia-sys` (matching their plain CI invocations). Exact
  match.
- **Rendering.** `git diff CLAUDE.md` reviewed: list punctuation and the two
  new fenced-block entries render correctly; continuation-`#` alignment
  within each new entry matches the pattern already used for
  `cas`/`virtio-blk`/`storage-server`/`loader`.
- No `cargo fmt`/`cargo verus verify`/smoke-test gate applies (doc-only
  change, no source or proof files touched).

## Surface left trusted

None — comment/documentation-only change, no runtime path, wire/ABI, or
verified obligation moves.

## Follow-ups

- If `scratchpad` ever needs the same fmt-caveat treatment as `eunomia-sys`/
  `le-bytes` were given here, it is not blocked on anything — just out of
  this task's scope per the recorded decision above.
