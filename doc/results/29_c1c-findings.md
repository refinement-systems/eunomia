# C1C — rewire init ↔ shell onto the named-grant table; standard names resolve

Phase **C1C** of `doc/plans/17_c1-detail.md` (the detailed decomposition of
parent-plan C1, `doc/plans/0_address_audit_rev0.md:662-679`). C1A delivered the
unified `b"EUS1"` startup-block codec behind the `loader::startup` seam with no
producer/consumer rewired; **C1C migrates the first standard-name pair** — the
init→shell bootstrap channel — off the bespoke 12-byte `SH01` block and onto the
codec, and makes the shell **resolve the standard names `time`/`storage`/`root`**
from the table instead of hardcoding the time-page VA, cspace slot 1, and storage
handle 0. This is the named-grant headline ("standard names resolve").

C1C is independent of C1B (init↔storaged / `SD02`) and C1D (shell↔child / `ST01`),
neither of which is landed; both blocks are **untouched** here.

## What landed

- **`user/init/src/main.rs`** — `build_sh01(time_va) -> [u8;12]` replaced by
  `build_shell_block(out: &mut [u8]) -> Result<usize, EncodeError>`, which encodes
  an `EUS1` table carrying three grants:
  - `time`  → `Region { va: TIME_VA (0xA300_0000), len: 4096, pa: 0 }` — the
    read-only page init already maps into the shell's aspace (rev1§2.6); only the
    VA travels, exactly as before.
  - `storage` → `CapSlot(1)` — the session channel init `cap_install`s at the
    shell's cspace slot 1.
  - `root` → `StorageHandle(0)` — the full-rights ref root.

  The send site encodes into a `[u8; MAX_BLOCK]` and maps an `EncodeError` to a
  clean boot failure (`debug_write` + `exit`, refuse-not-crash, rev1§2.7) — never
  a panic. A new `SHELL_SESSION_SLOT = 1` const is used **both** in the table and
  in the session `cap_install`, so the name and the install cannot drift.
- **`user/shell/src/main.rs`** — three pure, host-tested resolvers:
  `resolve_storage_slot` / `resolve_root_handle` / `resolve_time_va`, each a
  `match` on `Startup::grant(name)` returning the slot/handle/VA or `None` for an
  absent-or-wrong-kind grant.
- **`user/shell/src/runtime.rs`** — `_start` now `loader::startup::decode`s the
  bootstrap message and resolves the three names. `storage`/`root` are stored in
  two `AtomicU32`s (`STORE_SLOT`, `ROOT_HANDLE`) whose defaults equal the old
  constants (`STORE_CHAN = 1`, handle `0`); `request()` reads `store_slot()` and
  every storage `Request` reads `root_handle()` (14 call sites, was `handle: 0`).
  `time` resolves to the region VA passed to `urt::time::attach`.
- **Tests** — init's `shell_block_carries_named_grants` drives the *shared* codec
  on both ends (encode → decode → assert grants), replacing the old `SH01` golden
  / round-trip tests and the `parse_sh01` mirror; the `SD02` tests are kept
  intact (C1B's territory). The shell gains `resolve_names_golden`,
  `resolve_names_absent_yields_none`, and `resolve_wrong_kind_yields_none` (the
  negative control: a name delivered as the wrong kind is unresolvable).

No spec, no ledger, no Verus/TLA, no new fuzz target touched (see "Scope" below).

## Design notes

- **Genuine resolution, not assert-the-consts.** The plan offered keeping the
  constants and merely asserting the table agrees as a lower-risk intermediate;
  C1C does the full thing — the shell stops hardcoding the slot/handle/VA and
  reads them from the table. Authority is **identical** (the resolved values equal
  the old constants), so an absent grant degrades to today's behaviour via the
  atomic defaults; the win is that the contract now flows through the table, so if
  init ever moves the slot the shell follows (rev1§5.1's "caps resolve to cspace
  slots"). The boot smoke gates the rewire.
- **Two "time" concepts, only one named.** The shell holds *two* time things: the
  read-only page mapped at `TIME_VA` (its own clock, delivered as the `time`
  region grant) and a re-grantable time **cap** at cspace slot 5 (`SH_TIME`) that
  it copies/maps into each child it spawns. C1C names only the former; the slot-5
  re-grant cap stays a `cap_install` convention used by the unchanged
  shell→child (`ST01`) path — that is C1D's surface, deliberately left alone.
- **`Relaxed` atomics are sufficient.** The shell is single-threaded
  (cooperative `yield_now`), and the names are resolved in `_start` before the
  REPL runs, so the stores happen-before every read in program order.

## Scope (recorded so it is not mistaken for a gap)

- **`stdin`/`stdout` reserved, not populated** (Design decision 4): init emits no
  entry for them; the shell keeps the rev1§7 `debug_getc`/`debug_write` scaffold.
  C-M9 becomes a pure population step (init grants the console channel under both
  names; the shell switches its I/O) — **no format change** needed then.
- **`tmp` reserved, not delivered** (Design decision 3): there is no `tmp` subtree
  today (`mkfs` builds only `main`), so a writable subtree-scoped grant is not a
  one-liner. The `NAME_TMP` id exists; a follow-on (overlapping B1/B5) delivers it.
- **No spec or ledger edit.** The two `spec_rev1.md` edits (§5.1 forward note,
  §8.3 split) land with the *final* migrated pair, when the table is fully in use
  (`17_c1-detail.md:686`); C1B/C1D are still pending. C1A already recorded the
  codec's test/fuzz gates in the ledger; C1C adds no new seam (tally stays **14**)
  and no new fuzz target — the decoder it exercises is C1A's, already fuzzed.

## Verification

- `cargo test --manifest-path user/shell/Cargo.toml` — **20 passed** (B15B logic
  + the 3 new resolver tests).
- `cargo test --manifest-path user/init/Cargo.toml` — **5 passed** (new shell-block
  test + retained `SD02`/RTC tests).
- `cargo test -p loader` — **2 passed** (unaffected).
- `cd kernel && cargo build` — links every `user/*` binary for
  `aarch64-unknown-none-softfloat` (build.rs `rerun-if-changed` on `user/init` +
  `user/shell` rebuilds both).
- `cargo fmt --check` clean across root + `user/init` + `user/shell` manifests
  (the workspace-split trap).
- **QEMU boot smoke** (`scripts/run-demo.sh`, CLAUDE.md timeout harness) — green:

  ```text
  [storaged] store mounted → serving
  eunomia> date                 → 2026-06-22T19:49:54.015713008Z   (time resolved)
  eunomia> ls                   → bin/ docs/ hello.txt             (storage/root resolved)
  eunomia> write docs/c1c …     → ok
  eunomia> cat docs/c1c         → hello-from-c1c                   (round-trips through the store)
  eunomia> df / snap / snaps    → all behave
  eunomia> run bin/selftest 42  → exited(42)                       (ST01 child path unchanged)
  eunomia> run bin/selftest 255 → faulted(translation, 0xdead0000)
  eunomia> runloop bin/selftest 5 → 5/5 ok, slots 56/56
  ```

  `date` printing a real timestamp witnesses the `time` region grant resolving;
  the store-backed built-ins witness `storage`/`root` resolving; the selftest
  spawn/reap loop witnesses the untouched shell→child (`ST01`) path. No
  panic/`Corrupt`/`unwrap`.

## Follow-ons

- **C1B** (init↔storaged, `SD02` → table; the region kind on the most
  region-heavy block) and **C1D** (shell↔child, `ST01` → table + argv; selftest +
  hello) — independent, can land in any order after C1A.
- The two `spec_rev1.md` edits + any ledger Baselines note land with whichever of
  C1B/C1D is last.
- **C-M9** populates `stdin`/`stdout` (no format change); a follow-on delivers
  `tmp`.
