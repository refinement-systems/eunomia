#!/usr/bin/env bash
# Run the committed negative-control cfgs and assert each FAILS as designed.
#
#   scripts/tla-neg-controls.sh
#
# Each control is a deliberately-broken spec variant paired with the invariant /
# property it must violate — the runnable proof that a load-bearing guard has
# teeth (doc/plans/0_tla-optimization.md §2 A7). They are also the standing
# soundness monitor for any future SYMMETRY: TLC never validates a symmetry
# itself, so a mis-scoped one that silently hides bugs is caught here when a
# control it should still trip stops tripping. A control that PASSES (TLC exits
# 0, no error) is therefore the failure this script reports.
#
# For each: assert TLC exits non-zero AND reports a violation; best-effort-check
# that the expected invariant/property is the one named (liveness violations
# print a generic "Temporal properties were violated", so a missing name warns
# rather than fails). Exits 0 only if all controls failed as expected.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/target/tla-neg-controls}"
mkdir -p "$OUT_DIR"

# spec_relpath | cfg | expected violated invariant/property
CONTROLS=(
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_NegControl.cfg|LiveParent"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_Safety_NegControl.cfg|LiveParent"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_AsymBug.cfg|DeadNowhere"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_CapAsymBug.cfg|DeadNowhere"
  "tla/cap_revocation/CapRevocation.tla|CapRevocation_NegLiveness.cfg|EventuallyRevoked"
  "tla/commit_protocol/CommitProtocol.tla|CommitProtocol_NegControl.cfg|RecoverReconstructs"
  "tla/ipc_reactor/IpcReactor.tla|IpcReactor_NegControl.cfg|NoLostWakeup"
  "tla/ipc_reactor/IpcReactor.tla|IpcReactor_NegBackpressure.cfg|NoLostWakeupWritable"
  "tla/ipc_reactor/IpcReactor.tla|IpcReactor_NegLostWakeup.cfg|NoLostWakeup"
)

rc=0
for entry in "${CONTROLS[@]}"; do
  IFS='|' read -r spec cfg want <<<"$entry"
  base="${cfg%.cfg}"
  log="$OUT_DIR/$base.log"
  metadir="$ROOT/target/tla-states/neg-$base"
  rm -rf "$metadir"

  # Single worker: the counterexample is short and we only need a clean verdict.
  TLC_WORKERS=1 TLC_METADIR="$metadir" TLA_JAVA_OPTS="-Xmx2g" \
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
  echo "all ${#CONTROLS[@]} negative controls failed as designed"
else
  echo "ERROR: a negative control did not behave as designed — a soundness guard has rotted" >&2
fi
exit $rc
