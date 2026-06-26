# 8 — storage-server wire decode header/version gate under Verus (Task 8)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 8
(storage-server wire decode header/version gate; Tier 1, depends on the Task 4
storage-server gate). Outcome: **verified, shipped, first attempt.**
`cargo verus verify -p storage-server --no-default-features --lib` now reads
**`19 verified, 0 errors`** (was 14). No reverts; no new trusted seam (tally stays
14); the gated deps (cas 75, ipc 71) are unchanged.

## What was attempted

Bring the storage protocol's wire-decode **header + version prefix** under Verus.
`wire::decode()` (`storage-server/src/wire.rs`) checked the 3-byte header
(`buf.len() >= 3`, magic `buf[..2] == PROTO_MAGIC`) and the per-message version
(`ipc::version_ok(buf[2], negotiated)`) in plain Rust, guarded only by host tests.
The goal: prove, ∀ bytes, that this prefix is total (never panics / reads OOB),
refuses `BadHeader` exactly on short-or-bad-magic and `Version` exactly on
good-magic-but-wrong-version (composing on the already-verified `ipc::version_ok`),
with the magic check strictly preceding the version check — while the postcard
*body* decode stays a trusted seam.

The pure prefix was extracted into an always-compiled `verus!{}` function with a
ghost model, mirroring the ipc header codec (`ipc/src/header.rs`):

```rust
pub open spec fn spec_check_header(buf: Seq<u8>, negotiated: u8) -> Result<usize, WireError> {
    if buf.len() < 3 || buf[0] != PROTO_MAGIC@[0] || buf[1] != PROTO_MAGIC@[1] {
        Err(WireError::BadHeader)
    } else if buf[2] != negotiated {
        Err(WireError::Version)
    } else {
        Ok(3)
    }
}

pub fn check_header(buf: &[u8], negotiated: u8) -> (r: Result<usize, WireError>)
    ensures r == spec_check_header(buf@, negotiated),
{
    broadcast use vstd::slice::group_slice_axioms, vstd::array::group_array_axioms;
    if buf.len() < 3 || buf[0] != PROTO_MAGIC[0] || buf[1] != PROTO_MAGIC[1] {
        return Err(WireError::BadHeader);
    }
    if !ipc::version_ok(buf[2], negotiated) {
        return Err(WireError::Version);
    }
    Ok(3)
}
```

The single `ensures` equality captures every plan obligation at once: totality (a
verified exec fn provably returns; no panic/OOB), the magic-precedes-version
ordering (the spec's structural `if`/`else if`), `BadHeader`-iff-short-or-bad-magic,
`Version`-iff-good-magic-wrong-version, and `Ok(3)` (the body offset) on a good
header. The serde-gated `decode<T>` now delegates the prefix and keeps the postcard
body as plain Rust:

```rust
let off = check_header(buf, negotiated)?;
let (v, rest) = postcard::take_from_bytes(&buf[off..]).map_err(|_| WireError::Body)?;
```

To make the prefix verifiable, `wire.rs`'s file-level `#![cfg(feature = "serde")]`
was removed: the header consts (`PROTO_MAGIC`/`PROTO_VERSION`/`MAX_MSG`), the
`WireError` enum, and the `verus!{}` block are now always compiled (so the gate's
`--no-default-features` build sees them), while the postcard codec functions
(`encode`/`decode`/`encode_request`/…) and their `use crate::{Request, Response}` /
`use alloc::vec::Vec` imports carry per-item `#[cfg(feature = "serde")]`.

## The headline finding: the postcard seam is routed out by feature-exclusion, not `external_body`

The plan's task text suggested wrapping `postcard::take_from_bytes` in
`external_body` and adding a ledger seam row. **This is infeasible under the
committed gate and was not done.** `postcard` is an *optional, serde-gated*
dependency (`Cargo.toml`), and the gate runs `--no-default-features` (Task 4's
deliberate island, mirroring cas, to keep serde/postcard/BTreeMap out of verify).
Under verification postcard is therefore *not compiled at all* — there is nothing
to mark `external_body`. Forcing one in would mean re-enabling serde for verify and
marking the entire session/handle/postcard dispatch external — the opposite of Task
4's design. So the only correct design is: verify the pure prefix (no serde), and
leave the postcard body exactly where it is — serde-gated, outside verified
compilation, guarded by host tests. The boundary is **trust-routed by
feature-exclusion**, which adds **no `external_body` and no new trusted seam**. This
is the same posture the whole serde codec already had; the only change is that the
*prefix* graduated out of it into verified scope.

## Result

`cargo clean -p storage-server && cargo verus verify -p storage-server
--no-default-features --lib` — real run (results line present; prover
`0.2026.06.07.cd03505` / toolchain `1.95.0`):

| crate | result |
|---|---|
| cas `--no-default-features` (dep) | 75 verified, 0 errors |
| ipc (dep) | 71 verified, 0 errors |
| **storage-server** | **19 verified, 0 errors** (was 14; re-verifies cas 75, ipc 71 in-session) |

The count rose 14 → 19 (+5): `wire::PROTO_MAGIC`, `wire::PROTO_VERSION`,
`wire::MAX_MSG` (consts, +1 each), the derived `wire::WireError::clone` (+1, see
finding 3), and `wire::check_header` (exec fn, +1). The `spec_check_header`
`open spec fn` is transparent and carries no standalone obligation, so it adds 0
(same as `has_right` and the Task-7 `out_of_range`).

Per-function `rlimit` (`scripts/verus-baseline.sh storage-server`, storage-server's
own obligations; the existing rights-lattice obligations are **byte-identical**):

| obligation | rlimit (pre → post) |
|---|---|
| `attenuate` (control) | 30889 → 30889 (±0) |
| `lemma_attenuate_monotone` (control) | 44064 → 44064 (±0) |
| `lemma_attenuate_r_all_denies_stat_store` (control) | 4115 → 4115 (±0) |
| the seven `R_*` rights consts (control) | 2 → 2 each (±0) |
| `check_header` (new) | — → 10964 |
| `PROTO_MAGIC` / `PROTO_VERSION` / `MAX_MSG` / `WireError::clone` (new) | — → 2 each |

Unlike Task 7, even the co-module controls did not drift: `attenuate` and its
lemmas live in the `lib.rs` block, a different module from the new `wire` block, so
adding `wire` siblings did not enlarge their SMT context. The crate total rises only
by the new obligations' own cost.

## Findings that mattered

1. **`PROTO_MAGIC@[0]` (the array view) is the clean way to compare a fixed-byte
   prefix in spec.** The 2-byte magic is a `pub const PROTO_MAGIC: [u8; 2]`. In the
   spec model it is read as `PROTO_MAGIC@[0]` / `@[1]` (the `Seq<u8>` view of the
   array), and the exec body indexes `PROTO_MAGIC[0]` directly; the bridge between
   the two is `broadcast use vstd::array::group_array_axioms` (alongside
   `group_slice_axioms` for the `buf` slice). No new const, no literal duplication —
   the proof stays tied to the wire ABI's single source of truth. (The plan's
   spec-model fallback was not needed.)

2. **Cross-crate composition on a verified `ensures` just works.** `check_header`
   calls `ipc::version_ok` full-pathed (not `use`-imported); its
   `ensures ok == (header_version == negotiated)` discharges the `Version` clause
   directly — `!version_ok(buf[2], n)` gives the prover `buf[2] != n`, and the true
   branch gives `buf[2] == n` for the `Ok` arm. Because storage-server links ipc and
   re-verifies it under the alloc prelude in the same session, the contract is
   visible at the call site with no extra annotation. This is the first verified
   storage-server function to *compose on another gated crate's* proof rather than
   restating an inline fact.

3. **A `#[derive(Clone)]` on a type inside `verus!{}` adds a verified item.**
   Moving `WireError` into the block surfaced its derived `WireError::clone` as a
   (trivial, rlimit-2) obligation — the fifth of the five new items. Worth knowing
   when predicting a count delta from a struct/enum move: count the derives, not
   just the hand-written fns. (`PartialEq`/`Eq`/`Debug` did not add obligations
   here; only `Clone` did.)

4. **Splitting a `#![cfg(feature = …)]` module into a verified core + a gated codec
   is a clean onboarding move.** The whole `wire` module was previously excluded
   from the `--no-default-features` build (so *nothing* in it could be gated); per
   item `#[cfg(feature = "serde")]` on just the postcard functions let the header
   layer graduate into verified scope without touching the body codec's behavior or
   the public API (`wire::encode_request`/`decode_request`/… still resolve under the
   default features every consumer uses). Pre-existing condition confirmed unchanged:
   `cargo test -p storage-server --no-default-features` could not build the `tests/`
   fuzz targets before *or* after (they need serde's `encode_request`); the gate's
   `--lib` build and the default-feature `cargo test` are both green.

## Reverted vs kept

Nothing reverted — the proof succeeded on the first attempt. Kept: the
`spec_check_header` ghost model + `check_header` exec fn, the wire-module
restructure (file-level `serde` gate → per-item gates with the header layer always
compiled), the delegating `decode<T>` wrapper, the new always-compiled
`header_tests` (`check_header_cases` covering short/bad-magic/wrong-version/ok, and
`magic_strictly_precedes_version_has_teeth` pinning the ordering), and the
bookkeeping (ledger Baseline row + postcard feature-exclusion routing note,
CLAUDE.md gate comment, Cargo.toml `metadata.verus` comment, CI comment). The
existing serde tests (`roundtrip_and_strictness`, `version_is_stamped_and_validated`)
are unchanged except for tightening their gate to `#[cfg(all(test, feature =
"serde"))]`; they remain the companion oracle tier for the full decode path,
including the postcard body seam (truncated-body / trailing-bytes / wrong-magic-wins
teeth).

## Proposed guideline additions (`doc/guidelines/verus.md`)

1. **A serde-gated interpreted decoder (postcard/serde) is routed out of verify by
   feature-exclusion, not `external_body`.** When the gate runs
   `--no-default-features` and the interpreted codec is an *optional, feature-gated*
   dependency, that dependency is not compiled during verify, so there is no
   function to mark `external_body` and no seam row to add. Verify the pure prefix
   that precedes the body (extracted into an always-compiled `verus!{}` island);
   leave the body codec serde-gated and host-tested. Prefer this to fabricating an
   `external_body` that would force the interpreted dependency back into verify
   scope. (Extends the §13 decision map: "interpreted seam, but feature-excluded ⇒
   no seam row.")
2. **Compose a decoder prefix on another gated crate's verified `ensures`,
   full-pathed.** A cross-crate verified predicate (here `ipc::version_ok`) is
   usable at the call site with no extra annotation when the dependency is
   re-verified in-session; reference it `crate::path`-qualified rather than
   `use`-importing it, to keep the dependency edge explicit.
3. **Reading a fixed-byte constant prefix in spec: use the array's `@` view.** For a
   `pub const M: [u8; N]`, compare `M@[i]` in the spec model and `M[i]` in exec, and
   `broadcast use vstd::array::group_array_axioms` to bridge them — no literal
   duplication, the proof stays tied to the ABI const.
4. **Predict a count delta from derives, not just hand-written fns.** Moving a
   `#[derive(Clone)]` type into a `verus!{}` block adds its derived `clone` as a
   verified item; budget for it when forecasting the Baseline bump.

## Trusted base

**Tally unchanged at 14.** `check_header` adds no `external_body`/
`assume_specification` — it is pure `u8`-slice / array-view reasoning over the
`vstd` slice/array axiom groups and composes on the existing `ipc::version_ok`
contract; no project seam is introduced. The postcard body decode that follows the
prefix stays trusted, but **adds no seam row**: it is routed out of verify by
feature-exclusion (serde-gated, dropped under `--no-default-features`), not by an
`external_body` marker — recorded as a routing note in the storage-server Baseline
row. storage-server was already gated (Task 4), so there is no crate onboarding: the
CI line, `verus-baseline.sh` entry, and CLAUDE.md gate list already include it (only
their descriptive comments were broadened). Baseline row updated: `-p storage-server`
→ 19 verified, 0 errors (was 14).
