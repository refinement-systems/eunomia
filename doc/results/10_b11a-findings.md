# B11A findings — extract the verified `FreeList<N>` into a shared `freelist` crate

**Phase:** B11A (first sub-phase of B11, `doc/plans/12_b11-detail.md`). Lands the verified
free-list core where the `urt` heap can reuse it in B11B. **No heap behaviour change.**

**Decision pinned:** Design decision 2 — *extract* (recommended), not *copy-into-urt*. The
proof now has one home; `dma-pool` and `urt` both depend on it.

## What landed

- **New crate `freelist/`** — `no_std`, vstd-pinned, `[package.metadata.verus] verify = true`.
  `src/lib.rs` is the `verus!{}` `FreeList<N>` block lifted **verbatim** from
  `dma-pool/src/lib.rs:86–1321` (the `wf`/`covers`/`alloc`/`free`/`remove_at`/`insert_at`/
  `is_full`/`is_allocated` surface, every `alloc_proof_*`/`split_*`/`free_*` lemma, and the
  top-level `lemma_two_allocs_disjoint`). The one FreeList-only test (`accessor_sanity`)
  travelled with it, with `MAX_FREE_RANGES` → a concrete `FreeList::<64>` (the cap const
  stays in dma-pool). Cargo.toml mirrors `dma-pool/Cargo.toml`, including the
  `#![cfg_attr(not(any(feature = "std", test)), no_std)]` + `std` feature so host tests link
  while the aarch64 cross-build stays `no_std`.
- **`dma-pool`** re-points at `freelist`: the `verus!{}` block is deleted, `use
  freelist::FreeList;` replaces `use vstd::prelude::*;` (the wrapper is plain Rust and needs
  no vstd prelude now), the `DmaPool` field/`new`/`alloc`/`free`/`is_full`/`is_allocated`
  call-sites are otherwise untouched, and the module doc + Cargo dep updated. `MAX_FREE_RANGES`
  stays in dma-pool.
- **`urt/Cargo.toml`** gains the `freelist` path-dep — **unused in B11A** (the B11B heap core).
- **Workspace** `members` gains `"freelist"`; **CI** gains `cargo verus verify -p freelist`.
- **Ledger** (`verus_trusted-base.md`): the "DMA-pool `FreeList`" baseline row split into a
  `freelist` row and a slimmed `dma-pool` row; scope prose notes the new home. urt's heap is
  **not** added to the verified surface yet (that is the B11C flip).

## Verification (all green, run locally)

Verus toolchain present locally at `/Users/mjm/inst/verus/cargo-verus`, version
`0.2026.06.07.cd03505` — exactly the pin. So every gate ran locally, not just CI.

| Check | Result |
|---|---|
| `cargo verus verify -p freelist` | **29 verified, 0 errors** |
| `cargo verus verify -p dma-pool` | **0 verified, 0 errors** |
| `cargo verus verify -p urt` | **58 verified, 0 errors** (own slots+time) + freelist dep **29/0** |
| `cargo test -p freelist` | `accessor_sanity` passes |
| `cargo test -p dma-pool` | 18 pass (was 19; `accessor_sanity` moved out) |
| `cargo test -p urt` | 17 pass (unchanged) |
| `cargo build --workspace --exclude kernel` | clean |
| aarch64 cross-build of `dma-pool`+`urt` (`-Z build-std`) | clean; kernel ELF built |
| `scripts/boot-test.sh` (end-to-end QEMU) | **BOOT TEST PASS** |

## The obligation-count reconciliation (honesty note)

The parent plan phrased the guard as "`cargo verus verify -p dma-pool` ≥ **29/0** unchanged".
That described the *combined* surface, not dma-pool's own count. The actual mechanism:

- The 29 FreeList obligations were **wholly relocated** to `freelist` (now 29/0).
- `dma-pool`'s own count is **0** — its `DmaPool` wrapper was always plain Rust *outside*
  `verus!{}` (the trusted PA/backing seam), so removing the block leaves it with nothing of
  its own to verify. It stays green (0 errors) as a tripwire.
- Cross-crate: when `cargo verus verify` runs on a crate whose dependency has `verify = true`,
  it **re-verifies** that dependency too (observed: `-p urt` printed both `58 verified` and
  `29 verified`). So the freelist proof is re-checked transitively from urt as well.

Net: the original 29 obligations are preserved (29 ≥ 29), zero errors, **no proof weakened
or deleted** — exactly "a move, not a weakening". The ledger's `dma-pool` row records the
0/0 with that annotation so the drop from 29 is not misread as a regression.

## Incidental notes

- No external crate imported `dma_pool::FreeList` (consumers use `DmaPool`/`DmaBacking`/
  `DmaBuf`/`host::*`/`DeviceAddress` only), so the move broke no downstream user.
- `dma-pool` keeps its `vstd` dependency and `verify = true` even though it now has no
  `verus!{}` code — harmless (vstd erases in ordinary builds), and it keeps the gate live for
  any future verus code re-added to the wrapper.
- The pre-existing `kcore` unused-import warnings and the `cargo build --workspace` E0152 on
  the bare-metal `kernel` bin are unrelated to B11A.

## Not in B11A (the chain continues)

- **B11B** rewires `Heap<N>` onto `freelist::FreeList` and deletes the raw-`*mut Block`
  allocator (the fragmentation cap + leak-at-cap policy land there).
- **B11C** adds the wrapper Miri+proptest tier, extends the `CLAUDE.md` Miri sweep to name
  `urt`, and *flips* the ledger to record the urt heap free-list as verified surface.
