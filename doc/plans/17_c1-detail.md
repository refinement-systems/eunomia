# Plan — Part C1 detail: the named-grant table / argv / env / standard names (replace the three hand-rolled fixed-layout startup blocks — `"SD02"` init→storaged, `"SH01"` init→shell, `"ST01"` shell→child, plus `hello`'s `b"startup:hello"` magic-string check — with **one** versioned, self-describing **startup-block format** carrying *argv*, *env*, and a **named-grant table** whose entries discriminate the spec's two kinds of name — kernel caps → cspace slots, storage grants → handle numbers (rev1§5.1) — extended with the pre-mapped-**region** kind the time page / MMIO window / DMA pool already need (a VA travels in every block today). C1 defines the codec behind a host-testable, fuzzed seam (rev1§2.7 refuse-not-crash), wires init/storaged/shell/selftest/hello onto it, delivers the **standard names** that map to authority that exists today — `time`, `storage`, `root` (and `tmp` where carvable) — and *reserves* `stdin`/`stdout` for **C-M9** to populate with the console channel. This is the M-9 unblocker and retires the magic-byte blocks the audit flags. Not test-only: it changes the startup-block **bytes** and the shell's per-child argv behaviour. No Verus, no TLA, no new trusted seam (ledger tally stays **14**); the verified-surface gates are held by not touching them.)

Detailed, separately-implementable decomposition of **Phase C1** from
`doc/plans/0_address_audit_rev0.md` (parent-plan C1 at `:662-679`). C1 is **Wave-5**
work, **pulled forward** out of Part C because the high-severity userspace console
(**C-M9** / M-9) depends on it (parent plan `:655-660`, `:788-790`). It depends only on
**Part A** being blessed (the rev1§5.1 startup-block / named-grant-table / standard-name
text it conforms to, parent plan `:671`) and on nothing else in Part B/C; **C-M9** depends
on it (and on **B-IRQ**, the kernel device-IRQ→notification path). The debug-UART scaffold
(rev1§7, Phase A3/S-8) keeps the system usable until M-9 lands, so there is no correctness
emergency forcing C1 ahead of Part B — it is sequenced for the console track, not for a bug.

The framing that shapes the whole phase: unlike B15 (a coverage backfill that changed no
bytes), **C1 changes the startup-block format itself** — the first message on every child's
bootstrap channel. That message is a **decoder of an untrusted-shaped input** consumed in
`_start` before anything else exists, so a malformed block must be **refused, never a panic**
(a panic in `_start` is a boot failure) — the rev1§2.7 discipline B15C already pinned for the
fixed blocks, now applied to a *variable-length* format with real decode surface (counts,
lengths, an entry discriminator). The audit found the current state **correct but
hand-rolled and under-powered** (only `time` is a named grant; there is no argv/env, no
discriminated table, no standard-name resolution): the three blocks are bespoke, position-
fixed byte layouts that each new consumer re-hand-parses. C1's engineering content is
**one format, one codec, behind one fuzzed seam**, then the mechanical-but-careful rewiring
of five binaries onto it — chosen to deliver exactly the authority that exists today under
real names, and to leave a clean socket where C-M9 plugs the console channel in.

**Closes (from the parent plan / audit).** Parent plan C1 `:663-665`; audit
`doc/results/0_audit_rev0.md` §3.2 `:381-385` (verbatim):

> **Named-grant table / argv / env / standard names** (`root/stdin/stdout/tmp/storage`,
> the deliberately-split stdin/stdout) are unimplemented; init and shell hand-roll
> fixed-layout byte blocks ("SD02"/"SH01"/"ST01") carrying only magic + mode + time-VA
> (`user/*/src/main.rs`). Only `time` is delivered as a standard grant. rev0§8.3 defers
> "a real named-grant-table format". [confirmed-deferred]

Plus the parent-plan C1 obligations: *"define and implement a real named-grant-table
format in the startup block … carrying argv and env … deliver the standard names … replace
the hand-rolled blocks"* (`:674-678`), and *"unblocks M-9; cleans up the hand-rolled startup
blocks"* (`:663-665`).

Two scope notes, both load-bearing for C1's boundary:

- **The console (`stdin`/`stdout` as a real channel) is C-M9, not C1.** The parent plan
  splits them: C1 *delivers the named-grant table*, C-M9 *delivers the console driver +
  channel and grants it under `stdin`/`stdout`* (`:681-712`, esp. `:704-706`). C1 **reserves
  and resolves** the `stdin`/`stdout` standard names in the format so C-M9 is a pure
  population step (init grants a channel cap under those names; the shell reads them), but
  C1 does **not** build the console — the shell keeps the rev1§7 debug-syscall scaffold for
  terminal I/O. Recorded in Design decision 4.
- **Cross-session storage delegation (a *grandchild*'s own storage session) is out of
  scope.** rev1§2.4 (`spec_rev1.md:97`) describes a parent funding a channel pair and asking
  the server *"over its own session, to open a new session on the offered endpoint,
  pre-populated with specified handles and attenuated in the same request."* That wire op
  **does not exist** (the `Request` enum has `OpenChild` — attenuate *within* a session,
  `storage-server/src/lib.rs:133-138` — but no cross-session connect-with-delegation;
  `open_session` is an in-process API used only by storaged's `_start`). So C1 delivers
  `storage`/`root` under names only to init's *direct* children, where the session already
  exists; giving the shell's children their own storage session needs new protocol (overlaps
  B5/B1) and is recorded out of scope (Design decision 3).

---

## Spec target — Part A is blessed; C1 makes two small edits on landing

Every citation is `rev1§` against the already-blessed text. C1 touches **no** proof
boundary (it flips no `rev1§6.1` seam line and adds no Verus/TLA), so the only spec edits
are the two the parent plan names (`:666-667`: *"rev1§5.1 (startup block, named-grant table,
standard names), rev1§8.3 (mark delivered)"*), made when C1 lands.

- **rev1§5.1 — the startup block & named-grant table** (`spec_rev1.md:359-367`, table at
  `:361`, standard names at `:363`). This is C1's normative target, and the blessed text
  already describes the table as the design:
  > *"a startup block … containing argv and env (byte-string vectors) and a named-grant
  > table. Table entries carry a discriminator for two kinds of name: kernel caps resolve
  > to cspace slots, and storage grants resolve to handle numbers on the process's storage
  > session channel … Standard names: `root` … `stdin` and `stdout` (deliberately split …)
  > … `tmp`, `storage` … and `time` …"*
  C1 makes this true. **Edit on landing:** a forward note in §5.1 that the table is
  implemented as of C1 (the present-tense description is no longer aspirational), and that
  the **region** kind (a pre-mapped VA — the time page today, MMIO/DMA for storaged) joins
  the spec's two kinds (it is the mechanism the `time` grant already uses, generalized;
  Design decision 2). The two security-relevant kinds the spec names (cap, storage-handle)
  are unchanged; the region kind is additive and carries no new authority (the parent maps
  the page before start exactly as today — only its VA travels).
- **rev1§8.3 — mark the table delivered, keep the *public ABI* deferred**
  (`spec_rev1.md:487`). The blessed deferral bundles the named-grant table with the
  non-Rust-userspace effort: *"Foreign-language support also needs a stable syscall ABI and
  a real named-grant-table format (§5.1), so it lands as one deliberate public-ABI effort."*
  C1 implements the **mechanism** (a Rust-internal format), not a **stable public ABI**.
  **Edit on landing:** disentangle the two — the named-grant table *exists* as of C1
  (§5.1); what remains §8.3-deferred is its *stable, language-neutral public-ABI form* (a
  versioned wire contract for foreign-language userspace, alongside the IDL + stable syscall
  ABI). The honest split: C1 delivered the table; C1 did **not** freeze a public ABI.
- **rev1§2.7 — the syscall/decode boundary** (`spec_rev1.md:125-131`), the home Phase A3
  created (S-7) and the discipline the startup-block decoder obeys: *"An unrecognized opcode
  returns an error, never a crash … a length from untrusted input is validated against
  ground truth before use."* The startup block is exactly such an input (variable counts +
  lengths consumed in `_start`); C1's decoder is **total over arbitrary bytes** —
  refuse-not-crash — fuzzed and proptested like the wire/CAS decoders (rev1§3.7/§6, "decoders
  get fuzz targets"). No text change; C1 conforms.
- **rev1§2.4 — delegation along spawn** (`spec_rev1.md:85-103`, delegation at `:97`). C1's
  storage-handle names (`root`, `tmp`) deliver *existing* handles by number on the session
  the parent pre-populated; the **cross-session** delegation prose at `:97` stays the target
  for the deferred grandchild-session work (Design decision 3). No text change.
- **rev1§2.6 — time** (`spec_rev1.md:117-121`): *"The frame … arrives in the startup block
  under the name `time` (§5.1)."* Already true (the only named grant today); C1 keeps it,
  now as a **region** entry in the unified table rather than a bespoke field. No text change.
- **rev1§7 — the console scaffold** (`spec_rev1.md:431-433`): the debug syscalls are a
  time-boxed scaffold *"until the userspace console and its prerequisites are built — the
  device-interrupt-to-notification path … (§3.6) and the named-grant table that delivers the
  console cap under standard names (§5.1)."* C1 delivers **one** of M-9's two prerequisites
  (the table + the `stdin`/`stdout` standard names); B-IRQ delivers the other. No text
  change; C1 satisfies the §5.1 half of the §7 carve-out's exit condition.

---

## What is actually true today — three bespoke blocks, one named grant, no argv/env

The inventory that shapes the phase. Five binaries exchange startup state; each block is a
hand-rolled fixed layout, re-parsed by hand at the consumer.

### The three producer→consumer block pairs

- **init → storaged: `"SD02"`** (init `build_sd02`, `user/init/src/main.rs:84-93`; sent at
  `:219-223`). A 44-byte block: magic + **five** little-endian `u64` *region VAs* — MMIO
  window VA, DMA region VA, DMA device **PA**, DMA length, time-page VA. storaged decodes the
  inverse (`parse_config`, `user/storaged/src/main.rs:138-150`; `Config` at `:123-130`;
  consumed in `_start` at `:155-167`, time page attached at `:170`). **Every field here is a
  pre-mapped region** (init `map`s each into storaged's aspace before start, `:189-217`) whose
  VA must travel — the SD02 block *is* a region table in disguise.
- **init → shell: `"SH01"`** (init `build_sh01`, `:99-104`; sent at `:280-284`). A 12-byte
  block: magic + the time-page VA. The shell decodes it inline in `_start`
  (`user/shell/src/runtime.rs:626-637`: `blen >= 12 && &boot[..4] == b"SH01"`, then the
  8-byte VA → `urt::time::attach`). The shell's *other* authority is delivered **out of
  band**, by slot convention: init `cap_install`s the bootstrap channel into the child's
  cspace slot 0, the session channel into slot 1, the spawn untyped into slot 2, and a
  re-grantable time cap into slot 5 (`user/init/src/main.rs:236-296`); the shell **hardcodes**
  those slot numbers (`runtime.rs:27-56`: `BOOT_CHAN=0`, `STORE_CHAN=1`, `POOL=2`, `SH_TIME=5`)
  and the storage **handle** number (`request(&Request{ handle: 0, … })`, every built-in,
  e.g. `runtime.rs:118`). So the shell already *holds* `storage` (slot 1) and `root`
  (handle 0) and `time` (slot 5 / the mapped VA) — they are simply **un-named**, baked into
  constants on both sides. That is precisely the gap: the names exist in the spec, the
  authority exists in the code, and nothing connects them.
- **shell → child: `"ST01"`** (shell builds it inline, `runtime.rs:419-423`: magic + a
  **one-byte mode** + the child's time-page VA, 13 bytes; sent at `:423`). The mode byte is a
  degenerate argv — `run <path> [mode]` parses one integer (`cmd_run`, `:506-522`) and passes
  it as the mode. selftest decodes ST01 (`parse_st01`, `user/selftest/src/main.rs:70-79`;
  `St01` at `:64-68`; consumed `:107-115`) and switches on the mode (`:117-146`). The child
  gets **time + a mode byte** and nothing else — no storage session (grandchildren have no
  store access today), no argv beyond the single mode integer, no env.
- **`hello`** (`user/hello/src/main.rs:15-35`) ignores the structured blocks entirely: it
  reads the bootstrap message and checks `&buf[..len] == b"startup:hello"` (`:29`), a magic
  string no producer sends (init/shell send `SD02`/`SH01`/`ST01`). It is a degenerate
  smoke subject; C1 aligns it onto the real format (or retires its bespoke check).

### The delivery mechanism C1 builds on (unchanged by C1)

- **Caps reach a child by `cap_install`**, not by the block. `cap_install(child_cspace,
  src_slot, dst_index)` (`ipc/src/sys.rs:180-182`) places a cap from the parent's cspace into
  the child's at a chosen index, *before* `start`. The named-grant table is therefore **data
  that names already-installed slots** — the table says "`storage` → slot 1," it does not
  move the cap. This is exactly the spec's "kernel caps resolve to cspace slots" (rev1§5.1).
  C1 does **not** change how caps are installed; it changes how their slots are *named*.
- **The block is one channel message ≤ 256 bytes.** `chan_send`/`chan_recv`
  (`ipc/src/sys.rs:184-203`) over the bootstrap channel; the kernel message payload is
  `MSG_PAYLOAD = 256` bytes plus `MSG_CAPS = 4` cap slots (`kcore/src/channel.rs:36-37`).
  Every consumer reads into a `[u8; 256]` buffer (storaged `:156`, shell `:630`, selftest
  `:99`, hello `:18`). **256 bytes is C1's hard budget** for argv + env + the grant table.
  (The 4 message-cap slots are *unused* by the startup path today — caps go via `cap_install`
  — and C1 keeps it that way; see Design decision 2, Rejected.)
- **Storage is one shared session, handle 0.** storaged `root_grant(b"main")` →
  `open_session(vec![grant])` once at boot (`user/storaged/src/main.rs:214-218`;
  `storage-server/src/lib.rs:387-396`, `:420-436`). The shell uses `handle: 0` everywhere.
  `R_STAT_STORE` (`lib.rs:52`) rides on this single full-rights grant; the storaged comment
  at `:211-213` already names C1 as the phase that would split this into per-process child
  sessions — which C1 **scopes out** for grandchildren (Design decision 3), because the
  cross-session connect op does not exist.

### What B15C already established at this exact boundary

B15C host-tested **current** behaviour at the C1 seam — the SD02/SH01/ST01 round-trips and
refuse-not-crash totality (`user/init/src/main.rs:310-402`, `user/storaged/src/main.rs:309-395`,
`user/selftest/src/main.rs:156-253`; `doc/results/27_b15c-findings.md`). Those tests
**document the pre-C1 format** and will be **replaced** by C1's tests against the new format
(the B15C suite is the regression witness that C1's rewiring preserves the *semantics* —
time still attaches, storaged still gets its five regions, selftest still sees its mode —
while changing the *bytes*). C1 inherits B15C's `#[cfg(not(test))]` host-harness gating and
the `cfg(miri){4}else{256}` case-count idiom; the harness is already in place.

---

## Primary files (current line numbers)

- **The codec home (Design decision 1).** Recommended: a new `loader::startup` module
  (`loader/src/startup.rs`, **new**) beside the existing host-testable decoder
  `loader/src/elf.rs` (`loader/src/lib.rs:9` `pub mod elf;` — startup joins it, *not* under
  the `target_os = "none"` gate that hides `spawn`, `:11-12`); fuzzed via the existing
  `loader/fuzz` crate. `loader/Cargo.toml` already carries `proptest` as a dev-dep.
- `user/init/src/main.rs` — the producer of two blocks. `build_sd02` (`:84-93`), `build_sd02`
  send (`:219-223`); `build_sh01` (`:99-104`), `build_sh01` send (`:280-284`); the
  `cap_install` slot wiring whose indices become names (`:236-296`); the slot/VA consts
  (`:31-63`); the B15C builder tests (`:310-402`, replaced).
- `user/storaged/src/main.rs` — SD02 consumer. `parse_config` (`:138-150`), `Config`
  (`:123-130`), `_start` recv+parse+attach (`:155-170`), the slot consts (`:34-41`),
  `root_grant`/`open_session` (`:214-218`), the B15C parse tests (`:309-395`, replaced).
- `user/shell/src/runtime.rs` — SH01 consumer **and** ST01 producer. SH01 parse in `_start`
  (`:626-637`); the hardcoded slot/handle constants that become named lookups (`:27-56`);
  ST01 build+send (`:419-423`); `CHILD_TIME_VA` (`:56`); `cmd_run` mode parsing (`:506-522`);
  the per-child `cap_install` (`:418`). `user/shell/src/main.rs` — pure logic lives here
  (`:49-186`); any new pure argv/table helpers join it for host-testing.
- `user/selftest/src/main.rs` — ST01 consumer. `parse_st01` (`:70-79`), `St01` (`:64-68`),
  mode dispatch (`:107-146`), the B15C parse tests (`:156-253`, replaced).
- `user/hello/src/main.rs` — degenerate consumer. The `startup:hello` check (`:29-33`).
- `ipc/src/sys.rs` — the unchanged mechanism: `cap_install` (`:180-182`), `chan_send`/
  `chan_recv` (`:184-203`), the `OBJ_*`/`RIGHT_*` consts. C1 does **not** touch `sys`.
- `kcore/src/channel.rs:36-37` — `MSG_PAYLOAD = 256` / `MSG_CAPS = 4`, the block's size
  budget. **Not touched** (cited as the constraint).
- `storage-server/src/lib.rs` — the session/handle model C1's storage-handle names ride on:
  `Request::OpenChild` (`:133-138`), `open_session`/`root_grant` (`:387-436`), `R_STAT_STORE`/
  `R_ALL` (`:52-57`). **Not touched by C1** (grandchild sessions are out of scope, DD3).
- `loader/fuzz/` — the existing loader fuzz workspace; C1A adds a `fuzz_targets/startup.rs`
  (the decoder fuzz target) and seeds a corpus, mirroring `loader/fuzz`'s ELF target.
- `doc/spec/spec_rev1.md` — the two edits on landing (§5.1 forward note `:359-367`, §8.3
  split `:487`).
- `doc/guidelines/verus_trusted-base.md` — the ledger. C1 adds **no seam** (tally stays
  **14**, `:92`); it **may** add a Baselines row recording the startup codec's host tests +
  fuzz target as test-routed gates (the B15 precedent), no `[verifying]` line to flip.
- `scripts/run-demo.sh` — the unchanged QEMU integration gate (boot green:
  `[storaged] store mounted` → `serving`, shell commands echo, no panic/`Corrupt`).

---

## Verification tier & baseline (applies to all sub-phases)

C1's codec is rev1§6 **Baseline** tier (Miri + proptest) **plus a cargo-fuzz target** (it is
a genuine untrusted-shaped *decoder*, the rev1§2.7/§3.7 routing — "decoders get fuzz
targets," parent plan `:33`). The rewiring is integration-gated by the QEMU boot smoke. Five
notes up front so nothing is silently dropped or over-claimed:

- **C1 is not test-only — it changes the startup-block bytes and runtime behaviour.** Unlike
  B15, the on-the-wire startup format changes (the three blocks → one format) and the shell's
  per-child argv behaviour changes (a mode byte → an argv vector). So the QEMU boot smoke is
  a **real** gate (the format must boot the actual system), not just a regression check.
  Everything else — the aarch64 cross-build linking every `user/*` binary, the kcore/CAS/
  ipc/dma-pool/freelist/urt Verus counts, the three TLA models — is held **by not touching
  them**.
- **No Verus, no TLA, no new seam.** The startup codec is userspace tooling outside the
  verified surface (`verus_trusted-base.md` "Scope"); like `loader::elf` it lives at the
  Baseline tier. C1 adds no `external_body`/`assume_specification` (tally stays **14**), no
  `verus!{}`, no `.tla`. The decode-totality property (refuse-not-crash) is the *fuzz +
  proptest* obligation, not a mechanized one — recorded honestly (rev1§6.1 honesty rule), the
  same posture `loader::elf`/`sysabi::decode`'s structural decode gets *before* it is lifted.
- **The decoder gets a fuzz target (the tier's headline).** A `loader/fuzz/fuzz_targets/
  startup.rs` over arbitrary bytes asserting `decode` never panics / never allocates
  unboundedly / never reads past the buffer (rev1§2.7), with any crash promoted to a
  regression test (the `loader/tests/fuzz_regressions.rs` pattern, where ELF-1 already lives,
  parent plan B3 `:275-277`). The Quickest-UB pass in CLAUDE.md already sweeps
  `loader --test fuzz_regressions --test fuzz_corpus` under Miri; the startup corpus joins it.
- **Producer totality too.** The decoder is the fuzzed surface, but the **encoder** must also
  be total in the other direction: building a block that would exceed the 256-byte budget
  returns a clean `Err` the producer (init/shell) maps to a boot/spawn failure, never a panic
  or a silent truncation (the rev1§4.5 `format`-contract spirit applied to block
  construction — "refuse, never panic," Design decision 2). A proptest pins encode→decode
  round-trip and the over-budget refusal.
- **The `cargo fmt` workspace-split trap applies.** Every `user/*` file C1 touches must be
  formatted via its own manifest (`cargo fmt --manifest-path user/storaged/Cargo.toml`,
  etc.); `loader` and `loader/fuzz` format via the root and the fuzz manifest respectively
  (CLAUDE.md "Formatting"). `loader` is a root-workspace member; the user binaries and
  `loader/fuzz` are not.

**Baseline to re-establish at end of C1:**

- `cargo test -p loader` green: the new `startup` round-trip + totality proptests and the
  golden-layout unit tests, alongside the existing `elf` tests.
- `cargo test --manifest-path user/{storaged,init,selftest}/Cargo.toml` green: the rewired
  consumer/producer tests (the B15C suite, ported to the new format).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p loader --test
  fuzz_regressions --test fuzz_corpus` clean (the startup corpus replays UB-free).
- The aarch64 cross-build links **every** `user/*` binary, and **`scripts/run-demo.sh` boots
  green** under the CLAUDE.md timeout-harness pattern: `[storaged] store mounted` → `serving`,
  `date`/`cat`/`write`/`ls`/`df` behave, `run`/`runloop` (the selftest spawn/reap modes,
  incl. the `time-ok` time-grant witness) pass, no panic/`Corrupt`.
- The verified-surface gates (kcore/cas/ipc/freelist/dma-pool/urt Verus counts, the three TLA
  models, the fuzz corpora + Miri replay) unchanged — C1 touches none of them.

---

## Design decision 1 — where the codec lives: a `loader::startup` module *(resolve in C1A)*

The format needs one canonical encoder + decoder reachable by **both** producers (init,
shell) and **all** consumers (storaged, shell, selftest, hello), host-buildable for tests,
and fuzzable. Three homes are viable; they differ in churn and theme.

- **Adopted — a new `loader::startup` module** (`loader/src/startup.rs`), beside
  `loader::elf`.
  - *Thematically exact.* The startup convention **is** rev1§5.1, the loader's spawn
    convention; `loader/src/lib.rs:1` is literally "Program loader (rev1§5)." `elf` is the
    sibling precedent — a host-testable, fuzzed decoder in the same crate, *not* under the
    `target_os = "none"` gate that hides `spawn` (`lib.rs:9` vs `:11-12`). `startup` slots in
    identically.
  - *Fuzz infra exists.* `loader/fuzz` is already a standalone fuzz workspace with an ELF
    target and a `fuzz_regressions`/`fuzz_corpus` test pair wired into the CLAUDE.md Miri
    sweep — the startup decoder's fuzz target is one file alongside it, no new fuzz crate.
  - *Cost: three new deps.* init and the shell already depend on `loader`
    (`user/init/Cargo.toml`, `user/shell/Cargo.toml`, both `default-features = false`).
    storaged, selftest, and hello gain `loader = { path = "../../loader", default-features =
    false }` — a `no_std`, host-buildable dep pulling only `elf` + `startup` (loader's only
    transitive dep is `ipc`, which they all already carry). Three one-line manifest additions.
- **Rejected — `ipc::startup`.** Zero new deps (every user binary already depends on `ipc`),
  but `ipc` is "the async IPC crate" (reactor, endpoints, wire header) — the startup block is
  a *spawn-convention* artifact, not an IPC-protocol one, and folding it in blurs the crate's
  charter. The fuzz target would live in `ipc/fuzz` (also fine). This is the **lower-churn
  fallback** if adding `loader` to three manifests is judged not worth it; the format and
  tests are identical either way.
- **Rejected — a new standalone `startup` crate.** The cleanest isolation (own fuzz crate,
  zero deps, the freelist/dma-pool precedent), but a new workspace member **plus** a path-dep
  in *six* manifests (loader + five user binaries) is the most churn for a format that
  `loader` already has the perfect home and fuzz infra for. Reach for this only if `startup`
  later needs to be verified in isolation (it will not — it is Baseline tier).

**Recommendation: a `loader::startup` module beside `loader::elf`, fuzzed via `loader/fuzz`;
add `loader` (default-features off) to storaged/selftest/hello. Fall back to `ipc::startup`
only to avoid the three manifest additions.**

---

## Design decision 2 — the startup-block format: a versioned, self-describing block with a three-kind grant table *(resolve in C1A)*

The format must carry argv, env, and a named-grant table within 256 bytes, be total to
decode (refuse-not-crash), and subsume every field the three current blocks carry — including
the **region** fields SD02 needs (VA, len, and a device **PA** for DMA) that the spec's
two named kinds (cap-slot, storage-handle) do not cover.

- **Adopted — one versioned block: a fixed header, then a grant table of tagged entries,
  then argv, then env; counts and lengths validated against the remaining buffer before every
  index.** A sketch (finalize byte offsets in C1A — concrete but revisable, the detail-plan
  habit):
  ```
  Startup block  (first bootstrap-channel message, ≤ MSG_PAYLOAD = 256 bytes)
    magic    : [u8;4] = b"EUS1"      Eunomia startup, version 1 — supersedes SD02/SH01/ST01
    ngrants  : u8                    number of grant entries
    nargv    : u8                    number of argv byte-strings
    nenv     : u8                    number of env byte-strings
    grants[ngrants]                  (tagged, see below)
    argv : nargv × (len:u16, bytes)  byte-string vector (rev1§5.1)
    env  : nenv  × (len:u16, bytes)  byte-string vector
  Grant entry (tagged by kind; size is a function of kind, so decode is total):
    name : u8     well-known name id (the standard names + bring-up device names)
    kind : u8     CAP_SLOT=1 | STORAGE_HANDLE=2 | REGION=3
    CAP_SLOT       : slot   : u32                 → cspace slot index (rev1§5.1 kind 1)
    STORAGE_HANDLE : handle : u32                 → handle number on the storage session (kind 2)
    REGION         : va:u64, len:u64, pa:u64      → pre-mapped region; pa=0 unless DMA (kind 3)
  ```
  - **Three kinds, two from the spec + one additive.** `CAP_SLOT` and `STORAGE_HANDLE` are
    the spec's literal two (rev1§5.1: "kernel caps resolve to cspace slots, and storage
    grants resolve to handle numbers"). `REGION` is the generalization of the *one* grant the
    system already delivers — `time` is a pre-mapped VA in every block today; SD02's MMIO and
    DMA fields are the same shape (VA + len, plus a device PA for DMA). Folding them into the
    table kills the bespoke SD02 layout. The region carries **no new authority** — the parent
    still `map`s the page before start (`user/init/src/main.rs:189-217`); only the VA travels,
    exactly as now (so a child still never *assumes* an address, rev1§2.6/§5.1).
  - **Well-known name ids, not strings (Design sub-decision).** Standard names are a small
    `u8` enum — `ROOT=1, STDIN=2, STDOUT=3, TMP=4, STORAGE=5, TIME=6`, plus bring-up device
    names `VIRTIO_MMIO=16, DMA=17` for storaged — so a `no_std` `_start` resolves a name with
    an integer match, not string handling, and the table stays tiny. `name=0` is reserved as
    a *string-name escape* (a future entry could carry `(len:u8, bytes)` before `kind`) so the
    eventual stable public ABI (rev1§8.3) can widen to byte-string names without a format
    break — the spec's "names" are honoured by the escape, the MVP uses ids. (Recorded so the
    id scheme is a deliberate compaction, not a divergence from rev1§5.1's "name.")
  - **Total decode (rev1§2.7).** `decode(&[u8]) -> Option<Startup>` (or `Result`): validate
    `magic`, then for each of `ngrants`/`nargv`/`nenv` check that the declared entry/length
    fits the **remaining** slice before reading — any shortfall, bad `kind`, or bad `magic`
    returns `None`, never a panic, never an out-of-bounds read, never an unbounded alloc. This
    is the fuzz/proptest obligation. Borrowed-slice decode where possible (argv/env as
    `&[u8]` into the message buffer) to avoid allocation in `_start`.
  - **Total encode + budget refusal.** `encode(&Startup) -> Result<heapless-or-bounded buf,
    Overflow>` returns `Err` if the block would exceed 256 bytes; init/shell map that to a
    clean boot/spawn failure (`check(...)` / `RunErr`), never a panic. Headroom is generous —
    storaged's block (3 regions ≈ 78 B + header) and the shell's (1 region + 2 small entries +
    a short argv) sit far under 256 — but the bound is enforced, not assumed.
- **Rejected — carry caps in the message's 4 cap slots instead of naming `cap_install`ed
  slots.** Channel messages *can* move up to `MSG_CAPS = 4` caps (`chan_send(.., Some(&[u32;
  4]))`), so the block could transfer caps inline. But (a) the spec is explicit — "kernel
  caps resolve to **cspace slots**" (rev1§5.1) — i.e. the table *names* pre-installed slots;
  (b) 4 is too few (the shell alone holds bootstrap + session + pool + time ≥ 4); (c) it would
  rewrite the working `cap_install` path (`user/init/src/main.rs:236-296`) for no gain. Keep
  caps flowing via `cap_install`; the table names the slots. The 4 message-cap slots stay
  unused on the startup path.
- **Rejected — keep three separate (renamed) formats.** Defeats the purpose; the audit's
  complaint is precisely the *bespoke, per-pair* layouts. One format, one codec, one fuzz
  target.
- **Rejected — a length-prefixed self-describing TLV per field (postcard-style).** More
  general than needed and heavier to keep total in `_start`; the fixed header + kind-tagged
  fixed entries are simpler to prove refuse-not-crash and to fuzz, and the `name=0` escape
  already buys the extensibility a TLV would. (CAS uses TLV where it must migrate on disk;
  the startup block never persists, so it does not need TLV's open-endedness now.)

**Recommendation: one `b"EUS1"` block — fixed header, kind-tagged grant entries (CAP_SLOT /
STORAGE_HANDLE / REGION), then argv then env; well-known `u8` name ids with a `name=0`
string escape reserved; total decode (fuzzed) and budget-checked encode. Caps stay on the
`cap_install` path; the table names slots.**

---

## Design decision 3 — storage-handle delivery: name the existing session for direct children; defer grandchild sessions *(resolve across C1)*

The spec's storage-handle kind delivers "handle numbers on the process's storage session
channel … pre-populated by the parent (§2.4)." What that requires depends on *whose* child.

- **Adopted — deliver `storage`/`root` (and `tmp` if carvable) under names to init's direct
  children, where the session already exists; record grandchild sessions as out of scope.**
  - *init → shell:* the session channel is `cap_install`ed at slot 1 and the shell uses
    `handle: 0` (`user/shell/src/runtime.rs`). C1 emits a `STORAGE → CAP_SLOT(1)` entry and a
    `ROOT → STORAGE_HANDLE(0)` entry; the shell resolves `storage`/`root` from the table
    instead of hardcoding `STORE_CHAN = 1` / `handle: 0`. Authority is identical — C1 *names*
    it. This is the parent plan's "standard names resolve" (`:678`).
  - *`tmp`:* there is no `tmp` ref or subtree today (mkfs creates `main`,
    `mkfs/src/main.rs`). C1 delivers `tmp` only if a `tmp` subtree under `main` is trivially
    grantable as a **subtree-scoped** `STORAGE_HANDLE` (an `OpenChild`-style attenuation init
    performs in-process before opening the shell's session — `HandleEntry` with a `subtree`,
    `storage-server/src/lib.rs:71-88`). If standing up a writable `tmp` subtree pulls in store
    layout choices, C1 **reserves** the `TMP` name (defined, unpopulated) and a follow-on
    delivers it — recorded, not silently dropped. *Recommendation: reserve `tmp`; deliver it
    only if the subtree-scoped grant is a one-liner.*
  - *Grandchildren (the shell's children) get no storage* — exactly as today. Their own
    session needs the rev1§2.4 cross-session connect-with-delegation op, which **does not
    exist** (only `OpenChild` *within* a session; no "open a new session on an offered
    endpoint"). The shell cannot share its single-holder session channel (move semantics,
    rev1§3.4). So C1's table *defines* `STORAGE`/`ROOT` for the shell→child block but init/the
    shell simply omit those entries for grandchildren until the connect op lands.
- **Rejected — build the rev1§2.4 connect-with-delegation op as part of C1.** It is real
  protocol surface (a new `Request` variant that opens a session on an offered endpoint with
  attenuated handles, plus the funding/teardown story) overlapping **B5** (storage protocol)
  and **B1** (per-process sessions + `stat-store` stripping — the storaged comment at
  `user/storaged/src/main.rs:211-213` already names this). Bundling it makes C1 a storage-
  protocol phase, not the startup-format phase the console track is waiting on. Keep C1 to the
  format + naming; the connect op is its own follow-on (and B1/B5's natural home).

**Recommendation: deliver `storage`/`root` under names to init's direct children (rename, no
new authority); reserve `tmp` (deliver only if the subtree grant is trivial); leave
grandchild storage sessions and the rev1§2.4 connect op out of scope, recorded.**

---

## Design decision 4 — `stdin`/`stdout`: reserve the names in C1, populate them in C-M9 *(resolve in C1C)*

The parent plan splits the console across C1 (the table) and C-M9 (the driver + channel):
C-M9 *"deliver[s] the console cap under standard names"* (`:704-706`), and the rev1§7 exit
condition needs *both* the table (C1) and the device-IRQ path (B-IRQ). So C1 must make
`stdin`/`stdout` *resolvable* without building the console.

- **Adopted — define `STDIN`/`STDOUT` as standard `CAP_SLOT` names in the format; do not
  populate them in C1; the shell keeps the rev1§7 debug-syscall scaffold for terminal I/O.**
  - The format reserves `STDIN=2`/`STDOUT=3` (Design decision 2). C1's producers (init →
    shell) **omit** these entries — there is no console channel cap to name yet — and the
    shell's terminal I/O stays on `sys::debug_getc`/`debug_write` (`user/shell/src/runtime.rs:
    656`, `:72`), the sanctioned scaffold (rev1§7).
  - C-M9 then becomes a pure *population* step: init grants the console-driver channel cap
    into the shell's cspace, emits `STDOUT → CAP_SLOT(n)` and `STDIN → CAP_SLOT(n)` (the same
    channel under both names — "an interactive console is the same channel granted under both
    names," rev1§5.1), and the shell switches its I/O to that channel. **No format change in
    C-M9** — that is the whole point of reserving the names now.
  - The "deliberately split" property (rev1§5.1: a pipeline wires one process's `stdout` to
    another's `stdin`) is a *format* capability C1 delivers (two independent name slots that
    can point at different channels) even though C1's only producer grants neither — the
    socket is there for shells/pipelines to use once channels exist.
- **Rejected — wire `stdin`/`stdout` to a placeholder (e.g. the bootstrap channel) in C1.**
  A name that resolves to the wrong cap is worse than an absent name — the shell would have to
  special-case it, and C-M9 would have to un-wire it. Absent-but-defined is the honest state.
- **Rejected — pull the console into C1.** That is C-M9 (depends on B-IRQ); C1 is its
  prerequisite, not its body (parent plan `:788-790`).

**Recommendation: reserve `STDIN`/`STDOUT` as `CAP_SLOT` names, leave them unpopulated in C1
(shell stays on the debug scaffold), and document the C-M9 hand-off (populate the names, no
format change).**

---

## Design decision 5 — argv replaces the ST01 mode byte; env is plumbed but empty *(resolve in C1D)*

The shell→child block today carries a one-byte *mode* (`runtime.rs:419-423`), parsed by
`run <path> [mode]` (`cmd_run`, `:506-522`) and switched on by selftest (`:117-146`). C1
generalizes this into a real argv vector — the concrete, host-testable argv deliverable.

- **Adopted — the shell builds `argv` from the command line and delivers it in the block;
  env is carried (empty) for forward-compatibility; selftest reads its mode from argv.**
  - `cmd_run`/`cmd_runloop` split the command tail into argv byte-strings (the existing
    `splitn`/`parse_path` machinery, `runtime.rs:507-509`) and the producer encodes them into
    the block's argv vector (Design decision 2). The first element is conventionally the
    program path; subsequent elements are arguments.
  - **selftest's mode** moves from the bespoke ST01 mode byte to `argv` — e.g. mode is
    `argv[1]` parsed as the existing integer (`parse_st01` → read argv), preserving every
    QEMU smoke mode (`0xFF` fault, `0xFE` panic, `0xFD` time-ok, otherwise `exited(mode)`,
    `user/selftest/src/main.rs:117-146`). The smoke (`run <selftest> 254` etc.) still drives
    each path; the *byte that selects the path* now arrives as an argument, not a fixed field.
  - **env** is encoded as an empty vector (`nenv = 0`) — no producer sets env yet and no
    consumer reads it, but the format carries it (rev1§5.1 lists both argv *and* env), so the
    field is plumbed and tested (round-trips, refuse-not-crash) rather than retrofitted. The
    honest statement: argv is *live*, env is *defined and empty*.
  - **`time`** continues to ride the shell→child block as a `REGION` entry (`CHILD_TIME_VA`,
    `runtime.rs:56`), so the selftest `0xFD` time-grant witness still passes.
- **Rejected — keep the mode byte alongside argv.** Two ways to pass the same datum invites
  drift; the mode *is* an argument, so it becomes `argv`. (B15C's ST01 tests, which pin the
  mode byte, are replaced by C1D's argv tests.)
- **Rejected — deliver env from a real source now.** There is no env source (no shell env
  var support); carrying it empty is the right MVP — defined, total, untested-against-content
  beyond round-trip. A real env populates the same field later, no format change.

**Recommendation: argv from the command line replaces the mode byte (selftest reads its mode
from argv, every smoke path preserved); `time` stays a region grant; env is carried empty.**

---

## Sub-phase C1A — the startup-block codec, host tests, and fuzz target *(must-do; the foundation; no wiring change)*

The format + codec behind the `loader::startup` seam (Design decisions 1, 2), fully tested
and fuzzed, with **no** producer/consumer rewired yet — the three hand-rolled blocks keep
working. A pure, low-risk addition that unblocks C1B/C1C/C1D.

- **Touches:**
  - `loader/src/startup.rs` — **new**: the `Startup` model, the well-known name ids + kind
    constants (Design decision 2), `pub fn encode(&Startup) -> Result<…, Overflow>` and
    `pub fn decode(&[u8]) -> Option<Startup<'_>>` (borrowed where possible), and a
    `#[cfg(test)] mod tests`.
  - `loader/src/lib.rs` — add `pub mod startup;` beside `pub mod elf;` (`:9`), **not** under
    the `target_os = "none"` gate (`:11-12`) — it is host-buildable.
  - `loader/fuzz/fuzz_targets/startup.rs` + `loader/fuzz/Cargo.toml` — **new** fuzz target
    (decode totality), mirroring the ELF target; seed a small corpus.
  - `loader/tests/fuzz_regressions.rs` — the home for any crash the fuzz target finds
    (alongside ELF-1).
- **Depends on:** Part A blessed (rev1§5.1/§2.7 text). No intra-C1 dependency.
- **Work:**
  1. Define the `Startup` model + name-id/kind constants; write `decode` (total: validate
     magic, then bounds-check every count/length against the remaining slice before reading)
     and `encode` (budget-checked, `Err` on > 256 bytes).
  2. Golden-layout unit tests (a hand-built block decodes to the expected entries; the byte
     offsets are pinned) + an encode→decode round-trip proptest (`cfg(miri){4}else{256}`).
  3. A **totality** proptest over arbitrary `Vec<u8>`: `decode` never panics (the rev1§2.7
     floor) — the same shape as B15C's `parse_config_is_total`.
  4. The cargo-fuzz target (decode over libfuzzer-arbitrary bytes); add it to the CLAUDE.md
     Quickest-UB Miri pass alongside loader's existing targets.
  5. A **negative control** (the project's anti-theater habit): a deliberately wrong expected
     decode must make a round-trip test fail — confirming the oracle has teeth.
- **Acceptance:**
  - `cargo test -p loader` green (startup tests + the existing elf tests).
  - The fuzz target runs clean over a short campaign; the corpus replays UB-free under Miri
    (`--test fuzz_regressions --test fuzz_corpus`).
  - `decode` is total over arbitrary bytes (refuse-not-crash); `encode` refuses an
    over-budget block with `Err`, never a panic; the broken-oracle control fails.
  - No `user/*` binary changed yet; `scripts/run-demo.sh` still boots green on the *old*
    blocks (C1A is a pure addition).
- **Effort/Risk:** M / low. The substance is getting `decode` provably total and the budget
  refusal right; the model is small.

---

## Sub-phase C1B — rewire init ↔ storaged (SD02 → the named-grant table) *(must-do; the region-grant proof-out)*

Migrate the first producer/consumer pair onto the format. SD02's five region VAs become three
`REGION` entries (`TIME`, `VIRTIO_MMIO`, `DMA`), exercising the region kind end to end. Pair
changes together; touches only init's storaged-half and storaged.

- **Touches:** `user/init/src/main.rs` — replace `build_sd02` (`:84-93`) + its send
  (`:219-223`) with a `loader::startup::encode` of a table carrying `TIME`(region VA),
  `VIRTIO_MMIO`(region VA+len), `DMA`(region VA+len+PA); drop the B15C `build_sd02` tests
  (`:341-356`, `:387-394`). `user/storaged/src/main.rs` — replace `parse_config`/`Config`
  (`:123-150`) + the `_start` parse (`:155-167`) with `loader::startup::decode` + name lookups
  (`mmio_va = grants[VIRTIO_MMIO].va`, etc.); the time attach (`:170`) and the rest of `_start`
  unchanged; port the B15C parse tests to the new decoder. `user/storaged/Cargo.toml` — add
  `loader = { path = "../../loader", default-features = false }` (Design decision 1).
- **Depends on:** C1A. No other C1 sub-phase.
- **Work:**
  1. init: build the storaged grant table (3 regions) and `encode`; `check(...)` the
     `Overflow`/`chan_send` results (refuse-not-crash on the producer side).
  2. storaged: `decode` the block, look up the three region names, fail cleanly
     (`fail(b"bad startup block")`) if any required name is absent (a missing grant is a boot
     failure, but a *clean* one — no panic). Drive the device probe / DMA pool exactly as
     today from the looked-up values.
  3. Port the B15C SD02 round-trip + refuse-not-crash tests to the new format (init builds via
     `encode`, storaged reads via `decode`; the format is now *shared*, so the test drives the
     real codec on both ends — no more mirrored hand-parsers).
- **Acceptance:**
  - `cargo test --manifest-path user/storaged/Cargo.toml` green; `cargo test -p loader`
    unaffected.
  - `scripts/run-demo.sh`: storaged still finds the virtio-blk device, mounts, and serves
    (`[storaged] virtio-blk up` → `store mounted` → `serving`) — the region grants arrived.
  - A storaged block missing a required region is refused cleanly (no panic); the aarch64
    build links.
- **Effort/Risk:** S–M / low. Mechanical once C1A exists; the value is proving the region kind
  on the most region-heavy block.

---

## Sub-phase C1C — rewire init ↔ shell (SH01 → the table) + resolve the standard names *(must-do; the headline "standard names resolve")*

The named-grant headline: the shell stops hardcoding slot 1 / handle 0 / the time VA and
resolves `storage`/`root`/`time` from the table; `stdin`/`stdout`/`tmp` are reserved
(Design decisions 3, 4). Pair changes together.

- **Touches:** `user/init/src/main.rs` — replace `build_sh01` (`:99-104`) + send (`:280-284`)
  with `encode` of a table carrying `TIME`(region VA), `STORAGE`(cap-slot 1), `ROOT`(storage-
  handle 0), and (if trivial, DD3) `TMP`(subtree-scoped handle); the slot/handle numbers come
  from init's existing `cap_install` wiring (`:286-296`), now *named*. `user/shell/src/runtime.rs`
  — replace the inline SH01 parse (`:626-637`) with `decode` + name lookups; replace the
  hardcoded `STORE_CHAN`/`SH_TIME`/`handle: 0` constants (`:27-56`, and `handle: 0` call sites)
  with the resolved values (or keep the consts but *assert* they match the table, the lower-risk
  intermediate). `user/shell/src/main.rs` — any pure argv/table-lookup helper joins the
  host-tested logic (`:49-186`).
- **Depends on:** C1A. Independent of C1B/C1D.
- **Work:**
  1. init: build the shell grant table (`TIME`, `STORAGE`, `ROOT`, reserved-not-emitted
     `STDIN`/`STDOUT`, optional `TMP`) and `encode`/send.
  2. shell: `decode` in `_start`; resolve `storage` → the session-channel slot used by
     `request` (`runtime.rs:94-111`), `root` → the handle passed to every `Request`, `time` →
     the VA passed to `urt::time::attach`. A degraded path stays (an absent `time` → no clock,
     `date` degrades — the current `blen >= 12` fallback, `:632`), now keyed on name presence.
  3. Pin (host test, in `main.rs`/`mod tests`) the pure name-resolution helper: given a decoded
     block, the right slot/handle/VA come back; an absent name yields the documented fallback.
- **Acceptance:**
  - `cargo test --manifest-path user/shell/Cargo.toml` green (B15B logic + the new resolution
    helper tests).
  - `scripts/run-demo.sh`: the shell boots, `date` works (time resolved), and the store-backed
    built-ins work (`storage`/`root` resolved) — `ls`/`cat`/`write`/`df`/`snap` behave.
  - `stdin`/`stdout` are defined in the format but unpopulated; the shell's terminal I/O is
    still the debug scaffold (rev1§7); the C-M9 hand-off is documented.
- **Effort/Risk:** S–M / low–medium. The care is in resolving names without changing the
  shell's authority (rename, not re-wire); the assert-the-consts intermediate de-risks it.

---

## Sub-phase C1D — rewire shell ↔ child (ST01 → the table) + argv; update selftest & hello *(must-do "where feasible"; the argv deliverable)*

The last block pair, and the argv headline (Design decision 5): the shell delivers argv (the
command line) instead of a mode byte; selftest reads its mode from argv; hello aligns onto the
format. Pair changes together.

- **Touches:** `user/shell/src/runtime.rs` — replace the inline ST01 build+send (`:419-423`)
  with `encode` of a table carrying `TIME`(region) + an `argv` vector built from the command
  tail (`cmd_run`/`cmd_runloop`, `:506-564`); the per-child `cap_install` (`:418`) unchanged.
  `user/selftest/src/main.rs` — replace `parse_st01`/`St01` (`:64-79`) + the `_start` parse
  (`:107-115`) with `decode` + read mode from `argv` and `time` from the table; the mode
  dispatch (`:117-146`) unchanged; port the B15C tests to argv. `user/hello/src/main.rs` —
  replace the `startup:hello` check (`:29-33`) with `decode` (read argv, or just decode-and-ack
  — hello is a smoke subject). `user/{selftest,hello}/Cargo.toml` — add `loader`
  (default-features off).
- **Depends on:** C1A. Independent of C1B/C1C.
- **Work:**
  1. shell: build the child grant table (`TIME` region + argv from the command line) and
     `encode`/send; `cmd_run`'s mode parse (`:509`) becomes "argv[1] if present."
  2. selftest: `decode`; mode = parse argv (preserving `0xFF`/`0xFE`/`0xFD`/other); `time` from
     the table → `attach`. Every QEMU smoke mode preserved.
  3. hello: `decode` the block (refuse-not-crash) and ack — retire the bespoke magic string.
  4. Port the B15C ST01 tests to the argv form (the decoder is now shared `loader::startup`).
- **Acceptance:**
  - `cargo test --manifest-path user/selftest/Cargo.toml` green; `cargo test -p loader`
    unaffected.
  - `scripts/run-demo.sh`: `run <selftest> 254` → `exited(254)`, `run <selftest> 255` →
    `faulted(...)`, the panic mode → `panicked`, the time mode → `time-ok`, and `runloop`
    still recycles slots — i.e. the argv-carried mode drives every path the ST01 mode byte did.
  - `hello` (if spawned) boots and acks on the new format; no consumer reads the old blocks.
- **Effort/Risk:** S–M / low. The argv encode/decode is the new bit; the mode→argv move is
  mechanical and smoke-covered.

---

## Execution order

```
C1A  startup codec + host tests + fuzz target   [foundation; pure addition, no wiring change]
       │
       ├─► C1B  init ↔ storaged (SD02 → table; the region kind)
       ├─► C1C  init ↔ shell   (SH01 → table; standard names resolve; reserve stdin/stdout)
       └─► C1D  shell ↔ child  (ST01 → table; argv; selftest + hello)
```

**C1A is the prerequisite**; **C1B / C1C / C1D are mutually independent** (different
producer/consumer pairs, different binaries) and can land in any order or in parallel after
C1A. Each pair migrates atomically (the producer and its consumer change together, since the
format on that channel changes), but the *pairs do not serialize* — the bootstrap message is
per-channel, so storaged's block, the shell's block, and the child's block migrate
independently. C1 as a whole depends only on Part A being blessed.

The cleanest **landing discipline**: C1A behind the seam first (no behaviour change, full
green); then each rewiring pair, **re-running `scripts/run-demo.sh` after each** (the format
is on the boot path — a half-migrated block is an un-bootable system, so the boot smoke is the
gate that each pair flipped both ends together). The two spec edits (§5.1 forward note, §8.3
split) land with the final pair, when the table is fully in use.

## Out of scope for C1 (recorded so it is not mistaken for a gap)

- **The userspace console driver + `stdin`/`stdout` population** — that is **C-M9** (depends
  on **B-IRQ** + C1). C1 *reserves and resolves* the `stdin`/`stdout` names so C-M9 is a pure
  population step (init grants the console channel under both names; the shell switches its
  I/O), but C1 builds no driver and the shell keeps the rev1§7 debug scaffold (Design
  decision 4; parent plan `:681-712`).
- **Cross-session storage delegation (a grandchild's own storage session)** — needs the
  rev1§2.4 connect-with-delegation wire op, which does not exist (only `OpenChild` *within* a
  session). It overlaps **B5**/**B1** (the `user/storaged/src/main.rs:211-213` per-process-
  session + `stat-store`-stripping work) and is its own follow-on. C1 delivers `storage`/`root`
  under names only to init's direct children, where the session already exists (Design
  decision 3).
- **A stable, language-neutral public ABI for the startup block** — stays §8.3-deferred
  (the non-Rust-userspace / IDL / stable-syscall-ABI effort). C1 delivers a Rust-internal
  format with a `name=0` string escape reserved so widening to a public ABI later needs no
  format break; it does **not** freeze a wire contract (spec edit: split §8.3 line 487).
- **A real `env` source and shell env-var support** — C1 carries `env` as a defined, empty,
  round-tripped field; populating it from a real source is later work, no format change
  (Design decision 5).
- **`cwd`** — reserved by rev1§5.1 (`spec_rev1.md:363`: "whether the shell passes it or folds
  it into how it constructs `root` is a shell-level choice"); C1 does not deliver it.
- **The broker / registry process** — rev1§5.2/§8.3 deferred; init stays "the only binder"
  (static wiring). C1 is the named-grant *table*, not service discovery.
- **Verus/TLA/Loom for the codec** — it is Baseline-tier userspace tooling (like
  `loader::elf`), so C1 adds no mechanized proof, no model, no concurrency harness; the tally
  stays **14** and every Verus/TLA gate is held by not touching it. The decode-totality
  guarantee is the *fuzz + proptest* obligation, recorded honestly (rev1§6.1).
- **Adding `loader`/`user/*` to the standing CLAUDE.md Verus/Miri *crate* sweeps** — the
  startup codec joins loader's existing `fuzz_regressions`/`fuzz_corpus` Miri pass (the
  decoder UB gate); no new crate-level sweep is added.
