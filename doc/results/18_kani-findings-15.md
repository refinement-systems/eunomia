# Kani verification findings — part 15 (`-Z function-contracts` spike)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `17_kani-findings-14.md`.
This part implements the **last** open item of the second conformance review
(`14_kani-review-2.md`), **recommendation #6**: a time-boxed `-Z function-contracts`
/ `-Z loop-contracts` spike on the `cspace::revoke`/`obj_unref` recursion, kept
**off the pinned CI path**. It is a **research spike** — the deliverable is this
honest report on what the technique can and cannot do on the real code at
cargo-kani 0.67.0, not a productionized proof. It introduces **DN-14**.

## Why a different instrument

The review's residuals 1–3 — multi-level recursive container teardown, `revoke`
over arbitrary trees, and the full-alphabet multi-op composition — are not
closable by more bounded harnesses of the current genre: CBMC OOMs (DN-12).
They are inherently **unbounded/recursive**:

- `cspace::revoke` is a nested `while` walk over a pointer-linked CDT of
  runtime-unknown size;
- `cspace::delete` → `obj_unref` → `destroy_cspace`/`destroy_channel`/
  `destroy_tcb` → `delete` is mutual recursion of runtime-unknown depth.

Kani's **function contracts** (`requires`/`ensures`/`modifies` +
`proof_for_contract` + contract-replacement via `stub_verified`) and **loop
contracts** (`loop_invariant`) are the standard instruments for exactly these:
replace a recursive call / unbounded loop by a *contract* instead of unrolling
it. cargo-kani 0.67.0 lists both under `-Z help`. They are **unstable**, which is
why the plan deferred them — hence a spike, behind the `kani_contracts` cargo
feature, run only by `scripts/deep-verify.sh contracts` (which passes the
unstable `-Z` flags). The contract attributes are `cfg_attr(all(kani, feature =
"kani_contracts"))`, so they are inert in `cargo build`/`cargo test`, the per-PR
`cargo kani -p kcore`, and the `kani_deep` job alike.

## What was tried, and what happened (cargo-kani 0.67.0)

### 1. Baseline — function contracts DO work on kcore ✅

A `requires`/`modifies`/`ensures` contract on `cspace::unref_cspace` (a refcount
drop whose modified object is a **direct pointer parameter** `cs: *mut CSpaceObj`,
with `requires (*cs).hdr.refs >= 2` keeping it on the non-destroy path):

```rust
#[cfg_attr(all(kani, feature = "kani_contracts"),
    kani::requires(unsafe { (*cs).hdr.refs >= 2 }),
    kani::modifies(cs),
    kani::ensures(|_| unsafe { (*cs).hdr.refs } == old(unsafe { (*cs).hdr.refs }) - 1))]
```

`#[kani::proof_for_contract(cspace::unref_cspace)]` (harness
`contract_unref_cspace_refcount`) → **VERIFICATION SUCCESSFUL**, `0 of 1137
failed`, ~0.30 s. So `-Z function-contracts` is usable on kcore: the no-amplification /
refcount-soundness discipline *can* be stated and proven as a contract. This is
the positive control. (Two syntax notes for the next reader: the pre-state
accessor inside `ensures` is bare `old(…)`, **not** `kani::old`; and `modifies`
wants a pointer that is a *place expressible from the signature*.)

### 2. The recursion seam `delete` — blocked by an inexpressible `modifies` ❌

The headline target. A contract on `cspace::delete` over the structurally
simplest input — an **isolated leaf** notification cap (no parent/children/
siblings), destructors stubbed (DN-4) so nothing recurses:

```rust
#[cfg_attr(all(kani, feature = "kani_contracts"),
    kani::requires(unsafe { !(*slot).cap.is_empty() }),
    kani::modifies(slot),
    kani::ensures(|_| unsafe { (*slot).cap.is_empty() && (*slot).parent.is_null() }))]
```

`#[kani::proof_for_contract(cspace::delete)]` (harness `contract_delete_leaf`) →
**VERIFICATION FAILED**, ~12.8 s, with exactly one failing check:

```
Failed Checks: Check that h->refs is assignable
```

> **Tripwire (cargo-kani upgrades).** `contract_delete_leaf` is a *committed
> expected-to-FAIL* harness, and nothing in CI gates its expected outcome (it runs
> only under the manual `scripts/deep-verify.sh contracts`). The DN-14 conclusion
> holds **only at the pinned cargo-kani 0.67.0**. When the pin moves, re-run
> `deep-verify.sh contracts` and check this harness: if it now **VERIFIES**, or
> **fails with any string other than** `Check that h->refs is assignable`, the
> `modifies`-expressibility wall has changed — re-evaluate DN-14 (function
> contracts may have become a viable route to the unbounded teardown/revoke
> proofs, residuals 1–2). This is on the cargo-kani upgrade checklist (plan §5).

`delete` decrements the **designated object's** header refcount (`obj_unref`'s
`(*h).refs -= 1`), and `h` is reached through the cap's *embedded* pointer
(`(*slot).cap.header()`), not through `delete`'s `(slot, env)` signature. So the
write is outside `modifies(slot)` — and there is **no place expression nameable
from `delete`'s signature** that denotes it: the object's kind (and thus which
pointer field of the cap holds it) is only known at runtime. Even this
zero-recursion leaf case cannot be given a sound `modifies` clause. For a
*non-isolated* cap the same wall reappears for the CDT-neighbour writes
(`parent.first_child`, `prev_sib.next_sib`, `next_sib.prev_sib`, each child's
`parent`) at runtime-determined addresses.

Because contract-*replacement* of the recursion (`stub_verified(delete)` inside
`destroy_cspace`/`revoke`) requires `delete` to have a *verified* contract first,
this wall blocks the entire modular route to residuals 1 and 2 at 0.67.0.

### 3. The `revoke` loop contract — blocked before semantics, at packaging ❌

`#[kani::loop_invariant(…)]` on `revoke`'s outer `while` could in principle let
CBMC verify the walk without unrolling it. But the invariant attribute sits on a
loop **expression**, and gating it with `cfg_attr` (as every other spike
attribute is) fails to compile:

```
error[E0658]: attributes on expressions are experimental
error[E0658]: custom attributes cannot be applied to expressions
```

Applying it would require crate-wide `#![feature(stmt_expr_attributes, …)]` in
the production source — unacceptable for an unstable, off-path spike — and even
then the loop body's `delete` call carries the §2 `modifies` wall. So the
loop-contract path is doubly blocked here; it is documented rather than committed
as broken code (no `loop_invariant` ships in `cspace.rs`).

## Isolation (the contract attributes are inert everywhere else)

Non-negotiable and verified on the spike tree:

- `cargo build -p kcore` — clean; `cargo test -p kcore` — 11 passed, 2 ignored.
- `cargo kani -p kcore --harness check_delete_cspace -Z stubbing` (no feature) →
  **SUCCESSFUL**, ~1.6 s: the per-PR teardown harness over the *exact* annotated
  functions (`delete`/`obj_unref`) is unchanged.
- `cargo kani -p kcore --harness contract_unref_cspace_refcount` (no feature) →
  `no harnesses matched`: the whole `proofs::contracts` module is `cfg`-gated out.

## Verdict (DN-14)

**Function contracts are usable on kcore's value-level refcount discipline, but
not — at cargo-kani 0.67.0 — on the cap-algebra teardown, because that machinery's
write set is reached through pointers embedded in the data (the cap's designated
object) and the CDT link structure, which `modifies` cannot name from the
functions' signatures.** Closing residuals 1–3 by contracts would need either
(a) a Kani `modifies` that can denote "the object designated by this cap, whatever
its kind" (a dependent/embedded-pointer write set), or (b) restructuring the
production teardown so the modified objects are explicit parameters — which would
distort the architecture for the prover's benefit. Neither is warranted now.

So the **practical instrument for residuals 1–3 remains the committed exhaustive
plain-Rust replay** (the DN-12 "mini-TLC", `kcore::proofs::exhaustive`, run by
`deep-verify.sh replay`): it already exercises the full alphabet incl.
delete/revoke over all reachable shapes, in seconds, with none of CBMC's
recursion/`modifies` limitations. The contracts spike is preserved, gated and
documented, as the scaffold a future Kani (with embedded-pointer `modifies` or
mature loop contracts) would build on.

## What shipped

- `kcore/Cargo.toml` — `kani_contracts` feature.
- `kcore/src/cspace.rs` — `cfg_attr`-gated contracts on `unref_cspace` (baseline)
  and `delete` (the wall); `revoke` left clean (loop-contract not committed, §3).
- `kcore/src/proofs/contracts.rs` (new, `cfg(all(kani, feature="kani_contracts"))`)
  + its `mod` line — the two `proof_for_contract` harnesses.
- `scripts/deep-verify.sh` — a `contracts` mode (exploratory, non-gating).
- `CLAUDE.md` — the `contracts` mode documented. **Not** added to `kani-deep.yml`
  (manual-only; unstable surface).

## Status of recommendation #6

✅ Spiked and documented. **All six recommendations of `14_kani-review-2.md`
(#1–#6) are now addressed** — the routine five landed as proofs/CI/docs changes,
and the research sixth is honestly reported with a reproducible positive control,
a precisely-located wall, and the standing instrument (the exhaustive replay)
named for the residuals it cannot yet close.
