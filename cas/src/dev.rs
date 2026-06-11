//! Block-device abstraction for the storage engine.
//!
//! `flush` is the fsync barrier — the single trusted axiom of the storage
//! stack (§4.8): after `flush` returns, every prior write is durable.
//!
//! `CrashDev` models exactly the volatile/durable split the CommitProtocol
//! TLA+ model checks: writes land in a volatile log; `flush` promotes them;
//! a crash resolves each unflushed write independently to kept / dropped /
//! torn (prefix only), in original order — page-cache semantics.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Device error — the no_std-friendly analogue of std::io::Error. The
/// `std` feature adds conversions for host backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevError {
    OutOfRange,
    /// Injected or real I/O failure (power loss, transport error).
    Io(&'static str),
}

impl core::fmt::Display for DevError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DevError::OutOfRange => write!(f, "access past end of device"),
            DevError::Io(w) => write!(f, "device i/o: {w}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DevError {}

pub type DevResult<T> = Result<T, DevError>;

pub trait BlockDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> DevResult<()>;
    fn write(&mut self, offset: u64, data: &[u8]) -> DevResult<()>;
    /// fsync barrier: all prior writes are durable when this returns.
    fn flush(&mut self) -> DevResult<()>;
    fn len(&self) -> u64;
}

/// Plain in-memory device (no crash modeling) for fast tests.
pub struct MemDev {
    data: RefCell<Vec<u8>>,
}

impl MemDev {
    pub fn new(len: usize) -> MemDev {
        MemDev { data: RefCell::new(vec![0; len]) }
    }

    /// Wrap an existing byte buffer as a device (the device length is the
    /// buffer length). Used to present arbitrary fuzz input as a whole
    /// image to `Store::mount`.
    pub fn from_bytes(data: Vec<u8>) -> MemDev {
        MemDev { data: RefCell::new(data) }
    }
}

impl BlockDev for MemDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> DevResult<()> {
        let data = self.data.borrow();
        let start = offset as usize;
        let end = start + buf.len();
        if end > data.len() {
            return Err(DevError::OutOfRange);
        }
        buf.copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> DevResult<()> {
        let mut d = self.data.borrow_mut();
        let start = offset as usize;
        let end = start + data.len();
        if end > d.len() {
            return Err(DevError::OutOfRange);
        }
        d[start..end].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> DevResult<()> {
        Ok(())
    }

    fn len(&self) -> u64 {
        self.data.borrow().len() as u64
    }
}

/// Host-file device for mkfs and manual testing.
#[cfg(feature = "std")]
pub struct FileDev {
    file: std::fs::File,
    len: u64,
}

#[cfg(feature = "std")]
impl FileDev {
    pub fn create(path: &std::path::Path, len: u64) -> std::io::Result<FileDev> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len)?;
        Ok(FileDev { file, len })
    }

    pub fn open(path: &std::path::Path) -> std::io::Result<FileDev> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(FileDev { file, len })
    }
}

#[cfg(feature = "std")]
impl BlockDev for FileDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> DevResult<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset).map_err(|_| DevError::Io("file read"))
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> DevResult<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(data, offset).map_err(|_| DevError::Io("file write"))
    }

    fn flush(&mut self) -> DevResult<()> {
        self.file.sync_data().map_err(|_| DevError::Io("fsync"))
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// Crash-injection device. Reads see volatile state (the running system's
/// view); `crash` rewinds to durable state plus a per-write random subset
/// of unflushed writes, possibly torn.
pub struct CrashDev {
    durable: Vec<u8>,
    current: RefCell<Vec<u8>>,
    /// Unflushed writes in application order.
    pending: Vec<(u64, Vec<u8>)>,
    /// Power-loss injection: fail every write/flush once the countdown
    /// reaches zero (the moment the cord is pulled).
    fail_after: Option<u64>,
}

impl CrashDev {
    pub fn new(len: usize) -> CrashDev {
        CrashDev {
            durable: vec![0; len],
            current: RefCell::new(vec![0; len]),
            pending: Vec::new(),
            fail_after: None,
        }
    }

    /// Fail the n-th subsequent write/flush and every one after it.
    pub fn set_fail_after(&mut self, n: u64) {
        self.fail_after = Some(n);
    }

    pub fn clear_fail(&mut self) {
        self.fail_after = None;
    }

    fn check_fail(&mut self) -> DevResult<()> {
        if let Some(n) = self.fail_after.as_mut() {
            if *n == 0 {
                return Err(DevError::Io("injected power loss"));
            }
            *n -= 1;
        }
        Ok(())
    }

    /// Crash and "reboot": volatile state is replaced by durable state plus
    /// each pending write independently kept / dropped / torn, decided by
    /// `seed`. Returns the device in its post-reboot state.
    pub fn crash(&mut self, seed: u64) {
        let mut s = seed;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s >> 33
        };
        let mut disk = self.durable.clone();
        for (off, data) in self.pending.drain(..) {
            match next() % 3 {
                0 => {} // dropped
                1 => {
                    // fully persisted
                    let start = off as usize;
                    disk[start..start + data.len()].copy_from_slice(&data);
                }
                _ => {
                    // torn: an arbitrary prefix made it
                    let keep = (next() as usize) % (data.len() + 1);
                    let start = off as usize;
                    disk[start..start + keep].copy_from_slice(&data[..keep]);
                }
            }
        }
        self.durable = disk.clone();
        *self.current.borrow_mut() = disk;
    }

    pub fn pending_writes(&self) -> usize {
        self.pending.len()
    }
}

impl BlockDev for CrashDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> DevResult<()> {
        let cur = self.current.borrow();
        let start = offset as usize;
        let end = start + buf.len();
        if end > cur.len() {
            return Err(DevError::OutOfRange);
        }
        buf.copy_from_slice(&cur[start..end]);
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> DevResult<()> {
        self.check_fail()?;
        let mut cur = self.current.borrow_mut();
        let start = offset as usize;
        let end = start + data.len();
        if end > cur.len() {
            return Err(DevError::OutOfRange);
        }
        cur[start..end].copy_from_slice(data);
        self.pending.push((offset, data.to_vec()));
        Ok(())
    }

    fn flush(&mut self) -> DevResult<()> {
        self.check_fail()?;
        self.durable.copy_from_slice(&self.current.borrow());
        self.pending.clear();
        Ok(())
    }

    fn len(&self) -> u64 {
        self.durable.len() as u64
    }
}
