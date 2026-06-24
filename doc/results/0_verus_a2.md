# A2 — cas `prolly` codec helper extractions (evaluation)

Task **A2** (ranks 8/12/14, Wave A) from `doc/plans/0_verus-optimization.md`: extract
the inline 3-arm content branches (and a tail field-assembly proof) out of the two
hot `cas::prolly` codec functions into tightly-keyed helpers, so each sub-proof
discharges against a small context (`doc/guidelines/verus.md` §10 — decomposition
is the default fix). This file records the per-attempt evaluation under the plan's
§2 protocol. Temporary intermediate report (per `CLAUDE.md`, not citable from
code/specs/guidelines).

- **Kind:** A2a/A2b optimization (decompose); A2c optimization (decompose),
  conditional.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p cas` before each);
  `cargo verus verify -p cas --no-default-features`. Gate from the plain-text
  `verification results::` line; timing from a separate cold
  `-- --time-expanded --output-json` run, ranking
  `.["times-ms"].smt["smt-run-module-times"][].function-breakdown[]`.

**Baseline note (which number is cas).** The on-disk baseline
(`target/verus-baseline/cas.json`, plan §2 table) reports **1533 verified /
10 312 ms SMT** — that run was cold across the *whole* dependency closure, so those
figures are dominated by **vstd** (its ~1500 library lemmas re-verified in that
run). cas's *own* surface is the second JSON doc / the plain-text gate line:
**80 verified / 1826 ms SMT**. An incremental `cargo clean -p cas` (vstd cached)
re-verifies only cas, so all numbers below are **cas-own** and directly comparable.

## The change

Three extractions in `cas/src/prolly.rs`, all of the "key it tightly" shape
(`requires` = the local facts the body already proves; `ensures` = the heavy
result):

- **A2a — `decode_content`.** The 3-arm content-tag parse moves out of `decode_raw`
  into `fn decode_content(buf, p_ctag) -> Result<(RawContent, usize), TlvErr>`
  (`requires p_ctag < buf@.len()`; `ensures Ok((c,end)) ==> p_ctag < end <= buf@.len()
  && content_bytes(c) == buf@.subrange(p_ctag, end)`), keeping its own
  `broadcast use group_slice_axioms`. `decode_raw` becomes
  `let (content, p_optlen) = decode_content(buf, p_ctag)?;` + one bridging assert.
- **A2b — `encode_content`.** Symmetric: the 3-arm push match moves out of
  `encode_raw` into `fn encode_content(out: &mut Vec<u8>, c: &RawContent)`
  (`ensures final(out)@ == old(out)@ + content_bytes(*c)`). `encode_raw` becomes
  `encode_content(out, &e.content);`.
- **A2c — `lemma_entry_assemble`** (attempted, **reverted** — see below).

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p cas && cargo verus verify -p cas --no-default-features` ended with

```
verification results:: 82 verified, 0 errors
```

**present** (a real run, not stale cache). `N` rose **80 → 82**, exactly the **+2**
predicted for the two new `fn`s (`decode_content`, `encode_content`) — the gate
counts items, not lines. **Gate: PASS (Y).**

## Measurement (§2 step 2b — cold timing vs. baseline)

Crate-level, **cas-own** SMT (ms):

| metric | before | after | ratio |
|---|---:|---:|---:|
| **SMT cpu total (cas-own)** | **1 826** | **1 053** | **1.73×** |

Per-function (SMT `ms` / `rlimit`):

| function | before | after | ms ratio | rlimit ratio |
|---|---:|---:|---:|---:|
| `decode_raw` | 752 / 21 976 289 | **172 / 3 343 522** | **4.37×** | **6.57×** |
| `encode_raw` | 250 / 11 712 418 | **60 / 1 840 881** | **4.17×** | **6.36×** |
| `decode_content` (new) | — | 28 / 318 646 | — | — |
| `encode_content` (new) | — | 12 / 194 355 | — | — |

Net per path (function + its new helper): decode **752 → 200 ms** (3.76×), encode
**250 → 72 ms** (3.47×). The crate-total drop (−773 ms) equals the sum of the
function savings ((752−172) + (250−60) = 770 ms), and is an order of magnitude
beyond the ±5–15 % noise band, so one cold measure is decisive. The big `rlimit`
cuts (~6.5×) are the steadier signal and confirm a genuine proof-size reduction,
not a scheduling artifact. Reproduces the plan's `[measured]` projection
(`decode_raw` 752→~185, `encode_raw` 250→~83) — slightly better here.

## A2c — `lemma_entry_assemble` tail lemma (attempted, REVERTED)

Extracted `decode_raw`'s closing field-assembly (the six `lemma_cat` calls + final
`canonical_bytes` assert) into `proof fn lemma_entry_assemble(...)` taking the five
per-field facts (+ the content/opt facts) as `requires`. Cold result: **83 verified,
0 errors** (the expected +1 for the new lemma), but:

| function | after A2a/A2b | after A2c | Δ |
|---|---:|---:|---:|
| `decode_raw` | 172 / 3 343 522 | 175 / 3 345 712 | flat (noise) |
| `lemma_entry_assemble` (new) | — | 3 / 26 018 | +3 ms |
| crate total (cas-own) | 1 053 | 1 082 | **+29 ms** |

A2c is classified as an **optimization** whose explicit keep-gate is "verify it
actually moves `decode_raw` before keeping" (plan, Wave A). It does **not**:
`decode_raw` is 172→175 ms (within noise) with `rlimit` flat (3.343 M → 3.346 M),
and the crate total *rose* by the new lemma's cost. Per the optimization asymmetry
("an optimization that does not measurably speed verification is worthless even if
harmless — drop it"), **A2c reverted.** The field-assembly stays inline in
`decode_raw`. (The plan anticipated this: "expect a modest further cut" — the cut
did not materialize because the tail is already cheap after A2a shrank the query.)

## Clarity (§2 step 4)

**Cleaner.** Two ~15–55-line inline match blocks become named helpers with a single
round-trip contract each, matching the file's existing small-helper style
(`read_arr32`, `copy_range`, `push_arr32`). `decode_raw`/`encode_raw` read as a
linear field walk with the content branch behind one call. The two `pub fn`
doc-comments stayed attached to their functions (the helpers carry their own).

## Host tests

`cargo test -p cas` — the prolly/store proptest suite stays green (round-trip and
decoder properties exercise both extracted helpers).

## Decision

**KEEP A2a + A2b; DROP A2c.** Optimization asymmetry satisfied for A2a/A2b: both
target functions **and** the cas-own SMT total dropped hard (1.73× crate, ~4× per
fn, ~6.5× rlimit) and clarity improved. A2c failed its decode_raw-moves gate.

> verified **Y** (80 → **82**, +2) · `decode_raw` **752 → 172 ms** (rlimit 22.0 M →
> 3.34 M) · `encode_raw` **250 → 60 ms** (rlimit 11.7 M → 1.84 M) · cas-own crate
> total **1 826 → 1 053 ms** (1.73×) · clarity **cleaner** → **KEEP A2a+A2b** ·
> A2c **neutral on `decode_raw` → DROP**
