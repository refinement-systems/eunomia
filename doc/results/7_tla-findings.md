# TLA+ / TLC optimization findings — Tier D (hygiene / cosmetic)

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines. (B1's outcome is in `0_tla-findings.md`, B2's in
`1_tla-findings.md`, B3's in `2_tla-findings.md`, B4's in `3_tla-findings.md`,
B5's in `4_tla-findings.md`, B6's in `5_tla-findings.md`, C1's in
`6_tla-findings.md`.)*

Tier D is the hygiene tier (D1–D4): no model semantics, no coverage change, "do
not regress perf". The headline finding is that the plan's Tier-D prose predates
Tiers A–C, and **two of the four items were already delivered by Tier A**, so the
implemented scope is narrower than the plan text suggests. Each item below records
what was actually needed.

All verification runs used the vendored `tools/tla/tla2tools.jar` (matches its
`.sha1`), Temurin 17, host Darwin arm64.

---

## D1 — stop TLC scratch from accumulating in `tla/`

**Status: adopted, but reduced to a one-flag fix.** The plan's D1 asks to delete
the stray scratch *and* point `-metadir` outside the source tree. The
`-metadir` half was **already done in Tier A**:
`tools/tla/tla-model-check.sh` routes `-metadir` to
`target/tla-states/<cfg>`.

**Finding (the gap `-metadir` does not cover):** the runner `cd`s into the spec
directory before invoking TLC, and on a property/invariant violation TLC writes a
*trace-exploration (TE) spec* — `<Spec>_TTrace_<ts>.tla` and a companion
`.bin` — into the **current working directory**, i.e. the spec dir under `tla/`.
`-metadir` redirects the fingerprint `states/`/checkpoints but **not** the TE
spec. So every run that trips a violation still littered `tla/`; the recurring
source is `scripts/tla-neg-controls.sh`, whose ten controls each trip a
counterexample on every invocation. The working tree held **142** such files
(71 `.tla`/`.bin` pairs) at the start.

**Fix:** add `-noGenerateSpecTE` to the shared runner's TLC invocation, alongside
the existing `-workers`/`-metadir`. This skips TE-spec generation entirely, so no
`*_TTrace_*` is ever written regardless of cwd. It is verdict- and
coverage-neutral: TLC still prints the full counterexample to stdout, which is the
only thing the negative-control runner reads (it greps the log for the violated
invariant/property name). The flag was chosen over `-teSpecOutDir <dir>` (which
would *relocate* the TE spec into the scratch metadir): the TE spec has no
consumer in this repo — the Toolbox that re-explores it is being retired in this
same change (D2) and the stdout trace already carries the counterexample — and
suppression is the single unambiguous guarantee that both the `.tla` and the
`.bin` stop appearing. Trade-off recorded per the plan's §0.3: the runner no
longer emits an on-demand TE spec; anyone who genuinely wants one can invoke TLC
directly without the flag.

**The deletion is not in the commit.** The 142 files are gitignored
(`tla/.gitignore` covers `states/`, `*_TTrace_*.{tla,bin}`, `*_nosym_tmp.cfg`),
so `git clean -fdX tla/` removed them with no effect on the tracked tree. The
committed deliverable is therefore *only* the runner flag that prevents
recurrence.

**Null/correction findings:**
* The plan expected ~181 files across 3 `states/` trees; **none were present** in
  this clone (already cleaned, or never created here — consistent with `-metadir`
  having routed `states/` to `target/` since Tier A). Only TE specs accumulate.
* "~245 stray files / ~64 `*_TTrace_*`" in the plan vs. the actual 142: the plan
  counted unique trace IDs, not the `.tla`+`.bin` pair each ID writes.

**Verification:** after the fix, `scripts/tla-neg-controls.sh` reports "all 10
negative controls failed as designed" and `find tla -name '*_TTrace_*'` is
**empty** — the run that used to deposit ten-plus pairs now deposits none. A
`tools/tla/tla-model-check.sh` run on `IpcReactor` and a
`scripts/tla-baseline.sh IpcReactor` run likewise left `tla/` clean.

---

## D2 — retire the deprecated TLA+ Toolbox tier-3 fallback

**Status: adopted.** `tools/tla/find-tla-tools.sh` resolved tools in three tiers:
(1) explicit `TLA_TOOLS`+`JAVA` override, (2) vendored `tla2tools.jar` + a system
JDK, (3) a legacy fallback probing `/Applications/TLA+ Toolbox.app`. The
auto-verified record (and local runs) land on tier 2; the Toolbox path is unused,
unmaintained, and its bundled JRE is x86_64/Rosetta. The ~40-line tier-3 block
was removed.

**Finding (not a pure deletion):** tier 3 also *held the not-found error
handling* — deleting it wholesale would let the script source "successfully" with
`TLA_TOOLS`/`JAVA` unset, turning a clear "install the tools" message into a
confusing downstream failure. So the block was **replaced** with a direct
not-found path after the tier-2 success/return: if `TLA_TOOLS` is unset it points
at `./fetch-tools.sh`; if `JAVA` is unset it suggests installing a JDK; then
`return 1`. The header doc-comment lost its resolution-order item 3 + the
DEPRECATED paragraph, and the tier-1 inline comment lost its dangling "without
the Toolbox" reference.

**Verification:** `tools/tla/tla-model-check.sh tla/ipc_reactor/IpcReactor.tla
IpcReactor.cfg` still resolves the vendored jar + Temurin 17 and reports the
expected 39 distinct with no error, confirming tier 2 is unaffected.

---

## D3 — constants / expected-distinct-state manifest

**Status: already complete (null result — no implementation).** This was the
plan's biggest surprise: D3 asks to *add* the TLC analogue of the Verus
trusted-base ledger, but Tier A already created `tools/tla/model-manifest.tsv` —
exactly that artifact. It records, per cfg, the canonical CONSTANTS (in the
header, so the lock-step cfgs cannot silently drift) and the expected
distinct/diameter, and `scripts/tla-baseline.sh` reads it for both its model list
and its coverage-regression assertion. The committed counts already reflect the
post-B/C state (`CapRevocation` 503070, `CapRevocation_Safety` 1240344,
`CapRevocation_Teardown` 132, `CommitProtocol` 3444, `IpcReactor` 39).

No change was made. Verified the manifest still matches the committed cfgs by
running `scripts/tla-baseline.sh IpcReactor`: observed 39 distinct / diameter 13,
asserted `ok` against the manifest.

---

## D4 — tighten the `CapRevocation.cfg` constants-rationale comment

**Status: adopted (comment hygiene).** The header comment narrated history (the
"atomic-revoke baseline ran 4 caps / 2 procs … ~799k states … Splitting revoke …
explodes (~13M reachable / >40M-state tableau)"), which reads as "what was"
rather than "what is" (CLAUDE.md comment discipline). It was rewritten to keep
only the load-bearing rationale — why these constants are the floor for the
heap-bound liveness arm: 4 caps as the minimum that build a multi-level CDT
subtree reaching all three residences (cspace / queue slot / TCB binding slot),
and Threads/QueueDepth held at 1 to keep the tableau within heap — and to point
at the invariant-only sibling `CapRevocation_Safety.cfg` for safety at the larger
constants (which is where "safety at full scale" now lives, rather than the
removed atomic model the old comment referenced).

The `CONSTANTS` block is byte-identical (`git diff` shows only the comment), so
there is no coverage change.

---

## Net

D2 + D4 are real, self-contained hygiene edits; D1 collapsed to a single
`-noGenerateSpecTE` flag (its `-metadir` half pre-existed) plus a gitignored local
cleanup; D3 needed nothing (Tier A delivered it). No model semantics, constants,
invariants, properties, symmetries, or expected counts changed — confirmed by the
unchanged `IpcReactor` count and the ten negative controls still tripping as
designed.
