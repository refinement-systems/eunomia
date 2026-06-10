//! Userspace virtio-blk driver (spec §2.5, M2).
//!
//! Written exclusively against DmaPool — never touches physical addresses
//! directly. Implements the virtio-blk spec over virtio-mmio for QEMU virt.
//!
//! M2 work items:
//!   - Virtqueue setup (descriptor ring, available ring, used ring)
//!   - Block read/write via DmaPool-backed descriptors
//!   - Interrupt/notification binding for completions (spec §3.6)

#![cfg_attr(not(feature = "std"), no_std)]

pub struct VirtioBlk;

impl VirtioBlk {
    pub fn new() -> Self {
        todo!("M2: probe virtio-mmio, negotiate features, init queues")
    }

    pub fn read_block(&mut self, _lba: u64, _buf: &mut [u8]) {
        todo!("M2: submit read request via virtqueue")
    }

    pub fn write_block(&mut self, _lba: u64, _buf: &[u8]) {
        todo!("M2: submit write request via virtqueue")
    }
}
