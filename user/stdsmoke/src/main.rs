// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! The std runtime GATE fixture: the first *live* `std` binary on Eunomia. It
//! exercises the core std runtime surfaces — entry/argv/env, GlobalAlloc,
//! stdio→debug-log, time — end to end and prints a green-boot marker
//! (`STD2 PASS`, the `…M1 PASS` style) the boot harness
//! greps (`scripts/std-smoke-test.sh`).
//!
//! It is a real std program — no `#![no_std]`, no `#![no_main]`, no
//! `#[panic_handler]`. std owns `_start` (the eunomia PAL, rev2§5.1) and the
//! panic handler. `extern crate eunomia_sys;` is the one non-obvious line: it
//! forces the seam rlib into the link so the linker resolves the PAL's undefined
//! `__eunomia_*` `extern "Rust"` symbols against eunomia-sys's `#[no_mangle]`
//! definitions (the `__rust_alloc` pattern; the first std user binary must do
//! this).
//!
//! Argument `argv[1]` selects an arm: `panic` drives the std-owned panic path
//! (panic → `abort_internal` → `__eunomia_thread_exit(STATUS_PANIC)`, the
//! panic-status override) so the parent shell reaps `panicked`, not `exited(_)`;
//! `spawn`/`sync` exercise threads and locks; `hashmap` exercises
//! `HashMap` over the per-process entropy DRBG — building a default-hasher
//! map draws `hashmap_random_keys` → `fill_bytes` → `urt::random`, seeded from
//! the `NAME_RANDOM_SEED` grant the shell hands each child. A process not granted
//! a seed would abort loudly here rather than hash predictably. `env` exercises the
//! inherited environment: `env::var`/`env::vars` read the values init defines
//! and the shell forwards, and `env::temp_dir()` resolves from `TMPDIR`.

extern crate eunomia_sys; // links the PAL↔seam bridge (see module doc)

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 2020-01-01T00:00:00Z in Unix seconds. The granted time page is host-synced
/// (rev2§2.6), so a real `SystemTime::now()` is well past this; a reading below
/// it means the time grant never attached or is garbage.
const Y2020_SECS: u64 = 1_577_836_800;

fn main() {
    // stdio: every line below rides `println!` → debug-log → the serial
    // log the harness greps. The `[stdsmoke]` prefix keeps the markers from
    // colliding with kernel/shell/storaged output on the shared console.
    println!("[stdsmoke] alive");

    // argv/env: the shell delivers the command line as the startup block's
    // argv; `argv[0]` is the path. Collecting into a `Vec<String>` also exercises
    // the allocator and `String`.
    let args: Vec<String> = std::env::args().collect();
    println!("[stdsmoke] argv={args:?}");

    // The deliberate panic path: std's own handler must terminate as
    // STATUS_PANIC so the parent distinguishes a crash from a clean exit.
    if args.get(1).map(String::as_str) == Some("panic") {
        println!("[stdsmoke] panicking");
        panic!("stdsmoke deliberate panic");
    }

    // the thread-spawn path. Spawn two threads that each allocate in a
    // tight loop, forcing simultaneous access to the one process heap — the concurrent
    // allocation the heap spinlock serializes (Loom-certified; here the on-target
    // witness). Each reads `thread::current().id()`, which lives in the per-thread
    // `TPIDR_EL0` TLS: distinct ids across the two threads prove the storage
    // is genuinely per-thread, not the process-global it was before real TLS. Join
    // both, check the results, and confirm the ids differ.
    if args.get(1).map(String::as_str) == Some("spawn") {
        use std::thread;
        println!("[stdsmoke] spawning threads");
        let handles: Vec<thread::JoinHandle<(u64, thread::ThreadId)>> = (0..2u64)
            .map(|id| {
                thread::spawn(move || {
                    let mut acc: u64 = 0;
                    for i in 0..500u64 {
                        // Heap churn: a fresh Vec + String every iteration.
                        let v: Vec<u64> = (0..64u64)
                            .map(|x| x.wrapping_add(i).wrapping_add(id))
                            .collect();
                        acc = acc.wrapping_add(v.iter().copied().sum::<u64>());
                        let s = format!("t{id}-{i}");
                        acc = acc.wrapping_add(s.len() as u64);
                    }
                    // The current-thread handle lives in per-thread TLS.
                    (acc, thread::current().id())
                })
            })
            .collect();
        let mut total: u64 = 0;
        let mut ids: Vec<thread::ThreadId> = Vec::new();
        for h in handles {
            let (acc, tid) = h.join().expect("thread join failed");
            total = total.wrapping_add(acc);
            ids.push(tid);
        }
        // A nonzero total proves both threads ran their allocation loops to completion.
        if total == 0 {
            println!("[stdsmoke] spawn-bad total=0");
            std::process::exit(5);
        }
        // Distinct ids prove per-thread TLS: a shared (global) current-thread handle
        // would give both the same id (and, before real TLS, an abort at spawn).
        if ids[0] == ids[1] {
            println!("[stdsmoke] spawn-bad shared-tls-id");
            std::process::exit(6);
        }
        println!("[stdsmoke] threads joined total={total} distinct-tls-ids");
        println!("STD32 PASS");
        return;
    }

    // the lock stack over `sys::futex`. Two threads alternate turns
    // incrementing a shared counter, each guarding it with a `Mutex` and blocking on
    // a `Condvar` until its parity comes up — real cross-thread `futex_wait`/
    // `futex_wake` (the *blocking* path, not just the uncontended CAS fast path). If
    // the futex backend lost a wakeup, a thread would park forever and the join would
    // hang; a wrong count would mean lost/duplicated critical sections. This is the
    // on-target witness for the whole Mutex/Condvar stack the Loom/Shuttle models
    // certify in the abstract.
    if args.get(1).map(String::as_str) == Some("sync") {
        use std::sync::{Arc, Condvar, Mutex};
        use std::thread;
        println!("[stdsmoke] sync start");
        const ROUNDS: u64 = 50;
        // (counter, condvar): thread `me` acts only when `counter % 2 == me`, so the
        // two strictly alternate and each performs exactly ROUNDS increments.
        let shared = Arc::new((Mutex::new(0u64), Condvar::new()));
        let handles: Vec<thread::JoinHandle<()>> = (0..2u64)
            .map(|me| {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    let (lock, cv) = &*shared;
                    for _ in 0..ROUNDS {
                        let mut counter = lock.lock().unwrap();
                        // Not my turn → block on the condvar (releases the mutex).
                        while *counter % 2 != me {
                            counter = cv.wait(counter).unwrap();
                        }
                        *counter += 1;
                        cv.notify_all();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("sync thread join failed");
        }
        let total = *shared.0.lock().unwrap();
        let expected = 2 * ROUNDS;
        if total != expected {
            println!("[stdsmoke] sync-bad total={total} expected={expected}");
            std::process::exit(7);
        }
        println!("[stdsmoke] sync done total={total}");
        println!("STD33 PASS");
        return;
    }

    // the entropy path via `HashMap`. Building a default-hasher map
    // constructs `RandomState`, which calls `hashmap_random_keys` → `fill_bytes` →
    // the per-process DRBG (`urt::random`) seeded from the `NAME_RANDOM_SEED` grant
    // the shell handed this child. An unseeded process aborts loudly here (the
    // no-seed posture); a correctly-provisioned one hashes and looks up normally.
    // This is the on-target witness that the whole seed-grant → DRBG → SipHash path
    // works end to end, unblocking `HashMap` for real std binaries.
    if args.get(1).map(String::as_str) == Some("hashmap") {
        use std::collections::HashMap;
        println!("[stdsmoke] hashmap start");
        let mut m: HashMap<String, u64> = HashMap::new();
        for i in 0..1000u64 {
            m.insert(format!("k{i}"), i.wrapping_mul(i));
        }
        // Every inserted key reads back its value (the hasher round-trips).
        let mut sum: u64 = 0;
        for i in 0..1000u64 {
            match m.get(&format!("k{i}")) {
                Some(&v) if v == i.wrapping_mul(i) => sum = sum.wrapping_add(v),
                other => {
                    println!("[stdsmoke] hashmap-bad k{i}={other:?}");
                    std::process::exit(8);
                }
            }
        }
        // A key never inserted is absent (no phantom hit from a broken hasher).
        if m.get("absent").is_some() {
            println!("[stdsmoke] hashmap-bad phantom-hit");
            std::process::exit(9);
        }
        if m.len() != 1000 {
            println!("[stdsmoke] hashmap-bad len={}", m.len());
            std::process::exit(10);
        }
        println!("[stdsmoke] hashmap done entries={} sum={sum}", m.len());
        println!("STD34 PASS");
        return;
    }

    // real `thread_local!` macro storage + destructors. A spawned
    // thread touches two `thread_local!`s — a `Drop` sentinel and a per-thread
    // `Cell` — then exits. Two things must hold that the old single-threaded
    // `no_threads` storage got wrong: (a) the sentinel's destructor **runs on the
    // spawned thread's exit** (the key-based teardown — `DROPS` bumps), and (b)
    // the `Cell` is **genuinely per-thread** — the child sets its own to 7 while the
    // main thread's stays 0 (shared `no_threads` storage would leak the 7 across).
    // This is the on-target witness for the verified `urt::tls` key table + the
    // trampoline dtor runner.
    if args.get(1).map(String::as_str) == Some("tls") {
        use std::cell::Cell;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;
        println!("[stdsmoke] tls start");

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        struct Sentinel;
        impl Drop for Sentinel {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        thread_local! {
            static MARKER: Sentinel = Sentinel;
            static COUNTER: Cell<u64> = Cell::new(0);
        }

        // The child inits its thread_local!s (first touch), mutates its own COUNTER,
        // and returns the value it observes.
        let child = thread::spawn(|| {
            MARKER.with(|_| {});
            COUNTER.with(|c| c.set(c.get() + 7));
            COUNTER.with(|c| c.get())
        });
        let child_counter = child.join().expect("tls thread join failed");

        // (a) The child's MARKER destructor ran at its exit — join returns only after
        // the trampoline's `run_dtors`, so `DROPS` is already bumped here.
        let drops = DROPS.load(Ordering::SeqCst);
        if drops == 0 {
            println!("[stdsmoke] tls-bad no-dtor drops=0");
            std::process::exit(11);
        }
        // (b) The child saw its own per-thread COUNTER (7), not a shared global.
        if child_counter != 7 {
            println!("[stdsmoke] tls-bad child-counter={child_counter}");
            std::process::exit(11);
        }
        // The main thread's COUNTER is independent — 0, never having touched it.
        // Shared `no_threads` storage would surface the child's 7 here.
        let main_counter = COUNTER.with(|c| c.get());
        if main_counter != 0 {
            println!("[stdsmoke] tls-bad main-counter={main_counter}");
            std::process::exit(11);
        }
        println!(
            "[stdsmoke] tls done drops={drops} child_counter={child_counter} main_counter={main_counter}"
        );
        println!("STD35 PASS");
        return;
    }

    // the environment path. The shell forwards its inherited env (from
    // init: `PATH=/bin`, `TMPDIR=/tmp`, `TERM=eunomia`) into this child's startup
    // block, so `env::var`/`env::vars` read real values and `env::temp_dir()` returns
    // a sane path instead of panicking. Asserting `PATH`/`TERM` — not just `TMPDIR` —
    // is deliberate: `/tmp` is both the `TMPDIR` value and the `temp_dir` fallback, so a
    // non-TMPDIR var is what proves inheritance actually happened, not the fallback.
    if args.get(1).map(String::as_str) == Some("env") {
        use std::path::Path;
        println!("[stdsmoke] env start");

        // Each inherited var reads back its exact value.
        for (key, want) in [("PATH", "/bin"), ("TMPDIR", "/tmp"), ("TERM", "eunomia")] {
            match std::env::var(key) {
                Ok(v) if v == want => {}
                other => {
                    println!("[stdsmoke] env-bad {key}={other:?} want={want:?}");
                    std::process::exit(12);
                }
            }
        }

        // `env::vars()` enumerates the whole inherited environment (non-empty, and it
        // contains the keys above).
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let has = |k: &str| vars.iter().any(|(vk, _)| vk == k);
        if vars.is_empty() || !has("PATH") || !has("TMPDIR") || !has("TERM") {
            println!("[stdsmoke] env-bad vars={vars:?}");
            std::process::exit(13);
        }

        // `env::temp_dir()` resolves from `TMPDIR` (the eunomia paths arm) — no longer
        // a panic. It must equal the inherited `/tmp`.
        let tmp = std::env::temp_dir();
        if tmp != Path::new("/tmp") {
            println!("[stdsmoke] env-bad tmp={tmp:?}");
            std::process::exit(14);
        }

        println!(
            "[stdsmoke] env ok vars={} tmp={}",
            vars.len(),
            tmp.display()
        );
        println!("STD52 PASS");
        return;
    }

    // alloc: Vec growth + Box, with a checked value the harness asserts.
    let v: Vec<u64> = (1..=100).collect();
    let sum: u64 = v.iter().sum();
    if sum != 5050 {
        println!("[stdsmoke] vec-bad sum={sum}");
        std::process::exit(2);
    }
    let boxed: Box<u64> = Box::new(sum * 2);
    // format!/String: a heap-built string, then printed.
    let s = format!("box={} argc={}", boxed, args.len());
    println!("[stdsmoke] vec sum={sum} {s}");

    // Instant: the grant-free monotonic counter (CNTVCT). Assert ordering
    // rather than a nonzero delta — the virtual counter is coarse, so a tiny
    // workload can fall inside one tick; ordering is the robust invariant.
    let t0 = Instant::now();
    let mut acc = 0u64;
    for i in 0..200_000u64 {
        acc = acc.wrapping_add(i);
    }
    std::hint::black_box(acc);
    let t1 = Instant::now();
    if t1 < t0 {
        println!("[stdsmoke] instant-bad");
        std::process::exit(3);
    }
    println!(
        "[stdsmoke] instant-ok ns={}",
        t1.duration_since(t0).as_nanos()
    );

    // SystemTime: the rev2§2.6 time page the shell grants every child. A
    // post-2020 wall clock proves the grant attached and the tick→ns conversion
    // works in a spawned std process, not just in the shell.
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) if d.as_secs() > Y2020_SECS => println!("[stdsmoke] systemtime-ok"),
        other => {
            println!("[stdsmoke] systemtime-bad {other:?}");
            std::process::exit(4);
        }
    }

    // The green-boot marker. Reached only if every arm above succeeded.
    println!("STD2 PASS");
}
