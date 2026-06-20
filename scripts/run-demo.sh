#!/bin/bash
# Build everything, assemble the demo disk image with mkfs, boot the full
# system in QEMU (MVP demo script, spec rev1§1). Interactive by default;
# pipe commands on stdin for scripted runs.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${IMG:-/tmp/eunomia.img}"
DEMO_ROOT="${DEMO_ROOT:-/tmp/eunomia-demo-root}"

cargo build --manifest-path "$ROOT/Cargo.toml" -p mkfs
(cd "$ROOT/kernel" && cargo build)

rm -rf "$DEMO_ROOT"
mkdir -p "$DEMO_ROOT/docs" "$DEMO_ROOT/bin"
printf 'Hello from the versioned store!\n' > "$DEMO_ROOT/hello.txt"
printf 'Eunomia: capability-based OS with versioned storage.\n' > "$DEMO_ROOT/docs/readme"
cp "$ROOT/target/user/aarch64-unknown-none-softfloat/release/hello" "$DEMO_ROOT/bin/hello"
# selftest exercises the rev1§5.1 spawn/reclaim loop interactively:
#   run bin/selftest 42      → exited(42)
#   run bin/selftest 255     → faulted(translation, 0xdead0000)
#   runloop bin/selftest 100 → 100 spawn/wait/reclaim cycles, slots 56/56
cp "$ROOT/target/user/aarch64-unknown-none-softfloat/release/selftest" "$DEMO_ROOT/bin/selftest"

"$ROOT/target/debug/mkfs" "$IMG" "$DEMO_ROOT" 64

exec qemu-system-aarch64 \
    -machine virt,gic-version=3 \
    -cpu cortex-a72 -m 256M -nographic \
    -serial mon:stdio \
    -rtc base=utc,clock=host \
    -global virtio-mmio.force-legacy=false \
    -drive if=none,file="$IMG",format=raw,id=hd \
    -device virtio-blk-device,drive=hd \
    -kernel "$ROOT/target/aarch64-unknown-none-softfloat/debug/kernel"
