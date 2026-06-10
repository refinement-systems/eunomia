//! storaged — the storage server on Eunomia (spec §4): mounts the
//! versioned store over virtio-blk and serves the handle-relative session
//! protocol to the shell over a channel.
//!
//! World (built by init, §5.1): slot 0 = bootstrap channel whose first
//! message is the config block; slot 1 = the shell's session channel.
//! The MMIO window and DMA region are pre-mapped by init; the DMA
//! region's device address arrives in the config (init read it via
//! frame_paddr — the phys-read path, §2.5).

#![no_std]
#![no_main]

extern crate alloc;

use dma_pool::{DeviceAddress, DmaBacking, DmaPool};
use ipc::sys;
use storage_server::wire;
use storage_server::Server;
use virtio_blk::blockdev::VirtioBlockDev;
use virtio_blk::{Mmio, VirtioBlk};

#[global_allocator]
static HEAP: urt::Heap<{ 3 * 1024 * 1024 }> = urt::Heap::new();

const BOOT_CHAN: u32 = 0;
const SESSION_CHAN: u32 = 1;
const WAKE_NOTIF: u32 = 2;

struct MmioWindow {
    base: usize,
}

impl Mmio for MmioWindow {
    fn read32(&self, offset: usize) -> u32 {
        unsafe { ((self.base + offset) as *const u32).read_volatile() }
    }

    fn write32(&mut self, offset: usize, value: u32) {
        unsafe { ((self.base + offset) as *mut u32).write_volatile(value) }
    }
}

struct DmaRegion {
    va: *mut u8,
    pa: u64,
    len: usize,
}

unsafe impl DmaBacking for DmaRegion {
    fn cpu_base(&self) -> *mut u8 {
        self.va
    }

    fn device_base(&self) -> DeviceAddress {
        DeviceAddress(self.pa)
    }

    fn len(&self) -> usize {
        self.len
    }
}

fn counter() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {v}, cntvct_el0", v = out(reg) v) };
    v
}

fn recv_blocking(chan: u32, buf: &mut [u8; 256]) -> usize {
    loop {
        let (len, _) = sys::chan_recv(chan, buf.as_mut_ptr(), None);
        if len >= 0 {
            return len as usize;
        }
        sys::yield_now();
    }
}

fn send_blocking(chan: u32, data: &[u8]) {
    while sys::chan_send(chan, data, None) == sys::ERR_FULL {
        sys::yield_now();
    }
}

fn fail(msg: &[u8]) -> ! {
    sys::debug_write(b"[storaged] FATAL: ");
    sys::debug_write(msg);
    sys::debug_write(b"\n");
    sys::exit()
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    let mut buf = [0u8; 256];
    let len = recv_blocking(BOOT_CHAN, &mut buf);
    if len < 36 || &buf[..4] != b"SD01" {
        fail(b"bad config block");
    }
    let rd = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    let (mmio_va, dma_va, dma_pa, dma_len) = (rd(4), rd(12), rd(20), rd(28));

    // Probe the 32 virtio-mmio transports for the block device.
    let mut blk = None;
    for i in 0..32usize {
        let win = MmioWindow { base: mmio_va as usize + i * 0x200 };
        if win.read32(0) != 0x7472_6976 || win.read32(8) != 2 {
            continue;
        }
        let pool = DmaPool::new(DmaRegion {
            va: dma_va as *mut u8,
            pa: dma_pa,
            len: dma_len as usize,
        });
        match VirtioBlk::new(win, pool, 64 * 1024) {
            Ok(b) => {
                blk = Some(b);
                break;
            }
            Err(_) => continue,
        }
    }
    let Some(blk) = blk else { fail(b"no virtio-blk device") };
    sys::debug_write(b"[storaged] virtio-blk up\n");

    let dev = VirtioBlockDev::new(blk);
    let store = match cas::store::Store::mount(dev, cas::store::StoreOptions::default()) {
        Ok(s) => s,
        Err(_) => fail(b"mount failed"),
    };
    sys::debug_write(b"[storaged] store mounted\n");

    let mut server = Server::new(store, counter());
    let grant = match server.root_grant(b"main") {
        Ok(g) => g,
        Err(_) => fail(b"no main ref"),
    };
    let session = server.open_session(alloc::vec![grant]);
    sys::debug_write(b"[storaged] serving\n");

    // Drain-then-wait (the §3.6 lost-wakeup discipline): handle every
    // queued request, then block on the readable→notification binding.
    loop {
        loop {
            let (len, _) = sys::chan_recv(SESSION_CHAN, buf.as_mut_ptr(), None);
            if len < 0 {
                break;
            }
            let resp = match wire::decode_request(&buf[..len as usize]) {
                Ok(req) => server.handle(session, req, counter()),
                Err(_) => storage_server::Response::Err(storage_server::ErrorCode::Internal),
            };
            match wire::encode_response(&resp) {
                Ok(bytes) => send_blocking(SESSION_CHAN, &bytes),
                Err(_) => {
                    // Response too big for a message — report instead
                    // (the bulk path is post-MVP; requests are bounded).
                    let e = storage_server::Response::Err(storage_server::ErrorCode::Internal);
                    send_blocking(SESSION_CHAN, &wire::encode_response(&e).unwrap());
                }
            }
        }
        sys::notif_wait(WAKE_NOTIF);
    }
}

struct DebugOut;

impl core::fmt::Write for DebugOut {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        sys::debug_write(s.as_bytes());
        Ok(())
    }
}

#[panic_handler]
fn on_panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let _ = write!(DebugOut, "[storaged] PANIC: {info}\n");
    sys::exit()
}
