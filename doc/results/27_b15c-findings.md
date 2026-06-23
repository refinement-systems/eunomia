# B15C findings — storaged/init/selftest startup-block parsing host tests

**Phase:** B15C of `doc/plans/16_b15-detail.md` (baseline test backfill). The hand-rolled
**SD02/SH01/ST01** startup blocks (rev1§5.1) are decoders of an untrusted-shaped message
(rev1§2.7): a short or garbage block must be **refused, never panic** (a panic in `_start`
is a boot failure). B15C lifts the parse/construct logic behind a thin `#[cfg(not(test))]`
seam and host-tests the round-trip + the refuse-not-crash floor — pinning current behaviour
at the boundary where **Phase C1** will replace these blocks with the named-grant table.
Closes (with B15A/B15B) the `user/*` half of the audit §4.2 gap
(`doc/results/0_audit_rev0.md:514-517`). **Test-only**: no on-disk byte change, no wire
change, no public type any other crate consumes, no spec edit, no Verus/TLA/seam touched
(these binaries are rev1§6 Baseline tier, not proof-boundary seams; tally stays **14**).

## Scope — B15C does NOT touch the shell crate

The plan's B15C text mentions an SH01 round-trip against the *shell's* parse, but B15B
(shell-only, developed in parallel) gates the shell's `_start` — exactly where that parse
lives (`user/shell/src/main.rs:743-748`). Editing the shell from B15C would collide with
B15B, and the plan itself routes B15B/B15C to **different crates** so they "do not
serialize" (`16_b15-detail.md:598-602`). Resolution (confirmed with the user): B15C touches
only **storaged, init, selftest**. The SH01 format is pinned from the **producer** side
(init's `build_sh01` + a round-trip through a local parser mirroring the shell's
`blen >= 12 && b"SH01"` rule); the real shell-side SH01 *consumer* parse stays B15B's domain
/ the QEMU boot gate.

## Pre-implementation findings (from exploration)

1. **The bare-metal items block a host `cargo test`; `#[cfg(not(test))]` gating is the fix
   (Design decision 2).** Each binary is `#![no_std] #![no_main]` and only links for aarch64.
   Under host std a second `#[panic_handler]` / `#[global_allocator]`, a `#[no_mangle] _start`
   (symbol clash with the C runtime), and init's `include_bytes!(env!("STORAGED_ELF_PATH"))`
   (env unset off the kernel build → `env!` compile error) all hard-error. Gating each behind
   `#[cfg(not(test))]` + `#![cfg_attr(not(test), no_std/no_main)]` lets the crate build as a
   normal host harness while the **aarch64 build stays cfg(not(test)) → behaviour-identical**.

2. **Two host-absence traps that force gating beyond the obvious (confirmed by build):**
   - `urt::time::cntvct`/`cntfrq`/`now_utc_ns` are `#[cfg(all(target_arch="aarch64",
     target_os="none"))]` — they **don't exist on host**, so any caller (init's
     `read_boot_utc`, storaged's `now_utc`) must be **hard-gated** `#[cfg(not(test))]`, not
     merely `allow(dead_code)`. (`urt::time::attach` *is* host-available — gated only on
     loom/shuttle.)
   - `loader::spawn` is itself `#[cfg(target_os="none")]`, so even `use loader::spawn;` fails
     to resolve on host → the import is gated `#[cfg(not(test))]` alongside its `_start` user.

3. **The real blocker was the build scripts, not the source.** Each `user/*/build.rs` emits
   `cargo:rustc-link-arg=-T…/link.ld` + `-zmax-page-size=4096` for **all** targets, so the
   host libtest harness got the bare-metal linker script and `cc` failed
   (`unknown argument: '-zmax-page-size=4096'`). Fix: gate the args on the bare-metal target
   (`TARGET.contains("-none")`) and scope them to bin targets (`rustc-link-arg-bins`). The
   cross-build (`TARGET=aarch64-unknown-none-softfloat`) still receives them; host test builds
   don't. `cargo test` on these bins links **only** the test harness (no `tests/` dir, so the
   non-test bin is never linked on host).

4. **proptest house idiom.** `[dev-dependencies] proptest = "1"` + `#[cfg(test)] mod tests`
   in `src/main.rs` with `cases: if cfg!(miri) { 4 } else { 256 }` (precedent:
   `cas`/`urt`/B15A). Followed here; dev-deps don't reach the cross-build.

## What landed

Each binary keeps its exact wire bytes; the inline logic is extracted to a pure fn the
boot path now calls, and a `#[cfg(test)] mod tests` exercises it host-side.

- **storaged (`user/storaged/src/main.rs`)** — `fn parse_config(&[u8]) -> Option<Config>`
  (the SD02 consumer; the `len < 44 || != b"SD02"` guard + five LE fields). `_start` becomes
  `let Some(cfg) = parse_config(&buf[..len]) else { fail(..) }`. Tests: round-trip, reject
  (empty / 43-byte / wrong-magic → `None`), and proptests — totality over arbitrary bytes
  (never panics) + any `b"SD02"`-prefixed ≥44-byte buffer parses, trailing bytes ignored.
- **init (`user/init/src/main.rs`)** — `build_sd02(..) -> [u8;44]`, `build_sh01(..) -> [u8;12]`
  (the SD02/SH01 producers), and `rtc_sane(secs, cntfrq) -> bool` (the inverse of the `:87`
  insanity check; `read_boot_utc` now does `if !rtc_sane(..)`). Tests: golden byte layout,
  round-trip through local mirror-parsers of storaged's / the shell's rules (proptest over
  arbitrary fields), and `rtc_sane` cases around the 2020-01-01 threshold + `cntfrq == 0`.
- **selftest (`user/selftest/src/main.rs`)** — `parse_st01(&[u8]) -> St01 { mode, time_va }`
  (`len >= 5` guards magic+mode → mode `0` otherwise; `len >= 13` guards the optional time
  VA). `_start` destructures it then does the gated `urt::time::attach`. Tests: full / 5-byte
  / short / wrong-magic blocks, totality proptest, full-block round-trip.
- **Manifests/build scripts** — `[dev-dependencies] proptest = "1"` and the target-gated
  `build.rs` on all three; their per-workspace `Cargo.lock`s regenerated.

## Verification

| Check | Result |
|---|---|
| `cargo test --manifest-path user/storaged/Cargo.toml` | **green** — 4 tests (round-trip, reject, totality, well-formed) |
| `cargo test --manifest-path user/init/Cargo.toml` | **green** — 6 tests (golden, round-trips, rtc_sane, 2 proptests) |
| `cargo test --manifest-path user/selftest/Cargo.toml` | **green** — 5 tests (block variants, totality, round-trip) |
| `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test --manifest-path user/selftest/Cargo.toml` | **5 pass, no UB** (validates the `cfg!(miri)` idiom; pure decoders, `unsafe`-free) |
| `cd kernel && cargo build` (aarch64) | links every `user/*` binary (only pre-existing kcore warnings) |
| direct cross-build of storaged/selftest/init (`*-none`, build-std) | all link with `link.ld` (the target-gated build.rs) |
| `scripts/run-demo.sh` (QEMU boot smoke, timeout harness) | green — `[init] system up` → `[storaged] store mounted` → `serving`; `write`/`sync`/`cat`/`ls`/`df` all echo (`cat docs/smoke` → `hello`); no panic/`Corrupt` |

The boot smoke is the behaviour-preserving witness: init builds the blocks (`build_sd02`/
`build_sh01`), storaged decodes (`parse_config`), and the full stack mounts and serves
exactly as before.

## Host-tested vs QEMU-boot-gated (the rev1§6.1 honesty split, Design decision 3)

- **Host-tested (proptest/unit, Miri-replayable):** the SD02 decode (storaged) + builder
  (init), the SH01 builder (init) + format pin, the ST01 decode (selftest), the RTC sanity
  rule (init) — all *syscall-independent* logic.
- **QEMU-boot-gated, by design:** every `sys::*` interaction — init's wiring/spawn, storaged's
  mount + serve loop + virtio-MMIO probe, the shell-side SH01 *consumer* parse and its IPC,
  selftest's fault/panic/bss-leak modes, `hello`. These rest on `scripts/run-demo.sh`.

Recorded here (matching B15A's precedent) rather than in the trusted-base ledger: B15A added
no ledger row for `mkfs`, and B15B is in flight against the same file, so B15C leaves
`verus_trusted-base.md` untouched to avoid shared-file churn. No seam, no Verus/TLA gate, no
spec/CLAUDE.md change.

## Out of scope (recorded, not gaps)

- **The shell crate** — B15B's domain (date math/parsers/prune + the SH01 consumer parse).
- **`loader::prepare` page-rounding** — Phase B3, not B15.
- **The named-grant startup format** that replaces SD02/SH01/ST01 — Phase C1; B15C's tests
  pin *current* behaviour at that boundary.
- **Host-mocking the syscall I/O** — out of scope for a [low] backfill (would test the mock,
  not the kernel); kept on the QEMU boot gate.
