#!/bin/bash
# QEMU boot test — the std-port fs GATE (findings #13, extended by #15). It boots
# the full stack (mkfs image → virtio-blk → storaged → mount → console → shell) and
# drives the shell through the std fs client `user/stdfs`, asserting the whole
# `sys/fs/eunomia` surface works at EL0 against storaged:
#
#   run bin/stdfs → the fs run. Each `[stdfs]` line is one op: `alive` (stdio),
#     `wrote N bytes` (File::create + write_all + sync_all → Write/Sync),
#     `read back ok` (fs::read → chunked Read), `dotdot resolves; escape->denied,
#     malformed->invalid` (the verified `eunomia_sys::path::resolve`, std-port 4.2/4.3:
#     `.`/`..` resolved client-side, an escaping `..` → `PermissionDenied`, a NUL name
#     → `InvalidFilename`), `readdir found smoke` (read_dir → List), `metadata ok`
#     (fs::metadata dir/file kind + len → the Stat→List kind probe, std-port 4.3),
#     `renamed ok` (fs::rename → Rename), `removed ok` (remove_file → Unlink). It ends
#     with the green marker `STD4 PASS` and the shell reaps it as `exited(0)`.
#     marker `STD4 PASS` and the shell reaps it as `exited(0)`.
#
# The live witness for the *client-side connect handshake* + storaged's second
# session (multiplexed) is `[storaged] fs session negotiated wire version 2`,
# printed when stdfs connects over the delegated session — distinct from the
# shell's own `[storaged] negotiated wire version 2` at boot.
#
# Asserts the green marker, the fs-session connect witness, and no unexpected
# crash anywhere in the run.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${IMG:-/tmp/eunomia-fs-smoke.img}"
DEMO_ROOT="${DEMO_ROOT:-/tmp/eunomia-fs-smoke-root}"
LOG="${LOG:-/tmp/eunomia-fs-smoke.log}"

# cd, not --manifest-path: the kernel's .cargo/config.toml pins a bare-metal
# target by directory, so host builds must run from the root. The kernel build
# runs kernel/build.rs, which cross-builds user/stdfs (the std fs client).
(cd "$ROOT" && cargo build -p mkfs)
(cd "$ROOT/kernel" && cargo build)

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/bin"
# stdfs creates docs/smoke itself (creation is a side effect of Write, rev2§4.9),
# so the store starts effectively empty but for the binary it loads.
cp "$ROOT/target/user/aarch64-unknown-eunomia/release/stdfs" "$DEMO_ROOT/bin/stdfs"

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
            echo "FS SMOKE TEST FAIL: QEMU exited while waiting for: $1" >&2
            tail -60 "$LOG" >&2
            exit 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "FS SMOKE TEST FAIL: timeout waiting for: $1" >&2
            tail -60 "$LOG" >&2
            exit 1
        fi
        sleep 0.2
    done
}

wait_for '\[storaged\] serving' 60
wait_for 'eunomia> ' 30

# The fs run: every op, in order, then the green marker and a clean reap.
printf 'run bin/stdfs\r' >&3
wait_for '\[stdfs\] alive' 60
wait_for '\[storaged\] fs session negotiated wire version 2' 30
wait_for '\[stdfs\] wrote .* bytes' 30
wait_for '\[stdfs\] read back ok' 30
wait_for '\[stdfs\] dotdot resolves' 30
wait_for '\[stdfs\] readdir found smoke' 30
wait_for '\[stdfs\] metadata ok' 30
wait_for '\[stdfs\] renamed ok' 30
wait_for '\[stdfs\] removed ok' 30
wait_for 'STD4 PASS' 30
wait_for 'exited(0)' 30

kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

fail=0
# The green-boot marker — the gate's headline assertion.
if ! grep -q 'STD4 PASS' "$LOG"; then
    echo "FS SMOKE TEST FAIL: never reached STD4 PASS (an fs op failed)" >&2
    fail=1
fi
# The client-side connect handshake ran over storaged's *second* session.
if ! grep -q '\[storaged\] fs session negotiated wire version 2' "$LOG"; then
    echo "FS SMOKE TEST FAIL: fs session never connected (no second-session handshake)" >&2
    fail=1
fi
# A wrong result at any op prints an `fs-bad` line and exits non-zero.
if grep -q '\[stdfs\] fs-bad' "$LOG"; then
    echo "FS SMOKE TEST FAIL: an fs op returned the wrong result" >&2
    grep -n '\[stdfs\] fs-bad' "$LOG" >&2
    fail=1
fi
# A panic (e.g. a failed expect) reaps as STATUS_PANIC, not a clean exit.
if grep -q 'panicked' "$LOG"; then
    echo "FS SMOKE TEST FAIL: stdfs panicked (an fs op errored unexpectedly)" >&2
    grep -n 'panicked' "$LOG" >&2
    fail=1
fi
if grep -qE 'exited\(254\)|exited\(101\)' "$LOG"; then
    echo "FS SMOKE TEST FAIL: a panic surfaced as a clean exit" >&2
    fail=1
fi
if grep -q 'faulted(' "$LOG"; then
    echo "FS SMOKE TEST FAIL: a fault appeared — the fs client must not fault" >&2
    grep -n 'faulted(' "$LOG" >&2
    fail=1
fi
# No infrastructure crash. stdfs never prints uppercase 'PANIC'; any is a real
# kernel/shell/storaged crash.
if grep -q 'PANIC' "$LOG"; then
    echo "FS SMOKE TEST FAIL: an unexpected PANIC appeared in the run" >&2
    grep -n 'PANIC' "$LOG" >&2
    fail=1
fi
[ "$fail" -eq 0 ] || { echo "--- tail ---" >&2; tail -60 "$LOG" >&2; exit 1; }

echo "FS SMOKE TEST PASS:"
echo "  STD4 PASS — create/write/read/readdir/rename/remove/sync live at EL0 over storaged"
echo "  the fs client connected over storaged's second (multiplexed) session"
