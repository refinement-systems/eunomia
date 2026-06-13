#!/usr/bin/env bash
# Deep, off-CI verification supplements for the kcore object core.
#
#   ⚠️  HEAVY — RUN SPARINGLY.  These are NOT part of `cargo test`, the CI
#   suite, or the per-PR Kani job. They are the "more exhaustive" checks
#   recommended in doc/results/14_kani-review-2.md, deliberately kept off the
#   pinned path because they take minutes to hours. Run them by hand before a
#   release, after touching the cspace/CDT machinery, or when investigating a
#   suspected composition bug — not routinely.
#
# Two independent supplements, selected by the first argument:
#
#   replay   The "mini-TLC": an exhaustive plain-Rust enumeration of EVERY
#            sequence of CDT ops (derive/move/delete/revoke) up to a bounded
#            length, asserting cdt_wf + the refcount census after each step.
#            This is the multi-op composition coverage CBMC OOMs on (DN-12) and
#            the only check that exercises `revoke` over all reachable shapes.
#            Depth via EXHAUSTIVE_DEPTH (script default 5 ≈ 100M sequences,
#            ~15 s release). Depth 6 ≈ 4B sequences ≈ tens of minutes.
#
#   kani     The additive transition harness re-run at a DEEPER op-sequence
#            length (K 3→4) via the KANI_DEEP compile knob — the "raise K toward
#            4–6" of review rec. #2, kept off CI because K=3 is already at the
#            ~5-min per-harness budget. K=4 derive/move may take tens of minutes
#            or OOM; that is expected off-CI. Widening the OBJECT-count bounds
#            (POOL_SLOTS etc.) is a separate MANUAL edit — see bounds.rs (the
#            `#[kani::unwind]` literals must be bumped in lockstep), not this
#            toggle. The concrete `check_revoke` and the World/channel/notif/
#            aspace families keep fixed bounds by design.
#
#   all      replay then kani (default).
#
# Env knobs:
#   EXHAUSTIVE_DEPTH   replay sequence length (default 5)
#   DEEP_TIMEOUT       per-harness wall cap in seconds for `kani` (default 2400)
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-all}"
EXHAUSTIVE_DEPTH="${EXHAUSTIVE_DEPTH:-5}"
DEEP_TIMEOUT="${DEEP_TIMEOUT:-2400}"

banner() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

run_replay() {
  banner "mini-TLC exhaustive CDT replay (depth=${EXHAUSTIVE_DEPTH})"
  echo "Enumerating all derive/move/delete/revoke sequences; asserting"
  echo "cdt_wf + refcount census after every step. Release build for speed."
  EXHAUSTIVE_DEPTH="$EXHAUSTIVE_DEPTH" \
    cargo test -p kcore --release exhaustive_cdt_replay -- --ignored --nocapture
}

run_kani() {
  banner "deep Kani — transition harness at K=4 (KANI_DEEP=1)"
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

  # Only the additive transition harness scales SAFELY on the env toggle (K
  # 3→4; its unwind(6) still covers the K=4 loop and the unchanged census
  # scans). The object-count bounds need the manual edit described in the
  # header / bounds.rs, so widening the structural CDT harnesses is not done
  # here — it would only trip their fixed unwind literals.
  banner "deep kani: check_cdt_transition_system (K=4)"
  echo "NOT run here (need manual bounds + unwind edit): the structural CDT"
  echo "harnesses at POOL_SLOTS>4, check_revoke, and the World families."
  if KANI_DEEP=1 "${TO[@]}" cargo kani -Z stubbing -p kcore \
        --harness check_cdt_transition_system; then
    echo "[check_cdt_transition_system @K=4] OK"
    return 0
  else
    echo "::warning:: [check_cdt_transition_system @K=4] FAILED / TIMED OUT / OOM" \
         "— expected off-CI at K=4; this is why CI stays at K=3"
    return 1
  fi
}

case "$MODE" in
  replay) run_replay ;;
  kani)   run_kani ;;
  all)    run_replay; run_kani || true ;;
  *) echo "usage: $0 [replay|kani|all]" >&2; exit 2 ;;
esac

banner "deep-verify done"
