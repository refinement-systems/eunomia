//! Kernel-side notification surface: the word + waiter-queue logic lives in
//! [`kcore::notification`]; this module re-exports it and supplies the
//! `KernelStore`-bound wrapper for `signal` (which wakes a waiter through the
//! scheduler).

pub use kcore::notification::*;

use crate::store::KernelStore;
use kcore::id::ObjId;

/// See [`kcore::notification::signal`].
pub unsafe fn signal(n: *mut NotifObj, bits: u64) {
    kcore::notification::signal(&mut KernelStore, ObjId(n as u64), bits);
}
