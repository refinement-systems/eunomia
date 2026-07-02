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

#![cfg(bare_metal)]

use core::alloc::{GlobalAlloc, Layout};

use crate::{bootstrap, console, fs, futex, heap, io_error, random, stdio, syscall, thread, tls};
use core::sync::atomic::AtomicU32;

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

/// Allocate a TLS key + register its destructor (std-port 3.5): std's key-based TLS
/// (`sys/thread_local/os.rs` via the `key/eunomia.rs` bridge) over the verified
/// `urt::tls` key table. `dtor` is `os::destroy_value::<T>` (or `None` for a
/// `local_pointer!`). Returns `0` when the table is full — the std side aborts.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_create(dtor: Option<unsafe extern "C" fn(*mut u8)>) -> usize {
    tls::create(dtor)
}

/// This thread's value for `key` (the raw pointer std stored).
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_get(key: usize) -> *mut u8 {
    // SAFETY: `key` came from `__eunomia_tls_create` (std's `LazyKey`), so it is in
    // `1..=TLS_SLOTS`; the read is confined to this thread's own block.
    unsafe { tls::get(key) }
}

/// Store `val` as this thread's value for `key`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_set(key: usize, val: *mut u8) {
    // SAFETY: as `__eunomia_tls_get`.
    unsafe { tls::set(key, val) }
}

/// Free a TLS key (std's `LazyKey` race-loser cleanup).
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_destroy(key: usize) {
    // SAFETY: `key` came from `__eunomia_tls_create` and is currently live.
    unsafe { tls::destroy(key) }
}

/// Run this thread's `thread_local!` destructors at thread exit (std-port 3.5).
/// Called by the std trampoline / `_start` after the thread body, before
/// `free_thread`. Eunomia owns thread exit, so it drives destructors itself.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_run_dtors() {
    tls::run_thread_dtors();
}

/// Reclaim a spawned thread's heap TLS block at thread exit (std-port 3.5 — fixes
/// the 3.2 leak). A no-op for the main thread's static block.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_tls_free_thread() {
    tls::free_thread_block();
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

/// Wait on the futex `*futex` while it equals `expected` (std-port 3.3): the
/// `sys::futex` backend for the whole upstream lock stack (Mutex/Condvar/RwLock/
/// Once/Parker). `timeout_ns == u64::MAX` means no timeout; returns `false` only on
/// timeout. All logic lives in the seam (`urt::futex`); this arm only marshals.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_futex_wait(
    futex: &AtomicU32,
    expected: u32,
    timeout_ns: u64,
) -> bool {
    futex::wait(futex, expected, timeout_ns)
}

/// Wake one waiter on `*futex`; `true` iff one was woken.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_futex_wake(futex: &AtomicU32) -> bool {
    futex::wake(futex)
}

/// Wake all waiters on `*futex`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_futex_wake_all(futex: &AtomicU32) {
    futex::wake_all(futex)
}

/// Write `buf` to the kernel debug-log (rev2§7): the `sys/stdio` **panic last-words**
/// path (std-port 2.3/5.1). std-port 5.1 moved ordinary stdout/stderr onto the console
/// (below); this stays the panic sink so reporting never depends on the console channel
/// (rev2§7 C-M9). Split into `DEBUG_WRITE_MAX`-byte `DebugWrite` chunks — the kernel
/// `ERR_FAULT`s a longer write, so the chunking re-establishes that cap at the seam.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_stdio_write(buf: &[u8]) -> usize {
    stdio::write(buf)
}

/// The `sys/stdio` `Stdout` write body (std-port 5.1): `buf` to the `user/console`
/// `stdout` channel, else the debug-log fallback. All chunking/backpressure lives in
/// [`console`]; this shim only delegates.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_stdout_write(buf: &[u8]) -> usize {
    console::stdout_write(buf)
}

/// The `sys/stdio` `Stderr` write body (std-port 5.1): `buf` to the `user/console`
/// `stderr` channel (`NAME_STDERR` → else the `stdout` channel), else the debug-log —
/// a stream distinct from stdout so diagnostics never enter a pipeline's data.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_stderr_write(buf: &[u8]) -> usize {
    console::stderr_write(buf)
}

/// The `sys/stdio` `Stdin` read body (std-port 5.1): block for the next `user/console`
/// `stdin` message and deliver up to `buf.len()` bytes, or `0` (EOF) when no console
/// was granted. All blocking/carry logic lives in [`console`]; this shim only delegates.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_stdin_read(buf: &mut [u8]) -> usize {
    console::stdin_read(buf)
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

/// Fill `bytes` with random data for the `sys/random` arm (std-port 3.4): std's
/// `fill_bytes`/`hashmap_random_keys` over the per-process DRBG (`urt::random`)
/// seeded from the `NAME_RANDOM_SEED` grant. Loudly aborts if unseeded (the seam
/// re-establishes the "seed attached" precondition as a runtime guard, never a
/// bogus fill); all logic lives in `urt::random`, this arm only forwards.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fill_bytes(bytes: &mut [u8]) {
    random::fill_bytes(bytes)
}

/// Classify a raw syscall error code into the [`io_error::Kind`] discriminant
/// (`#[repr(u8)]`) the PAL maps to `io::ErrorKind`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_io_classify(code: i64) -> u8 {
    io_error::classify(code) as u8
}

// ── The storaged fs client (std-port 4.1) ──
// Each shim forwards to `crate::fs`, where the marshalling lives; raw path *bytes*
// cross the seam (the std arm holds the `OsStr` bytes) and are split into tree
// components PAL-side (the 4.2 seam). A `< 0` return is a raw fs code the std arm
// wraps via `io::Error::from_raw_os_error` (its kind from `__eunomia_io_classify`).

/// Read up to `buf.len()` bytes of `path` at `offset`. Returns bytes read (0 = EOF)
/// or a negative fs code.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_read(path: &[u8], offset: u64, buf: &mut [u8]) -> i64 {
    fs::read(path, offset, buf)
}

/// Write all of `data` to `path` at `offset` (creating it). Returns bytes written or
/// a negative fs code.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_write(path: &[u8], offset: u64, data: &[u8]) -> i64 {
    fs::write(path, offset, data)
}

/// The size of the file at `path`, or a negative fs code (`ERR_FS_NOT_FOUND` if absent).
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_stat(path: &[u8]) -> i64 {
    fs::stat(path)
}

/// Directory-aware metadata (kind + size) for the `sys/fs` `stat`/`lstat`/`file_attr`
/// arm (std-port 4.3): [`fs::Meta`] `{ code, size, is_dir }`, `#[repr(C)]` so it crosses
/// the seam with a fixed layout the std side mirrors. All probe logic (Stat then List)
/// lives in `crate::fs`; this arm only forwards.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_metadata(path: &[u8]) -> fs::Meta {
    fs::metadata(path)
}

/// Rename `from` to `to`. `0` or a negative fs code.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_rename(from: &[u8], to: &[u8]) -> i64 {
    fs::rename(from, to)
}

/// Remove the file at `path`. `0` or a negative fs code.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_unlink(path: &[u8]) -> i64 {
    fs::unlink(path)
}

/// Flush the ref durably (`fsync`/`sync_all`). `0` or a negative fs code.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_sync() -> i64 {
    fs::sync()
}

/// Open a `read_dir` snapshot for `path`: run the `List` round-trip and stash the listing
/// behind an integer handle (`>= 0`), or return a negative fs code. The std `ReadDir`
/// walks it with `readdir_next` and releases it with `readdir_close`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_readdir_open(path: &[u8]) -> i64 {
    fs::readdir_open(path)
}

/// Copy the next entry of the `read_dir` snapshot `handle` into `name_buf` and return its
/// [`fs::DirEntMeta`] head (`code`: `0` = entry, `1` = end, `< 0` = fs code). `#[repr(C)]`
/// so the head crosses the seam with a fixed layout the std side mirrors.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_readdir_next(handle: i64, name_buf: &mut [u8]) -> fs::DirEntMeta {
    fs::readdir_next(handle, name_buf)
}

/// Release the `read_dir` snapshot `handle` (the std `ReadDir` drop).
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_fs_readdir_close(handle: i64) {
    fs::readdir_close(handle)
}

/// A static human-readable message for a raw syscall error code, for `error_string`.
#[unsafe(no_mangle)]
pub extern "Rust" fn __eunomia_io_message(code: i64) -> &'static str {
    io_error::message(code)
}
