# Kani verification findings — part 7 (§4.7 host-side targets)

Continuation of `doc/results/2_kani-findings.md` (§4.1) through
`7_kani-findings-6.md` (§4.6). §4.7 is **tier 2**: the host-buildable userspace
crates (`urt`, `ipc`, `cas`, `dma-pool`). Harnesses live in each crate's
`#[cfg(kani)] mod proofs` and run via `cargo kani -p <crate>` (CI job `kani`,
pinned cargo-kani **0.67.0**). The standing caveat and design notes (DN-1…DN-9)
of the earlier parts apply unchanged.

Per the §4.7 preamble, **Kani is supplementary here** — proptest/fuzz stay
primary. It is applied where exhaustiveness at small bounds buys something a
fuzzer can't promise (overflow freedom, parse totality, allocator/geometry
safety) and **deliberately not** where a fuzzer/proptest already owns the
property or where CBMC is the wrong tool (concurrency; `Vec`-parsing; symbolic
division). The boundary lines below are the substance of this phase.

## What §4.7 verifies

| Crate · harness | Property |
|---|---|
| `urt::slots` `check_slots_alloc_unique` | draining hands out exactly `cap` distinct in-window slots; exhaustion exact — no double/over-allocation |
| `urt::slots` `check_slots_free_reuse` | a freed slot is handed back out |
| `urt::slots` `check_slots_double_free` | `#[should_panic]`: `free`'s double-free `debug_assert` fires |
| `urt::time` `check_time_conversion_total` | `utc_ns_at` is total — no panic/overflow for **all** `(wall_base, cntvct_base, cntfrq, cntvct)` (the naive `Δ·10⁹` overflow at ~5 min, caught only probabilistically by proptest, can't happen) |
| `ipc::header` `check_header_decode_total` | `decode` total over all byte strings; `Ok` **iff** length `== HEADER_SIZE` (short-input + trailing rejection) |
| `ipc::header` `check_header_roundtrip` | `encode`∘`decode` = id both directions — a total bijection |
| `dma-pool` `check_dma_alloc_disjoint` | **for all first sizes**: accept/reject boundary, in-pool, bijection `device_addr == device_base + offset`; + a concrete carve-split for alignment round-up & two-buffer disjointness (see DN-10 / part 10) |
| `dma-pool` `check_dma_free_reuse` | free merges the range back; whole pool reusable |
| `cas::disk` `check_superblock_geometry` | the §4.5 mount chokepoint: `validate_geometry` total (all `checked_add`); `Ok ⇒` the committed region lies within the device — no untrusted field vouches for another |
| `cas::disk` `check_superblock_decode_total` | `decode_checked` total over arbitrary superblock bytes — never panics (blake3 stubbed; see DN-11) |

All ten verify. **No defects found** — the value of this phase is the safety
properties above plus the precise mapping of *where Kani pays vs. where it
doesn't* on host code (below).

## New code: the `ipc` fixed header

§4.7 row 3 ("ipc wire header") had no codec to verify — the `ipc` crate's
header was an unimplemented stub, and the only real wire decoder
(`storage-server/src/wire.rs`) is postcard-based (out of Kani scope). So the
spec §3.7 "fixed, hand-defined header" is now implemented in `ipc/src/header.rs`
as a pure 10-byte little-endian codec (`proto`/`version`/`opcode`/`flags`/
`body_len`) and **verified** as a total bijection. It does no field-value
validation (a server validates `proto`/`opcode` — the dispatch layer's job),
which keeps the codec the clean bijection the harnesses prove.

## Design / solver notes new to §4.7

- **DN-10 — host-tier Kani tractability limits (where Kani does *not* pay).**
  Three §4.7 properties are owned by other tiers because CBMC cannot tractably
  discharge them; each was confirmed empirically (CBMC OOM / non-termination),
  not assumed:
  - **`urt::time` monotonicity** (`c1 ≤ c2 ⇒ utc_ns_at(c1) ≤ utc_ns_at(c2)`)
    forces CBMC to relate two `u128` **divisions**; with a symbolic `cntfrq`
    it never terminated, and even with a concrete frequency + bounded counter
    it did not finish in many minutes. Kani keeps the *overflow* proof
    (`check_time_conversion_total`); **monotonicity stays with the proptest
    `conversion_is_monotone`**. (Plan §1 already assigns the time-page seqlock
    — a *concurrency* property — to Loom/proptest; Kani harnesses neither.)
  - **`dma-pool` symbolic-size allocation** over the `[(usize,usize); 64]`
    free list (`MAX_FREE_RANGES`) generated a SAT instance that **exhausted
    CBMC's memory** (even at `POOL=16`, and even with a symbolic *alignment*
    the `& !(align-1)` mask alone blew up). **Refined in part 10
    (`12_kani-findings-10.md`, rec. #4):** a *single* symbolic-size `alloc` with
    concrete `align == 1` on a *fresh* pool **does** verify (~0.5 s) — it reads
    only the concrete `(0, POOL)` entry, so the symbolic size touches just the
    `len1 > POOL` boundary compare. `check_dma_alloc_disjoint` now proves the
    accept/reject boundary + in-pool + bijection **for all sizes** that way, and
    keeps a concrete second alloc for the alignment round-up + two-buffer
    disjointness. What still OOMs (so stays concrete): a *second* alloc with a
    symbolic size — it re-reads the now-symbolic remainder entry `(len1,
    POOL-len1)` and the round-up `(off+align-1) & !(align-1)` over that symbolic
    offset bit-blasts CaDiCaL — and symbolic *alignment*. "For all sizes"
    two-buffer disjointness stays with the unit tests + proptest.
  - **`cas::tlv` decode** allocates `Vec`s (name, inline content) of symbolic
    length; CBMC's `RawVec`/allocator modeling **OOM'd even at a 12-byte
    input** (18.5k VCCs). The decode-totality and canonical-form
    (decode→re-encode==id) oracles stay owned by the cargo-fuzz target
    `cas/fuzz/fuzz_targets/tlv_entry.rs`.

- **DN-11 — the `cas::hash` stub axiom.** `check_superblock_decode_total` stubs
  `Hash::of` (`-Z stubbing`) with a total deterministic ghost hash: blake3 is
  out of Kani scope (interpreted hashing is intractable for CBMC, §4.7). The
  totality property does **not** need collision-freedom — any total function
  proves "decode never panics" — so the stub is not claimed injective; a
  round-trip harness would instead axiomatize injectivity, stated here for the
  record. This is the only `-Z stubbing` use in the suite.

## CI

The `kani` job runs the host targets alongside `kcore`: `cargo kani -p urt -p
ipc -p dma-pool` and `cargo kani -p cas -Z stubbing` (stubbing is needed only
by the superblock-decode harness; harmless for the rest).

## Process note (not a code finding)

Several harnesses initially ran away for many minutes (the cases in DN-10). The
Bash tool's timeout does **not** hard-kill a detached `cargo kani`'s CBMC/SAT
children on macOS, so the suite is now run with an explicit guard — a
background `sleep N; pkill -9 cbmc/kissat/cadical` per harness — that bounds
every run. CI inherits the per-job budget (≤30 min) and the ≤5-min/harness
discipline; all ten harnesses finish in seconds.

## Findings

None. No defects; the phase delivered the safety harnesses above and the
tier-boundary determinations in DN-10/DN-11.

| ID | Date | Harness | Bounds | Severity | Description | Status |
|----|------|---------|--------|----------|-------------|--------|
| —  | —    | —       | —      | —        | (no defects found) | — |

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Measured on the dev machine (cargo-kani 0.67.0); all well under budget.

| Harness | Crate | Time |
|---------|-------|------|
| `check_slots_alloc_unique` | urt | ~1.5 s |
| `check_slots_free_reuse` | urt | ~1.6 s |
| `check_slots_double_free` | urt | ~1.0 s |
| `check_time_conversion_total` | urt | ~0.5 s |
| `check_header_decode_total` | ipc | ~1.2 s |
| `check_header_roundtrip` | ipc | ~1.6 s |
| `check_dma_alloc_disjoint` | dma-pool | ~a few s |
| `check_dma_free_reuse` | dma-pool | ~a few s |
| `check_superblock_geometry` | cas | ~a few s |
| `check_superblock_decode_total` | cas | ~a few s (stubbed) |
