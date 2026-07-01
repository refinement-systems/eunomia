//! Per-thread TLS block management (std-port 3.2): the `TPIDR_EL0`-based block std's
//! `local_pointer!` reads (`vendor/rust`'s `sys/thread_local/eunomia.rs`).
//!
//! std needs per-thread storage for the current-thread handle and id
//! (`set_current` refuses to run twice on one slot). 3.1 makes `TPIDR_EL0` survive a
//! context switch; this points it at a zeroed `[*mut (); TLS_SLOTS]` block: the main
//! thread's in `_start` (before `main`), each spawned thread's in the `sys/thread`
//! trampoline (before `ThreadInit::init`). std then uses `[TPIDR + slot]` per pointer.
//!
//! Trusted asm shell (rev2§6.1(d)): the `msr tpidr_el0` write is the userspace mirror
//! of the kernel's trusted TLS-register marshalling (3.1). No `verus!{}`.

#![cfg(any(target_os = "eunomia", target_os = "none"))]

use core::cell::UnsafeCell;
use core::ptr;

/// Pointer slots per thread block. **MUST match `TLS_SLOTS` in `vendor/rust`'s
/// `sys/thread_local/eunomia.rs`** — that side indexes `[0, TLS_SLOTS)` and would
/// read past a shorter block.
pub const TLS_SLOTS: usize = 64;

#[repr(C, align(16))]
struct Block(UnsafeCell<[*mut (); TLS_SLOTS]>);

// SAFETY: a `Block` is only ever pointed at by one thread's `TPIDR_EL0` at a time —
// the main static below by the main thread, a heap block by its one spawned thread.
unsafe impl Sync for Block {}

/// The main thread's block (there is exactly one main thread), zeroed in `.bss`.
static MAIN: Block = Block(UnsafeCell::new([ptr::null_mut(); TLS_SLOTS]));

#[inline]
fn set_tpidr(base: *mut ()) {
    // SAFETY: `msr tpidr_el0` sets this thread's TLS base (rev2§6.1(d)); 3.1
    // save/restores it across context switches. `nomem`: it touches no memory.
    unsafe {
        core::arch::asm!("msr tpidr_el0, {b}", b = in(reg) base, options(nomem, nostack));
    }
}

/// Point the main thread's `TPIDR_EL0` at its static block. Called once by the PAL
/// `_start`, before `main` and before any `local_pointer!` access. Idempotent.
pub fn init_main() {
    set_tpidr(MAIN.0.get() as *mut ());
}

/// Allocate a zeroed TLS block for a spawned thread and point its `TPIDR_EL0` at it.
/// Called by the `sys/thread` trampoline before `ThreadInit::init`. The block is
/// **leaked** for the thread's lifetime (an MVP bound, like `urt::thread`'s untyped:
/// bounded by lifetime spawn count; per-thread free is a follow-up).
pub fn init_thread() {
    // A boxed zeroed block over the process-global heap (needs no TLS itself).
    let b = alloc::boxed::Box::new([ptr::null_mut::<()>(); TLS_SLOTS]);
    set_tpidr(alloc::boxed::Box::into_raw(b) as *mut ());
}
