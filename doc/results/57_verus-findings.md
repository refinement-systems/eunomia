# Verus findings 37 — Phase 7a: the host-chokepoint pilot — `ipc::header`

Plan: `doc/plans/3_verus-rewrite.md` (§4.7, §7 step 6) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7a). Prior increment: `56`
(phase-6 closeout — kcore done). This increment opens **phase 7**, the migration
of the four host-side §4.7 chokepoint crates (`ipc`, `urt`, `dma-pool`, `cas`)
from Kani (bounded) to Verus (unbounded). **7a is the pilot**: the §3.7 fixed
message header in `ipc::header`, the cleanest end-to-end proof — *and* the
toolchain de-risk the whole phase rests on.

`cargo verus verify -p ipc`: **18 verified, 0 errors** (the header obligations;
`-p kcore` is unaffected — a separate crate). `cargo test -p ipc`: **17 passed**;
`cargo test -p ipc --features wire`: **21 passed** (the `verus!{}` block erases
under a normal build — the unit tests and the `header_only` fuzz seed see the same
bytes). `cargo kani -p ipc`: **5 harnesses, 0 failures** (was 7 — the two
`check_header_*` harnesses are deleted; the §4.6 session codecs stay on Kani until
7b). No rlimit bump or `spinoff_prover` needed.

---

## 1. The real deliverable: the toolchain proof

`ipc` is a path-dependency of all five userspace binaries
(`user/{hello,selftest,init,storaged,shell}`), each its own mini-workspace built
by `kernel/build.rs` for `aarch64-unknown-none-softfloat` with
`-Zbuild-std=core,compiler_builtins,alloc`. Adding `vstd` to `ipc` therefore drags
it into a build context the kcore recipe never exercised: `kcore`'s `vstd` is
linked by the *kernel* crate, not by the separate userspace workspaces. **This was
flagged as phase 7's chief new risk** (detail plan "the enabling concern the
master plan did not surface"); the pilot exists to clear it before any hard proof
rests on it.

It clears cleanly:

- `cd kernel && cargo build` — green; `build.rs` rebuilds all five user binaries
  (it `rerun-if-changed`s `ipc/src`) with the new transitive `vstd` dep.
- A fresh, isolated cross-build of `user/hello` (the exact `build.rs` invocation,
  separate `--target-dir`) compiles **`vstd v0.0.0-2026-05-31-0205` for
  `aarch64-unknown-none-softfloat`** (`libvstd-*.rlib` in the deps dir) and links
  the alloc-free `hello` binary. `verus!{}` erases to plain Rust and `vstd`
  compiles to nothing load-bearing — confirmed in the userspace target, not just
  under host `cargo test`.

The recipe applied to `ipc/Cargo.toml` is kcore's, verbatim:
`vstd = { version = "=0.0.0-2026-05-31-0205", default-features = false }`,
`[package.metadata.verus] verify = true`, and `cfg(verus_keep_ghost)` /
`cfg(verus_only)` added to the existing `unexpected_cfgs` check-cfg list. The
crate-root `#[allow(unused_imports)] use vstd::prelude::*;` mirrors
`kcore/src/lib.rs`.

A non-obvious enabler: `ipc::sys`'s three `core::arch::asm!("svc #0")` blocks —
which Verus's frontend would not handle — are gated behind
`#[cfg(all(target_arch = "aarch64", target_os = "none"))]`, so the host (x86-linux
CI / local) Verus build compiles the `unreachable!()` host fallback instead and
never sees the asm. No `#[verifier::external]` annotation was needed anywhere in
`ipc`: code outside `verus!{}` is external by default under the cargo-verus driver
(the same partial-adoption `kcore`'s non-`verus!{}` `id.rs`/`store.rs` rely on).

## 2. The header proof

Obligations (replacing `check_header_decode_total` / `check_header_roundtrip`),
all ∀:

- **`spec_decode` / `spec_encode`** — `pub open spec fn`s modelling the
  little-endian layout. `decode` ⟶ `ensures r == spec_decode(buf@)` plus the
  explicit `r is Ok <==> buf@.len() == HEADER_SIZE` (totality + accept-iff-length,
  i.e. short-input *and* trailing-byte rejection). `encode` ⟶
  `ensures b@ == spec_encode(*self)`.
- **`lemma_decode_encode(h)`**: `spec_decode(spec_encode(h)) == Ok(h)` — the
  value→bytes→value direction.
- **`lemma_encode_decode(s)`** (`requires s.len() == HEADER_SIZE`):
  `spec_encode(spec_decode(s)->Ok_0) == s` — the bytes→value→bytes direction.
  Together: a total bijection between `Header` values and `HEADER_SIZE`-byte
  strings.

### 2.1 Why the codec body was rewritten (the partial-adoption friction)

The pre-existing body used `u16::from_le_bytes([buf[2], buf[3]])` and
`b[2..4].copy_from_slice(&self.opcode.to_le_bytes())`. **Verus specs none of
`core::{u16,u32}::from_le_bytes`/`to_le_bytes`, the array-`TryInto`, nor
`copy_from_slice`** — they would be unverifiable calls inside `verus!{}`. `vstd`
*does* ship `bytes.rs` (`spec_u16_from_le_bytes` + proven round-trip lemmas), but
its exec wrappers split awkwardly: `uN_from_le_bytes(&[u8])` is no-alloc, yet
**`uN_to_le_bytes` is `#[cfg(feature="alloc")]` and returns `alloc::vec::Vec<u8>`**
— unusable for `ipc`'s no_std/no-alloc default build *and* for an `encode` that
returns a fixed `[u8; HEADER_SIZE]`. Calling the `vstd` exec helpers would also
make `vstd` **load-bearing at runtime** in the five user binaries, contradicting
the "erases to nothing" thesis the pilot is meant to confirm.

Resolution: rewrite `encode`/`decode` with **explicit mask/shift arithmetic**
(`(x & 0xff) as u8`, `(lo as u16) | ((hi as u16) << 8)`), which Verus reasons over
natively and which is byte-for-byte identical to the `to_le_bytes` form (matching
`vstd::bytes::spec_u16_to_le_bytes`'s own shape). `vstd` stays **ghost-only**. This
is also the no-`from_le_bytes` idiom kcore already uses (`sysabi`, `aspace`).

### 2.2 `by (bit_vector)` gotchas (worth recording for 7c/7d/7e)

The round-trip identities are nonlinear over bit-ops, so each is discharged
`by (bit_vector)`. Two solver constraints surfaced — both general, both relevant
to the harder upcoming chokepoints:

1. **bit_vector rejects struct-field projections.** `assert((h.opcode & 0xff) …)
   by (bit_vector)` errors with *"unsupported for bit-vector: expression
   conversion … Field … opcode"*. Fix: bind the field to a plain fixed-width local
   first (`let op = h.opcode;`) and reason over `op`.
2. **bit_vector does not see surrounding `let` definitions.** A spec
   `let bl = (s6 as u32) | …;` followed by `assert((bl & 0xff) as u8 == s6) by
   (bit_vector)` fails — inside the bit_vector query `bl` and `s6` are independent
   symbols. Fix: inline the full reassembly expression into the asserted goal so
   the solver sees the dependency. (The first two field pairs already inlined and
   passed; only the `let`-bound body_len byte failed, isolating the cause.)

`encode`'s `[u8; HEADER_SIZE]` array literal ties to `spec_encode`'s `seq!` via
`broadcast use vstd::array::group_array_axioms;` + `assert(b@ =~= spec_encode(…))`;
`decode`'s slice indexing uses `vstd::slice::group_slice_axioms` (the kcore
`aspace` pattern).

## 3. What changed

- `ipc/Cargo.toml` — `vstd` dep + `[package.metadata.verus]` + `unexpected_cfgs`.
- `ipc/src/lib.rs` — crate-root `vstd::prelude` import.
- `ipc/src/header.rs` — the `verus!{}` block (specs, rewritten exec codec, the two
  round-trip lemmas); the `#[cfg(test)]` regression tests kept.
- `ipc/src/proofs.rs` — `check_header_*` and the `crate::header` import deleted;
  the module doc updated to point at `crate::header`. Session harnesses untouched.
- `.github/workflows/ci.yml` — the `verus` job verifies `-p kcore -p ipc` (no
  per-proof filter; a new obligation auto-gates). The `kani` job's `-p ipc` stays
  (the session codecs) until 7b.
- `CLAUDE.md` — the `cargo verus verify` block, the `verus`/`kani` CI bullets, and
  the Verus-tier prose note phase 7a.

## 4. Next

**7b — `ipc::session`**: `ConnectReq`/`GrantReply` round-trips and `Admission`
never-over-grants ∀ (vs Kani's 3-step bound), then delete the rest of
`ipc/proofs.rs` and drop `-p ipc` from the `kani` job. The toolchain is now proven,
so 7b–7g can rest on it. The two unbounded trophies the master plan named —
`urt::time` monotonicity (7d) and `dma-pool` two-buffer disjointness (7e) — are the
properties Kani's own harnesses record as intractable; §2.2's bit_vector notes
apply directly there.
