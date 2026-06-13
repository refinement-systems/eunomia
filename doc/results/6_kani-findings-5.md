# Kani verification findings — part 5 (§4.5 aspace + the §2.4 rewrite)

Continuation of `doc/results/2_kani-findings.md` (§4.1) through
`5_kani-findings-4.md` (§4.4) for the address-space / page-table-walker suite
(plan `doc/plans/0_kani-rewrite.md` §4.5). Harnesses live in
`kcore/src/proofs/aspace.rs` under `#[cfg(kani)]` and run via `cargo kani -p
kcore` (CI job `kani`, pinned cargo-kani **0.67.0**). The standing caveat, the
bounds policy, and the design notes (DN-1…DN-6) of the earlier parts apply
unchanged; only what is *new* to §4.5 is recorded here.

Unlike §4.2–§4.4 (which *relocated* logic into `kcore`), §4.5 is **phase 5, the
rewrite**: the AArch64 page-table walker — previously `kernel/src/aspace.rs`,
all `(*l1e & MASK) as *mut u64` int→ptr casts and inline `tlbi`/`dsb` — is
rebuilt in `kcore::aspace` as **safe Rust over the table pool as an indexed
slice** (plan §2.4), then verified. This is the code where a logic bug *is* a
memory-isolation hole, so the gate is not just Kani but the **full QEMU suite**
(below).

## Standing caveat (unchanged)

**Every result here is bounded.** The pure harnesses (`check_pte_encode`,
`check_va_bounds`) run fully nondet over all `(pa, perms)` / `(va, pages)`. The
walker harnesses use a fresh L1 + a 1–3-table pool and **pin VAs to a small
window** so the L1/L2/L3 indices stay concrete — `[u64; 512]` tables make
symbolic indexing the cost driver (the §4.3 lesson, DN-5). The VA-coverage
obligation is therefore carried entirely by the pure `check_va_bounds`; the
walker harnesses prove the *algorithm* (atomicity, no-remap, exact unmap, pool
accounting) at concrete VAs.

## The rewrite (behaviour byte-identical)

The on-hardware descriptor format is unchanged (a table descriptor still stores
the table's PA); only the walker's *addressing* changed. `kcore::aspace` now
holds: pure `pte_encode`/`pte_output_pa`, the VA-split `l1_index/l2_index/
l3_index`, `va_range_ok`, the PA↔pool-index conversion (`pa_of_table` /
`pool_index`, the latter bounds-guarded so the walker is **total** — the old
pointer walk had neither bound nor provenance), and the slice walker
`map_in`/`unmap_in`/`range_mapped_in`/`lookup`. The TLBI/DSB maintenance moved
behind three new `Env` methods (`tlb_invalidate_page`, `barrier_after_map`,
`barrier_after_unmap`) — the §2.2 rule-3 `Hal` seam, landed as `Env` methods;
`KernelEnv` implements them with the exact asm the old `map`/`unmap` ran inline,
and `GhostEnv` records them (new `GhostEvent::{TlbInvalidate, BarrierMap,
BarrierUnmap}`) so `check_unmap_exact` can witness one TLBI per cleared page.
`kernel/src/aspace.rs` is now a thin shell: `init`/`ttbr0`/`destroy_aspace`
stay (ASID allocator, boot kernel-L1 copy), and `map`/`unmap`/`range_mapped`
keep their exact signatures but build the `&mut [[u64; 512]]` slice views over
the aspace's physical L1/pool addresses (the one sanctioned int→pointer
boundary) and call the verified `_in` functions.

## What §4.5 verifies

| Harness | Property |
|---|---|
| `check_pte_encode` | AF + PXN unconditional; valid page descriptor; address round-trips; W⇒`AP_EL0_RW` ¬W⇒RO; **device⇒UXN + SH_NONE + device attr** (AS-1); normal memory executable iff `PERM_X` |
| `check_va_bounds` | `va_range_ok` ⇔ page-aligned ∧ `[USER_VA_BASE,USER_VA_END)`; every page of an accepted range lands in L1 entries `[2,511]` — never the two shared kernel entries |
| `check_map_model` | map installs exactly the requested page or fails **atomically**; nondet pool size exercises the `NeedMemory` path; the two-pass design means a failure writes no leaf |
| `check_map_no_silent_remap` | re-mapping a mapped VA ⇒ `AlreadyMapped`, existing leaf untouched |
| `check_unmap_exact` | unmap clears exactly the mapped pages; ghost `Env` records one `TlbInvalidate` per cleared page + one `BarrierUnmap` |
| `check_range_mapped` | `range_mapped_in` ⇔ a ghost mapped-set model (+ writability), incl. `len==0` and the `va+len` overflow edge — the predicate the syscall layer trusts |
| `check_pool_accounting` | `pool_used ≤ pool_pages` always; `NeedMemory` exactly at exhaustion; freshly allocated tables are zeroed |

All seven verify.

## Findings

| ID | Date | Harness | Bounds | Severity | Description | Status |
|----|------|---------|--------|----------|-------------|--------|
| AS-1 | 2026-06-13 | `check_pte_encode` | none (all `u64`) | Medium | The walker built `xn = if perms & PERM_X { 0 } else { UXN }` with no device exception, so `PERM_DEVICE \| PERM_X` encoded an **executable MMIO page**, violating the "device never executable" contract (spec §2.5). Reachable by any `phys`-capable cap holder (`syscall.rs` gates device on the PHYS right but not on `!X`); `a[3]` perms are raw user input. | Fixed |

Confirmed real exactly as the §4.2 overflow findings were: on the pre-fix
encoding `check_pte_encode` fails with `assertion failed: pte & UXN != 0` for a
`PERM_DEVICE | PERM_X` input. The fix forces `UXN` whenever `PERM_DEVICE` is
set (ignoring `PERM_X`), and the harness then passes; it is the permanent
regression guard. Because the kernel `map` now routes through `pte_encode`, the
fix reaches the real kernel automatically — no separate kernel change.

## Design / engineering notes new to §4.5

- **DN-7 — the walker is total where the old one was not.** The old pointer
  walk dereferenced whatever PA a descriptor held; a corrupt descriptor was a
  wild read. The slice walker converts a descriptor to a pool index with
  `pool_index`, which **bounds-checks** against the pool length and returns
  `None`/`NeedMemory` on an out-of-range index. For well-formed tables
  (everything `map_in` writes) this is never taken — every descriptor stores
  `pool_base + idx*PAGE` with `idx < pool_used ≤ pool_pages` — but the guard
  keeps the walker total for CBMC (no symbolic wild index) and is strictly
  safer than the code it replaces.
- **VA-pinning for cost (DN-5 applied).** `[u64; 512]` tables are the cost
  driver; the walker harnesses use concrete VAs (so the three table indices are
  concrete) and 1–3-page pools, keeping every harness ≤ ~10 s. Fully-nondet VA
  coverage is `check_va_bounds`' job (pure arithmetic, no tables).

## QEMU regression gate (the phase-5 behavioural proof)

Kani proves the rewritten walker's logic; the QEMU suite proves the extraction
preserved on-hardware behaviour. Both run locally (`qemu-system-aarch64`):
`bash scripts/m1-test.sh` (caps/CDT/revoke/reports + the §3.3 teardown) and
`bash scripts/spawn-test.sh` (the 100× spawn/reclaim burn loop, which maps and
unmaps the per-child time-page frame every cycle — exercising the rewritten
`map_in`/`unmap_in` end-to-end) both pass on the rewritten walker.

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Measured on the dev machine (cargo-kani 0.67.0).

| Harness | Bounds | Time |
|---------|--------|------|
| `check_pte_encode` | none (all `u64`) | ~0.7 s |
| `check_va_bounds` | none (all `u64`) | ~0.5 s |
| `check_map_model` | L1 + 1–3-table pool, 1 page | ~5.5 s |
| `check_map_no_silent_remap` | L1 + 3-table pool, 1 page | ~4.6 s |
| `check_unmap_exact` | L1 + 3-table pool, 2 pages | ~10.5 s |
| `check_range_mapped` | L1 + 3-table pool, nondet query window | ~6.0 s |
| `check_pool_accounting` | L1 + pool (zeroing + exhaustion) | ~2.6 s |
