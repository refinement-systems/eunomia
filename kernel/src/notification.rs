//! Kernel-side notification surface: the word + waiter-queue logic lives in
//! [`kcore::notification`]; this module re-exports it and supplies the
//! `KernelEnv`-bound wrapper for `signal` (which wakes a waiter through the
//! scheduler).

pub use kcore::notification::*;

use crate::env::KernelEnv;

/// See [`kcore::notification::signal`].
pub unsafe fn signal(n: *mut NotifObj, bits: u64) {
    kcore::notification::signal(n, bits, &mut KernelEnv);
}
