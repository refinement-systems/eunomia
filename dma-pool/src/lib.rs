//! DmaPool — the single place in the system where physical addresses are
//! visible (spec §2.5).
//!
//! Drivers are written against this crate and never see a PA: buffers are
//! labeled with opaque `DeviceAddress`es (what the device dereferences)
//! and accessed by the CPU only through pool-mediated slices. The backing
//! is abstract: on the OS it is a frame mapping whose PA is read through
//! the `phys-read` rights bit (init grants that bit only to the pool's
//! holder); on the host it is plain memory with a fake device base.
//!
//! When the IO-space object lands (§2.5 committed upgrade), the backing
//! swaps to IOVA-labeled mappings and no driver changes.
//!
//! MVP allocator: first-fit free list with merge-on-free. The pool is
//! bounded and persistent for the driver's lifetime — the steady state
//! needs zero mapping operations per request.

#![cfg_attr(not(any(feature = "std", test)), no_std)]

/// Kani harnesses (plan §4.7), compiled only under `cargo kani`.
#[cfg(kani)]
mod proofs;

/// Opaque device-visible address — never dereference on the CPU side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAddress(pub u64);

/// The memory behind a pool. Implementors are the privileged boundary:
/// only they ever know real physical addresses.
///
/// Safety contract: `cpu_base` points at `len` writable bytes that stay
/// valid and pinned for the backing's lifetime, and `device_base` is the
/// address under which the device sees byte 0 of that region.
pub unsafe trait DmaBacking {
    fn cpu_base(&self) -> *mut u8;
    fn device_base(&self) -> DeviceAddress;
    fn len(&self) -> usize;
}

/// A buffer carved out of the pool. Offsets are pool-relative; the
/// device address is precomputed so drivers never do PA arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBuf {
    offset: usize,
    len: usize,
    device_addr: DeviceAddress,
}

impl DmaBuf {
    pub fn device_addr(&self) -> DeviceAddress {
        self.device_addr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

const MAX_FREE_RANGES: usize = 64;

pub struct DmaPool<B: DmaBacking> {
    backing: B,
    /// (offset, len), sorted by offset, non-adjacent (merged eagerly).
    free: [(usize, usize); MAX_FREE_RANGES],
    nfree: usize,
}

impl<B: DmaBacking> DmaPool<B> {
    pub fn new(backing: B) -> DmaPool<B> {
        let len = backing.len();
        let mut free = [(0, 0); MAX_FREE_RANGES];
        free[0] = (0, len);
        DmaPool { backing, free, nfree: 1 }
    }

    pub fn alloc(&mut self, len: usize, align: usize) -> Option<DmaBuf> {
        debug_assert!(align.is_power_of_two());
        if len == 0 {
            return None;
        }
        for i in 0..self.nfree {
            let (off, flen) = self.free[i];
            let start = (off + align - 1) & !(align - 1);
            let pad = start - off;
            if pad + len > flen {
                continue;
            }
            // Carve [start, start+len); the padding stays free, the
            // remainder becomes a (possibly empty) new range.
            let rest_off = start + len;
            let rest_len = flen - pad - len;
            match (pad > 0, rest_len > 0) {
                (false, false) => {
                    self.free.copy_within(i + 1..self.nfree, i);
                    self.nfree -= 1;
                }
                (true, false) => self.free[i] = (off, pad),
                (false, true) => self.free[i] = (rest_off, rest_len),
                (true, true) => {
                    if self.nfree == MAX_FREE_RANGES {
                        continue; // fragmentation cap; try another range
                    }
                    self.free[i] = (off, pad);
                    self.free.copy_within(i + 1..self.nfree, i + 2);
                    self.free[i + 1] = (rest_off, rest_len);
                    self.nfree += 1;
                }
            }
            return Some(DmaBuf {
                offset: start,
                len,
                device_addr: DeviceAddress(self.backing.device_base().0 + start as u64),
            });
        }
        None
    }

    pub fn free(&mut self, buf: DmaBuf) {
        let (mut off, mut len) = (buf.offset, buf.len);
        // Find insertion point, merge with neighbors.
        let mut i = 0;
        while i < self.nfree && self.free[i].0 < off {
            i += 1;
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == off {
            off = self.free[i - 1].0;
            len += self.free[i - 1].1;
            i -= 1;
            self.free.copy_within(i + 1..self.nfree, i);
            self.nfree -= 1;
        }
        if i < self.nfree && off + len == self.free[i].0 {
            len += self.free[i].1;
            self.free.copy_within(i + 1..self.nfree, i);
            self.nfree -= 1;
        }
        assert!(self.nfree < MAX_FREE_RANGES, "free list overflow");
        self.free.copy_within(i..self.nfree, i + 1);
        self.free[i] = (off, len);
        self.nfree += 1;
    }

    /// CPU view of a buffer (or a sub-range of it). The pool mediates all
    /// CPU access; drivers never hold raw pointers into DMA memory.
    pub fn bytes(&self, buf: &DmaBuf) -> &[u8] {
        // Volatile-correctness note: QEMU DMA is host memcpy and
        // cache-coherent (§2.5 real-hardware debt: cache maintenance owed
        // with real hardware, alongside barriers tighter than these).
        unsafe { core::slice::from_raw_parts(self.backing.cpu_base().add(buf.offset), buf.len) }
    }

    pub fn bytes_mut(&mut self, buf: &DmaBuf) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.backing.cpu_base().add(buf.offset), buf.len)
        }
    }

    pub fn write(&mut self, buf: &DmaBuf, offset: usize, data: &[u8]) {
        self.bytes_mut(buf)[offset..offset + data.len()].copy_from_slice(data);
    }

    pub fn read(&self, buf: &DmaBuf, offset: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.bytes(buf)[offset..offset + out.len()]);
    }
}

/// Host-side backing for tests and host tooling: plain heap memory with
/// a synthetic device base. The "device" (a fake) is handed the same
/// memory out of band.
#[cfg(any(feature = "std", test))]
pub mod host {
    use super::*;
    use std::rc::Rc;
    use std::cell::UnsafeCell;

    #[derive(Clone)]
    pub struct SharedMem(Rc<UnsafeCell<Vec<u8>>>);

    impl SharedMem {
        pub fn new(len: usize) -> SharedMem {
            SharedMem(Rc::new(UnsafeCell::new(vec![0u8; len])))
        }

        /// The fake device's side of the DMA region.
        ///
        /// Safety: test harnesses are single-threaded and never hold
        /// overlapping slices across driver calls — the same discipline
        /// real DMA imposes (device and CPU coordinate via the rings).
        pub unsafe fn raw(&self) -> *mut u8 {
            (*self.0.get()).as_mut_ptr()
        }

        pub fn len(&self) -> usize {
            unsafe { (*self.0.get()).len() }
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }

    pub struct HostBacking {
        pub mem: SharedMem,
        pub device_base: u64,
    }

    unsafe impl DmaBacking for HostBacking {
        fn cpu_base(&self) -> *mut u8 {
            unsafe { self.mem.raw() }
        }

        fn device_base(&self) -> DeviceAddress {
            DeviceAddress(self.device_base)
        }

        fn len(&self) -> usize {
            self.mem.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::host::*;
    use super::*;

    fn pool(len: usize) -> DmaPool<HostBacking> {
        DmaPool::new(HostBacking { mem: SharedMem::new(len), device_base: 0x4000_0000 })
    }

    #[test]
    fn alloc_respects_alignment_and_device_base() {
        let mut p = pool(4096);
        let a = p.alloc(10, 1).unwrap();
        let b = p.alloc(64, 256).unwrap();
        assert_eq!(a.device_addr().0, 0x4000_0000);
        assert_eq!(b.device_addr().0 % 256, 0);
        assert!(b.device_addr().0 >= 0x4000_0000 + 10);
    }

    #[test]
    fn exhaustion_and_free_merge() {
        let mut p = pool(1024);
        let a = p.alloc(512, 16).unwrap();
        let b = p.alloc(512, 16).unwrap();
        assert!(p.alloc(1, 1).is_none());
        p.free(a);
        p.free(b);
        // Merged back into one range covering everything.
        let c = p.alloc(1024, 16).unwrap();
        assert_eq!(c.device_addr().0, 0x4000_0000);
    }

    #[test]
    fn data_roundtrip_and_device_view() {
        let mem = SharedMem::new(4096);
        let mut p = DmaPool::new(HostBacking { mem: mem.clone(), device_base: 0 });
        let buf = p.alloc(16, 4).unwrap();
        p.write(&buf, 0, b"dma works fine!!");
        // The "device" sees the same bytes at the device address.
        let dev_view = unsafe {
            core::slice::from_raw_parts(mem.raw().add(buf.device_addr().0 as usize), 16)
        };
        assert_eq!(dev_view, b"dma works fine!!");
        let mut back = [0u8; 16];
        p.read(&buf, 0, &mut back);
        assert_eq!(&back, b"dma works fine!!");
    }
}
