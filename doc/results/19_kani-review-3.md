# Kani-rewrite conformance review — part 3 (post-review-2 closeout)

A third independent audit of the Kani verification effort, taken *after* the six
recommendations of the second review (`14_kani-review-2.md`) were implemented
(the first review's five were already done before review 2). Read against the
plan (`doc/plans/0_kani-rewrite.md`), the spec (`doc/spec/2_spec_rev2.md` §6), the
two prior reviews (`9_kani-review.md`, `14_kani-review-2.md`), the TLA+ models,
and the harness source on the current tree (branch `kani-contracts-spike`, which
carries rec #6; rec #6's PR is in CI, treated as landed per direction).

Where review 1 asked "was the plan implemented?" and review 2 asked "were the
corrections sound?", this one asks: **after two correction cycles, is the effort
*finished* — is the residual closed, correctly bounded, or merely relabelled? And
what, if anything, is the right next verification investment?**

## Method

Verified directly on the tree, not from the findings prose:

- **Re-inventoried the harnesses.** ~60 per-PR `#[kani::proof]` (kcore ~50: cdt
  10, channel 8, untyped 8, aspace 7, teardown 7, notification 3, thread 3,
  transition 2, sysabi 2; host 10: urt 4, ipc 2, cas 2, dma-pool 2) **plus 2
  off-CI, feature-gated contract harnesses** (`proofs/contracts.rs`). Review 1
  counted 56, review 2 counted 58; the delta is rec #3's two new routing
  witnesses (`check_delete_channel`/`_tcb`) and rec #6's two spike harnesses.
- **Traced the now-four-tier stack for the same machinery** (below) through
  `ci.yml`, `kani-deep.yml`, `scripts/deep-verify.sh`, and the gating wiring.
- **Read the load-bearing new/changed code:** `proofs/exhaustive.rs` (the rec #1
  mini-TLC, now the instrument that carries the teardown/revoke residual),
  `proofs/contracts.rs` + the `cfg_attr` contracts on `cspace::{unref_cspace,
  delete}` (rec #6), the rec #4 `bounds.rs` comment, and the rec #5 cover
  post-check in `ci.yml`.
- **Spot-verified the suite still verifies:** `check_delete_cspace` (1.6 s),
  `contract_unref_cspace_refcount` (0.30 s, SUCCESSFUL), and
  `contract_delete_leaf` (FAILED, by design — DN-14). Did not re-run the full
  ~23-min per-PR suite (CI runs it on every PR; reviews 1–2 spot-confirmed
  representatives). `cargo test -p kcore` → 11/11.
- Confirmed the §6 tiers grep: **Loom/Shuttle is still absent from the entire
  tree** (zero hits).

## Verdict

**The Kani rewrite is, at this point, correctly done and essentially complete —
and review 2's standing residual is genuinely closed, not papered over.** The
crucial development since review 2 is that the residual both prior reviews named —
*the destructive side of the cap algebra (revoke over arbitrary trees, multi-op
teardown composition) is the least Kani-covered* — has been resolved in the only
intellectually honest way available: **by reallocating that coverage to the right
instrument (exhaustive plain-Rust enumeration) and then *proving* (rec #6, DN-14)
that Kani/CBMC cannot do better at the pinned version.** That converts the
residual from "a gap to close with more Kani" into "a correctly-bounded scope,
with the part CBMC can't reach carried by an exhaustive enumerator that can."

The remaining qualifications are smaller than review 2's and are of three kinds:
(1) one real documentation drift the rec #6 finding created in the *plan*; (2) one
genuine coverage seam the reallocation opened (the composition replay is checked
for *invariants* but not for *UB*); and (3) the standing, correctly-out-of-scope
fact that the project's own §6 concurrency tier (Loom/Shuttle) remains entirely
unbuilt — now the conspicuous hole precisely because the Kani work is done.

## The verification stack is now coherently four-tiered

The single most reviewable improvement since review 2 is that the same machinery
(cspace/CDT/teardown) is now checked by four tiers with **distinct, non-redundant
jobs**, and the tiering is documented:

| Tier | What it proves | Cadence / gate |
|---|---|---|
| Per-PR Kani (`cargo kani -p kcore`, TLC bounds) | per-op **inductive** steps + **additive** K=3 transition, over a *superset* of states, **with CBMC memory-safety/UB/overflow** | every PR, **gating** |
| Per-PR exhaustive replay (`proofs::exhaustive`, depth 3) | the **multi-op composition** incl. delete/**revoke** over **all** reachable shapes and **all three CDT-visible homes** — structural invariants only | every PR, **gating** |
| Weekly deep-Kani (`kani_deep`, K=4 / `POOL_SLOTS`=6) + deep-replay (depth 5) | the same, **below the per-PR floor** | weekly + dispatch, non-PR |
| Off-CI contracts spike (`kani_contracts`) | the *frontier*: what `-Z function-contracts` can/can't reach (DN-14) | manual only |

This is the right shape. Review 1's critique 2 ("the transition harness is a
2-step shadow of the planned integration harness") and review 2's critiques 1–2
("Kani does not reproduce TLC's multi-op reachability; revoke is on one concrete
tree") are both now *answered* — not by forcing CBMC past its limits, but by the
replay tier, which does exactly the full-alphabet multi-op reachability over all
shapes that CBMC OOMs on, in seconds. The rec #4 `bounds.rs` correction makes the
division of labour explicit in the source, so a future reader will not re-confuse
the Kani tier's job with TLC's.

## Was the Kani rewrite done correctly?

**Yes.** The core thesis — *extract the object machinery into a host-buildable
`kcore` and re-check the TLA-derived invariants on the real code* — is realized,
and the structural guarantees that make it trustworthy hold on the current tree:

- The well-formedness predicates are substantive and proven non-vacuous (the
  three anti-vacuity unit tests still pass); `cdt_wf` is a real executable
  `TypeOK`, the census a real `RefCountSound`.
- There is no verified-vs-shipped drift: `kcore` *is* the kernel's object core,
  and the layering grep (no asm / no int→ptr) keeps it CBMC-admissible.
- The suite has earned its place twice over (UO-1 carve-overflow DoS, AS-1
  executable-MMIO encoding) — both remain fixed with their harnesses as guards.
- The security- and safety-critical properties are the ones checked: monotone
  derivation, move single-ownership, refcount soundness across every
  reference-holding edge, teardown fire-safety, report monotonicity, and the §2.5
  decode/PTE chokepoints.

What review 1 called the "shape of the residual risk" (additive machinery most
covered, destructive least) is no longer an accurate description of the *whole
effort*: it is true of the **Kani tier alone**, and is now the deliberate,
documented division of labour with the replay tier — not an unintended skew.

## Were the changes (the six rec-2 corrections) justified?

**Yes, each, and rec #6 is the one that retroactively justifies the others.**

- **#1 mini-TLC replay** — the highest-value change in the whole arc. It is the
  instrument that closes the revoke/composition residual, and it *exceeded* its
  brief: `exhaustive_cross_home_replay` checks revoke through channel-ring and
  TCB-bind homes over all shapes, answering review 1's critique 2 and review 2's
  critique 2 together. Cheap, exhaustive, gating at depth 3.
- **#2 deep-Kani job** — a sound "below-floor" cadence; the `kani_deep` feature
  widening object counts (not just K) is genuine added scope.
- **#3 DN-4 ghost witness** — closed the last *source-only* seam in the teardown
  decomposition for a few lines; the routing into `destroy_cspace`/`_channel`/
  `_tcb` is now a Kani assertion, not a comment.
- **#4 `bounds.rs` framing** — corrects the one over-read that would have let a
  future reader oversell the Kani↔TLC correspondence. Pure clarity.
- **#5 CI cover-message** — a fail-closed-message robustness fix; both modes
  still fail closed.
- **#6 function-contracts spike** — *justified precisely because it produced a
  negative result.* It established (DN-14) that function contracts verify kcore's
  value-level refcount discipline (`unref_cspace`, ~0.3 s) but **cannot** reach
  the cap-algebra teardown: a `modifies` clause on `delete` can't name the
  designated object (reached through the cap's embedded pointer) — `contract_
  delete_leaf` fails `Check that h->refs is assignable` even for an isolated leaf.
  This is what turns "we punted on the recursion" into "the recursion is provably
  out of CBMC's reach at 0.67.0, so the exhaustive replay is the *correct*
  instrument, not a stopgap." Without #6, the residual would still read as
  unfinished business; with it, it reads as a settled boundary.

No change was unjustified, and none introduced a behavioral regression (the
contract attributes are `cfg_attr`-gated and verified inert: `check_delete_cspace`
over the annotated `delete`/`obj_unref` still verifies without the feature).

## Any corrections needed?

No *code* corrections — the suite is green and no new defect surfaced. Three
items, in priority order, all small:

1. **Plan §5 now contradicts DN-14 — update it (documentation correction).**
   `0_kani-rewrite.md` §5 still says function/loop contracts are "not load-bearing
   in phase 1–4 … adopt selectively later for `revoke`'s walk if unwinding costs
   bite." Rec #6 *tested that hypothesis and refuted it* at cargo-kani 0.67.0 (the
   `modifies` wall, the loop-invariant-can't-be-cfg_attr-gated wall). The plan
   should record the DN-14 outcome so the next reader does not re-attempt the
   spike as if it were an open, promising option. (The §1 table's "unbounded
   proofs stay a `debug_assert` + TLA argument" row is consistent and needs no
   change.)

2. **The composition replay is checked for invariants but not for UB — wire it
   into the Miri sweep (a real coverage seam).** The reallocation to
   `proofs::exhaustive` has a subtle cost: the per-op Kani harnesses give the
   destructive ops CBMC's memory-safety/UB checking *singly*, but the multi-op
   *composition* is exercised only by the replay, which runs as plain `cargo test`
   — so a composition-only UB (e.g. `revoke` leaving a dangling link a later op or
   `cdt_wf` then dereferences) would be **caught by neither CBMC (OOMs) nor Miri
   (not wired)**; the replay's `cdt_wf` walk would read freed-but-not-reused
   memory without trapping. The ops are all `unsafe fn` over raw pointers, so this
   is a live category. Fix is cheap and idiomatic: run the replay under Miri at a
   *tiny* depth (the repo already runs the fuzz corpus under
   `MIRIFLAGS=-Zmiri-disable-isolation`; add `cargo +nightly miri test -p kcore
   exhaustive -- --ignored` at `EXHAUSTIVE_DEPTH=2`). That gives the composition
   the UB layer the single-op Kani harnesses already have.

3. **`contract_delete_leaf` is a committed *expected-to-fail* harness with nothing
   guarding the "expected" (a watch-item, not yet a correction).** Its failure is
   the finding, and committing it as reproducible scaffold is defensible — but if
   a future cargo-kani changes the `modifies` behavior, the harness could begin to
   pass, or fail with a *different* message, and nothing gates that: it only runs
   under the manual `deep-verify.sh contracts`. Recommend the findings doc /
   script comment state the exact expected failure string (`Check that h->refs is
   assignable`) as the tripwire, so a behavior change is noticed at the next
   cargo-kani upgrade rather than silently. (At 0.67.0 it is fine; this is about
   the pin moving.)

## Is there anything more to do in the Kani direction?

**For the Kani tier specifically: very little, and that is the correct state.**
The routine work is done; reviews 1–2's recommendations plus rec #6's negative
result have established the boundary of what bounded model checking buys this
codebase. Further bounded harnesses of the current genre would add cost without
coverage. The genuinely Kani-shaped items left are all *conditional* or
*maintenance*:

- **Re-test function-contracts when the cargo-kani pin moves.** DN-14 is
  *version-pinned*, not permanent: a future Kani with a dependent/embedded-pointer
  `modifies` (denoting "the object this cap designates") or mature loop contracts
  would make the `delete`↔`obj_unref` recursion and the `revoke` walk provable —
  turning residuals 1–2 into real unbounded proofs. The spike is already committed
  as the scaffold to retry against. Make "re-run `deep-verify.sh contracts`" part
  of the cargo-kani upgrade checklist.
- **Keep the census harness as the live drift-guard.** Plan §7 item 4 is working
  as intended: any *new* reference-holding edge added to the object model must be
  added to `refcount_sound` or the census harness fails. This is ongoing, not a
  task — but it is the one place where neglect would silently re-open a
  use-after-free, so it is worth restating as a standing invariant for future
  contributors.
- **The preemptible-revoke harnesses are owed *when the feature exists*.** Plan
  §10 / the M2 debt: when the revoke walk becomes preemptible, its partial-walk
  restartability needs new harnesses (TLA first, then Kani over the partial-walk
  states). That is future work gated on a feature that does not yet exist — not a
  current gap.
- **Optional, low-value: tighten the below-floor window.** The deep-Kani tier
  (K=4 / `POOL_SLOTS`=6) runs weekly and does not gate PRs, so a regression that
  *only* manifests above the per-PR floor would merge and surface a week later.
  The replay mitigates this for the composition (depth-3 per-PR, and bugs scale
  down to small depth), but the deep-Kani bound has a genuine below-floor window.
  If budget ever allows, rotating one deep harness into the per-PR job (or nudging
  the floor up one notch) would shrink it. This is a tuning knob, not a defect —
  the weekly cadence is a reasonable budget choice and is documented as such.

**The largest remaining verification gap is not Kani's — it is the empty §6
concurrency tier.** The spec §6 and `CLAUDE.md` name Loom/Shuttle as the
concurrency tier and call `ipc` "the first serious Loom/Shuttle target"; there is
still **zero** Loom or Shuttle in the tree. This is correctly *out of scope for
the Kani rewrite* (the kernel is single-core/non-preemptible, and the plan §1/§10
deliberately route concurrency away from CBMC) — review 2 already surfaced it as
an adjacent observation. But it is now the conspicuous hole: with the Kani effort
complete, `ipc` — genuinely concurrent userspace, the one place the bugs Kani is
*constitutionally* unable to find would live — is the highest-value *next*
verification investment, and it is a different tool entirely. The honest framing
for whoever picks this up: **do not spend more effort on Kani; spend it on Loom.**

## Bottom line

After two correction cycles the Kani rewrite is a faithful, complete, and
unusually honest realization of the plan. The standing residual that both prior
reviews circled — exhaustive coverage of the destructive cap algebra — is now
*closed by construction*: the exhaustive plain-Rust replay carries the multi-op /
all-shapes composition (including revoke through every CDT-visible home) that CBMC
provably cannot (rec #6 / DN-14), and the per-PR Kani tier carries the inductive,
additive, and memory-safety properties that are exactly its strength. The two
tiers gate every PR; the deep and contract tiers extend and document the frontier.

The corrections still owed are minor and almost entirely documentary: align plan
§5 with the DN-14 result, give the composition replay the UB layer (run it under
Miri at small depth) that its single-op Kani siblings already have, and pin the
contracts spike's expected-failure string against a future cargo-kani bump. None
touches the core thesis, which is met.

There is little more worth doing *in the Kani direction* — and recognising that is
itself a result: the effort has reached the point of diminishing returns honestly,
with the boundary of bounded model checking mapped rather than assumed. The next
verification dollar belongs to the project's own unbuilt concurrency tier
(Loom/Shuttle over `ipc`), not to more harnesses CBMC would only OOM on.

## Addendum — corrections applied in this change

The two **documentation-only** corrections from "Any corrections needed?" were
applied alongside this review:

- **#1 — plan §5 aligned with DN-14.** `doc/plans/0_kani-rewrite.md` §5 no longer
  reads as if `-Z function-contracts` is an open path for `revoke`'s walk; it now
  records that rec #6 spiked and refuted that at cargo-kani 0.67.0, names the
  exhaustive replay as the instrument, and adds "re-run `deep-verify.sh contracts`"
  to the upgrade checklist.
- **#3 — the expected-failure tripwire pinned.** `18_kani-findings-15.md` now
  states the exact `Check that h->refs is assignable` string and what a *different*
  outcome at a future cargo-kani pin would mean (re-evaluate DN-14).

Correction **#2 (run the exhaustive replay under Miri for the composition's UB
layer)** is **deferred**: it is a CI/test change, not documentation, and warrants
its own PR (a small `kani-deep.yml` / Miri-sweep addition at `EXHAUSTIVE_DEPTH=2`),
so it is left as the one open item from this review.
