//! The PAL↔seam ABI exports (rev2§6.1(d)).
//!
//! The vendored std PAL (`vendor/rust`'s `sys/pal/eunomia`) cannot depend on this
//! crate directly: the verified deps pull `vstd`, whose `verus_builtin` is not
//! buildable as a `rustc-dep-of-std` sysroot crate. So std declares a small set of
//! `extern "Rust"` symbols and a std binary links this crate (an ordinary dependency)
//! to satisfy them — the `__rust_alloc` pattern. These `#[no_mangle]` shims are that
//! satisfying side: each is a one-line delegation to the verified/host-tested surface
//! ([`bootstrap`](crate::bootstrap), [`syscall`](crate::syscall),
//! [`io_error`](crate::io_error)), holding no logic of its own.
//!
//! Gated to the eunomia/bare-metal targets so the `#[no_mangle]` names never leak into
//! a host build (where they could clash and where the `svc` shell is a stub anyway).

#![cfg(any(target_os = "eunomia", target_os = "none"))]

use core::alloc::{GlobalAlloc, Layout};

use crate::{bootstrap, heap, io_error, stdio, syscall, thread, tls};

/// The process-global std `System` heap (std-port 2.2): a fixed `.bss` arena over
/// the Verus-verified `freelist` allocator. A plain `static` — interior
/// `UnsafeCell` plus `urt`'s `unsafe impl Sync`, whose soundness is the heap's
/// yielding spinlock (std-port 3.2): in-process threads allocate concurrently, so
/// the allocator serializes its free-list access. `Heap::new()` is all-zero, so
/// it lands in `.bss`, which the loader maps and zeroes with the RW segment. `N` is
/// the per-binary reservation [`heap::HEAP_BYTES`] (committed RAM at spawn — no
/// demand paging in the MVP).
static HEAP: urt::Heap<{ heap::HEAP_BYTES }> = urt::Heap::new();

/// `GlobalAlloc::alloc` for the std `System` allocator. `urt::Heap` is total over
/// every `Layout` — null on over-`MAX_ALIGN`/exhaustion/fragmentation-cap — so this
/// shim re-establishes no precondition; it is the thinnest possible delegation.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_alloc(layout: Layout) -> *mut u8 {
    // SAFETY: GlobalAlloc::alloc's only contract (a non-zero-size layout) is upheld
    // by std's caller; `urt::Heap` additionally defends it with `size.max(1)`.
    unsafe { HEAP.alloc(layout) }
}

/// `GlobalAlloc::dealloc` for the std `System` allocator.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_dealloc(ptr: *mut u8, layout: Layout) {
    // SAFETY: `ptr` was handed out by `__eunomia_alloc` for this same `layout`
    // (std's GlobalAlloc contract); `urt::Heap::dealloc` round-trips the offset.
    unsafe { HEAP.dealloc(ptr, layout) }
}

/// Point the main thread's `TPIDR_EL0` at its TLS block (std-port 3.2). Called once
/// by the std PAL `_start`, before `bootstrap_init`/`main` and before any
/// `local_pointer!` access, so `set_current` works on the main thread.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_init_main() {
    tls::init_main();
}

/// Set up a spawned thread's `TPIDR_EL0` TLS block (std-port 3.2). Called first in
/// the `sys/thread` trampoline, before `ThreadInit::init` runs `set_current`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_init_thread() {
    tls::init_thread();
}

/// Receive + verified-decode the slot-0 startup block and stash argv/env/grants.
/// Called once by the std PAL `_start` before `main`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_bootstrap_init() {
    bootstrap::init();
}

/// The stashed argv as raw byte-strings (rev2§5.1), for the `sys::args` arm.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_argv() -> &'static [&'static [u8]] {
    bootstrap::argv()
}

/// The stashed environment as raw `KEY=VALUE` byte-strings, for the `sys::env` arm.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_env() -> &'static [&'static [u8]] {
    bootstrap::env()
}

/// Exit through the kernel thread-exit terminus (rev2§5.1); the parent reaper reads
/// `code` as the child's status. Also ends an in-process thread (std-port 3.2): the
/// thread's on-exit binding raises its notif, waking the joiner.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_thread_exit(code: u64) -> ! {
    syscall::thread_exit(code)
}

/// Spawn an in-process thread (std-port 3.2): `entry` is the std trampoline, `arg`
/// its closure pointer (crosses in `x0`). Returns the join handle (`>= 0`) or a
/// negative `ERR_*`; the `sys/thread` arm maps `< 0` through `from_raw_os_error`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_thread_spawn(entry: usize, stack: usize, arg: u64) -> i64 {
    thread::spawn(entry, stack, arg)
}

/// Join the in-process thread whose handle is `handle`. Returns 0 or a negative
/// `ERR_*`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_thread_join(handle: u64) -> i64 {
    thread::join(handle)
}

/// Cooperative yield (op 2), for `thread::yield_now`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_thread_yield() {
    thread::yield_now();
}

/// Sleep at least `nanos` (the MVP yield-poll), for `thread::sleep`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_thread_sleep(nanos: u64) {
    thread::sleep(nanos);
}

/// Write `buf` to the kernel debug-log (rev2§7) for the bring-up `sys/stdio` arm,
/// split into `DEBUG_WRITE_MAX`-byte `DebugWrite` chunks — the kernel `ERR_FAULT`s a
/// longer write, so the chunking re-establishes that cap at the seam (std-port 2.3).
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_stdio_write(buf: &[u8]) -> usize {
    stdio::write(buf)
}

/// Monotonic nanoseconds for the `sys/time` `Instant` arm (std-port 2.4): the
/// CNTVCT/CNTFRQ virtual counter via urt's Verus-verified `utc_ns_at`, needing no
/// `"time"` grant. Total + monotone (the urt time row); this shim re-establishes
/// no precondition.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_mono_ns() -> i64 {
    urt::time::now_mono_ns()
}

/// Wall-clock nanoseconds since the Unix epoch for the `sys/time` `SystemTime` arm
/// (std-port 2.4): the rev2§2.6 time page. Panics if no `"time"` grant was attached
/// (a process asking for wall time without it is mis-wired, not degraded — the urt
/// posture); `bootstrap::init` attaches the page when the grant is present.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_wall_ns() -> i64 {
    urt::time::now_utc_ns()
}

/// Classify a raw syscall error code into the [`io_error::Kind`] discriminant
/// (`#[repr(u8)]`) the PAL maps to `io::ErrorKind`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_io_classify(code: i64) -> u8 {
    io_error::classify(code) as u8
}

/// A static human-readable message for a raw syscall error code, for `error_string`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_io_message(code: i64) -> &'static str {
    io_error::message(code)
}
