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

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the crate builds as a host harness (Design decision 2,
// B15C): std and the default test `main` take over, the bare-metal items
// below are gated out, and only the pure `parse_config` decoder plus the
// boot-only helpers (dead, but host-compilable) remain — allow the dead-code
// / unused-import noise that leaves.
#![cfg_attr(test, allow(dead_code, unused_imports))]

extern crate alloc;

use dma_pool::{DeviceAddress, DmaBacking, DmaPool};
use ipc::{sys, Endpoint, Message, Reactor, RecvErr, SendErr, Signals, SyscallTransport};
use storage_server::wire;
use storage_server::Server;
use virtio_blk::blockdev::VirtioBlockDev;
use virtio_blk::{Mmio, VirtioBlk};

#[cfg(not(test))]
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
#[cfg(not(test))]
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

/// The init→storaged startup block (rev1§5.1): magic `"SD02"` followed by five
/// little-endian `u64` fields (MMIO window VA, DMA region VA, DMA device PA,
/// DMA length, time-page VA).
#[derive(Debug, PartialEq)]
struct Config {
    mmio_va: u64,
    dma_va: u64,
    dma_pa: u64,
    dma_len: u64,
    time_va: u64,
}

/// Decode the SD02 config block. This is a decode of an untrusted-shaped
/// message (rev1§2.7): a too-short or mis-magicked block is *refused* with
/// `None`, never a panic (a panic in `_start` is a boot failure). Total over
/// any byte slice — the length guard precedes every index, and a `>= 44`-byte
/// buffer covers the final `[36..44]` field; trailing bytes are ignored. init
/// builds the inverse (its `build_sd02`); the format is pinned on both ends.
fn parse_config(buf: &[u8]) -> Option<Config> {
    if buf.len() < 44 || &buf[..4] != b"SD02" {
        return None;
    }
    let rd = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
    Some(Config {
        mmio_va: rd(4),
        dma_va: rd(12),
        dma_pa: rd(20),
        dma_len: rd(28),
        time_va: rd(36),
    })
}

#[cfg(not(test))]
#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    let mut buf = [0u8; 256];
    let len = recv_blocking(BOOT_CHAN, &mut buf);
    let Some(cfg) = parse_config(&buf[..len]) else {
        fail(b"bad config block");
    };
    let Config {
        mmio_va,
        dma_va,
        dma_pa,
        dma_len,
        time_va,
    } = cfg;
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

#[cfg(not(test))]
#[panic_handler]
fn on_panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let _ = write!(DebugOut, "[storaged] PANIC: {info}\n");
    sys::thread_exit(sys::STATUS_PANIC)
}

#[cfg(test)]
mod tests {
    //! B15C — host tests for the SD02 startup-block decoder (rev1§6 Baseline
    //! tier). storaged is the SD02 *consumer*; init the producer. The two are
    //! separate `bin` mini-workspaces that cannot import each other, so the
    //! round-trip here drives the real `parse_config` against a local builder
    //! that mirrors init's `build_sd02` — the format is pinned on both ends.
    use super::*;
    use proptest::prelude::*;

    /// Mirror of init's `build_sd02` (the two bins can't share a module).
    fn sd02(mmio_va: u64, dma_va: u64, dma_pa: u64, dma_len: u64, time_va: u64) -> [u8; 44] {
        let mut b = [0u8; 44];
        b[..4].copy_from_slice(b"SD02");
        b[4..12].copy_from_slice(&mmio_va.to_le_bytes());
        b[12..20].copy_from_slice(&dma_va.to_le_bytes());
        b[20..28].copy_from_slice(&dma_pa.to_le_bytes());
        b[28..36].copy_from_slice(&dma_len.to_le_bytes());
        b[36..44].copy_from_slice(&time_va.to_le_bytes());
        b
    }

    #[test]
    fn parse_config_round_trips_the_five_fields() {
        let block = sd02(
            0xA000_0000,
            0xA100_0000,
            0x4321_0000,
            64 * 4096,
            0xA300_0000,
        );
        assert_eq!(
            parse_config(&block),
            Some(Config {
                mmio_va: 0xA000_0000,
                dma_va: 0xA100_0000,
                dma_pa: 0x4321_0000,
                dma_len: 64 * 4096,
                time_va: 0xA300_0000,
            })
        );
    }

    #[test]
    fn parse_config_refuses_short_and_garbage() {
        // Empty, one byte short of the 44-byte minimum, and a wrong magic —
        // each refused with None, never a panic (rev1§2.7 decode discipline).
        assert_eq!(parse_config(&[]), None);
        assert_eq!(parse_config(&sd02(1, 2, 3, 4, 5)[..43]), None);
        let mut wrong = sd02(1, 2, 3, 4, 5);
        wrong[3] = b'3'; // "SD03"
        assert_eq!(parse_config(&wrong), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Total over arbitrary bytes: `parse_config` never panics — the
        /// refuse-not-crash floor (rev1§2.7).
        #[test]
        fn parse_config_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = parse_config(&bytes);
        }

        /// Any "SD02"-prefixed, >= 44-byte buffer parses to `Some` with the
        /// five fields read at their LE offsets; trailing bytes are ignored.
        #[test]
        fn parse_config_accepts_well_formed(
            mmio_va in any::<u64>(),
            dma_va in any::<u64>(),
            dma_pa in any::<u64>(),
            dma_len in any::<u64>(),
            time_va in any::<u64>(),
            tail in proptest::collection::vec(any::<u8>(), 0..16),
        ) {
            let mut block = sd02(mmio_va, dma_va, dma_pa, dma_len, time_va).to_vec();
            block.extend(tail);
            prop_assert_eq!(
                parse_config(&block),
                Some(Config { mmio_va, dma_va, dma_pa, dma_len, time_va })
            );
        }
    }
}
