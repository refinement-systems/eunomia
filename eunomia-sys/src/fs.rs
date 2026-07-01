//! The storaged filesystem client (std-port 4.1).
//!
//! The client half of the storage session protocol (rev2§4): it marshals the std
//! `sys/fs/eunomia` arm's file ops into `storage_server::Request`s over the pre-wired
//! session channel and decodes the `Response`s back. `File = (root handle, path,
//! cursor)` — storaged is offset-stateless (`Read`/`Write` carry an explicit offset),
//! so the seek cursor lives entirely on the std side; regular file I/O rides the root
//! handle + path directly (there is no file-open handle, rev2§4.9).
//!
//! Trust posture: a **trusted marshalling shell** (the `sys/stdio` posture) over three
//! already-verified surfaces — the connect handshake ([`ipc::connect`]/`admit_connect`,
//! its `Admission` proven never to over-grant), the wire header/version prefix
//! (`wire::check_header`, total ∀ bytes), and the rights lattice (`attenuate`, monotone)
//! — plus the trusted `svc` shell underneath [`ipc::SyscallTransport`]. No new
//! byte-parsing logic lives here except the minimal path split ([`split_path`]), which
//! is the seam 4.2 replaces with a Verus-total, fuzzed, `.`/`..`-resolving parser.
//!
//! Gated to the eunomia/bare-metal targets: it links `storage-server`/`ipc` (target-only
//! deps), so the host `cargo verus verify -p eunomia-sys` graph never sees them.

#![cfg(any(target_os = "eunomia", target_os = "none"))]

use crate::grant::Startup;
use crate::io_error::{
    ERR_FS_BAD_HANDLE, ERR_FS_BAD_OFFSET, ERR_FS_BAD_PATH, ERR_FS_BAD_TICKET, ERR_FS_DENIED,
    ERR_FS_INTERNAL, ERR_FS_NOT_A_DIR, ERR_FS_NOT_FOUND, ERR_FS_NO_SESSION,
    ERR_FS_NO_SUCH_SNAPSHOT, ERR_FS_PINNED, ERR_FS_READ_ONLY, ERR_FS_STALE,
};
use crate::syscall;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ipc::{RecvErr, SendErr, SyscallTransport, Transport, VersionRange};
use storage_server::wire::{self, PROTO_VERSION};
use storage_server::{DirEnt, ErrorCode, Request, Response};

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
fn request(req: &Request) -> Result<Response, i64> {
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

/// Split a raw path into storage tree components — **the 4.2 seam** (std-port 4.1).
/// Minimal: split on `/`, drop empty components (leading/trailing/`//`). No `.`/`..`
/// resolution or root confinement yet; 4.2 replaces this with the Verus-total, fuzzed,
/// `.`/`..`-resolving, root-confining parser (`TreePath = Vec<Vec<u8>>`, rev2§4.9).
fn split_path(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(|c| c.to_vec())
        .collect()
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
    let want = buf.len().min(READ_CHUNK) as u32;
    let req = Request::Read {
        handle,
        path: split_path(path),
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
    let components = split_path(path);
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
/// if absent. Used by `File::open` (existence) and `File::seek(End)`/`metadata` (size).
/// `Stat` reads the file content length, so it answers for files; a directory's type is
/// read from `readdir` instead (4.3 gives directories a fuller `metadata`).
pub fn stat(path: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let req = Request::Stat {
        handle,
        path: split_path(path),
    };
    match request(&req) {
        Ok(Response::SnapId(size)) => size as i64,
        Ok(Response::NotFound) => ERR_FS_NOT_FOUND,
        Ok(Response::Err(e)) => err_code(e),
        Ok(_) => ERR_FS_INTERNAL,
        Err(c) => c,
    }
}

/// Rename `from` to `to` within the handle's subtree (`Rename`). `0` or a negative code.
pub fn rename(from: &[u8], to: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    status(request(&Request::Rename {
        handle,
        from: split_path(from),
        to: split_path(to),
    }))
}

/// Remove the file at `path` (`Unlink`). `0` or a negative code.
pub fn unlink(path: &[u8]) -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    status(request(&Request::Unlink {
        handle,
        path: split_path(path),
    }))
}

/// Flush the ref durably (`Sync`; storaged syncs the whole ref, so `fsync`/`datasync`
/// and a path-less `sync_all` all land here). `0` or a negative code.
pub fn sync() -> i64 {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    status(request(&Request::Sync { handle }))
}

// The readdir wire between `fs.rs` and the std `sys/fs/eunomia` arm: a flat `Vec<u8>`
// over the shared global allocator. Both sides are the same rustc/std, so ownership of
// a `Vec<u8>` crosses the `extern "Rust"` seam soundly (the `__eunomia_argv` posture).
// Layout:
//   byte 0: 0 = ok (entries follow), 1 = error (an 8-byte i64 code, LE, follows)
//   each entry: [kind: u8 (0 = file, 1 = dir)][size: u64 LE][name_len: u16 LE][name…]
const RD_OK: u8 = 0;
const RD_ERR: u8 = 1;

/// List the directory at `path` (`List`), encoded for the std `ReadDir` iterator. A
/// listing that overflows one 256-byte message errors (`ERR_FS_INTERNAL`) — big
/// directory listings await the bulk data plane (disclosed, rev2§3.1).
pub fn readdir(path: &[u8]) -> Vec<u8> {
    let handle = ROOT_HANDLE.load(Ordering::Relaxed);
    let req = Request::List {
        handle,
        path: split_path(path),
    };
    match request(&req) {
        Ok(Response::Listing(entries)) => {
            let mut out = Vec::new();
            out.push(RD_OK);
            for e in entries {
                let (kind, size, name) = match e {
                    DirEnt::File { name, size } => (0u8, size, name),
                    DirEnt::Dir { name } => (1u8, 0u64, name),
                };
                out.push(kind);
                out.extend_from_slice(&size.to_le_bytes());
                out.extend_from_slice(&(name.len() as u16).to_le_bytes());
                out.extend_from_slice(&name);
            }
            out
        }
        Ok(Response::NotFound) => err_buf(ERR_FS_NOT_FOUND),
        Ok(Response::Err(e)) => err_buf(err_code(e)),
        Ok(_) => err_buf(ERR_FS_INTERNAL),
        Err(c) => err_buf(c),
    }
}

/// An error-tagged readdir buffer carrying the raw fs `code`.
fn err_buf(code: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(RD_ERR);
    out.extend_from_slice(&code.to_le_bytes());
    out
}
