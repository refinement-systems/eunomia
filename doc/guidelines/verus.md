# Verus

Verus is the kernel core's **deductive-verification tier**: it proves the `kcore`
object model and the rev2§4.7/rev2§4.8 host chokepoints meet functional `ensures`,
**terminate**, and preserve their `wf` invariants **for all inputs**, with no
bound to pick — the proofs hold over the unbounded input space. The enumerated
trusted base is the ledger `doc/guidelines/verus_trusted-base.md`. (The
Verus-rewrite plan and the dated 21…67 findings series this note distills are
historical and not retained in-tree.)

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
into a feature change.

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

**The gate counts verified items, not source.** The verifier's number is one per
`exec`/`proof`/`spec fn`, not per line, branch, or match arm. Extending an
already-verified function — a new match arm, a new enum variant, a moved or
tightened `ensures` — re-verifies the *same* item and leaves the count flat even
though real new logic was proven; only a genuinely new fn raises it. Predict the
count from how many new fns appear, not how much code changed, and never read a
flat count after an in-place edit as the obligation having been skipped. A
`verify = true` dependency is **re-verified transitively**: running the verifier
on a crate re-discharges every such dependency's obligations in the dependent's
run, so a proof extracted into a shared crate stays checked from each consumer —
but the dependent's count is its *own* obligations only; never sum it with the
dependency's (a plain-Rust wrapper over a verified core adds zero of its own
while its run still re-checks the core). Leaving `verify = true` on a crate with
zero proof code of its own is harmless (ghost erases) and keeps the gate armed,
so re-added verified code auto-gates.

`verus!{}` **erases to nothing**: ghost/spec/proof code compiles away, so
`cargo build` (host) and the aarch64 kernel cross-build link the *same* `exec`
code the proofs run against, and `vstd` compiles to nothing load-bearing. This is
the load-bearing guarantee — the verified core cross-compiles unchanged, and a
green proof is a statement about the shipped binary. Erasure cuts both ways: a
green `cargo build`/`cargo test` is *no* evidence the spec models track the
code — add a field to a struct the specs mirror and every sibling
`relabeled`/`reset`/`clear` spec and whole-value equality silently goes stale
while the non-verifier build and host tests stay clean, since they link only the
`exec` code the field doesn't break. The `cargo verus verify` run is the only
complete frame audit after such a change. `scratchpad` is the minimal
`spec fn` canary that the pin + install + cross-build still cohere independent of
any real crate.

**Scoped runs can false-green from stale cache.** Verus caches verification per
build, so a `--verify-function`/`--verify-only-module`-scoped run can exit 0 from
stale cache without re-verifying. The tell is a *missing* `verification results::`
line (cached) versus a present one (real run); treat a missing line as "not
actually re-verified." Only a clean-then-verify of the whole crate is
authoritative — exactly why the CI job above runs whole-crate, no-filter. This
bites hardest after a shared-predicate edit, where a scoped recheck of the edited
function alone reports nothing:

```sh
# False green: scoped re-run hits stale cache, no `verification results::` line.
cargo verus verify -p c --verify-function foo   # EXIT=0, but cached
cargo clean -p c && cargo verus verify -p c     # authoritative: results line present == real run
```

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
  the solver. Because of that transparency, an in-module frame or widening lemma
  over a closed spec can carry an *empty proof body* — the solver discharges it
  straight from the definition when the asserted equality holds definitionally
  (e.g. the computed value is independent of the parameter that changes); try the
  empty body before writing case analysis (a *recursive* closed spec is the
  exception — it does not auto-unfold at a symbolic argument; Part B §10). Reach
  for `closed` whenever a public operation's correctness is
  naturally stated in internal terms. (If the operation is single-crate, narrowing
  the consts to `pub(crate)` is the lighter escape.)

## The trusted base

Verus trusts a fact only through a named construct, and **every trust boundary is
enumerated once** in the ledger (`doc/guidelines/verus_trusted-base.md`) — the
source of truth for CLAUDE.md's "the trusted base is exactly …" claim. The
discipline, in one line: **`external_body`/`external` only at a genuine boundary,
each paired with a host test, and no bare `assume` survives.** The four legitimate
`external_body` categories and the host-test-with-teeth method are Part B
("Trusted seams"). An `external_body`/`external` row that cannot name **both** a
reason and a test is a finding, not a boundary.

## When Verus is not the tool

Verus is one tier of a wider verification scheme (rev2§6). When a problem is not
a deductive-proof obligation on an extracted function — a design-level state
machine, code-level concurrency interleavings, adversarial bytes, or a pure
policy over already-verified ops — the method that fits it and the reason are in
the dispatcher, `doc/guidelines/verification.md`.

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
`Seq<Seq<T>>` (which forces a `deep_view` and per-element bridge lemmas). The dual
caution: **a partition of a flat sequence is a monotone end-index list, never
nested subsequences.** To prove an algorithm splits a flat `Seq` into contiguous
blocks, describe the split with a strictly-increasing `Seq<int>` of cumulative
block ends over the original sequence — keep the data flat, the indices carry the
structure. Conservation ("no element dropped, duplicated, or reordered") is then
one back-recursive `subrange`-concat induction, one step per block, generic over
the element type — versus the `deep_view` and per-element bridge lemmas a
`Seq<Seq<T>>` representation forces.

```rust
// partition described by cumulative end-indices, not nested Seqs
spec fn flatten(items: Seq<T>, ends: Seq<int>) -> Seq<T>
    decreases ends.len()
{
    if ends.len() == 0 { Seq::empty() }
    else {
        let prev = if ends.len() == 1 { 0 } else { ends[ends.len() - 2] };
        flatten(items, ends.drop_last()) + items.subrange(prev, ends.last())
    }
}
// monotone ends ending at items.len()  ==>  flatten(items, ends) == items
// (induction via items.subrange(0,a) + items.subrange(a,b) =~= items.subrange(0,b))
```

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
- **A property-keyed frame is false on phantoms even when dom-guarded — state it
  contrapositively.** A frame whose antecedent tests a per-key *property*
  (`final[k].field != X ==> unchanged`) says nothing about out-of-domain phantom
  keys, whose values are arbitrary, so an un-guarded consuming `forall k` lemma
  can't discharge it — and dom-guarding the *value* (the bullet above) doesn't fix a
  *property* antecedent. Key the antecedent on the **change itself**
  (`final[k] != old[k] ==> <property held in old[k]>`), which is vacuously true
  off-domain where nothing moved. That tells a caller *whether* a key moved, not
  *how*; when a caller needs a specific field preserved, pair it with the dual
  **positive field-frame** keyed on the small set of fields the op actually writes
  (`final[k] == View { f: final[k].f, ..old[k] }`), which rides the op's own setter
  frames. For an in-place intrusive-list op that re-threads a neighbour, frame that
  neighbour as **"only its link field changed," never "state and link both free"** —
  a state-free frame lets the neighbour adversarially appear in any state and
  defeats every state-reading invariant downstream.
- **A loop havocs any state-wide view unless the invariant pins it.** A function
  framing `view_X` across a body containing a loop must restate `view_X ==
  old(view_X)` in the loop invariant even if the loop never touches it.
- **Prove a deferred tail flushed by forcing the cut on the last element.** A loop
  that accumulates an open segment (`start..i`) and emits it only when a cut
  condition fires gives a generic invariant no way to show the final segment was
  emitted. Make the cut condition *include* `i + 1 == n` (the last element always
  cuts) and carry `i == n ==> start == n`: the not-cut branch forces `i + 1 != n`,
  so reaching `i == n` is only possible through a cut that closed the tail. That one
  clause discharges both "the end-list is non-empty" and "its last entry equals `n`"
  at exit.
- **Anchor a new view's frame on the same-mutation-profile view, not the
  most-present one.** When a structure carries several views and you add one, place
  its frame clause beside the existing view with the *same* mutation profile. An
  immutable view (residency fixed at creation) frames-unchanged even across
  destructors; an object view mutated by its own destructor (which runs *during*
  teardown) must NOT be framed there — it belongs in the per-summand census-delta
  lemmas, where the immutable view never appears. The most-present view (touched by
  every setter) is the tempting sweep anchor and is wrong both directions:
  over-added to teardown contexts, under-added to the delta lemmas. Rule of thumb:
  the new view is framed iff its same-profile twin is; let each failing
  post/precondition drive the sweep to the next site to add or remove.
- **`ensures` is additive; `requires` is not.** Adding a postcondition only adds
  facts at call sites and can never break a caller — front-load frame clauses onto
  shared helpers freely. A new precondition *can* break callers; introduce one only
  behind a require-and-preserve invariant. Corollary: a property a caller must
  thread across a `&mut` call belongs in the **callee's `ensures`** (the
  intermediate state is not nameable across `&mut`), stated conditionally
  `P(old) ==> P(final)` so callers without `P` gain no obligation. A new `requires`
  on a *shared* primitive cascades through its **whole transitive-caller closure** —
  enumerate it before strengthening: grep every call site of the primitive *and of
  any thin wrapper around it*, since a low-level op is reached from hot/fast paths
  through wrappers, not only from teardown, so a closure traced through one arm
  undercounts the carriers. Most carriers only *frame* the new invariant across an
  unrelated step — discharge each with one reusable frame lemma; the real work
  concentrates in the few nodes that establish or consume it. To thread the clause
  across a mutually-recursive SCC, **edit every contract in the SCC first, then fix
  bodies in any order** — verification closes against contracts, so each body
  verifies against its callees' already-updated contracts and the cluster reaches
  green before the last body is reworked. When the invariant only becomes true once
  an op is made *faithful*, the invariant-add and the behaviour-change are atomic —
  land both in one green commit, not as separable phases.
- **Weakening an existing conditional `ensures` is free for every producer.** The
  dual of the additive rule: strengthening a clause's antecedent
  (`P(x) ==> Q(x)` becomes `P(x) && R(x) ==> Q(x)`) makes it fire for fewer states,
  so every site that already proved the broader clause discharges the narrower one
  automatically — the cheap way to relax a too-strong invariant clause across many
  producers at once. A shared lemma is the mirror case: re-establish only the
  property every caller needs and require only the precondition every caller can
  supply (the greatest-lower-bound across callers); over-strengthening its
  `requires` breaks the weakest caller.
- **Per-arm postconditions over error-erasing preconditions.** Never add a
  `requires` that rules out a real error path — it makes the path dead code and
  silently drops its guarantees. State per-arm posts on every return variant
  (`r is Ok ==> …`, `r == Err(BadArg) ==> *store == *old(store)`); the per-arm form
  proves strictly more and stays faithful.
- **Grow a state-wide view by grepping its predecessor's frame line.** When you add
  a ghost view every op leaves unchanged, find every existing `X.some_view() ==
  Y.some_view()` frame line and add a sibling equality beside each — the grep *is*
  the completeness checklist — and re-audit **every** module's op `ensures`, not
  just the ones your change touched; a sweep limited to a subset leaves other
  modules' no-op ops missing the clause, surfacing only when a cross-module caller
  threads the view and gets stuck on an obviously-true preservation. Frame the new
  view in `ensures`; do **not** add the view-equality to the `requires` of a framing
  lemma whose conclusion never reads it — that manufactures a spurious obligation at
  every call site (and can blow rlimit) while proving nothing.

**Align and label parallel `wf`-re-establishment proofs — a zero-cost clarity win.**
When a predicate has several named clauses and multiple lemmas each re-prove a subset
after different edits, give them one uniform shape and put a `// (clause-name)`
comment above each `assert forall` sub-block naming the clause it discharges; a lemma
touching only some clauses says so in its header and labels only those sub-blocks,
noting the rest as immediate. Comments and blank-line changes leave the SMT
obligation byte-identical, so this is provably free. Do **not** instead split a
closed `wf` into sub-predicates (`wf_b1`/`wf_b2`) merely to label its clauses — that
changes a closed spec's auto-unfold behaviour for no speed benefit. (Splitting the
*proof* per conjunct into lemmas is the right move when an obligation is over budget —
§10 — but that splits the proof, not the closed spec.)

```rust
// Re-establish pt_wf clause by clause; this edit writes a leaf PTE, so only
// (b2) inner→leaf and (c2) injectivity need re-proof; (a), (b1), (c1) immediate.
assert forall|..| .. implies /* (b2) */ .. by { /* ... */ }
assert forall|..| .. implies /* (c2) */ .. by { /* ... */ }
```

**Layer well-formedness.** Split a heavy invariant into a **structural fragment**
(first-order, total, ∀ — domain/link/consistency) and separately-layered
properties (acyclicity, full refcount soundness) that are harder to preserve;
compose `wf && acyclic && …`. Cheap ops verify against only what they need. Add a
clause **only when an op first consumes it**, not by front-loading. Acyclicity
does *not* compose for free: every constructing op must `ensure` the full
invariant, or some consumer's precondition is discharged only at the trusted
boundary — a hidden gap; audit the call graph.

**Parameterize `wf` over the dimension you expect to grow.** Make the size a
*parameter* of `wf` (∀ live element, its index `< size`), not a baked-in `const`:
growing the structure then reuses the existing walker/index/allocation specs
unchanged and verifies as a frame proof, where baking size in forces a model
rewrite. To frame a grow-only op (length up, existing entries fixed, fresh tail),
relate the differently-sized pre/post states with a monotone **widening lemma** —
the lookup/accept result is invariant under the larger size because the computed
value depends only on per-element data while size appears *solely* in an
out-of-range reject bound; a multi-level walk bridges each level with this one
lemma, no nonlinear/`bit_vector` reasoning. Where the spec is `closed` and
in-module, the widening lemma's proof body is often empty.

```rust
spec fn wf(m: View, size: nat) -> bool {
    forall|e| live(m, e) ==> index_of(m, e) < size   // size is a parameter, not a const
}
proof fn lemma_index_widen(m: View, e: Elem, old_size: nat, new_size: nat)
    requires resolves(m, e, old_size), old_size <= new_size,
    ensures  index_of_at(m, e, new_size) == index_of_at(m, e, old_size) { }
```

**Key a carry-through frame lemma on the fields the invariant reads, not on full
view-equality.** When an invariant reads only a subset of each element's fields,
prove its frame lemma with a precondition that constrains *only that subset* (and
the domain), letting every other field change freely; an op that rewrites an
unrelated field then carries the invariant through the cheaper lemma where a
full-`view`-equality frame would over-constrain and fail to apply. For one
invariant carried across many ops, build a small ladder of such lemmas with
progressively weaker preconditions (equal-views ⊂ edits-confined-to-inactive
⊂ read-fields-preserved); at each call site pick the strongest-precondition lemma
that still applies — it is the cheapest to discharge.

```rust
// inv reads only {state, priority, link}; an op rewriting `payload` keeps those:
proof fn lemma_inv_frame_fields(s0: S, s1: S)
    requires
        s1.view().dom() =~= s0.view().dom(),
        forall|x| #[trigger] s1.view()[x].state    == s0.view()[x].state
               && s1.view()[x].priority == s0.view()[x].priority
               && s1.view()[x].link     == s0.view()[x].link,
    ensures inv(s1) == inv(s0) {}
```

**Route a per-element fact through a covenant that already travels.** When a
downstream op needs a per-element fact about elements of a structure (a field
bound, "this state implies that field is `None`"), fold it into the structural
covenant the relevant predicate already quantifies over — the chain/completeness
invariant carried through every contract — rather than threading a fresh global
invariant through the whole op surface. Only the single op that *appends* an
element then needs a matching leaf precondition; every carrier gets the fact for
free. **The break-set of such a strengthening is exactly the *field-hypothesis*
frame lemmas** — those preserving the covenant via per-field equalities
(`qnext == old`, `state == old`); add the matching `field == old_field` hypothesis
there. Lemmas that preserve the covenant via whole-view equality are untouched,
which bounds the edit before you start.

```rust
spec fn chain_ok(s: S) -> bool {
    forall|i| 0 <= i < chain(s).len() ==> P(elem(s, chain(s)[i]))   // P folded into the traveling covenant
}
fn append(s: &mut S, e: Id) requires P(elem(*old(s), e)) ensures chain_ok(*s) { /* only the appender re-proves P */ }
```

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

**A frame over a spliced neighbour must expose post==pre for every
census/invariant-read field of the *changed* node.** An intrusive splice that
rewrites only one link field still changes its neighbour, and the census reads that
neighbour's *other* fields (the hold-refs it accounts for), so the op contract must
preserve them for changed nodes — free, since the op wrote only the one field.
State the equality as `final[x] == old[x]` (post==pre), not a bare pre-state
predicate (`old[x].state == P`): an all-keys frame lemma then discharges the
post-state property uniformly for every key, vacuously for phantom keys, with no
phantom/`complete` reasoning.

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

**Making a stub op faithful perturbs a *second* element — and `refs == 0` is an
unsound "dead" proxy when liveness is non-refcounted.** Replacing a stub mutator
with its faithful body often changes one element *besides* the target: an intrusive
tail-append/splice rewrites a neighbour's link (the old tail's `next`). A
single-key frame or census that assumed only the target moved
(`view == old.insert(t, …)`) then becomes false; it survives only via an invariant
placing that second element off every counted chain. When the second element's
liveness flows from a structure carrying *no* refcount (a scheduler/membership
list), it can be live yet `refs == 0` — so the `dead := refs == 0` proxy above is
*unsound*: a live-but-unreferenced node matches "dead" yet is legitimately mutated.
A `dead ==> frozen` invariant then needs a state-based disjunct in its antecedent
to exclude it — a *weakening* (stronger antecedent), so every existing producer
re-verifies for free.

```rust
spec fn dead_frozen(s0: S, s1: S) -> bool {
    forall|x| (s0.refs(x) == 0 && s0.node(x).wait is None
               && s0.node(x).state != State::Active)   // exclude live-but-refs-0
              ==> s1.node(x) == s0.node(x)
}
```

**Carve a transient liveness violation into an `inv_except(x)` predicate, promote
at class-exit.** When a mid-cascade op must violate a global liveness/completeness
invariant (a `forall y. live(y) ==> charted(y)`) for exactly one element `x`, do
not keep it a clean `wf` conjunct: split a weakened `inv_except(x)`, have the op
ensure only that, thread it across the cascade, and re-promote to full `inv`
precisely at the step that removes `x` from the invariant's quantified class. The
promotion lemma needs both "`x` is now outside the class" and the crux "`x` was
already absent from the structure the invariant quantifies over" (so that structure
is `x`-free and unchanged), the latter from the new class's `wf` covenant or the
structural-removal step. This is the liveness-invariant analogue of the
`dead := refs == 0` monotone rule above. At an internal control-flow join feeding
the promotion, weaken every branch's stronger post to the common predicate set the
merge needs *on each arm*, rather than relying on the join to find the common
denominator.

```rust
spec fn complete_except(s: S, x: Id) -> bool {
    forall|y| live(s, y) && y != x ==> charted(s, y)
}
proof fn lemma_promote_on_exit(s0: S, s1: S, t: Id)
    requires inv_complete_except(s0, t), only_changed(s0, s1, t),
             !is_active(s1, t), off_all_chains(s0, t)
    ensures inv_complete(s1) { /* chains are t-free and unchanged */ }
```

**Defer a field write past a loop so its callees never frame it.** If a flag/field
must end in a known state across a loop whose body never mentions it, do not set it
*before* the loop — that forces every inner callee to `ensure` it framed (an edit
you may be forbidden to make, or pay N times). Run the loop unchanged and write the
field once afterward from the post-loop state: the only write touching it is that
single edit, so the whole frame question reduces to one frame lemma over one write
and the callees stay byte-for-byte unchanged. Sound only because nothing observes
the field mid-loop; if a callee read it, deferring would change behaviour.

```rust
while budget_left { delete_one(store); }      // helpers never mention `marker`; unchanged
let finished = subtree_empty(store);
set_slot(store, root, Slot { marker: !finished, ..read(store, root) }); // one post-loop write; one frame lemma
```

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

**Upward walks measure on a shrinking visited-set, not on rank.** A descent along
child links decreases `rank[cur]`, but an *ancestor* walk along `parent` links
cannot — rank *increases* every step. Carry a ghost visited-set and `decreases
dom().difference(visited).len()`, discharged by `vstd::set_lib`'s purpose-built
`Set::lemma_set_insert_diff_decreases` (it requires `dom.contains(cur)`,
`!visited.contains(cur)`, `dom.finite()`). The distinctness premise
`!visited.contains(cur)` falls out of the structural invariant — every visited
node ranks strictly below `cur` — which is exactly what makes the difference
strictly shrink. This avoids the "finite nat-image has a max" detour a rank-bound
argument would otherwise need.

```rust
while let Some(cur) = node
    invariant dom.finite(), forall|x| visited.contains(x) ==> rank[x] < rank[cur],
    decreases dom.difference(visited).len()
{
    proof { Set::lemma_set_insert_diff_decreases(dom, visited, cur); }
    visited = visited.insert(cur);
    node = m[cur].parent;
}
```

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

**Discharge concrete-value and constant-nonlinear obligations by computation.** When
a recursive `spec fn` won't reduce at a concrete argument under default fuel, or Z3
refuses to simplify a product of static constants, run Verus's interpreter:
`assert(e) by (compute)` reduces `e` internally and hands the rest to Z3 (it also
assumes the original `e`, so it composes even when simplification is only partial),
while `assert(e) by (compute_only)` requires `e` to reduce all the way to `true` with
no reliance on solver heuristics — making it a *stability* tool, since a proof that
holds under `compute_only` does not depend on heuristics that may drift across
versions. The interpreter runs **in isolation**: outer `let` bindings and ambient
facts are not in scope, so move a known `let` inside the asserted expression
(`assert({ let x = 2; pow(2, x) == 4 }) by (compute_only)`) or fall back to
`by (compute)`. For a property over a generic value in a concrete range, prove it
over the whole range with `vstd::compute::RangeAll::all_spec` (over `int`, wrapped in
a closure) then apply the closure to the specific value to fire the quantifier. The
interpreter does not cache by argument value; annotate the rare
overlapping-subproblem `spec fn` (naive Fibonacci) with `#[verifier::memoize]`, and
note it is bounded by `--rlimit` and is itself recursive, so a deeply nested
expression can exhaust the process stack.

```rust
assert(pow(2, 8) == 256) by (compute);        // interpret, then hand the rest to Z3
assert(pow(2, 8) == 256) by (compute_only);   // must reduce fully to true; no Z3 heuristics
```

**For a transitive `==`/`<=`/`<` proved through a chain of rewrites, reach for
`calc!`.** Each intermediate expression is named once and its justification lives in
a per-step block whose context is restricted to that single step, so step proofs
cannot pollute each other or the surrounding context, which sees only the end-to-end
fact. Steps may carry a tighter intermediate relation (`x; (==) {} y; (<) {} z;`)
that the macro checks composes into the top-level one. Use `calc!` for an in-line
transitive rewrite chain where a lemma per step would be heavier than the proof
itself; use extracted `proof fn`s when a step is reused or genuinely heavy.

```rust
calc! {
    (<=)
    lo;     (==) { /* lo == max(a.min, b.min) */ }
    max_lo; (<)  { /* max_lo < mid by the guard   */ }
    mid;         { /* mid <= hi                    */ }
    hi;
}
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
bound alone does not discharge it. An `int` equality does not propagate through a
cast inside a quantifier: when a
`forall|i: int|` knows `i == target` but the lemma it feeds is stated over `i as
u32`, Verus will not silently equate `i as u32` with `target as u32` — assert the
cast equality on the matching branch (and the cast inequality on the other)
explicitly, or the lemma's conclusion never connects to the quantified clause:

```rust
assert forall|i: int| 0 <= i < N implies coherent(i) by {
    if i == target { assert(i as u32 == target as u32); }  // not inferred from i == target
    else           { assert(i as u32 != target as u32); }
}
```

When a std numeric method lacks a vstd spec,
supply a one-line `assume_specification` mirroring the documented semantics (a
trusted seam, Part B §11) — but check vstd first: it ships axioms for the
bit-scan intrinsics (`vstd::std_specs::bits::axiom_u32_leading_zeros` /
`axiom_u64_trailing_zeros`: `x == 0 <==> count == width`, the bit at `count` set,
the others clear), so `broadcast use` the axiom and reason from it, verifying a
scan with **no new seam**.

**Negative lesson:** never bound a wide pre-clamp intermediate by the *post-clamp*
type's max — that bound is false; clamping is what handles the excess.

**Spec a selection by membership + extremality, not existence.** For a function
returning the best element of a set — a max, a highest-common, an intersection
pick — `ensures Some(v) ==> in_set(v) && forall|w| in_set(w) ==> w <= v` (and
`None ==> forall|w| !in_set(w)`) is strictly stronger than a round-trip/existence
spec *and* pins the result uniquely, so no separate uniqueness lemma (cf. §2) is
needed — extremality is the witness's defining property. State `in_set` once as a
shared predicate over both inputs. This composes with the keep-total discipline
(§3): an extremal pick over decoded inputs needs no well-formedness `requires` — a
malformed input (an interval intersection computed `lo = max(lowers)`, `hi =
min(uppers)` with `lo > hi`) simply denotes the empty set and returns `None`
cleanly.

```rust
spec fn common(a: Range, b: Range, v: u8) -> bool {
    a.min <= v <= a.max && b.min <= v <= b.max
}
fn negotiate(a: Range, b: Range) -> (r: Option<u8>)
    ensures
        r matches Some(v) ==> common(a, b, v) && forall|w: u8| common(a, b, w) ==> w <= v,
        r is None ==> forall|w: u8| !common(a, b, w),
{
    let lo = if a.min >= b.min { a.min } else { b.min };
    let hi = if a.max <= b.max { a.max } else { b.max };   // malformed min>max ⇒ lo>hi ⇒ empty
    if lo <= hi { Some(hi) } else { None }
}
```

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

State a bit-identity lemma `by (bit_vector)` on its **signature** with an **empty
body**, listing every write direction as a plain *unconditional* `ensures` — never
carry a runtime selector (a `bool` flag) and prove each direction with a guarded
inline `assert ... by (bit_vector)`. Each direction is unconditionally true, so the
selector adds no proof value; one signature-level query collapses several guarded
sub-obligations into one, cheaper and clearer. The calling exec branch (`if cond {
word | bit } else { word & !bit }`) selects which ensured direction it needs, so the
lemmas stay unconditional. Migrating *to* this recipe from a selector/guarded-inline
shape is itself a measurable optimization (it can halve the worst lemma's `rlimit`
and drop the crate's obligation count, since the inline asserts were separate
obligations). For other-bits-untouched, prefer the **mask-equal** form `(x | (1<<k))
& (1<<m) == x & (1<<m)` over the weaker boolean-equivalence `((x | (1<<k)) & (1<<m)
!= 0) == (x & (1<<m) != 0)`: the mask equality propagates through the `& mask != 0`
test a call site runs and reads more directly.

**Extract a recurring inline bit identity into one shared `by (bit_vector)` lemma.**
Bit-vector goals are discharged by bit-blasting into a SAT query, so re-spelling the
same fixed-width identity inline at N call sites pays the full bit-blast N times;
citing one shared empty-bodied lemma bit-blasts it once and merely references the
result. Write the lemma's `ensures` byte-identical to the inline assert it replaces
so a call delivers exactly the fact the surrounding proof needs with no bridging
assert (the trailing `=~=` line that closes a codec proof stays at the call site
verbatim); put construction facts in `requires`, the clean shift/mask identities in
`ensures`, body empty. This both deduplicates and measurably speeds the callers —
inline `by (bit_vector)` sub-obligations inflate the caller's context, and the win
scales with the operand width and the number of inline asserts (a 64-bit
little-endian reader benefits far more than a 16-bit one). Collapsing M inline
asserts into K lemmas *lowers* the crate's verified-item count by M − K (each inline
assert is its own obligation; each lemma signature counts once), so estimates
expecting the count to rise are wrong on the sign.

```rust
// name a width's split/reassemble identities once; call them everywhere:
proof fn lemma_u64_le_bytes(v: u64, b0: u8, /* ... */ b7: u8) by (bit_vector)
    requires v == (b0 as u64) | ((b1 as u64) << 8) | /* ... */ | ((b7 as u64) << 56),
    ensures (v >> 0) as u8 == b0, /* ... */ (v >> 56) as u8 == b7;
// reader body: proof { lemma_u64_le_bytes(v, b0, /* ... */ b7); } assert(v =~= u64_le(v));
```

Call such a verified-only helper by its **full path from inside a `proof fn`**, not
via a top-level `use` (a named `use` of a `verus!{}`-only item is a real import that
survives erasure and breaks the plain `cargo build`; see §12).

**A bitwise operator cannot be a trigger.** `&`/`<<`/`|` are not valid trigger
terms, so a `forall|j| #![trigger x & (1<<j)] …` that quantifies a bit-scan over
symbolic positions with the masked-bit expression as its anchor is rejected when
that expression is the thing being driven. Two consequences. Prove the
per-position facts as small two-argument `by (bit_vector)` lemmas (a `set_bit_self`
and a `set_bit_other` for `j != k`) and **instantiate them per index** inside the
enclosing `assert forall`, reasoning through their contracts above the bit level —
the same per-element discipline the packed-bitmap recipe uses, now forced by the
trigger rule rather than only by scope. And when a *postcondition* `forall` ranges
over all lower/higher positions of a scan (`forall|j| j < bit ==> x & (1<<j) !=
0`), pin the binder with the masked-bit term `#![trigger x & (1<<j)]` so it fires
on the index expressions downstream code mentions — the `forall` annotation is
fine; only a *driving* trigger is rejected. When such a scan rests on vstd's
trailing/leading-zeros axioms, the axiom states bits in a `(!x >> k) & 1` form
while set/clear writes use `x & (1<<k)`; insert one tiny `by (bit_vector)` lemma
bridging the two equivalent forms — the only bespoke bit reasoning a
lowest-clear / highest-set scan needs.

```rust
proof fn set_bit_other(x: u64, k: u64, j: u64) by (bit_vector)
    requires k < 64, j < 64, j != k,
    ensures (x | (1u64 << k)) & (1u64 << j) == x & (1u64 << j);
// instantiate per index, not a forall-over-bits:
assert forall|j: u64| 0 <= j < 64 implies coherent(j) by { set_bit_other(x, k, j); }
// scan postcondition annotates (does not drive) the masked-bit trigger:
//   ensures forall|j: u64| #![trigger used & (1u64 << j)] j < bit ==> used & (1u64 << j) != 0
```

Push tight bounds into extractor contracts (`ensures r < 512` for a 9-bit field)
so every downstream index is in-bounds from the contract alone, and state
"by construction" security claims as ∀-theorems (`assert(forall|r| (r & ALLOWED) &
FORBIDDEN == 0) by (bit_vector)`) rather than per-site asserts. Don't over-pin:
align-down facts hold for a *symbolic* mask (`(x & !m) <= x`); pin literals only
for the genuinely stride-bound step. **Parser gotcha:** a bare `ident < ident`
misparses (the `<` reads as a turbofish, e.g. `int<…>` taken as generics) — and
not only inside an inline `... by
(bit_vector) requires …;`: `(expr as int) < other` mis-parses the same way in
ordinary quantifier and assertion bodies. Use a literal RHS, or flip the
typed/cast term to the right (`other > (expr as int)`).

```rust
// fails:  assert(forall|k: int| ends[k] as int < flags.len() ==> ...);
assert(forall|k: int| flags.len() > ends[k] as int ==> ...);   // flip the comparison
```

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

**A lossy decode admits totality only — route the round-trip one grain up.** The
two-direction bijection presumes a *lossless* single-unit re-encoder; a decoder
that lowers or discards information at its grain (resolving a key into a derived
value, dropping a field) has nothing to equate against, so its only statable
theorem there is *totality* (the no-panic body itself). That is a deliberate
scope boundary, not an omitted proof — state it as such so totality-only is not
misread as a gap. Put the canonical-form / round-trip oracle on the *enclosing*
grain that retains the information (the whole structure, not the lowering unit),
or route it to a proptest oracle. Keep the discarded fields in the verified
`Hash`-free image even when this unit can't round-trip them, so the enclosing
grain can still check them. Corollary on rejection order: when verification
*lifts* a validation check into an earlier layer, the error *variant* returned
for an input that fails multiple checks can change (the rejection order moved) —
add a distinct variant per rejection class the lift introduces, and confirm no
caller or test pins which variant fires for a multiply-malformed input.

**Control-flow rewrites.** Verus is unfriendly to `?` and to `match` guards (`PAT
if cond =>`); the erased behaviour is identical but the proof is direct only when
the rejection branch is syntactically present. Make the explicit `match … { None =>
return Err(..) }` a uniform convention.

**Evolving a verified codec is cheaper than building one — budget it that way.**
*Append* new fields after the existing layout and bump the length constant: the
pre-change prefix is byte-identical, so the existing per-field mask/shift
identities re-verify verbatim and only the appended fields carry new proof (carry
simple appended fields directly; close the widened bijection lemmas by `=~=`).
Adding a new arm to a tag-dispatched accept-iff decoder likewise enlarges the
decode body but introduces *no new obligation* — the discharge is still the
single accept-iff `ensures` re-proved over the larger `match`, provided the arm
reuses existing field-walk helpers rather than new spec/proof items. Plan
estimates that expect the verified-item count to climb per appended field or per
tag arm are usually wrong.

```rust
// LEN: 5 -> 7. Old 5-byte prefix unchanged; two new bytes appended.
out = encode_old_prefix(r.window);   // existing proof reused verbatim
out.push(r.lo); out.push(r.hi);      // appended fields close by =~=
// new arm, same `ensures r == spec_decode(buf@)`, bigger body:
match tag { /* ... */ 2 => decode_rename(buf, off), _ => Err(BadTag) }
```

## 9. Keep foreign types off the proof surface

A codec whose real types carry a cryptographic `Hash` (or any opaque,
collision-dependent value) cannot be verified directly: the hash has no
first-order SMT model, and an `external_type_specification` makes it *opaque* —
which blocks both reasoning and **construction** inside `verus!{}` ("constructor
for an opaque datatype"). Keep crypto entirely off the proof surface; three
reusable moves.

**Pass a just-mutated value into a loop-step lemma by value; do not reconstruct it in
spec context.** When lifting a loop step's proof into a separate lemma and the element
pushed/inserted carries a non-trivial owned field (`Vec`, `String`, …), take the
already-built value as a by-value parameter and state the post-state as `r ==
prev.push(new)`. Reconstructing it inside the lemma forces that heap-bearing type to
appear on the proof surface; passing it in keeps it off.

```rust
proof fn lemma_push_preserves_rec_ok(prev: Seq<RecMeta>, r: Seq<RecMeta>, new: RecMeta)
    requires r == prev.push(new),   // take the built value; its Vec field never enters spec context
    ensures forall|k| 0 <= k < r.len() ==> rec_ok(r, k)
{ /* ... */ }
```

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

SMT solver time grows **superlinearly** in the number of facts in scope: each added
fact multiplies the search paths rather than adding to them. So halving a query's
context can cut solver time by far more than half and can flip a timeout into a fast
success — which is why decomposition, not a bigger budget, is the first move for a
slow or timing-out obligation.

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
`proof fn` — and key it tightly: its `requires` are exactly the cheap, local
facts the op body already proves (the single edited chain, the per-element
frame, the one touched-object delta), its `ensures` is the heavy global
invariant; the op then proves only the local facts and calls the lemma. Recurse
by splitting the largest independent sub-sweep out of the spinoff lemma itself.
(2) mark it `#[verifier::spinoff_prover]` — Verus discharges it in a
*separate solver instance* with a fresh context, so the caller's term families and
triggers don't bloat its query (it suits a heavy existential-set frame carried
across a shift/index correspondence, and often closes a proof that only *looked*
like it needed more budget); (3) only then a private `#[verifier::rlimit(N)]` on
that one body. **Nonlocal cost:** adding a *field* to a widely-referenced ghost
view enlarges every SMT term mentioning it and can push an unrelated borderline
proof past budget — budget the isolation ladder whenever you grow a shared view.

**Walk `rlimit` back down after a context-shrinking change.** An
`#[verifier::rlimit(N)]` sized for a monolithic query is a misleading "this proof is
hard" signal once the work is decomposed, spun off, or otherwise made cheaper. After
any change that removes work from a body, re-tighten its budget to a small notch above
the new consumption — bisect to the smallest passing cap, then add a modest margin
(the resource unit is version-specific). A trimmed cap keeps the next regression
visible as a failure instead of silently consuming slack, and an honest budget
documents that the proof is no longer expensive. Removing the annotation entirely is
correct whenever the body now verifies at the default. Lowering a cap cannot change
the work a passing proof does, so the SMT cost is unaffected — `rlimit` *consumption*
is the deterministic work a fixed proof performs, independent of the cap, which is
only a ceiling.

```rust
// after decomposition removed the heavy inline step, restore an honest budget:
#[verifier::rlimit(10)]   // small honest cap: the lemma's draw sits just under it
proof fn lemma_unlink_merge(/* ... */) { /* ... */ }
```

To find a crate's true floors after the proofs stabilize, remove every budget at once
and run the crate cold: an over-tight function surfaces as a named `Resource limit
(rlimit) exceeded` error pinpointing it, and any function with no error verifies at
the default and should drop the annotation.

**Measure proof-perf by deterministic `rlimit`, not wall-clock milliseconds.** Verus
reproduces the exact same per-function `rlimit` (resource units) for identical source
across cold runs, while SMT milliseconds wobble run-to-run by double-digit percent. So
`rlimit` is the trustworthy signal for whether a refactor genuinely shrank a query: a
large `rlimit` drop is a real proof-size reduction even when the ms figures are flat
or noisy, and a small ms move with byte-identical `rlimit` is pure jitter that proves
no change. Read it from a per-function cold-run breakdown (`--time-expanded
--output-json`; clean the crate first per Part A, or a cached run reports no timing at
all). The protocol that makes a before/after comparison conclusive:

- **Run cold** so nothing is served from stale cache (a cached run can false-green
  with no `verification results::` line — Part A).
- **Keep controls.** Pick obligations the change does not touch; confirm their
  `rlimit` is byte-identical before and after. Byte-identical controls prove any
  measured delta is the change, not solver scheduling noise, and let you attribute a
  crate-total shift to specific functions. When a single before/after pair *looks*
  like a regression, controls plus a median over a few cold runs settle it.
- **A clear order-of-magnitude `rlimit` win on the targeted obligation is decisive on
  one cold run** — it dwarfs the noise band, so a median-of-three is unnecessary.
  Reserve repeated runs for borderline deltas inside the noise.
- **Charge the new lemma's cost against the op before claiming a win.** Compare
  crate-own figures (clean only the single crate so `vstd` stays cached); a full cold
  closure is dominated by `vstd` re-verifying and will mislead.

**Keep an optimization only if it measurably helps.** An extraction or refactor that
leaves the target obligation within noise must be reverted even though it verifies
cleanly and reads acceptably — a change that does not speed verification (and is not a
distinct clarity win) is not worth its added surface and module-wide ripple (below).

**Decomposition pays off in more shapes than the per-conjunct split — and the win is
often larger than intuition predicts.** The cause of a hot obligation is frequently
not the heavy inline step itself but the *host function's surrounding context* (loop
invariants, recursive-predicate term families, `choose` witnesses) poisoning the
query; the inlined step pays for everything in scope even though it references none of
it. Lifting that step into a `proof fn` keyed only on the facts it needs gives it a
fresh small context and can cut its `rlimit` far more than a "slice of the cost"
estimate suggests. Recurring profitable shapes, all instances of the same default fix:

- **A tag-dispatched `match` ladder** → a thin top-level dispatcher (tag guard +
  dispatch only) plus one named helper per arm, each with a self-contained round-trip
  contract; the spec side mirrors the exec side (exec twins each ensuring equality
  with their spec helper, bounds discharged at the call site from the tag read). Each
  arm then discharges against a small context instead of one query carrying every arm
  at once.
- **A multi-arm content branch inside a hot codec function** → extract the branch
  keyed tightly (`requires` = the cheap local facts the body proves, `ensures` = the
  heavy round-trip equality), and scope its axiom group *inside the helper* with
  `broadcast use` so the related facts land only in the small sub-query.
- **A heavy end-of-op `assert(P) by { ... }`** → its conclusion escapes but its proof
  still runs against the whole op's loop invariant, quantifiers, and framing residue;
  a `by {}` block does **not** shrink the inner query's context. Lift `P` into a lemma
  keyed on the few cheap shape facts it needs.
- **A per-iteration loop-step proof** (re-establishing an invariant after each
  `Vec::push`/insert) → extract it so the work is discharged once in a small context
  instead of re-derived against the whole loop query every iteration. This is worth
  doing for the `rlimit` headroom and the self-documenting contract even when
  wall-clock time is flat (the lemma absorbs roughly the work the hot obligation
  sheds).
- **A linear multi-phase op** (each phase = one edit + an inline frame re-proof) → one
  private frame lemma per phase. Shape each phase's edit description (e.g. a single
  `Map::insert`) to match, term-for-term, the trigger of the downstream composition
  lemma it feeds, so the edit-shape obligation collapses to one equality.
- **A repeated multi-lemma transitivity composition** (the same cluster composing
  per-edge frames `(a,b)+(b,c)` into `(a,c)` at many sites) → fold the cluster into
  one composite lemma whose `requires` is the union of the per-edge preconditions and
  `ensures` is the composed frames; each site makes a single tightly-keyed call. (This
  is the composition counterpart to the predicate-application framing below — name the
  frame, then compose the named frames.)

```rust
// the same per-key map-equality split costs far less in a fresh context:
proof fn lemma_unlink_merge(/* slot roles + splice steps as Map::insert eqs */)
    requires /* exactly the cheap local facts the op already proves */
    ensures  mfin =~= unlinked(m0, slot, last)
{ /* the former inline per-key split, now with no loop/recursion in scope */ }
```

**Deduplicating an identical inline proof block across sibling call sites is itself
both a clarity and a speed win.** When two (or more) functions carry a byte-for-byte
identical inline proof block, extract it into one named `proof fn` whose `requires`
are the cheap local facts each site already proves and whose `ensures` states the
property — the same tight-keying recipe that splits one heavy obligation. The named
lemma's tight context replaces a large inline block re-elaborated in every caller, and
its `ensures` carries the identical `#[trigger]` term the next-iteration invariant
needs, so nothing is re-keyed. (This is the *deduplication* use of the recipe; the
split use isolates one heavy obligation. The speed payoff scales with the inlined
block's original cost — see §13 for the bound.)

**Decompose a *linear* derivation into a pipeline of stage-lemmas.** The per-conjunct
split fits a goal that is a conjunction of independent facts; a straight-line
derivation `A → B → C`, where each step depends on the prior, fits a sequential split
instead. Stage 1's `requires` are the function's `requires` and its `ensures`
summarize its first block; each later stage's `requires` match the previous stage's
`ensures`; stage n's `ensures` are the function's. The body becomes a sequence of
stage calls threading the intermediate values forward. Factor each repeated boundary
predicate into a shared `spec fn` so the matching `ensures`/`requires` are written
once.

```rust
spec fn mid1(x: u64, y: int) -> bool { /* boundary predicate, written once */ }
proof fn part1(x: u64) -> (y: int) requires r(x) ensures mid1(x, y) { /* P1 */ }
proof fn part2(x: u64, y: int) requires mid1(x, y) ensures e(x) { /* P2 */ }
proof fn whole(x: u64) requires r(x) ensures e(x) { let y = part1(x); part2(x, y); }
```

**After extracting any subproof, prune its hint steps.** Intermediate `assert`s and
helper calls that were forced by the host's bloated context are often unnecessary once
the lemma's context is just its `requires`, parameters, and `ensures`. Treat
extraction as lift-then-prune: move the block, delete its now-redundant annotation,
and re-verify to find the minimal proof. (The deleted hints are not wrong — they
compensated for context size, so they return if the lemma is re-inlined.)

**When decomposition backfires — four bounded failure modes.** Extraction is the
default fix, but it is *not* unconditional. Measure the target obligation *and the
crate total* before keeping any of these:

- **The caller's context must already be small.** A tightly-keyed lemma that halves
  one op's `rlimit` can *regress* a structurally-similar sibling whose context still
  carries extra live term families (e.g. a wake path that ran a queue manipulation,
  leaving ready-queue terms live): discharging the lemma's `requires` against the
  larger context, then firing its `ensures` across it, costs more than the inline
  derivation saved. Before reusing a winning extraction on a sibling, check whether
  that sibling's context is comparably narrow; if not, keep the inline form.
- **The inline block's intermediates may be load-bearing downstream.** If a block's
  inline `assert`s (single-field equalities, snapshot bridges, `m[dst] == ...`) are
  reused by the function's *later* obligations, extracting only the block's quantified
  conclusion strips them, forcing the later blocks to re-derive — so the function's
  own cost *rises*. Check whether the block's intermediates are consumed downstream;
  if so, leave it inline or have the lemma also `ensure` them.
- **Sharing one generic helper across callers with divergent post-conditions regresses
  the one whose need is stronger.** A helper whose `ensures` is weaker than a caller
  needs forces that caller to re-prove the gap as a single quantifier over *all*
  elements at once — work the inline loop paid cheaply, one isolated per-iteration
  fact at a time. Share scaffolding only when both callers genuinely need the *same*
  post-condition shape (identical per-iteration facts, not merely identical syntax).
  Otherwise keep the loop inline or give the helper the caller's exact post-condition.
  (Even a *single*-caller loop extraction earns a fresh-context speedup on its own —
  the dead end is the *sharing*, not the extraction.)
- **Wrapping the establishment of a quantified/existential predicate in `assert(P) by
  { ... }` when lemmas already produce `P`** forces the solver to re-derive the
  quantifier/witness as a standalone goal and then re-consume it — far costlier than
  letting the lemma outputs flow into the tail (re-deriving an existential witness is
  especially expensive). Likewise, scoping a function's *terminal* block with `assert
  ... by` yields nothing: there is no later obligation to shield. Reserve `assert ...
  by` scoping for *intermediate* heavy blocks whose facts would otherwise pollute
  later obligations.

When a tried extraction measurably regresses, revert to the inline form and leave a
short present-tense comment for the reason it stays inline (the context carries extra
term families the lemma would re-pay), so a future reader does not re-attempt the dead
end — and only where the inline form would otherwise look surprising, per the
project's comment discipline.

**Single-site extraction is rarely a clarity win, and adding a function ripples the
whole module.** Extracting a single-use block whose facts live only as local `let
ghost` bindings duplicates those bindings into the lemma's `requires` (the contract
restates the construction instead of hiding it), often netting more lines.
Extraction-for-clarity pays only for genuine multi-site deduplication. And inserting
*any* function — exec or proof — perturbs the term families the solver sees
module-wide, so neighbours' `rlimit` shift in both directions; the
directly-attributable per-function deltas can sum to less than the crate-total change.
Judge by the crate total, not just the touched functions.

**A named frame predicate can compose yet still cost more in heavy consumers.** §3's
named-predicate frames compose where index triggers don't — but folding an
already-composing inline frame behind an `open spec fn` predicate is not free, despite
the intuition that an open spec auto-unfolds to byte-identical terms. The cost is
asymmetric: where the predicate is *established* (the leaf op that proves the
conjuncts and folds them in) it is flat or cheaper, but where it is *consumed* — a
heavy caller that previously received the op's postcondition as N ground frame facts
now receives one predicate application Verus must auto-unfold inside its own
already-large, quantifier-dense query — it can roughly double that caller's proof.
Teardown/destructor paths thick with `wf`/reachability/census `forall`s are the worst
consumers. Folding the predicate into a *loop invariant* carries the same risk, since
the invariant is a consuming context re-evaluated each iteration. Measure the
consumers, not just the edited leaf, before keeping such a refactor. Two further
checks before extracting a shared frame helper at all: it pays only if the
**intersection** of what every site needs is most of each site's frame (when sites pin
overlapping-but-different view sets, the common core may be a small fraction and each
site still spells out the remainder); and where a per-view frame line is the
**grep-able completeness checklist** for a view-addition audit discipline (§3), keep
the lines spelled out inline — the explicit conjunction *is* the audit anchor, and a
predicate that lives in one module while siblings keep frames inline reads
inconsistently.

**`spinoff_prover` is redundant after a clean extraction.** Once a heavy step is
extracted into a self-contained lemma that is already a small isolated query,
additionally marking it `#[verifier::spinoff_prover]` buys nothing — a separate solver
instance helps only when the body still shares the caller's bloated context. Reach for
`spinoff_prover` when you *cannot* extract (the heavy reasoning is genuinely entangled
with the caller), not after extraction has already given it a fresh context.

**Triage a red obligation: is a callee lemma the cause?** When a proof error
looks unrelated to your change, check whether a lemma the function *calls* is
itself failing — a red callee lemma poisons the caller's later,
logically-unrelated obligations (the caller never gets the lemma's promised
facts), so an "unrelated" error often clears with no change to the caller's body
once the lemma is fixed. The companion to "an rlimit blowup hides a real gap":
before suspecting your own assertion, confirm every cited lemma is green.

**Unify a degenerate special case with the general one through a
position-parameterized lemma.** When an op has a head-pop special case and an
arbitrary-position-splice general case, parameterize the supporting lemma by
position and let both share it (`lemma_remove(s, 0)` for the pop) rather than
writing a special-case lemma plus a general one — the special case is literally
the general case's head branch. The dual of "split per conjunct": split the
*result-establishing* work by conjunct, but unify *position-varying* work into
one parameterized lemma each caller instantiates.

```rust
proof fn lemma_remove(s: S, k: int) ensures wf(remove(s, k)) { /* ... */ }
// pop calls lemma_remove(s, 0); splice calls lemma_remove(s, k). pop IS k == 0.
```

**`assert(F) by { P }` scopes a local proof's byproducts in place.** Only `F`
survives into the surrounding context; every other fact `P` introduces is discarded at
the closing brace, so a modest proof that drags in heavy or universally-quantified
lemma facts does not bloat the rest of the function (where they slow later, unrelated
obligations). Encoded to the solver, `lemma_A(); assert(F) by { lemma_B(); };
assert(G);` is roughly `(A && B ==> F) && (A ==> G)` — `B` is available only while
proving `F`. This is the lightweight middle rung between an inline proof (pollutes the
whole function) and a full extracted lemma (clean context but a signature plus
threaded `requires`): prefer `assert ... by` when the only goal is context isolation
and the proof is single-use; extract a lemma when the proof is reused or heavy enough
to warrant its own solver instance. (Reserve it for *intermediate* blocks — see the
dead-end note above: scoping a terminal block, or re-establishing facts that already
flow from lemma `ensures`, does not help.)

**`closed`/`open` and `opaque` are distinct tools — and `opaque` earns its keep only
on a *recursive* spec.** `closed`/`open` choose whether a `spec fn` body crosses a
*module* boundary (modularity, abstraction); `opaque` is purely a
verification-performance lever, hiding the body even inside its defining module (where
a `closed` body still unfolds transparently for the solver). Reserve it for a
**recursive** definition whose auto-unfolding floods an in-module query (the "Control
what enters the context" paragraph below), `reveal`-ing it only in the proof blocks
that need it. On a **non-recursive** spec it is typically net-negative: the body would
have unfolded to a small fixed term anyway, so hiding it only forces explicit
`reveal`s that cost more than they save — and a missed `reveal` fails verification at
every use site, a wide blast radius for no payoff.

**Control what enters the context.** Keep heavy definitions out of queries that
don't need them: make a recursive `spec fn` `closed`/`opaque` and `reveal` it only
where used — a `closed` recursive spec does *not* auto-unfold at a symbolic
argument like `(i+1) as nat`, so write a one-shot step lemma with
`reveal_with_fuel(acc, 2)`. Conversely, pull an axiom *group* in exactly where it
is needed with `broadcast use` (`vstd::slice::group_slice_axioms`;
`group_mul_is_commutative_and_distributive` inside an arithmetic helper) rather
than globally — the related facts land in one query without flooding the unrelated
ones.

**Find the over-firing quantifier with the quantifier profiler — don't guess from
source.** When a query times out or is slow and trigger over-firing is suspected, run
the prover's quantifier profiler instead of eyeballing the triggers: `--profile` for a
query that already fails, `--profile-all` (optionally `--verify-function fn`) for one
that verifies but is slow, combined with `--rlimit 1` so the prover stops early and
emits a small log. Read it by *two* numbers, not one: raw instantiation count, and a
**cost** metric that weights a quantifier by how many further expensive instantiations
its own provoke. The highest-cost quantifier is the trigger-loop source; a quantifier
with an equally high count but low cost is an innocent bystander that merely co-fires —
don't "fix" it. If every quantifier shows only a small count, quantifier
instantiation is probably not the bottleneck — look to a nonlinear/bit-blasted goal
(§5/§6) or a genuine logical gap (the "an rlimit blowup hides a real gap" path above).
This pairs with the per-function SMT-time profiling (`--time-expanded`, which ranks
*functions* by time): use that to find the worst function, then the quantifier
profiler to rank *quantifiers* within its query.

```sh
cargo verus verify -p crate -- --profile --rlimit 1                       # diagnose a timeout
cargo verus verify -p crate -- --profile-all --verify-function slow_fn    # profile a slow pass
```

**Trigger economy is the dominant scaling hazard.** Concrete traps:

- **A whole-aggregate trigger on a neighbour-relating `forall` self-perpetuates a
  matching loop.** When a `forall` quantifies over a sequence/map of tuples or structs
  but its body reads only certain fields, trigger on those projections (`#![trigger
  s@[k].0, s@[k].1]`), not on the whole element (`#![trigger s@[k]]`). A
  whole-aggregate trigger re-matches when the body relates a neighbour of the same
  shape (`s@[k+1]`): each instantiation produces a fresh term that re-matches the
  trigger, flooding the context. The risk is highest in
  sortedness/adjacency/monotonicity invariants where the body mentions both element `k`
  and `k±1`. A one-line projection trigger can roughly halve a crate's SMT time with no
  proof-body change. Keep the trigger shape **uniform across sibling conjuncts** of the
  same `wf`/invariant: a lone conjunct triggering on the whole aggregate where its
  siblings project is both the performance hazard and a clarity wart.
  ```rust
  // BAD: whole-tuple trigger re-matches every reintroduced sibling → matching loop
  //   forall|k: int| #![trigger s@[k]] 0 <= k < n-1 ==> s@[k].0 + s@[k].1 < s@[k+1].0
  // GOOD: project onto exactly the fields the body reads
  forall|k: int| #![trigger s@[k].0, s@[k].1] 0 <= k < n-1 ==> s@[k].0 + s@[k].1 < s@[k+1].0
  ```
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
- **A `forall x. P(x) ==> exists w. Q(x, w)` conjunct is unprovable on
  re-check** — re-proving the `forall` never re-surfaces the inner witness, so
  the stored fact never fires (annotating the `exists` binder doesn't help; the
  witness term still isn't in the re-proof's context). Eliminate the
  existential: define a deterministic selector `spec fn sel(x) -> W { choose|w|
  Q(x, w) }` and restate the conjunct `forall x. P(x) ==> Q(x, sel(x))`. The
  body now contains the selector term — a real trigger — so it re-proves with no
  witness-surfacing `by` block, and it is strictly stronger (it names the
  canonical witness instead of asserting mere existence).
  ```rust
  spec fn sel(m: M, i: int) -> Seq<Id> { choose|s| chain_at(m, i, s) }
  spec fn wf(m: M) -> bool {
      forall|i| valid(i) ==> chain_at(m, i, sel(m, i))   // sel(m,i) is the trigger
  }
  ```
- **A helper `assert forall` must mirror the target conjunct's range and
  trigger *verbatim*.** Proving a `wf` conjunct with an equivalent-looking range
  (`< N` vs `< N as int`) or a different binder/trigger shape silently fails to
  discharge it — no error, just an unmet obligation. Copy the conjunct's range,
  binder, and `#[trigger]` exactly.
  ```rust
  // the asserted forall must match the conjunct's exact range/trigger:
  //   conjunct: forall|level: int| 0 <= level < N as int ==> P(level)
  assert forall|level: int| 0 <= level < N as int implies P(level) by { /* ... */ };
  ```
- **Silence a persistent low-confidence auto-trigger note by naming the trigger Verus
  already infers.** When the prover prints `automatically chose triggers … low
  confidence` on a quantifier every run, read which term it reports selecting and
  annotate the binder with that exact term (`#![trigger self.free@[k].0]`). This
  documents the trigger already in use, adds no new term, and leaves the SMT work
  unchanged — a pure readability win removing recurring noise. (Distinct from the case
  where auto-inference *fails* and you must pick a *different* trigger to make the proof
  go through; here the inferred trigger is correct and you are only making it explicit.)
- **Match a helper `assert forall`'s range to the supporting per-index lemma's
  `requires`, not only to the target conjunct.** When a frame is discharged per index
  from an ambient fact that holds only over valid positions, bound the quantifier to
  exactly that valid range (`0 <= idx < depth`); an unbounded `forall` fails the inner
  lemma's precondition with a precondition-not-satisfied error rather than a trigger
  miss.

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

**Reach for the decomposed shape from the start, not as a rescue.** The
per-step/per-conjunct discipline is the idiomatic *starting* structure for any
multi-step structural operation, not only a fix for a monolith that timed out:

- **Establish per-iteration / per-phase frame lemmas up front** for any structural
  teardown, rebuild, or relabel — mirroring how a loop-based destroy already calls a
  per-iteration transitivity frame lemma. A linear unrolled sequence should reuse the
  same per-step frame discipline a loop version would, so each step is a stated,
  independently-checkable unit. Triage which phases earn a lemma: extract single-setter
  drop-ins with a clear shared composition lemma first; *defer* phases that branch into
  several distinct exec edit shapes (a design spike, not a mechanical relocation) and
  phases whose inline compositions carry no measurable cost.
- **Standardize one split idiom per file** — a thin dispatcher plus named per-case
  helpers, each `spec fn` paired with an exec twin that `ensures` equality with it —
  and model a new extraction on an existing sibling that already verifies cheaply in
  isolation. Per-case field layouts and contracts then live in each helper's
  doc-comment instead of one overview block. Grow a coherent named-lemma *family*
  around one operation (one lemma per invariant conjunct / sub-step), and for symmetric
  add/remove or producer/consumer ops write **mirror-pair** lemmas with identical
  off-key framing and only the delta flipped (`lemma_enqueue_census` `+1` vs
  `lemma_dequeue_census` `-1`; a "drop-head/shrink-window" set vs a
  "push-tail/grow-window" set) — the symmetric contracts read as obvious duals.

**Decomposition is a clarity win even when it adds lines.** Replacing a long inline
derivation with "do the edit, call the named phase lemma" turns an opaque block into a
sequence of independently-checkable units with explicit contracts; the extra lines are
the lemma `requires`/`ensures`, which *document* the obligation rather than obscure it.
The clarity metric is whether each step becomes a named contract, not the raw line
delta.

**Predict the verified-count delta as a correctness check on a decomposition.** Each
new `proof fn` adds exactly one to the verified count; a non-recursive `spec fn` adds
zero (it carries no obligation); each new exec fn with `ensures` adds one. So
extracting three `match` arms into bare spec helpers plus three exec twins raises the
count by exactly three. A delta matching the lemmas added (and nothing else) is
positive evidence the change did only what was intended and added no hidden seam.
(Verus also counts each `while`-loop as an obligation, so replacing two inline loops
with one helper containing one loop can leave the tally flat — read the *presence* of
the `verification results::` line, not the count, as proof of a real run.)

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

**A trusted mutation may have no exec counterpart — the deliverable is then a
lemma.** When the trusted shell reconstructs state from raw fields on every call (a
slice rebuilt from a length field, a view rebuilt from a count), the mutation does
no runtime work for the verified core to perform — there is nothing to put in an
exec stub. The whole verified deliverable is a **preservation lemma** (`wf`
preserved, observations unchanged across the trusted change), gated in the proof
count identically to an exec op but with no exec body. Don't force an exec wrapper
to satisfy a "the shell calls the verified op" plan when the honest shape is a
`proof fn`.

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

**A plain-`unsafe` wrapper around a verified algorithm is a seam too — Miri is its
oracle.** When a verified *pure* function (allocation/offset arithmetic, proven
∀-inputs) is consumed by a thin `unsafe` wrapper that actually touches raw memory
(`UnsafeCell`, `base.add(off)`), the wrapper carries no `external_body`/`assume`
yet is trusted: Verus proves the arithmetic, never the pointer write. Discharge its
no-UB across sampled op-sequences with a randomized `alloc`/`dealloc`/`realloc`
proptest run under `cargo +nightly miri test` as the UB oracle. Give it teeth by
*transiently* skewing the load-bearing offset (`add(off)` → `add(0)`) and
confirming the suite trips — Miri flags the OOB/aliasing write, and even a normal
build trips std's own debug precondition (`copy_nonoverlapping`'s overlap check);
document the broken variant, never commit it. **Size the oracle to the
carved/owned extent, not the request:** if the implementation rounds a request up
(alignment, a minimum granule), fill and re-check the whole rounded extent
(`need = size.next_multiple_of(GRAN)`) — two real extents can overlap while their
request-sized prefixes miss, so a `size`-based check is blind to the bug.

```rust
// verified: pure place(size, align) -> Option<usize>  (in-arena, aligned, disjoint)
unsafe fn alloc(&self, size: usize, align: usize) -> *mut u8 {
    match self.place(size, align) { Some(off) => self.base.add(off), None => null_mut() }
}
// oracle: proptest a random alloc/dealloc/realloc stream under `cargo +nightly miri test`;
//   write a unique pattern through each pointer, re-read after every op.
// teeth (documented, never committed): skew add(off) -> add(0); Miri reports the OOB write.
// carve-sized: check `need = size.next_multiple_of(GRAN)` bytes, not `size`.
```

**`external_body` carries no `requires` obligation — and honesty beats strength.**
Only the declared `ensures` crosses the boundary, so a verified caller can invoke a
trusted op with minimal facts (the lever for staged verification). For an op whose
real effects are entangled (a teardown releasing per-element refs with no closed
form), **assume only the robustly-true checkable core** (wf preserved, domain
fixed, specific slots cleared, untouched fields stated unchanged) and let a host
differential test cover the rest — a *false strong* clause is worse than an *honest
narrow* one.

**The inverse leak: an unverified wrapper assumes every `requires` it doesn't
discharge.** When a verified type is wrapped by a public API *outside* `verus!{}`,
the wrapper gets no compile-time obligation to discharge the verified ops'
preconditions — so the crate verifies green while its real entry point is unsound.
This is the mirror of the rule above: an unverified caller of a verified op
*assumes* that op's `requires` at the call site, exactly as a verified caller
assumes a trusted op's `ensures`. That assumption is sound **only** when the caller
is an enumerated trusted-base seam — then the verified `requires` becomes a
trusted-base obligation re-asserted at that boundary, not a proven fact. Audit
every out-of-macro wrapper over a verified core: each `requires` (`nfree < N`,
`off + n <= len`, buffer-belongs-to-this-pool) must be re-established by the
wrapper, and a runtime guard demoted into a `requires` (category (3)) needs a
runtime backstop there or it is simply gone.

```rust
// inner: verified, sound under its requires
fn free(&mut self, off: usize, n: usize)
    requires self.nfree() < N, off + n <= self.len()
{ /* ... */ }

// outer: OUTSIDE verus!{} — no obligation to discharge the above
pub fn release(&mut self, b: Buf) {
    // BUG: nfree < N and bounds never re-checked here; the `requires` is vacated.
    self.inner.free(b.off, b.len);
}
```

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

**Make the oracle itself correct.** The traps above keep the *mock* honest; three
more keep the *oracle* honest:

- **The oracle is an *independent* recomputation, never the op rerun.** When
  production already runs the verified op, a test that calls the op again proves
  nothing — circular. Recompute the structural predicate *from raw state* (walk the
  links yourself) and check the op's result against it.
- **Mirror the op's *exact* postcondition, even when partial.** An op that
  preserves only a weakened `X_except(t)` (full `X` minus the slot it is
  mid-restoring) must be oracled by that exact predicate — assert `wf_exec` plus the
  precise narrow effect ("`t` off every chain"), not full `X`. Asserting the full
  invariant after a partial restore makes the oracle *wrong for valid post-states*.
- **Bound any chain-walking mirror against a cyclic fixture.** An executable walk of
  a linked chain loops forever on a malformed cycle. Cap it by `nodes.len() + 1` so
  a sound structure terminates and a cyclic/over-long one is a detectable rejection
  — the executable counterpart of the proof-side cycle guard (§4).

```rust
fn chain_ids(store: &Store, head: Option<Id>) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = head;
    let cap = store.nodes.len() + 1;                 // cycle guard: sound ⇒ terminates
    while let Some(id) = cur {
        if out.len() >= cap { panic!("malformed cycle"); }
        out.push(id.0);
        cur = store.nodes[&id].next;
    }
    out
}
```

**Route production and mock through one verified op.** Where a seam has a verified
op, realize it at a single delegation point that *both* the production handle and
the host-test mock call into — the mock is then not a parallel reimplementation that
can drift, and the differential test exercises exactly the path production runs.
This is the strong form of "the mirror must be faithful": don't hand-write the
mock's body to match the contract — call the op.

```rust
impl Seam for Store     { fn op(&mut self, x: Id) { verified::op(&mut self.0, x); } }
impl Seam for ArrayMock { fn op(&mut self, x: Id) { verified::op(self, x); } }
```

**The teeth must reach the subject, not just the oracle.** A `_has_teeth` test that
rejects a tampered *expectation* exercises only the oracle's own logic. The
end-to-end check is to transiently inject a one-byte mutation into the *real* path
(`data[0] ^= 1;`, an off-by-one in the production fn), confirm the property/golden
test goes RED *and shrinks to a minimal case*, then revert — and to flip the
negative control's own assertion (`assert_ne!`→`assert_eq!`) and confirm it fails.
Keep a deliberately-wrong alternate model wired as a *standing* test asserting the
real impl **diverges** from it (`assert_ne!(real, broken_model)`) — the strongest
anti-theater control, since substituting the broken model into the equivalence
proptest *would* fail it. A reference oracle is real only if it is **independently
derived** (an inverse formula written from scratch, a brute-force scan of the input
domain, anchored to external goldens — never a refactor of the forward code, or the
agreement check is a tautology), **lazy-matched** (read/materialize at observation
time when the impl is lazy; an eager model materializes a base the lazy impl never
touches and silently diverges), and **id-paired** (compare observable outputs under
an explicit real-id↔model-id pairing; the two need not allocate the same opaque
ids). And the fixture must start satisfying the *exact* invariant the test asserts
(`refs == census`), not a plausible stand-in (`refs = 1` with no matching census
term) — a phantom fixture makes the soundness assertion fail at entry, so the test
errors or silently skips.

**Two teeth-traps beyond the seam mocks.** A runtime-only guard (category 3) bodied
with `debug_assert!`/`panic!` compiles out under `--release`, so one
`#[should_panic]` test passes by *never panicking* there — vacuous. Cfg-split it: a
`#[cfg(debug_assertions)] #[should_panic]` case proving the witness fires, and a
`#[cfg(not(debug_assertions))]` case proving the release fallback path runs without
aborting. And when proptesting a verified function whose contract states a
*characterizing* property, assert that property directly — never compare the output
to a second call of the same algorithm (a tautology that catches nothing); if the
proof's oracle is a *ghost* spec fn uncallable from exec, re-derive the same notion
independently in plain Rust (a brute-force scan, different arithmetic).

```rust
#[cfg(debug_assertions)] #[test] #[should_panic]
fn guard_fires_in_debug() { drive_to_violation(); }
#[cfg(not(debug_assertions))] #[test]
fn guard_falls_back_in_release() { assert!(drive_to_violation().still_serving()); }
// characterize, don't re-run the impl:
let b = lowest_clear_bit(used).unwrap();
assert!(used & (1 << b) == 0);
for j in 0..b { assert!(used & (1 << j) != 0); }   // it is the *lowest* clear bit
```

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
carries its twin and a fuzz/proptest oracle. **Determinism and injectivity come
free over the seam:** when the whole pipeline is pure `spec`-modeled functions
composed over a total, deterministic seam, "equal inputs produce identical output"
holds *definitionally* — the output is a pure function of the input routed only
through the seam. Write no determinism or injectivity lemma, and reach for no
injective-on-small-inputs hash ghost: trust nothing beyond the seam's totality and
let the structural `ensures` carry the rest.

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

**A proven lemma is load-bearing only if real code reaches it *and* discharges its
`requires`.** A `proof fn` with zero call sites, or whose hypothesis is a
"documented invariant" established at no site Verus sees, is *dead* — it proves a
true, sound theorem the shipped binary never relies on. This is the satisfiable
sibling of the vacuous contract above: vacuity is a `requires` false for every
input (true-of-nothing); a dead lemma is true-of-everything-but-uncalled. Audit
both — a lemma counts only when some `ensures` reachable from a real op invokes it
and each of its `requires` is discharged by code, not merely asserted in a comment.

**Thread a clock as an explicit `now: u64`, never a clock seam.** Where the
verified/tested core needs the time, take it as a `now: u64` parameter at every
entry point rather than embedding an internal clock or injecting a synthetic-clock
trait. Real callers pass the wall-clock read; proofs and tests pass plain integers.
The result is deterministic and Miri-safe by construction (no wall-clock sleeps, no
trait plumbing), and the "injectable clock" a test needs is just the parameter.

## 12. Toolchain and syntax gotchas

A standing checklist of mechanical traps that block compilation or verification
with opaque errors:

- **`&mut` postcondition syntax** — `final(self)` / `old(self)`, never bare `self`
  in an `ensures` (detailed in §2, "Mut-ref postcondition syntax").
- **A local named `old`** shadows the `old(...)` keyword — rename it.
- **A `use vstd::prelude::*` glob brings short type names into scope** (`real`, …);
  a local of the same name collides, surfacing as a type/binding clash far from the
  import. Rename the local, not the import.
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

## 13. Proof-performance tuning: a decision map

This section maps §§5–12 for tuning proof performance: which techniques reliably pay,
which are bounded dead ends, what the idiomatic starting shape already gets right, and
which tools to keep in mind. It cross-references the detail rather than repeating it.
One rule underpins everything: **judge by deterministic `rlimit`, on cold runs,
against byte-identical controls, and keep a change only if it measurably helps** (§10).

**Techniques that carry the wins.** Almost every real reduction comes from *shrinking
a solver query's context*, because SMT cost grows superlinearly in the facts in scope
(§10):

- **Decomposition into tightly-keyed lemmas** — `requires` = the cheap local facts the
  op proves, `ensures` = the heavy result. It pays across many shapes: tag-dispatch
  `match` ladders, multi-arm codec branches, heavy end-of-op `by {}` blocks (whose
  context is *not* shrunk by the brace), per-iteration loop steps, linear multi-phase
  ops, and repeated transitivity clusters folded into one composite lemma (§10). The
  realized win often exceeds intuition, because the *host context*, not the inlined
  step, is usually the cost driver. Sequential derivations decompose into a *pipeline*
  of stage-lemmas; after any extraction, **prune** the hint steps the bloated context
  had forced (§10).
- **Deduplicating an identical inline block** across sibling sites into one shared
  lemma — both clarity and speed (§10).
- **Extracting a recurring `by (bit_vector)` identity** into one empty-bodied
  signature-level lemma, cited everywhere: the identity bit-blasts once instead of per
  site, and the win scales with operand width and the number of inline asserts (§6).
- **`bit_vector` recipe migration** — unconditional both-direction `ensures`, no
  runtime selector, mask-equal form (§6).
- **Right-sizing `rlimit` down** after a context-shrinking change, and silencing the
  low-confidence auto-trigger note by naming the inferred trigger — honesty/clarity
  wins that are SMT-neutral (§10).
- **Projection triggers over whole-aggregate triggers**, which break a
  self-perpetuating matching loop and can roughly halve a crate's SMT time with a
  one-line change (§10).

**Dead ends, and the context that bounds each.** A technique that misses in one
setting is not useless in general — each fails for a stateable structural reason and
still pays in the opposite situation (all §10 unless noted):

- **Extraction regresses** when the caller's context is still large (the lemma's
  `requires`/`ensures` pay against the big context), when the inline block's
  intermediates are load-bearing for later obligations (they vanish from the lemma's
  `ensures`), or when a *shared* helper's weaker post-condition forces a strong-needs
  caller to re-prove the gap as one whole-quantifier. It still wins where the context
  is small, the block is a self-contained dead end, and both callers need the same
  post-condition shape.
- **`assert(P) by { ... }` around a quantified/existential establishment** that lemmas
  already produce, or around a *terminal* block, costs more than it saves; the scoping
  idiom helps only for *intermediate* heavy blocks that pollute later obligations.
- **A named `open spec fn` frame predicate** can compose yet still ~double a heavy
  *consuming* caller's proof (the establish-vs-consume asymmetry); it pays where
  consumers are small/few, where the shared intersection is most of each site's frame,
  and where the inline frame lines are not themselves a grep-able audit checklist (§3).
- **`opaque` on a *non-recursive* spec** is typically net-negative (the body would
  unfold to a small fixed term anyway); it earns its keep only on a recursive
  definition whose auto-unfolding floods an in-module query (§10).
- **`spinoff_prover` after a clean extraction** is redundant — it is the tool for
  heavy steps you *cannot* extract.
- **Extraction of an already-trivial block** is clarity-only, never a speed lever; the
  payoff scales with the inlined block's original cost. **Single-site extraction onto
  local `let ghost` bindings** is usually a clarity *loss* (the `requires` restate the
  construction). **Splitting a closed `wf` into sub-predicates** to label clauses
  changes auto-unfold for no benefit — use clause-naming comments (§3).
- Adding *any* function ripples the whole module's `rlimit`; judge by the crate total.

**Patterns the idiomatic starting shape already gets right.** Reach for these from the
start, not as a rescue: per-step/per-phase frame lemmas for every multi-step structural
op; one standardized split idiom per file (thin dispatcher + named per-case helpers,
`spec` paired with an exec twin); coherent named-lemma families with mirror-pair
contracts for symmetric ops; aligned, clause-labeled parallel `wf`-re-establishment
proofs; passing a just-mutated heap-bearing value into a loop-step lemma *by value* to
keep it off the proof surface (§9); and predicting the verified-count delta as a
correctness check (§10). Decomposition is a clarity win even when it adds lines — each
step becomes a named contract.

**Tools to keep in mind.** Reach for these when the situation calls for it, though the
patterns above cover the common cases: the **quantifier profiler**
(`--profile`/`--profile-all` + `--rlimit 1`) to locate an over-firing quantifier by
*cost*, not guesswork — paired with `--time-expanded`, which ranks functions while the
quantifier profiler ranks quantifiers within the worst one (§10); **`assert(F) by { P
}`** to scope a local proof's byproducts without a full lemma (§10); **`by
(compute)`/`by (compute_only)`** for concrete-value and constant-nonlinear obligations,
with `compute_only` as a heuristic-independence stability check (§5); **`calc!`** for a
transitive rewrite chain with per-step localized contexts (§5); and **`opaque` vs
`closed`** chosen by whether you need in-module hiding (performance, recursive specs)
or only cross-module hiding (abstraction) (§10).

---

*This guideline distills the technique; the enumerated source of record is the
trusted-base ledger `doc/guidelines/verus_trusted-base.md` (the dated 21…67
findings series it distills is historical, not retained in-tree). When a snippet
here and the live code disagree, the code is authoritative — this note is
code-independent by design and is not
updated for every refactor.*
