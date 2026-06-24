# A4 — cas `recover_records` push-preserves-`rec_ok` lemma (evaluation)

Task **A4** (rank 36, Wave A) from `doc/plans/0_verus-optimization.md`: extract the
inline ~30-line `assert forall|k| … implies rec_ok(wal@, r, k) by {…}` block inside
`cas::store::recover_records` (`cas/src/store.rs`) — the loop step that proves the
freshly pushed record keeps the per-record framing invariant — into a named
`proof fn lemma_push_preserves_rec_ok` (`doc/guidelines/verus.md` §10 —
decomposition is the default fix). One of the pre-measured (`[measured]`) entries,
flagged a **simplification, not a speedup**: the lemma costs roughly what the hot
obligation sheds, so the payoff is rlimit headroom on the gate's hottest store
obligation plus a named, self-documenting block. This file records the per-attempt
evaluation under the plan's §2 protocol. Temporary intermediate report (per
`CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** simplification (decompose).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p cas` before each);
  `cargo verus verify -p cas --no-default-features`. Gate from the plain-text
  `verification results::` line; timing from a separate cold
  `-- --time-expanded --output-json` run, ranking
  `.["times-ms"].smt["smt-run-module-times"][]."function-breakdown"[]`.
- **Baseline note (which number is A4's).** A4 is branched off `main` (which carries
  A1+A2 but **not** A3 — A3/PR #186 was still in flight) and edits only the store
  module's `recover_records` region, disjoint from A3's payload-ok split. The on-disk
  `target/verus-baseline/cas.json` is **pre-A2**, so its store total would conflate
  the A2 prolly pass; A4's before/after are therefore both measured cold on **this
  branch's base** (`a4-before.json` / `a4-after.json`) to isolate A4 cleanly. The
  base `recover_records` figure (96 ms / rlimit **660 042**) reproduces the plan's
  `[measured]` projection exactly, confirming the base is the right reference. The
  deterministic `rlimit` field — run-independent — is the decisive signal here (§2:
  a large rlimit drop is strong evidence even when ms wobble).

## The change

`recover_records`'s per-step proof block kept a ~30-line
`assert forall|k| … implies rec_ok(wal@, r, k) by { if k < n {…} else {…} }`
re-establishing the loop's `rec_ok` invariant after each `records.push(…)`. That
block now moves verbatim into
`proof fn lemma_push_preserves_rec_ok(wal, prev, r, new: RecMeta, rlen)`, sited
beside the existing `rec_ok`/`laid_out` cluster (`lemma_forall_laid_out`). Its
`requires` are exactly the cheap local facts the call site already had in scope —
`forall|j| rec_ok(wal, prev, j)` (the loop invariant), `r == prev.push(new)`, the
new record's `frame_at`/`content_ok_spec`/`seq < u64::MAX` facts, and the two cursor
contiguity clauses (`prev.len() > 0 ==> new.off/seq just past the previous last`,
the loop's own invariants). Its `ensures` is the full
`forall|k| 0 <= k < r.len() ==> rec_ok(wal, r, k)`. The pushed `RecMeta` is passed
**by value** (`new`) rather than reconstructed, so the `Vec<u8>` `ref_name` literal
never appears in spec context (the hazard the plan flags). The call site collapses
to four bridging asserts (establishing the `requires`) plus the lemma call. No logic
change, no verifier attributes (none existed). Matches the file's existing
`lemma_forall_laid_out` / `rec_ok` / `laid_out` decomposition style.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p cas && cargo verus verify -p cas --no-default-features` ended with

```
verification results:: 83 verified, 0 errors
```

**present** (a real cold run, not stale cache). `N` rose **82 → 83**, **+1** — the
single new `proof fn lemma_push_preserves_rec_ok`. (A `proof fn` carries an `ensures`
to discharge, so unlike A3's non-recursive *spec* helpers it does increment `N`; the
delta is exactly the one new lemma and nothing else shifted.) **Gate: PASS (Y).**

## Measurement (§2 step 2b — cold timing vs. branch base)

The hot obligation sheds half its SMT and ~42 % of its rlimit; the extracted lemma
absorbs a comparable slice in its own isolated context:

| obligation | SMT ms | rlimit |
|---|---:|---:|
| `recover_records` (**before**) | 96 | **660 042** |
| `recover_records` (after) | 47 | **381 205** |
| `lemma_push_preserves_rec_ok` (new) | 53 | 338 213 |

Store module (A4's only surface):

| metric | before | after | ratio |
|---|---:|---:|---:|
| store-module SMT time | 245 ms | 249 ms | 1.02× (+1.6 %) |

The `recover_records` rlimit cut (**660 042 → 381 205**, 1.73×) reproduces the plan's
`[measured]` projection (`660 042 → 384 799`) to within run noise and is the
decisive, run-independent signal. As the plan anticipated, the new lemma's ~53 ms
roughly offsets the obligation's ~49 ms saving, so the store-module total is **flat**
(+4 ms / +1.6 %, inside the §2 ±5–15 % ms band — a wash, not a regression). This is a
*simplification*: the win is the ~279 K rlimit headroom freed on the gate's hottest
store obligation plus a named 30-line block, not wall-clock.

## Clarity (§2 step 4)

**Cleaner.** A ~30-line inline `assert forall … by { … }` buried in the recovery
loop becomes a one-line lemma call against a named, contract-bearing
`proof fn` whose `requires`/`ensures` document *exactly* what the push step needs and
guarantees. The lemma joins the file's existing `rec_ok`/`laid_out` lemma cluster,
matching `lemma_forall_laid_out`'s idiom. The loop body now reads as four short
fact-establishing asserts plus the call, and the proof obligation runs against a
small isolated context instead of being re-derived against the whole loop query each
iteration.

## Host tests

`cargo test -p cas` — green: 133 lib (the `mount_recovery` / `wal_replay_scan`
proptests drive `recover_records` through the WAL replay path), 9 integration
(1 ignored), 10 fuzz-regression; 0 failed. The change is proof-only (lives entirely
inside `proof { }`), so runtime behavior is unaffected by construction.

## Decision

**KEEP.** Simplification asymmetry satisfied: the diff is a clear readability win
(named lemma + contract vs. an inline forall in the hot loop), the gate passes
83/0, and the store-module total did not materially regress (+1.6 %, within the §2
noise band and well under the 5 % tolerance). The deterministic 1.73× rlimit drop on
the hottest store obligation is banked headroom on the clarity axis.

> verified **Y** (82 → **83**, +1 lemma) · `recover_records` **96 ms / rlimit
> 660 042 → 47 ms / rlimit 381 205** (1.73×) + new lemma 53 ms · store module
> **245 → 249 ms** (+1.6 %, flat) · clarity **cleaner** → **KEEP**
