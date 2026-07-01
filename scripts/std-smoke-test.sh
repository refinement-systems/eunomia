#!/bin/bash
# QEMU boot test — the std-port Phase-2 GATE (findings 7-1). Phase 2's four
# sub-phases (2.1 entry+argv/env, 2.2 GlobalAlloc, 2.3 stdio→debug-log + exit
# terminus, 2.4 time) each deferred their *live* QEMU demonstration to this
# combined gate. It boots the full system and drives the shell through the first
# std user binary (`user/stdsmoke`), asserting the whole std stack works at EL0:
#
#   run bin/stdsmoke alpha beta  → the success run. Each `[stdsmoke]` line is one
#     arm: `alive` (println!/stdio, 2.3), `argv=[…alpha…beta…]` (env::args, 2.1),
#     `vec sum=5050 box=… argc=3` (Vec/Box/String/format!/GlobalAlloc, 2.2),
#     `instant-ok` (Instant ← CNTVCT, 2.4), `systemtime-ok` (SystemTime ← the
#     granted time page, 2.4). It ends with the green-boot marker `STD2 PASS`
#     and the shell reaps it as `exited(0)`.
#   run bin/stdsmoke panic       → the std-owned panic path (2.3): std's handler
#     terminates as the reserved STATUS_PANIC, so the parent reads `panicked`,
#     NOT `exited(_)`. This is the live witness that a *std* binary's panic reaps
#     correctly (selftest proves it for a no_std binary with its own handler;
#     here std owns the handler).
#
# Asserts the green marker, the argv echo, both time arms, the panic reap, and
# no unexpected crash anywhere in the run.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${IMG:-/tmp/eunomia-std-smoke.img}"
DEMO_ROOT="${DEMO_ROOT:-/tmp/eunomia-std-smoke-root}"
LOG="${LOG:-/tmp/eunomia-std-smoke.log}"

# cd, not --manifest-path: the kernel's .cargo/config.toml pins a bare-metal
# target by directory, so host builds must run from the root. The kernel build
# runs kernel/build.rs, which cross-builds user/stdsmoke (the first std binary)
# — that link is itself the first proof the PAL↔seam bridge resolves.
(cd "$ROOT" && cargo build -p mkfs)
(cd "$ROOT/kernel" && cargo build)

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/bin"
printf 'std smoke\n' > "$DEMO_ROOT/hello.txt"
cp "$ROOT/target/user/aarch64-unknown-eunomia/release/stdsmoke" "$DEMO_ROOT/bin/stdsmoke"

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
            echo "STD SMOKE TEST FAIL: QEMU exited while waiting for: $1" >&2
            tail -60 "$LOG" >&2
            exit 1
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            echo "STD SMOKE TEST FAIL: timeout waiting for: $1" >&2
            tail -60 "$LOG" >&2
            exit 1
        fi
        sleep 0.2
    done
}

wait_for '\[storaged\] serving' 60
wait_for 'eunomia> ' 30

# 1. The success run: every std arm, in order, then the green marker and a clean
#    reap. The shell delivers argv = [bin/stdsmoke, alpha, beta].
printf 'run bin/stdsmoke alpha beta\r' >&3
wait_for '\[stdsmoke\] alive' 60
wait_for '\[stdsmoke\] argv=.*alpha.*beta' 30
wait_for '\[stdsmoke\] vec sum=5050' 30
wait_for '\[stdsmoke\] instant-ok' 30
wait_for '\[stdsmoke\] systemtime-ok' 30
wait_for 'STD2 PASS' 30
wait_for 'exited(0)' 30

# 2. The thread-spawn path (std-port 3.2): two std threads each allocate in a tight
#    loop (concurrent access to the one process heap — the race the heap spinlock
#    serializes), each with its own TPIDR_EL0 TLS, then join. `STD32 PASS` and a
#    clean reap witness the whole spawn/join/heap-lock/per-thread-TLS stack live.
printf 'run bin/stdsmoke spawn\r' >&3
wait_for '\[stdsmoke\] spawning threads' 30
wait_for '\[stdsmoke\] threads joined total=' 30
wait_for 'STD32 PASS' 30

# 2b. The lock stack over sys::futex (std-port 3.3): two std threads alternate turns
#     under a Mutex + Condvar, blocking on the condvar until their parity comes up —
#     real cross-thread futex_wait/futex_wake. A lost wakeup would hang the join (no
#     STD33 PASS); a wrong count would exit(7). STD33 PASS witnesses the whole
#     Mutex/Condvar stack live at EL0.
printf 'run bin/stdsmoke sync\r' >&3
wait_for '\[stdsmoke\] sync start' 30
wait_for '\[stdsmoke\] sync done total=' 30
wait_for 'STD33 PASS' 30

# 2c. The entropy path (std-port 3.4): build a default-hasher HashMap of 1000
#     entries and read every key back. RandomState draws hashmap_random_keys →
#     fill_bytes → the per-process DRBG seeded from the NAME_RANDOM_SEED grant the
#     shell handed this child. An unseeded child would abort loudly (no STD34); a
#     broken hasher would exit(8/9/10). STD34 PASS witnesses seed-grant → DRBG →
#     SipHash → HashMap end to end.
printf 'run bin/stdsmoke hashmap\r' >&3
wait_for '\[stdsmoke\] hashmap start' 30
wait_for '\[stdsmoke\] hashmap done entries=1000' 30
wait_for 'STD34 PASS' 30

# 2d. Real thread_local! storage + destructors (std-port 3.5): a spawned thread
#     touches a Drop-sentinel thread_local! and a per-thread Cell, then exits. Its
#     destructor must run on the spawned thread's exit (drops>0), and the Cell must
#     be genuinely per-thread (child sees 7, main sees 0) — the old single-threaded
#     no_threads storage got both wrong. A regression exits(11) (tls-bad). STD35 PASS
#     witnesses the verified urt::tls key table + the trampoline dtor runner.
printf 'run bin/stdsmoke tls\r' >&3
wait_for '\[stdsmoke\] tls start' 30
wait_for '\[stdsmoke\] tls done drops=' 30
wait_for 'STD35 PASS' 30

# 3. The std-owned panic path: the parent must read 'panicked' (STATUS_PANIC),
#    not exited(_). std's own panic hook prints 'panicked at …' first.
printf 'run bin/stdsmoke panic\r' >&3
wait_for 'panicked' 30

kill "$QPID" 2>/dev/null || true
wait "$QPID" 2>/dev/null || true
trap - EXIT

fail=0
# The green-boot marker — the gate's headline assertion.
if ! grep -q 'STD2 PASS' "$LOG"; then
    echo "STD SMOKE TEST FAIL: never reached STD2 PASS (a std arm failed)" >&2
    fail=1
fi
# The thread-spawn marker (std-port 3.2): two threads spawned, allocated
# concurrently, and joined. Its absence means spawn/join/heap-lock/TLS failed.
if ! grep -q 'STD32 PASS' "$LOG"; then
    echo "STD SMOKE TEST FAIL: never reached STD32 PASS (thread spawn/join failed)" >&2
    fail=1
fi
# A "current thread handle already set" abort means per-thread TLS regressed.
if grep -q 'current thread handle already set' "$LOG"; then
    echo "STD SMOKE TEST FAIL: per-thread TLS regressed (set_current aborted on a spawned thread)" >&2
    fail=1
fi
# The lock-stack marker (std-port 3.3): two threads ping-ponged under a Mutex +
# Condvar over sys::futex. Its absence means a lost wakeup hung the join, or the
# futex backend never linked.
if ! grep -q 'STD33 PASS' "$LOG"; then
    echo "STD SMOKE TEST FAIL: never reached STD33 PASS (Mutex/Condvar over sys::futex failed)" >&2
    fail=1
fi
# A wrong critical-section count (lost/duplicated mutual exclusion) exits with 7.
if grep -q '\[stdsmoke\] sync-bad' "$LOG"; then
    echo "STD SMOKE TEST FAIL: sync counter wrong — lock did not serialize the critical section" >&2
    fail=1
fi
# The entropy/HashMap marker (std-port 3.4): a default-hasher HashMap worked over
# the seed-grant → DRBG → SipHash path. Its absence means the seed never attached
# (a loud abort) or the hasher round-trip failed.
if ! grep -q 'STD34 PASS' "$LOG"; then
    echo "STD SMOKE TEST FAIL: never reached STD34 PASS (HashMap over the entropy seed failed)" >&2
    fail=1
fi
# A wrong HashMap lookup / phantom hit / bad len exits with 8/9/10.
if grep -q '\[stdsmoke\] hashmap-bad' "$LOG"; then
    echo "STD SMOKE TEST FAIL: HashMap lookup wrong — the DRBG-seeded hasher misbehaved" >&2
    fail=1
fi
# The no-seed abort message would mean a child was spawned without NAME_RANDOM_SEED.
if grep -q 'no entropy seed attached' "$LOG"; then
    echo "STD SMOKE TEST FAIL: a child ran without an entropy seed grant (NAME_RANDOM_SEED missing)" >&2
    fail=1
fi
# The TLS marker (std-port 3.5): a spawned thread's thread_local! destructor ran on
# exit and the macro storage was genuinely per-thread. Its absence means the key
# table / dtor runner failed; a per-thread-storage or missed-destructor bug exits(11).
if ! grep -q 'STD35 PASS' "$LOG"; then
    echo "STD SMOKE TEST FAIL: never reached STD35 PASS (thread_local! storage/destructors failed)" >&2
    fail=1
fi
if grep -q '\[stdsmoke\] tls-bad' "$LOG"; then
    echo "STD SMOKE TEST FAIL: thread_local! wrong — destructor missed or storage not per-thread" >&2
    fail=1
fi
# env::args delivered the command line (2.1).
if ! grep -q '\[stdsmoke\] argv=.*alpha.*beta' "$LOG"; then
    echo "STD SMOKE TEST FAIL: argv not delivered to the std binary (env::args)" >&2
    fail=1
fi
# Both time arms (2.4). systemtime-bad means the time grant never attached.
if grep -q 'systemtime-bad' "$LOG"; then
    echo "STD SMOKE TEST FAIL: SystemTime could not read its granted time page" >&2
    fail=1
fi
if grep -q '\[stdsmoke\] instant-bad' "$LOG"; then
    echo "STD SMOKE TEST FAIL: Instant was not monotonic" >&2
    fail=1
fi
# The std panic reaps as the reserved status, not a clean exit and not a fault.
if ! grep -q 'panicked' "$LOG"; then
    echo "STD SMOKE TEST FAIL: parent did not read 'panicked' — std panic not propagated as STATUS_PANIC" >&2
    fail=1
fi
if grep -qE 'exited\(254\)|exited\(101\)' "$LOG"; then
    echo "STD SMOKE TEST FAIL: a panic surfaced as a clean exit — STATUS_PANIC leaked through" >&2
    fail=1
fi
if grep -q 'faulted(' "$LOG"; then
    echo "STD SMOKE TEST FAIL: a fault appeared — a std panic must not be a hardware fault" >&2
    grep -n 'faulted(' "$LOG" >&2
    fail=1
fi
# No infrastructure crash. The std binary's panic prints lowercase 'panicked at',
# never uppercase 'PANIC' (and no selftest runs here), so any 'PANIC' is a real
# kernel/shell/storaged crash.
if grep -q 'PANIC' "$LOG"; then
    echo "STD SMOKE TEST FAIL: an unexpected PANIC appeared in the run" >&2
    grep -n 'PANIC' "$LOG" >&2
    fail=1
fi
[ "$fail" -eq 0 ] || { echo "--- tail ---" >&2; tail -60 "$LOG" >&2; exit 1; }

echo "STD SMOKE TEST PASS:"
echo "  STD2 PASS — println!/format!/Vec/Box/String/Instant/SystemTime live at EL0"
echo "  STD32 PASS — two std threads spawned/joined, concurrent heap alloc under the lock, per-thread TLS"
echo "  STD33 PASS — two std threads ping-ponged under a Mutex + Condvar over sys::futex"
echo "  STD34 PASS — HashMap over the per-process entropy seed (seed-grant → DRBG → SipHash)"
echo "  STD35 PASS — thread_local! per-thread storage + destructor ran on spawned-thread exit"
echo "  env::args delivered the command line (alpha beta)"
echo "  SystemTime read its granted time page; Instant monotonic"
echo "  std panic reaped as STATUS_PANIC (parent read 'panicked'), not exited/faulted"
