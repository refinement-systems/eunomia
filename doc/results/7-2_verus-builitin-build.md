# Findings — `verus_builtin` is not sysroot-buildable: bridge vs. patching Verus

Investigation requested after Phase 2.1 (`doc/results/4_entry-argv-env_findings.md`)
hit the wall it documents: the moto-style **sysroot path-dependency** — std reaching
the seam by `library/std/Cargo.toml` depending on `eunomia-sys` and the PAL doing
`pub use eunomia_sys::…` — is blocked because `vstd`'s `verus_builtin` cannot build as
a `rustc-dep-of-std` sysroot crate. Phase 2.1 pivoted to the documented fallback (an
`extern "Rust"` bridge), which is **live and green** today (the `STD2 PASS` gate,
`user/stdsmoke`, findings 7-1).

This doc answers: **how bad is the block, and is it worth replacing the working bridge
by patching the vendored Verus and wiring it into the build?** Short answer:
**keep the bridge.** The block is real but fully contained; patching Verus is a large,
recurring maintenance burden that buys only cosmetic parity with the moto template and
unlocks no capability the bridge cannot already reach.

---

## The problem, precisely

`-Zbuild-std` compiles `std` **and every crate `std` transitively depends on** as
*sysroot crates*. A sysroot crate is special: the sysroot is being bootstrapped, so
`core`/`alloc` are **not** injected as implicit `extern` preludes the way they are for
an ordinary crate built against an already-existing sysroot. Every crates.io
dependency of std solves this the same way — a `rustc-dep-of-std` Cargo feature that
swaps the crate's `core`/`alloc` edges to the **`rustc-std-workspace-core` /
`rustc-std-workspace-alloc`** shim crates, which cargo's build-std machinery
`[patch]`-redirects to the real sysroot `core`/`alloc`. Confirmed in the vendored tree:
`libc`, `hashbrown`, `cfg-if`, `moto-rt`, … all carry
`features = ['rustc-dep-of-std']` in `vendor/rust/library/std/Cargo.toml`, and the
shims live at `vendor/rust/library/rustc-std-workspace-{core,alloc,std}` (each is just
`pub use core::*;` / `pub use alloc::*;`).

`verus_builtin` has **none** of this plumbing:

- `vendor/verus/source/builtin/Cargo.toml` has **zero `[dependencies]`** (verified:
  `grep -c dependencies` → 0) and no `rustc-dep-of-std` feature.
- `builtin/src/lib.rs` is `#![cfg_attr(not(verus_verify_core), no_std)]` and uses
  `core` directly (`use core::future::Future;`, `use core::marker::PhantomData;`).

So the instant `verus_builtin` enters std's sysroot graph, `core` is unresolvable →
`error[E0463]: can't find crate for core` — exactly what Phase 2.1's canary hit. It is
an external pinned crate (pulled from crates.io as a transitive dep of
`vstd = "=0.0.0-2026-05-31-0205"`), so it cannot be patched **in place**.

### Why it works as a *normal* dep but not a *sysroot* dep

As an ordinary dependency, `verus_builtin` builds fine for `aarch64` — it does so
**today**, transitively, for the no_std user binaries (`user/hello` links
`ipc` + `loader` → `vstd` → `verus_builtin`, and the kernel build is green). A normal
dep is compiled against the already-built sysroot, so `core` is present. **Only
sysroot membership breaks it.** The bridge's entire value is that it keeps
`eunomia-sys` and its whole Verus tree in the *normal-dep* regime where everything
already compiles.

---

## How bad is it — the blast radius

The block is **not** limited to `verus_builtin`. For std to reach the seam the moto
way, `library/std/Cargo.toml` must list `eunomia-sys`, which drags **eunomia-sys's
entire transitive graph** into std's sysroot crate set. Tracing it:

```
eunomia-sys ─┬─ loader ─┬─ ipc ─── vstd ─┬─ verus_builtin            (no_std, uses core::)
             │          ├─ le-bytes ─ vstd ├─ verus_builtin_macros   (proc-macro)
             │          └─ vstd            └─ verus_state_machines_macros (proc-macro)
             ├─ vstd
             └─ urt (target-gated) ─┬─ ipc
                                    ├─ freelist ─ vstd
                                    └─ vstd
```

Crates that would each need `rustc-dep-of-std` plumbing (compiled for the target,
use `core`/`alloc`):

- **6 project crates** — `eunomia-sys`, `loader`, `ipc`, `le-bytes`, `urt`,
  `freelist`. In-tree, so *patchable*, but each has a carefully-curated, heavily-
  commented Cargo.toml and a clean `verus!{}` posture; each would gain a
  `rustc-dep-of-std` feature + a conditional `extern crate rustc_std_workspace_core as
  core`.
- **2 Verus crates** — `verus_builtin` and `vstd`. **External (crates.io), so
  *not* patchable in place.** Note `vstd` additionally does `extern crate alloc`
  (verified in `vstd.rs:24`), so it needs the **`rustc-std-workspace-alloc`** shim too,
  not only `-core`.
- The two **macro crates** (`verus_builtin_macros`, `verus_state_machines_macros`) are
  `proc-macro = true`, host-compiled, and therefore **exempt** — they never enter the
  target sysroot graph.

`verus_builtin` is merely the *first* crate to fail (it is a leaf, compiled early). Fix
it and the next crate in the graph fails the same way. **The whole graph must be made
sysroot-aware**, not one crate.

---

## Option A — keep the `extern "Rust"` bridge (status quo)

std declares a fixed, small set of undefined `extern "Rust"` symbols
(`vendor/rust/.../sys/pal/eunomia/mod.rs` + the `args`/`env`/`io/error` arms); a std
binary links `eunomia-sys` as an **ordinary** dependency, whose `#[no_mangle]` shims in
`eunomia-sys/src/pal.rs` define them; they resolve at final link (the `__rust_alloc`
pattern). Currently **10 symbols**: `__eunomia_{alloc,dealloc,bootstrap_init,argv,env,
thread_exit,stdio_write,mono_ns,wall_ns,io_classify,io_message}`.

**Pros**

- **Already works, end to end.** Green QEMU boot; `user/stdsmoke` prints `STD2 PASS`
  exercising entry/argv/env/alloc/stdio/`Instant`/`SystemTime` (findings 7-1).
- **Keeps the Verus tree a normal dep** — builds exactly as it does for the no_std
  user binaries today; **zero** sysroot plumbing in any of the 8 crates.
- **Verification gate untouched.** `vstd` stays on the crates.io pin
  (`=0.0.0-2026-05-31-0205`); `vendor/verus` stays documentation-only; the trusted-base
  ledger and `doc/guidelines/verus.md` ("the ghost libraries are all on crates.io at
  the pin") are undisturbed.
- **All real logic stays verified/host-tested in `eunomia-sys`** — the PAL holds zero
  new logic, satisfying the plan's thinness rule either way.

**Cons**

- Each std binary needs a one-line `extern crate eunomia_sys;` to force the rlib into
  the link (the global-allocator ergonomic; documented for 5.3 — a tiny `eunomia-rt`
  glue crate could absorb it).
- The bridge re-exports **functions**, not **types**, so it is slightly more verbose
  than the moto `pub use` for any arm that wants to surface a type. (In practice this
  costs nothing concrete — see "What the sysroot path actually buys" — the only
  type-heavy arm, `sys::futex` in Phase 3.3, surfaces *core* types
  (`AtomicU32`/`u32`), not eunomia types, so the bridge handles it cleanly.)
- The `extern "Rust"` ABI is unstable across rustc versions — but **both sides are
  built by the one pinned toolchain in one build**, so it is sound (noted for the 6.3
  forward-port runbook). This caveat does **not** go away under Option B: sysroot
  crates are built by that same one toolchain.

---

## Option B — patch Verus sysroot-aware and wire `vendor/verus` into the build

To take the moto path you must, in order:

1. **Wire the vendored Verus in.** Add `[patch.crates-io]` redirecting `vstd`
   (and transitively `verus_builtin`, the macro crates) to `vendor/verus/source/*`
   path-deps, in whatever workspace builds the std binary. (The vendored submodule is
   at `release/rolling/0.2026.06.07.cd03505`; its `vstd` is `0.0.0-2026-05-31-0205` —
   it **matches** the crates.io pin, so the source is the right version.)
2. **Fork the vendored Verus.** Add a `rustc-dep-of-std` feature + the
   `rustc-std-workspace-core` edge to `verus_builtin`, and `-core` + `-alloc` edges to
   `vstd`, plus the conditional `extern crate … as core/alloc` in their sources.
3. **Make all 6 project crates sysroot-aware** — the same feature + conditional extern
   in `eunomia-sys`, `loader`, `ipc`, `le-bytes`, `urt`, `freelist`.
4. **Win feature unification** across the sysroot graph so `vstd` resolves to its
   no-default (no_std) config while std is being built — build-std feature unification
   is notoriously fiddly, and `vstd`'s `default = ["std"]` actively fights it.

**Pros**

- **Parity with the moto template** — the PAL arm becomes `pub use eunomia_sys::futex;`
  etc., the exact `sys/pal/motor/mod.rs` shape; no `__eunomia_*` symbol list, no
  per-binary `extern crate eunomia_sys;`.
- Marginally tidier *if* the target is ever upstreamed to tier-3 with an in-tree
  `sys/pal/eunomia` (a deferred-work item) — though even that does not force it (below).

**Cons**

- **It forks the vendored Verus.** `vendor/verus` stops being a clean mirror of an
  upstream release tag and becomes a locally-patched fork that the **forward-port
  runbook (6.3) must re-apply on every Verus pin bump** — and Verus bumps are already
  "their own PR" (`doc/guidelines/verus.md` §"The pin"). A new standing maintenance
  axis on the most trust-sensitive dependency in the project.
- **It touches the verification-gate trust wiring.** If the `[patch]` is workspace-
  wide, the gate's `vstd` moves from the crates.io pin to the vendored path — a change
  to the trusted toolchain that `verus.md` and the trusted-base ledger both assume is
  crates.io-pinned. Scoping the `[patch]` to only the std-binary mini-workspace avoids
  this **but** then creates a second `vstd` source that must be kept byte-identical-in-
  effect to the crates.io one — a new invariant with a silent failure mode (a verified
  proof passing against one `vstd` while the shipped binary links another).
- **It pollutes 6 clean verified crates** with build-config noise (a `rustc-dep-of-std`
  feature each), and that new configuration must be exercised in CI or it silently rots
  — it is only ever built by the one std-binary path.
- **High effort, fiddly failure modes** (feature unification, the `-alloc` shim, the
  proc-macro/exec split) for, in the end, **no new capability** — both options are
  trusted-shell delegation over the identical verified `eunomia-sys` surface.
- The unstable-ABI concern is **not** relieved (same one-toolchain build).

---

## What the sysroot path actually buys (the honest benefit)

**Nothing the bridge cannot already do.** Both options are pure trusted-shell
delegation; both keep every non-trivial step verified/host-tested inside `eunomia-sys`;
both are equally auditable against `pal/unsupported`. The *only* differences are
ergonomic: `pub use` of types/functions vs. a fixed list of `extern "Rust"` functions
plus one `extern crate` line per binary.

The one arm that looked like it might need real type re-export — `sys::futex` (Phase
3.3, where moto does `pub use moto_rt::futex;`) — does **not**: std's futex contract is
expressed over **core** types (`AtomicU32`, `u32`), which are FFI-clean across an
`extern "Rust"` boundary. The four futex functions cross the bridge as plain functions;
no eunomia-specific type needs to traverse it. So even the hardest upcoming arm has no
sysroot-only requirement.

---

## Recommendation

**Keep the bridge (Option A). Leave `vendor/verus` documentation-only.**

The block is contained to exactly one stylistic approach, and its fallback is a
complete, working, lower-risk substitute that is *already in production*. Patching
Verus would fork the most trust-sensitive vendored dependency, add a recurring re-patch
burden on every Verus bump, risk the verification-gate wiring, and pollute six clean
crates — all to gain cosmetic parity with the moto template and zero capability.
Correctness-first discipline does not favor trading a green, contained solution for a
larger trusted-surface change that proves nothing new.

Concrete follow-throughs that make the bridge's one ergonomic cost vanish:

- **Absorb `extern crate eunomia_sys;`** into a tiny non-verified `eunomia-rt` glue
  crate (already floated in findings 4 / Phase 5.3) so std binaries need no boilerplate.
- **Record the decision in the 6.3 forward-port runbook**: the `extern "Rust"` ABI is
  sound under the one-toolchain build; the bridge symbol list is the forward-port diff
  surface to re-check on a nightly bump.

### The one future scenario, and why it still doesn't force patching

If `aarch64-unknown-eunomia` is ever upstreamed to **tier-3** (a deferred-work item),
an in-tree `sys/pal/eunomia` would ideally `pub use` a runtime crate the moto way. Even
then, that runtime crate can be a **thin, non-verified `eunomia-rt`** that depends on
no `vstd` and reaches `eunomia-sys` over the *same* bridge — sidestepping vstd-in-
sysroot entirely. (A runtime crate that itself pulled `eunomia-sys` → `vstd` would just
re-create this whole problem one level up.) So upstreaming is a reason to keep the
bridge *behind* a clean runtime-crate facade, not a reason to make Verus
sysroot-buildable.

---

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is
not referenced from code, specs, or guidelines.
