//! Named-grant resolution (rev2§5.1).
//!
//! A thin, reusable resolver over the `loader::startup` decoder: it reads the
//! well-known grants out of an already-decoded [`Startup`] block. No new decode logic
//! — the untrusted byte boundary is `loader::startup::decode` (verified separately),
//! so this is plain bookkeeping over a validated structure. It consolidates the
//! per-binary `resolve_*` helpers (the shell's `user/shell/src/main.rs`, storaged's
//! inline `.grant()` matches) into one seam the PAL and future binaries share.
//!
//! The actual `chan_recv` of the slot-0 bootstrap message is the PAL `_start`'s job,
//! not this module's; here we only resolve grants out of the decoded block.

pub use loader::startup::{
    decode, Grant, GrantKind, Startup, NAME_DMA, NAME_PL011_MMIO, NAME_ROOT, NAME_SELF_ASPACE,
    NAME_SELF_CSPACE, NAME_STDIN, NAME_STDOUT, NAME_STORAGE, NAME_STRING, NAME_THREAD_SLOT_BASE,
    NAME_THREAD_UNTYPED, NAME_TIME, NAME_TMP, NAME_VIRTIO_MMIO,
};

/// The cspace slot holding a child's bootstrap channel (rev2§5.1): init installs the
/// child's endpoint at slot 0 of its cspace, and `_start` reads the startup block as
/// that channel's first message. The convention every child shares (`BOOT_CHAN = 0`).
pub const BOOTSTRAP_CHANNEL: u32 = 0;

/// The cspace slot of the cap grant named `name`, if present and a `CapSlot`.
pub fn cap_slot(s: &Startup, name: u8) -> Option<u32> {
    match s.grant(name)? {
        GrantKind::CapSlot(slot) => Some(slot),
        _ => None,
    }
}

/// The handle number of the storage grant named `name`, if present and a
/// `StorageHandle`.
pub fn storage_handle(s: &Startup, name: u8) -> Option<u32> {
    match s.grant(name)? {
        GrantKind::StorageHandle(h) => Some(h),
        _ => None,
    }
}

/// The `(va, len, pa)` of the region grant named `name`, if present and a `Region`.
pub fn region(s: &Startup, name: u8) -> Option<(u64, u64, u64)> {
    match s.grant(name)? {
        GrantKind::Region { va, len, pa } => Some((va, len, pa)),
        _ => None,
    }
}

/// The virtual address of the region grant named `name` (the field a consumer of a
/// pre-mapped page actually needs).
pub fn region_va(s: &Startup, name: u8) -> Option<u64> {
    region(s, name).map(|(va, _, _)| va)
}

/// `stdin` → the cspace slot of the console-channel endpoint the process reads from
/// (rev2§5.1). The console driver owns the PL011 RX line, so an absent grant has no
/// ambient fallback; the caller refuses cleanly.
pub fn stdin_slot(s: &Startup) -> Option<u32> {
    cap_slot(s, NAME_STDIN)
}

/// `stdout` → the cspace slot of the console-channel endpoint the process writes to
/// (rev2§5.1). An interactive console is one channel named under both `stdin` and
/// `stdout`, so this resolves to the same endpoint as [`stdin_slot`].
pub fn stdout_slot(s: &Startup) -> Option<u32> {
    cap_slot(s, NAME_STDOUT)
}

/// `storage` → the cspace slot holding the storage-session channel (rev2§5.1).
pub fn storage_slot(s: &Startup) -> Option<u32> {
    cap_slot(s, NAME_STORAGE)
}

/// `root` → the storage handle number for the process's full-rights ref root
/// (rev2§5.1).
pub fn root_handle(s: &Startup) -> Option<u32> {
    storage_handle(s, NAME_ROOT)
}

/// `time` → the virtual address of the read-only monotonic time page (rev2§2.6).
pub fn time_va(s: &Startup) -> Option<u64> {
    region_va(s, NAME_TIME)
}

/// The four in-process-threading self-cap slots (std-port 3.2), all present iff the
/// process is thread-capable: (self-aspace, self-cspace, thread-untyped,
/// free-slot-range base). `None` if any is missing — the least-authority default,
/// which the PAL maps to an `Unsupported` `thread::spawn`.
pub fn thread_caps(s: &Startup) -> Option<(u32, u32, u32, u32)> {
    Some((
        cap_slot(s, NAME_SELF_ASPACE)?,
        cap_slot(s, NAME_SELF_CSPACE)?,
        cap_slot(s, NAME_THREAD_UNTYPED)?,
        cap_slot(s, NAME_THREAD_SLOT_BASE)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loader::startup::{encode, MAX_BLOCK};

    /// Build a representative block (the shell's grant set) and round-trip it through
    /// `encode`/`decode`, then confirm each typed resolver returns the right value and
    /// rejects a wrong-kind lookup.
    #[test]
    fn resolvers_read_each_named_grant() {
        let mut s = Startup::new();
        s.push_grant(Grant {
            name: NAME_STORAGE,
            kind: GrantKind::CapSlot(7),
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_ROOT,
            kind: GrantKind::StorageHandle(3),
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_STDIN,
            kind: GrantKind::CapSlot(9),
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_STDOUT,
            kind: GrantKind::CapSlot(9),
        })
        .unwrap();
        s.push_grant(Grant {
            name: NAME_TIME,
            kind: GrantKind::Region {
                va: 0xA300_0000,
                len: 4096,
                pa: 0,
            },
        })
        .unwrap();

        let mut buf = [0u8; MAX_BLOCK];
        let n = encode(&s, &mut buf).unwrap();
        let d = decode(&buf[..n]).unwrap();

        assert_eq!(storage_slot(&d), Some(7));
        assert_eq!(root_handle(&d), Some(3));
        assert_eq!(stdin_slot(&d), Some(9));
        assert_eq!(stdout_slot(&d), Some(9));
        assert_eq!(time_va(&d), Some(0xA300_0000));
        assert_eq!(region(&d, NAME_TIME), Some((0xA300_0000, 4096, 0)));

        // A grant that is not present resolves to None.
        assert_eq!(stdin_slot(&Startup::new()), None);
        // A wrong-kind lookup (storage handle asked of a cap-slot grant) is None, not
        // a misread.
        assert_eq!(storage_handle(&d, NAME_STORAGE), None);
        assert_eq!(cap_slot(&d, NAME_ROOT), None);
        assert_eq!(region_va(&d, NAME_STORAGE), None);
    }
}
