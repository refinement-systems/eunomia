//! Block-device abstraction for the storage engine.
//!
//! `flush` is the fsync barrier — the single trusted axiom of the storage
//! stack (§4.8): after `flush` returns, every prior write is durable.
//!
//! `CrashDev` models exactly the volatile/durable split the CommitProtocol
//! TLA+ model checks: writes land in a volatile log; `flush` promotes them;
//! a crash resolves each unflushed write independently to kept / dropped /
//! torn (prefix only), in original order — page-cache semantics.

use std::cell::RefCell;
use std::io;

pub trait BlockDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// fsync barrier: all prior writes are durable when this returns.
    fn flush(&mut self) -> io::Result<()>;
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
}

impl BlockDev for MemDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let data = self.data.borrow();
        let start = offset as usize;
        let end = start + buf.len();
        if end > data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end"));
        }
        buf.copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut d = self.data.borrow_mut();
        let start = offset as usize;
        let end = start + data.len();
        if end > d.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "write past end"));
        }
        d[start..end].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn len(&self) -> u64 {
        self.data.borrow().len() as u64
    }
}

/// Host-file device for mkfs and manual testing.
pub struct FileDev {
    file: std::fs::File,
    len: u64,
}

impl FileDev {
    pub fn create(path: &std::path::Path, len: u64) -> io::Result<FileDev> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(len)?;
        Ok(FileDev { file, len })
    }

    pub fn open(path: &std::path::Path) -> io::Result<FileDev> {
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(FileDev { file, len })
    }
}

impl BlockDev for FileDev {
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(data, offset)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_data()
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

    fn check_fail(&mut self) -> io::Result<()> {
        if let Some(n) = self.fail_after.as_mut() {
            if *n == 0 {
                return Err(io::Error::other("injected power loss"));
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
    fn read(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let cur = self.current.borrow();
        let start = offset as usize;
        let end = start + buf.len();
        if end > cur.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end"));
        }
        buf.copy_from_slice(&cur[start..end]);
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.check_fail()?;
        let mut cur = self.current.borrow_mut();
        let start = offset as usize;
        let end = start + data.len();
        if end > cur.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "write past end"));
        }
        cur[start..end].copy_from_slice(data);
        self.pending.push((offset, data.to_vec()));
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check_fail()?;
        self.durable.copy_from_slice(&self.current.borrow());
        self.pending.clear();
        Ok(())
    }

    fn len(&self) -> u64 {
        self.durable.len() as u64
    }
}
