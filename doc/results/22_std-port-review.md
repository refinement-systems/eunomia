# std-port review (independent) — findings #22

An independent audit of the Rust-standard-library port to Eunomia, covering the
whole effort from the draft-plan commit `e6dee85` through `7e06ad1` (the current
`main`). The port is planned in `doc/plans/2_plan-std-revised.md` (the revised
plan, which superseded and deleted the first plan in `c689ac8`) and recorded in
findings docs `0`–`21` (with inserts `7-1`, `7-2`, `16-1`, `20-1`, `20-2`).

This report answers four questions:

1. Were the original goals achieved?
2. Were any new problems introduced (unverified code without justification, or
   anything else dubious)?
3. Was the comment & documentation discipline from `CLAUDE.md` upheld?
4. Any other insights.

## Scope & method

The effort touches ~219 files / ~21k inserted lines across 6 phases (0–6, ~40
commits). It was reviewed **independently and adversarially**: plan text, findings
docs, and code comments were treated as *claims* and checked against the actual
code and, where cheap, by running the tool — not taken at face value.

Mechanically: a multi-agent fan-out placed one investigator on each phase's
goal-claims and one on each cross-cutting axis (verification integrity,
comment/doc discipline, ledger integrity, dubious-code/PAL-thinness,
completeness), plus a **live `cargo verus verify` run** over every std-port crate
from a cold (`cargo clean -p`) cache. Material negative findings were then routed
to skeptic agents told to default to *refuting*.

Honest limits of the method:

- The live verus gate is authoritative and was run to completion (see §1).
- The live QEMU boot / smoke tests were **not** re-run here; that claim rests on
  code inspection plus confirmation that CI actually wires them (`.github/
  workflows/ci.yml` on-os job runs `std-smoke-test.sh`, `fs-smoke-test.sh`,
  `libtest-on-target.sh --ci`; the smoke scripts hard-fail on the negative
  markers, so they are not vacuous).
- The adversarial-verification and synthesis stages were partly cut short by a
  session limit. The discipline dimension was independently reproduced by **six**
  agents (so its failure to complete one changed nothing), and the Phase-2/Phase-3
  goal-detail and escape-hatch-delta dimensions — where three agents degenerated
  to placeholder output — were **re-done by hand** for this report (the Phase-3
  and escape-hatch checks below are the author's own).

### Severity legend

`blocker` (goal unmet / unjustified unverified code / broken) · `major` · `minor`
· `observation` · `positive` (notably good).

### Summary of findings

| # | Q | Sev | Finding |
|---|---|-----|---------|
| 1 | goals | positive | All 5 std-port verus crates verify green on a live cold run, matching the ledger exactly |
| 2 | goals | positive | Verified surfaces are real and strong: syscall encoder (total, host-pinned), path resolver (total, `..`-confinement machine-checked), startup/`le-bytes` decoders, TLS key table |
| 3 | goals | positive | `extern "Rust" __eunomia_*` bridge exists on both sides; the sysroot std-dep was genuinely dropped for the documented reason |
| 4 | goals | positive | Phase 3 (kernel track) delivered: `TPIDR_EL0` + `offset_of` const-assert, `ThreadStartAs` x6 arg, futex on Loom/Shuttle (not Verus), entropy DRBG, verified TLS |
| 5 | goals | positive | Unsupported fs surface genuinely returns `ErrorKind::Unsupported`; fs client is message-bounded |
| 6 | goals | positive | Console stdio + `NAME_STDERR` fallback + panic-on-debug-log; env producer; shell keeps raw `loader::spawn` |
| 7 | goals | positive | Finding 16-1 fixed a genuine **pre-existing** verified-kernel census bug that the port surfaced |
| 8 | new-problems | major | fs readdir wire codec is hand-rolled, unverified LE parsing on **both** the PAL decode side and the gated encode side — a bespoke format outside the sanctioned unverified categories; the PAL arm's own doc falsely claims "never any protocol logic" |
| 9 | new-problems | minor | `urt::random` DRBG carries no Verus — a 4th unverified category; disclosed and fails loud, but a carve-out beyond the three sanctioned ones |
| 10 | new-problems | minor | stderr routing cites "rev2§5.1 stderr fallback" but rev2§5.1 never mentions stderr (dangling citation + spec/code gap); deep-path write fails as `Uncategorized` |
| 11 | new-problems | observation | Every std user binary now links the full `storage-server`/`cas`/blake3 stack via `eunomia-sys` `[dependencies]`; relies on release LTO/DCE |
| 12 | discipline | major | Pervasive plan-phase / findings / milestone references in **code comments** (~216 `std-port N.N` sites across 48 files, plus `findings #N`, `Phase-N GATE`, `MVP`) |
| 13 | discipline | major | The trusted-base **ledger** (a `doc/guidelines` file) is saturated with plan-phase / `findings #N` / `Task N` references, which guidelines may not carry |
| 14 | discipline | minor | The ledger narrates "what was" (count-delta arrows, `retired`/`replaces` verbs) rather than current state only |
| 15 | discipline | observation | CI YAML step names carry `std-port Phase-N` milestone labels (gray-area under the rule's scope) |
| 16 | other | minor | Ledger internal inconsistencies: stale `eunomia-sys 7` notes vs the correct `16`; stale line-number citations; shim count claimed 36/39 vs actual 38 |
| 17 | other | minor | `CLAUDE.md` fmt-caveat crate lists went stale (omit `eunomia-sys`, `eunomia-sys/fuzz`, `le-bytes`) |
| 18 | other | observation | `coretests` on-target skip list is empty (only comments); verified-count-vs-ledger is human convention, not CI-enforced |

---

## 1. Were the original goals achieved?

**Verdict: yes, substantially.** All 22 numbered tasks (0–21, plus the five
hyphenated inserts) shipped, and the load-bearing claim — a *verified* single- and
multi-threaded std runtime whose trusted surface is confined to sanctioned seams —
holds up under independent checking. The strongest evidence is that I could
reproduce the whole verification gate.

### 1.1 The verus gate is green on a live cold run (finding 1, positive)

Using the pinned prover (`verus --version` = `0.2026.06.07.cd03505`, toolchain
`1.95.0`), each std-port crate was `cargo clean -p`'d and re-verified. Every run
printed a real `verification results::` line (not a stale-cache no-op) with **0
errors**:

| crate | live result | ledger |
|---|---|---|
| `le-bytes` | `6 verified, 0 errors` | 6 ✓ |
| `eunomia-sys` | `16 verified, 0 errors` | 16 ✓ |
| `urt` | `29 verified, 0 errors` | 29 ✓ |
| `loader` (`--no-default-features`) | `30 verified, 0 errors` | 30 ✓ |
| `kcore` | `408 verified, 0 errors` | 408 ✓ |

Every count matches the trusted-base ledger exactly (the numbers this review was
originally handed — `eunomia-sys 7`, `loader 29`, `kcore 407` — were stale
mid-effort snapshots; the ledger tracks the *current* code, and the `7→16`
path-resolver, `29→30` `KIND_SEED`, and `407→408` census-fix bumps are all real
and documented). `kcore` emits only benign warnings (assert-forall style in
`irq.rs`; a trigger note in `cspace.rs`), no errors. The CI `verus` job runs the
same one-`-p`-per-crate gate across all 11 gated crates, including the two new ones
(`le-bytes`, `eunomia-sys`).

### 1.2 The verified surfaces are genuinely strong (findings 2, 3, positive)

Spot-audited against the code, not just the findings docs:

- **`eunomia-sys/src/encode.rs`** — `encode(Call) → Result<Encoded, CallError>` is
  entirely inside `verus!{}`, total over all 26 typed calls, with a per-variant
  `ensures` proving exact register placement *and* the inverse-leak refusals
  (`ObjType ≥ 8`, `len > 256`, `event > 2`, `prio ≥ 32`, `which > 1`) paired with
  their in-range acceptance. A host test round-trips every variant through the
  **real** `kcore::sysabi::decode`, with an anti-vacuity "teeth" control. This is
  the §11 inverse-leak rule re-established as a theorem at the seam.
- **`eunomia-sys/src/path.rs`** — `resolve(&[u8]) → Result<ResolvedPath, …>` is
  Verus-total over all inputs; `ensures … well_formed_resolved(p, buf@)` proves
  every output component is well-formed *and* a subrange of the input, and that no
  `..` survives resolution (depth-0 `..` → `Err(Escape)`). The `..`-above-root
  confinement is therefore machine-checked, not merely documented — with
  defense-in-depth on the server (`storage-server` `validate_path` → `validate_name`
  per component). A cargo-fuzz differential oracle uses a structurally independent
  reference resolver, so agreement is a real test.
- **`le-bytes`** and **`loader::startup::decode`** are genuinely inside `verus!{}`
  (total ∀-bytes decoder with `decreases` loops and subrange invariants).
- **`urt::tls::KeyTable`** is a verified allocator over the verified
  `SlotAlloc` (the `NEXT_SLOT` runtime counter was retired).

### 1.3 The extern-"Rust" bridge pivot is real (finding 3, positive)

The vendored std PAL declares undefined `extern "Rust" __eunomia_*` symbols
(`sys/pal/eunomia/{mod,common,futex}.rs`), and `eunomia-sys/src/pal.rs`
`#[no_mangle]`-defines the matching shims resolved at final link (the `__rust_alloc`
pattern). The vendored `library/std/Cargo.toml` has **no** `eunomia-sys` dependency —
only a NOTE explaining that it cannot be a sysroot dep because its verified deps pull
`vstd`, whose `verus_builtin` will not build as a rustc-dep-of-std. This corroborates
finding 7-2's account of why the sysroot path-dep was dropped.

### 1.4 Phase 3 — the kernel-track phase — is delivered (finding 4, positive)

Verified directly for this report:

- `kcore/src/thread.rs`: `TrapFrame` grew to 288 bytes with `pub tpidr: u64` at
  offset 272, guarded by a `const _` block asserting `size_of == 288` and the
  `offset_of` of `sp_el0`/`elr`/`spsr`/`tpidr` — exactly the const-assert the plan
  demanded to keep asm offsets honest. `kernel/src/exceptions.rs` has the
  `mrs/msr tpidr_el0` save/restore.
- `kcore/src/sysabi.rs`: `decode(nr, a: [u64; 7])` was widened from `[u64;6]`;
  `ThreadStartAs` carries the 7th `arg` in x6; decode is proven **total** with the
  `prio < NUM_PRIOS` range check preserved.
- `urt/src/futex.rs` and `urt/src/lock.rs`: **zero** `verus!{}` (correct — the
  concurrent-wakeup path is Loom/Shuttle-certified, never Verus per the SeqCst-pin
  constraint), with a four-way `cfg(loom)`/`cfg(shuttle)`/real/test backoff seam.
- `urt/src/random.rs`: loud no-seed abort; a test asserts `fill_bytes` never hands
  back the raw seed; per-child sub-seeds are proven distinct — the fork-without-reseed
  trap is addressed, and the non-crypto MVP status is loudly disclosed.

### 1.5 Phases 4–6 met their goals (findings 5, 6, positive)

- fs `Unsupported` surface routes through `unsupported()` → `ErrorKind::Unsupported`
  (never a silent success); the fs client is message-bounded (write loops
  `WRITE_CHUNK`, `encode_request` returns `TooLarge` past `MAX_MSG=256`).
- stderr resolves `NAME_STDERR → stdout channel → debug-log`; panic last-words stay
  on the debug-log path; init emits a `BASE_ENV` so `env::vars()` is non-empty; the
  shell is a genuine std binary that keeps spawn/reap on raw `loader::spawn` (its
  only `std::process` use is `exit`).
- CI wiring is real (finding A6-5); the PAL thin-delegator audit is a genuine
  38-symbol lockstep check, not an assertion (finding A6-6); the `20-1`/`20-2`
  follow-ups landed; `doc/guidelines/forward-port.md` is a real 324-line runbook
  that — unlike the ledger — is discipline-clean.

### 1.6 A side benefit: the port surfaced a latent kernel bug (finding 7, positive)

Finding 16-1's `cap_copy`/`derive` endpoint-census fix repairs a **pre-existing**
verified-kernel defect (baseline `derive` never called `endpoint_cap_added` and
carried no `end_caps_sound` obligation), surfaced by console-cap donation to a
child. `kcore` rises 407→408 for the added lemma. This is the integration work
stress-testing the kernel into revealing a real correctness bug — a positive.

---

## 2. Were any new problems introduced?

**Verdict: no blocker-class unverified code, but one real verification-posture
deviation (the fs readdir codec) and a handful of minor imprecisions.** No genuine
`TODO`/`FIXME`/`unimplemented!` loose ends survive in the changed non-vendor code
(every hit is a `#[cfg(test)]` mock, an off-target host-test stub, or a deliberate
loud-abort). Independently confirmed: the effort introduced **zero** new Verus
escape hatches — the global `external_body`/`assume`/`admit`/`assume_specification`
tally is 48 at both baseline and HEAD, `eunomia-sys`/`le-bytes`/`loader` add none,
and `urt`'s only hatch is the pre-existing `slots.rs` `external_body`.

### 2.1 The fs readdir wire codec is unverified on both sides (finding 8, major)

This is the single most substantive issue. `vendor/rust/.../sys/fs/eunomia.rs`
`parse_listing` (lines 484–517) is a **hand-rolled binary decoder**: a tag byte,
then per-entry `[kind:u8][size:u64 LE][name_len:u16 LE][name]`, parsed with
`u64/u16::from_le_bytes`, manual 11-byte-head bounds checks, and slice-cursor
arithmetic. That is protocol logic in the PAL — which the effort's own resolving
principle says must hold *zero* new logic and be "term-for-term delegation," and
which this very file's module doc claims: *"this file holds only std's
`File`/`ReadDir`/`FileAttr` bookkeeping … never any protocol logic"* and *"Every
real op is a one-line delegation."* Both statements are contradicted by
`parse_listing`.

Compounding it: the **gated** side that owns and encodes this same bespoke format,
`eunomia-sys/src/fs.rs`, has **no** `verus!{}` at all (its encoder hand-rolls the
mirror `to_le_bytes`). So a custom readdir wire format is unverified on *both*
sides, even though (a) a verified LE-reader crate (`le-bytes`, 6 verified) and
verified byte-decoders (`path`, `encode`) exist in-tree for exactly this shape, and
(b) it is not one of the three sanctioned unverified categories (inline asm; the
futex wakeup path; virtio-rng). The seam deliberately returns a raw `Vec<u8>`
(`__eunomia_fs_readdir → Vec<u8>`), forcing the PAL to decode, although it owns the
format and could have returned decoded entries.

Mitigations, stated fairly: the decoder is **bounded and memory-safe** by
inspection (explicit `len < 11` / `len < name_len` guards; `try_into().unwrap()` on
exactly-sized slices cannot panic) and returns `InvalidData` on malformed input, so
this is **not a correctness or safety bug** and the MVP data volumes are small. The
`postcard`-encoded request/response *body* is a sanctioned trusted seam by
feature-exclusion; the readdir flat-buffer is an adjacent bespoke codec that slipped
outside that justification. Rated **major** on verification-posture grounds (it is
the clearest bend of the thinness/verify-on-arrival rule and ships with a false
thinness claim), not on correctness.

### 2.2 `urt::random` DRBG is unverified (finding 9, minor)

`urt/src/random.rs` has no `verus!{}`; its structural invariants (never returns the
raw seed, distinct sub-seeds) rest on unit tests. This is explicitly disclosed
(`urt/src/lib.rs` and the module header: randomness quality is MVP-only, *not* a
verification property — only the seed *decode* in `loader::startup` is verified) and
fails loud on no seed. A deliberate, documented carve-out — but still a fourth
unverified category beyond the three the plan sanctions, resting on tests for its
non-quality structural properties.

### 2.3 Minor gaps (finding 10, minor)

- **Dangling spec citation.** `eunomia-sys/src/console.rs` cites "rev2§5.1 stderr
  fallback" as authority, but `stderr` appears **nowhere** in `doc/spec/spec_rev2.md`
  (the rev2§5.1 names list is root/stdin/stdout/tmp/storage/time/random-seed). The
  stderr decision was locked in the plan and implemented in code, but the spec was
  never extended to match, leaving both a dangling citation and a spec/code gap.
- **Imprecise errno.** A write to a sufficiently deep/long path can push
  `Request::Write` past `MAX_MSG=256` → `TooLarge` → `ERR_FS_INTERNAL`
  (`Uncategorized`). Honestly disclosed as an MVP limit and returns a clean error,
  but `Uncategorized` is a poor errno for "path too long for one message."

### 2.4 Dependency surface (finding 11, observation)

`eunomia-sys` lists `ipc` and `storage-server` under plain `[dependencies]` (only
`urt` is target-gated), so even `user/hello` now pulls `blake3`/`cas`/`storage-server`/
`serde`/`postcard` into its lock and relies on release LTO+strip DCE to shed them.
Bloat / trusted-surface note, not a defect (`loom`/`shuttle`/`proptest` are dev-deps
and never compiled in).

---

## 3. Was the comment & documentation discipline upheld?

**Verdict: no — this is the effort's weakest dimension, by a wide margin.**
`CLAUDE.md` is explicit: comments and documentation "describe what is, not what was,
or what was removed," "may reference `doc/spec` and `doc/guidelines`, nothing else,"
and "documents in `doc/plans` and `doc/results` … may not be referenced in comments,
or in specs and guidelines." Under the reviewer's stated expansion — *a plan-phase
or milestone reference counts as referencing the plan even without a path* — the
port breaches this systematically. (This was reproduced independently by six agents
with consistent counts, so it is not an artifact of one pass.)

### 3.1 Plan-phase / milestone references saturate code comments (finding 12, major)

`git grep -cE 'std-port [0-9]'` over non-doc, non-vendor source/manifests returns
**~216 occurrences across 48 files** (the baseline tree has **0** — the entire
`std-port N.N` vocabulary is new), plus ~30 `findings #N` / `Phase-N GATE`
references and `MVP`-as-milestone uses. These are the plan's sub-phase numbers
(`doc/plans/2_plan-std-revised.md`) and its findings-doc numbers appearing directly
in code comments. A representative sample (all verified verbatim in-tree):

| site | comment |
|---|---|
| `eunomia-sys/src/stdio.rs:7` | `//! Phase 5.1 moves stdout/stdin onto the userspace console channel` |
| `eunomia-sys/src/pal.rs:104` | `/// … at thread exit (std-port 3.5 — fixes the 3.2 leak).` |
| `eunomia-sys/src/heap.rs:4` | `… On the MVP there is no demand paging …` |
| `kernel/build.rs:259/262/265` | `… std-port Phase-2 GATE fixture (findings 7-1) …` / `Phase-4.1 …` / `Phase-5.1 …` |
| `user/hello/Cargo.toml:8` | `# Phase-5.3 real hello (findings #18) …` |
| `user/stdio/src/main.rs:1` | `//! The std-port console GATE fixture (findings #16)` |
| `user/hello/src/main.rs:2` | `//! findings #18). …` |
| `kcore/src/cspace.rs:10497…` | `// … endpoint census (findings 16-1) …` |
| `scripts/std-smoke-test.sh:2` | `# std-port Phase-2 GATE (findings 7-1)` |

Many `pal.rs`/`tls.rs` comments (`std-port 3.5 — fixes the 3.2 leak`, `3.2 leaked
it`) additionally violate the "what is, not what was" clause. Note the false
positives that were checked and **cleared**: `cas/src/store.rs:3010` "Phase 1:
directory moves" (an algorithm step, not a plan phase) and `kcore/src/cspace.rs`
"Plan the … teardown" ("Plan" as a verb) are *not* violations; `rev2§…` and
`doc/guidelines` references are explicitly allowed.

### 3.2 The trusted-base ledger — a guideline — carries plan references (finding 13, major)

Worse, `doc/guidelines/verus_trusted-base.md` (a `doc/guidelines` file, which may
cite only spec and other guidelines) contains **26** plan-phase / `findings #N` /
`Task N` references introduced by this effort — the baseline ledger had zero.
Examples: `std-port Phase 2.1` (line 282), `findings #9/#10/#11/#12` (393/421/448/474),
`Task 12/13` (227/231), `std-port 6.2 retired the copy` (271), `the phase 5.1/5.2/6.2
trigger reductions` (647). Notably, findings 21 claims its ledger audit removed "the
only such reference in the file" — it removed a `doc/results/…` path citation but
left these 26 plan-phase/milestone references untouched. (Sub-claim as literally
worded — no `doc/plans`/`doc/results` *path* remains — does pass; the phase/findings
references violate the same discipline under the expansion.)

### 3.3 The ledger narrates history (finding 14, minor)

The ledger states "what was" via count-delta arrows (`404 → 406`, `25 → 29`,
`7 → 16`, `272 → 288`, rlimit `169163 → 177414`) and history verbs (`retired`,
`replaces std-port 3.2's raw NEXT_SLOT counter`). A current-state ledger of the
trusted base should state the current seams and counts, not their phase-to-phase
evolution.

### 3.4 CI YAML step names (finding 15, observation)

`.github/workflows/ci.yml` step names/comments carry `std-port Phase-2 GATE` (210),
`std-port Phase-4.1 fs GATE` (216), `findings #19` (223). CI YAML is not clearly a
"code comment" nor a `doc/spec`/`doc/guidelines` file, so this is a gray-area
observation rather than a clear violation — but it is the same milestone-label smell,
in labels a maintainer reads.

**In fairness**, the discipline was clearly *understood*: `forward-port.md` is
scrupulously clean (it references only rev2§ and other guidelines and carries an
explicit "does not cite `doc/plans`/`doc/results`" disclaimer), and the sole spec
edit references only internal spec sections. The failure was one of *application* to
code comments and the ledger, not of comprehension.

---

## 4. Other insights

- **The ledger has internal inconsistencies (finding 16, minor).** Its authoritative
  Baseline row correctly records `eunomia-sys 16`, but two older routing notes (lines
  322, 418) still say `7` — a superseded count contradicting the same file. Several
  seam-row line-number citations have drifted (e.g. `is_boundary` cited at
  `prolly.rs:1457` now points at an unrelated lemma; `CapSlot::empty` at
  `cspace.rs:1595` points at `destroy_tcb`). And the human-written shim tally is
  inconsistent (findings 20 says "36 shims", 20-1/21 say "39") against the actual
  **38** (verified by diffing the declared vs defined symbol sets — empty symmetric
  difference; the lockstep itself is intact). These are documentation-accuracy slips,
  not code defects.

- **`CLAUDE.md` fmt lists went stale (finding 17, minor).** The fmt-caveat crate
  enumeration omits `eunomia-sys` and `le-bytes` (both root-workspace members), and
  the `*/fuzz` enumeration omits `eunomia-sys/fuzz` (a separate excluded workspace —
  precisely the trap the caveat exists to warn about). The effort both introduced
  these crates and edited `CLAUDE.md`, but did not update the lists. (`le-bytes`'s
  omission predates the effort.)

- **The gate is weaker than it reads (finding 18, observation).** The `coretests`
  on-target skip list (`scripts/libtest-skips/coretests.skip`) has **0** real
  entries — header comments only — so the "committed skip-list deliverable" is
  effectively vacuous on the coretests half (`alloctests.skip` has 2). Separately,
  the CI verus job asserts only "0 errors", never the `N verified` number, so a count
  drift from the ledger would pass CI silently — the count↔ledger binding is human
  convention, not automation (contrast the TLA jobs, which pin distinct-state counts
  via `TLC_ASSERT_MANIFEST`). Mitigant: the live cold runs in §1.1 show every count
  currently matches, so the convention is being honored in practice.

- **The "claimed counts" moved under the plan.** Any reviewer starting from the
  plan/early-findings numbers (`eunomia-sys 7`, `loader 29`, `kcore 407`) will find
  the live tool disagrees; the ledger, not the plan prose, is the current
  source-of-record and is accurate. Worth internalizing before trusting any number
  quoted mid-plan.

- **Housekeeping is clean (positive).** The draft/first/review plans were properly
  deleted (only `2_plan-std-revised.md` remains); no stray `.orig`/`.bak` files; the
  `scratchpad` workspace member is pre-existing, not this effort's artifact; the
  `.gitmodules` `vendor/rust` fork addition (with its nested `library/backtrace`) is
  consistent with `CLAUDE.md`. The smoke/gate tests assert positive markers *and*
  negative anti-markers and are CI-wired.

## Conclusion

The port achieved its goals: a verified std runtime for Eunomia, with every gated
crate verifying green on a live cold run, the trusted surface confined to sanctioned
seams, and the verification-heavy pieces (syscall encode, path/startup decode, TLS
key table) genuinely proven rather than asserted. It even repaired a latent
verified-kernel bug in passing. The verification discipline held almost everywhere;
the one real deviation is the bespoke, unverified readdir wire codec carried on both
sides of the fs seam (§2.1), shipped under a PAL doc-comment that wrongly claims the
arm contains no protocol logic.

The comment & documentation discipline, by contrast, was **not** upheld: hundreds of
plan-phase/findings/milestone references were introduced into code comments and — more
seriously — into a `doc/guidelines` ledger that is meant to reference only spec and
guidelines, together with "what-was" changelog narration in that same ledger. None of
this affects correctness, but under the project's own `CLAUDE.md` rule it is a
systematic breach and the clearest thing for a follow-up to remediate (a mechanical
sweep of `std-port N.N`/`findings #N`/`Phase-N` out of comments, the ledger, and the
CI step names would close most of it).
