//! A userspace `sys::futex` backend: the one primitive upstream
//! std needs to light `Mutex`/`Condvar`/`RwLock`/`Once`/`Parker`. We write no lock
//! logic — upstream's futex impls come free — only the three futex functions over a
//! process-global address→waiter table.
//!
//! **The emulation.** A futex is any `&AtomicU32`; a thread "waits" on one until
//! another thread "wakes" it. There is no kernel futex, so each waiter parks on its
//! *own* per-thread notification ([`crate::thread::current_park_notif`]): a bucket
//! table keyed by the futex address holds `(addr, park-notif)` for each parked
//! waiter (guarded by the yielding [`SpinLock`]), a `futex_wake` scans it for
//! the address and signals one waiter's notif. A shared notif could not do this —
//! the kernel delivers a notification's whole word to *one* FIFO waiter and clears
//! it, so waking a *specific* waiter needs a per-thread object.
//!
//! **No lost wakeup.** `futex_wait` re-checks `*addr == expected` **under the bucket
//! lock** before enqueuing; a waker stores the new value **before** it takes the
//! same lock to dequeue. The lock's Acquire/Release orders the waker's store before
//! a later waiter's load, and serializes enqueue against dequeue — so either the
//! waiter observes the change and never parks, or it is enqueued and the waker finds
//! it. The kernel `notif_wait` additionally checks the accumulated word before
//! sleeping, so a signal that races ahead of `notif_wait` still wakes. The waker
//! dequeues each waiter exactly once (under the lock) and then signals, so the
//! waiter never needs to remove itself.
//!
//! **Verification routing (rev2§6, `doc/guidelines/verification.md`).** The
//! wait/wake protocol is a *concurrency interleaving* over `Acquire`/`Release`
//! atomics and a notification, not arithmetic — so it is **Loom-certifying, never
//! Verus** (the version-pinned Verus ghost atomics are SeqCst-only; a proof would
//! certify a different binary), the same tier as the heap spinlock it reuses.
//! The `futex_no_lost_wakeup` model (below) drives the real table under Loom
//! (exhaustive) and Shuttle (randomized) over a mock parker, and the abstract
//! recheck-before-block discipline is the same one `tla/ipc_reactor` model-checks.
//!
//! **Timeouts** are the MVP yield-poll (the process holds no timer cap): every
//! `Some(timeout)` caller (condvar `wait_timeout`, `park_timeout`) mutates the word
//! before its wake, so a non-enqueued waiter that polls the word to a deadline is
//! correct. Busy; timer-bit blocking is deferred.

// The futex WORD atomic rides the same three-way cfg seam as `lock.rs`/`time.rs`:
// loom/shuttle get the model atomics, every other build `core`'s (which is what
// std hands us on the target — `Atomic<u32>` == `AtomicU32`).
#[cfg(all(not(loom), not(shuttle)))]
use core::sync::atomic::{AtomicU32, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{AtomicU32, Ordering};
#[cfg(shuttle)]
use shuttle::sync::atomic::{AtomicU32, Ordering};

use crate::lock::SpinLock;
use core::cell::UnsafeCell;

/// Max simultaneous waiters: one per live thread, since a thread waits on at most
/// one futex at a time — the [`crate::thread`] pool plus the main thread.
const MAX_WAITERS: usize = crate::thread_layout::MAX_THREADS + 1;

// ── The park primitive seam ──────────────────────────────────────────────
// Real target: a waiter blocks on its per-thread kernel notification. Model/host
// test: a per-thread `Parker` (word + condvar) — the `kcore::notification` /
// `ipc::model::ModelTransport` shape — so Loom/Shuttle can schedule the wait/wake
// without the target-only `urt::thread` / `svc` shell. The module is compiled only
// under `any(test, <target>)` (see `lib.rs`), so exactly one arm is ever active.

#[cfg(all(not(test), bare_metal))]
mod park {
    /// The park token is the thread's own futex park-notif cspace slot.
    pub type Token = u32;

    /// The bit a futex wake raises in the park-notif word (private to one thread,
    /// so any nonzero bit means "woken").
    const WAKE_BIT: u64 = 1;

    /// This thread's own park-notif slot, or `Err` (unconfigured / exhausted) — the
    /// caller then degrades to a yield-poll rather than a bogus syscall.
    pub fn current() -> Result<Token, i64> {
        crate::thread::current_park_notif()
    }

    /// Block until signaled. `notif_wait` checks the accumulated word before
    /// sleeping, so a wake that raced ahead returns immediately (the lost-wakeup
    /// guard).
    pub fn wait(tok: &Token) {
        let _ = ipc::sys::notif_wait(*tok);
    }

    /// Wake the thread owning `tok`.
    pub fn wake(tok: Token) {
        let _ = ipc::sys::notif_signal(tok, WAKE_BIT);
    }
}

#[cfg(test)]
mod park {
    // The concurrency seam (mirrors `ipc/src/sync.rs`): std by default (host
    // `cargo test`), loom/shuttle under their cfgs — all compiled only under test.
    #[cfg(loom)]
    use loom::sync::{Arc, Condvar, Mutex};
    #[cfg(shuttle)]
    use shuttle::sync::{Arc, Condvar, Mutex};
    #[cfg(all(not(loom), not(shuttle)))]
    use std::sync::{Arc, Condvar, Mutex};

    /// A per-thread parker modeling the kernel notification: a `word` (source of
    /// truth) plus a condvar (wake mechanism). `wait` checks the word before
    /// blocking — the same lost-wakeup guard as `kcore::notification` and the
    /// `ipc::model::ModelTransport` this mirrors.
    pub struct Parker {
        word: Mutex<u32>,
        cv: Condvar,
    }

    impl Parker {
        fn new() -> Parker {
            Parker {
                word: Mutex::new(0),
                cv: Condvar::new(),
            }
        }
    }

    pub type Token = Arc<Parker>;

    #[cfg(all(not(loom), not(shuttle)))]
    std::thread_local! {
        static SELF_PARK: Arc<Parker> = Arc::new(Parker::new());
    }
    #[cfg(loom)]
    loom::thread_local! {
        static SELF_PARK: Arc<Parker> = Arc::new(Parker::new());
    }
    #[cfg(shuttle)]
    shuttle::thread_local! {
        static SELF_PARK: Arc<Parker> = Arc::new(Parker::new());
    }

    /// This thread's parker (infallible in the model — the `Result` matches the
    /// target arm so the table code is cfg-uniform).
    pub fn current() -> Result<Token, i64> {
        Ok(SELF_PARK.with(|p| p.clone()))
    }

    pub fn wait(tok: &Token) {
        let mut w = tok.word.lock().unwrap();
        while *w == 0 {
            w = tok.cv.wait(w).unwrap();
        }
        *w = 0;
    }

    pub fn wake(tok: Token) {
        let mut w = tok.word.lock().unwrap();
        *w = 1;
        tok.cv.notify_one();
    }
}

use park::Token;

// ── The bucket table ─────────────────────────────────────────────────────

/// One parked waiter: the futex address it waits on (`0` = empty — a real
/// `&AtomicU32` is never null) and the token to signal it with.
struct Entry {
    addr: usize,
    token: Option<Token>,
}

impl Entry {
    // A const item (not a `Copy` value): valid to repeat into an array even when
    // `Token` is the non-`Copy` model `Arc<Parker>`, and const on the target so the
    // `static TABLE` lands in `.bss`.
    const EMPTY: Entry = Entry {
        addr: 0,
        token: None,
    };
}

struct Inner {
    waiters: [Entry; MAX_WAITERS],
}

/// The process-global futex table, guarded by its own [`SpinLock`] (distinct from
/// the heap's and the thread pool's — the only lock a `futex_wait`/`wake` holds, and
/// it is released before any blocking `notif_wait`, so no lock-ordering hazard).
struct FutexTable {
    lock: SpinLock,
    inner: UnsafeCell<Inner>,
}

// SAFETY: every access to `inner` is under `lock` (mutual exclusion, Loom-certified
// in `lock.rs`); the `UnsafeCell` interior is never reached by two threads at once.
unsafe impl Sync for FutexTable {}

impl FutexTable {
    // loom's/shuttle's `AtomicU32::new` (behind `SpinLock::new`) is not `const`, so
    // a model build drops `const`; the body is identical. The real table is the
    // `.bss` `static TABLE`, where const construction (all-zero) is load-bearing.
    #[cfg(all(not(loom), not(shuttle)))]
    const fn new() -> FutexTable {
        FutexTable {
            lock: SpinLock::new(),
            inner: UnsafeCell::new(Inner {
                waiters: [Entry::EMPTY; MAX_WAITERS],
            }),
        }
    }

    #[cfg(any(loom, shuttle))]
    fn new() -> FutexTable {
        FutexTable {
            lock: SpinLock::new(),
            inner: UnsafeCell::new(Inner {
                waiters: [Entry::EMPTY; MAX_WAITERS],
            }),
        }
    }

    /// Block until woken, unless `*futex != expected` already. Always returns `true`
    /// (an untimed wait either observed the change or was woken); the caller
    /// re-checks its own condition. See the module doc for the no-lost-wakeup guard.
    fn wait_block(&self, futex: &AtomicU32, expected: u32) -> bool {
        let addr = futex as *const AtomicU32 as usize;
        let tok = match park::current() {
            Ok(t) => t,
            // Unconfigured / exhausted: degrade to a yield-poll (never a bogus
            // syscall, never UB). Unreachable in practice — a wait implies
            // contention implies live threads implies a configured process.
            Err(_) => return fallback_poll(futex, expected),
        };
        {
            let _g = self.lock.lock();
            // The word-check under the lock: if the value already moved, do not
            // park (the waker's store is ordered before this load by the lock).
            if futex.load(Ordering::Relaxed) != expected {
                return true;
            }
            // SAFETY: exclusive under `lock`.
            let inner = unsafe { &mut *self.inner.get() };
            let mut placed = false;
            for e in inner.waiters.iter_mut() {
                if e.addr == 0 {
                    e.addr = addr;
                    e.token = Some(tok.clone());
                    placed = true;
                    break;
                }
            }
            if !placed {
                // Impossible: `MAX_WAITERS` = live threads + 1, and only a live
                // thread waits. Degrade to a spurious return (the caller re-checks);
                // never UB.
                debug_assert!(false, "urt futex: waiter table full");
                return true;
            }
        }
        // Block outside the lock. A wake dequeued us under the lock before signaling,
        // so on return our entry is already gone — no self-dequeue needed.
        park::wait(&tok);
        true
    }

    /// Remove and return one waiter parked on `futex`, if any (under the lock).
    fn dequeue_one(&self, futex: &AtomicU32) -> Option<Token> {
        let addr = futex as *const AtomicU32 as usize;
        let _g = self.lock.lock();
        // SAFETY: exclusive under `lock`.
        let inner = unsafe { &mut *self.inner.get() };
        for e in inner.waiters.iter_mut() {
            if e.addr == addr {
                e.addr = 0;
                return e.token.take();
            }
        }
        None
    }

    /// Wake one waiter parked on `futex`; `true` iff one was woken. Signals after
    /// releasing the lock (shorter hold; `notif_signal` is non-blocking anyway).
    fn wake_one(&self, futex: &AtomicU32) -> bool {
        match self.dequeue_one(futex) {
            Some(t) => {
                park::wake(t);
                true
            }
            None => false,
        }
    }

    /// Wake every waiter parked on `futex`. Drains one at a time (each dequeue under
    /// the lock, each signal after release); terminates because the waker's store
    /// stops new waiters enqueuing on this address.
    // Used by the target `futex_wake_all` and the std `wake_all_wakes_every_waiter`
    // test; the loom/shuttle model certifies only the wake-*one* interleaving.
    #[cfg_attr(any(loom, shuttle), allow(dead_code))]
    fn wake_all(&self, futex: &AtomicU32) {
        while let Some(t) = self.dequeue_one(futex) {
            park::wake(t);
        }
    }

    /// NEGATIVE CONTROL: the word-check is moved *outside* the bucket
    /// lock. The store-before-enqueue interleaving then loses the wakeup — the waker
    /// finds no entry and the waiter parks forever, a deadlock Loom/Shuttle flag.
    /// Enable with `--cfg futex_neg_control` (`RUSTFLAGS="--cfg loom --cfg
    /// futex_neg_control" cargo test -p urt --lib` must FAIL). The `tla/ipc_reactor`
    /// controls exercise the IPC poll-once path, not this futex recheck; this is its
    /// teeth.
    #[cfg(all(test, futex_neg_control))]
    fn wait_block_neg(&self, futex: &AtomicU32, expected: u32) -> bool {
        let addr = futex as *const AtomicU32 as usize;
        let tok = match park::current() {
            Ok(t) => t,
            Err(_) => return fallback_poll(futex, expected),
        };
        // BUG: the word-check is not under the bucket lock.
        if futex.load(Ordering::Relaxed) != expected {
            return true;
        }
        {
            let _g = self.lock.lock();
            // SAFETY: exclusive under `lock`.
            let inner = unsafe { &mut *self.inner.get() };
            for e in inner.waiters.iter_mut() {
                if e.addr == 0 {
                    e.addr = addr;
                    e.token = Some(tok.clone());
                    break;
                }
            }
        }
        park::wait(&tok);
        true
    }
}

/// The unconfigured/exhausted fallback for [`FutexTable::wait_block`]: on the target,
/// a busy yield-poll (never UB); in the model, unreachable (the parker is
/// infallible).
#[cfg(all(not(test), bare_metal))]
fn fallback_poll(futex: &AtomicU32, expected: u32) -> bool {
    loop {
        if futex.load(Ordering::Relaxed) != expected {
            return true;
        }
        ipc::sys::yield_now();
    }
}

#[cfg(test)]
fn fallback_poll(_futex: &AtomicU32, _expected: u32) -> bool {
    unreachable!("the model parker is infallible")
}

// ── The target free-function API (what `eunomia_sys::futex` calls) ────────

#[cfg(all(not(test), bare_metal))]
static TABLE: FutexTable = FutexTable::new();

/// Wait while `*futex == expected`. `timeout_ns == u64::MAX` means no timeout (block
/// on the notif); otherwise a yield-poll to the deadline. Returns `false` only on
/// timeout, `true` otherwise (the upstream `sys::futex` contract).
#[cfg(all(not(test), bare_metal))]
pub fn futex_wait(futex: &AtomicU32, expected: u32, timeout_ns: u64) -> bool {
    if timeout_ns == u64::MAX {
        TABLE.wait_block(futex, expected)
    } else {
        // MVP yield-poll (no timer cap). Correct because every timeout caller
        // mutates the word before its paired wake (see the module doc); busy.
        let deadline = crate::time::now_mono_ns().saturating_add(timeout_ns as i64);
        loop {
            if futex.load(Ordering::Relaxed) != expected {
                return true;
            }
            if crate::time::now_mono_ns() >= deadline {
                return false;
            }
            ipc::sys::yield_now();
        }
    }
}

/// Wake one waiter on `futex`; `true` iff one was woken.
#[cfg(all(not(test), bare_metal))]
pub fn futex_wake(futex: &AtomicU32) -> bool {
    TABLE.wake_one(futex)
}

/// Wake all waiters on `futex`.
#[cfg(all(not(test), bare_metal))]
pub fn futex_wake_all(futex: &AtomicU32) {
    TABLE.wake_all(futex)
}

// ── The Loom (certifying) + Shuttle (breadth) + std model ─────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // The harness's shared word/table use the concurrency seam (mirrors
    // `ipc/src/sync.rs`): std by default, loom/shuttle under their cfgs.
    #[cfg(loom)]
    use loom::sync::Arc;
    #[cfg(loom)]
    use loom::thread;
    #[cfg(shuttle)]
    use shuttle::sync::Arc;
    #[cfg(shuttle)]
    use shuttle::thread;
    #[cfg(all(not(loom), not(shuttle)))]
    use std::sync::Arc;
    #[cfg(all(not(loom), not(shuttle)))]
    use std::thread;

    /// The no-lost-wakeup model: a waiter parks on a shared word while a waker
    /// stores the new value and wakes it. Whatever the interleaving — store before
    /// enqueue, in-window, or after park — the waiter must terminate and observe the
    /// store (a lost wakeup deadlocks, which Loom/Shuttle report). The futex analogue
    /// of `ipc::model::reactor_no_lost_wakeup`. The negative control
    /// ([`FutexTable::wait_block_neg`]) makes it deadlock.
    fn futex_no_lost_wakeup() {
        let table = Arc::new(FutexTable::new());
        let word = Arc::new(AtomicU32::new(0));

        let tw = Arc::clone(&table);
        let ww = Arc::clone(&word);
        let waiter = thread::spawn(move || {
            #[cfg(not(futex_neg_control))]
            tw.wait_block(&ww, 0);
            #[cfg(futex_neg_control)]
            tw.wait_block_neg(&ww, 0);
            assert_eq!(
                ww.load(Ordering::Relaxed),
                1,
                "waiter woke but the word was never set (lost wakeup)"
            );
        });

        let tk = Arc::clone(&table);
        let wk = Arc::clone(&word);
        let waker = thread::spawn(move || {
            wk.store(1, Ordering::Release);
            tk.wake_one(&wk);
        });

        waiter.join().unwrap();
        waker.join().unwrap();
    }

    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn futex_no_lost_wakeup_std() {
        futex_no_lost_wakeup();
    }

    /// `wake_all` releases *every* waiter on an address (the upstream `Condvar
    /// notify_all` path). Two waiters park on one word; a single `wake_all` after the
    /// store must free both. Std-only (real threads): the multi-waiter breadth is not
    /// what Loom/Shuttle need to certify — the wake-one interleaving is.
    #[cfg(all(not(loom), not(shuttle)))]
    #[test]
    fn wake_all_wakes_every_waiter() {
        let table = Arc::new(FutexTable::new());
        let word = Arc::new(AtomicU32::new(0));

        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let t = Arc::clone(&table);
                let w = Arc::clone(&word);
                thread::spawn(move || {
                    t.wait_block(&w, 0);
                    assert_eq!(w.load(Ordering::Relaxed), 1);
                })
            })
            .collect();

        let tk = Arc::clone(&table);
        let wk = Arc::clone(&word);
        let waker = thread::spawn(move || {
            // Store then a single wake_all — the real waker shape. A waiter enqueued
            // by now is drained and signaled; one that enqueues later locks the
            // bucket after this wake_all released it, so its under-lock word-check
            // sees the store and it returns without parking. Either way both
            // terminate — no lost wakeup, no loop needed.
            wk.store(1, Ordering::Release);
            tk.wake_all(&wk);
        });

        waker.join().unwrap();
        for w in waiters {
            w.join().unwrap();
        }
    }

    /// Certifying: exhaustive over the Acquire/Release interleavings. Run with
    /// `RUSTFLAGS="--cfg loom" cargo test -p urt --lib`.
    #[cfg(loom)]
    #[test]
    fn futex_no_lost_wakeup_loom() {
        loom::model(futex_no_lost_wakeup);
    }

    /// Breadth twin (non-certifying). Run with `RUSTFLAGS="--cfg shuttle" cargo test
    /// -p urt --lib`.
    #[cfg(shuttle)]
    #[test]
    fn futex_no_lost_wakeup_shuttle() {
        shuttle::check_random(futex_no_lost_wakeup, 1000);
    }
}
