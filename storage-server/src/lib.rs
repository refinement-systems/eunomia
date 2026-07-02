// SPDX-License-Identifier: 0BSD
//! Storage server core: sessions, handles, tickets (spec rev2§2.2-2.4).
//!
//! A storage cap at the boundary is a small integer handle, meaningful
//! only relative to its session. The server keeps, per session:
//!
//!   handle → (kind: snapshot | ref, target, subtree, rights, gen-at-grant)
//!
//! The wire protocol is handle-relative: every operation names a handle
//! plus a component-list path resolved *under* the handle's subtree —
//! confinement by unreachability, not checked policy (rev2§2.3). Raw hashes
//! never appear as request parameters.
//!
//! This module is transport-agnostic and host-testable: `Server::handle`
//! maps one `Request` to one `Response`. The IPC binding (channel
//! sessions, postcard bodies via the ipc crate, peer-closed cleanup)
//! lives in the on-OS server; session lifecycle here is driven by explicit
//! `open_session` / `close_session` calls that the transport owns.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod wire;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use cas::dev::BlockDev;
use cas::hash::Hash;
use cas::overlay::Path as TreePath;
use cas::prolly::{Content, Entry, EntryKind};
use cas::store::{Store, StoreError};
// Re-exported: `RefEdit` is part of the public `Request::Apply` wire type, so
// wire consumers reach it from this crate (rev2§4.7 guarded batch vocabulary).
pub use cas::store::RefEdit;

// Verus is the deductive-proof tier for the rights lattice (`attenuate` + the
// rights bits, below). `vstd::prelude` supplies the `verus!{}` macro + ghost
// vocabulary; Verus requires it imported at the crate root. In an ordinary build
// the macro erases ghost code, so this import is otherwise unused — hence the
// allow (same as kcore/ipc/virtio-blk).
#[allow(unused_imports)]
use vstd::prelude::*;

// ── Rights (rev2§2.3) ───────────────────────────────────────────────────
//
// The rights lattice is the Verus-verified deductive core of delegation: the
// rights bits, the `has_right` reading of the dispatch guards, and `attenuate`'s
// monotone / deny-by-default contract all live in the `verus!{}` block so the
// `by (bit_vector)` proofs see the bit literals (doc/guidelines/verus.md §6).
verus! {

pub const R_READ: u8 = 1 << 0;

pub const R_WRITE: u8 = 1 << 1;

pub const R_SNAPSHOT: u8 = 1 << 2;

/// Destructive enough to deserve its own bit (rev2§2.3); also gates mass
/// revocation (generation bump, rev2§2.2).
pub const R_REWRITE_HISTORY: u8 = 1 << 3;

pub const R_ENUMERATE: u8 = 1 << 4;

/// Store-global observation (rev2§2.3): gates `statfs(handle)` and any
/// future global observable (GC counters, index occupancy). The one right
/// whose scope ignores the subtree its handle denotes — and the one right
/// kept OUT of `R_ALL`, so ordinary delegation strips it by default
/// (deny-by-default). It originates only on the privileged `root_grant`.
pub const R_STAT_STORE: u8 = 1 << 5;

/// All ordinary, subtree-scoped, *delegatable* rights. Deliberately excludes
/// `R_STAT_STORE` (bit 5): attenuation is plain intersection, so a delegated
/// handle masked by `R_ALL` (or narrower) strips `stat-store` for free. The
/// numeric value is stable — the committed fuzz corpora depend on it.
pub const R_ALL: u8 = 0b1_1111;

/// `has_right(bits, r)`: a handle carrying `bits` holds the right named by the
/// single-bit mask `r` — the spec reading of the dispatch guards `e.rights & R_x
/// != 0`. Phrasing the lattice in these terms makes `attenuate`'s monotonicity
/// legible: a derived handle holds no right its parent lacked.
pub open spec fn has_right(bits: u8, r: u8) -> bool {
    bits & r != 0
}

/// Monotone rights attenuation (rev2§2.3): a derived handle's rights are the
/// intersection of the parent's rights with the requested mask — never a
/// superset. The sole arithmetic by which delegation narrows authority; it is
/// also what strips `R_STAT_STORE` when a mask omits bit 5. Mechanized for all
/// `u8` inputs: the result is exactly `parent & mask`, sets no bit absent from
/// `parent`, and drops `R_STAT_STORE` whenever the mask does.
pub fn attenuate(parent: u8, mask: u8) -> (r: u8)
    ensures
        r == parent & mask,
        r & !parent == 0,
        (mask & R_STAT_STORE == 0) ==> (r & R_STAT_STORE == 0),
{
    let r = parent & mask;
    assert(r & !parent == 0) by (bit_vector)
        requires
            r == parent & mask,
    ;
    assert((mask & R_STAT_STORE == 0) ==> (r & R_STAT_STORE == 0)) by (bit_vector)
        requires
            r == parent & mask,
    ;
    r
}

/// Monotonicity, the right-keyed reading: an attenuated handle (`parent & mask`)
/// holds a right only if its parent did. Delegation never grows authority
/// (rev2§2.3) — for any single-bit (or composite) `right`.
pub proof fn lemma_attenuate_monotone(parent: u8, mask: u8, right: u8)
    ensures
        has_right(parent & mask, right) ==> has_right(parent, right),
{
    assert(((parent & mask) & right != 0) ==> (parent & right != 0)) by (bit_vector);
}

/// Deny-by-default (rev2§2.3): attenuating by `R_ALL` always clears
/// `R_STAT_STORE`, because `R_ALL` (bits 0..=4) omits bit 5 — ordinary
/// delegation strips store-global observation for free, for any parent.
pub proof fn lemma_attenuate_r_all_denies_stat_store(parent: u8)
    ensures
        !has_right(parent & R_ALL, R_STAT_STORE),
{
    assert(R_ALL == 0b1_1111u8);
    assert(R_STAT_STORE == 1u8 << 5);
    assert((parent & 0b1_1111u8) & (1u8 << 5) == 0) by (bit_vector);
}

} // verus!
pub type SessionId = u64;
pub type HandleId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleTarget {
    /// Immutable subtree, denoted by its node hash (internal only — the
    /// hash never crosses the boundary).
    Snapshot { root: Hash },
    /// Live ref, subtree-scoped by server-side path resolution (rev2§2.3).
    Ref {
        name: Vec<u8>,
        subtree: TreePath,
        gen_at_grant: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleEntry {
    pub target: HandleTarget,
    pub rights: u8,
}

#[derive(Default)]
struct Session {
    handles: BTreeMap<HandleId, HandleEntry>,
    next_handle: HandleId,
}

impl Session {
    fn insert(&mut self, entry: HandleEntry) -> HandleId {
        let id = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(id, entry);
        id
    }
}

// ── Protocol ────────────────────────────────────────────────────────────

/// Handle-relative requests (rev2§2.4). Paths are component lists; `/` is
/// shell presentation. Capability-bearing results are handle ids; tickets
/// are the only bearer tokens and are one-shot with a TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
pub enum Request {
    Read {
        handle: HandleId,
        path: TreePath,
        offset: u64,
        len: u32,
    },
    Write {
        handle: HandleId,
        path: TreePath,
        offset: u64,
        data: Vec<u8>,
    },
    Unlink {
        handle: HandleId,
        path: TreePath,
    },
    List {
        handle: HandleId,
        path: TreePath,
    },
    /// Attenuate: sub-subtree + rights mask, in one step (rev2§2.4 delegation).
    OpenChild {
        handle: HandleId,
        path: TreePath,
        rights_mask: u8,
    },
    Close {
        handle: HandleId,
    },
    Sync {
        handle: HandleId,
    },
    Snapshot {
        handle: HandleId,
        message: Vec<u8>,
        class: u8,
    },
    ListSnapshots {
        handle: HandleId,
    },
    /// A snapshot handle from a ref handle's history, subtree-scoped.
    OpenSnapshot {
        handle: HandleId,
        snap_id: u64,
        path: TreePath,
        rights_mask: u8,
    },
    Rollback {
        handle: HandleId,
        snap_id: u64,
    },
    /// Mass revocation: bump the ref's generation; every outstanding
    /// handle on it (all sessions) goes stale on next use (rev2§2.2).
    RevokeRef {
        handle: HandleId,
    },
    MintTicket {
        handle: HandleId,
        ttl_nanos: u64,
    },
    RedeemTicket {
        ticket: [u8; 16],
    },
    /// Size of a file (None response = absent).
    Stat {
        handle: HandleId,
        path: TreePath,
    },
    EnumerateSession,
    /// History rewriting (rev2§4.6-4.7): drop one snapshot row. Sets the
    /// post-rewrite GC trigger; the reclamation itself is asynchronous.
    DeleteSnapshot {
        handle: HandleId,
        snap_id: u64,
    },
    /// Edit a snapshot's retention class (the "mark survivors keep" flow).
    SetClass {
        handle: HandleId,
        snap_id: u64,
        class: u8,
    },
    /// Run a GC cycle now (the manual trigger).
    Gc {
        handle: HandleId,
    },
    /// Chunk-region space accounting.
    Statfs {
        handle: HandleId,
    },
    /// Guarded ref-table batch (rev2§4.7): apply `edits` to the ref
    /// all-or-nothing, but only if its edit version still equals
    /// `expected_version`. The read-then-act race fix — a
    /// concurrent snapshot/edit/write between the caller's enumerate and this
    /// op has advanced the version, so the batch is refused with the current
    /// version (`Response::VersionMismatch`) and the caller re-reads. Requires
    /// `may-rewrite-history` on a ref-root handle (the `DeleteSnapshot`/`Gc`
    /// gate). Appended last to keep every prior variant's postcard discriminant
    /// — and the committed `request_dispatch` corpus — stable.
    Apply {
        handle: HandleId,
        expected_version: u64,
        edits: Vec<RefEdit>,
    },
    /// Pin a snapshot under a tag name (rev2§4.7 "Tags"): `name → snapshot id`,
    /// surviving metadata edits, acting as a `keep`-strength pin. Row surgery,
    /// so it requires `may-rewrite-history` on a ref-root handle (the
    /// `DeleteSnapshot`/`Apply` gate) and the tag is scoped to that ref.
    Tag {
        handle: HandleId,
        name: Vec<u8>,
        snap_id: u64,
    },
    /// Delete a tag, unpinning its snapshot (rev2§4.7). Ref-scoped to the
    /// handle's ref and `may-rewrite-history`-gated, like `Tag`.
    Untag {
        handle: HandleId,
        name: Vec<u8>,
    },
    /// Enumerate the handle ref's tags (rev2§4.7). Read-only, so it needs only
    /// `R_READ`, like `ListSnapshots`. Appended after `Apply`/`Tag`/`Untag` to
    /// keep every prior variant's postcard discriminant — and the committed
    /// `request_dispatch` corpus — stable.
    ListTags {
        handle: HandleId,
    },
    /// Rename `from` to `to` within the handle's ref (rev2§4.9). `R_WRITE`-gated
    /// and ref-only, like `Write`/`Unlink`; both paths are subtree-scoped under
    /// the handle, so a cross-subtree target is unnameable and therefore denied
    /// for free. Appended last to keep every prior variant's postcard
    /// discriminant — and the committed `request_dispatch` corpus — stable.
    Rename {
        handle: HandleId,
        from: TreePath,
        to: TreePath,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DirEnt {
    File { name: Vec<u8>, size: u64 },
    Dir { name: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapInfo {
    pub id: u64,
    pub timestamp: u64,
    pub provenance: Vec<u8>,
    pub message: Vec<u8>,
    pub class: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Response {
    Ok,
    Data(Vec<u8>),
    NotFound,
    Handle(HandleId),
    Listing(Vec<DirEnt>),
    /// Snapshot rows plus the ref's current rev2§4.7 edit version, read in the
    /// same call so a retention daemon's enumerate and its later guarded-batch
    /// `expected_version` come from one atomic snapshot of the ref.
    Snapshots {
        snaps: Vec<SnapInfo>,
        edit_version: u64,
    },
    SnapId(u64),
    Ticket([u8; 16]),
    SessionDump(Vec<(HandleId, String)>),
    Err(ErrorCode),
    GcReport {
        live_objects: u64,
        freed_objects: u64,
        freed_bytes: u64,
    },
    Space {
        total: u64,
        used: u64,
        free: u64,
    },
    /// A guarded batch (`Request::Apply`) committed; carries the ref's
    /// post-batch edit version (rev2§4.7).
    Applied {
        edit_version: u64,
    },
    /// A guarded batch was refused because `expected_version` was stale;
    /// carries the ref's current edit version so the caller re-reads and
    /// retries. A data-carrying reply, not an `ErrorCode` — "fails carrying
    /// the current version" (rev2§4.7).
    VersionMismatch {
        edit_version: u64,
    },
    /// The handle ref's tags (rev2§4.7), each `(name, ref_name, snap_id)`.
    /// Appended last to keep prior variants' postcard discriminants stable.
    Tags(Vec<(Vec<u8>, Vec<u8>, u64)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ErrorCode {
    BadHandle,
    /// Generation mismatch: the handle was mass-revoked (rev2§2.2).
    Stale,
    Denied,
    BadPath,
    NotADir,
    ReadOnly,
    NoSuchSnapshot,
    BadTicket,
    Internal,
    /// The snapshot is a tag target; tags are keep-strength pins (rev2§4.7).
    Pinned,
    /// Write offset/length out of range (overflow or beyond store capacity).
    BadOffset,
}

// ── Server ──────────────────────────────────────────────────────────────

/// Maximum claim-ticket lifetime (rev2§2.4): the caller's requested TTL is
/// clamped to this so no ticket outlives the bound. Tickets are for prompt
/// peer hand-off, not durable authority — that stays in the handle/session
/// regime. Default 60 s; a tunable policy default, not an ABI promise.
pub const MAX_TICKET_TTL_NANOS: u64 = 60_000_000_000;

struct PendingTicket {
    entry: HandleEntry,
    expires: u64,
}

pub struct Server<D: BlockDev> {
    store: Store<D>,
    sessions: BTreeMap<SessionId, Session>,
    next_session: SessionId,
    tickets: BTreeMap<[u8; 16], PendingTicket>,
    ticket_seq: u64,
    ticket_seed: u64,
    /// GC requested by a trigger (rev2§4.6): a history-rewriting op, or the
    /// space watermark. The transport drains it after replying, so the
    /// foreground op stays O(small) and reclamation follows promptly.
    gc_requested: bool,
    /// Generation at the last completed GC — the watermark re-arms only
    /// after new commits, so a full-but-idle store doesn't thrash GC.
    gc_done_gen: u64,
}

impl<D: BlockDev> Server<D> {
    pub fn new(store: Store<D>, ticket_seed: u64) -> Server<D> {
        Server {
            store,
            sessions: BTreeMap::new(),
            next_session: 1,
            tickets: BTreeMap::new(),
            ticket_seq: 0,
            ticket_seed,
            gc_requested: false,
            gc_done_gen: 0,
        }
    }

    pub fn store(&mut self) -> &mut Store<D> {
        &mut self.store
    }

    pub fn gc_requested(&self) -> bool {
        self.gc_requested
    }

    /// Run a GC cycle now — the manual `gc` request, or the transport
    /// draining a pending trigger.
    pub fn run_gc(&mut self) -> Result<cas::store::GcStats, StoreError> {
        self.gc_requested = false;
        let stats = self.store.gc()?;
        self.gc_done_gen = self.store.generation();
        Ok(stats)
    }

    /// Open a session pre-populated with grants — the delegation-along-
    /// spawn path (rev2§2.4): the parent names handles + attenuation, the
    /// child gets a fresh session. Grant construction is the caller's
    /// (init's / the parent's) authority; there is no other way to get a
    /// first handle.
    pub fn open_session(&mut self, grants: Vec<HandleEntry>) -> SessionId {
        let id = self.next_session;
        self.next_session += 1;
        let mut session = Session::default();
        for g in grants {
            session.insert(g);
        }
        self.sessions.insert(id, session);
        id
    }

    /// Transport's peer-closed path (rev2§2.4 cleanup): drop the whole table.
    pub fn close_session(&mut self, id: SessionId) {
        self.sessions.remove(&id);
    }

    /// Effective rights bits currently granted on a handle, or `None` if the
    /// session/handle is absent. Inspection only — reports the stored grant
    /// without the liveness/generation check that `lookup` performs (a caller
    /// that needs the stale check goes through a request).
    pub fn handle_rights(&self, session: SessionId, handle: HandleId) -> Option<u8> {
        self.sessions
            .get(&session)?
            .handles
            .get(&handle)
            .map(|e| e.rights)
    }

    /// The privileged init/maintenance grant: a full-rights handle at a
    /// ref's root. This is the **sole origin** of `R_STAT_STORE` (rev2§2.3) —
    /// every other handle is derived by intersection, which strips it. init
    /// hands this (or a stat-store-bearing attenuation of it) only to the
    /// shell and to maintenance holders.
    pub fn root_grant(&self, ref_name: &[u8]) -> Result<HandleEntry, StoreError> {
        let gen = self
            .store
            .refs()
            .find(|(n, _)| n.as_slice() == ref_name)
            .ok_or(StoreError::NoSuchRef)?
            .1
            .generation;
        Ok(HandleEntry {
            target: HandleTarget::Ref {
                name: ref_name.to_vec(),
                subtree: Vec::new(),
                gen_at_grant: gen,
            },
            rights: R_ALL | R_STAT_STORE,
        })
    }

    pub fn handle(&mut self, session: SessionId, req: Request, now: u64) -> Response {
        let resp = match self.dispatch(session, req, now) {
            Ok(resp) => resp,
            Err(e) => Response::Err(e),
        };
        // The crude space watermark (rev2§4.6): below ~20% free, request a
        // cycle — unless GC already ran at this generation and this is
        // simply how full the store is.
        let sp = self.store.space();
        if sp.free * 5 < sp.total && self.store.generation() != self.gc_done_gen {
            self.gc_requested = true;
        }
        resp
    }

    /// Validate handle liveness (generation check — lazy mass revocation,
    /// rev2§2.2) and the rights needed for the op.
    fn lookup(
        &self,
        session: SessionId,
        handle: HandleId,
        need: u8,
    ) -> Result<HandleEntry, ErrorCode> {
        let s = self.sessions.get(&session).ok_or(ErrorCode::BadHandle)?;
        let e = s.handles.get(&handle).ok_or(ErrorCode::BadHandle)?.clone();
        if let HandleTarget::Ref {
            name, gen_at_grant, ..
        } = &e.target
        {
            let current = self
                .store
                .refs()
                .find(|(n, _)| n == &name)
                .ok_or(ErrorCode::Stale)?
                .1
                .generation;
            if current != *gen_at_grant {
                return Err(ErrorCode::Stale);
            }
        }
        if e.rights & need != need {
            return Err(ErrorCode::Denied);
        }
        Ok(e)
    }

    fn full_path(subtree: &TreePath, path: &TreePath) -> TreePath {
        let mut p = subtree.clone();
        p.extend(path.iter().cloned());
        p
    }

    /// "." and ".." are path syntax for shells, never sent to the server
    /// (rev2§4.9); reject them and any malformed component up front.
    fn validate_path(path: &TreePath) -> Result<(), ErrorCode> {
        for c in path {
            cas::prolly::validate_name(c).map_err(|_| ErrorCode::BadPath)?;
        }
        Ok(())
    }

    fn dispatch(
        &mut self,
        session: SessionId,
        req: Request,
        now: u64,
    ) -> Result<Response, ErrorCode> {
        match &req {
            Request::Read { path, .. }
            | Request::Write { path, .. }
            | Request::Unlink { path, .. }
            | Request::List { path, .. }
            | Request::OpenChild { path, .. }
            | Request::Stat { path, .. }
            | Request::OpenSnapshot { path, .. } => Self::validate_path(path)?,
            Request::Rename { from, to, .. } => {
                Self::validate_path(from)?;
                Self::validate_path(to)?;
            }
            _ => {}
        }
        match req {
            Request::Read {
                handle,
                path,
                offset,
                len,
            } => {
                let e = self.lookup(session, handle, R_READ)?;
                let data = match &e.target {
                    HandleTarget::Ref { name, subtree, .. } => self
                        .store
                        .read(name, &Self::full_path(subtree, &path))
                        .map_err(store_err)?,
                    HandleTarget::Snapshot { root } => {
                        self.store.read_at_root(root, &path).map_err(store_err)?
                    }
                };
                Ok(match data {
                    Some(d) => {
                        let start = (offset as usize).min(d.len());
                        let end = start.saturating_add(len as usize).min(d.len());
                        Response::Data(d[start..end].to_vec())
                    }
                    None => Response::NotFound,
                })
            }
            Request::Write {
                handle,
                path,
                offset,
                data,
            } => {
                let e = self.lookup(session, handle, R_WRITE)?;
                let HandleTarget::Ref { name, subtree, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly); // snapshots are immutable
                };
                self.store
                    .write(name, &Self::full_path(subtree, &path), offset, &data, now)
                    .map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::Unlink { handle, path } => {
                let e = self.lookup(session, handle, R_WRITE)?;
                let HandleTarget::Ref { name, subtree, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                self.store
                    .unlink(name, &Self::full_path(subtree, &path), now)
                    .map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::Rename { handle, from, to } => {
                let e = self.lookup(session, handle, R_WRITE)?;
                let HandleTarget::Ref { name, subtree, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly); // snapshots are immutable
                };
                // Both paths are resolved under the handle's subtree, so a
                // target outside it is unnameable (rev2§4.9) — no extra check.
                self.store
                    .rename(
                        name,
                        &Self::full_path(subtree, &from),
                        &Self::full_path(subtree, &to),
                        now,
                    )
                    .map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::List { handle, path } => {
                let e = self.lookup(session, handle, R_READ)?;
                let raw = match &e.target {
                    HandleTarget::Ref { name, subtree, .. } => self
                        .store
                        .list(name, &Self::full_path(subtree, &path))
                        .map_err(store_err)?,
                    HandleTarget::Snapshot { root } => {
                        self.list_at_root(root, &path).map_err(store_err)?
                    }
                };
                Ok(Response::Listing(
                    raw.into_iter()
                        .map(|(name, kind, size)| match kind {
                            EntryKind::File => DirEnt::File { name, size },
                            EntryKind::Dir => DirEnt::Dir { name },
                        })
                        .collect(),
                ))
            }
            Request::OpenChild {
                handle,
                path,
                rights_mask,
            } => {
                let e = self.lookup(session, handle, 0)?;
                // Monotone derivation (rev2§2.3): intersection only. This is
                // also what strips `R_STAT_STORE` from delegated children — a
                // mask of `R_ALL` (which omits bit 5) clears it for free, so
                // it survives only when the holder has it AND sets bit 5.
                let rights = attenuate(e.rights, rights_mask);
                let entry = match &e.target {
                    HandleTarget::Ref {
                        name,
                        subtree,
                        gen_at_grant,
                    } => {
                        // The subtree must currently resolve to a directory.
                        let full = Self::full_path(subtree, &path);
                        self.resolve_ref_dir(name, &full)?;
                        HandleEntry {
                            target: HandleTarget::Ref {
                                name: name.clone(),
                                subtree: full,
                                gen_at_grant: *gen_at_grant,
                            },
                            rights,
                        }
                    }
                    HandleTarget::Snapshot { root } => {
                        let child = self.resolve_snap_dir(root, &path)?;
                        HandleEntry {
                            target: HandleTarget::Snapshot { root: child },
                            rights,
                        }
                    }
                };
                let s = self
                    .sessions
                    .get_mut(&session)
                    .ok_or(ErrorCode::BadHandle)?;
                Ok(Response::Handle(s.insert(entry)))
            }
            Request::Close { handle } => {
                let s = self
                    .sessions
                    .get_mut(&session)
                    .ok_or(ErrorCode::BadHandle)?;
                s.handles.remove(&handle).ok_or(ErrorCode::BadHandle)?;
                Ok(Response::Ok)
            }
            Request::Sync { handle } => {
                let e = self.lookup(session, handle, R_WRITE)?;
                let HandleTarget::Ref { name, .. } = &e.target else {
                    return Ok(Response::Ok); // snapshots are always durable
                };
                self.store.sync_ref(&name.clone()).map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::Snapshot {
                handle,
                message,
                class,
            } => {
                let e = self.lookup(session, handle, R_SNAPSHOT)?;
                let HandleTarget::Ref { name, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                // Provenance is server-filled (rev2§4.7), never client-supplied.
                let prov = format!("session={session}");
                let id = self
                    .store
                    .snapshot(&name.clone(), prov.as_bytes(), &message, class, now)
                    .map_err(store_err)?;
                Ok(Response::SnapId(id))
            }
            Request::ListSnapshots { handle } => {
                let e = self.lookup(session, handle, R_READ)?;
                let HandleTarget::Ref { name, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                let snaps = self
                    .store
                    .snapshots(name)
                    .map(|r| SnapInfo {
                        id: r.id,
                        timestamp: r.timestamp,
                        provenance: r.provenance.clone(),
                        message: r.message.clone(),
                        class: r.class,
                    })
                    .collect();
                // Same atomic read as the rows above: the version a daemon will
                // present as `expected_version` (rev2§4.7).
                let edit_version = self.store.edit_version(name).unwrap_or(0);
                Ok(Response::Snapshots {
                    snaps,
                    edit_version,
                })
            }
            Request::OpenSnapshot {
                handle,
                snap_id,
                path,
                rights_mask,
            } => {
                let e = self.lookup(session, handle, R_READ)?;
                let HandleTarget::Ref { name, subtree, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                let root = self
                    .store
                    .snapshot_root(name, snap_id)
                    .map_err(|_| ErrorCode::NoSuchSnapshot)?;
                // Scope to the handle's subtree first: a subtree handle
                // must not see the snapshot's wider world.
                let scoped = if subtree.is_empty() {
                    root
                } else {
                    self.resolve_snap_dir(&root, subtree)?
                };
                let child = if path.is_empty() {
                    scoped
                } else {
                    self.resolve_snap_dir(&scoped, &path)?
                };
                // Masking to read/enumerate also drops `R_STAT_STORE`:
                // snapshot handles never carry store-global observation.
                let rights = attenuate(attenuate(e.rights, rights_mask), R_READ | R_ENUMERATE);
                let s = self
                    .sessions
                    .get_mut(&session)
                    .ok_or(ErrorCode::BadHandle)?;
                Ok(Response::Handle(s.insert(HandleEntry {
                    target: HandleTarget::Snapshot { root: child },
                    rights,
                })))
            }
            Request::Rollback { handle, snap_id } => {
                let e = self.lookup(session, handle, R_REWRITE_HISTORY)?;
                let HandleTarget::Ref { name, subtree, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                if !subtree.is_empty() {
                    // Ref-head surgery from a subtree view is not a thing.
                    return Err(ErrorCode::Denied);
                }
                self.store
                    .rollback(&name.clone(), snap_id)
                    .map_err(store_err)?;
                // Post-rewrite trigger (rev2§4.6): the abandoned head (unless
                // snapshotted) just became garbage.
                self.gc_requested = true;
                Ok(Response::Ok)
            }
            Request::RevokeRef { handle } => {
                let e = self.lookup(session, handle, R_REWRITE_HISTORY)?;
                let HandleTarget::Ref { name, subtree, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                if !subtree.is_empty() {
                    return Err(ErrorCode::Denied);
                }
                self.store
                    .bump_generation(&name.clone())
                    .map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::MintTicket { handle, ttl_nanos } => {
                let e = self.lookup(session, handle, 0)?;
                // rev2§2.4: the caller requests the TTL, the server clamps it.
                let ttl = ttl_nanos.min(MAX_TICKET_TTL_NANOS);
                self.ticket_seq += 1;
                let mut seed = [0u8; 24];
                seed[..8].copy_from_slice(&self.ticket_seed.to_le_bytes());
                seed[8..16].copy_from_slice(&self.ticket_seq.to_le_bytes());
                seed[16..24].copy_from_slice(&now.to_le_bytes());
                let digest = Hash::of(&seed);
                let mut ticket = [0u8; 16];
                ticket.copy_from_slice(&digest.as_bytes()[..16]);
                self.tickets.insert(
                    ticket,
                    PendingTicket {
                        entry: e,
                        expires: now.saturating_add(ttl),
                    },
                );
                Ok(Response::Ticket(ticket))
            }
            Request::RedeemTicket { ticket } => {
                // One-shot by construction: redemption removes the ticket.
                let pending = self.tickets.remove(&ticket).ok_or(ErrorCode::BadTicket)?;
                if now > pending.expires {
                    return Err(ErrorCode::BadTicket);
                }
                let s = self
                    .sessions
                    .get_mut(&session)
                    .ok_or(ErrorCode::BadHandle)?;
                Ok(Response::Handle(s.insert(pending.entry)))
            }
            Request::Stat { handle, path } => {
                let e = self.lookup(session, handle, R_READ)?;
                let data = match &e.target {
                    HandleTarget::Ref { name, subtree, .. } => self
                        .store
                        .read(name, &Self::full_path(subtree, &path))
                        .map_err(store_err)?,
                    HandleTarget::Snapshot { root } => {
                        self.store.read_at_root(root, &path).map_err(store_err)?
                    }
                };
                Ok(match data {
                    Some(d) => Response::SnapId(d.len() as u64),
                    None => Response::NotFound,
                })
            }
            Request::EnumerateSession => {
                let s = self.sessions.get(&session).ok_or(ErrorCode::BadHandle)?;
                // Enumerate is a per-handle right; require it on at least
                // one handle in the session (the supervisor pattern).
                if !s.handles.values().any(|h| h.rights & R_ENUMERATE != 0) {
                    return Err(ErrorCode::Denied);
                }
                let mut dump: Vec<(HandleId, String)> =
                    s.handles.iter().map(|(id, h)| (*id, describe(h))).collect();
                dump.sort();
                Ok(Response::SessionDump(dump))
            }
            Request::DeleteSnapshot { handle, snap_id } => {
                let name = self.rewrite_target(session, handle)?;
                self.store
                    .delete_snapshot(&name, snap_id)
                    .map_err(store_err)?;
                // Post-rewrite trigger (rev2§4.6): reclamation follows
                // promptly while this op stays O(small).
                self.gc_requested = true;
                Ok(Response::Ok)
            }
            Request::SetClass {
                handle,
                snap_id,
                class,
            } => {
                let name = self.rewrite_target(session, handle)?;
                self.store
                    .set_snapshot_class(&name, snap_id, class)
                    .map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::Gc { handle } => {
                self.rewrite_target(session, handle)?;
                let stats = self.run_gc().map_err(store_err)?;
                Ok(Response::GcReport {
                    live_objects: stats.live_objects,
                    freed_objects: stats.freed_objects,
                    freed_bytes: stats.freed_bytes,
                })
            }
            Request::Statfs { handle } => {
                // Store-global observation needs `stat-store` (rev2§2.3),
                // deny-by-default. `lookup` runs the generation check before
                // the rights check, so a revoked handle dies with `Stale` and
                // a handle lacking the bit is `Denied`. The handle's subtree
                // is intentionally ignored: this right's scope is the store.
                self.lookup(session, handle, R_STAT_STORE)?;
                let sp = self.store.space();
                Ok(Response::Space {
                    total: sp.total,
                    used: sp.used,
                    free: sp.free,
                })
            }
            Request::Apply {
                handle,
                expected_version,
                edits,
            } => {
                // Same gate as DeleteSnapshot/Gc: `may-rewrite-history` on a
                // ref-root handle. The store is the single authority — it does
                // the version check, validates every edit, and commits once.
                let name = self.rewrite_target(session, handle)?;
                // A batched snapshot deletion is history rewriting (rev2§4.6),
                // so it arms the same post-rewrite GC trigger DeleteSnapshot
                // uses — but only once the batch actually commits.
                let deletes = edits
                    .iter()
                    .any(|e| matches!(e, RefEdit::DeleteSnapshot { .. }));
                match self.store.apply_batch(&name, expected_version, &edits) {
                    Ok(edit_version) => {
                        if deletes {
                            self.gc_requested = true;
                        }
                        Ok(Response::Applied { edit_version })
                    }
                    // Carries the current version, so it is a data reply, not
                    // an `ErrorCode` (rev2§4.7 "fails carrying the version").
                    Err(StoreError::VersionMismatch { current }) => Ok(Response::VersionMismatch {
                        edit_version: current,
                    }),
                    Err(e) => Err(store_err(e)),
                }
            }
            Request::Tag {
                handle,
                name,
                snap_id,
            } => {
                // Tags are row surgery (rev2§4.7), so the same gate as
                // DeleteSnapshot/Apply: `may-rewrite-history` on a ref-root
                // handle. The tag is scoped to that ref.
                let ref_name = self.rewrite_target(session, handle)?;
                self.store
                    .tag(&name, &ref_name, snap_id)
                    .map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::Untag { handle, name } => {
                let ref_name = self.rewrite_target(session, handle)?;
                self.store.untag(&ref_name, &name).map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::ListTags { handle } => {
                // Read-only enumeration, scoped to the handle's ref (mirrors
                // `ListSnapshots`): a ref handle sees only its own tags.
                let e = self.lookup(session, handle, R_READ)?;
                let HandleTarget::Ref { name, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                let tags = self
                    .store
                    .tags()
                    .filter(|(_, r, _)| *r == name.as_slice())
                    .map(|(n, r, id)| (n.to_vec(), r.to_vec(), id))
                    .collect();
                Ok(Response::Tags(tags))
            }
        }
    }

    /// Common gate for history rewriting: a live ref handle with the
    /// `may-rewrite-history` right, at the ref root (surgery from a
    /// subtree view is not a thing). Returns the ref name.
    fn rewrite_target(&self, session: SessionId, handle: HandleId) -> Result<Vec<u8>, ErrorCode> {
        let e = self.lookup(session, handle, R_REWRITE_HISTORY)?;
        let HandleTarget::Ref { name, subtree, .. } = &e.target else {
            return Err(ErrorCode::ReadOnly);
        };
        if !subtree.is_empty() {
            return Err(ErrorCode::Denied);
        }
        Ok(name.clone())
    }

    fn resolve_ref_dir(&self, ref_name: &[u8], path: &TreePath) -> Result<(), ErrorCode> {
        if path.is_empty() {
            return Ok(());
        }
        let root = self
            .store
            .refs()
            .find(|(n, _)| n.as_slice() == ref_name)
            .ok_or(ErrorCode::Stale)?
            .1
            .root;
        self.resolve_snap_dir(&root, path).map(|_| ())
    }

    fn resolve_snap_dir(&self, root: &Hash, path: &TreePath) -> Result<Hash, ErrorCode> {
        let comps: Vec<&[u8]> = path.iter().map(|c| c.as_slice()).collect();
        match self.store.lookup_at_root(root, &comps) {
            Ok(Some(Entry {
                content: Content::DirRoot(h),
                ..
            })) => Ok(h),
            Ok(Some(_)) => Err(ErrorCode::NotADir),
            Ok(None) => Err(ErrorCode::BadPath),
            Err(_) => Err(ErrorCode::Internal),
        }
    }

    fn list_at_root(
        &self,
        root: &Hash,
        path: &TreePath,
    ) -> Result<Vec<(Vec<u8>, EntryKind, u64)>, StoreError> {
        let node = if path.is_empty() {
            *root
        } else {
            match self.resolve_snap_dir(root, path) {
                Ok(h) => h,
                Err(_) => return Ok(Vec::new()),
            }
        };
        self.store.list_dir_node(&node)
    }
}

fn describe(h: &HandleEntry) -> String {
    let rights = h.rights;
    match &h.target {
        HandleTarget::Ref { name, subtree, .. } => format!(
            "ref {} subtree depth {} rights {rights:#x}",
            String::from_utf8_lossy(name),
            subtree.len()
        ),
        HandleTarget::Snapshot { .. } => format!("snapshot rights {rights:#x}"),
    }
}

fn store_err(e: StoreError) -> ErrorCode {
    match e {
        StoreError::NoSuchRef => ErrorCode::Stale,
        StoreError::NoSuchSnapshot => ErrorCode::NoSuchSnapshot,
        StoreError::NotAFile => ErrorCode::BadPath,
        StoreError::Format(_) => ErrorCode::BadPath,
        StoreError::Pinned => ErrorCode::Pinned,
        StoreError::WriteOutOfRange => ErrorCode::BadOffset,
        // Only `Store::rename` produces `NotFound` (a missing source); surface
        // it as a path error rather than the catch-all `Internal`.
        StoreError::NotFound => ErrorCode::BadPath,
        _ => ErrorCode::Internal,
    }
}
