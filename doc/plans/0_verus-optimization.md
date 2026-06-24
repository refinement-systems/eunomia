# Verus proof optimization & simplification plan

A worklist of candidate changes to the gated Verus crates (`kcore`, `ipc`, `urt`,
`freelist`, `dma-pool`, `cas`) that either **speed up verification** (optimization)
or **make a proof easier to read** (simplification), drawn from the six techniques
in `doc/guidelines/verus-computation.html`,
`doc/guidelines/verus-local-proofs.html`, `doc/guidelines/verus-modules.html`,
`doc/guidelines/verus-quantifier-profiling.html`,
`doc/guidelines/verus-smaller.html`, `doc/guidelines/verus-structured-proofs.html`,
and the scaling/trigger discipline already codified in `doc/guidelines/verus.md`
(§3, §5, §6, §10).

Candidates were produced by a fan-out scan of every hot proof region, then each was
adversarially triaged against the live code and `verus.md`'s anti-patterns. The
seven highest-confidence entries (marked **[measured]**) were *empirically
reproduced* on this host during triage; the rest carry an honest confidence and a
per-task accept/reject test. This document is a temporary intermediate report (it
may not be cited from code, specs, or guidelines, per `CLAUDE.md`).

---

## 1. Baseline (the thing every attempt is measured against)

Captured 2026-06-24 with `scripts/verus-baseline.sh` (cold `cargo clean -p <crate>`
before each crate; verus `0.2026.06.07.cd03505`; this host). Raw JSON +
`summary.txt` live under `target/verus-baseline/`.

| crate | verified | SMT cpu (ms) | verify (ms) | wall | share of SMT |
|---|---:|---:|---:|---:|---:|
| **kcore** | 391 | **105 193** | 32 980 | 36 s | ~78 % |
| **freelist** | 29 | **13 017** | 4 907 | 5 s | ~10 % |
| **cas** (`--no-default-features`) | 1 533 | 10 312 | 11 654 | 22 s | ~8 % |
| ipc | 69 | 310 | 1 205 | 1 s | <1 % |
| urt | 29 | 153 | 520 | 1 s | <1 % |
| dma-pool | 0 | 0 | 101 | 1 s | 0 |

Hottest obligations (ms SMT / rlimit):

| ms | rlimit | mode | function |
|---:|---:|---|---|
| 29 716 | 63.7 M | exec | `kcore::cspace::cdt_unlink` — #1, alone ~28 % of kcore |
| 21 050 | 46.1 M | exec | `kcore::thread::destroy_tcb` |
| 14 052 | 39.3 M | exec | `kcore::notification::remove_waiter` (rlimit at the 40 M cap) |
| 13 726 | 20.9 M | exec | `kcore::notification::signal` |
| 4 539 | 169.3 M | exec | `freelist::FreeList::free` |
| 4 139 | 6.3 M | exec | `kcore::cspace::slot_move` |
| 2 799 | 194.4 M | proof | `freelist::FreeList::free_insert` |
| 2 114 | — | exec | `kcore::cspace::delete` |
| 1 825 | — | exec | `kcore::cspace::cdt_insert_child` |
| 1 484 | — | exec | `kcore::channel::recv` |
| 752 | 22.0 M | exec | `cas::prolly::decode_raw` — the cas outlier |

**Where the wall-clock lives.** The four `kcore` teardown ops (`cdt_unlink`,
`destroy_tcb`, `remove_waiter`, `signal`) are ~78 s of kcore's 105 s SMT. kcore +
freelist are >90 % of the whole gate. Everything in `cas`/`ipc`/`urt` is a
sub-second-per-obligation **clarity** target, not a speed target — rank them below
the kcore/freelist movers and judge them on readability, not stopwatch.

Re-establish the baseline any time with `scripts/verus-baseline.sh` (all crates) or
`scripts/verus-baseline.sh <crate>`. Per-crate JSON ranks functions under
`.["times-ms"].smt["smt-run-module-times"][].function-breakdown[]`.

---

## 2. The per-attempt protocol (run this for *every* numbered task)

Do each task on its own branch/commit so the implementer can keep the winners and
drop the rest. For each:

1. **Apply** the change to exactly the function(s)/lemma(s) named — nothing else.
2. **(a) Verify — cold, authoritative, whole-crate.** The CI gate is whole-crate
   with no per-proof filter; a scoped run can false-green from stale cache
   (`verus.md`, "Scoped runs can false-green"). So:
   ```sh
   cargo clean -p <crate> && cargo verus verify -p <crate>   # cas: add --no-default-features
   ```
   The run **must** end `verification results:: N verified, 0 errors` with that line
   **present** (a missing line == cached == not a real run). If it errors, the
   attempt **failed its gate — revert it.** Expected `N`: kcore 391, ipc 69, urt 29,
   freelist 29, cas 1533. A decomposition that adds a lemma *raises* `N` by the number
   of new `proof`/`spec fn` it introduces — predict the new count and treat a *different*
   delta as a red flag (`verus.md`: the gate counts items, not lines).
3. **(b) Measure — did it get faster?** Re-run cold with timing and diff against the
   baseline JSON *and* the previous attempt:
   ```sh
   cargo clean -p <crate> && cargo verus verify -p <crate> -- --time-expanded --output-json > after.json
   # compare the target fn's .time and the crate's times-ms.smt.total to target/verus-baseline/<crate>.json
   ```
   (or just `scripts/verus-baseline.sh <crate>`, which prints the top-8 table).
4. **(c) Judge clarity** — read the diff. Does it read cleaner, or at least neutral?
5. **Apply the asymmetry and keep/drop:**
   - **Optimization** task → keep only if the target fn **and** the crate SMT total
     measurably dropped, and clarity did not get *much* worse. An optimization that
     does not measurably speed verification is worthless even if harmless — drop it.
   - **Simplification** task → keep only if the diff is a clear readability win and
     the crate SMT total did **not** materially regress (rule of thumb: tolerate
     <5 % crate-total regression for a real clarity win; revert otherwise).
6. **Record** `verified Y/N · target-fn before→after ms · crate-total before→after ·
   clarity verdict` in a results table so later picking is mechanical.

*Noise:* per-fn SMT ms varies ±5–15 % run-to-run. For a borderline result run the
cold measure 2–3× and compare medians; trust only a delta that clears the band. The
`rlimit` field is steadier than wall ms — a large **rlimit drop** is strong evidence
of a genuine proof-size reduction even when the ms wobble.

---

## 3. Master ranked list (most → least impactful)

Impact = expected SMT-time win grounded in the baseline; ties broken by confidence;
clarity-only wins ranked lower but retained. **[measured]** = empirically reproduced
during triage (numbers are real, not projected). `dep:` = land after the named task.

| # | crate | task | kind | technique | impact | conf | clarity | risk | dep |
|--:|---|---|---|---|---|--:|---|---|---|
| 1 | freelist | **A1 ·** `wf` sortedness-trigger projection fix **[measured 13.3→4.7 s]** | opt | quant-profiling | **high** | 0.97 | neutral | low | — |
| 2 | kcore | **B1 ·** `destroy_tcb` per-phase frame lemmas | both | decompose | **high** | 0.58 | cleaner | med | — |
| 3 | kcore | **B2 ·** `signal`/`remove_waiter` shared census-delta lemma | both | decompose | **high** | 0.60 | cleaner | med | — |
| 4 | kcore | **B3 ·** `cdt_unlink` merge-block extraction | both | decompose | **high** | 0.50 | cleaner | med | — |
| 8 | cas | **A2a ·** `decode_raw` → `decode_content` helper **[measured 752→185 ms]** | both | decompose | med | 0.95 | cleaner | low | — |
| 5 | kcore | **B5b ·** share the children-walk loop (`cdt_unlink`+`slot_move`) | both | refactor | med | 0.50 | cleaner | high | B5a,B3 |
| 6 | kcore | **B5a ·** children-walk per-iteration peel lemma | both | decompose | med | 0.50 | cleaner | low | — |
| 7 | kcore | **B6 ·** `slot_move` C3 relabel-block lemma | both | decompose | med | 0.45 | cleaner | med | — |
| 9 | kcore | **B3+ ·** spinoff the extracted `cdt_unlink` merge lemma | opt | decompose | med | 0.50 | neutral | low | B3 |
| 10 | kcore | **B4a ·** `recv` post-loop `chan_wf`/FIFO lemmas | opt | decompose | med | 0.50 | neutral | med | — |
| 11 | cas | **A3 ·** `e_payload_ok`/`s_payload_ok` per-tag split **[measured rlimit 962K→145K]** | both | decompose | low | 0.95 | cleaner | low | — |
| 12 | cas | **A2b ·** `encode_raw` → `encode_content` helper **[measured 250→83 ms]** | both | decompose | low | 0.90 | cleaner | low | — |
| 13 | cas | *(duplicate of #8 — do not implement separately)* | — | — | — | — | — | — | =A2a |
| 14 | cas | **A2c ·** `decode_raw` field-assembly tail lemma | opt | decompose | med | 0.50 | cleaner | low | A2a |
| 15 | kcore | **B4b ·** `send` post-loop `chan_wf`/push lemmas | opt | decompose | low | 0.50 | neutral | med | B4a |
| 16 | ipc | **C1a ·** shared `u32` split-bytes `bit_vector` lemma | simp | decompose | low | 0.60 | cleaner | low | — |
| 17 | ipc | **C1b ·** shared `u32` reassemble `bit_vector` lemma | simp | decompose | low | 0.62 | cleaner | low | — |
| 18 | urt | **A5 ·** bit-frame lemmas → §6 recipe form **[measured 158→121 ms]** | both | vstd-reuse | low | 0.95 | cleaner | low | — |
| 19 | ipc | **C1c ·** parallel `u16` codec lemmas | simp | decompose | low | 0.50 | cleaner | low | C1a,C1b |
| 20 | kcore | **B2+ ·** fold `wait`'s census copy into the shared lemma | simp | vstd-reuse | low | 0.50 | cleaner | low | B2 |
| 21 | kcore | **B2+ ·** hide `remove_waiter` `census_dom_complete` block | both | assert-by | low | 0.50 | cleaner | low | B2 |
| 22 | kcore | **C2c ·** hide `signal`/`remove_waiter` dead-frozen tail | opt | assert-by | low | 0.50 | neutral | low | — |
| 23 | kcore | **B2+ ·** retire `remove_waiter`/`signal` rlimit after decompose | opt | decompose | low | 0.50 | cleaner | low | B2 |
| 24 | freelist | **A1+ ·** re-tune raised rlimits after the trigger fix | simp | refactor | low | 0.55 | cleaner | med | A1 |
| 25 | freelist | **A1+ ·** annotate `free` search-loop invariant trigger **[measured]** | simp | refactor | low | 0.92 | cleaner | low | — |
| 26 | kcore | **C2a ·** shared `ring_fifo` "other ring untouched" frame lemma | simp | decompose | low | 0.60 | cleaner | low | — |
| 27 | kcore | **C2b ·** composite `running`-frame transitivity lemma (`destroy_tcb`) | simp | decompose | low | 0.50 | cleaner | low | — |
| 28 | kcore | **C2d ·** `cdt_insert_child` acyclic asserts → `assert-by` | simp | assert-by | low | 0.60 | cleaner | low | — |
| 29 | kcore | **C3b ·** read-only-walk view-frame predicate (ready/timer) | simp | refactor | low | 0.45 | cleaner | low | — |
| 30 | kcore | **C3a ·** `all_obj_views_eq` named frame predicate (cspace) | simp | refactor | low | 0.40 | cleaner | low | — |
| 31 | kcore | *(subsumed by B1 — do not implement separately)* | — | — | — | — | — | — | =B1 |
| 33 | kcore | **C4a ·** align/comment the four `pt_wf_leveled` blocks (aspace) | simp | refactor | low | 0.50 | cleaner | low | — |
| 35 | cas | **C4b ·** `prolly` readers → extracted per-width `bit_vector` lemmas | both | refactor | low | 0.40 | cleaner | low | — |
| 36 | cas | **A4 ·** `recover_records` push-preserves-`rec_ok` lemma **[measured rlimit 660K→385K]** | simp | decompose | low | 0.90 | cleaner | med | — |

Two entries are **defensive / skip** (#32, #34) — see §6.

---

## 4. Implementation waves

The same 36 candidates, grouped for execution. Several are coordinated (one crate
pass, or an ordered chain) — the grouping reflects the crosscutting structure the
scan found. Each lettered task is independently attemptable and keep/drop-able under
the §2 protocol unless a `dep:` is noted.

### Wave A — de-risked quick wins (do first)

These are the empirically-tested changes plus their tightly-coupled follow-ons. Low
risk, fast to land, several already proven on this host.

- **A1 — freelist `wf` sortedness trigger (rank 1, the single biggest lever). [measured]**
  `freelist/src/lib.rs:82`. Change the third `wf` conjunct's trigger from the bare
  whole-tuple `#![trigger self.free@[k]]` to the projection pair
  `#![trigger self.free@[k].0, self.free@[k].1]`, matching the sibling conjuncts on
  lines 79/81. One line, no proof-body change. The bare index trigger self-perpetuates
  a matching loop (the body keeps reintroducing `self.free@[k+1]` of the same shape);
  the projections cover exactly the terms the body reads and stop it.
  *Measured cold:* freelist SMT **13 287 → 4 651 ms (2.85×)**, `free` 4 630→149 ms
  (31×), `free_insert` 2 865→1 749, `free_replace` 2 679→1 462, `free_both` 1 673→620,
  `alloc` 1 001→308; stays 29/0. Implements `verus.md` §10 (a bare index trigger floods
  context) and §3 (projection over raw index). **Accept:** crate total falls hard, 29/0.
- **A1+ — re-tune the raised rlimits (rank 24). dep: A1.** After A1, the
  `#[verifier::rlimit(…)]` budgets on `is_allocated` (:289), `alloc` (:351),
  `free_insert` (:794), `free_replace` (:896), `free_both` (:1003), `free_covers_both`
  (:1067), `free` (:1130) are over-provisioned. *Reduce* (do not blanket-delete) each
  to the post-fix minimum, cold-verifying after each. Triage found that
  `free_insert`, `free_replace`, **and** `free_both` (620 ms post-fix) still exceed the
  default and must keep a reduced budget; the others may go to default. Pure clarity
  (`verus.md` §10: retire misleading "this proof is hard" signals). **Accept:** 29/0
  with smaller, honest budgets; **revert any reduction that 0-errors.**
- **A1++ — annotate the `free` search-loop invariant trigger (rank 25). [measured]**
  `freelist/src/lib.rs:1157`. Add `#![trigger self.free@[k].0]` to the
  `forall|k| 0<=k<i ==> self.free@[k].0 < off` loop invariant. Verus prints a
  "low confidence" trigger note there on every run; annotating it (the trigger Verus
  already infers) silences the note and documents intent. Cold-verified 29/0, note
  gone, no SMT change. Independent of A1 but land in the same freelist pass.
- **A2 — cas `prolly` codec helper extractions (ranks 8/12/14). [measured for a, b]**
  All edit `cas/src/prolly.rs`; apply together, cold `cargo clean -p cas` once
  (`--no-default-features`). Build: `cargo verus verify -p cas --no-default-features`.
  - **A2a — `decode_content` helper (rank 8). [measured]** Extract the 3-arm
    content-tag parse (orig ~1033–1089) into
    `fn decode_content(buf, p_ctag) -> Result<(RawContent, usize), TlvErr>` with
    `requires p_ctag < buf@.len()`, `ensures Ok((c,end)) ==> p_ctag < end <= buf@.len()
    && content_bytes(c) == buf@.subrange(p_ctag, end)`; helper keeps its own
    `broadcast use group_slice_axioms`. `decode_raw` becomes `match decode_content(…)`
    + one bridging assert. *Measured:* `decode_raw` **752 ms / rlimit 22.0 M → 185 ms /
    3.6 M**; new helper 33 ms; 81/0 (was 80, the one expected new obligation). (Rank 13
    is the same change described twice — do **not** implement it separately.)
  - **A2b — `encode_content` helper (rank 12). [measured]** Symmetric: extract the
    3-arm push match (orig ~935–949) into
    `fn encode_content(out: &mut Vec<u8>, c: &RawContent)` with
    `ensures final(out)@ == old(out)@ + content_bytes(*c)`. *Measured:* `encode_raw`
    **250 ms / rlimit 11.7 M → 83 ms / 2.83 M**; 83/0 with both helpers.
  - **A2c — `decode_raw` field-assembly tail lemma (rank 14). dep: A2a.** Extract the
    closing field-assembly (orig ~1195–1208: five per-field subrange asserts + six
    `lemma_cat` calls + the final `canonical_bytes` assert) into
    `proof fn lemma_entry_assemble(...)` taking the five field facts as `requires` and
    `canonical_bytes(e) == buf@.subrange(start, opt_end)` as `ensures`. Smaller slice
    than A2a/A2b (the `lemma_cat`s are ~0.2 ms each), so expect a modest further cut;
    verify it actually moves `decode_raw` before keeping.
- **A3 — cas payload-ok per-tag split (rank 11). [measured]** `cas/src/store.rs:744`
  (`s_payload_ok`) + `:906` (`e_payload_ok`). Split the spec into three helpers
  `s_payload_{write,unlink,rename}_ok(pay, p_tag)` (top keeps the `s_take(pay,0,1)`
  tag-byte guard + dispatch); mirror three exec twins `e_payload_{…}_ok` each
  `requires p_tag <= pay@.len()`, `ensures r == s_payload_<arm>_ok(...)`. *Measured:*
  `e_payload_ok` **rlimit 962 665 → 145 084**, payload-ok SMT ~75→~41 ms, module total
  228→211; verifies 0 errors. Matches the file's existing `rec_ok`/`laid_out` split style.
- **A4 — cas `recover_records` push-preserves-`rec_ok` lemma (rank 36). [measured]**
  `cas/src/store.rs:1442`. Extract the inline ~30-line
  `assert forall|k| … implies rec_ok(wal@, r, k) by {…}` into
  `proof fn lemma_push_preserves_rec_ok(wal, prev, r, new: RecMeta, rlen)`. **Pass the
  pushed `RecMeta` as a ghost param** (do not reconstruct it) to dodge the `Vec<u8>`
  `ref_name` literal in spec context. *Measured:* `recover_records` **95 ms / rlimit
  660 042 → 51 ms / 384 799**, but the lemma itself costs ~50 ms so module total is
  flat (228→232–238). **This is a simplification, not a speedup** — its payoff is the
  rlimit headroom on the hot obligation + a named 30-line block. Keep on the clarity
  axis only.
- **A5 — urt bit-frame lemmas → §6 recipe form (rank 18). [measured]**
  `urt/src/slots.rs:394` (`lemma_set_bit`) + `:406` (`lemma_bit_other`), call sites
  `:142,:148`. Rewrite both into the canonical packed-bitmap form (`verus.md` §6,
  lines 816–828): drop the runtime `free: bool` param, move `by (bit_vector)` onto the
  signature, state both write directions as plain unconditional `ensures` with empty
  body; drop the `free` arg at the call sites (the `if free` exec branch is unchanged).
  *Measured:* `lemma_bit_other` 36→18 ms, `SlotAlloc::set` 29→18, `lemma_set_bit` 18→13,
  crate total **158→121 ms (~23 %)**; 25/0 (was 29 — the four inline `by (bit_vector)`
  sub-obligations collapse into the two signatures, expected), all 22 host tests pass.

### Wave B — high-impact kcore decompositions (the real prize)

These target the four multi-second teardown ops that are ~78 s of the gate. None are
pre-measured (they need real implementation work) and confidences are 0.45–0.60, but
this is where the wall-clock win lives. `verus.md` §10 names decomposition the
**default** fix and these ops have already exhausted the last-resort levers
(`spinoff_prover`/`rlimit`), which is exactly when §10 says to decompose. Apply one
phase at a time and cold-verify after each. Build/verify: `cargo clean -p kcore &&
cargo verus verify -p kcore` (predict the new `N` = 391 + new lemmas).

- **B1 — `destroy_tcb` per-phase frame lemmas (rank 2).** `kcore/src/thread.rs`.
  `destroy_tcb` (21 050 ms, #2 in the gate) is already
  `#[verifier::spinoff_prover]+rlimit(30)`, so its cost is one isolated context
  carrying ~20 `ensures` whose six frame clauses are `pub open spec` quantified
  predicates (`dead_tcb_frozen_at`, `home_views_frozen`, `unhomed_frozen_free`,
  `refs_death_persist`, `emptied_via_dead_home_free` — the last a `forall ⇒ exists`,
  the fragile shape §10 says to keep out of the giant context) that auto-unfold in
  *every* inline `proof` block. Extract each teardown phase's invariant
  re-establishment into its own `proof fn` (§10 "key it tightly": `requires` = that
  phase's local edit facts, `ensures` = the six global frames composed onto `st0`).
  Start with the two heaviest self-contained phases: **halt** (733–783) →
  `lemma_destroy_tcb_halt_frame(...)` and **cspace-release** (899–957) →
  `lemma_destroy_tcb_cspace_release_frame(...)`; then detach / delete-bind /
  aspace-release. Mirrors the in-tree `destroy_cspace` (`cspace.rs:10318`). Likely a
  20–40 % cut (the op body must still establish ~20 `ensures`), not a 2×.
  **(Rank 31 — the exit-state quadruple — is subsumed here: it becomes the per-phase
  lemmas' requires/ensures. Do not extract it separately.)**
- **B2 — shared `signal`/`remove_waiter` census-delta lemma (rank 3) + its three
  follow-ons.** One PR across `notification.rs` + a new lemma in `cspace.rs`.
  - **B2 anchor.** Add `proof fn lemma_signal_census_delta<S:Store>(s0,s1,n)` (and a
    `+1` twin, or one delta-parameterized fn) whose `requires` are the cheap local
    facts the bodies already prove (tcb/refs domains equal; `nv` frame for `o!=n`;
    every changed TCB names `n` in `wait_notif`; the `waiter_refs(n) ±1` delta) and
    whose `ensures` is the full per-object map `obj_census(s1,o) == if o==n {…−1} else
    {…}`. Key the map trigger on `obj_census(s1,o)` so it does not over-fire in
    census-agnostic callers (§10). `signal` (378–402), `remove_waiter` (943–964 and
    1018–1036) each prove only the local facts then call it once;
    `census_delta_frozen`, `census_off_by_one`, `census_dom_complete` all derive from
    the one map. Strong precedent: `cspace.rs:6739 lemma_census_after_hold_clear`
    already produces this map shape for `destroy_tcb`. Hits **two** multi-second
    obligations (14 052 + 13 726 ms) at once. Decisive evidence it's a context-size
    problem: `notification::wait` proves the *same* `+1` census forall via the *same*
    lemma loop yet costs only 523 ms — because its `ensures` list is short.
  - **B2+ rank 20 (dep: B2).** Have `wait` (637–655) call the `+1` twin, removing the
    third hand-rolled copy. `wait` is 523 ms (rlimit 1.8 M, ample headroom) — pure
    simplification, must not regress.
  - **B2+ rank 21 (dep: B2).** `remove_waiter` (1014–1037): once the lemma lands this
    block collapses to a one-line citation of the map's `o!=n` case. (If B2 is deferred,
    the cheaper standalone fallback is to wrap 1014–1037 in `assert(census_dom_complete(old)
    ⇒ census_dom_complete(store)) by { … }` to stop the per-object equalities leaking
    into the `dead_tcb_frozen`/ready tail. Do not do both.)
  - **B2+ rank 23 (dep: B2).** Re-measure and **reduce** `remove_waiter`'s
    `#[verifier::rlimit(40)]` (it's at 39.3 M / 40 M cap *today* — load-bearing, do not
    touch before B2) toward default; same for `signal`'s `rlimit(50)`. §10 cleanup:
    retire the last resort once the heavy sub-proof is extracted.
- **B3 — `cdt_unlink` merge-block extraction (rank 4) + optional spinoff (rank 9).**
  `kcore/src/cspace.rs`, the gate's #1 obligation (29 716 ms), already at
  `rlimit(60)+spinoff_prover`. Extract the merge/`forall`-match by-block (9693–9729)
  into `proof fn lemma_unlink_merge(m0, slot, last, parent, prev, next, first, head,
  ma/mb/mc/md/mw)` with `requires` = `cspace_wf(m0)` + the 5-step splice-map defining
  equalities (9481–9610) + the `lemma_unlink_roles` role facts + `md[slot]==m0[slot]`,
  `ensures` = `mfin =~= unlinked(m0, slot, last)`. **State the contract via the
  insert-chain `md` equalities, not a broad `forall|j|` frame** (§10 "prefer a single
  `Map::insert` equality"). The merge does *not* need the `next_reach`/`valid_srank`
  recursion (the `choose` at 9466) or the children-walk residue, so the lemma gets a
  fresh solver shorn of that quantifier soup. Precedent: the isomorphic
  `lemma_unlink_children` (8732–8806) verifies in 237 ms isolated. **Risk (the 0.5
  confidence):** the `requires` must replicate the five nested splice-map equalities
  exactly or the cold verify breaks; and the 29.7 s is *also* fed by the children-walk
  loop, so the merge alone is unlikely to halve it. **Rank 9 (dep: B3):** if the
  extracted lemma is still multi-second, mark *that lemma* (not `cdt_unlink`)
  `#[verifier::spinoff_prover]` (§10 ladder step 2) — only if the extraction alone
  doesn't bring it down enough.
- **B4 — channel post-loop lemmas (ranks 10, 15).** `kcore/src/channel.rs`.
  - **B4a — `recv` (rank 10).** Post-loop `chan_wf` block (1519–1552) and `drop_first`
    FIFO block (1555–1575). Even though both are `assert-by`-wrapped, `by {}` restricts
    only what *leaves* the block — *inside*, the full 14-clause pass-2 loop invariant +
    `dests` forall + `slot_move` framing are visible, so the obligation is solved
    against the big query. Extract into `proof fn`(s) keyed on the ~8 shape facts
    already asserted at 1507–1513, **one lemma per conjunct** (`chan_wf` and the
    `ring_fifo … =~= … .drop_first()`; `verus.md` line 1074). Keep the
    `#![trigger (head+j)%depth]` + explicit `choose` binding when moving the
    existential-witness block (§10 lines 1150–1161). `recv` is 1 484 ms.
  - **B4b — `send` (rank 15). dep: B4a.** Symmetric extraction for `send`'s post-loop
    `chan_wf` (window-grew case) + push FIFO (1057–1094, shape facts at 1041–1045).
    `send` is 579 ms and simpler — do it for symmetry once B4a shares the scaffolding.
- **B5 — cspace children-walk dedup (ranks 6 then 5).** `cdt_unlink` (9495–9569) and
  `slot_move` (9991–10072) carry a near-verbatim re-parent `while` loop.
  - **B5a — peel lemma (rank 6, low risk, do first).** Extract the verbatim
    per-iteration sibling-walk step (9547–9566 vs 10048–10070) into
    `proof fn lemma_children_walk_peel(m0, cur, nn, srk)`. The peel `assert forall …
    next_reach(m0,cur,x,srk)==next_reach(m0,nn,x,srk) by {}` is already empty (Z3
    discharges from one-step unfolding), so this is primarily a clarity dedup with a
    modest speed effect (evicts the loop-local frame cloud from the derivation).
  - **B5b — full-loop share (rank 5, high risk). dep: B5a, B3.** *Investigate* (don't
    assume) factoring the whole ~13-invariant loop into a shared exec helper
    `reparent_children<S:Store>(store, slot, new_parent, …)`, or — if the exec shapes
    diverge (unlink targets the grandparent, move targets `Some(dst)` and threads
    `lemma_child_relabeled`) — at least a shared invariant-preservation `proof fn`.
    Hits both hot ops and removes ~75 mirrored lines, but unifying two exec loops that
    write different parents may need a closure/trait-param the `Store` seam doesn't
    cleanly support. A design spike, not a drop-in.
- **B6 — `slot_move` C3 relabel block (rank 7).** `cspace.rs:9954–9977`. Extract the
  C3 non-child arm (the `assert forall|k| … k!=src && m0[k].parent!=Some(src) ⇒
  m4[k]==rl[k]` with dst-cases + per-key `lemma_generic_relabeled`) into
  `proof fn lemma_slot_move_m4_nonchild(...)` (`requires` the m2/m3/m4 splice
  equalities + `cspace_wf(m0)` + `is_empty_cap(m0[dst].cap)`). Leave C1/C2 inline; the
  same forall recurs post-loop (10090–10118) and the lemma can serve those too.
  `slot_move` is 4 139 ms (~4 % of kcore), so even halving it saves ~2 s — medium.
  Mechanical but fiddly (the body-local maps must be restated as params), hence 0.45.

### Wave C — clarity-only simplifications (do if the diff earns it)

Neutral-to-low speed; judge purely on readability, and **revert any that regress the
crate total** (the simplification asymmetry). Several have a noted tension with
`verus.md`:422–431 — see the per-task note.

- **C1 — ipc codec `bit_vector` lemmas (ranks 16/17/19).** One coordinated sweep over
  `ipc/src/session.rs` + `ipc/src/header.rs`; land together (crate is 293 ms total —
  value is removing duplication, not speed).
  - **C1a (rank 16).** Add `proof fn lemma_u32_le_split_bytes(b0 b1 b2 b3: u8) by
    (bit_vector)` (the four `&0xff`/`>>8&0xff`/… extractions over the reassembled
    `(b0 as u32)|((b1 as u32)<<8)|…`); replace the inline four-assert blocks
    (session.rs 517–524, 562–577; header.rs 174–181) with calls.
  - **C1b (rank 17).** Add `proof fn lemma_u32_le_reassemble(x: u32) by (bit_vector)`
    (the double-cast `as u8 as u32` round-trip == x); replace the four inline
    `by (bit_vector)` asserts (session.rs 501–503, 539–544; header.rs 147–151).
  - **C1c (rank 19). dep: C1a,C1b.** Parallel `u16` pair for the opcode/flags sites in
    `header.rs`. Lowest value — bundle or skip. Leave `reactor.rs:lowest_clear_bit`
    untouched (canonical §6 bit-scan, not a duplication target).
- **C2 — kcore local clarity lemmas (ranks 26/27/28/22).**
  - **C2a (rank 26).** Extract one `proof fn lemma_ring_fifo_frame(...)` for the
    near-verbatim "other ring untouched" blocks in `channel.rs` send (1097–1109) and
    recv (1578–1590). **Express the per-index `requires` via the named
    `lemma_ring_msg_eq` congruence, not a raw `ring_cap`-index forall** (§10
    1143–1147: an index-trigger forall verifies singly but fails to *compose*). Speed
    neutral (both blocks already `assert-by`-wrapped); clarity dedup of ~13×2 lines.
  - **C2b (rank 27).** Add `proof fn lemma_running_frame_trans<S:Store>(a,b,c)`
    composing the four per-edge frames over (a,b)+(b,c) → (a,c), replacing the seven
    4-lemma trans clusters in `destroy_tcb` (826–830, 870–874, …). The hidden bodies
    are trivial (1–3 lines) so speed is ~neutral; modest clarity. The cross-unit claim
    (that it drops into `cdt_unlink`) is **false** — cspace's trans lemmas are not
    co-grouped; keep it scoped to `destroy_tcb`.
  - **C2d (rank 28).** Wrap `cdt_insert_child`'s two acyclic-preservation calls
    (9093–9114) each in its own `assert(acyclic(m1)) by { … }` /
    `assert(sib_acyclic(m1)) by { … }`. **Note:** that block is the *final* block of
    the fn, so scoping the broad frame foralls yields ~0 speed here — it's pure hygiene
    (apply the "broad frame forall floods context" rule prophylactically), correctly a
    simplification.
  - **C2c (rank 22, opt).** Wrap the `dead_tcb_frozen`/`refs_death_persist` tail in
    `signal` (424–439) and `remove_waiter` (1040–1054) in `assert(… && …) by { … }` to
    drop the two intermediate `ObjId` foralls from context. In `signal` the block is
    terminal (≈ no-op); in `remove_waiter` the ready-frame tail (1059–1065) follows, so
    it helps there. **Optimization → verify the per-fn ms actually drops before keeping.**
    Likely marginal once B2 has shrunk the query; skip if B2 lands.
- **C3 — named frame predicates (ranks 30/29). Tension — read first.** Both factor a
  repeated view-equality frame block behind a `pub open spec` predicate. They are
  **zero-speed** (an open spec auto-unfolds in-module; the SMT terms are byte-identical
  — `cdt_unlink`'s cost is the `cspace_wf`/`valid_srank`/`next_reach` quantifiers, not
  the scalar view equalities) and they have a **real tension with `verus.md`:422–431**:
  the inline frame style is a deliberate "the grep *is* the completeness checklist"
  audit aid, and no aggregate view predicate exists today (suggesting the inline style
  is intentional). The project *does* factor heavier cross-object frames
  (`home_views_frozen`, `census_delta_frozen`) but leaves the pure all-frozen block
  expanded.
  - **C3a (rank 30).** `all_obj_views_eq<S>(s0,s1)` conjoining the 8 object-view
    equalities (chan/notif/tcb/timer/timer_head/ready/cspace/irq), modeled on
    `home_views_frozen`; substitute *only* the ~8–9 cspace.rs op/invariant sites that
    frame all 8 against `old(store)`, leaving any `slot_view`/`refs_view` conjuncts
    those sites also carry spelled out. Re-verify `cdt_unlink`/`dec_ref`/`delete_prepare`.
  - **C3b (rank 29).** Optional `store_views_pinned<S>(a,b)` for the read-only-walk loop
    invariants in `ready.rs`/`timer.rs`. Each fn pins a *different* mutated-view subset,
    so the predicate captures only the always-pinned core and each site still lists its
    remainder — a partial consolidation. **Lower priority; prefer skip** unless defined
    adjacent to the view list as the documented audit anchor.
  - **Recommendation:** treat C3 as *opt-in* clarity. If you do it, define the predicate
    right beside the view list and comment it as the audit anchor; otherwise skip to
    preserve the grep discipline.
- **C4 — cosmetic / bounded (ranks 33/35).**
  - **C4a (rank 33).** `aspace.rs` `pt_wf_leveled` blocks (`lemma_link_l1` :695,
    `lemma_link_l2` :1021, `lemma_grow_pool` :890, `lemma_leaf_write` :1188): ~120 ms
    total, no SMT to recover. **Only** the low-risk variant — visually align the four
    blocks and add a comment cross-referencing the (b1/b2/c1/c2) clause numbers in the
    `pt_wf_leveled` doc-comment (476–510). **Do not** introduce `pt_wf_b1/_b2/…`
    sub-predicates (changes a closed spec's auto-unfold for zero speed).
  - **C4b (rank 35).** `cas/src/prolly.rs` readers (`read_u16/u32/u64_le`, 713–796)
    each carry inline `by (bit_vector)` queries because prolly's value-form spec doesn't
    match the `b0|(b1<<8)|…` construction definitionally. Extract the per-byte facts
    into three tiny `lemma_u{16,32,64}_le_bytes` called once per reader (the §8 shift-form
    rewrite is more invasive and only *relocates* the bit_vector cost to the writers —
    avoid). Real but small ceiling (~130 ms: read_u64_le 104 + read_u32_le 32 +
    read_u16_le 16). **Note:** the scan's original "~1.2 s of vstd bit/endian prelude"
    claim was triage-**refuted** — those vstd lemmas are library modules verified
    regardless of prolly, and `by (bit_vector)` bitblasts to a fixed-width SAT query that
    instantiates *none* of them. Modest clarity, possibly a wash.

---

## 5. Suggested order of attack

1. **A1 / A1+ / A1++** — the freelist pass. Highest impact-×-confidence in the whole
   set (a verified one-line 2.85× win) plus its mandatory rlimit cleanup. One crate,
   one afternoon.
2. **A5, A2, A3, A4** — the empirically-tested cas/urt wins. High confidence, small
   absolute SMT but real rlimit-headroom + clarity; cheap insurance.
3. **B1, B2, B3** — the kcore movers, in that order (B1/B2 are independent and higher
   confidence than B3; B3 is the biggest single obligation but the trickiest). This is
   where the real wall-clock lives; budget for iteration and cold re-verify per phase.
4. **B4, B5a, B6** — second-tier kcore decompositions once the big three are in.
5. **B5b** — only if B5a + B3 land cleanly and the loop-share spike looks tractable.
6. **C1 / C2 / C4** — clarity passes, any time, judged on the diff.
7. **C3** — opt-in only, mind the grep-discipline tension.

After each wave, re-run `scripts/verus-baseline.sh` and diff the summary against
`target/verus-baseline/summary.txt` to bank the cumulative win.

---

## 6. Considered and rejected (do **not** spend effort here)

- **Rank 34 — opaque the cspace `acyclic`/`sib_acyclic` existentials.** Confidence
  0.2; measured evidence is *against* it. `acyclic`/`sib_acyclic` are cheap one-liner
  `exists` wrappers; the expensive part of `cspace_wf` is `cdt_wf`'s seven per-conjunct
  `forall #[trigger] m.dom().contains(k)` quantifiers, which opaque does **not** touch.
  Worse, a missed `reveal` *silently* fails verification across ~85 `cspace_wf` sites /
  25 `choose` points — high blast radius, no payoff. `verus.md` §10 endorses opaque for
  a *recursive* spec; `acyclic` is non-recursive, and the real hazard text (1153–1158)
  prescribes a *deterministic selector* for the `forall ⇒ exists` re-check, not opaque.
  Skip unless the Wave-B decompositions fail to move `cdt_unlink`.
- **Rank 32 — lowering the kcore `ready.rs`/`timer.rs` rlimits as a "speedup."** A
  non-lever: `rlimit` is a per-obligation *cost cap*, not a time determinant — lowering
  it cannot make a passing proof faster, only break it. Those ops' real cost is
  irreducible k-indexed chain-splice bookkeeping (no trigger loop, no unquarantined
  nonlinear arith, no excess fuel — confirmed by reading the loops); the hard sub-facts
  are already extracted into cspace lemmas. This unit is ~7 % of kcore SMT with nothing
  to gain on speed.
- **Ranks 13 and 31 — duplicate markers.** Rank 13 (`decode-raw-extract-content-dispatch`)
  is the same change as A2a; rank 31 (`destroy_tcb` exit-state extraction) is subsumed
  by B1. Retained in the master list only so a future reader doesn't re-discover them as
  "new" candidates and double-count.

---

## 7. Technique coverage note

Of the six techniques: **decomposition** (verus-smaller) and **quantifier profiling**
(verus-quantifier-profiling) carried essentially all the impact — the single biggest
win (A1) is a profiling-driven trigger fix, and every high-impact kcore item is a
lemma extraction. **`assert-by` hiding** (verus-local-proofs) appears as low-value
hygiene (C2c/C2d/B2+) because the project already `assert-by`-wraps heavily, so the
remaining leaks are small. **opaque/reveal** (verus-modules) was found *net-negative*
on the one place it looked applicable (rank 34). **calc!** (verus-structured-proofs)
found **no** adopted home — the codec bijection lemmas it might suit (ipc) are better
served by the small named `bit_vector` lemmas (C1), and the arithmetic chains are
already quarantined behind `vstd::arithmetic` per `verus.md` §5; flag it as a tool to
keep in mind for any *future* multi-step (in)equality chain rather than a retrofit.
**Proof by computation** (verus-computation) likewise found no current fit (no
concrete-range or recursive-evaluation obligation is on a hot path). These two
absences are a finding, not a gap — the existing proofs already avoid the patterns
those techniques rescue.
