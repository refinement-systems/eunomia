# 11 — loader `parse()` + le readers under Verus (Task 11)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 11
(ELF `parse()` + little-endian readers — total bounded decoder over arbitrary
bytes; Tier 1, "yes, med risk", depends on the Task 5 loader gate). Outcome:
**verified, shipped — full contract, no fallback used, first attempt.**
`cargo verus verify -p loader --no-default-features` now reads **`18 verified,
0 errors`** (was 9). No reverts; no new trusted seam (tally stays 14); the gated
dep `ipc` (71) is unchanged and re-verifies byte-identical transitively.

## What was attempted

Bring the loader's ELF decoder under Verus so the rev2§5.3 refuse-not-crash
guarantee — `parse()` never panics and never reads out of bounds for *any*
`&[u8]` — holds deductively for all inputs, not just the fuzz corpus. ELF images
are data in the versioned store, so any writer feeds bytes to this parser.
Previously `parse` and the `u16le/u32le/u64le` readers were plain Rust outside
`verus!{}`, guarded only by unit tests + a committed fuzz corpus.

The existing single `verus!{}` island in `loader/src/elf.rs` (Task 5's
page-geometry cluster) was extended to also enclose `MAX_SEGMENTS`, `Image`, the
ported little-endian readers, and `parse`. Three pieces:

1. **Le readers, ported wholesale from cas** (`cas/src/prolly.rs:648–830`):
   `open spec fn u{16,32,64}_le(x) -> Seq<u8>` (the little-endian byte split),
   `read_u{16,32,64}_le(buf, off) -> v` with `requires off+N <= buf@.len()` /
   `ensures buf@.subrange(off, off+N) == u*_le(v)`, and the
   `lemma_u{16,32,64}_le_bytes(...) by (bit_vector)` byte↔value identities. The
   mask/shift body + `proof { lemma_… }` + an extensional `=~=` assert is a
   turnkey unit; it verified unchanged but for the renames.

2. **`parse` under a total bounded-decoder contract**:
   ```rust
   pub fn parse(bytes: &[u8]) -> (r: Result<Image<'_>, ElfError>)
       ensures r matches Ok(img) ==> well_formed_image(img),
   ```
   with `seg_ok` and `well_formed_image` as the spec oracle:
   ```rust
   pub open spec fn seg_ok(s: Segment, len: nat) -> bool {
       &&& s.offset + s.filesz <= len
       &&& s.vaddr + s.memsz + PAGE_MASK <= u64::MAX     // == page_layout().is_ok()
   }
   pub open spec fn well_formed_image(img: Image) -> bool {
       &&& 1 <= img.nsegments <= MAX_SEGMENTS
       &&& forall|j: int| 0 <= j < img.nsegments
               ==> seg_ok(#[trigger] img.segments@[j], img.bytes@.len())
   }
   ```
   Totality is automatic (a verified exec fn provably returns with no panic/OOB);
   the `Ok`-clause captures all four plan obligations — file extent in bounds,
   `page_layout` composition, `nsegments in 1..=MAX_SEGMENTS`, segment in-bounds.

3. The `parse`/`page_layout_*` unit tests, the `layout_props` proptest, and the
   `elf_parse` fuzz target + 83-input corpus + Miri replay
   (`tests/fuzz_{corpus,regressions}.rs`) are **kept** as the companion oracle
   tier (Phase 4 already existed; it now exercises the verified code).

## Result

Full gate, cold (`cargo clean && cargo verus verify -p loader
--no-default-features`; results line present == real run; prover
`0.2026.06.07.cd03505` / toolchain `1.95.0`):

| crate | result |
|---|---|
| ipc (transitive dep) | 71 verified, 0 errors |
| **loader** `--no-default-features` | **18 verified, 0 errors** (was 9) |

The count rose 9 → 18 (+9): the three `read_u*_le` exec readers, the three
`lemma_u*_le_bytes`, `parse`, the `MAX_SEGMENTS` const, and `Image`'s derive
obligation. The `u*_le`, `seg_ok`, and `well_formed_image` `open spec fn`s are
transparent and carry no standalone obligation (0 each).

Per-function `rlimit` (cold, `--time-expanded --output-json`), pre vs post:

| obligation | rlimit (pre → post) |
|---|---|
| `parse` (new) | — → 449,343 |
| `lemma_align_down` (control, unchanged source) | 297,434 → 391,892 |
| `read_u64_le` (new) | — → 203,693 |
| `read_u32_le` (new) | — → 84,646 |
| `lemma_u64_le_bytes` (new) | — → 73,535 |
| `lemma_u32_le_bytes` (new) | — → 71,171 |
| `lemma_u16_le_bytes` (new) | — → 69,979 |
| `page_layout` (control, unchanged source) | 67,254 → 52,278 |
| `read_u16_le` (new) | — → 34,869 |
| `lemma_pages_exact` (control, unchanged source) | 20,055 → 21,913 |
| crate total (loader own) | 385,487 → 1,455,877 |

The rise is the cost of the *new* proofs (parse 449k, the three readers 323k
combined, the three le lemmas 215k combined); all modest absolutely (vs
freelist's 110M+). The unchanged page-geometry items only drift by additive
module-context overhead — `page_layout` fell 67k → 52k, `lemma_align_down` rose
297k → 392k, `lemma_pages_exact` ~flat — the §10 / Task-7-finding-5 signal that
the drift is SMT-context, not a logic regression of the controls. This is a
net-new verified-surface change (it proves strictly more), not a perf change, so
the total rising is correct, not a regression.

Behavioural + UB tiers all green against the refactored code:
`cargo test -p loader` (12 unit + 3 fuzz_corpus + 3 fuzz_regressions + 2
layout_props), `cargo +nightly miri test -p loader --test fuzz_regressions
--test fuzz_corpus` (202 corpus inputs, no UB), and the aarch64 kernel
cross-build (loader links, incl. `spawn` calling `parse`). `cargo fmt --check`
clean (rustfmt does not descend into `verus!{}`).

## Findings that mattered

1. **The full contract verified on the first attempt — the plan's sanctioned
   totality-only fallback was not needed.** The per-segment `forall` loop
   invariant, the array-write framing, and the `page_layout` composition all
   discharged without a wall.

2. **`[T; N]` index-assignment `ar[i] = v` is supported natively under Verus**
   (`requires i < N`, update framing: the written index takes the new value,
   others are unchanged), confirmed against `vendor/.../tests/arrays.rs`. So the
   fixed `[Segment; MAX_SEGMENTS]` array stayed — no `Vec`, no `alloc` pulled
   into loader's `--no-default-features` verify config, no `Image` API change for
   `spawn`/tests. The prefix invariant `forall j: 0<=j<n ==> seg_ok(segments@[j],
   len)` was re-established after the write with a `let ghost prev = segments@`
   snapshot + `assert forall|j| 0<=j<prev_n+1 implies seg_ok(segments@[j], …) by
   { if j < prev_n { ==prev[j] } else { ==seg } }`.

3. **`seg_ok`'s overflow clause IS the spec-level negation of `page_layout`'s
   `Err` condition**, so Task 5 composes for free: bind `let pl =
   seg.page_layout()` (its `ensures` `pl is Err <==> vaddr+memsz+PAGE_MASK >
   u64::MAX` enters scope), refuse on `pl.is_err()`, and the accept arm yields
   `vaddr+memsz+PAGE_MASK <= u64::MAX` — no re-derivation of page geometry.

4. **Bounding the whole phentsize-strided entry up front discharges every
   in-record read and offset-add at once.** After `ph_end == ph + phentsize <=
   len` (from the `checked_add` Some arm) and `phentsize >= 56` (loop invariant),
   one `assert(ph + 56 <= bytes@.len())` gives `ph + 48 <= len`, which is exactly
   what every field read's `off+N <= len` precondition and every `ph + k` usize
   overflow check needs (widest read is `ph+40 .. ph+48`).

5. **Magic bytes checked individually, not via slice `!=`.** `&bytes[0..4] !=
   b"\x7FELF"` has no Verus spec (`[u8]: PartialEq` is unspecced); four `bytes[k]
   != …` comparisons (each in bounds from `len >= 64`) are the verified idiom.

6. **`continue` was eliminated, not relied upon.** The two `continue`s (skip
   non-PT_LOAD, skip `memsz == 0`) became nested `if` bodies — the cas
   `decode_node` loop shape, avoiding any dependence on Verus `continue` support.
   The `.checked_mul().and_then().ok_or()?` header chain likewise became explicit
   nested `match` (Task 5 finding 4).

## Reverted vs kept

Nothing reverted — the proof succeeded once. Kept: the ported `u*_le` spec fns,
`read_u*_le` readers, and `lemma_u*_le_bytes` lemmas; the `seg_ok` /
`well_formed_image` predicates; the rewritten `parse` under `verus!{}` (byte-wise
magic check, nested-`match` arithmetic, `continue`→nested-`if`, the `decreases`
loop with the per-segment `forall` invariant, the array-write framing proof);
`MAX_SEGMENTS` + `Image` relocated inside the block. All existing tests are
unchanged and kept as the companion oracle tier — the `elf_parse` fuzz target +
corpus already covered the Task-11 edge cases (truncation / bad-magic /
too-many-segments / field & page-rounding overflow), so no new corpus seed was
required.

## Proposed guideline additions (`doc/guidelines/verus.md`)

1. **Port the cas le-reader trio wholesale for any hand-rolled little-endian
   byte decoder; do not reach for `from_le_bytes` (unspecced).** `read_u*_le`
   (`requires off+N <= buf@.len()`, `ensures buf@.subrange(off,off+N) ==
   u*_le(v)`) + the `lemma_u*_le_bytes` `by (bit_vector)` identity + the `=~=`
   extensional assert is a turnkey unit that ports across crates unchanged but
   for names (cas → ipc `le_bytes` → loader now share it).

2. **`[T; N]` index-assignment `ar[i] = v` is verified-supported; prefer it to a
   `Vec` when a decoder fills a fixed-capacity array**, to keep `alloc` out of a
   `no_std --no-default-features` verify config. Re-establish a `forall`-prefix
   loop invariant after the write with a `let ghost prev = arr@` snapshot + an
   `assert forall|j| … by { if j < n { == prev[j] } else { == v } }` over the
   update framing.

3. **Compose a downstream decoder on an upstream validator by making the
   accept-predicate the spec negation of the validator's `Err` `ensures`.** Bind
   the call (`let r = x.validate()`) so the `ensures` is in scope; the refusal
   arm then yields the accept-predicate for free, with no re-derivation. (Here
   `seg_ok`'s overflow clause ⟷ `page_layout`'s `Err` boundary.)

4. **Bound a strided record's whole stride up front (one `assert(base + stride <=
   len)`)** to discharge all in-record field-read `off+N <= len` preconditions
   and all `base + k` offset-add overflow checks together, rather than per field.

5. **Check magic/tag bytes individually, not via slice `==`/`!=`** — Verus has no
   `[u8]: PartialEq` spec. (Extends the Task-5 / §6 "no slice-level shortcuts in
   spec positions" note from arithmetic to equality.)

## Trusted base

**Tally unchanged at 14.** `parse` and the readers add no `external_body`/
`assume_specification` (`rg "external_body|assume_specification" loader/src`
returns nothing): pure `u64`/`usize` checked arithmetic + bit/shift reassembly
citing only `vstd`'s already-trusted slice/bit library specs, composing on the
Task-5 `page_layout` contract. loader was already gated (Task 5), so there is no
new crate onboarding — the CI line, `verus-baseline.sh` entry, and CLAUDE.md gate
list already include it (only their descriptive comments were broadened).
Baseline row updated: `-p loader --no-default-features` → **18 verified, 0
errors**, with the parse/le-reader contract recorded alongside the page-geometry
one and the companion fuzz/Miri/proptest tier named.
