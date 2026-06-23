# Plan — Part B11 detail: `urt` heap-allocator verification (the `GlobalAlloc` brought up to the crate's own Verus bar by switching the free list from a raw-`*mut Block` arena graph to the verified side-stored `FreeList<N>` extent model, plus a wrapper Miri+proptest tier)

Detailed, separately-implementable decomposition of **Phase B11** from
`doc/plans/0_address_audit_rev0.md`. B11 is Wave-2 work and is **independent — nothing
depends on it, and it depends on nothing** beyond a blessed Part A. It closes the single
largest block of unverified `unsafe` in a crate the project advertises as Verus-verified:
`urt`'s actual `#[global_allocator]`.

**Closes (from the parent plan):**
- **The `urt` heap allocator is wholly unverified** [audit §4.2, **high**]. Verbatim from
  `doc/results/0_audit_rev0.md` §4.2 (lines 499–505):
  > **The `urt` heap allocator is wholly unverified. [high]** `urt` is in the Verus
  > verify set and its `slots`/`time` modules are proven, but the crate's actual
  > `GlobalAlloc` — `Heap<N>::alloc/dealloc` (`urt/src/lib.rs:48-159`): first-fit
  > free-list traversal, alignment padding, block splitting, two-sided
  > address-ordered coalescing over raw `*mut Block` — is heavy `unsafe` pointer
  > arithmetic with only two happy-path tests, no Miri target, no proptest, no proof.
  > This is the largest single block of unverified `unsafe` in a "verified" crate.
- The audit's own follow-up item (§8, line 700): "Miri-cover the `urt` heap allocator and
  the DMA-pool wrapper" — the DMA-pool half landed in **B4C**; B11 closes the `urt` half.

**Spec target (already blessed in rev1 — B11 only conforms code to it):**
- **rev1§6** — the Verus tier's routing names "the host chokepoints (the IPC crate, **the
  userspace runtime**, the DMA pool, and the CAS layer)"; they "verify without bound." `urt`
  *is* the userspace runtime, and its `GlobalAlloc` is the one chokepoint in it the rev1§6
  baseline ("everything gets Miri + proptest"; chokepoints get Verus) has never reached. The
  crate already meets the bar everywhere else — `slots` (the cspace-slot bitmap free list,
  Verus) and `time::utc_ns_at` (the seqlock tick→ns conversion, Verus) — so the heap is the
  lone hole, exactly as the audit says.
- The trusted-base ledger (`doc/guidelines/verus_trusted-base.md`) lists `urt`'s verified
  surface as "slot bitmap + `utc_ns_at`" and its lone trusted seam as `debug_check_free`
  (a runtime double-free guard, category (3)). B11 adds the heap free-list arithmetic to the
  verified surface and records the (small, plain-Rust) arena byte-region seam exactly as the
  ledger already records the structurally-identical DMA-pool wrapper.

Because Part A is blessed first (the parent plan's hard dependency), **B11 makes no spec
edits** — the rev1 text above is the fixed target. Every citation here is `rev1§`.

**Primary files:**
- `urt/src/lib.rs` — the allocator: `Heap<N>` (`:48`), `init_once` (`:66`), `round` (`:76`),
  `GlobalAlloc::alloc` (`:88`), `GlobalAlloc::dealloc` (`:132`), the `Block`/`MIN_BLOCK`
  scaffolding (`:39-45`), and the two inline tests (`:166-191`). All of the raw-`*mut Block`
  machinery is **deleted** by B11B and replaced by a verified-`FreeList`-backed wrapper.
- The verified `FreeList<N>` core (today `dma-pool/src/lib.rs:84-1321`, `verus!{}`) — made
  available to `urt` per Design decision 2.
- `urt/Cargo.toml` (the `freelist` dep + the `proptest` dev-dep is already present `:24`),
  `Cargo.toml` workspace members (the new `freelist` crate, if extracted), and `CLAUDE.md`
  (the Miri-sweep command, which already names `cas`/`loader`/`storage-server` and gained
  `dma-pool` in B4C — `urt` joins it in B11C).

---

## Verification tier & baseline (applies to all sub-phases)

Per rev1§6 routing, **`urt` is a Verus chokepoint** — like B4 (and unlike the test-only
B1/B2/B3/B15 phases), B11 touches the verified surface, so the regression gate is
load-bearing. Five honesty notes up front so nothing is silently dropped or over-claimed:

- **The allocation *algorithm* moves into Verus; the gate rises.** Today
  `cargo verus verify -p urt` passes over `slots` + `time` only (the ledger states it
  qualitatively — "verified (slot bitmap + `utc_ns_at`)" — without a count, the convention
  this plan keeps). B11 brings the free-list arithmetic (first-fit search, alignment
  round-up, split, two-sided coalesce) under proof, so the verified surface grows; **record
  the new state** in the ledger's Baselines table and Scope prose. No existing `verus!{}`
  proof is weakened (B11 does not touch `slots`/`time`).
- **The raw-pointer/arena seam stays *trusted* — it gets Miri+proptest, not Verus.** Forming
  the returned `*mut u8` is `(arena_base).add(offset)` over an `UnsafeCell<[u8; N]>` — a
  raw-pointer/interior-mutability operation, not first-order arithmetic. This is **the exact
  same seam shape the DMA-pool wrapper already has** (`from_raw_parts(cpu_base().add(off),
  len)`, B4), and the ledger already designates that boundary trusted-plain-Rust, kept
  honest by Miri+proptest. B11 keeps that line: it does **not** pull the `Heap` wrapper into
  `verus!{}` (that would force `external_body`/`assume` across the byte-region seam, *growing*
  the trusted surface — the opposite of the B7 shrink-the-seam direction and of the dma-pool
  design). The verified core is the side-stored `FreeList<N>`; the wrapper is checked
  scalar arithmetic over public offsets, Miri+proptest-covered.
- **This avoids the pointer-graph proof, not by attempting it.** `doc/guidelines/verus.md`
  Part B §1 is explicit: a "pointer-linked graph forces a permission token per reachable
  node — the dominant cost and failure mode of memory-model verification … Making links
  *data* trades that whole burden for ordinary `Map`/`Set` reasoning." The current heap is
  precisely a pointer-linked graph (`*mut Block` next-links smeared into the freed bytes). A
  faithful Verus proof of *that* structure is the research-grade lift Open Decision 5 calls
  "disproportionate." B11's representation switch (Design decision 1) makes the links *data*
  (side-stored `(offset, len)` extents), so the proof is the same first-order `Seq` reasoning
  already discharged for `slots` and the DMA-pool `FreeList` — proportionate, and reusing an
  existing proof rather than inventing one.
- **`assert!`/leak, not `Result` — heap input is trusted in-process, not untrusted wire.**
  The allocator is fed by `core`'s own `alloc`/`dealloc` calls inside one single-threaded
  process; a malformed `(ptr, layout)` is a *program bug*, not adversarial data (contrast the
  ELF loader, B3, which `Result`-refuses untrusted images). So B11 does **not** add a
  fuzz target (the rev1§3.7 "decoders are fuzz targets" routing does not apply — the heap
  decodes nothing), keeps the infallible `GlobalAlloc` signatures, and backstops contract
  violations with a defined panic/`debug_assert` exactly as the DMA-pool wrapper does
  (B4 Design decision 2). proptest + Miri is the wrapper tier.
- **No Loom/Shuttle.** Eunomia processes are single-threaded by construction (the `Heap`'s
  `unsafe impl Sync` rests on "no concurrent access by construction", `lib.rs:54-55`); there
  is no second mutator and no atomic in the allocator, so nothing for a weak-memory model to
  witness (same posture as B4's pool note). B11 records this rather than adding a no-value
  harness. (`urt`'s *one* concurrency object, the `time.rs` seqlock, keeps its existing
  Loom/Shuttle tier — untouched by B11.)

**Baseline to re-establish at end of B11:**
- `cargo verus verify -p urt` green, at the **new** (higher) total — the slot bitmap and
  `utc_ns_at` proofs unchanged, plus the heap free-list arithmetic. If the `FreeList` is
  extracted to a shared crate (Design decision 2), also `cargo verus verify -p freelist`
  green and `cargo verus verify -p dma-pool` ≥ **29/0** unchanged (a move, not a weakening).
- `cargo test -p urt` green (the two existing heap tests rewritten against the new
  allocator + the new proptests).
- A **new** Miri leg `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p urt`
  clean — the wrapper's `UnsafeCell` + `base.add(off)` raw access across randomized
  alloc/dealloc/realloc sequences is exactly what Miri validates (the audit's "no UB under
  Miri across randomized sequences" acceptance).
- The aarch64 userspace cross-build still links every binary that sets
  `#[global_allocator] static HEAP: urt::Heap<…>` (`user/init`, `user/shell`,
  `user/storaged`, …) and QEMU boot stays green (the heap is on the boot path of every
  alloc-using userspace program).

---

## Design decision 0 — the verification bar: Verus via a representation switch, vs. the Miri+proptest floor *(resolves the parent plan's Open Decision 5; pin in B11A)*

The parent plan (Open decision 5) leaves the bar open: "Full Verus proof vs Miri+proptest
floor. *Recommendation:* target Verus (it's in the verify set); accept the Miri+proptest
floor only if the pointer-graph proof is disproportionate." B11 resolves it.

- **The disproportionate path is a *direct* Verus proof of the current structure.** Verifying
  `Heap<N>` as it stands means proving an intrusive `*mut Block` free list: a heap-allocated
  graph reached by raw pointers, with overlapping typed views (`&mut Block` carved out of the
  same `&mut [u8]` the allocator also hands to callers). That is the `PointsTo`-per-node
  regime `verus.md` §1 names the dominant failure mode — research-grade, and it would also
  *grow* the trusted surface (raw-pointer permissions, provenance axioms) rather than shrink
  it. **Rejected.**
- **The proportionate path is to change the representation so the proof is the one we already
  have.** Switch the free list from in-arena pointers to the **side-stored, pure value-type
  `FreeList<N>`** — a sorted, pairwise-disjoint, non-adjacent list of `(offset, len)` extents
  over `[0, N)`, exactly the structure the DMA-pool already verifies (`dma-pool/src/lib.rs:93`,
  `cargo verus verify -p dma-pool` 29/0). Then the entire allocation algorithm is first-order
  arithmetic over a `Seq` — *the proof exists* (Design decision 1). This is the Verus bar the
  parent plan prefers, reached without the pointer-graph lift. **Adopted.**
- **Even on the Verus path, the Miri+proptest tier is mandatory, not an alternative.** It
  keeps the one remaining trusted seam (the `UnsafeCell` + `base.add(off)` byte access)
  honest and satisfies the audit's "no UB under Miri" acceptance. This is precisely what
  dma-pool does — verified `FreeList` *and* a wrapper proptest/Miri tier (B4C). B11 mirrors
  it: **Verus (B11A/B11B) + Miri+proptest (B11C)**, not either/or.
- **The pure Miri+proptest floor (keep the intrusive `*mut Block` list, add only tests) is
  the recorded fallback.** It is the parent plan's "minimum acceptable" and would close the
  audit's literal acceptance ("covered by Verus *or* Miri+proptest; no UB under Miri"). Its
  cost: the largest `unsafe` block in a "verified" crate stays **out of the verified surface**
  — the ledger would record it as test-routed, not mechanized, and the rev1§6 "userspace
  runtime is a Verus chokepoint" claim stays unmet for the heap. Its one *advantage* over the
  Verus path is that the intrusive list has **no fragmentation cap** (Design decision 3); the
  Verus path trades that for a bounded-but-verified allocator. B11 judges the verification win
  worth the disclosed bound, so the floor is the fallback, not the plan.

**Recommendation: adopt Verus via the `FreeList<N>` representation switch (Design decision 1),
paired with the mandatory wrapper Miri+proptest tier (B11C). Keep the pure-Miri floor over the
existing intrusive list as the documented fallback if the fragmentation-cap trade (Design
decision 3) is judged unacceptable for some userspace workload.**

---

## Design decision 1 — representation: side-stored `FreeList<N>` extents vs. the intrusive arena graph *(pin in B11B)*

The current allocator stores its free list **in the freed bytes themselves**: each free block
begins with a `Block { size, next: *mut Block }` header (`lib.rs:39-43`), the head is a
`*mut Block`, and `alloc`/`dealloc` walk and splice that pointer chain in place. Zero
per-block metadata overhead, unbounded fragmentation — and unverifiable without per-node
pointer permissions (Decision 0).

- **Adopted — the free list becomes side-stored data: `FreeList<HEAP_RANGES>` beside the
  arena.** The arena `UnsafeCell<[u8; N]>` becomes **pure storage** (handed to callers, never
  holds allocator metadata); the free list lives in its own field as a bounded array of
  `(offset, len)` extents. The entire algorithm — first-fit search, leading-pad handling,
  trailing split, address-ordered insert, two-sided coalesce — is then *the same operation
  the verified `FreeList` already implements*:
  - `FreeList::alloc(n, align) -> Option<usize>` (`dma-pool/src/lib.rs:410`): first-fit carve
    of `n` aligned bytes from the first fitting extent, `ensures` the returned offset is
    `align`-aligned, in-pool (`start + n <= len`), was free, is now used, and *every other
    position's coverage is unchanged* (the `covers` frame). This *is* the heap's
    search+pad+split, proven.
  - `FreeList::free(off, n)` (`:1189`): address-ordered insert with the two-sided adjacency
    merge — *the heap's coalescing dealloc*, proven, with the canonical
    sorted/disjoint/non-adjacent `wf` invariant re-established (`:134`).
  - `lemma_two_allocs_disjoint` (`:1288`): two live allocations are disjoint **∀** — the
    property the proptest's "no write to one allocation perturbs another" oracle rests on,
    already a verified corollary.
  - The alignment round-up is modular (`off + (align - off%align)%align`, `:185-217`), so
    `start % align == 0` is pure `vstd::arithmetic` — no `by (bit_vector)`. The heap's current
    bit-mask `base.next_multiple_of(align)` (`:97`) is behaviourally identical and the modular
    form is what is proven.
- **The representation switch *removes* an entire class of the heap's fiddly edge cases.**
  With metadata side-stored, a free extent has **no minimum size** — any positive remainder is
  a valid extent. So the current allocator's `MIN_BLOCK`-driven logic — the "padding too small
  to stand alone, skip the block" branch (`:103-107`), the "leading pad stays as a free block
  only if `pad >= MIN_BLOCK`" branch (`:109-113`), the "rest unusable: absorbed into the
  allocation" branch (`:119-120`) — all **disappear**. `FreeList::alloc` keeps the leading pad
  and the trailing rest as exact extents unconditionally (no `>= MIN_BLOCK` test), which is
  both simpler *and* less wasteful than the code being replaced. `MIN_BLOCK`/`Block` are
  deleted.
- **What the wrapper still owns (the trusted seam, kept tiny).** Three plain-Rust steps, none
  of them the algorithm:
  1. `UnsafeCell<FreeList> → &mut FreeList` (interior mutability; sound by the single-threaded
     `Sync` contract, same `UnsafeCell` deref the current `init_once` already does, `:67-72`);
  2. `offset → *mut u8` as `(self.mem.get() as *mut u8).add(off)` — sound because
     `FreeList::alloc` `ensures start + n <= spec_len() == N`, so `off` is in-arena; this is
     the lone raw-pointer formation, the analogue of dma-pool's `range_ptr`;
  3. `*mut u8 → offset` on dealloc as `(p as usize) - (self.mem.get() as usize)`, with the
     `FreeList::free` preconditions discharged by the verified `is_full`/`is_allocated`
     accessors (B4 already added these, `:334`/`:348`) exactly as the dma-pool wrapper does.
- **Rejected — keep the intrusive list, prove it over raw pointers.** Decision 0's
  disproportionate path.
- **Rejected — a bitmap allocator (the `slots.rs` shape).** A one-bit-per-granule bitmap over
  `[0, N)` is fully verifiable (it is literally `urt::slots`), but at byte granularity over a
  multi-MiB heap the bitmap is huge and allocation is an O(N/granule) scan — wrong for a
  general heap. The extent list is the right structure (few entries, O(extents) ops); it is
  also the one already proven. **Rejected** in favour of reuse.

**Recommendation: adopt the side-stored `FreeList<HEAP_RANGES>` representation, reusing the
verified DMA-pool free-list arithmetic verbatim; the arena becomes pure storage and the
wrapper shrinks to the three trusted scalar/pointer steps above.**

---

## Design decision 2 — where the verified `FreeList<N>` lives: extract to a shared crate vs. copy into `urt` *(pin in B11A)*

The proof B11 reuses is ~1300 lines of `verus!{}` currently inside `dma-pool` (`:84-1321`).
It is **self-contained**: the `FreeList` module references none of dma-pool's
`DmaBacking`/`DmaBuf`/`DeviceAddress` — it is "No backing, no pointers" by its own doc
(`:90`). Two ways to make it reachable from `urt`:

- **Adopted (recommended) — extract `FreeList<N>` + its lemmas into a new `no_std`,
  vstd-pinned crate `freelist`, depended on by both `dma-pool` and `urt`.** The move is
  mechanical (lift the `verus!{}` block, `MAX_FREE_RANGES` becomes the caller's chosen `N`
  type-param, the `lemma_two_allocs_disjoint` travels with it); dma-pool re-points its wrapper
  at `freelist::FreeList` and **re-verifies unchanged** (same code, new path — the 29/0 gate
  is a no-op re-check), urt depends on the same crate. Decisive reasons:
  1. **One source of truth for the proof.** The ledger ethos (and the whole kcore split) is a
     small verified base with no duplicated surfaces; two copies of a 1300-line proof is two
     things to keep in lockstep, two ledger rows, and a drift hazard. Extraction keeps one.
  2. **No wrong-direction coupling.** `urt` must not depend on `dma-pool` (the crate "where
     PAs are visible", pulled into every userspace binary) just to borrow a free list. A
     neutral `freelist` crate both depend on is the clean shape.
  3. **The new crate gets its own gate** (`cargo verus verify -p freelist`), and dma-pool's
     and urt's gates each shrink to *their* wrappers — a cleaner accounting than today's
     "dma-pool 29/0 includes the FreeList."
  - Cost: it touches **landed** dma-pool surface (a Wave-1 crate) and restructures the
    ledger's "DMA-pool `FreeList`" line into "shared `FreeList` (dma-pool + urt)". That blast
    radius is the only mark against it, and it is mechanical + gate-guarded.
- **Fallback — copy-and-adapt `FreeList<N>` into a `urt::heap::freelist` module.** Keeps
  B11's blast radius to `urt` alone (honoring "B11 is independent"), at the cost of the
  duplication in (1). Choose this only if churning landed dma-pool is judged not worth the
  dedup — the proof is stable/done, so the drift risk is low in practice. If taken, the ledger
  carries two `FreeList` rows (dma-pool's and urt's) and the ledger's reconciliation note
  records why.
- **Rejected — `urt` depends on `dma-pool`.** The wrong-direction coupling of (2), for no
  benefit over extraction.

**Recommendation: extract `FreeList<N>` into a shared `freelist` crate (single proof, no
coupling, gate-guarded mechanical move); fall back to copy-into-`urt` only if touching the
landed dma-pool surface is unwanted.** Either way, the heap's algorithm is verified by the
*same* proof — the choice is packaging, not assurance.

---

## Design decision 3 — the fragmentation cap and its over-capacity policy *(pin in B11B)*

Side-stored extents impose a bound the intrusive list does not have: at most `HEAP_RANGES`
free extents. The intrusive list stores metadata *in* the freed blocks, so it fragments
without limit; the verified `FreeList<N>` is a fixed `[(usize, usize); N]` array
(`dma-pool/src/lib.rs:101`). This is the one real trade of the Verus path, and B11 must
size the cap and define what happens at it.

- **Size `HEAP_RANGES` generously and disclose it.** The number of free extents equals the
  number of gaps between live allocations; pathological alloc/free churn can drive it toward
  `heap_bytes / min_alloc`. dma-pool uses `N = 64` because a driver holds a handful of
  buffers; a general userspace heap fragments more, so B11 sets a far larger cap (recommend
  **`HEAP_RANGES = 1024`**, tunable per binary — a 16 KiB side table of `(usize, usize)`
  pairs, negligible against a multi-MiB heap). Record the cap as a **disclosed MVP bound**
  (the project's standing discipline for accepted simplifications), noting that real userspace
  processes here are small and short-lived (`init`, `shell`, `storaged`, mkfs-like tools), for
  which a 1024-extent ceiling is not reachable in normal operation.
- **`alloc` at the cap → return null (out of memory).** `FreeList::alloc` already returns
  `None` when no extent fits *or* (by first-fit) when it refuses with space left — its `None`
  is explicitly "not an exact-exhaustion claim" (`:403-407`). The wrapper maps `None →
  ptr::null_mut()`, the correct `GlobalAlloc` OOM signal. No special handling. (A split that
  would exceed the cap is just a fit that the allocator declines — same null.)
- **`dealloc` at the cap → leak the block (don't record it), with a `debug_assert` witness.**
  This is the substantive choice, and it differs from dma-pool deliberately. `FreeList::free`
  *requires* `spec_nfree() < N` unconditionally (its `insert_at` may index before merging), so
  at `nfree == N` the wrapper cannot call `free` even for a region that would coalesce. The
  options:
  - **Adopted — leak.** `if self.fl.is_full() { debug_assert!(false, "urt heap: free-list at
    fragmentation cap; block leaked"); return; }` before delegating. Not recording a freed
    region is **safe** (those bytes are simply never reused — the free-list invariant "every
    listed extent is truly free" is preserved; nothing reads or writes the leaked bytes), and
    a heap must **never abort the process on a `dealloc`**. The leak is a disclosed,
    debug-observable degradation under pathological fragmentation, acceptable for the MVP
    userspace.
  - **Rejected — panic (dma-pool's choice).** dma-pool `assert!(!is_full())` and aborts
    (B4 Design decision 2), *correctly* — for a 64-extent **driver pool** where hitting the
    cap is pathological and should fail loud. A general heap's calculus is the opposite:
    aborting a process because its allocator's side table filled is far worse than leaking a
    block. So B11 takes the path B4 explicitly named as the alternative for "a future caller
    [that] wants a non-panicking pool" (B4 detail, Design decision 2, last bullet).
  - **Optional future hardening (noted, not in B11) — a verified `FreeList::free_or_coalesce`**
    that admits a free when `nfree < N` *or* the region merges with a neighbour (net
    non-increasing extent count), shrinking the leak window to the genuinely-no-neighbour case.
    It complicates the `free` proof for a tail case a generous `HEAP_RANGES` already makes
    rare; recorded so its omission is a decision, not a gap.
- **Alignment beyond the arena's alignment (`align > MIN_ALIGN`).** `FreeList::alloc` aligns
  the *offset*; `base.add(off)` is aligned in *address* space only if the arena base is
  itself `align`-aligned. `Heap` is `#[repr(C, align(16))]` (`:47`), so `align ≤ 16` (every
  standard Rust allocation) is exact. For the rare over-aligned request (`#[repr(align(N))]`
  types, SIMD, cache-line), B11 raises the arena attribute to a `MAX_ALIGN` const (recommend
  **`align(64)`**, covering cache-line and all common SIMD) and **asserts `layout.align() <=
  MAX_ALIGN`** (returning null above it — a clean OOM, not UB). This is the same
  offset-vs-address-alignment posture dma-pool already lives with (its alignment guarantee is
  offset-relative); documented so it is a decision, not a latent bug.

**Recommendation: `HEAP_RANGES = 1024` (disclosed bound, tunable); `alloc` over-cap → null;
`dealloc` over-cap → leak with a `debug_assert` witness (never abort a free); `MAX_ALIGN = 64`
with null above it. Record the fragmentation cap and the leak policy on the MVP-simplification
list and in the trusted-base ledger.**

---

## Sub-phase B11A — make the verified `FreeList<N>` available to `urt` *(lands the algorithm proof where the heap can reuse it)*

Foundational and mergeable alone: it produces **no behaviour change** to the heap (the
intrusive allocator still runs after B11A) — it only lands the verified core B11B rewires
onto. Resolves Design decision 2.

- **Touches (recommended extract path):**
  - **New crate `freelist`** — `freelist/Cargo.toml` (`no_std`, the vstd pin from
    `verus.md`'s "## The pin", `[package.metadata.verus] verify = true`, the
    `unexpected_cfgs` lint stanza mirroring `dma-pool`/`urt`), `freelist/src/lib.rs` (the
    `verus!{}` `FreeList<N>` module lifted verbatim from `dma-pool/src/lib.rs:84-1321`,
    including `wf`/`covers`/`alloc`/`free`/`remove_at`/`insert_at`/`is_full`/`is_allocated`,
    the bit/arith lemmas, and `lemma_two_allocs_disjoint`; the `FreeList`-local tests travel
    with it).
  - `Cargo.toml` (workspace) — add `"freelist"` to `members`.
  - `dma-pool/src/lib.rs` — delete the moved `verus!{}` block; `use freelist::FreeList`;
    `DmaPool` holds `freelist::FreeList<MAX_FREE_RANGES>` (one line). The wrapper, its
    `range_ptr` guard, its proptests, and its accessors-call-sites are untouched.
  - `dma-pool/Cargo.toml` — add the `freelist` path-dep.
  - `urt/Cargo.toml` — add the `freelist` path-dep (urt already carries vstd transitively via
    `ipc`, so no new cross-build risk — same note as `urt/Cargo.toml:16-19`).
- **Touches (fallback copy path):** `urt/src/lib.rs` (or a new `urt/src/heap/freelist.rs`) —
  the `verus!{}` `FreeList<N>` copied and adapted into `urt`; no dma-pool or workspace change.
- **Depends on:** Part A blessed (rev1§6 boundary). No intra-B11 dependency.
- **Work:** the mechanical move (or copy) above. Then re-verify: `cargo verus verify -p
  freelist` green (carries the FreeList count); `cargo verus verify -p dma-pool` ≥ **29/0**
  unchanged (re-check at the new path — must be a no-op, proving the move weakened nothing);
  `cargo test -p dma-pool` green (the wrapper tests are representation-agnostic). The aarch64
  cross-build still compiles `storaged`.
- **Acceptance:**
  - `freelist` verifies standalone; dma-pool's gate holds at ≥ 29/0 and its tests stay green
    — the extraction is provably behaviour- and proof-preserving.
  - `urt` builds against `freelist` (no use yet) on host *and* aarch64 cross-build.
  - No ledger change yet beyond noting the `FreeList`'s new home (the urt heap is not wired
    until B11B; the ledger flips on B11C).
- **Effort/Risk:** S / low (extract) — a mechanical, gate-guarded move. The only risk is a
  stale citation; the dma-pool 29/0 re-check is the guard. (Copy path: S, urt-only, at the
  cost of duplication.)

---

## Sub-phase B11B — rewire `Heap<N>` onto the verified `FreeList` *(the headline: delete the raw-`*mut Block` allocator)*

Replaces the intrusive pointer allocator with the `FreeList`-backed wrapper. After B11B the
heap's *algorithm* is the verified one; the only `unsafe` left is the three-step arena seam
(Design decision 1). Resolves Design decision 3.

- **Touches:** `urt/src/lib.rs` — a near-total rewrite of the allocator:
  - **Delete** `Block` (`:39-43`), `MIN_BLOCK` (`:45`), the `head: UnsafeCell<*mut Block>`
    field (`:50`), the pointer-walking `alloc` body (`:93-129`), and the pointer-splicing
    `dealloc` body (`:136-157`).
  - **New struct** (sketch):
    ```rust
    const HEAP_RANGES: usize = 1024;   // free-extent fragmentation cap (Design decision 3)
    const MIN_ALIGN:   usize = 16;     // arena granularity; offsets stay 16-aligned
    const MAX_ALIGN:   usize = 64;     // arena base alignment; larger requests refuse (null)

    #[repr(C, align(64))]              // = MAX_ALIGN, so base.add(off) meets layout.align() ≤ 64
    pub struct Heap<const N: usize> {
        mem: UnsafeCell<[u8; N]>,                              // pure storage now
        fl:  UnsafeCell<Option<FreeList<HEAP_RANGES>>>,        // None until first alloc
    }
    ```
    `Option` makes the const `new()` trivial (`fl: UnsafeCell::new(None)`) and a zeroed/`None`
    state *safe* before init (alloc just builds it); no `MaybeUninit`, no separate
    `initialized` bool. The `.bss` placement and loader-zeroing note (`:4-6`) is unchanged.
  - **`fl_mut` helper** (the lazy init, replacing `init_once` `:66-74`): `unsafe fn fl_mut(&self)
    -> &mut FreeList<HEAP_RANGES> { (*self.fl.get()).get_or_insert_with(|| FreeList::new(N)) }`.
    `FreeList::new(N)` (`dma-pool:295`) `ensures` the single full extent `[0, N)` and `wf` — the
    "fresh heap" state, proven.
  - **`alloc`** (sketch):
    ```rust
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        if align > MAX_ALIGN { return ptr::null_mut(); }            // Decision 3 (over-aligned → OOM)
        let need = layout.size().max(1).next_multiple_of(MIN_ALIGN);// keeps offsets 16-aligned
        let fl = self.fl_mut();
        match fl.alloc(need, align.max(MIN_ALIGN)) {                // FreeList::alloc, verified
            Some(off) => (self.mem.get() as *mut u8).add(off),      // the lone raw former; off+need ≤ N by ensures
            None => ptr::null_mut(),                                // OOM / cap / no fit
        }
    }
    ```
  - **`dealloc`** (sketch):
    ```rust
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let need = layout.size().max(1).next_multiple_of(MIN_ALIGN);// identical to alloc's → round-trips
        let off  = (p as usize) - (self.mem.get() as usize);
        let fl = self.fl_mut();
        if fl.is_full() {                                           // Decision 3: leak, never abort a free
            debug_assert!(false, "urt heap: free-list at fragmentation cap; block leaked");
            return;
        }
        debug_assert!(fl.is_allocated(off, need), "urt heap: double free / overlap");
        fl.free(off, need);   // preconditions: nfree<N (is_full checked), n>0, off+n≤N, !covers (is_allocated)
    }
    ```
    `is_full`/`is_allocated` are the verified accessors B4 already added to `FreeList`
    (`dma-pool:334`/`:348`), now reused — the wrapper discharges every `free` precondition at
    the seam exactly as `DmaPool::free` does (B4 Design decision 2). The `is_allocated`
    double-free guard is a `debug_assert` (heap input is trusted in-process — note 4 of the
    verification tier; release rests on `core`'s correctness, the same line dma-pool draws for
    a trusted producer).
  - **Default `realloc`** is inherited from `GlobalAlloc` (alloc-new + copy + dealloc-old) —
    no override; B11C's proptest exercises it through that default path.
- **Depends on:** B11A (the verified `FreeList` must be reachable). No other dependency.
- **Work:** the rewrite above; delete the now-dead `round`/`Block`/`MIN_BLOCK`; update the
  module doc (`:1-11`) to describe the side-stored `FreeList` design and the disclosed
  fragmentation cap; add the cap + leak policy to the crate's MVP-simplification disclosure.
  Rewrite the two inline tests (`:166-191`) against the new allocator (they stay valid:
  `alloc_free_reuse` and `exhaustion_returns_null` are representation-agnostic behaviour).
- **Acceptance:**
  - `cargo verus verify -p urt` green at the **new** total — the heap algorithm is now under
    proof via `FreeList` (the slot/time proofs unchanged). Record the number.
  - `cargo test -p urt` green: the rewritten `alloc_free_reuse` (alloc two, free both, a
    near-full alloc fits again after coalescing) and `exhaustion_returns_null` pass; add a
    direct unit test that a `dealloc` at `HEAP_RANGES` leaks rather than panics (debug build:
    the `debug_assert` fires; release: silent safe leak — assert no abort, space simply not
    reused), and that `align > MAX_ALIGN` returns null.
  - The aarch64 cross-build links every `#[global_allocator] static HEAP: urt::Heap<…>`
    consumer (`user/init`/`shell`/`storaged`/…) and QEMU boot is green — the live witness
    that the new allocator serves real userspace allocation on the boot path.
- **Effort/Risk:** M / medium. The wrapper is small, but it is on every userspace binary's
  hot path, so the boot smoke is the load-bearing acceptance; the fragmentation-cap behaviour
  is the one new semantic to get right.

---

## Sub-phase B11C — wrapper Miri + proptest tier + ledger/baseline update *(closes the audit's "Miri-cover the `urt` heap" follow-up)*

The verified `FreeList` proves the *arithmetic*; B11C proves the **wrapper drivers actually
run** (`alloc → use the bytes → dealloc/realloc`, over a real arena through the `UnsafeCell` +
`base.add(off)` seam) is sound under randomized sequences and Miri — the rev1§6 "everything
gets Miri + proptest" baseline the heap has never met (two happy-path tests, no proptest,
absent from the Miri sweep). Mirrors B4C for the DMA-pool wrapper.

- **Touches:**
  - `urt/Cargo.toml` — `proptest` is already a dev-dep (`:24`); no change unless a new
    `urt/tests/heap_props.rs` is preferred over inline `mod tests` (recommend **inline**: the
    `Heap` is host-constructible with no feature gate, so `cargo +nightly miri test -p urt`
    covers it directly, same posture as B4C's inline choice).
  - `urt/src/lib.rs` `mod tests` — the proptest tier below.
  - `CLAUDE.md` — extend the Miri-sweep command to name `urt` (it already lists
    `cas`/`loader`/`storage-server` and gained `dma-pool` in B4C).
  - `doc/guidelines/verus_trusted-base.md` — the ledger update below.
- **Depends on:** B11A + B11B (it exercises the rewired allocator over real sequences, with
  Miri as the UB oracle).
- **Work:**
  - **Property 1 — alloc/dealloc/realloc round-trip invariants.** proptest a random op
    sequence over a fixed-size `Heap`: each step `alloc(Layout{size, align})` (random small
    `size`, power-of-two `align ≤ MAX_ALIGN`), `dealloc` of a previously-returned live block,
    or `realloc` of one (through the default `GlobalAlloc::realloc`). Maintain a **model** of
    live blocks (`ptr, size, align, written-pattern`). After each `alloc(non-null)`: the
    pointer is `align`-aligned, lies in `[base, base+N)`, and the returned region is
    **disjoint** from every other live block (the wrapper-level corollary of
    `lemma_two_allocs_disjoint`). Write a unique byte pattern through each live block, and
    after every op assert **no live block's bytes were perturbed by any other op** (the
    disjointness the free-list + the arena seam together guarantee) — this is the property a
    pointer-arithmetic bug (overlap, off-by-one split, mis-coalesce) would break.
  - **Property 2 — exhaustion and coalescing.** A sequence that fills the heap asserts `alloc`
    returns null at capacity (never a bad pointer); freeing everything and re-allocating the
    full span succeeds (two-sided coalescing restored the single extent) — the `FreeList`
    behaviour, observed end-to-end through the wrapper.
  - **Property 3 — fragmentation cap never UB.** A deliberately maximally-fragmenting pattern
    (alloc many small blocks, free alternate ones) drives `nfree` toward `HEAP_RANGES`; assert
    that a `dealloc` at the cap **leaks safely** (no abort, no UB — the block's bytes are just
    not re-handed-out) and the allocator keeps serving other requests. Miri is the oracle: a
    mis-handled cap (e.g. an unchecked `FreeList::free` indexing `free[N]`) is an immediate
    Miri error, not a silent pass.
  - **Miri case-count convention.** `#![proptest_config(ProptestConfig { cases: if
    cfg!(miri) { 4 } else { 256 }, ..Default::default() })]` (the workspace convention,
    `cas/src/file.rs:121-123`, B4C). `urt` has no BLAKE3, so Miri is fast; the 4-case floor is
    sweep-time uniformity, not a fidelity limit.
  - **Oracle sanity (negative control, project style).** A `#[cfg(test)]`-gated variant that
    reverts the seam to an *unchecked* offset (or a deliberately overlapping carve) shows
    Property 1's disjointness check fails / Miri reports UB — proving the proptest guards a
    real hole, not a tautology. Document it (as B3B/B4C do) rather than committing the unsound
    variant.
  - **Ledger + rev1§6.1 update.** Move the heap free-list into the verified-surface prose
    ("`urt`'s slot bitmap, seqlock `utc_ns_at`, **and heap free-list**"); update the Baselines
    table line for `urt` to name the heap; record the **arena byte-region seam**
    (`UnsafeCell<[u8;N]>` + `base.add(off)`) as the **trusted plain-Rust boundary**, kept
    honest by the B11C Miri+proptest tier — **structurally identical to the DMA-pool wrapper
    row's posture, and like it requiring no new `external_body`/`assume_specification`** (the
    leak policy and bounds live in plain-Rust wrapper code, not a `verus!{}` seam, so the
    ledger's tally of 13 named constructs is unchanged). If Design decision 2 extracted
    `freelist`, rewrite the "DMA-pool `FreeList`" line as "shared `FreeList` (dma-pool + urt)"
    and add the `freelist` crate to the Baselines table. Record the **fragmentation cap +
    leak-at-cap policy** on the MVP-simplification disclosure (the "test-routed, not
    Verus-mechanized" discipline — here a *disclosed bound*, not a property).
- **Acceptance:**
  - `cargo test -p urt` green including the new proptests at 256 cases natively / 4 under Miri;
    the rewritten heap unit tests and the `slots`/`time` tests unchanged.
  - `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p urt` clean — no UB across
    randomized alloc/dealloc/realloc, exhaustion, and fragmentation-cap sequences (the audit's
    headline acceptance).
  - The oracle-sanity control fails (model mismatch / Miri UB) when the seam's bound is
    reverted — proving the suite guards the real soundness obligation.
  - `cargo verus verify -p urt` (+ `-p freelist`, + `-p dma-pool` ≥ 29/0) green; the ledger
    and rev1§6.1 agree line-for-line on the new verified surface and the trusted arena seam
    (the A5 discipline).
- **Effort/Risk:** S–M / low. Pure test/dev + doc work behind the host-buildable `Heap`; no
  production change beyond B11B.

---

## Execution order

```
B11A  land the verified FreeList where urt can use it     [extract to `freelist` crate (rec.) or copy into urt; no heap change]
   └─► B11B  rewire Heap<N> onto FreeList                  [delete the raw-*mut Block allocator; the headline]
          └─► B11C  wrapper Miri+proptest tier + ledger    [the audit's "Miri-cover the urt heap"; flip the ledger]
```

- **B11A** is foundational and mergeable alone (no heap behaviour change). The extract path
  touches landed dma-pool but is gate-guarded (29/0 re-check); the copy fallback is urt-only.
- **B11B** is the headline change — it *replaces* the unverified allocator, so it depends on
  B11A's verified core and is gated by the QEMU boot smoke (the heap is on every userspace
  binary's path).
- **B11C** depends on both (it exercises the rewired wrapper over randomized sequences with
  Miri as the UB oracle) and is where the ledger/rev1§6.1 flip from "trusted/test-routed" to
  "verified (algorithm) + trusted-seam (arena bytes)".
- Unlike B4 (whose B4A/B4B were independent), B11's sub-phases are a **strict chain** — there
  is no behaviour-preserving way to test the new wrapper (B11C) before it exists (B11B), nor
  to rewire (B11B) before the verified core is reachable (B11A).

## Out of scope for B11 (recorded so it is not mistaken for a gap)

- **A direct Verus proof of the intrusive `*mut Block` list.** The pointer-graph,
  `PointsTo`-per-node lift (`verus.md` §1) Open Decision 5 calls disproportionate; B11
  *avoids* it by switching the representation, it does not attempt it. The intrusive allocator
  is deleted, not verified.
- **Eliminating the fragmentation cap.** The side-stored model's `HEAP_RANGES` ceiling is the
  disclosed trade of the Verus path (Design decision 3); a generous cap + safe leak-at-cap is
  the MVP posture. The `free_or_coalesce` refinement (shrink the leak window) and a fully
  unbounded verified allocator are future hardening, recorded so the bound is a decision.
- **A cargo-fuzz target for the heap.** The allocator decodes nothing — its input is `core`'s
  own `(ptr, layout)` calls inside one trusted process, not untrusted wire/disk bytes — so the
  rev1§3.7 "decoders are fuzz targets" routing does not apply (note 4 of the verification
  tier). proptest + Miri is the correct tier; the fuzzed decoders are elsewhere
  (cas/ipc/loader).
- **Loom/Shuttle for the heap.** Single-threaded by construction (`unsafe impl Sync`,
  `lib.rs:54`); no second mutator, no atomic — nothing for a weak-memory model to witness.
  `urt`'s seqlock (`time.rs`) keeps its existing Loom/Shuttle tier, untouched.
- **Per-allocation metadata / a slab or size-class allocator.** B11 verifies the *existing*
  first-fit allocator's behaviour (re-expressed over extents), it does not redesign the
  allocation policy. A more sophisticated allocator (if a workload ever needs one) is separate
  future work and would carry its own proof.
- **`urt::slots` and `urt::time`.** Already verified; B11 does not touch them. The crate's
  third (and last) unverified chokepoint — the heap — is the whole of B11.
