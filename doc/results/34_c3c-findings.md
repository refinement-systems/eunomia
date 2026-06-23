# C3C findings — end-to-end storage version-negotiation witness

Phase **C3C** (`doc/plans/20_c3-detail.md:514-553`), the running-path witness for the C3
multi-version-wire track (Design decision 5 **Option A**). C3A (#175) put the version dimension in
the verified `ipc` connect layer and C3B (#177) brought it to the rev1§6 fuzz/proptest bar; C3C
makes negotiation **real on the running storaged↔shell session**: the session's first exchange
selects a wire version via the `ipc` connect codecs, the bespoke storage header's version byte
becomes **dynamic + validated**, and the QEMU smoke witnesses both a negotiated version and a
clean refusal. Branch `c3c-storage-negotiation`, based on `origin/main` @ `9eefb2a` (which already
carries C3B #177 and the cm9b console wiring #176).

C3C is **running-path + storage-crate only — it touches no verified surface.** `cargo verus
verify -p ipc` stays **69/0** and the trusted-base tally stays **14**: zero `ipc/src` files were
edited (the version check reuses C3A's already-verified `ipc::version_ok`), so the verification
inputs are byte-identical to `origin/main`. It performs only the **version+window** step of the
connect handshake; the endpoint-cap funding step and the wire-unification effort stay deferred.

## The load-bearing gotcha: two version namespaces

The single fact that shaped the wiring: **the storage wire version is `2`, a different namespace
from `ipc::PROTOCOL_VERSION` (which is `1`).** The connect codec carries an *offered range* and a
*selected version* but is agnostic about which protocol's version those are — it is the
never-migrating carrier. So the storage handshake must offer/select the **storage** version
(`wire::PROTO_VERSION == 2`). Concretely: **the shell must not use `ipc::ConnectReq::for_window()`**
— that constructor bakes in `ipc::PROTOCOL_VERSION == 1`, which is disjoint from storaged's `[2,2]`
and would be *refused*. Both ends construct `ConnectReq { requested_window, versions:
VersionRange::new(PROTO_VERSION, PROTO_VERSION) }` against `wire::PROTO_VERSION`, the single source
of truth (now a `pub const` in `storage-server/src/wire.rs`).

## What landed

1. **Dynamic, validated version byte** (`storage-server/src/wire.rs`). The fixed
   `HEADER = [0x45,0x51,0x02]` splits into `pub const PROTO_MAGIC: [u8;2] = [0x45,0x51]` and
   `pub const PROTO_VERSION: u8 = 2`. `encode`/`decode` (and the four public wrappers) take a
   version param: `encode` stamps `[magic, magic, version]`; `decode` checks magic exactly →
   `BadHeader`, then `ipc::version_ok(buf[2], negotiated)` → a new **`WireError::Version`** on
   mismatch (distinct from `BadHeader`, refused cleanly, never a panic), then the unchanged
   postcard body + trailing-byte strictness. The 3-byte **layout is unchanged** (rev1§3.7: the
   header never migrates); only the version *value* is dynamic, and the validation is
   dispatch-discipline *outside* any codec, so `ipc::header.rs`'s bijection proofs are untouched.
   This wires C3A's previously-inert `version_ok` helper to its first dispatch site.

2. **storaged pre-serve `admit_connect`** (`user/storaged/src/main.rs`). After the reactor
   registers and before the serve loop, storaged reads the session's first message as a raw
   `ipc::ConnectReq`, runs `admit_connect(&mut Admission::new(WINDOW_BUDGET), VersionRange[2,2],
   payload)` at the single admission point, replies with the encoded `GrantReply`, and records the
   negotiated version. The handshake reuses the **already-registered reactor** (its `register`
   self-signals, so there is no lost-wakeup window) and loops on a bare `RecvErr::Empty` wakeup.
   Every subsequent request decodes / response encodes **at the negotiated version**.

3. **shell connect-once** (`user/shell/src/runtime.rs`). In `_start`, after the cm9b startup-block
   resolution (storage slot, root handle, time page, stdin) and before the REPL, the shell sends
   one `ConnectReq{[2,2], window:0}` over the pre-wired storage channel, decodes the `GrantReply`,
   and stores the selected version in a `static NEGOTIATED_VERSION: AtomicU8`. `request()` reads it
   and threads it through `wire::encode_request`/`decode_response`. A `Refused` reply is a fatal,
   visible exit (single-version build ⇒ no shared version is unrecoverable), never a crash.

4. **Two boot witnesses** (`user/storaged/src/main.rs`). `[storaged] negotiated wire version 2`
   (the happy path), and `[storaged] version-mismatch refused cleanly` — emitted only after a frame
   stamped with `negotiated.wrapping_add(1)` is driven through the **live** `wire::decode_request`
   and actually returns `Err(WireError::Version)`. The line is silent if the decoder ever stops
   checking the version, so the smoke grep has teeth at the running-system level.

5. **Fuzz/test/example threading** (`storage-server/{fuzz,tests,examples}`). The five wire call
   sites pass `wire::PROTO_VERSION`. The `request_dispatch` fuzzer now also explores the stamped
   version byte (any value ≠ 2 is refused cleanly *before* dispatch); `structured_request` round-
   trips at `PROTO_VERSION`; the corpus replay + regression + seed-generator are updated in kind.
   A new wire unit test `version_is_stamped_and_validated` is the anti-theater witness at the codec
   level: a wrong-version frame must decode to `Err(WireError::Version)` (a decoder that ignored
   the version would return `Ok` and fail the test).

## Design choices worth recording

- **The handshake rides the raw `ipc` connect codec, not a storage `Request`.** A versioned-body
  "hello" cannot itself be versioned without a bootstrap version (it is circular); the connect
  codec exists precisely to be the unversioned carrier. So the session's *first bytes* are the
  7-byte `ConnectReq`, distinguishable from a storage frame, and the reply is the 10/1-byte
  `GrantReply` — both transparent payloads over the same channel the serve loop already uses.

- **Per-session version as a storaged-local, not a `Server`/`Session` field.** Validation lives in
  the `wire` layer (`user/storaged/main.rs` calls `wire::decode_request(payload, negotiated)`),
  which is outside `storage_server::Server::handle`. Threading a local `negotiated: u8` is the
  thinnest faithful wiring and keeps the session-relative handle logic untouched; a `Session` field
  would be dead weight the dispatch path never reads.

- **A zero bulk window, honestly.** The inline Read/Write path needs no shared-memory window yet
  (rev1§3.1, deferred), so the shell requests `window: 0`. `admit_connect` still runs the full
  version+window decision against a token `WINDOW_BUDGET`; the grant is recorded but unused, which
  the comment states rather than implying a bulk path that does not exist.

- **`WireError::Version` distinct from `BadHeader`.** A bad magic/proto is a wrong-protocol frame;
  a wrong version is a same-protocol peer at a version we did not negotiate. Keeping them separate
  makes the refusal diagnosable and mirrors C3A's internal `ConnectErr::VersionMismatch`. Not
  conflated with `Response::VersionMismatch`, which is the CAS optimistic-concurrency reply
  (rev1§4.7) — an unrelated meaning.

## Verification (all green)

- `cargo test -p storage-server` → green: the lib unit tests incl. the new
  `version_is_stamped_and_validated`, `roundtrip_and_strictness` (now version-parameterised), the
  20 session integration tests, the corpus replay, and the regression suite.
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p storage-server --test
  fuzz_regressions --test fuzz_corpus` → clean (decode path with the version byte is UB-free).
- `cargo +nightly check --manifest-path storage-server/fuzz/Cargo.toml` → both fuzz targets compile
  with the threaded version param.
- **aarch64 cross-build:** `cd kernel && cargo build` green; storaged and shell each compile under
  the exact `build.rs` flags (`--release --target aarch64-unknown-none-softfloat
  -Zbuild-std=core,compiler_builtins,alloc`).
- **QEMU smoke** (`scripts/run-demo.sh` under the CLAUDE.md perl timeout-harness, 90 s): boot shows
  `store mounted` → `serving` → `negotiated wire version 2` → `version-mismatch refused cleanly`;
  the REPL `write docs/smoke c3c-witness` → `ok`, `cat docs/smoke` → `c3c-witness` (round-trips
  through the negotiated v2 wire), `ls`/`df` work; no panic/`Corrupt`/`FATAL`/`unwrap`. (Exit 124
  is the harness killing QEMU after stdin EOF — the documented green outcome.)
- **Verified surface unchanged:** no `ipc/src` file touched ⇒ `cargo verus verify -p ipc` stays
  **69/0** by construction (verification inputs byte-identical to `origin/main`); trusted-base
  **tally 14**; `ipc::header.rs` proofs untouched. No spec or ledger edit (C3A landed the rev1§8.3
  forward note and the `-p ipc` row).
- `cargo fmt --check` clean for the root workspace and the `storage-server/fuzz`, `user/storaged`,
  `user/shell` workspaces (the workspace-split fmt trap — each formatted via its own manifest).

## Out of scope (recorded, not a gap)

- **Endpoint-cap connect-funding.** C3C performs only version+window on the *pre-wired* channel;
  the client retyping a channel pair and sending an endpoint cap stays deferred (`ipc::session`
  module comment).
- **Wire unification.** storaged keeps its bespoke 3-byte header; migrating it onto the `ipc`
  10-byte `header.rs` is a separate effort. Only the version byte became dynamic.
- **Multi-window sessions / a version bitset / a TLA negotiation model** — all per the C3 plan's
  out-of-scope list; negotiation is the connect-time sequential decision, Verus + proptest-routed
  (the `IpcReactor` model is untouched).
