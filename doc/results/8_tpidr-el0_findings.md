# Findings — Phase 3.1: real TLS via `TPIDR_EL0` save/restore

Task 3.1 of `doc/plans/2_plan-std-revised.md` — the one genuine kernel-track change of the
threading phase, and the forced prerequisite for 3.2 (spawn/join) and 3.5 (TLS key table).
Before this, the kernel saved/restored only `{x[0..31], sp_el0, elr, spsr}` on an EL0 trap;
`TPIDR_EL0` (the AArch64 EL0 thread pointer / TLS base, RW at EL0) was touched **nowhere** in
`kernel/`/`kcore/` (grep empty), so it was a single hardware register shared by every thread —
after thread B writes it, thread A reads B's value, corrupting per-thread TLS the moment two
threads coexist. This makes `TPIDR_EL0` survive a context switch: it grows `kcore::thread::
TrapFrame` by a `tpidr` field, teaches the exception save/restore asm to spill/reload the
register through that slot uniformly on every EL0 entry/exit, seeds it in `enter_first_thread`,
and pins the hand-coded asm byte offsets to the struct with a compile-time assertion.

**Trusted asm shell (rev2§6.1(d)), no new seam.** The whole change is plain-Rust /
inline-asm outside `verus!{}`; the verified `TcbView` (`kcore/src/cspace.rs`) models no
register frame, so **no `verus!{}` obligation is touched and the kcore Baseline is unchanged**
(measured **407/0** cold, before *and* after). It widens the "asm context switch is inherently
unverifiable" construct already enumerated under the Thread-lifecycle shell seam — the tally
**stays 14**.

## What shipped

- **`kcore/src/thread.rs`** (the frame layout, plain Rust before the file's first `verus!{}`):
  - `TrapFrame` gains `pub tpidr: u64` (byte offset 272) after `spsr`, followed by a private
    `_pad: u64` — `#[repr(C)]` + all-`u64` makes the struct **288 bytes** (`31*8 + 5*8`), and
    the pad keeps it a multiple of 16 so the exception entry's `sub sp` stays SP-aligned (280
    alone is not 16-aligned). `TrapFrame::zeroed()` initializes both to 0.
  - A compile-time `const _: () = { assert!(…) }` block pins `size_of == 288` and the four
    offsets the asm hard-codes (`sp_el0 == 248`, `elr == 256`, `spsr == 264`, `tpidr == 272`),
    mirroring the sole existing precedent `urt::time::TimePage` (`urt/src/time.rs:73`). None
    existed before — the 272/248/256/264 literals lived only as bare asm immediates + a doc
    comment, with nothing to catch a struct/asm drift.
- **`kernel/src/exceptions.rs`** (the `global_asm!` EL0 save/restore, raw numeric offsets):
  - `el0_entry`: `sub sp, sp, #272` → `#288`; after the `spsr_el1` save, add
    `mrs x0, tpidr_el0` / `str x0, [sp, #272]` (x0 is free scratch there, the real x0 was
    saved first at `[sp]`).
  - `el0_restore`: after the `spsr_el1` reload and **before** the x0/x1 reload, add
    `ldr x0, [sp, #272]` / `msr tpidr_el0, x0`; `add sp, sp, #272` → `#288`.
  - The layout doc comment updated (272 → 288, tpidr at 272).
- **`kernel/src/main.rs`** — `enter_first_thread` gains `"msr tpidr_el0, {tpidr}"` +
  `tpidr = in(reg) frame.tpidr`, so the very first `eret` into EL0 starts with a defined TLS
  base (0, from the zeroed frame) rather than boot-garbage.
- **`kernel/src/user.rs`** (the m1-test EL0 scaffold — the on-target witness):
  - `get_tpidr()`/`set_tpidr(v)` `#[inline(always)]` helpers (`mrs`/`msr tpidr_el0`,
    `options(nomem, nostack)` — the `urt::time::cntvct` reader style), and two distinct
    nonzero markers `T1_TLS`/`T2_TLS`.
  - `user_main` (T1) sets `T1_TLS` at entry and, after `wait_for(N1, BIT_CAP_PROOF)` — a point
    where T2 provably ran and set its own `tpidr` — re-reads it, aborting `ET!` on mismatch;
    `user_thread2` (T2) sets `T2_TLS` at entry and re-checks after `wait_for(T2_NOTIF, BIT_GO)`,
    aborting `EU!`. T1 emits a new stage marker `8` before `M1 PASS`.
- **`scripts/m1-test.sh`** — success marker `1234567M1 PASS` → `12345678M1 PASS`, plus the
  stage-8 description.
- **`doc/guidelines/verus_trusted-base.md`** — a TPIDR_EL0 routing note (no new
  `external_body`, tally stays 14, kcore Baseline unchanged), and the IRQ-delivery-shell row's
  host-witness marker updated to `12345678M1 PASS`.

## Decisions (and rejected alternatives)

- **Append `tpidr` after `spsr` (offset 272) + a pad word to 288 — not insert mid-frame.**
  Appending leaves every existing GP/sysreg offset (0…264) untouched, so the asm change is two
  `#272→#288` edits plus one new `str`/`ldr` pair. *Rejected:* inserting `tpidr` anywhere below
  the end (e.g. right after `x[30]`) would shift every offset at/above it by 16 on **both** the
  save and restore sides — a far larger, error-prone edit for no benefit.
- **Do the save/restore in the exception asm, uniformly on every EL0 entry/exit — not in
  `maybe_switch`.** The asm is the true register↔frame boundary; `maybe_switch`
  (`kernel/src/thread.rs:155`) only shuffles whole `TrapFrame` structs (`(*cur).frame =
  *frame` / `*frame = (*next).frame`), so once `tpidr` is a field it rides those copies
  automatically, and the asm handles the actual register on the way out. A same-thread syscall
  reloads the identical value (a no-op); a switch reloads the next thread's. *Deferred
  optimization (plan-sanctioned):* because the kernel never touches `TPIDR_EL0` at EL1, only
  the context-switch branch strictly needs the `mrs`/`msr`; restricting it there would thread a
  separate `tpidr` copy through `maybe_switch`'s frame-copy — more complex and more fragile than
  two cheap system-register ops per entry, so the MVP keeps it uniform.
- **Add the compile-time offset assertion.** The asm offsets were coupled to the struct only by
  a comment; a drift silently corrupts `eret`. The `const _` `offset_of`/`size_of` block makes
  any future field reshuffle a compile error. (Confirmed load-bearing by the red experiment
  below — while the *values* there were runtime, the assertion is the static guard for the
  offset literals.)
- **Witness on-target via the m1-test scaffold — not a host test, not a new user binary.** See
  the two dedicated sections below.
- **Do not rewrite the ledger's kcore Baseline count.** The measured cold count is **407/0**,
  but the ledger's kcore Baseline row still reads **406** (its narrative only accounts for the
  `404 → 406` history; the `+1` to 407 — attributed by the plan to task 2.3's added Verus item —
  was never recorded there). This change is count-neutral, so per the plan it does **not**
  silently rewrite that number; the new routing note claims "unchanged" without pinning a
  figure that would contradict the stale row. See Follow-ups.

## Why the "host test" in the plan is impossible (and the chosen witness)

The plan's gate line names a "host test: 2 threads share an aspace, read distinct TLS markers."
That is **not achievable as a `cargo test`**, and the impossibility is structural, not a
shortcut: the verified `Store`/`TcbView` seam models **no register frame** — its TCB accessors
are `state`/`qnext`/`wait_notif`/`report`/`priority`/`bind_*`/`cspace`/`aspace` plus
`set_tcb_retval` (the woken word), with no `tcb_frame`, no `TrapFrame`, no `tpidr` — and the
host test model `TcbState` has no `frame` field. `TPIDR_EL0` is hardware register state touched
only by the trusted asm shell (`exceptions.rs`) and `maybe_switch`/`enter_first_thread`, none of
which executes under a host interpreter. So the property is only observable **on-target under
QEMU**. This is disclosed here rather than worked around.

**The witness is the m1-test EL0 scaffold** (`kernel/src/user.rs`, `cargo build --features
m1-test`, `scripts/m1-test.sh`). It already runs two EL0 threads — `user_main` (T1) and
`user_thread2` (T2) — **sharing one address space** (T2 is started via `thread_start`, opcode
13, which leaves `aspace = None` → the boot identity map), coordinating via notifications that
force real context switches. `tpidr` save/restore is per-thread and independent of *how* the
aspace is shared (it happens in `el0_entry`/`el0_restore` regardless of whether the shared
aspace came from the identity map or a later `thread_start_as`), so these two same-aspace
threads are a valid and sufficient witness — with **zero** new cap plumbing, no new binary, and
no `build.rs`/`init` change. Each thread writes a distinct `TPIDR_EL0` and, after a handoff
during which the other thread provably ran and set a *different* value, re-reads its own; a
mismatch aborts with a tagged error marker the harness fails on.

## Problems hit and how they were solved

- **Red/green confidence — a controlled break to prove the test isn't vacuous.** With the full
  change the run is green (`12345678M1 PASS`); to prove stage 8 actually catches the bug, the
  restore-side `msr tpidr_el0, x0` was temporarily removed and the run rebuilt: the log showed
  `1ET!` — T1 printed its alive marker `1`, then read T2's `TPIDR_EL0` value and aborted its TLS
  check (`ET!`), so `m1-test.sh` reported **M1 TEST FAIL**. The `msr` was then restored and the
  run went green again. This confirms the stage is a genuine red→green gate, not a no-op.
- **`cargo fmt` mis-indented a standalone comment.** rustfmt aligned the two-line `//` comment
  I placed immediately below `putc(b'1'); // marker…` to that line's *trailing-comment* column
  (a known rustfmt quirk: a standalone comment right after a statement-with-trailing-comment
  gets pulled to the trailing column). Fixed by inserting a blank line between them (matching the
  file's existing logical-group spacing); `cargo fmt --check` is then clean. No semantic change.
- **The `mrs`/`msr` cannot be optimized away.** `get_tpidr`/`set_tpidr` are opaque inline asm
  (no `pure`), and the compiler has no model of `TPIDR_EL0`, so the read genuinely reads the
  register and the `!= marker` comparison stands — no risk of constant-folding the check to
  `true`. Distinct nonzero markers (`0x1111…`/`0x2222…`) also make the check catch a
  stuck-at-zero restore, not just a swap.

## Verification record

Toolchain `nightly-2026-06-26` (== `vendor/rust` `bd08c9e7…`) for the cross-build; Verus binary
`0.2026.06.07.cd03505`, toolchain `1.95.0` (confirmed via `verus --version`).

- **Verus (authoritative, cold)** — `cargo clean -p kcore && cargo verus verify -p kcore` →
  **407 verified, 0 errors**, both on the pre-change tree (baseline) and after this change:
  count-neutral, as expected for a plain-Rust `TrapFrame` field + a `const` assertion (no
  `verus!{}` obligation).
- **On-target (QEMU) — the gate** — `scripts/m1-test.sh` → **`12345678M1 PASS`** (no `E<tag>!`,
  no `M1 FAIL`, no `PANIC`), machine `virt,gic-version=3 -cpu cortex-a72`. Baseline before the
  change was `1234567M1 PASS`. The red experiment above produced `1ET!` → **M1 TEST FAIL** with
  the restore disabled.
- **Kernel builds** — `cd kernel && cargo build` (no_std boot path) clean; `cargo build
  --features m1-test` clean (both exercise the const-assert, which would fail compilation on an
  offset drift).
- **Formatting** — `cargo fmt --check` clean (root workspace covers `kcore`/`kernel`;
  `kernel/src/user.rs` is a kernel-crate file, not a `user/*` mini-workspace). `scripts/
  verusfmt.sh --check` clean (my `thread.rs` edit is outside the `verus!{}` blocks, so the macro
  interiors are byte-identical to base).

## Surface left trusted (and why it could not be verified)

- **The `el0_entry`/`el0_restore` `tpidr` spill/reload and `enter_first_thread`'s `msr`** are
  inline asm marshalling saved EL0 register state — the exact rev2§6.1(d) "asm context switch is
  inherently unverifiable" construct the Thread-lifecycle shell seam already covers. No new seam;
  the compile-time offset assertion is the static guard, and the m1-test stage-8 the runtime
  witness.
- **`TrapFrame`'s `tpidr`/`_pad` fields** are `#[repr(C)]` plain Rust outside `verus!{}`; the
  verified `TcbView` models no register frame, so there is nothing for Verus to say about the
  register layout. Correctness rests on the offset assertion + the on-target witness.
- **Standing constraint (unchanged, restated):** the frame is GP-only (softfloat EL0 — the
  `TrapFrame` saves no `q0–q31`/`fpsr`/`fpcr`). Hardware FP/NEON in EL0 stays off the table;
  growing the frame to save the V-register file is its own future phase, **not** part of this
  `tpidr` bump.

## Follow-ups

- **kcore Baseline row is stale (406 vs measured 407).** `doc/guidelines/verus_trusted-base.md`'s
  kcore Baseline row reads `406`, but a cold `cargo verus verify -p kcore` reports `407/0`
  (verified here on the pre-change tree, so not introduced by 3.1). The `+1` predates this task
  (plan-attributed to 2.3). Reconciling the row — and identifying the 407th item so its narrative
  stays complete — belongs to whoever owns that `+1` (a ledger cleanup / task 6.3), not to this
  count-neutral change. Flagged, not silently rewritten.
- **Switch-only save/restore optimization** (deferred): restrict the `mrs`/`msr` to
  `maybe_switch`'s context-switch branch once the extra frame-copy plumbing is judged worthwhile.
- **3.2 will exercise `tpidr` through `thread_start_as`** with an explicitly-retyped shared
  aspace (the std threading path); the mechanism verified here is identical, so no further kernel
  change is expected — 3.2 adds the userspace in-process thread primitive + the heap lock over it.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
