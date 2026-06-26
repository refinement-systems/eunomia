# 17 — Verus findings: `startup::decode` declined as a mechanization candidate (Phase 3.3)

Date: 2026-06-26. Crates: `loader`. This is a temporary intermediate record per CLAUDE.md;
it is not referenced from comments, specs, or guidelines.

## Purpose

Phase 3.3 of `doc/plans/0_verus-improvements.md` is a **recording-only** task. It does not
mechanize `loader/src/startup.rs::decode` under Verus; it makes the decision to leave that
decoder on its proof-less *oracle tier* a conscious one rather than an omission, and records
the evidence a future implementer needs to re-decide. The status quo is unchanged: no code
obligation moved, no trusted seam touched, the `external_body`/`assume_specification` tally
stays 14.

## The decision — decline auto-mechanization

`startup::decode` (`loader/src/startup.rs:268-305`) is **not** scheduled for a Verus lift.
It stays plain Rust, kept honest by its existing fuzz + proptest + unit-test oracle tier
(below). Schedule a deductive twin **only** if the parent later decides the `_start`-input
refuse-not-crash floor (rev2§2.7) warrants one beyond that tier.

## Why it is a genuine §8/§9 adversarial-decode candidate

`decode` is the `_start`-time wire decoder for the bootstrap startup block (rev2§5.1: argv,
env, and the named-grant table), consumed before anything else exists in the child. It is
untrusted-shaped input — not trusted-provenance — so per `verus.md` §8/§9 it is a real
adversarial-decode surface, and a legitimate mechanization candidate in principle. The facts
that make it one:

- It is a structural sibling of the **already-verified** `elf::parse` (`loader/src/elf.rs:219`,
  inside the crate's `verus! {` block at `:29`; ledger loader Baseline = 12 verified, 0
  errors). Both use the same `checked_add`/`get`-bounded `Reader` cursor
  (`startup.rs:229-259`, `take` at `:235-240`), the same fixed-array arena with explicit
  counts (`MAX_GRANTS`/`MAX_ARGV`/`MAX_ENV` = 8, `startup.rs:89-93`; `elf`'s `MAX_SEGMENTS`),
  and the same rev2§2.7 refuse-not-crash contract.
- It is *total over arbitrary bytes today* — the magic check, the per-count arena-cap gate
  (`startup.rs:276`), and the `get`/`checked_add` bounds on every grant body / argv / env
  length make a malformed block return `None`, never a panic, OOB read, or unbounded
  allocation. But that totality is established only by the **oracle tier**, not deductively.

## Why NOT to auto-schedule (the cost)

Mechanizing it is a large new proof surface — out of proportion to the marginal assurance
over the existing oracle tier:

- **Borrowed-slice-into-input return values.** argv/env decode as `&'a [u8]` borrowed back
  into the message buffer (`Startup<'a>` at `:129-136`; `decode(buf) -> Option<Startup<'_>>`
  at `:268`; `push_argv(r.take(len)?)`). Carrying borrowed slices that alias the input
  through a Verus `ensures` is the hard part of such a lift, with no precedent in the crate
  (`elf::parse` returns an `Image<'_>` but the proof obligations there are over the integer
  framing, not the borrow provenance the `decode_is_total` proptest checks by pointer-range).
- **`Reader` rewrite.** `Reader::u16/u32/u64` use `u16::from_le_bytes(...)` (`:246-258`),
  which §8 forbids in verified code (Verus has no spec for `from_le_bytes`/`try_into`); a lift
  would route them through the shared `le-bytes` crate's byte-indexed `read_u*_le` readers,
  the way `elf::parse` was migrated in Phase 2.3.
- **Arena push-loop invariant.** Discharging the `.ok()?` guards on
  `push_grant`/`push_argv`/`push_env` (`:294,298,302`) deductively needs a loop invariant
  proving the running count stays `<= cap` so the push can never overflow the arena — the
  invariant the per-count gate at `:276` establishes informally.

## The existing oracle tier that already covers the floor

`startup::decode`'s refuse-not-crash floor is already exercised by a faithful oracle tier
(this is what the ledger's "stay external plain Rust" posture rests on):

- the `startup` cargo-fuzz target (`loader/fuzz/fuzz_targets/startup.rs`, registered at
  `loader/fuzz/Cargo.toml:39-40`) — feeds arbitrary bytes through `decode`, asserts the arena
  caps hold, then re-encodes and re-decodes any accepted block;
- the `decode_is_total` proptest (`startup.rs:653`) — over arbitrary byte vectors, asserts
  `decode` never panics, every count is within its arena, and every borrowed argv/env slice
  lies inside the input buffer (a pointer-range check);
- the `round_trips` proptest (`:623`) and the `golden_layout` / `rejects_malformed` /
  `decode_tolerates_trailing_bytes` / `round_trip_oracle_has_teeth` unit tests.

## Do not strip the never-fires guards

The `.ok()?` guards on `push_grant`/`push_argv`/`push_env` (`startup.rs:294,298,302`) never
fire — each loop count was already validated `<= MAX_*` at `:276`, as the `:293` comment
states — but they must **not** be removed. Deleting an unverified guard requires a licensing
proof this plain-Rust file does not carry; without the deductive arena invariant above, the
guard is the only thing standing between a future refactor and an arena overflow.

## Secondary findings surfaced during verification

Recorded here per the plan's "record new findings as you go" rule; **neither is acted on in
this recording-only task** (changing no seam/Baseline keeps the Phase-3.3 contract intact).

- **A — loader ledger row omits startup's oracle tier.** The loader Baselines row in
  `doc/guidelines/verus_trusted-base.md` enumerates *elf*'s kept companion oracle tier
  (the `elf_parse` fuzz target + corpus + Miri replay, the `layout_props` proptest, the
  `parse`/`page_layout_*` unit tests) but does not enumerate *startup*'s (the `startup` fuzz
  target, the `decode_is_total` / `round_trips` proptests, the startup unit tests), even
  though it correctly states "The startup byte codec … stay external plain Rust." A future
  ledger-hygiene pass (the Phase 1.2 "code is authoritative" reconciliation) should fold the
  startup oracle tier into that row so the proof-less surface and its companion tier are both
  enumerated.
- **B — stale `Reader` comment (fixed here).** The `startup.rs` `Reader` doc comment claimed
  it mirrored `elf::u16le`/`u32le`/`u64le`. Those readers were migrated to the shared
  `le-bytes` crate in Phase 2.3 and no longer exist in `elf.rs`, so the reference was stale
  (CLAUDE.md: comments describe what is). This change re-points it to the verified
  `elf::parse` decoder's same `checked_add`/`get` bounds discipline — a stable anchor.

## Verification

The recording adds no proof obligation, so there is nothing to re-verify for it. The one
code change is the finding-B comment fix in `startup.rs`, a comment-only edit to a
non-verified plain-Rust module (startup is outside the verified surface — loader verifies
only `elf.rs`'s `verus!{}` block) ⇒ SMT byte-identical.

- `cargo build -p loader` clean; `cargo test -p loader` green (the startup oracle tier still
  passes).
- Belt-and-suspenders, per the plan's Baseline-reestablishment discipline (a present
  `verification results::` line = a real cold run): `cargo clean -p loader && cargo verus
  verify -p loader --no-default-features` still ends with `12 verified, 0 errors` — unchanged.
