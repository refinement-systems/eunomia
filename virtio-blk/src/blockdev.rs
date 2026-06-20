//! Adapter: the byte-addressed `cas::dev::BlockDev` over sector-addressed
//! virtio-blk. Partial-sector writes are read-modify-write at sector
//! granularity; `flush` maps to VIRTIO_BLK_T_FLUSH — the fsync axiom
//! (rev1§4.8) rides on QEMU honoring FLUSH under cache=writeback.

use crate::{Mmio, VirtioBlk, VirtioError, SECTOR};
use alloc::vec;
use cas::dev::{BlockDev, DevError, DevResult};
use core::cell::RefCell;
use dma_pool::DmaBacking;

pub struct VirtioBlockDev<M: Mmio, B: DmaBacking> {
    inner: RefCell<VirtioBlk<M, B>>,
    len: u64,
}

fn io_err(_e: VirtioError) -> DevError {
    DevError::Io("virtio-blk request failed")
}

impl<M: Mmio, B: DmaBacking> VirtioBlockDev<M, B> {
    pub fn new(blk: VirtioBlk<M, B>) -> VirtioBlockDev<M, B> {
        let len = blk.capacity_sectors() * SECTOR as u64;
        VirtioBlockDev { inner: RefCell::new(blk), len }
    }
}

impl<M: Mmio, B: DmaBacking> BlockDev for VirtioBlockDev<M, B> {
    fn read(&self, offset: u64, buf: &mut [u8]) -> DevResult<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut blk = self.inner.borrow_mut();
        let max = blk.max_transfer();
        let mut tmp = vec![0u8; max];
        let mut pos = offset;
        let mut out = 0usize;
        while out < buf.len() {
            let lba = pos / SECTOR as u64;
            let in_sector = (pos % SECTOR as u64) as usize;
            let span = (max - in_sector).min(buf.len() - out + in_sector);
            let sectors = span.div_ceil(SECTOR);
            blk.read_sectors(lba, &mut tmp[..sectors * SECTOR]).map_err(io_err)?;
            let take = (sectors * SECTOR - in_sector).min(buf.len() - out);
            buf[out..out + take].copy_from_slice(&tmp[in_sector..in_sector + take]);
            out += take;
            pos += take as u64;
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> DevResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let mut blk = self.inner.borrow_mut();
        let max = blk.max_transfer();
        let mut tmp = vec![0u8; max];
        let mut pos = offset;
        let mut consumed = 0usize;
        while consumed < data.len() {
            let lba = pos / SECTOR as u64;
            let in_sector = (pos % SECTOR as u64) as usize;
            let span = (max - in_sector).min(data.len() - consumed + in_sector);
            let sectors = span.div_ceil(SECTOR);
            let take = (sectors * SECTOR - in_sector).min(data.len() - consumed);
            let tail = in_sector + take;
            // RMW only when the edges are partial sectors.
            if in_sector != 0 || tail % SECTOR != 0 {
                blk.read_sectors(lba, &mut tmp[..sectors * SECTOR]).map_err(io_err)?;
            }
            tmp[in_sector..tail].copy_from_slice(&data[consumed..consumed + take]);
            blk.write_sectors(lba, &tmp[..sectors * SECTOR]).map_err(io_err)?;
            consumed += take;
            pos += take as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> DevResult<()> {
        self.inner.borrow_mut().flush().map_err(io_err)
    }

    fn len(&self) -> u64 {
        self.len
    }
}
