//! io-error classification policy (rev2§3.7).
//!
//! The total map from the syscall ABI error codes (the `ERR_*` constants the kernel
//! returns, [`crate::syscall`]) to a small, std-agnostic [`Kind`] the PAL translates
//! term-for-term into `std::io::ErrorKind`. The policy lives here — host-tested by
//! proptest for totality and against the ABI table — rather than in the trusted PAL
//! shell, so the mapping is a tested artifact and the PAL stays a thin translator.
//! Not byte-parsing, so proptest (not Verus) is the load-bearing tool; no `verus!{}`
//! obligation. The fs `ErrorCode` set (storage-server) extends this in a later phase.

use crate::syscall::{
    ERR_AGAIN, ERR_ARG, ERR_BADSLOT, ERR_CLOSED, ERR_EMPTY, ERR_FAULT, ERR_FULL, ERR_NOMEM,
    ERR_NOSLOT, ERR_PERM, ERR_STATE, ERR_TYPE,
};

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
        // ERR_STATE has no clean io::ErrorKind analog (rev2§3.7); it and every
        // non-ABI code fall through to Uncategorized.
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
        fn non_abi_codes_are_uncategorized(code in any::<i64>()) {
            prop_assume!(!ABI.iter().any(|&(c, _)| c == code));
            prop_assert_eq!(classify(code), Kind::Uncategorized);
        }
    }
}
