# Kani-rewrite conformance review — part 2 (post-correction)

A second, independent audit of the Kani verification effort, taken *after* the
five recommendations of the first review (`9_kani-review.md`) were implemented.
Read against the plan (`doc/plans/0_kani-rewrite.md`), the spec
(`doc/spec/2_spec_rev2.md` §6), the TLA+ models (`tla/cap_revocation`,
`tla/commit_protocol`), the findings parts 8–11
(`10_…` … `13_kani-findings-11.md`), and the harness source. Where the first
review asked "was the plan implemented?", this one asks "were the *corrections*
sound, and what is the true residual?"

## Method

What I verified directly on the current tree (`kani-tidyup-rec5`), not from the
findings prose:

- **Inventoried all 58 `#[kani::proof]` harnesses** (kcore 48, urt 4, ipc 2,
  cas 2, dma-pool 2) and re-mapped them to the §4.1–§4.7 catalog. Every plan
  row has at least one present harness; §4.1 grew the DN-4 teardown set.
- **Read the load-bearing code, not just the docs:** `cspace.rs`
  `obj_unref`/`delete`/`revoke`/`destroy_cspace`, the wf predicates
  (`cdt_wf`, `chan_wf`, `refcount_sound`), the DN-4 stubs (`proofs/stubs.rs`),
  the two transition harnesses, and the dma-pool rec-#4 harness.
- **Ran the host tier myself:** `cargo test -p kcore` → 11/11 pass, including
  the three anti-vacuity guards (`broken_sibling_link_fails_wf`,
  `corrupt_refcount_fails_soundness`, `out_of_window_ring_cap_fails_chan_wf`)
  and the positive `built_cdt_chain_is_wf` / `empty_world_is_wf_and_sound`. The
  predicates reject corrupted shapes — they are not vacuous. `cargo-kani 0.67.0`
  is installed and pinned; kcore is a host workspace member, so
  `cargo test --workspace --exclude kernel` does run these on every PR.
- **Read the CI `kani` job** including the rec-#3 cover post-check and the
  rec-#5 `timeout-minutes` guard, and the rec-#5 findings-filename rename.
- I did **not** re-run the full ~23-min solver suite (it runs per-PR in CI; the
  first review already spot-confirmed representative harnesses
  `VERIFICATION: SUCCESSFUL`). This audit is about *what the harnesses prove*,
  which is a source question, not a re-run question.

## Verdict

**The corrections were implemented competently and — most creditably — honestly,
including documenting where a recommendation could not be met and why.** Recs
#1 (DN-4), #3 (`cover!`), #4 (dma), and #5 (housekeeping) are done well. Rec #2
(broaden the transition harness) is **partially** done, and the team correctly
discovered that the recommendation itself rested on a false premise (closing
DN-4 would *not* unlock a wider transition harness — that wall is CBMC memory,
finding DN-12) and recorded that rather than forcing a hollow harness.

The plan, after corrections, is faithfully implemented to the limit of what
CBMC can do at this scale. The honest residual is unchanged in *shape* from the
first review but is now better bounded: **the destruction/teardown machinery and
the multi-op composition of the cap algebra remain the least Kani-covered, while
the additive machinery is the most** — the opposite of where the highest-value
proof would sit. The corrections narrowed this; they did not, and at this scale
could not, remove it.

## Status of the five corrections

| Rec | What was asked | Outcome | Assessment |
|---|---|---|---|
| #1 | Close DN-4 (recursive teardown as real Kani proofs) | **Done, by decomposition.** `check_delete_frame`/`check_delete_cspace` (stub the recursion, prove the dispatch) ∘ `check_destroy_cspace`/`check_destroy_channel`/`check_thread_teardown` (drive the body directly). | Sound and clever; covers **one level** of container nesting with leaf residents. Deeper nesting stays TSpec+QEMU. See critique 3. |
| #2 | Broaden transition harness: add delete/revoke, raise K to 4–6 | **Partial.** `delete` added *inductively over all wf shapes* (stronger than the planned fixed-K reachability); additive K raised 2→3; **revoke not added** (OOM); K not raised to 4–6 (OOM, DN-12). | The most honest finding in the set: the rec's premise was wrong, and the docs say so. The realized coverage is the soundest that fits. See critique 1. |
| #3 | `kani::cover!` vacuity checkpoints | **Done, +CI post-check.** 41 covers across 15 harnesses; all SATISFIED. | Correct, and uncovered a real gotcha (DN-13: covers are *informational* in Kani 0.67), handled with an `awk` post-check in CI. Genuinely strengthens the suite. See critique 4 for one robustness nit. |
| #4 | Strengthen or retire `check_dma_alloc_disjoint` | **Strengthened where tractable, honest where not.** Part 1 now "for all first-allocation sizes" (boundary + in-pool + bijection + totality, ~0.5 s); Part 2 keeps the concrete two-buffer round-up, *labelled* as representative. | Exactly the "strengthen **or** say plainly it's representative" the rec offered, and did both. The symbolic-offset round-up genuinely OOMs (DN-10); leaving it concrete is correct. |
| #5 | Cosmetic: filename drift + CI budget | **Done.** `6_kani-findings_6.md` → `6_kani-findings-5.md` (+ cross-refs); convention written down; `timeout-minutes: 45` added; matrix-split escape valve documented. | Fine. The budget audit (~23 min, ~7 min headroom; one harness ~5.25 min over the per-harness target) is candid. |

## Independent technical critique

Ranked by how much it qualifies the effort's headline claim ("re-check the TLA
invariants on the real implementation").

### 1. The Kani↔TLC relationship is more nuanced than "the same state space on real code"

`bounds.rs` says the harnesses "re-check the same state space TLC found sound."
That is true of the *object counts* (CapIds=4, Procs=2, QueueDepth=2, etc.) but
not of the *exploration*. The two tools cover different things, and conflating
them oversells the suite in one direction and undersells it in another:

- **TLC** does a full breadth-first reachability over the *entire* action
  alphabet to fixpoint — every interleaving of Copy/Send/Receive/Bind/
  ThreadExit/ThreadFault/Revoke/Retype, but only over *reachable* states.
- **The Kani contract harnesses** (`check_slot_move`, `check_delete_step`,
  `check_derive_monotone`, …) prove a *single op preserves wf + its
  postcondition over an arbitrary asserted-wf shape*. This is an **inductive
  step**, and it is in one respect **stronger than TLC**: it quantifies over
  *all* wf states the bound admits, including states TLC never reaches. But it
  does **not** chain — Kani does not compose the steps for you.
- **The Kani transition harness** (`check_cdt_transition_system`) is the only
  true TLC analog (real ops from `Init`, asserting invariants after each), and
  it is the **weakest piece**: K=3, alphabet `{derive, slot_move}` only.

So the accurate statement is: *Kani proves the per-op inductive steps that
correspond to the TLA actions, over a superset of states, but does not reproduce
TLC's multi-op reachability composition.* That is a complementary result, not a
redundant one — which is good — but the suite does **not** "re-run the TLC
result on real code" in the literal sense the plan's prose implies. This is the
single most important framing correction for any future reader, and it is worth
fixing the `bounds.rs` comment to say so.

### 2. `revoke` — the operation the whole model is named after — is Kani-checked on exactly one concrete tree

`CapRevocation` exists to prove revoke is complete (LiveParent + DeadNowhere).
In the Kani suite, revoke is exercised only by `check_revoke`: a single
hand-built 5-cap tree (root + four parked descendants across cspace/queue/
TCB-bind homes + one grandchild), ~193 s. There is **no** inductive
`check_revoke_step` over a nondet shape — it OOMs (DN-12) — and revoke is
excluded from the transition harness. So the headline operation's correctness
over *arbitrary* trees rests on: TLC (abstract), **one** concrete Kani shape,
and QEMU (`m1-test.sh`). The single concrete shape is well-chosen (it hits all
three CDT-visible homes the §2.2 "sees through queues/TCB slots" guarantee
cares about), but for the property that motivates the entire model, one shape is
thin. This is the sharpest instance of the coverage skew the first review named.

### 3. The DN-4 closure is sound but is a *manually composed* proof with a one-line un-checked seam

The decomposition is legitimate: `check_delete_cspace` proves
`delete(cspace cap)` empties the slot, detaches the CDT, and drops the refcount
to zero, with `destroy_cspace` stubbed; `check_destroy_cspace` proves the body
(residents emptied, their objects unref'd) by direct call. Compose and you have
`delete`-of-a-cspace-cap for leaf residents.

Two honest caveats a reader should carry:

- **The stub is a silent no-op, so the routing is verified by source
  inspection, not by Kani.** `check_delete_cspace` asserts only
  `(*cs1).hdr.refs == 0`; it never witnesses that `obj_unref` actually took the
  `CapKind::CSpace => destroy_cspace` arm. The arm is forced by the concrete
  cap kind (and is one source line), so this is a sliver — but it is closable
  for free: the stubs receive `&mut env`, so they could record a `GhostEvent`
  exactly as `check_delete_frame` already does for `AspaceUnmap`/`AspaceDestroy`,
  turning "routing reaches destroy_cspace" into a Kani assertion instead of a
  comment. Recommended below.
- **Only one level of recursion is covered.** Both halves use *leaf*
  (notification) residents. A container whose resident is itself a live
  container (`delete → destroy_* → delete → destroy_*`) — the actual
  seL4-zombie-cap concern, the case with the hardest lifetime invariant — is
  covered by **neither** half and stays TSpec+QEMU. The findings say this
  plainly; it is the true residual of rec #1.

### 4. Minor robustness nits in the corrections

- The rec-#3 CI cover post-check (`grep … | awk '{ if ($2 != $4) … }'`) is
  correct for the present output, but it **fails closed in a misleading way**:
  with `set -o pipefail` in effect, if `grep` ever matches *zero*
  `cover properties satisfied` lines (a Kani output-format change, or a log that
  didn't write), the pipeline returns non-zero and the job reports
  "a cover was UNREACHABLE" — a confusing message for an absent-line condition.
  Failing closed is the right direction; the message should distinguish
  "no cover lines found" from "a cover went unreachable."
- `check_cdt_transition_system` at K=3 is ~315 s ≈ 5.25 min, over the
  per-harness ≤5-min target (documented, one-line revert lever). With the
  aggregate at ~23 min and ~7 min headroom, the suite has little room to grow
  before the matrix-split escape valve (already documented) is forced.

## Answers to the review questions

**Was the plan properly implemented after the corrections?** Yes, to the limit
of the tool. All seven §4.x sections are present and verify; the five
recommendations are addressed; the two earlier defects (UO-1 carve-overflow
DoS, AS-1 executable-MMIO encoding) remain fixed with their harnesses as
regression guards. The one recommendation not fully met (#2) is not met because
it was not *achievable* at this scale, and that is documented rather than
papered over.

**Were the changes justified?** Yes. Every deviation traces to a concrete CBMC
property (symbolic-discriminant match non-folding, symbolic-offset mask
bit-blasting, large symbolic free-lists, `Vec`/allocator modeling) or to a plan
tier boundary (concurrency→Loom, unbounded termination→TLA). The dma decision
(strengthen Part 1, label Part 2) and the DN-4 decomposition are both the
correct call. The bug-fixes were bundled with their harnesses (good practice).

**Is there anything left to do in that direction?** Yes — and it is the same
residual, now precisely located:

1. Multi-level recursive container teardown is not Kani-proven (DN-4 residual).
2. `revoke` over arbitrary trees is not Kani-proven (one concrete shape; DN-12).
3. The multi-op transition composition is K=3 / `{derive, move}` only; the full
   TLA alphabet at K=4–6 is infeasible under CBMC as currently shaped (DN-12).
4. Three §4.7 properties (urt `utc_ns_at` *monotonicity*, the time-page seqlock,
   `cas::tlv` canonical-form) are by-design on other tiers (proptest/Loom/fuzz);
   correct, but they are not Kani results despite the §4.7 row listing them.

Items 1–3 are not closable by writing more harnesses of the current genre —
they need a different instrument (see below).

**Should more time-consuming tests be added to supplement what fits in CI?**
**Yes — this is the highest-leverage remaining work, and it does not require
fighting CBMC.** Three concrete supplements, in value order:

- **Promote the DN-12 manual replay into a committed exhaustive model test (a
  "mini-TLC in Rust").** Finding DN-12 records that the team *already*
  exhaustively replayed all length-2 op sequences in plain Rust to confirm the
  CBMC "SATISFIABLE" was a modeling artifact. That replay is exactly the
  multi-op composition coverage CBMC OOMs on — and it is cheap. Commit it as a
  host test that runs the full alphabet (incl. delete/revoke) to depth 3–4 over
  the `BarePool`/`World`, asserting `cdt_wf` + `refcount_sound` + `RevokedDead`
  after every step. It runs in seconds, fills the transition-harness gap with a
  different tool, scales by a constant, and would have caught anything the K=3
  Kani harness misses. This is the single best return on effort in the whole
  list.
- **A separate "deep Kani" CI job at raised bounds, on a slow cadence
  (nightly/weekly), off the per-PR path.** The per-PR suite is pinned at the
  TLC floor (CapIds=4, K=3, depth 2). A scope-sensitive bug can hide below the
  floor. Run the harnesses that *do* scale (the pure/structural ones — carve,
  pte_encode, va_bounds, range_mapped, the contract harnesses) at
  CS_SLOTS=6 / CHAN_DEPTH=3 / K=4 with a multi-hour budget. Document explicitly
  which harnesses are excluded (the ones that already OOM at the floor) so the
  deep job is not mistaken for "everything, bigger."
- **A `-Z function-contracts` / loop-contract spike on `revoke` and
  `obj_unref`.** The findings already name this as the route to the
  unbounded/recursive proofs the bounded harnesses can't reach. It is research,
  not routine (unstable surface, the reason the plan deferred it), but it is the
  only path that would turn items 1–2 above into real proofs rather than
  bigger-but-still-bounded ones. Worth a time-boxed experiment, kept off the
  pinned CI path.

**Are the tested properties meaningful for the project?** Yes — the properties
are the right ones. They are the security- and safety-critical invariants the
architecture stands on: monotone derivation (`check_derive_monotone` — the
no-amplification security property), move-semantics single-ownership
(`check_slot_move`/`check_send_move`), refcount soundness across every
reference-holding edge (the census, with armed-timers and frame-mappings
included — exactly the drift §7 worried about), fire-safety of teardown
bindings (`check_bind_fire_safe`/`check_teardown_fire_safe`), report
monotonicity, and the §2.5 chokepoints (carve totality, PTE encoding, syscall
decode totality). The two real defects caught (a user-triggerable DoS and an
executable-MMIO hole) prove the properties bite.

The qualification is about *proof-strength allocation*, not property choice: the
strongest, most-exhaustive proofs sit on the *additive/constructive* operations
(derive, retype, map, decode), and the *thinnest* coverage sits on the
*destructive* operations (revoke on one shape, deep teardown on none) — which
are the operations with the hardest lifetime invariants and the highest
blast radius if wrong. The properties are meaningful; the residual risk is
concentrated where exhaustive proof is hardest to obtain, and a reader should
treat the teardown/revoke results as "checked at representative scope," not
"proven for all shapes."

## Recommendations (ranked)

The first review's five recommendations are all addressed; these are the *next*
set, reflecting the post-correction state.

1. **Commit the DN-12 exhaustive replay as a host test (the mini-TLC).** Highest
   value, lowest cost; fills the transition gap with the one tool that does not
   OOM. (Supplement, above.)
2. **Add a slow-cadence deep-Kani job at raised bounds** for the harnesses that
   scale, with the excluded set named explicitly. (Supplement, above.)
3. **Make the DN-4 routing a Kani assertion, not a comment:** have
   `no_destroy_cspace`/`no_destroy_channel`/`no_destroy_tcb` record a
   `GhostEvent`, and assert it in `check_delete_cspace` (and a channel/tcb
   analog), as `check_delete_frame` already does. Closes the last source-only
   seam in the DN-4 decomposition for ~10 lines.
4. **Correct the `bounds.rs` "same state space as TLC" comment** to the accurate
   "TLC-scale object bounds; per-op inductive coverage plus a K=3 additive
   transition check — *not* TLC's full-alphabet reachability." Stops the next
   reader over-reading the TLA↔Kani correspondence (critique 1).
5. **Tighten the CI cover post-check message** to distinguish "no cover lines
   found" from "cover unreachable" (critique 4).
6. **Time-box a `-Z function-contracts` spike on `revoke`/`obj_unref`** off the
   pinned path — the only route to items 1–3 of "what's left" becoming proofs.

## Adjacent observation (outside the Kani plan, inside the spec)

The spec §6 and `CLAUDE.md` name **Loom/Shuttle** as the concurrency tier and
call the `ipc` crate "the first serious Loom/Shuttle target." There is currently
**no Loom or Shuttle anywhere in the tree** (grep: zero hits). This is correctly
*out of scope for the Kani rewrite* (the kernel is single-core/non-preemptible,
so kcore needs no concurrency tier, and the plan §1 explicitly routes
concurrency away from Kani). But it is a real, unfilled tier of the project's
own §6 verification policy: the `ipc` crate is genuinely concurrent userspace,
and it is the one place where the bugs Kani is *constitutionally* unable to
find would live. Not this effort's job — but worth surfacing so it is not lost
between the Kani plan (which disclaims it) and the spec (which promises it).

## Bottom line

The post-correction Kani suite is a faithful, honest, and well-engineered
realization of the plan, bounded by CBMC's nature rather than by effort or
candor. Four of the five corrections are clean; the fifth (#2) is partial *and
correctly diagnosed as infeasible*, which is the better outcome than a hollow
harness. The properties are the right ones and two of them caught real,
security-relevant defects.

The standing residual is precise and unchanged in shape: **the destructive side
of the cap algebra — `revoke` over arbitrary trees and multi-level container
teardown — is the least Kani-covered, and the multi-op composition is K=3 /
two-op only.** Closing that does not need more harnesses of the same genre — it
needs the cheap exhaustive Rust replay (recommendation #1) for composition and a
`function-contracts` spike (recommendation #6) for the unbounded cases. Until
then, the teardown/revoke results should be read as "verified at representative
scope, backed by TLC and QEMU," not as exhaustive proof — which is exactly how
the findings docs already, commendably, frame them.

## Addendum — supplements landed (`scripts/deep-verify.sh`)

Recommendations #1 and #2 of this review's "supplements" list were implemented
immediately after it was written; they are off-CI, marked HEAVY, and run via
`scripts/deep-verify.sh` (documented in `CLAUDE.md`):

- **The mini-TLC exhaustive replay** (`kcore/src/proofs/exhaustive.rs`, an
  `#[ignore]`d host test). It brute-forces **every** sequence of CDT ops
  (`derive`/`slot_move`/`delete`/`revoke`) up to a depth (`EXHAUSTIVE_DEPTH`,
  default 5 ≈ 100M sequences, ~15 s release; depth 4 ≈ 2.6M in ~0.4 s measured),
  asserting `cdt_wf` (⊇ LiveParent + DeadNowhere-in-pool) and the refcount
  census after **every** step, with non-vacuity counters proving each op fired.
  This is the multi-op composition coverage CBMC OOMs on (DN-12), and the only
  check that exercises `revoke` over *all* reachable shapes rather than the one
  concrete tree of `check_revoke` — directly addressing critiques 1 and 2.
  It exercises real ops at volume (depth 4: derive≈765k, move≈765k, delete≈274k,
  revoke≈26k) with zero invariant violations.
- **The deep-Kani transition run at K=4** (the `KANI_DEEP` compile knob widens
  `bounds::K_STEPS`; CI stays at K=3). This is the safe slice of "raise K toward
  4–6": widening `K_STEPS` alone keeps the harness's `unwind(6)` valid, whereas
  widening the object-count bounds needs a manual unwind-literal bump (recorded
  in `bounds.rs`), so it is left as a deliberate manual step rather than an env
  toggle.

These do not change the bounded-Kani picture above; they add a *different tool*
(concrete exhaustive enumeration) over the exact operations CBMC cannot compose,
which is the soundest available answer to the residual this review identifies.

## Addendum 2 — follow-ups landed

After the addendum above, the four follow-ups it implied were completed:

1. **Cross-home coverage** (`exhaustive_cross_home_replay`, `proofs::exhaustive`).
   A second replay over a `World` parks derived caps in a **channel ring slot**
   and a **TCB binding slot** as well as cspace slots, asserting `cdt_wf` +
   `refcount_sound` + `chan_wf` after each op. This is the first check of the
   §2.2 "revocation sees through queues and TCB binding slots" guarantee over
   *all* reachable shapes — closing critique 2's "revoke is checked on one
   concrete cross-home tree." (depth 4 ≈ 13 M sequences, ~21 s; depth 3 ≈ 216 k.)
2. **Continuous CI exercise.** Both replays now run at depth 3 in the per-PR
   `host-tests` job (~0.25 s), so they are regression-guarded, not just manual.
3. **Feature-driven Kani deepening.** The env knob became a `kani_deep` cargo
   feature; `bounds.rs` reads `cfg!(feature)` and the two composition harnesses
   (`check_cdt_transition_system`, `check_delete_step`) carry `cfg_attr` unwind
   literals, so they now deepen on **object count too** (`POOL_SLOTS` 4→6) — not
   just K — when run under the feature. (The structural single-op harnesses keep
   fixed bounds, named explicitly in `deep-verify.sh`.)
4. **Scheduled job.** `.github/workflows/kani-deep.yml` runs `deep-verify.sh`
   (deep replays + widened-bound Kani) weekly and on `workflow_dispatch`,
   mirroring `fuzz.yml`.

With these, recommendation 1 is fully realized (composition + cross-home,
continuously exercised) and recommendation 2 is realized as both a scheduled job
and genuine object-count widening — leaving recommendations 3–6 (the DN-4
ghost-witness, the `bounds.rs` comment, the CI cover-message, and the
`function-contracts` spike) as the open items.
