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

//! Kernel-side channel surface: the ring/binding/teardown logic lives in
//! [`kcore::channel`]; this module re-exports it and supplies the
//! `KernelStore`-bound wrappers for `send`/`recv` (which fire readable/
//! writable events). `endpoint_cap_dropped` and `destroy_channel` take a
//! store too, but are only reached from within kcore (cap delete / last-ref
//! teardown), so they need no kernel wrapper.

pub use kcore::channel::*;

use crate::store::KernelStore;
use kcore::cspace::{CapSlot, ChanEnd};
use kcore::id::{ObjId, SlotId};

/// A `*mut CapSlot` array → `&[Option<SlotId>; MSG_CAPS]` (null slot ⇒ None).
#[inline]
fn slot_ids(caps: &[*mut CapSlot; MSG_CAPS]) -> [Option<SlotId>; MSG_CAPS] {
    core::array::from_fn(|i| {
        if caps[i].is_null() {
            None
        } else {
            Some(SlotId(caps[i] as u64))
        }
    })
}

/// See [`kcore::channel::send`].
pub unsafe fn send(
    ch: *mut Channel,
    end: ChanEnd,
    data: &[u8],
    caps: &[*mut CapSlot; MSG_CAPS],
) -> Result<(), ChanError> {
    kcore::channel::send(
        &mut KernelStore,
        ObjId(ch as u64),
        end,
        data,
        &slot_ids(caps),
    )
}

/// See [`kcore::channel::recv`].
pub unsafe fn recv(
    ch: *mut Channel,
    end: ChanEnd,
    buf: &mut [u8; MSG_PAYLOAD],
    dests: &[*mut CapSlot; MSG_CAPS],
) -> Result<(usize, u8), ChanError> {
    kcore::channel::recv(
        &mut KernelStore,
        ObjId(ch as u64),
        end,
        buf,
        &slot_ids(dests),
    )
}
