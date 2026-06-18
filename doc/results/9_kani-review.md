# Kani-rewrite conformance review

A review of how the repository conforms to `doc/plans/0_kani-rewrite.md`, read
against the intermediate reports (`doc/results/2_…` … `8_…`), the spec
(`doc/spec/2_spec_rev2.md` §6), and the TLA+ models (`tla/`). This is an
independent audit, not a self-certification.

## Method

- Inventoried every `#[kani::proof]` in the tree (56 harnesses across `kcore`,
  `urt`, `ipc`, `cas`, `dma-pool`) and mapped each to the plan's §4.1–§4.7
  catalog.
- Read the load-bearing predicates (`cdt_wf`, `chan_wf`, `refcount_sound`) for
  non-vacuity, and confirmed the anti-vacuity unit tests exist
  (`broken_sibling_link_fails_wf`, `corrupt_refcount_fails_soundness`,
  `out_of_window_ring_cap_fails_chan_wf` — each asserts the predicate *rejects*
  a corrupted shape).
- Confirmed every TLA property the §3 mapping cites is real in
  `tla/cap_revocation/CapRevocation.tla` (`TypeOK`, `LiveParent`, `DeadNowhere`,
  `FireSafe`, `RevokedDead`, `ReportMonotone`, `MoveSemantics`,
  `ChannelFireSafe`, `RefCountSound`, `ReclaimedReleased`) and that
  `CommitProtocol.tla` carries `AckedWritesRecoverable`.
- Verified the §8 closeout landed (spec §6 Verus→deferred / Kani row; CLAUDE.md
  commands + tiers; `scratchpad/` removed).
- Spot-ran the host test tier (`cargo test --workspace --exclude kernel` — all
  green) and two cheap harnesses (`check_carve_no_overflow`,
  `check_decode_total` — both `VERIFICATION: SUCCESSFUL`) to confirm the suite
  still verifies on the current tree.

## Verdict

**The plan was implemented faithfully, with good engineering judgment and an
unusually honest record of its own limits.** All seven harness sections exist,
the predicates are substantive (not vacuous), the TLA↔Kani mapping is real, and
the suite found and fixed two genuine defects — including a security-relevant
one. The deviations are forced by CBMC's nature or by the plan's own tier
boundaries (§1), and every one is documented (DN-1…DN-11). The honest caveat: a
few of the plan's most ambitious obligations — the recursive teardown path, the
full-alphabet transition harness, several "for all" host-side claims — are
**narrower in code than in the plan**, and a reader should treat those as
*documented coverage boundaries*, not as the exhaustive proofs the plan's prose
implies. None of the narrowings is hidden; all are defensible; one
(DN-4) is worth a follow-up.

## Conformance at a glance

| Section | Plan harnesses | Implemented as Kani proof | Scoped/dropped (documented) |
|---|---|---|---|
| §4.1 CDT | 8 named | 6 (`insert_child`, `unlink`, `slot_move`, `derive_*`, `revoke`, `delete_reparent`, transition) | `check_delete` (general) and `check_destroy_cspace` → DN-4 (recursive teardown); covered by TSpec + QEMU |
| §4.2 untyped | 5 | all 5 (+3 extra reset/derive negatives) | — (found UO-1, UO-2) |
| §4.3 channel | 8 | all 8 | `destroy_channel`/`teardown_fire_safe` constrained to empty/notif-only contents (DN-4 refinement) |
| §4.4 notif+thread | 6 | all 6 | `thread_teardown` runs with empty bind slots (DN-4) |
| §4.5 aspace | 7 | all 7 | walker harnesses pin VAs concrete (DN); found AS-1 |
| §4.6 syscall | 2 | both | — |
| §4.7 host | ~6 areas | urt(4), ipc(2), cas(2), dma(2) | urt monotonicity, urt seqlock, cas::tlv → DN-10 |

Every present harness verifies; the suite found **2 real bugs** (UO-1 carve
overflow DoS, AS-1 executable-MMIO encoding), both fixed with the harness as the
permanent regression guard.

## What is done well

1. **The predicates are real `TypeOK`/`RefCountSound`, and proven non-vacuous.**
   `cdt_wf` checks membership, empty⇒detached, doubly-linked siblings,
   first-child back-pointers, parent/first-child consistency, and bounded-walk
   acyclicity; the unit tests confirm it *fails* on corrupted shapes. This is
   the single most important thing to get right (a vacuous predicate would make
   every CDT harness worthless) and it is right.
2. **The TLA↔code mapping is faithful and concrete.** `LiveParent` →
   `check_revoke` (post-revoke no descendants, queue + TCB-bind slots emptied,
   census sound); `FireSafe` → `check_bind_fire_safe`; `ChannelFireSafe` →
   `check_teardown_fire_safe` (the M1 step-6 scenario as a proof);
   `ReportMonotone` → `check_report_monotone`; `MoveSemantics` →
   `check_slot_move`. The cited invariants all exist in the `.tla`.
3. **Kani earned its place.** UO-1 (a user-triggerable `next_multiple_of`
   panic) and AS-1 (`PERM_DEVICE | PERM_X` → an executable MMIO mapping, which
   `syscall.rs` does not gate on `!X`) are exactly the latent edges §7 flagged;
   the harnesses confirmed them and the fixes are checked. AS-1 is genuinely
   security-relevant.
4. **The risky surgery (§2.4 aspace walker rewrite, §2.5 syscall split) is
   gated on the QEMU suites**, and the layering grep keeps `kcore` free of
   asm/int→ptr — so the verified core and the shipped core are one body of code
   (no drift, plan §9).
5. **The findings docs are candid.** DN-1…DN-11 record where the proofs are
   weaker than they look, rather than papering over it — which is what makes
   this review tractable and trustworthy.

## Genuine gaps and weaknesses

Ranked by how much they narrow the plan's stated ambition.

1. **The recursive teardown path is not Kani-proven on the real code (DN-4).**
   This is the most significant gap. The plan's §4.1 promised `check_delete`
   (incl. mapped-frame unmap + peer-closed-fire-before-unref) and
   `check_destroy_cspace` (container recursion). CBMC cannot constant-fold
   `obj_unref`'s `match` on the slot-read cap kind, so it explores every arm —
   including `destroy_cspace`/`destroy_channel` looping over symbolic slot
   counts and recursing into `delete` — and blows past budget. What *is* proven:
   notification-cap delete (`check_delete_reparent`, `check_revoke`), and
   channel teardown with empty or notification-only contents
   (`check_destroy_channel`, `check_teardown_fire_safe`). What is **not**: the
   recursive container teardown and frame-unmap-on-delete on the real code —
   these rest on the TLC `TSpec` (a model), source-order review (DN-2), and
   QEMU (dynamic, not exhaustive). Since the plan's thesis is "re-check the TLA
   invariants on the *real implementation*," the most safety-critical path
   (destruction) is the one where that thesis is only partly met. The findings
   name the fix as deferred future work (`-Z stubbing` the `destroy_*`
   recursion, or `-Z function-contracts` on `obj_unref`) — see the
   recommendation below.

2. **The transition-system harness is far narrower than planned.** §4.1 wanted
   a K-step nondet harness over the full action alphabet (retype, derive, move,
   send, recv, bind, thread_exit/fault, delete, revoke, reset) plus the
   `RevokedDead` ghost — the integration check that most directly re-runs the
   TLC result on real code. The implemented `check_cdt_transition_system` is
   **derive + move only, K = 2** (K = 3 is over budget). The destructive ops are
   excluded for the same DN-4 reason. It still exercises derive/move
   interleavings the single-op harnesses miss, but it is a 2-step shadow of the
   planned integration harness, and `RevokedDead` is consequently not exercised
   as a transition property (only `check_revoke`'s single post-state).

3. **`DeadNowhere` is realized in a weakened form (DN-1).** §3 maps it to
   "object destroyed exactly at zero." The census actually asserts
   `hdr.refs == census` (which holds at `0 == 0`); it does **not** assert
   destruction-at-zero, because three sites (`signal`, `remove_waiter`,
   `destroy_channel` binding release) drop a refcount with a bare `-= 1`. This
   is benign (those teardowns are no-ops / memory returns via revoke), but the
   Kani property is strictly weaker than the plan's words.

4. **Several "for all" host-side claims are concrete in code.**
   - `check_dma_alloc_disjoint` is **fully concrete** (two fixed-size allocs).
     CBMC OOMs on the `[(usize,usize); 64]` free list with symbolic sizes
     (DN-10), so the harness proves no-panic + disjoint/in-pool/bijection for
     *one* representative pair — only marginally beyond the existing unit test;
     the symbolic "for all sizes" disjointness is not Kani-proven. This is the
     thinnest harness in the suite.
   - The aspace walker harnesses pin VAs concrete (the for-all VA arithmetic
     lives in the pure `check_va_bounds`); reasonable, but `check_map_model`'s
     "adds exactly the requested pages" is proven at specific VAs, not all.

5. **Three §4.7 properties are not Kani-verified at all** (urt `utc_ns_at`
   monotonicity, the time-page seqlock, `cas::tlv` decode/canonical-form),
   owned instead by proptest / Loom / cargo-fuzz (DN-10). These are
   **well-justified** — symbolic `u128` division, concurrency, and `Vec`-parse
   allocator modeling are genuinely the wrong shape for CBMC, and the §4.7
   preamble plus §1 explicitly route them to other tiers. The minor friction:
   the §4.7 row literally lists the seqlock, while §1 assigns concurrency to
   Loom — an internal plan inconsistency the implementation resolved sensibly in
   favor of §1.

6. **Op-sequence depth is below the stated bounds policy.** §3 says "op
   sequences of 4–6 steps"; the realized depths are `K_STEPS = 3` and the
   transition `K = 2`. Documented as CI-budget-driven and a one-line bump in
   `bounds.rs`, but worth noting the suite runs at the low end of its own policy.

## Are the deviations justified?

Yes — with one qualification. Every deviation is forced by a real property of
CBMC (match/recursion non-folding, symbolic division, large symbolic arrays,
`Vec`/allocator modeling) or by the plan's own §1 tier boundaries
(concurrency → Loom, unbounded termination → TLA + review). The §4.7 preamble
("Kani supplementary; applied only where exhaustiveness buys something a fuzzer
can't") explicitly sanctions the host-side scoping. The bug-fix bundling
(UO-1/2, AS-1 fixed alongside their harness) follows good practice.

The qualification: the deviations are *individually* justified but they
*cumulatively* mean the destruction/teardown machinery — the part with the
hardest lifetime invariants — is the least Kani-covered, while the additive
machinery (derive, retype, map, decode) is the most. That is the opposite of
where one might most want exhaustive proof. It is defensible (TLC + QEMU do
cover it) and honestly disclosed, but it is the shape of the residual risk and
should be stated as such rather than left implicit.

## Recommendations (ranked)

1. **Close DN-4** — the highest-value follow-up. Either `-Z stubbing` the
   `destroy_cspace`/`destroy_channel` recursion with a bounded ghost, or apply
   `-Z function-contracts` to `obj_unref`, so `check_delete` (frame/channel/
   cspace kinds) and `check_destroy_cspace` become real Kani proofs and the
   recursive teardown stops relying on QEMU alone. The findings already name
   this as deferred; it is the one gap that touches the plan's core thesis.
2. **Broaden the transition harness** once DN-4 lifts: add delete/revoke (and,
   budget permitting, retype/send/recv) to the action alphabet and raise K to
   the planned 4–6 — this is what actually re-runs the TLC result on the code.
3. **Add `kani::cover!` checkpoints** to the nondet harnesses (e.g. that the
   `Ok` and each error branch of `decode`, both scenarios of `check_send_move`,
   the in-set/out-of-set cases of `check_range_mapped` are all reachable). The
   wf-predicate unit tests guard the predicates; `cover` would guard the
   *harnesses* against an over-constraining `kani::assume` silently making a
   proof vacuous.
4. **Strengthen or retire `check_dma_alloc_disjoint`** — as concrete-only it
   barely exceeds the unit test; a single symbolic-size alloc (one `alloc`, not
   two) may fit CBMC and would restore some "for all" content, else say plainly
   it's a no-panic smoke check.
5. **Cosmetic:** the findings filenames drift (`6_kani-findings_6.md` uses an
   underscore; the file number leads the part number by one), and the aggregate
   `kani` CI job is approaching its ≤30-min budget (CDT + transition + ring_fifo
   each in the 1.3–3.3 min range). Neither is urgent; both are worth a tidy-up
   before the suite grows further.

## Other remarks

- **Process artifact worth keeping:** CLAUDE.md now records that a Kani harness
  must never be left to hang and that the macOS Bash-tool timeout does not reap
  detached CBMC children (use a `pkill` guard). This is hard-won operational
  knowledge — the monotonicity/dma/tlv harnesses each ran away for tens of
  minutes before being bounded or scoped out — and capturing it is correct.
- **Drift is structurally prevented:** `kcore` *is* the kernel's object
  machinery (the kernel links it), so there is no verified-vs-shipped second
  copy; the layering grep enforces the no-asm/no-int→ptr boundary in CI. Good.
- **Scope honesty:** the suite never claims to prove unbounded revoke
  termination, concurrency, or anything behind boot/asm/MMIO; those are
  explicitly other tiers. The standing caveat in `2_kani-findings.md` is
  repeated in each part. A reader is unlikely to over-trust the results.

## Bottom line

The Kani rewrite is a faithful, competent realization of the plan: the kernel
object core and the host chokepoints are bounded-model-checked against the
TLA-derived invariants on the real code, two real defects were caught and
fixed, and the risky rewrites are gated by the QEMU suites. The deviations are
all justified by tool limits or tier boundaries and are documented in unusual
detail. The one substantive piece of unfinished business is **DN-4**: the
recursive object-teardown path is proven only for the tractable (notification /
empty-container) cases on the real code and otherwise rests on TLC + QEMU. That
is a reasonable place to have stopped given CBMC's limits, but it is the gap to
close next, and it should be read as a known boundary rather than as full
coverage.
