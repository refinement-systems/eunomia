//! A yielding spinlock — the process-global heap's mutual-exclusion primitive
//! once in-process threads exist (rev2§5.3).
//!
//! Before threading, `urt::Heap` was `Sync` "by construction" (a single-threaded
//! process never races its own allocator). `thread::spawn` breaks that: the
//! parent's `Box::into_raw` of the closure, the child's `ThreadInit::init`
//! allocation, and any user/std allocation on either side hit the one process
//! `static HEAP` concurrently. This lock serializes the free-list critical
//! section so the arithmetic (the Verus-verified `freelist`) sees exclusive
//! access.
//!
//! **Verification routing (rev2§6, `doc/guidelines/verification.md`).** The lock is
//! a raw `AtomicU32` acquired/released through an `Acquire`/`Release` pair — its
//! correctness is a *concurrency interleaving*, not arithmetic, so it is
//! **Loom-certifying, never Verus**: the version-pinned Verus ghost atomics are
//! SeqCst-only, so a Verus proof would certify a different binary than the
//! `Acquire`/`Release` one that ships. `loom_tests` is the proof of record (two
//! threads, one shared non-atomic cell — mutual exclusion holds under every
//! interleaving; a lock-removal control makes Loom flag the race), with a Shuttle
//! breadth twin. This is the same tier as the futex bucket lock, which reuses
//! this primitive.
//!
//! **Priority-inversion mitigation (rev2§5.4).** The acquire loop *yields* (the
//! `Yield` syscall, op 2) rather than pure-spinning, so a same/comparable-priority
//! holder preempted by the tick still makes progress under round-robin. The
//! standing caveat (a high-priority acquirer waiting on a strictly-lower-priority
//! holder is not cured by yield under strict fixed priority) is bounded here: the
//! heap hold is a handful of instructions and, in the common single-threaded case,
//! uncontended.

// The atomic comes through the same cfg-selected seam as `time.rs`: `--cfg loom`
// (the certifying model) gets loom's instrumented atomic, `--cfg shuttle` the
// breadth-smoke's, every normal/aarch64 build the real one.
#[cfg(all(not(loom), not(shuttle)))]
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU32, Ordering};
#[cfg(shuttle)]
use shuttle::sync::atomic::{AtomicU32, Ordering};

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;

/// The acquire-loop backoff rides a four-way seam. Under loom/shuttle it yields to
/// the model scheduler (a raw `spin_loop` is opaque to them — it blows loom's
/// branch budget and never preempts under shuttle). On the real target it issues
/// the `Yield` syscall (op 2, the rev2§5.4 mitigation above). A host non-model
/// build (plain `cargo test`) keeps the CPU hint — it must not reach the `svc`
/// shell, which is a stub off-target.
#[cfg(loom)]
#[inline]
fn backoff() {
    loom::thread::yield_now();
}
#[cfg(shuttle)]
#[inline]
fn backoff() {
    shuttle::thread::yield_now();
}
#[cfg(all(not(loom), not(shuttle), bare_metal))]
#[inline]
fn backoff() {
    ipc::sys::yield_now();
}
#[cfg(all(not(loom), not(shuttle), not(bare_metal)))]
#[inline]
fn backoff() {
    core::hint::spin_loop();
}

/// A raw yielding spinlock. `UNLOCKED == 0`, so an all-zero `SpinLock` is the
/// unlocked state — a `static Heap` with a `SpinLock` field stays in `.bss` (the
/// loader zeroes it with the RW segment), the property the `.bss` heap depends on.
pub struct SpinLock {
    locked: AtomicU32,
}

impl SpinLock {
    // loom's / shuttle's `AtomicU32::new` is not `const`, so a model build drops
    // `const`; the body is identical. The real lock lives in the `.bss` `static
    // Heap`, where const construction (all-zero = unlocked) is load-bearing.
    #[cfg(all(not(loom), not(shuttle)))]
    pub const fn new() -> SpinLock {
        SpinLock {
            locked: AtomicU32::new(UNLOCKED),
        }
    }

    #[cfg(any(loom, shuttle))]
    pub fn new() -> SpinLock {
        SpinLock {
            locked: AtomicU32::new(UNLOCKED),
        }
    }

    /// Acquire, returning an RAII [`Guard`] that releases on drop. `Acquire` on the
    /// winning CAS pairs with the `Release` in `Guard::drop`, so the critical
    /// section's reads/writes cannot leak past either edge (the ordering the Loom
    /// model certifies; a `Relaxed` failure ordering is correct — a failed CAS
    /// synchronizes with nothing).
    #[inline]
    pub fn lock(&self) -> Guard<'_> {
        while self
            .locked
            .compare_exchange_weak(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            backoff();
        }
        Guard { lock: self }
    }
}

impl Default for SpinLock {
    fn default() -> Self {
        Self::new()
    }
}

/// The lock guard: holds exclusive access for its lifetime and releases on drop.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct Guard<'a> {
    lock: &'a SpinLock,
}

impl Drop for Guard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.locked.store(UNLOCKED, Ordering::Release);
    }
}

// A plain single-threaded smoke: lock, release on drop, re-acquire. The
// interleaving property is `loom_tests`' job; this only covers the uncontended
// acquire/release path (and that `new()` is usable).
#[cfg(all(test, not(loom), not(shuttle)))]
mod tests {
    use super::*;

    #[test]
    fn acquire_release_reacquire() {
        let lock = SpinLock::new();
        {
            let _g = lock.lock();
        } // released here
        let _g = lock.lock(); // re-acquires: a stuck release would deadlock
    }
}

/// The certifying model: two threads each take the lock and increment one shared
/// **non-atomic** cell; mutual exclusion holds under every interleaving iff the
/// final value is exactly 2 (a lost update ⇒ non-exclusion). Loom's instrumented
/// `UnsafeCell` also flags the data race directly. Red control (run manually):
/// dropping the `lock()` call makes both the assertion and Loom's cell-race
/// detector fire. Run with `RUSTFLAGS="--cfg loom" cargo test -p urt --lib`.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::thread;

    #[test]
    fn mutual_exclusion_under_any_interleaving() {
        loom::model(|| {
            let lock = Arc::new(SpinLock::new());
            let data = Arc::new(UnsafeCell::new(0u32));

            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let lock = Arc::clone(&lock);
                    let data = Arc::clone(&data);
                    thread::spawn(move || {
                        let _g = lock.lock();
                        // SAFETY: the guard makes this the sole live accessor.
                        data.with_mut(|p| unsafe { *p += 1 });
                    })
                })
                .collect();

            for h in handles {
                h.join().unwrap();
            }

            let total = data.with(|p| unsafe { *p });
            assert_eq!(total, 2, "lost update: heap spinlock not exclusive");
        });
    }
}

/// Shuttle breadth twin of `loom_tests` — a second scheduler over the same
/// two-thread critical section. NON-CERTIFYING (Shuttle reinterprets the
/// Acquire/Release as SeqCst), a randomized deadlock/logic smoke and the template
/// for the futex work. Run with `RUSTFLAGS="--cfg shuttle" cargo test -p urt
/// --lib`.
#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use shuttle::sync::Arc;
    use shuttle::thread;
    use std::cell::UnsafeCell;

    #[test]
    fn mutual_exclusion_under_random_schedules() {
        shuttle::check_random(
            || {
                struct Cell(UnsafeCell<u32>);
                // SAFETY: exclusivity is provided by the SpinLock under test.
                unsafe impl Sync for Cell {}

                let lock = Arc::new(SpinLock::new());
                let data = Arc::new(Cell(UnsafeCell::new(0u32)));

                let handles: Vec<_> = (0..2)
                    .map(|_| {
                        let lock = Arc::clone(&lock);
                        let data = Arc::clone(&data);
                        thread::spawn(move || {
                            let _g = lock.lock();
                            // SAFETY: the guard makes this the sole live accessor.
                            unsafe { *data.0.get() += 1 };
                        })
                    })
                    .collect();

                for h in handles {
                    h.join().unwrap();
                }

                let total = unsafe { *data.0.get() };
                assert_eq!(total, 2, "lost update: heap spinlock not exclusive");
            },
            1000,
        );
    }
}
