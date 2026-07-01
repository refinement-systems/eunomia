# Findings 14 — std-port 4.2: the verified path decoder

Task 4.2 of `doc/plans/2_plan-std-revised.md`: replace the fs client's placeholder
`split_path` with a **Verus-total, cargo-fuzzed, `.`/`..`-resolving, root-confining**
path decoder. `.`/`..` are path syntax resolved by the walk and never sent on the wire
(rev2§4.9); a `..` escaping the process root handle is unnameable and denied
(rev2§2.3). Proven live in QEMU (`STD4 PASS` now exercises resolution + a refused
escape at EL0).

## What shipped

- **`eunomia-sys/src/path.rs`** — a new **verified, host-buildable** module.
  `resolve(&[u8]) -> Option<ResolvedPath<'_>>` is mechanized **total** over all bytes
  (no panic / no out-of-bounds read) with `ensures res matches Some(p) ==>
  well_formed_resolved(p, buf@)`: every returned component is well-formed (the
  `cas::prolly::validate_name` predicate — 1..=255 bytes, no NUL, no `/`, not `.`/`..`)
  and a subrange of the input. No-alloc: it fills a fixed `[&[u8]; MAX_COMPONENTS=64]`
  arena with borrowed subranges, the `loader::startup::decode` shape.
- **`eunomia-sys/src/fs.rs`** — `split_path` replaced by `resolve_path`, a thin `alloc`
  adapter that calls the verified `path::resolve` and copies the borrowed components
  into the `Vec<Vec<u8>>` wire path (`TreePath`). Every path op (`read`/`write`/`stat`/
  `rename`/`unlink`/`readdir`) now fails a rejected path with `ERR_FS_BAD_PATH` instead
  of blindly forwarding it.
- **`eunomia-sys/fuzz/`** — a new standalone fuzz workspace (the `loader/fuzz` template)
  with the `path` target: a **differential oracle** against a plain reference resolver
  plus the structural invariants. Corpus seeds cover the interesting cases.
- **`eunomia-sys/tests/{fuzz_corpus,fuzz_regressions,path_proptest}.rs`** — Miri corpus
  replay, pinned regression reproducers, and a proptest (differential over arbitrary
  bytes + join↔resolve idempotence).
- **`user/stdfs`** — the fs gate fixture gains a step exercising `docs/./smoke` /
  `docs/../docs/smoke` resolution and an escaping-`..` refusal end-to-end.

## Decisions (and rejected alternatives)

- **Home: a new un-gated `verus!{}` module in `eunomia-sys`, not a new crate and not
  `fs.rs`.** Forced by two facts: (a) `fs.rs` is `#![cfg(any(target_os = "eunomia",
  target_os = "none"))]`, gated *out* of the host build where Verus runs — so the
  verified logic cannot live there (the encoder/io_error posture); (b) the storage
  server *validates* names (`validate_name`) but does not *resolve* `.`/`..`, so a
  shared resolver crate buys nothing (a `storage-server → eunomia-sys` edge would also
  be a cycle). Rejected: a `le-bytes`-style shared crate (heavier, no real sharing);
  putting it in `loader` (semantic mismatch — loader is ELF/startup).
- **No-alloc verified core (borrowed subranges + fixed arena), forced by the verify
  graph.** The host `-p eunomia-sys` session pulls `vstd` with `default-features =
  false` and only does `extern crate alloc` on the *target* build, so `Vec`/`alloc`
  proofs are unavailable on the host. The resolver hands back `&[u8]` subranges of the
  input into a `[&[u8]; 64]` arena; the `alloc` into `Vec<Vec<u8>>` happens in the
  target-only `fs.rs`. This keeps the host verify graph byte-identical (the crate's
  deliberate minimal posture) and matches `startup::decode`.
- **`MAX_COMPONENTS = 64`, a disclosed depth cap** (deeper → refused). `..` re-pushes do
  not count against it (`a/../a/../…` stays depth ≤ 1). The 256-byte `MAX_MSG` wire cap
  already bounds a *sendable* path near this depth, so 64 is not the binding limit for
  real paths; the fixed `[&[u8]; 64]` arena is ~1 KB.
- **`.`/`..`-not-`seq!`-literal specs.** `is_dot_name`/`is_dotdot_name` are written as
  `c.len() == 1 && c[0] == DOT` (length + index) rather than `c@ == seq![b'.']`. This
  keeps `well_formed_component` in pure length/index reasoning that the exec checks
  discharge directly, sidestepping the `Seq` extensionality burden a literal comparison
  would add — the load-bearing simplification behind the first-try green.
- **`component_ok` is one-directional (`r ==> well_formed_component(c@)`).** `resolve`'s
  `ensures` only constrains the accepted (`Some`) case, so a `false` result carries no
  obligation; only the accept branch needs a proof. Halves the proof surface.
- **A rejection maps uniformly to `ERR_FS_BAD_PATH` for the MVP** (escape, malformed,
  and too-deep alike). Distinguishing a confinement escape as `Denied` (rev2§2.3
  "unnameable … denied") is a natural refinement for **4.3** (the errno decision table)
  — noted, not done here.
- **`File` unchanged** — 4.1's `(PathBuf, cursor)` is kept and components are re-derived
  per op via `resolve_path`. Memoizing a resolved `TreePath` in `File` (the plan's
  `File=(HandleId, TreePath, offset)` phrasing) is a representation change beyond 4.2's
  "path decode" scope; left as a follow-up.

## Problems

- **`rustfmt` module reordering orphaned a doc-comment.** `reorder_modules` (default on)
  sorted `pub mod path;` after `pub mod pal;` but left my comment block attached to
  `pal`. Fixed by moving the comment to sit directly above `pub mod path;`. (Watch for
  this whenever inserting a `mod` whose alphabetical position differs from the source
  position.)
- Otherwise the proof went green on the **first** `cargo verus verify` — the
  `startup::decode` idioms (bounds-checked cursor helper, `slice_subrange` for
  provenance, the `let ghost prev` accumulation-loop push with the `forall`-invariant,
  `broadcast use vstd::slice::group_slice_axioms`) transferred directly. The one novelty
  — the `..` pop — needed no extra proof: a prefix invariant (`forall j < k: …`) is
  weakened for free when `k` shrinks.

## Verification record

- **Verus (the gate):** `cargo clean -p eunomia-sys && cargo verus verify -p
  eunomia-sys` → `eunomia-sys` **`16 verified, 0 errors`** (was 7; the `+9` is
  `path.rs`), transitively `loader 30`, `ipc 71`. Re-run green over the
  verusfmt-formatted tree.
- **cargo-fuzz:** `cargo +nightly fuzz run path -- -max_total_time=60` →
  **16,546,936 runs, 0 crashes** (the differential oracle held over every input; corpus
  grew to 106 entries). `scripts/fuzz.sh smoke eunomia-sys` replays the committed corpus
  (111 entries) clean.
- **Miri (UB):** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p
  eunomia-sys --test fuzz_regressions --test fuzz_corpus` → **UB-clean** (6 tests).
- **Host tests:** `cargo test -p eunomia-sys` → lib 22, `fuzz_corpus` 1, `fuzz_regressions`
  5, `path_proptest` 3, all pass (the existing encode round-trip oracle unaffected).
- **QEMU end-to-end:** `scripts/fs-smoke-test.sh` → **`FS SMOKE TEST PASS`** (`STD4
  PASS`) — the new `[stdfs] dotdot resolves + escape refused` step proves `.`/`..`
  resolution and an escaping-`..` refusal live at EL0 over storaged. Regression:
  `scripts/std-smoke-test.sh` → all `STD*` PASS.
- **Formatting:** `cargo fmt --check` + `scripts/verusfmt.sh --check` clean (root + the
  `eunomia-sys/fuzz` and `user/stdfs` manifests). `path.rs` needs no verusfmt skip-list
  entry (single `verus!{}` block, no `x[..n]` index, no inter-block comments).
- **CI wiring:** added `-p eunomia-sys` to `fuzz.yml`'s `replay-tests` and to
  `scripts/fuzz.sh`'s crate list (so the smoke/hunt jobs build+replay the new target);
  the `-p eunomia-sys` verus line already existed (CI asserts 0 errors, not a count).
  Updated the `CLAUDE.md` Miri quick-UB command to include `-p eunomia-sys`. Added
  `eunomia-sys/fuzz` to the root `Cargo.toml` `exclude`.

## Ledger

`doc/guidelines/verus_trusted-base.md`: the **eunomia-sys Baseline row** count rises
`7 → 16` and the row title/prose now cover the path resolver; a new **Path-resolver
routing note (std-port 4.2)** records the split — `resolve` is verified surface (pure
slice/byte reasoning over `vstd`, **no `external_body`, no new seam, tally stays 14**),
the resolution *semantics* are the lossy-decode tier carried by fuzz/proptest/Miri, and
the `fs.rs` marshalling stays the trusted shell. Host test named: `cargo test -p
eunomia-sys` + `cargo +nightly fuzz run path` + `STD4 PASS`.

## Surface left trusted / test-routed (the only unverified code, and why)

- **The resolution *semantics* are not a Verus property.** Per `verus.md` §8, a lossy
  decoder (one that *drops* `.`, *pops* on `..`) states totality + output
  well-formedness at its own grain; that the output is the *intended* resolution
  (`a/../b → [b]`) is one grain up and goes to the differential fuzz oracle + proptest +
  regression reproducers. What Verus *does* prove is the security-load-bearing half: the
  output is always well-formed and root-confined (no `..` survives), so no accepted path
  can escape the handle subtree — for *any* resolution the code computes.
- **`eunomia-sys/src/fs.rs` stays a trusted marshalling shell** (the `sys/stdio`
  posture). PAL-thinness check for this arm (the plan's per-task gate): `resolve_path`
  adds no byte logic — it calls the verified resolver and copies borrowed slices — and
  the §11 inverse-leak boundary is re-established by construction, since the verified
  resolver's output *is* the `validate_name` predicate the server requires; a rejection
  becomes a clean `ERR_FS_BAD_PATH`, never a bogus round-trip.

## Follow-ups

- **Escape → `Denied` errno refinement** (4.3): give a confinement escape a distinct
  errno from a malformed component.
- **Memoize the resolved `TreePath` in `File`** (perf): avoids re-resolving on every op.
- **Share the resolver with `user/shell`/`user/init`'s `parse_path`** (identical minimal
  splits today) when the shell moves to std (5.3) / the 6.2 consolidation.
- Defense in depth is intact: even though confinement is now enforced client-side, the
  server still independently rejects `.`/`..` (`validate_path`), so a compromised or
  buggy client cannot smuggle unresolved path syntax past storaged.
