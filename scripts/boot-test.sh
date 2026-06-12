#!/bin/bash
# QEMU boot test — the §2.6 proof: wall-clock time observable end to end.
# Boots the full system, takes two snapshots, and asserts that their
# UTC timestamps land in a sane window around the host clock, are
# strictly ordered (the §4.7 per-ref clamp), and print as ISO-8601; the
# shell's `date` (zero syscalls on the read path) must agree.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${IMG:-/tmp/eunomia-boot-test.img}"
DEMO_ROOT="${DEMO_ROOT:-/tmp/eunomia-boot-test-root}"
LOG="${LOG:-/tmp/eunomia-boot-test.log}"

# cd, not --manifest-path: the kernel's .cargo/config.toml pins a
# bare-metal target by directory, so host builds must run from the root.
(cd "$ROOT" && cargo build -p mkfs)
(cd "$ROOT/kernel" && cargo build)

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT"
printf 'boot test\n' > "$DEMO_ROOT/hello.txt"

T_BEFORE=$(date -u +%s)
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

wait_for() { # <pattern> <timeout-secs>
    local deadline=$(($(date +%s) + $2))
    until grep -q "$1" "$LOG" 2>/dev/null; do
        if ! kill -0 "$QPID" 2>/dev/null; then
            echo "BOOT TEST FAIL: QEMU exited while waiting for: $1" >&2
            tail -40 "$LOG" >&2
            exit 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "BOOT TEST FAIL: timeout waiting for: $1" >&2
            tail -40 "$LOG" >&2
            exit 1
        fi
        sleep 0.2
    done
}

wait_for '\[storaged\] serving' 60
wait_for 'eunomia> ' 30
printf 'snap first\r' >&3
wait_for 'snapshot #2' 30
printf 'snap second\r' >&3
wait_for 'snapshot #3' 30
printf 'snaps\r' >&3
wait_for '#3  auto' 30
printf 'date\r' >&3
# The standalone ISO line is the last asserted output, and the shell
# processes commands serially — once it appears, every earlier line
# (including the full #3 row) is complete in the log. Never kill on a
# bare sleep: that is a race against the guest on a loaded host.
wait_for '^[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}T[0-9:]*\.[0-9]\{9\}Z' 30
kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

T_AFTER=$(date -u +%s)

T_BEFORE="$T_BEFORE" T_AFTER="$T_AFTER" python3 - "$LOG" <<'EOF'
import os, re, sys
from datetime import datetime, timezone

log = open(sys.argv[1], errors="replace").read()
t_before = int(os.environ["T_BEFORE"])
t_after = int(os.environ["T_AFTER"])
# The RTC carries +-1 s absolute error by design (one-shot whole-second
# read, spec 2.6); allow a little slack on top for host scheduling.
MARGIN = 3

iso = r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})\.(\d{9})Z"

def to_ns(m):
    base = datetime.strptime(m.group(1), "%Y-%m-%dT%H:%M:%S")
    base = base.replace(tzinfo=timezone.utc)
    return int(base.timestamp()) * 10**9 + int(m.group(2))

def fail(msg):
    print(f"BOOT TEST FAIL: {msg}", file=sys.stderr)
    sys.exit(1)

rows = {}
for line in log.splitlines():
    m = re.match(r"#(\d+)\s+\w+\s+\[[^\]]*\]\s+" + iso, line)
    if m:
        rows[int(m.group(1))] = to_ns(re.search(iso, line))
for snap in (2, 3):
    if snap not in rows:
        fail(f"snapshot #{snap} missing from `snaps` output")

lo = (t_before - MARGIN) * 10**9
hi = (t_after + MARGIN) * 10**9
for snap in (2, 3):
    if not lo <= rows[snap] <= hi:
        fail(f"snapshot #{snap} timestamp {rows[snap]} outside [{lo}, {hi}]")
if not rows[2] < rows[3]:
    fail(f"snapshot timestamps not strictly ordered: {rows[2]} >= {rows[3]}")

dates = [to_ns(m) for m in re.finditer(r"^" + iso + r"\s*$", log, re.M)]
if not dates:
    fail("no `date` output found")
if not lo <= dates[-1] <= hi:
    fail(f"`date` {dates[-1]} outside [{lo}, {hi}]")

print("BOOT TEST PASS:")
print(f"  snapshot #2 at {rows[2]} ns")
print(f"  snapshot #3 at {rows[3]} ns (strictly later)")
print(f"  date        at {dates[-1]} ns")
print(f"  window      [{lo}, {hi}]")
EOF
