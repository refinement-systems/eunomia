# Kani verification findings — part 9 (`kani::cover!` vacuity guards)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `10_kani-findings-8.md`.
This part implements recommendation #3 of the conformance review
(`9_kani-review.md`): add `kani::cover!` checkpoints to the nondet harnesses so
an over-constraining `kani::assume` cannot silently make a proof *vacuous*. The
standing caveat and design notes (DN-1…DN-12) apply unchanged.

## Why

A harness that asserts a property *conditionally* — `if x.is_ok() { assert!(…) }`,
`if let Ok(sys) = decode(…) { … }` — or constrains a nondet input with
`kani::assume`, is only as strong as the reachability of that condition. If the
guard is ever unsatisfiable the assertions inside never run, the harness reports
`SUCCESSFUL`, and the proof is worthless. The wf-predicate unit tests guard the
*predicates*; nothing guarded the *harnesses* against this. `kani::cover!(c)`
asks Kani to prove `c` is reachable, turning "is this branch live?" into a
checkable obligation.

## What was instrumented

`kani::cover!` was previously used nowhere in the tree. Covers were added only
to *nondet* harnesses with a real vacuity surface — a conditional-assertion
guard, a decode/parse Ok-vs-error split, or a multi-scenario nondet gated by an
`assume`. Fully-concrete harnesses (e.g. `check_revoke`, `check_retype_cdt`, the
two `dma-pool` harnesses) and `#[kani::should_panic]` negatives were
deliberately left alone — there is no nondet there to be vacuous.

**15 harnesses, 41 cover checkpoints — every one `SATISFIED`** (guarded local
runs, cargo-kani 0.67.0):

| Crate · harness | Covers | Guards |
|---|---|---|
| `kcore` `channel::check_send_move` | 3/3 | the three send scenarios (Ok / Full / PeerClosed) — review-cited |
| `kcore` `channel::check_ring_fifo` | 4/4 | send-Ok, Full, recv-Ok, Empty all reached |
| `kcore` `aspace::check_range_mapped` | 4/4 | in-set / out-of-set / zero-len / write-granted — review-cited |
| `kcore` `aspace::check_map_model` | 2/2 | map Ok **and** `NeedMemory` exhaustion both reachable |
| `kcore` `sysabi::check_decode_total` | 2/2 | a known op decodes `Ok` **and** an unknown errors — review-cited |
| `kcore` `sysabi::check_validate_lengths` | 4/4 | `decode` reaches `Ok` (the `if let` guard); `ObjType::from_u64` both sides |
| `kcore` `untyped::check_carve_geometry` | 2/2 | `carve` succeeds (the `let Ok else return` — highest-risk) |
| `kcore` `untyped::check_carve_no_overflow` | 2/2 | totality not vacuous over only the error path |
| `kcore` `untyped::check_retype_rights` | 4/4 | each rights arm (Frame/Thread/Untyped/other) reached |
| `kcore` `notification::check_remove_waiter` | 4/4 | head/middle/tail/absent removal all reached |
| `kcore` `transition::check_cdt_transition_system` | 2/2 | a real derive **and** a real move execute (not all guard-skipped) |
| `ipc` `check_header_decode_total` | 2/2 | accept (exact len) **and** reject (short/trailing) — the review's headline example |
| `cas` `check_superblock_geometry` | 2/2 | the `validate_geometry().is_ok()` body is reachable, plus a reject |
| `cas` `check_superblock_decode_total` | 2/2 | parsed **and** refused both reachable (stubbed hash) |
| `urt` `check_slots_free_reuse` | 2/2 | freed-index window boundaries (`i==0`, `i==CAP-1`) reached |

**No vacuity defect was found** — every `assume` in the suite is honest; the
covers are now the standing evidence of that, and will fail if a future edit
over-constrains one.

## Findings about `kani::cover!` itself

- **DN-13 — `kani::cover!` is *informational*, not a gate, in cargo-kani 0.67.**
  An unreachable cover does **not** fail the run: Kani still reports
  `VERIFICATION: SUCCESSFUL` and exits 0, only lowering the summary tally to
  `** N of M cover properties satisfied` (N < M) and marking that check
  `Status: UNSATISFIABLE`. There is no `--fail-on-uncoverable` flag in 0.67
  (`--coverage` is the unrelated source-coverage feature). Confirmed directly: a
  deliberately-unreachable probe cover reported `1 of 2`/`UNSATISFIABLE` yet the
  harness passed. **So the covers only become a CI guard with a post-check.**
  `.github/workflows/ci.yml`'s kani job now `tee`s each `cargo kani` run to a log
  and fails if any `** N of M cover properties satisfied` line has `N != M`
  (`awk '{ if ($2 != $4) bad=1 } END { exit bad }'`). This is a deviation from
  the rec-#3 plan, which assumed an unreachable cover would fail on its own; the
  post-check restores the intended guard.

- **Gotcha — don't put `matches!` inside `cover!`.** `kani::cover!(matches!(r,
  Err(E::X)))` lowered to a `match` whose dead arm CBMC instrumented as a
  *second*, spurious `UNSATISFIABLE` cover (seen as `2 of 3` on `check_map_model`
  with one real condition). Using `r == Err(E::X)` (the error enums are already
  `PartialEq`, as the harnesses' own asserts rely on) gives a clean single
  cover. All covers here use `is_ok`/`is_err`/`==`/scalar comparisons, never
  `matches!`.

## Cost

Covers are reachability queries (find one satisfying assignment), so the
per-harness cost is negligible: the instrumented harnesses verify in the same
band as before (e.g. `check_ring_fifo` ~132 s, `check_cdt_transition_system`
~293 s, the host-side and sysabi/untyped harnesses in seconds). No CI-budget
concern beyond what rec #5 already flagged; no harness logic changed (covers
only observe).

## Status of recommendation #3

✅ Done. The nondet harnesses the review named (and the rest of the genuine
vacuity surface) now carry reachability checkpoints; CI fails if any goes
unreachable. The one wrinkle the rec did not anticipate — that Kani treats
covers as informational — is handled by the CI post-check and recorded as DN-13.
