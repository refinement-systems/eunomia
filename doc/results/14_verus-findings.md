# 14 — Verus findings: `le-bytes` alloc-prelude cost measurement (Phase 2.2)

Date: 2026-06-26. Crate: `le-bytes` (created in Phase 2.1). This is a temporary
intermediate record per CLAUDE.md; it is not referenced from comments, specs, or
guidelines.

## Purpose

Phase 2.2 of `doc/plans/0_verus-improvements.md` is a measurement gate between the
`le-bytes` crate's creation (2.1) and the consumer migration (2.3). It decides whether
any `le-bytes` obligation needs an explicit `#[verifier::rlimit(N)]` budget sized to the
*alloc-prelude worst context* before consumers (cas, loader) re-verify it under
`vstd[alloc]`. `rlimit` is deterministic only for byte-identical SMT input, and a shared
crate's SMT input depends on the `vstd` prelude in scope, which cargo feature-unifies
globally per invocation (`doc/guidelines/verus.md` §10/§15.5). The freelist precedent
(`verus_trusted-base.md` row for `freelist`) carries `rlimit(120)/rlimit(40)` on its
merge proofs sized for a ~1.4–1.85× alloc-context blowup. The plan predicted `le-bytes`'s
blowup would be far milder — "possibly zero" — and instructs: do not add an `rlimit`
speculatively, only if a cold alloc-context run exceeds the default.

`le-bytes` has 6 proof obligations: `lemma_u{16,32,64}_le_bytes` (empty-bodied
`by (bit_vector)` split identities) and `read_u{16,32,64}_le` (exec byte readers). The
`u{16,32,64}_le` specs are `open spec fn`s and carry no SMT obligation of their own.

## Method (both runs cold, `cargo clean` first)

1. No-alloc standalone baseline:
   ```sh
   cargo clean
   cargo verus verify -p le-bytes -- --time-expanded --output-json
   ```

2. Alloc prelude. The plan's literal `-p virtio-blk` alone would not touch the still-
   orphaned `le-bytes` (no consumer depends on it before 2.3). To exercise it under the
   alloc prelude it is co-verified in the **same** cargo invocation as an alloc-pulling
   crate, so cargo feature-unifies `vstd[alloc]` across the whole build graph and
   re-checks `le-bytes` under the larger prelude:
   ```sh
   cargo clean
   cargo verus verify -p le-bytes -p virtio-blk -- --time-expanded --output-json
   ```
   (`virtio-blk → cas → vstd[alloc]`, the path the freelist ledger note names.)

Per-function `rlimit` was read from
`times-ms.smt.smt-run-module-times[].function-breakdown[]`. Both runs used Verus
`0.2026.06.07.cd03505`, toolchain `1.95.0` (the pinned binary).

### Confirmation the alloc prelude was actually in scope

In run 2 the co-verified crates `cas` (77), `freelist` (30), `dma-pool`, `virtio-blk`
(3) all verified with 0 errors — these require `vstd[alloc]`. Decisively, the same
`vstd` reported **1495** verified obligations in the no-alloc run and **1533** in the
alloc run: vstd is building its `alloc`-gated specs, so `vstd[alloc]` was unified onto
the single shared vstd that `le-bytes` was verified against. The `le-bytes` lemma
`rlimit` figures also shifted between the runs (see below), which — since `rlimit` is
deterministic for byte-identical SMT input — independently confirms `le-bytes` saw a
different (alloc) prelude in run 2, not the no-alloc one.

## Results

Both runs: `le-bytes` = **6 verified, 0 errors**. The default `--rlimit` is 10
("roughly seconds"), i.e. a 10,000,000-unit Z3 ceiling; the table's `% default` column
is the alloc-context consumption against that ceiling.

| function              | no-alloc `rlimit` | alloc `rlimit` | factor | % default |
|-----------------------|------------------:|---------------:|-------:|----------:|
| `read_u64_le`         |           171,177 |        162,580 |  0.95× |    1.626% |
| `read_u32_le`         |            76,410 |         69,149 |  0.90× |    0.691% |
| `lemma_u64_le_bytes`  |            40,181 |         43,644 |  1.09× |    0.436% |
| `lemma_u32_le_bytes`  |            37,817 |         41,280 |  1.09× |    0.413% |
| `lemma_u16_le_bytes`  |            36,625 |         40,088 |  1.09× |    0.401% |
| `read_u16_le`         |            27,745 |         27,863 |  1.00× |    0.279% |

The three `by (bit_vector)` lemmas rose ~9% under the alloc prelude (a few thousand
units each); the three exec readers were flat-to-slightly-lower (the readers cite
`vstd::slice::group_slice_axioms` and `Seq::subrange`, so they are the prelude-sensitive
ones, yet the different axiom set happened to make their queries marginally cheaper —
within the determinism caveat that a changed prelude is a changed input). The worst
alloc-context consumption is `read_u64_le` at 162,580 ≈ **1.6% of the default ceiling**
(~61× headroom).

## Decision — Branch A: no `rlimit` added

No `le-bytes` obligation comes near the default ceiling under the alloc prelude, and the
blowup is ≤1.09× (vs freelist's 1.4–1.85×) — well inside the plan's "possibly zero"
expectation. The decision is robust, not marginal: even applying freelist's worst 1.85×
to the heaviest no-alloc reader (171,177 → ~317,000) lands at ~3.2% of the default
ceiling. Per the plan's "do not add an `rlimit` speculatively" rule:

- `le-bytes/src/lib.rs` is unchanged — no `#[verifier::rlimit]` attribute.
- `doc/guidelines/verus_trusted-base.md` is unchanged — no alloc-cost routing note (that
  note is added only alongside an `rlimit`, mirroring freelist). The `le-bytes` Baseline
  row itself is added in Phase 2.3 when the relocation from cas/loader/ipc nets to zero.

## No-weakening check

No proof obligation, spec, `ensures`, or input coverage was touched (this phase adds no
code). Both cold runs end with 0 errors and 6 verified `le-bytes` obligations — byte-
identical count to the 2.1 standalone gate. Sizing an `rlimit` ceiling cannot change a
passing proof's work; here no ceiling was added at all.
