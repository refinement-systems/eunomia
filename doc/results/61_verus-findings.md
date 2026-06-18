# Verus findings 41 — Phase 7e: `dma-pool` — the free-list allocator (trophy #2)

Plan: `doc/plans/3_verus-rewrite.md` (§4.7, §7 step 6) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7e). Prior increment: `60`
(phase 7d — `urt::time`). This increment is the fifth host-chokepoint migration:
the first-fit DMA free-list allocator in `dma-pool/src/lib.rs` — from Kani
(POOL=16, one concrete buffer pair) to Verus (unbounded ∀ pool length, request
size, and alignment). It is **trophy #2** of the §4.7 thesis: the **DN-10**
two-buffer disjointness + alignment round-up that "bit-blasts CaDiCaL to OOM"
(the harness's own note — only *one* concrete pair was ever checked) becomes a ∀
theorem.

`cargo verus verify -p dma-pool`: **26 verified, 0 errors**. `cargo test -p
dma-pool`: **3 passed** (the `verus!{}` block erases — `alloc_respects_alignment`,
`exhaustion_and_free_merge`, `data_roundtrip_and_device_view` run the same code).
`cargo test --workspace --exclude kernel`: green (the `dma-pool` consumers
`virtio-blk`/`storage-server` use only the unchanged public API). `cargo kani`:
`dma-pool` is **gone** from the job (`dma-pool/src/proofs.rs` deleted — its two
harnesses were the last); the `kani` job now runs only `-p cas -Z stubbing`. `cd
kernel && cargo build`: green — the new `verus!{}` block erases into storaged's
aarch64 cross-build (vstd already arrived transitively via `ipc`/`urt`, so this
edge adds no new binary).

---

## 1. What was bounded, and the trophy

Kani's `check_dma_alloc_disjoint` had **two parts**, and its own doc-comment
records the split: Part 1 (a single first-allocation, `align == 1`) was symbolic
"∀ first sizes"; Part 2 — two live buffers pairwise disjoint with the **alignment
round-up** honoured — had to stay a *concrete pair* (`alloc(5,1)` then `alloc(4,4)`)
because the round-up `(off+align-1) & !(align-1)` over a *symbolic* remainder offset
"bit-blasts CaDiCaL to OOM" (DN-10). Phase 7e makes the whole thing ∀: every
`alloc` hands out an in-pool, aligned offset whose region was free and is now used
with coverage elsewhere unchanged, and **two-buffer disjointness is a corollary**
(`lemma_two_allocs_disjoint`) — for *all* sizes and alignments. The unit tests stay
as differential coverage (§5 discipline).

## 2. The shape: verified core / trusted PA seam

`dma-pool` is, by §2.5, "the single place physical addresses are visible" — so the
honest line is to verify the *arithmetic* and trust the *PA boundary*, not to drag
raw pointers into Verus. The port therefore **extracts a non-generic `FreeList<const
N: usize>`** (`{ len, free: [(usize,usize); N], nfree }`) inside `verus!{}` holding
exactly the data the old `DmaPool.free`/`.nfree` held plus the captured pool length;
`DmaPool<B: DmaBacking>` becomes a thin `{ backing: B, fl: FreeList<MAX_FREE_RANGES> }`
wrapper **outside** the block. `new`/`alloc`/`free` delegate to `fl`, then compute
`device_addr = device_base + offset` (the bijection — one line, by construction, kept
out of the proof because `base + offset` can't be shown non-overflowing for an
abstract base, and the PA seam is the trusted boundary anyway). `bytes`/`bytes_mut`/
`write`/`read` (raw-pointer slice formation over `backing`) are unchanged. This is
the 7c `urt::slots` split (a self-contained verified struct + a thin outer shell),
applied to the PA boundary.

## 3. The model (`closed`, private-field struct)

Five `closed spec fn` (the 7b/7c opaque-field rule — bodies read the private fields):

- `spec_len`, `spec_nfree`: ghost-`int` accessors so the *public* `alloc`/`free`
  contracts can speak of in-pool/not-full without exposing fields. (A *public* fn's
  contract may not name a private field — `error: disallowed: field expression for an
  opaque datatype` — hence `spec_nfree` for `free`'s not-full precondition.)
- `ext_has(k, p)`: position `p` lies in extent `k` = `[off, off+len)` — the atom the
  splice frame proofs are written against.
- `covers(p)`: `exists k. 0 ≤ k < nfree && ext_has(k, p)` — the free *set*, the model
  the disjointness theorem is stated against (trigger `ext_has(k, p)`).
- `wf`: `free@.len() == N`; `nfree ≤ N`; every extent non-empty and in `[0, len)`; and
  **strictly sorted with a gap** (`free[k].end < free[k+1].start`) — the merged-canonical
  invariant every op preserves.

`lemma_chain` (sortedness is transitive **and strict** — the gap survives because each
extent is non-empty) and `lemma_disjoint` (extents `j ≠ k` share no position) are the
two reusable consequences the `covers` frame rests on.

## 4. The obligations (∀)

| Item | `ensures` (the unbounded theorem) |
|---|---|
| `new` | `wf`; `covers(p) ⇔ 0 ≤ p < len` |
| `alloc` | `Some(start)`: in-pool `start+n ≤ len`; **aligned** `start % align == 0`; `[start,start+n)` was free, is now not; coverage elsewhere unchanged. `None`: coverage unchanged (honest: `None` is *not* exact-exhaustion — first-fit + the `N`-cap can refuse with space left) |
| `free` | `wf`; `[off,off+n)` is now covered; coverage elsewhere unchanged |
| `lemma_two_allocs_disjoint` | two successive `alloc`s return disjoint intervals (`a+na ≤ b ∨ b+nb ≤ a`) — **∀**, the DN-10 trophy |

The disjointness lemma is a pure corollary: `fl1` is the pool *after* the first carve,
so `alloc`'s `!final.covers` gives `![a]` covered in `fl1`, while the second carve's
`old.covers` says `[b] ⊆` covered-`fl1` — a shared position would be both covered and
not, so `max(a,b)` witnesses the contradiction. Stated interval-wise (`a+na ≤ b ∨ …`)
to stay trigger-free.

## 5. Two exec restructures for verifiability (both behaviour-identical)

The plan flagged `copy_within` and the bit-mask round-up as the chief 7e risks; both
are sidestepped by restructuring the exec (the 7a/7c/7d precedent — std combinators are
unspecced, so they get rewritten):

- **`copy_within` → explicit shift loops.** `<[T]>::copy_within` has no Verus model, so
  the array splices become two verified helpers, `remove_at` / `insert_at` (invariant-
  carrying shift loops giving a clean index correspondence). This is the same
  array-splice reasoning kcore closed for `cdt_unlink` / `slot_move`.
- **Bit-mask round-up → modular round-up.** `start = (off+align-1) & !(align-1)` —
  the term that OOM'd CaDiCaL — becomes `pad = (align - off%align)%align; start = off +
  pad`, so `start % align == 0` is pure `vstd::arithmetic::div_mod`
  (`lemma_round_up_aligned`: `lemma_fundamental_div_mod` + one `nonlinear_arith` +
  `lemma_fundamental_div_mod_converse_mod`), **no `by (bit_vector)`**. The precondition
  weakens from "power of two" to just `align > 0` (all the modular identity needs); the
  `DmaPool::alloc` wrapper keeps its `debug_assert!(is_power_of_two)` sanity check.

`free`'s three nested `copy_within`s collapse further: computing the merge from
*original* indices (left/right merges are independent — both pivot on `off+n` being the
merged region's end) reduces the surgery to one `insert_at` (no merge), one in-place
`set` (single merge), or `set`+`remove_at` (both merges) — never the original dance.

## 6. The proof, op by op

**`alloc`** holds `self == old(self)` across the first-fit search (the carve only
mutates on the returning iteration — the 7c `slots::alloc` pattern), so each of the four
carve arms `(pad>0?, rest>0?)` is a localised splice proved by a per-arm helper
(`alloc_proof_set` for the two single-extent arms, `alloc_proof_remove` for the exact-fit
removal, `alloc_proof_split` for the pad+remainder split). Each proves `wf` survives and
`covers` loses exactly `[start, start+n)` via the index correspondence + `lemma_disjoint`.
The `None` path needs a `covers` *congruence* (same `free@`/`nfree` ⇒ same `ext_has` ⇒
same `covers`) — Verus does not derive it through the `closed` specs unaided.

**`free`** mirrors it: the search establishes the insertion index `i`, the precondition
`![off,off+n)` covered (the region was allocated) gives — with sortedness — that the left
neighbour ends `≤ off` and the right starts `≥ off+n`, so merges are the equality cases.
Four covers-**add** helpers (`free_insert` / `free_replace` / `free_both`, with their
`free_covers_*` halves) prove `covers` *gains* exactly `[off,off+n)`. The original's
`assert!(nfree < MAX_FREE_RANGES)` overflow guard is formalised as the precondition
`spec_nfree() < N` (only the no-merge case grows the list).

**rlimit hygiene.** The split/merge helpers are heavy (an existential `covers` frame
across a shift correspondence), so the covers half of each is a separately
`#[verifier::spinoff_prover]`'d proof fn (`split_covers`, `free_covers_*`) — the doc 25
§2 decomposition discipline — with a modest `#[verifier::rlimit(..)]` on the splice
helpers and `alloc`/`free`. **A logic bug masqueraded as divergence:** an early
`alloc_proof_split` backward branch asserted a false `ext_has` for the `k<i` case, and Z3
*thrashed* trying to prove the impossible (rlimit-exceeded at 250); fixing the branch made
it pass at a fraction of the budget — a reminder that "rlimit exceeded" can mean "the goal
is false", not "needs more time".

## 7. Toolchain notes worth recording

- **`as int <` mis-parses as generics.** `x as int < y` is read as `int<…>` (a type with
  generic args), `error: expected ','`; parenthesising the left operand (`(x as int) < y`)
  or using the quantifier-head `#![trigger …]` form fixes it. (`<=`/`>=`/`>` are immune.)
  Hit repeatedly across the new strict-sortedness clauses.
- **Public-fn contract ⇏ private field.** A `pub fn`'s `requires`/`ensures` may not name a
  private field; route through a `closed spec` accessor (`spec_nfree`). Private `proof fn`s
  (the helpers) read the fields directly, as in 7c.
- **Mut-ref postconditions need `final(self)`** (not `self`) — the 7c precedent, now also
  for the splice helpers' `&mut`.
- **A generic `DmaPool<B: DmaBacking>` and the derived-`Copy` `DmaBuf`/`DeviceAddress` live
  outside `verus!{}` with zero friction** — the verified `FreeList` is non-generic and
  pointer-free, so the trait/raw-pointer surface never enters the proof.
- **Behavioural tidy-up:** `FreeList::new(0)` sets `nfree = 0` (the old code left a
  degenerate `(0,0)` extent); harmless and keeps `wf`'s non-empty invariant clean.

## 8. What changed

- `dma-pool/src/lib.rs` — one `verus!{}` block holding `FreeList`, the five `closed`
  specs, `lemma_chain`/`lemma_disjoint`/`lemma_round_up_aligned`, the shift helpers
  `remove_at`/`insert_at`, the verified `new`/`alloc`/`free`, the per-arm/per-case frame
  helpers, and `lemma_two_allocs_disjoint`; `DmaPool` rewritten as the thin
  `backing + fl` wrapper; `use vstd::prelude::*;`. `DmaBuf`/`DeviceAddress`/`DmaBacking`,
  the `host` module, and the three unit tests stay outside, verbatim.
- `dma-pool/src/proofs.rs` — **deleted** (`check_dma_alloc_disjoint`,
  `check_dma_free_reuse` — the last harnesses).
- `dma-pool/Cargo.toml` — `vstd` dep + `[package.metadata.verus] verify = true` added;
  `unexpected_cfgs` swaps `cfg(kani)` (dma-pool is fully off Kani) for the Verus cfgs.
- `.github/workflows/ci.yml` — `kani` job: `cargo kani -p dma-pool` dropped (only
  `-p cas -Z stubbing` remains, +comment); `verus` job: `cargo verus verify -p dma-pool`
  added after `-p urt` (+comment).
- `CLAUDE.md` — the `cargo kani`/`cargo verus` examples, the `kani`/`verus` CI bullets,
  the Verus-tier table row, and the `### Kani` / `### Verus` prose (dma-pool off Kani;
  add the 7e note; only `cas` remains on Kani).

## 9. Next

**7f — `cas::disk`** superblock (`validate_geometry` + `decode_checked`, blake3
ghost-stubbed) and **the holdout decision**: if it ports without disproportionate
`vstd::Vec` pain, 7g retires the `kani` job entirely (only `cas` is left on it now);
otherwise `cas::disk` stays the single allowed Kani holdout. §3–§6's `closed`-glue,
modular-`div_mod`, splice-helper, and spinoff/rlimit notes carry forward.
