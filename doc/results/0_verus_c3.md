# C3 — named frame predicates (evaluation)

Task **C3** (ranks 30 + 29, Wave C) from `doc/plans/0_verus-optimization.md`: factor
the repeated all-object-views-frozen frame block behind a `pub open spec fn`, the
plan's explicitly *opt-in* clarity item carrying a flagged tension with the rev2§6
grep-completeness audit discipline (`doc/guidelines/verus.md`). Two sub-tasks —
**C3a** (`all_obj_views_eq`, the cspace object-view frame) and **C3b**
(`store_views_pinned`, the ready/timer read-only-walk loop invariants). This file
records the per-attempt evaluation under the plan's §2 protocol. Temporary
intermediate report (per `CLAUDE.md`, not citable from code/specs/guidelines).

**Verdict: C3a REVERT, C3b SKIP.** C3a is implementable and **sound** (gate 404/0,
whole-crate) and a genuine *readability* win at the eight edited sites (−34 lines, the
eight-line frame walls → one named, documented predicate). But it is a **measured
crate-total regression of +12.4 %** (kcore SMT 58 769 → 66 070 ms) — far outside the
§2 simplification tolerance (<5 %). The regression lands not on the edited leaf ops
(flat/better) but on their **callers**: `thread::destroy_tcb` **+83.5 % rlimit** and
`cspace::delete` **+117.6 % rlimit**, the two heaviest teardown contexts, which must
now auto-unfold the predicate inside their existing quantifier soup. This both
**refutes the plan's "zero-speed" projection** for C3 and vindicates its skepticism.
Details below.

- **Kind:** the plan rates C3 a **simplification** (clarity-only, technique `refactor`)
  and projects it "zero-speed (an open spec auto-unfolds in-module; the SMT terms are
  byte-identical)". Under §2 the simplification axis keeps it iff the diff is a clear
  readability win **and** the crate SMT total does not materially regress (tolerate
  <5 % for a real clarity win; revert otherwise). C3a clears the clarity bar but
  **fails the regression bar by a wide margin** — the projection that the terms stay
  byte-identical held for the leaf ops but **not** for their callers.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each); `cargo verus verify -p
  kcore` for the gate, `scripts/verus-baseline.sh kcore` (`--time-expanded
  --output-json`) for timing. Per §2 the deterministic **rlimit** field carries the
  claim (per-fn wall ms wobbles ±5–15 %). The two ops C3a does not touch —
  `notification::signal` (rlimit 20 909 644) and `notification::remove_waiter`
  (19 005 838) — are the controls: their rlimits are **byte-identical** before→after,
  so every delta below is the change, not noise.
- **Baseline.** C3 branches off `main` (`8e83395`, post-C1/C2) and edits only
  `kcore/src/cspace.rs`. Fresh cold pre-C3 baseline on this host: **404 verified, 0
  errors, kcore SMT 58 769 ms**. Per-fn before: `dec_ref` 35 ms / rlimit 131 615,
  `obj_ref` 8 / 39 159, `unref_aspace` 51 / 181 909, `ref_aspace` 25 / 87 125,
  `delete_prepare` 433 / 1 201 530, `cdt_unlink` 4 357 / 7 156 805, `cdt_insert_child`
  1 092 / 3 216 582, `slot_move` 3 851 / 6 268 877, `delete` 1 203 / 4 483 052,
  `destroy_tcb` 10 428 / 24 609 374.

## C3a — `all_obj_views_eq` cspace object-view frame (rank 30) · REVERT

### The change (reverted) — one file: `kcore/src/cspace.rs`

A new `pub open spec fn all_obj_views_eq<S: Store>(s0: &S, s1: &S) -> bool` conjoining
the eight object-view equalities (`chan/notif/tcb/timer/timer_head/ready/cspace/irq`),
placed **directly beside `home_views_frozen`** (the existing cross-object frame
predicate) with a doc-comment naming it the rev2§6 grep-discipline audit anchor: the
per-view `X.foo_view() == Y.foo_view()` lines *are* the completeness checklist, so a
future view addition must extend the conjunction in lock-step with the inline frame
lines in the other object modules. `slot_view`/`refs_view` — whichever the op mutates —
stay spelled out at each site as the per-op remainder.

Substituted at the **eight** sites that frame all eight object views whole (the eight-
line wall → one `all_obj_views_eq(old(store), final(store))`, the rationale comment
kept/merged above the call): `dec_ref`, `obj_ref`, `unref_aspace`, `ref_aspace`
(refcount ops, remainder `slot_view`), `delete_prepare` (remainder `refs_view`),
`cdt_unlink` ensures **and** its children-walk loop invariant (`all_obj_views_eq(
old(store), store)`), and `cdt_insert_child`. `slot_move` was **left inline** on
purpose: its three frame lines carry distinct, view-specific rationale comments
(`chan_view`→`send`/`recv`; `notif/tcb`→`binding_notif_wf`; `irq`→`irq_binding_refs`
census) that do not merge into one predicate call without loss. Net file change:
**+41 / −75 = −34 lines**; the pre-existing `ready_view` mis-indentation artifact is
incidentally removed at all eight sites.

### Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p kcore && cargo verus verify -p kcore` ended

```
verification results:: 404 verified, 0 errors
```

**present** (a real cold run). **Gate: PASS (Y).** The change is sound; the rejection is
the §2 regression bar, not correctness.

**On the count (predicted 405, observed 404 — explained, not a red flag):** a non-
recursive `open spec fn` whose body is a pure boolean conjunction carries **no
recommends and no recursion, so it adds zero obligations** — the gate counts items, not
clauses, and the eight callers' obligation counts are unchanged (the predicate merely
auto-unfolds back into the same conjuncts they already proved). No function was removed,
so nothing dropped; 404 → 404 is correct. The trusted base is untouched: `all_obj_views_eq`
is an ordinary `spec fn` over `S: Store`, not a `Store` method — no new seam, tally 14.

### Measurement (§2 step 2b — cold, rlimit deterministic)

The **edited leaf ops** are flat-to-better; the **callers** that consume their
postconditions in a large context blow up:

| obligation | role | SMT ms (before → after) | rlimit (before → after) | verdict |
|---|---|---:|---:|---|
| `cspace::dec_ref` | edited leaf | 35 → 21 | 131 615 → **93 524** (−28.9 %) | improved |
| `cspace::obj_ref` | edited leaf | 8 → 7 | 39 159 → 35 577 (−9.1 %) | improved |
| `cspace::delete_prepare` | edited leaf | 433 → 480 | 1 201 530 → 1 192 897 (−0.7 %) | flat |
| `cspace::cdt_unlink` | edited leaf | 4 357 → 4 355 | 7 156 805 → 7 344 594 (+2.6 %) | flat |
| `cspace::cdt_insert_child` | edited leaf | 1 092 → 1 135 | 3 216 582 → 3 264 932 (+1.5 %) | flat |
| `cspace::ref_aspace` | edited leaf | 25 → 24 | 87 125 → 93 164 (+6.9 %) | ~flat |
| `cspace::unref_aspace` | edited leaf | 51 → 67 | 181 909 → 223 051 (+22.6 %) | regressed |
| **`cspace::delete`** | **caller** | **1 203 → 1 684** | **4 483 052 → 9 754 182 (+117.6 %)** | **regressed** |
| **`thread::destroy_tcb`** | **caller** | **10 428 → 17 189** | **24 609 374 → 45 159 540 (+83.5 %)** | **regressed** |
| `cspace::slot_move` | left inline (control) | 3 851 → 3 938 | 6 268 877 → 6 323 427 (+0.9 %) | flat (noise) |
| `notification::signal` | untouched (control) | 11 280 → 11 477 | 20 909 644 → **20 909 644** (±0) | control |
| `notification::remove_waiter` | untouched (control) | 9 320 → 9 351 | 19 005 838 → **19 005 838** (±0) | control |

Crate:

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT total | 58 769 ms | 66 070 ms | **1.124× (+12.4 %)** |

The two controls' rlimits are **byte-identical** (20 909 644; 19 005 838), and
`slot_move` (left inline) barely moves (+0.9 %), so the +12.4 % crate total is the
change. The dominant terms are `destroy_tcb` **+6.8 s / +20.6 M rlimit** and `delete`
**+0.5 s / +5.3 M rlimit** — both **callers** of the edited ops, both deterministic.

### Why the regression — the substantive finding

The plan projected "zero-speed" on the premise that an `open spec fn` auto-unfolds to
byte-identical SMT terms. That holds where the predicate is **established** (the leaf
op proves the eight equalities and folds them into the predicate cheaply — `dec_ref`
even *drops* −29 %). It fails where the predicate is **consumed**: a caller that
previously received the op's postcondition as **eight ground frame facts** now receives
a single `all_obj_views_eq(old, final)` application that Verus must auto-unfold *inside
the caller's own query*. In `destroy_tcb` and `delete` — the gate's heaviest teardown
contexts, dense with `cspace_wf`/`valid_srank`/`next_reach`/census quantifiers — that
extra predicate-application layer changes the matching landscape and roughly doubles the
proof size. This is precisely the §10 hazard (`verus.md`:1143–1147: a named frame "can
verify each single use yet silently fail to *compose*… across a transitivity lemma or
loop") and the same failure mode the **C2c/C2d** sub-tasks hit — naming/wrapping a frame
that is fine in isolation explodes the consuming context. The clarity-vs-grep tension the
plan flagged turns out to be moot here: the change is rejected on the **speed** axis
before the audit-discipline question is even reached.

### Clarity (§2 step 4)

**A clear readability win at the eight sites, in isolation.** Eight nine-line frame
walls become one self-documenting `all_obj_views_eq(…)` citation each; the predicate is
defined beside its sibling `home_views_frozen` as the documented audit anchor; every
rationale comment is preserved (and `dec_ref` *gains* a frame comment it lacked); the
stray indentation artifact is cleaned up; −34 lines. Were it free, it would pass the
simplification bar. It is **not** free — and a +12.4 % crate-total regression is not a
cost a clarity refactor may carry under §2. (A secondary clarity cost also stands: the
predicate lives only in `cspace.rs` while the other object modules keep the frames
inline, so the codebase would read inconsistently — but the measurement makes this
academic.)

### Host tests

kcore has no host unit suite — its verification *is* its test. The change is proof-only
(a `spec fn` plus eight `ensures`/invariant rewrites), erased from the exec build, so
behaviour is unchanged by construction. `cargo verus verify -p kcore` reported 404/0
with the change; after reverting (a git-exact restore of the committed blob), the tree
is byte-identical to the baseline (404/0, 58 769 ms).

### Decision

**REVERT.** C3a fails the §2 simplification asymmetry: a real readability win (−34
lines, named/documented frame predicate) cannot redeem a **+12.4 % crate-SMT
regression**, driven by `destroy_tcb` (+83.5 % rlimit) and `delete` (+117.6 % rlimit),
both deterministic against byte-identical controls. The plan's "zero-speed" premise is
**refuted by measurement** — an open-spec frame is zero-cost where *established* but
costly where *consumed* in a large caller context. The code is reverted; the
trusted-base kcore row stays **404**. Recorded so a future reader does not re-attempt
the predicate as "new, and surely free."

## C3b — `store_views_pinned` ready/timer loop invariants (rank 29) · SKIP

The plan rates C3b "Lower priority; prefer skip", and exploration confirms the premise
is too weak to clear the clarity bar even before measuring. The three read-only-walk
loops are `ready::ready_unqueue` (ready.rs:848) and `timer::disarm` (timer.rs:160),
which pin **all ten** views, and `timer::check_expired` (timer.rs:762), which pins only
**two** (`slot_view`, `chan_view`) and lets the other eight mutate through its in-loop
`disarm`/`signal`. The intersection — the only views a shared `store_views_pinned<S>(a,b)`
could capture — is therefore just `{slot_view, chan_view}`: it would remove **two** lines
at the two big loops (which still spell out the other eight as remainder) and exactly
match only `check_expired`. That is a partial consolidation, not a clear win.

Independently, the **C3a measurement is decisive against C3b**: the same open-spec-frame-
in-an-invariant pattern is exactly what `cdt_unlink`'s children-walk invariant used, and
these ready/timer loops are themselves multi-second consuming contexts (`ready_unqueue`
1 257 ms; `disarm`/`check_expired` similar). Folding a predicate into their invariants
risks the identical compose-blowup for a two-line dedup. **SKIP** — no code change.

## Outcome summary

| sub-task | kind | result | crate effect | decision |
|---|---|---|---|---|
| **C3a** `all_obj_views_eq` (8 sites) | simp | gate 404/0; −34 lines, clear local clarity win | `destroy_tcb` +83.5 %, `delete` +117.6 % rlimit; **kcore SMT 58 769 → 66 070 ms (+12.4 %)** | **REVERT** |
| **C3b** `store_views_pinned` | simp | always-pinned core only `{slot,chan}` (2/10); weak + C3a-confirmed blowup risk | — (not implemented) | **SKIP** |

Tree reverts to baseline; kcore stays **404/0, 58 769 ms**; trusted-base unchanged
(tally 14).

> C3a verified **Y** (gate **404 / 0**, whole-crate; open-spec fn adds 0 obligations,
> N held at 404) · edited leaf ops flat/better (`dec_ref` −28.9 %, `delete_prepare`
> −0.7 %, `cdt_unlink` +2.6 %) · **callers regressed**: `destroy_tcb` **10 428 →
> 17 189 ms / rlimit 24.6 M → 45.2 M (+83.5 %)**, `delete` **1 203 → 1 684 ms / 4.48 M
> → 9.75 M (+117.6 %)** · controls `signal`/`remove_waiter` rlimit **byte-identical** ·
> kcore SMT **58 769 → 66 070 ms (+12.4 %)** · clarity a clear local win but the
> "zero-speed" projection is **refuted** → **REVERT**. C3b core only `{slot,chan}`
> (2/10), same blowup risk → **SKIP**.
