//! storaged — the storage server on Eunomia (spec rev1§4): mounts the
//! versioned store over virtio-blk and serves the handle-relative session
//! protocol to the shell over a channel.
//!
//! World (built by init, rev1§5.1): slot 0 = bootstrap channel whose first
//! message is the config block; slot 1 = the shell's session channel.
//! The MMIO window, DMA region, and time page are pre-mapped by init;
//! their addresses arrive in the config block (the DMA device address
//! via frame_paddr — the phys-read path, rev1§2.5; the time page under the
//! `"time"` grant, rev1§2.6/rev1§5.1).

#![no_std]
#![no_main]

extern crate alloc;

use dma_pool::{DeviceAddress, DmaBacking, DmaPool};
use ipc::{sys, Endpoint, Message, Reactor, RecvErr, SendErr, Signals, SyscallTransport};
use storage_server::wire;
use storage_server::Server;
use virtio_blk::blockdev::VirtioBlockDev;
use virtio_blk::{Mmio, VirtioBlk};

#[global_allocator]
static HEAP: urt::Heap<{ 3 * 1024 * 1024 }> = urt::Heap::new();

const BOOT_CHAN: u32 = 0;
const SESSION_CHAN: u32 = 1;
const WAKE_NOTIF: u32 = 2;
// The reactor source key for the session channel. storaged multiplexes one
// source today; the key is what a second session (a future connect) would
// add alongside — the reactor returns it from `wait`, so the dispatch below
// never names a notification bit (rev1§3.6: the API hides the bit shape).
const SESSION_KEY: ipc::Key = 0;

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

/// The server clock: UTC nanoseconds from the time page (rev1§2.6). One
/// value feeds snapshot timestamps, file mtimes, and ticket TTLs. The
/// spec representation is signed 64-bit; init refuses an insane RTC at
/// boot, so the value is positive and the u64 cast at the storage API
/// boundary carries identical bytes.
fn now_utc() -> u64 {
    urt::time::now_utc_ns() as u64
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

/// Send one response message, retrying on backpressure. Responses are
/// message-bounded (`encode_response` refuses > 256 bytes, rev1§3.1), so a single
/// `Message` always suffices; `Full` means the shell hasn't drained its reply
/// ring yet (it reads one reply per request), so a yield-retry drains promptly.
/// `Closed` ends the loop — the peer is gone.
fn send_response<T: ipc::Transport>(ep: &Endpoint<T>, bytes: &[u8]) {
    let msg = Message::bytes(bytes);
    loop {
        match ep.send_nb(&msg) {
            Ok(()) | Err(SendErr::Closed) => return,
            Err(SendErr::Full) => sys::yield_now(),
            Err(SendErr::Other(_)) => return,
        }
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
    if len < 44 || &buf[..4] != b"SD02" {
        fail(b"bad config block");
    }
    let rd = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    let (mmio_va, dma_va, dma_pa, dma_len, time_va) = (rd(4), rd(12), rd(20), rd(28), rd(36));
    // Safety: init mapped the read-only time page at this address before
    // starting us, and the mapping lives as long as the process.
    unsafe { urt::time::attach(time_va as usize) };

    // Probe the 32 virtio-mmio transports for the block device.
    let mut blk = None;
    for i in 0..32usize {
        let win = MmioWindow {
            base: mmio_va as usize + i * 0x200,
        };
        if win.read32(0) != 0x7472_6976 || win.read32(8) != 2 {
            continue;
        }
        let pool = DmaPool::new(DmaRegion {
            va: dma_va as *mut u8,
            pa: dma_pa,
            len: dma_len as usize,
        });
        // The driver's completion poll observes the device's used-index update
        // via the pool's volatile read (B2A/I-4); the IRQ-driven wait between
        // polls, replacing the busy-spin, arrives with B-IRQ/C-M9 (rev1§3.6).
        match VirtioBlk::new(win, pool, 64 * 1024) {
            Ok(b) => {
                blk = Some(b);
                break;
            }
            Err(_) => continue,
        }
    }
    let Some(blk) = blk else {
        fail(b"no virtio-blk device")
    };
    sys::debug_write(b"[storaged] virtio-blk up\n");

    let dev = VirtioBlockDev::new(blk);
    let store = match cas::store::Store::mount(dev, cas::store::StoreOptions::default()) {
        Ok(s) => s,
        Err(_) => fail(b"mount failed"),
    };
    sys::debug_write(b"[storaged] store mounted\n");

    let mut server = Server::new(store, now_utc());
    // The privileged stat-store holder (rev1§2.3): `root_grant` is the sole
    // origin of `R_STAT_STORE`, so this single session can `statfs`. Phase C1
    // splits this into per-process child sessions, which receive attenuated
    // handles that strip `stat-store` unless explicitly granted.
    let grant = match server.root_grant(b"main") {
        Ok(g) => g,
        Err(_) => fail(b"no main ref"),
    };
    let session = server.open_session(alloc::vec![grant]);
    sys::debug_write(b"[storaged] serving\n");

    // The IPC reactor (rev1§3.6) — storaged is its first production consumer.
    // `register` binds the session channel's readable event to WAKE_NOTIF and
    // self-signals (poll once), so the first `wait` drains anything already
    // queued and the bind-poll-wait lost-wakeup discipline lives in the crate,
    // not here. Dispatch is by the opaque `key`, never a notification bit — the
    // same loop would multiplex a second session by registering its channel
    // under a second key (the connect path).
    let transport = SyscallTransport;
    let ep = Endpoint::new(&transport, SESSION_CHAN);
    let mut reactor = Reactor::new(&transport, WAKE_NOTIF);
    if reactor
        .register(SESSION_CHAN, Signals::READABLE, SESSION_KEY)
        .is_err()
    {
        fail(b"reactor register");
    }
    let mut msg = Message::new();
    loop {
        // Staleness sweep (rev1§4.4 trigger 4, B12D): before parking the reactor,
        // flush any ref quietly dirty past the staleness bound so it eventually
        // becomes committed tree even with no further writes (Design decision 5 —
        // opportunistic, no armed kernel timer). This is the reactor-idle point:
        // the request ring has just drained (the inner loop broke on Empty), so we
        // are about to block. Best-effort — a flush failure is non-fatal (the next
        // write still logs durably). A no-op until B12F ships the 30 s default.
        let _ = server.store().flush_stale(now_utc());
        let (key, _signals) = reactor.wait();
        debug_assert_eq!(key, SESSION_KEY);
        // Drain every queued request for the ready source, then wait again
        // (a wakeup is level-ish — keep serving until the ring is Empty).
        loop {
            match ep.recv_nb(&mut msg) {
                Ok(()) => {}
                Err(RecvErr::Empty) => break,
                // NoSlot can't arise (no caps in storage requests); Closed/other
                // means the peer is gone — stop draining and re-wait.
                Err(_) => break,
            }
            let resp = match wire::decode_request(msg.payload()) {
                Ok(req) => server.handle(session, req, now_utc()),
                Err(_) => storage_server::Response::Err(storage_server::ErrorCode::Internal),
            };
            match wire::encode_response(&resp) {
                Ok(bytes) => send_response(&ep, &bytes),
                Err(_) => {
                    // Response too big for a message — report instead
                    // (the bulk path is post-MVP; requests are bounded).
                    let e = storage_server::Response::Err(storage_server::ErrorCode::Internal);
                    send_response(&ep, &wire::encode_response(&e).unwrap());
                }
            }
            // Drain a pending GC trigger (rev1§4.6: post-rewrite or
            // watermark) after replying: the foreground op stays fast,
            // reclamation follows promptly.
            if server.gc_requested() {
                use core::fmt::Write;
                match server.run_gc() {
                    Ok(s) => {
                        let _ = write!(
                            DebugOut,
                            "[storaged] gc: freed {} objects / {} bytes, {} live\n",
                            s.freed_objects, s.freed_bytes, s.live_objects
                        );
                    }
                    Err(_) => sys::debug_write(b"[storaged] gc failed\n"),
                }
            }
        }
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
    sys::thread_exit(sys::STATUS_PANIC)
}
