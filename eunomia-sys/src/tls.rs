// SPDX-License-Identifier: 0BSD
//! Per-thread TLS block + the key-based TLS backend.
//!
//! Eunomia has real per-thread TLS — `TPIDR_EL0` survives a context switch;
//! this points it at a zeroed `[*mut (); TLS_SLOTS]` block of pointer slots: the main
//! thread's `static` block in `_start`, each spawned thread's heap block in the
//! `sys/thread` trampoline. A TLS **key** is an index into that block; the global key
//! *allocation* + per-key destructor registry are the verified `urt::tls` key table,
//! while the per-thread *value* read/write (`get`/`set`) live here —
//! each thread touches only its own block, so they need no lock, just the block base.
//!
//! This backend is what std's key-based TLS (`vendor/rust`'s `sys/thread_local/os.rs`
//! + the `key/eunomia.rs` bridge) drives: `create`/`get`/`set`/`destroy` are the five
//! `key`-module symbols; `run_thread_dtors` + `free_thread_block` are the thread-exit
//! teardown the trampoline calls (eunomia owns thread exit, so it runs the registered
//! `thread_local!` destructors itself — the hermit posture — rather than relying on an
//! OS to iterate keys).
//!
//! Trusted asm shell (rev2§6.1(d)): the `msr`/`mrs tpidr_el0` and the per-thread block
//! pointer arithmetic are the userspace mirror of the kernel's trusted TLS-register
//! marshalling. No `verus!{}`; witnessed by the QEMU spawn/tls smoke.

#![cfg(bare_metal)]

use core::cell::UnsafeCell;
use core::ptr;

/// Pointer slots per thread block. **MUST equal `urt::tls::TLS_KEYS`** — a key is an
/// index in `[0, TLS_KEYS)` and would read past a shorter block. The `const _`
/// coupling below fails the build if the two ever drift.
pub const TLS_SLOTS: usize = 64;
const _: () = assert!(TLS_SLOTS == urt::tls::TLS_KEYS);

/// A TLS key: a **1-based** block index (`Key = 0` is reserved — std's `LazyKey`
/// uses `0` as its uninitialized sentinel, and a full table maps to it so the std
/// side aborts "out of TLS keys"). Key `k` ⇒ slot `k - 1` ⇒ `TPIDR[k - 1]`.
pub type Key = usize;

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

/// This thread's TLS block base — the `TPIDR_EL0` the seam set up (preserved
/// across context switches). A pointer to the first of `TLS_SLOTS` pointer slots.
#[inline]
fn tpidr_base() -> *mut *mut () {
    let v: usize;
    // SAFETY: `mrs tpidr_el0` is unconditionally readable at EL0; the seam guarantees
    // it points at a live block before any key access on this thread.
    unsafe {
        core::arch::asm!("mrs {v}, tpidr_el0", v = out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v as *mut *mut ()
}

/// Point the main thread's `TPIDR_EL0` at its static block. Called once by the PAL
/// `_start`, before `main` and before any key access. Idempotent.
pub fn init_main() {
    set_tpidr(MAIN.0.get() as *mut ());
}

/// Allocate a zeroed TLS block for a spawned thread and point its `TPIDR_EL0` at it.
/// Called by the `sys/thread` trampoline before `ThreadInit::init`. Reclaimed by
/// [`free_thread_block`] at thread exit.
pub fn init_thread() {
    // A boxed zeroed block over the process-global heap (needs no TLS itself).
    let b = alloc::boxed::Box::new([ptr::null_mut::<()>(); TLS_SLOTS]);
    set_tpidr(alloc::boxed::Box::into_raw(b) as *mut ());
}

// ── The key-based TLS backend (std's `os.rs` storage drives these) ──

/// Allocate a TLS key and register its destructor (std's `os::destroy_value::<T>`,
/// or `None` for a `local_pointer!`). `0` when the verified table is full — the std
/// bridge maps that to the "out of TLS keys" abort.
pub fn create(dtor: Option<urt::tls::Dtor>) -> Key {
    match urt::tls::create(dtor) {
        Some(i) => i as usize + 1,
        None => 0,
    }
}

/// This thread's value for `key` (the raw pointer std stored, or null/sentinel).
///
/// # Safety
/// `key` must come from [`create`] (so `1 <= key <= TLS_SLOTS`); reads only this
/// thread's own block.
pub unsafe fn get(key: Key) -> *mut u8 {
    // SAFETY: `key - 1 < TLS_SLOTS`; `tpidr_base()` is this thread's live block.
    unsafe { *tpidr_base().add(key - 1) as *mut u8 }
}

/// Store `val` as this thread's value for `key`.
///
/// # Safety
/// As [`get`].
pub unsafe fn set(key: Key, val: *mut u8) {
    // SAFETY: as `get`; writes only this thread's own block.
    unsafe { *tpidr_base().add(key - 1) = val as *mut () }
}

/// Free a TLS key (std's `LazyKey` destroys a redundant key it lost the race to
/// publish).
///
/// # Safety
/// `key` must come from [`create`] and be currently live.
pub unsafe fn destroy(key: Key) {
    urt::tls::destroy((key - 1) as u32)
}

/// Bounded rounds a value's destructor may re-arm across (a destructor that sets
/// another `thread_local!`). The POSIX `_POSIX_THREAD_DESTRUCTOR_ITERATIONS` value;
/// a value still live after this leaks (the same bound POSIX imposes).
const MAX_DTOR_ROUNDS: usize = 5;

/// Run this thread's `thread_local!` destructors at thread exit. For each key with a
/// registered destructor and a live per-thread value (`addr > 1`, past std's `os.rs`
/// null/`1` sentinels), call the destructor; repeat until a round runs none, up to
/// [`MAX_DTOR_ROUNDS`]. The registry is re-snapshotted each round so a value a
/// destructor creates is also dropped. Called by the trampoline / `_start` after the
/// thread body, before `free_thread_block`.
pub fn run_thread_dtors() {
    let base = tpidr_base();
    for _ in 0..MAX_DTOR_ROUNDS {
        let mut dtors: [Option<urt::tls::Dtor>; TLS_SLOTS] = [None; TLS_SLOTS];
        urt::tls::collect_dtors(&mut dtors);
        let mut ran = false;
        for (i, dtor) in dtors.iter().enumerate() {
            if let Some(d) = dtor {
                // SAFETY: `i < TLS_SLOTS`; `base` is this thread's live block.
                let v = unsafe { *base.add(i) };
                if v.addr() > 1 {
                    // SAFETY: `d` is std's `destroy_value` for this key, `v` its live
                    // value; `destroy_value` resets the slot to null when done.
                    unsafe { d(v as *mut u8) };
                    ran = true;
                }
            }
        }
        if !ran {
            break;
        }
    }
}

/// Reclaim a spawned thread's heap TLS block at thread exit. A no-op for the main thread
/// (its block is the static `.bss` `MAIN`). Called
/// last in the trampoline, after [`run_thread_dtors`] and `rt::thread_cleanup` — no
/// TLS access follows on this thread.
pub fn free_thread_block() {
    let base = tpidr_base();
    if base == MAIN.0.get() as *mut *mut () {
        return;
    }
    // SAFETY: a spawned thread's base is the `Box::into_raw` from `init_thread`;
    // reconstruct and drop it. Runs once, at exit, with nothing left to read it.
    unsafe {
        drop(alloc::boxed::Box::from_raw(
            base as *mut [*mut (); TLS_SLOTS],
        ));
    }
}
