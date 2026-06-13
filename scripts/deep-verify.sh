#!/usr/bin/env bash
# Deep, off-CI verification supplements for the kcore object core.
#
#   ⚠️  HEAVY — RUN SPARINGLY.  These are NOT the per-PR path. CI runs the
#   replays at a cheap depth (host-tests) and the Kani suite at TLC-scale
#   bounds; this script runs the replays MUCH deeper and the Kani harnesses at
#   WIDENED bounds. It is the "more exhaustive" tier recommended in
#   doc/results/14_kani-review-2.md — run by hand before a release, after
#   touching the cspace/CDT machinery, or from the scheduled kani-deep workflow.
#
# Two independent supplements, selected by the first argument:
#
#   replay   The "mini-TLC" host tests (kcore::proofs::exhaustive): exhaustive
#            plain-Rust enumeration of EVERY CDT op sequence
#            (derive/move/delete/revoke), asserting cdt_wf + the refcount census
#            (+ chan_wf) after each step. Two tests:
#              - exhaustive_cdt_replay        — BarePool, all reachable trees
#                                               (EXHAUSTIVE_DEPTH, default 5
#                                               ≈ 100M seqs, ~15 s release)
#              - exhaustive_cross_home_replay — World; revoke seen through a
#                                               channel ring slot AND a TCB bind
#                                               slot (CROSS_HOME_DEPTH, default 4
#                                               ≈ 13M seqs, ~21 s release)
#            This is the multi-op composition coverage CBMC OOMs on (DN-12) and
#            the only place `revoke` is checked over all reachable shapes.
#
#   kani     The composition/inductive CDT harnesses re-run at WIDENED bounds
#            via the `kani_deep` cargo feature (POOL_SLOTS 4→6, transition K
#            3→4; the matching #[kani::unwind] literals switch via cfg_attr):
#              - check_cdt_transition_system   (additive K-step, now K=4 over 6)
#              - check_delete_step             (inductive delete over 6 slots)
#            Tens of minutes or OOM each — expected off-CI; that is why CI keeps
#            the TLC-scale bounds. Only these two carry the cfg_attr unwind, so
#            only these two are run under the feature. The concrete check_revoke
#            and the World/channel/notif/aspace families keep fixed bounds by
#            design (a wider bound only slows a concrete scenario).
#
#   all      replay then kani (default).
#
# Env knobs:
#   EXHAUSTIVE_DEPTH   BarePool replay length      (default 5)
#   CROSS_HOME_DEPTH   cross-home replay length    (default 4)
#   DEEP_TIMEOUT       per-harness wall cap (s) for `kani` (default 2400)
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-all}"
EXHAUSTIVE_DEPTH="${EXHAUSTIVE_DEPTH:-5}"
CROSS_HOME_DEPTH="${CROSS_HOME_DEPTH:-4}"
DEEP_TIMEOUT="${DEEP_TIMEOUT:-2400}"

banner() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

run_replay() {
  banner "mini-TLC exhaustive replays (BarePool depth=${EXHAUSTIVE_DEPTH}, cross-home depth=${CROSS_HOME_DEPTH})"
  echo "Enumerating all derive/move/delete/revoke sequences; asserting cdt_wf +"
  echo "refcount census (+ chan_wf) after every step. Release build for speed."
  EXHAUSTIVE_DEPTH="$EXHAUSTIVE_DEPTH" CROSS_HOME_DEPTH="$CROSS_HOME_DEPTH" \
    cargo test -p kcore --release exhaustive -- --ignored --nocapture
}

run_kani() {
  banner "deep Kani — composition CDT harnesses at widened bounds (--features kani_deep)"
  if ! command -v cargo-kani >/dev/null 2>&1; then
    echo "cargo-kani not installed (pin: 0.67.0). See CLAUDE.md. Skipping." >&2
    return 0
  fi
  # macOS Bash-tool / interactive runs do not reap detached CBMC solver
  # children on timeout — reap them on any exit (CLAUDE.md operational note).
  trap 'pkill -9 cbmc kissat cadical 2>/dev/null || true' EXIT INT TERM

  # Per-harness wall cap, if a `timeout`-alike is available (coreutils
  # `timeout`, or `gtimeout` from Homebrew coreutils on macOS).
  local TO=()
  if command -v timeout >/dev/null 2>&1; then TO=(timeout "$DEEP_TIMEOUT")
  elif command -v gtimeout >/dev/null 2>&1; then TO=(gtimeout "$DEEP_TIMEOUT")
  else echo "(no timeout(1) found — running without a per-harness wall cap)"; fi

  # Only these two carry the cfg_attr unwind that tracks POOL_SLOTS=6, so only
  # these are sound to verify under the feature (others keep unwind(6)).
  local harnesses=(check_cdt_transition_system check_delete_step)
  echo "Widened: POOL_SLOTS=6, transition K=4. Running: ${harnesses[*]}"
  echo "NOT run (fixed bounds by design): check_revoke, the structural single-op"
  echo "CDT harnesses, and the World/channel/notification/aspace families."

  local fail=0
  for h in "${harnesses[@]}"; do
    banner "deep kani: $h (kani_deep)"
    if "${TO[@]}" cargo kani --features kani_deep -Z stubbing \
          -p kcore --harness "$h"; then
      echo "[$h] OK"
    else
      echo "::warning:: [$h] FAILED / TIMED OUT / OOM at the widened bound" \
           "— expected off-CI; this is why CI keeps TLC-scale bounds"
      fail=1
    fi
  done
  return "$fail"
}

case "$MODE" in
  replay) run_replay ;;
  kani)   run_kani ;;
  all)    run_replay; run_kani || true ;;
  *) echo "usage: $0 [replay|kani|all]" >&2; exit 2 ;;
esac

banner "deep-verify done"
