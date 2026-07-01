#!/usr/bin/env bash
# Drive the cargo-fuzz harnesses across every fuzz crate.
#
#   scripts/fuzz.sh smoke            # replay the committed corpus only (fast; CI per-PR)
#   scripts/fuzz.sh hunt [seconds]   # time-boxed fuzzing per target (default 300s; scheduled)
#   scripts/fuzz.sh hunt 60 cas      # restrict to one crate's targets
#
# Requires a nightly toolchain and cargo-fuzz (`cargo install cargo-fuzz`).
# debug-assertions + overflow-checks are forced on by each fuzz crate's
# profile, so arithmetic on untrusted lengths traps rather than wraps.
set -euo pipefail

MODE="${1:-smoke}"
SECS="${2:-300}"
ONLY="${3:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATES=(cas storage-server loader ipc eunomia-sys)

# A low per-allocation cap turns a length-field-driven allocation into a
# reportable crash; a generous RSS limit avoids false trips on libFuzzer's
# own steady-state footprint.
case "$MODE" in
  smoke) RUN_ARGS=(-runs=0 -rss_limit_mb=2048) ;;
  hunt)  RUN_ARGS=(-max_total_time="$SECS" -rss_limit_mb=2048 -malloc_limit_mb=128) ;;
  *) echo "usage: $0 {smoke|hunt} [seconds] [crate]" >&2; exit 2 ;;
esac

rc=0
for c in "${CRATES[@]}"; do
  [ -n "$ONLY" ] && [ "$ONLY" != "$c" ] && continue
  pushd "$ROOT/$c" >/dev/null
  for t in $(cargo +nightly fuzz list); do
    echo "== $c :: $t ($MODE) =="
    if ! cargo +nightly fuzz run "$t" -- "${RUN_ARGS[@]}"; then
      echo "!! FAILED: $c/$t"
      rc=1
    fi
  done
  popd >/dev/null
done
exit $rc
