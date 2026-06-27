//! The std-port Phase-2 GATE fixture (findings 7-1): the first *live* `std`
//! binary on Eunomia. Phase 2's four sub-phases (entry/argv/env, GlobalAlloc,
//! stdio→debug-log, time) each deferred their live QEMU demonstration to this
//! combined gate; this binary exercises every one of them end to end and prints
//! a green-boot marker (`STD2 PASS`, the `…M1 PASS` style) the boot harness
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
//! What it deliberately does NOT touch: `HashMap`/`fill_bytes`/`std::random`.
//! The eunomia `sys/random` arm is `unsupported` until Phase 3.4 — `fill_bytes`
//! panics — so the gate stays clear of entropy. Argument `argv[1] == "panic"`
//! drives the std-owned panic path (panic → `abort_internal` →
//! `__eunomia_thread_exit(STATUS_PANIC)`, the Phase-2.3 override) so the parent
//! shell reaps `panicked`, not `exited(_)`.

extern crate eunomia_sys; // links the PAL↔seam bridge (see module doc)

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 2020-01-01T00:00:00Z in Unix seconds. The granted time page is host-synced
/// (rev2§2.6), so a real `SystemTime::now()` is well past this; a reading below
/// it means the time grant never attached or is garbage.
const Y2020_SECS: u64 = 1_577_836_800;

fn main() {
    // stdio (2.3): every line below rides `println!` → debug-log → the serial
    // log the harness greps. The `[stdsmoke]` prefix keeps the markers from
    // colliding with kernel/shell/storaged output on the shared console.
    println!("[stdsmoke] alive");

    // argv/env (2.1): the shell delivers the command line as the startup block's
    // argv; `argv[0]` is the path. Collecting into a `Vec<String>` also exercises
    // the allocator (2.2) and `String`.
    let args: Vec<String> = std::env::args().collect();
    println!("[stdsmoke] argv={args:?}");

    // The deliberate panic path: std's own handler must terminate as
    // STATUS_PANIC so the parent distinguishes a crash from a clean exit (2.3).
    if args.get(1).map(String::as_str) == Some("panic") {
        println!("[stdsmoke] panicking");
        panic!("stdsmoke deliberate panic");
    }

    // alloc (2.2): Vec growth + Box, with a checked value the harness asserts.
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

    // Instant (2.4): the grant-free monotonic counter (CNTVCT). Assert ordering
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

    // SystemTime (2.4): the rev2§2.6 time page the shell grants every child. A
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
