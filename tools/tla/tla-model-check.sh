#!/bin/bash

# Permission to use, copy, modify, and/or distribute this software for
# any purpose with or without fee is hereby granted.
#
# THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
# WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
# OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
# FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
# DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
# AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
# OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

# Run TLC model checker on a .tla file.
# Usage: tla-model-check.sh <spec.tla> [config.cfg]
# Defaults config to <spec-basename>.cfg in the same directory as the spec.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=find-tla-tools.sh
source "$SCRIPT_DIR/find-tla-tools.sh"

SPEC="${1:-}"
if [ -z "$SPEC" ]; then
    echo "Usage: $(basename "$0") <spec.tla> [config.cfg]" >&2
    exit 1
fi

SPEC_ABS="$(cd "$(dirname "$SPEC")" && pwd)/$(basename "$SPEC")"
SPEC_DIR="$(dirname "$SPEC_ABS")"
SPEC_BASE="$(basename "$SPEC_ABS")"
CFG="${2:-${SPEC_BASE%.tla}.cfg}"

# Optional knobs (all wrap the *identical* state graph and properties — they
# change only parallelism, resourcing, instrumentation, and scratch location):
#   TLA_JAVA_OPTS : JVM args placed before -cp (e.g. -Xmx4g for the heap-bound
#                   liveness tableau).
#   TLC_WORKERS   : worker threads (default auto). CI and the baseline harness
#                   pin a fixed K for reproducibility; auto floats with host cores.
#   TLC_FLAGS     : extra TLC flags after the class (e.g. -coverage 1 -fp 0
#                   -fpmem 0.5).
#   TLC_METADIR   : TLC scratch dir; defaults under the repo target/ so the
#                   state graph + checkpoints never accumulate inside tla/.
#   TLC_ASSERT_MANIFEST : when set, after a clean run assert this cfg's
#                   distinct-states (the coverage metric) against its pinned row
#                   in model-manifest.tsv (via tla-assert-coverage.sh) — so a
#                   silent coverage shrink fails here, not just a local baseline
#                   run. Set per-arm in CI; left unset everywhere else (the run is
#                   then byte-identical to having no hook).
#
# Determinism caveat: with TLC_WORKERS>1 the generated-state count and the
# reported counterexample are nondeterministic (threads interleave expansion);
# distinct-states stay worker-invariant (the diameter too at the pinned low worker
# counts, though TLC's reported depth can wobble ±1 under high -workers — which is
# why TLC_ASSERT_MANIFEST gates on distinct, not diameter). Pin TLC_WORKERS and
# -fp 0 (via TLC_FLAGS) for an A/B-comparable run.
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TLC_METADIR="${TLC_METADIR:-$REPO_ROOT/target/tla-states/$(basename "${CFG%.cfg}")}"
mkdir -p "$TLC_METADIR"

echo "Model checking: $SPEC_BASE (config: $CFG, workers: ${TLC_WORKERS:-auto})"
cd "$SPEC_DIR"
# -noGenerateSpecTE: on a violation TLC otherwise writes a trace-exploration
# spec (*_TTrace_*.{tla,bin}) into the cwd — i.e. the spec dir under tla/, which
# -metadir does not cover — littering the source tree (the negative controls trip
# one on every run). The full counterexample is still printed below, so the TE
# spec adds nothing here; suppress it so no scratch lands in tla/.
echo "+ $JAVA ${TLA_JAVA_OPTS:-} -cp $TLA_TOOLS tlc2.TLC -workers ${TLC_WORKERS:-auto} -metadir $TLC_METADIR -noGenerateSpecTE ${TLC_FLAGS:-} -config $CFG $SPEC_BASE"
run_tlc() {
    # shellcheck disable=SC2086  # word-splitting of the flag lists is intentional
    "$JAVA" ${TLA_JAVA_OPTS:-} -cp "$TLA_TOOLS" tlc2.TLC \
        -workers "${TLC_WORKERS:-auto}" -metadir "$TLC_METADIR" -noGenerateSpecTE ${TLC_FLAGS:-} \
        -config "$CFG" "$SPEC_BASE"
}

if [ -n "${TLC_ASSERT_MANIFEST:-}" ]; then
    # Capture the run while still streaming it live (tee), then assert its
    # distinct-states against the manifest pin (diameter advisory — see the
    # asserter). pipefail makes the pipeline carry TLC's exit status, so a *failed*
    # run aborts here under set -e before we assert — a failed check has no
    # coverage to pin; only a clean run is asserted, where a distinct drift makes
    # the asserter exit non-zero and fail the step.
    runlog="$TLC_METADIR/tlc-run.log"
    run_tlc 2>&1 | tee "$runlog"
    bash "$SCRIPT_DIR/tla-assert-coverage.sh" "$CFG" "$runlog"
else
    run_tlc
fi
