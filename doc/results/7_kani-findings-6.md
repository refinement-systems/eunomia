# Kani verification findings — part 6 (§4.6 syscall decode + the §2.5 split)

Continuation of `doc/results/2_kani-findings.md` (§4.1) through
`6_kani-findings_6.md` (§4.5) for the syscall-ABI suite (plan
`doc/plans/0_kani-rewrite.md` §4.6). Harnesses live in
`kcore/src/proofs/sysabi.rs` under `#[cfg(kani)]` and run via `cargo kani -p
kcore` (CI job `kani`, pinned cargo-kani **0.67.0**). The standing caveat, the
bounds policy, and the design notes (DN-1…DN-7) of the earlier parts apply
unchanged; only what is *new* to §4.6 is recorded here.

§4.6 is the **§2.5 syscall split**: `kernel/src/syscall.rs` (633 lines) mixed
register decoding, argument validation, capability lookup, and execution in one
`match nr`. The pure decode + validation moved into a host-buildable
`kcore::sysabi` layer — `decode(nr, a) -> Result<Sys, SysError>` — and the
kernel `dispatch` is now `decode(...)` then `execute(sys, frame)`, where
`execute` is the old per-arm body with argument extraction removed. The outcome:
"no user-controlled value reaches kernel arithmetic unvalidated" and "an unknown
opcode yields an error, never a crash" (spec §3.7) become Kani-checked.

## Standing caveat (unchanged)

**Every result here is bounded** — except, in spirit, the decode harnesses:
`decode` is pure `u64` arithmetic with no pointers and no loops, so
`check_decode_total` runs over *fully nondeterministic* `(nr, a) : u64⁷` with no
unwinding — within the 64-bit input domain this is genuinely exhaustive (like
`check_carve_no_overflow`). The one slot-bound check uses the TLC-scale
`CSpacePool` (`CS_SLOTS = 4`).

## What §4.6 verifies

| Harness | Property |
|---|---|
| `check_decode_total` | `decode(nr, a)` never panics/overflows for **any** `(nr, a)`; a known `nr` (0..=23) never reports `UnknownCall`, and any other `nr` always does — unknown opcode ⇒ error, never UB (spec §3.7) |
| `check_validate_lengths` | every value `decode` validates holds on `Ok`: `ChanSend.len <= MSG_PAYLOAD` (so `channel::send`'s `as u16` is lossless), `event <= 2`, `which <= 1`, `prio < NUM_PRIOS`, `Retype.ty` round-trips through `ObjType::from_u64`; plus `ObjType::from_u64` totality (`Some` iff code `< 8`) and `CSpaceObj::slot(cs, i)` null **iff** `i >= num_slots` (the "slot index < cspace size before use" guard behind `cur_slot`) |

Both verify. No defects found — this phase makes existing validations *checkable*
rather than hunting a bug; the QEMU suite confirms the split is behaviour-preserving.

## Design / engineering notes new to §4.6

- **DN-8 — the ABI uses six argument registers, not seven.** The plan text
  writes `decode(nr, args: [u64; 7])` / "u64⁸"; the real trap-frame read is
  `x7 = nr`, `x0..x5 = a[0..6]` (`x6` is unused). `decode` therefore takes
  `[u64; 6]`, and `check_decode_total` ranges over `nr` + 6 args. The totality
  property is identical regardless of count.

- **DN-9 — decode-first changes error *precedence* for multiply-malformed
  requests (benign).** The old dispatch validated per arm in a fixed order
  (slot → type → rights → arg shape); `decode` now runs all the *pure* checks
  up front. So a request that is wrong in two ways at once — e.g. a `chan_send`
  with both a bad channel slot and an over-`MSG_PAYLOAD` length — now returns
  `ERR_ARG` (from decode) where it previously returned `ERR_BADSLOT` (from the
  slot lookup). The spec pins no error precedence, every such input still
  errors, and no test (nor any well-formed caller) exercises it; recorded here
  rather than papered over. Every decode-time `SysError` maps to `ERR_ARG` —
  the exact code each condition already returned — so single-fault behaviour is
  unchanged.

- **What stayed in `execute` (and why).** Validation that needs *live state*
  is not pure decode and remains kernel-side: all capability lookups
  (`cur_slot`), rights checks, the `user_range_ok`/`range_mapped` user-pointer
  walks (they need the current thread's aspace), the priority *ceiling* check
  (`prio > caller.priority`, `ERR_PERM`), and `frame_write`'s
  `off + len <= pages*4096` bound — the last is coupled to the frame cap's
  `pages` and is already panic-safe (`checked_add`), so moving only half of it
  into decode would add a precedence change for no safety gain. `NUM_PRIOS`
  moved to `kcore::sysabi` (one source for the decoder's range check and the
  kernel's ready-queue array; `kernel::thread` re-exports it).

## Findings

None. The decode layer is total and the validations are exactly the pre-split
checks, now mechanized; the §2.5 goal ("no user-controlled value reaches kernel
arithmetic unvalidated") is a checked property.

| ID | Date | Harness | Bounds | Severity | Description | Status |
|----|------|---------|--------|----------|-------------|--------|
| —  | —    | —       | —      | —        | (no defects found) | — |

## QEMU regression gate (behaviour preservation)

The syscall path is the kernel's central surface, so the behavioural gate is
load-bearing. All three suites pass on the split (run locally):
`m1-test.sh` → `M1 TEST PASS` (retype, cap copy/delete/revoke, channels,
notifications, timer, thread bind/exit/read_report); `spawn-test.sh` → `SPAWN
TEST PASS` (the 100× spawn/reclaim loop drives retype, cap_install,
thread_start_as, map, frame_write, untyped_reset); `boot-test.sh` → `BOOT TEST
PASS` (the storage demo: chan_send/recv + the time page).

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Measured on the dev machine (cargo-kani 0.67.0).

| Harness | Bounds | Time |
|---------|--------|------|
| `check_decode_total` | none (all `u64`) | ~1.6 s |
| `check_validate_lengths` | all `u64` + a 4-slot `CSpacePool` | ~1.5 s |
