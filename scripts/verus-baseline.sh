#!/usr/bin/env bash
# Establish a Verus verification *timing* baseline for every gated crate.
#
#   scripts/verus-baseline.sh                 # baseline all crates (the CI gate set)
#   scripts/verus-baseline.sh kcore cas       # restrict to named crates
#   OUT_DIR=/path NO_CLEAN=1 scripts/verus-baseline.sh   # see knobs below
#
# For each crate it runs `cargo verus verify -p <crate> -- --time-expanded
# --output-json`: `--time-expanded` adds the per-module / per-function timing
# breakdown, `--output-json` emits it as JSON on stdout (notes and cargo's own
# lines go to stderr, so stdout is pure JSON). The raw JSON is the authoritative
# baseline artifact; a human-readable summary table is printed and saved too.
#
# Verus caches verification per build, so a re-run over an unchanged target/ can
# exit 0 *without re-verifying* — and report no timing at all (doc/guidelines/
# verus.md, "Scoped runs can false-green from stale cache"). To measure a real,
# cold verification this script `cargo clean -p <crate>` before each crate. Set
# NO_CLEAN=1 to skip that (faster, but a cached crate yields no fresh timing).
#
# Requires the pinned Verus on PATH (see README "Prerequisites" / CLAUDE.md
# "Verus verification"): cargo-verus + verus + z3, version 0.2026.06.07.cd03505,
# Rust toolchain 1.95.0. If your install is not on PATH, point VERUS_BIN_DIR at
# the directory holding cargo-verus. jq is required for the summary.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXPECT_VERSION="0.2026.06.07.cd03505"
OUT_DIR="${OUT_DIR:-$ROOT/target/verus-baseline}"
TOP_N="${TOP_N:-8}"               # slowest functions to list per crate
NO_CLEAN="${NO_CLEAN:-0}"

# The CI gate set, in CI order (.github/workflows/ci.yml `verus` job). cas and
# storage-server are Vec/serde-heavy, so their feature-agnostic verified cores
# verify in the no_std+alloc variant (--no-default-features); storage-server also
# scopes to --lib (its placeholder bin carries no proofs). loader's verified core
# (page_layout) is no_std + no-alloc, so it too verifies under --no-default-features.
ALL_CRATES=(kcore ipc urt freelist dma-pool cas virtio-blk storage-server loader)
verus_args_for() {
  case "$1" in
    cas) echo "--no-default-features" ;;
    storage-server) echo "--no-default-features --lib" ;;
    loader) echo "--no-default-features" ;;
    *) echo "" ;;
  esac
}

[ -n "${VERUS_BIN_DIR:-}" ] && PATH="$VERUS_BIN_DIR:$PATH"
command -v cargo-verus >/dev/null || { echo "error: cargo-verus not on PATH (see README Prerequisites; or set VERUS_BIN_DIR)" >&2; exit 1; }
command -v jq          >/dev/null || { echo "error: jq not found (needed for the summary)" >&2; exit 1; }

# Confirm the binary matches the pin — a baseline from the wrong build is noise.
got_version="$(verus --version 2>/dev/null | sed -n 's/^[[:space:]]*Version:[[:space:]]*//p')"
if [ "$got_version" != "$EXPECT_VERSION" ]; then
  echo "warning: verus version '$got_version' != pinned '$EXPECT_VERSION' — timings not comparable to CI" >&2
fi

# Crates to run: positional args, else the whole gate set.
if [ "$#" -gt 0 ]; then CRATES=("$@"); else CRATES=("${ALL_CRATES[@]}"); fi

mkdir -p "$OUT_DIR"
SUMMARY="$OUT_DIR/summary.txt"
: > "$SUMMARY"
{
  echo "# Verus verification timing baseline"
  echo "# verus $got_version, host $(uname -sm), $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "# total = verus wall time; verify = verification phase; smt = SMT cpu summed over threads"
  printf '%-12s %5s %5s %9s %9s %9s\n' crate verif errs total/ms verify/ms smt/ms
} | tee -a "$SUMMARY"

rc=0
for c in "${CRATES[@]}"; do
  json="$OUT_DIR/$c.json"
  log="$OUT_DIR/$c.log"
  extra="$(verus_args_for "$c")"
  echo "== verifying $c ${extra} ==" >&2

  if [ "$NO_CLEAN" != "1" ]; then
    cargo clean -p "$c" 2>>"$log" || true   # force a real (uncached) run
  fi

  start=$SECONDS
  # shellcheck disable=SC2086  # $extra is an intentional word-split flag list
  cargo verus verify -p "$c" $extra -- --time-expanded --output-json >"$json" 2>"$log"
  vrc=$?
  elapsed=$((SECONDS - start))

  if ! jq empty "$json" >/dev/null 2>&1; then
    echo "  !! $c: no parseable JSON (exit $vrc) — see $log" | tee -a "$SUMMARY" >&2
    rc=1
    continue
  fi

  read -r verified errors success <<EOF
$(jq -r '[.["verification-results"] | .verified, .errors, .success] | @tsv' "$json")
EOF
  read -r total verify smt threads <<EOF
$(jq -r '[.["times-ms"] | .total, .verification.total, .smt.total, .["num-threads"]] | @tsv' "$json")
EOF
  printf '%-12s %5s %5s %9s %9s %9s\n' "$c" "$verified" "$errors" "$total" "$verify" "$smt" | tee -a "$SUMMARY"
  [ "$success" = "true" ] || { echo "  !! $c: verification FAILED ($errors errors) — see $log" | tee -a "$SUMMARY" >&2; rc=1; }

  # Slowest functions (the actual profiling payload of --time-expanded).
  {
    echo "  top $TOP_N slowest (ms / rlimit / mode / fn), ${threads} threads, ${elapsed}s wall:"
    jq -r --argjson n "$TOP_N" '
      [ .["times-ms"].smt["smt-run-module-times"][]?["function-breakdown"][]? ]
      | sort_by(-.time) | .[0:$n][]
      | "    \(.time)\t\(.rlimit)\t\(.["mode:"])\t\(.function)"' "$json"
  } | tee -a "$SUMMARY"
done

echo "baseline written to $OUT_DIR (per-crate <crate>.json + summary.txt)" | tee -a "$SUMMARY" >&2
exit $rc
