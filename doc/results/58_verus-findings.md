# Verus findings 38 — Phase 7b: `ipc::session` — codecs + the `Admission` quota

Plan: `doc/plans/3_verus-rewrite.md` (§4.7, §7 step 6) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7b). Prior increment: `57`
(phase 7a — the `ipc::header` pilot). This increment is the second host-chokepoint
migration: the §4.6 session layer in `ipc/src/session.rs` — the fixed
`ConnectReq`/`GrantReply` wire codecs and the safety-bearing `Admission` window
quota — from Kani (bounded) to Verus (unbounded). It rests on the 7a toolchain
proof (vstd erases into the five userspace binaries); no toolchain risk remained.

`cargo verus verify -p ipc`: **58 verified, 0 errors** (18 header + 40 session;
no rlimit bump or `spinoff_prover`). `cargo test -p ipc`: **17 passed**;
`cargo test -p ipc --features wire`: **21 passed** (the `verus!{}` block erases —
the session unit tests and the wire fuzz seed see the same bytes).
`cargo kani -p urt -p dma-pool`: **6 verified, 0 failures** (was `-p urt -p ipc
-p dma-pool`); `cargo kani -p cas -Z stubbing`: unchanged. The five session Kani
harnesses are deleted and `ipc/src/proofs.rs` is **gone** — `ipc` is now fully off
Kani (header in 7a, session here), so `-p ipc` drops from the `kani` CI job.

---

## 1. The codecs (`ConnectReq` / `GrantReply`)

Mechanically the 7a `header` recipe, applied twice:

- **Ghost models** `req_encode`/`req_decode`, `grant_encode`/`grant_decode`
  (`pub open spec fn` over `Seq<u8>`) describe the little-endian byte layout. The
  exec `encode`/`decode` are tied to them by `ensures` (`b@ == req_encode(*self)`;
  `r == req_decode(buf@)`), with the explicit accept-iff: `ConnectReq::decode` is
  `Some <==> (len == REQ_LEN && buf@[0] == TAG_REQ)`; `GrantReply::decode` is `Some
  <==>` the 9-byte-grant *or* 1-byte-refusal shape. Total over every byte string.
- **Bijection lemmas** `lemma_{req,grant}_decode_encode` (value→bytes→value) and
  `lemma_{req,grant}_encode_decode` (bytes→value→bytes, `requires` the accepted
  shape) — the same `by (bit_vector)` split/reassemble identities as the header,
  one per `u32` field (window, size, requested_window). Together each codec is a
  total bijection between its values and its accepted byte strings (a strict
  strengthening of the Kani harnesses, which proved totality + accept-iff +
  *decode∘encode* only).

Two structural rewrites the codec needed to stay inside `verus!{}` (the 7a
friction, again):

1. **Mask/shift, not `to_le_bytes`/`copy_from_slice`.** Verus specs none of those,
   so `encode` builds a fixed array literal with `(x >> k) & 0xff as u8` and
   `decode` reassembles with `(b as u32) << k`. Byte-identical output; `vstd` stays
   ghost-only and erases into the alloc-free user binaries.
2. **`GrantReply::decode` lost its `?`/match-guard.** The old body was
   `match buf.first().copied()? { TAG_GRANT if buf.len()==GRANT_LEN => …, … }`.
   Rewritten to explicit `if buf.len()==GRANT_LEN && buf[0]==TAG_GRANT {…} else if
   …refused… else { None }` — Verus-friendly (no try-operator, no match guards) and
   behaviourally identical (empty/short/trailing all fall to `None`).

**`pub open spec` cannot name a private `const`** (`error: in pub open spec
function, cannot refer to private const item`). The codec models reference the
tag/length constants, so `TAG_REQ`/`TAG_GRANT`/`TAG_REFUSED`/`REQ_LEN`/`GRANT_LEN`/
`REFUSED_LEN` became `pub` — the same call header made for `HEADER_SIZE`. (The
alternative — `closed` models — would hide the bijection body needlessly; `open`
is the project's preferred transparent form.)

## 2. The trophy: `Admission` never over-grants ∀ sequences

The Kani harness `check_admission_never_over_grants` ran a **hard-coded 3-step**
symbolic admit/release loop (`#[kani::unwind(4)]`) and leaned on Kani's overflow
check to catch a `budget - granted` underflow. Verus replaces the bound with a
**modular invariant**: the never-over-grant property is a pre/post-condition of
*each* operation, so it composes over *any* sequence by induction — unbounded.

- `pub closed spec fn well_formed(self) == (granted <= budget)` is the quota
  invariant. `new` establishes it; `admit`/`release`/`admit_connect` all
  `requires`/`ensures` it. Because every reachable `Admission` satisfies it,
  `remaining()`'s `budget - granted` is proven non-underflowing — for all states,
  not three steps.
- The functional contract is stated over a second `pub closed spec fn
  spec_remaining(self) -> int == budget - granted` (the *observable* quota): `admit`
  grants iff `requested <= old.spec_remaining()`, decrements it by exactly
  `requested` on `Ok`, leaves it untouched on `Err`; `release` only ever raises it.

**Why `closed` accessors, not raw fields:** `Admission`'s `budget`/`granted` are
private, and this Verus pin forbids a *public* function's contract from naming a
field of a datatype that is opaque outside the module (`error: disallowed: field
expression for an opaque datatype … must be well-formed everywhere`). Routing every
contract reference through `well_formed`/`spec_remaining` (whose `closed` bodies may
read the private fields, since they live in the module) fixes this — and yields a
*better* contract: it speaks of the observable remaining quota and the encapsulated
invariant, never the `budget`/`granted` split. The public surface is unchanged.

## 3. Toolchain note worth recording (mut-ref postconditions)

The pinned Verus (`0.2026.06.07.cd03505`) uses the **new mutable-reference
support**: in an `ensures` clause the post-state of a `&mut` parameter must be
written `final(self)` (or `final(adm)`), not bare `self` — `error: to dereference
a mutable reference parameter in a postcondition, disambiguate by wrapping it in
either old or final`. `old(self)` remains the entry value; `requires` uses bare
`self` (unambiguous). (kcore's `timer.rs` already uses `final(store)` over the
`Store` seam — this is the first time the host-chokepoint ports hit it, since 7a's
header had no `&mut`.) `vstd` specs `u32::saturating_sub` (`#[verifier::
allow_in_spec]`, `if y > x { 0 } else { x - y }`), so `release` kept the
`saturating_sub` form unchanged.

## 4. What changed

- `ipc/src/session.rs` — one `verus!{}` block: the type decls + `pub` tag/length
  consts, the four `pub open spec` codec models, the rewritten exec codecs, the
  `Admission` impl (two `closed` accessors + `new`/`remaining`/`admit`/`release`
  contracts) and `admit_connect`, and the four bijection lemmas; the `#[cfg(test)]`
  regression tests kept verbatim.
- `ipc/src/proofs.rs` — **deleted** (the 5 session harnesses; 7a removed the 2
  header ones).
- `ipc/src/lib.rs` — the `#[cfg(kani)] mod proofs;` declaration removed.
- `.github/workflows/ci.yml` — `kani` job: `-p ipc` dropped (`cargo kani -p urt
  -p dma-pool`); `verus` job comment notes 7b (the verify line already covers `ipc`
  with no per-proof filter, so the session obligations auto-gated).
- `CLAUDE.md` — the `cargo kani`/`cargo verus` examples, the `kani`/`verus` CI
  bullets, the IPC-crate narrative (proofs.rs → Verus), the Verus-tier table row,
  and the `### Verus` phase-7 prose note 7b.

## 5. Next

**7c — `urt::slots`**: the bitmap free-list ∀ `cap`/`WORDS` (Kani: CAP=4) — needs
`bit_vector`-mode reasoning relating `free[i/64] & (1<<(i%64))` to a ghost free set,
plus restructuring the `.find().map()` combinators into invariant-carrying loops.
Then the two unbounded trophies the master plan named — `urt::time` monotonicity
(7d) and `dma-pool` two-buffer disjointness (7e) — the properties Kani's own
harnesses record as intractable. §1's bit_vector / mut-ref notes carry forward.
