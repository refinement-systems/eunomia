# Kani verification findings — part 2 (§4.2 untyped/retype)

Continuation of `doc/results/2_kani-findings.md` for the untyped/retype suite
(plan `doc/plans/0_kani-rewrite.md` §4.2). Harnesses live in
`kcore/src/proofs/untyped.rs` under `#[cfg(kani)]` and run with the rest of
the suite via `cargo kani -p kcore` (CI job `kani`, pinned cargo-kani
**0.67.0**). The standing caveat, the bounds policy, and the design notes
(DN-1…DN-4) of part 1 apply unchanged; only what is *new* to §4.2 is recorded
here.

## Standing caveat (unchanged from part 1)

**Every result here is bounded.** Kani/CBMC proves a property over *all*
inputs only within the stated scope. The carve harnesses are the exception in
spirit, not in letter: `carve` is pure `u64` arithmetic with no pointers and
no loops, so `check_carve_no_overflow` / `check_carve_geometry` run over
*fully nondeterministic* `(base, size, watermark, ty, param)` with no
unwinding — within the 64-bit input domain this is genuinely exhaustive. The
install/reset harnesses use the TLC-scale `BarePool` (`POOL_SLOTS = 4` = TLA
`CapIds`), so their scope is the same small-world scope as the §4.1 suite.

## What §4.2 verifies

The harnesses re-check the TLA `Retype` action on the implementation, split
across the plan's three concerns:

| Harness | Property | Plan row |
|---|---|---|
| `check_carve_no_overflow` | `carve` is **total**: no panic/overflow for any `(base, size, watermark, ty, param)`, no input assumptions | §4.2 row 1 |
| `check_carve_geometry` | success ⇒ `start` aligned per type, `[start, end) ⊆ [base+wm, base+size)`, `bytes > 0`; the next carve at the bumped watermark is disjoint and the watermark strictly advances | §4.2 row 2 |
| `check_retype_cdt` | the new cap is a CDT child of the untyped (so `revoke(untyped)` reaches it); watermark = `end − base` | §4.2 row 3 |
| `check_retype_channel` | a channel retype installs **both** endpoints as children and lands the object at `refs == 2` (`endpoint_cap_added` × 2, `end_caps == [1,1]`) | §4.2 row 3 |
| `check_retype_rights` | rights table: Frame inherits the parent's rights; sub-Untyped masked to `READ\|WRITE` and **never** `PHYS` (§2.5 by-construction, now proven); Thread → `THREAD_ALL`; others → `ALL` | §4.2 row 4 |
| `check_reset` / `_refuses_children` / `_refuses_not_untyped` | reset clears the watermark iff no children exist (the impl form of the `Descendants = {}` guard) and the cap is Untyped | §4.2 row 5 |

### Dependencies pulled in

§4.2 needed no new infrastructure beyond what the §4.1 PRs landed: the
`BarePool` slot pool and `nondet_shape`/`pick` builders (`proofs/world.rs`),
the bounds module, and the `cspace`/`untyped`/`channel` object machinery from
the kcore extraction (phase 2). One source visibility change:
`ObjType::align` is now `pub(crate)` so `check_carve_geometry` can assert the
alignment it expects rather than re-encode the table. The
`check_retype_channel` harness builds a bare `Channel` header (no trailing
ring array) because retype's channel dance touches only `end_caps`, `hdr`, and
the two destination slots — never `Channel::slot`.

A note on `check_retype_rights`: the install's rights are a function of the
*type* and the parent's rights alone (the kind is stored verbatim and only the
`Channel` arm is special-cased), so the harness passes a notification kind for
every non-channel type and varies `ty` — exercising all four rights arms
without constructing a distinct object per type. This is asserted, not
assumed: a future change that made rights depend on the kind would break it.

## Findings

Two real defects, both predicted by inspection in plan §7.1 and **confirmed**
by `check_carve_no_overflow` (4 failing overflow checks on the pre-hardening
code), then **fixed** in the same change that adds the harness — the carve
arithmetic is now checked end-to-end. The harness is the permanent regression
guard.

| ID | Date | Harness | Bounds | Severity | Description | Status | Fix |
|----|------|---------|--------|----------|-------------|--------|-----|
| UO-1 | 2026-06-13 | `check_carve_no_overflow` | none (all `u64`) | Medium (user-triggerable DoS) | `ObjType::Untyped`'s `(param as usize).next_multiple_of(4096)` panics for `param` within a page of `usize::MAX`. `param` is raw user input (register `a[2]`), so any untyped-holder could panic the kernel by requesting a sub-untyped of a pathological size. | Fixed | `checked_next_multiple_of` → `BadArg` |
| UO-2 | 2026-06-13 | `check_carve_no_overflow` | none (all `u64`) | Low (defensive) | The placement adds `base + watermark + align − 1` (`untyped.rs:172`) and the limit `base + size` (`untyped.rs:174`) overflow at the very top of the 64-bit address space. No real untyped (physical RAM) reaches there, so this was unreachable via current callers, but an unchecked add would panic rather than reject the retype — the kind of latent edge §4.2 is meant to close. | Fixed | `checked_add` → `NoMemory` |

Both fixes preserve behaviour for every input that previously succeeded
(`checked_*` returns the same value where the unchecked op did not overflow);
only previously-*panicking* inputs now return `BadArg`/`NoMemory`. The kernel
cross-builds and the host `cargo test -p kcore` suite is unchanged. `param *
4096` (Frame) and the `bytes_for` size computations were already guarded by
their `param` bounds (`≤ 1<<16`, `≤ 1024`, `≤ 256`) and proven overflow-free
by the same harness — those guards are sufficient, no change needed (the
"prove the guard set sufficient" half of plan §7.1 item 1).

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Measured on the dev machine (cargo-kani 0.67.0). The carve harnesses are the
cheapest in the whole suite — pure loop-free arithmetic over nondet `u64`s.

| Harness | Bounds | Time |
|---------|--------|------|
| `check_carve_no_overflow` | none (all `u64`) | ~0.2 s |
| `check_carve_geometry` | none (two carves) | ~1.0 s |
| `check_retype_cdt` | `BarePool` (4 slots) | ~0.2 s |
| `check_retype_channel` | `BarePool` + bare Channel | ~0.3 s |
| `check_retype_rights` | `BarePool` (4 slots) | ~0.2 s |
| `check_reset` | `BarePool` (4 slots) | ~0.06 s |
| `check_reset_refuses_children` | `BarePool` (4 slots) | ~0.14 s |
| `check_reset_refuses_not_untyped` | `BarePool` (4 slots) | ~0.06 s |
