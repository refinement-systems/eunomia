# Plan — Part B3 detail: loader / ELF page-rounding hardening (checked VA rounding, parse↔prepare agreement, host model + cargo-fuzz target)

Detailed, separately-implementable decomposition of **Phase B3** from
`doc/plans/0_address_audit_rev0.md`. B3 is Wave-1 work: a confirmed loader
correctness hazard (`I-5`) on adversarial program images, plus the host-test/fuzz
gap that let it escape.

**Closes (from the parent plan):**
- `I-5` [medium, confirmed] — `loader/src/spawn.rs:57-59` rounds an untrusted segment
  VA with **unchecked** arithmetic (`va_end = (vaddr + memsz + PAGE-1) & !(PAGE-1)`,
  then `pages = (va_end - va_start) / PAGE`), while `loader/src/elf.rs:124` rejects
  only `vaddr + memsz` overflowing `u64` (it *permits* `== u64::MAX`). A crafted image
  with `vaddr + memsz` within `PAGE-1` of `u64::MAX` overflows the `+ PAGE-1`: a
  **debug-build abort** (overflow-checks on under `[profile.dev]`, with
  `panic = "abort"`) or a **release-build wrap** underflowing the page count
  (`[profile.release]` sets no `overflow-checks`, so it defaults off — silent wrap).
  (`doc/results/0_audit_rev0.md` §2.1.)
- `loader::prepare` host-model + fuzz gap [low] — `elf::parse` is a cargo-fuzz target
  and is total, but its consumer `prepare()` is **aarch64-only and unfuzzed**, so the
  unchecked rounding math has no host test (`audit` §4.2: "`loader::prepare`'s
  page-rounding (the I-5 site) has no host model").

**Spec target (already blessed in rev1 — B3 only conforms code to it):**
- **rev1§5** — spawn takes an ELF image (typically read via a snapshot handle on a
  storage session) and maps it fully (no demand paging). Program images are *data in
  the versioned store*, so any holder of write access to a path supplies the bytes:
  the image is **untrusted input** and the loader must refuse-not-crash on any of it.
- **rev1§5.3** — "every fault is a bug": the loader maps programs fully with unmapped
  guard regions, so a *spawn-path* panic on an adversarial image (a crash inside the
  spawner, i.e. the shell or init) is exactly the failure mode this discipline forbids;
  the spawner must return a clean `SpawnError`, never abort or wrap.
- **rev1§3.7** — the decode discipline for untrusted input: "decoders treat all
  payloads as untrusted … and are fuzz targets on the host (§6)". The ELF parser is the
  program-image decoder; B3 keeps it total and extends the same posture to the one piece
  of post-parse arithmetic the consumer still performs (the page-layout computation),
  so nothing the parser blesses can crash the consumer.

Because Part A is blessed first (the parent plan's hard dependency), **B3 makes no spec
edits** — the rev1 text above is the fixed target. Every citation here is `rev1§`.

**Primary files:** `loader/src/elf.rs` (the host-buildable, already-fuzzed parser:
`Segment` :9, the segment-validation block :119-127, the new layout home), and
`loader/src/spawn.rs` (the target-only consumer: `PAGE` :12, the inline rounding
:57-59, the file-write offset :63). Secondary: `loader/src/lib.rs`,
`loader/tests/*`, `loader/fuzz/*`, `loader/examples/gen_loader_corpus.rs`,
`loader/Cargo.toml`. No change is required in the callers (`user/init`, `user/shell`,
`kernel/main`) — they keep calling `parse`/`prepare`/`start` unchanged.

---

## Verification tier & baseline (applies to both sub-phases)

Per rev1§6 routing, loader is **userspace decode + spawn glue**: the ELF parser is a
**decoder** (→ fuzz target on the host, rev1§3.7/§6), and the page-layout arithmetic is
**pure sequential math** (→ proptest + Miri, the rev1§6 "Miri + proptest — everything"
baseline). Four honesty notes recorded up front so nothing is silently dropped or
over-claimed:

- **No Verus obligation.** The loader is not a CAS/IPC/DMA/kernel *chokepoint*
  (rev1§6), so it is not routed to Verus. The fix is checked integer arithmetic, fully
  covered by proptest + cargo-fuzz + Miri. B3 touches none of the verified crates, so
  the regression baselines in the parent plan (`kcore` 335/0, `cas` 58/0, `dma-pool`
  26/0, the TLC models, the kcore/CAS seam ledger) are **untouched** and need no
  re-establishment.
- **No Loom/Shuttle target.** `prepare` is a single-shot sequential routine over one
  image with no shared state, atomics, or concurrency; there is nothing for a
  weak-memory model to witness. (Same posture as B1's rights-lattice note and B2's
  driver note.)
- **The "host model" is the *real* code, not a parallel reimplementation.** The parent
  plan asks for a "host model + cargo-fuzz target" for `prepare`'s rounding. Rather than
  write a second copy of the math that can drift from the target code, B3A **extracts
  the actual rounding arithmetic** into a host-buildable pure function that *both*
  `prepare` (target) and the tests/fuzzer (host) call. So the host tier exercises the
  exact arithmetic the spawner runs — there is no model-vs-impl gap to maintain. This is
  strictly stronger than the parent plan's "host model" wording, and is recorded as the
  deliberate interpretation.
- **The fix's guarantee is *totality*, and the profiles make it load-bearing in both
  builds.** Under `[profile.dev]` (overflow-checks on, `panic = "abort"`) the unchecked
  math **aborts**; under `[profile.release]` (no `overflow-checks`) it **silently
  wraps** to a bogus page count. Checked arithmetic is correct in *both* regimes (clean
  `Err`, never abort, never wrap). The proptest/fuzz tier asserts the totality property
  directly (no panic on any input; outputs internally consistent), so the guarantee is
  build-independent and mechanically checked, not profile-dependent.

**Baseline to re-establish at end of B3:** `cargo test -p loader` green (today: the
`elf` unit tests + `fuzz_corpus` + `fuzz_regressions`; after B3: those plus the new
`layout_props` proptest and the extended regression). The committed corpora still parse
and lay out without panic under the workspace Miri replay
(`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas -p loader
-p storage-server --test fuzz_regressions --test fuzz_corpus`, per CLAUDE.md). The
target build still compiles for `aarch64-unknown-none` (`cd kernel && cargo build` boots
init/shell/storaged, which all go through `spawn::prepare`).

---

## Design decision 1 — where the extracted page-layout function lives *(resolve in B3A)*

The fix and the host-testability both require the overflow-prone arithmetic to leave
the target-gated `spawn.rs` (which is `#[cfg(all(target_arch = "aarch64", target_os =
"none"))]`, wholesale, because it calls `ipc::sys`) and move into host-buildable code.
Three candidate homes; B3A pins the design:

- **Adopted — a method on `Segment` in `elf.rs`:**
  `pub fn page_layout(&self) -> Result<PageLayout, ElfError>`, returning a small
  plain-data `PageLayout { va_start, va_end, pages, page_offset }`, with the canonical
  `pub const PAGE: u64 = 4096` moved here too. Decisive reasons:
  1. **One shared predicate ⇒ producer/consumer agreement is structural.** The I-5 fix
     is fundamentally "make `parse` (producer) and `prepare` (consumer) agree on which
     segments are layout-able." If `parse` validates a segment by calling the *same*
     `page_layout` that `prepare` later calls, agreement is guaranteed by construction —
     there is no second bound to keep in sync. This is exactly what `elf.rs:124` failed
     to do (it under-checked relative to `prepare`).
  2. `elf.rs` is **already** the host-buildable, total, fuzzed module; `Segment` lives
     here; the existing fuzz target and corpus replay already `use loader::elf`. The
     layout fn drops into the surface that is already covered.
  3. It mirrors the established extraction pattern: B1B factored `attenuate`, B2B
     factored `avail_ring_slot` — a small pure helper pulled into the existing module so
     it is directly proptest/fuzz-addressable. `page_layout` is the B3 analogue.
- **Rejected — a new `loader/src/layout.rs` module** holding `PAGE`/`PageLayout`/a free
  `segment_layout(vaddr, memsz)`. Cleaner parsing-vs-mapping separation in principle,
  but it adds a module + cross-module error plumbing for one tiny function and *splits*
  the producer (`parse`) from the predicate it must share — the opposite of reason (1).
  Noted as the fallback if `page_layout` ever grows beyond geometry.
- **Rejected — un-gate just the pure fn inside `spawn.rs`.** `spawn.rs` is target-gated
  as a unit (its body is all `ipc::sys` orchestration); carving one `#[cfg]`-flipped fn
  out of it is awkward and leaves the predicate stranded away from `parse`.

After extraction, `spawn.rs` keeps `PAGE` available via `pub use crate::elf::PAGE;` (so
`spawn::PAGE` — referenced only inside `spawn.rs` today, for the stack math — and any
external user keep resolving), and `prepare`'s segment loop becomes thin orchestration:
all overflow-prone arithmetic now lives in the host-tested `page_layout`.
**Recommendation: adopt the `Segment::page_layout` method in `elf.rs`.**

---

## Design decision 2 — what to fuzz, and at which layer *(resolve in B3B)*

The parent plan asks for "a host model + cargo-fuzz target for `prepare`'s rounding."
`prepare` itself cannot be host-fuzzed (it issues syscalls), so the fuzzable surface is
the extracted `page_layout` plus the existing `parse`. Two layers; B3B does **both**,
with the segment-level target as the primary new one:

- **Primary — a new segment-level fuzz target (`segment_layout`).** Interpret the fuzz
  bytes as a `(vaddr, memsz)` pair (first 16 bytes, little-endian; short inputs padded)
  and call `page_layout` directly. This **maximizes density at the I-5 overflow
  boundary** — the fuzzer reaches `vaddr + memsz` within `PAGE-1` of `u64::MAX` in two
  drawn words, instead of having to build a near-`u64::MAX` vaddr through a whole
  well-formed ELF. Property set: never panics; and on `Ok(layout)`, the outputs are
  internally consistent (see B3B for the exact invariants).
- **Secondary — extend the existing `elf_parse` target with parse↔layout agreement.**
  For every segment `parse` returns, assert `seg.page_layout()` is `Ok` and consistent.
  This is the **producer/consumer-agreement property** in fuzz form: *anything `parse`
  accepts, `prepare` can lay out without overflow*. It is cheap (the corpus is already
  parsed) and it is the direct guard against the `elf.rs:124` class of bug regressing.
- **Plus a proptest tier (`tests/layout_props.rs`).** rev1§6's host baseline is "Miri +
  proptest"; the proptest gives a fast, Miri-replayable, in-`cargo test` gate over the
  same `(vaddr, memsz)` invariants (cargo-fuzz is not part of ordinary `cargo test`).

The fuzz *coverage* the parent plan wants and the *totality guarantee* are both carried
here: cargo-fuzz for adversarial depth, proptest+Miri for the always-on gate, and any
crash promoted to `fuzz_regressions.rs` (where the ELF-1 phoff-overflow case already
lives). **Recommendation: adopt the dedicated `segment_layout` target + the `elf_parse`
agreement extension + the proptest.**

---

## Sub-phase B3A — checked page-rounding: extract `page_layout`, harden `prepare`, tighten `parse` *(closes I-5)*

The headline correctness fix. Self-contained and mergeable alone: after B3A no
adversarial image can make the spawn path abort or wrap — `prepare` returns a clean
`SpawnError`, and `parse` refuses the same images up front. Atomic by necessity: the
extraction, the checked consumer math, and the matching producer check are the I-5 fix,
and they share one predicate (Design decision 1).

- **Touches:** `loader/src/elf.rs`
  - add `pub const PAGE: u64 = 4096;`, `pub struct PageLayout { va_start, va_end, pages,
    page_offset }` (all `u64`; `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`), and
    `impl Segment { pub fn page_layout(&self) -> Result<PageLayout, ElfError> }`;
  - the segment-validation block `:119-127` — replace the under-checking
    `seg.vaddr.checked_add(seg.memsz).is_none()` clause with `seg.page_layout().is_err()`
    so the producer rejects exactly what the consumer cannot lay out.
  - `loader/src/spawn.rs`
  - drop the local `pub const PAGE` `:12` → `pub use crate::elf::PAGE;`;
  - the segment loop `:55-72` — replace the inline `va_start`/`va_end`/`pages` math
    `:57-59` and the `seg.vaddr - va_start` write offset `:63` with one
    `let l = seg.page_layout().map_err(SpawnError::Elf)?;` and uses of
    `l.pages` / `l.page_offset` / `l.va_start`.
- **Depends on:** Part A blessed (rev1§5/§5.3/§3.7 text). No intra-B3 dependency.
- **Work:**
  1. The extracted layout (the fix — all overflow-prone math, checked):
     ```rust
     /// Page geometry the loader maps a segment into (rev1§5). All arithmetic is
     /// checked: a segment whose page-rounded end would exceed u64::MAX is refused
     /// (BadSegment), never wrapped or aborted — `prepare` runs this on untrusted
     /// images (rev1§3.7) and must refuse-not-crash (rev1§5.3). `parse` runs the
     /// same check so the producer never hands `prepare` a segment it cannot lay
     /// out (the I-5 gap was `parse` under-checking relative to `prepare`).
     pub fn page_layout(&self) -> Result<PageLayout, ElfError> {
         let va_start = self.vaddr & !(PAGE - 1);            // round down: cannot overflow
         let va_end = self
             .vaddr
             .checked_add(self.memsz)                         // subsumes the old :124 check
             .and_then(|e| e.checked_add(PAGE - 1))           // the I-5 overflow point
             .map(|e| e & !(PAGE - 1))                        // round up to page boundary
             .ok_or(ElfError::BadSegment)?;
         // va_end >= va_start (round-up of vaddr+memsz vs round-down of vaddr), so the
         // subtraction cannot underflow once the checked add above succeeds; checked_sub
         // is belt-and-suspenders and keeps the fn total under any future caller.
         let span = va_end.checked_sub(va_start).ok_or(ElfError::BadSegment)?;
         Ok(PageLayout {
             va_start,
             va_end,
             pages: span / PAGE,
             page_offset: self.vaddr - va_start,              // in [0, PAGE): cannot underflow
         })
     }
     ```
     Note `page_layout` is total for *all* `(vaddr, memsz)` including `memsz == 0`
     (yields `pages` possibly 0, no panic); `parse` already drops `memsz == 0` segments
     (`elf.rs:128`), so `prepare` only ever sees `memsz > 0` (⇒ `pages >= 1`). The
     fuzz/proptest invariants in B3B respect this (`memsz > 0 ⇒ pages >= 1`, not
     unconditional).
  2. `parse` producer check (one-line tighten at `:119-127`): the
     `|| seg.vaddr.checked_add(seg.memsz).is_none()` clause becomes
     `|| seg.page_layout().is_err()`. This rejects the previously-permitted
     `vaddr + memsz` within `PAGE-1` of `u64::MAX` (the I-5 witness) at parse time —
     decode discipline (rev1§3.7): refuse malformed input at the boundary.
  3. `prepare` consumer (the target-only orchestration, now thin):
     ```rust
     let l = seg.page_layout().map_err(SpawnError::Elf)?;
     check(sys::retype(untyped, OBJ_FRAME, l.pages, frame_slot, 0))?;
     let file = &image[seg.offset as usize..(seg.offset + seg.filesz) as usize];
     check(sys::frame_write(frame_slot, l.page_offset, file))?;
     // …perms unchanged…
     check(sys::map(aspace_slot, frame_slot, l.va_start, perms))?;
     ```
     `prepare` re-runs `page_layout` rather than trusting `parse` (defense in depth: the
     fuzzer hits `page_layout` directly, and `prepare` must be total on its own). An
     oversized-but-non-overflowing `pages` (e.g. small `vaddr`, huge `memsz`) is **not**
     a `prepare` panic — it flows to `sys::retype`, which fails on untyped exhaustion and
     returns a clean `SpawnError::Sys` via `check()`. Record this in a comment so it is
     not mistaken for an unhandled case.
- **Acceptance (unit tests in `elf.rs`'s `mod tests`):**
  - `page_layout` on the audit's I-5 witness — a segment with `vaddr + memsz` within
    `PAGE-1` of `u64::MAX` (e.g. `vaddr = u64::MAX - 8, memsz = 8`) → `Err(BadSegment)`,
    **no panic** (the negative control: the *old* `(vaddr + memsz + PAGE - 1)` would
    abort in dev / wrap in release).
  - `page_layout` on a normal segment (e.g. `vaddr = 0x8000_0123, memsz = 0x2000`) →
    `va_start = 0x8000_0000`, `va_end = 0x8000_3000`, `pages = 3`,
    `page_offset = 0x123`; and the universal invariants hold (`va_start <= vaddr`,
    `vaddr < va_end`, `pages * PAGE == va_end - va_start`, `page_offset < PAGE`).
  - `parse` on a hand-built ELF whose single PT_LOAD carries the I-5 witness vaddr/memsz
    → `Err(BadSegment)` (the producer now refuses what it used to pass). Extends the
    existing `rejects_malformed` test.
  - `parses_minimal_image` / `rejects_malformed` still pass unchanged (the tiny ELF's
    segment lays out fine; no behavioral change for valid images).
  - Target still builds: `cd kernel && cargo build` (the only host-invisible consumer,
    `prepare`, compiles against the new `page_layout`/`PAGE` re-export).
- **Effort/Risk:** S / low. The single correctness change — closes the confirmed
  spawn-path crash on adversarial images, at both the producer and the consumer, with
  one shared predicate.

---

## Sub-phase B3B — host model: proptest + cargo-fuzz target + Miri replay *(closes the host-model/fuzz gap)*

The coverage gap that let I-5 escape (`prepare`'s math was aarch64-only and unfuzzed).
B3B brings the extracted `page_layout` under the rev1§6 host baseline (proptest + Miri)
and adds the cargo-fuzz depth the parent plan asks for, including the producer/consumer
agreement property that pins the `elf.rs:124` class of bug.

- **Touches:**
  - `loader/Cargo.toml` — add `proptest = "1"` to `[dev-dependencies]` (matches
    `cas`/`virtio-blk`/`storage-server`/`urt`).
  - new `loader/tests/layout_props.rs` (the proptest tier).
  - `loader/fuzz/Cargo.toml` — add a `[[bin]]` `segment_layout`.
  - new `loader/fuzz/fuzz_targets/segment_layout.rs` (the segment-level target).
  - `loader/fuzz/fuzz_targets/elf_parse.rs` — extend the per-segment loop with the
    parse↔layout agreement assertions (no new file).
  - `loader/examples/gen_loader_corpus.rs` — seed the new `segment_layout` corpus with a
    few `(vaddr, memsz)` pairs incl. the boundary (and keep the existing `elf_parse`
    seeds).
  - `loader/tests/fuzz_corpus.rs` — replay the new `segment_layout` corpus too (so the
    documented Miri command, which names `--test fuzz_corpus`, covers the new target's
    inputs without growing the command).
  - `loader/tests/fuzz_regressions.rs` — add the I-5 witness as a pinned regression
    (ELF-2), beside the existing ELF-1 phoff-overflow case.
- **Depends on:** B3A (it tests the extracted `page_layout` and the tightened `parse`).
- **Work:**
  - **The layout invariants (one oracle, shared by proptest + both fuzz targets).** For
    any `(vaddr, memsz)`, `page_layout` **must not panic**, and on `Ok(l)`:
    - `l.va_start <= vaddr` and `l.va_start % PAGE == 0` (round-down);
    - `l.va_end % PAGE == 0` and `l.va_end >= l.va_start` (round-up, no underflow);
    - `vaddr < l.va_end` **iff** `memsz > 0` (a non-empty segment ends past its start);
    - `l.page_offset == vaddr - l.va_start` and `l.page_offset < PAGE`;
    - `l.pages.checked_mul(PAGE) == Some(l.va_end - l.va_start)` (page count exact, no
      overflow); and `memsz > 0 ⇒ l.pages >= 1`.
    On `Err`, the only legal error is `BadSegment`, and it must occur **iff**
    `vaddr.checked_add(memsz).and_then(|e| e.checked_add(PAGE-1)).is_none()` (the
    overflow boundary is the *exact* refusal condition — pins the fix, not just "doesn't
    crash").
  - **`tests/layout_props.rs` (proptest).** Generate `(vaddr, memsz)` over the full
    `u64 × u64` range with extra weight near `u64::MAX` (proptest's default `u64`
    strategy already samples boundaries; add explicit `prop_oneof!` biases toward
    `u64::MAX - k` for small `k` and page-aligned/`±1` vaddrs). Assert the invariant set
    above. Use the workspace Miri case-count convention:
    `#![proptest_config(ProptestConfig { cases: if cfg!(miri) { 4 } else { 256 },
    ..ProptestConfig::default() })]` (mirrors `cas/src/file.rs:121-123`,
    `storage-server/tests/rights_lattice.rs`). Add one **oracle sanity** case: a helper
    computing the *old* unchecked formula behind `checked_*` and asserting it would have
    overflowed on the boundary input the fix now refuses (so the test proves it is
    guarding a real wrap, in the project's negative-control style).
  - **`fuzz_targets/segment_layout.rs` (primary new target).** Draw `(vaddr, memsz)`
    from the input bytes and call `Segment { vaddr, memsz, ..zeroed }.page_layout()`;
    assert the invariant set. The fuzz profile already forces
    `overflow-checks = true`/`debug-assertions = true` (`loader/fuzz/Cargo.toml:17-20`),
    so any unchecked wrap that slips back in aborts the run — the differential that would
    have caught I-5.
  - **`fuzz_targets/elf_parse.rs` (agreement extension).** Inside the existing
    `for seg in &img.segments[..img.nsegments]` loop, add:
    `let l = seg.page_layout().expect("parse accepted a segment prepare cannot lay
    out");` then the consistency asserts. This makes the **parse↔prepare agreement**
    (the `elf.rs:124` tightening) a live fuzz invariant: a future loosening of `parse`
    that re-permits an unlayout-able segment fails here.
  - **Seed corpus (`gen_loader_corpus.rs`).** Add a `segment_layout` corpus with a
    handful of 16-byte `(vaddr, memsz)` seeds: a normal mapping, a page-aligned vaddr, an
    unaligned vaddr, `memsz = 0`, and the boundary (`vaddr = u64::MAX - 0xfff, memsz =
    0x1000`) so the fuzzer starts adjacent to the I-5 edge. Keep the existing `elf_parse`
    seeds. Document the run command in the file header (the example already prints what
    it writes).
  - **`fuzz_corpus.rs` replay.** Add a `segment_layout` test that reads the new corpus
    dir, decodes each file's leading 16 bytes to `(vaddr, memsz)`, and re-checks the
    layout invariants — keeping the new fuzz inputs alive as ordinary, Miri-checkable
    tests (same pattern as the existing `elf_parse` replay).
  - **`fuzz_regressions.rs` (ELF-2).** Pin the I-5 witness two ways: (a) a direct
    `page_layout` call on the boundary segment → `Err(BadSegment)` no-panic; (b) a
    hand-built ELF carrying that segment → `parse` → `Err(BadSegment)`. Mirror ELF-1's
    docstring style (state the hazard and that the math is now checked).
- **Acceptance:**
  - `cargo test -p loader` green including `layout_props`, the extended
    `fuzz_corpus`/`fuzz_regressions`; proptest passes at 256 cases natively and 4 under
    Miri.
  - The oracle-sanity case fails if `page_layout`'s `checked_add` is reverted to a plain
    `+` (the test guards a real overflow, not a tautology).
  - `cargo +nightly fuzz build` (in `loader/fuzz`) builds both `elf_parse` and
    `segment_layout`; a short `cargo +nightly fuzz run segment_layout -- -runs=200000`
    finds no crash; the `elf_parse` agreement assertion holds across the existing corpus.
  - Miri replay clean:
    `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p loader
    --test fuzz_regressions --test fuzz_corpus` (the new `segment_layout` corpus replay
    rides the existing `--test fuzz_corpus`; if a separate `layout_corpus` file is
    preferred instead, the documented Miri command grows by one `--test`, recorded here).
- **Effort/Risk:** S–M / low. Pure test/fuzz addition behind the host-buildable
  `page_layout` seam B3A created; no further production change.

---

## Execution order

```
B3A  checked page_layout + harden prepare + tighten parse   [the I-5 fix; do first, mergeable alone]
  └─► B3B  proptest + cargo-fuzz + corpus + regression + Miri [needs page_layout/parse from B3A]
```

- **B3A** is the load-bearing correctness fix and is independently shippable: it fully
  closes `I-5` (no spawn-path abort/wrap on any image) at both the consumer (`prepare`)
  and the producer (`parse`), with unit-test coverage, in one atomic change.
- **B3B** depends on B3A (it exercises the extracted `page_layout` and the tightened
  `parse`, and pins the I-5 witness as a regression).
- B3A and B3B *may* be reviewed as one change if preferred, but B3A alone is a complete,
  mergeable unit — keep them separable so the correctness fix can land fast (same posture
  as B1A/B2A).

## Out of scope for B3 (recorded so it is not mistaken for a gap)

- **Fuzzing `prepare`/`start` end-to-end** (the syscall orchestration). These are
  `ipc::sys`-bound and target-only; B3 fuzzes the *arithmetic* `prepare` performs by
  extracting it (`page_layout`), which is the only part that can crash on input. The
  syscall sequencing is covered by the QEMU boot integration path (init/shell/storaged
  spawn), not by a host fuzzer — recorded so the Miri/fuzz scope is honest (the layout
  math, not the syscalls).
- **A fault-injecting `sys` stub to host-run `prepare`.** A mock `ipc::sys` could let
  `prepare` run on the host, but it would test the *mock*, not the kernel; the
  arithmetic (the I-5 surface) is fully covered by `page_layout`, so the mock earns
  nothing for this phase. Noted as the path if a future phase wants `prepare`'s slot
  allocation / error sequencing under host test.
- **Broader ELF hardening** beyond the page-rounding overflow — e.g. overlapping-segment
  detection, `entry` ∈ a mapped segment, `p_align` honoring, W^X enforcement. The audit
  scoped I-5 to the rounding overflow; these are separate (and partly policy, rev1§5).
  Not a B3 gap.
- **Oversized-but-valid images** (`pages` huge but non-overflowing): handled at runtime
  by `sys::retype` returning a clean `SpawnError::Sys` (untyped exhaustion), not by
  `prepare` — confirmed in B3A's comment, not a crash path, so no extra guard is owed.
- **Loom/Shuttle for the loader** — deliberately omitted (no concurrency; `prepare` is a
  single-shot sequential routine) per the verification-tier note above.
