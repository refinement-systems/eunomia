//! The storaged filesystem client (std-port 4.1).
//!
//! The client half of the storage session protocol (rev2§4): it marshals the std
//! `sys/fs/eunomia` arm's file ops into `storage_server::Request`s over the pre-wired
//! session channel and decodes the `Response`s back. `File = (root handle, path,
//! cursor)` — storaged is offset-stateless (`Read`/`Write` carry an explicit offset),
//! so the seek cursor lives entirely on the std side; regular file I/O rides the root
//! handle + path directly (there is no file-open handle, rev2§4.9). `read_dir` is the one
//! op with client-side state: `List` returns the whole listing, which this crate
//! snapshots behind an integer handle (a small spinlock-guarded table) and hands to the
//! std `ReadDir` iterator one entry per `readdir_next` call, releasing it on drop.
//!
//! Trust posture: a **trusted marshalling shell** (the `sys/stdio` posture) over four
//! already-verified surfaces — the connect handshake ([`ipc::connect`]/`admit_connect`,
//! its `Admission` proven never to over-grant), the wire header/version prefix
//! (`wire::check_header`, total ∀ bytes), the rights lattice (`attenuate`, monotone),
//! and the path resolver ([`crate::path::resolve`], total ∀ bytes, root-confined,
//! std-port 4.2) — plus the trusted `svc` shell underneath [`ipc::SyscallTransport`].
//! No byte-parsing logic lives here: [`resolve_path`] is a thin `alloc` adapter that
//! calls the verified [`crate::path::resolve`] and copies its borrowed components into
//! the `Vec<Vec<u8>>` wire path, and the `read_dir` snapshot table is client bookkeeping
//! over already-decoded `DirEnt`s, not a wire codec.
//!
//! Gated to the eunomia/bare-metal targets: it links `storage-server`/`ipc` (target-only
//! deps), so the host `cargo verus verify -p eunomia-sys` graph never sees them.

#![cfg(bare_metal)]

use crate::grant::Startup;
use crate::io_error::{
    ERR_FS_BAD_HANDLE, ERR_FS_BAD_OFFSET, ERR_FS_BAD_PATH, ERR_FS_BAD_TICKET, ERR_FS_DENIED,
    ERR_FS_INTERNAL, ERR_FS_NOT_A_DIR, ERR_FS_NOT_FOUND, ERR_FS_NO_SESSION,
    ERR_FS_NO_SUCH_SNAPSHOT, ERR_FS_PINNED, ERR_FS_READ_ONLY, ERR_FS_STALE,
};
use crate::readdir::entry_head;
pub use crate::readdir::DirEntMeta;
use crate::syscall;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};
use ipc::{RecvErr, SendErr, SyscallTransport, Transport, VersionRange};
use storage_server::wire::{self, PROTO_VERSION};
use storage_server::{DirEnt, ErrorCode, Request, Response};
use urt::lock::SpinLock;

/// The storage-session channel's cspace slot (`NAME_STORAGE`), or `SLOT_NONE` when
/// this process was granted no session (a non-fs process — its fs ops then refuse
/// with [`ERR_FS_NO_SESSION`], never a bogus round-trip).
static SESSION_CHAN: AtomicU32 = AtomicU32::new(syscall::SLOT_NONE);
/// The full-rights ref-root handle on that session (`NAME_ROOT`, 0 in practice).
static ROOT_HANDLE: AtomicU32 = AtomicU32::new(0);
/// The negotiated storage wire version; `0` means "no live session" — either no
/// grant, or the connect handshake failed. Written once by [`attach`] at bootstrap.
static VERSION: AtomicU32 = AtomicU32::new(0);

/// Resolve the storage grants and run the client-side connect handshake, once, at
/// bootstrap (called from [`crate::bootstrap`] after the startup block is decoded).
/// A process without a `NAME_STORAGE` grant leaves the session unset — the
/// least-authority default. A handshake failure leaves `VERSION == 0`, so every fs
/// op refuses cleanly with [`ERR_FS_NO_SESSION`] rather than desyncing the channel.
pub fn attach(s: &Startup) {
    let Some(chan) = crate::grant::storage_slot(s) else {
        return;
    };
    SESSION_CHAN.store(chan, Ordering::Relaxed);
    ROOT_HANDLE.store(crate::grant::root_handle(s).unwrap_or(0), Ordering::Relaxed);
    // The connect handshake (rev2§3.5): offer the single storage wire version this
    // build speaks and record what storaged negotiates. Backpressure/empty-ring are
    // yield-polled — a bootstrap client has no notification cap for a reactor.
    if let Ok(ver) = ipc::connect(
        &SyscallTransport,
        chan,
        VersionRange::single(PROTO_VERSION),
        syscall::yield_now,
    ) {
        VERSION.store(ver as u32, Ordering::Relaxed);
    }
}

/// The live negotiated version, or `None` if there is no session.
fn version() -> Option<u8> {
    match VERSION.load(Ordering::Relaxed) {
        0 => None,
        v => Some(v as u8),
    }
}

/// One request/response round-trip against storaged (the shell's `request()` shape):
/// encode at the negotiated version, `send` (retry on backpressure), `recv` (yield on
/// an empty ring), decode. Every message is message-bounded (≤ 256 bytes, rev2§3.1);
/// the caller's chunk loops keep it so. A dead/absent session is [`ERR_FS_NO_SESSION`].
///
/// Public as the **admin escape hatch** for a client that delegates its whole storaged
/// session to this crate (the std-port 5.3 shell): its `std::fs` file ops ride the arms
/// below, while its versioned-store admin ops (`Snapshot`/`ListSnapshots`/`Rollback`/
/// `DeleteSnapshot`/`SetClass`/`Gc`/`Statfs`) — which `std::fs` cannot express — send
/// their `Request` here directly and read the raw `Response`. It reuses the one session
/// [`attach`] already connected ([`SESSION_CHAN`]/[`VERSION`]), so a caller must **not**
/// run its own connect handshake (storaged admits slot 1 exactly once, rev2§3.5).
pub fn request(req: &Request) -> Result<Response, i64> {
    let ver = version().ok_or(ERR_FS_NO_SESSION)?;
    let chan = SESSION_CHAN.load(Ordering::Relaxed);
    let bytes = wire::encode_request(req, ver).map_err(|_| ERR_FS_INTERNAL)?;
    let t = SyscallTransport;
    loop {
        match t.send_nb(chan, &bytes, None) {
            Ok(()) => break,
            Err(SendErr::Full) => syscall::yield_now(),
            Err(SendErr::Closed) => return Err(ERR_FS_NO_SESSION),
            Err(SendErr::Other(_)) => return Err(ERR_FS_INTERNAL),
        }
    }
    let mut buf = [0u8; 256];
    loop {
        match t.recv_nb(chan, &mut buf, None) {
            Ok(rx) => {
                return wire::decode_response(&buf[..rx.len], ver).map_err(|_| ERR_FS_INTERNAL);
            }
            Err(RecvErr::Empty) => syscall::yield_now(),
            Err(RecvErr::Closed) => return Err(ERR_FS_NO_SESSION),
            Err(RecvErr::NoSlot) | Err(RecvErr::Other(_)) => return Err(ERR_FS_INTERNAL),
        }
    }
}

/// Map a storaged [`ErrorCode`] (rev2§4) to its raw fs code (first-cut; 4.3 refines).
fn err_code(e: ErrorCode) -> i64 {
    match e {
        ErrorCode::BadHandle => ERR_FS_BAD_HANDLE,
        ErrorCode::Stale => ERR_FS_STALE,
        ErrorCode::Denied => ERR_FS_DENIED,
        ErrorCode::BadPath => ERR_FS_BAD_PATH,
        ErrorCode::NotADir => ERR_FS_NOT_A_DIR,
        ErrorCode::ReadOnly => ERR_FS_READ_ONLY,
        ErrorCode::NoSuchSnapshot => ERR_FS_NO_SUCH_SNAPSHOT,
        ErrorCode::BadTicket => ERR_FS_BAD_TICKET,
        ErrorCode::Internal => ERR_FS_INTERNAL,
        ErrorCode::Pinned => ERR_FS_PINNED,
        ErrorCode::BadOffset => ERR_FS_BAD_OFFSET,
    }
}

/// A `Response` a mutating op expects to be `Ok` → a `0`/`-err` status.
fn status(r: Result<Response, i64>) -> i64 {
    match r {
        Ok(Response::Ok) => 0,
        Ok(Response::NotFound) => ERR_FS_NOT_FOUND,
        Ok(Response::Err(e)) => err_code(e),
        Ok(_) => ERR_FS_INTERNAL,
        Err(c) => c,
    }
}

/// Resolve a raw path into storage tree components (`TreePath = Vec<Vec<u8>>`,
/// rev2§4.9), or a negative fs code if it is unnameable. The `.`/`..` resolution and
/// the confinement check are the **verified** [`crate::path::resolve`] (total ∀ bytes);
/// this only copies its borrowed components into owned `Vec`s over the global
/// allocator (the `alloc` step the no-alloc verified core leaves to the caller) and
/// translates the reject reason into an errno: a confinement **escape** (a `..` above
/// the process root handle, rev2§2.3 "unnameable → denied") is [`ERR_FS_DENIED`]
/// (`PermissionDenied`); a **malformed** component (NUL / > 255 bytes / too deep) is
/// [`ERR_FS_BAD_PATH`] (`InvalidFilename`) — the std-port 4.3 split.
fn resolve_path(path: &[u8]) -> Result<Vec<Vec<u8>>, i64> {
    let r = match crate::path::resolve(path) {
        Ok(r) => r,
        Err(crate::path::RejectReason::Escape) => return Err(ERR_FS_DENIED),
        Err(crate::path::RejectReason::Malformed) => return Err(ERR_FS_BAD_PATH),
    };
    let mut out = Vec::with_capacity(r.n);
    for j in 0..r.n {
        out.push(r.comps[j].to_vec());
    }
    Ok(out)
}

// Every storaged message is ≤ 256 bytes (rev2§3.1). A read requests a data chunk that
// leaves room for the `Response::Data` framing; a write chunk leaves room for the
// `Request::Write` framing *including the encoded path* — a very long path shrinks the
// usable write chunk (a disclosed limit until the bulk data plane, rev2§3.1).
const READ_CHUNK: usize = 192;
const WRITE_CHUNK: usize = 128;

/// Read up to `buf.len()` bytes at `offset` (one message; a short read is EOF/allowed —
/// std's readers loop). Returns bytes read (`0` at EOF) or a negative fs code.
pub fn read(path: &[u8], offset: u64, buf: &mut [u8]) -> i64 {
    if buf.is_empty() {
        return 0;
    }
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let components = match resolve_path(path) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let want = buf.len().min(READ_CHUNK) as u32;
    let req = Request::Read {
        handle,
        path: components,
        offset,
        len: want,
    };
    match request(&req) {
        Ok(Response::Data(d)) => {
            let n = d.len().min(buf.len());
            buf[..n].copy_from_slice(&d[..n]);
            n as i64
        }
        Ok(Response::NotFound) => ERR_FS_NOT_FOUND,
        Ok(Response::Err(e)) => err_code(e),
        Ok(_) => ERR_FS_INTERNAL,
        Err(c) => c,
    }
}

/// Write all of `data` starting at `offset`, chunked to fit one message each. The file
/// is created on first write (creation is a side effect of `Write`, rev2§4.9). Returns
/// bytes written (`== data.len()` on success) or a negative fs code.
pub fn write(path: &[u8], offset: u64, data: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let components = match resolve_path(path) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let mut written = 0usize;
    while written < data.len() {
        let end = (written + WRITE_CHUNK).min(data.len());
        let req = Request::Write {
            handle,
            path: components.clone(),
            offset: offset + written as u64,
            data: data[written..end].to_vec(),
        };
        match request(&req) {
            Ok(Response::Ok) => written = end,
            Ok(Response::NotFound) => return ERR_FS_NOT_FOUND,
            Ok(Response::Err(e)) => return err_code(e),
            Ok(_) => return ERR_FS_INTERNAL,
            Err(c) => return c,
        }
    }
    written as i64
}

/// The size of the file at `path` (`Stat`), or a negative fs code — [`ERR_FS_NOT_FOUND`]
/// if absent. Used by `File::open` (existence) and `File::seek(End)` (size); `Stat`
/// reads the file content length, so it answers for files only. Directory-aware
/// metadata (kind + size) is [`metadata`], which probes `List` when `Stat` reports no
/// file content.
pub fn stat(path: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let components = match resolve_path(path) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let req = Request::Stat {
        handle,
        path: components,
    };
    match request(&req) {
        Ok(Response::SnapId(size)) => size as i64,
        Ok(Response::NotFound) => ERR_FS_NOT_FOUND,
        Ok(Response::Err(e)) => err_code(e),
        Ok(_) => ERR_FS_INTERNAL,
        Err(c) => c,
    }
}

/// Resolved file metadata for the std `sys/fs::stat`/`lstat`/`file_attr` arm
/// (std-port 4.3): the entry kind + size. `code == 0` on success (and `size`/`is_dir`
/// are meaningful); otherwise `code` is a negative fs code and `size`/`is_dir` are
/// zeroed. `#[repr(C)]` so it crosses the `extern "Rust"` seam to the std arm with a
/// fixed layout the std side mirrors (the `Vec<u8>`/slice seam posture, made explicit).
#[repr(C)]
pub struct Meta {
    pub code: i64,
    pub size: u64,
    pub is_dir: bool,
}

impl Meta {
    fn err(code: i64) -> Meta {
        Meta {
            code,
            size: 0,
            is_dir: false,
        }
    }
}

/// Resolve the kind + size of the entry at `path` by probing storaged. `Stat` answers
/// for a **file** with its content length; a **directory** is not a file, so storaged
/// answers its `Stat` with `Err(BadPath)`/`Err(NotADir)` (the store's `NotAFile`), and
/// a `List` probe then confirms the directory (rev2§4.9: a path is a file or a
/// directory, never both). A genuinely **absent** path answers `Stat` with `NotFound`
/// (the store read returns nothing) — no probe, so it keeps the clean `NotFound` errno.
/// Any other server `Err` is surfaced as-is. **Disclosed limit:** a directory whose
/// listing overflows one 256-byte message probes as [`ERR_FS_INTERNAL`] until the bulk
/// data plane (the same cap [`readdir`] discloses, rev2§3.1).
pub fn metadata(path: &[u8]) -> Meta {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let components = match resolve_path(path) {
        Ok(c) => c,
        Err(code) => return Meta::err(code),
    };
    // A file answers Stat with its content length.
    match request(&Request::Stat {
        handle,
        path: components.clone(),
    }) {
        Ok(Response::SnapId(size)) => {
            return Meta {
                code: 0,
                size,
                is_dir: false,
            };
        }
        // NotFound means the path is genuinely absent (the store read returned
        // nothing) — a directory is reported "not a file", below, not NotFound.
        Ok(Response::NotFound) => return Meta::err(ERR_FS_NOT_FOUND),
        // "Not a file" (the store's `NotAFile` → BadPath, or NotADir): the entry may
        // be a directory — probe List to confirm and report `is_dir`.
        Ok(Response::Err(ErrorCode::BadPath | ErrorCode::NotADir)) => {}
        Ok(Response::Err(e)) => return Meta::err(err_code(e)),
        Ok(_) => return Meta::err(ERR_FS_INTERNAL),
        Err(c) => return Meta::err(c),
    }
    // Not a file. A directory answers List with a listing.
    match request(&Request::List {
        handle,
        path: components,
    }) {
        Ok(Response::Listing(_)) => Meta {
            code: 0,
            size: 0,
            is_dir: true,
        },
        Ok(Response::NotFound) => Meta::err(ERR_FS_NOT_FOUND),
        Ok(Response::Err(e)) => Meta::err(err_code(e)),
        Ok(_) => Meta::err(ERR_FS_INTERNAL),
        Err(c) => Meta::err(c),
    }
}

/// Rename `from` to `to` within the handle's subtree (`Rename`). `0` or a negative code.
pub fn rename(from: &[u8], to: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let from = match resolve_path(from) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let to = match resolve_path(to) {
        Ok(c) => c,
        Err(code) => return code,
    };
    status(request(&Request::Rename { handle, from, to }))
}

/// Remove the file at `path` (`Unlink`). `0` or a negative code.
pub fn unlink(path: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let path = match resolve_path(path) {
        Ok(c) => c,
        Err(code) => return code,
    };
    status(request(&Request::Unlink { handle, path }))
}

/// Flush the ref durably (`Sync`; storaged syncs the whole ref, so `fsync`/`datasync`
/// and a path-less `sync_all` all land here). `0` or a negative code.
pub fn sync() -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    status(request(&Request::Sync { handle }))
}

// ── The `read_dir` snapshot table ──
// A `read_dir` snapshots the whole `List` response and walks it one entry at a time.
// Structured `DirEnt`s stay structured all the way to the std arm — no byte layout
// crosses the seam — so the snapshot lives here as an owned `Vec<DirEnt>` behind an
// integer handle, and `readdir_next` copies one name out per call. Client bookkeeping,
// not protocol logic.

/// One open `read_dir`: the whole directory snapshot captured at open time (the same
/// whole-listing snapshot the old flat buffer materialized, rev2§4.9) plus the
/// client-side cursor into it.
struct DirHandle {
    entries: Vec<DirEnt>,
    cursor: usize,
}

/// The process-global open-`read_dir` table — client bookkeeping guarded by a spinlock
/// (the `urt::random` `STATE` posture: a `SpinLock` over an `UnsafeCell`, every access
/// under the lock). A handle is an index into `slots`; a `None` slot is free and reused
/// by the next open.
struct ReadDirTable {
    lock: SpinLock,
    slots: UnsafeCell<Vec<Option<DirHandle>>>,
}

// SAFETY: `slots` is only ever reached while `lock` is held (the `urt::random` `STATE`
// posture — mutual exclusion by the spinlock).
unsafe impl Sync for ReadDirTable {}

impl ReadDirTable {
    const fn new() -> ReadDirTable {
        ReadDirTable {
            lock: SpinLock::new(),
            slots: UnsafeCell::new(Vec::new()),
        }
    }

    /// Stash `entries` in a free slot and return its handle (`>= 0`).
    fn open(&self, entries: Vec<DirEnt>) -> i64 {
        let _g = self.lock.lock();
        // SAFETY: exclusive under `lock`.
        let slots = unsafe { &mut *self.slots.get() };
        let h = DirHandle { entries, cursor: 0 };
        match slots.iter().position(Option::is_none) {
            Some(i) => {
                slots[i] = Some(h);
                i as i64
            }
            None => {
                slots.push(Some(h));
                (slots.len() - 1) as i64
            }
        }
    }

    /// Copy the next entry's name into `name_buf` and return its head, advancing the
    /// cursor. `code == 1` at end of listing; `code < 0` for a bad handle or an over-long
    /// name (both advance-safe — the cursor still moves on an over-long name so a resilient
    /// consumer terminates).
    fn next(&self, handle: i64, name_buf: &mut [u8]) -> DirEntMeta {
        let _g = self.lock.lock();
        // SAFETY: exclusive under `lock`.
        let slots = unsafe { &mut *self.slots.get() };
        let Some(h) = usize::try_from(handle)
            .ok()
            .and_then(|i| slots.get_mut(i))
            .and_then(Option::as_mut)
        else {
            return DirEntMeta::err(ERR_FS_BAD_HANDLE);
        };
        let Some(e) = h.entries.get(h.cursor) else {
            return DirEntMeta::end();
        };
        let (kind, size, name) = match e {
            DirEnt::File { name, size } => (0u8, *size, name.as_slice()),
            DirEnt::Dir { name } => (1u8, 0u64, name.as_slice()),
        };
        let head = entry_head(kind, size, name, name_buf);
        h.cursor += 1;
        head
    }

    /// Drop the snapshot at `handle`, freeing its slot for reuse. A stale/out-of-range
    /// handle is a no-op.
    fn close(&self, handle: i64) {
        let _g = self.lock.lock();
        // SAFETY: exclusive under `lock`.
        let slots = unsafe { &mut *self.slots.get() };
        if let Ok(i) = usize::try_from(handle) {
            if let Some(slot) = slots.get_mut(i) {
                *slot = None;
            }
        }
    }
}

static READDIR_TABLE: ReadDirTable = ReadDirTable::new();

/// Open a `read_dir` snapshot for `path` (`List`): run the round-trip and stash the
/// listing behind a handle (`>= 0`), or return a negative fs code — an error surfaces at
/// `read_dir` time, like the old tagged buffer did. A listing that overflows one 256-byte
/// message errors (`ERR_FS_INTERNAL`) — big directory listings await the bulk data plane
/// (disclosed, rev2§3.1).
pub fn readdir_open(path: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let components = match resolve_path(path) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let req = Request::List {
        handle,
        path: components,
    };
    match request(&req) {
        Ok(Response::Listing(entries)) => READDIR_TABLE.open(entries),
        Ok(Response::NotFound) => ERR_FS_NOT_FOUND,
        Ok(Response::Err(e)) => err_code(e),
        Ok(_) => ERR_FS_INTERNAL,
        Err(c) => c,
    }
}

/// Copy the next entry of the snapshot `handle` into `name_buf` and return its head
/// (`code`: `0` = entry, `1` = end, `< 0` = fs code).
pub fn readdir_next(handle: i64, name_buf: &mut [u8]) -> DirEntMeta {
    READDIR_TABLE.next(handle, name_buf)
}

/// Release the snapshot `handle` (called from the std `ReadDir` drop).
pub fn readdir_close(handle: i64) {
    READDIR_TABLE.close(handle);
}
