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

//! Object and slot handles.
//!
//! The arena rewrite replaces `kcore`'s intrusive `*mut` graph with opaque,
//! `Copy` handles resolved through the [`Store`](crate::store::Store) seam. The
//! verified core only ever **stores and compares** handles — it never
//! dereferences one. A handle's meaning is the `Store` impl's business:
//!
//!   - in production (`kernel` crate) a handle wraps the object's/slot's live
//!     address, so the resolver is a behaviour-preserving cast at the one
//!     sanctioned `unsafe` boundary;
//!   - in proofs/host tests a handle is an index into a plain array.
//!
//! Because the core treats them opaquely, the same code verifies over the array
//! model and runs over the address model — the `Env`/`Hal` seam, extended to
//! object storage.

/// A handle to a refcounted kernel object (cspace, channel, TCB, notification,
/// timer, aspace). Tagged with its kind by the carrying [`CapKind`];
/// the raw handle itself is kind-agnostic.
///
/// [`CapKind`]: crate::cspace::CapKind
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjId(pub u64);

/// A handle to a [`CapSlot`](crate::cspace::CapSlot). Addresses every slot home
/// uniformly — a cspace resident, a channel ring cap, a TCB binding slot — so
/// the CDT links (`Option<SlotId>`) span containers exactly as the old
/// `*mut CapSlot` links did, with no special case in the revoke walk.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u64);
