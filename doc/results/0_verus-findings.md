# 0 — Verus concurrency prerequisite: the state-machine macros are buildable in-tree

Date: 2026-06-25. Attempt against the prerequisite in
`doc/plans/0_verus-concurrency.md` §1.2 ("set up the state-machine macro suite
before writing any proof"). Outcome: **prerequisite satisfied with a one-line
dependency**; the vendored prover is not required to unblock the plan, and its
role is now documented. No verified-crate proof code was touched.

## What was attempted

Validate whether a project crate can author and **verify** a `state_machine!{}`
and a `tokenized_state_machine!{}` using only a direct dependency on
`verus_state_machines_macros`, with no vendored/path wiring — then, if so, adopt
the minimal integration (keep the downloaded prebuilt prover; pull ghost crates
from crates.io; make `vendor/verus` the in-tree reading copy; retire
`reference/vstd`).

## Premise correction

`doc/plans/0_verus-concurrency.md` §1.2 calls `verus_state_machines_macros` the
"highest-risk blocker" on the premise that it **is not on crates.io** and "ships
only inside the Verus install." That premise is **outdated**. The current
`Cargo.lock` already resolves it — and the whole ghost-crate set — from crates.io
with checksums, as transitive deps of `vstd`:

| crate | version | source |
|---|---|---|
| `vstd` | `0.0.0-2026-05-31-0205` | crates.io |
| `verus_builtin` | `0.0.0-2026-05-17-0151` | crates.io |
| `verus_builtin_macros` | `0.0.0-2026-05-31-0205` | crates.io |
| `verus_state_machines_macros` | `0.0.0-2026-05-31-0205` | crates.io |
| `verus_syn` | `0.0.0-2026-05-31-0205` | crates.io |

`vstd`'s prelude does **not** re-export the macros (confirmed against
`vendor/verus/source/vstd`), so an authoring crate must name
`verus_state_machines_macros` directly — but since it is published, that is a
one-line `Cargo.toml` add, not a vendoring problem.

## Smoke test (gating experiment)

Harness: the `scratchpad` crate (already `[package.metadata.verus] verify =
true`), reverted afterward. Added one direct dependency
(`verus_state_machines_macros = "=0.0.0-2026-05-31-0205"`; `verus_builtin` was
**not** needed) and authored a non-tokenized `state_machine!{Adder}` (with
`#[invariant]` + `#[inductive(..)]`) and a `tokenized_state_machine!{Counter}`
(one `#[sharding(variable)]` field, init/transition/invariant), modeled on
`vendor/verus/examples/state_machines/{adder,counting}.rs`.

Results — both pass criteria held:
- `cargo build -p scratchpad` (erased, no `verus_keep_ghost`): **builds clean**
  (warnings only: `unexpected_cfgs` for `cfg(verus_keep_ghost)` — the macros emit
  it, so an authoring crate must add it to its `[lints.rust] unexpected_cfgs`
  check-cfg — and `non_snake_case` on the machine module names).
- `cargo clean -p scratchpad && cargo verus verify -p scratchpad`: a **real** run
  (vstd recompiled at `1690 verified, 0 errors`) ending
  `verification results:: 5 verified, 0 errors`.

So the **downloaded prebuilt prover** discharges obligations from macro code whose
macro crate came from **crates.io**, with no version/identity mismatch — a direct
crates.io dependency on `verus_state_machines_macros` is sufficient to author and
verify both macros in-tree.

## Version & fork facts

`vendor/verus` is the Verus project vendored as a git submodule from the fork
`refinement-systems/verus`, checked out at `cd0350583`. That commit **is** the
upstream release tag `release/0.2026.06.07.cd03505` (`git describe` shows no
divergence suffix; the submodule's `upstream` remote is `verus-lang/verus`), so
the fork currently carries **zero custom patches** — it equals the upstream
release the CI verify gate already downloads. `source/vstd` is
`0.0.0-2026-05-31-0205`; `rust-toolchain.toml` pins `1.95.0`. All three match the
project pin byte-for-byte. Building the prover from source would therefore produce
an identical binary at much higher cost — no functional gain while the fork
tracks upstream.

The verifier itself (`rust_verify`/`cargo-verus`/`z3`) is **not** a cargo crate
and cannot be a workspace member: it builds only through Verus's own `vargo`
system (`tools/activate` → `tools/get-z3.sh` → `vargo build --release`).

## Adopted integration

Minimal "reading copy + fork base" (the plan's option 1). Changes in this pass:
- **`doc/guidelines/verus.md`** — new subsection "The vendored prover and
  authoring state machines": the one-line macro-dep recipe, the
  `cfg(verus_keep_ghost)` check-cfg note, and the honest role of `vendor/verus`
  (in-tree reading copy; not workspace-built; the staging ground for the day the
  fork must diverge to patch a prover gap, at which point `vargo` from-source +
  path deps become mandatory).
- **`CLAUDE.md`** — workspace-layout row repointed from `reference/` + `reference/vstd`
  to `vendor/verus/`.
- **`doc/plans/0_verus-concurrency.md`** — the `reference/vstd/atomic.rs` path
  citation repointed to `vendor/verus/source/vstd/atomic.rs`.
- **`reference/vstd/`** — deleted (read-only copy referenced by no crate;
  redundant with `vendor/verus/source/vstd` at the same version).
- The CI `verus` job, the pin, and every verified crate's `Cargo.toml` are
  **unchanged**.

## Regression

The change set touches only documentation and deletes a directory that was not a
workspace member and referenced by no crate, so no verified crate's inputs
changed. An authoritative per-crate `cargo clean -p <crate> && cargo verus verify
-p <crate>` was run across the gate to confirm the baselines hold (one `-p` per
crate, `cas` with `--no-default-features`):

| crate | baseline | observed |
|---|---|---|
| kcore | 404 verified, 0 errors | 404 verified, 0 errors |
| ipc | 47 verified, 0 errors | 47 verified, 0 errors |
| urt | 25 verified, 0 errors | 25 verified, 0 errors |
| freelist | 29 verified, 0 errors | 29 verified, 0 errors |
| dma-pool | 0 verified, 0 errors | 0 verified, 0 errors |
| cas (`--no-default-features`) | 75 verified, 0 errors | 75 verified, 0 errors |

Every `verification results::` line was present (real runs, not stale cache;
prover `0.2026.06.07.cd03505` / toolchain `1.95.0`). No regression.

## What this unblocks, and what it does not

The macro suite is available now: a concurrency task can add the one-line dep and
write a `state_machine!{}` / `tokenized_state_machine!{}` that verifies in CI with
no tooling change. This does **not** address the genuine prover gaps the plan
flags as the real blockers for the hard tier — atomics weaker than SeqCst (every
`vstd` atomic is hardcoded SeqCst) and minting a `PointsToRaw` for a pre-existing
`static [u8; N]`. Those can only be fixed by patching Verus, which is the
strategic purpose the `vendor/verus` fork now stands ready to serve.
