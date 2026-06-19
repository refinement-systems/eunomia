# Verus

Verus is the kernel core's **deductive-verification tier**: it proves the `kcore`
object model and the §4.7/§4.8 host chokepoints meet functional `ensures`,
**terminate**, and preserve their `wf` invariants **for all inputs** — the
unbounded successor to the retired Kani tier (`kani.md`). The authoritative plan
is `../plans/3_verus-rewrite.md`; the enumerated trusted base is
`../results/68_verus-findings.md`; the dated technique record is the series of 47
findings docs, `../results/21_verus-findings.md` through `…/67_verus-findings.md`,
which this note distills so a contributor need not read them.

Two parts: **Part A** is the working discipline (pin, CI, structure, trusted
base, scope); **Part B** is the technique harvested from the rewrite, written to
be usable without reading the current code — general rules plus minimal,
self-contained snippets.

---

# Part A — working discipline

## The pin

Three versions move as **one unit**, pinned in every verified crate's
`Cargo.toml` and the `verus` CI job:

| Component | Pin | Why exact |
|---|---|---|
| Verus binary | `0.2026.06.07.cd03505` | no stable API; behaviour moves between builds |
| `vstd` | `=0.0.0-2026-05-31-0205` | the ghost library; tracks the binary in lockstep |
| Rust toolchain | `1.95.0` | Verus's `rust_verify` driver requires the *exact* rust it was built against; `@stable` floats off it and the run fails |

Verus has no crates.io binary: CI fetches the release zip (it bundles
`verus`/`cargo-verus`/`z3`) and caches it by version. **An upgrade is its own PR**
— bump binary + `vstd` + toolchain together, re-run the whole suite, never fold
into a feature change (the cargo-kani-0.67.0 discipline).

## The CI job and erasure

The `verus` job runs one `cargo verus verify` per verified crate, **no per-proof
filter** — a new `verus!{}` obligation auto-gates, the discipline the
`concurrency` and fuzz jobs also use:

```sh
cargo verus verify -p kcore
cargo verus verify -p ipc
cargo verus verify -p urt
cargo verus verify -p dma-pool
cargo verus verify -p cas --no-default-features   # cas is Vec-heavy; the feature-agnostic codecs verify in the no_std+alloc variant
```

`verus!{}` **erases to nothing**: ghost/spec/proof code compiles away, so
`cargo build` (host) and the aarch64 kernel cross-build link the *same* `exec`
code the proofs run against, and `vstd` compiles to nothing load-bearing. This is
the load-bearing guarantee — the verified core cross-compiles unchanged, and a
green proof is a statement about the shipped binary. `scratchpad` is the minimal
`spec fn` canary that the pin + install + cross-build still cohere independent of
any real crate.

## Three-layer structure

Each verified module is three layers:

- a **`spec fn` model** — the math the code refines: a `wf` well-formedness
  predicate, a FIFO as a `Seq`, a page table as a `Map<VA, …>`, a refcount census
  as a `Map<ObjId, nat>`;
- **`exec fn`** operations carrying `requires`/`ensures` against that model;
- **`proof fn`** lemmas for the hard steps (acyclicity preserved, census equals
  stored, a frame lemma).

`decreases` on every loop and recursion (Part B); ghost `Seq`/`Map`/`Set` as the
models; the arena representation (Part B) is what keeps all of it first-order.

## Visibility: `open`, `closed`, and the `verus!{}` boundary

Three rules govern what a contract may name:

- **A `const` (or `spec`/`proof` fn) is visible to spec/contract clauses only if
  declared *inside* `verus!{}`.** Items outside the macro are invisible to the
  verifier and uncallable from verified code; moving one in is mechanical and it
  erases to the same `pub const` at the same path. Visibility (`pub`/`pub(crate)`)
  is orthogonal to this.
- **A `pub open spec fn` body may name only public items** — Verus rejects "in
  pub open spec function, cannot refer to private const item".
- **`pub closed spec fn` is the escape**: the *name* is exported (a `pub` `ensures`
  may reference it) while the *body* stays module-private and may read private
  consts/fields; inside the module the closed body still unfolds transparently for
  the solver. Reach for `closed` whenever a public operation's correctness is
  naturally stated in internal terms. (If the operation is single-crate, narrowing
  the consts to `pub(crate)` is the lighter escape.)

## The trusted base

Verus trusts a fact only through a named construct, and **every trust boundary is
enumerated once** in the ledger (`../results/68_verus-findings.md`) — the source
of truth for CLAUDE.md's "the trusted base is exactly …" claim. The discipline,
in one line: **`external_body`/`external` only at a genuine boundary, each paired
with a host test, and no bare `assume` survives.** The four legitimate
`external_body` categories and the host-test-with-teeth method are Part B
("Trusted seams"). An `external_body`/`external` row that cannot name **both** a
reason and a test is a finding, not a boundary.

## When Verus is not the tool

The plan's best-tool table (`../plans/3_verus-rewrite.md` §2) is authoritative;
the short form:

- **Concurrency interleavings** (lost-wakeup, seqlock torn reads) → Loom/Shuttle +
  TLA+. Verus *can* do tokenized concurrency, but it is a research-grade lift and
  the existing tier is strong.
- **Adversarial bytes** (wire/on-disk decoders, ELF, mount over arbitrary device
  contents) → cargo-fuzz (`fuzzing.md`). Verus proves decode *totality* and
  *canonical form*; differential/corpus coverage stays fuzzing's.
- **Design-level state machines** (revocation, the commit protocol) → TLA+. Verus
  closes the model-to-code gap on the extracted function; TLA+ owns the design and
  the content-coverage half.
- **The asm shell** (boot, MMU/TLB, GIC, MMIO, the one PA→pointer site) → trusted
  base, inherently unverifiable; the whole `kcore` split exists to keep it small.
- **Crypto/perf inner loops** (blake3, the FastCDC gear loop) → out of scope; stub
  a hash with an injective-on-small-inputs ghost where a proof needs one.

---

# Part B — technique distilled from the rewrite

## 1. The enabler: index newtypes and typed arenas

**The foundational decision, made before any property is stated.** Store objects
in typed arenas and link them by **index newtypes** (`SlotId`, `ObjId`), never by
raw pointers or references. Every later proof — invariants, census, termination —
rests on this; nothing else compensates for getting it wrong.

Indices are plain values, so the verified core stays **first-order**: no
`PointsTo` permissions to thread through call chains, no aliasing obligations, no
separation-logic bookkeeping. State is a map from index to object, and an
operation is a pure function on that map. A pointer-linked graph instead forces a
permission token per reachable node — the dominant cost and failure mode of
memory-model verification. Making links *data* trades that whole burden for
ordinary `Map`/`Set` reasoning.

```rust
// Links are indices, not pointers. The "heap" is a map.
struct SlotId(u32);
struct ObjId(u32);
struct CdtNode { parent: Option<SlotId>, first_child: Option<SlotId>,
                 next_sib: Option<SlotId>, prev_sib: Option<SlotId> }
type Slots = Map<SlotId, CdtNode>;   // first-order: a total map, no permissions
```

Structural well-formedness becomes a `spec fn` quantified over the domain
(every link in-domain, back-pointers agree); refcounts become a **census** (count
the references *from* the structure) equated to a stored number — statable only
because the whole store is one inspectable value. This is also good kernel design
independent of Verus (stable identity across compaction, serializable links, no
lifetime entanglement, bounds-checked by construction); verification just makes
the payoff non-negotiable.

**The one structural caveat — model for the proofs you'll *need*, not just the
ones you have.** A purely structural invariant (links in-domain, siblings
consistent) does not pin parent↔child-list reachability: a node can name a parent
yet be absent from that parent's child chain. The gap is invisible until a later
recursive/looping op needs a well-founded measure or "the loop visited every
child." Decide the invariant's strength up front; under-specifying it doesn't
block early non-recursive proofs but walls off every recursion later.

## 2. Spec models: choose the representation that makes ops one-liners

Bring a concrete mutable object behind a verification seam by defining a
spec-only **ghost view** that mirrors exactly its mutable state and nothing more
(keep length, identity, order; abstract payload bytes to length+identity). Expose
it as a trait getter and relate every accessor to it in `requires`/`ensures`. The
production type needs no change — the view erases. This proof-light seam ("the
enabling refactor") is the recurring keystone: it lets later, harder ops reason
over a settled abstract representation.

Pick the representation that makes operations one-liners:

- **FIFO ring → `Seq`.** Model a circular queue as a `Seq` of length `count`,
  element `j` at physical index `(head + j) % depth`. Then `send == Seq::push`,
  `recv == Seq::drop_first`; the modular arithmetic is quarantined in the
  projection and the op spec is a one-line equation.
- **Intrusive linked list → existential `Seq` witness.** Model head/tail + per-node
  `next` as a `Seq<NodeId>` witness with distinct elements threading head→…→tail;
  well-formedness is "such a witness exists." The `Seq` index doubles as the
  acyclicity rank for free. Markedly lighter than a doubly-linked or ring model.
- **Partial / multi-level map → pointwise spec walk returning `Option`.** A page
  table / trie / sparse index is a `spec fn` that walks levels and returns
  `Some(leaf)` (with a present-but-empty sentinel where needed) or `None`.
- **Immutable structure → view with getters, no setter.** State fixed at creation
  (layout, residency, handles) frames *definitionally* across every mutator —
  surface these early, they are the cheapest cross-call handles.

Two modeling cautions. **Store identity, not contents:** when an aggregate holds
cells that really live in a shared arena, model it as holding only the `Id`s; the
contents live in the one arena view. Copying contents into a per-object view
creates stale duplicates a later mutation of the arena can't reach — a real bug.
And model arrays-of-arrays as `Seq<[T; N]>` (the natural `@` view), not
`Seq<Seq<T>>` (which forces a `deep_view` and per-element bridge lemmas).

A `choose`-defined canonical order needs a **uniqueness lemma** before op effects
can be stated as equalities (without it `choose` yields an arbitrary witness and
you can prove only existence); prove it by induction on the chain. Guard any
`choose`-derived count so its out-of-domain value is a deliberate constant
(`if witness_exists { seq.len() } else { 0 }`).

**Mut-ref postcondition syntax.** In a `&mut self` method's `ensures`, name the
post-state `final(self)` (or the returned value) and the pre-state `old(self)`.
Bare `self` in a postcondition is a hard compile error on current toolchains —
many stale web examples use it and will not compile (the restriction is on
`ensures`; `requires` may use bare `self`). A read-only `&mut` frame is
free: keep the ergonomic signature and prove `*x == *old(x)` by calling no
mutator.

## 3. Frames and invariants

**Enumerate frames; do not approximate.** A frame is *every view and every
per-element field any downstream caller reads across the call* — not one clause. A
function that rewrites only an object's *link* fields still needs a clause pinning
every other element's *content* field, or a distant caller cannot prove an
obviously-true preservation. Under-framing surfaces as a stuck, trivially-true
proof far from its cause.

```rust
fn slot_move(store: &mut S, src: Id, dst: Id)
    ensures
        store.other_view() == old(store).other_view(),       // other views unchanged
        forall|x| x != src && x != dst ==>                    // content frame, not just links
            store.slot_view()[x].content == old(store).slot_view()[x].content;
```

Practical refinements:

- **Add a concrete per-key clause beside the universal `forall`** (`final[child].cap
  == old[child].cap`): directly usable with no trigger gymnastics, cheap to state,
  saves every call site.
- **Guard frame antecedents with domain membership** (`view().dom().contains(x)`,
  threading domain preservation via `=~=`); without it the frame implicitly claims
  a junk default value is frozen for out-of-domain keys.
- **A loop havocs any state-wide view unless the invariant pins it.** A function
  framing `view_X` across a body containing a loop must restate `view_X ==
  old(view_X)` in the loop invariant even if the loop never touches it.
- **`ensures` is additive; `requires` is not.** Adding a postcondition only adds
  facts at call sites and can never break a caller — front-load frame clauses onto
  shared helpers freely. A new precondition *can* break callers; introduce one only
  behind a require-and-preserve invariant. Corollary: a property a caller must
  thread across a `&mut` call belongs in the **callee's `ensures`** (the
  intermediate state is not nameable across `&mut`), stated conditionally
  `P(old) ==> P(final)` so callers without `P` gain no obligation.
- **Per-arm postconditions over error-erasing preconditions.** Never add a
  `requires` that rules out a real error path — it makes the path dead code and
  silently drops its guarantees. State per-arm posts on every return variant
  (`r is Ok ==> …`, `r == Err(BadArg) ==> *store == *old(store)`); the per-arm form
  proves strictly more and stays faithful.

**Layer well-formedness.** Split a heavy invariant into a **structural fragment**
(first-order, total, ∀ — domain/link/consistency) and separately-layered
properties (acyclicity, full refcount soundness) that are harder to preserve;
compose `wf && acyclic && …`. Cheap ops verify against only what they need. Add a
clause **only when an op first consumes it**, not by front-loading. Acyclicity
does *not* compose for free: every constructing op must `ensure` the full
invariant, or some consumer's precondition is discharged only at the trusted
boundary — a hidden gap; audit the call graph.

**Counts are a census.** To verify a refcounted store, define a census `spec fn`
that recounts every reference *from the structural state*, and an invariant
`refs[o] == census(o)`. Decompose census as a **sum of independent per-kind
terms** — one per distinct way state can hold a reference — so one mutation
perturbs exactly one term and frame lemmas compose.

```rust
spec fn census(s: S, o: ObjId) -> nat {
    slot_refs(s, o) + mapping_refs(s, o) + queue_refs(s, o) + binding_refs(s, o)
}
spec fn refcount_sound(s: S) -> bool {
    forall|o| s.refs_view().dom().contains(o) ==> s.refs_view()[o] == census(s, o)
}
```

The decisive rules: **keep the census strictly off the count it constrains**
(read only structural views, never `refs_view`) — then an op editing only the
stored count preserves the census *by framing alone*, no recount lemma; **enumerate
all reference-holding mechanisms** (a missed one undercounts); **need the exact
equality, not a `>= 1` lower bound** (a lower bound doesn't survive a decrement
that strands a sibling at zero); and **per-term-zero is a gift** — at the last-ref
point (`count == 0 == census`) every non-negative term is individually zero,
handing you "no waiters," "not self-bound," etc. for free at the destructor.

**Off-by-one windows.** A teardown that clears a designating cap *before* the
matching decrement is transiently off-by-one. State count deltas **additively**
(`refs(old) == refs(new) + delta`), never subtractively (`(refs(old)-delta) as
nat`, which re-proves no-underflow on every recombination). Inside a window where a
conditional `inv(old) ==> inv(final)` is useless (the hypothesis is already
false), use an unconditional lockstep delta:

```rust
spec fn census_delta_frozen(s0: S, s1: S) -> bool {
    forall|x| s1.refs(x) + census(s0, x) == s0.refs(x) + census(s1, x)   // additive: no nat underflow
}
```

Order a destructor's writes so the invariant is only ever transiently false in the
direction the next callee's contract expects (clear the count-dropping field
*first*, then call the unref that consumes that window). **Model "destroyed" as
`refs == 0`, not domain removal** — most destructors leave the object in the map;
prove `dead(s,o) := !dom.contains(o) || refs[o] == 0` monotone ("dead stays dead")
so cross-object cascades can rely on it.

## 4. Termination: a finite quantity that strictly drops

Every loop and recursion needs a `decreases` measure — a value provably bounded
below that strictly drops each step. Verifying the body *with* the measure **is**
the totality/no-panic theorem for all inputs; there is no unwind bound to pick.
The discipline is always "find a finite arena quantity the step shrinks." What
varies is which quantity and how you handle the floor and the awkward exit step.

- **Forward index walk:** `decreases seq.len() - k`. Lightest measure; reach for it
  first.
- **Variable-length parser:** `decreases buf.len() - off`, sound only if every
  iteration advances `off` by a *positive* amount. Guarantee it by having the
  framing parser's contract promise a **minimum record length** (`HEADER_LEN <=
  rlen`) — this turns a "bounded by construction" trust comment into a proven
  anti-DoS property: a forged buffer cannot hang boot/recovery.
- **Stride / overshoot loop** (cursor steps by `stride` toward an arbitrary `end`,
  overshooting): the naive `end - cursor` goes negative and is rejected. Clamp:
  `decreases if page < end { (end - page) as int } else { 0int }`.
- **`Some→None` exit step** of a linked walk: `rank[cur]` can't drop on the final
  step. Add one: `decreases rank[cur] + 1`.

```rust
while let Some(f) = decode_frame(buf, off)
    decreases buf@.len() - off    // f.rlen >= HEADER_LEN > 0  ⇒  strict drop
{ off += f.rlen as usize; }
```

**Ghost rank witnesses for acyclic recursion.** When acyclicity is not pinned by a
depth field, define it as an existential over a ghost rank map: `acyclic(m) =
exists r. valid_rank(m, r)` with a strict decrease across every edge. A descent
*chooses* a witness and uses `decreases r[cur]`. **Using a rank is cheap;
re-constructing one after a mutation is the hard direction** and forces a stronger
structural invariant: to re-parent a detached childless node, shift every old rank
up by one and seat the new node at 0 — sound only if no slot already names it as a
parent, which the `wf` predicate must guarantee.

**Lexicographic `(count, height)` for mutual-recursion teardown.** For a destructor
SCC where the cycle-breaking edge drops a global count (empties a slot) but every
other intra-cluster edge is count-flat, give *every* SCC member the measure
`(count_nonempty(view), height)`. The non-obvious crux is the **height direction**:
the count-dropping leaf gets the *lowest* tag, the dispatcher it calls the
*highest*, so every count-flat edge strictly descends in `height` while the single
count-dropping edge wins on the first component. A teardown *loop* over an unbounded
subtree terminates the same way — `decreases count_nonempty(store)`, each iteration
empties a slot.

## 5. Arithmetic: keep the main proof linear

Z3 is reliable on *linear* arithmetic and flaky on *nonlinear* (multiplicative)
and bit-blasted goals. The whole discipline is to keep the main proof linear and
push every product, `%`, and division behind a named one-line lemma.

**Quarantine every nonlinear/modular step in a tiny `proof fn`** backed by
`vstd::arithmetic`, and cite it from the main proof:

```rust
proof fn lemma_scaled_lt(x: nat, y: nat, w: nat)
    requires x < y, w > 0,
    ensures  x * w < y * w,
{ lemma_mul_strict_inequality(x as int, y as int, w as int); }
```

**Modular round-up beats the bit-mask.** `(off + align - 1) & !(align - 1)`
bit-blasts the solver to OOM over a symbolic offset. Rewrite modularly — no
`by (bit_vector)`, and the precondition weakens from "power of two" to `align > 0`:

```rust
let pad   = (align - off % align) % align;
let start = off + pad;                 // start % align == 0, from vstd::arithmetic::div_mod
```

**Relating two divisions (the division-hoist recipe).** Proving a decomposed
computation equals a single division — `secs*N + frac == (delta*N)/f` where `secs =
delta/f` — is the classic step a bounded checker can't take with a symbolic
divisor. Three lines: `lemma_fundamental_div_mod`, one `by (nonlinear_arith)`
rearrangement, then `lemma_hoist_over_denominator` (`x/d + j == (x + j*d)/d`, the
load-bearing find in `vstd::arithmetic::div_mod`).

**Prove overflow-freedom, don't carry it.** State the *exact functional value* as
the postcondition (`r as int == result_spec(input)`); Verus cannot prove it
without first proving every multiply/add/cast is overflow- and panic-free, so a
separate "totality" harness is *subsumed*. For an increment with a `< MAX`
precondition the production path never discharges, **refuse at the ceiling before
mutating**, then drop the precondition:

```rust
let r = self.refs(o);
if r == u32::MAX { return Err(Overflow); }   // refuse pre-mutation
self.set_refs(o, r + 1);                       // now provably no wrap
```

Smaller rules: narrowing casts (`as u8`) carry **no** obligation — they are total;
for a widening cast needing a bound, order the guard *before* the cast so the bound
falls out; restate a `usize` add inside a spec `invariant` over `int` (`p as int ==
base + 7`) to avoid a spurious overflow obligation; and the verifier learns a slice
length fits `usize` only from an **actual exec `.len()` call** — materialize a fresh
`let end = off + n` with `assert(off + n <= buf.len())`, the ghost `buf@.len()`
bound alone does not discharge it. When a std numeric method lacks a vstd spec,
supply a one-line `assume_specification` mirroring the documented semantics (a
trusted seam, Part B §11).

**Negative lesson:** never bound a wide pre-clamp intermediate by the *post-clamp*
type's max — that bound is false; clamping is what handles the excess.

## 6. `bit_vector`: scope it to pure bit identities

`assert(...) by (bit_vector)` is the right tactic for **pure, fixed-width bit
identities** the SMT arithmetic theory handles poorly — mask algebra,
disjointness, field extraction, alignment — and it proves them ∀, not sampled.
The hard scope boundary: **do not aim it at nonlinear or division goals**
(tick→ns, pool offsets); those are `by (nonlinear_arith)`. Even the index-split of
a bitmap proof (`i < words*64 ⟹ i/64 < words ∧ i%64 < 64`) is a `nonlinear_arith`
goal, not a `bit_vector` one.

Two facts explain almost every confusing `bit_vector` failure:

- **It knows only the literals in the goal** — not symbolic consts, not enclosing
  `let`s. Pin a named const first (`assert(MASK == 0xFF) by (compute)`), and inline
  a `let`'s full defining expression into the asserted goal.
- **It rejects struct/datatype field projections** ("unsupported for bit-vector:
  Field"). Bind the field to a plain fixed-width local first.

```rust
// fails:  let w = (b0 as u32) | ((b1 as u32) << 8);
//         assert((w & 0xff) as u8 == b0) by (bit_vector);
assert((((b0 as u32) | ((b1 as u32) << 8)) & 0xff) as u8 == b0) by (bit_vector); // works
```

**The packed-bitmap pattern** (bit `i` lives in `word[i/64]` at position `i%64`)
is the canonical recipe for allocators / presence maps: confine `bit_vector` to
three tiny per-word lemmas — index-split (`nonlinear_arith`), set-bit readback,
other-bits-untouched — and never use it above them. A single `set(i, val)`
write-helper combines them; all loop-carrying ops then reason purely through
`set`'s and `is_free_spec`'s contracts, and slot-distinctness falls out as a
corollary.

```rust
proof fn lemma_set_bit(x: u64, k: u64) by (bit_vector)
    requires k < 64,
    ensures (x | (1u64 << k)) & (1u64 << k) != 0,
            (x & !(1u64 << k)) & (1u64 << k) == 0;
```

Push tight bounds into extractor contracts (`ensures r < 512` for a 9-bit field)
so every downstream index is in-bounds from the contract alone, and state
"by construction" security claims as ∀-theorems (`assert(forall|r| (r & ALLOWED) &
FORBIDDEN == 0) by (bit_vector)`) rather than per-site asserts. Don't over-pin:
align-down facts hold for a *symbolic* mask (`(x & !m) <= x`); pin literals only
for the genuinely stride-bound step. **Parser gotcha:** inside an inline `... by
(bit_vector) requires …;`, a bare `ident < ident` misparses (the `<` reads as a
turbofish) — use a literal RHS or flip to `>`.

## 7. Std combinators with no model: hand-roll the loop

Verus gives no usable spec to many `std` iterator/slice/`Vec` combinators —
`.find().map()`, `.filter().count()`, `copy_within`, `.max(1)`, sometimes
`.saturating_sub`. **First check vstd** (some carry `#[verifier::allow_in_spec]`);
for the rest, two tactics:

- **Rewrite into explicit, invariant-carrying control flow** when the call is on the
  path of a real obligation. A scalar combinator becomes the obvious branch
  (`let f = if self.freq == 0 { 1 } else { self.freq };`); a search becomes a
  `while` loop holding exactly the invariant the surrounding proof needed anyway
  ("everything scanned so far failed; the collection is unchanged"). The rewrite is
  behaviour-identical — keep the pre-existing proptests as the witness that loop and
  combinator agree.
- **Keep the combinator *outside* `verus!{}`** when the call is bookkeeping, not an
  obligation (test helpers, leak/quota assertions, debug counters): a plain `impl`
  block Verus never sees.

**Verified shift helpers for array splices.** `copy_within` (no model) and
`Vec::extend_from_slice` (a `cloned`-predicate spec that fights clean `u8` `Seq`
equality) are best replaced by small helpers carrying an exact index/append
postcondition — the one place to invest, because the same array-splice reasoning
recurs at every free-list unlink, slot move, and extent merge. Factor it once into
`remove_at` / `insert_at` shift loops and a byte-append loop:

```rust
fn extend_bytes(out: &mut Vec<u8>, src: &[u8])
    ensures out@ == old(out)@ + src@
{
    let mut i = 0;
    while i < src.len()
        invariant out@ == old(out)@ + src@.subrange(0, i as int)
    { out.push(src[i]); i += 1; }
}
```

Discharge concatenation/push rearrangements (`(old ++ prefix).push(x) =~= old ++
prefix.push(x)`) with the extensional-equality operator `=~=` in one `assert`, not a
hand-written induction.

## 8. Wire codecs: explicit byte-indexing, accept-iff specs

Verus cannot reason over the ergonomic byte-codec stdlib. Treat the following as
the standing recipe for any fixed-layout, length-prefixed, or tagged binary codec.

**Build values with explicit indexing + mask/shift.** Verus specs *none* of
`uN::from_le_bytes`/`to_le_bytes`, the array `TryInto`, nor `copy_from_slice` —
each is an unverifiable call inside `verus!{}`, and routing through vstd's exec
wrappers makes vstd *runtime* load-bearing (`to_le_bytes` is `alloc`-only and
returns a `Vec`). Hand-write mask/shift arithmetic, which Verus reasons over
natively and is byte-for-byte the little-endian form:

```rust
fn read_u32_le(buf: &[u8], off: usize) -> u32
    requires off + 4 <= buf@.len()
{
    broadcast use vstd::slice::group_slice_axioms;   // links exec buf.len() to ghost buf@.len()
    (buf[off] as u32) | ((buf[off+1] as u32) << 8)
        | ((buf[off+2] as u32) << 16) | ((buf[off+3] as u32) << 24)
}
```

**Index bytes; do not range-slice.** For fixed fields read individual bytes;
slicing (`buf[off..off+n]`) drags in vstd's closed subslice specs and forces a
manual `bit_vector` bridge. Build a fixed `[u8; N]` element-by-element, never
`try_into().unwrap()`. Compare magic bytes as per-byte numeric equalities, never
slice `==`. **Broadcast the axioms** — open each byte-reading helper and the
top-level `decode` with `broadcast use vstd::slice::group_slice_axioms;` (and
`vstd::array::group_array_axioms` for array literals, closing with extensional
`=~=`); without it, byte-indexing proofs fail to link exec length to ghost length,
the near-universal first stumble.

**Spec the codec as accept-iff + a two-direction bijection.** Tie exec functions to
`spec_encode`/`spec_decode` over `Seq<u8>`, and state totality and acceptance as a
single iff — capturing short-input *and* trailing-byte rejection at once:

```rust
fn decode(buf: &[u8]) -> (r: Result<Header, DecodeErr>)
    ensures
        r == spec_decode(buf@),
        r is Ok <==> buf@.len() == HEADER_SIZE;   // and buf[0] == TAG, if tagged
```

Then prove *both* bijection directions (value→bytes→value and bytes→value→bytes):
together they establish a total bijection between values and accepted byte strings
— strictly stronger than a decode∘encode round-trip, and what catches a decoder
that silently accepts non-canonical input. Verifying a fixed-input decoder's body
*is* its totality theorem; attach shape guarantees as `ensures` (`r == Ok(Msg{ len,
.. }) ==> len <= CAP`) so a downstream cast's precondition is discharged at the
decode boundary.

**Control-flow rewrites.** Verus is unfriendly to `?` and to `match` guards (`PAT
if cond =>`); the erased behaviour is identical but the proof is direct only when
the rejection branch is syntactically present. Make the explicit `match … { None =>
return Err(..) }` a uniform convention.

## 9. Keep foreign types off the proof surface

A codec whose real types carry a cryptographic `Hash` (or any opaque,
collision-dependent value) cannot be verified directly: the hash has no
first-order SMT model, and an `external_type_specification` makes it *opaque* —
which blocks both reasoning and **construction** inside `verus!{}` ("constructor
for an opaque datatype"). Keep crypto entirely off the proof surface; three
reusable moves.

**Feed the proof a `Hash`-free image.** Define a parallel `Raw*` struct replacing
every `Hash` field with its decoded bytes — `[u8; 32]` for a digest, `Vec<u8>` for
an inline payload. A fixed array and a byte vector *round-trip inside the proof*
with no hash axiom (`encode_raw(decode_raw(b)) == b` proves directly). The verified
core works only on the image; the `Real ↔ Raw` conversion is a thin plain-Rust
delegator whose only `Hash` contact is a transparent newtype wrap, covered by a
fuzz/differential oracle rather than by proof.

```rust
struct RawEntry { name: Vec<u8>, size: u64, content: RawContent }   // no Hash, no crypto axiom
enum RawContent { Inline(Vec<u8>), ChunkList([u8; 32]) }

fn decode(buf: &[u8]) -> Option<Entry> {       // thin delegator: the only place Hash is touched
    let (raw, k) = decode_raw(buf).ok()?;       // verified core returns the Hash-free image
    if k != buf.len() { return None; }
    validate_entry(&raw).then(|| raw.into_entry())   // plain-Rust validation
}
```

**Split framing (verified) from content (trusted).** When acceptance also depends
on a checksum match or a heavyweight decode you can't express in SMT, prove the
*framing* — magic compare, length reads, `checked_add`, bounds, minimum-length —
fully, and delegate *content acceptance* to a thin trusted function with an
**`uninterp spec fn`** model (Part B §11). Totality and termination need no
collision-freedom, so the hash never enters the proof.

**Own a verus-visible twin of any external enum you must construct.** An error type
exposed via `external_type_specification` — especially one whose variant carries a
`Hash` — is opaque and unconstructable inside `verus!{}`. Declare a small in-block
enum with the same cases, build *that* in the verified body, and map it 1:1 in a
plain-Rust converter (preserve exact messages). The same shape recurs for
survivor/slot/result decision enums — anywhere the verified function *creates*
rather than merely *inspects* the value.

```rust
verus! {
    enum DecodeErr { Truncated, BadEntry(&'static str) }   // built freely by verified code
    fn decode_raw(buf: &[u8]) -> Result<RawValue, DecodeErr> { /* ... */ }
}
fn to_format_error(e: DecodeErr) -> FormatError {           // 1:1 at the boundary, plain Rust
    match e { DecodeErr::Truncated => FormatError::Truncated,
              DecodeErr::BadEntry(m) => FormatError::BadEntry(m) }
}
```

Note that `external_type_specification` also hides layout: Verus **cannot derive
`size_of::<T>() > 0`** for an opaque type even when it is genuinely non-ZST. Treat
every layout/field fact about such a type as something you must *provide* (a
trusted `external_body` helper with `ensures r > 0` + a host test), not something
the verifier recovers.

## 10. Proof scaling: small contexts and trigger economy

A solver query discharges fast only when its context is small. **Decomposition is
the default fix; `rlimit` and `spinoff_prover` are last resorts.** When a query
blows the resource limit, extract the heaviest sub-step into its own `proof fn`
with explicit `requires`/`ensures` rather than raising the limit — split a
multi-clause `wf`-preservation into one lemma per conjunct, and the case analysis
that timed out as a monolith verifies first-try when partitioned.

```rust
proof fn lemma_f_links(m: Map)    ensures links_in_domain(f(m)) { ... }
proof fn lemma_f_siblings(m: Map) ensures siblings_consistent(f(m)) { ... }
proof fn lemma_f_wf(m: Map) ensures wf(f(m)) { lemma_f_links(m); lemma_f_siblings(m); /* … */ }
```

Crucially, **an rlimit blowup on a large inline body often hides a real logical
gap** (an underflow, a wrong branch, a trigger that can never fire) — Z3 thrashes
equally on an impossible goal and an under-resourced one. Suspect a false assertion
*before* raising `rlimit`; splitting the query turns the timeout into a concrete
assertion failure that pinpoints the bug, and the fix usually passes at a fraction
of the budget. Escalation ladder: (1) isolate the heavy obligation into its own
`proof fn`; (2) mark it `#[verifier::spinoff_prover]` — Verus discharges it in a
*separate solver instance* with a fresh context, so the caller's term families and
triggers don't bloat its query (it suits a heavy existential-set frame carried
across a shift/index correspondence, and often closes a proof that only *looked*
like it needed more budget); (3) only then a private `#[verifier::rlimit(N)]` on
that one body. **Nonlocal cost:** adding a *field* to a widely-referenced ghost
view enlarges every SMT term mentioning it and can push an unrelated borderline
proof past budget — budget the isolation ladder whenever you grow a shared view.

**Control what enters the context.** Keep heavy definitions out of queries that
don't need them: make a recursive `spec fn` `closed`/`opaque` and `reveal` it only
where used — a `closed` recursive spec does *not* auto-unfold at a symbolic
argument like `(i+1) as nat`, so write a one-shot step lemma with
`reveal_with_fuel(acc, 2)`. Conversely, pull an axiom *group* in exactly where it
is needed with `broadcast use` (`vstd::slice::group_slice_axioms`;
`group_mul_is_commutative_and_distributive` inside an arithmetic helper) rather
than globally — the related facts land in one query without flooding the unrelated
ones.

**Trigger economy is the dominant scaling hazard.** Concrete traps:

- **`Seq::no_duplicates` carries an O(n²) trigger** (`forall i,j. self[i] !=
  self[j]`); extract it into its own lemma mentioning only the relevant sequences.
- **Prefer a single `Map::insert` equality over a broad frame `forall`.** `m2 ==
  m1.insert(k, m2[k])` feeds one term; `forall|j| j != k ==> m2[j] == m1[j]` floods
  the context. Assert single-key instances in a hot body; push genuine multi-key
  arguments into a separate `proof fn`.
- **Quantify frames over a named predicate, not a raw `Map::index`.** A map-index
  trigger can verify each single use yet silently fail to *compose* two frames
  across a transitivity lemma or loop. Define `spec fn frozen_at(s0,s1,x)` and
  quantify `forall|x| #[trigger] frozen_at(s0,s1,x)` — predicate-application
  triggers compose where index triggers don't.
- **A heavy `ensures` on a looping callee must trigger only on terms its callers
  mention**, or it fires for callers that don't care and blows rlimit.
- For an `exists`/`choose` over a purely arithmetic/modular body, auto-trigger
  inference fails — annotate the binder with the modular term itself:
  `exists|j: int| #![trigger (head + j) % (depth as int)] …`.

**Loops cut the proof context — re-pin everything.** Entering a `while`/`for`,
Verus discards all context except the loop invariant. This is the single
most-hit, hardest-to-diagnose family:

```rust
let ghost v0 = old(store).view();
while i < n
    invariant
        v0 == old(store).view(),        // a `let ghost` is NOT known to equal its definition inside the loop
        pool.len() == old(pool).len(),  // bridge entry facts so early returns inside the loop can use them
{ if found { return ...; } }            // the body sees only the invariant
```

`old()` is usable inside an invariant (it refers to function entry). The
pathological symptom: `assert(view.dom() =~= g.dom())` passes while the
syntactically-equal `... == old(store).view().dom()` fails, because the loop
severed the `g == old(store).view()` link. Also note `assert forall|x| P(x) ==>
Q(x) by {…}` does **not** bind `P(x)` as a hypothesis — use `implies` when the
proof needs the antecedent.

**Two patterns that keep large structural proofs first-order.** For an imperative
in-place mutation, define a pure **closed-form target map** (`relabeled(m,..)`,
`unlinked(m,..)`), prove *once* that the target preserves all invariants, then
prove the body produces exactly that target by per-slot case analysis — separating
"the result is well-formed" (reusable) from "the code computes the result"
(mechanical). And track straight-line writes as a **ghost-snapshot chain** (one
`Map::insert` per write, asserting `store.view() =~= m_i` after each step) so the
solver's map model stays concrete.

## 11. Trusted seams, kept honest by host tests

Verus proves the verifiable core and *trusts* an irreducible boundary. The
discipline keeps that surface explicit, minimal, and continuously checked.

**The four legitimate `external_body` categories** (each pairable with a host
test): (1) **hardware/scheduler/Store seam** — effectful ops Verus can't model
(TLB invalidation, ready-queue mutation, barriers); (2) **out-of-scope total
function** — interpreted hashing/crypto/FFI, where you trust *totality and
determinism*, **not** any deeper property; (3) **runtime-only guard** — a body that
must `debug_assert!`/`panic!`, forbidden in `verus!{}` exec, whose *static*
guarantee lives in a caller `requires`; (4) **opaque layout fact** — e.g.
`size_of > 0` for an opaque type. Audit rule: **every `external_body` names both
why it is a boundary and the host test that exercises it.**

**A bare in-proof `assume` must not survive.** It is the weakest trusted form —
buried, invisible, untested. Triage the fact per case: prove what is provable; for
the genuine residue, move the assumption onto the *signature* of the external
helper as an `ensures`, backed by a host test over boundary inputs (`0 / 1 / mid /
max`). Strictly stronger — named, observable, regression-guarded.

```rust
// WEAK: caller-side, untested.   →   STRONG: the fact named on the boundary + a host test.
#[verifier::external_body]
fn struct_bytes(kind: Kind) -> (r: usize) ensures r > 0 { /* size_of-based body */ }
#[test] fn struct_bytes_positive() { for k in ALL_KINDS { assert!(struct_bytes(k) > 0); } }
```

**`external_body` carries no `requires` obligation — and honesty beats strength.**
Only the declared `ensures` crosses the boundary, so a verified caller can invoke a
trusted op with minimal facts (the lever for staged verification). For an op whose
real effects are entangled (a teardown releasing per-element refs with no closed
form), **assume only the robustly-true checkable core** (wf preserved, domain
fixed, specific slots cleared, untouched fields stated unchanged) and let a host
differential test cover the rest — a *false strong* clause is worse than an *honest
narrow* one.

**Host-test every assumed contract, with teeth.** Maintain a concrete reference
impl of the seam (an array-backed mock) and, per assumed op, a differential test
that asserts **(a)** the frame holds (snapshot, compare field-by-field) *and*
**(b)** the intended effect happened (so the frame is not a vacuous no-op),
exercising both branches of any conditional. Three traps make such a test pass
while checking nothing:

- **The mirror must have teeth.** An executable mirror of a ghost `wf` is worthless
  if it accepts everything — add a `_has_teeth` test with one *deliberately
  malformed* shape per clause (a cycle, half-linked siblings, a phantom child), each
  asserted *rejected*, plus one valid shape accepted.
- **The mirror must be faithful** — if the contract says the op removes key `a`, the
  mock's body must actually remove it.
- **The fixture must satisfy the precondition** — a differential test silently
  *skips* its assertion when the fixture violates the invariant's precondition.
  Build fixtures with the *verified* constructors so the generator can't start
  ill-formed.

**Modeling effect *ordering* at the seam.** To verify the *order* of side effects
(TLB invalidations, log records) — not just final state — add a ghost
**effect-log view** to the seam trait: the effect method *appends* its record,
fences *frame* the log unchanged. The append clause makes "one effect per event, in
order" provable; back the trait with a real `Vec` so a host test checks it.
**Disjointness decouples the proofs:** if the effect method takes neither data
slice and the data mutation never touches the log, the two `&mut` targets can't
perturb each other, so the data postcondition and the ordering postcondition prove
independently and conjoin.

```rust
fn invalidate_page(&mut self, asid: u16, va: u64)
    ensures self.tlb_log() == old(self).tlb_log().push((asid, va));   // append: load-bearing for order
fn barrier(&mut self)
    ensures self.tlb_log() == old(self).tlb_log();                    // fence frames the log
```

**Content predicates: `uninterp spec fn` + an `external_body` twin.** For a
predicate whose truth depends on out-of-scope machinery (a checksum, a heavyweight
decode), pair an `uninterp spec fn content_ok(rec) -> bool` with an
`external_body` exec twin `ensures r == content_ok(rec@)`. Verus then proves
*which* records are processed (structural / in-bounds / maximal-run) without
proving *what* each contains; the uninterpreted fn is never standalone — it always
carries its twin and a fuzz/proptest oracle.

**Workflow for a recursive teardown cluster.** Opaqueness hides recursion cycles
from the termination checker: keeping a mutually-recursive op `external_body` makes
every call into it a *contract application*, not a visible recursive edge — so
verify the rest of the cluster with plain loop measures and no `decreases`, then
flip `external_body` off the *entire* SCC in one final PR and add the lexicographic
measure (Part B §4) only then. Doing destructors piecemeal is unsound; defer the
whole cluster together. **Audit caution:** a contract whose `requires` is false for
every real input is **vacuous — a green proof of nothing** — and is the
higher-severity defect; the teeth/faithful/satisfying-fixture tests are what keep a
seam on the satisfiable side of that line.

## 12. Toolchain and syntax gotchas

A standing checklist of mechanical traps that block compilation or verification
with opaque errors:

- **`&mut` postcondition syntax** — `final(self)` / `old(self)`, never bare `self`
  in an `ensures` (detailed in §2, "Mut-ref postcondition syntax").
- **A local named `old`** shadows the `old(...)` keyword — rename it.
- **Cross-module spec/proof items: full-path them inside contracts, never
  `use`-import.** A `spec`/`proof fn` erases to nothing, so a module-top `use` of
  one becomes an unresolved import (`E0432`) — the import survives erasure but the
  item does not. Only real exec/struct/trait items may be `use`-imported (a
  spec-only trait whose ghost method a bound names needs `#[allow(unused_imports)]`).
- **A `matches`-with-`&&` as an operand of another binary operator** is rejected
  ("matches with && is currently not allowed on the right-hand side …") — wrap it:
  `A ==> (B matches Pat && C)`; the bindings stay in scope across the parenthesized
  chain.
- **A function's `requires` does not auto-instantiate inside a `while` loop** —
  restate any needed precondition as a loop invariant (diagnostic signature: the
  identical `assert` passes at the body's top and fails inside the loop).
- **Byte-char literals** (`b'E'`) are an "Unsupported constant type" — use the hex
  form `0x45u8`.
- **`CONST - 1` is `int` arithmetic in spec position**, so `!(CONST-1)` /
  `x & (CONST-1)` fail to type-check — define a separately-typed mask const
  (`pub const MASK: u64 = SIZE - 1;`).
- **Functional record update in spec:** express a single-field setter's `ensures`
  as `view().insert(k, View { field: …, ..old[k] })` (spec struct-update for the
  unchanged fields; `Seq::update` / `Map::insert` for indexed/keyed sub-state).
- **Gate unsupported constructs (`asm!`) out with `cfg`, not annotations** — code
  outside `verus!{}` is external by default under `cargo-verus`, so partial adoption
  needs no per-item `#[verifier::external]`.

---

*This guideline distills the technique; the dated source of record stays the
findings docs `../results/21…67_verus-findings.md` and the trusted-base ledger
`../results/68_verus-findings.md`. When a snippet here and the live code disagree,
the code is authoritative — this note is code-independent by design and is not
updated for every refactor.*
