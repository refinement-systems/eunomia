# Plan — Part C3 detail: multi-version wire negotiation (make rev1§3.7's *"versions are negotiated once at session establishment … a server may speak several concurrently"* true by adding the **version dimension** to the already-**verified** connect layer in `ipc/src/session.rs` — extend the byte-stable `ConnectReq`/`GrantReply` codecs so a connecting client offers a supported-version range and the server selects the highest common version or refuses cleanly, with a pure, Verus-verified `negotiate()` selection and `admit_connect` choosing version *and* window at the **single admission point**; the negotiated version is then stamped into the **existing** header `version` field (`ipc/header.rs`, `storage-server/wire.rs`) and validated per-message at dispatch — so **the header layout never migrates and its bijection proofs are untouched** (the acceptance bar). The connect codecs are the never-migrating layer that makes every other layer migratable (rev1§3.7), and they are *not yet on any running path* (the handshake wiring is independently deferred, `session.rs:68-72`), so extending them now is free. C3 delivers the **negotiation mechanism**, not the future IDL/second-codec-backend or stable public syscall ABI — those stay grouped under the deferred "non-Rust userspace" effort (rev1§8.3:487); minimal now means *negotiate even when only one version is offered*. Re-establish `cargo verus verify -p ipc` ≥ 62/0 with **no new trusted seam** (tally stays 14); the `IpcReactor` TLA model is untouched (negotiation is a connect-time decision, proptest-routed, consistent with the IPC "single-source by design" routing). Whether C3 also wires a thin end-to-end version handshake onto the *running* storage session (the QEMU-smoke witness) — versus delivering+verifying the mechanism in the `ipc` crate and recording the storaged integration as a follow-on — is the one scope decision, surfaced as an Open Decision. Coordinate with **B14** (the IPC reactor/TLA work — C3 touches `session.rs`, B14 touches the reactor/dispatch; no overlap) and **C1** (the `b"EUS1"` named-grant table the shell already resolves its `storage` channel from, `runtime.rs:76-82`).

Detailed, separately-implementable decomposition of **Phase C3** from
`doc/plans/0_address_audit_rev0.md` (parent-plan C3 at `:730-747`). C3 is **Wave-6**
work (`:790`), a spec-deferred Part-C gap that gates nothing: **C-M9** (the console) depends
on C1 + B-IRQ, **C4** (concurrent GC) on B6 — neither on C3. It depends only on **Part A**
being blessed (the rev1§3.7/§3.5 text it conforms to) and on nothing else structurally; the
parent plan's framing is *"Low urgency until a second concurrent version exists"* (`:739`) and
*"mostly mechanism for the future 'non-Rust userspace' effort (rev1§8.3) and can stay minimal
now (negotiate, even if only one version is offered)"* (`:743-744`). It unblocks nothing
downstream, so it is scheduled late for its own sake.

The framing that shapes the whole phase: **the verified connect handshake already exists, but
its wiring is deferred, and it has no version dimension.** `ipc/src/session.rs` is the
canonical, Verus-verified session-establishment layer — `ConnectReq` (`:54-58`), `GrantReply`
(`:60-66`), `Admission` (`:245-329`), and the `admit_connect` server step (`:331-349`) — with
the codecs proven total bijections (`:351-440`) and the quota proven never-over-grant
(`:287-314`). Its own doc comment (`:68-72`) records that *"the client-side connect mechanism
(the endpoint-cap handshake, rev1§3.5) is deferred"* — i.e. the **codecs and admission are
verified, but no running path performs the handshake yet**. Two facts fall out of this and
decide C3's shape:

1. **Version negotiation belongs *in* this verified layer**, not in a bespoke per-server
   "hello": it is the spec's literal "session establishment" point (rev1§3.5), it is already
   the single admission point, and putting it here keeps it verified and shared (Design
   decision 1).
2. **Because the connect codecs are not yet on any running path, extending them now is free** —
   there is no deployed format to migrate. C3 completes the version dimension of the
   still-being-built connect layer while it is still cheap to do so.

The audit found the gap honestly disclosed, not a violation; C3 is **conformance work against
a correct spec** (rev1§3.7 already states the target; C3 makes it true), with a deceptively
small verified surface (a few extra bytes in two codecs + one pure selection function) and one
genuine scope question (whether to wire it end-to-end onto the running storage session, or
deliver+verify the mechanism in the `ipc` crate and defer the storaged integration).

**Closes (from the parent plan / audit).** Parent plan C3 `:731-732`; audit
`doc/results/0_audit_rev0.md` §3.2 `:388-389` (verbatim):

> **Multi-version wire negotiation** (rev0§3.7): the protocol is at version 2 but
> both peers ship from one tree; no negotiation is implemented.

Labeled **[confirmed-deferred]** under audit §3.2 (`:379` *"Spec-deferred (disclosed in
rev0§8.3) — present as gaps but not violations"*). Plus the parent-plan C3 work obligation
(`:740-744`): *"implement version negotiation at session establishment so a server can speak
several versions concurrently; keep the fixed header (the never-migrating layer) untouched.
This is mostly mechanism for the future 'non-Rust userspace' effort (rev1§8.3) and can stay
minimal now (negotiate, even if only one version is offered)."*

Adjacent doc fix (free, while in the file): audit §1.3 `:112-113` — *"`ipc/src/wire.rs:3`
calls the header 'Kani-verified'; it is now Verus-verified."* C3 touches the IPC wire neighborhood;
correct any remaining stale "Kani" labels it passes (the header is Verus-verified, `header.rs`).

Three scope notes, all load-bearing for C3's boundary:

- **The header layout never migrates; C3 adds *no* header field.** rev1§3.7 (`:196`): *"The
  header layout never migrates — it is the layer that makes every other layer migratable."*
  Both headers already carry a `version` field — `ipc/header.rs` (`proto`,`version`,`opcode`,
  `flags`,`body_len`, `:37-44`) and `storage-server/wire.rs` (the `0x02` byte, `:16`). C3 uses
  the existing field: negotiate at connect, *stamp* the agreed version into the header on every
  subsequent message, *validate* it at dispatch. The header's `encode`/`decode`/bijection
  lemmas (`header.rs:89-182`) **do not change** — *"header codec proofs unchanged"* is the
  parent-plan acceptance bar (`:746`).
- **Negotiation rides the never-migrating connect codecs, not the versioned body.** The thing
  that negotiates the version cannot itself be versioned. The `ipc` connect forms
  (`ConnectReq`/`GrantReply`) are deliberately *"fixed, hand-written little-endian codecs …
  boring and byte-stable"* (`session.rs:11-13`) — the never-migrating connect layer, exactly
  the right carrier for a version handshake. C3 extends *these*, not the postcard message body
  (Design decision 1).
- **C3 is the negotiation *mechanism*; the IDL backend and stable public ABI stay deferred.**
  rev1§8.3 (`:487`) groups *"IDL-based wire encoding and a stable public syscall ABI … as one
  future 'non-Rust userspace' effort … add a second codec backend, and bump protocol versions
  so old and new clients coexist per session."* C3 delivers the **"bump protocol versions so
  old and new clients coexist per session"** half — the negotiation mechanism — and leaves the
  IDL/second-codec-backend and stable syscall ABI deferred (Out of scope). Minimal now:
  negotiate correctly even though exactly one version (2) is offered today.

---

## Spec target — Part A is blessed; C3 makes one small edit on landing

Every citation is `rev1§` against the already-blessed text. C3 touches the verified surface in
exactly one place (the `ipc` connect codecs + a new pure selection function) and **adds no
trusted seam** — the extension is *verified*, not trusted, so the ledger tally is unchanged.

- **rev1§3.7 — the wire protocol & versioning** (`spec_rev1.md:194-200`, the load-bearing
  sentence at `:196`): *"Every message begins with a **fixed, hand-defined header**: protocol
  id, version, opcode, flags, and body length. **Versions are negotiated once at session
  establishment**; an unknown opcode yields an error reply, never a crash; a breaking change is
  a new version number, and **a server may speak several concurrently**. The header layout
  never migrates — it is the layer that makes every other layer migratable."* The blessed text
  already specifies negotiation; today it does not happen (both peers ship one version from one
  tree, `storage-server/wire.rs:13-15`). C3 makes §3.7 true. **No text change** — C3 conforms
  to the already-correct claim.
- **rev1§3.5 — sessions & the connect request** (`spec_rev1.md:174-182`, connect at `:176`):
  *"Servers publish a connection endpoint, and the client funds the session: it retypes a
  channel pair from its own untyped (§3.2) and **sends one endpoint in the connect request**,
  together with a requested bulk-window size (§3.1) that the server grants or refuses against
  its **total window budget** at this single admission point."* C3 adds the **version** to that
  same connect request and the same single admission point: `admit_connect` selects *version
  and window together*, refusing if either fails (Design decision 1/4). **No text change.**
- **rev1§2.7 / §3.7 — the untrusted-decode discipline** (`spec_rev1.md:125-135`, `:194-200`):
  rev1§2.7 (`:129-133`) makes the syscall boundary apply *"the same untrusted-decode discipline
  the wire protocol applies to IPC message opcodes (§3.7)"* — *"An unrecognized opcode returns
  an error, never a crash,"* *"Every argument is validated against ground truth before use,"*
  *"Decode is total over arbitrary arguments."* The new `ConnectReq`/`GrantReply` fields decode
  totally (refuse-not-crash, like every codec in `session.rs`), and a message whose stamped
  `version` does not match the session's negotiated value is **refused, never a crash** (Design
  decision 3). **No text change; C3 conforms.**
- **rev1§8.3 — deferred / future work** (`spec_rev1.md:471-491`). Two items frame C3's
  boundary:
  - **`:487`** *"IDL-based wire encoding and a stable public syscall ABI, grouped as one future
    'non-Rust userspace' effort … add a second codec backend, and **bump protocol versions so
    old and new clients coexist per session**."* C3 delivers the negotiation mechanism (the
    bolded clause). **Edit on landing:** add a forward note that the *negotiation mechanism* is
    implemented as of C3, leaving the IDL second-codec-backend and the stable public syscall
    ABI deferred — so the item shrinks to "an IDL backend + stable ABI over the now-existing
    negotiation." This is the single normative touch.
  - **`:477`** *"Multi-window sessions (grow-only) … the descriptor's window-index field is
    reserved now (§3.1)."* C3 leaves multi-window deferred; `WindowGrant.window` stays `0`
    (`session.rs:45-52`). Mentioned only to scope C3 *against* it (Out of scope).
- **rev1§6 / §6.1 — the verified surface & the IPC seam** (the ledger row, `verus_trusted-base.md:189`):
  *"IPC header + session codecs + reactor bit-allocator core | `cargo verus verify -p ipc` | 62
  verified, 0 errors."* The connect codecs are already in the verified surface; C3 **extends
  them** (more codec bytes + a pure `negotiate`) and re-establishes ≥ 62/0. **Edit on landing:**
  the ledger Baselines `-p ipc` row records the new total; the IPC seam description is unchanged
  (no new `external_body`/`assume_specification`), so the **tally stays 14** (no new trusted
  seam). The lone trusted facts (model.rs Loom/Shuttle as the concurrency oracle, TLA as the
  protocol oracle, `:86-87`) are untouched.

---

## What is actually true today — a verified-but-unwired connect layer, two headers, zero negotiation

The inventory that shapes the phase.

### The connect handshake is verified in `ipc/src/session.rs` but its wiring is deferred

- **The codecs are verified, byte-stable bijections.** `ConnectReq { requested_window: u32 }`
  (`:54-58`, tag `TAG_REQ = 0xC0`, `REQ_LEN = 5`) and `GrantReply::{Grant(WindowGrant{window,
  size}), Refused}` (`:60-66`, tags `0x01`/`0x00`, `GRANT_LEN = 9`/`REFUSED_LEN = 1`), with
  ghost models (`req_encode`/`req_decode` `:83-105`, `grant_encode`/`grant_decode` `:107-141`),
  exec codecs (`:143-237`), and **four ∀ round-trip lemmas** (`lemma_req_decode_encode`,
  `lemma_req_encode_decode`, `lemma_grant_decode_encode`, `lemma_grant_encode_decode`,
  `:351-440`) — total bijections proven by `bit_vector`.
- **The admission decision is verified never-over-grant.** `Admission { budget, granted }`
  (`:245-329`) with `well_formed: granted <= budget` (`:256-258`) a pre/post-condition of every
  `admit`/`release`, so `remaining()` never underflows for *any* connect flood (`:287-314`, the
  unbounded theorem). `admit_connect(adm, req_bytes) -> GrantReply` (`:331-349`) is the pure
  server step: decode → admit → reply, refusing a malformed request.
- **But no running path performs the handshake.** The module comment (`:68-72`) is explicit:
  *"the client-side connect mechanism (the endpoint-cap handshake, rev1§3.5) is deferred, so its
  richer errors … are not yet constructed. They return when that mechanism lands."* `ConnectErr`
  has one variant, `Refused` (`:73-77`). So C3 builds on a layer whose **codecs+admission are
  proven but whose handshake wiring is independently future work** — directly relevant to the
  C3C scope decision.
- The layer stays in the default `no_std` build (no postcard, no `alloc`) — fixed mask/shift
  codecs, `vstd` ghost-only (`:24-27`). Any C3 addition must preserve this.

### Two headers, both with a `version` field, neither negotiated

- **`ipc/src/header.rs` — the spec header** (rev1§3.7): a 10-byte fixed layout `proto:u8`,
  `version:u8`, `opcode:u16`, `flags:u16`, `body_len:u32` (`HEADER_SIZE = 10` `:35`, struct
  `:37-44`), with Verus-verified `encode` (`:89-108`), total `decode` (`:112-129`), and bijection
  lemmas `lemma_decode_encode` (`:134-152`) / `lemma_encode_decode` (`:158-182`). `proto`/`version`
  are **carried, not validated** here — `wire::encode(proto, version, opcode, flags, body)`
  (`wire.rs:60-79`) takes them as parameters; the *server* validates at dispatch (rev1§3.7).
  Field validation is exactly where C3's per-message version check belongs.
- **`storage-server/src/wire.rs` — the bespoke storage header** (the one the running
  storaged↔shell path actually uses): a 3-byte `HEADER = [0x45, 0x51, 0x02]` (`:16`) — magic
  `'E'`, protocol `0x51`, **version `2`** — whose own comment states the gap (`:13-15`): *"both
  peers ship from this tree, so no multi-version negotiation is implemented yet."* `encode`
  prepends it (`:27-36`); `decode` does an **exact-match** `buf[..3] != HEADER → BadHeader`
  (`:38-47`, check at `:39`) — a hardcoded version, no negotiation, no fallback. `MAX_MSG = 256`
  (`:25`). This 3-byte header is **not** the `ipc` 10-byte header — the running path has its own
  wire (the audit's "version 2" is *this* byte). Whether C3 makes this byte dynamic+negotiated
  is the C3C scope decision (Design decision 5).

### The running path pre-wires sessions — there is no connect handshake on the wire

- **init pre-wires the channel.** init retypes the channel pair and `cap_install`s each end into
  storaged's and the shell's cspaces (the storaged↔shell session is established by capability
  installation, not a connect request); storaged's server side opens a pre-populated session via
  `Store::open_session(grants)` (`storage-server/src/lib.rs:395-404`), closed on peer-close
  (`close_session` `:406-408`). The `Request` enum (`:112-248`) has **no** `Connect`/`Hello`
  variant.
- **The shell client loop** (`user/shell/src/runtime.rs:117-135`): `request()` =
  `wire::encode_request` → `chan_send` (retry on `ERR_FULL`) → `chan_recv` → `wire::decode_response`.
  No version exchange; the storage channel slot is resolved from the C1 `b"EUS1"` named-grant
  table (`storage` → slot, `root` → handle, `:76-93`).
- **The storaged serve loop** (`user/storaged/src/main.rs:260-284`): `reactor.wait()` →
  drain `ep.recv_nb` → `wire::decode_request` → `server.handle` → `wire::encode_response` →
  `send_response`. A decode failure becomes `ErrorCode::Internal` (`:274`). No connect step, no
  per-message version check.
- So *"a session negotiates a version explicitly"* (parent acceptance `:745`) has no place to
  happen on the running path today — there is no session-establishment exchange. Making it real
  end-to-end means **adding a connect step to the storage session** (C3C); proving the mechanism
  correct does not (C3A/C3B).

### Verification coverage today (the baseline C3 must hold or raise)

- **Verus:** `cargo verus verify -p ipc` → **62/0** (ledger `:189`; header bijections + session
  codecs/admission + the B14B `lowest_clear_bit` reactor core). Trusted-base **tally 14**, no
  IPC `external_body`/`assume_specification` (`:86-87`).
- **Fuzz:** `ipc/fuzz/fuzz_targets/wire_decode.rs` (+ `ipc/tests/fuzz_corpus.rs` replay) — the
  body codec is total/round-trip-stable; `storage-server/fuzz/fuzz_targets/{request_dispatch,
  structured_request}.rs` — storage request decode/dispatch never panics, round-trips.
- **Model / TLA:** `ipc/src/model.rs` Loom/Shuttle harnesses (`rig_smoke`, `fifo_no_drop`,
  `reactor_no_lost_wakeup`, `full_backpressure_no_drop`); `tla/ipc_reactor/IpcReactor.tla` — the
  reactor protocol, **39 distinct states**, with **three committed negative controls** (`NegControl`,
  `NegBackpressure`, `NegLostWakeup`) (ledger `:193`). IPC dispatch is **"single-source by design"**
  — the multi-source dispatch and cap-marshalling are *proptest-routed*, live concurrency is
  *Loom/Shuttle-routed*, **not** TLA-mechanized (`:193`). Negotiation, a connect-time decision,
  follows that routing: Verus + proptest, no new TLA (Design decision 4 / Verification tier).

---

## Primary files (current line numbers)

- **`ipc/src/session.rs`** — the verified connect layer, C3's core (C3A). `ConnectReq` `:54-58`,
  `GrantReply`/`WindowGrant` `:45-66`, `ConnectErr` `:73-77`, tags/lens `:37-43`, ghost codecs
  `:83-141`, exec codecs `:143-237`, `Admission` `:245-329`, `admit_connect` `:331-349`, bijection
  lemmas `:351-440`, the deferred-wiring comment `:68-72`, tests `:444-533`.
- **`ipc/src/header.rs`** — the spec header (**unchanged**; cite for "proofs unchanged"). Struct
  `:37-44`, `encode` `:89-108`, `decode` `:112-129`, bijection lemmas `:134-182`.
- **`ipc/src/wire.rs`** — `encode`/`decode` carry `proto`/`version` as params `:60-79` (the
  stamping site; **unchanged** layout). Fix the stale "Kani" doc label if present (audit `:112-113`).
- **`ipc/src/lib.rs`** — re-export any new public items (`negotiate`, a `Version`/range type).
- **`ipc/src/reactor.rs`** — **unchanged**; the negotiation runs as a normal connect step inside
  the reactor loop (`register` `:196-220`, `wait` `:266-281`).
- **`ipc/fuzz/fuzz_targets/`**, **`ipc/tests/fuzz_corpus.rs`** — extend connect-codec fuzz/replay
  (C3B): the widened `ConnectReq`/`GrantReply` decoders stay total.
- **`storage-server/src/wire.rs`** — the bespoke storage header (C3C, *only if in scope*):
  `HEADER` `:16`, comment `:13-15`, `encode`/`decode` `:27-47`. C3C makes the version byte
  dynamic+validated; the 3-byte layout is otherwise unchanged.
- **`storage-server/src/lib.rs`** — `open_session` `:395-404`, `close_session` `:406-408`,
  `Request`/`Response` `:112-248`/`:259-311` (C3C: per-session negotiated version; **no** new
  `Request` variant — the handshake rides the `ipc` connect codecs, not the postcard body).
- **`user/shell/src/runtime.rs`** — `request()` `:117-135`, grant resolution `:76-93` (C3C:
  connect-once-at-startup, stamp the negotiated version).
- **`user/storaged/src/main.rs`** — serve loop `:260-284` (C3C: an `admit_connect` step before
  the serve loop; per-message version validation).
- **`doc/spec/spec_rev1.md`** — the single edit on landing (rev1§8.3:487 forward note).
- **`doc/guidelines/verus_trusted-base.md`** — Baselines `-p ipc` row `:189` (new verify total);
  **no seam row changes** (tally stays 14).
- **`scripts/run-demo.sh`** — the QEMU smoke gate; C3C adds a negotiation + refusal witness.

---

## Verification tier & baseline (applies to all sub-phases)

C3's substance is small but it is **decoder + pure-decision** work, so it lands squarely in the
Verus tier (parent plan `:32-34`, *"no logic change lands without its verification tier"*;
rev1§6 routing: *"decoders get fuzz targets, … chokepoints get Verus, … everything gets
Miri+proptest"*):

- **The widened connect codecs are Verus tier AND fuzzed.** `ConnectReq`/`GrantReply` grow a
  small fixed number of bytes (the offered version range; the selected version — Design decision
  2). Their ghost models, exec codecs, and the **four ∀ bijection lemmas** (`session.rs:351-440`)
  extend to the new bytes and re-prove (the `bit_vector` pattern is identical — more fields, same
  shape). The connect-decode fuzz/replay (`ipc/fuzz`, `ipc/tests/fuzz_corpus.rs`) gains seeds for
  the widened forms; decode stays **total** (refuse-not-crash, rev1§3.7/§2.7).
- **`negotiate()` is Verus tier (a pure decision).** A small `requires`-free `ensures`: the
  result is `Some(v)` iff the client and server version ranges overlap, with `v` the **highest
  common** version (`>=` both mins, `<=` both maxes, and `>=` any other common version); `None`
  iff disjoint (Design decision 4). `admit_connect` (extended) preserves the `Admission`
  invariant unchanged and refuses if *either* version selection or window admission fails.
- **The per-message version check is dispatch-discipline (proptest/fuzz).** A message whose
  stamped header `version` ≠ the session's negotiated value is **refused, never a crash** (rev1§2.7
  `:131`); covered by proptest + (in C3C) the storage request fuzz corpus.
- **Negotiation is proptest-routed, not TLA-mechanized.** Consistent with the IPC "single-source
  by design" routing (ledger `:193`): the connect-time selection is a sequential decision, fully
  covered by Verus (`negotiate`, the codecs) + a proptest with a **negative control** (a server
  that ignores version-mismatch and serves at the wrong version must make the test fail — the
  project's anti-theater habit). The `IpcReactor` TLA model and `ipc/src/model.rs` Loom/Shuttle
  harnesses are **untouched** (C3 changes neither the reactor nor the concurrency shape).
- **The `cargo fmt` workspace-split trap applies.** `ipc`/`storage-server` format via the root;
  `ipc/fuzz`/`storage-server/fuzz` via their fuzz manifests; `user/shell`/`user/storaged` (C3C)
  via their own manifests (CLAUDE.md "Formatting").

**Baseline to re-establish at end of C3:**

- `cargo verus verify -p ipc` ≥ **62/0** (higher, with the widened-codec lemmas + `negotiate`);
  trusted-base **tally 14** (no new seam); `header.rs` proofs **unchanged** (no header edit) —
  the explicit acceptance bar (parent `:746`).
- `cargo test -p ipc` green: existing unit tests + std harnesses + the new negotiation tests.
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p ipc` clean over the new codecs
  and `negotiate`.
- `ipc` connect-codec fuzz corpus + the storage request corpus replay UB-free under Miri
  (`--test fuzz_corpus` / `--test fuzz_regressions`).
- The `IpcReactor` TLA model (39 states, 3 negative controls) and the Loom/Shuttle harnesses
  unchanged; `-p kcore`/`-p cas`/`-p dma-pool`/`-p urt` Verus totals unchanged (C3 touches none).
- **If C3C is in scope:** `cargo test -p storage-server` green; `scripts/run-demo.sh` boots green
  under the CLAUDE.md timeout-harness (`[storaged] store mounted` → `serving`) and the negotiation
  witness behaves (an explicit version is selected; a version-mismatch witness is refused cleanly,
  no panic/`Corrupt`).

---

## Design decision 1 — where negotiation lives: extend the **verified** `ipc` connect layer, not a per-server hello *(resolve in C3A)*

rev1§3.7 (`:196`) puts negotiation *"at session establishment"*; rev1§3.5 (`:176`) makes that the
connect request. The `ipc` crate already owns that point, verified.

- **Adopted — add the version dimension to `ConnectReq`/`GrantReply`/`admit_connect` in
  `session.rs`.** The client offers its supported versions in the `ConnectReq`; the server picks
  one (or refuses) in `admit_connect`, returning the selected version in `GrantReply::Grant`.
  This is *the* single admission point (already there), it keeps negotiation **verified and
  shared** by every server, and it rides the never-migrating connect codecs (rev1§3.7) — the one
  layer that can carry a version handshake because it is itself unversioned. Because the connect
  codecs are not yet on any running path (`session.rs:68-72`), widening them now costs no
  migration.
- **Rejected — a bespoke per-server `Request::Hello`/`Response::Hello` (e.g., in the storage
  protocol).** It duplicates the mechanism per server, is unverified (postcard body, not the
  verified connect codecs), and is **circular**: a versioned-body hello cannot itself be
  versioned without a bootstrap version. The connect codecs exist precisely to avoid this.
- **Rejected — a new header field.** Violates *"the header layout never migrates"* (rev1§3.7) and
  would break the `header.rs` bijection proofs — the opposite of the acceptance bar. The header's
  existing `version` field already carries the per-message version (Design decision 3).

**Recommendation: extend `ConnectReq`/`GrantReply`/`admit_connect` in `ipc/src/session.rs`; the
connect codecs are the verified, never-migrating carrier the spec intends.**

---

## Design decision 2 — version representation: a contiguous `[min,max]` range offered, one version selected *(resolve in C3A)*

rev1§3.7 (`:196`): *"a breaking change is a new version number, and a server may speak several
concurrently."* Versions are monotone `u8`s; the question is how a client advertises "several."

- **Adopted — `ConnectReq` carries a `[min_version, max_version]` range; `GrantReply::Grant`
  carries the single selected `version`.** Two extra bytes in the request (REQ_LEN 5→7), one in
  the grant (GRANT_LEN 9→10). A contiguous span matches the monotone "new version number"
  framing and rev1§8.3's *"old and new clients coexist"* (`:487`) — a server speaking versions
  *N* and *N+1* is the realistic case. `negotiate` = `min(client_max, server_max)` when the
  ranges overlap, else refuse (Design decision 4). The bijection proofs extend trivially (more
  little-endian bytes, same `bit_vector` shape). Server-side, the server's own `[min,max]` is a
  constant it holds (today `[2,2]`).
- **Rejected — a version *bitset* (u32/u64) for non-contiguous support** (e.g., speak v1 and v3
  but not a withdrawn v2). Strictly more general and a literal reading of "several concurrently,"
  but **YAGNI now** — there is exactly one version, and a withdrawn-middle-version scenario is
  hypothetical. The range is the minimal verified addition; if non-contiguous support is ever
  needed, the bitset is a forward, append-only widening of the same connect codec. Recorded as
  the more-general alternative (Out of scope).
- **Rejected — a single offered version (no range).** Cannot express "a client that understands
  v2 or v3," so it cannot demonstrate negotiation at all (the acceptance criterion). The range is
  the minimum that makes negotiation real even with one version deployed.

**Recommendation: a `[min,max]` range in `ConnectReq`, a single selected `version` in
`GrantReply::Grant`; note the bitset as the forward-compatible generalization.**

---

## Design decision 3 — enforcement: stamp the negotiated version into the **existing** header field, validate at dispatch *(resolve in C3A; wired in C3C)*

Once negotiated, every message must be at the agreed version, and a mismatch must be refused, not
crash — without touching the header layout.

- **Adopted — the negotiated version is written into the header's existing `version` field on
  every message; the server validates `header.version == session.negotiated` at dispatch (with
  `proto`/`opcode`, rev1§3.7).** The header `encode`/`decode`/bijection proofs (`header.rs:89-182`)
  are **unchanged** — only the *value* in the field varies and a *validation step* is added
  *outside* the codec (dispatch-discipline, rev1§2.7 `:131-132`). A mismatch returns a clean
  error reply (in the storage path, `ErrorCode` per `storaged main.rs:274`), never a panic. This
  keeps *"the header layout never migrates"* and *"header codec proofs unchanged"* literally true.
- **Rejected — validate inside the header codec.** Would couple the never-migrating header to a
  per-session value and force the bijection lemmas to carry session state — exactly what
  rev1§3.7 forbids and the acceptance bar protects against.

**Recommendation: stamp the negotiated version into the existing header `version` field; validate
it at dispatch as untrusted-decode discipline; leave the header codec and its proofs untouched.**

---

## Design decision 4 — the verified selection: `negotiate()` picks the highest common version or refuses *(resolve in C3A)*

The decision rev1§3.7 (`:196`) names: pick a version both speak, else a clean refusal.

- **Adopted — a pure `negotiate(client: VersionRange, server: VersionRange) -> Option<u8>`,
  Verus-verified.** `ensures`: result `is Some(v)` ⟺ the ranges overlap (`client.min <=
  server.max && server.min <= client.max`); when `Some(v)`, `v == min(client.max, server.max)`
  and `v` is `>=` both mins, `<=` both maxes, and `>=` every other common version (highest
  common); `None` ⟺ disjoint. `admit_connect` calls `negotiate` first, then `Admission::admit`
  for the window: refuse if **either** fails, returning `GrantReply::Refused`; otherwise
  `GrantReply::Grant { window: 0, size, version }`. The `Admission` never-over-grant invariant
  (`session.rs:287-314`) is preserved verbatim — version selection is orthogonal accounting-wise.
- **Rejected — pick the client's max (ignore server overlap).** Lets a client force a version the
  server cannot speak — the bug negotiation exists to prevent. The server must select within its
  own range.
- **Rejected — fold version-mismatch into the window-quota `ConnectErr::Refused` with no
  distinction.** Acceptable on the wire (a refusal is a refusal), but internally a
  `ConnectErr::VersionMismatch` arm (alongside `Refused`, `session.rs:73-77`) aids diagnosability
  and a future client that should *not* retry on a version refusal. The wire `GrantReply` may stay
  a single `Refused` (minimal) or gain a one-byte reason — see Open Decision 3.

**Recommendation: a pure, Verus-verified `negotiate()` returning the highest common version or
`None`; `admit_connect` refuses on version-or-window failure; add an internal
`ConnectErr::VersionMismatch` arm.**

---

## Design decision 5 — running-path integration scope: a thin end-to-end witness vs mechanism-in-`ipc` only *(resolve in C3C — Open Decision 1)*

The one genuine scope question. The running storage session **pre-wires** and uses the **bespoke
3-byte wire** with a hardcoded version (`storage-server/wire.rs:39`), and the `ipc` connect
handshake **wiring itself is deferred** (`session.rs:68-72`). So demonstrating *"a session
negotiates a version explicitly"* end-to-end means **adding a connect step to the storage
session** — bounded, but real.

- **Option A (Adopted as recommended) — wire a thin version handshake onto the storage session
  for a QEMU-smoke witness.**
  - The storage session's **first exchange** uses the `ipc` connect codecs directly over the
    pre-wired channel (raw, version-independent `ConnectReq`/`GrantReply` bytes — *not* a new
    `Request` variant, which would be circular): the shell sends `ConnectReq{[min,max], window}`
    once at startup, storaged answers with `admit_connect` (version + window), and both sides
    record the negotiated version. This performs the **version+window** step of the connect
    handshake; the **endpoint-cap funding** step stays deferred (`session.rs:68-72`) — an honest
    subset.
  - `storage-server/wire.rs`'s version byte becomes **dynamic + validated**: `encode` stamps the
    negotiated version; `decode` checks magic+proto exactly and the version against the negotiated
    value, refusing a mismatch cleanly (replacing the exact-match `:39`). The 3-byte **layout is
    unchanged**.
  - storaged gains a pre-serve `admit_connect` step (before the loop at `main.rs:260`) and a
    per-message version check; the shell connects once before the REPL (`runtime.rs:117-135`) and
    stamps the negotiated version. init is unchanged (the handshake rides the existing pre-wired
    channel — the first messages on it).
  - **Why recommended:** the parent acceptance reads like a real session (*"a session negotiates
    a version explicitly; an unsupported version is refused cleanly"* `:745-746`), the QEMU smoke
    is the project's integration gate, and the parent plan lists *"the servers' connect handlers"*
    among C3's touches (`:735`). It is genuinely thin (no cap-funding, no wire unification) and
    keeps risk at the parent's "S–M / low."
- **Option B — deliver+verify the mechanism in the `ipc` crate; defer the storaged integration.**
  C3A/C3B only: the negotiation mechanism is verified and demonstrated by the `ipc` proptest/model;
  the storaged adoption (and the bespoke-wire→`ipc`-header unification) is recorded as a follow-on,
  since the connect-handshake wiring is independently deferred (`session.rs:68-72`). Acceptance is
  met at the verified-mechanism level. Lower surface, but no running-system witness.
- **Rejected — full connect-handshake wiring (endpoint-cap funding) + migrating storaged onto the
  `ipc` 10-byte header.** That is the deferred connect mechanism (`session.rs:68`) plus a wire
  unification — well beyond C3's S–M billing and orthogonal to *version* negotiation. Out of scope
  either way.

**Recommendation: Option A — include a thin end-to-end witness (the storage session's first
exchange negotiates version+window via the `ipc` connect codecs; the storage version byte becomes
dynamic+validated), explicitly *not* building cap-funding or wire unification. Fall back to Option
B if the storaged startup-state change proves more entangled than the witness is worth. This is
Open Decision 1.**

---

## Sub-phase C3A — version negotiation in the verified connect layer *(must-do; the core; no running-path change)*

The mechanism, entirely within `ipc` and verified: widen the connect codecs (Design decisions
2/4), add `negotiate` (DD4), add the per-message version-validate helper (DD3). Pure addition
behind the connect API — no server or running-path change, so it lands and verifies in isolation.

- **Touches:** `ipc/src/session.rs` — a `VersionRange` (or `min/max` fields) on `ConnectReq`
  (`:54-58`), a selected `version` on `GrantReply::Grant` / `WindowGrant` (`:45-66`); extend the
  ghost codecs (`:83-141`), exec codecs (`:143-237`), and the **four bijection lemmas**
  (`:351-440`) to the new bytes (bump `REQ_LEN`/`GRANT_LEN`, `:41-42`); add `fn negotiate(...)`
  (Verus-verified) and a `ConnectErr::VersionMismatch` arm (`:73-77`); extend `admit_connect`
  (`:331-349`) to select version then window. A small `fn version_ok(header_version, negotiated)
  -> bool` helper (or inline at the future dispatch site) for DD3. `ipc/src/lib.rs` — re-export
  the new public items. **No** `header.rs` change (DD3), **no** reactor change.
- **Depends on:** Part A blessed (rev1§3.7/§3.5 text). No intra-C3 dependency.
- **Work:**
  1. Add the offered range to `ConnectReq` and the selected version to `GrantReply::Grant`;
     bump the lengths; extend ghost+exec codecs.
  2. Re-prove the four ∀ bijection lemmas over the widened forms (same `bit_vector` pattern).
  3. `negotiate(client, server) -> Option<u8>` with the highest-common `ensures` (DD4); unit
     tests incl. disjoint (refuse), nested, touching, and single-version (`[2,2]` vs `[2,2]` → 2).
  4. Extend `admit_connect`: `negotiate` first, then `admit`; refuse on either failure;
     `ConnectErr::VersionMismatch` internally. Confirm the `Admission` invariant proofs are
     untouched.
  5. The `version_ok` dispatch helper (inert until C3C wires it) + its unit tests.
- **Acceptance:**
  - `cargo verus verify -p ipc` ≥ 62/0 (higher, with the widened lemmas + `negotiate`);
    `header.rs` proofs unchanged; tally 14 (no new seam).
  - `cargo test -p ipc` green incl. the negotiation unit tests; Miri clean over the new codecs.
  - The connect codecs stay total (a malformed widened `ConnectReq`/`GrantReply` decodes to
    `None`, never a crash).
- **Effort/Risk:** S–M / low. The codec widening is mechanical given the existing proof pattern;
  `negotiate` is a small pure function.

---

## Sub-phase C3B — verification tier: fuzz, proptest, negative control *(must-do; closes the theater gap)*

Bring the new surface to the rev1§6 bar beyond Verus: the widened decoders get fuzz seeds, and
negotiation gets a proptest with a negative control (the project's anti-theater discipline).

- **Touches:** `ipc/fuzz/fuzz_targets/` (extend the connect/wire decode target with the widened
  `ConnectReq`/`GrantReply`), `ipc/tests/fuzz_corpus.rs` (replay), `ipc/src/session.rs` tests
  (the negotiation proptest) or a new `ipc/tests/` file.
- **Depends on:** C3A.
- **Work:**
  1. Fuzz: arbitrary bytes through the widened `ConnectReq::decode`/`GrantReply::decode` stay
     total (refuse-not-crash); round-trip stable on accepted inputs. Add seeds for the new forms.
  2. **Negotiation proptest:** random client/server `[min,max]` ranges → `negotiate` returns the
     highest common version iff overlapping, `None` iff disjoint; `admit_connect` refuses when
     version *or* window fails and grants (at the selected version) otherwise, never over-granting
     across a sequence. Run under `cfg(miri)` caps like the rest.
  3. **Negative control:** a deliberately-wrong oracle — a `negotiate` that returns the client's
     max ignoring server overlap, or a dispatch that skips `version_ok` — **must** make the
     proptest fail (proving the test has teeth).
  4. Confirm the storage request fuzz corpus (`storage-server/fuzz`) still replays UB-free
     (C3A changed nothing there; C3C will extend it).
- **Acceptance:**
  - The widened connect decoders are fuzzed and total; the negotiation proptest is green and its
    negative control fails; Miri clean.
- **Effort/Risk:** S / low.

---

## Sub-phase C3C — end-to-end witness: the storage session negotiates *(scope-gated; Open Decision 1; recommended thin inclusion)*

Make negotiation real on the running storaged↔shell path (Design decision 5, Option A): the
session's first exchange selects a version via the `ipc` connect codecs, and the storage header's
version byte becomes dynamic+validated. **Build only if Open Decision 1 chooses Option A.**

- **Touches:** `storage-server/src/wire.rs` — `encode`/`decode` (`:27-47`) take/stamp/validate a
  version param (the 3-byte layout unchanged; the exact-match `:39` becomes magic+proto exact +
  version-against-negotiated); `storage-server/src/lib.rs` — per-session negotiated version
  alongside `open_session` (`:395-404`); `user/storaged/src/main.rs` — an `admit_connect` step
  before the serve loop (`:260`) + per-message `version_ok`; `user/shell/src/runtime.rs` —
  connect-once at startup, stamp the negotiated version (`:117-135`);
  `storage-server/fuzz/fuzz_targets/` — cover the dynamic version byte; `scripts/run-demo.sh` —
  the witness. **No** new `Request` variant (the handshake rides the `ipc` connect codecs); init
  unchanged.
- **Depends on:** C3A (the codecs + `negotiate` + `version_ok`). Independent of C3B.
- **Work:**
  1. Storage wire: thread the version through `encode`/`decode`; validate magic+proto exactly and
     version against the session's negotiated value, refusing a mismatch cleanly (not `BadHeader`
     panic-adjacent — a clean `WireError`/`ErrorCode`). Update the `:13-15` comment.
  2. storaged: do `admit_connect` on the first message of a fresh session (server range `[2,2]`),
     store the negotiated version per session, validate every subsequent request's version; reply
     `GrantReply` via the `ipc` codec.
  3. shell: send `ConnectReq{[2,2], window}` once before the REPL, decode the `GrantReply`, store
     the negotiated version, stamp it on every `request()`.
  4. Fuzz: the storage request corpus exercises the dynamic version byte (a wrong version →
     refused, never a crash); promote any crash to a regression test.
  5. The `scripts/run-demo.sh` witness: boot shows an explicit negotiated version in the log; a
     synthetic version-mismatch (a client offering a disjoint range, or a crafted wrong version
     byte) is **refused cleanly** — no panic/`Corrupt`; normal `write`/`cat`/`ls` still work at
     the negotiated version.
- **Acceptance:**
  - `cargo test -p storage-server` green (wire round-trips at a negotiated version; a mismatch is
    refused); the storage request fuzz corpus replays UB-free under Miri.
  - `scripts/run-demo.sh` boots green under the timeout-harness; the session negotiates version 2
    explicitly and a version-mismatch witness is refused cleanly.
  - The endpoint-cap funding step is **not** built (stays deferred, `session.rs:68-72`); the
    `ipc` 10-byte header is **not** adopted by storaged (no wire unification).
- **Effort/Risk:** M / low–medium. The care is the storaged startup state (a connect step before
  the serve loop) and keeping the smoke green; the codec/selection it calls is already verified.

---

## Execution order

```
C3A  version negotiation in the verified ipc connect layer   [core; no running-path change]
       │
       ├─► C3B  fuzz + negotiation proptest + negative control
       │
       └─► C3C  end-to-end storage witness   [scope-gated — Open Decision 1; recommended thin]
```

**C3A is the prerequisite.** **C3B and C3C both depend only on C3A and are mutually
independent** (C3B is the verification tier; C3C is the running-path witness, built only if Open
Decision 1 chooses Option A). C3 as a whole depends only on Part A being blessed. **Coordinate
with B14** (same crate, disjoint files: C3 = `session.rs`, B14 = `reactor.rs`/dispatch/TLA — no
overlap; both must leave `-p ipc` green) and **C1** (the shell's `storage` channel comes from the
`b"EUS1"` named-grant table, `runtime.rs:76-82`, already landed).

The cleanest **landing discipline:** C3A first (re-run `cargo verus verify -p ipc`, confirm ≥
62/0 and `header.rs` proofs unchanged before moving on); then C3B (the fuzz + negative-control
proptest); then C3C if in scope (re-run `scripts/run-demo.sh`). The single spec edit
(rev1§8.3:487 forward note) and the ledger Baselines `-p ipc` row update land with C3A (the
verified-surface change); **no seam row changes** (tally stays 14).

## Out of scope for C3 (recorded so it is not mistaken for a gap)

- **IDL-based wire encoding / a second codec backend, and a stable public syscall ABI.**
  rev1§8.3 (`:487`) groups these as the future "non-Rust userspace" effort. C3 delivers only the
  **negotiation mechanism** that effort builds on (*"bump protocol versions so old and new clients
  coexist per session"*); the IDL backend and stable ABI remain deferred.
- **The endpoint-cap connect-funding handshake.** `session.rs:68-72` records this as deferred —
  the client retyping a channel pair and *sending an endpoint cap* with the connect request
  (rev1§3.5). C3 (even C3C) performs only the **version+window** negotiation step on the
  *pre-wired* channel; the cap-funding step is not built.
- **Wire unification.** Migrating the running storaged↔shell path off the bespoke 3-byte
  `storage-server/wire.rs` header onto the `ipc` 10-byte `header.rs` is a separate effort; C3C
  keeps storaged on its own wire (only making its version byte dynamic).
- **Multi-window sessions.** rev1§8.3 (`:477`) defers grow-only multi-window; `WindowGrant.window`
  stays `0` (`session.rs:45-52`). C3 negotiates a version, not additional windows.
- **Non-contiguous version sets (a version bitset).** C3 offers a contiguous `[min,max]` range
  (Design decision 2); the bitset for a withdrawn-middle-version is the recorded
  forward-compatible generalization, not built now (YAGNI with one version deployed).
- **A TLA negotiation model.** Negotiation is a connect-time sequential decision, Verus +
  proptest-routed, consistent with the IPC "single-source by design" routing (ledger `:193`); the
  `IpcReactor` model is untouched.
- **Richer connect errors.** The deferred connect mechanism's richer errors (peer-closed session
  channel, undecodable reply, transport error — `session.rs:68-72`) return when that mechanism
  lands; C3 adds only `ConnectErr::VersionMismatch`.

## Open decisions requiring sign-off

1. **C3C scope (Design decision 5).** Wire a thin end-to-end version handshake onto the running
   storage session (Option A — the QEMU-smoke witness; storage version byte dynamic+validated;
   *no* cap-funding, *no* wire unification), **or** deliver+verify the mechanism in the `ipc` crate
   and defer the storaged integration (Option B). *Recommendation:* Option A — the parent acceptance
   reads like a real session and lists "the servers' connect handlers" among C3's touches; the
   witness is genuinely thin and keeps risk at S–M/low. Fall back to B if the storaged
   startup-state change proves disproportionate.
2. **Version representation (Design decision 2).** A contiguous `[min,max]` range (recommended —
   minimal verified addition, matches monotone version numbering and "old and new coexist") vs a
   version bitset (more general, non-contiguous, but YAGNI). *Recommendation:* range now; bitset
   as a forward append-only widening if ever needed.
3. **Refusal granularity (Design decision 4).** A single wire `GrantReply::Refused` for both
   version-mismatch and quota-refusal (minimal), vs a one-byte reason so a client can distinguish
   "wrong version, don't retry" from "no window, retry later." *Recommendation:* single `Refused`
   on the wire now (an internal `ConnectErr::VersionMismatch` for diagnosability); add a reason
   byte only when a client needs to branch on it.
