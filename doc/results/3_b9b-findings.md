# B9B — Preemptible revoke: the `EAGAIN` syscall surface + userspace retry loop (findings)

Working notes from the implementation of **Phase B9B** (`doc/plans/9_b9-detail.md`,
sub-phase B9B — the trusted shell deliverable, rev1§6.1(d)). Records what landed, the
findings that diverged from / extended the plan, and the verification tooling facts worth
keeping. Closes the **latency-bound** half of audit **M-1** (revoke now bounds interrupt
latency to one quantum); conforms rev1§5.4 (preemptive scheduler).

This is the **one** sub-phase that changes observable behaviour (honesty note 1): the
`CapRevoke` opcode/args/decode are unchanged, only its return contract (now can return
`EAGAIN`) and the callers adapt. The EL1 IRQ-masking / exception-entry model is untouched
(honesty note 2): preemption is delivered at the syscall boundary by the bounded quantum +
the existing EL0-unmasked retry path, not by unmasking mid-walk.

---

## 0. Headline

All B9B gates green, no kcore/Verus change (B9B is shell-only):

- `cd kernel && cargo build` (real boot, aarch64-none-softfloat) — green; also cross-builds
  the `shell`/user binaries via `kernel/build.rs`, linking the new `cap_revoke_all`.
- `cargo build --features m1-test` and `cargo build -p ipc` — green.
- `scripts/m1-test.sh` → `123456M1 PASS` (the real in-QEMU revoke exerciser — see §1).
- `scripts/boot-test.sh` → `BOOT TEST PASS` (init→storaged→shell, snap/snaps/date).
- **EAGAIN-drain proof:** the same `scripts/m1-test.sh` re-run with a transient
  `REVOKE_QUANTUM = 1` still reaches `123456M1 PASS` — every revoke deletes one leaf and
  returns `EAGAIN`, so the multi-call drain (and revoke-through-queue + cspace teardown) is
  exercised across many bounded steps, not just compiled.

## What landed

- `kernel/src/syscall.rs` — `pub const ERR_AGAIN: i64 = -12;` and the shell-policy
  `pub const REVOKE_QUANTUM: usize = 16;`. `Sys::CapRevoke` now calls
  `cspace::revoke_step(s, REVOKE_QUANTUM)` and maps `Done → 0`, `More → ERR_AGAIN`.
  `Sys::CapCopy` pre-checks the revoke guard (see §3) and returns `ERR_AGAIN` on a hit.
- `kernel/src/cspace.rs` — the `revoke` wrapper became `revoke_step(slot, budget) ->
  RevokeStatus`. The unbounded `kcore::cspace::revoke` is left in kcore but no longer driven
  from the kernel.
- `ipc/src/sys.rs` — `ERR_AGAIN` (lockstep with the kernel block) and a
  `cap_revoke_all(slot)` convenience that loops `while cap_revoke(slot) == ERR_AGAIN`,
  `yield_now()`-ing between tries (the existing `chan_send`-retry idiom).
- `user/shell/src/main.rs` — `Spawner::scrub` calls `cap_revoke_all(DONATION)`.
- `kernel/src/user.rs` — the M1 test's own `cap_revoke` loops on `EAGAIN` (see §1).

---

## 1. Finding: the M1 embedded test is a second `cap_revoke` caller — and the *real* revoke gate

The plan stated "grep found only the shell" caller of `cap_revoke`. That grep matched
`sys::cap_revoke`; it missed **`kernel/src/user.rs`**, the embedded EL0 M1 test
(`#[cfg(feature = "m1-test")]`, entered as the initial thread on the test path,
`main.rs:180`). It has its own freestanding `cap_revoke(slot: u64) -> i64` driven via
`check(cap_revoke(N1), b'q')` and `check(cap_revoke(UA), b'M')`, where `check()` **exits on
any `r < 0`** — so a raw `EAGAIN` return would fail the test.

Worse for the plan's stated gate: the shell's `cap_revoke(DONATION)` lives in the
abort-only `Spawner::scrub` path and is documented as a no-op on a childless untyped, so the
**happy-path boot never drives revoke**. The honesty-note-1 gate ("the shell's
`cap_revoke(DONATION)` still fully revokes") therefore does not actually exercise the new
return contract in a green boot. The thing that *does* is `scripts/m1-test.sh`, which walks
caps/CDT, revoke-through-queue + receiver-cspace teardown, and rev1§3.3 channel
whole-object teardown, asserting exactly `123456M1 PASS`.

**Resolution (a touch beyond the plan's listed files):** `kernel/src/user.rs`'s `cap_revoke`
now loops on `EAGAIN` internally, so all `check(...)` sites see only a terminal status. This
makes the M1 test correct regardless of `REVOKE_QUANTUM` (verified by the
`REVOKE_QUANTUM = 1` run, where every revoke takes the `EAGAIN` path).

**Takeaway:** when a syscall's return contract changes, grep the *kernel* for freestanding
EL0 callers (`kernel/src/user.rs` is a hand-written EL0 blob with its own syscall wrappers),
not just the `ipc`/`user` crates.

## 2. Finding: `REVOKE_QUANTUM` distinguishes "compiles" from "the loop works"

With a production quantum (16) the M1 test's revoke subtrees (a handful of caps each)
complete in one `revoke_step`, so `EAGAIN` never fires and the new retry loops are never
*entered* — the test passes without exercising them. To get genuine end-to-end coverage,
re-run `scripts/m1-test.sh` with a transient `REVOKE_QUANTUM = 1`: each `revoke_step` deletes
one leaf and returns `More → EAGAIN`, forcing the kernel→userspace loop to drain a
multi-call revoke. The test still reaches `123456M1 PASS`, which also confirms the
intermediate (partially-revoked) CDT states are well-formed across many preemption points —
exactly the safety the B9C TLA model checks formally. Restore `16` afterward. A permanent
small quantum was considered as a standing regression guard but rejected as off-policy
(more syscall round-trips); the transient run + this note is the chosen substitute.

## 3. Finding: `derive`'s bare `Err(())` can't carry the guard refusal — pre-check instead

B9A's `derive` guard refuses derivation into a revoking subtree by returning `Err(())` — the
*same* value as a structural failure (no free slot, etc.), which `CapCopy` already maps to
`ERR_NOSLOT`. So the handler cannot tell "retry later" (`EAGAIN`) from "permanently failed"
(`ERR_NOSLOT`) by the return value alone. Rather than widen the verified `derive` signature
(a B9A/kcore change, out of B9B scope), the `CapCopy` handler **pre-checks** the public,
read-only `kcore::cspace::ancestor_or_self_revoking(&KernelStore, src)` and returns
`ERR_AGAIN` before calling `derive`. Single-core + masked at EL1 means the pre-check and the
`derive` run atomically, so there is no TOCTOU window. `derive`'s internal guard remains as
verified defense-in-depth (it just never fires on this path, since the pre-check intercepts
first).

## 4. Smaller notes

- **No `Sys::CapMint` handler exists yet.** The plan mentions wiring the guard errno in
  "`CapCopy`/`CapMint`", but the dispatch (`kernel/src/syscall.rs`) has no `CapMint` arm —
  only `CapCopy` calls `derive`, so only it needed the pre-check.
- **The `revoke` wrapper was replaced, not duplicated.** Only `syscall.rs` called the kernel
  `cspace::revoke` wrapper, so it became `revoke_step` outright. `kernel::cspace` still
  glob-re-exports the unbounded `kcore::cspace::revoke`; nothing in the kernel calls it now.
- **Tooling: the two QEMU gates.** `scripts/m1-test.sh` (cap mechanism / revoke, the M1 exit
  criterion) and `scripts/boot-test.sh` (real init→storaged→shell). Both self-terminate
  (internal deadlines + kill-on-exit), so they need no external `timeout` wrapper — useful on
  macOS where `timeout` is absent (`gtimeout` only via coreutils).

## Out of scope (unchanged, per plan)

No `exceptions.rs` / IRQ-masking / `maybe_switch` change; no kcore/Verus change (B9A done);
no TLA change (B9C); no `REVOKE_QUANTUM` tuning beyond the safe default.
