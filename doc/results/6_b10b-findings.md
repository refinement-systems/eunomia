# B10B — the `AspaceTopUp` syscall + abutment-checked carve + libcall (findings)

Working notes from the implementation of **Phase B10B** (`doc/plans/10_b10-detail.md`,
sub-phase B10B — the shell deliverable plus the one verified-decoder change). Records
what landed, the load-bearing design call (no new cap; advance the funding untyped's
watermark), the test/scope boundary that B10's out-of-scope funding convention imposes,
and the verification facts. Closes the **recoverable-`NEED_MEMORY`** path of audit
**M-2**; conforms rev1§2.5 ("accepts top-ups") and rev1§2.7 (total decode). Consumes
B10A's verified `lemma_grow_pool`; the teardown/accounting tests + ledger finalization are
B10C.

---

## 0. Headline

All B10B gates green:

- `cargo verus verify -p kcore` — **384 verified, 0 errors** (unchanged from B10A). Adding
  the `AspaceTopUp` decode arm + moving the totality bound re-verifies `decode` *in place*:
  it is one verified item, so the count does **not** rise (see Finding 3). The ledger's
  kcore baseline (384) is therefore already correct; **no ledger edit in B10B**.
- `cargo test -p kcore` — **105 passed** (no new test fns; two assertions added to the
  existing `sysabi::tests` — `decode(24) → AspaceTopUp`, `decode(25) → UnknownCall`).
- `cd kernel && cargo build` (aarch64-none-softfloat) — green; the `grow_pool` shell
  wrapper's `#[allow(dead_code)]` is dropped (now wired). Only the 3 pre-existing
  unused-import warnings (`cspace.rs`/`ready.rs`/`timer.rs`) remain.
- `cargo build -p ipc` — green; the user binaries (built by `kernel/build.rs`) link against
  the new `aspace_topup`/`map_grow` libcalls.
- **QEMU boot smoke green** — boots to the `eunomia> ` shell prompt (MMU + GICv3 + tick up,
  `[init] system up`, shell running). The new opcode is additive and unexercised at boot, so
  the boot path is byte-identical; opcode 25+ is still `UnknownCall` (the verified `ensures`).

## What landed

- `kcore/src/sysabi.rs` — `enum Sys` gains `AspaceTopUp { aspace, ut, pages }` (opcode 24);
  `decode` gains a `24 => …` arm and its totality `ensures` moves `nr >= 24` → `nr >= 25`
  (the **one** verified-ABI change). No field needs a range `ensures` — all three `u64`s are
  validated downstream by the carve + `grow_pool`. Two test assertions added.
- `kernel/src/untyped.rs` — `aspace_topup(ut_slot, asp, pages)`: the trusted int→ptr carve.
  Destructure the untyped cap, check `pages != 0` + abutment (`base + watermark ==
  pool_base + pool_pages*PAGE`), `carve_place` for room/placement, advance the watermark,
  `grow_pool`. The twin of `retype`.
- `kernel/src/syscall.rs` — the `Sys::AspaceTopUp` dispatch arm: slot/`Aspace`-type/`WRITE`-
  right validation mirroring `Sys::Map`, then `untyped::aspace_topup`, mapping to the
  **existing** errno set (`0`/`ERR_TYPE`/`ERR_NOMEM`/`ERR_ARG` — no new errno).
- `kernel/src/aspace.rs` — dropped the `#[allow(dead_code)]` on `grow_pool` (now reachable).
- `ipc/src/sys.rs` — `aspace_topup(aspace, ut, pages)` (opcode 24) and the `map_grow`
  convenience (top-up + retry on `ERR_NOMEM`).

## Finding 1 — the accounting call: advance the watermark, mint no cap

The crux of the shell was *how the topped-up bytes are owned*. Design decision 2 rejected a
dedicated pool-extension cap; the pool is internal to the aspace (rev1§2.5 gives up per-table
caps). So `aspace_topup` advances the funding untyped's **watermark** by `pages*PAGE` and
installs **no cap** — the opposite of `retype`, which always lands a CDT child via
`retype_install`. Soundness of that asymmetry:

- The watermark is a free-running bump pointer; gaps already exist below it (alignment
  padding between carved objects has no cap). Advancing it past the pool extension is the
  *same* situation — no `cspace_wf`/CDT invariant ties a watermark value to a child cap, so
  the direct field write `(*ut_slot).cap.kind = Untyped { …, watermark: c.end - base }` is
  sound. It is exactly what `KernelStore::set_slot` does (`*slot_ptr = v`) and what
  `kernel/src/main.rs` does to bootstrap the root cspace's caps — the sanctioned trusted
  int→ptr posture.
- **Teardown rides the existing machinery (B10C verifies by test).** The aspace cap is a CDT
  child of the untyped; `revoke(ut)` deletes it (`destroy_aspace` is a no-op) and
  `UntypedReset` resets the watermark to 0, reclaiming *all* allocated bytes — the original
  pool and the extension together — because the extension is below the watermark and within
  `[base, base+size)`. No new teardown code; the abutment carve keeps the property true by
  construction.

The abutment equality is what makes the extension a no-gap continuation of the single-base
pool (`pool_index_spec`'s affine map stays valid) **and** forces the untyped's free pointer
page-aligned, so `carve_place` rounds nothing and `c.start == pool_end` exactly — the region
`grow_pool` then zeroes (`[pool_base + old_len*PAGE, …)`) is precisely the carved region.

## Finding 2 — `map_grow`'s retry is safe because `map_frame` frames the slot on `NeedMemory`

`map_grow` does `map → (ERR_NOMEM) → aspace_topup → map`. The retry would be a bug if the
first `map` had already recorded the mapping on the frame cap (the second call would then hit
`ERR_STATE` via the `mapping.is_some()` guard). It does not: `kcore::cspace::map_frame`
records `mapping: Some((asp, va))` **only on the `Ok` path**, and its `Err` postcondition is
`final(store).slot_view() == old(store).slot_view()` — the frame cap is left `mapping: None`.
So a pool-exhaustion failure is genuinely retryable. (B10A independently confirmed the kcore
side: a `map_in` that fails for lack of pool allocates nothing, `pool_used` unchanged.)

## Finding 3 — a decode *arm* is not a verified *item*; the gate count is flat

The plan anticipated "`> 381`" for B10B and a possible bump to 385. In practice the count
stayed **384**: `decode` is a single verified function, and adding a match arm + a new enum
variant + moving the `ensures` bound re-verifies that one function rather than adding a new
one (unlike B10A's three new `proof fn`s, each +1). The `> 381` floor holds trivially. The
ledger already records 384 from B10A, so B10B needs no baseline edit — B10C's closeout
confirms it.

## Finding 4 — the syscall-path functional exercise is gated on B10's out-of-scope funding convention

The plan's B10B acceptance lists "a synthetic aspace exhausts its pool, tops up, the next map
succeeds (via `map_grow`)." Realizing that *through the syscall path at runtime* requires a
caller holding an untyped **whose free region abuts the target aspace's pool** — the
contiguous-extension funding contract. Arranging that (an init/loader convention dedicating an
untyped per topupable aspace) is explicitly **out of scope for B10** (detail plan, "Out of
scope"). `loader/src/spawn.rs` and the `user/` binaries are unchanged.

So the M-2 *mechanism* is exercised at the level where it is testable without that convention:
B10A's host tests `map_in_grow_pool_continues` (exhaust → grow → map succeeds) and
`map_in_grow_pool_lookup_stable` (already green in `cargo test -p kcore`). B10B adds the ABI
boundary tests (`decode(24)`/`decode(25)`) and rides the QEMU boot-green regression. A true
runtime top-up smoke is **B10C**'s ("a runtime-topped-up server aspace is fully reclaimed at
child teardown"). Recorded so the absence of a runtime `map_grow` exercise in B10B is not read
as a gap.

## Verification facts

- Verus pin unchanged (`doc/guidelines/verus.md`): Verus `0.2026.06.07.cd03505`. The trigger
  notes printed during the run (`cspace.rs:9416`) are pre-existing, not from this change.
- Error mapping (no new errno): `NotUntyped → ERR_TYPE`, `NoMemory → ERR_NOMEM` (no room in
  the untyped), `BadArg → ERR_ARG` (non-abutting untyped, or `pages == 0` / `pages*PAGE`
  overflow). The aspace cap needs `WRITE`; the untyped needs no rights check (as `Sys::Retype`).
- `Sys::Map`'s `NeedMemory → ERR_NOMEM` arm is **unchanged** — it is the recoverable condition
  top-up now answers, not a site that B10B edits.
