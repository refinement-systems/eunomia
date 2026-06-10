//! DmaPool crate — the single place in the system where physical addresses
//! are visible (spec §2.5).
//!
//! All DMA drivers are written against this crate and never see a PA.
//! The pool hands out opaque `DeviceBuffer` handles; the backing PA is
//! only accessible via the `phys-read` rights bit on the owning frame cap.
//!
//! M2 work items:
//!   - Allocate/free contiguous DMA buffers from a pre-mapped pool
//!   - `DeviceAddress` type (opaque u64 for virtio queue descriptors)
//!   - Copy helpers: host_to_device / device_to_host memcpy

#![cfg_attr(not(feature = "std"), no_std)]

/// Opaque device-visible address — never dereference on the CPU side.
#[derive(Debug, Clone, Copy)]
pub struct DeviceAddress(u64);

/// Handle to an allocated DMA buffer.
pub struct DeviceBuffer {
    pub device_addr: DeviceAddress,
    pub len: usize,
}

pub struct DmaPool;

impl DmaPool {
    pub fn new() -> Self {
        todo!("M2: initialise from a frame cap with phys-read rights")
    }

    pub fn alloc(&mut self, _len: usize, _align: usize) -> Option<DeviceBuffer> {
        todo!("M2: allocate from pool")
    }

    pub fn free(&mut self, _buf: DeviceBuffer) {
        todo!("M2: return to pool")
    }
}
