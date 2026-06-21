#!/bin/bash
# QEMU boot test — the M1 exit criterion (rev1§1), the kernel's cap-mechanism
# regression. Builds the embedded EL0 test program (`cargo build --features
# m1-test`, kernel/src/user.rs) and boots it. The program walks the whole
# M1/rev1§5.1 surface and prints a marker per stage:
#
#   1  thread 1 alive at EL0
#   2  a derived (signal-only) cap arrived over a channel and was used
#   3  revoke reached through a queued message AND a receiver's cspace
#   4  a timer object signalled a bound notification
#   5  the child's on-exit report fired, read exited(42), and the report
#      surface is gated by the bind-reports / read-report rights bits
#   6  channel whole-object teardown (rev1§3.3): revoking a channel's backing
#      sub-untyped fired every endpoint's peer-closed binding before
#      reclamation, into a separately-funded notification that survived
#   7  device IRQ → notification (rev1§1, rev1§3.6): a bound PL011 IRQ-handler
#      cap signalled its notification through the real GIC + exception path,
#      was acked, and a second interrupt was delivered (the mask-on-deliver /
#      unmask-on-ack cycle). The line is software-pended from EL1 on the
#      m1-test path (no real device, no stdin); see doc/results/9_b-irq-c.
#
# Success is exactly the line "1234567M1 PASS" with no error marker
# ("E<tag>!"), no "M1 FAIL", and no PANIC.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="${LOG:-/tmp/eunomia-m1-test.log}"

# cd, not --manifest-path: the kernel's .cargo/config.toml pins a bare-metal
# target by directory, so the build must run from the kernel dir.
(cd "$ROOT/kernel" && cargo build --features m1-test)

KERNEL="$ROOT/target/aarch64-unknown-none-softfloat/debug/kernel"
: > "$LOG"

qemu-system-aarch64 \
    -machine virt,gic-version=3 \
    -cpu cortex-a72 -m 256M -nographic \
    -nic none \
    -serial mon:stdio \
    -rtc base=utc,clock=host \
    -kernel "$KERNEL" \
    < /dev/null > "$LOG" 2>&1 &
QPID=$!
trap 'kill $QPID 2>/dev/null || true' EXIT

deadline=$(($(date +%s) + 30))
until grep -qE 'M1 PASS|M1 FAIL|PANIC|E.!' "$LOG" 2>/dev/null; do
    if ! kill -0 "$QPID" 2>/dev/null; then
        echo "M1 TEST FAIL: QEMU exited before any verdict" >&2
        tail -40 "$LOG" >&2
        exit 1
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        echo "M1 TEST FAIL: timeout (a stage blocked — likely a signal that never fired)" >&2
        tail -40 "$LOG" >&2
        exit 1
    fi
    sleep 0.2
done

kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

if ! grep -q '1234567M1 PASS' "$LOG"; then
    echo "M1 TEST FAIL: did not reach the full marker sequence '1234567M1 PASS'" >&2
    grep -nE 'M1 FAIL|PANIC|E.!|1234' "$LOG" >&2 || true
    tail -40 "$LOG" >&2
    exit 1
fi

echo "M1 TEST PASS:"
echo "  1234567M1 PASS — caps/CDT, revoke-through-queue + cspace, timer,"
echo "  thread reports (exit(42), rights gating), rev1§3.3 channel"
echo "  whole-object teardown, and the rev1§3.6 device-IRQ → notification path"
