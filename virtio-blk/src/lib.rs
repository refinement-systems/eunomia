//! Userspace virtio-blk driver (rev2§2.5): virtio-mmio (modern,
//! version 2) split virtqueue, written exclusively against DmaPool —
//! the driver never sees a physical address, only opaque
//! `DeviceAddress`es.
//!
//! Transport is abstract (`Mmio`), so the identical driver logic runs
//! against the QEMU virt MMIO window on the OS and against the fake
//! device in `fake.rs` on the host (where the whole storage stack is
//! integration-tested over it).
//!
//! MVP shape: one queue, one synchronous in-flight request, completion
//! by polling the used ring. On the OS the device interrupt binds to a
//! notification (rev2§3.6) and the poll loop becomes a wait; the driver
//! exposes `complete()` so the caller owns the waiting strategy.
//! QEMU note: modern MMIO needs `-global virtio-mmio.force-legacy=false`.

#![cfg_attr(not(any(feature = "std", test)), no_std)]
// Clippy is not a CI gate: these are device-driver cosmetics — a
// block-device size trait where `is_empty` is meaningless, MMIO `unsafe` methods
// documented with prose contracts rather than a `# Safety` heading, an explicit
// alignment check, and a cohesive descriptor type. Suppressed, not applied.
#![allow(
    clippy::assign_op_pattern,
    clippy::len_without_is_empty,
    clippy::manual_is_multiple_of,
    clippy::missing_safety_doc,
    clippy::type_complexity
)]

use dma_pool::{DmaBacking, DmaBuf, DmaPool};

pub const SECTOR: usize = 512;

/// Volatile 32-bit register access. Implementors: real MMIO on the OS,
/// the fake device on the host.
pub trait Mmio {
    fn read32(&self, offset: usize) -> u32;
    fn write32(&mut self, offset: usize, value: u32);
}

// Virtio-mmio register offsets (virtio spec 4.2.2, version 2 layout).
mod reg {
    pub const MAGIC: usize = 0x000;
    pub const VERSION: usize = 0x004;
    pub const DEVICE_ID: usize = 0x008;
    pub const DEVICE_FEATURES: usize = 0x010;
    pub const DEVICE_FEATURES_SEL: usize = 0x014;
    pub const DRIVER_FEATURES: usize = 0x020;
    pub const DRIVER_FEATURES_SEL: usize = 0x024;
    pub const QUEUE_SEL: usize = 0x030;
    pub const QUEUE_NUM_MAX: usize = 0x034;
    pub const QUEUE_NUM: usize = 0x038;
    pub const QUEUE_READY: usize = 0x044;
    pub const QUEUE_NOTIFY: usize = 0x050;
    pub const INTERRUPT_STATUS: usize = 0x060;
    pub const INTERRUPT_ACK: usize = 0x064;
    pub const STATUS: usize = 0x070;
    pub const QUEUE_DESC_LOW: usize = 0x080;
    pub const QUEUE_DESC_HIGH: usize = 0x084;
    pub const QUEUE_DRIVER_LOW: usize = 0x090;
    pub const QUEUE_DRIVER_HIGH: usize = 0x094;
    pub const QUEUE_DEVICE_LOW: usize = 0x0A0;
    pub const QUEUE_DEVICE_HIGH: usize = 0x0A4;
    pub const CONFIG: usize = 0x100; // blk: capacity (sectors) u64 LE
}

pub mod status {
    pub const ACKNOWLEDGE: u32 = 1;
    pub const DRIVER: u32 = 2;
    pub const DRIVER_OK: u32 = 4;
    pub const FEATURES_OK: u32 = 8;
    pub const FAILED: u32 = 128;
}

const MAGIC_VIRT: u32 = 0x7472_6976;
const DEVICE_ID_BLOCK: u32 = 2;
/// VIRTIO_F_VERSION_1 (bit 32) — the only feature we negotiate.
const F_VERSION_1_SEL1: u32 = 1;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

// Request types (virtio-blk). `pub` so the split-phase `submit` is callable
// from the host tests and the future OS IRQ path (rev2§3.6).
pub const REQ_IN: u32 = 0; // device → driver (read)
pub const REQ_OUT: u32 = 1; // driver → device (write)
pub const REQ_FLUSH: u32 = 4;

const STATUS_OK: u8 = 0;

/// Byte-offset of `avail.ring[idx % size]` within the avail buffer
/// (`flags: u16`, `idx: u16`, then the `size`-entry `u16` ring). Pure so the
/// ring/`u16`-wrap arithmetic is directly proptest-addressable.
pub fn avail_ring_slot(idx: u16, qsize: u16) -> usize {
    4 + (idx % qsize) as usize * 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioError {
    BadMagic,
    BadVersion,
    NotABlockDevice,
    FeatureNegotiation,
    NoQueue,
    NoMemory,
    TooLarge,
    /// A transfer whose last sector would run past the device's reported
    /// capacity. Distinct from `TooLarge` (exceeds `max_transfer`). Defensive
    /// local bound (rev2§4.5): the device stays ground truth for its own
    /// geometry; this turns a device-dependent `DeviceError` into a
    /// deterministic local refusal and is never a correctness dependency.
    OutOfRange,
    DeviceError,
    Unsupported,
}

pub struct VirtioBlk<M: Mmio, B: DmaBacking> {
    mmio: M,
    pool: DmaPool<B>,
    queue_size: u16,
    desc: DmaBuf,
    avail: DmaBuf,
    used: DmaBuf,
    hdr: DmaBuf,
    data: DmaBuf,
    status: DmaBuf,
    avail_idx: u16,
    last_used: u16,
    capacity: u64,
    max_transfer: usize,
}

impl<M: Mmio, B: DmaBacking> VirtioBlk<M, B> {
    /// Probe, negotiate VERSION_1, build queue 0 from pool memory,
    /// DRIVER_OK. `max_transfer` bounds a single request's data size.
    pub fn new(
        mut mmio: M,
        mut pool: DmaPool<B>,
        max_transfer: usize,
    ) -> Result<Self, VirtioError> {
        if mmio.read32(reg::MAGIC) != MAGIC_VIRT {
            return Err(VirtioError::BadMagic);
        }
        if mmio.read32(reg::VERSION) != 2 {
            return Err(VirtioError::BadVersion);
        }
        if mmio.read32(reg::DEVICE_ID) != DEVICE_ID_BLOCK {
            return Err(VirtioError::NotABlockDevice);
        }

        mmio.write32(reg::STATUS, 0); // reset
        mmio.write32(reg::STATUS, status::ACKNOWLEDGE);
        mmio.write32(reg::STATUS, status::ACKNOWLEDGE | status::DRIVER);

        mmio.write32(reg::DEVICE_FEATURES_SEL, 1);
        if mmio.read32(reg::DEVICE_FEATURES) & F_VERSION_1_SEL1 == 0 {
            mmio.write32(reg::STATUS, status::FAILED);
            return Err(VirtioError::FeatureNegotiation);
        }
        mmio.write32(reg::DRIVER_FEATURES_SEL, 0);
        mmio.write32(reg::DRIVER_FEATURES, 0);
        mmio.write32(reg::DRIVER_FEATURES_SEL, 1);
        mmio.write32(reg::DRIVER_FEATURES, F_VERSION_1_SEL1);

        let st = status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK;
        mmio.write32(reg::STATUS, st);
        if mmio.read32(reg::STATUS) & status::FEATURES_OK == 0 {
            mmio.write32(reg::STATUS, status::FAILED);
            return Err(VirtioError::FeatureNegotiation);
        }

        mmio.write32(reg::QUEUE_SEL, 0);
        let max = mmio.read32(reg::QUEUE_NUM_MAX);
        if max == 0 {
            return Err(VirtioError::NoQueue);
        }
        let queue_size = (max as u16).min(8);
        mmio.write32(reg::QUEUE_NUM, queue_size as u32);

        let n = queue_size as usize;
        let desc = pool.alloc(16 * n, 16).ok_or(VirtioError::NoMemory)?;
        let avail = pool.alloc(6 + 2 * n, 2).ok_or(VirtioError::NoMemory)?;
        let used = pool.alloc(6 + 8 * n, 4).ok_or(VirtioError::NoMemory)?;
        let hdr = pool.alloc(16, 16).ok_or(VirtioError::NoMemory)?;
        let data = pool
            .alloc(max_transfer, SECTOR)
            .ok_or(VirtioError::NoMemory)?;
        let stat = pool.alloc(1, 1).ok_or(VirtioError::NoMemory)?;
        for buf in [&desc, &avail, &used] {
            let len = buf.len();
            pool.bytes_mut(buf)[..len].fill(0);
        }

        let mut w64 = |low: usize, high: usize, addr: u64| {
            mmio.write32(low, addr as u32);
            mmio.write32(high, (addr >> 32) as u32);
        };
        w64(
            reg::QUEUE_DESC_LOW,
            reg::QUEUE_DESC_HIGH,
            desc.device_addr().0,
        );
        w64(
            reg::QUEUE_DRIVER_LOW,
            reg::QUEUE_DRIVER_HIGH,
            avail.device_addr().0,
        );
        w64(
            reg::QUEUE_DEVICE_LOW,
            reg::QUEUE_DEVICE_HIGH,
            used.device_addr().0,
        );
        mmio.write32(reg::QUEUE_READY, 1);
        mmio.write32(reg::STATUS, st | status::DRIVER_OK);

        let capacity =
            (mmio.read32(reg::CONFIG) as u64) | ((mmio.read32(reg::CONFIG + 4) as u64) << 32);

        Ok(VirtioBlk {
            mmio,
            pool,
            queue_size,
            desc,
            avail,
            used,
            hdr,
            data,
            status: stat,
            avail_idx: 0,
            last_used: 0,
            capacity,
            max_transfer,
        })
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity
    }

    pub fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    fn write_desc(&mut self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&addr.to_le_bytes());
        d[8..12].copy_from_slice(&len.to_le_bytes());
        d[12..14].copy_from_slice(&flags.to_le_bytes());
        d[14..16].copy_from_slice(&next.to_le_bytes());
        let buf = self.desc;
        self.pool.write(&buf, i as usize * 16, &d);
    }

    /// One synchronous request: submit the chain and block on completion.
    fn request(
        &mut self,
        req_type: u32,
        sector: u64,
        data_len: usize,
        device_writes: bool,
    ) -> Result<(), VirtioError> {
        self.submit(req_type, sector, data_len, device_writes);
        self.complete()
    }

    /// Publish one request and ring the doorbell, without waiting: build the
    /// header / data / status descriptor chain, push the head onto the avail
    /// ring, `QUEUE_NOTIFY`. Pairs with `try_complete`/`complete`. This is the
    /// rev2§3.6 "submit, then poll once" primitive the OS IRQ path reuses (it
    /// waits on the device notification between `submit` and `try_complete`).
    pub fn submit(&mut self, req_type: u32, sector: u64, data_len: usize, device_writes: bool) {
        let mut hdr = [0u8; 16];
        hdr[0..4].copy_from_slice(&req_type.to_le_bytes());
        hdr[8..16].copy_from_slice(&sector.to_le_bytes());
        let hbuf = self.hdr;
        self.pool.write(&hbuf, 0, &hdr);
        let sbuf = self.status;
        self.pool.write(&sbuf, 0, &[0xFF]);

        let data_flags = if device_writes { DESC_F_WRITE } else { 0 };
        if data_len > 0 {
            self.write_desc(0, self.hdr.device_addr().0, 16, DESC_F_NEXT, 1);
            self.write_desc(
                1,
                self.data.device_addr().0,
                data_len as u32,
                data_flags | DESC_F_NEXT,
                2,
            );
            self.write_desc(2, self.status.device_addr().0, 1, DESC_F_WRITE, 0);
        } else {
            self.write_desc(0, self.hdr.device_addr().0, 16, DESC_F_NEXT, 1);
            self.write_desc(1, self.status.device_addr().0, 1, DESC_F_WRITE, 0);
        }

        // avail.ring[idx % size] = head; then publish idx+1.
        let slot = avail_ring_slot(self.avail_idx, self.queue_size);
        let abuf = self.avail;
        self.pool.write(&abuf, slot, &0u16.to_le_bytes());
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.pool.write(&abuf, 2, &self.avail_idx.to_le_bytes());

        self.mmio.write32(reg::QUEUE_NOTIFY, 0);
    }

    /// Poll the used ring once for the in-flight request's completion.
    ///
    /// The used-index is device-written, so it is read **volatile**: a plain
    /// `pool.read` is a non-volatile load the optimizer may hoist out of the
    /// spin loop, which could never then observe the device's update. On
    /// observing an advance, issue an `Acquire` fence so the device's
    /// pre-index writes (status byte, payload) are not reordered after the
    /// index observation — `finish()` reads them only after this returns true.
    fn poll_used(&mut self) -> bool {
        let mut idx = [0u8; 2];
        self.pool.read_volatile(&self.used, 2, &mut idx);
        if u16::from_le_bytes(idx) != self.last_used {
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
            true
        } else {
            false
        }
    }

    /// Post-completion work: consume the used entry, ack the ISR, read the
    /// device's status byte. Reached only after `poll_used` observed the
    /// advance under the `Acquire` fence.
    fn finish(&mut self) -> Result<(), VirtioError> {
        self.last_used = self.last_used.wrapping_add(1);
        let isr = self.mmio.read32(reg::INTERRUPT_STATUS);
        if isr != 0 {
            self.mmio.write32(reg::INTERRUPT_ACK, isr);
        }
        let mut st = [0u8; 1];
        self.pool.read(&self.status, 0, &mut st);
        if st[0] == STATUS_OK {
            Ok(())
        } else {
            Err(VirtioError::DeviceError)
        }
    }

    /// Poll the in-flight request once: `Some(status)` if the device has
    /// completed it, `None` if still pending. The non-blocking half of
    /// `complete` — the OS IRQ path calls this once per notification instead
    /// of busy-spinning (rev2§3.6).
    pub fn try_complete(&mut self) -> Option<Result<(), VirtioError>> {
        if self.poll_used() {
            Some(self.finish())
        } else {
            None
        }
    }

    /// Wait for the in-flight request. Polling MVP; the OS binds the
    /// device IRQ to a notification and waits between polls instead
    /// (rev2§3.6 "poll once, then wait").
    fn complete(&mut self) -> Result<(), VirtioError> {
        while !self.poll_used() {
            core::hint::spin_loop();
        }
        self.finish()
    }

    /// Host-test affordance: reach the transport after it has been moved into
    /// the driver (the fake device uses this to switch on deferred completion
    /// and step the queue between polls).
    #[cfg(any(feature = "std", test))]
    pub fn mmio_mut(&mut self) -> &mut M {
        &mut self.mmio
    }

    /// Host-test affordance: copy the read bounce buffer out after a read
    /// request completes — the split-phase counterpart to `read_sectors`'
    /// post-completion copy (the OS path extracts read data the same way).
    #[cfg(any(feature = "std", test))]
    pub fn read_data(&self, out: &mut [u8]) {
        self.pool.read(&self.data, 0, out);
    }

    /// Host-test affordance: stage write data into the bounce buffer before a
    /// `submit(REQ_OUT, ..)` — the split-phase counterpart to `write_sectors`'
    /// pre-submit copy.
    #[cfg(any(feature = "std", test))]
    pub fn write_data(&mut self, data: &[u8]) {
        let dbuf = self.data;
        self.pool.write(&dbuf, 0, data);
    }

    /// Defensive LBA bound (rev2§4.5). The device remains ground truth
    /// for its own geometry; this refuses a transfer whose last sector runs
    /// past the reported `capacity` *before* any device round-trip — a local
    /// hardening, never a correctness dependency. Checked so an adversarial
    /// `lba` near `u64::MAX` refuses rather than wraps (same discipline as
    /// `cas::dev::access_range`).
    fn check_capacity(&self, lba: u64, len: usize) -> Result<(), VirtioError> {
        let nsectors = (len / SECTOR) as u64;
        if lba
            .checked_add(nsectors)
            .is_none_or(|end| end > self.capacity)
        {
            return Err(VirtioError::OutOfRange);
        }
        Ok(())
    }

    pub fn read_sectors(&mut self, lba: u64, out: &mut [u8]) -> Result<(), VirtioError> {
        debug_assert_eq!(out.len() % SECTOR, 0);
        if out.len() > self.max_transfer {
            return Err(VirtioError::TooLarge);
        }
        self.check_capacity(lba, out.len())?;
        self.request(REQ_IN, lba, out.len(), true)?;
        let data = self.data;
        self.pool.read(&data, 0, out);
        Ok(())
    }

    pub fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), VirtioError> {
        debug_assert_eq!(data.len() % SECTOR, 0);
        if data.len() > self.max_transfer {
            return Err(VirtioError::TooLarge);
        }
        self.check_capacity(lba, data.len())?;
        let dbuf = self.data;
        self.pool.write(&dbuf, 0, data);
        self.request(REQ_OUT, lba, data.len(), false)
    }

    /// VIRTIO_BLK_T_FLUSH — the fsync barrier the storage stack trusts
    /// (rev2§4.8: stated axiom; QEMU honors FLUSH with cache=writeback).
    pub fn flush(&mut self) -> Result<(), VirtioError> {
        self.request(REQ_FLUSH, 0, 0, false)
    }
}

extern crate alloc;

#[cfg(any(feature = "std", test))]
pub mod fake;

pub mod blockdev;
