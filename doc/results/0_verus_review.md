# Independent review — Verus optimization & simplification effort

Scope: every change between `bfa5ee6` (the pre-effort baseline) and `d5043dc`
(HEAD of the merged effort). This is an *independent* audit — the plan
(`doc/plans/0_verus-optimization.md`) and the per-task result docs
(`doc/results/0_verus_{a1..c4}.md`) were read for claims but **none of their
justifications were taken at face value**; every load-bearing claim was
re-checked against the code and re-measured on this host. Temporary intermediate
report (per `CLAUDE.md`, not citable from code/specs/guidelines).

Host: Darwin arm64, Verus `0.2026.06.07.cd03505`, Rust 1.95.0 (matches the CI pin).

## Verdict

**The effort is sound and a genuine success.** It is overwhelmingly proof-only;
the few executable changes are behaviour-preserving extractions; it proves
exactly the same high-level properties (no contract weakened, no trusted-base
growth, no proof obligation silently dropped); and it makes verification
**materially faster** — the whole gate's SMT cpu drops **~1.77×** (kcore, the
dominant crate, **1.73×**; the single hottest obligation `cdt_unlink` **6.5×**).
The simplification claims hold, and the process was unusually honest — three
candidate changes that *measured* as regressions were reverted with the
evaluation kept (no source residue).

Two **minor documentation defects** were found, neither affecting soundness:
1. Two source comments narrate an rlimit before→after transition (violates the
   `CLAUDE.md` "describe what is, not what was" rule).
2. The trusted-base **ledger's `ipc` row is stale** — it still says `69 verified`
   but the crate now verifies **47** (the C1 result doc documents the change
   correctly; the canonical ledger was not updated alongside the kcore/cas/urt
   rows).

---

## What landed

13 task-commits plus tooling/plan/results docs. Wave A (de-risked quick wins),
Wave B (kcore decompositions), Wave C (clarity). Three were **measured
regressions and reverted** — and the revert commits (`606cfac` B5b, `9c91c67`
B6, `d02d960` C3a/C3b) touch **zero `.rs` files**, landing only the evaluation
doc. Net source change touches 6 gated crates:

| crate | files | tasks landed |
|---|---|---|
| freelist | `src/lib.rs` | A1, A1+, A1++ |
| cas | `src/prolly.rs`, `src/store.rs` | A2, A3, A4, C4b |
| urt | `src/slots.rs` | A5 |
| kcore | `thread.rs`, `cspace.rs`, `notification.rs`, `channel.rs`, `aspace.rs` | B1, B2, B3, B4, B5a, C2, C4a |
| ipc | `header.rs`, `session.rs`, `le_bytes.rs` (new), `lib.rs` | C1 |

---

## Q1 — Did the changes only touch proof code? If executable, was it justified?

**Almost entirely proof/ghost code.** The categories of change are: trigger
annotations (`#![trigger ...]`), `#[verifier::rlimit(N)]`/`spinoff_prover`
attributes, new `proof fn` lemmas, `proof {}` block contents, and ghost-argument
removals at proof-fn call sites — all erased from the executable.

**Two tasks touch executable code, both behaviour-preserving extractions:**

- **A2 (`cas/src/prolly.rs`)** adds two *new* crate-private exec fns,
  `encode_content` and `decode_content`. These are **verbatim moves** of the
  inline 3-arm content match out of `encode_raw`/`decode_raw`, which now call
  them; the `?` on `decode_content(...)` propagates `Err` exactly as the original
  `return Err(...)`. Same operations, same order, same bytes, same `Result`. The
  helpers carry tight `ensures` (`final(out)@ == old(out)@ + content_bytes(*c)`;
  `Ok((c,end)) ==> content_bytes(c) == buf@.subrange(p_ctag, end)`) that fully
  reconstitute what was proved inline. Justified by the §10 decomposition
  technique (shrinking the verification context); confirmed no public API change
  (helpers are `fn`, not `pub fn`).

- **A3 (`cas/src/store.rs`)** splits `e_payload_ok` into a 2-line tag dispatcher
  plus three exec twins `e_payload_{write,unlink,rename}_ok`; each arm body is a
  **verbatim move**, and Verus proves each twin `== s_payload_<arm>_ok` and the
  dispatcher `== s_payload_ok`, so runtime acceptance is identical.

Everything else verified as proof-only by inspection: the urt A5 call sites only
drop a ghost `free` argument (`set`'s `if free { w|b } else { w&!b }` exec branch
is byte-identical); the C4b prolly readers keep their exec `let v = …; v` body and
swap inline `assert … by (bit_vector)` for a `proof { lemma_…() }` call; the
freelist rlimit changes are attributes only. **No runtime behaviour changed
anywhere.** (Corroborated by the per-file review fan-out; `cargo test -p ipc`
host tests stay green, confirming identical wire bytes for the C1 codecs.)

---

## Q2 — Do the proofs still prove the same high-level properties?

**Yes — verified four independent ways:**

1. **No baseline obligation disappeared.** I set-diffed the verified-function
   list (warm, crate-own, vstd filtered out) baseline vs HEAD for every crate.
   The "functions present at baseline but missing at HEAD" set is **empty** for
   kcore, ipc, cas, urt, freelist. Coverage strictly *increased* by exactly the
   26 named new items: +13 kcore lemmas (`lemma_destroy_tcb_*`,
   `lemma_running_frame_trans`, `lemma_waiter_{dequeue,enqueue}_census`,
   `lemma_unlink_merge`, `lemma_children_walk_peel`, the five
   `channel::lemma_*`), +4 ipc le_bytes lemmas, +9 cas helpers/lemmas.

2. **Public contracts byte-identical.** I diffed the `requires`/`ensures`
   signature span of every hot function (`destroy_tcb`, `cdt_unlink`, `signal`,
   `remove_waiter`, `wait`, `recv`, `send`, `e_payload_ok`, `s_payload_ok`)
   between baseline and HEAD: **all identical**. The changes live in proof
   bodies and new internal lemmas, not in what is proven.

3. **No trusted-base growth.** A full diff scan for `assume`, `admit`,
   `external_body`, `assume_specification`, `#[verifier::external]`, `opaque`,
   `reveal` found **no new occurrences** (the only `broadcast use` additions are
   the standard `vstd::slice::group_slice_axioms` in the two new cas helpers).
   The trusted-base tally stays **14**, as the ledger claims.

4. **The obligation-count *drops* are restructuring, not lost coverage.** The
   "N verified" item count fell for three crates; each is fully explained by
   inline `by (bit_vector)` assert sub-items (which *are* counted) folding into
   lemma signatures:
   - **ipc 69 → 47** = 26 inline split/reassemble asserts collapse, +4 le_bytes
     lemmas (I independently confirmed the authoritative gate text line: `47
     verified, 0 errors`).
   - **urt 29 → 25** = 4 inline asserts fold into 2 lemma signatures.
   - **cas 80 → 75** = net of +9 new items and the folded reader asserts.
   - kcore 391 → **404** (+13), freelist 29 → 29, dma-pool 0 → 0.

   The new lemmas' `requires` are **all discharged at their call sites** (the
   per-file pass verified the census, merge, peel, and channel-frame lemmas each
   have every `requires` clause materialised immediately before the call — no
   silent assumption smuggled in via an undischargeable precondition).

A nuance worth recording: one strictly-stronger change. urt's `lemma_bit_other`
went from a gated boolean equivalence `(A!=0)==(B!=0)` to an unconditional mask
equality `A==B` (which *implies* the old form), and `lemma_set_bit` dropped its
`free ==>` guards for unconditional facts. These prove *more*, not less.

**All six gated crates verify `0 errors` at HEAD** (kcore 404, ipc 47, urt 25,
freelist 29, dma-pool 0, cas 75 — each re-run cold with the `verification
results::` line present).

---

## Q3 — How does final performance compare to the baseline?

Re-measured on this host with `scripts/verus-baseline.sh`, warm-vstd both sides
(baseline measured in a `git worktree` at `bfa5ee6`). SMT cpu is summed over
threads; **rlimit is deterministic** and is the primary evidence (per-fn wall ms
wobbles ±5–15 %).

### Per-crate SMT cpu (ms)

| crate | baseline | HEAD | speedup |
|---|---:|---:|---:|
| **kcore** | 104,834 | 60,749 | **1.73×** |
| **freelist** | 12,775 | 5,915 | **2.16×** |
| **cas** (`--no-default-features`) | 1,814 | 962 | **1.89×** |
| **ipc** | 309 | 151 | **2.05×** |
| **urt** | 153 | 124 | 1.23× |
| **total** | **119,885** | **67,901** | **~1.77×** |

(My warm baseline kcore 104,834 ms matches the plan's §1 figure of 105,193 ms,
confirming the measurement is comparable.) Verify-phase wall time falls in step
(kcore 32.8 s → 18.4 s).

### Hottest obligations (SMT ms / rlimit)

| obligation | baseline | HEAD | new helper cost | win |
|---|---|---|---|---|
| `cspace::cdt_unlink` (B3) | 29,841 / 63.7M | 4,568 / **7.16M** | `lemma_unlink_merge` 2,286 / 5.57M | **6.5× ms, 8.9× rlimit** |
| `thread::destroy_tcb` (B1) | 21,447 / 46.1M | 10,727 / 24.6M | 3 frame lemmas ~15–20 ms ea | 2.0× ms, 1.87× rlimit |
| `notification::remove_waiter` (B2) | 14,035 / 39.3M | 9,528 / 19.0M | census lemmas ~25 ms | 1.47× ms, 2.07× rlimit |
| `notification::signal` (B2) | 13,422 / 20.9M | 11,591 / 20.9M | — | 1.16× ms (rlimit flat) |
| `channel::recv` (B4) | 1,413 / 3.62M | 494 / 1.22M | frame lemmas ~24 ms | **2.9× ms, 3.0× rlimit** |
| `channel::send` (B4) | 545 / 1.62M | 335 / 1.04M | — | 1.63× ms |
| `cspace::delete` (indirect) | 2,124 / 6.67M | 1,221 / 4.48M | — | 1.74× ms |
| `cspace::slot_move` | 4,283 / 6.30M | 4,039 / 6.27M | — | ~flat (B6 reverted) |

### freelist (the A1 trigger fix — the single biggest lever)

| fn | baseline ms/rlimit | HEAD ms/rlimit |
|---|---|---|
| `free` | 4,442 / **169.3M** | 1,242 / **24.2M** (7× rlimit) |
| `free_insert` | 2,719 / 194.4M | 1,806 / 110.9M |
| `free_replace` | 2,530 / 85.0M | 1,573 / 42.8M |
| `free_both` | 1,552 / 94.6M | 640 / 31.4M |
| `alloc` | 886 / 17.1M | 309 / 2.94M (5.8×) |

### cas

`decode_raw` 745 / 22.0M → 172 / 3.22M (A2); `encode_raw` 246 / 11.7M → 66 / 2.09M
(A2); `e_payload_ok` 78 / 962K split away (A3); the `read_u*_le` inline bit_vector
cost folds into the named width lemmas (C4b).

**Conclusion:** every gated crate is faster, the wins land exactly where the plan
targeted, and the deterministic rlimit drops (cdt_unlink 8.9×, free 7×) confirm a
genuine reduction in proof size, not stopwatch noise. The new lemmas pay for
themselves: the heaviest (`lemma_unlink_merge`, 2,286 ms) makes the
`cdt_unlink`+lemma path ~12.7M rlimit vs the baseline 63.7M — still 5× cheaper.

The plan's per-task headline magnitudes are *directionally* accurate but
occasionally optimistic against a clean warm measurement (e.g. A1 claims a 2.85×
crate win; I measure 2.16× warm — still large, and the per-fn rlimit drops are if
anything *bigger* than claimed).

---

## Q4 — Do the simplification / readability claims hold?

**Yes, with one honest framing caveat.** The decomposition tasks (B1–B5a, C2) are
**net line *additions*** — the explicit `requires`/`ensures` of an extracted
lemma cost more lines than the inline block they replace (e.g. thread.rs +205,
cspace.rs +193 net). This is the standard, correctly-disclosed Verus
decomposition tradeoff, not bloat: at every *hot call site* the nesting and
local proof clutter drop sharply (a ~30–55-line inline `assert … by { … }`
case-split becomes "establish the local shape facts, then one named lemma call"),
and the heavy reasoning moves into a named, contracted lemma placed beside its
existing family (`lemma_unlink_*`, `lemma_census_*`, `rec_ok`/`laid_out`). Naming
is consistent and the recv/send lemma pairs read as mirror images.

The pure-clarity tasks are real wins on the line count too: A5 (−6, recipe form),
C1 (header/session both shrink and de-nest; 26 inline asserts → 12 one-line
citations of 4 named lemmas), C4a (uniform clause labels across the four
`pt_wf_leveled` blocks), C4b (read_u64_le's eight repeated bit_vector asserts →
one lemma call). A1+'s rlimit retune (replacing seven over-provisioned budgets
with four defaults + three honest near-floor caps) is a genuine "stop signalling
*this proof is hard* falsely" improvement.

The **process** earns particular credit on the honesty axis: B5b, B6, and C3a
were implemented, *measured to regress* (C3a: +12.4 % kcore SMT, driven by the
predicate auto-unfolding inside `destroy_tcb`/`delete` — a real finding that
refutes the plan's own "zero-speed" projection), and reverted, with the negative
result written up. The result docs' qualitative claims match the diffs (the
per-file cross-check found no overstated structural claim).

---

## Q5 — Do the comments uphold the `CLAUDE.md` comment discipline?

**Mostly yes, with two clear violations and a few borderline cases.** The new
lemma headers and module docs are clean: they describe current structure in
present tense and cite only `rev2§…` (doc/spec) and `doc/guidelines/verus.md §…`
(allowed). No comment cites `doc/plans` or `doc/results`; no `A1`/`B2`/`C3`/
`wave`/`[measured]`/`REVERT` task markers leak into source (those `c2`/`c3`
tokens that grep flags are loop counters, not phase labels).

**Clear violations** — both are rlimit-rationale comments that narrate the
before→after transition (the rule is "describe what *is*, not what was"):

- `kcore/src/thread.rs:778` — *"…shrank that cap **from 30 to 24** — the per-phase
  derivations **no longer share** this one query."* References a former value
  (30) that exists nowhere in the code. (The B1 result doc's self-assessment even
  says this comment was "updated to describe the post-decomposition budget" — but
  it narrates the transition instead.)
- `kcore/src/notification.rs:717-718` — *"Lifting the per-object census map …
  **halved** this obligation's rlimit, so the budget is **reduced from its former
  40M-cap value**."* Same defect: "halved", "reduced", "former 40M-cap".

Suggested fix (both): state the present rationale without the delta, e.g.
*"rlimit(24): the per-phase frame re-establishment lives in the keyed
`lemma_destroy_tcb_*_frame` proof fns, so this isolated body needs only a modest
raised cap."*

**Borderline** (defensible, worth a cleanup pass): `channel.rs:1281` *"Extracted
from `recv`'s inline post-loop block…"* (faint history framing — but it documents
the present decomposition relationship + cites §10); `cspace.rs:6964` *"the
extraction measurably regressed it"* (experiment narration); and most concretely,
`notification.rs:382-384`, whose path-not-taken rationale embeds **measurement
figures in a code comment** — *"the extraction measurably regressed `signal`
(**rlimit +63 %**) while it cut `remove_waiter` by half"*. Path-not-taken
rationale is the exceptional case the discipline allows, but the embedded
effort-measurement numbers are the kind of transient data that belongs in the
result doc, not the source. The "split out for a small verification context
(verus.md §10)" doc-comments on the cas helpers are **acceptable** — they describe
current structure, the form the discipline explicitly permits.

(One pre-existing `session.rs` "…as before" comment predates the baseline and is
out of scope.)

---

## Documentation defect — stale trusted-base ledger row

`doc/guidelines/verus_trusted-base.md` is the canonical "kept honest" ledger.
The effort correctly updated its **kcore** (389→404, with the new lemmas
enumerated), **cas** (80→75), and **urt** (29→25) rows. It **did not update the
`ipc` row**, which still reads `69 verified, 0 errors` — but the crate now
verifies **47** (confirmed: `cargo verus verify -p ipc` → `47 verified, 0
errors`). The drop is benign (it is the C1 inline-assert folding, and function
coverage rose by 4), and the **C1 result doc records 69→47 accurately** — indeed
the C1 *commit message itself* (`80f0d62`) states "Cold whole-crate gate: 69 → 47
verified", so the author was aware. The ledger row was simply not updated
alongside the kcore/cas/urt rows — an oversight, not a misunderstanding.

This has a concrete downstream effect: the new `CLAUDE.md`/`README.md` prose
added by this same effort directs readers to the trusted-base ledger for the
"expected per-crate counts" — so a reader following that guidance to check `ipc`
would hit a `69`-vs-`47` mismatch. Recommend updating the ipc row to `47` with
the same one-line "26 inline `by (bit_vector)` sub-obligations folded into the
four named `le_bytes` width lemmas" explanation used for the urt/cas rows.

---

## Summary scorecard

| question | finding |
|---|---|
| Q1 proof-only? | Yes except A2/A3, which are behaviour-preserving exec extractions (verbatim moves, byte-identical runtime, no API change) — justified. |
| Q2 same properties? | Yes. Zero obligations dropped; all public contracts byte-identical; no trusted-base growth (tally 14); count drops are inline-assert folding; new lemmas' requires all discharged. |
| Q3 performance | Substantially better: gate SMT ~1.77× (kcore 1.73×, freelist 2.16×, cas 1.89×, ipc 2.05×); cdt_unlink 6.5× (rlimit 8.9×), recv 2.9×, free rlimit 7×. |
| Q4 simplification/readability | Holds. Decompositions are net +lines but lower nesting/clutter at hot sites with consistent naming; clarity tasks reduce lines; three measured regressions honestly reverted. |
| Q5 comment discipline | Two clear violations (rlimit comments narrating 30→24 / "former 40M-cap"); a few borderline; otherwise clean. |
| Extra | Trusted-base ledger `ipc` row stale (says 69, actual 47). |

**Recommended follow-ups (all cosmetic, none blocking):** rephrase the two rlimit
comments to present-tense; update the ledger `ipc` row to 47; optionally clean
the borderline history-framed comments.
