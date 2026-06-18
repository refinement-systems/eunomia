# Plan detail: Verus phase 9 — closeout (audit, distillation, doc reconciliation)

**Status: proposed.** This is the per-phase detail for the final step of the
Verus rewrite (`doc/plans/3_verus-rewrite.md` §7 step 8, §8, §11). Phase 8 was
the last *proof* phase (the `cas::store` recovery core, docs 64–67). Phase 9
ships **no new kernel/chokepoint proofs** — but it is deliberately **more than
the documentation-only closeout the master plan sketched**. The master plan §8
named three edits (spec §6, `CLAUDE.md`, a `0_kani-rewrite.md` banner). A
verification project earns the right to *make* those claims only after it has
(a) **proven it did not cheat** to get green, (b) **independently re-checked**
that the verified code still implements the spec — without leaning on its own
accumulated justifications, (c) **harvested the reusable methodology** so the
next verifier (here or elsewhere) does not relearn it from 47 findings docs, and
(d) **certified the whole suite green in one pass** so the doc claims are grounded
in a run, not in memory. Those four are this phase's added substance; the three
doc edits are the last of its six sub-phases, written *after* the audits so they
state what is true rather than what was hoped.

---

## Phase-number reconciliation (the slip lands here, last)

The master plan's §7 numbering drifted by one against the implementation: §4.1
folded the whole teardown cluster into phase 2, but it could not be proven until
every object type existed, so it grew into a standalone **phase 6** (docs 41–56),
pushing host chokepoints to **phase 7** (docs 57–63) and commit recovery to
**phase 8** (docs 64–67). The phase 7 and phase 8 detail docs each recorded the
drift in a reconciliation table but left the master plan's §7 text itself stale.
The final map:

| Plan §7 step | Actual repo phase | Status |
|---|---|---|
| 0 toolchain + pilot (`carve`) | phase 0 | done |
| 1 arena rewrite | phase 1 | done |
| 2 cspace/CDT | phase 2 (2b/2c, docs 21–25) | done |
| 3 untyped + channel | phase 3 (docs 26–30) | done |
| 4 notification + thread + timer | phase 4 (docs 31–35) | done |
| 5 aspace + sysabi | phase 5 (docs 36–40) | done |
| *(folded into §4.1)* | **phase 6** cross-object teardown + refcount census (6a–6f, docs 41–56) | done |
| 6 host chokepoints (§4.7) | **phase 7** (7a–7g, docs 57–63) | done |
| 7 commit recovery (§4.8) | **phase 8** (8a–8d, docs 64–67) | done |
| **8 closeout** | **→ phase 9 — THIS doc** | proposed |

So **closeout is phase 9**, and one of its jobs (sub-phase 9e) is to fix the
master plan's §7 numbering *in place* — the two detail-doc reconciliation tables
were a stopgap; phase 9 retires the stopgap by editing §7 to match reality and
flipping every `3_verus-rewrite*.md` from **proposed** to **done**.

---

## What this phase is — and is not

- **It is not a proof phase, but it is not pure prose either.** Sub-phases 9a and
  9b are *audits that may land real code changes*: discharging an in-proof
  `assume`, deleting an unjustified `external_body`, fixing a warning, or — if the
  conformance re-read finds a genuine drift — a spec-faithfulness fix or an
  explicitly recorded accepted-deviation. The "don't do them now" applies to
  executing the phase; the phase, when executed, has teeth.
- **It is where the project audits its own integrity.** Every prior phase added
  obligations and asserted a shrinking trusted base. Nobody has yet gone back and
  *enumerated* the full trusted surface in one ledger, or re-derived spec
  conformance from the spec alone. Phase 9 does both, once, authoritatively.
- **It is the knowledge-capture phase.** 47 findings docs (`doc/results/21…67`)
  encode hard-won Verus technique scattered across the chronology. Phase 9
  distills the future-useful fraction into one guideline so a contributor does not
  read 47 docs to learn that `b'E'` is an unsupported constant or that a `const`
  outside `verus!{}` is invisible inside it.

The honest framing for the writeup: phase 9 **certifies and harvests**; it does
not extend coverage. Any coverage gap it surfaces (in 9b) is recorded as a
finding with a disposition (fix now / follow-on / accepted-and-documented), not
silently closed.

---

## Sub-phasing

Six sub-phases. The ordering is **audits → distillation → authoritative docs →
certification**, and it is deliberate (see *Ordering rationale* below): the docs
in 9e may only claim what 9a/9b have established, and the certification in 9f is
the final gate that everything the docs assert actually runs green. Each
sub-phase is one PR; 9a, 9b, and 9f each append a `doc/results/68+` writeup (the
per-phase findings-doc discipline); 9c, 9d, and 9e land documentation.

### 9a — proof-hygiene audit + trusted-base ledger (user step 1)

**Goal:** prove the proofs do not cheat, and produce the one authoritative
enumeration of what remains trusted.

**Inventory to triage (current counts, the audit produces the exact ledger):**

- **In-proof `assume` / `admit`.** Exactly one real site today:
  `kcore/src/untyped.rs:409` `assume(bytes > 0)` in `retype_check`, resting on the
  external size helpers (`AspaceObj::bytes_for`, the `checked_next_multiple_of`
  arms). Each match arm either returns `Err` or yields a provably-positive
  `bytes`, so this `assume` is a *triage exemplar*: is it **dischargeable** (prove
  positivity per arm, delete the `assume`), or does it genuinely rest on an
  unproven external helper (then it becomes an `external_body` *contract* on that
  helper with a host test, not a bare `assume` in the caller)? The audit must pick
  one — a bare `assume`, even commented, is the weakest form and should not
  survive closeout. (The `adm.admit(...)` hits in `ipc/session.rs` are a method
  named `admit`, not the Verus `admit()` — false positives; the audit script must
  exclude them.)
- **`#[verifier::external_body]` (~39: kcore 32, urt 2, cas 5).** Each is a
  trusted boundary. The audit produces a table — one row per site —
  `{ site, what it assumes, why it cannot/should-not be proven, the host-side test
  that exercises the contract }`. Known-legitimate categories to confirm, not
  re-litigate: the `Store` hardware/scheduler seam (`make_runnable`,
  `aspace_unmap`/`aspace_destroy`, the TLB hooks) checked in `test_store.rs`; the
  blake3 totality seam (`checksum_ok` / `Hash::of`, totality needs no
  collision-freedom); the `urt::slots` runtime double-free guard (the static
  guarantee is `free`'s precondition; the `external_body` helper exists only
  because `debug_assert!` lowers to `panic!` inside `verus!{}`). **Any
  `external_body` row that cannot name both a reason and a test is a finding** —
  either give it a test or justify in prose why the contract is unobservable.
- **`assume_specification` / `external_fn_specification` / `external_type_specification`
  / `#[verifier::external]` (~22).** These tell Verus to trust a std/library
  signature or treat a type as opaque (e.g. the 7g `external_type_specification`
  on `FormatError`, the slice/`Vec` axiom imports). Confirm each is a library
  boundary (sound by Verus's own vstd discipline) and not a project-code proof
  being silently skipped.

**Warnings (the "fix all warnings" half).** Run and drive to **zero**: `cargo verus
verify -p kcore -p ipc -p urt -p dma-pool` + `-p cas --no-default-features` (note
any Verus diagnostics — unmet `recommends`, unused lemmas, `spinoff_prover`
noise); `cargo build` (host workspace) and the **kernel cross-build** (`cd kernel
&& cargo build` — the erasure path); `cargo clippy --workspace --exclude kernel`;
`cargo test --workspace --exclude kernel`. Each surviving warning is either fixed
or suppressed with a one-line justifying comment (the no-`unsafe`-without-comment
discipline, applied to lints).

**Deliverable:** `doc/results/68_verus-findings.md` — the **trusted-base ledger**
(the itemized table above) + the warning-zeroing record + the disposition of the
`untyped.rs:409` `assume`. This ledger is the source of truth that 9d's verus.md
trusted-base section and 9e's CLAUDE.md "trusted base is exactly …" claim both
cite, so it must land before them.

### 9b — independent spec-to-code conformance re-read (user step 2)

**Goal:** confirm the verified code implements `doc/spec/2_spec_rev2.md`, judged
**only against the spec and the code** — explicitly *not* against the findings
docs, the CLAUDE.md narrative, or the plan's own §4 obligation tables. The
accumulated justification prose is exactly what this audit must ignore: it is a
fresh-eyes re-derivation, because a proof can be green against a *drifted*
obligation and the chain of justifications can rationalize the drift.

**Method (per spec section that maps to verified code):**

1. Read the spec section cold (§2 capabilities/derivation/untyped/aspace, §3 IPC
   channels/notifications/reports, §4 storage commit/recovery, §5 process model).
2. From the spec text alone, write down the property the implementation *ought* to
   guarantee — in the auditor's own words, before looking at any `ensures`.
3. Open the verified code (`kcore`, the host chokepoints, `cas::store`) and read
   the actual `spec fn` model + `requires`/`ensures`. Ask: does the proven
   postcondition *match the spec-derived property*, or a convenient weakening of
   it? Is the `spec fn` model (e.g. `cdt_wf`, `chan_wf`, the FIFO `Seq`, the
   `pt_wf` tree) a faithful encoding of the spec's intent, or has the model been
   shaped to what was provable?
4. Record every gap as a **drift**, classified: **(D1) model weaker than spec**
   (the `ensures` proves less than the spec promises), **(D2) precondition hides a
   case** (a `requires` excludes inputs the spec admits — e.g. an `is_homed`
   guard, a `gen_a != gen_b` assumption — confirm the excluded case is handled
   elsewhere or is genuinely impossible), **(D3) spec ambiguity** (the spec is
   underspecified and the code picked a reading — record the reading), **(D4) code
   correct, spec stale** (the spec text lags the implemented design).

**Known watch-list (places the chronology hints drift may hide — but judge
independently, do not assume these are the only ones):** `revoke` root-survival is
conditional on `!is_homed(slot)` (the doc 23/doc 55 retraction — does the spec's
"revoke destroys all descendants" admit the seL4-zombie self-empty case?); the
phase-8 commit-recovery proof is the **structural/arithmetic half only**, with the
content-coverage half left to TLA+ (does the spec §6 "recovered state = committed
roots + replay" read as fully mechanized when it is not?); the construction-op
`refcount_sound` **system clause** is a recorded per-op follow-on for several ops
(notification `wait`, timer `arm`/`disarm`, channel `send`/`recv`, `thread::bind`,
`retype_install`) — is "refcount soundness" claimed more globally than proven?

**Disposition per drift:** fix-now (small, faithfulness-restoring), follow-on
(tracked, out of closeout scope), or accepted-and-documented (the spec or a
prominent code comment gains an explicit note — e.g. "the mechanized recovery
proof is the structural half; content-coverage is the `CommitProtocol` TLA+
obligation"). **No drift is closed silently.**

**Deliverable:** `doc/results/69_verus-findings.md` — the drift ledger with
dispositions. Its accepted-deviations feed 9e's spec/CLAUDE.md edits (so the docs
state the boundaries honestly, the phase-8 §4.8 discipline generalized).

### 9c — `doc/guidelines/kani.md` (user step 3)

**Goal:** a short standing guideline on Kani's place *after* its retirement, so a
future contributor does not reintroduce it by reflex or, conversely, assume it is
forbidden when it is the right tool.

**Content:**

- **What happened:** Kani was the interim bounded mechanized tier; every target it
  covered is now proven unbounded in Verus (kcore phase 2; the §4.7 chokepoints
  phases 7a–7f). The job, the pinned-`cargo-kani-0.67.0` install, and the
  `#[cfg(kani)]` scaffolding are gone (`doc/results/62`). Historical findings stay
  in `doc/results/2…8_kani-findings*.md`.
- **The rule:** *Kani may not be used where Verus is the better tool* — and for
  this codebase's shape (host-buildable `kcore`, explicit `wf()` predicates, the
  handle/`Store` seam, no int→ptr in the core) Verus is strictly better for the
  kernel core and the chokepoints (unbounded + termination + functional `ensures`,
  vs TLC-scale bounds). Reintroducing Kani for any of those is a regression.
- **Where Kani could still legitimately earn a place:** as a *fast bounded triage*
  on new code before the Verus proof is written (a counterexample-trace tier —
  Kani prints concrete failing inputs, Verus prints an SMT context); or on code
  that is genuinely intractable for Verus *and* small enough to bound, where a
  bounded check beats no mechanized check. Frame this as a high bar: the default
  is Verus; Kani returns only with a recorded justification of why Verus does not
  fit, mirroring the master plan's "best tool for the job, applied honestly."
- **Relationship to the other tiers:** Kani never competed with TLA+ (design),
  Loom/Shuttle (concurrency), or cargo-fuzz (adversarial bytes); those are
  unaffected by its retirement.

`doc/guidelines/` today holds only `fuzzing.md`; match its format and brevity.

### 9d — `doc/guidelines/verus.md` (user step 4)

**Goal:** the standing Verus guideline = **general working guidelines** + a
**distilled, future-useful compendium** of the 47 findings docs, written so it is
usable *without* reading the current code (general statements; inline code
snippets where a snippet is the clearest form).

**Part A — general guidelines:**

- The pin (`0.2026.06.07.cd03505` / `vstd =0.0.0-2026-05-31-0205`), the upgrade
  discipline (binary + vstd move together, in their own PR), the CI job
  (`cargo verus verify` per crate, **no per-proof filter** so a new obligation
  auto-gates), and the erasure guarantee (`verus!{}` compiles to nothing — the
  kernel cross-build and host tests run the same `exec` code).
- The project's split discipline: `spec fn` model (`wf`/FIFO `Seq`/`Map` tree) +
  `exec fn` with `requires`/`ensures` + `proof fn` lemmas; the **`closed`-model /
  opaque-field** rule (a `pub open` spec body may name only public items); the
  trusted-seam discipline (`external_body` only at a hardware/library boundary,
  each paired with a host test — the 9a ledger is the live instance).
- When Verus is *not* the tool (cross-reference master plan §2 and 9c): concurrency
  interleavings, adversarial bytes, the asm shell, crypto/perf inner loops.

**Part B — the distilled findings compendium**, organized by theme rather than by
chronology, including **only** items that generalize. Candidate themes harvested
from `doc/results/21…67` (the writeup must verify each is still accurate and state
it code-independently):

- **Parsing/codec recipe** (7a/7f/7g): explicit byte-indexing + mask/shift, *not*
  `from_le_bytes`/`try_into`/slice `==` (unspecced/`alloc`-only); `broadcast use
  vstd::slice::group_slice_axioms`; byte-char literals `b'E'` are an "Unsupported
  constant type" → use `0x45u8`; a `const` declared **outside** `verus!{}` is
  invisible inside it (move it in — it erases to the same `pub const`).
- **`Hash`-free verified core** (7f/7g/8): feed the proof already-decoded
  scalars/slices, return a `Hash`-free `Raw*` struct, keep `Hash` assembly in a
  thin plain-Rust delegator — so neither `Hash` nor an `external_type_specification`
  enters the proof surface; variable-length payloads (`Vec<u8>` + `[u8;32]`)
  round-trip *inside* the proof with no hash axiom.
- **Std-combinator restructuring** (7c/7d/7e): `.find().map()`, `.max(1)`,
  `.saturating_sub`, `copy_within` have no Verus model → restructure into explicit
  invariant-carrying loops / branches / verified shift helpers (`remove_at` /
  `insert_at` — the array-splice reasoning shared by `cdt_unlink`, `slot_move`,
  `dma-pool`).
- **Arithmetic technique** (7d/7e/8): prefer modular round-up
  `off + (align - off%align)%align` over the bit-mask `(off+align-1)&!(align-1)`
  (pure `vstd::arithmetic`, no `by (bit_vector)`); the division-hoist decomposition
  `secs·10⁹ + frac == (Δ·10⁹)/f` via `lemma_hoist_over_denominator`
  (`vstd::arithmetic::div_mod`); restate usize `+` overflow as `int` in spec
  `invariant`s; tie a fresh `let end = off + n` to the exec `buf.len()` bound.
- **`bit_vector` mode** (7c): bridge a packed bitmap `free[i/64] & (1<<(i%64))` to a
  per-element `is_free_spec` with `by (bit_vector)` frame lemmas — and the negative
  lesson, that nonlinear/division goals are *not* for `bit_vector` (7d/7e avoided it).
- **Termination** (7g/8c, the teardown cluster): `decreases` on a remaining-buffer
  length (opt-loop parser) or a `(count_nonempty, height)` lexicographic measure
  (seL4-zombie teardown recursion).
- **Opaque-external-type friction** (7g): an `external_type_specification` type
  cannot be *constructed* inside `verus!{}` ("constructor for an opaque datatype")
  → mirror it with an in-block enum mapping 1:1 (`TlvErr`→`FormatError`,
  `Survivor`/`Slot` in phase 8).
- **Proof-engineering scaling**: `spinoff_prover` to split heavy frame proofs (doc
  25 §2); when to bump `rlimit`; `broadcast`-`use` for axiom groups.
- **The arena enabler** (phase 1): index newtypes + typed arenas keep the core
  first-order (no `PointsTo` permission threading) — the single decision that made
  the kernel-core proofs tractable.

The writeup explicitly *omits* one-off, code-specific contortions that do not
generalize (the user's "only findings useful in the future" filter). Where a
finding is only meaningful with code, inline a minimal snippet.

### 9e — authoritative doc updates + plan/spec reconciliation (master plan §8, §11 — the original closeout, now informed)

The master plan's original three edits, **plus** the reconciliation the two detail
docs deferred — written *after* 9a/9b so they state audited fact:

- **Spec `2_spec_rev2.md` §6:** un-defer the Verus row (strike the ~~struck~~
  Verus row's "deferred" — it *became* the kernel implementation tier, its
  original assignment) and **strike or shrink the Kani row** (Kani is retired —
  per 9c). Add, per 9b's accepted-deviations, the honest boundary notes (e.g. the
  recovery proof is the structural half; TLA+ owns content-coverage).
- **`CLAUDE.md`:** the §6 verification-tiers table, the `### Verus` and `### Kani`
  sections, and the "trusted base is exactly the `Store` seam" sentence — all
  brought to final state, the last citing **9a's ledger** (so the claim is the
  enumerated truth, not a remembered summary). Collapse the long per-phase Verus
  narrative if a contributor is now better served by pointing at
  `doc/guidelines/verus.md`.
- **`0_kani-rewrite.md`:** the closeout banner — "superseded for the kernel core
  by `3_verus-rewrite.md`; Kani served as the interim mechanized tier and found
  DN-1…DN-14, recorded in `doc/results/2…8_kani-findings*.md`."
- **The plan docs themselves:** flip `3_verus-rewrite.md` and every
  `3_verus-rewrite_phase{3,4,5,6,7,8}-detail.md` + this closeout doc from **Status:
  proposed** to **done**; **fix the master plan §7 numbering in place** (retire the
  two stopgap reconciliation tables by making §7 match the 0–9 reality); update §11
  to "landed."
- **`scratchpad`'s fate (master plan §5 — the deferred decision):** kcore now
  carries its own `verus!{}` blocks, so scratchpad has graduated past "the first
  real proof module's home." Decide and execute: **keep** it as the minimal
  toolchain-smoke (`spec fn min`) — recommended, it is a cheap canary that the pin
  + CI install + cross-build still work independent of any real crate — or
  **remove** it and fold the smoke into a kcore obligation. Record the decision and
  update CLAUDE.md's workspace map accordingly.

### 9f — final certification run + completeness gate (added)

**Goal:** ground every claim the closeout docs make in one green pass, and confirm
nothing was silently dropped from verification.

- **One green run of the whole suite**, numbers recorded: `cargo verus verify` for
  each verified crate (the `N verified, 0 errors` per crate); the host-tests job
  (`cargo test --workspace --exclude kernel`, incl. `test_store` and every
  proptest/fuzz-corpus replay that was *kept* as differential coverage); the
  kernel cross-build; the on-OS gates (`scripts/spawn-test.sh`, `scripts/m1-test.sh`,
  `scripts/boot-test.sh`); the TLA+ `model` job; the `concurrency` Loom/Shuttle
  job; `scripts/fuzz.sh smoke`. The closeout docs cite these numbers.
- **Completeness / no-silent-drop gate.** Confirm the master-plan invariant "a
  property is never unguarded between tiers" held to the end: that the `verus` CI
  job carries **no per-proof filter** (a new obligation auto-gates); that no
  verified function was quietly moved behind `#[verifier::external]` to dodge a
  failing proof (cross-check against 9a's ledger — every `external`/`external_body`
  must be a *boundary*, never an escaped obligation); and that every chokepoint the
  spec §4.7/§4.8 and the kernel ops name actually carries a live `verus!{}`
  obligation in the job. Confirm the kept fuzz/proptest oracles (`tlv_entry`,
  `crash_recovery_preserves_acked_state`, the canonical-form suites) still run and
  still guard the now-proven code.
- **Deliverable:** `doc/results/70_verus-findings.md` — the certification record
  (the green numbers + the completeness-gate confirmations), the document a reader
  can point to as "the rewrite is done and this is the evidence."

---

## Ordering rationale (the user's "maybe a different order?")

The dependency chain forces audits-before-docs:

1. **9a and 9b first**, because they are the only sub-phases that can *change the
   facts*. Writing the authoritative spec/CLAUDE.md claims (9e) before auditing
   would risk asserting "the trusted base is exactly the `Store` seam" while an
   un-triaged `external_body` or the `untyped.rs:409` `assume` quietly says
   otherwise — the precise failure a closeout exists to prevent. 9a and 9b are
   independent of each other and may run in parallel (one audits *internal*
   integrity, the other *external* spec-conformance); neither depends on the
   other's output.
2. **9c, 9d next.** 9c (kani.md) is independent and could be written any time, but
   is grouped with the distillation work. 9d (verus.md) draws its trusted-seam
   section from 9a's ledger, so it follows 9a.
3. **9e after 9a/9b/9d**, because it is the authoritative statement and must
   reflect the audits (9a's ledger, 9b's accepted-deviations) and may point at the
   new guidelines (9d).
4. **9f last**, because it certifies the *final* tree — including any fixes 9a/9b
   landed and any doc state 9e set — in one green pass.

The master plan put closeout as a single trailing step; the only reordering versus
that is the internal one above. The expansion (9a, 9b, 9d, 9f beyond the three
master-plan doc edits) is the "more involved" the closeout warrants.

---

## CI / pinning deltas

- **No new `-p` and no new job.** Phase 9 adds no verified crate. 9a may *reduce*
  obligations (discharging the `assume`) and must keep the `verus` job green; 9f
  asserts the job has no per-proof filter (unchanged from today).
- **No Kani change.** Kani was retired in 7f. 9c documents its retirement; nothing
  to delete.
- **No Verus upgrade.** Stays pinned at `0.2026.06.07.cd03505` /
  `vstd =0.0.0-2026-05-31-0205`. (If 9a's warning sweep is blocked by a toolchain
  diagnostic that only a newer build fixes, that upgrade is its *own* PR per the
  pin discipline — not folded into closeout.)
- **`scratchpad`** is kept or removed per 9e's decision; if removed, drop it from
  the `verus` job and CLAUDE.md.
- The **`layering`** grep and all **kept differential tiers** (proptest, fuzz,
  Loom/Shuttle, TLA+) are unchanged — 9f only confirms they still run.

---

## Risks specific to phase 9

- **9b surfaces a real drift that is not cheap to fix (chief).** A conformance gap
  between spec and proven `ensures` that needs genuine proof work, not a doc note.
  Mitigation: the disposition framework — fix-now only if small; otherwise a
  tracked follow-on + an honest accepted-deviation note (the phase-8 §4.8 "the
  proof is the structural half" precedent generalized). Closeout does not block on
  new proof work; it *records* the boundary truthfully.
- **The `untyped.rs:409` `assume` is harder to discharge than expected.** The
  external size helpers may not carry positivity specs. Mitigation: the fallback is
  a *justified* conversion (an `external_body` contract on the helper + a host
  test), which is strictly stronger than the bare `assume` even if it is not a full
  discharge — closeout's bar is "no unjustified cheat," not "zero trusted boundary."
- **The verus.md distillation drifts from the code over time** (a guideline citing
  a snippet that later changes). Mitigation: write Part B *code-independently* (the
  user's explicit instruction) — general statements + minimal illustrative
  snippets, not references to live line numbers; the findings docs remain the
  dated source of record.
- **Warning-zeroing churn.** Driving every lint to zero can touch many files for
  little semantic gain. Mitigation: fix or one-line-justify; do not refactor for
  lint cosmetics beyond what the lint demands.

---

## Explicitly *not* in this phase

- **New verification coverage.** Any gap 9b finds is recorded, not closed with new
  proofs (those would be a *post*-closeout phase). The deferred construction-op
  `refcount_sound` **system clause** (the per-op follow-on recorded in phase 6) and
  the **content-coverage half** of commit recovery (TLA+'s, per phase 8) stay where
  they are — 9b confirms the docs describe them honestly, it does not mechanize
  them.
- **A Verus toolchain upgrade.** Its own PR if ever needed (pin discipline).
- **Rewriting the findings docs.** `doc/results/21…67` stay as the dated record;
  9d *distills* from them, it does not edit or replace them.
- **The asm shell, concurrency, crypto/perf, and the rest of `store.rs`/`disk.rs`**
  — out of scope by the master plan §10, unchanged.
