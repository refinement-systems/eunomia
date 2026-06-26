# Findings — Phase 1.1: the `eunomia-sys` PAL↔OS seam crate

Task 1.1 of `doc/plans/1_plan-rust-std-port.md`. Creates the new Verus-gated crate
`eunomia-sys`: the raw `svc #0` syscall wrappers (trusted inline-asm shell), a
**Verus-verified** syscall-argument encoder (the inverse of `kcore::sysabi::decode`),
and a named-grant resolver over `loader::startup`. Joins the `cargo verus verify` gate
as a new Baseline row — **not** a new trusted seam (the tally stays 14).

## What shipped

- `eunomia-sys/` — new root-workspace crate, `#![cfg_attr(not(test), no_std)]`:
  - `src/encode.rs` — the **verified** core (`verus!{}`): `Call` (a field-for-field
    twin of `kcore::sysabi::Sys`, 26 defined variants), `Encoded {nr, a0..a5}`,
    `CallError`, and `encode(Call) -> Result<Encoded, CallError>` proven total with
    full per-variant placement + inverse-leak refusal. Local ABI bound consts
    (`MSG_PAYLOAD`, `NUM_PRIOS`, `OBJ_COUNT`). Host tests (round-trip oracle, teeth,
    refusal, constant-drift, proptests).
  - `src/syscall.rs` — the **trusted shell** (no `verus!{}`): the `imp` `svc #0` asm
    (cfg-gated aarch64+eunomia/none, host `unreachable!` stub) + the typed wrappers
    (`chan_send`, `yield_now`, …) that run each `Call` through `encode` then issue the
    `svc`; an `encode` refusal maps to `ERR_ARG`. The ABI constant surface (`ERR_*`,
    `OBJ_*`, rights, events, `STATUS_PANIC`, …).
  - `src/grant.rs` — the named-grant resolver over `loader::startup` (typed
    `stdin_slot`/`stdout_slot`/`storage_slot`/`root_handle`/`time_va`/… + the
    `BOOTSTRAP_CHANNEL = 0` constant), re-exporting loader's types.
- Wiring: added to the root `Cargo.toml` members; `cargo verus verify -p eunomia-sys`
  added to the `verus` CI job; a Baseline row + a routing note in
  `doc/guidelines/verus_trusted-base.md`.

## Decisions (and rejected alternatives)

- **The verified obligation is the syscall *encoder*, the inverse of
  `sysabi::decode`.** `encode` is mechanized total over every `Call`: it always emits a
  defined opcode (`nr < 27`), places each used argument in the register `decode` reads
  it from, and *refuses* exactly the out-of-range fields the kernel rejects (send-length
  cap, `ObjType`/event/which/priority ranges), accepting the in-range complement. This
  makes the §11 inverse-leak rule machine-checked: the PAL provably cannot construct a
  shape-rejectable syscall.

- **Total `Result`-returning encoder, *not* a `requires`-guarded partial function** (a
  refinement of the approved plan's "panic-free with inverse-leak `requires`" posture).
  Validating internally and returning `Err` moves the inverse-leak *discharge* into the
  verified surface — `encode` is the verified place the bound is re-established — rather
  than leaving it to inspection of the (trusted, outside-`verus!{}`) wrappers. It is
  also truly total like `decode`, and plain-Rust wrappers compose safely (map `Err` →
  `ERR_ARG`). Strictly stronger; adopted.

- **The verified gate stays `kcore`-free; cross-side agreement is a host test.**
  `encode`'s constants are a local independent twin of rev2§3.7 — exactly the posture
  `ipc/src/sys.rs` already takes (it hardcodes opcodes, never linking `kcore`). `kcore`
  is a **dev-dependency** only, so it never enters `cargo verus verify`'s graph
  (verify builds the lib, not the test target). The host
  `encode_round_trips_through_kernel_decode` proptest asserts
  `decode(encode(call)) == Ok(call)` against the *real* kernel decoder.
  - *Rejected:* a round-trip *proof* against `kcore::decode`. (a) It would drag
    `kcore`'s 406 obligations into eunomia-sys's verify session (CI-budget sensitive).
    (b) It breaks the userspace/kernel decoupling. (c) It is anyway **not available**:
    `decode`'s `ensures` are shape-only (e.g. there is no
    `Ok(ChanSend{chan,..}) ⇒ chan == a@[0]`), so a functional inverse cannot be stated
    against the existing contract without first writing a complete `decode_spec`. The
    host proptest gives the same agreement guarantee at zero gate cost.

- **Host the `svc` asm in `eunomia-sys`; do not refactor `ipc` in 1.1.** The asm is
  copied (the `ipc::sys::imp` pattern), kept **outside** `verus!{}` as plain cfg-gated
  Rust — category-(d) trusted inline asm, so it adds 0 to the `external_body` tally and
  no new seam. *Rejected for 1.1:* moving the asm out of `ipc` and making `ipc`/`urt`
  delegate — large blast radius on the green stack. The ~40 lines of duplicated trusted
  asm are temporary; consolidation is a follow-up.

- **Return a flat struct `Encoded {nr, a0..a5}`, not `[u64;6]`.** Named-field `ensures`
  (`e.a0 == chan`) are trivial; an array `e@[i]` view requires
  `broadcast use vstd::array::group_array_axioms` to discharge `e@[i] == field` (the
  `freelist::new` pattern). The struct dodges it; the proof is near-automatic.

- **No `std` feature; `#![cfg_attr(not(test), no_std)]` (the kcore posture).** A
  `default = ["std"]` feature would make a plain `cargo verus verify -p eunomia-sys`
  check the *std* variant (not what ships) and force `--no-default-features` on the CI
  line — the exact reason `loader` carries that flag. The kcore posture avoids it: the
  verify run sees no_std (cfg(test) unset), host tests get std under cfg(test).

- **Grant lookup is a thin resolver over the (separately-verified) `loader::startup`
  decoder.** 1.1 *adds* the canonical `resolve_*` helpers; it does not delete
  `user/shell`'s / `user/init`'s private copies (separate workspaces — switching them
  over is the blast-radius-deferred follow-up). `BOOTSTRAP_CHANNEL = 0` matches every
  child's convention (`BOOT_CHAN = 0` in shell/storaged; init installs each child's
  endpoint at slot 0).

## Problems hit and how they were solved

- **Verus `matches`-binder scope across `==>`.** First attempt wrapped the antecedent
  matches in parens — `(call matches Pat) ==> consequent` — and the pattern bindings did
  **not** reach the consequent (rustc E0425, "not found in this scope"). Verus only
  treats `EXPR matches Pat ==> body` as a binding form when the `matches` is the bare
  left child of `==>` (the `decode` idiom). Fix: drop the parens.

- **Verus `matches`-binding `&&` is greedy.** The next attempt,
  `result matches Ok(e) && call matches Pat ==> placement`, *compiled* but **failed
  verification** at the `Err` early-returns: Verus extends the `matches`-binding `&&`
  rightward, grouping it as `result matches Ok(e) && (call matches Pat ==> placement)` —
  i.e. `result matches Ok(e)` became a *hard conjunct* of the postcondition, false when
  `encode` returns `Err`. Fix: **fully nested** implications,
  `result matches Ok(e) ==> (call matches Pat ==> placement)` and
  `call matches Pat ==> (cond ==> result is/matches …)`, each a clean single-`matches`
  binding form. Documented inline on `encode` so the next editor does not "simplify" it
  back. Verified `7 verified, 0 errors`.

- **verusfmt ↔ cargo fmt non-fixed-point.** With the asm shell, the constants, and the
  `verus!{}` block all in one file, `verusfmt --verus-only` reformatted the *whole* file
  (it processes any file containing `verus!`), including the plain-Rust
  `#[cfg(all(...))]` on the asm module and comments inside `ensures`, and disagreed with
  `cargo fmt` — so `scripts/verusfmt.sh --check` never settled. Fix: **split** the
  verified encoder into `encode.rs` (the only `verus!` file) and the asm shell +
  wrappers + constants into `syscall.rs` (no `verus!`, so verusfmt skips it entirely),
  and drop the inline comments inside `ensures` (rationale moved to the fn doc). The two
  tools now have disjoint domains — a stable fixed point (`scripts/verusfmt.sh --check`
  + `cargo fmt --check` both clean). No skip-list entry needed.

## Verification record

- **Gate (authoritative, cold):** `cargo clean && cargo verus verify -p eunomia-sys`
  → per-crate results lines: `le-bytes 6`, `ipc 71`, `loader 12` (transitive deps,
  no-alloc prelude), then **`eunomia-sys: 7 verified, 0 errors`** (its own count). Re-run
  green over the formatted, split tree.
- **Host tests:** `cargo test -p eunomia-sys` → **8 passed, 0 failed**
  (`encode_round_trips_through_kernel_decode`, `round_trip_oracle_has_teeth`,
  `encode_refuses_what_the_kernel_rejects`, `constants_match_kcore`, the
  `map_round_trips` / `chan_send_honors_payload_cap` / `thread_start_honors_prio_cap`
  proptests, and the grant `resolvers_read_each_named_grant`).
- **no_std cross-build:** `cargo build -p eunomia-sys --target
  aarch64-unknown-none-softfloat` finishes clean — the crate is no_std and the real
  `svc #0` asm path (`target_os = "none"`) compiles (the host build links the stub).
  The runtime exercise of the asm (executing the `svc`) is deferred to the std-port
  Phase 2 QEMU boot.
- **Formatting:** `scripts/verusfmt.sh --check` + `cargo fmt --check` clean.
- **Ledger:** new Baseline row (`-p eunomia-sys`, 7 verified) + the
  "eunomia-sys syscall-marshalling routing note" added to
  `doc/guidelines/verus_trusted-base.md`; **seam tally unchanged at 14**.

## Surface left trusted (and why it could not be verified)

- **The `svc #0` inline asm (`syscall.rs::imp`).** rev2§6.1(d) register marshalling —
  inline asm is inherently unverifiable, the userspace mirror of the kernel's trusted
  syscall-dispatch marshalling. Folds under the existing thread-lifecycle-shell seam; no
  new seam. Runtime witness arrives in std-port Phase 2 (the QEMU boot, when the PAL
  first calls a wrapper); in 1.1 it is inspection-audited against `ipc::sys::imp` and the
  arguments it will carry are the verified encoder's.
- **The typed wrappers (`syscall.rs`).** Thin trusted shell: each builds a `Call` and
  calls the verified `encode`; the placement is the encoder's, only the `Call`
  construction and the `svc` are trusted.
- **The grant resolver (`grant.rs`).** Plain bookkeeping over an already-decoded
  `Startup`; the untrusted byte boundary is `loader::startup::decode`, verified in 1.2.

## Follow-ups

- Consolidate `ipc`/`urt` (and the `user/shell`/`user/init` `resolve_*` helpers) onto
  `eunomia-sys`'s syscall layer, retiring the duplicated asm/resolvers.
- If the `verus` CI job's budget ever bites, split the vstd-only `encode` proof into its
  own crate so it stops carrying `loader`→`ipc`→`le-bytes`'s ~89 transitive obligations
  on every run (currently within budget).
- Wire `eunomia-sys` into the std PAL as a target-gated dependency of `vendor/rust`'s
  `std` (the `moto_rt` pattern) — std-port Phase 2.1; validate its no_std dep tree in
  the sysroot build there.
- `NAME_STDERR` (std-port 5.1); the io-error map (std-port 2.1, proptested).

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is not
referenced from code, specs, or guidelines.
