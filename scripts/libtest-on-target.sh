#!/bin/bash
# On-target library-test triage. Runs subsets of the
# upstream Rust `coretests` and `alloctests` unit-test suites against the eunomia PAL
# under QEMU, so a PAL regression (or a forward-port to a newer nightly) is caught by
# real library tests, not only the bespoke `stdsmoke` fixture.
#
# Why "triage": the target is panic=abort with no process spawn, so libtest's default
# per-test-subprocess isolation is impossible. We pass `--force-run-in-process`, which
# runs every test in one process — but then `catch_unwind` cannot catch, so a single
# failing/panicking test ABORTS the whole run. Getting a suite green therefore means
# discovering which tests can't pass here and skipping them; that skip set (the
# `scripts/libtest-skips/*.skip` files) is the committed deliverable.
#
#   * `--exclude-should-panic` drops #[should_panic] tests wholesale — they panic by
#     design and so can never pass in-process under panic=abort.
#   * Each suite is driven one top-level module at a time (positive filter as argv[1]),
#     because the 256-byte startup block only fits ~1-2 `--skip=` filters per command.
#   * The culprit oracle: libtest (serial — available_parallelism()==1) prints
#     `test <NAME> ...` BEFORE running each test, so on an abort the last such line
#     names the failing test. Add it (or a module prefix) to the suite's .skip file.
#
# Usage:
#   scripts/libtest-on-target.sh [--ci | --full] [--suite coretests|alloctests|both]
#     --ci    : a small curated subset of fast, known-green modules (the CI gate).
#     --full  : every top-level module of each suite (the local triage sweep; default).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${IMG:-/tmp/eunomia-libtest.img}"
DEMO_ROOT="${DEMO_ROOT:-/tmp/eunomia-libtest-root}"
LOG="${LOG:-/tmp/eunomia-libtest.log}"
SKIP_DIR="$ROOT/scripts/libtest-skips"
PER_MODULE_TIMEOUT="${PER_MODULE_TIMEOUT:-240}"

MODE=full
SUITE=both
while [ $# -gt 0 ]; do
    case "$1" in
        --ci) MODE=ci ;;
        --full) MODE=full ;;
        --suite) SUITE="$2"; shift ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

# Base libtest flags for every run. -Zunstable-options unlocks the two unstable flags;
# it is accepted because build-std compiles libtest with the nightly toolchain (its
# build.rs sets `enable_unstable_features`), so no RUSTC_BOOTSTRAP env is needed.
BASE_FLAGS="--force-run-in-process --exclude-should-panic -Zunstable-options"

# Full top-level module lists (mirror the `mod X;` decls in each vendored tests/lib.rs).
CORETESTS_FULL="alloc any array ascii ascii_char asserting async_iter atomic bool bstr \
cell char clone cmp const_ptr convert ffi fmt future hash hint index intrinsics io iter \
lazy macros manually_drop mem net nonzero num ops option panic pattern pin pin_macro ptr \
result simd slice str str_lossy task time tuple unicode waker wtf8"
ALLOCTESTS_FULL="alloc_test arc autotraits borrow boxed btree_set_hash c_str c_str2 \
collections const_fns cow_str fmt heap linked_list misc_tests num rc slice sort str \
string sync task testing thin_box vec vec_deque"

# The CI subset: fast, self-contained, PAL-light modules that are green regressions.
CORETESTS_CI="num option result cmp convert iter slice"
ALLOCTESTS_CI="boxed vec string"

# --- build (opt in to the libtest binaries via kernel/build.rs) -----------------------
# cd, not --manifest-path: the kernel .cargo/config.toml pins the bare-metal target by
# directory. EUNOMIA_BUILD_LIBTESTS makes kernel/build.rs cross-build the two suites
# (large — several minutes on a cold build) and *embed* them into the shell, which spawns
# them from `.rodata` on `run bin/{coretests,alloctests}` — bypassing the store, whose MVP
# read path cannot practically serve a multi-MiB file. So the suites are NOT staged on the
# disk image; the image only needs a valid store for storaged to mount.
(cd "$ROOT" && cargo build -p mkfs)
(cd "$ROOT/kernel" && EUNOMIA_BUILD_LIBTESTS=1 cargo build)

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/bin"
printf 'libtest on-target\n' > "$DEMO_ROOT/hello.txt"
"$ROOT/target/debug/mkfs" "$IMG" "$DEMO_ROOT" 64

FIFO=$(mktemp -u)
mkfifo "$FIFO"
: > "$LOG"

qemu-system-aarch64 \
    -machine virt,gic-version=3 \
    -cpu cortex-a72 -m 256M -nographic \
    -nic none \
    -serial mon:stdio \
    -rtc base=utc,clock=host \
    -global virtio-mmio.force-legacy=false \
    -drive if=none,file="$IMG",format=raw,id=hd \
    -device virtio-blk-device,drive=hd \
    -kernel "$ROOT/target/aarch64-unknown-none-softfloat/debug/kernel" \
    < "$FIFO" > "$LOG" 2>&1 &
QPID=$!
exec 3>"$FIFO"
rm -f "$FIFO"
trap 'kill $QPID 2>/dev/null || true' EXIT

# Send a command line to the shell, paced. A long line written to the FIFO in one burst
# overflows QEMU's PL011 RX FIFO faster than the interrupt-driven console driver drains it
# (32-byte buffer), silently dropping bytes so the line is never recognized. Writing in
# small chunks with a brief pause lets the driver keep up. Terminates with `\r`.
send_cmd() { # <line>
    local s="$1" i=0 n=${#1}
    while [ "$i" -lt "$n" ]; do
        printf '%s' "${s:$i:16}" >&3
        i=$((i + 16))
        sleep 0.05
    done
    printf '\r' >&3
}

wait_for() { # <pattern> <timeout-secs>
    local deadline=$(($(date +%s) + $2))
    until grep -q "$1" "$LOG" 2>/dev/null; do
        if ! kill -0 "$QPID" 2>/dev/null; then
            echo "LIBTEST FAIL: QEMU exited while waiting for: $1" >&2
            tail -60 "$LOG" >&2; exit 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "LIBTEST FAIL: timeout waiting for: $1" >&2
            tail -60 "$LOG" >&2; exit 1
        fi
        sleep 0.2
    done
}

# Per-module `--skip=` args from the suite's committed skip file: one test-name
# substring per line (`#` comments and blanks ignored). Only entries for THIS module
# are emitted, to stay within the 256-byte startup-block argv budget.
skip_args() { # <suite> <module>
    local f="$SKIP_DIR/$1.skip" out=""
    [ -f "$f" ] || { echo ""; return; }
    local line
    while IFS= read -r line; do
        line="${line%%#*}"
        line="${line#"${line%%[![:space:]]*}"}"   # ltrim
        line="${line%"${line##*[![:space:]]}"}"    # rtrim
        [ -z "$line" ] && continue
        case "$line" in
            "$2"|"$2"::*) out="$out --skip=$line" ;;
        esac
    done < "$f"
    echo "$out"
}

# A module aborts the *child* (STATUS_PANIC on the first failing test), but the shell
# survives and reaps it, so the sweep continues to the next module — one boot maps every
# failure. FAILS collects "suite::module culprit=<test>" for the triage report; the run
# is green iff FAILS is empty. The culprit oracle: libtest (serial) prints `test <NAME>
# ...` before running each test, so on an abort the last such line names the failing test.
FAILS=()
# Total child reaps seen so far (`exited(...)` on pass, `panicked`/`faulted(` on abort).
# Keying on the *reap* — not the `test result:` line — is what synchronizes the sweep:
# the next module's command must not be sent until the shell has fully reaped the child
# and re-prompted, or the console input path drops it (the shell isn't reading yet).
reap_count() { grep -cE 'exited\(|panicked|faulted\(' "$LOG" 2>/dev/null || true; }
run_module() { # <suite> <module>
    local suite="$1" module="$2"
    local skips; skips="$(skip_args "$suite" "$module")"
    local pre_reap; pre_reap=$(reap_count)
    send_cmd "run bin/$suite $module:: $BASE_FLAGS $skips"
    local deadline=$(($(date +%s) + PER_MODULE_TIMEOUT)) status=timeout
    while :; do
        if [ "$(reap_count)" -gt "$pre_reap" ]; then status=reaped; break; fi
        if ! kill -0 "$QPID" 2>/dev/null; then status=died; break; fi
        [ "$(date +%s)" -ge "$deadline" ] && { status=timeout; break; }
        sleep 1
    done
    if [ "$status" = died ]; then
        echo "LIBTEST FATAL: QEMU died during $suite::$module (kernel/shell fault)" >&2
        tail -40 "$LOG" >&2
        kill "$QPID" 2>/dev/null || true
        exit 1
    fi
    if [ "$status" = reaped ]; then
        # The module finished. Green iff its child exited(0) with a `test result: ok` and
        # no abort/failure in its window (the lines between the previous reap and this one).
        # awk: print lines while the running reap-count equals `pre_reap` (this module's
        # segment), including the reap line itself (checked before the counter advances).
        local since; since=$(awk -v c="$pre_reap" '{ if (n==c) print } /exited\(|panicked|faulted\(/ { n++ }' "$LOG" 2>/dev/null)
        if printf '%s' "$since" | grep -qE 'panicked|faulted\(|test result: FAILED|memory allocation of'; then
            local culprit
            culprit=$(printf '%s' "$since" | grep -E 'test .* \.\.\.' | tail -1 | sed -E 's/ \.\.\..*$//; s/^test +//')
            echo "  FAIL(abort): $suite::$module culprit=${culprit:-<none>}" >&2
            FAILS+=("$suite::$module (abort) culprit=${culprit:-<unknown>}")
        elif printf '%s' "$since" | grep -q 'test result: ok'; then
            echo "  ok: $suite::$module"
        else
            echo "  FAIL(no-result): $suite::$module (reaped without a test result)" >&2
            FAILS+=("$suite::$module (no-result)")
        fi
        sleep 1   # settle: let the fresh prompt appear before the next command
        return
    fi
    # timeout: no reap within the window — a hang.
    local culprit
    culprit=$(grep -E 'test .* \.\.\.' "$LOG" | tail -1 | sed -E 's/ \.\.\..*$//; s/^test +//')
    echo "  FAIL(timeout): $suite::$module last=${culprit:-<none>}" >&2
    FAILS+=("$suite::$module (timeout) last=${culprit:-<unknown>}")
}

run_suite() { # <suite> <module-list>
    local suite="$1" mods="$2"
    echo "=== $suite ($MODE) ==="
    for m in $mods; do run_module "$suite" "$m"; done
}

# Every skip entry for a suite, as `--skip=<name>` args (budget permitting) — used by the
# whole-suite run, which has no per-module scoping.
all_skip_args() { # <suite>
    local f="$SKIP_DIR/$1.skip" out="" line
    [ -f "$f" ] || { echo ""; return; }
    while IFS= read -r line; do
        line="${line%%#*}"
        line="${line#"${line%%[![:space:]]*}"}"; line="${line%"${line##*[![:space:]]}"}"
        [ -z "$line" ] && continue
        out="$out --skip=$line"
    done < "$f"
    echo "$out"
}

# Run a whole suite in ONE child process/spawn. Repeated console-child spawn/reap wedges
# the shell's console input after a few dozen iterations (a per-spawn resource leak the
# mass on-target run surfaced), so `--full` avoids 50+ spawns by running the entire suite
# at once. A single failing test aborts the whole suite (panic=abort, in-process); the
# last `test <NAME> ...` line names it for the skip list, and a scoped `--ci`/per-module
# re-run isolates it.
WHOLE_TIMEOUT="${WHOLE_TIMEOUT:-900}"
run_whole() { # <suite>
    local suite="$1"
    local skips; skips="$(all_skip_args "$suite")"
    local pre_reap; pre_reap=$(reap_count)
    echo "=== $suite (whole) ==="
    send_cmd "run bin/$suite $BASE_FLAGS $skips"
    local deadline=$(($(date +%s) + WHOLE_TIMEOUT)) status=timeout
    while :; do
        [ "$(reap_count)" -gt "$pre_reap" ] && { status=reaped; break; }
        if ! kill -0 "$QPID" 2>/dev/null; then status=died; break; fi
        [ "$(date +%s)" -ge "$deadline" ] && { status=timeout; break; }
        sleep 2
    done
    if [ "$status" = died ]; then
        echo "LIBTEST FATAL: QEMU died during $suite (whole)" >&2; tail -40 "$LOG" >&2
        kill "$QPID" 2>/dev/null || true; exit 1
    fi
    if [ "$status" = timeout ]; then
        local last; last=$(grep -E 'test .* \.\.\.' "$LOG" | tail -1 | sed -E 's/ \.\.\..*$//; s/^test +//')
        echo "  FAIL(timeout): $suite (whole) last=${last:-<none>}" >&2
        FAILS+=("$suite (whole,timeout) last=${last:-<unknown>}"); return
    fi
    local since; since=$(awk -v c="$pre_reap" '{ if (n==c) print } /exited\(|panicked|faulted\(/ { n++ }' "$LOG" 2>/dev/null)
    if printf '%s' "$since" | grep -qE 'panicked|faulted\(|test result: FAILED|memory allocation of'; then
        local culprit; culprit=$(printf '%s' "$since" | grep -E 'test .* \.\.\.' | tail -1 | sed -E 's/ \.\.\..*$//; s/^test +//')
        echo "  FAIL(abort): $suite (whole) culprit=${culprit:-<none>}" >&2
        FAILS+=("$suite (whole,abort) culprit=${culprit:-<unknown>}")
    elif printf '%s' "$since" | grep -q 'test result: ok'; then
        echo "  ok: $suite (whole) — $(printf '%s' "$since" | grep -oE 'test result: ok\. [0-9]+ passed[^)]*' | head -1)"
    else
        echo "  FAIL(no-result): $suite (whole)" >&2; FAILS+=("$suite (whole,no-result)")
    fi
}

wait_for '\[storaged\] serving' 90
wait_for 'eunomia> ' 30

if [ "$MODE" = ci ]; then
    # Per-module curated subset — few spawns, stays under the console-churn threshold.
    { [ "$SUITE" = both ] || [ "$SUITE" = coretests ]; } && run_suite coretests "$CORETESTS_CI"
    { [ "$SUITE" = both ] || [ "$SUITE" = alloctests ]; } && run_suite alloctests "$ALLOCTESTS_CI"
else
    # Whole-suite (one spawn each) to avoid the mass-spawn console wedge.
    { [ "$SUITE" = both ] || [ "$SUITE" = coretests ]; } && run_whole coretests
    { [ "$SUITE" = both ] || [ "$SUITE" = alloctests ]; } && run_whole alloctests
fi

kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

oks=$(grep -c 'test result: ok' "$LOG" 2>/dev/null || true)
passed=$(grep -oE 'test result: ok\. [0-9]+ passed' "$LOG" | grep -oE '[0-9]+' | paste -sd+ - | bc 2>/dev/null || echo '?')
if [ "${#FAILS[@]}" -gt 0 ]; then
    echo "LIBTEST ON-TARGET FAIL ($MODE): ${#FAILS[@]} module(s) failed/aborted, $oks green ($passed tests passed):" >&2
    printf '  %s\n' "${FAILS[@]}" >&2
    exit 1
fi
echo "LIBTEST ON-TARGET PASS ($MODE): $oks module runs green, $passed tests passed, 0 failed, 0 aborts"
grep -E 'test result: ok\.' "$LOG" | tail -60
