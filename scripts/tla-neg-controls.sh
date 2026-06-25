#!/usr/bin/env bash
# Run the committed negative-control cfgs and assert each FAILS as designed.
#
#   scripts/tla-neg-controls.sh [safety|liveness|all]
#
# Each control is a deliberately-broken spec variant paired with the invariant /
# property it must violate — the runnable proof that a load-bearing guard has
# teeth. They are also the standing
# soundness monitor for any future SYMMETRY: TLC never validates a symmetry
# itself, so a mis-scoped one that silently hides bugs is caught here when a
# control it should still trip stops tripping. A control that PASSES (TLC exits
# 0, no error) is therefore the failure this script reports.
#
# For each: assert TLC exits non-zero AND reports a violation; best-effort-check
# that the expected invariant/property is the one named (liveness violations
# print a generic "Temporal properties were violated", so a missing name warns
# rather than fails). Exits 0 only if all selected controls failed as expected.
#
# The optional argument selects controls by KIND (default `all`):
#   safety   — invariant/property controls; each trips on a short counterexample,
#              so they are cheap and run single-worker for a deterministic trace.
#   liveness — the temporal-property controls (EventuallyRevoked). They MUST keep
#              TLC's default periodic liveness checking — never `-lncheck final`,
#              which on a failing spec forfeits the early-exit and balloons the
#              livelock detection — and one explores a 5-cap reachable set, so
#              they are the suite's slowest checks. CI runs them in their own
#              parallel job, off the serial `model` job, at a higher worker count.
#   all      — both; keeps the whole suite together for a local re-derivation.
# TLC_WORKERS and TLA_JAVA_OPTS are honoured from the environment (default 1 /
# -Xmx2g) so that parallel job can raise them without editing this script.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/target/tla-neg-controls}"
mkdir -p "$OUT_DIR"

KIND="${1:-all}"
case "$KIND" in
  safety | liveness | all) ;;
  *)
    echo "usage: $(basename "$0") [safety|liveness|all]" >&2
    exit 2
    ;;
esac

# Worker count / heap are overridable from the environment so the separate
# liveness CI job can raise them; the defaults keep the safety controls
# single-worker (the counterexample is short and we only need a clean verdict).
WORKERS="${TLC_WORKERS:-1}"
JAVA_OPTS="${TLA_JAVA_OPTS:--Xmx2g}"

# spec_relpath | cfg | expected violated invariant/property | kind
CONTROLS=(
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_NegControl.cfg|LiveParent|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_Safety_NegControl.cfg|LiveParent|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_AsymBug.cfg|DeadNowhere|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_CapAsymBug.cfg|DeadNowhere|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_ThreadAsymBug.cfg|FireSafe|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_ReportMonotoneBad.cfg|ReportMonotone|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_MoveSemanticsBad.cfg|MoveSemantics|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_RevokedDeadBad.cfg|RevokedDead|safety"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_NegLiveness.cfg|EventuallyRevoked|liveness"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_NegFairness.cfg|EventuallyRevoked|liveness"
  "tla/commit_protocol/CommitProtocol.tla|CommitProtocol_NegControl.cfg|RecoverReconstructs|safety"
  "tla/commit_protocol/CommitProtocol.tla|CommitProtocol_AsymBug.cfg|RecoverReconstructs|safety"
  "tla/ipc_reactor/IpcReactor.tla|IpcReactor_NegControl.cfg|NoLostWakeup|safety"
  "tla/ipc_reactor/IpcReactor.tla|IpcReactor_NegBackpressure.cfg|NoLostWakeupWritable|safety"
  "tla/ipc_reactor/IpcReactor.tla|IpcReactor_NegLostWakeup.cfg|NoLostWakeup|safety"
)

rc=0
ran=0
for entry in "${CONTROLS[@]}"; do
  IFS='|' read -r spec cfg want kind <<<"$entry"
  if [ "$KIND" != "all" ] && [ "$kind" != "$KIND" ]; then
    continue
  fi
  ran=$((ran + 1))
  base="${cfg%.cfg}"
  log="$OUT_DIR/$base.log"
  metadir="$ROOT/target/tla-states/neg-$base"
  rm -rf "$metadir"

  TLC_WORKERS="$WORKERS" TLC_METADIR="$metadir" TLA_JAVA_OPTS="$JAVA_OPTS" \
    bash "$ROOT/tools/tla/tla-model-check.sh" "$ROOT/$spec" "$cfg" </dev/null >"$log" 2>&1
  trc=$?

  if [ "$trc" -eq 0 ]; then
    printf '  FAIL  %-34s expected %s violated, but TLC found NO error (exit 0)\n' "$cfg" "$want"
    rc=1
  elif grep -qE '(is|are|was|were) violated' "$log"; then
    if grep -q "$want" "$log"; then
      printf '  ok    %-34s %s violated as expected (exit %s)\n' "$cfg" "$want" "$trc"
    else
      printf '  ok*   %-34s violation found (exit %s) but %s not named — check %s\n' "$cfg" "$trc" "$want" "$log"
    fi
  else
    printf '  FAIL  %-34s exited %s but reported no violation (tooling error?) — see %s\n' "$cfg" "$trc" "$log"
    rc=1
  fi
done

if [ "$rc" -eq 0 ]; then
  echo "all $ran negative controls ($KIND) failed as designed"
else
  echo "ERROR: a negative control did not behave as designed — a soundness guard has rotted" >&2
fi
exit $rc
