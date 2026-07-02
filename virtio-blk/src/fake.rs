// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! A register-accurate fake virtio-mmio block device for host tests.
//!
//! Backed by a Vec "disk" and the same shared memory the driver's
//! DmaPool uses — it really walks the descriptor chains the driver
//! builds, so ring arithmetic, chain flags, and status bytes are all
//! exercised for real.

use crate::{Mmio, SECTOR};
use dma_pool::host::SharedMem;

const MAGIC: u32 = 0x7472_6976;
const F_VERSION_1_SEL1: u32 = 1;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

const REQ_IN: u32 = 0;
const REQ_OUT: u32 = 1;
const REQ_FLUSH: u32 = 4;

const ST_OK: u8 = 0;
const ST_IOERR: u8 = 1;
const ST_UNSUPP: u8 = 2;

pub struct FakeBlock {
    mem: SharedMem,
    device_base: u64,
    pub disk: Vec<u8>,
    pub flush_count: u64,
    status: u32,
    dev_feat_sel: u32,
    drv_feat_sel: u32,
    drv_features: [u32; 2],
    queue_num: u32,
    queue_ready: u32,
    desc_addr: u64,
    avail_addr: u64,
    used_addr: u64,
    isr: u32,
    last_avail: u16,
    used_idx: u16,
    // Deferred completion (host async-poll tests): when set, QUEUE_NOTIFY only
    // stages the queue; `device_step` runs it later, so the driver's poll loop
    // observes a stale used-index first and runs as a real loop.
    deferred: bool,
    pending: bool,
}

impl FakeBlock {
    pub fn new(mem: SharedMem, device_base: u64, sectors: usize) -> FakeBlock {
        FakeBlock {
            mem,
            device_base,
            disk: vec![0u8; sectors * SECTOR],
            flush_count: 0,
            status: 0,
            dev_feat_sel: 0,
            drv_feat_sel: 0,
            drv_features: [0; 2],
            queue_num: 0,
            queue_ready: 0,
            desc_addr: 0,
            avail_addr: 0,
            used_addr: 0,
            isr: 0,
            last_avail: 0,
            used_idx: 0,
            deferred: false,
            pending: false,
        }
    }

    /// Defer completion: while on, `QUEUE_NOTIFY` stages the queue instead of
    /// processing it, and `device_step` runs the staged work. Lets a test
    /// interleave a stale poll between submit and completion.
    pub fn set_deferred(&mut self, on: bool) {
        self.deferred = on;
    }

    /// Run the queue staged by a deferred `QUEUE_NOTIFY`, advancing the used
    /// ring. No-op if nothing is staged.
    pub fn device_step(&mut self) {
        if self.pending {
            self.pending = false;
            self.process_queue();
        }
    }

    fn guest(&self, addr: u64, len: usize) -> &mut [u8] {
        let off = (addr - self.device_base) as usize;
        assert!(off + len <= self.mem.len(), "fake DMA out of range");
        unsafe { core::slice::from_raw_parts_mut(self.mem.raw().add(off), len) }
    }

    fn read_u16(&self, addr: u64) -> u16 {
        u16::from_le_bytes(self.guest(addr, 2).try_into().unwrap())
    }

    fn process_queue(&mut self) {
        let qsize = self.queue_num as u64;
        loop {
            let avail_idx = self.read_u16(self.avail_addr + 2);
            if self.last_avail == avail_idx {
                break;
            }
            let slot = self.avail_addr + 4 + (self.last_avail as u64 % qsize) * 2;
            let head = self.read_u16(slot);

            // Walk the chain.
            let mut chain: Vec<(u64, u32, bool)> = Vec::new();
            let mut di = head;
            loop {
                let d = self.guest(self.desc_addr + di as u64 * 16, 16).to_vec();
                let addr = u64::from_le_bytes(d[0..8].try_into().unwrap());
                let len = u32::from_le_bytes(d[8..12].try_into().unwrap());
                let flags = u16::from_le_bytes(d[12..14].try_into().unwrap());
                let next = u16::from_le_bytes(d[14..16].try_into().unwrap());
                chain.push((addr, len, flags & DESC_F_WRITE != 0));
                if flags & DESC_F_NEXT == 0 {
                    break;
                }
                di = next;
            }

            let st = self.execute(&chain);
            let (st_addr, _, _) = *chain.last().unwrap();
            self.guest(st_addr, 1)[0] = st;

            // Used ring entry: id + bytes written to device-writable descs.
            let written: u32 = chain
                .iter()
                .filter(|(_, _, w)| *w)
                .map(|(_, l, _)| *l)
                .sum();
            let ue = self.used_addr + 4 + (self.used_idx as u64 % qsize) * 8;
            self.guest(ue, 4)
                .copy_from_slice(&(head as u32).to_le_bytes());
            self.guest(ue + 4, 4)
                .copy_from_slice(&written.to_le_bytes());
            self.used_idx = self.used_idx.wrapping_add(1);
            self.guest(self.used_addr + 2, 2)
                .copy_from_slice(&self.used_idx.to_le_bytes());

            self.last_avail = self.last_avail.wrapping_add(1);
            self.isr |= 1;
        }
    }

    fn execute(&mut self, chain: &[(u64, u32, bool)]) -> u8 {
        let (hdr_addr, hdr_len, _) = chain[0];
        if hdr_len != 16 {
            return ST_IOERR;
        }
        let hdr = self.guest(hdr_addr, 16).to_vec();
        let req_type = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let off = sector as usize * SECTOR;
        match req_type {
            REQ_IN => {
                let (daddr, dlen, w) = chain[1];
                if !w || off + dlen as usize > self.disk.len() {
                    return ST_IOERR;
                }
                self.guest(daddr, dlen as usize)
                    .copy_from_slice(&self.disk[off..off + dlen as usize]);
                ST_OK
            }
            REQ_OUT => {
                let (daddr, dlen, w) = chain[1];
                if w || off + dlen as usize > self.disk.len() {
                    return ST_IOERR;
                }
                let data = self.guest(daddr, dlen as usize).to_vec();
                self.disk[off..off + dlen as usize].copy_from_slice(&data);
                ST_OK
            }
            REQ_FLUSH => {
                self.flush_count += 1;
                ST_OK
            }
            _ => ST_UNSUPP,
        }
    }
}

impl Mmio for FakeBlock {
    fn read32(&self, offset: usize) -> u32 {
        match offset {
            0x000 => MAGIC,
            0x004 => 2,
            0x008 => 2, // block device
            0x00C => 0x1AF4,
            0x010 => {
                if self.dev_feat_sel == 1 {
                    F_VERSION_1_SEL1
                } else {
                    0
                }
            }
            0x034 => 8, // QueueNumMax
            0x044 => self.queue_ready,
            0x060 => self.isr,
            0x070 => self.status,
            0x100 => (self.disk.len() / SECTOR) as u32,
            0x104 => ((self.disk.len() / SECTOR) as u64 >> 32) as u32,
            _ => 0,
        }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        match offset {
            0x014 => self.dev_feat_sel = value,
            0x020 => self.drv_features[(self.drv_feat_sel & 1) as usize] = value,
            0x024 => self.drv_feat_sel = value,
            0x030 => assert_eq!(value, 0, "fake has one queue"),
            0x038 => self.queue_num = value,
            0x044 => self.queue_ready = value,
            0x050 => {
                if self.deferred {
                    self.pending = true;
                } else {
                    self.process_queue();
                }
            }
            0x064 => self.isr &= !value,
            0x070 => {
                // Writing FEATURES_OK is accepted only with VERSION_1 set.
                if value & crate::status::FEATURES_OK != 0
                    && self.drv_features[1] & F_VERSION_1_SEL1 == 0
                {
                    self.status = value & !crate::status::FEATURES_OK;
                } else {
                    self.status = value;
                }
            }
            0x080 => self.desc_addr = (self.desc_addr & !0xFFFF_FFFF) | value as u64,
            0x084 => self.desc_addr = (self.desc_addr & 0xFFFF_FFFF) | ((value as u64) << 32),
            0x090 => self.avail_addr = (self.avail_addr & !0xFFFF_FFFF) | value as u64,
            0x094 => self.avail_addr = (self.avail_addr & 0xFFFF_FFFF) | ((value as u64) << 32),
            0x0A0 => self.used_addr = (self.used_addr & !0xFFFF_FFFF) | value as u64,
            0x0A4 => self.used_addr = (self.used_addr & 0xFFFF_FFFF) | ((value as u64) << 32),
            _ => {}
        }
    }
}
