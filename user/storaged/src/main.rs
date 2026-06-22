//! storaged — the storage server on Eunomia (spec rev1§4): mounts the
//! versioned store over virtio-blk and serves the handle-relative session
//! protocol to the shell over a channel.
//!
//! World (built by init, rev1§5.1): slot 0 = bootstrap channel whose first
//! message is the unified startup block (`b"EUS1"`, the rev1§5.1 named-grant
//! table — C1B); slot 1 = the shell's session channel. The MMIO window, DMA
//! region, and time page are pre-mapped by init and arrive in the block as three
//! `REGION` grants — `virtio-mmio`, `dma` (its device address read via
//! frame_paddr, the phys-read path, rev1§2.5), and `time` (rev1§2.6/rev1§5.1).

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
use loader::startup::{self, GrantKind};
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

/// The three pre-mapped regions storaged needs from the init→storaged startup
/// block (rev1§5.1): the virtio MMIO window VA, the DMA region (VA, length, and
/// device PA — the phys-read path, rev1§2.5), and the time-page VA (rev1§2.6).
/// All three arrive as `REGION` grants in the unified `b"EUS1"` block (C1B);
/// init maps each page before start, so only the VAs travel — never assumed.
#[derive(Debug, PartialEq)]
struct Config {
    mmio_va: u64,
    dma_va: u64,
    dma_pa: u64,
    dma_len: u64,
    time_va: u64,
}

/// The `(va, len, pa)` of the first `REGION` grant named `name`, or `None` if
/// the name is absent or carries a non-region kind.
fn region(s: &startup::Startup, name: u8) -> Option<(u64, u64, u64)> {
    match s.grant(name)? {
        GrantKind::Region { va, len, pa } => Some((va, len, pa)),
        _ => None,
    }
}

/// Decode the init→storaged startup block and extract the three required
/// regions. The block is an untrusted-shaped message (rev1§2.7): a malformed
/// block — bad magic, a truncated entry, or a missing/wrong-kind required region
/// — is *refused* with `None`, never a panic (a panic in `_start` is a boot
/// failure). Totality is `loader::startup::decode`'s (fuzzed) guarantee; this
/// layer adds only the name lookups. init builds the inverse (its
/// `build_storaged_block`); the codec is now shared on both ends (C1B).
fn parse_config(buf: &[u8]) -> Option<Config> {
    let s = startup::decode(buf)?;
    let (mmio_va, _mmio_len, _) = region(&s, startup::NAME_VIRTIO_MMIO)?;
    let (dma_va, dma_len, dma_pa) = region(&s, startup::NAME_DMA)?;
    let (time_va, _time_len, _) = region(&s, startup::NAME_TIME)?;
    Some(Config {
        mmio_va,
        dma_va,
        dma_pa,
        dma_len,
        time_va,
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
    //! C1B — host tests for the storaged startup-block *consumer* (rev1§6
    //! Baseline tier). storaged decodes the unified `b"EUS1"` block (init is the
    //! producer); the codec is now the **shared** `loader::startup`, so these
    //! tests drive the real `decode` through `parse_config` against blocks built
    //! by the real `encode` — the format is pinned on both ends by the same code,
    //! not by mirrored hand-parsers. `parse_config`'s own job is the three name
    //! lookups + the region-kind check; decode totality is `loader`'s (fuzzed).
    use super::*;
    use proptest::prelude::*;

    /// Build the storaged startup block via the shared codec — the inverse of
    /// `parse_config`, mirroring init's `build_storaged_block` (the two bins
    /// can't share a module, but they now share `loader::startup`).
    fn storaged_block(
        mmio_va: u64,
        dma_va: u64,
        dma_pa: u64,
        dma_len: u64,
        time_va: u64,
    ) -> Vec<u8> {
        let mut s = startup::Startup::new();
        s.push_grant(startup::Grant {
            name: startup::NAME_VIRTIO_MMIO,
            kind: GrantKind::Region {
                va: mmio_va,
                len: 32 * 0x200,
                pa: 0,
            },
        })
        .unwrap();
        s.push_grant(startup::Grant {
            name: startup::NAME_DMA,
            kind: GrantKind::Region {
                va: dma_va,
                len: dma_len,
                pa: dma_pa,
            },
        })
        .unwrap();
        s.push_grant(startup::Grant {
            name: startup::NAME_TIME,
            kind: GrantKind::Region {
                va: time_va,
                len: 4096,
                pa: 0,
            },
        })
        .unwrap();
        let mut buf = [0u8; startup::MAX_BLOCK];
        let n = startup::encode(&s, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    #[test]
    fn parse_config_round_trips_the_three_regions() {
        let block = storaged_block(
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
    fn parse_config_refuses_bad_magic_and_truncation() {
        // Empty, a wrong magic, and a one-byte-truncated valid block — each
        // refused with None, never a panic (rev1§2.7 decode discipline).
        assert_eq!(parse_config(&[]), None);
        assert_eq!(parse_config(b"SD02\x00\x00\x00"), None);
        let block = storaged_block(1, 2, 3, 4, 5);
        assert_eq!(parse_config(&block[..block.len() - 1]), None);
    }

    #[test]
    fn parse_config_refuses_a_missing_required_region() {
        // A well-formed EUS1 block that simply omits the MMIO region: decode
        // succeeds, but the name lookup fails, so parse_config refuses cleanly
        // (a missing required grant is a boot failure, never a panic).
        let mut s = startup::Startup::new();
        s.push_grant(startup::Grant {
            name: startup::NAME_DMA,
            kind: GrantKind::Region {
                va: 0xA100_0000,
                len: 4096,
                pa: 0x4321,
            },
        })
        .unwrap();
        s.push_grant(startup::Grant {
            name: startup::NAME_TIME,
            kind: GrantKind::Region {
                va: 0xA300_0000,
                len: 4096,
                pa: 0,
            },
        })
        .unwrap();
        let mut buf = [0u8; startup::MAX_BLOCK];
        let n = startup::encode(&s, &mut buf).unwrap();
        assert!(startup::decode(&buf[..n]).is_some()); // the block itself is valid…
        assert_eq!(parse_config(&buf[..n]), None); // …but MMIO is absent.
    }

    #[test]
    fn parse_config_refuses_a_wrong_kind_region() {
        // The TIME name carried as a cap-slot rather than a region: present but
        // the wrong kind, so the region lookup refuses (no panic, no misread).
        let mut s = startup::Startup::new();
        s.push_grant(startup::Grant {
            name: startup::NAME_VIRTIO_MMIO,
            kind: GrantKind::Region {
                va: 0xA000_0000,
                len: 4096,
                pa: 0,
            },
        })
        .unwrap();
        s.push_grant(startup::Grant {
            name: startup::NAME_DMA,
            kind: GrantKind::Region {
                va: 0xA100_0000,
                len: 4096,
                pa: 0x4321,
            },
        })
        .unwrap();
        s.push_grant(startup::Grant {
            name: startup::NAME_TIME,
            kind: GrantKind::CapSlot(5),
        })
        .unwrap();
        let mut buf = [0u8; startup::MAX_BLOCK];
        let n = startup::encode(&s, &mut buf).unwrap();
        assert_eq!(parse_config(&buf[..n]), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Total over arbitrary bytes: `parse_config` never panics — the
        /// refuse-not-crash floor (rev1§2.7), inherited from `decode`.
        #[test]
        fn parse_config_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = parse_config(&bytes);
        }

        /// Any block carrying the three required regions parses to `Some` with
        /// the looked-up VAs/PA/len; trailing bytes (the recv buffer's zero
        /// padding) are ignored.
        #[test]
        fn parse_config_accepts_well_formed(
            mmio_va in any::<u64>(),
            dma_va in any::<u64>(),
            dma_pa in any::<u64>(),
            dma_len in any::<u64>(),
            time_va in any::<u64>(),
            tail in proptest::collection::vec(any::<u8>(), 0..16),
        ) {
            let mut block = storaged_block(mmio_va, dma_va, dma_pa, dma_len, time_va);
            block.extend(tail);
            prop_assert_eq!(
                parse_config(&block),
                Some(Config { mmio_va, dma_va, dma_pa, dma_len, time_va })
            );
        }
    }
}
