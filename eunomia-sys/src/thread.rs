//! The std-PAL thread bridge (std-port 3.2): thin delegation to `urt::thread`,
//! the verification-disciplined in-process thread primitive. Holds no logic of its
//! own — the `pal` `__eunomia_thread_*` shims are one-line calls into here, and
//! here is one-line calls into `urt`. Gated to the eunomia/bare-metal targets
//! (where `urt` is a dependency and the `svc` shell is real), like [`crate::pal`].

#![cfg(any(target_os = "eunomia", target_os = "none"))]

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
