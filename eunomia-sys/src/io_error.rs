//! io-error classification policy (rev2§3.7).
//!
//! The total map from the syscall ABI error codes (the `ERR_*` constants the kernel
//! returns, [`crate::syscall`]) to a small, std-agnostic [`Kind`] the PAL translates
//! term-for-term into `std::io::ErrorKind`. The policy lives here — host-tested by
//! proptest for totality and against the ABI table — rather than in the trusted PAL
//! shell, so the mapping is a tested artifact and the PAL stays a thin translator.
//! Not byte-parsing, so proptest (not Verus) is the load-bearing tool; no `verus!{}`
//! obligation.
//!
//! The fs `ErrorCode` set (storage-server, rev2§4) rides here too (std-port 4.1):
//! the fs client ([`crate::fs`], target-only) maps each storaged `Response::Err(code)`
//! / `NotFound` to one of the [`ERR_FS_NOT_FOUND`]-band raw codes below — a band well
//! clear of the syscall `ERR_*` block (`-1..-12`) so this one [`classify`] serves both.
//! std wraps them through `io::Error::from_raw_os_error`, so their kind flows through
//! the same path as a syscall error. This is the full rev2§4.9 decision table (all 11
//! `ErrorCode` variants + the client-only no-session code): each raw code maps to its
//! nearest std `io::ErrorKind` (std-port 4.3). Two of them — `Stale` and `Pinned` —
//! have no clean POSIX analog, so their targets ([`Kind::StaleNetworkFileHandle`] and
//! [`Kind::ResourceBusy`]) are documented nearest-fits, not a verification property.

use crate::syscall::{
    ERR_AGAIN, ERR_ARG, ERR_BADSLOT, ERR_CLOSED, ERR_EMPTY, ERR_FAULT, ERR_FULL, ERR_NOMEM,
    ERR_NOSLOT, ERR_PERM, ERR_STATE, ERR_TYPE,
};

// ── Storage-server fs error band (rev2§4, std-port 4.1) ──
// One raw code per `storage_server::{Response::NotFound, ErrorCode}` variant, based
// at `-256` so it never collides with the syscall `ERR_*` block. `crate::fs` (the
// target-only client) owns the `ErrorCode -> code` direction; `classify`/`message`
// below own `code -> Kind`/label — kept here so the map stays host-tested.
pub const ERR_FS_NOT_FOUND: i64 = -256;
pub const ERR_FS_BAD_HANDLE: i64 = -257;
pub const ERR_FS_STALE: i64 = -258;
pub const ERR_FS_DENIED: i64 = -259;
pub const ERR_FS_BAD_PATH: i64 = -260;
pub const ERR_FS_NOT_A_DIR: i64 = -261;
pub const ERR_FS_READ_ONLY: i64 = -262;
pub const ERR_FS_NO_SUCH_SNAPSHOT: i64 = -263;
pub const ERR_FS_BAD_TICKET: i64 = -264;
pub const ERR_FS_INTERNAL: i64 = -265;
pub const ERR_FS_PINNED: i64 = -266;
pub const ERR_FS_BAD_OFFSET: i64 = -267;
/// The client could not reach storaged (no session grant, or the handshake/round-trip
/// failed) — distinct from a server-returned error so the PAL can tell them apart.
pub const ERR_FS_NO_SESSION: i64 = -268;

/// A std-agnostic error category. Each variant maps 1:1 to a `std::io::ErrorKind` in
/// the PAL `sys/io/error/eunomia.rs` arm. Only the categories the ABI codes produce
/// are present; `Uncategorized` is the total fallback.
///
/// `#[repr(u8)]` with explicit discriminants: the PAL reaches this policy across the
/// `extern "Rust"` seam as a `u8` (the `pal::__eunomia_io_classify` shim), so these
/// numbers are ABI — the PAL's `decode_error_kind` match is kept in lockstep.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    PermissionDenied = 0,
    WouldBlock = 1,
    InvalidInput = 2,
    OutOfMemory = 3,
    BrokenPipe = 4,
    Uncategorized = 5,
    // std-port 4.1: the fs client needs a distinct `NotFound` (a missing file /
    // path), the most load-bearing fs error kind. Appended so the existing
    // discriminants (ABI to the PAL's `decode_error_kind`) stay put.
    NotFound = 6,
    // std-port 4.3: the fuller rev2§4.9 fs decision table maps each storaged
    // `ErrorCode` to its nearest std `io::ErrorKind`. These are appended (7..)
    // for the same ABI-stability reason; each name matches its `ErrorKind`
    // target so the PAL `decode_error_kind` lockstep is one-to-one. All six are
    // *stable* `io::ErrorKind`s (1.83–1.87).
    NotADirectory = 7,
    ReadOnlyFilesystem = 8,
    // `Stale` is a rev2§2.2 handle generation-mismatch (mass-revoke), *not* a
    // network filesystem — `StaleNetworkFileHandle` (ESTALE) is the nearest std
    // kind, a documented approximation.
    StaleNetworkFileHandle = 9,
    InvalidFilename = 10,
    NotConnected = 11,
    // `Pinned` is a rev2§4.7 tag pin refusing a deletion ≈ EBUSY (the resource is
    // in use); `ResourceBusy` is the nearest std kind, a documented approximation.
    ResourceBusy = 12,
}

/// Classify a raw syscall error code. Total: every `i64` yields a [`Kind`], never
/// panics. `code` is the negative `ERR_*` the kernel returned (the PAL passes it
/// straight from `io::Error::from_raw_os_error`).
pub fn classify(code: i64) -> Kind {
    match code {
        ERR_PERM => Kind::PermissionDenied,
        ERR_FULL | ERR_EMPTY | ERR_AGAIN => Kind::WouldBlock,
        ERR_NOMEM | ERR_NOSLOT => Kind::OutOfMemory,
        ERR_CLOSED => Kind::BrokenPipe,
        ERR_BADSLOT | ERR_TYPE | ERR_FAULT | ERR_ARG => Kind::InvalidInput,
        // ── fs band (std-port 4.3, the full rev2§4.9 decision table) ──
        ERR_FS_NOT_FOUND | ERR_FS_NO_SUCH_SNAPSHOT => Kind::NotFound,
        ERR_FS_DENIED => Kind::PermissionDenied,
        // A malformed/unnameable path component (the dominant `BadPath` case is the
        // client-side `path::resolve` rejection); a confinement escape is `Denied`
        // (rev2§2.3), not `BadPath`, so it lands on `PermissionDenied` above.
        ERR_FS_BAD_PATH => Kind::InvalidFilename,
        ERR_FS_NOT_A_DIR => Kind::NotADirectory,
        ERR_FS_READ_ONLY => Kind::ReadOnlyFilesystem,
        ERR_FS_STALE => Kind::StaleNetworkFileHandle,
        ERR_FS_PINNED => Kind::ResourceBusy,
        ERR_FS_NO_SESSION => Kind::NotConnected,
        // A bad handle id, a bad/expired ticket, and an out-of-range offset are all
        // "the argument was bad" — `InvalidInput`, no more specific stable kind.
        ERR_FS_BAD_HANDLE | ERR_FS_BAD_TICKET | ERR_FS_BAD_OFFSET => Kind::InvalidInput,
        // ERR_STATE (rev2§3.7) and ERR_FS_INTERNAL (a storaged fault / client
        // transport error) have no user-actionable analog; they and every non-ABI
        // code fall through to Uncategorized.
        _ => Kind::Uncategorized,
    }
}

/// A static human-readable string for a raw syscall error code. Total; no allocation
/// (the PAL `to_string()`s it for `error_string`). Unknown codes get a generic label.
pub fn message(code: i64) -> &'static str {
    match code {
        ERR_BADSLOT => "invalid capability slot",
        ERR_TYPE => "wrong object type",
        ERR_PERM => "operation not permitted",
        ERR_FULL => "channel full",
        ERR_EMPTY => "channel empty",
        ERR_NOSLOT => "no free capability slot",
        ERR_FAULT => "bad user pointer",
        ERR_NOMEM => "out of memory",
        ERR_ARG => "invalid argument",
        ERR_CLOSED => "channel peer closed",
        ERR_STATE => "object in wrong state",
        ERR_AGAIN => "resource temporarily unavailable",
        // ── fs band (std-port 4.1) ──
        ERR_FS_NOT_FOUND => "no such file or directory",
        ERR_FS_BAD_HANDLE => "bad storage handle",
        ERR_FS_STALE => "storage handle revoked (generation mismatch)",
        ERR_FS_DENIED => "storage access denied",
        ERR_FS_BAD_PATH => "invalid path",
        ERR_FS_NOT_A_DIR => "not a directory",
        ERR_FS_READ_ONLY => "read-only storage handle",
        ERR_FS_NO_SUCH_SNAPSHOT => "no such snapshot",
        ERR_FS_BAD_TICKET => "bad or expired ticket",
        ERR_FS_INTERNAL => "storage server internal error",
        ERR_FS_PINNED => "snapshot is pinned",
        ERR_FS_BAD_OFFSET => "offset out of range",
        ERR_FS_NO_SESSION => "no storage session",
        _ => "unknown error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // The full ABI map (rev2§3.7) — the oracle the PAL's `RawOsError` decode relies on.
    const ABI: &[(i64, Kind)] = &[
        (ERR_BADSLOT, Kind::InvalidInput),
        (ERR_TYPE, Kind::InvalidInput),
        (ERR_PERM, Kind::PermissionDenied),
        (ERR_FULL, Kind::WouldBlock),
        (ERR_EMPTY, Kind::WouldBlock),
        (ERR_NOSLOT, Kind::OutOfMemory),
        (ERR_FAULT, Kind::InvalidInput),
        (ERR_NOMEM, Kind::OutOfMemory),
        (ERR_ARG, Kind::InvalidInput),
        (ERR_CLOSED, Kind::BrokenPipe),
        (ERR_STATE, Kind::Uncategorized),
        (ERR_AGAIN, Kind::WouldBlock),
    ];

    // The fs band (std-port 4.3): the full rev2§4.9 storaged `ErrorCode` -> `Kind`
    // decision table — the oracle for the `classify` fs arm above.
    const FS: &[(i64, Kind)] = &[
        (ERR_FS_NOT_FOUND, Kind::NotFound),
        (ERR_FS_NO_SUCH_SNAPSHOT, Kind::NotFound),
        (ERR_FS_DENIED, Kind::PermissionDenied),
        (ERR_FS_BAD_PATH, Kind::InvalidFilename),
        (ERR_FS_NOT_A_DIR, Kind::NotADirectory),
        (ERR_FS_READ_ONLY, Kind::ReadOnlyFilesystem),
        (ERR_FS_STALE, Kind::StaleNetworkFileHandle),
        (ERR_FS_PINNED, Kind::ResourceBusy),
        (ERR_FS_NO_SESSION, Kind::NotConnected),
        (ERR_FS_BAD_HANDLE, Kind::InvalidInput),
        (ERR_FS_BAD_TICKET, Kind::InvalidInput),
        (ERR_FS_BAD_OFFSET, Kind::InvalidInput),
        (ERR_FS_INTERNAL, Kind::Uncategorized),
    ];

    #[test]
    fn abi_table_is_exact() {
        for &(code, kind) in ABI {
            assert_eq!(classify(code), kind, "classify({code})");
            assert_ne!(
                message(code),
                "unknown error",
                "ABI code {code} should have a specific message"
            );
        }
    }

    #[test]
    fn fs_band_is_exact_and_disjoint_from_syscall_band() {
        for &(code, kind) in FS {
            assert_eq!(classify(code), kind, "classify({code})");
            assert_ne!(
                message(code),
                "unknown error",
                "fs code {code} should have a specific message"
            );
            // The fs band must never collide with a syscall ERR_* code.
            assert!(
                !ABI.iter().any(|&(c, _)| c == code),
                "fs code {code} collides with the syscall band"
            );
        }
    }

    proptest! {
        #[test]
        fn classify_is_total(code in any::<i64>()) {
            let _ = classify(code); // never panics for any input
        }

        #[test]
        fn message_is_total(code in any::<i64>()) {
            let _ = message(code);
        }

        #[test]
        fn unmapped_codes_are_uncategorized(code in any::<i64>()) {
            // Every code outside the two mapped bands (syscall `ABI` + the fs `FS`
            // band) is the total fallback. Both must be excluded: the fs band is
            // *not* in `ABI` yet is not `Uncategorized`, so excluding `ABI` alone
            // (the pre-4.3 form) would contradict e.g. `ERR_FS_NOT_FOUND`.
            prop_assume!(!ABI.iter().any(|&(c, _)| c == code));
            prop_assume!(!FS.iter().any(|&(c, _)| c == code));
            prop_assert_eq!(classify(code), Kind::Uncategorized);
        }
    }
}
