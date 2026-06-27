# Findings — Phase 1.2: the verified startup-block decoder

Task 1.2 of `doc/plans/1_plan-rust-std-port.md` — **the one real upfront proof** of
the std port. Lifts `loader::startup::decode` (the EUS1 startup block read in `_start`
before the heap exists, rev2§5.1) from total-by-construction plain Rust into a
`verus!{}` total ∀-bytes contract, mirroring `elf::parse`'s `well_formed_image`. No
new trusted seam — verified surface on the existing `loader` Baseline row; the tally
stays **14**.

## What shipped

- `loader/src/startup.rs` restructured around a single `verus!{}` block:
  - **Moved into `verus!{}`** (so the proofs can name them): the arena-cap consts
    `MAX_GRANTS`/`MAX_ARGV`/`MAX_ENV` and grant-kind tags
    `KIND_CAP_SLOT`/`KIND_STORAGE_HANDLE`/`KIND_REGION`; the `GrantKind`/`Grant`/
    `Startup` types; the spec fns `subseq_of` + `well_formed_startup`; the
    bounds-checked cursor helpers `take_u8`/`take_u16`/`take_u32`/`take_u64`/
    `take_bytes`; and `decode`.
  - **Verified contract:** `decode(buf) -> Option<Startup>` with
    `ensures res matches Some(s) ==> well_formed_startup(s, buf@)`. Totality
    (never panics / reads OOB, rev2§2.7) is the implicit guarantee of successful
    verification; `well_formed_startup` is the explicit postcondition — the three
    counts within their arenas (`ngrants <= MAX_GRANTS`, `nargv <= MAX_ARGV`,
    `nenv <= MAX_ENV`) and every borrowed argv/env byte-string a subrange of `buf`
    (`subseq_of`, the `elf::seg_ok` file-extent twin).
  - **Stayed external plain Rust** (unchanged behavior, callers/tests/fuzz
    untouched): the encoder `encode`/`Writer`, `EncodeError`, the `Startup` builder
    API (`new`/`push_*`/`grant`), the prefix-comparing `PartialEq`/`Eq`/`Default`,
    a hand-written `Clone`, the `MAGIC`/`NAME_*` consts, and `#[cfg(test)] mod
    tests`.
- `decode`'s public signature is unchanged (`pub fn decode(buf: &[u8]) ->
  Option<Startup<'_>>`), so every caller (`storaged`, `console`, `init`, `shell`,
  …), the `startup` fuzz target, `tests/fuzz_corpus.rs`, `tests/fuzz_regressions.rs`,
  and the in-file proptests are untouched.
- `doc/guidelines/verus_trusted-base.md`: loader Baseline row bumped **12 → 29
  verified** with the `startup::decode` description; eunomia-sys row's transitive
  "(loader 12, …)" → "(loader 29, …)". No seam added; tally stays 14.

## Proof shape (mirrors `elf::parse`)

- The cursor helpers are the verified replacement for the hand-rolled `Reader`:
  each is total (returns `None` on `checked_add` overflow or `end > buf.len()`),
  advances the cursor by exactly the field width, and keeps it `<= buf.len()`.
  Fixed-width little-endian fields read through the shared
  `le_bytes::read_u{16,32,64}_le` readers (their `requires off+N <= len` discharged
  by the helper's bound check); `take_bytes` borrows the byte-string via
  `vstd::slice::slice_subrange`, whose `ensures out@ == buf@.subrange(pos, end)` is
  what proves `subseq_of`.
- `decode` runs three `decreases` loops (grants, argv, env) over a cursor `pos`,
  each maintaining `pos <= buf@.len()`. The argv/env loops carry
  `forall|j| 0<=j<count ==> subseq_of(argv@[j]@, buf@)` and grow it with the
  `parse` ghost-capture append idiom: `let ghost prev = argv@; argv[k] = sl;` then
  `assert forall|j| … implies subseq_of(#[trigger] argv@[j]@, buf@) by { if j <
  prev_k { argv@[j] == prev[j] } else { argv@[j] == sl } }`, with
  `subseq_of(sl@, buf@)` established before the write (witnesses `p1`,`p2` from
  `take_bytes`). The env loop keeps the finished-argv `forall` in its invariant
  (it never touches `argv`), so both quantifiers survive to the return.

## Decisions + rejected alternatives

- **Provenance as an existential `subseq_of` (chosen)** vs threading ghost
  offset-pairs through the output. `Startup` carries the borrowed slices, not
  offsets, so the postcondition is necessarily existential
  (`exists a,b: 0<=a<=b<=buf.len() && sub == buf.subrange(a,b)`); the witnesses are
  supplied at each `take_bytes` site, so the `exists`-introduction is local and
  cheap. This is the content/provenance analog of `seg_ok`'s `offset + filesz <=
  len`. Note Verus models the relationship by *content* (`Seq<u8>` views), not
  pointers — memory-safety of the borrowed slices is already Rust's lifetime
  system (`Startup<'a>` over `buf: &'a [u8]`); the literal pointer-range
  containment stays covered by the `startup` fuzz target / `decode_is_total`
  proptest as the companion oracle.
- **Inline magic bytes in `decode`** (`buf[0] != 0x45 …`, matching `elf::parse`)
  vs `MAGIC[i]`. Keeps `MAGIC` a single encode-side const and sidesteps indexing a
  const array in `verus!{}` exec; the round-trip oracle tests (`golden_layout`,
  `round_trip_oracle_has_teeth`) couple the two byte sources.
- **Verified `take_*` helpers** vs inlining checked reads in `decode`. The helpers
  localize the bound checks and the `le_bytes`/`slice_subrange` `requires`
  discharge, keeping `decode`'s body close to the original control flow and the
  proof light (each helper is a small obligation; see perf below).
- **Types in `verus!{}` + external impls outside, no `#[verifier::external]`.**
  Confirmed: a `verus!{}`-declared `Startup` carries plain-Rust trait/inherent
  impls (`PartialEq`/`Eq`/`Default`/`Clone`, `new`/`push_*`/`grant`) and the
  `encode`/`Writer` plain Rust *after* the block, and the crate verifies clean —
  unannotated outside-block items are auto-external (the repo uses zero
  `#[verifier::external]`). The escape hatch was prepared as a contingency and not
  needed.
- **`Clone` hand-written outside `verus!{}`** vs derived inside vs deriving
  `Copy`. The derived non-`Copy` `Clone` inside `verus!{}` produced a warning
  ("Verus does not (yet) support autoderive Clone impl when the clone is not a
  copy"); deriving `Copy` would silently make the few-hundred-byte struct
  copyable. The hand-written field-wise `Clone` (all fields are `Copy`) keeps the
  original `Clone`-not-`Copy` intent and leaves the tree warning-clean.
- **Empty arena filler via `slice_subrange(buf, 0, 0)`** vs the `&[]` literal.
  Inside `verus!{}` the empty subrange of `buf` ties the filler to `buf`'s lifetime
  and avoids relying on an empty-slice literal in exec proof code; the filler's
  view is irrelevant (`well_formed_startup` only constrains `j < n*`).

## Problems hit

- **Derived-`Clone` warning** (the only snag). The first cold verify was clean at
  **30 verified, 0 errors** but emitted the autoderive-`Clone` warning on
  `#[derive(Debug, Clone)] struct Startup`. Moving `Clone` to a hand-written impl
  outside the block removed the warning; the count settled at **29** (the derived
  `Clone` obligation is gone). The provenance proof itself went through on the
  first attempt — the `argv@[j]@` array-of-references view, the `subseq_of`
  existential, and the ghost-capture append all behaved as the `elf::parse`
  precedent predicted.

## Verification record

All from a clean checkout of the change, verus `0.2026.06.07.cd03505`,
Toolchain `1.95.0`:

- **Gate (cold):** `cargo clean -p loader && cargo verus verify -p loader
  --no-default-features` → `verification results:: 29 verified, 0 errors`
  (transitively re-verified `le-bytes 6`, `ipc 71`). Up from 12.
- **Transitive:** `cargo clean -p eunomia-sys && cargo verus verify -p
  eunomia-sys` → `7 verified, 0 errors` (loader now contributes 29).
- **Host oracle tests:** `cargo test -p loader` → 12 lib tests
  (`golden_layout`, `rejects_malformed`, `round_trip_oracle_has_teeth`,
  `round_trips`, `decode_is_total`, …), 3 `fuzz_regressions`, 2 `layout_props` —
  all pass.
- **Miri UB replay:** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test
  -p loader --test fuzz_regressions --test fuzz_corpus` → all green (the 99-seed
  `startup` corpus + the `startup1_oversized_counts_refused` regression, no UB).
- **Live fuzz:** `cargo +nightly fuzz run startup -- -max_total_time=60` →
  42,585,319 runs in 61 s, no crash. The corpus entries the run minimized in were
  **not** committed (corpus growth is Phase 6.2; this PR keeps the 99 committed
  seeds unchanged).
- **Formatting:** `cargo fmt --check` and `scripts/verusfmt.sh --check` both clean.
  `startup.rs` did **not** need the verusfmt skip list (single `verus!{}` block, no
  `x[..n]` index inside it).
- **Proof perf** (`scripts/verus-baseline.sh loader`, cold, `rlimit`): existing
  `elf` obligations are **byte-identical** — `parse` 469596, `lemma_align_down`
  359382, `page_layout` 52278, `lemma_pages_exact` 21895 — i.e. **zero regression**
  on prior proofs. The change is purely additive: `decode` 169163, `take_u64`
  14639, `take_bytes` 11625, `take_u32` 10856, `take_u16` 10022, `take_u8` 9364,
  the consts/spec-fns ~2 each (≈ 226k `rlimit` added in total).

## Surface left trusted / external (and why)

- The startup **encoder** (`encode`/`Writer`) stays external plain Rust — Phase 1.2
  scopes only the decoder (the untrusted-input boundary); the encoder is the
  producer side, total-by-construction (clean `Err`, no panic/truncation), exactly
  the ledger's standing posture. No need or intent to verify it here.
- The `Startup` builder API and trait impls are plain Rust — exec/host helpers with
  no proof obligation; they verified-clean as auto-external items beside the block.
- Pointer-level "the returned slice's address lies inside `buf`" is Rust's lifetime
  guarantee, not a Verus property (Verus reasons over `Seq<u8>` content, not
  provenance); the `startup` fuzz target's `as_ptr_range` check remains the
  companion oracle for the literal pointer containment.

## Follow-ups

- None blocking. Corpus growth for the verified decoders is Phase 6.2; the PAL
  thin-delegator audit is Phase 6.2; encoder verification is out of scope and not
  planned. The `eunomia-sys` grant resolver's ledger note ("plain bookkeeping over
  the **separately-verified** `loader::startup` decoder") is now literally true.
