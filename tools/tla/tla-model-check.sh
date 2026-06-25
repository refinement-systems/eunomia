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
#
# Determinism caveat: with TLC_WORKERS>1 the generated-state count and the
# reported counterexample are nondeterministic (threads interleave expansion);
# distinct-states and the diameter stay worker-invariant. Pin TLC_WORKERS and
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
# shellcheck disable=SC2086  # word-splitting of the flag lists is intentional
"$JAVA" ${TLA_JAVA_OPTS:-} -cp "$TLA_TOOLS" tlc2.TLC \
    -workers "${TLC_WORKERS:-auto}" -metadir "$TLC_METADIR" -noGenerateSpecTE ${TLC_FLAGS:-} \
    -config "$CFG" "$SPEC_BASE"
