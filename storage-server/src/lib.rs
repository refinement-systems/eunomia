//! Storage server core: sessions, handles, tickets (spec rev0§2.2-2.4).
//!
//! A storage cap at the boundary is a small integer handle, meaningful
//! only relative to its session. The server keeps, per session:
//!
//!   handle → (kind: snapshot | ref, target, subtree, rights, gen-at-grant)
//!
//! The wire protocol is handle-relative: every operation names a handle
//! plus a component-list path resolved *under* the handle's subtree —
//! confinement by unreachability, not checked policy (rev0§2.3). Raw hashes
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

// ── Rights (rev0§2.3) ───────────────────────────────────────────────────

pub const R_READ: u8 = 1 << 0;
pub const R_WRITE: u8 = 1 << 1;
pub const R_SNAPSHOT: u8 = 1 << 2;
/// Destructive enough to deserve its own bit (rev0§2.3); also gates mass
/// revocation (generation bump, rev0§2.2).
pub const R_REWRITE_HISTORY: u8 = 1 << 3;
pub const R_ENUMERATE: u8 = 1 << 4;
pub const R_ALL: u8 = 0b1_1111;

pub type SessionId = u64;
pub type HandleId = u32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleTarget {
    /// Immutable subtree, denoted by its node hash (internal only — the
    /// hash never crosses the boundary).
    Snapshot { root: Hash },
    /// Live ref, subtree-scoped by server-side path resolution (rev0§2.3).
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

/// Handle-relative requests (rev0§2.4). Paths are component lists; `/` is
/// shell presentation. Capability-bearing results are handle ids; tickets
/// are the only bearer tokens and are one-shot with a TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
pub enum Request {
    Read { handle: HandleId, path: TreePath, offset: u64, len: u32 },
    Write { handle: HandleId, path: TreePath, offset: u64, data: Vec<u8> },
    Unlink { handle: HandleId, path: TreePath },
    List { handle: HandleId, path: TreePath },
    /// Attenuate: sub-subtree + rights mask, in one step (rev0§2.4 delegation).
    OpenChild { handle: HandleId, path: TreePath, rights_mask: u8 },
    Close { handle: HandleId },
    Sync { handle: HandleId },
    Snapshot { handle: HandleId, message: Vec<u8>, class: u8 },
    ListSnapshots { handle: HandleId },
    /// A snapshot handle from a ref handle's history, subtree-scoped.
    OpenSnapshot { handle: HandleId, snap_id: u64, path: TreePath, rights_mask: u8 },
    Rollback { handle: HandleId, snap_id: u64 },
    /// Mass revocation: bump the ref's generation; every outstanding
    /// handle on it (all sessions) goes stale on next use (rev0§2.2).
    RevokeRef { handle: HandleId },
    MintTicket { handle: HandleId, ttl_nanos: u64 },
    RedeemTicket { ticket: [u8; 16] },
    /// Size of a file (None response = absent).
    Stat { handle: HandleId, path: TreePath },
    EnumerateSession,
    /// History rewriting (rev0§4.6-4.7): drop one snapshot row. Sets the
    /// post-rewrite GC trigger; the reclamation itself is asynchronous.
    DeleteSnapshot { handle: HandleId, snap_id: u64 },
    /// Edit a snapshot's retention class (the "mark survivors keep" flow).
    SetClass { handle: HandleId, snap_id: u64, class: u8 },
    /// Run a GC cycle now (the manual trigger).
    Gc { handle: HandleId },
    /// Chunk-region space accounting.
    Statfs { handle: HandleId },
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
    Snapshots(Vec<SnapInfo>),
    SnapId(u64),
    Ticket([u8; 16]),
    SessionDump(Vec<(HandleId, String)>),
    Err(ErrorCode),
    GcReport { live_objects: u64, freed_objects: u64, freed_bytes: u64 },
    Space { total: u64, used: u64, free: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ErrorCode {
    BadHandle,
    /// Generation mismatch: the handle was mass-revoked (rev0§2.2).
    Stale,
    Denied,
    BadPath,
    NotADir,
    ReadOnly,
    NoSuchSnapshot,
    BadTicket,
    Internal,
    /// The snapshot is a tag target; tags are keep-strength pins (rev0§4.7).
    Pinned,
    /// Write offset/length out of range (overflow or beyond store capacity).
    BadOffset,
}

// ── Server ──────────────────────────────────────────────────────────────

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
    /// GC requested by a trigger (rev0§4.6): a history-rewriting op, or the
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
    /// spawn path (rev0§2.4): the parent names handles + attenuation, the
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

    /// Transport's peer-closed path (rev0§2.4 cleanup): drop the whole table.
    pub fn close_session(&mut self, id: SessionId) {
        self.sessions.remove(&id);
    }

    /// Convenience for wiring init's world: a full-rights handle at a
    /// ref's root.
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
            rights: R_ALL,
        })
    }

    pub fn handle(&mut self, session: SessionId, req: Request, now: u64) -> Response {
        let resp = match self.dispatch(session, req, now) {
            Ok(resp) => resp,
            Err(e) => Response::Err(e),
        };
        // The crude space watermark (rev0§4.6): below ~20% free, request a
        // cycle — unless GC already ran at this generation and this is
        // simply how full the store is.
        let sp = self.store.space();
        if sp.free * 5 < sp.total && self.store.generation() != self.gc_done_gen {
            self.gc_requested = true;
        }
        resp
    }

    /// Validate handle liveness (generation check — lazy mass revocation,
    /// rev0§2.2) and the rights needed for the op.
    fn lookup(
        &self,
        session: SessionId,
        handle: HandleId,
        need: u8,
    ) -> Result<HandleEntry, ErrorCode> {
        let s = self.sessions.get(&session).ok_or(ErrorCode::BadHandle)?;
        let e = s.handles.get(&handle).ok_or(ErrorCode::BadHandle)?.clone();
        if let HandleTarget::Ref { name, gen_at_grant, .. } = &e.target {
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
    /// (rev0§4.9); reject them and any malformed component up front.
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
            _ => {}
        }
        match req {
            Request::Read { handle, path, offset, len } => {
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
            Request::Write { handle, path, offset, data } => {
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
            Request::OpenChild { handle, path, rights_mask } => {
                let e = self.lookup(session, handle, 0)?;
                // Monotone derivation (rev0§2.3): intersection only.
                let rights = e.rights & rights_mask;
                let entry = match &e.target {
                    HandleTarget::Ref { name, subtree, gen_at_grant } => {
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
                let s = self.sessions.get_mut(&session).ok_or(ErrorCode::BadHandle)?;
                Ok(Response::Handle(s.insert(entry)))
            }
            Request::Close { handle } => {
                let s = self.sessions.get_mut(&session).ok_or(ErrorCode::BadHandle)?;
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
            Request::Snapshot { handle, message, class } => {
                let e = self.lookup(session, handle, R_SNAPSHOT)?;
                let HandleTarget::Ref { name, .. } = &e.target else {
                    return Err(ErrorCode::ReadOnly);
                };
                // Provenance is server-filled (rev0§4.7), never client-supplied.
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
                Ok(Response::Snapshots(
                    self.store
                        .snapshots(name)
                        .map(|r| SnapInfo {
                            id: r.id,
                            timestamp: r.timestamp,
                            provenance: r.provenance.clone(),
                            message: r.message.clone(),
                            class: r.class,
                        })
                        .collect(),
                ))
            }
            Request::OpenSnapshot { handle, snap_id, path, rights_mask } => {
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
                let rights = e.rights & rights_mask & (R_READ | R_ENUMERATE);
                let s = self.sessions.get_mut(&session).ok_or(ErrorCode::BadHandle)?;
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
                // Post-rewrite trigger (rev0§4.6): the abandoned head (unless
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
                self.store.bump_generation(&name.clone()).map_err(store_err)?;
                Ok(Response::Ok)
            }
            Request::MintTicket { handle, ttl_nanos } => {
                let e = self.lookup(session, handle, 0)?;
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
                    PendingTicket { entry: e, expires: now.saturating_add(ttl_nanos) },
                );
                Ok(Response::Ticket(ticket))
            }
            Request::RedeemTicket { ticket } => {
                // One-shot by construction: redemption removes the ticket.
                let pending = self.tickets.remove(&ticket).ok_or(ErrorCode::BadTicket)?;
                if now > pending.expires {
                    return Err(ErrorCode::BadTicket);
                }
                let s = self.sessions.get_mut(&session).ok_or(ErrorCode::BadHandle)?;
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
                let mut dump: Vec<(HandleId, String)> = s
                    .handles
                    .iter()
                    .map(|(id, h)| (*id, describe(h)))
                    .collect();
                dump.sort();
                Ok(Response::SessionDump(dump))
            }
            Request::DeleteSnapshot { handle, snap_id } => {
                let name = self.rewrite_target(session, handle)?;
                self.store.delete_snapshot(&name, snap_id).map_err(store_err)?;
                // Post-rewrite trigger (rev0§4.6): reclamation follows
                // promptly while this op stays O(small).
                self.gc_requested = true;
                Ok(Response::Ok)
            }
            Request::SetClass { handle, snap_id, class } => {
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
                self.lookup(session, handle, 0)?;
                let sp = self.store.space();
                Ok(Response::Space { total: sp.total, used: sp.used, free: sp.free })
            }
        }
    }

    /// Common gate for history rewriting: a live ref handle with the
    /// `may-rewrite-history` right, at the ref root (surgery from a
    /// subtree view is not a thing). Returns the ref name.
    fn rewrite_target(
        &self,
        session: SessionId,
        handle: HandleId,
    ) -> Result<Vec<u8>, ErrorCode> {
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
            Ok(Some(Entry { content: Content::DirRoot(h), .. })) => Ok(h),
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
        _ => ErrorCode::Internal,
    }
}
