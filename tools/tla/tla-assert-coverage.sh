#!/bin/bash
# SPDX-License-Identifier: 0BSD

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

# Assert a finished TLC run's distinct-states against the pinned row in the
# coverage ledger tools/tla/model-manifest.tsv.
# Usage: tla-assert-coverage.sh <config.cfg> <tlc-output-log>
#
# tla-model-check.sh calls this when TLC_ASSERT_MANIFEST is set, so a silent
# coverage SHRINK (TLC explores fewer states — proves LESS — while the verdict is
# unchanged) fails CI here, not only a local scripts/tla-baseline.sh run.
#
# distinct-states is the coverage metric — the reachable-set size, deterministic
# and independent of -workers/-fp/-lncheck — so it is the HARD gate. diameter is
# the structural depth: exact at a fixed low worker count (scripts/tla-baseline.sh
# at -workers 1 hard-asserts it), but TLC's reported "depth of the complete state
# graph search" can wobble ±1 under high -workers concurrency, so on this
# multi-worker path a diameter mismatch is advisory, not fatal — the distinct gate
# already catches any real shrink.
#
# The log parse below is kept in lock-step with scripts/tla-baseline.sh (which
# reads the same two TLC summary lines for its fuller, multi-field report).
set -euo pipefail

CFG_ARG="${1:-}"
LOG="${2:-}"
if [ -z "$CFG_ARG" ] || [ -z "$LOG" ]; then
    echo "Usage: $(basename "$0") <config.cfg> <tlc-output-log>" >&2
    exit 1
fi
CFG="$(basename "$CFG_ARG")"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
MANIFEST="${MANIFEST:-$SCRIPT_DIR/model-manifest.tsv}"
[ -f "$MANIFEST" ] || { echo "error: manifest not found: $MANIFEST" >&2; exit 1; }
[ -f "$LOG" ] || { echo "error: TLC log not found: $LOG" >&2; exit 1; }

# Pinned row, keyed by cfg basename (column 3); columns 4/5 are the expected
# distinct/diameter. The flag asks for an assertion, so a cfg with no row — or one
# whose distinct (the coverage metric) is unpinned (`-`) — is a fail-loud
# misconfiguration, not a quiet pass: it would otherwise disarm the guard.
row="$(awk -F'\t' -v c="$CFG" '$3==c {print $4"|"$5; exit}' "$MANIFEST")"
if [ -z "$row" ]; then
    echo "COVERAGE ASSERT FAILED: $CFG has no row in $MANIFEST (TLC_ASSERT_MANIFEST set but nothing to assert)" >&2
    exit 1
fi
exp_distinct="${row%%|*}"
exp_diameter="${row##*|}"
if [ "$exp_distinct" = "-" ]; then
    echo "COVERAGE ASSERT FAILED: $CFG distinct is unpinned ('-') in $MANIFEST — pin it to arm the CI guard" >&2
    exit 1
fi

distinct="$(sed -n 's/^\([0-9][0-9,]*\) states generated, \([0-9][0-9,]*\) distinct states found.*/\2/p' "$LOG" | tail -1 | tr -d ,)"
diameter="$(sed -n 's/^The depth of the complete state graph search is \([0-9]*\).*/\1/p' "$LOG" | tail -1)"

# distinct — the hard gate.
if [ -z "$distinct" ]; then
    echo "COVERAGE ASSERT FAILED: $CFG — no 'distinct states found' line in $LOG" >&2; exit 1
fi
if [ "$distinct" != "$exp_distinct" ]; then
    echo "COVERAGE REGRESSION: $CFG distinct $distinct != expected $exp_distinct" >&2; exit 1
fi

# diameter — advisory on this multi-worker path (see header); a mismatch warns but
# does not fail, since distinct already proved coverage is intact.
if [ "$exp_diameter" != "-" ] && [ -n "$diameter" ] && [ "$diameter" != "$exp_diameter" ]; then
    echo "warning: $CFG diameter $diameter != pinned $exp_diameter — TLC's reported depth can vary under high -workers; re-confirm with scripts/tla-baseline.sh (-workers 1). Distinct matched, so coverage is intact." >&2
fi

echo "coverage ok: $CFG distinct=$distinct (pinned $exp_distinct); diameter=${diameter:--} (pinned $exp_diameter)"
