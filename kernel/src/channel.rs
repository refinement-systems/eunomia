//! Kernel-side channel surface: the ring/binding/teardown logic lives in
//! [`kcore::channel`]; this module re-exports it and supplies the
//! `KernelEnv`-bound wrappers for `send`/`recv` (which fire readable/
//! writable events). `endpoint_cap_dropped` and `destroy_channel` take an
//! env too, but are only reached from within kcore (cap delete / last-ref
//! teardown), so they need no kernel wrapper.

pub use kcore::channel::*;

use crate::env::KernelEnv;
use kcore::cspace::{CapSlot, ChanEnd};

/// See [`kcore::channel::send`].
pub unsafe fn send(
    ch: *mut Channel,
    end: ChanEnd,
    data: &[u8],
    caps: &[*mut CapSlot; MSG_CAPS],
) -> Result<(), ChanError> {
    kcore::channel::send(ch, end, data, caps, &mut KernelEnv)
}

/// See [`kcore::channel::recv`].
pub unsafe fn recv(
    ch: *mut Channel,
    end: ChanEnd,
    buf: &mut [u8; MSG_PAYLOAD],
    dests: &[*mut CapSlot; MSG_CAPS],
) -> Result<(usize, u8), ChanError> {
    kcore::channel::recv(ch, end, buf, dests, &mut KernelEnv)
}
