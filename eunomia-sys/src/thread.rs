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

//! The std-PAL thread bridge: thin delegation to `urt::thread`,
//! the verification-disciplined in-process thread primitive. Holds no logic of its
//! own — the `pal` `__eunomia_thread_*` shims are one-line calls into here, and
//! here is one-line calls into `urt`. Gated to the eunomia/bare-metal targets
//! (where `urt` is a dependency and the `svc` shell is real), like [`crate::pal`].

#![cfg(bare_metal)]

use urt::thread::{self, JoinHandle};

/// Spawn an in-process thread entering `entry` with `arg` in `x0`. Returns the join
/// handle (the pool slot, `>= 0`) or a negative syscall error (`ERR_*`), which the
/// std arm maps through `io::Error::from_raw_os_error`. An unconfigured (non-thread-
/// capable) process returns `ERR_STATE` — surfaced as `Unsupported`.
pub fn spawn(entry: usize, stack: usize, arg: u64) -> i64 {
    match thread::spawn(entry, stack, arg) {
        Ok(h) => h.index() as i64,
        Err(e) => e,
    }
}

/// Join the thread whose handle (pool slot) is `handle`. Returns 0 or a negative
/// syscall error.
pub fn join(handle: u64) -> i64 {
    match thread::join(JoinHandle::from_index(handle as usize)) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

/// Cooperative yield (op 2).
pub fn yield_now() {
    thread::yield_now();
}

/// Sleep at least `nanos` (the MVP yield-poll, rev2§5.4).
pub fn sleep(nanos: u64) {
    thread::sleep(nanos);
}
