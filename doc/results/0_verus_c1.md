# C1 — ipc codec `bit_vector` lemmas (evaluation)

Task **C1** (ranks 16/17/19, Wave C) from `doc/plans/0_verus-optimization.md`:
the six codec-bijection lemmas in `ipc/src/session.rs` and `ipc/src/header.rs`
each re-derive the *same* little-endian byte split/reassemble identities inline
with `assert(…) by (bit_vector)`. C1 extracts the four distinct identities
(u16/u32 × split/reassemble) into named `by (bit_vector)` lemmas and replaces
every inline block with a one-line call. The plan groups it as three sub-tasks —
**C1a** (`lemma_u32_le_split_bytes`), **C1b** (`lemma_u32_le_reassemble`), and
**C1c** (the `u16` pair, the "bundle or skip" item) — all landed together as one
coordinated sweep. This file records the per-attempt evaluation under the plan's
§2 protocol. Temporary intermediate report (per `CLAUDE.md`, not citable from
code/specs/guidelines).

- **Kind:** the plan rates C1 a **simplification** (`simp`, technique
  `decompose`): "the value is removing duplication, not speed." Under §2 the
  simplification axis keeps it iff the diff is a clear readability win **and** the
  crate SMT total did not materially regress (<5 % tolerance). In the event it is
  also a large *speedup* (the same identity is now bit-blasted once instead of at
  every call site), so both axes are satisfied with room to spare.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p ipc` before each); gate from the
  plain-text `verification results::` line, timing from a separate cold
  `-- --time-expanded --output-json` run ranking
  `.["times-ms"].smt["smt-run-module-times"][]."function-breakdown"[]`.
- **Baseline.** C1 branches off `main` (`e20baa4`) and edits only the ipc crate.
  A fresh cold "before" was captured on the unedited branch base *prior to any
  edit* (so before/after isolate C1 exactly, no stash needed): **69 verified, 0
  errors, ipc SMT 301 ms** — matching the committed `target/verus-baseline/ipc.json`
  (310 ms) within run-to-run noise. The untouched `reactor::lowest_clear_bit`
  obligation is the control: its deterministic `rlimit` must not move.

## The change

A new crate-internal module `ipc/src/le_bytes.rs` holds the four identities as
`pub(crate) proof fn … by (bit_vector)` in the **§6 recipe form** (`by (bit_vector)`
on the signature, the fact as an unconditional `ensures`, empty body — the shape
`urt/src/slots.rs` adopted in task A5): `lemma_u16_le_reassemble`,
`lemma_u16_le_split_bytes`, `lemma_u32_le_reassemble`, `lemma_u32_le_split_bytes`.
Each `ensures` clause is **byte-identical** to the inline assert expression it
replaces, so a call delivers exactly the fact the surrounding proof needs with no
bridging assert. `lib.rs` gains `pub(crate) mod le_bytes;`.

The six codec lemmas then drop their inline `by (bit_vector)` blocks for one-line
calls (every surrounding `let` and the trailing plain `assert(… =~= …)`
extensionality line is kept verbatim):

- `session::lemma_req_decode_encode` — 1 assert → `lemma_u32_le_reassemble(rw)`
- `session::lemma_req_encode_decode` — 4 asserts → `lemma_u32_le_split_bytes(s1,s2,s3,s4)`
- `session::lemma_grant_decode_encode` — 2 asserts → two `lemma_u32_le_reassemble`
- `session::lemma_grant_encode_decode` — 8 asserts → two `lemma_u32_le_split_bytes`
- `header::lemma_decode_encode` — 3 asserts → two `lemma_u16_le_reassemble` + one `lemma_u32_le_reassemble`
- `header::lemma_encode_decode` — 8 asserts → two `lemma_u16_le_split_bytes` + one `lemma_u32_le_split_bytes`

**Cross-module reference (the one non-obvious detail).** The `verus!{}` macro
cfg's `proof fn`s out of a normal (non-Verus) `cargo build`/`cargo test`, so a
top-level `use crate::le_bytes::{lemma_…}` naming those items fails to resolve in
the plain build (it is a real Rust `use`, not erased). The lemmas are therefore
called by **full path** — `crate::le_bytes::lemma_u32_le_reassemble(rw)` — inside
the `proof fn`s, which *are* cfg'd out together with their bodies, so nothing
leaks to the plain build. (`vstd` sidesteps this with glob `use super::seq_lib::*`,
which tolerates cfg'd-out items; a named import does not.) The now-stale header.rs
comment explaining why the reassembly was "written inline here" is dropped, as
extraction makes it a call.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p ipc && cargo verus verify -p ipc` ended with

```
verification results:: 47 verified, 0 errors
```

**present** (a real cold run, not stale cache). `N` fell **69 → 47**, **−22**,
exactly as predicted: the **26** inline `by (bit_vector)` sub-obligations (15 in
session, 11 in header) collapse, and **4** new lemma obligations are added
(69 − 26 + 4 = 47). **Gate: PASS (Y).**

## Measurement (§2 step 2b — cold timing vs. branch base)

Every codec lemma sheds SMT time and rlimit; the crate total nearly halves. The
new lemmas are cheap (≈21 ms combined) and the control is flat:

| obligation | SMT ms (before → after) | rlimit (before → after) |
|---|---:|---:|
| `session::lemma_grant_encode_decode` | 51 → **10** | 157 433 → **71 099** |
| `header::lemma_encode_decode` | 51 → **9** | 113 641 → **57 671** |
| `session::lemma_grant_decode_encode` | 40 → **11** | 305 295 → **76 379** (4.0×) |
| `header::lemma_decode_encode` | 29 → **7** | 137 358 → **45 094** |
| `session::lemma_req_encode_decode` | 28 → **5** | 74 892 → **36 096** |
| `session::lemma_req_decode_encode` | 19 → **4** | 142 613 → **29 399** |
| `le_bytes::lemma_u16_le_reassemble` | — → 5 | — → 1 672 |
| `le_bytes::lemma_u16_le_split_bytes` | — → 5 | — → 1 846 |
| `le_bytes::lemma_u32_le_reassemble` | — → 6 | — → 19 210 |
| `le_bytes::lemma_u32_le_split_bytes` | — → 5 | — → 2 184 |
| **control** `reactor::lowest_clear_bit` | 34 → 35 | 165 531 → **165 531** |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| crate SMT total | 301 ms | 152 ms | **0.50× (−49.5 %)** |

The control's rlimit is **byte-identical** (165 531 → 165 531; the 34→35 ms is the
±1 ms wobble §2 warns of), so the whole crate-total drop is attributable to the
change, not noise. The decisive run-independent signal is the per-lemma **rlimit
collapse** — `lemma_grant_decode_encode` 305 K → 76 K (4×) is a genuine proof-size
reduction: each unique bit identity is now bit-blasted **once** in `le_bytes` and
merely *cited* at its call sites, instead of re-bit-blasting 26 inline SAT queries.
Far from a regression, the crate halves; the simplification axis's <5 % regression
tolerance is moot.

## Host tests

`cargo test -p ipc` — green: **33 passed, 0 failed** (the codec round-trip and
admission/reactor proptests, plus the fuzz-corpus harness). The change is
proof-only (the lemmas live in `proof fn`s, erased from the exec build), so the
wire bytes are unchanged by construction — the suite confirms the plain build
resolves the full-path calls and behavior is identical.

## Clarity (§2 step 4)

**Cleaner.** Twenty-six multi-line inline `assert(…) by (bit_vector)` blocks —
the same four identities re-spelled at six sites with the full reassembly written
out each time — become twelve one-line citations of four named, self-documenting
lemmas defined once in a dedicated module. The codec-bijection lemmas now read as
"split recovers the bytes / reassembly recovers the value, then extensionality
closes it," with the bit arithmetic named rather than re-derived; a reader (or
auditor) sees the *intent* at the call site and the *proof* in one place. The new
module matches the established §6 recipe and the crate's existing
`lemma_<width>_le_<dir>` precedent. The only cost is the full-path call form, a
minor verbosity that also documents where the shared identity lives.

## Decision

**KEEP.** Both §2 axes are satisfied with margin: as a *simplification* the diff
is a clear readability win (26 inline asserts → 4 reusable lemmas + 12 calls, gate
47/0, 33 host tests green), and the crate SMT total did not merely avoid
regressing — it **halved** (301 → 152 ms), a free optimization bonus on a
clarity task.

> verified **Y** (69 → **47**, −22: 26 inline `by (bit_vector)` sub-obligations →
> 4 reusable lemmas) · codec lemmas **51→10 / 51→9 / 40→11 / 29→7 / 28→5 / 19→4 ms**
> (rlimits collapse up to 4×) · crate SMT **301 → 152 ms (−49.5 %)** · control
> rlimit byte-identical · clarity **cleaner** → **KEEP**
