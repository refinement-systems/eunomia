# C3A findings — version negotiation in the verified IPC connect layer

Phase **C3A** (`doc/plans/20_c3-detail.md`), the core of the C3 multi-version-wire
track: the negotiation **mechanism**, entirely inside the `ipc` crate and
Verus-verified, with **no running-path change**. C3B (fuzz/proptest tier) and C3C
(the end-to-end storaged↔shell witness) are separate sub-phases that depend on
C3A and are **not** part of this work. Branch `c3a-version-negotiation`, based on
`origin/main` @ `0feebce`.

C3A makes rev1§3.7 (*"versions are negotiated once at session establishment … a
server may speak several concurrently"*) true at the mechanism level by adding the
**version dimension** to the already-verified connect codecs in
`ipc/src/session.rs`. The enabling fact: those codecs are verified but their
*wiring is deferred* (`session.rs` module doc) — they are not yet on any running
path, so widening them now costs no migration. Design decisions 1–4 and open
decisions 2–3 are resolved here per the plan's recommendations.

## What landed

1. **`VersionRange { min: u8, max: u8 }`** (`session.rs`) — a contiguous `[min,max]`
   inclusive span of supported wire versions, with `new`/`single` constructors.
   Plus `PROTOCOL_VERSION: u8 = 1`, the sole version this build offers today.

2. **Widened connect codecs** (DD2). `ConnectReq` gains `versions: VersionRange`
   (`REQ_LEN 5→7`, the two version bytes **appended** so the pre-C3 prefix is
   byte-identical). `GrantReply::Grant` gains the negotiated version as a second
   field — `Grant(WindowGrant, u8)` (`GRANT_LEN 9→10`, version byte appended). The
   ghost models (`req_*`/`grant_*`), exec codecs, and the four ∀ bijection lemmas
   extend over the new bytes: the `u32` reassembly keeps its existing `bit_vector`
   identities verbatim; the version bytes are carried directly and close by
   sequence extensionality (`=~=`).

3. **`negotiate(client, server) -> Option<u8>`** (DD4), pure and Verus-verified:
   the **highest common** version (`min(client.max, server.max)` when the ranges
   overlap), else `None`. The `ensures` is the standard "max of the intersection"
   spec via a `common(client, server, v)` predicate —
   `Some(v) ⟹ common(v) ∧ ∀w. common(w) ⟹ w ≤ v`; `None ⟹ ∀w. ¬common(w)`. Total
   over arbitrary `u8` ranges, so a malformed decoded range (`min > max`) denotes
   an empty set and refuses cleanly. **No `bit_vector` needed** — the proof is
   linear arithmetic over `u8`, discharged without extra hints.

4. **`admit_connect(adm, server: VersionRange, req_bytes)`** (DD4) — the single
   admission point now selects version *then* window: decode → `negotiate`
   (`None` ⇒ internal `ConnectErr::VersionMismatch`) → `admit`
   (`Err` ⇒ `ConnectErr::Refused`) ⇒ `Grant(window, version)`. Both `ConnectErr`
   arms are constructed inline and collapsed to the single wire `GrantReply::Refused`
   (open decision 3: no wire reason byte; the internal variant is for
   diagnosability). The `Admission` quota invariant proofs are **untouched**.

5. **`version_ok(header_version, negotiated) -> bool`** (DD3) — the per-message
   version check (`ensures ok == (header_version == negotiated)`), inert until the
   dispatch site is wired in C3C. Stamping/validating rides the **existing** header
   `version` field, so `ipc/src/header.rs` and its bijection proofs are untouched
   (the parent acceptance bar).

6. **Re-exports** (`ipc/src/lib.rs`): `negotiate, version_ok, VersionRange,
   PROTOCOL_VERSION` (`ConnectErr` already re-exported, gains `VersionMismatch`).

7. **Docs/ledger**: the single spec touch (rev1§8.3:487 forward note — the
   negotiation mechanism is implemented as of C3, IDL backend + stable ABI stay
   deferred); the ledger `-p ipc` row updated 62 → 69 (**tally stays 14**, no new
   trusted seam).

## Deviation from the literal plan: `model.rs` needed mechanical caller fixes

The plan's C3A "Touches" listed only `session.rs` + `lib.rs` and asserted the
Loom/Shuttle harness was "untouched." That is true of its *logic*, but the
signature/variant changes force three **mechanical** edits in `ipc/src/model.rs`'s
`fairness_smoke` connect harness, without which `cargo test -p ipc` does not
compile:

- `admit_connect(&mut adm, msg.payload())` → adds the server range
  `VersionRange::single(PROTOCOL_VERSION)` (overlaps `for_window`'s default, so
  negotiation always succeeds and the grant/refuse counts the test asserts are
  governed solely by the window quota, exactly as before);
- `matches!(r, GrantReply::Grant(_))` → `GrantReply::Grant(..)` (two-field variant);
- the harness `use` import gains `VersionRange, PROTOCOL_VERSION`.

The harness still drives the same connect-flood admission test; no concurrency
shape or assertion changed.

## Design choices worth recording

- **Version on the `Grant` variant, not on `WindowGrant`.** Keeping
  `WindowGrant { window, size }` and `Admission`/`admit`/`release` byte-for-byte
  unchanged is the strongest reading of "Admission proofs untouched," and keeps
  `admit`'s direct-call unit tests intact. The cost — one pattern (`Grant(..)`) in
  the harness — is trivial.
- **Range over bitset** (open decision 2): a contiguous `[min,max]` matches the
  monotone "a breaking change is a new version number" framing and the "old and
  new clients coexist" case. A non-contiguous version *bitset* (a withdrawn middle
  version) is the recorded forward, append-only generalization — YAGNI with one
  version deployed.
- **`negotiate` is total over malformed ranges.** Because the client range comes
  from decoded bytes, `min > max` is possible; the `lo = max(mins) <= hi = min(maxes)`
  formulation makes that an empty intersection (clean `None`), no precondition
  needed. Covered by `negotiate_malformed_range_refuses`.

## Verification (all green)

- `cargo verus verify -p ipc` → **69 verified, 0 errors** (was 62; +7 — `negotiate`,
  `version_ok`, the two `VersionRange` constructors, `ConnectReq::new`, and the
  re-proved widened codecs/lemmas). `header.rs` proofs are part of this total and
  unchanged; **tally stays 14**, no new `external_body`/`assume_specification`.
- `cargo test -p ipc` → 30 passed (the new `negotiate_*`, `version_ok_matches_exactly`,
  `admit_connect_*` cases — disjoint/nested/touching/single-version, highest-common
  selection, version-mismatch-refused-without-touching-quota — plus the unchanged
  codec/admission/reactor/model tests, incl. `fairness_smoke_std`).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p ipc` → clean (29
  tests UB-free over the widened codecs and `negotiate`).
- The widened connect decoders stay **total**: a malformed widened
  `ConnectReq`/`GrantReply` decodes to `None`, never a crash (the
  `*_rejects_*` tests at `REQ_LEN±1`/`GRANT_LEN`).
- `kcore` is **not** affected (C3A touches no kcore file and kcore does not depend
  on `ipc`); the `IpcReactor` TLA model and the Loom/Shuttle harness logic are
  unchanged (only the mechanical caller fixes above).

## Follow-ons (out of scope here)

- **C3B** — fuzz the widened connect decoders + a negotiation proptest with a
  negative control (a `negotiate` that ignores server overlap must fail the test).
- **C3C** — the running-path witness: the storage session's first exchange
  negotiates version+window via the `ipc` connect codecs, and
  `storage-server/wire.rs`'s version byte becomes dynamic + validated (`version_ok`
  at dispatch); QEMU-smoke witness. Open decision 1 (recommended: thin inclusion).
