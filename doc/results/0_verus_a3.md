# A3 — cas payload-ok per-tag split (evaluation)

Task **A3** (rank 11, Wave A) from `doc/plans/0_verus-optimization.md`: split the
monolithic WAL-payload structural validator `s_payload_ok` / `e_payload_ok`
(`cas/src/store.rs`) — one tag-dispatched body covering the Write/Unlink/Rename
arms — into a thin top-level dispatcher plus one helper per tag, so each arm
discharges against a small SMT context (`doc/guidelines/verus.md` §10 —
decomposition is the default fix). One of the pre-measured (`[measured]`) entries.
This file records the per-attempt evaluation under the plan's §2 protocol. Temporary
intermediate report (per `CLAUDE.md`, not citable from code/specs/guidelines).

- **Kind:** optimization + simplification (decompose).
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p cas` before each);
  `cargo verus verify -p cas --no-default-features`. Gate from the plain-text
  `verification results::` line; timing from a separate cold
  `-- --time-expanded --output-json` run, ranking
  `.["times-ms"].smt["smt-run-module-times"][]."function-breakdown"[]`.
- **Baseline note (which number is A3's).** A3 is stacked on A2 (`store.rs` vs
  `prolly.rs` — no code overlap). The on-disk baseline
  (`target/verus-baseline/cas.json`) is **pre-A2**, so its cas-own crate total
  (1826 ms) folds in A2's prolly win; comparing it to the post-A2+A3 total would
  conflate the two passes. Because A3 touches **only the store module** and A2 only
  the prolly module, A3's effect is isolated as the **store-module** delta (and the
  payload-ok obligation), both of which are A2-independent. The deterministic
  `rlimit` field — identical across runs for the same obligations — is the decisive,
  noise-free signal here (§2: a large rlimit drop is strong evidence even when ms
  wobble).

## The change

`s_payload_ok` keeps only the `s_take(pay, 0, 1)` tag-byte guard + tag dispatch; its
three arm bodies move verbatim into non-recursive spec helpers
`s_payload_{write,unlink,rename}_ok(pay, p_tag)`. `e_payload_ok` mirrors this: it
reads the tag byte then dispatches to three exec twins
`e_payload_{write,unlink,rename}_ok(pay, p_tag)`, each
`requires p_tag <= pay@.len()` (discharged at the call site from the tag-byte
`e_take`'s `Some`) and `ensures r == s_payload_<arm>_ok(pay@, p_tag as int)`. No
logic change, no verifier attributes (none existed). Matches the file's existing
`rec_ok`/`laid_out` split and A2's `decode_content`/`encode_content` helpers.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p cas && cargo verus verify -p cas --no-default-features` ended with

```
verification results:: 85 verified, 0 errors
```

**present** (a real run, not stale cache). `N` rose **82 → 85**, **+3** — the three
new exec twins (`e_payload_write_ok`, `e_payload_unlink_ok`, `e_payload_rename_ok`).
The three new **non-recursive** `spec fn`s carry no proof obligation, so they do not
increment `N` (the gate counts items that verify, and a bare non-recursive spec fn
has nothing to discharge). The plan §2 anticipated exactly this (predicted "88, or
85 if non-recursive spec fns aren't counted"); the delta equals precisely the new
exec items and nothing else shifted. **Gate: PASS (Y).**

## Measurement (§2 step 2b — cold timing vs. baseline)

The headline obligation — `e_payload_ok` was the single hottest store obligation —
splits into four, the heaviest at a 6.6×-smaller rlimit:

| obligation | SMT ms | rlimit |
|---|---:|---:|
| `e_payload_ok` (monolith, **before**) | 82 | **962 665** |
| `e_payload_ok` (dispatcher, after) | 2 | 16 122 |
| `e_payload_write_ok` (after) | 22 | **145 084** |
| `e_payload_unlink_ok` (after) | 6 | 51 725 |
| `e_payload_rename_ok` (after) | 12 | 88 433 |
| **payload path total** | **82 → 42** | **peak 962 665 → 145 084 (6.6×)** |

Store module (A3's only surface; A2-independent):

| metric | before | after | ratio |
|---|---:|---:|---:|
| store-module SMT time | 260 ms | 229 ms | 1.14× |
| store-module rlimit | 2 333 458 | 1 718 396 | 1.36× |

The peak-arm rlimit of **145 084** reproduces the plan's `[measured]` projection
(`962 665 → 145 084`) exactly, and the summed payload SMT (~82 → ~42 ms) matches its
"~75 → ~41 ms". `recover_records` (105 ms) and `decode_frame` (27 ms) are unchanged —
nothing else regressed. The rlimit cuts are deterministic (run-independent), so the
proof-size reduction is decisive regardless of ms noise.

## Clarity (§2 step 4)

**Cleaner.** One ~85-line spec match and one ~110-line exec match (each a deep,
right-drifting `match` ladder) become a thin two-line tag dispatcher plus three
named, self-documenting per-arm helpers with a single round-trip contract each —
the file's established split idiom (`rec_ok`/`laid_out`, A2's `decode_content`/
`encode_content`). The per-arm field layouts now live in the helper doc-comments
rather than a single overview block.

## Host tests

`cargo test -p cas` — green: 133 lib (the payload round-trip / mount-recovery
proptests exercise the dispatch through `wal_struct_ok`), 9 integration, 10
fuzz-regression; 0 failed.

## Decision

**KEEP.** Optimization asymmetry satisfied: the targeted obligation's peak rlimit
fell 6.6× and the store-module time **and** rlimit both dropped (deterministic
evidence), with no other obligation regressing; clarity improved. The
simplification axis is also a clear win (named per-tag helpers, no crate regression).

> verified **Y** (82 → **85**, +3 exec twins) · `e_payload_ok` **82 ms / rlimit
> 962 665 → 4 obligations, peak 145 084** (6.6×) · store module **260 → 229 ms /
> rlimit 2.33 M → 1.72 M** · clarity **cleaner** → **KEEP**
