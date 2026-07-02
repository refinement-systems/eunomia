# Findings 26 — trusted-base ledger rewrite (C3, review findings 13, 14, 16, 9-doc)

Task **C3** of `doc/plans/3_plan-std-correction.md`, one coherent pass over
`doc/guidelines/verus_trusted-base.md` (the ledger — the single source of truth for
`CLAUDE.md`'s "the trusted base is exactly …" claim). It acts on the review
(`doc/results/22_std-port-review.md`) findings 13 (plan-provenance in a guideline),
14 (history narration), 16 (internal drift / stale citations), and the doc half of 9
(the DRBG carve-out was under-sanctioned). All four sub-tasks land as one reviewable
change; the ledger's structure is preserved (five sections, the **14-seam tally**, the
`rev2§` citations, the `## Baselines` table).

**Headline:** the ledger is now a present-tense enumeration of the current trusted base.
Removed every plan-phase / `findings #N` / `Task N` reference (~30 sites, incl. the
capital-`Std-port` headers the plan's literal gate would have missed); collapsed every
count-delta arrow and "retired/replaces/widened/rises" history verb to current state;
re-derived every drifted seam-table citation against the live tree (~15 of them,
including two the plan flagged and a dozen it did not); refreshed the two stale
`eunomia-sys 7` notes to **16** and the whole **urt row 29 → 30** (the C1.3 verified
`u64_to_le` the ledger had not caught up to); and tightened the entropy routing note so
the DRBG is explicitly sanctioned under `verus.md` §11 categories (2) and (3) with every
host test named. Documentation-only: no code, no `verus!{}`, no wire/ABI change.

## Decision 1 — de-plan the ledger (C3.1), keeping only spec/guideline citations

A guideline may reference only spec and other guidelines. Every `std-port Phase N` /
`std-port N.N` / `findings #N` / `findings 16-1` / `Task N` / `Task-N` token was removed:
routing-note titles keep the **surface** ("Entropy-seed routing note", "Futex-backend
routing note", …), the "Std-port Phase 2.x backs …" paragraph leads become plain
descriptions ("The `GlobalAlloc` arm is …"), and in-prose `Task 8`/`Task-5`/`findings
16-1` citations are dropped or re-anchored to the construct.

- **Corrected gate (recorded for C6/CI).** The plan's literal C3.1 gate is
  case-sensitive and would false-green on the capital-`Std-port Phase 2.2/2.3/2.4/3.1`
  headers. The authoritative gate is case-insensitive **and** catches the bare
  `phase N` form (one lived in the freelist Baseline row, "the phase 5.1/5.2/6.2 trigger
  reductions"):
  `grep -niE 'std-port|findings #|findings [0-9]+-[0-9]|Task [0-9]|Task-[0-9]|doc/(plans|results)|phase [0-9]' doc/guidelines/verus_trusted-base.md`
  → **0 hits**.
- **Kept, deliberately:** `C-M9` (spec vocabulary — `spec_rev2.md:135,435` use it as the
  console-driver status marker), every `rev2§` citation, "the Rust std port" as a noun
  (space, not the hyphenated token), and the footer's bare words "findings"/"plan" (they
  point at `doc/guidelines/verus.md`, not a plan/result path). False-positive `phase`
  uses stay ("per-phase frame lemmas", "one teardown phase's edit shape" — algorithm
  steps of `destroy_tcb`).

## Decision 2 — current state, not history (C3.2), with a reconciliation rule

Count-delta arrows (`404 → 406`, `75 → 77`, `25 → 29`, `29 → 30`, `7 → 16`, `272 → 288`
bytes, rlimit `169163 → 177414`) and history verbs (`retired`, `replaces`, `widened`,
"rises/raises to", "moves to Verus", temporal "is now …") are gone.

The subtle case: three routing-note deltas end *below* the crate's current Baseline total
because later changes bumped it further, so restating the delta endpoint as an absolute
would be a new falsehood. Rule applied — describe the note's contribution and **defer the
absolute to the Baseline row** (the single place the count lives):

| Note | Old (delta) | Current crate total | Rewrite |
|---|---|---|---|
| kcore FireSafe | `404 → 406` | **408** | "two verified items … Baseline row" |
| cas RecoverReconstructs | `75 → 77` | **79** | "the corollary + teeth … counted in cas's Baseline" |
| urt KeyTable | `25 → 29` | **30** | "counted on the urt row (Baseline row)" |
| loader KIND_SEED | `29 → 30` | 30 | collapsed to `30` (endpoint is current) |
| eunomia-sys path resolver | `7 → 16` | 16 | collapsed to `16` |

The kcore/urt/loader/eunomia-sys Baseline rows themselves stopped narrating their own
history (dropped "rise to 408 (from the 407 … from 404 …)" chains and the "since a
previous phase" clauses); each states only its current count and what it covers. One
`CLAUDE.md`-sanctioned path-not-taken rationale survives (why TLS keys are a verified
table rather than a raw counter), rephrased so it no longer names what it replaced.

## Decision 3 — fix internal drift (C3.3)

**Stale counts.** The two `eunomia-sys 7` prose notes → **16**; the ThreadStartAs note's
`kcore 407, eunomia-sys 7 — count-neutral` → present-tense "both counts live in the
Baseline rows". The **urt row 29 → 30**: the ledger predated C1.3, so it lacked the
verified `random::u64_to_le` (proven `r@ == le_bytes::u64_le(w)`, rlimit 13775) and the
new no_std `le-bytes` path-dep; both are now recorded, and the entropy/heap/time notes
that cited "urt 25/0" or "stays 25" are corrected.

**A drift the plan did not flag.** The `GlobalAlloc` note claimed a cold `-p eunomia-sys`
session "re-verifies urt (25) + freelist (30) transitively" — flatly false: `urt` is a
**target-gated** dep of `eunomia-sys` (`eunomia-sys/Cargo.toml:33`, whose own comment
says "the host `cargo verus verify` graph stays byte-identical — no urt obligations enter
this crate's session"), confirmed by the cold sweep (the `-p eunomia-sys` run emits only
loader 30 + eunomia-sys 16). Rewritten to state urt/freelist verify under their own
gates, not eunomia-sys's session.

**Re-derived citations** (verified symbol-by-symbol against the live tree — an automated
`awk 'NR==L'` + `grep -F` spot-check confirmed all 20 resolve):

| Construct | Ledger (stale) | Current (live) |
|---|---|---|
| `is_boundary` | `prolly.rs:1457` | `cas/src/prolly.rs:1320` |
| `wal_struct_ok_has_teeth` | `store.rs:4562` | `cas/src/store.rs:4586` |
| `checked_next_multiple_of` | `untyped.rs:258` | `kcore/src/untyped.rs:274` |
| `CapSlot::empty` | `cspace.rs:1595` | `kcore/src/cspace.rs:1849` (`assume_specification`; `const fn empty` at `:177`) |
| `debug_check_free` | `slots.rs:340` | `urt/src/slots.rs:362` |
| `ExTcb` / `ExNotifObj` / `ExTimerObj` | `:246/:250/:254` | `kcore/src/untyped.rs:260/265/270` |
| `fixed_object_bytes` | `:273` | `kcore/src/untyped.rs:287` |
| `CSpaceObj`/`Channel`/`AspaceObj::bytes_for` | `:234/:235/:236` | `kcore/src/untyped.rs:237/242/247` |
| `object_size_positive` / `bytes_for_positive` | `:820` / `:804` | `kcore/src/untyped.rs:829` / `:813` |
| cspace `Ex*` transparent block | `cspace.rs:268-324` | `kcore/src/cspace.rs:278-331` |
| untyped opaque `Ex*` block | `untyped.rs:246-254` | `kcore/src/untyped.rs:257-270` |

Unchanged/correct, kept: `checksum_ok` `cas/src/disk.rs:342`, `wal_checksum_ok`
`cas/src/store.rs:1111`, `saturating_mul` `kcore/src/aspace.rs:76`. The **14-seam tally
holds** — a live census (`rg "external_body|assume_specification"`) is exactly 8
`external_body` + 6 `assume_specification`.

**Baseline counts vs. a live cold verify** (`cargo clean -p <c> && cargo verus verify -p
<c> <flags>`, pinned Verus `0.2026.06.07.cd03505`). Result lines:

```
kcore            408 verified, 0 errors
cas (--no-default-features)                79 verified, 0 errors
ipc               71 verified, 0 errors
freelist          30 verified, 0 errors
dma-pool           0 verified, 0 errors
urt               30 verified, 0 errors   (was 29 — the C1.3 delta)
virtio-blk         3 verified, 0 errors
storage-server (--no-default-features --lib)   19 verified, 0 errors
loader (--no-default-features)             30 verified, 0 errors
le-bytes           6 verified, 0 errors
eunomia-sys       16 verified, 0 errors
```

Every Baseline cell matches; **urt was the only crate whose count had drifted** from the
ledger (29 → 30), exactly as the plan anticipated. The tally line stays **14**.

## Decision 4 — make the DRBG sanction explicit (C3.4)

The entropy routing note now names both `verus.md` §11 categories the DRBG folds under
(`doc/guidelines/verus.md:1896-1902`): **(2) out-of-scope total function** for the
xoshiro256\*\* / `expand_seed`/`fresh_seed` shuffle (trusted for *totality + determinism
only* — quality is off the proof surface per rev2§5.1, and "`fresh_seed` never returns
the raw seed" is a *sampled* fixed-point property, not a theorem over the 2²⁵⁶ state
space), and **(3) runtime-only guard** for the `no_seed_abort()` loud abort. Every host
test is named — the ten `urt/src/random.rs` `mod tests` cases
(`deterministic_stream_from_a_fixed_seed`, `distinct_seeds_diverge`,
`sub_seeds_are_all_distinct`, `expand_widens_a_scalar_without_collision`,
`never_returns_the_raw_seed`, `all_zero_seed_does_not_degenerate`,
`fill_serves_any_length`, `fill_locked_happy_path_fills`,
`fill_locked_aborts_when_unseeded`, `global_seed_then_fill`). The C1.3 verified `fill`
serialization is recorded: mechanized surface is the seed *decode* (loader row) **and**
`Drbg::fill`'s little-endian word serialization (`u64_to_le`, urt row 30); the xoshiro
transition stays trust-routed under (2)+(3). After this note, no reader can classify the
DRBG as an unsanctioned carve-out.

## Rejected alternatives

- **Symbol-only citations for the whole seam tables** (the plan's escape hatch for chronic
  drifters). Applied *judgment*: kept the `file:line` format for structural consistency
  with the rest of the tables, since the live line numbers were cleanly re-derivable this
  pass; `CapSlot::empty` gained a belt-and-braces symbol note (`assume_specification` +
  the `const fn` at `:177`) because it had drifted furthest from its cited line.
- **Restating each note's delta endpoint as its new absolute** (e.g. FireSafe "406").
  Rejected — three endpoints are now below the crate total, so this would inject fresh
  falsehoods. Deferred all absolutes to the Baseline rows instead.

## Gates (all green)

- C3.1: the case-insensitive provenance grep above → **0 hits**.
- C3.2: `grep -nE '[0-9]+ → [0-9]+'` → **0 numeric-arrow hits**; a history-verb grep
  (`retired|replaces|widened|rises|raises|now …`) leaves only semantic uses (a key that
  "is now live" inside `create`; "previously-empty slot" as a slot state).
- C3.3: the 20-citation `awk`/`grep -F` spot-check all-OK; the cold-verify counts above.
- C3.4: the entropy note names §11 (2) and (3) and all ten host tests; consistent with the
  urt Baseline row (30).
- Discipline: the C3.1 grep over the ledger is also the check that this effort left no
  `doc/plans`/`doc/results` reference in the guideline.

## Left trusted / follow-ups

- The 14 seams are unchanged — C3 is documentation-only; it re-cites and re-describes
  them, it does not alter the trusted base.
- **C6 (gate hardening)** should pin these counts in `tools/verus/verus-manifest.tsv` and
  add the corrected case-insensitive provenance grep as a guideline lint; the ledger's
  `## Baselines` cells are the machine-readable twin C6 asserts against.
