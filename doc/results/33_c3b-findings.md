# C3B findings — fuzz + negotiation proptest + negative control for the IPC connect layer

Phase **C3B** (`doc/plans/20_c3-detail.md:486-510`), the verification tier for the C3
multi-version-wire track. It brings the surface C3A added — the widened
`ConnectReq`/`GrantReply` connect codecs and the pure `negotiate`/`admit_connect` in
`ipc/src/session.rs` — to the rev1§6 bar **beyond Verus** (*"decoders get fuzz targets …
everything gets Miri+proptest"*): the connect decoders get a fuzz target + corpus replay, and
negotiation gets a proptest **with a negative control** (the project's anti-theater discipline).
Branch `c3b-connect-fuzz-proptest`, based on `origin/main` @ `b8fe956` (C3A, PR #175, already
merged).

C3B is **tests/fuzz only** — it touches **no verified surface**. `cargo verus verify -p ipc`
stays **69/0** and the trusted-base tally stays **14**. It depends only on C3A (landed) and is
independent of C3C (the running-path witness, still scope-gated under Open Decision 1).

## What landed

1. **`connect_decode` fuzz target** (`ipc/fuzz/fuzz_targets/connect_decode.rs`, new; registered
   as a second `[[bin]]` in `ipc/fuzz/Cargo.toml`). Drives arbitrary bytes through
   `ConnectReq::decode` and `GrantReply::decode`. Oracle: decode is **total** (never panics) and
   any accepted input re-encodes **byte-for-byte**. `scripts/fuzz.sh` enumerates targets via
   `cargo +nightly fuzz list`, so the new target is auto-discovered by the CI smoke/hunt jobs —
   no workflow edit needed.

2. **Connect-codec corpus + seeds** (`ipc/examples/gen_ipc_corpus.rs` extended;
   `ipc/fuzz/corpus/connect_decode/` committed). Seven seeds the random search rarely hits by
   chance: valid single-version (`req_single`) and multi-version (`req_range`) requests, a grant
   carrying a negotiated version (`grant`), a refusal (`refused`), and the edge inputs `empty`,
   `req_wrong_tag` (right length, wrong tag), `req_trailing` (length mismatch).

3. **Connect-codec corpus replay** (`ipc/tests/fuzz_corpus.rs`, new `connect_decode` test).
   Replays the committed corpus through both decoders with the same byte-exact oracle, so every
   seed (and any future fuzz-found input) stays alive as an ordinary test and is UB-checked under
   Miri even where libFuzzer doesn't run — mirrors the existing `wire_decode` replay.

4. **Negotiation proptests + negative control** (`ipc/src/session.rs`, new
   `#[cfg(all(test, not(loom), not(shuttle)))] mod proptests`, mirroring `reactor.rs`'s gate and
   `cfg(miri)` caps — `cases: if cfg!(miri) {4} else {256}`, `failure_persistence: None` under
   Miri):
   - **`negotiate_is_highest_common`** — for random `(c_min,c_max,s_min,s_max): u8`, `negotiate`
     equals an **independent brute-force oracle** (highest/lowest scan over the whole u8 domain),
     covering overlapping/nested/touching/disjoint/malformed (`min>max`) ranges uniformly; plus a
     symmetry check (`negotiate(c,s) == negotiate(s,c)` — the selection is a property of the
     intersection, not of argument order).
   - **`admit_connect_sequence_grants_and_never_over_grants`** — over a random sequence of
     connects against one server range + one quota, `admit_connect` grants **at the negotiated
     version** iff a common version exists *and* the window fits, else refuses; and the runtime
     accounting tracks `budget − Σgranted` exactly and never exceeds the budget (the
     never-over-grant invariant as a witness over whole sequences).
   - **`negotiate_negative_control_has_teeth`** — a deterministic `#[test]` (same posture as
     `loader/tests/layout_props.rs`'s `old_unchecked_formula_would_wrap` witness): a
     deliberately-wrong `bad_negotiate` that returns the client's max ignoring the server — the
     exact bug negotiation prevents — is shown to select a **non-common** version on witnesses
     (disjoint `[4,5]`/`[1,2]`; overlapping-but-overshooting `[1,5]`/`[1,3]`), so substituting it
     would break the proptest property.

## Design choices worth recording

- **Byte-exact round-trip, not value-only.** Unlike the postcard body in `wire_decode` (varints
  are not guaranteed minimal, so only the *value* round-trips), the connect codecs are
  fixed-width and **byte-canonical**, so the oracle asserts the re-encode equals the input bytes
  exactly. This is the runtime witness for the Verus bijection lemmas
  (`lemma_req_encode_decode`, `lemma_grant_encode_decode`) — the stronger property the proof
  already guarantees.

- **Independent exec oracle, because `common` is ghost.** `negotiate`'s Verus `ensures` is stated
  against the `common(client, server, v)` **spec fn**, which is ghost and not callable from
  exec/test code. The proptest therefore re-derives the same notion in plain Rust by brute force
  over `0u8..=u8::MAX` (`is_common`/`highest_common`), independent of `negotiate`'s `lo/hi`
  arithmetic — so the agreement check is a real test, not a tautology (the project's
  independent-oracle posture).

- **Negative control as a deterministic witness, not a literally-failing proptest.** Per the
  project idiom, the control is a `#[test]` proving the wrong oracle disagrees with the
  independent oracle on concrete witnesses, so swapping it in *would* fail the property. As an
  out-of-band confirmation during development, temporarily substituting `bad_negotiate` into the
  proptest body did make `negotiate_is_highest_common` fail (minimal counterexample
  `client=[0,0], server=[1,0]`), then was reverted — the proptest has teeth.

- **Separate target over folding into `wire_decode`.** The connect codecs are a distinct decoder
  family (fixed-width vs the postcard body) and use the default `no_std` API, so a separate
  `connect_decode` target keeps the corpus seeds semantically clean and matches the project's
  one-target-per-decoder-family structure (cf. storage-server's two targets).

## Verification (all green)

- `cargo test -p ipc` → **33 passed** (was 30 after C3A; +3: the two proptests + the negative
  control).
- `cargo test -p ipc --features fuzzing --test fuzz_corpus` → **2 passed** (`wire_decode` +
  the new `connect_decode`).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p ipc` → clean (32 lib tests
  UB-free, incl. the new proptests at the `cfg(miri)` 4-case cap); and
  `… miri test -p ipc --features fuzzing --test fuzz_corpus` → 2 passed, clean over the
  connect-codec corpus.
- `cargo +nightly fuzz build connect_decode` builds; a 200 000-run hunt found no crash (decode
  stays total, byte-exact round-trip holds); `cargo +nightly fuzz list` shows both targets.
- **C3A surface unchanged:** `cargo verus verify -p ipc` → **69 verified, 0 errors** (C3B adds no
  verified code); trusted-base **tally 14**, `header.rs` proofs untouched.
- **Storage request corpus unaffected** (plan `:505`): `cargo test -p storage-server --test
  fuzz_corpus --test fuzz_regressions` → green (C3A/C3B touch nothing there; C3C will extend it).
- `cargo fmt --check` clean for the root workspace and the `ipc/fuzz` workspace (the
  workspace-split fmt trap — `ipc/fuzz/Cargo.toml` is formatted via its own manifest).

## Follow-ons (out of scope here)

- **C3C** — the running-path witness: the storage session's first exchange negotiates
  version+window via the `ipc` connect codecs, `storage-server/wire.rs`'s version byte becomes
  dynamic + validated (`version_ok` at dispatch), and `storage-server/fuzz` covers it; QEMU-smoke
  witness. Scope-gated by Open Decision 1 (recommended: thin inclusion).
