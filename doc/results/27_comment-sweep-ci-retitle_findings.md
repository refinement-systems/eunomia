# Findings 27 — comment sweep + CI step retitles (C4, review findings 12 & 15)

Task **C4** of `doc/plans/3_plan-std-correction.md`, acting on findings **12**
(plan-provenance references leaked into code comments) and **15**
(milestone-flavoured CI step labels) of the independent review
(`doc/results/22_std-port-review.md`). Both sub-tasks land here as one reviewable
change: **C4.1** de-plans the comments across code/scripts/manifests, **C4.2**
retitles the CI steps by the surface they exercise.

**Headline:** the tree carried ~250 plan-provenance tokens in comments —
`(std-port N.N)`, `(findings #N)`, `Phase-N`, and the bare phase shorthand
(`(2.3)`, `the 3.2 heap spinlock`, `pre-5.1 behavior`). All are removed;
every `rev2§` / spec / guideline citation, and every legitimate number (Rust
versions, byte sizes, `spec 2.6`, `§4.6`, `sleep 0.2`), is kept. The one genuine
algorithm-step `Phase 1:` in `cas/src/store.rs` stays (it names a step of the
directory-move algorithm, not a plan milestone). The result: the two gate greps
below are clean, the CI step names describe surfaces, and no comment needs the
plan doc to decode.

## The problem (findings 12 & 15)

Per `CLAUDE.md`, comments describe *what is*, not *what was*, and may reference
**only** `doc/spec` and `doc/guidelines`. References to `doc/plans` /
`doc/results` — including the concepts they introduce (`phase 2.1`, `finding 3`,
`milestone M3`) — are disallowed in code, comments, scripts, and manifests.
Finding 12 catalogued the leak in code comments; finding 15 the CI step names
(`std-port Phase-2 GATE`, …).

## Decision 1 — the `\bPhase [0-9]` gate is vacuous under `git grep -E` here

The plan's secondary gate is `git grep -nE '\bPhase [0-9]'`. On this platform
(Apple git 2.54) `-E` (POSIX ERE) does **not** honour `\b` as a word boundary —
it matches zero regardless, so the gate passes *trivially* whether or not
space-form `Phase N` references exist. Only `git grep -nP '\bPhase [0-9]'` (PCRE)
actually detects them. Re-running with `-P` surfaced real references the `-E`
gate silently missed, including two the plan's line-based enumeration did not
list: `.github/workflows/ci.yml:3` ("Phase 4 of the process-caps plan") and
`ci.yml:245` ("2_ipc.md Phase 0") — both direct plan-doc references. **Recorded
so any future audit uses `-P` (or `(^|[^-])Phase [0-9]`), not `-E`.**

## Decision 2 — sweep the bare phase shorthand too, not just the enumerated tokens

The plan enumerated `std-port N.N` / `findings #N` / `Phase-N`. A triage found the
same violation in shorthand the gate regex never catches: `(2.3)`, `(3.1/3.2)`,
`the 3.2 heap spinlock`, `the 4.2 seam`, `pre-5.1 behavior`, `the 3.5 key-based
teardown`, and two prose refs to "the plan" (`user/hello/src/main.rs:14`,
`user/shell/src/runtime.rs:118`). These are finding-12's exact class — a reader
needs the plan doc to decode `(2.3)` — and leaving them would be a half-fix that
a reviewer (this effort *is* a review response) would flag. They were swept.
Legitimate `N.N` numerals were kept by triage: Rust versions (`1.83–1.87`), spec
sections (`spec 2.6`, `§4.6`, all `rev2§*`), byte sizes, and the `sleep 0.2`
command.

*Kept:* `cas/src/store.rs:3010` `// Phase 1: directory moves …` — an algorithm
step (paired with "the file phase below"), not a plan milestone; the plan
explicitly calls this a false positive to keep.

## Decision 3 — the `findings 16-1` sites are swept even though they are not std-port

`kcore/src/{cspace,test_store}.rs` cited `findings 16-1` (the `cap_copy`/`derive`
endpoint-census bug) — unrelated to the std port, but the tree-wide primary gate
(`findings [0-9]+-[0-9]`) catches them and `CLAUDE.md` bars *all* `doc/results`
references, so they are in scope. The `rev2§3.3` citations stay; only the
`findings 16-1` token goes. One "what-was" clause is rewritten to present:
`cspace.rs`'s "the invariant `cap_copy` previously violated at runtime" →
"without the bump, `cap_copy` would spuriously fire peer-closed on a live end"
(the failure the lockstep census bump guards against, stated as present-tense
rationale rather than history). These edits are comment-only inside `verus!{}`;
verification is unaffected (see record).

## Method — conservative transform + full-diff review + hand-edits

The ~218 mechanical sites (a trailing/embedded plan token on an otherwise
current-state sentence) were removed by a one-off Python transform run in
dry-run first, its full diff reviewed, then applied — more reviewable than ~250
blind edits, and deterministic. Its rules drop the token, keep every `rev2§` /
guideline cite, and clean the resulting punctuation without touching leading
indentation. Precise `(file, line)` skips held back every line needing judgment;
those ~60 were hand-edited from the original text:

- **Named "what-was" rewrites** to present tense: `eunomia-sys/src/pal.rs`
  (TLS-block reclaim; the panic-sink stdio path), `eunomia-sys/src/tls.rs`,
  `urt/src/tls.rs` ("one shared allocator", dropping "replacing … `NEXT_SLOT`"),
  `urt/src/lib.rs` (the `Sync`-soundness rationale, from "before/replaced" to a
  present statement of why the lock is what makes concurrent allocation sound),
  `eunomia-sys/src/stdio.rs`.
- **Multi-line parentheticals** where a token spanned lines and a naive removal
  would orphan a `)`, `.`, `,`, or `;` (e.g. `fs.rs`, `alloctests.skip`,
  `urt/thread.rs`, `user/hello/src/main.rs`, `user/storaged/src/main.rs`,
  `runtime.rs`).
- **Surface-named labels** where dropping the phase left a bare "GATE fixture":
  the std-runtime gate fixtures/scripts now read "std runtime GATE"
  (consistent with the C4.2 CI retitle).

An artifact scan over every changed line confirmed no orphaned punctuation,
empty parens, double spaces, trailing whitespace, or residual tokens.

## C4.2 — CI step retitles

`.github/workflows/ci.yml`: the three `on-os` step names now name the surface,
and the step comments are rewritten:

| Old step name | New step name |
|---|---|
| `std-port Phase-2 GATE (println!/…)` | `std runtime QEMU gate (println!/format!/Vec/Box/String/Instant/SystemTime)` |
| `std-port Phase-4.1 fs GATE (…)` | `std fs QEMU gate (File/read/write/read_dir/rename/remove/sync)` |
| `std-port Phase-6.1 on-target library-test triage (…)` | `on-target library-test triage (coretests/alloctests subset)` |

Comments swept: the "Phase 2's sub-phases (2.1–2.4) … deferred to this gate"
sentence, the `(findings #19)` reference, the header "Phase 4 of the process-caps
plan", and the "2_ipc.md Phase 0" doc reference.

## What shipped

59 files changed (`git diff --stat`: 319 insertions, 322 deletions) — comments,
script comments, `Cargo.toml` comments, `.skip` headers, and the CI YAML. No
non-comment line was modified (verified by the artifact scan and by fmt/verify
still passing). Heaviest: `user/shell/src/runtime.rs`, `scripts/std-smoke-test.sh`,
`eunomia-sys/src/pal.rs`, `user/init/src/main.rs`, `user/stdsmoke/src/main.rs`.

## Verification record

- **Primary gate** — `git grep -nE 'std-port [0-9]|std-port Phase|findings #[0-9]|findings [0-9]+-[0-9]|Phase-[0-9]' -- ':!doc/plans' ':!doc/results' ':!vendor'` → **0 hits**.
- **Secondary gate (space-form Phase)** — run with **`-P`** (the `-E` form is
  vacuous, Decision 1): `git grep -nP '\bPhase [0-9]' …` → the single expected
  algorithm-step line `cas/src/store.rs:3010`, nothing else.
- **Bare-shorthand + `plan`-prose re-triage** — clean (only legitimate
  version/section/size/`sleep` numerals remain).
- **Formatting** — `cargo fmt --check` clean (root); `scripts/verusfmt.sh --check`
  clean (macro interiors, incl. the `kcore/src/cspace.rs` comment edits);
  `cargo fmt` re-run per touched separate workspace (`user/{hello,init,storaged,shell,stdsmoke,stdfs,stdio}`, `eunomia-sys/fuzz`) — no churn.
- **kcore re-verify** — `cargo clean -p kcore && cargo verus verify -p kcore` →
  `verification results:: 408 verified, 0 errors` (a real run — results line
  present). Comment-only edits inside `verus!{}` leave the proof surface intact;
  no `scripts/verus-baseline.sh` needed (not a proof change).
- **QEMU smoke (markers untouched)** — `scripts/std-smoke-test.sh` and
  `scripts/fs-smoke-test.sh` green; the sweep touched only `#` comment lines,
  never a marker string (`STD2 PASS`, `[stdsmoke]`, `[stdfs] readdir found
  smoke`, …) or the code that emits one.

## Surface left trusted

Nothing new — this task changes only comments and CI labels; no runtime path,
wire/ABI, or verified obligation moves. The smoke-harness markers and their
emitting code are untouched.

## Follow-ups

- **Out of scope, noted:** `doc/guidelines/fuzzing.md:32,83` reference
  `../results/1_fuzzing-findings.md` — a guideline citing a `doc/results` doc,
  a real `CLAUDE.md` discipline violation, but pre-dating the std port and not
  caught by C4's gates (C3 was scoped to the trusted-base ledger). Worth a
  separate cleanup.
- The `-E` vs `-P` `\b` gotcha (Decision 1): if the Verus/CI gate hardening (C6)
  or any future audit scripts a `\bPhase` check, it must use `-P` or an
  ERE-safe `(^|[^-])Phase [0-9]`, else it silently passes.
