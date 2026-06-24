#!/usr/bin/env bash
# Establish a TLC model-checking baseline for every committed model/cfg.
#
#   scripts/tla-baseline.sh                       # baseline every cfg in the manifest
#   scripts/tla-baseline.sh IpcReactor CommitProtocol   # restrict to named cfgs
#   TLC_WORKERS=4 scripts/tla-baseline.sh         # faster (see determinism note)
#
# This is the TLC analogue of scripts/verus-baseline.sh and operationalises the
# "measure every change, correctness first" discipline (doc/plans/0_tla-
# optimization.md §1). For each cfg in tools/tla/model-manifest.tsv it does a
# COLD run (its TLC scratch is wiped first — a warm fingerprint/checkpoint set is
# the false-green equivalent of a stale Verus cache) at pinned flags and records:
#
#   * distinct states — the coverage metric (a drop means TLC proved LESS); it is
#     asserted against the manifest's expected value, so a coverage regression
#     fails the script the way a dropped Verus obligation fails the gate.
#   * generated states — work done; comparable only under identical workers/-fp.
#   * diameter — worker-invariant structural depth; also asserted.
#   * per-action -coverage — attributes generation cost to the hot disjunct
#     (e.g. RevokeStep/Copy/Send); the data that justifies an expression rewrite.
#   * wall-clock — advisory.
#
# Determinism: distinct and diameter are worker-invariant, so the assertions hold
# at any TLC_WORKERS. generated-states and any counterexample are nondeterministic
# at TLC_WORKERS>1; the default is 1 (fully deterministic) and -fp is pinned, so a
# generated-states comparison between two runs is only valid at the SAME workers.
#
# Knobs (env): TLC_WORKERS (default 1), TLC_FP (0), TLC_FPMEM (0.5), TLA_XMX (4g),
# OUT_DIR (target/tla-baseline), MANIFEST, TOP_N (8 hottest actions to list).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="${MANIFEST:-$ROOT/tools/tla/model-manifest.tsv}"
OUT_DIR="${OUT_DIR:-$ROOT/target/tla-baseline}"
WORKERS="${TLC_WORKERS:-1}"
FP="${TLC_FP:-0}"
FPMEM="${TLC_FPMEM:-0.5}"
XMX="${TLA_XMX:-4g}"
TOP_N="${TOP_N:-8}"

[ -f "$MANIFEST" ] || { echo "error: manifest not found: $MANIFEST" >&2; exit 1; }

# Confirm the vendored jar matches its pin — a baseline from a different jar is
# not comparable to CI (the §1 pinned-toolchain control).
if ! ( cd "$ROOT/tools/tla" && shasum -c tla2tools.jar.sha1 ) >/dev/null 2>&1; then
  echo "warning: tools/tla/tla2tools.jar does not match its .sha1 — baseline not comparable to CI" >&2
fi

# Restrict to named cfgs if any positional args are given.
SELECT=("$@")
selected() {
  [ "${#SELECT[@]}" -eq 0 ] && return 0
  local s; for s in "${SELECT[@]}"; do [ "$s" = "$1" ] && return 0; done
  return 1
}

mkdir -p "$OUT_DIR"
SUMMARY="$OUT_DIR/summary.txt"
: > "$SUMMARY"
{
  echo "# TLC model-checking baseline"
  echo "# host $(uname -sm), workers=$WORKERS fp=$FP fpmem=$FPMEM Xmx=$XMX"
  echo "# distinct = coverage metric (a drop = proved less); diameter = worker-invariant depth"
  printf '%-26s %10s %12s %9s %5s  %s\n' cfg distinct generated gen:dist diam verdict
} | tee -a "$SUMMARY"

rc=0
while IFS=$'\t' read -r name spec cfg exp_distinct exp_diameter; do
  case "$name" in ''|\#*|name) continue ;; esac
  selected "$name" || continue

  log="$OUT_DIR/$name.log"
  metadir="$ROOT/target/tla-states/$name"
  rm -rf "$metadir"   # cold run — no warm fingerprint set

  TLC_WORKERS="$WORKERS" \
  TLC_METADIR="$metadir" \
  TLA_JAVA_OPTS="-Xmx$XMX" \
  TLC_FLAGS="-fp $FP -fpmem $FPMEM -coverage 1" \
    bash "$ROOT/tools/tla/tla-model-check.sh" "$ROOT/$spec" "$cfg" </dev/null >"$log" 2>&1
  trc=$?

  stats="$(sed -n 's/^\([0-9][0-9,]*\) states generated, \([0-9][0-9,]*\) distinct states found.*/\1 \2/p' "$log" | tail -1 | tr -d ,)"
  generated="?"; distinct="?"
  [ -n "$stats" ] && { generated="${stats%% *}"; distinct="${stats##* }"; }
  diameter="$(sed -n 's/^The depth of the complete state graph search is \([0-9]*\).*/\1/p' "$log" | tail -1)"
  diameter="${diameter:-?}"
  wall="$(sed -n 's/^Finished in \(.*\) at .*/\1/p' "$log" | tail -1)"
  wall="${wall:-?}"

  ratio="-"
  [ "$distinct" != "?" ] && [ "$distinct" -gt 0 ] 2>/dev/null \
    && ratio="$(LC_NUMERIC=C awk -v g="$generated" -v d="$distinct" 'BEGIN{printf "%.1f", g/d}')"

  verdict="ok"
  if [ "$trc" -ne 0 ]; then verdict="RUN FAILED (exit $trc) — see $log"; rc=1; fi
  if [ "$exp_distinct" != "-" ] && [ "$distinct" != "$exp_distinct" ]; then
    verdict="COVERAGE REGRESSION: distinct $distinct != expected $exp_distinct"; rc=1
  elif [ "$exp_diameter" != "-" ] && [ "$diameter" != "$exp_diameter" ]; then
    verdict="DIAMETER CHANGED: $diameter != expected $exp_diameter"; rc=1
  elif [ "$exp_distinct" = "-" ] && [ "$verdict" = "ok" ]; then
    verdict="ok (unpinned — record $distinct/$diameter in manifest to arm guard)"
  fi

  printf '%-26s %10s %12s %9s %5s  %s\n' "$name" "$distinct" "$generated" "$ratio" "$diameter" "$verdict" | tee -a "$SUMMARY"

  # Hottest actions from -coverage: lines "<Action ...>: a:b"; rank by b
  # (the larger count). See https://explain.tlapl.us/module-coverage-statistics.
  {
    echo "  top $TOP_N actions by coverage count (\"<action>: a:b\"), wall ${wall}:"
    if ! grep -oE '^<[A-Za-z][^>]*>: [0-9]+:[0-9]+$' "$log" | sort -t: -k3 -rn | head -n "$TOP_N" | sed 's/^/    /'; then
      echo "    (no per-action coverage parsed — was -coverage on?)"
    fi
  } | tee -a "$SUMMARY"
done < "$MANIFEST"

echo "baseline written to $OUT_DIR (per-cfg <name>.log + summary.txt)" | tee -a "$SUMMARY" >&2
exit $rc
