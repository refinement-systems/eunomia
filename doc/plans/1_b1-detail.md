# Plan — Part B1 detail: storage-server authority (`stat-store` + statfs gate, rights-lattice tests, ticket-TTL clamp)

Detailed, separately-implementable decomposition of **Phase B1** from
`doc/plans/0_address_audit_rev0.md`. B1 is Wave-1 work: the one real authority bug
the audit found (`I-1`), plus the two small adjacent items the parent plan folds in.

**Closes (from the parent plan):**
- `I-1` [high] — `stat-store` right does not exist; `statfs` is ungated → deny-by-default
  violated (`doc/results/0_audit_rev0.md` §2.1).
- storage-server rights-lattice proptest gap [low] (`audit` §4.2).
- `S-5` [spec→code] — claim-ticket TTL is caller-chosen with no server clamp (`audit` §5).

**Spec target (already blessed in rev1 — B1 only conforms code to it):**
- **rev1§2.3** — `stat-store` is a distinct ref right gating store-global observation;
  deny-by-default; "delegation helpers strip it, init grants it only to the shell and to
  maintenance holders, and `statfs` without it is refused"; its *scope* ignores the
  subtree but it "strips, enumerates, and dies with a generation bump like any other
  right."
- **rev1§2.4** — a claim ticket's TTL "the caller requests but the server clamps to a
  server-imposed maximum, so no ticket can outlive that bound."

Because Part A is blessed first (the parent plan's hard dependency), **B1 makes no spec
edits** — the rev1 text above is the fixed target. Every citation here is `rev1§`.

**Primary file:** `storage-server/src/lib.rs` (host-buildable, `no_std`+alloc; the
handle/rights logic is plain Rust, transport-agnostic, host-testable). Secondary:
`storage-server/tests/*`, `storage-server/Cargo.toml`, and one comment/grant touch in
`user/storaged/src/main.rs`.

---

## Verification tier & baseline (applies to all sub-phases)

Per rev1§6 routing, storage-server is a **userspace server**: the baseline is
**Miri + proptest**, and the rights lattice is pure sequential logic, so **proptest
(+ Miri replay)** is the load-bearing tier. There is no kernel/CAS/IPC chokepoint here,
so **B1 adds no Verus and no TLA obligation**, and touches none of the regression
baselines in the parent plan (kcore 335/0, cas 58/0, dma-pool 26/0, the TLC models, the
committed fuzz corpora). Two honesty notes recorded up front so nothing is silently
dropped:

- **No Loom/Shuttle target for the rights lattice.** `audit` §4.2 notes rev0§6 routes
  servers to concurrency testing, but `Server::handle` processes one request at a time
  (the reactor in `user/storaged` serializes dispatch) and the handle table contains no
  atomics — Loom/Shuttle's distinctive value (weak-memory reorderings) is moot. The
  server's *concurrency* surface is the IPC reactor, covered separately by Phase B14.
  B1 records this decision rather than adding a no-value harness.
- **No wire-format or protocol-version change.** Rights cross the wire only as the
  existing `rights_mask: u8` field of `OpenChild`/`OpenSnapshot`; defining a new bit
  value (bit 5) inside that `u8` changes no encoding. `R_ALL`'s numeric value is kept
  stable so the committed `request_dispatch` corpus keeps its meaning. The `wire.rs`
  header codec and version (`0x45 0x51 0x02`) are untouched.

**Baseline to re-establish at end of B1:** `cargo test -p storage-server` green
(currently 10 session tests + `fuzz_corpus` + `fuzz_regressions`); the committed fuzz
corpora still decode and dispatch without panic under the Miri replay
(`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p storage-server
--test fuzz_regressions --test fuzz_corpus`).

---

## Design decision — where `stat-store` lives in the bit set *(resolve in B1A)*

The parent plan's B1 work-list says "add a `stat-store` bit … (widen `R_ALL`)" **and**
"delegation/attenuation helpers **strip** `stat-store` by default." Those two can only be
jointly satisfied cleanly one way, so B1A pins the design:

- `R_STAT_STORE = 1 << 5` is a **distinct bit, kept *out* of `R_ALL`.** `R_ALL` stays
  `0b0_1_1111` (its current numeric value) and means "all ordinary, subtree-scoped,
  *delegatable* rights." `stat-store` is the one right "whose meaning ignores the
  subtree" (rev1§2.3), so it deliberately sits outside the ordinary delegatable set.
- **Origin:** `Server::root_grant` — documented as "init's world … full-rights handle"
  — becomes the **privileged constructor** and returns `R_ALL | R_STAT_STORE`. This is
  the *only* place the bit originates. "init grants it only to the shell and to
  maintenance holders" (rev1§2.3) is satisfied because the privileged root/maintenance
  grant is the sole source.
- **Strip-by-default falls out for free.** Attenuation stays plain monotone intersection
  (`e.rights & rights_mask`): a delegated subtree handle whose mask is `R_ALL` (or
  narrower) **automatically lacks** `stat-store`, because `R_ALL` excludes it.
  `OpenSnapshot` already masks to `R_READ | R_ENUMERATE`, so snapshot handles strip it
  too. The spec's "it strips … like any other right" holds with no special-casing.
- **Still delegable when explicitly intended.** rev1§2.3 contemplates "a handle
  attenuated to a single directory but carrying `stat-store`": a holder that *has* the
  bit and passes a mask with bit 5 set (`R_ALL | R_STAT_STORE`) keeps it on the child —
  whose `statfs` then observes the whole store despite the subtree scope (scope ignores
  the subtree). Intersection already gives exactly this.

This refines the parent plan's parenthetical "(widen `R_ALL`)": we widen the *rights
namespace*, not the `R_ALL` *mask*. **Recommendation: adopt as above.**

*Lower-churn alternative (not recommended):* put the bit inside `R_ALL` and special-case
`OpenChild` to strip it unless explicitly re-requested. Rejected — it contradicts
"strips … like any other right" and adds a carve-out to the one path that is currently a
clean intersection.

---

## Sub-phase B1A — the `stat-store` right and the `Statfs` gate *(closes I-1)*

The headline authority fix. Security-complete on its own: after B1A, `statfs` is refused
on any handle lacking `stat-store`, and the bit cannot be acquired by ordinary
delegation. Atomic by necessity — gating `statfs` without also fixing the bit's origin
would break `statfs` for every legitimate holder, so the gate, the origin, and the
strip discipline land together.

- **Touches:** `storage-server/src/lib.rs`
  - rights block `:37-44` (add `R_STAT_STORE`; document why `R_ALL` excludes it);
  - `root_grant` `:254-270` (return `R_ALL | R_STAT_STORE`; tighten the doc comment to
    "privileged init/maintenance grant — the sole origin of `stat-store`");
  - `Statfs` handler `:616-620` (gate change);
  - `OpenSnapshot` mask `:501` and `OpenChild` `:407-435` — **no code change**, but
    add a confirming comment that intersection is what strips `stat-store`.
  - `describe` `:683-693` — no change (already prints `rights {:#x}`, so the bit
    "enumerates" automatically, per rev1§2.3).
  - `user/storaged/src/main.rs:157` — no code change (it already opens its single
    session from `root_grant`, which now carries the bit); add a one-line comment that
    this session is the privileged holder and that per-process child sessions (Phase C1)
    will receive attenuated handles that strip `stat-store`.
- **Depends on:** Part A blessed (rev1§2.3 text). No intra-B1 dependency.
- **Work:**
  1. Add the constant next to the others:
     ```rust
     /// Store-global observation (rev1§2.3): gates `statfs(handle)` and any
     /// future global observable (GC counters, index occupancy). The one right
     /// whose scope ignores the subtree its handle denotes — and the one right
     /// kept OUT of `R_ALL`, so ordinary delegation strips it by default
     /// (deny-by-default). It originates only on the privileged `root_grant`.
     pub const R_STAT_STORE: u8 = 1 << 5;
     // R_ALL stays 0b0_1_1111 — all ordinary, subtree-scoped, *delegatable* rights.
     ```
  2. `root_grant` → `rights: R_ALL | R_STAT_STORE`.
  3. `Statfs` handler: `self.lookup(session, handle, 0)?` → `self.lookup(session, handle,
     R_STAT_STORE)?`. `lookup` already performs the generation/stale check before the
     rights check, so a stat-store handle "dies with a generation bump" for free
     (rev1§2.3) and a zero-/limited-rights handle is `Denied`.
- **Acceptance (regression tests in `tests/sessions.rs`, extending
  `manual_gc_and_statfs`):**
  - `statfs` through a handle lacking the bit (a zero-rights handle, and an
    `R_READ`-only handle) → `Err(Denied)`.
  - `statfs` through the privileged `root_grant` handle → `Response::Space`.
  - An `OpenChild` subtree handle derived with `rights_mask: R_ALL` → `statfs` refused
    (deny-by-default: the bit is stripped because `R_ALL` omits it).
  - An `OpenChild` subtree handle derived with `rights_mask: R_ALL | R_STAT_STORE` from
    the privileged parent → `statfs` **succeeds** and returns whole-store space
    (scope-ignores-subtree, rev1§2.3).
  - After `RevokeRef` (generation bump) the stat-store handle's `statfs` → `Err(Stale)`.
  - Existing `manual_gc_and_statfs` keeps passing (its `statfs` is through `root_grant`).
  - The `fuzz_corpus`/`request_dispatch` harness handle 0 (`root_grant`) keeps exercising
    the `statfs` success path; handles 1–2 (`R_READ | R_ENUMERATE`) now hit the `Denied`
    branch. No harness asserts a specific `statfs` response, so corpora stay valid; rerun
    the Miri replay to confirm.
- **Effort/Risk:** S / low. The single high-value change — closes the one real authority
  bug.

---

## Sub-phase B1B — rights-lattice proptest tier *(closes audit §4.2 [low])*

The lattice currently has only example tests. B1B adds the property test the audit asks
for: **monotone attenuation across arbitrary derivation chains** (no chain ever *gains* a
right), with `stat-store` strip/scope as a first-class case.

- **Touches:**
  - `storage-server/Cargo.toml` — add `proptest = "1"` to `[dev-dependencies]` (today
    only `cas`/`urt` carry it).
  - new `storage-server/tests/rights_lattice.rs`.
  - `storage-server/src/lib.rs` — optionally factor the attenuation arithmetic into a
    pure `pub fn attenuate(parent: u8, mask: u8) -> u8 { parent & mask }` so the
    arithmetic core is unit/proptest-addressable and self-documenting; and add a small
    inspection accessor `pub fn handle_rights(&self, session: SessionId, handle:
    HandleId) -> Option<u8>` (useful for audit too) so the proptest can read effective
    rights directly instead of parsing `EnumerateSession` dumps.
- **Depends on:** B1A (the bit + gate must exist).
- **Work:**
  - **Property 1 — monotonicity.** Build a fixed directory tree (reuse `new_server`'s
    `pub/deep/leaf` shape). Generate a random sequence of `OpenChild` steps: each step
    picks an existing handle, a valid in-tree child path, and an arbitrary `rights_mask:
    u8`. Track the *expected* rights as the running intersection fold. Assert at every
    step `child_rights == parent_rights & mask` and `child_rights ⊆ parent_rights` (no
    chain gains a right). Interleave `OpenSnapshot` steps and assert their result
    `⊆ parent & (R_READ | R_ENUMERATE)`.
  - **Property 2 — `stat-store` strip.** Across the same chains: if a parent lacks
    `R_STAT_STORE`, no descendant has it; and a child whose mask omits bit 5 lacks it
    regardless of the parent. Probe **behaviorally** (the property that matters is the
    gate, not just the arithmetic): `statfs` succeeds **iff** `handle_rights & R_STAT_STORE
    != 0`. Pair with the same iff-probe for `R_READ` (via `Read`) and `R_REWRITE_HISTORY`
    (via `Gc`) so the test pins the gate↔bit correspondence, not only the fold.
  - **Property 3 — scope ignores subtree.** When a chain *does* carry `stat-store` onto a
    deep subtree handle (explicit `R_ALL | R_STAT_STORE` mask), `statfs` through it returns
    the *same* whole-store `Space` as through the root handle (subtree scope does not
    shrink the observable).
  - Use the workspace Miri convention for case counts:
    `#![proptest_config(ProptestConfig { cases: if cfg!(miri) { 4 } else { 256 },
    ..ProptestConfig::default() })]` (mirrors `cas/src/overlay.rs:201-205`).
  - Update the `fuzz_corpus`/`request_dispatch` seed harness to add one
    `R_STAT_STORE`-bearing handle if helpful for `statfs`-success coverage under fuzz
    (optional — handle 0 already covers it via `root_grant`).
- **Acceptance:** `cargo test -p storage-server` green including `rights_lattice`; the
  monotonicity and strip properties pass at 256 cases natively and 4 under Miri; a
  deliberately-broken intersection (e.g. `parent | mask`) makes Property 1 fail (sanity
  of the oracle).
- **Effort/Risk:** S–M / low. Pure test addition behind the host-testable seam.

---

## Sub-phase B1C — claim-ticket TTL server clamp *(closes S-5)*

Independent of B1A/B1B (touches only `MintTicket`); may land in any order. Brings
`MintTicket` into conformance with rev1§2.4's "the caller requests but the server clamps
to a server-imposed maximum."

- **Touches:** `storage-server/src/lib.rs` — `MintTicket` handler `:536-551`; a new
  named maximum near the `Server` definition.
- **Depends on:** Part A blessed (rev1§2.4 text). No intra-B1 dependency.
- **Work:**
  - Define the server-imposed bound as a named constant (a tunable default, documented as
    such — rev1§2.4 mandates *that* a bound exists, not its value; tickets are for
    immediate peer hand-off, so the window is short):
    ```rust
    /// Maximum claim-ticket lifetime (rev1§2.4): the caller's requested TTL is
    /// clamped to this so no ticket outlives the bound. Tickets are for prompt
    /// peer hand-off, not durable authority — that stays in the handle/session
    /// regime. Default 60 s; a tunable policy default, not an ABI promise.
    pub const MAX_TICKET_TTL_NANOS: u64 = 60_000_000_000;
    ```
    (If per-deployment tuning is wanted later, promote to a `Server` field seeded in
    `Server::new`; a const is sufficient for B1.)
  - In `MintTicket`: `let ttl = ttl_nanos.min(MAX_TICKET_TTL_NANOS);` then
    `expires: now.saturating_add(ttl)` (keep the existing saturating add).
- **Acceptance (test in `tests/sessions.rs`, extending
  `tickets_are_one_shot_with_ttl`):**
  - Mint with `ttl_nanos = u64::MAX`; a redemption at `now + MAX_TICKET_TTL_NANOS + 1`
    → `Err(BadTicket)` (the clamp bit); a redemption at `now + MAX_TICKET_TTL_NANOS`
    → `Ok` (boundary still valid).
  - Existing ticket assertions (ttl 1 000 / 5) unaffected — both are far below the bound.
- **Effort/Risk:** S / low.

---

## Execution order

```
B1A  stat-store right + statfs gate        [the I-1 fix; do first]
  └─► B1B  rights-lattice proptest tier     [needs the bit from B1A]
B1C  ticket-TTL clamp                       [independent; any time, incl. first]
```

- **B1A** is the load-bearing security fix and is independently shippable (it fully
  closes `I-1` and establishes deny-by-default in one atomic change).
- **B1B** depends on B1A (it tests the new bit/gate).
- **B1C** is fully independent (only `MintTicket`); sequence wherever convenient.
- B1A and B1B *may* be reviewed as one change if preferred, but B1A alone is a complete,
  mergeable unit — keep them separable so the high-severity fix can land fast.

## Out of scope for B1 (recorded so it is not mistaken for a gap)

- **Per-session / per-ref `statfs` views and quotas** (rev1§8 future work): an
  unprivileged holder reading *quota-relative* numbers without `stat-store`. B1 delivers
  only the global-tier gate; the quota tier is explicitly deferred (rev1§2.3 last line,
  rev1§8 "Per-ref and per-session disk-space quotas").
- **The named-grant table / standard-name wiring** that will let init hand `stat-store`
  to a *specific* shell process distinct from other children — that is **Phase C1**
  (pulled forward there for the M-9 console). B1A makes `root_grant` the privileged
  origin and leaves a forward comment in `user/storaged`; C1 does the per-process split.
- **Loom/Shuttle for the server** — deferred to Phase B14 (the IPC reactor is the real
  concurrency surface), per the verification-tier note above.
