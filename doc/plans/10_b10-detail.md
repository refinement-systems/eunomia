# Plan — Part B10 detail: aspace page-table pool top-up (a verified monotone `grow_pool` + an `AspaceTopUp` syscall over donated untyped + the three-part error story closed)

Detailed, separately-implementable decomposition of **Phase B10** from
`doc/plans/0_address_audit_rev0.md`. B10 is the Wave-3 kernel item that completes rev1§2.5's
**pool-at-creation** address-space story. The spec blesses a *three-part* contract — the kernel
"draws intermediate page tables from the aspace's own pool, returns `NEED_MEMORY` when the pool is
exhausted, **accepts top-ups**, and returns the pool with the object at teardown"
(`spec_rev1.md:107`). The running kernel implements **two** of the three: `NEED_MEMORY` on
exhaustion (`MapError::NeedMemory`, `kcore/src/aspace.rs:88` → `ERR_NOMEM`, `syscall.rs:580`) and
return-at-teardown (the pool is part of the AspaceObj carve, reclaimed when the donor untyped is
revoked — `kernel/src/aspace.rs:130`). The **middle part is absent**: `pool_pages` is set once at
retype (`kernel/src/aspace.rs:72`, written inside `init`) and never grows, so an aspace that
exhausts its pool returns `NEED_MEMORY` **permanently** — there is no top-up syscall (the `Sys` enum
stops at opcode 23, `kcore/src/sysabi.rs:188`). B10 adds the top-up, making `NEED_MEMORY` a
*recoverable* condition: donate more untyped, grow the pool, continue mapping.

Unlike B8 (verification-only, behaviour-identical) and like B9 (which added the `EAGAIN` ABI), **B10
adds a new syscall** — `AspaceTopUp` — and therefore touches the **verified syscall decoder**
(`kcore::sysabi::decode`, `:110`): one new opcode arm and the `nr >= 24 ==> UnknownCall` bound
(`:113`) moves to `nr >= 25`. That decoder change is the only widening of the verified ABI surface;
the new growth logic itself is a small verified `kcore` op plus trusted carve/accounting shell. This
is recorded up front as honesty note 1 so no reviewer expects the byte-for-byte ABI stability B8 had.

**Closes (from the parent plan):**
- **M-2 [medium] — aspace pool top-up.** rev1§2.5's pool-at-creation contract has three parts;
  today's code does two. `AspaceObj.pool_pages` (`kcore/src/aspace.rs:103`) is set once at retype
  (`kernel/src/aspace.rs:67-74`) and the table allocator `alloc_table` (`kcore/src/aspace.rs:626`)
  returns `NeedMemory` the instant the bump cursor hits the slice length (`:650-651`), with no path
  to extend it. The audit's M-2 is exactly this: "an exhausted pool returns `NEED_MEMORY`
  permanently … rather than today's two parts." B10 adds a top-up that grows the pool from donated
  untyped, honoring accounting (the top-up is funded by the caller's untyped and returned at
  teardown by the same revoke that frees the object).

**Conforms rev1§2.5 (pool-at-creation "accepts top-ups").** B10 is a *conformance* phase: rev1§2.5
already blesses "accepts top-ups" as the target (Part A is blessed first), so B10 brings the code
into conformance and does **not** soften the spec. The verified page-table model already in the
trusted base (the `pt_wf` tree invariant, the `pool_index_spec` addressing primitive, the verified
walker `map_in`/`unmap_in`/`range_mapped_in`) is **preserved**; B10 adds one verified operation that
extends the pool monotonically and mechanizes that the extension keeps `pt_wf` and every existing
mapping intact.

**Spec target (blessed in rev1 — B10 conforms code to it; no normative spec edit, see honesty note 4):**
- **rev1§2.5 "Memory: frames, mappings, and DMA"** (`spec_rev1.md:107`) — *"An address-space object
  … is **pool-at-creation**: the kernel draws intermediate page tables from the aspace's own pool,
  returns `NEED_MEMORY` when the pool is exhausted, **accepts top-ups**, and returns the pool with
  the object at teardown. This gives one error path and a trivial allocator."* The exact claim B10
  makes true. The same paragraph fixes the contiguity premise B10's growth model leans on:
  *"Frames are retyped from untyped, at 4 KiB and **larger contiguous sizes** (contiguity is free
  from retype)"* — the property that lets a top-up carve a contiguous extension of the pool.
- **rev1§6.1(c) "Cap-to-page-table correspondence, mapping, and clearing"** (`spec_rev1.md:417`) —
  the page-table walker and the cap-side map/unmap are verified; *"the real writing and clearing of
  page-table entries is proven separately over raw page-table memory"*; the **join** (cap-recorded
  coordinates ↔ true entry) stays **[trusted]**. B10 adds the pool-extension op **inside** the
  already-verified walker's model (`pt_wf` over the table pool) — it neither flips nor weakens (c);
  it records, in the ledger's verified-surface scope paragraph, that the pool now *grows* under a
  verified invariant-preserving op alongside `map_in`/`unmap_in` (honesty note 4).
- **rev1§2.7 "The syscall boundary"** (`spec_rev1.md:129-135`) — *"the same untrusted-decode
  discipline … every `nr` outside the defined range is `UnknownCall`."* B10's new `AspaceTopUp`
  opcode extends the defined range under this discipline; the decode stays total (unknown → error,
  never crash) and its `ensures` is re-established with the new arm (honesty note 1).

Because Part A is blessed first, **B10 makes no normative spec edits** — rev1§2.5/§2.7/§6.1 are the
fixed targets. The only doc touches are the A4-style ledger updates: the verified-surface scope
paragraph gains the pool-growth op, and the kcore baseline rises (honesty note 3).

**Primary files:**
- `kcore/src/aspace.rs` — the **verified core**:
  - `AspaceObj` `:98-105` (`pool_base` `:102`, `pool_pages` `:103`, `pool_used` `:104`) and
    `bytes_for` `:112-114` — the object whose pool B10 grows. (Design decision 1 leaves the **fields
    and layout unchanged** under the adopted contiguous model; under the rejected segmented model
    they would gain a segment directory.)
  - `pt_wf` `:448` / `pt_wf_leveled` `:468-510` — the tree well-formedness invariant, **parameterized by
    `pool_used` (high-water mark) and `pool_len` (slice length)**. The load-bearing observation:
    every clause already quantifies over `pool_len`, so *growing `pool_len`* (with the new tables
    zeroed and `pool_used` unchanged) is a monotone widening B10A proves preserves `pt_wf`. The
    accounting clause `pool_used <= pool_len` `:479`, the `leaves ⊆ [0, pool_used)` clause `:480`,
    and the inner/leaf-resolution clauses `:483-510` are all stable under the extension.
  - `pool_index_spec` `:335-345` — the affine PA↔index primitive (`(pa - pool_base)/PAGE`, `None`
    past `pool_len`). Under the adopted model **this is unchanged** (the extension keeps one
    `pool_base`); under the rejected segmented model it would generalize to a piecewise-affine search.
  - `pool_geom_ok` `:519-521` — the pool's address-geometry precondition (`pool_base` page-aligned,
    `pool_base + pool_len*PAGE` overflow-free); B10A's `grow_pool` re-establishes it for the grown
    `pool_len`.
  - `alloc_table` `:626-654` (`NeedMemory` at `:650-651`) — the bump allocator; **unchanged** —
    after a top-up grows `pool_pages`, the same allocator simply has more slots before it returns
    `NeedMemory`.
  - `map_in` `:1497`, `unmap_in`, `range_mapped_in` — the verified walker; **unchanged** (it already
    works over an arbitrary `pool_len`).
  - **New:** `grow_pool` — the verified monotone pool-extension op (Design decision 1).
- `kernel/src/aspace.rs` — the trusted int→ptr shell:
  - `init` `:48` (the `AspaceObj` write `:67-74`, `pool_base = base + 2*PAGE` `:71`, `pool_pages`
    `:72`) — the set-once creation B10 leaves intact; top-up grows the value `init` set.
  - `pool_view` `:38-43` (builds `&mut [[u64;512]]` from `pool_base`/`pool_pages`) and `map` `:84-101`
    (rebuilds `pool_view` each call from the **current** `pool_pages`) — the key shell fact: once
    `pool_pages` grows, `map`/`pool_view` automatically see the larger pool, so **no map-path change
    is needed**.
  - **New:** `grow_pool` shell wrapper (zero the new pages, call the verified `kcore` op, write back
    the grown `pool_pages`) — the twin of `map` `:84`.
  - `destroy_aspace` `:130` (no-op; "the memory (tables included) returns to the donor untyped via
    revoke") — the third part of the three-part story, **already satisfied** for the adopted model
    (honesty note 2).
- `kernel/src/untyped.rs` — `retype` `:29` (the Aspace arm `:91-95`, `aspace::init(p, param)` `:93`);
  the carve that places the AspaceObj contiguously from the untyped watermark. B10B adds the
  **abutment-checked contiguous carve** for the top-up (Design decision 2), reusing `carve`/
  `carve_place`.
- `kcore/src/untyped.rs` — `retype_check` `:150` (reads the untyped `base`/`size`/`watermark`),
  `carve`/`carve_place` `:369`/`:296` (verified placement), `retype_install` `:470` (advances the
  watermark, installs the CDT child) — the verified untyped vocabulary B10B's abutment carve reuses;
  the **watermark-monotone / disjointness** model (`:3-7`) is what makes the topped-up pages a
  proper CDT descendant reclaimed at teardown.
- `kernel/src/syscall.rs` — the `Sys::Map` handler `:543-584` (the `NeedMemory → ERR_NOMEM` arm
  `:580` — the recoverable condition top-up answers); the errno block `:59-73`; the `Sys::Retype`
  handler `:216-244` (the carve template). **New:** the `Sys::AspaceTopUp` handler.
- `kcore/src/sysabi.rs` — `enum Sys` `:43`, `decode` `:110`, the `nr >= 24 ==> UnknownCall` bound
  `:113`, the opcode arms `:128-189` (last is `23 => UntypedReset` `:188`). B10B adds
  `Sys::AspaceTopUp { aspace, ut, pages }` (opcode 24), a decode arm, and moves the bound to
  `nr >= 25` — **the one verified-decoder change** (honesty note 1).
- `ipc/src/sys.rs` — the userspace libcall surface: the errno block `:6-20`, `retype` `:140`,
  `map` `:236`. **New:** an `aspace_topup(aspace, ut, pages)` libcall (opcode 24) and an optional
  `map_grow` convenience that, on `ERR_NOMEM`, tops up and retries the map.
- `doc/guidelines/verus_trusted-base.md` — the verified-surface scope paragraph `:18-38` (add "the
  pool-growth op `grow_pool`, which extends the table pool under a verified `pt_wf`-preserving and
  mapping-stable invariant" beside the aspace walker `:28`); the Baselines `:130-138` (raise the
  kcore total `:136` above 381); the `AspaceObj::bytes_for` `assume_specification` row `:105` stays
  unchanged (top-up reuses the same size helper).

Secondary: `kcore/src/test_store.rs` / the aspace host tests (a synthetic aspace whose pool is
exhausted by repeated `map_in`, then `grow_pool`'d, then maps again; a `grow_pool` that preserves
every prior lookup; a non-abutting top-up refused cleanly); `loader/src/spawn.rs:57`
(`retype(untyped, OBJ_ASPACE, 16, …)` — the creation site, **unchanged by B10**; topupable server
aspaces are a funding-convention follow-on, see Design decision 2 and Out of scope).

---

## Verification tier & baseline (applies to all sub-phases)

B10's verified work is a single tier: the **`kcore` Verus chokepoint** (rev1§6 routing — the
page-table walker and syscall decode are Verus). The new `grow_pool` op and the extended `decode`
join the `cargo verus verify -p kcore` gate; the carve abutment, the handler, the accounting, and
the libcall are trusted shell (rev1§6.1(c)/(d), the same int→ptr posture as `retype`). Five honesty
notes up front:

1. **B10 adds a syscall and therefore touches the verified decoder — but nothing else in the ABI
   shifts.** The new `AspaceTopUp` opcode (24) means `kcore::sysabi::decode` gains one arm and its
   `nr >= 24 ==> UnknownCall` `ensures` `:113` becomes `nr >= 25`; the decode stays **total** over
   all `(nr, args)` (unknown → `UnknownCall`, never a panic) per rev1§2.7, and the new arm packs
   three `u64` args with no field that needs a range `ensures` (unlike `ThreadStart`'s `prio`). This
   is the only widening of the verified ABI. Existing opcodes 0..=23, their decode `ensures`, and
   every existing handler are byte-for-byte unchanged — `storaged`/`init`/`shell`/`loader` see
   identical signatures for everything they already call. The regression gate is therefore **the
   QEMU boot still green** plus the new top-up exercised, not full ABI immutability.

2. **The third part of the three-part story — return-at-teardown — is already satisfied; B10 must
   only keep it true.** Under the adopted contiguous model (Design decision 1) the topped-up pages
   are carved from the caller's untyped and become part of the same contiguous region the AspaceObj
   occupies; `destroy_aspace` is a no-op (`kernel/src/aspace.rs:130`) because revoking the donor
   untyped reclaims the whole region — the grown pool included — via the existing
   watermark/CDT/`reset` machinery (`kcore/src/untyped.rs:3-7`). So B10 writes **no new teardown
   code**; B10C's job is to *verify by test* that an aspace topped up from an untyped, then revoked,
   returns the grown bytes (accounting closes). Recorded so the absence of a teardown code change is
   not read as a gap.

3. **The gate is a floor that rises; no existing proof is weakened.** `cargo verus verify -p kcore`
   is **381/0** today (ledger `:136`). B10A adds verified items — `grow_pool` and its
   `pt_wf`-preservation + mapping-stability lemma — and B10B adds one decode arm (re-establishing
   `decode`'s `ensures` at the new bound), so the count goes **above 381** (record the new total in
   the ledger). The seven `external_body` + six `assume_specification` seams (ledger `:107-114`,
   including `AspaceObj::bytes_for` `:105`) are **untouched** — B10 adds verified ops, it does not
   widen the trusted base. The verified walker (`pt_wf`/`pool_index_spec`/`map_in`/`unmap_in`/
   `range_mapped_in`) is **reused unchanged**; that reuse is the whole reason B10 is M/medium and not
   a page-table-model rewrite (Design decision 1).

4. **No §6.1 `[verifying]` flip — B10 is a conformance + additive verified-surface gain (like B8C's
   ready queue).** rev1§6.1(c) carries no `[verifying]` tag for pool growth; the walker is already
   verified and the page-table-write **join** stays [trusted] regardless. So B10 makes **no
   normative §6.1 edit**: `grow_pool` is a *new verified op inside the already-verified model*, not a
   trusted seam being drawn in. B10 records the gain in the **ledger** alone — adding `grow_pool` to
   the verified-surface scope paragraph (`:28`, beside the aspace walker) and bumping the baseline.
   The page-table-write join (c) and the carve/accounting shell stay trusted exactly as today.

5. **The growth model is a load-bearing design choice — flagged for sign-off (Design decision 1).**
   The verified page-table model addresses pool tables via a single affine map
   (`pool_index_spec`, `pool_base + idx*PAGE`), so "grow the pool" is fundamentally "how does the
   pool stay addressable as it grows." The adopted answer — a **contiguous** extension carved to
   abut the pool's current end — keeps the verified walker untouched (the cheap, spec-faithful path)
   at the cost of a caller-side funding contract (the top-up untyped's free region must abut the
   pool). The principled alternative — a **segmented** pool — removes that contract but reworks the
   audit-blessed addressing model (M–L / high). B10's effort rating and touch surface depend on this
   choice; it is the one decision to confirm before B10A starts.

**Baseline to re-establish at end of B10:**
- `cargo verus verify -p kcore` ≥ **381/0**, **> 381** after B10A/B10B (record the new total in the
  ledger). The `external_body` + `assume_specification`s unchanged.
- The aarch64 build boots: `cd kernel && cargo build` + the QEMU boot smoke pass; an aspace that
  exhausts its pool can be topped up and continue mapping (the M-2 acceptance, exercised by a
  synthetic harness — functional, not just compiling).
- `cargo test -p kcore` green (the `grow_pool` host units: exhaust→topup→map; lookup stability;
  non-abutting refusal); `cargo build -p ipc` and the user binaries build against the new libcall.
- The ledger scope paragraph names `grow_pool`; the kcore baseline `:136` reflects the final total;
  no §6.1 prose changed.

---

## Design decision 1 — the pool growth model: a verified **contiguous** extension vs. a **segmented** pool *(the crux — resolve before B10A; flagged for sign-off)*

The pool is a single contiguous region: `pool_base = aspace_base + 2*PAGE`
(`kernel/src/aspace.rs:71`), `pool_pages` tables laid out after the header+L1, and the verified model
maps a descriptor's physical address back to a pool index by the **affine** formula
`(pa - pool_base)/PAGE`, valid iff the result is `< pool_len` (`pool_index_spec`,
`kcore/src/aspace.rs:335-345`). The entire `pt_wf` tree invariant (`:448-510`) and the verified
walker are built on this one-base addressing. "Top-up" is therefore not primarily an allocator
question — `alloc_table` is already a trivial bump cursor — it is the question of **how the pool
remains addressable under this model as it grows.** Three answers:

- **Adopted — contiguous extension: carve the top-up to abut the pool's current end, grow
  `pool_pages` (= `pool_len`), prove the widening preserves `pt_wf` and every existing mapping.**
  Concretely:
  1. **`kcore::aspace::grow_pool`** — a verified op that takes the current `(l1, pool, pool_base,
     pool_used, old_len)` and a count `add` of fresh **zeroed** tables physically contiguous at
     `pool_base + old_len*PAGE`, and yields `pool_len == old_len + add`. `requires`:
     `pt_wf(l1, pool, pool_base, pool_used, old_len, ·)`, `pool_geom_ok(pool_base, old_len + add)`
     (the grown region stays page-aligned and overflow-free, `:519-521`), and the appended tables are
     zero. `ensures`: `pt_wf(l1, pool, pool_base, pool_used, old_len + add, ·)` **with the same
     `leaves` and the same `pool_used`** (the new tables are unused), and — the load-bearing
     stability fact — `forall|va| pt_lookup(l1, pool, pool_base, va)` is **unchanged** (every
     existing mapping resolves identically, because every live descriptor targets a table
     `< pool_used <= old_len`, untouched by extending the tail).
  2. **Why this is cheap.** `pt_wf` is *already* quantified over `pool_len` (`:479`, `:480`,
     `:483-510`), and `pool_index_spec` only widens its accept set as `pool_len` grows — no existing
     descriptor (all pointing `< pool_used`) changes resolution. So `grow_pool` is a **monotone
     widening lemma**, not a model change: the affine primitive, the walker, `alloc_table`, and the
     leaf/inner clauses are reused verbatim. This is the reason B10 stays M/medium.
  3. **The shell side** (`kernel/src/aspace.rs`, trusted int→ptr): a `grow_pool` wrapper zeroes the
     `add` new pages at `pool_base + old_len*PAGE`, calls the verified op over the slice views, and
     writes back `(*this).pool_pages = old_len + add`. Because `map`/`pool_view` (`:38-43`, `:84`)
     rebuild the slice from the **current** `pool_pages` every call, the existing map path
     automatically sees the larger pool with **no map-path edit** (honesty note: this is the single
     biggest reason the change is contained).
  4. **The contiguity contract** (Design decision 2 wires it): the top-up carves `add` pages to abut
     `pool_base + old_len*PAGE`, checked in the trusted carve shell against the donated untyped's
     watermark. This is the one real cost — the caller must supply an untyped whose free region abuts
     the pool (i.e. the aspace's pool is at the tail of a region with headroom).
  - **Decisive reasons:** (a) it keeps the audit-blessed page-table model (`pt_wf`,
    `pool_index_spec`, the walker) **untouched** — the parent plan's "self-contained / M / medium";
    (b) it is the most faithful reading of rev1§2.5's "**trivial allocator**" and "**the pool**"
    (singular) — the grown pool is still one contiguous region, one base, one bump cursor; (c) the
    third part of the story (return-at-teardown) falls out for free — the contiguous extension is
    reclaimed by the same untyped revoke that frees the object (honesty note 2); (d) rev1§2.5's
    "contiguity is free from retype" is exactly the premise the carve relies on.
- **Rejected — segmented pool: a directory of `(seg_base, seg_len)` segments, each a separately
  donated contiguous region, with a piecewise-affine `pool_index_spec`.** Generalize `AspaceObj` to
  carry a bounded segment directory; `pool_index_spec` searches segments to map a PA to a global
  index; `map_in`/`unmap_in`/`range_mapped_in` index `pool[seg][local]` instead of a single slice;
  `pt_wf` re-states every clause over the segment sequence; the kernel passes a slice-of-segments
  rather than one `&mut [[u64;512]]`.
  - **Why rejected:** it removes the contiguity contract (any donated untyped works, growth is
    effectively unbounded) but **reworks the entire audit-blessed addressing model** — the affine
    primitive becomes a bounded search on the hot lookup path, and every `pool[idx]` access and every
    `pt_wf` clause changes. That is M–L / high and risks churning the page-table proofs B7/B8 just
    stabilized — disproportionate to closing M-2. Kept as the principled fallback **iff** the
    contiguity contract is judged unacceptable for the real top-up callers (then B10 is rescoped to
    L/high and the segment directory replaces fields `:102-104`).
- **Rejected — copy-rebase: carve a fresh larger contiguous pool, copy the live tables, rewrite the
  descriptors to the new base.** Keeps one contiguous pool without a funding contract, but (a) the
  original pool is mid-AspaceObj (after the header+L1) and cannot be individually freed — it is
  orphaned until teardown, wasting the bytes; (b) it requires a bounded descriptor-rewrite walk
  (rebasing every L1 entry and every inner-table entry that points into the pool, leaving frame PTEs
  alone) plus re-proving the rebased tree is `pt_wf` over the new base — delicate proof for no
  benefit over the adopted extension; (c) "returns the pool at teardown" becomes ambiguous. Strictly
  worse than the contiguous extension.

**Recommendation: adopt the contiguous extension — a verified `grow_pool` monotone-widening op
(reusing `pt_wf`/`pool_index_spec`/the walker unchanged) plus an abutment-checked carve. Confirm the
contiguity contract is acceptable for the intended top-up callers; fall back to the segmented model
only if it is not.** This is the load-bearing sign-off (honesty note 5).

---

## Design decision 2 — the top-up syscall surface: an `AspaceTopUp` opcode + an abutment-checked contiguous carve + caller-funded accounting *(resolve in B10B)*

Top-up must be a new syscall (the only mutator of `pool_pages` after creation), funded by donated
untyped, with the new pages a proper CDT descendant so revoke reclaims them.

- **Adopted — `Sys::AspaceTopUp { aspace, ut, pages }` (opcode 24); the handler carves `pages`
  tables to abut the pool, calls `grow_pool`, and leaves the new region parented to the untyped.**
  Concretely:
  1. **Decode** (`kcore/src/sysabi.rs`): add `AspaceTopUp { aspace: u64, ut: u64, pages: u64 }` to
     `Sys` `:43`; add `24 => Sys::AspaceTopUp { aspace: a[0], ut: a[1], pages: a[2] }` to the arms;
     move the `nr >= 24` bound `:113` to `nr >= 25`. No field needs a range `ensures` (the carve and
     `grow_pool` validate `pages` against the untyped's remaining bytes and `pool_geom_ok`). The
     decode stays total — the rev1§2.7 discipline (honesty note 1).
  2. **The abutment carve** (`kernel/src/untyped.rs` / the handler, trusted int→ptr): read the
     untyped's `(base, size, watermark)` via `retype_check` `:150`; require
     `base + watermark == pool_base + pool_pages*PAGE` (the untyped's free pointer **abuts** the
     pool's current end) and `watermark + pages*PAGE <= size` (room). On mismatch return a clean
     errno (`ERR_ARG` for non-abutting, `ERR_NOMEM` for no room) — **refuse, never panic**
     (rev1§2.7). On success, `carve`/`carve_place` (`:369`/`:296`) place the `pages` tables exactly
     at `pool_base + pool_pages*PAGE` and the watermark advances by `pages*PAGE` (`retype_install`
     `:470` discipline) — so the new region is, by the same watermark/CDT machinery as any retype, a
     **CDT descendant of `ut`**.
  3. **The growth** (the handler): zero the new pages, call `kcore::aspace::grow_pool` over the slice
     views, write back `pool_pages += pages` (the `grow_pool` shell wrapper, Design decision 1.3).
  4. **The handler** (`kernel/src/syscall.rs`): resolve `aspace`/`ut` slots (`ERR_BADSLOT` on null),
     destructure `CapKind::Aspace`/`CapKind::Untyped` (`ERR_TYPE` otherwise), check the aspace cap's
     `WRITE` right (`ERR_PERM`, as `Sys::Map` does `:557`), then the abutment carve + `grow_pool`,
     mapping results to the existing errno set (`0`, `ERR_ARG`, `ERR_NOMEM`, `ERR_PERM`). No new
     errno is needed.
  5. **The libcall** (`ipc/src/sys.rs`): `aspace_topup(aspace, ut, pages) -> i64` (opcode 24); plus
     an optional `map_grow(aspace, ut, frame, va, perms, step)` convenience that, on `map` returning
     `ERR_NOMEM`, calls `aspace_topup(aspace, ut, step)` and retries the map — the userspace shape of
     the recoverable-`NEED_MEMORY` story.
  - **Decisive reasons:** (a) one opcode, one handler, reusing the verified `carve`/`retype_check`
    vocabulary and the existing errno set — minimal new surface; (b) the new pages are funded by the
    caller's untyped and parented to it, so accounting and teardown ride the existing untyped CDT
    (honesty note 2) — no new lifecycle; (c) the abutment check is the trusted-shell discharge of
    `grow_pool`'s "contiguous at `pool_base + old_len*PAGE`" precondition (the int→ptr fact, §6.1(c),
    trusted exactly as `retype`'s placement is).
- **Rejected — fund the top-up from a fresh untyped placed anywhere (no abutment).** Removes the
  contiguity contract but breaks `grow_pool`'s single-base precondition — it *is* the segmented model
  (Design decision 1's rejected branch). Not a separable option.
- **Rejected — mint a new "pool-extension" cap per top-up.** Adds a cap kind and CDT bookkeeping for
  no benefit: the pool is internal to the aspace (rev1§2.5 gives up "per-table caps … nothing in
  this design needs"); the topped-up bytes are accounted by the funding untyped, not a new cap.

**Recommendation: add `Sys::AspaceTopUp` (opcode 24); the handler does an abutment-checked contiguous
carve from the donated untyped, then `grow_pool`; reuse the existing errno set; add the
`aspace_topup`/`map_grow` libcalls. The abutment check is the trusted discharge of the verified op's
contiguity precondition.**

---

## Design decision 3 — accounting & teardown: ride the untyped CDT, verify by test *(resolve in B10C)*

rev1§2.5's third part — "returns the pool with the object at teardown" — and "honor accounting
(top-up funded by the caller's untyped)" must hold across top-up.

- **Adopted — the topped-up pages are CDT descendants of the funding untyped; teardown is the
  existing revoke; B10 verifies the property by test, writing no new teardown code.**
  - **Accounting:** the abutment carve advances the funding untyped's watermark by `pages*PAGE`
    (Design decision 2.2), so the bytes are debited from the caller's untyped exactly like a retype —
    the rev1§2.5 "no kernel allocation that is not user-accounted" principle holds unchanged.
  - **Teardown:** because the extension is contiguous with the AspaceObj's region **and** carved from
    the (same or a descendant) untyped that funds the aspace, revoking that untyped — the canonical
    whole-child teardown (rev1§2.2, the parent loop "revoke the donated untyped") — reclaims the
    object and the grown pool together. `destroy_aspace` stays a no-op (`kernel/src/aspace.rs:130`);
    `UntypedReset` (`syscall.rs:756`) / revoke handle the bytes via the watermark model.
  - **Verification:** B10C adds a host test that a topped-up aspace's funding untyped, after revoke +
    reset, reports its watermark/free bytes restored to include the topped-up pages (accounting
    closes), and a QEMU smoke that a server aspace topped up at runtime is fully reclaimed at child
    teardown.
  - **Decisive reasons:** the existing untyped watermark/CDT/`reset` machinery already implements
    "return at teardown"; top-up only has to *not break* it, which the abutment carve guarantees by
    construction (the pages are an ordinary watermark advance on a CDT-parented untyped).
- **Rejected — a dedicated pool-return path in `destroy_aspace`.** Unnecessary: the no-op is correct
  because revoke owns reclamation; adding a return path would duplicate the watermark machinery and
  risk double-free.

**Recommendation: no new teardown code; the topped-up pages ride the funding untyped's CDT and are
reclaimed by the existing revoke/reset. B10C proves accounting closes by test (exhaust→topup→revoke
returns the grown bytes).**

---

## Sub-phase B10A — verified `grow_pool` monotone pool extension *(closes M-2's verification core; conforms rev1§2.5)*

The Verus deliverable. Adds the verified `grow_pool` op proving that extending the table pool with
zeroed contiguous tables preserves `pt_wf` and every existing mapping — reusing the affine
addressing, the walker, and `alloc_table` unchanged. Independent of B10B's shell wiring (B10B
consumes its signature).

- **Touches:**
  - `kcore/src/aspace.rs` — add `grow_pool` (Design decision 1.1): `requires`
    `pt_wf(.., old_len, ·)` + `pool_geom_ok(pool_base, old_len + add)` + the appended-tables-zero
    fact; `ensures` `pt_wf(.., old_len + add, ·)` with `pool_used`/`leaves` unchanged **and**
    `pt_lookup` pointwise-stable for all `va`. Prove the monotone-widening lemma over the existing
    `pt_wf_leveled` clauses `:468-510` (each already quantified over `pool_len`); cite
    `pool_index_spec` `:335` (accept-set only widens) for the stability fact. Leave `alloc_table`
    `:626`, `map_in` `:1497`, `unmap_in`, `range_mapped_in`, `pt_wf`, `pool_index_spec` **unchanged**.
  - `kernel/src/aspace.rs` — add the `grow_pool` shell wrapper (zero the `add` new pages at
    `pool_base + old_len*PAGE`, build the slice views, call the verified op, write back
    `pool_pages`), the twin of `map` `:84`.
  - `kcore/src/test_store.rs` / aspace host tests — `grow_pool` widens `pool_pages`; a pre-top-up
    `map_in` sequence resolves identically after `grow_pool` (lookup stability); a post-top-up
    `map_in` succeeds in a slot beyond the old `pool_len` that previously returned `NeedMemory`.
  - `doc/guidelines/verus_trusted-base.md` — record the raised kcore total `:136`; add `grow_pool` to
    the verified-surface scope paragraph `:28`.
- **Depends on:** Part A blessed; Design decision 1 signed off (contiguous vs segmented). No intra-B10
  dependency (B10B/B10C consume its signature).
- **Work:** Design decision 1 — the `grow_pool` op + the `pt_wf`-preservation and `pt_lookup`-stability
  lemmas, reusing the `pool_len`-parameterized clauses. The substance is the stability proof (every
  live descriptor targets `< pool_used`, untouched by the tail extension) and re-establishing
  `pool_geom_ok` for the grown length.
- **Acceptance:**
  - `grow_pool` verifies with `pt_wf(.., old_len + add, ·)` preserved (same `pool_used`/`leaves`) and
    `pt_lookup` pointwise-stable; the walker/`alloc_table`/`pool_index_spec` are unchanged.
  - `cargo verus verify -p kcore` **> 381/0** (record the new total); `cargo test -p kcore` green
    (the exhaust→grow→map and lookup-stability units).
- **Effort/Risk:** M / medium. The monotone widening reuses the existing `pool_len`-parameterized
  invariant; the work is the pointwise lookup-stability lemma, not a model change.

---

## Sub-phase B10B — the `AspaceTopUp` syscall + abutment-checked carve + libcall *(closes M-2's recoverable-`NEED_MEMORY` path; conforms rev1§2.7)*

The shell deliverable (trusted int→ptr, §6.1(c)/(d)) plus the one verified-decoder change. Adds the
`AspaceTopUp` opcode, the handler that carves a contiguous extension from the donated untyped and
calls `grow_pool`, and the userspace libcall. Depends on B10A's `grow_pool` signature.

- **Touches:**
  - `kcore/src/sysabi.rs` — add `Sys::AspaceTopUp { aspace, ut, pages }` `:43`; add the
    `24 => …` decode arm; move the bound `nr >= 24 ==> UnknownCall` `:113` to `nr >= 25`;
    re-establish `decode`'s `ensures` (Design decision 2.1) — **the one verified-decoder change**.
  - `kernel/src/untyped.rs` / `kernel/src/syscall.rs` — add the abutment carve (Design decision 2.2):
    `retype_check`-read the untyped, require `base + watermark == pool_base + pool_pages*PAGE` and
    room, `carve`/`carve_place` the `pages` tables, advance the watermark; add the `Sys::AspaceTopUp`
    handler (slot/type/`WRITE`-right validation mirroring `Sys::Map` `:543-573`, then carve +
    `grow_pool`, mapping to `0`/`ERR_ARG`/`ERR_NOMEM`/`ERR_PERM`). No new errno.
  - `ipc/src/sys.rs` — add `aspace_topup(aspace, ut, pages)` `:140`-style; add the `map_grow`
    convenience that tops up and retries on `ERR_NOMEM`.
- **Depends on:** B10A (the `grow_pool` signature). Independent of B10C.
- **Work:** Design decision 2 — the decode arm + bound move, the abutment check + contiguous carve,
  the handler, the libcalls. Confirm the decode stays total (the rev1§2.7 negative case: opcode 25+
  still `UnknownCall`). **No `Sys::Map` change** — the existing `NeedMemory → ERR_NOMEM` arm `:580`
  is the condition top-up now answers.
- **Acceptance:**
  - `AspaceTopUp` decodes (opcode 24) and dispatches; a non-abutting untyped is refused (`ERR_ARG`),
    an undersized one `ERR_NOMEM`, an abutting one grows the pool and returns `0`; opcode 25+ still
    `UnknownCall`.
  - `cargo verus verify -p kcore` **> 381/0** (the decoder re-verifies with the new arm); QEMU boot
    green; a synthetic aspace exhausts its pool (`map` → `ERR_NOMEM`), tops up, and the next `map`
    succeeds (the M-2 acceptance, via `map_grow`).
  - `cargo build` (kernel) + `cargo build -p ipc` + the user binaries build against the new libcall.
- **Effort/Risk:** S–M / low–medium. Mostly the carve/handler wiring + the small decoder change; the
  verified core is B10A's. The judgment is the abutment errno split and the `map_grow` retry shape.

---

## Sub-phase B10C — teardown/accounting conformance + tests + ledger closeout *(closes the three-part story; conforms rev1§2.5)*

The conformance-closeout deliverable. Verifies (by test, no new code) that a topped-up pool is
reclaimed at teardown with accounting intact, and lands the ledger/baseline updates. Depends on
B10A+B10B for the mechanism; can land alongside them so the ledger updates once.

- **Touches:**
  - `kcore/src/test_store.rs` / kernel integration — a host/QEMU test that an aspace topped up from
    an untyped, then revoked + `UntypedReset` (`syscall.rs:756`), restores the untyped's free bytes
    to include the topped-up pages (accounting closes, Design decision 3); a QEMU smoke that a
    runtime-topped-up server aspace is fully reclaimed at child teardown.
  - `doc/guidelines/verus_trusted-base.md` — finalize the kcore baseline `:136` (the B10A+B10B
    total); confirm the verified-surface scope paragraph `:28` names `grow_pool`; **no `[verifying]`
    table edit, no §6.1 spec edit** (honesty note 4).
  - `doc/spec/spec_rev1.md` — **no change** (rev1§2.5 already blesses "accepts top-ups"; honesty
    note 4).
- **Depends on:** B10A + B10B (the mechanism). No new mechanism.
- **Work:** Design decision 3 — the accounting/teardown tests; the ledger baseline + scope-paragraph
  finalization. Confirm `destroy_aspace` stays a no-op and revoke owns reclamation (no double-free).
- **Acceptance:**
  - An exhaust→topup→revoke→reset cycle returns the grown bytes to the funding untyped (accounting
    closes); the QEMU teardown smoke is green.
  - The ledger scope paragraph names `grow_pool`; the kcore baseline reflects the final total; no
    §6.1 prose changed; `destroy_aspace` unchanged.
- **Effort/Risk:** S / low. Tests + docs; the third part of the story rides existing machinery.

---

## Execution order

```
B10A  verified grow_pool monotone pool extension        [Verus core; independent; the long pole]
B10B  AspaceTopUp syscall + abutment carve + libcall     [shell + decoder; depends on B10A signature]
B10C  teardown/accounting conformance + tests + ledger   [tests/docs; depends on B10A+B10B mechanism]
```

- **B10A is the long pole** (the `pt_wf`-preservation + `pt_lookup`-stability proof), though it is
  M/medium precisely because the model is reused, not rebuilt. **B10B depends on B10A's `grow_pool`
  signature** and carries the only ABI change (the new opcode + the decoder arm). **B10C** verifies
  the three-part story closes and lands the ledger updates — land it alongside B10A/B10B so the kcore
  baseline and scope paragraph update once.
- The parent plan sequences **B10 in parallel with B8/B9** ("self-contained") within the kernel wave;
  it is independent of B-IRQ. Sequencing after B8 is courtesy (avoid churning the freshly-verified
  surface), but B10 touches a disjoint part of `kcore` (the aspace pool, not cspace/thread), so the
  ordering is soft.
- **The one sign-off gate is Design decision 1** (contiguous vs segmented, honesty note 5): it sets
  whether B10 is M/medium (adopted contiguous) or M–L/high (segmented fallback) and whether the
  `AspaceObj` fields change. Confirm before B10A.

## Out of scope for B10 (recorded so it is not mistaken for a gap)

- **Reworking the verified page-table addressing model (the segmented pool).** Design decision 1's
  rejected branch. B10 keeps the affine `pool_index_spec` + the `pool_len`-parameterized `pt_wf` and
  the walker unchanged; it adds a monotone-widening op, not a new addressing scheme. Adopt the
  segmented model only if the contiguity contract is signed off as unacceptable (then B10 is
  rescoped).
- **A loader/server funding convention for runtime top-up.** The adopted contiguous model needs the
  top-up untyped's free region to abut the pool (the aspace's pool at the tail of a region with
  headroom). `loader/src/spawn.rs:57` (which carves the aspace then more objects from one untyped) is
  **unchanged** by B10 — B10 delivers the *mechanism* and a synthetic test harness that satisfies the
  contract; arranging it for real long-running server aspaces (init dedicating an untyped per
  topupable aspace) is a small follow-on convention, not B10's mechanism. Recorded so the unchanged
  loader is not read as the feature being unused.
- **Changing `destroy_aspace` / the teardown path.** The third part of the story (return-at-teardown)
  rides the existing untyped revoke/reset (honesty note 2); B10 adds no teardown code, only a test
  that accounting closes.
- **Shrinking / compacting the pool.** rev1§2.5 specifies growth ("accepts top-ups"), not shrink;
  reclamation is whole-object at teardown. A live pool only ever grows; no per-table free.
- **Per-table caps or revocation of individual intermediate tables.** rev1§2.5 explicitly gives these
  up ("nothing in this design needs"). Top-up funds via an ordinary untyped watermark advance, not a
  new cap kind (Design decision 2's rejected branch).
- **Any §6.1 `[verifying]` flip or normative spec edit.** There is none (honesty note 4): rev1§2.5
  already blesses top-ups, the walker is already verified, and the page-table-write join stays
  [trusted]. B10 records the `grow_pool` gain in the ledger scope paragraph + baseline only.
- **The IO-space / IOMMU pool-at-creation analog** (rev1§8.3, `spec_rev1.md:479`). That future
  IO-mapping object "mirroring frame mapping, pool-at-creation" would reuse B10's top-up shape, but
  it is deferred future work, not B10.
- **Tuning the default `pool_pages` (16) or a top-up step size.** Shell policy (the loader's
  `retype(.., OBJ_ASPACE, 16, ..)` and `map_grow`'s `step`), not a verified parameter; B10 keeps the
  default and leaves sizing to measurement.
