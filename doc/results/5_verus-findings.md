# 5 — loader `Segment::page_layout` under Verus (Task 5)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 5
(ELF `Segment::page_layout` — total, overflow-safe page geometry; Tier 1, the
loader crate-onboarding step). Outcome: **verified, shipped — full contract, no
fallback used.** `cargo verus verify -p loader --no-default-features` is a new
gate line reading **`9 verified, 0 errors`**. No reverts. No collateral change to
any other crate's proofs or `rlimit` budgets — the whole gate stays green at its
prior numbers (ipc 68 re-verified transitively, byte-identical).

## What was attempted

Bring `loader/src/elf.rs`'s `Segment::page_layout` under Verus and, in doing so,
onboard `loader` into the verification gate for the first time. `page_layout`
computes the page-aligned VA span a segment maps into; it runs on untrusted
images in both `parse()` and `spawn::prepare()` (rev2§3.7), so rev2§5.3 demands
refuse-not-crash totality: a segment whose page-rounded end overflows `u64` must
be refused, never wrapped or panicked. Previously this was guarded only by two
by-example unit tests (`page_layout_normal`, `page_layout_overflow_boundary_refused`).

The page-geometry cluster — `PAGE`, `PAGE_MASK`, `Segment`, `PageLayout`,
`ElfError`, and `page_layout` — now lives in one `verus!{}` island; the ELF/
startup byte decoders (`parse`, `u*le`, `Image`) and the target-only `spawn` stay
external plain Rust.

Contract added (plain Verus; `by (bit_vector)` for the mask identities,
modular-arithmetic lemmas for the exact page count):

```rust
pub fn page_layout(&self) -> (res: Result<PageLayout, ElfError>)
    ensures
        res is Err <==> self.vaddr + self.memsz + PAGE_MASK > u64::MAX,
        res matches Err(e) ==> e == ElfError::BadSegment,
        res matches Ok(l) ==> {
            &&& l.va_start & PAGE_MASK == 0
            &&& l.va_end & PAGE_MASK == 0
            &&& l.va_start <= self.vaddr
            &&& (self.memsz > 0 ==> self.vaddr < l.va_end)
            &&& l.page_offset < PAGE
            &&& l.page_offset == self.vaddr - l.va_start
            &&& l.pages * PAGE == l.va_end - l.va_start          // the "hard" clause — proven
        },
```

discharged by two helper lemmas:

```rust
// align-down by an arbitrary mask: holds for EVERY mask, so one symbolic by (bit_vector).
proof fn lemma_align_down(x: u64, m: u64)
    ensures (x & !m) <= x, (x & !m) & m == 0, (x & !m) + (x & m) == x, (x & m) <= m
{ assert(...) by (bit_vector); }

// span between two PAGE-aligned bounds is an exact multiple of PAGE.
proof fn lemma_pages_exact(lo: u64, hi: u64)
    requires lo & PAGE_MASK == 0, hi & PAGE_MASK == 0, lo <= hi
    ensures (hi - lo) / (PAGE as int) * (PAGE as int) == hi - lo
{
    vstd::arithmetic::power2::lemma2_to64();
    vstd::bits::lemma_u64_low_bits_mask_is_mod(lo, 12);
    vstd::bits::lemma_u64_low_bits_mask_is_mod(hi, 12);
    vstd::arithmetic::div_mod::lemma_sub_mod_noop(hi as int, lo as int, PAGE as int);
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod((hi - lo) as int, PAGE as int);
}
```

These mechanize, ∀ `(vaddr, memsz)`, the refuse-not-crash totality the two unit
tests checked by example. The tests are **kept** as the companion oracle tier.

## Result

Full gate, re-run cold (`scripts/verus-baseline.sh`; every crate real-run,
results line present; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`):

| crate | result |
|---|---|
| kcore | 404 verified, 0 errors |
| ipc | 68 verified, 0 errors |
| urt | 25 verified, 0 errors (freelist dep 29) |
| freelist | 29 verified, 0 errors (no-alloc gate) |
| dma-pool | 0 verified, 0 errors |
| cas `--no-default-features` | 75 verified, 0 errors |
| virtio-blk | 29 verified, 0 errors |
| storage-server `--no-default-features --lib` | 14 verified, 0 errors |
| **loader** `--no-default-features` | **9 verified, 0 errors** |

The 9 own-surface items are `PAGE`, `PAGE_MASK`, the `Segment`/`PageLayout`/
`ElfError` types (their derive obligations), the two helper lemmas, and
`page_layout`. Per-function `rlimit` (cold, `--time-expanded --output-json`), all
modest (vs freelist's 110M+):

| item | rlimit |
|---|---|
| `lemma_align_down` | 297,434 |
| `Segment::page_layout` | 67,254 |
| `lemma_pages_exact` | 20,055 |
| `PAGE_MASK` | 736 |
| consts / derives | ~2 each |

A cold `-p loader` run also re-verifies the gated dep `ipc` (68 verified) — its
`rlimit` total is byte-identical to its standalone gate (no proof of ipc is
touched). No budget anywhere needed touching.

## Findings that mattered

1. **`PAGE - 1` written inline is mathematical-`int` subtraction in spec
   positions, and `&`/`u64` lemma args reject it.** Verus spec arithmetic on
   `u64` promotes to `int`, so `self.vaddr & (PAGE - 1)` in an `ensures` is a
   type error (`expected u64, found int`), as is passing `PAGE - 1` to a
   `proof fn(_: u64)`. Fix: a named `pub const PAGE_MASK: u64 = PAGE - 1;` (the
   `aspace.rs` `PAGE_MASK` precedent). It must be `pub` because the public
   `page_layout`'s `ensures` names it ("cannot refer to private const item").
   Int division in an `ensures` likewise needs the divisor cast: `(hi - lo) /
   (PAGE as int)`, not `/ PAGE`.

2. **No subtraction inside `by (bit_vector)` — and "difference of two aligned
   values is aligned" is FALSE for a symbolic mask.** The first instinct,
   `assert((va_end - va_start) & (PAGE-1) == 0) by (bit_vector) requires
   va_start & (PAGE-1)==0, va_end & (PAGE-1)==0`, fails twice: (a) `by
   (bit_vector)` sees `PAGE-1` as a symbolic const, and the claim only holds for
   a *contiguous low-bit* mask (counterexample for `m = 4`: `8 & 4 == 0`,
   `2 & 4 == 0`, but `(8-2) & 4 == 4`), so it is genuinely unprovable symbolically;
   (b) `va_end - va_start` is `int` in the spec context, and `&` is undefined on
   `int`. **The clean route stays in the modular world**: prove both bounds
   `≡ 0 (mod PAGE)` via vstd's `low_bits_mask_is_mod` (the low-12-bit mask *is*
   `% 4096` since `PAGE = 2^12`), then `sub_mod_noop` (difference of two
   multiples is a multiple) and `fundamental_div_mod` (`x % d == 0 ⇒ x/d*d == x`).
   No subtraction ever enters a bit-vector query.

3. **The `lemma_align_down` facts are mask-agnostic, so one symbolic
   `by (bit_vector)` covers them.** `(x & !m) <= x`, `(x & !m) & m == 0`,
   `(x & !m) + (x & m) == x`, and `(x & m) <= m` hold for *every* `m`, so the
   lemma takes `m: u64` symbolic and needs no PAGE literal. Only the
   *exact-page-count* fact (point 2) is value-specific (needs `PAGE = 2^12`);
   keeping the two concerns in separate lemmas is what let the bit-vector half
   stay symbolic. This is the §10 "decomposition into tightly-keyed lemmas"
   discipline paying off.

4. **The combinator chain was rewritten to explicit `match`.** vstd *does* spec
   `checked_add`/`checked_sub` and `Option::and_then/map/ok_or` (`std_specs`), so
   the original `.checked_add(..).and_then(..).map(..).ok_or(..)?` compiles under
   Verus — but threading postconditions through the closures' `FnOnce` specs is
   awkward, and the repo idiom inside `verus!{}` is explicit `match` (cas
   `decode_*`). The body is now nested matches on `checked_add`; the `Err
   <==> overflow` biconditional then falls straight out of the two `checked_add`
   `returns` specs. **The defensive `checked_sub` was dropped**: the proof shows
   `va_start <= va_end` (round-up never drops below the input), so the span
   subtraction is total — a stronger guarantee than the runtime guard, and
   keeping `checked_sub` would add a dead `Err` path that breaks the
   `Err <==> overflow` biconditional. Behavior is unchanged (the kept unit tests
   pass).

5. **The full `pages * PAGE == span` clause verified — the plan's sanctioned
   unit-test fallback was not needed.** The modular route (point 2) closed it
   cleanly at `rlimit` 20,055.

## Reverted vs kept

Nothing reverted — the proof succeeded once the `PAGE_MASK` typing and the
modular `pages_exact` route were in place. Kept: the loader gate (Cargo.toml
`vstd` + `metadata.verus` + lints), the `verus!{}` page-geometry island (cluster
relocated inside it, `PAGE_MASK`, the two lemmas, the `page_layout` contract, the
combinator→match rewrite, the `checked_sub` removal), and the bookkeeping (CI
line, CLAUDE.md gate list, `verus-baseline.sh` `ALL_CRATES` + the `loader →
--no-default-features` arg case, ledger Baseline row). The `page_layout_*` /
`parse` unit tests and the `tests/layout_props.rs` proptest are unchanged (the
kept companion oracle tier).

## Proposed guideline additions (`doc/guidelines/verus.md`)

1. **Name `mask = align - 1` as a `u64` const, never `align - 1` inline in
   specs.** In a spec position `align - 1` is `int` and fails to type against
   `&` and `u64` lemma parameters; a `pub const MASK: u64 = ALIGN - 1;` (the
   `aspace.rs` `PAGE_MASK` shape) fixes it, and must be `pub` if a public fn's
   `ensures` references it. Cast the divisor in int-division `ensures`
   (`x / (ALIGN as int)`).

2. **Keep subtraction out of `by (bit_vector)`; do "aligned − aligned is
   aligned" via `low_bits_mask_is_mod` + `sub_mod_noop`.** "difference of two
   `m`-aligned values is `m`-aligned" is only true for a *contiguous low-bit*
   mask, so it is unprovable with a symbolic mask, and `&` is undefined on the
   `int` that spec subtraction yields. Route through the modulus instead (the
   low-`k`-bit mask equals `% 2^k`), staying in `vstd::arithmetic::div_mod`.

3. **Split mask-agnostic bit facts from value-specific ones.** Align-down /
   partition identities hold for every mask — prove them with one symbolic
   `by (bit_vector)` lemma — and reserve the value-specific (power-of-two)
   reasoning for a separate modular lemma. Mixing them forces literals into the
   bit-vector half for no reason.

## Trusted base

**Tally unchanged at 14.** loader adds no `external_body`/`assume_specification`
— `page_layout` is pure `u64` bit/modular reasoning citing only vstd's
already-trusted `bits`/`div_mod` lemmas, and the ELF/startup byte decoders and
`spawn` stay external (outside `verus!{}`), unverified-by-construction as before
(Task 11 will bring `parse()` under the gate atop this `page_layout` contract).
New Baseline row: `-p loader --no-default-features` → 9 verified, 0 errors.
CLAUDE.md gate list, CI `verus` job, and `verus-baseline.sh` `ALL_CRATES` updated
to include loader.
