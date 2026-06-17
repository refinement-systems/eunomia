//! Userspace runtime: a global allocator over a static in-image heap.
//!
//! Eunomia processes are single-threaded (M3); the allocator is a plain
//! first-fit free list with address-ordered coalescing, no locking. The
//! heap lives in .bss, so the loader maps and zeroes it with the RW
//! segment — no untyped or mapping calls needed to get a heap.
//!
//! Usage in a process binary:
//!   #[global_allocator]
//!   static HEAP: urt::Heap<{ 2 * 1024 * 1024 }> = urt::Heap::new();

#![no_std]

// Verus (plan doc/plans/3_verus-rewrite.md phase 7c): the deductive-proof tier
// for the §4.7 host chokepoints. `vstd::prelude` supplies the `verus!{}` macro +
// ghost vocabulary the `slots` proof uses; Verus requires it imported at the crate
// root. In an ordinary build the macro erases ghost code, so this import is
// otherwise unused — hence the allow (same as kcore/src/lib.rs, ipc/src/lib.rs).
#[allow(unused_imports)]
use vstd::prelude::*;

pub mod slots;
pub mod time;

/// Kani harnesses (plan §4.7), compiled only under `cargo kani`.
#[cfg(kani)]
mod proofs;

// The spawn-lifecycle helper issues syscalls, so it only exists on the
// bare-metal target; `slots` is pure bookkeeping and host-tested.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod spawn;

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

#[repr(C)]
struct Block {
    size: usize,
    next: *mut Block,
}

const MIN_BLOCK: usize = core::mem::size_of::<Block>();

#[repr(C, align(16))]
pub struct Heap<const N: usize> {
    mem: UnsafeCell<[u8; N]>,
    head: UnsafeCell<*mut Block>,
    initialized: UnsafeCell<bool>,
}

// Single-threaded processes; no concurrent access by construction.
unsafe impl<const N: usize> Sync for Heap<N> {}

impl<const N: usize> Heap<N> {
    pub const fn new() -> Self {
        Heap {
            mem: UnsafeCell::new([0; N]),
            head: UnsafeCell::new(ptr::null_mut()),
            initialized: UnsafeCell::new(false),
        }
    }

    unsafe fn init_once(&self) {
        if !*self.initialized.get() {
            let first = self.mem.get() as *mut Block;
            (*first).size = N;
            (*first).next = ptr::null_mut();
            *self.head.get() = first;
            *self.initialized.get() = true;
        }
    }

    fn round(size: usize) -> usize {
        size.max(MIN_BLOCK).next_multiple_of(16)
    }
}

impl<const N: usize> Default for Heap<N> {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl<const N: usize> GlobalAlloc for Heap<N> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.init_once();
        let need = Self::round(layout.size());
        let align = layout.align().max(16);

        let mut prev: *mut *mut Block = self.head.get();
        let mut cur = *prev;
        while !cur.is_null() {
            let base = cur as usize;
            let aligned = base.next_multiple_of(align);
            let pad = aligned - base;
            if pad + need <= (*cur).size {
                // Padding too small to stand alone keeps the block from
                // splitting; skip such blocks (simple and rare with
                // align ≤ 16).
                if pad != 0 && pad < MIN_BLOCK {
                    prev = &mut (*cur).next;
                    cur = *prev;
                    continue;
                }
                let rest = (*cur).size - pad - need;
                if pad >= MIN_BLOCK {
                    // Leading pad stays as a free block.
                    (*cur).size = pad;
                    prev = &mut (*cur).next;
                }
                if rest >= MIN_BLOCK {
                    let tail = (aligned + need) as *mut Block;
                    (*tail).size = rest;
                    (*tail).next = if pad >= MIN_BLOCK { *prev } else { (*cur).next };
                    *prev = tail;
                } else if pad >= MIN_BLOCK {
                    // rest unusable: absorbed into the allocation
                } else {
                    *prev = (*cur).next;
                }
                return aligned as *mut u8;
            }
            prev = &mut (*cur).next;
            cur = *prev;
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        let size = Self::round(layout.size());
        let blk = p as *mut Block;
        (*blk).size = size;
        // Address-ordered insert with two-sided coalescing.
        let mut prev: *mut *mut Block = self.head.get();
        while !(*prev).is_null() && (*prev as usize) < blk as usize {
            prev = &mut (**prev).next;
        }
        (*blk).next = *prev;
        *prev = blk;
        // Merge forward.
        if !(*blk).next.is_null() && blk as usize + (*blk).size == (*blk).next as usize {
            (*blk).size += (*(*blk).next).size;
            (*blk).next = (*(*blk).next).next;
        }
        // Merge backward: re-walk from head (cheap at MVP scale).
        let mut cur = *self.head.get();
        while !cur.is_null() {
            if !(*cur).next.is_null() && cur as usize + (*cur).size == (*cur).next as usize {
                (*cur).size += (*(*cur).next).size;
                (*cur).next = (*(*cur).next).next;
                continue;
            }
            cur = (*cur).next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;

    #[test]
    fn alloc_free_reuse() {
        static H: Heap<65536> = Heap::new();
        unsafe {
            let l = Layout::from_size_align(1000, 8).unwrap();
            let a = H.alloc(l);
            let b = H.alloc(l);
            assert!(!a.is_null() && !b.is_null() && a != b);
            H.dealloc(a, l);
            H.dealloc(b, l);
            // After coalescing, a near-full-heap allocation fits again.
            let big = Layout::from_size_align(60000, 16).unwrap();
            let c = H.alloc(big);
            assert!(!c.is_null());
            H.dealloc(c, big);
        }
    }

    #[test]
    fn exhaustion_returns_null() {
        static H: Heap<4096> = Heap::new();
        unsafe {
            let l = Layout::from_size_align(8192, 8).unwrap();
            assert!(H.alloc(l).is_null());
        }
    }
}
