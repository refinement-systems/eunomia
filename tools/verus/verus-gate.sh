#!/usr/bin/env bash
# Verus verified-count gate — cold-verify every gated crate and assert its
# verified-obligation count against the pinned row in
# tools/verus/verus-manifest.tsv (the machine-readable twin of the trusted-base
# ledger's ## Baselines section, doc/guidelines/verus_trusted-base.md).
#
# The Verus analogue of tools/tla/tla-assert-coverage.sh: a silently-dropped
# obligation — Verus proving *less* while still exiting 0 — fails here, in CI, not
# only in a local scripts/verus-baseline.sh run. Per row it runs a COLD
# `cargo clean -p <crate> && cargo verus verify -p <crate> <flags>`, so a stale
# target/ cannot false-green: the absence of the `verification results:: N
# verified, M errors` line is itself a hard failure (the exact stale-cache trap
# CLAUDE.md and doc/guidelines/verus.md warn about). N != the pin is a coverage
# regression; M != 0 is a proof failure.
#
# Requires the pinned Verus on PATH (cargo-verus + verus + z3, version
# 0.2026.06.07.cd03505 — see README "Prerequisites" / CLAUDE.md). Set VERUS_BIN_DIR
# to the directory holding cargo-verus if it is not already on PATH. Point
# MANIFEST elsewhere to gate a different table (the anti-vacuity self-check).
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MANIFEST="${MANIFEST:-$ROOT/tools/verus/verus-manifest.tsv}"
LOG_DIR="${LOG_DIR:-$ROOT/target/verus-gate}"

[ -f "$MANIFEST" ] || { echo "error: manifest not found: $MANIFEST" >&2; exit 1; }
[ -n "${VERUS_BIN_DIR:-}" ] && PATH="$VERUS_BIN_DIR:$PATH"
command -v cargo-verus >/dev/null || {
  echo "error: cargo-verus not on PATH (see README Prerequisites; or set VERUS_BIN_DIR)" >&2
  exit 1
}

mkdir -p "$LOG_DIR"

rc=0
# Read the manifest on fd 3 so the cargo invocations in the loop body cannot
# consume it via stdin. Split each row by hand rather than `IFS=$'\t' read`: tab
# is an IFS *whitespace* character, so read would collapse the adjacent tabs of
# an empty-flags row (`kcore<tab><tab>408`) into one and mis-shift the columns.
while IFS= read -r rawline <&3 || [ -n "$rawline" ]; do
  crate="${rawline%%$'\t'*}"
  # Skip blank lines, `#` comments, and the header row.
  case "$crate" in '' | \#* | crate) continue ;; esac
  rest="${rawline#*$'\t'}"    # everything after the first tab
  flags="${rest%%$'\t'*}"     # up to the next tab (empty for a plain crate)
  expected="${rest##*$'\t'}"  # after the last tab

  # An unpinned count would silently disarm the guard — fail loud (the
  # tla-assert-coverage.sh `-` rule).
  if [ "$expected" = "-" ] || [ -z "$expected" ]; then
    echo "GATE FAILED: $crate verified-count is unpinned ('-') in $MANIFEST — pin it to arm the guard" >&2
    rc=1
    continue
  fi

  log="$LOG_DIR/$crate.log"
  echo "== verifying $crate ${flags} (expect $expected) ==" >&2
  cargo clean -p "$crate" >"$log" 2>&1 || true # force a real (uncached) run
  # shellcheck disable=SC2086  # $flags is an intentional word-split flag list
  cargo verus verify -p "$crate" $flags >>"$log" 2>&1
  vrc=$?

  # The prover prints `verification results:: N verified, M errors` per verified
  # crate; cargo builds the -p target last, so its line is the final one.
  line="$(grep -E 'verification results:: [0-9]+ verified, [0-9]+ errors' "$log" | tail -1)"
  if [ -z "$line" ]; then
    echo "GATE FAILED: $crate — no 'verification results::' line (cargo exit $vrc; stale cache or build failure) — see $log" >&2
    rc=1
    continue
  fi
  verified="$(printf '%s\n' "$line" | sed -n 's/^verification results:: \([0-9]*\) verified.*/\1/p')"
  errors="$(printf '%s\n' "$line" | sed -n 's/^verification results:: [0-9]* verified, \([0-9]*\) errors.*/\1/p')"

  if [ "$errors" != "0" ]; then
    echo "GATE FAILED: $crate — $errors verification errors — see $log" >&2
    rc=1
    continue
  fi
  if [ "$verified" != "$expected" ]; then
    echo "COUNT REGRESSION: $crate verified $verified != expected $expected — reconcile tools/verus/verus-manifest.tsv with the ## Baselines ledger cell (doc/guidelines/verus_trusted-base.md) — see $log" >&2
    rc=1
    continue
  fi
  echo "ok: $crate $verified verified, 0 errors (pinned $expected)"
done 3<"$MANIFEST"

if [ "$rc" -eq 0 ]; then
  echo "verus gate: all crates match their pinned verified counts"
fi
exit "$rc"
