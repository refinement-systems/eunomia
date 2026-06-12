#!/bin/bash
# QEMU boot test — the §5.1 spawn/reclaim proof, same genre as M1's
# revocation test. Boots the full system and drives the shell through:
#
#   1. runloop bin/selftest 100  — spawn / wait / reclaim a trivial child
#      100x in one boot. The recyclable slot window is far smaller than 100,
#      so reaching 100/100 *is* slot + untyped reuse; un-reclaimed resources
#      would wedge within the first few spawns.
#   2. run bin/selftest 42       — exit-status propagation: thread_exit(42)
#      surfaces as exited(42) in the parent.
#   3. run bin/selftest 255      — the fault demo: a wild store suspends the
#      child (§5.3) and the parent reads faulted(translation, 0xdead0000)...
#   4. run bin/selftest 7        — ...and then spawns ANOTHER program, which
#      exits(7): the burn fix witnessed in the same breath as the fault, the
#      donation reused after a faulted child as cleanly as after an exited one.
#
# Asserts no BSS-LEAK (retype re-zeroes reused frames) and no kernel/shell
# PANIC anywhere in the run.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${IMG:-/tmp/eunomia-spawn-test.img}"
DEMO_ROOT="${DEMO_ROOT:-/tmp/eunomia-spawn-test-root}"
LOG="${LOG:-/tmp/eunomia-spawn-test.log}"

# cd, not --manifest-path: the kernel's .cargo/config.toml pins a bare-metal
# target by directory, so host builds must run from the root.
(cd "$ROOT" && cargo build -p mkfs)
(cd "$ROOT/kernel" && cargo build)

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/bin"
printf 'spawn test\n' > "$DEMO_ROOT/hello.txt"
cp "$ROOT/target/user/aarch64-unknown-none-softfloat/release/selftest" "$DEMO_ROOT/bin/selftest"

"$ROOT/target/debug/mkfs" "$IMG" "$DEMO_ROOT" 64

FIFO=$(mktemp -u)
mkfifo "$FIFO"
: > "$LOG"

qemu-system-aarch64 \
    -machine virt,gic-version=3 \
    -cpu cortex-a72 -m 256M -nographic \
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

wait_for() { # <pattern> <timeout-secs>
    local deadline=$(($(date +%s) + $2))
    until grep -q "$1" "$LOG" 2>/dev/null; do
        if ! kill -0 "$QPID" 2>/dev/null; then
            echo "SPAWN TEST FAIL: QEMU exited while waiting for: $1" >&2
            tail -60 "$LOG" >&2
            exit 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "SPAWN TEST FAIL: timeout waiting for: $1" >&2
            tail -60 "$LOG" >&2
            exit 1
        fi
        sleep 0.2
    done
}

wait_for '\[storaged\] serving' 60
wait_for 'eunomia> ' 30

# 1. The 100x burn-fix loop.
printf 'runloop bin/selftest 100\r' >&3
wait_for 'runloop: 100/100 ok' 90

# 2. Exit-status propagation.
printf 'run bin/selftest 42\r' >&3
wait_for 'exited(42)' 30

# 3. The fault demo: faulted report with the wild address...
printf 'run bin/selftest 255\r' >&3
wait_for 'faulted(translation, 0xdead0000)' 30

# 4. ...and another program runs right after, reusing the donation.
printf 'run bin/selftest 7\r' >&3
wait_for 'exited(7)' 30

kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

fail=0
if grep -q 'BSS-LEAK' "$LOG"; then
    echo "SPAWN TEST FAIL: a child observed a non-zero .bss — retype did not re-zero a reused frame" >&2
    fail=1
fi
if grep -q 'PANIC' "$LOG"; then
    echo "SPAWN TEST FAIL: a PANIC appeared in the run" >&2
    grep -n 'PANIC' "$LOG" >&2
    fail=1
fi
# The loop's own slot accounting must close: free count back to its start.
if ! grep -q 'runloop: 100/100 ok, slots 56/56' "$LOG"; then
    echo "SPAWN TEST FAIL: spawn slots leaked across the loop (expected 56/56)" >&2
    grep -n 'runloop:' "$LOG" >&2
    fail=1
fi
[ "$fail" -eq 0 ] || exit 1

echo "SPAWN TEST PASS:"
echo "  runloop 100/100, slots fully reclaimed"
echo "  exit(42) and exit(7) statuses propagated"
echo "  fault demo: faulted(translation, 0xdead0000) then a clean re-spawn"
echo "  no BSS-LEAK (retype re-zeroes reused frames), no PANIC"
