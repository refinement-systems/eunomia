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

use crate::{bootstrap, heap, io_error, stdio, syscall};

/// The process-global std `System` heap (std-port 2.2): a fixed `.bss` arena over
/// the Verus-verified `freelist` allocator. A plain `static` — interior
/// `UnsafeCell` plus `urt`'s `unsafe impl Sync` (eunomia processes are
/// single-threaded, so the allocator takes no lock); `Heap::new()` is all-zero, so
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
/// `code` as the child's status.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_thread_exit(code: u64) -> ! {
    syscall::thread_exit(code)
}

/// Write `buf` to the kernel debug-log (rev2§7) for the bring-up `sys/stdio` arm,
/// split into `DEBUG_WRITE_MAX`-byte `DebugWrite` chunks — the kernel `ERR_FAULT`s a
/// longer write, so the chunking re-establishes that cap at the seam (std-port 2.3).
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_stdio_write(buf: &[u8]) -> usize {
    stdio::write(buf)
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
