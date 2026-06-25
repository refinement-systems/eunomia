# 6 — Reactor `register` path coherence + the `wf` dispatch invariant (Task 6)

Date: 2026-06-26. Attempt against `doc/plans/0_verus-concurrency.md` Task 6
(`reactor dispatch: pure bitmap-coherence invariant`, the feasible plain-Verus
subset of the proposed dispatch state machine). Outcome: **verified on the second
attempt** (one missing `bit_vector` hypothesis); `cargo verus verify -p ipc` rises
`68 → 71 verified, 0 errors`. No reverts; a latent `register` coherence/bit-leak bug
was found and fixed along the way.

## What was attempted

Task 1 brought the IPC reactor's pure dispatch arithmetic under Verus, but verified
only the *bound* (mask) registration path (`register_bound_into` over the `coherent`
slot/used bijection). The channel **`register` (lowest-clear) path** stayed
proptest-only: `Reactor::register` called the verified `alloc_lowest` for the bit but
filled `self.slots[bit] = Some(..)` in plain Rust, so the register path's slot-fill +
coherence preservation was unverified. Task 6 closes that gap:

1. **A named `wf` dispatch invariant** — `spec fn wf(slots, used) = slots.len()==64 &&
   coherent(slots, used)` — the single name the plan asks for the dispatch invariant
   to carry. `register_bound_into` was re-keyed from bare `coherent` + a separate
   `len()==64` clause to `requires/ensures wf` (naming only, no proof change).
2. **`register_into`** — the verified twin of `register_bound_into` for the
   lowest-clear path: by-value `(slots, used, signals, key) -> (slots', used',
   Result<usize, RegisterErr>)`, `requires wf` / `ensures wf`, proving the register
   path preserves coherence (no double-allocation; `Full` (state unchanged) iff the
   word is exhausted; on success exactly the allocated bit set and exactly that slot
   filled — `r.0@ == slots@.update(bit, Some(Reg{key, signals}))`).
3. **Rewired `Reactor::register`** to delegate to `register_into` (bind-before-commit).

`pending` carries **no** clause in `wf`: it may hold signaled-but-unregistered bits
(`wait` skips them), so its only discipline stays `drain_one`'s lowest-first
single-step `ensures`. `wait`'s blocking loop, `Transport::{bind,notif_signal,
notif_wait}`, and cap-marshalling stay trust-routed (no `state_machine!`: the reactor
is single-threaded `&mut self`, so tokens are pointless — plan decision rule 3).

## Latent bug found and fixed (an incidental keep)

Bringing the register path under a coherence invariant forced the bind-failure window
into the open. The old `register` set the `used` bit (via `alloc_bit`) **before** the
`Transport::bind`s and filled the slot **after** them:

```rust
let bit = self.alloc_bit().ok_or(RegisterErr::Full)?;   // used bit set
self.bind(source, Event::Readable, mask)?;              // <-- early return on Err
...
self.slots[bit] = Some(Reg { key, signals });           // slot filled (never reached on bind Err)
```

A `bind` failure returns after the `used` bit is set but before the slot is filled,
leaving `used` bit set / `slots[bit] == None` (coherence **violated**) and the bit
permanently leaked (there is no deregister). The path is dead under `ModelTransport`
(its `bind` never returns `Err`, `ipc/src/model.rs`) so no test exercised it, but it
is live under `SyscallTransport` (a real syscall that can fail).

The fix falls out of delegating to `register_into`: compute the coherent `(slots',
used')` purely first, **bind before committing**, and write `self.slots`/`self.used`
only after every bind succeeds. A bind failure now leaves `self` entirely unchanged —
the bit is not even consumed. The poll-once `notif_signal` stays the final step, so
lost-wakeup ordering is preserved (the restructure is observationally identical in
every Loom/Shuttle/proptest harness and in the TLA `Register` action, which is atomic
and models no bind failure).

## Result

`cargo clean -p ipc && cargo verus verify -p ipc` → **`71 verified, 0 errors`** (real
run, results line present; prover `0.2026.06.07.cd03505` / toolchain `1.95.0`). Delta
`+3` over `68`: `register_into` (one exec obligation) plus its two `assert … by
(bit_vector)` sub-obligations (the gate counts items, not lines). `wf` is a `spec fn`
(no proof obligation, `+0`), and re-keying `register_bound_into` to `wf` added nothing.

Companion oracle tier kept and green: `cargo test -p ipc` = **33 passed**
(`alloc_bit_is_lowest_clear` — re-pointed at `alloc_lowest`, see below —
`register_sequence_keeps_used_coherent`, `pending_drain_is_lowest_first`,
`alloc_exhausts_at_word_bits`). Host build (`cargo build -p ipc`) and the aarch64
kernel cross-build link (erasure intact — `verus!{}` erases to the exec the proof ran
against).

### Proof shape that worked

`register_into` is the *simpler* single-bit version of `register_bound_into`'s
per-step coherence block — no drain loop, no `decreases`. `alloc_lowest`'s
postcondition hands over `bit < 64`, `used & (1<<bit) == 0`, `u == used | (1<<bit)`,
and lowest-clear directly. After `out[bit] = Some(Reg{key, signals})` the slot view is
`out@ =~= slots@.update(bit as int, Some(..))` (array set axiom, `broadcast use
vstd::array::group_array_axioms`), and `coherent(out@, u)` re-establishes by a case
split on `b == bit` (slot now `Some`, and `u`'s bit `bit` is set) vs `b != bit` (slot
unchanged; OR-ing in `1<<bit` leaves bit `b` of `used` untouched, so membership carries
over from the entry coherence) — exactly the `kcore/src/ready.rs`
`lemma_set_bit_self`/`lemma_set_bit_other` shape, here inline `by (bit_vector)` in the
u64 idiom this module already uses.

### Walls hit and how they were cleared

- **`by (bit_vector)` needs `g < 64` even when the bit is "obviously" in range.** The
  first attempt verified the `b != bit` arm but failed the `b == bit` arm
  (`70 verified, 1 errors`): `assert(u & (1u64 << g) != 0) by (bit_vector) requires u
  == used | (1u64 << g), bb == g` is **not** valid without `g < 64` — the solver is
  free to pick `g >= 64`, where `1u64 << g == 0` (shift-wrap semantics) makes `u & 0 ==
  0` and the assertion false. The fix is one extra `requires g < 64` (in context from
  `alloc_lowest`'s `bit < 64`). The sibling `b != bit` arm already carried `g < 64`,
  which is why only one arm failed. This is the bit_vector-shift companion to Task 1's
  "feed the aggregate trailing-zeros form" note: **any `1 << k` inside `by
  (bit_vector)` needs an explicit `k < width` hypothesis or the shift can wrap to 0.**

## Performance

Measured cold (`cargo clean -p ipc`), deterministic `rlimit`, against a baseline
re-derived from the pre-change tree (`scripts/verus-baseline.sh ipc`). **All four
cross-module controls byte-identical** (`session::lemma_grant_encode_decode`
71099→71099, `lemma_grant_decode_encode` 76379→76379, `GrantReply::decode`
49951→49951, `header::lemma_encode_decode` 57671→57671) — the measurement is clean and
no unrelated proof was perturbed.

Crate-total `rlimit` `3299673 → 3799763` (`+500090`, `+15%`), entirely the new
register-path surface plus in-reactor-module context inflation:

| obligation | rlimit (pre → post) |
|---|---|
| `register_into` (new) | — → 436058 |
| `register_bound_into` (heaviest in crate) | 1457008 → 1488833 (+31825) |
| `lemma_pop_lowest` | 399811 → 430429 (+30618) |
| `lowest_clear_bit` | 490466 → 493659 (+3193) |
| `drain_one` | 340842 → 339198 (−1644) |

The in-module rises are the same **module-context inflation** Task 1 documented:
introducing `wf` + `register_into` (and `register_into`'s `Reg`/`Signals` literals)
enlarges the SMT background every reactor-module query carries. They are not a
regression of an existing obligation's intrinsic cost — the cross-module controls
staying byte-identical confirms it is confined to the reactor module. Heaviest single
obligation (1.49M) verifies **within the default ceiling** — no `#[verifier::rlimit]`
bump needed. Per the §10 discipline this is additive new surface, kept as the
baseline; `#[verifier::spinoff_prover]` on the two `register_*_into` (or hoisting the
verified cores into their own module) remains the documented lever if the cost bites.

## Reverted vs kept

Nothing reverted — the proof succeeded. **Kept:** the `wf` named invariant, the
verified `register_into`, the `register` rewire (which fixes the bind-failure leak),
and the `register_bound_into` re-key to `wf`. The dead `alloc_bit` shell method
(`register` was its only production caller; after delegating to `register_into` it was
reachable only from a test, a `-D warnings` hazard) was **deleted** and the
`alloc_bit_is_lowest_clear` proptest re-pointed at the verified `alloc_lowest` core it
characterizes — an incidental tidy-up, not a coverage change. All three reactor
proptests stay the companion oracle tier (`register_sequence_keeps_used_coherent` is
the end-to-end `register`/`register_bound` coherence oracle and still drives the real
shells).

## Proposed guideline additions (`doc/guidelines/verus.md`)

1. **`1 << k` inside `by (bit_vector)` always needs an explicit `k < width`
   hypothesis** (§6 bit_vector scope). Even when `k < width` is "obviously" true in
   context (here `bit < 64` from `alloc_lowest`), the SMT bit-vector solver models the
   shift with wrap semantics and is free to pick `k >= width` (`1 << k == 0`) unless
   `k < width` is in the assert's `requires`. Symmetric arms that differ only in their
   bit-vector hypotheses are the tell — one verifies, its twin fails because it dropped
   the bound. (The set/clear companion to Task 1's "feed the aggregate trailing-zeros
   form, not the per-bit `forall`" recipe.)
2. **A coherence invariant on a delegating shell surfaces the shell's mid-op
   windows.** When you lift a struct method's mutation into a verified pure
   `*_into(state) -> state'` helper and have the method commit `state'` atomically, any
   *intermediate* incoherence the old in-place method tolerated (here: `used` bit set
   before the slot fill, across a fallible `bind`) becomes visible and is naturally
   fixed by **compute-pure-then-commit-last** (bind/IO before the commit, so a failure
   leaves the struct untouched). The carrier pattern (§ "verified free-fn core + plain
   shell", Task 1) and this commit-ordering rule compose: the helper proves the new
   state is well-formed; the shell's commit-last ordering makes *every* path well-formed.

## Trusted base

No change to the **14**-seam tally: `register_into` + `wf` are pure `u64`/array
reasoning over `vstd`'s `axiom_u64_trailing_zeros` + `bit_vector`, no new
`external_body`/`assume_specification`. `doc/guidelines/verus_trusted-base.md` updated:
the IPC dispatch routing note now records both registration paths (`register_into`
joins `register_bound_into`) preserving the named `wf` invariant, and that the
`register` shell's `Transport::bind`s + poll-once `notif_signal` stay trust-routed (the
verified `register_into` computes the allocation; the shell commits only after the
binds succeed). The `-p ipc` Baseline row is bumped `68 → 71 verified`, and the three
transitive references (the `Admission` note; storage-server and loader cold re-verify
notes) follow. `CLAUDE.md`'s gate list and the CI `verus` job already include `ipc` —
no change.
