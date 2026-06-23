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
