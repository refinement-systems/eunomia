# Plan — Part B4 detail: DMA-pool wrapper soundness + verification (extent-guarded CPU access, restored `MAX_FREE_RANGES` backstop + discharged `FreeList` preconditions, wrapper proptest + Miri tier)

Detailed, separately-implementable decomposition of **Phase B4** from
`doc/plans/0_address_audit_rev0.md`. B4 is Wave-1 work: a confirmed soundness hole in
the one crate where physical addresses are visible (`DmaPool<B>`, the type drivers
actually use), plus the verification gap that the public wrapper — unlike the verified
`FreeList<N>` core it wraps — has never been brought under the rev1§6 baseline.

**Closes (from the parent plan):**
- **DMA-pool public-wrapper soundness hole + unverified glue** [audit §4.2, medium; the
  UB hazard → treated as high]. Verbatim from `doc/results/0_audit_rev0.md` §4.2
  (lines 464-473):
  - Only `FreeList<N>` is verified; the type drivers use, `DmaPool<B>`
    (`dma-pool/src/lib.rs:1255-1312`), calls `FreeList::free`/`alloc` **without
    discharging their preconditions** (`spec_nfree() < N`, `off+n <= len`, …), and the
    runtime `assert!(nfree < MAX_FREE_RANGES)` overflow guard was **demoted to a Verus
    precondition with no runtime backstop in the wrapper**.
  - `bytes()/bytes_mut()` (`:1277/:1284`) build raw slices
    `from_raw_parts(cpu_base().add(buf.offset), buf.len)` **with no check that `buf`
    originated from this pool** — a `DmaBuf` (`Copy`, private fields) from a larger pool
    used against a smaller pool's `bytes()` is **out-of-bounds UB**.
- The audit's own follow-up item (§8, line 700): "Miri-cover … the DMA-pool wrapper" —
  the wrapper carries only three happy-path unit tests (`:1369-1416`) and is **absent
  from the workspace Miri sweep** (CLAUDE.md names `cas`/`loader`/`storage-server`, not
  `dma-pool`).

**Spec target (already blessed in rev1 — B4 only conforms code to it):**
- **rev1§2.5** — the DMA pool is "the single place in the system where physical
  addresses appear"; the crate "hands out buffers labeled with opaque device addresses;
  drivers are written against it and never see a physical address," and **"a DMA-capable
  driver is inside the memory-isolation TCB"** (its device is confined by nothing). The
  wrapper *is* that seam, so a memory-safety hole here is a hole in the isolation TCB —
  why the audit's "medium" is treated as high.
- **rev1§6** — Verus routing names "the host chokepoints (the IPC crate, the userspace
  runtime, **the DMA pool**, and the CAS layer)"; they "verify without bound." The
  blessed split is in the module doc (`dma-pool/src/lib.rs:18-35`) and the trusted-base
  ledger (`verus_trusted-base.md:103`, "DMA-pool `FreeList`"): the **`FreeList`
  arithmetic is the verified chokepoint**; the **`DmaPool` wrapper that touches the
  `DmaBacking`/raw-pointer/device-address seam stays plain Rust** — "the honest line,
  since `dma-pool` *is* 'the single place PAs are visible', so the PA/backing boundary is
  exactly the trusted seam" (§6.1 trusted-seam posture). B4 keeps that line: it does not
  drag the raw-pointer seam into `verus!{}`; it **guards** the seam with checked
  arithmetic (verified where it lives in `FreeList`, Miri+proptest-covered where it lives
  in the wrapper).

Because Part A is blessed first (the parent plan's hard dependency), **B4 makes no spec
edits** — the rev1 text above is the fixed target. Every citation here is `rev1§`.

**Primary file:** `dma-pool/src/lib.rs` — the wrapper `DmaPool<B>` (`:1250-1312`:
`alloc` :1261, `free` :1271, `bytes` :1277, `bytes_mut` :1284, `write` :1290, `read`
:1294, `read_volatile` :1306), the `FreeList` accessor additions (inside `verus!{}`,
beside `spec_nfree` :115 / `free` :1116 / `alloc` :337), and the inline `mod tests`
:1369. Secondary: `dma-pool/Cargo.toml` (add the `proptest` dev-dep), and a possible new
`dma-pool/tests/pool_props.rs`. No consumer change: `virtio-blk` and `user/storaged`
keep calling `alloc`/`free`/`bytes`/`read`/`write`/`read_volatile` with the same
(infallible) signatures.

---

## Verification tier & baseline (applies to all sub-phases)

Per rev1§6 routing, **dma-pool is a Verus chokepoint** — unlike B1/B2/B3, B4 *does* touch
the verified surface, so the regression gate is load-bearing. Four honesty notes up front
so nothing is silently dropped or over-claimed:

- **`FreeList<N>` stays Verus-verified; the gate must hold (and should rise).** The
  parent plan's baseline `cargo verus verify -p dma-pool` ≥ **26/0** is a hard regression
  gate. B4B *adds verified exec accessors* to `FreeList` (`is_full`, an `is_allocated`
  region probe — see Design decision 2), so the **count rises above 26**; B4 re-records
  the new number in the trusted-base ledger. No existing `verus!{}` proof is weakened.
- **The wrapper's raw-pointer/backing seam is the *trusted* seam — it gets Miri+proptest,
  not Verus.** Forming `from_raw_parts(cpu_base().add(…), …)` is a raw-pointer hardware-
  seam operation over `DmaBacking` (rev1§2.5: "the single place PAs are visible"); it
  cannot be Verus logic, exactly as the module doc designates (`:18-35`). So B4 does **not**
  pull `DmaPool<B>` into `verus!{}` (that would force `external_body`/`assume` across the
  PA seam, *growing* the trusted surface — the opposite of the crate's design and of
  B7's shrink-the-seam direction). Instead the wrapper's new soundness guards are *checked
  arithmetic* over public scalars (`buf.offset`, `buf.len`, `backing.len()`, the verified
  `is_full`/`is_allocated` results), and that arithmetic is covered by **proptest + Miri**
  (the rev1§6 "everything gets Miri + proptest" baseline the wrapper currently lacks).
- **`assert!`, not `Result` — DMA buffers are trusted-driver input, not untrusted wire.**
  B3 made the ELF loader *refuse-not-crash* with a `SpawnError` because program images are
  untrusted data in the store (rev1§3.7/§5.3). A `DmaBuf` is the **opposite**: it is
  produced by the trusted DMA driver inside the isolation TCB (rev1§2.5), never decoded
  from an adversary. A bad `DmaBuf` (wrong pool, out of extent, double-freed) is therefore
  a **driver bug**, and the correct backstop is a defined panic — exactly the posture of
  the `assert!(nfree < MAX_FREE_RANGES)` the audit found demoted. So B4 restores a hard
  `assert!` (present in release, where the UB lives), keeps the infallible signatures (no
  consumer churn), and does **not** add a fuzz target (the wrapper is not a decoder of
  untrusted input — the rev1§3.7 fuzz routing does not apply; proptest+Miri is the tier).
- **No Loom/Shuttle.** One driver owns one pool for its lifetime; the host `SharedMem`
  backing is single-threaded by contract (`:1333-1335`). There are no atomics in the
  wrapper and no second mutator — nothing for a weak-memory model to witness (same posture
  as B2's driver note). B4 records this rather than adding a no-value harness.

**Coordinate with B2 (same crate, no conflict).** B2 added `DmaPool::read_volatile`
(`:1306`, now in-tree) in the same `cpu_base().add(buf.offset + off)` shape **specifically
so B4's guard drops into one place** (B2's out-of-scope note: "B4 adds the
pool-identity/extent soundness guard … the two phases touch adjacent lines, not the same
logic"). B4A honors that by factoring **one** guarded pointer helper that all raw-pointer
formation routes through — `bytes`, `bytes_mut`, and `read_volatile` (the three that touch
`cpu_base()` directly); `read`/`write` inherit safety because they slice the already-valid
`&[u8]`/`&mut [u8]` `bytes`/`bytes_mut` return.

**Baseline to re-establish at end of B4:** `cargo test -p dma-pool` green (today's 3
inline tests + the new proptests/regressions); `cargo verus verify -p dma-pool` ≥ **26/0**
(higher, with B4B's accessors — record the new total); and a **new** Miri leg
`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p dma-pool` clean (the
wrapper's `unsafe` raw slices + the host `SharedMem` are exactly what Miri validates).
The aarch64 userspace cross-build still compiles (`cd kernel && cargo build` boots
`storaged`, which constructs a `DmaPool` over its `DmaRegion` backing and feeds
`virtio-blk`).

---

## Design decision 1 — closing the provenance hole: extent validation vs. a pool-identity tag, and the failure mode *(resolve in B4A)*

The parent plan offers two shapes: "Add a pool-identity/extent check (**tagged `DmaBuf`**,
**or** validate `offset+len` against this pool's arena)." B4A pins the design.

- **Adopted — extent validation as a single hard-`assert!` guarded pointer helper.** Add a
  private `impl DmaPool<B>` helper that is the *only* place a raw pointer into the backing
  is formed:
  ```rust
  /// The CPU pointer at `buf.offset + offset`, after proving the `len`-byte
  /// access lies wholly inside this pool's backing. This is the one place a raw
  /// pointer into DMA memory is formed (rev1§2.5), so the soundness obligation of
  /// every `from_raw_parts`/`read_volatile` below is discharged here, ONCE, for
  /// any `DmaBuf` — foreign or not. `DmaBuf` is `Copy` with private fields, so a
  /// buffer carved from a *different* pool can reach this method; the checked
  /// bound makes that defined behaviour (a panic — a driver bug, rev1§2.5 TCB),
  /// never the out-of-bounds read/write the audit flagged.
  fn range_ptr(&self, buf: &DmaBuf, offset: usize, len: usize) -> *mut u8 {
      let end = buf.offset
          .checked_add(offset)
          .and_then(|o| o.checked_add(len))
          .expect("dma-pool: buffer range overflows usize");
      assert!(end <= self.backing.len(), "dma-pool: buffer range outside pool arena");
      // SAFETY: end <= backing.len() proven above; cpu_base() points at
      // backing.len() valid bytes (DmaBacking contract), so base..base+? is in-range.
      unsafe { self.backing.cpu_base().add(buf.offset + offset) }
  }
  ```
  `bytes`/`bytes_mut` call `range_ptr(buf, 0, buf.len)` then `from_raw_parts{,_mut}(p,
  buf.len)`; `read_volatile` calls `range_ptr(buf, offset, out.len())`. Decisive reasons:
  1. **It discharges the actual soundness obligation.** The UB is "the raw slice may
     exceed the backing." `end <= backing.len()` is *exactly* the predicate that makes
     `from_raw_parts` sound — proven at the point of use, for **any** `DmaBuf`. Provenance
     becomes irrelevant to *memory safety*: a foreign buf that would overrun is rejected;
     a foreign buf that happens to fit is served safely (in-bounds). The audit's named UB
     scenario — "from a **larger** pool used against a **smaller** pool" — is precisely the
     overrun case, so it is always caught.
  2. **No new fields, no global state, no_std-clean.** `storaged` is `no_std` (+alloc); an
     extent check is pure scalar arithmetic over fields that already exist. A pool-identity
     tag (below) would add an `AtomicU64` id source + a field to both `DmaBuf` and
     `DmaPool` for a guarantee the UB fix does not need.
  3. **One choke, matching B2's anticipation.** All three raw-pointer formers route through
     `range_ptr`, so the guard lives in exactly the "one place" B2 wrote `read_volatile`
     to share.
- **Optional refinement (recommended only if a multi-pool future arrives) — a pool-identity
  tag.** Mint a `PoolId(u64)` from a `static NEXT: AtomicU64` in `DmaPool::new`, stamp it
  into `DmaPool` and into every `DmaBuf` at `alloc`, and `assert!(buf.pool == self.id)` in
  `range_ptr`. This rejects **all** cross-pool use *deterministically* (even a same-size,
  in-bounds foreign buf — a logic error extent validation lets through safely), giving the
  acceptance's "a cross-pool `DmaBuf` is rejected" its strongest reading. **Not required
  for soundness** (the in-bounds-wrong-pool case is not UB), and there is exactly one pool
  per driver today, so B4A treats the tag as documented future hardening, not the fix.
  Recorded so its omission is a decision, not a gap.
- **Rejected — making `bytes`/`read` fallible (`Option`/`Result`).** Ripples `?`/`unwrap`
  through every `virtio-blk` call site (`pool.write`/`pool.read`/`pool.bytes_mut` ×8) for a
  programming-bug class the driver cannot meaningfully recover from — it would just
  `unwrap`. The infallible-signature + hard-`assert!` posture matches the demoted
  `assert!` and keeps consumers untouched.

**Recommendation: adopt the `range_ptr` extent-validation helper (hard `assert!`, infallible
signatures); leave the pool-identity tag as documented optional hardening.**

---

## Design decision 2 — restoring the `free` precondition backstop: runtime-guard via verified accessors vs. pulling the wrapper into Verus *(resolve in B4B)*

`FreeList::free` (`:1116`) carries four real preconditions beyond `wf()`:
`spec_nfree() < N`, `n > 0`, `off + n <= spec_len()`, and `forall p ∈ [off, off+n):
!covers(p)` (the no-double-free / no-overlap guard). The Verus rewrite turned the original
`assert!(nfree < MAX_FREE_RANGES)` into the *static* `spec_nfree() < N` precondition, but
`DmaPool::free` (`:1271`) calls `self.fl.free(buf.offset, buf.len)` in **erased plain Rust**,
where preconditions are no-ops — so on a full list `insert_at` indexes `self.free[N]` →
out-of-bounds panic (the original was a clean `assert!`; the meaning was lost, not the
crash). `alloc` (`:337`) needs only `align > 0`, and returns `None` on a full list, so it is
already total w.r.t. fragmentation; the backstop is a `free`-only concern. Two routes:

- **Adopted — discharge the preconditions at runtime in the wrapper, reading the list state
  through new *verified* exec accessors on `FreeList`.** `spec_nfree`/`spec_len`/`covers`
  are `closed spec fn`s (`:108-128`) — unreachable from exec code — so the wrapper cannot
  test them directly. Add small `verus!{}` exec methods whose `ensures` tie them to the
  spec, then assert in the wrapper:
  ```rust
  // inside verus!{}, on FreeList<N>:
  pub fn is_full(&self) -> (r: bool) ensures r == (self.spec_nfree() == N as int) { self.nfree == N }
  /// True iff [off, off+n) is wholly allocated (no position is in a free extent) —
  /// the exec witness for free()'s `!covers` precondition. O(nfree) over the ≤ N=64
  /// extents; the loop invariant mirrors free()'s own covers-reasoning (:1150-1177).
  pub fn is_allocated(&self, off: usize, n: usize) -> (r: bool)
      ensures r == (forall|p: int| off <= p < off + n ==> !self.covers(p)) { /* scan */ }
  ```
  `DmaPool::free` then becomes:
  ```rust
  pub fn free(&mut self, buf: DmaBuf) {
      assert!(buf.len > 0, "dma-pool: zero-length buffer");                    // n > 0
      assert!(!self.fl.is_full(), "dma-pool: free-list fragmentation cap (MAX_FREE_RANGES)"); // nfree < N — the restored backstop
      assert!(buf.offset.checked_add(buf.len).is_some_and(|e| e as u64 <= self.backing.len() as u64),
              "dma-pool: buffer outside pool arena");                          // off + n <= len  (== spec_len)
      assert!(self.fl.is_allocated(buf.offset, buf.len), "dma-pool: double free / overlap"); // !covers
      self.fl.free(buf.offset, buf.len);
  }
  ```
  Now **every** precondition of `fl.free` is established by a runtime check before the call
  (the parent plan's "or add runtime checks" branch), the demoted overflow guard is
  literally back as an `assert!`, and the `off+n <= len` check is the *same* extent
  predicate B4A's `range_ptr` uses (one notion of "inside the arena", shared). `alloc` gains
  a sibling `assert!(align > 0)` (today only `debug_assert!(is_power_of_two)`, which elides
  in release where `align == 0` would divide-by-zero in `FreeList::alloc`).
- **Why include the `!covers` (double-free) guard, beyond the two the audit names.** B4A's
  extent check stops every *out-of-bounds* access, but it does **not** stop the one residual
  UB chain: a double-free corrupts the free list into overlapping coverage → a later `alloc`
  hands out a buffer overlapping a still-live one → two `bytes_mut` over the overlap are
  aliasing `&mut [u8]` = UB, **both in-bounds** so B4A's guard never fires. `is_allocated`
  closes it cheaply (N ≤ 64). Together, B4A (no OOB) + B4B (no double-free → no overlap →
  no aliasing) make the wrapper a **complete** memory-safety story, not a partial one.
  *(Effort fallback: if the verified `is_allocated` `ensures` proves disproportionate,
  ship a `debug_assert!` over a plain unverified scan and document that release rests on
  driver discipline — the parent plan's "…" latitude. The verified form is preferred; it
  raises the verify count and makes the closure argument mechanical.)*
- **Rejected — pull `DmaPool<B>` into `verus!{}` and discharge statically.** It would
  require Verus contracts (an invariant linking `fl.spec_len()` to `backing.len()`) across
  the raw-pointer/`DmaBacking` seam, forcing `external_body`/`assume` at the PA boundary and
  **growing the trusted surface** — against the module-doc/ledger line (FreeList verified,
  wrapper trusted) and B7's shrink-the-seam direction. The accessors give the same runtime
  guarantee while keeping the verified/trusted boundary exactly where rev1§6.1 draws it.
- **Alternative noted — a verified-total `FreeList::checked_free(off, n) -> bool`** that
  validates internally and no-ops on violation (the wrapper then owes nothing). Higher
  assurance and tidy, but it changes the **full-list posture from panic to silent leak**
  (the buffer stays "allocated" forever) — strictly worse than the original `assert!` for a
  bounded driver pool, where hitting the fragmentation cap is pathological and should fail
  loud. Offered as the upgrade path if a future caller wants a non-panicking pool.

**Recommendation: adopt the verified-accessor + hard-`assert!` backstop (restores the
demoted guard, discharges all four `free` preconditions at the seam, keeps the trusted
boundary fixed); include the `is_allocated` double-free guard to close the aliasing chain.**

---

## Sub-phase B4A — extent-guarded CPU access *(closes the provenance / OOB-UB hole)*

The headline soundness fix. Self-contained and mergeable alone: after B4A no `DmaBuf` —
whatever pool it came from — can make `bytes`/`bytes_mut`/`read`/`write`/`read_volatile`
form a raw slice outside this pool's backing. Atomic by necessity: the helper and its three
call sites are one change (the guard must cover every raw-pointer former at once, or the
hole stays open on the unguarded one).

- **Touches:** `dma-pool/src/lib.rs`
  - add the private `range_ptr(&self, buf, offset, len) -> *mut u8` helper (Design
    decision 1) just above `bytes` `:1277`;
  - `bytes` `:1277` / `bytes_mut` `:1284` — form the pointer via `range_ptr(buf, 0,
    buf.len)`, keep the `from_raw_parts{,_mut}(p, buf.len)`;
  - `read_volatile` `:1306` — replace the inline `cpu_base().add(buf.offset + offset)` with
    `range_ptr(buf, offset, out.len())` (so the spin-polled path is bounds-checked too);
  - `read` `:1294` / `write` `:1290` — **no change**: they slice the already-validated
    `&[u8]`/`&mut [u8]` from `bytes`/`bytes_mut`, so the sub-range `[offset..offset+len]` is
    a safe Rust slice index (panics on overrun, never UB). Add a one-line comment noting the
    safety is inherited.
- **Depends on:** Part A blessed (rev1§2.5 text). No intra-B4 dependency. Coordinates with
  B2 (the `read_volatile` it guards is B2's, already in-tree).
- **Work:** the helper as in Design decision 1; thread it through the three raw-pointer
  formers; keep the existing volatile-correctness doc comments (`:1278-1281`, `:1298-1305`)
  — the rev1§2.5 cache-maintenance/compiler-hazard split is B2's and is untouched.
- **Acceptance (regression tests in `mod tests`):**
  - **Cross-pool rejection (the audit's UB witness).** Build a large pool (e.g. 8192) and a
    small pool (e.g. 256); `alloc` a buffer near the **end** of the large pool (offset +
    len > 256); call `small.bytes(&big_buf)` (and `bytes_mut`, and `read_volatile`) →
    **panics** ("outside pool arena"), where pre-B4A it was an out-of-bounds read/write.
    Use `std::panic::catch_unwind` (or `#[should_panic]`) to assert the panic, not the UB.
  - **In-bounds access unaffected.** A buffer used against its own pool round-trips exactly
    as the existing `data_roundtrip_and_device_view` test (`:1401-1415`) — no behavioural
    change for correct use; that test stays green verbatim.
  - **Sub-range bound.** `read`/`write`/`read_volatile` with `offset + len == buf.len`
    succeed; with `offset + len > buf.len` panic (the inherited slice-index bound for
    `read`/`write`; the `range_ptr` bound for `read_volatile`).
  - `cargo verus verify -p dma-pool` ≥ 26/0 unchanged (B4A adds no `verus!{}` code — the
    helper is wrapper-side plain Rust); the aarch64 cross-build (`cd kernel && cargo build`)
    still compiles `storaged`.
- **Effort/Risk:** S / low. The single high-value change — closes the confirmed OOB-UB at
  the one place PAs are visible, for every CPU-access method, behind one guarded helper.

---

## Sub-phase B4B — restore the `MAX_FREE_RANGES` backstop + discharge the `FreeList` wrapper preconditions *(closes the demoted-guard / undischarged-precondition half)*

Independent of B4A (touches `alloc`/`free` + new `FreeList` accessors, not the CPU-access
path) — may land in either order. Brings `DmaPool::free`/`alloc` into honest agreement with
the verified `FreeList` contract: every precondition the erased call relies on is
established at runtime, and the overflow guard the audit found demoted is literally back.

- **Touches:** `dma-pool/src/lib.rs`
  - inside `verus!{}` on `FreeList<N>`: add `pub fn is_full(&self) -> bool` and
    `pub fn is_allocated(&self, off, n) -> bool` with the `ensures` of Design decision 2
    (the `is_allocated` scan's loop invariant mirrors `free`'s own covers-reasoning at
    `:1150-1177`; expect a small `rlimit` bump, in the style of `alloc`/`free` `:335-336`);
  - `DmaPool::free` `:1271` — the four `assert!` guards (Design decision 2) before
    `self.fl.free(...)`;
  - `DmaPool::alloc` `:1261` — promote `debug_assert!(align.is_power_of_two())` to a hard
    `assert!(align != 0, …)` (or keep the power-of-two `debug_assert` *and* add the hard
    `align != 0`), discharging `FreeList::alloc`'s sole `align > 0` precondition in release.
- **Depends on:** Part A blessed (rev1§6/§6.1 trusted-vs-verified boundary). No intra-B4
  dependency (does not need B4A, though both edit the wrapper).
- **Work:** the accessors + guards as above. Re-run `cargo verus verify -p dma-pool`;
  **record the new total** (> 26) and update the trusted-base ledger line 103 and rev1§6.1's
  DMA-pool note to read "`FreeList` core + wrapper guard accessors verified; the
  raw-pointer/backing seam remains trusted" — keeping the ledger and §6.1 line-for-line
  agreed (the A5 discipline).
- **Acceptance (regression tests in `mod tests`):**
  - **Full-list backstop.** Drive the pool into `nfree == MAX_FREE_RANGES` via a maximally
    fragmenting alloc/free pattern (alloc N+1 small buffers, free every other one so the
    list reaches the cap), then `free` a non-adjacent buffer → **panics** with the
    fragmentation-cap message — the restored `assert!`, where pre-B4B it was a raw
    `self.free[N]` index-out-of-bounds. (`#[should_panic]` / `catch_unwind`.)
  - **Double-free / overlap.** `free` a buffer, then `free` the *same* `DmaBuf` again (it is
    `Copy`) → panics ("double free / overlap"); a single `free` of a live buffer succeeds and
    the space is re-allocatable (extends `exhaustion_and_free_merge` `:1388-1399`).
  - **Zero-length / bad align.** `free` of a hand-built `len == 0` buf → panics; `alloc(_,
    0)` → panics in release (not div-by-zero).
  - **`is_full`/`is_allocated` agree with the spec.** Covered by the Verus `ensures`; a unit
    test sanity-checks `is_full` flips exactly at the cap and `is_allocated` flips across a
    free/alloc of a region.
  - `cargo verus verify -p dma-pool` green at the **new** total ≥ 26 + (accessors); the
    aarch64 cross-build still compiles.
- **Effort/Risk:** M / medium. The verified-accessor proofs are the work (especially
  `is_allocated`'s covers-quantifier `ensures`); the wrapper guards are trivial. Medium
  because it touches the verified surface and the only place PAs are visible.

---

## Sub-phase B4C — wrapper proptest + Miri tier *(extends rev1§6 coverage from `FreeList<N>` to `DmaPool<B>`)*

The verification gap the audit's follow-up names ("Miri-cover … the DMA-pool wrapper"). The
verified `FreeList` proves the *arithmetic*; B4C proves the **wrapper drivers actually use**
(`alloc`→`bytes`/`read`/`write`→`free`, over a real backing) is sound under randomized
sequences and Miri — the rev1§6 "everything gets Miri + proptest" baseline the wrapper has
never met (3 happy-path unit tests, no proptest, absent from the Miri sweep).

- **Touches:**
  - `dma-pool/Cargo.toml` — add `proptest = "1"` to `[dev-dependencies]` (matches
    `cas`/`virtio-blk`/`storage-server`/`urt`).
  - `dma-pool/src/lib.rs` `mod tests` (or a new `dma-pool/tests/pool_props.rs`) — the
    proptest tier. *Recommend inline `mod tests`*: the `host` module is `#[cfg(any(feature =
    "std", test))]` (`:1317`), so under `test` `HostBacking` is available with no
    `--features std`, and `cargo +nightly miri test -p dma-pool` then covers it directly.
- **Depends on:** B4A + B4B (it exercises the extent guard, the backstop, and the verified
  accessors over real sequences).
- **Work:**
  - **Property 1 — alloc/free/access round-trip invariants.** proptest a random op sequence
    over a fixed-size pool: each step `alloc(len, align)` (random small `len`, power-of-two
    `align`) or `free` of a previously-returned live buffer. Maintain a **model** of live
    buffers (`offset, len, written-bytes`). After each `alloc(Some(buf))`: `buf.offset +
    buf.len <= pool_len`, `buf.device_addr == device_base + buf.offset`, and the new buffer
    is **disjoint** from every other live buffer (the wrapper-level corollary of
    `lemma_two_allocs_disjoint` `:1221`). Write a unique pattern through `bytes_mut`/`write`,
    read it back via `bytes`/`read` (and the leading bytes via `read_volatile`), assert it
    matches the model and that **no write to one live buffer perturbs another** (the
    disjointness property that the provenance/extent guards and the free-list together
    guarantee).
  - **Property 2 — cross-pool safety under randomization.** With two pools of random
    (different) sizes, apply a random buffer from pool A to pool B's `bytes`/`read_volatile`:
    assert it either round-trips safely (in-bounds) **or panics** (`catch_unwind`) — **never
    UB** (Miri is the oracle here: a missed bound is an immediate Miri error, not a silent
    pass).
  - **Property 3 — fragmentation backstop never UB.** A proptest that deliberately fragments
    toward the `MAX_FREE_RANGES` cap and frees non-adjacent regions asserts the wrapper
    either succeeds or panics cleanly — Miri confirms the restored `assert!` fires *before*
    any `self.free[N]` out-of-bounds.
  - Use the workspace Miri case-count convention: `#![proptest_config(ProptestConfig {
    cases: if cfg!(miri) { 4 } else { 256 }, ..ProptestConfig::default() })]` (mirrors
    `cas/src/file.rs:121-123`, `storage-server/tests/rights_lattice.rs`). dma-pool has no
    BLAKE3, so Miri is fast — the 4-case floor is conservative, kept for sweep-time
    uniformity.
  - **Oracle sanity (negative control, project style).** A test that, with the B4A
    `range_ptr` bound *removed* (a `#[cfg(test)]` shadow or a documented manual check),
    Property 2's cross-pool access is UB under Miri — i.e. the proptest is guarding a real
    hole, not a tautology. Document it like B3B's oracle-sanity case rather than committing
    the unsound variant.
  - **Add dma-pool to the Miri sweep.** Record in the sub-phase (and, if the team keeps the
    command in CLAUDE.md, extend it) that the workspace UB pass now includes
    `cargo +nightly miri test -p dma-pool` alongside `cas`/`loader`/`storage-server`.
- **Acceptance:**
  - `cargo test -p dma-pool` green including the new proptests at 256 cases natively and 4
    under Miri; the 3 existing unit tests unchanged.
  - Miri replay clean: `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p
    dma-pool` — no UB across the randomized alloc/free/access and cross-pool sequences.
  - The oracle-sanity control shows the proptest fails (Miri UB) if `range_ptr`'s bound is
    reverted — proving it guards the real soundness hole.
  - `cargo verus verify -p dma-pool` ≥ 26/0 (+ B4B's accessors) unchanged by B4C.
- **Effort/Risk:** S–M / low. Pure test/dev-dep addition behind the host-buildable
  `HostBacking`; no production change beyond B4A/B4B.

---

## Execution order

```
B4A  extent-guarded CPU access (range_ptr)        [the OOB-UB fix; do first, mergeable alone]
B4B  MAX_FREE_RANGES backstop + discharge preconds [independent of B4A; verified accessors]
   └─► B4C  wrapper proptest + Miri tier            [needs the guards + accessors from B4A+B4B]
```

- **B4A** is the load-bearing soundness fix and is independently shippable: it closes the
  confirmed OOB-UB across all five CPU-access methods in one atomic change behind a single
  guarded helper, with regression tests.
- **B4B** is independent of B4A (different methods; both edit the wrapper but not the same
  lines). It restores the demoted `assert!` and discharges every `FreeList::free`/`alloc`
  precondition at the seam, raising the Verus count.
- **B4C** depends on both (it exercises the guard, the backstop, and the verified accessors
  over randomized sequences, with Miri as the UB oracle).
- B4A and B4B *may* be reviewed together, but each alone is a complete, mergeable unit — keep
  them separable so the high-severity OOB-UB fix (B4A) can land fast (same posture as
  B1A/B2A/B3A).

## Out of scope for B4 (recorded so it is not mistaken for a gap)

- **Same-size, in-bounds cross-pool use** (logic confusion, not UB). Extent validation
  serves it safely (in-bounds) rather than rejecting it; deterministic rejection of *every*
  cross-pool buf needs the optional pool-identity tag (Design decision 1), deferred as
  future hardening since there is one pool per driver today. Not a memory-safety gap.
- **`DmaPool<B>` under Verus.** The raw-pointer/`DmaBacking` seam is the trusted plain-Rust
  boundary by design (rev1§2.5/§6.1, module doc `:18-35`); B4 *guards* it, it does not
  *verify* it. The verified surface grows only by `FreeList`'s exec accessors (B4B), not the
  PA seam — keeping the trusted base from widening (the B7 direction).
- **A cargo-fuzz target for the wrapper.** The wrapper is not a decoder of untrusted wire
  input (a `DmaBuf` is trusted-driver-produced inside the isolation TCB, rev1§2.5), so the
  rev1§3.7 "decoders are fuzz targets" routing does not apply. proptest + Miri is the correct
  tier; the verified decoders that *do* get fuzzed are elsewhere (cas/ipc/loader).
- **Real-hardware cache maintenance / DMB barriers** — the disclosed rev1§2.5/§8.1 debt
  (cache-coherence on real hardware, owed alongside SMP/PSCI). B4 is about CPU-side raw-slice
  *bounds*, orthogonal to device-coherence barriers; on the QEMU target DMA is coherent. Not
  a B4 concern (and the compiler-reordering half was B2's).
- **The IO-space object / IOMMU migration** (rev1§8.3 future work). When the backing swaps to
  IOVA-labeled mappings, `range_ptr`'s extent check moves with it unchanged (it is over the
  backing's `len()`, backend-agnostic) — noted so the guard is known to survive that
  migration, not re-litigated by it.
- **Loom/Shuttle for the pool** — deliberately omitted (one driver, one pool, single-threaded
  `SharedMem` by contract, no atomics) per the verification-tier note above.
