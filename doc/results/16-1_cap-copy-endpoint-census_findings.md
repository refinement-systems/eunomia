# Findings 16-1 — the `cap_copy`/`derive` endpoint-census fix + console widening

The kernel-track follow-up flagged in `16_console-stdio_findings.md`: `cap_copy`
(kernel `derive`) now maintains the rev2§3.3 per-endpoint capability census
(`Channel::end_caps`), closing a latent correctness bug, and re-establishing the
`end_caps_sound` invariant it previously violated at runtime. With the kernel fixed,
the userspace containment shipped by finding #16 is removed: the shell now donates its
console endpoint to **every** child (true foreground-terminal inheritance), and the
QEMU gate witnesses a console child being reaped while the shell keeps serving.

This supersedes finding #16's containment (the `CONSOLE_CAPABLE = [bin/stdio]`
allowlist and the "run bin/stdio last" smoke arrangement); it does not rewrite that
historical report.

## The bug (recap from finding #16)

The per-endpoint cap census `Channel::end_caps[2]` (rev2§3.3) fires `EV_PEER_CLOSED`
when the *last* cap of an end is deleted. It is maintained by
`kcore::channel::endpoint_cap_added`/`_dropped`. `endpoint_cap_added` was called
**only** from the retype path (`kcore/src/untyped.rs`), **never** from `cap_copy`
(`kernel/src/syscall.rs::Sys::CapCopy` → `cspace::derive`). `derive` produced a second
`Channel(ch, end)` cap and bumped the *object* refcount (`obj_refs`), but left
`end_caps[end]` unchanged — so a copy under-counted the census. A later
donate→reap cycle then net-decremented `end_caps` below the true live-cap count,
spuriously firing peer-closed against a *still-live* end. Symptom: after the shell
donated its console to a child and the child reaped, the console driver's next
`chan_send` to the shell returned `ERR_CLOSED` and the shell's console input died.

## What shipped

### Kernel (verified) — `cspace::derive` maintains the census

1. **`kcore/src/cspace.rs` `derive`.** After installing the copied slot + bumping the
   object refcount + `cdt_insert_child`, when the derived cap is `CapKind::Channel(o,
   e)` it now calls `crate::channel::endpoint_cap_added(store, o, e)` — the mirror of
   the `delete` path's `endpoint_cap_dropped`, already used by retype. A channel copy
   thus bumps `end_caps[end]` in lockstep with the arena's new end cap.
2. **Overflow guard.** `endpoint_cap_added` requires `end_caps[end] < u32::MAX`. A
   read-only guard mirroring the existing `obj_refs == u32::MAX → Err` refusal
   (`if store.chan_end_caps(o, ei) == u32::MAX { return Err(()) }`) discharges it and
   keeps the `Err` path store-unchanged.
3. **Contract.** `derive` was the outlier that did not thread the census invariants
   (`delete` requires+ensures both `caps_consistent` and `end_caps_sound`). It now
   `requires end_caps_sound(old)` plus the projected src fact `Channel src ⇒
   chan_wf(o)` (residency + `end_caps.len() == 2`, the two remaining
   `endpoint_cap_added` preconditions), and `ensures end_caps_sound(final)`. The
   narrow `chan_wf` projection is used in place of the full `caps_consistent`
   quantifier to keep the prover cost down (the only caller is the unverified
   `CapCopy` syscall shell, so the `requires` is discharge-free there; `KernelStore`
   maintains both facts as system invariants).
4. **New lemma.** `cspace::lemma_set_slot_end_cap`, the add-direction dual of the
   existing `lemma_clear_slot_end_cap`: installing a cap into a previously-empty slot
   raises `end_cap_count` by one at the new cap's channel end (`+1`), leaving every
   other filter unchanged. `derive` composes it with `lemma_same_caps_same_end_cap`
   (framing the `obj_ref` + `cdt_insert_child` cap-preserving edits) and
   `endpoint_cap_added`'s `+1` to re-establish `end_caps_sound` everywhere.

No `kernel/src` change: `Sys::CapCopy` is unverified glue that just calls `derive`.

### Kernel host regression test

**`kcore/src/test_store.rs` `derive_channel_endpoint_bumps_census_no_false_peer_closed`.**
Runs the real `derive` on a channel end-A cap and asserts `end_caps` goes `[1,1] →
[2,1]` (the fix — previously left at `[1,1]`), then binds end B's peer-closed to a
live notif and drops one of the two end-A caps (`check_endpoint_cap_dropped`),
asserting `end_caps → [1,1]` **and** the peer notif word stays `0` — the peer is
**not** closed while the original end-A cap lives. This is the "copy a channel end
cap, delete the copy, assert the peer is not closed" case finding #16 called for.

### Userspace — console donation to every child

5. **`user/shell/src/runtime.rs`.** The `CONSOLE_CAPABLE` allowlist and
   `is_console_capable` are removed; the `console_capable` flag is dropped from
   `run_once`/`spawn_inner` and both call sites (`cmd_run`, `cmd_runloop`). The
   donation block is now unconditional (best-effort, as before: a failed copy leaves
   the child on the debug-log fallback rather than failing the spawn). Every child the
   shell runs — foreground, one at a time to completion — inherits the console
   endpoint, so its std `sys/stdio` arm rides `user/console`. The runloop's per-spawn
   copy→reap now additionally exercises the census round-trip.
6. **`user/stdio/src/main.rs`** — the comment naming the removed allowlist is
   generalized to "every child."

**Grant budget (unchanged `MAX_GRANTS = 8`, `loader/src/startup.rs`).** With console
donated to every child: default+console = 4, fs+console = 6, thread+console = 8 (exact
capacity, already guarded by `build_child_block_thread_child_with_console_within_max_grants`).
The only over-budget shape is a child that is *simultaneously* thread- **and**
fs-capable + console = 10, which does **not** exist today (the `THREAD_CAPABLE` /
`FS_CAPABLE` allowlists are disjoint). `MAX_GRANTS` is deliberately left at 8 (raising
it would ripple into the verified `loader` startup codec); a future thread+fs binary
would need it raised to 10 (and a re-check of the 256-byte `MAX_BLOCK`).

### On-target — the smoke test witnesses reap-survival

7. **`scripts/std-smoke-test.sh`.** The "RUN LAST" containment on the `bin/stdio` arm
   is removed (that constraint existed only because reaping a console child wedged the
   shell). A subsequent arm — `run bin/stdsmoke gamma delta`, with a fresh argv marker
   — runs *after* the `bin/stdio` console child reaps; its appearance proves the
   shell's console survived the reap and still delivered the next command. Because
   donation is now unconditional, every arm's child is a console child, so arms 1→2→…
   already cross that boundary; arm 5 is the explicit assertion. `bin/stdsmoke`'s
   `println!` markers now route over the console (still reaching the UART), and all
   prior markers stayed green (see below).

## Verification record

- **Verus (cold, authoritative).** `cargo clean -p kcore && cargo verus verify -p
  kcore` ⇒ **408 verified, 0 errors** (was 407 pre-change; +1 for the new
  `lemma_set_slot_end_cap`). The ledger Baselines row is updated 406→408 (the 406→407
  step was the earlier `ThreadStartAs x6` landing; the row had not been re-synced).
  Trusted-base tally unchanged at **14** — no new `external_body`/seam.
- **Verus proof-perf** (deterministic `rlimit`, cold, byte-identical control). Threading
  the census through `derive` raised its `rlimit` **683 088 → 1 010 917** (+48%) — the
  cost of the new `end_caps_sound(final)` obligation; the new lemma is 30 187 and
  `endpoint_cap_added` (called, not modified) is unchanged at 29 787. `derive` remains
  comfortably within bounds (its module neighbours run 5–24 M). This is a pure
  correctness fix, and per `doc/guidelines/verus.md` §10 correctness outranks checker
  speed; the narrow `chan_wf` precondition already avoids the heavier `caps_consistent`
  quantifier.
- **kcore host tests.** `cargo test -p kcore` ⇒ 113 passed, 0 failed, incl. the new
  `derive_channel_endpoint_bumps_census_no_false_peer_closed`.
- **Shell host tests.** `cargo test --manifest-path user/shell/Cargo.toml` ⇒ 30
  passed, 0 failed, incl. `build_child_block_emits_console_grants` and
  `…_within_max_grants` (now the steady-state thread-child path).
- **Cross-build.** `cd kernel && cargo build` green (runtime.rs cross-compiles; rebuilds
  `bin/stdio`/`stdsmoke`).
- **Formatting.** `cargo fmt --check` clean over the root workspace + `user/shell` and
  `user/stdio`; `scripts/verusfmt.sh --check` clean over the touched `verus!{}` code in
  `kcore/src/cspace.rs` (cspace.rs is not on the verusfmt skip list).
- **QEMU gate.** `scripts/std-smoke-test.sh` ⇒ `STD SMOKE TEST PASS`, with every prior
  marker (`STD2/STD32/STD33/STD34/STD35`, argv, both time arms, the std panic reap) and
  `STD51 PASS` still green, **plus** the new reap-survival witness
  (`[stdsmoke] argv=…gamma…delta` after the `bin/stdio` console child reaped) — the
  end-to-end proof that reaping a console child no longer wedges the shell's console.

## Trusted / unverified surface

No new trusted-base seam; the tally stays **14**. The fix is entirely within the
already-verified `kcore` surface (`derive`'s proof, one new lemma). The userspace
widening is unverified glue as before (the `CapCopy` syscall and the shell donation).

## Follow-ups

- **Concurrent `a | b` pipeline + its stderr/stdin separation → 5.3** (unchanged from
  finding #16; the shell's own stdio moves onto std there too).
- **Power-efficient console I/O** — reactor + timer-bit blocking (unchanged).
- **`MAX_GRANTS` → 10** only if/when a binary becomes both thread- and fs-capable *and*
  console (today none is); it would re-touch the verified `loader` startup codec.
