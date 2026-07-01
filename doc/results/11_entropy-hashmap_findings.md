# Findings — Phase 3.4: entropy seed grant + HashMap

Task 3.4 of `doc/plans/2_plan-std-revised.md` (findings **#11**). Unblocks the last
mandatory std arm that still panicked — `sys/random` (`fill_bytes`/
`hashmap_random_keys`) — so `HashMap`'s `RandomState` works. A new **verified**
startup-block grant carries a 256-bit entropy seed; a per-process **non-cryptographic
MVP DRBG** (`urt::random`) is seeded from it; `init` (the seed-tree root) mixes a
documented-predictable MVP seed from RTC + `CNTVCT` and hands each child a
DRBG-drawn sub-seed; the `shell` relays a fresh sub-seed per child. Proven live:
`run bin/stdsmoke hashmap` → **`STD34 PASS`** (a 1000-entry `HashMap` over the seed
path, `scripts/std-smoke-test.sh`, wired into the CI `on-os` job).

## Decisions (and the alternatives)

- **Fixed 256-bit `GrantKind::Seed([u64;4])`, read via four verified `take_u64`.**
  Chosen over a generic length-prefixed inline-bytes grant. It is the lightest-proof
  shape: the seed is *owned* (nothing borrowed out of the message), so
  `well_formed_startup` needs **no** new `subseq_of` clause and the grant loop needs
  **no** new invariant — only the `pos <= buf@.len()` advance each `take_u64` already
  ensures. It reuses the existing verified readers (no new proof pattern), stays
  `Copy`/lifetime-free (so `Grant`/`Startup`/the derives are unchanged in shape), and
  is cleanly zeroizable. *Rejected:* a generic `InlineBytes { len, bytes: [u8; N] }`
  — it honors the plan's literal "inline-bytes" wording and is reusable, but needs a
  bounded copy-loop proof for zero MVP benefit (the only consumer is a fixed 32-byte
  seed). The user confirmed the fixed shape. Cost: **loader 29 → 30 verified**, one
  added-arm obligation; `decode` `rlimit` 169163 → 177414 (+4.9%), measured against
  the pre-change tree — a proportionate cost for a genuinely-more-total decoder
  (correctness/thoroughness outranks checker speed, `verus.md` §10).
- **No-seed behavior = loud abort at first `fill_bytes`.** The `urt::time::now_utc_ns`
  "time page not attached" precedent: a process that reaches `fill_bytes`/`HashMap`
  without a `NAME_RANDOM_SEED` grant is mis-provisioned, so it aborts visibly rather
  than hashing predictably. *Rejected:* a silent fallback to a locally-mixed seed
  (`CNTVCT` ⊕ a stack address) — it never aborts but makes the misconfiguration
  invisible (silent predictability, the failure the plan warns against). The user
  confirmed loud-abort. Since `RandomState::new` is lazy (only on the first
  default-hasher map), this bites only a binary that actually uses `HashMap` unseeded.
- **The DRBG lives in `urt::random`; the std glue lives in `eunomia-sys` (the 3.3
  split).** `urt::random` holds the generator so all three consumers reach it directly
  — `init` and `shell` (no_std parents drawing sub-seeds) and std (via the bridge).
  `eunomia-sys/src/random.rs` is a thin bridge and `pal.rs` exposes the
  `__eunomia_fill_bytes` seam symbol, exactly as `eunomia-sys::futex` bridges
  `urt::futex`. *Rejected:* the whole DRBG in `eunomia-sys` — the parents need it too,
  and the `SpinLock` it reuses lives in `urt`.
- **xoshiro256\*\*, seeded directly as state, non-crypto, zero-dep.** The plan mandates
  an "explicitly non-cryptographic MVP DRBG"; xoshiro256\*\* is a standard non-crypto
  PRNG needing no external crate (the tree has no ChaCha/blake3 in `urt`/`eunomia-sys`,
  and blake3 lives only in `cas`). `splitmix64` (the finalizer already in
  `cas/src/chunk.rs`) is re-implemented as `expand_seed` for `init` to widen its few
  hardware words into 256 bits. The first `next_u64` returns a *scramble* of the seed
  (computed before the state rolls), never the raw seed bytes — the plan's "never a
  copy of the seed bytes" requirement.
- **Uniform per-child seeding.** `init` seeds all three children (storaged, console,
  shell) and the `shell` seeds every child it spawns — each a fresh `fresh_seed()`
  draw. no_std children that never hash (storaged keys sorted prolly trees, not
  `HashMap`s, rev2§4.9; the console does no hashing) simply carry a grant they ignore.
  Uniform is the disciplined choice and future-proofs; the block-size cost is 34 bytes
  each (all well within `MAX_BLOCK = 256`).
- **Concurrency reuses the certified `SpinLock`; no new model.** `fill_bytes` needs
  only mutual exclusion over the generator state (no wait/wake), so `urt::random`
  wraps an `UnsafeCell<Option<Drbg>>` with the Loom-certified `lock::SpinLock` — the
  exact `urt::Heap` `unsafe impl Sync` posture. No new Loom/Shuttle obligation (unlike
  the 3.3 futex wakeup protocol); Miri is the data-race oracle.

## What shipped

- **Verified decoder (`loader/src/startup.rs`).** `KIND_SEED = 4`,
  `NAME_RANDOM_SEED = 11`, `GrantKind::Seed([u64;4])`, the decode arm (four
  `take_u64`), the encode arm, the wire-format doc, and tests (`seed_grant_golden`,
  `decode_refuses_truncated_seed`, the `grant_kind()` proptest strategy so
  `round_trips` exercises the new kind). loader **30 verified, 0 errors**.
- **DRBG (`urt/src/random.rs`, new).** `Drbg` (xoshiro256\*\* core), `seed`/
  `fill_bytes`/`fresh_seed`/`is_seeded` over a `.bss` `SpinLock`-guarded singleton,
  `expand_seed` (splitmix64), and the loud-abort no-seed guard. `pub mod random`
  gated `not(any(loom, shuttle))` in `lib.rs` (it holds a process-global over the
  *const* `SpinLock::new()` those model builds drop, and has no interleaving model of
  its own). 10 host tests.
- **Bridge + attach (`eunomia-sys`).** `src/random.rs` (delegates to `urt::random`),
  `pub mod random` in `lib.rs`, the `__eunomia_fill_bytes` shim in `pal.rs`, the
  `seed()` resolver + `NAME_RANDOM_SEED` re-export in `grant.rs`, and the seed attach
  (with a volatile zeroize of the transient copy) in `bootstrap.rs::attach_grants`.
- **std PAL arm (`vendor/rust`).** New `sys/random/eunomia.rs` (the
  `__eunomia_fill_bytes` extern + `fill_bytes`); three `sys/random/mod.rs` edits —
  drop eunomia from the `unsupported` group, add a dedicated `mod eunomia; pub use
  eunomia::fill_bytes;` arm, and drop eunomia from the `hashmap_random_keys` exclusion
  so it gets the generic one (which calls `fill_bytes`) — the `motor` (Shape-B) shape.
- **Producers.** `init` seeds its DRBG from RTC + `CNTVCT` + `CNTFRQ` (via
  `expand_seed`) and threads a fresh sub-seed into each `build_*_block`; the `shell`
  resolves its own `NAME_RANDOM_SEED` grant → `urt::random::seed` in `_start` and
  draws `fresh_seed()` per child in `spawn_inner`/`build_child_block`. Both zeroize
  the transient seed copy.
- **Consumer + gate.** `user/stdsmoke` gains a `hashmap` arm (1000-entry
  `HashMap<String,u64>` insert/lookup, `STD34 PASS`); `scripts/std-smoke-test.sh`
  drives it and asserts the marker (and the no-seed-abort message as a fail).
- **Fuzz + spec.** A hand-added `loader/fuzz/corpus/startup/entropy_seed` seed, the
  `startup2_truncated_seed_refused` regression (negative + positive control), and the
  rev2§5.1 named-grant-table addition (`random-seed` + the additive inline-value kind).

## Problems hit and how they were solved

- **`urt::random` under the loom/shuttle model builds.** The module holds a
  process-global `static` over the *const* `SpinLock::new()`, which loom/shuttle drop
  (their `AtomicU32::new` is non-const). It has no interleaving model of its own to
  run there (the lock it reuses is already modeled), so `pub mod random` is gated
  `not(any(loom, shuttle))` — present for the target, plain `cargo test`, Miri, and
  verus; absent (unreferenced) in the model builds. This is a *narrower* gate than
  `lock` (fully portable) and *wider* than `futex` (target-or-model only).
- **Global-state test flakiness.** The DRBG singleton is shared across host tests, so
  a seeded-path test and a `#[should_panic]` no-seed test on the same global would
  race on order. Solved by factoring the no-seed policy into a pure `fill_locked(&mut
  Option<Drbg>, …)` helper (tested with `None`/`Some` directly, no global) and keeping
  the one global-touching test *seed-only* (order-independent), the `bootstrap.rs`
  single-toucher precedent.
- **Shell `resolve_seed` not imported in `runtime.rs`.** The kernel cross-build caught
  it (the host test build did not exercise `_start`); added to the `use crate::{…}`
  resolver import beside `resolve_time_va`.
- **Producer test + `ngrants` churn.** Adding the seed grant shifted the storaged
  block from 3→4 grants and the console block from 1→2; the init/shell builder tests
  were updated to pass a seed and assert the round-trip, and the shell over-budget
  arithmetic comment adjusted for the +34-byte grant.

## Verification record

Toolchain `nightly-2026-06-26`; Verus binary `0.2026.06.07.cd03505`, toolchain `1.95.0`.

- **Verus (authoritative, cold).**
  - `cargo clean -p loader && cargo verus verify -p loader --no-default-features` →
    **30 verified, 0 errors** (was 29; the one new `KIND_SEED` decode arm). Baseline
    re-derived from the pre-change tree (git stash): **29**. `--time-expanded`
    `--output-json`: `decode` `rlimit` **169163 → 177414** (+4.9%), `take_u64`
    unchanged at 14639.
  - `cargo clean -p urt && cargo verus verify -p urt` → **25 verified, 0 errors**
    (unchanged; `random` carries no `verus!{}`).
  - `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` → **7 verified,
    0 errors** (unchanged; the bridge/shim/resolver/attach carry no `verus!{}`);
    loader 30 re-verified transitively.
- **Fuzz.** `cargo fuzz run startup -- -max_total_time=45` → **32,944,795 runs, 0
  crashes** (the decoder, incl. the `KIND_SEED` arm, total over arbitrary bytes; 171
  new corpus units). `cargo test -p loader` (corpus incl. `entropy_seed` +
  `startup2_truncated_seed_refused`) green.
- **Host tests.** loader (14 lib + 3 corpus + 4 regressions), urt (**42**, incl. the
  10 `random::` tests), eunomia-sys (21, incl. the seed resolver), init (5), shell
  (27) — all pass.
- **Miri.** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p urt
  -j4` → **42 tests, 42 passed** (data-race freedom for concurrent `fill_bytes` under
  the lock; the DRBG models run under Miri's thread model). Loader corpus +
  regressions Miri-clean (the seed decode arm UB-clean under the Miri oracle).
- **Build.** `cd kernel && cargo build` clean (std + all user binaries + the
  target-gated `eunomia-sys`/`urt` arms cross-build).
- **On-target (QEMU) — the gate.** `scripts/std-smoke-test.sh` → **`STD SMOKE TEST
  PASS`** with the pre-existing `STD2`/`STD32`/`STD33` and the new **`STD34 PASS`**
  (a 1000-entry `HashMap` over the seed-grant → DRBG → SipHash path), no panic/fault.
  Machine `virt,gic-version=3 -cpu cortex-a72`.
- **Formatting.** `cargo fmt --check` clean (root + `user/{init,shell,stdsmoke}`);
  `scripts/verusfmt.sh --check` clean.

## Surface left trusted / unsupported (and why)

- **The DRBG (`urt::random`) is host-tested plain Rust, not a new seam.** Randomness
  *quality* is explicitly not a verification property (rev2§5.1 MVP,
  documented-predictable non-cryptographic seed — QEMU `virt` has no source); only the
  seed *decode* is mechanized (the loader row). Its `unsafe impl Sync` over
  `UnsafeCell<Option<Drbg>>` is the `urt::Heap` posture — mutual exclusion by the
  reused Loom-certified `SpinLock`, kept honest by Miri + proptest. Folding note, not
  a seam; tally stays 14.
- **The seed source is deliberately predictable and non-cryptographic.** `init` mixes
  the one-shot RTC wall time with the boot `CNTVCT`/`CNTFRQ` — disclosed loudly as
  MVP-only, acceptable only because the HashDoS surface is thin today (storaged keys
  sorted prolly trees; the shell reads trusted interactive input). The real source
  (`RNDR`/virtio-rng) is a deferred backend swap that moves only the seed bytes'
  origin, leaving the DRBG and per-child-reseed contract unchanged.
- **§11 inverse-leak (the new PAL arm).** `sys/random/eunomia.rs`, `eunomia_sys::
  random`, and `urt::random` add zero logic vs `pal/unsupported`: `fill_bytes` loudly
  aborts (the runtime guard re-establishing the "seed attached" precondition) rather
  than a bogus fill, and the seam is term-for-term delegation.
- **Disclosed MVP bounds.** The seed still lives in the stashed startup block (`BOOT`
  for std, the shell's decoded block) after the transient working copies are zeroized;
  a full scrub of that stash is a deferred follow-up (it is a non-crypto seed anyway).

## Follow-ups

- **Real entropy source** (`RNDR` via `-cpu max`, or virtio-rng as a 14→15 seam) —
  the deferred-work item; trigger is the first untrusted input reaching a `HashMap`.
- **Full startup-stash scrub** — zeroize the `Seed` grant bytes in the process-global
  `BOOT`/decoded block after seeding, not just the transient copies (defense-in-depth
  for the future real source).
- **Per-binary `EUNOMIA_HEAP_BYTES` / env producer wiring** — unrelated Phase-2/5
  carry-forwards, untouched here.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

- Added the **Entropy-seed routing note (std-port 3.4)** after the 3.3 Futex-backend
  note: the seed decode is verified surface on the loader row (count 29 → 30), the
  DRBG is a Miri/proptest folding note reusing the certified `SpinLock` (no new
  interleaving model, the `urt::Heap` `unsafe impl Sync` posture), and the bridge/
  shim/std arm are thin delegation. No `external_body`, no new seam. §11 host test:
  `cargo test -p urt` + the QEMU `STD34` smoke.
- Updated the loader Baseline row to **30 verified** with the `KIND_SEED`-arm
  description and the `rlimit` note, and the eunomia-sys transitive re-verify note to
  `loader 30`.

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is
not referenced from code, specs, or guidelines.
