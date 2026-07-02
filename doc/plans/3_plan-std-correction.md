# Plan — std-port corrections (acting on the findings-#22 review)

> Targets `spec rev2`. Grounded in the **current** tree (`main`, post-#292, the
> completed std port). This plan does **not** supersede
> `doc/plans/2_plan-std-revised.md` — the port it planned is done; this is the
> follow-up correction effort acting on the independent review
> `doc/results/22_std-port-review.md` (findings 8–18; findings 1–7 are positives
> requiring no action). Conventions carry over: each sub-phase is a
> separately-implementable task producing exactly one findings doc, numbering
> continuing at **23** (22 is the review itself); inserted work takes hyphenated
> numbers (the `7-1`/`7-2` precedent).

## Status at a glance

| Task | Scope | Review findings addressed | State |
|---|---|---|---|
| **C1** | fs readdir cursor seam; errno precision; *(optional)* verified DRBG fill | 8 (major), 10-errno, (9 in part) | remaining |
| **C2** | rev2§5.1 `stderr` amendment + citation audit | 10-spec | remaining |
| **C3** | trusted-base ledger rewrite (references, history, drift) | 13 (major), 14, 16, 9 (doc half) | remaining |
| **C4** | comment sweep over code/scripts/manifests + CI step retitles | 12 (major), 15 | remaining |
| **C5** | `CLAUDE.md` crate-list refresh | 17 (+ one gap the review missed) | remaining |
| **C6** | verified-count pinning in CI; skip-list honesty | 18 | remaining |
| **C7** | dependency-surface posture: verify and record | 11 | remaining |

Every reviewed problem maps to exactly one task above; the review's positives
(findings 1–7) and its "claimed counts moved under the plan" insight need no code
change (the latter is closed structurally by C6's count pinning).

---

## Where the review itself is corrected

Three of the review's claims were re-checked against the tree and found imprecise.
The tasks below act on the corrected versions, so implementers should not "fix"
the review's literal wording:

- **Finding 11's premise is wrong, its substance stands.** `ipc` and
  `storage-server` are **not** plain `[dependencies]` — they sit with `urt` under
  `[target.'cfg(any(target_os = "eunomia", target_os = "none"))'.dependencies]`
  (`eunomia-sys/Cargo.toml:33-51`), so host builds and the verify graph never see
  them. The substance survives because user binaries are built *for* that target:
  every std binary still links the `storage-server`/`cas`/`blake3` stack and
  relies on release LTO/DCE to shed it. C7 verifies and records that posture
  rather than re-gating anything.
- **"Three sanctioned unverified categories" is the review's shorthand, not the
  project's taxonomy.** `doc/guidelines/verus.md` §11 enumerates **four**
  legitimate `external_body` categories, and concrete runtime carve-outs are
  sanctioned as routing notes in `doc/guidelines/verus_trusted-base.md`. The
  `urt::random` DRBG already has such a note (the entropy routing note). Finding
  9 therefore reduces to *tightening* that sanction, not creating a new one — see
  the C3.4 deliverable and Global decision 2.
- **The 36/39 shim-tally inconsistency lives only in `doc/results` findings docs**
  (20 vs 20-1/21), which are temporary intermediate reports; the ledger itself
  states no shim count. Nothing to fix in guidelines — C1.1 changes the true
  count anyway (38 → 40) and updates the one guideline that enumerates the
  symbols (`doc/guidelines/forward-port.md`).

---

## Global decisions

Made once, each with its rationale.

1. **readdir (finding 8): eliminate the bespoke codec by restructuring the seam —
   do not verify it in place.** The flat `[tag][kind][size:u64 LE][name_len:u16
   LE][name]` buffer is a *redundant second serialization*: storaged already
   sends structured `Response::Listing(Vec<DirEnt>)` over the sanctioned postcard
   body seam; `eunomia-sys` postcard-decodes it, re-encodes it flat purely to
   cross the `extern "Rust"` bridge, and the PAL re-decodes it. Verifying the
   encoder would leave the PAL decoder unverified forever (std cannot depend on
   `vstd`), the thinness claim still false, and the cross-bridge layout
   duplication alive. Replacing the one `Vec<u8>` shim with a cursor protocol in
   the existing seam vocabulary deletes the format on both sides: nothing new to
   verify, the PAL becomes a genuine thin delegator again, and one of the three
   review-kept cross-bridge duplications disappears.
2. **`urt::random` (finding 9): sanction precisely, don't verify.** The DRBG's
   structural invariants are not theorems: "`fresh_seed()` never returns the raw
   seed" is a *sampled* property of the xoshiro256\*\* transition (fixed points
   over the 2^256 state space are not excluded ∀ states), and sub-seed
   distinctness is statistical. Randomness quality is explicitly off the proof
   surface (rev2§5.1: only the seed *decode* is mechanized). The correct posture
   is the one the ledger already routes — host-tested plain Rust under the §11
   categories (2) out-of-scope function and (3) runtime guard — stated
   *explicitly* so no future review reads it as an unsanctioned carve-out
   (C3.4). Optionally, the one genuinely provable piece — `Drbg::fill`'s
   little-endian word serialization — can be lifted onto the existing
   `le_bytes::u64_le` spec (C1.3, kept only if the proof cost is clean).
3. **Sweep rules (findings 12–15).** The violations are almost always a
   parenthetical `(std-port N.N)` / `(findings #N)` appended to a sentence whose
   rationale is already current-state. The sweep therefore: drops the
   plan-phase/findings/milestone token; keeps the rationale and every `rev2§` /
   `doc/guidelines` citation (both explicitly allowed); rewrites the handful of
   "what was" clauses ("fixes the 3.2 leak", "replaces the raw `NEXT_SLOT`
   counter") into present-tense descriptions of what the code does now. **"MVP"
   is spec vocabulary** (rev2§5.1 uses it for the entropy posture) — keep it
   where it describes the current system's scope; rephrase only where it labels
   a milestone. The project-authored files inside `vendor/rust` are already
   clean (verified: zero hits) and need no sweep.
4. **Ordering: code → spec/citations → ledger → sweep → `CLAUDE.md` → gate
   hardening → posture record.** The ledger rewrite re-derives file:line
   citations and counts, so it must see the post-C1 tree; the comment sweep must
   not rewrite comments that C1/C2 are about to delete or re-cite; count pinning
   (C6) must see final counts (including the optional C1.3 delta). C5 and C7 are
   independent but batched late as documentation passes.

---

## Tasks

Ordered by implementation order. Gate notation as in the previous plan: the
verification/test that must be green before the task is "done".

> A *real* `cargo verus verify` run ends each crate with a `verification
> results:: N verified, 0 errors` line; a re-run over an unchanged `target/`
> reports *nothing* (stale cache). Clean the crate (`cargo clean -p <crate>`)
> before any gate that claims a count, per `CLAUDE.md`. Every task also carries
> the standing `CLAUDE.md` obligations: `cargo fmt` per touched workspace
> (`user/*` and `*/fuzz` need their own `--manifest-path` runs),
> `scripts/verusfmt.sh` for any `verus!{}` touch, and
> `scripts/verus-baseline.sh` before/after any proof change (C1.3).

### C1 — seam & code corrections

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C1.1** | delete the readdir wire codec (finding 8) | Replace `__eunomia_fs_readdir(path: &[u8]) -> Vec<u8>` (decl `vendor/rust/library/std/src/sys/fs/eunomia.rs:60`, def `eunomia-sys/src/pal.rs:314`) with a cursor protocol in the existing seam vocabulary: **(a)** `__eunomia_fs_readdir_open(path: &[u8]) -> i64` — performs the `Request::List` round-trip, keeps the postcard-decoded `Vec<DirEnt>` snapshot in a small handle table, returns handle ≥ 0 or a negative fs error code; **(b)** `__eunomia_fs_readdir_next(handle: i64, name_buf: &mut [u8]) -> DirEntMeta` — copies one entry's name into the caller's buffer and returns a mirrored `#[repr(C)]` head on the `Meta`/`FsMeta` precedent (`eunomia-sys/src/fs.rs:264-269` / `sys/fs/eunomia.rs:41-46`): `{code: i64 (0 = entry, 1 = end, < 0 = error), kind: u8, size: u64, name_len: u16}`; `name_buf` sized to the 255-byte component bound the path resolver enforces, over-long names refused with an error code, never truncated; **(c)** `__eunomia_fs_readdir_close(handle: i64)`, called from the PAL `ReadDir` drop. The handle table is SpinLock-guarded client bookkeeping (the `urt::Heap`/`random.rs` `unsafe impl Sync` posture), *not* protocol logic; snapshot semantics are unchanged (today's flat buffer is the same whole-listing snapshot). **Delete** `parse_listing` (`sys/fs/eunomia.rs:481-517`), the flat encoder `readdir`/`err_buf` and `RD_OK`/`RD_ERR`/`RD_ENTRY_HEAD` + its `const` guard (`eunomia-sys/src/fs.rs:372-425`). **Rewrite both module docs** so the thinness claims are true statements about the new shape (`sys/fs/eunomia.rs:1-6` "never any protocol logic"; `fs.rs:16-18` "no byte-parsing logic"). **Update `doc/guidelines/forward-port.md`**: the fs shim enumeration (line 142) and the cross-bridge-duplication note (line 259 — the readdir-layout duplication ceases to exist); the symbol lockstep goes 38 → 40. Where cheap, keep the cursor arithmetic in a cfg-free helper with a host unit test (the fs client is otherwise `cfg(bare_metal)`-only and untestable off-target) | `scripts/fs-smoke-test.sh` (the `[stdfs] readdir found smoke` STD4 marker) + `scripts/std-smoke-test.sh` (shell `ls`) green under QEMU; symbol lockstep re-diffed (declared vs defined `__eunomia_*` sets, empty symmetric difference, **40**); `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` still **16 verified, 0 errors** (no `verus!{}` change); fmt | **23** |
| **C1.2** | precise errno for over-long requests (finding 10, errno half) | In `eunomia-sys/src/fs.rs:95` (`request()`), replace `.map_err(\|_\| ERR_FS_INTERNAL)` with a match that maps `WireError::TooLarge` → `ERR_FS_BAD_PATH` (classifies to `Kind::InvalidFilename` — `io_error.rs:124`, the kind Unix maps `ENAMETOOLONG` to) and keeps the other `WireError` variants on `ERR_FS_INTERNAL`. This is semantically honest: after `WRITE_CHUNK` caps the data payload, the encoded path is the only input that can push a request past `MAX_MSG` — a *nameable* path (components ≤ 255, depth ≤ 64) that is too long to frame in one message. No wire/ABI change; the `(ERR_FS_BAD_PATH, InvalidFilename)` oracle row already exists. **Optionally** add the anti-vacuity teeth: a deep-path negative case in `user/stdfs` asserting `ErrorKind::InvalidFilename` with a new smoke marker awaited by `fs-smoke-test.sh` | fs + std smoke green (incl. the new marker if added); `io_error` host oracle table unchanged; fmt | **24** |
| **C1.3** *(optional)* | verified DRBG byte serialization (finding 9, optional half) | Lift `Drbg::fill`'s little-endian word-chunking (`urt/src/random.rs`) into `verus!{}`, proving the output bytes are the concatenation of `le_bytes::u64_le(word)` images (the spec and its `by (bit_vector)` lemmas already exist in `le-bytes`; `urt` would gain the dep). Scope strictly the serialization — the xoshiro transition and all quality properties stay out of the proof surface per Global decision 2. Adopt **only if** the cold-run `rlimit` delta measured with `scripts/verus-baseline.sh` (before/after, byte-identical control) is acceptable; on regression, drop the task — the sanction in C3.4 stands alone | `cargo clean -p urt && cargo verus verify -p urt` (count rises above 29; record the new count for C3/C6); baseline diff recorded; Miri/proptest suite unchanged-green; verusfmt + fmt | **23-1** (insert, only if adopted) |

### C2 — spec & citation integrity (finding 10, spec half)

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C2.1** | put `stderr` in the spec | Amend the rev2§5.1 Standard-names sentence (`doc/spec/spec_rev2.md:363`) to add `stderr` — the edit the previous plan's spec-change row #5 decided but never applied. Phrasing in the section's own style, e.g.: *"`stderr` (kept distinct from `stdout` so diagnostics never enter a pipeline's data; a terminal grants the same console channel under both names; when ungranted, a process's `stderr` falls back to its `stdout` channel, else the debug log)"*. In-place edit per the spec's convention (no changelog; §8.4 is only for superseded approaches, which this is not) | `stderr` resolves in rev2§5.1; the spec's names list matches the grant names init actually emits | **25** |
| **C2.2** | every citation backed | Re-check each `rev2§5.1` citation in the stderr/entropy neighborhoods against the amended text: `eunomia-sys/src/console.rs:4,47,134,186,265`, `eunomia-sys/src/grant.rs:45`, `user/init/src/main.rs:276`, and the `urt/src/random.rs` module-header "two invariants (rev2§5.1)" claim (§5.1's random-seed sentence covers fresh-seed-per-child and the decode-only-mechanized posture; if the never-returns-the-raw-seed invariant is not attributable to it, either extend the sentence minimally or re-anchor the comment to what §5.1 does say). Adjust cite or spec until each resolves to backing text — a citation is a claim, and this task makes every touched claim true | grep-driven checklist: every `rev2§5.1` cite in the files above resolves to backing spec text; no new dangling references introduced | **25** (same doc as C2.1 — one reviewable change) |

### C3 — trusted-base ledger rewrite (findings 13, 14, 16, + 9's doc half)

One coherent pass over `doc/guidelines/verus_trusted-base.md` — a guideline, which
may reference only spec and other guidelines — preserving its structure and
function: the five sections, the 14-seam tally, the `rev2§` citations, and the
`## Baselines` regression table. Line numbers below are pre-C1 and shift with it;
re-locate by content.

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C3.1** | purge plan-provenance references | Remove all ~28 plan-phase / `findings #N` / `Task N` reference lines: the `std-port Phase N` headers (280/282/301/325/348/368), routing-note titles and prose (271/345/352/392/420/448/473/482/504), the `findings #9`–`#12` keys (393/421/448/474), `Task N` refs (227/231/232/651), and the Baseline-cell refs (643/649/652/653). Routing notes are retitled by **surface** ("Futex-backend routing note", "Entropy-seed routing note", "Console-stdio routing note", …) — most already lead with the surface; only the parenthetical provenance goes | `grep -nE 'std-port\|findings #\|findings [0-9]+-[0-9]\|Task [0-9]\|doc/(plans\|results)' doc/guidelines/verus_trusted-base.md` → 0 hits | **26** |
| **C3.2** | current state, not history | Convert the "what was" narration to current-state prose: count-delta arrows (`404 → 406`, `25 → 29`, `7 → 16`, `272 → 288`, rlimit `169163 → 177414`; lines 208/252/371/456/478/510/643/649/652/653) become the current number only; `retired`/`replaces`/`widened` history verbs (178/230/251/271/345/413/482, 384/417) become descriptions of what is. Keep a path-not-taken note **only** where the current shape is surprising without it (the `CLAUDE.md`-sanctioned exception — e.g. why TLS keys are a verified table rather than a counter can survive as rationale, without naming what it replaced) | no count-delta arrows between numerals remain; a read-through finds no sentence whose subject is a past state of the tree | **26** |
| **C3.3** | fix internal drift | Correct the two stale `eunomia-sys 7` notes (lines 322/418 → **16**); re-derive **every** seam-table file:line citation against the post-C1 tree, not just the two known-bad ones (`is_boundary` → `cas/src/prolly.rs:1320`; `CapSlot::empty` → `kcore/src/cspace.rs:177`, its `assume_specification` at 1849). Where a citation keeps drifting, prefer symbol-only citations (file + construct name) over hard line numbers | every cited symbol found at its cited location (spot-check script or manual pass recorded in the findings doc); Baseline counts match a live cold verify of each gated crate | **26** |
| **C3.4** | make the DRBG sanction explicit | Tighten the entropy routing note (448-471): name the `verus.md` §11 categories the DRBG folds under — (2) out-of-scope function for the xoshiro/splitmix shuffle (trusting totality and determinism, no deeper property) and (3) runtime guard for the no-seed loud abort — and enumerate its host tests by name (deterministic-stream, distinct-sub-seeds, never-returns-raw-seed, all-zero-seed guard, no-seed abort). The ledger is the sanctioning mechanism; after this note no reader can classify the DRBG as an unsanctioned carve-out. If C1.3 was adopted, note the verified `fill` serialization and the new urt count here | the note names both §11 categories and every host test; consistent with the (possibly updated) urt Baseline row | **26** |

### C4 — comment sweep (findings 12, 15)

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C4.1** | de-plan the code comments | Apply Global decision 3 across the 48 files (~217 `std-port N.N` sites + ~47 `findings #N`/`Phase-N` tokens, ~245 lines). Heaviest: `user/shell/src/runtime.rs` (28), `eunomia-sys/src/pal.rs` (20), `scripts/std-smoke-test.sh` (17), `user/init/src/main.rs` (15), then `user/shell/src/main.rs`, `eunomia-sys/src/lib.rs`, `urt/src/lib.rs`, `eunomia-sys/src/io_error.rs`, the `user/*/Cargo.toml` comments, `kernel/build.rs`, `kcore/src/{cspace,test_store}.rs`, both `scripts/libtest-skips/*.skip` headers, and the remaining scripts. Named "what-was" rewrites (not just de-numbering): `eunomia-sys/src/pal.rs:104`, `eunomia-sys/src/tls.rs:78,160`, `urt/src/tls.rs:7`. **Cautions:** smoke scripts *grep for output markers* — sweep comments and headers only, never a marker string or the code that emits one; `user/*` and `*/fuzz` are separate workspaces, format each via its own manifest; false positives stay ("Phase 1: directory moves" in `cas/src/store.rs` is an algorithm step; "Plan the teardown" is a verb; `rev2§`/guideline cites are allowed and stay) | `git grep -nE 'std-port [0-9]\|std-port Phase\|findings #[0-9]\|findings [0-9]+-[0-9]\|Phase-[0-9]' -- ':!doc/plans' ':!doc/results' ':!vendor'` → 0 hits, **plus** a recorded manual pass over `git grep -nE '\bPhase [0-9]'` (the space-separated form the first regex misses — e.g. `ci.yml:213` "Phase 2's sub-phases (2.1–2.4)"), clearing algorithm-step uses and rewriting milestone uses; `scripts/std-smoke-test.sh` + `scripts/fs-smoke-test.sh` still pass under QEMU (markers untouched); `cargo fmt --check` clean in every touched workspace | **27** |
| **C4.2** | de-milestone the CI labels | Retitle the three `on-os` step names (`.github/workflows/ci.yml:210/216/221`) by the surface exercised, e.g. `std runtime QEMU gate (println!/format!/Vec/Box/String/Instant/SystemTime)`, `std fs QEMU gate (File/read/write/read_dir/rename/remove/sync)`, `on-target library-test triage (coretests/alloctests subset)`; rewrite the step *comments* too — the `findings #19` reference (line 223) and the "Phase 2's sub-phases (2.1–2.4)" sentence (lines 213-214) | same greps as C4.1 cover the YAML; CI workflow parses (a dry `workflow_dispatch` or push to a branch) | **27** |

### C5 — `CLAUDE.md` refresh (finding 17 + one gap the review missed)

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C5.1** | true crate enumerations | Three list fixes in `CLAUDE.md`: **(a)** the fmt-caveat root-workspace member list (lines 316-317) gains `eunomia-sys` and `le-bytes`; **(b)** the excluded-fuzz-workspace list (line 320) gains `eunomia-sys/fuzz`; **(c)** the Verus gate command list (lines 217-246) gains `cargo verus verify -p le-bytes` and `cargo verus verify -p eunomia-sys` with one-line purpose annotations in the list's house style — CI already runs both (`ci.yml` verus job), so the documented gate becomes the full gate | the three lists match the root `Cargo.toml` `members`/`exclude` and the CI verus job's `-p` lines exactly (a diff recorded in the findings doc) | **28** |

### C6 — gate hardening (finding 18; after counts settle)

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C6.1** | pin verified counts in CI | Give the Verus gate the TLA treatment (`TLC_ASSERT_MANIFEST` / `tools/tla/model-manifest.tsv`, "a coverage shrink fails a PR"): add `tools/verus/verus-manifest.tsv` — one row per gated crate: crate, extra flags (`--no-default-features`, `--lib`, …), `expected_verified` — and a driver (`tools/verus/verus-gate.sh`) that per row runs `cargo clean -p <crate> && cargo verus verify -p <crate> <flags>`, extracts the `verification results:: N verified, 0 errors` line, **fails if the line is absent** (the stale-cache false-green `CLAUDE.md` warns about) or if `N` mismatches the manifest. Point the CI verus job at the driver (replacing its 11 hand-rolled steps, or wrapping them); the ledger's `## Baselines` section names the manifest as the machine-readable twin of its Result cells, which must stay in agreement | CI verus job green through the driver with all counts asserted; a deliberate off-by-one in the manifest fails the job (anti-vacuity check, then reverted); ledger counts == manifest counts | **29** |
| **C6.2** | honest skip lists | `scripts/libtest-skips/coretests.skip` has zero entries — currently reading as a placeholder. Re-run the on-target triage (`scripts/libtest-on-target.sh`) for coretests; if zero skips is the true state, rewrite the header to say so (an *empty list as verified current state*, with the run that established it named); if skips surface, populate with per-entry rationale in the `alloctests.skip` style. Headers themselves were de-planned in C4 | libtest-on-target run recorded; header states the verified meaning of emptiness | **29** |

### C7 — dependency-surface posture (finding 11; smallest, independent)

| Sub | Goal | Deliverables | Gate | Findings |
|---|---|---|---|---|
| **C7.1** | verify and record the DCE posture | Measure what finding 11 only inferred: build a minimal std binary (`user/hello`) in release, confirm via `llvm-nm`/`llvm-size` (or the linker map) that no `blake3`/`cas`/`storage-server` symbols survive LTO+DCE, and record the measured posture (binary size, symbol check) in the findings doc. The Cargo.toml comments already document the target-gating rationale; extend `doc/guidelines/forward-port.md` only if the audit finds the posture worth a standing check. **If** DCE does *not* shed the stack, escalate the deferred feature-gating item into a real task | measurement recorded; either "posture confirmed" or a follow-up task filed | **30** |

### Dependency & parallelism map

```
C1.1 ──┬────────────► C3 ──► C4 ──► C5 ──► C6
C1.2 ──┤                                  ▲
C2   ──┘   (C1.3 optional insert after C1.1; its urt count feeds C3.4 + C6.1)
C7 ─── independent, any time (batched last as a documentation pass)
```

- C1 and C2 are mutually independent and can run in parallel; both precede C3
  (ledger line-citations and the routing-note content must reflect the final
  code and spec) and C4 (the sweep must not rewrite comments C1/C2 delete or
  re-cite).
- C6.1 is last among the gated work: the manifest pins the counts that C1.3
  (optional) and C3 finalize.

---

## Findings-doc requirement

As in the previous plan: every task produces exactly one findings document at
`doc/results/<N>_<slug>_findings.md`; the optional C1.3 is an insert and takes a
hyphenated number only if adopted.

| N | Task |
|---|---|
| 23 | C1.1 readdir cursor seam |
| 23-1 | C1.3 verified DRBG fill *(only if adopted)* |
| 24 | C1.2 errno precision |
| 25 | C2 spec `stderr` amendment + citation audit |
| 26 | C3 trusted-base ledger rewrite |
| 27 | C4 comment sweep + CI retitles |
| 28 | C5 `CLAUDE.md` refresh |
| 29 | C6 gate hardening |
| 30 | C7 dependency-surface posture |

Each records decisions (with rejected alternatives), problems hit, the exact gate
commands and result lines, any surface left trusted and why, and follow-ups. Per
`CLAUDE.md`, `doc/plans` and `doc/results` are temporary intermediate reports:
nothing this effort writes may reference them from code comments, specs, or
guidelines — the whole point of C3/C4 is to *remove* such references, so the
sweep gates double as the discipline check on this effort's own output.

## Deferred work

- **Feature-gating fs out of `eunomia-sys`.** *Replaces:* the LTO/DCE posture C7
  records. std's PAL declares the `__eunomia_fs_*` symbols unconditionally, so
  gating the shims out is link-fragile; only worth designing if C7's measurement
  shows DCE failing.
- **Verified write-direction `le-bytes`.** *Replaces:* nothing today — C1.1
  deletes the last hand-rolled LE encoder outside `verus!{}` reach. Worth
  extracting only when a next verified encoder needs it (the `u*_le` specs it
  would prove against already exist).
- **A dedicated name-too-long error code + `Kind` variant.** *Replaces:* C1.2's
  `ERR_FS_BAD_PATH`/`InvalidFilename` mapping, which is honest and precise enough
  for the MVP; a dedicated code would need the full appended-discriminant
  lockstep (const pins + PAL `decode_error_kind`) for marginal errno fidelity.
