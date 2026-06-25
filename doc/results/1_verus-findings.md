# 1 — Reactor dispatch arithmetic under Verus (Task 1 pilot)

Date: 2026-06-25. Attempt against `doc/plans/0_verus-concurrency.md` Task 1
(`reactor-alloc-bit-verus`, the recommended pilot). Outcome: **all three
obligations verified**; `cargo verus verify -p ipc` rises `47 → 68 verified, 0
errors`. No reverts; the verified cores ship.

## What was attempted

Convert the three proptest-only characterizations of the IPC reactor's
single-threaded dispatch (`ipc/src/reactor.rs`) into deductive, all-inputs `ensures`:

- **`alloc_bit`** returns a clear bit, sets exactly it (`used' == used | (1<<bit)`),
  `None` iff `used == u64::MAX`.
- **`register_bound`** sets exactly `mask` in `used` and the matching slots, leaves
  state unchanged on `Taken`, and maintains the slot/used **coherence** bijection
  (`slots[b].is_some() <==> used bit b set`).
- a pure **`drain_one(pending) -> (u32, u64)`** returns the lowest set bit and
  clears exactly it.

`wait`'s blocking loop + `notif_wait` stay outside `verus!{}` (Loom/Shuttle/TLA-
routed). The three proptests are kept as the companion oracle tier.

## Carrier decision (the one design choice)

The plan flagged one real unknown: the verified functions reason about `self.used`
/`self.slots`, which would drag the `Reactor<'t, T: Transport>` generic, its
`&'t T` field, and the **external** `Transport` trait bound into `verus!{}`.

Resolved **without** moving `Reactor` into `verus!{}` at all. The reactor's own
design is already "verified pure core + plain shell" (the in-tree comment: *"`alloc_bit`
(plain Rust, the trusted shell over the slot array) calls this"*). Extending that
idiom is the lowest-risk, most faithful carrier:

- The verified cores are **free functions over explicit state** —
  `alloc_lowest(used: &mut u64)`, `register_bound_into(slots, used, mask, key)`,
  `drain_one(pending)` — needing only `Reg` / `Signals` / `RegisterErr` moved into
  `verus!{}` (small datatypes; in-repo precedent: `Header`, `GrantReply`,
  `HeaderError`, `ObjType` all derive + live inside `verus!{}`). **No generic, no
  trait bound, no reference** enters verified code.
- `Reactor` stays plain Rust; its `alloc_bit` / `register_bound` methods delegate
  in one line. **All three proptests are unchanged** (they read `reactor.used` /
  `reactor.slots[bit]` directly, which still works — `Reactor`'s fields did not move).

`register_bound_into` is **by-value** (`slots: [Option<Reg>; 64]` in, returned out)
rather than `&mut`: this Verus pin's new mut-ref support requires `*old(_)` /
`*final(_)` disambiguation in postconditions (and is least-documented inside *loop
invariants*), so owning the array sidesteps all of it — the loop mutates an owned
local, and the postcondition names the entry params and `r.0`/`r.1`/`r.2` directly.
The cost is a setup-time array copy at the `register_bound` call boundary (a
registration is rare); the verified algorithm is otherwise the original
`bits & (bits - 1)` popcount scan, unchanged.

## Result

`cargo clean -p ipc && cargo verus verify -p ipc` → **`68 verified, 0 errors`**
(real run, results line present; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`).
Delta `+21` items over the `47` baseline = the new `exec`/`proof`/`spec` fns plus
their `assert ... by (bit_vector)` sub-obligations (the gate counts items, not
lines): `drain_one`, `alloc_lowest`, `coherent`, `lemma_pop_lowest`,
`register_bound_into`, and the bit-vector asserts each tool carries.

Companion oracle tier kept and green: `cargo test -p ipc` = 33 passed
(`alloc_bit_is_lowest_clear`, `register_sequence_keeps_used_coherent`,
`pending_drain_is_lowest_first`, `alloc_exhausts_at_word_bits`). Host build and the
aarch64 kernel cross-build link (erasure intact — `verus!{}` erases to the same
exec the proofs ran against).

### Proof shape that worked

- **`drain_one`** is a structural twin of `lowest_clear_bit`: `pending.trailing_zeros()`
  + `axiom_u64_trailing_zeros`, bridged by `bit_vector` from the axiom's
  `(pending >> k) & 1` form to the `pending & (1 << k)` form.
- **`register_bound_into`** loop invariant tracks the *processed* set as
  `mask & !bits` (slots filled there, entry value elsewhere) with `decreases bits`.
  The keystone is one `by (bit_vector)` lemma, **`lemma_pop_lowest`**: given `bit`
  is the lowest set bit (`bits & (1<<bit) != 0` **and** `bits << (64-bit) == 0` — the
  axiom's *aggregate* trailing-zeros form, fact 4, which gives "low `bit` bits clear"
  as a single mask fact with **no** `forall`), it proves
  `bits & (bits-1) == bits & !(1<<bit)` and `bits & (bits-1) < bits`. With that, the
  per-step coherence re-establishment is a case split on `b == bit` vs `b != bit`,
  each a small `bit_vector` fact.

### Walls hit and how they were cleared (for the next implementer)

- **Spec `bits - 1` is `int`, not `u64`** — `bits & (bits - 1)` in a spec/`ensures`
  re-types to `int` and breaks `&`. Use `sub(bits, 1u64)` (the axiom's idiom); bridge
  the exec `bits - 1` to it with one `assert(newbits == bits & !(1u64 << g))` after
  the lemma.
- **`b as u64` inside `by (bit_vector)`** fails with *"expected finite-width integer
  … got Int"*. Bind `let bb = b as u64;` **outside** the `by (bit_vector)` and use `bb`.
- **`open spec fn` requires `pub`** — a crate-private helper spec returning a private
  type cannot be `open`. Inline the datatype literal (`Some(Reg { .. })`) in specs
  instead of a helper.
- **mut-ref postconditions need `*final(_)`/`*old(_)`** (not bare `*used`) on this pin
  — used in `alloc_lowest`; avoided entirely in `register_bound_into` via by-value.
- **Loop-entry quantified invariant** had to be proven explicitly (`assert forall …
  by { … }`) before the loop; Verus would not discharge "every slot still equals its
  entry value" from `mask & !mask == 0` on its own.

## Performance

Measured cold (`cargo clean -p ipc`), deterministic `rlimit`, against a baseline
re-derived from the pre-change tree. **Cross-module controls byte-identical**
(`session::lemma_grant_decode_encode` 76379→76379, `lemma_grant_encode_decode`
71099→71099, `header::lemma_encode_decode` 57671→57671, `GrantReply::decode`
49951→49951, `le_bytes::lemma_u32_le_reassemble` 19210→19210,
`header::lemma_decode_encode` 45094→45094) — so the measurement is clean and no
unrelated proof was perturbed.

Crate-total `rlimit` `768598 → 3299673` (`+2.53M`). This is entirely the new
obligations' own cost plus one in-module collateral:

| obligation | rlimit |
|---|---|
| `register_bound_into` (heaviest in crate) | 1457008 |
| `lemma_pop_lowest` | 399811 |
| `drain_one` | 340842 |
| `alloc_lowest` | 8471 |
| `lowest_clear_bit` (untouched source) | 165531 → 490466 (**+325k collateral**) |

The `lowest_clear_bit` rise is **module-context inflation**: introducing the `Reg`
/ `Signals` / `RegisterErr` / `coherent` datatypes enlarges the SMT background every
reactor-module query carries (the cross-*module* controls staying byte-identical
confirms it is confined to this module). Highest single `rlimit` (1.46M) verifies
**within the default ceiling** — no `#[verifier::rlimit]` bump needed. Per the §10
discipline this is additive new surface, not a regression of existing obligations,
so it is kept as the baseline; `#[verifier::spinoff_prover]` on `register_bound_into`
(and/or hoisting the verified cores to their own module to keep `lowest_clear_bit`
cheap) is a documented lever if the cost ever bites.

## Reverted vs kept

Nothing reverted — the proof succeeded. Kept: the three verified cores, the moved
datatypes, the delegating shells, and the `wait`→`drain_one` rewire. The three
proptests are unchanged (still the companion oracle tier).

## Proposed guideline additions (`doc/guidelines/verus.md`)

1. **The "verified free-fn core + plain generic shell" carrier.** When the function
   to verify is a method on a struct that is generic over an *external* trait or
   holds a reference (here `Reactor<'t, T: Transport>`), do **not** drag the struct
   into `verus!{}`. Lift the pure state into a free function over explicit args
   (`fn f(state: &mut S, …)` or by-value `fn f(state: S, …) -> (S, …)`) and have the
   method delegate. Only the small payload datatypes need to enter `verus!{}`. This
   is the existing `lowest_clear_bit`/`alloc_bit` idiom, generalized.
2. **Prefer by-value over `&mut` to dodge the mut-ref postcondition syntax.** On this
   pin, `&mut` params force `*old(_)`/`*final(_)` in postconditions and are least
   documented inside loop invariants; for a small owned aggregate (a `[T; N]` of Copy
   elements), taking it by value and returning it keeps every postcondition/invariant
   in plain `r.0@`/entry-param terms. Note the setup-time copy as a deliberate
   verification-carrier trade.
3. **The `bits & (bits - 1)` clear-lowest-bit loop recipe** (the §6 bit-vector
   companion to §4's `decreases`): track the processed set as `mask & !bits`, key the
   step on the one `by (bit_vector)` identity `lemma_pop_lowest` above, and feed it
   the axiom's **aggregate** `i << (64 - tz) == 0` form (not the per-bit `forall`) so
   "lower bits clear" is a single mask hypothesis. Plus the three syntax traps above
   (`sub(_,1u64)` for spec subtraction; `let bb = b as u64` before `by (bit_vector)`;
   inline datatype literals instead of a crate-private `open spec`).
