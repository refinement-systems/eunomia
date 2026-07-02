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
