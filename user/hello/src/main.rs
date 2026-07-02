// SPDX-License-Identifier: 0BSD
//! The real `hello` — the first *non-fixture* user program on std. Where
//! `user/stdsmoke` is a gate fixture, this is the actual
//! "hello world" a user runs from the shell (`run bin/hello`), and its whole point
//! is that a real program now boots on the std runtime with no bare-metal
//! scaffolding of its own.
//!
//! It is a real std program — no `#![no_std]`, no `#![no_main]`, no
//! `#[panic_handler]`. std owns `_start` (the eunomia PAL, rev2§5.1), the
//! allocator, and the panic handler. `extern crate eunomia_sys;` is the one
//! non-obvious line: it forces the seam rlib into the link so the linker resolves
//! the PAL's undefined `__eunomia_*` `extern "Rust"` symbols against eunomia-sys's
//! `#[no_mangle]` definitions (the `__rust_alloc` pattern).
//!
//! Arms (validating entry/argv/alloc/exit/STATUS_PANIC):
//!   - argv via `env::args`, allocation via `Vec`/`String`/`format!`,
//!   - the inherited environment via `env::var` (init→shell→child),
//!   - a monotonic `Instant` delta,
//!   - a clean `exit(0)` (returning from `main`), and
//!   - `run bin/hello panic` → std's own handler terminates as STATUS_PANIC so the
//!     parent shell reaps `panicked`, not `exited(_)`.

extern crate eunomia_sys; // links the PAL↔seam bridge (see module doc)

use std::time::Instant;

fn main() {
    // stdio: `println!` rides the `user/console` channel (the shell donates
    // its console endpoint to every child). The `[hello]` prefix keeps the markers
    // from colliding with kernel/shell/storaged lines on the shared console.
    println!("[hello] alive in its own aspace on std");

    // argv + allocation: the shell delivers the command line as the
    // startup block's argv; collecting into a `Vec<String>` exercises the heap.
    let args: Vec<String> = std::env::args().collect();
    println!("[hello] argv={args:?}");

    // The std-owned panic path: std's handler must terminate as STATUS_PANIC
    // so the parent distinguishes a crash from a clean exit.
    if args.get(1).map(String::as_str) == Some("panic") {
        println!("[hello] panicking");
        panic!("hello deliberate panic");
    }

    // A little heap churn through `format!`/`String` — the allocator on a real
    // (non-fixture) workload.
    let mut greeting = String::new();
    for who in args.iter().skip(1) {
        greeting.push_str(&format!("hello, {who}! "));
    }
    if greeting.is_empty() {
        greeting.push_str("hello, world!");
    }
    println!("[hello] {}", greeting.trim_end());

    // Inherited environment: init defines `TERM=eunomia`, the shell forwards
    // it. Reading it back witnesses the init→shell→child inheritance from a real
    // program (not just the stdsmoke fixture).
    match std::env::var("TERM") {
        Ok(term) => println!("[hello] TERM={term}"),
        Err(_) => println!("[hello] TERM unset"),
    }

    // Monotonic clock: `Instant` is zero-syscall (reads CNTVCT), no grant.
    let t0 = Instant::now();
    let mut acc: u64 = 0;
    for i in 0..1000u64 {
        acc = acc.wrapping_add(i);
    }
    let elapsed = t0.elapsed();
    println!("[hello] sum={acc} in {}us", elapsed.as_micros());

    // The green marker the boot harness greps, then a clean exit(0) (returning from
    // `main`; std's runtime calls the PAL `exit(0)`).
    println!("STD53 PASS");
}
