# Findings 25 — spec `stderr` amendment + rev2§5.1 citation audit (C2, review finding 10)

Task **C2** of `doc/plans/3_plan-std-correction.md`, acting on the spec half of
finding 10 of the independent review (`doc/results/22_std-port-review.md`). Both
sub-tasks land here as one reviewable change: **C2.1** puts `stderr` in the spec,
**C2.2** audits the stderr/entropy `rev2§5.1` citations against the amended text.

**Headline:** the std port already ships a working `stderr` — `NAME_STDERR`
(id 12) is emitted by init, resolved by `console.rs` as
`NAME_STDERR → stdout channel → debug-log`, and kept distinct from stdout — but
`doc/spec/spec_rev2.md` §5.1's "Standard names" sentence never listed it, the
edit the previous plan's spec-change row #5 decided but never applied. Adding
`stderr` to §5.1 both closes that gap and **retroactively backs** a family of
`rev2§5.1` stderr citations that were effectively dangling (they cited spec text
that wasn't there yet). The audit finds exactly one citation *not* backed by the
amendment — `urt/src/random.rs`'s "Two invariants … (rev2§5.1)", whose first
invariant §5.1 does not state — and re-anchors it. Documentation-only: a spec
sentence plus one doc comment; no runtime behavior, no verified obligation, and
no wire/ABI change.

## The problem (from finding 10, spec half)

A citation is a claim, and a `rev2§5.1` cite is a claim that §5.1 backs it. §5.1
listed `stdin`/`stdout` but never `stderr`, so every comment citing §5.1 for
stderr behavior — the fallback chain, "a stream distinct from `stdout`", "the
same channel under both names" for a terminal — pointed at absent text. The
behavior is real and shipped; only the spec anchor was missing.

## Decision 1 — amend §5.1 in place (C2.1)

Insert a `stderr` clause into the "Standard names:" sentence
(`doc/spec/spec_rev2.md:363`), immediately after the `stdin`/`stdout`
parenthetical, in the section's own style:

> `stderr` (kept distinct from `stdout` so diagnostics never enter a pipeline's
> data; a terminal grants the same console channel under both names; when
> ungranted, a process's `stderr` falls back to its `stdout` channel, else the
> debug log)

In-place per the spec's convention (no changelog; §8.4 is only for superseded
approaches, which this is not). The clause was checked against the code before
adoption — each half maps to a real mechanism: the distinct-name/pipeline
rationale to `NAME_STDERR = 12` (`loader/src/startup.rs:115`) vs `NAME_STDOUT`,
the both-names terminal grant to init emitting all three names at
`SHELL_CONSOLE_SLOT` (`user/init/src/main.rs:284-295`), and the fallback to
`console.rs::resolve` (`stderr_slot(s).or(stdout).unwrap_or(SLOT_NONE)`, then the
`write_chan` debug-log fallback). The amended names list matches every grant name
init actually emits to a child (`root`, `stdin`, `stdout`, `stderr`, `storage`,
`time`, `random_seed`); `tmp`/`cwd` remain spec names that need not be granted
(init's own test asserts `NAME_TMP` unpopulated, `main.rs:762`).

## Decision 2 — re-anchor the urt citation, don't extend the spec (C2.2)

`urt/src/random.rs`'s module header read `Two invariants hold regardless of
source (rev2§5.1):` over (1) "`fill_bytes` never hands back the raw seed — every
output is an advanced xoshiro state word" and (2) a **fresh sub-seed per child**.
§5.1's random-seed sentence backs (2) verbatim — "a parent draws a fresh seed for
every child so siblings never share a stream" — but says nothing about (1).

Per the plan's Global decision 2, (1) is an explicitly *sampled* property (fixed
points of the xoshiro256\*\* transition are not excluded ∀ states over the 2^256
space) and randomness quality is off the proof surface (§5.1 mechanizes only the
seed *decode*). Extending §5.1 to assert (1) as a guaranteed invariant would
therefore overstate the spec. So the citation is **re-anchored**, not the spec
extended: drop the blanket `(rev2§5.1)`, cite §5.1 precisely on (2) (quoting its
words), and leave (1) as a self-evident xoshiro structural fact with no spec
cite.

*Rejected:* extend §5.1's random-seed sentence to state "output never repeats the
raw seed" — it would assert a non-∀-guaranteed property the plan explicitly keeps
off the proof surface, contradicting the same sentence's "quality … is out of
scope". *Rejected:* leave the blanket cite — it mis-attributes (1) to §5.1, the
exact dangling-claim class this task exists to remove.

The `std-port 3.4` token on the same module's line 1 is **left untouched** — the
comment sweep (C4) owns plan-provenance de-referencing; this task only makes
`rev2§5.1` citations resolve.

## What shipped

- **`doc/spec/spec_rev2.md:363`** — the `stderr` clause added to the §5.1
  Standard-names sentence (C2.1). Purely additive; no other spec line touched.
- **`urt/src/random.rs:15-20`** — the "Two invariants" citation re-anchored
  (C2.2). A module-level `//!` doc comment; no `verus!{}` code, no logic.
- **No other file edited.** The audit confirmed the remaining stderr/entropy
  citations resolve to the amended §5.1 as-is (below); C2.2's mandate is
  satisfied by C2.1 backing them, with no comment change required.

## Citation audit (C2.2)

`git grep -nE 'rev2§5\.1'` over the stderr/entropy neighborhood. Each row's claim
now resolves to backing §5.1 text.

Retroactively backed by the C2.1 amendment (dangling before, resolve now):

| Cite | Claim | §5.1 backing |
|---|---|---|
| `eunomia-sys/src/console.rs:4` | "the rev2§5.1 capability-routed terminal" | the console standard names |
| `eunomia-sys/src/console.rs:47` | stderr fallback `NAME_STDERR → stdout channel → SLOT_NONE` | "falls back to its `stdout` channel, else the debug log" |
| `eunomia-sys/src/console.rs:134` | "`stdin`/`stdout`/`stderr` console-channel cspace slots" | all three names now listed |
| `eunomia-sys/src/console.rs:265` | "the rev2§5.1 terminal case: one console under both names" | "a terminal grants the same console channel under both names" |
| `eunomia-sys/src/grant.rs:45` | "a stream distinct from `stdout`; … falls back to the `stdout` channel, then to the kernel debug-log" | "kept distinct from `stdout`" + the fallback clause |
| `user/init/src/main.rs:276` | stderr grant: "all three name the same console-channel endpoint" | the both-names terminal grant |
| `user/init/src/main.rs:748` | "(rev2§5.1 'same channel under both names')" | same clause |
| `user/stdio/src/main.rs:39` | "a stream distinct from stdout (rev2§5.1)" | "kept distinct from `stdout`" |
| `loader/src/startup.rs:110` | "a stream distinct from `stdout` so diagnostics never enter a pipeline's data path" | the same, near-verbatim |

Changed:

| Cite | Action |
|---|---|
| `urt/src/random.rs:15,19` | re-anchored (Decision 2) — now cites §5.1's fresh-seed-per-child sentence exactly |

Already backed, unchanged (resolve independently of the amendment):

| Cite | §5.1 backing |
|---|---|
| `urt/src/random.rs:156` | "a … per-process seed (rev2§5.1)" — "the child seeds a process-local generator from it" |
| `eunomia-sys/src/grant.rs:1,25,31,38,52,58` | named-grant resolution for `stdin`/`stdout`/`storage`/`root` etc. — all pre-existing §5.1 names |
| the wider `rev2§5.1` surface (thread/report/spawn/env cites across `kcore`, `ipc`, `user/*`, `tla`) | outside the stderr/entropy neighborhood; §5.1 already backs them, untouched here |

**No new dangling reference introduced:** the spec edit adds no citation, and the
one changed cite (`random.rs:19`) resolves to §5.1's random-seed sentence.

Nuance: `console.rs:186` carries the same stderr fallback claim but cites
`std-port 5.1`, not `rev2§5.1` — so it is a C4 comment-sweep target, not part of
this citation set. Its content nonetheless matches the amended spec.

## Verification record

- **C2.1 gate** — `stderr` resolves in §5.1 (`grep -n stderr doc/spec/spec_rev2.md`
  shows the new §5.1 clause); the names list matches every grant name init emits
  to a child.
- **C2.2 gate** — the grep-driven checklist above: every `rev2§5.1` cite in the
  stderr/entropy neighborhood resolves to amended §5.1 text; no new dangling
  reference introduced.
- **fmt** — `cargo fmt --check`: clean (parsing `random.rs` also confirms the
  doc-comment edit is syntactically valid). The markdown and doc-comment edits
  are fmt-neutral.
- **No re-verify** — no `verus!{}` code is touched, so nothing the prover checks
  changes; `cargo verus verify -p urt` and `scripts/verusfmt.sh`/
  `scripts/verus-baseline.sh` are not implicated (urt's verified count is
  unaffected). **No QEMU** — the stderr runtime path is unchanged and already
  green in `scripts/std-smoke-test.sh`; this task edits only documentation.

## Surface left trusted

Nothing new. `stderr` behavior remains the plain-Rust `console.rs` resolver and
its host tests (`resolve_all_three_distinct_slots`,
`resolve_stderr_falls_back_to_stdout_channel`, `resolve_no_console_is_all_none`);
this task changes no code, only the spec anchor those comments already point at.
Randomness quality — including the never-returns-raw-seed property — stays off
the proof surface by design (rev2§5.1, plan Global decision 2); making its
citation honest is the whole point of Decision 2.

## Follow-ups

- The `std-port N.N` provenance tokens co-located with these citations
  (`grant.rs:45`, `console.rs:185`, `init/main.rs:748`, `startup.rs:110`,
  `random.rs:1`) are the comment sweep's (C4) job, deliberately left here.
- The DRBG sanction the ledger routes (`doc/guidelines/verus_trusted-base.md`
  entropy routing note) is C3.4's; this task's Decision 2 keeps the code comment
  consistent with the posture C3.4 will state explicitly.
