//! The `read_dir` cursor head shared with the std `sys/fs/eunomia` arm (std-port 4.1).
//!
//! `ReadDir` crosses the seam as an integer handle, not a serialized buffer: the
//! target-gated [`crate::fs`] client snapshots the directory listing at open time and
//! hands the std iterator one entry per `readdir_next` call. Each call returns a
//! [`DirEntMeta`] head **by value** — the `#[repr(C)]` twin the std side mirrors (the
//! `Meta`/`FsMeta` posture) — and copies the entry name into a caller-provided buffer.
//! No byte layout crosses the bridge, so there is no codec to keep in lockstep.
//!
//! This module holds only the cfg-free head type and the pure name-copy arithmetic
//! ([`entry_head`]) — core-only, no `DirEnt`/wire types — so they build and unit-test on
//! the host (the fs client is otherwise `cfg(bare_metal)`-only and untestable off-target).
//! The snapshot table and the `List` round-trip live in [`crate::fs`].

use crate::io_error::ERR_FS_INTERNAL;

/// The per-entry head `readdir_next` returns across the `extern "Rust"` seam, mirrored
/// `#[repr(C)]` by the std arm (`FsDirEntMeta`) with identical field order/types — the
/// fixed layout that makes the by-value return sound (the `Meta`/`FsMeta` posture). `code`
/// is the tag: `0` = an entry (`kind`/`size`/`name_len` meaningful, the name copied into
/// the caller's buffer), `1` = end of the listing, `< 0` = a raw fs error code. `kind` is
/// `0` for a file, `1` for a directory (the wire `DirEnt` split).
#[repr(C)]
pub struct DirEntMeta {
    pub code: i64,
    pub kind: u8,
    pub size: u64,
    pub name_len: u16,
}

impl DirEntMeta {
    /// The end-of-listing head (`code == 1`); the other fields are unread by the arm.
    pub fn end() -> DirEntMeta {
        DirEntMeta {
            code: 1,
            kind: 0,
            size: 0,
            name_len: 0,
        }
    }

    /// An error head carrying the raw fs `code` (`< 0`); the other fields are unread.
    pub fn err(code: i64) -> DirEntMeta {
        DirEntMeta {
            code,
            kind: 0,
            size: 0,
            name_len: 0,
        }
    }
}

/// Copy one entry's `name` into `name_buf` and build its entry head. A name that does not
/// fit the buffer is **refused with [`ERR_FS_INTERNAL`], never truncated**: the path
/// resolver bounds a component at 255 bytes (rev2§4.9), so the std arm sizes `name_buf` to
/// that bound and a longer name is a server-side invariant break, not a client limit.
/// Pure over its arguments (no snapshot table, no wire types) so it host-unit-tests;
/// [`crate::fs`] wraps it with the lock, the handle lookup, and the cursor advance.
pub fn entry_head(kind: u8, size: u64, name: &[u8], name_buf: &mut [u8]) -> DirEntMeta {
    if name.len() > name_buf.len() {
        return DirEntMeta::err(ERR_FS_INTERNAL);
    }
    name_buf[..name.len()].copy_from_slice(name);
    DirEntMeta {
        code: 0,
        kind,
        size,
        name_len: name.len() as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_head_carries_kind_size_and_name() {
        let mut buf = [0u8; 255];
        let m = entry_head(0, 42, b"smoke", &mut buf);
        assert_eq!(m.code, 0);
        assert_eq!(m.kind, 0);
        assert_eq!(m.size, 42);
        assert_eq!(m.name_len, 5);
        assert_eq!(&buf[..5], b"smoke");
    }

    #[test]
    fn dir_head_is_kind_one_size_zero() {
        let mut buf = [0u8; 255];
        let m = entry_head(1, 0, b"docs", &mut buf);
        assert_eq!(m.code, 0);
        assert_eq!(m.kind, 1);
        assert_eq!(m.size, 0);
        assert_eq!(m.name_len, 4);
        assert_eq!(&buf[..4], b"docs");
    }

    #[test]
    fn empty_name_is_zero_length_entry() {
        let mut buf = [0u8; 255];
        let m = entry_head(0, 0, b"", &mut buf);
        assert_eq!(m.code, 0);
        assert_eq!(m.name_len, 0);
    }

    #[test]
    fn name_that_exactly_fills_the_buffer_is_accepted() {
        let name = [b'a'; 255];
        let mut buf = [0u8; 255];
        let m = entry_head(0, 0, &name, &mut buf);
        assert_eq!(m.code, 0);
        assert_eq!(m.name_len, 255);
        assert_eq!(&buf[..], &name[..]);
    }

    #[test]
    fn over_long_name_is_refused_not_truncated() {
        let name = [b'a'; 256];
        let mut buf = [0u8; 255];
        let m = entry_head(0, 7, &name, &mut buf);
        assert_eq!(m.code, ERR_FS_INTERNAL);
        // The buffer is left untouched — no partial fill.
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn end_and_err_heads() {
        assert_eq!(DirEntMeta::end().code, 1);
        assert_eq!(DirEntMeta::err(ERR_FS_INTERNAL).code, ERR_FS_INTERNAL);
    }
}
