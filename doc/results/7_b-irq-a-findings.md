# B-IRQ-A findings — the verified kcore IRQ object

Implementation notes from B-IRQ-A (`doc/plans/11_birq-detail.md`): `CapKind::Irq` + the `IrqObj`
(the timer object's **census twin**, minus the armed list) + the `irq_binding_refs` census term +
verified `irq_bind`/`irq_unbind`/`destroy_irq`. Gate moved **384 → 389** (`cargo verus verify -p
kcore`), `cargo test -p kcore` 105 → 108, kernel builds, QEMU boots to the shell. Behaviour-preserving:
no runtime IRQ path is wired (that is B-IRQ-B); B-IRQ-A is purely the verified core + the Store seam.

The new ops were cheap (as predicted); the cost was the **central `obj_census` perturbation**. The
findings below are mostly about *that* — they generalize to any future census-summand addition.

---

## 1. The load-bearing insight: a new object view's frame belongs *exactly where `timer_view`'s does* — never key it off `cspace_view`

`obj_census` reads five object views (slot, chan, notif, tcb, timer); B-IRQ adds `irq_view` as the
sixth. The mechanical work is threading `irq_view`-unchanged through every place a census proof needs
it. The tempting shortcut — "add an `irq_view` frame clause next to every existing `cspace_view` frame
clause" (sweep keyed on `cspace_view`) — is **wrong in both directions**, because the two views have
*opposite mutation profiles*:

- **`cspace_view` is immutable** — residency is fixed at construction, so it is framed unchanged
  *even across destructors* (`delete`/`obj_unref`/`destroy_cspace` all preserve it). It appears in
  teardown ensures, teardown loop invariants, and the `home_views_frozen` provenance predicate.
- **`irq_view` is an object view** — it changes in exactly its own ops (`irq_bind`/`irq_unbind`/
  `destroy_irq`), and `destroy_irq` runs *during teardown* (when an `Irq` cap is the target of
  `delete`/`obj_unref`). So `irq_view` is **not** framed across destructors — exactly like `timer_view`,
  which `destroy_timer` mutates.

Consequence: the `cspace_view`-anchored sweep **over-added** `irq_view`-unchanged to every teardown
context (where `cspace_view` is framed but object views are not) and **under-added** it to the
census-delta lemmas (which frame `timer_view`/`notif_view` but *not* `cspace_view`). Both classes then
showed up as verify failures. The correct mental model is simply:

> **`irq_view` is framed iff `timer_view` is framed.** Add the `irq_view` clause beside every
> `timer_view`-unchanged clause; never beside a `cspace_view` clause that lacks a `timer_view` sibling.

In hindsight, anchoring the sweep on `timer_view` (the object-view twin) instead of `cspace_view` would
have been correct in one pass. The `cspace_view` anchor was attractive because it is present in *more*
places (every setter), but "more places" was the bug — those extra places are precisely the teardown
contexts where an object view must *not* be framed.

The teardown ops that had to have the wrong `irq_view` frame **removed** (they can run `destroy_irq`):
`obj_unref` (ensures), `delete` (ensures), `destroy_cspace` (ensures + resident loop invariant),
`destroy_channel` (ensures + every ring/peer loop invariant), `destroy_tcb` (ensures), `unref_cspace`
(ensures), and the `home_views_frozen` *predicate* itself. Each instead frames `irq_view` only
**conditionally** — in the `cap_obj(cap) is None ==> {…}` and `cap_notif(cap) is Some ==> {…}` blocks
(dropping a non-designating or notification cap changes no object view), exactly mirroring `timer_view`.

## 2. No list ⇒ the ops are single-key edits ⇒ one lemma covers bind/unbind/destroy

Because delivery is by direct INTID→object lookup (Design decision 2), `IrqObj` has **no `next`/armed
list**. So `irq_bind`/`irq_unbind`/`destroy_irq` each edit a single key `i`, and the entire timer
list-proof apparatus is absent: no `timer_chain`/`timer_seq`/`timer_complete`, no
`lemma_timer_remove_chain`/`lemma_timer_push_head_chain`/`lemma_timer_chain_unique`, no `while`/
`decreases`, no splice walk. Two lemmas suffice for the census bookkeeping:

- `lemma_irq_binding_refs_pos` (the `lemma_armed_timer_refs_pos` copy) — a bound IRQ witnesses
  `irq_binding_refs(o) ≥ 1`, used to discharge the bound-notif-live precondition from the census.
- `lemma_irq_binding_retarget` (the `lemma_armed_timer_retarget` copy) — the general single-key
  transition. Because every IRQ op is a single-key edit, this **one** lemma discharges bind (post
  bound), unbind (post unbound), *and* destroy (via unbind) uniformly. The timer needs both a
  `disarm`-shaped and a `retarget`-shaped lemma only because `disarm` edits *two* keys (the node + its
  predecessor's `next`); with no list, the retarget form is enough.

`irq_wf` is correspondingly trivial — a pointwise `bound ⇒ notif is Some`, with **no head/existential
chain** (contrast `timer_wf`'s `exists ts. timer_chain ∧ timer_complete`).

## 3. Adding a `Store` view forces the kernel + test impls, even for a "kcore-only" phase

The verified core reasons over the abstract `ExStore`/`StoreSpec` views, but the trait's *exec*
accessors must be implemented by every `Store`: `KernelStore` (`kernel/src/store.rs`) and `ArrayStore`
(`kcore/src/test_store.rs`). So adding `irq_intid`/`irq_notif`/… to the `Store` trait means the kernel
build breaks unless `KernelStore` implements them — even though B-IRQ-A adds *no* runtime IRQ path. The
kernel impl is the trusted int→ptr shell (`(*obj_ptr::<IrqObj>(i)).field`), the `TimerObj` deref
pattern. This is the B8C ready-queue precedent (trait widening + impls land together). The
INTID→`ObjId` reverse lookup (`irq_for_intid`) is *not* added here — it needs the `IRQ_TABLE`, which is
B-IRQ-B boot wiring, and has no verified-core consumer.

Note the **boot-static** payoff (Design decision 3): `IrqObj` is *not* retyped from untyped, so it
needs **no `ExIrqObj` opaque-size `external_body`** — the trusted-base tally stays at **13**, unchanged.
The kernel deref works without a size registration because the verified core never touches the concrete
`IrqObj` (only the `irq_view` seam).

## 4. `setter ensures` terminator gotcha when mechanically inserting a frame clause

The setter `ensures` blocks in the `ExStore` *trait declaration* terminate the last clause with `;`
(no method body), whereas op/lemma bodies terminate with `,` before `{`. A naive "duplicate the
`cspace_view` line, s/cspace_view/irq_view/" pass produced `…cspace_view();` followed by
`…irq_view();` — two `;`-terminated clauses, orphaning the second (syntax error). The fix: when the
matched clause ends in `;`, the *original* must become `,` and the inserted sibling keeps the `;` (it is
now last). Copy-and-`s///` preserves indentation, `&&&` prefixes, and commas automatically; only the
last-clause `;` needs this special-case.

## 5. Host-test `refcount_sound` round-trip needs a *sound* fixture (refs == census, not refs == 1)

The timer host tests seed `refs[notif] = 1` and never assert `refcount_sound_exec` (the fixture is a
phantom — `refs = 1` but census `= 0`, since no cap designates the notif). To actually pin down the
census round-trip for IRQ, the fixture must start **sound**: `refs[notif] = 0` (census `= 0`), so each
`irq_bind` `+1` raises both `refs` and `irq_binding_refs` in lockstep and `refcount_sound_exec` holds
end to end. Asserting soundness on the timer-style `refs = 1` fixture fails at entry.

## 6. Tooling

- **`cargo check -p kcore --tests` is the fast exhaustiveness/type loop.** Adding an enum variant breaks
  every non-`_` match on `CapKind`; `cargo check` (compiles the erased `verus!` output) flags all of
  them (`E0004`) in ~seconds, vs. a multi-minute `cargo verus verify`. Run it before the verifier. (The
  variant needed arms in `cap_obj`, `inc_ref`, `derive`'s `obj_opt`, `obj_unref`, plus the exec mirrors
  `cap_obj_exec`/`cap_obj_of` in `test_store`; `derived_kind`/`cap_max_prio` were covered by `_`.)
- **Redirection order for capturing verify errors:** `cargo verus verify … 2>&1 > f` sends stderr to
  the *terminal* (errors are on stderr); use `> f 2>&1`. The errors with the real messages are on
  stderr.
- The verifier is the source of truth for the perturbation sweep: make the additions, run verify, and
  let each `postcondition`/`invariant`/`precondition` failure point at the next frame that needs an
  `irq_view` clause added (census lemma) or removed (teardown context). The cascade converged in ~5
  verify rounds from 11 → 0 errors.

## 7. Count delta

384 → **389** = **+5 verified items**: `irq_bind`, `irq_unbind`, `destroy_irq` (3 exec fns) +
`lemma_irq_binding_refs_pos`, `lemma_irq_binding_retarget` (2 proof fns). The `open spec fn`s `irq_wf`
and `irq_binding_refs` do not add to the verified count (spec definitions, not proof obligations) — the
B10B observation that the *count* tracks proof/exec items, not spec fns, holds here too.
