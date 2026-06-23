# B11C findings — `urt` heap wrapper Miri+proptest tier + the trusted-base ledger flip

**Phase:** B11C (third and final sub-phase of B11, `doc/plans/12_b11-detail.md`). The verified
`freelist::FreeList` proves the allocation *arithmetic* (rewired in B11B); B11C proves the
**wrapper drivers** — `alloc` → write the bytes → `dealloc`/`realloc` over a real arena through
the `UnsafeCell` + `base.add(off)` seam — are sound under randomized sequences, with **Miri as
the UB oracle**. Closes the audit's "Miri-cover the `urt` heap allocator" follow-up (§4.2 +
§8) and **flips the trusted-base ledger**: the heap free-list is now recorded as verified
surface (via the `freelist` proof) and the arena byte-region as the trusted plain-Rust seam,
exactly as for the DMA-pool wrapper. **B11 is complete.** Test/dev/doc only — no production
change beyond B11B, so the Verus and boot gates are pure re-checks.

**Decision exercised:** Design decision 3 — the fragmentation cap `HEAP_RANGES = 1024`, the
dealloc-at-cap safe-leak policy (debug witness, never an abort), and `MAX_ALIGN = 64` are now
covered end-to-end (and disclosed in the ledger). Mirrors B4C for `dma-pool`.

## What landed

- **`urt/src/lib.rs` — the wrapper proptest tier** (inline in `#[cfg(test)] mod tests`,
  alongside the five existing unit tests, which are unchanged). Helpers `pattern` /
  `catch_silent` / `need_of` / `fill` / `snapshot`, an `Op` enum + `op_strategy`, and a `Live`
  model, all adapted from the dma-pool B4C template (`dma-pool/src/lib.rs`):
  - **Property 1 — `alloc_dealloc_realloc_roundtrip`.** A 0–64-op random sequence over a fresh
    `Heap<4096>`: each non-null `alloc` is asserted `align`-aligned, in-arena, and its *whole
    carved extent* disjoint from every live block; a unique pattern is written through the raw
    pointer and re-read after every op, so any overlap/perturbation is caught. `realloc` goes
    through the inherited `GlobalAlloc` default (alloc-new + copy + dealloc-old).
  - **Property 2 — `exhaustion_then_coalesce`.** Fill to null (never a bad pointer at capacity),
    free everything, re-allocate the near-full span — two-sided coalescing restored the single
    extent, observed through the wrapper.
  - **Property 3 — `fragmentation_cap_never_ub`.** Fully carve a `Heap<{2050*16}>` into 16-byte
    blocks, free a random subset; each free either records a new extent or, at the cap, leaks
    safely (`catch_silent` swallows the debug witness panic) — never UB, never an abort.
- **`CLAUDE.md`** — the Miri-sweep block now names `urt` (after the `dma-pool` line from B4C),
  noting the cap leg caps Miri at one case.
- **`doc/guidelines/verus_trusted-base.md` — the ledger flip:** scope prose elevates the urt
  heap free-list to verified surface and names the arena byte-region as the lone trusted
  plain-Rust seam; the urt Baselines row becomes "slots + time + heap" with the count
  reconciliation below; a note records that the arena seam is **not** one of the 13 named
  constructs (it adds nothing — like the DMA-pool wrapper); the disclosed MVP bounds
  (`HEAP_RANGES`/`MAX_ALIGN`/leak-at-cap) are attached to the row.
- **`#![no_std]` → `#![cfg_attr(not(test), no_std)]`** (crate root). Test-only: under
  `cargo test` the crate links `std` so the proptests can use `std::panic::catch_unwind` + the
  panic hook. Verus is not a test build, so it still sees `no_std`; the aarch64 build is
  `not(test)`, so the shipped allocator is unchanged. Same idiom `dma-pool` already uses.

## Verification (all green, run locally)

Verus toolchain `/Users/mjm/inst/verus` (the pin); nightly `miri 0.1.0 (beae781308 2026-06-09)`.

| Check | Result |
|---|---|
| `cargo test -p urt` (debug) | **22 passed** — incl. the three new proptests at 256 cases |
| `cargo test -p urt --release` | new proptests + `dealloc_at_cap_leaks_in_release` pass; only `slots::double_free_panics` fails (pre-existing — a `#[should_panic]` on a `debug_assert!`, compiled out in release; unrelated to B11C, `11_b11b-findings.md:93`) |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p urt` | **22 passed, 0 failed, 38.6 s — clean** (no UB across alloc/dealloc/realloc, exhaustion, fragmentation-cap) |
| `cargo verus verify -p urt` | **29 verified, 0 errors** (slots + time; heap algorithm re-checked via the `freelist` dep) |
| `cargo verus verify -p freelist` | **29 verified, 0 errors** (untouched) |
| `cargo verus verify -p dma-pool` | **0 verified, 0 errors** (untouched; proof lives in `freelist`) |
| `cd kernel && cargo build` (aarch64 cross + user binaries) | clean (pre-existing kcore warnings only) |

## Key findings

1. **The oracle is tightened to the *carved extent*, not the requested size.** The wrapper
   rounds every request up to `MIN_ALIGN` (16), so a `size = 1` request occupies a 16-byte
   extent. If the disjointness/no-perturbation check used only `size`, two extents could overlap
   while their `size`-prefixes miss (e.g. `[0,16)` carved vs. a buggy `[8,24)`, prefixes `[0,1)`
   and `[8,9)`). So Property 1 fills and checks the **whole `need = size.max(1).next_multiple_of(16)`
   extent** (`need_of`), making any extent overlap a guaranteed content mismatch — a strictly
   stronger oracle than dma-pool's `len`-based one, warranted because urt rounds and dma-pool
   does not.

2. **The fragmentation-cap leg is the one heavy Miri case, so it gets its own config.** Reaching
   `HEAP_RANGES = 1024` requires ≥1025 separated live blocks, i.e. a ~2050-block carve — there is
   no smaller heap that reaches a 1024-cap. Property 3 therefore sits in its own `proptest!`
   block at `cases: if cfg!(miri) { 1 } else { 64 }` (Properties 1–2 keep the standard
   `{ 4 } else { 256 }`). One Miri case suffices: the deterministic `dealloc_at_cap_*` unit tests
   already drive the exact at-cap path under Miri; Property 3 adds randomized fragmentation-mask
   breadth. The whole urt Miri sweep still finishes in ~38 s (no blake3), keeping the "urt is
   fast" promise the sweep comment makes.

3. **The negative control has teeth — the broken seam is caught as a hard UB detection.**
   Reverting the wrapper's `base.add(off)` to a dropped (`add(0)`) or halved (`add(off/2)`)
   offset makes Property 1 fail immediately: the default `realloc`'s `copy_nonoverlapping` trips
   the std debug **"Undefined Behavior" precondition** on the now-overlapping old/new regions
   (process abort), and the alloc-arm disjointness assertion guards the same overlap. Under Miri
   the same skew is an out-of-bounds / aliasing write. This proves the suite guards the real
   soundness obligation, not a tautology. The unsound variant was **not committed** (the B3B/B4C
   discipline — documented, not shipped, so it never breaks the Miri sweep).

4. **Per-case heaps are fresh stack locals, not `static`s.** A `static Heap` would carry
   free-list state across proptest cases (cross-contamination); each case binds
   `let h = Heap::<N>::new()` and uses `&h`, whose address is stable for the closure and never
   moved, so the `*mut u8` pointers it hands out stay valid. The five pre-existing unit tests
   keep their `static H` (each is a single deterministic sequence, so no contamination).

5. **`prop_assert!` cannot wrap a brace-bearing expression.** `prop_assert!(unsafe { h.alloc(l) }
   .is_null())` fails to compile — the macro stringifies its condition into a `concat!` format
   string and the `{ }` are read as format holes. Fixed by binding to a local first
   (`let p = unsafe { h.alloc(l) }; prop_assert!(p.is_null());`). `prop_assert_eq!` is immune (it
   formats the two *values*, not the stringified expression).

6. **The verified-count reconciliation (closing the B11A "58").** urt's own Verus count is and
   was **29/0** (slots + `utc_ns_at`); the heap's *algorithm* is `freelist`'s **29/0**, which urt
   re-checks transitively (the dep has `verify = true`). B11A's findings table summed the two as
   "58"; the ledger now reads "urt 29/0 own; heap algorithm via freelist 29/0; heap wrapper 0
   obligations (trusted arena seam)". The seam tally stays **13** — the arena byte-region is
   plain-Rust wrapper code, not a `verus!{}` `external_body`/`assume_specification` construct.

## B11 complete — and what was deliberately *not* done

With B11C landed, the urt heap — the audit's "largest single block of unverified `unsafe` in a
verified crate" — is closed: its algorithm is the verified `freelist` proof, and its lone
remaining `unsafe` (the arena byte-region) is a disclosed, Miri+proptest-covered trusted seam,
the DMA-pool wrapper's posture. Out of scope, recorded so the bounds are decisions not gaps:
the side-stored model's `HEAP_RANGES = 1024` fragmentation cap (disclosed MVP bound; a
`free_or_coalesce` refinement to shrink the leak window is future hardening); no cargo-fuzz
target (the heap decodes nothing — trusted in-process input); no Loom/Shuttle (single-threaded
by construction). `urt::slots` and `urt::time` were already verified and are untouched.
