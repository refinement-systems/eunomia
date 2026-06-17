# Verus findings 39 — Phase 7c: `urt::slots` — the bitmap free-list

Plan: `doc/plans/3_verus-rewrite.md` (§4.7, §7 step 6) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7c). Prior increment: `58`
(phase 7b — the `ipc::session` codecs + `Admission` quota). This increment is the
third host-chokepoint migration: the cspace-slot bitmap free-list in
`urt/src/slots.rs` — from Kani (bounded, CAP=4 / unwind 6) to Verus (unbounded,
∀ `cap` and `WORDS`). It is the first 7-series port whose proof is **not**
straight-line byte arithmetic: it restructures the `.find().map()` combinators
into invariant-carrying loops and relates the packed bitmap `free[i/64] &
(1<<(i%64))` to a per-slot free predicate by `by (bit_vector)` frame lemmas.

`cargo verus verify -p urt`: **21 verified, 0 errors** (no rlimit bump or
`spinoff_prover`). `cargo test -p urt`: **17 passed** (the `verus!{}` block erases
— the `slots` unit tests, incl. `double_free_panics`, and the `time` tests see the
same code). `cargo kani -p urt`: **1 harness, 0 failures** (was 4 — the three
`check_slots_*` are deleted; `check_time_conversion_total` stays — `time` is 7d).
`cd kernel && cargo build`: green — `urt`'s new direct `vstd` dep erases into all
five user binaries (it already arrived transitively via `ipc` since 7a, so no new
cross-build risk).

---

## 1. Toolchain: `vstd` into `urt`

The 7a recipe, applied to `urt`: `vstd = { version = "=0.0.0-2026-05-31-0205",
default-features = false }` (lockstep pin), `[package.metadata.verus] verify =
true`, the crate-root `#[allow(unused_imports)] use vstd::prelude::*;`, and
`cfg(verus_keep_ghost)`/`cfg(verus_only)` added to `unexpected_cfgs`. `urt` is in
every userspace binary's build graph and already pulled `vstd` transitively through
its `ipc` path-dep, so the userspace-cross-build risk the 7a pilot cleared did not
recur — confirmed by the kernel cross-build. The `spawn` module is
`#[cfg(target_os = "none")]` and the `proofs` module is `#[cfg(kani)]`, so the host
Verus build sees neither.

## 2. The model (direct bit-test, `closed`)

- `wf(self)`: the bitmap is wide enough (`cap <= WORDS*64`) and the whole window
  fits the `u32` slot-id space (`base + cap <= u32::MAX`, so `base + i` never
  overflows — a well-formedness conjunct the old code left implicit).
- `is_free_spec(self, i)`: `self.free@[i/64] & (1u64 << ((i%64) as u64)) != 0` — the
  per-slot free bit, meaningful for `0 <= i < cap` (then `i/64 < WORDS`).
- `spec_base()`/`spec_cap()`: `closed` accessors for the window geometry.

**All four are `pub closed`, not `pub open`.** A `pub open spec fn` body must be
"well-formed everywhere," which forbids it from naming the private `base`/`cap`/
`free` fields (`error: disallowed: field expression for an opaque datatype`). The
same rule hits a *public function's* `requires`/`ensures` that name a field
directly — so every public contract is routed through `wf`/`is_free_spec`/
`spec_base`/`spec_cap` instead of `.base`/`.cap`/`.free@`. This is exactly the 7b
`Admission` recipe (`well_formed`/`spec_remaining` as `closed` accessors); `closed`
bodies stay transparent to the in-module proofs, opaque to callers. **Private
helpers (`is_free`, `set`) are exempt** — only `pub fn` contracts trip the rule —
so they keep naming `self.base`/`self.cap`/`self.free@` directly.

## 3. The obligations (∀ `cap`, `WORDS`)

| Op | `ensures` (the unbounded theorem) |
|---|---|
| `new` | `wf`; base/cap as requested; every slot in `[0,cap)` free |
| `alloc` | `Some(s)`: in-window, was free, now used, all other slots unchanged. `None`: **exhaustion exact** — every slot used — and the allocator unchanged |
| `alloc_range` | `Some(start)`: contiguous in-window run that was all free and is now all used, outside-run frame; `n==0 ∨ n>cap ⟹ None`; `None`: unchanged |
| `free` | the freed slot is free again; others unchanged; **`!is_free_spec` precondition** (double-free a contract-checked impossibility) |
| `free_range` | the whole range freed (loop over `free`, requiring the range allocated) |

**Distinctness is now a corollary, not a bounded drain loop.** Kani's
`check_slots_alloc_unique` drained a CAP=4 allocator and asserted pairwise-distinct
results. Verus instead proves `alloc`'s modular contract — *returns a currently-free
slot and marks it used* — from which distinctness follows for any sequence: a
later `alloc` cannot return an earlier result because that slot is no longer free.
Strictly stronger than the bounded enumeration.

**Negative completeness of `alloc_range`** ("`None` ⟹ no free `n`-run exists") was
scoped out as the harder stretch goal; it was never covered by any Kani harness
(`alloc_range` was unharnessed), so omitting it regresses nothing. The positive
contract (the run is contiguous, was free, is now used) *is* proven.

## 4. The bit-frame lemmas (the crux)

The packed-bitmap reasoning is three small `by (bit_vector)` lemmas:

- `lemma_index_split(i, words)`: `i < words*64 ⟹ i/64 < words ∧ i%64 < 64` (the
  word/bit split is in range). Discharged `by (nonlinear_arith)`.
- `lemma_set_bit(x, k, free)`: writing bit `k` reads back as `free` —
  `(x | (1<<k)) & (1<<k) != 0` / `(x & !(1<<k)) & (1<<k) == 0` for `k < 64`.
- `lemma_bit_other(x, k, m, free)`: writing bit `k` leaves every other bit `m != k`
  of the word untouched (`(x | (1<<k)) & (1<<m) == x & (1<<m)` and the clear analog).

`set` combines them: the written word reads `free` at bit `i%64`; same-word other
bits are untouched (`lemma_bit_other`, using `j/64 == i/64 ∧ j != i ⟹ j%64 != i%64`);
other words are untouched by the array-element assignment. The loop-carrying ops
(`new`/`alloc`/`alloc_range`/`free_range`) then reason purely through `set`'s and
`is_free`'s contracts — no bit-vector below the helper layer.

### 4.1 The `closed`-spec glue

Because `is_free_spec` is `closed`, "the scan never wrote `self`" (a loop invariant
`self.free@ == old(self).free@`) does not *automatically* give "`is_free_spec`
agrees with entry." Two `assert forall … by { assert(self.free@[j/64] ==
old(self).free@[j/64]); }` blocks bridge it at the `None`/`Some` exits, forcing the
seq-element equality that the unfolded `is_free_spec` rests on. The `alloc`
exhaustion exit additionally needs `assert(!self.is_free_spec(j))` inside the `by`
to trigger the scan invariant, and `assert(i == self.cap)` to make its range
concrete.

## 5. Toolchain notes worth recording

- **`debug_assert!` is forbidden inside `verus!{}`** — it lowers to `panic!`
  (`error: panic is not supported`), and a format-interpolated message is an
  `Unsupported constant type` even before that. The runtime double-free guard that
  the deleted `check_slots_double_free` Kani harness exercised (and the host
  `double_free_panics` unit test still does) is preserved via a tiny
  `#[verifier::external_body] fn debug_check_free(&self, i)` holding the two
  `debug_assert!`s. Verus does not inspect its body; the **static** double-free
  guarantee is `free`'s `!is_free_spec` precondition. This adds one trivially-
  trusted, debug-only, no-ghost-effect (`&self`) function to `slots` — the only
  trusted residue in the module.
- **mut-ref postconditions use `final(self)`** (not bare `self`) — the 7b note,
  now hit by every `&mut self` op in `slots`.
- **`available()`** keeps its `(0..cap).filter().count()` iterator form in a plain
  `impl` block *outside* `verus!{}` (test/leak-assertion bookkeeping, not a named
  obligation) — Verus does not see the unsupported combinators.

## 6. What changed

- `urt/Cargo.toml` — `vstd` dep + `[package.metadata.verus]` + `unexpected_cfgs`.
- `urt/src/lib.rs` — crate-root `vstd::prelude` import.
- `urt/src/slots.rs` — one `verus!{}` block: the `closed` model
  (`wf`/`is_free_spec`/`spec_base`/`spec_cap`), the three bit-frame lemmas, and the
  contracted/loop-restructured `new`/`is_free`/`set`/`alloc`/`alloc_range`/`free`/
  `free_range`; the `external_body` debug guard; `available()` + the `#[cfg(test)]`
  tests kept (the latter verbatim).
- `urt/src/proofs.rs` — the three `check_slots_*` harnesses + the `SlotAlloc`
  import / `BASE`/`CAP` consts deleted; module doc points `slots` at `crate::slots`.
  `check_time_conversion_total` + the `Sample` import kept (7d).
- `.github/workflows/ci.yml` — `verus` job verifies `-p urt`; `kani` job keeps
  `-p urt` (the `time` harness) with a refreshed comment.
- `CLAUDE.md` — the `cargo verus verify` example, the `verus`/`kani` CI bullets, the
  Verus-tier table row, and the `### Verus` phase-7 prose note 7c.

## 7. Next

**7d — `urt::time`** (trophy #1): `utc_ns_at` totality is easy, but the prize is
**monotonicity** (`c1 ≤ c2 ⇒ utc_ns_at(c1) ≤ utc_ns_at(c2)`) — relating two u128
divisions, which Kani could not do (`doc/results/8` SOLVER note) and which proptest
only samples. Budget for `nonlinear_arith` + a division-monotonicity helper.
Landing it retires `-p urt` from the `kani` job. Then **7e — `dma-pool`** (trophy
#2: two-buffer disjointness). §1's bit_vector / mut-ref / `closed`-glue notes carry
forward.
