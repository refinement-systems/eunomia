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
#   5. run bin/selftest 254      — the panic path (§5.1 U2): the child panics,
#      its runtime handler exits with the reserved STATUS_PANIC, and the
#      parent reads 'panicked' — NOT exited(254). A crash can't pass for a
#      clean stop. A following run bin/selftest 9 reclaims as cleanly again.
#   6. run bin/selftest 253      — the time grant (§2.6, S1): the shell maps
#      the read-only time page into the child and passes its VA in the
#      startup block; the child reads a sane UTC clock (time-ok). The
#      init→shell time grant, one hop further (shell→child). run 11 after
#      proves the donation reclaims with the time copy unmapped first.
#
# Asserts no BSS-LEAK (retype re-zeroes reused frames) and no UNEXPECTED
# PANIC (the one deliberate selftest panic aside) anywhere in the run.
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

# 5. Panic path (U2): a child that panics exits with the reserved status,
#    so the parent reads 'panicked', NOT exited(254) — a crash can't pass
#    for a clean stop. The donation reclaims as cleanly as after a fault.
printf 'run bin/selftest 254\r' >&3
wait_for 'panicked' 30
printf 'run bin/selftest 9\r' >&3
wait_for 'exited(9)' 30

# 6. Time grant (§2.6, S1): the shell maps the read-only time page into the
#    child and passes its VA in the startup block; the child reads a sane
#    UTC clock — the init→shell time grant, one hop further (shell→child).
#    A following run reclaims cleanly (the time copy is unmapped before the
#    revoke that frees the child aspace).
printf 'run bin/selftest 253\r' >&3
wait_for 'time-ok' 30
printf 'run bin/selftest 11\r' >&3
wait_for 'exited(11)' 30

kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

fail=0
if grep -q 'BSS-LEAK' "$LOG"; then
    echo "SPAWN TEST FAIL: a child observed a non-zero .bss — retype did not re-zero a reused frame" >&2
    fail=1
fi
# Infrastructure must not panic. The deliberate panic-path test (mode 254)
# prints exactly one '[selftest] PANIC'; any other PANIC — kernel, shell,
# storaged, or selftest panicking when it shouldn't — is a real failure.
if grep 'PANIC' "$LOG" | grep -qv '\[selftest\] PANIC'; then
    echo "SPAWN TEST FAIL: an unexpected PANIC appeared in the run" >&2
    grep -n 'PANIC' "$LOG" | grep -v '\[selftest\] PANIC' >&2
    fail=1
fi
sel_panics=$(grep -c '\[selftest\] PANIC' "$LOG" || true)
if [ "${sel_panics:-0}" -ne 1 ]; then
    echo "SPAWN TEST FAIL: expected exactly one deliberate selftest panic, saw ${sel_panics:-0}" >&2
    fail=1
fi
# The parent must observe the reserved panic status, not exited(254).
if ! grep -q 'panicked' "$LOG"; then
    echo "SPAWN TEST FAIL: parent did not read 'panicked' — panic status not propagated (U2)" >&2
    fail=1
fi
if grep -q 'exited(254)' "$LOG"; then
    echo "SPAWN TEST FAIL: a panic surfaced as exited(254) — the reserved status leaked through" >&2
    fail=1
fi
# The child must read its granted time page (§2.6 shell→child grant).
if grep -q 'time-bad' "$LOG"; then
    echo "SPAWN TEST FAIL: a child could not read its time grant" >&2
    fail=1
fi
if ! grep -q 'time-ok' "$LOG"; then
    echo "SPAWN TEST FAIL: child did not confirm the time grant (S1)" >&2
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
echo "  panic demo: panicked (reserved status), not exited(254), then a clean re-spawn"
echo "  time grant: child read its mapped time page (time-ok) then a clean re-spawn"
echo "  no BSS-LEAK (retype re-zeroes reused frames), no unexpected PANIC"
