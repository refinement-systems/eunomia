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

//! The std-port console GATE fixture: the first std binary whose
//! `stdout`/`stdin`/`stderr` ride the userspace `user/console` channel (rev2§5.1)
//! instead of the kernel debug-log. The shell donates its console endpoint to every child
//! it runs, so this binary's `println!`/`eprintln!` and `stdin().read_line` all flow over
//! the console driver → serial UART.
//!
//! It reads one line from stdin, echoes it to stdout, and writes a diagnostic to
//! stderr, then prints `STD51 PASS`. Stdin has **no** debug-log path (the console
//! driver owns the UART RX line), so a successful echo witnesses the whole
//! `stdin → console → stdout` round-trip; the stderr line witnesses that stderr routes
//! (here, via the terminal fallback onto the stdout channel — `NAME_STDERR` is a
//! distinct name the resolver honors when granted separately, exercised by init→shell
//! and the `eunomia_sys::console` unit tests).
//!
//! It is a real std program — no `#![no_std]`, no `#[panic_handler]`. std owns `_start`
//! (the eunomia PAL) and the panic handler; `extern crate eunomia_sys;` forces the seam
//! rlib into the link so the `__eunomia_std{out,err}_write` / `__eunomia_stdin_read`
//! console shims resolve.

extern crate eunomia_sys; // links the PAL↔seam bridge (incl. the console client)

use std::io::BufRead;

fn main() {
    // Printed before the blocking read so the harness can wait for readiness, then send
    // the input line (a line lost to a not-yet-reading child would hang the test).
    println!("[stdio] start");

    let mut line = String::new();
    let n = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("read a stdin line");

    // Echo the line (trimmed of the trailing newline) back to stdout over the console.
    let echo = line.trim_end();
    println!("[stdio] echo={echo}");

    // A separate diagnostic on stderr — a stream distinct from stdout (rev2§5.1).
    eprintln!("[stdio] stderr diag n={n}");

    println!("STD51 PASS");
}
