// SPDX-License-Identifier: 0BSD
//! The std-PAL futex bridge: thin delegation to `urt::futex`, the
//! userspace `sys::futex` backend (an address→waiter table over kernel
//! notifications). Holds no logic of its own — the `pal` `__eunomia_futex_*` shims
//! are one-line calls into here, and here is one-line calls into `urt`. Gated to the
//! eunomia/bare-metal targets (where `urt::futex` is the notif-backed table), like
//! [`crate::thread`].

#![cfg(bare_metal)]

use core::sync::atomic::AtomicU32;

/// Wait while `*futex == expected`. `timeout_ns == u64::MAX` means no timeout;
/// returns `false` only on timeout, `true` otherwise (the upstream contract).
pub fn wait(futex: &AtomicU32, expected: u32, timeout_ns: u64) -> bool {
    urt::futex::futex_wait(futex, expected, timeout_ns)
}

/// Wake one waiter on `futex`; `true` iff one was woken.
pub fn wake(futex: &AtomicU32) -> bool {
    urt::futex::futex_wake(futex)
}

/// Wake all waiters on `futex`.
pub fn wake_all(futex: &AtomicU32) {
    urt::futex::futex_wake_all(futex)
}
