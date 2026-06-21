# B11B findings — rewire `urt`'s `Heap<N>` onto the verified `freelist::FreeList`

**Phase:** B11B (second sub-phase of B11, `doc/plans/12_b11-detail.md`). The headline: delete the
raw-`*mut Block` intrusive allocator and rewire `Heap<N>` onto the side-stored, Verus-verified
`freelist::FreeList<HEAP_RANGES>` extent model. Closes the *algorithm* half of the audit's "urt
heap allocator wholly unverified" finding (§4.2, **high**). The wrapper Miri+proptest tier and the
trusted-base ledger flip remain for **B11C**.

**Decisions pinned:** Design decision 1 (side-stored `FreeList` extents, not the intrusive arena
graph) and Design decision 3 (fragmentation cap + over-cap/over-align policy).

## What landed (`urt/src/lib.rs`)

A near-total rewrite of the allocator; `slots`, `time`, `spawn` untouched.

- **Deleted** `struct Block`, `MIN_BLOCK`, the `head: UnsafeCell<*mut Block>` +
  `initialized: UnsafeCell<bool>` fields, `init_once`, `round`, and the entire pointer-walking
  `alloc`/`dealloc` bodies (the largest single block of unverified `unsafe` in the crate).
- **New shape:** `#[repr(C, align(64))] Heap<N> { mem: UnsafeCell<[u8; N]>, fl:
  UnsafeCell<Option<FreeList<HEAP_RANGES>>> }`. The arena `mem` is now pure storage (never holds
  allocator metadata); the free list is side-stored. Consts: `HEAP_RANGES = 1024` (fragmentation
  cap), `MIN_ALIGN = 16` (offset granularity), `MAX_ALIGN = 64` (arena base alignment).
- **`fl_mut`** lazily builds `FreeList::new(N)` (the proven fresh-heap state, single extent
  `[0, N)`) on first alloc via `Option::get_or_insert_with`. `Heap::new()` stays `const fn`.
- **`alloc`** = over-align guard (`align > 64 → null`) + `need = size.max(1).next_multiple_of(16)`
  + `fl.alloc(need, align.max(16))` (verified) + the lone raw former `(mem.get() as *mut
  u8).add(off)` (`off+need ≤ N` by `alloc`'s `ensures`; aligned because base is `align(64)`).
- **`dealloc`** = `is_full() → debug_assert witness + return` (leak, never abort a free) +
  `debug_assert!(is_allocated(off, need))` (double-free guard, debug-only) + `fl.free(off, need)`.
  `realloc` is the inherited `GlobalAlloc` default.
- **Module doc** rewritten to describe the side-stored design + a `MVP simplifications, recorded:`
  block (the `cas/src/store.rs` pattern) disclosing the `HEAP_RANGES = 1024` cap, the dealloc-leak
  policy, and `MAX_ALIGN = 64`.

## Verification (all green, run locally)

Verus toolchain at `/Users/mjm/inst/verus`, version `0.2026.06.07.cd03505` (the pin), Rust 1.95.0.

| Check | Result |
|---|---|
| `cargo verus verify -p urt` | **29 verified, 0 errors** (own slots+time) + re-checks `freelist` **29/0** |
| `cargo verus verify -p freelist` | **29 verified, 0 errors** (the heap's algorithm; now *exercised* by urt) |
| `cargo verus verify -p dma-pool` | **0 verified, 0 errors** (unchanged; proof lives in freelist) |
| `cargo test -p urt` (debug) | **19 passed** incl. new `over_alignment_returns_null` + `dealloc_at_cap_witness_in_debug` |
| `cargo test -p urt --release` | new `dealloc_at_cap_leaks_in_release` passes (see note below) |
| `cargo test -p freelist -p dma-pool` | freelist 1, dma-pool 18 — all pass |
| `cd kernel && cargo build` (aarch64 cross + user binaries) | clean (pre-existing kcore warnings only) |
| `.bss` placement (storaged ELF) | `.data = 0`, `.bss = 3,162,184` NOBITS, align 64 — **no `.data` bloat** |
| `.bss` placement (ushell ELF) | `.data = 0`, `.bss = 1,065,032` NOBITS |
| `bash scripts/boot-test.sh` | **BOOT TEST PASS** (snapshots #2/#3 strictly ordered; `date` in window) |

## The Verus-count reconciliation (honesty note)

The B11B plan anticipated `cargo verus verify -p urt` rising to a "new, higher total." It did
**not** rise — and that is correct, given B11A's extract decision:

- **urt's own verus count is `29/0`, unchanged before and after B11B** (confirmed by `git stash`:
  the pre-B11B heap also verified urt at 29). urt's verified surface is `slots` + `time`; the heap
  wrapper is plain Rust (the trusted arena seam), so it adds **zero** obligations to urt — exactly
  as the DMA-pool wrapper adds zero to dma-pool.
- **The heap's *algorithm* proof is `freelist`'s `29/0`.** B11A made urt *depend* on `freelist`
  but left it unused; **B11B wires the allocator onto it**, so those 29 obligations are now the
  live proof behind every userspace `alloc`/`dealloc`. `cargo verus verify -p urt` re-checks them
  transitively (the dependency has `verify = true`).
- **B11A's findings table mis-recorded urt as "58 verified".** That number was `29` (urt) + `29`
  (freelist) summed; urt's own count has always been 29. No proof was lost in B11B. *(B11C should
  correct the ledger/findings wording to match: urt 29/0 own; heap algorithm via freelist 29/0.)*

So the honest statement: **the heap algorithm is now under proof** (freelist 29/0, reused by urt),
the **wrapper is the trusted byte-region seam** (plain Rust, Miri+proptest in B11C), and **no
existing proof was weakened** (slots/time 29/0, freelist 29/0, dma-pool 0/0 all unchanged).

## Decision-3 behaviour (the one new semantic)

- **`alloc` over-cap / no-fit → null.** `FreeList::alloc` returns `None`; the wrapper maps it to
  `ptr::null_mut()` — the correct `GlobalAlloc` OOM signal.
- **`alloc` over-aligned (`align > MAX_ALIGN`) → null.** Tested by `over_alignment_returns_null`.
- **`dealloc` at the cap → leak, never abort.** `FreeList::free` requires `nfree < N` (its
  no-merge arm's `insert_at` would index `free[N]`), so at the cap the wrapper returns without
  recording the region. Safe: the bytes are simply never re-handed-out; the invariant "every
  listed extent is truly free" holds. A `debug_assert!(false, …)` is a **debug witness only** —
  release is a silent leak. This deliberately differs from dma-pool (which `assert!`s and aborts,
  correct for a 64-extent driver pool); a general heap must never abort a free.
- **Cap-leak test profile split.** Because the witness is a `debug_assert!`, the test is cfg-split:
  `dealloc_at_cap_witness_in_debug` (`#[cfg(debug_assertions)]`, `#[should_panic]`) confirms the
  debug witness fires; `dealloc_at_cap_leaks_in_release` (`#[cfg(not(debug_assertions))]`) confirms
  the release path returns without aborting and the allocator keeps serving. Both drive `nfree` to
  exactly `HEAP_RANGES` via a fully-carved `Heap<{2050*16}>` (2050 16-byte blocks; free the first
  1024 even-indexed → 1024 non-adjacent extents; victim = block 2048, both neighbours live).

## Incidental notes

- **`slots::tests::double_free_panics` fails under `cargo test --release`** — pre-existing and
  unrelated: it is a `#[should_panic]` test resting on `slots.rs`'s `debug_check_free`
  `debug_assert!` (untouched by B11B), so it can only panic in debug. The project runs `cargo test`
  in debug; this is not a B11B regression. (B11B's own cap test avoids this trap by cfg-gating on
  `debug_assertions`.)
- `user/init` does **not** instantiate `urt::Heap` (no `#[global_allocator]`); only `ushell`
  (1 MiB) and `storaged` (3 MiB) do. Both link and boot on the new allocator.
- The `align(64)` arena attribute is visible in the ELF: `.bss` section alignment is 64.

## Not in B11B (the chain continues)

- **B11C** adds the wrapper Miri+proptest tier (mirrors B4C: Properties 1–3 + oracle-sanity
  control), extends the `CLAUDE.md` Miri sweep to name `urt`, and **flips the trusted-base ledger**
  to record the urt heap free-list as verified surface + the arena byte-region seam as the trusted
  plain-Rust boundary (and should correct the B11A "58" wording per the reconciliation above).
