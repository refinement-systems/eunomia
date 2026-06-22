//! init — the one process the kernel constructs (rev1§1). Holds all initial
//! authority and wires the running system (rev1§5.2: init is the only
//! binder): it reads the PL031 once and publishes the time page (rev1§2.6),
//! spawns storaged (granting the virtio MMIO window, a DMA region whose
//! device address it reads via phys-read, the session channel, and the
//! time page) and the shell (granting the session's other end, an untyped
//! for spawning, the time page, and the console-by-syscall), each with an
//! explicitly constructed cspace and a rev1§5.1 startup block.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the crate builds as a host harness (Design decision 2,
// B15C): std and the default test `main` take over, the bare-metal items and
// the ELF `include_bytes!`d at boot are gated out, and only the pure startup-
// block builders + the RTC sanity rule remain. Allow the dead-code / unused-
// import noise the boot-only items leave behind.
#![cfg_attr(test, allow(dead_code, unused_imports))]

use ipc::sys::{self, OBJ_CHANNEL, OBJ_FRAME, PERM_DEVICE, PERM_W, RIGHT_READ};
// The shared startup-block codec (rev1§5.1, C1B) — host-buildable, used by both
// `_start` (the producer) and the builder tests.
use loader::startup::{self, Grant, GrantKind};
// `loader::spawn` exists only on the bare-metal target (it is gated
// `target_os = "none"`), so the import is boot-only — gated out of the host
// test build alongside the `_start` that is its sole user.
#[cfg(not(test))]
use loader::spawn;

#[cfg(not(test))]
static STORAGED_ELF: &[u8] = include_bytes!(env!("STORAGED_ELF_PATH"));
#[cfg(not(test))]
static SHELL_ELF: &[u8] = include_bytes!(env!("SHELL_ELF_PATH"));

// Kernel-bestowed slots.
const UNTYPED: u32 = 0;
const UNTYPED2: u32 = 2;
const DEVICE_FRAME: u32 = 3;
const PL031_FRAME: u32 = 4;
const SELF_ASPACE: u32 = 5;

// Our allocations.
const SD_BOOT_A: u32 = 6;
const SD_BOOT_B: u32 = 7;
const SH_BOOT_A: u32 = 8;
const SH_BOOT_B: u32 = 9;
const SESSION_A: u32 = 10; // storaged end
const SESSION_B: u32 = 11; // shell end
const DEV_COPY: u32 = 12;
const DMA_FRAME: u32 = 13;
const TIME_FRAME: u32 = 14;
const TIME_SD: u32 = 15;
const TIME_SH: u32 = 16;
const SD_NOTIF: u32 = 17;
/// A second read-only time copy for the shell — installed into its cspace
/// (not just mapped), so the shell can re-grant the page to its children.
const TIME_SH_CHILD: u32 = 18;
const SD_SPAWN_BASE: u32 = 20;
const SH_SPAWN_BASE: u32 = 40;

const MMIO_VA: u64 = 0xA000_0000;
/// The virtio-mmio window storaged probes: 32 transports × 0x200 each. Carried
/// in the `virtio-mmio` region grant for completeness — storaged drives its own
/// probe loop and does not consume the length (it is informational).
const MMIO_LEN: u64 = 32 * 0x200;
/// The time page is a single frame (rev1§2.6); its length rides the `time`
/// region grant. storaged `attach`es by VA and ignores the length.
const TIME_LEN: u64 = 4096;
const DMA_VA: u64 = 0xA100_0000;
/// PL031 window in init's own aspace (the one self-mapping in the system).
const RTC_VA: u64 = 0xA200_0000;
/// Where the time page lands in every child; the address still travels in
/// the startup block (the `"time"` grant, rev1§5.1) — never assumed.
const TIME_VA: u64 = 0xA300_0000;
const DMA_PAGES: u64 = 64;

/// 2020-01-01T00:00:00Z. An RTC reading before this code's own era means
/// the device is absent or unbacked, not that it is 1970.
const RTC_MIN_SANE_SECS: u64 = 1_577_836_800;

const RIGHTS_WITH_PHYS: u64 = 0b111;

/// Is the one-shot RTC reading sane? A reading before this code's own era
/// (2020-01-01, `RTC_MIN_SANE_SECS`) or a zero counter frequency means the
/// device is absent/unbacked, not that it is 1970 — a boot failure, since
/// every store timestamp inherits this value (rev1§2.6).
fn rtc_sane(secs: u64, cntfrq: u64) -> bool {
    secs >= RTC_MIN_SANE_SECS && cntfrq != 0
}

/// Build the init→storaged startup block (rev1§5.1, C1B): the unified `b"EUS1"`
/// format carrying three `REGION` grants — the virtio MMIO window, the DMA pool
/// (with its device PA, the phys-read path rev1§2.5), and the time page
/// (rev1§2.6). Supersedes the bespoke `"SD02"` fixed layout; storaged decodes
/// the inverse (its `parse_config`), the codec now shared on both ends. Returns
/// the encoded length or a clean `EncodeError` (the producer maps it to a boot
/// failure — refuse, never panic). The regions carry no new authority: init
/// `map`s every page before start, so only the VAs travel.
fn build_storaged_block(
    out: &mut [u8],
    mmio_va: u64,
    dma_va: u64,
    dma_pa: u64,
    dma_len: u64,
    time_va: u64,
) -> Result<usize, startup::EncodeError> {
    let mut s = startup::Startup::new();
    s.push_grant(Grant {
        name: startup::NAME_VIRTIO_MMIO,
        kind: GrantKind::Region {
            va: mmio_va,
            len: MMIO_LEN,
            pa: 0,
        },
    })?;
    s.push_grant(Grant {
        name: startup::NAME_DMA,
        kind: GrantKind::Region {
            va: dma_va,
            len: dma_len,
            pa: dma_pa,
        },
    })?;
    s.push_grant(Grant {
        name: startup::NAME_TIME,
        kind: GrantKind::Region {
            va: time_va,
            len: TIME_LEN,
            pa: 0,
        },
    })?;
    startup::encode(&s, out)
}

/// Build the init→shell startup block (rev1§5.1): magic `"SH01"` followed by
/// the time-page VA as a little-endian `u64`. The shell decodes the inverse
/// (`&boot[..4] == b"SH01"` + the 8-byte VA, `shell:_start`); B15C pins the
/// producer side.
fn build_sh01(time_va: u64) -> [u8; 12] {
    let mut c = [0u8; 12];
    c[..4].copy_from_slice(b"SH01");
    c[4..12].copy_from_slice(&time_va.to_le_bytes());
    c
}

fn check(r: i64, what: &[u8]) -> i64 {
    if r < 0 {
        sys::debug_write(b"[init] FAILED: ");
        sys::debug_write(what);
        sys::debug_write(b"\n");
        sys::exit();
    }
    r
}

/// One-shot PL031 read (rev1§2.6): map the RTC read-only into our own aspace,
/// pair seconds-since-epoch from RTCDR with CNTVCT, and never touch the
/// device again — there is deliberately no RTC driver. Boot-only: it reads
/// the aarch64-only `urt::time::cntvct`/`cntfrq` intrinsics, so it is gated
/// out of the host test build (the sanity rule it applies, `rtc_sane`, is
/// host-tested separately).
#[cfg(not(test))]
fn read_boot_utc() -> (i64, u64, u64) {
    check(
        sys::map(SELF_ASPACE, PL031_FRAME, RTC_VA, PERM_DEVICE),
        b"map pl031",
    );
    // RTCDR (offset 0): seconds since the Unix epoch. Safety: the map
    // above just established RTC_VA as a live read-only device mapping.
    let secs = unsafe { (RTC_VA as *const u32).read_volatile() } as u64;
    let cntvct_base = urt::time::cntvct();
    let cntfrq = urt::time::cntfrq();
    // A missing or insane RTC is a boot failure, not a degraded mode:
    // the one-shot read is the design and QEMU virt always provides the
    // device (rev1§2.6). Every timestamp in the store inherits this value —
    // fail loudly rather than seed them garbage.
    if !rtc_sane(secs, cntfrq) {
        sys::debug_write(b"[init] FAILED: insane PL031/CNTFRQ read\n");
        sys::exit();
    }
    // The RTC's one-second granularity leaves ±1 s absolute error in
    // wall_base. Polling for a tick edge would shrink it at the cost of
    // up to a second of boot latency — wrong trade for retention rules
    // denominated in hours. Accepted, not polled away (rev1§2.6).
    ((secs as i64) * 1_000_000_000, cntvct_base, cntfrq)
}

#[cfg(not(test))]
#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    sys::debug_write(b"[init] wiring the system\n");

    check(
        sys::retype(UNTYPED, OBJ_CHANNEL, 4, SD_BOOT_A, SD_BOOT_B),
        b"sd boot chan",
    );
    check(
        sys::retype(UNTYPED, OBJ_CHANNEL, 4, SH_BOOT_A, SH_BOOT_B),
        b"sh boot chan",
    );
    check(
        sys::retype(UNTYPED, OBJ_CHANNEL, 4, SESSION_A, SESSION_B),
        b"session chan",
    );

    // ── the time page (rev1§2.6) ────────────────────────────────────────
    // Funded from init's untyped — the rev1§2.5 grant rule in its degenerate,
    // correct form: the supervisor whose liveness dominates everyone's
    // funds the mapping everyone shares, so nobody can fault anybody.
    let (wall_base_ns, cntvct_base, cntfrq) = read_boot_utc();
    check(
        sys::retype(UNTYPED, OBJ_FRAME, 1, TIME_FRAME, 0),
        b"time frame",
    );
    let page = urt::time::encode_boot(wall_base_ns, cntvct_base, cntfrq);
    check(sys::frame_write(TIME_FRAME, 0, &page), b"time page write");

    // ── storaged ────────────────────────────────────────────────────
    let sd = match spawn::prepare(STORAGED_ELF, UNTYPED, SD_SPAWN_BASE, 8) {
        Ok(p) => p,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: prepare storaged\n");
            sys::exit();
        }
    };
    // The MMIO window: a phys-capable copy, device-mapped into the
    // child. The phys-read bit travels only along this one grant (rev1§2.5).
    check(
        sys::cap_copy(DEVICE_FRAME, DEV_COPY, RIGHTS_WITH_PHYS),
        b"dev copy",
    );
    check(
        sys::map(sd.aspace_slot, DEV_COPY, MMIO_VA, PERM_DEVICE | PERM_W),
        b"map mmio",
    );
    // The DMA pool: ordinary RAM whose PA init reads and tells the
    // driver — the only place a PA crosses into userspace.
    check(
        sys::retype(UNTYPED, OBJ_FRAME, DMA_PAGES, DMA_FRAME, 0),
        b"dma frame",
    );
    let dma_pa = check(sys::frame_paddr(DMA_FRAME), b"frame_paddr") as u64;
    check(
        sys::map(sd.aspace_slot, DMA_FRAME, DMA_VA, PERM_W),
        b"map dma",
    );
    // The "time" grant (rev1§5.1): a read-only derivation per consumer —
    // rights-level read-only, so no holder can ever map it writable.
    check(
        sys::cap_copy(TIME_FRAME, TIME_SD, RIGHT_READ),
        b"time sd copy",
    );
    check(
        sys::map(sd.aspace_slot, TIME_SD, TIME_VA, 0),
        b"time sd map",
    );

    let mut sd_block = [0u8; startup::MAX_BLOCK];
    let sd_len = match build_storaged_block(
        &mut sd_block,
        MMIO_VA,
        DMA_VA,
        dma_pa,
        DMA_PAGES * 4096,
        TIME_VA,
    ) {
        Ok(n) => n,
        // The block is built from fixed init constants, so an overflow would be
        // a build-time bug — but refuse cleanly rather than panic (rev1§2.7).
        Err(_) => {
            sys::debug_write(b"[init] FAILED: build storaged block\n");
            sys::exit();
        }
    };
    check(
        sys::chan_send(SD_BOOT_A, &sd_block[..sd_len], None),
        b"sd startup block",
    );
    // Block-don't-spin: requests wake storaged through a readable→
    // notification binding (rev1§3.6) — under strict priorities a busy-poll
    // server would starve its clients.
    check(
        sys::retype(UNTYPED, sys::OBJ_NOTIF, 0, SD_NOTIF, 0),
        b"sd notif",
    );
    check(
        sys::chan_bind(SESSION_A, sys::EV_READABLE, SD_NOTIF, 1),
        b"sd bind",
    );
    check(
        sys::cap_install(sd.cspace_slot, SD_BOOT_B, 0),
        b"sd boot install",
    );
    check(
        sys::cap_install(sd.cspace_slot, SESSION_A, 1),
        b"sd session install",
    );
    check(
        sys::cap_install(sd.cspace_slot, SD_NOTIF, 2),
        b"sd notif install",
    );
    check(spawn::start(&sd, 5).map_or(-1, |_| 0), b"start storaged");

    // ── shell ───────────────────────────────────────────────────────
    // 64-slot cspace: slots 0-4 are wired below / carved by the shell,
    // slot 5 is the re-grantable time cap, and 8.. is the shell's
    // recyclable spawn window (rev1§5.1 reclaim loop).
    let sh = match spawn::prepare(SHELL_ELF, UNTYPED, SH_SPAWN_BASE, 64) {
        Ok(p) => p,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: prepare shell\n");
            sys::exit();
        }
    };
    check(
        sys::cap_copy(TIME_FRAME, TIME_SH, RIGHT_READ),
        b"time sh copy",
    );
    check(
        sys::map(sh.aspace_slot, TIME_SH, TIME_VA, 0),
        b"time sh map",
    );
    // Re-grantable copy in the shell's cspace slot 5: the shell holds a
    // read-only time cap it can copy and map into each child it spawns,
    // extending the rev1§2.6 time grant one hop (init→shell→child, rev1§5.1).
    check(
        sys::cap_copy(TIME_FRAME, TIME_SH_CHILD, RIGHT_READ),
        b"time child copy",
    );
    check(
        sys::cap_install(sh.cspace_slot, TIME_SH_CHILD, 5),
        b"time child install",
    );

    let sh_config = build_sh01(TIME_VA);
    check(
        sys::chan_send(SH_BOOT_A, &sh_config, None),
        b"sh startup block",
    );
    check(
        sys::cap_install(sh.cspace_slot, SH_BOOT_B, 0),
        b"sh boot install",
    );
    check(
        sys::cap_install(sh.cspace_slot, SESSION_B, 1),
        b"sh session install",
    );
    check(
        sys::cap_install(sh.cspace_slot, UNTYPED2, 2),
        b"sh untyped install",
    );
    check(spawn::start(&sh, 4).map_or(-1, |_| 0), b"start shell");

    sys::debug_write(b"[init] system up\n");
    sys::exit()
}

#[cfg(not(test))]
#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[init] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}

#[cfg(test)]
mod tests {
    //! Host tests for init's startup-block *builders* and the RTC sanity rule
    //! (rev1§6 Baseline tier). C1B: the init→storaged block now uses the shared
    //! `loader::startup` codec, so its round-trip drives the real `encode`
    //! through the real `decode` — no mirrored hand-parser. The init→shell block
    //! is still the bespoke `SH01` (migrated by C1C), so its test keeps the local
    //! `parse_sh01` mirror of the shell's rule until then.
    use super::*;
    use proptest::prelude::*;

    /// Mirror of the shell's SH01 parse (`shell:_start`: `blen >= 12` and the
    /// `b"SH01"` magic, then the 8-byte time VA). Retired by C1C.
    fn parse_sh01(buf: &[u8]) -> Option<u64> {
        if buf.len() >= 12 && &buf[..4] == b"SH01" {
            Some(u64::from_le_bytes(buf[4..12].try_into().unwrap()))
        } else {
            None
        }
    }

    #[test]
    fn build_storaged_block_carries_the_three_regions() {
        let mut buf = [0u8; startup::MAX_BLOCK];
        let n = build_storaged_block(
            &mut buf,
            0xA000_0000,
            0xA100_0000,
            0x4321_0000,
            64 * 4096,
            0xA300_0000,
        )
        .unwrap();
        let s = startup::decode(&buf[..n]).unwrap();
        assert_eq!(s.ngrants, 3);
        assert_eq!(
            s.grant(startup::NAME_VIRTIO_MMIO),
            Some(GrantKind::Region {
                va: 0xA000_0000,
                len: MMIO_LEN,
                pa: 0
            })
        );
        assert_eq!(
            s.grant(startup::NAME_DMA),
            Some(GrantKind::Region {
                va: 0xA100_0000,
                len: 64 * 4096,
                pa: 0x4321_0000
            })
        );
        assert_eq!(
            s.grant(startup::NAME_TIME),
            Some(GrantKind::Region {
                va: 0xA300_0000,
                len: TIME_LEN,
                pa: 0
            })
        );
        // The storaged block carries no argv/env (rev1§5.1 fields, empty here).
        assert_eq!(s.nargv, 0);
        assert_eq!(s.nenv, 0);
    }

    #[test]
    fn build_sh01_golden_and_round_trip() {
        let c = build_sh01(0xA300_0000);
        assert_eq!(c.len(), 12);
        assert_eq!(&c[..4], b"SH01");
        assert_eq!(parse_sh01(&c), Some(0xA300_0000));
        // The shell's guard refuses a short block (refuse-not-crash, rev1§2.7).
        assert_eq!(parse_sh01(&c[..11]), None);
    }

    #[test]
    fn rtc_sane_threshold_and_zero_freq() {
        // Below the 2020-01-01 threshold → insane.
        assert!(!rtc_sane(RTC_MIN_SANE_SECS - 1, 24_000_000));
        // At / above the threshold with a nonzero counter → sane.
        assert!(rtc_sane(RTC_MIN_SANE_SECS, 24_000_000));
        assert!(rtc_sane(RTC_MIN_SANE_SECS + 1_000_000, 62_500_000));
        // A zero counter frequency → insane regardless of the seconds.
        assert!(!rtc_sane(RTC_MIN_SANE_SECS + 1_000_000, 0));
        // 1970 (epoch 0) → insane.
        assert!(!rtc_sane(0, 24_000_000));
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// `build_storaged_block` ↔ the real `loader::startup::decode`
        /// round-trips any region fields (the codec is shared, so this drives
        /// the actual producer→consumer path, not a mirror).
        #[test]
        fn build_storaged_block_round_trips_arbitrary_fields(
            mmio_va in any::<u64>(), dma_va in any::<u64>(), dma_pa in any::<u64>(),
            dma_len in any::<u64>(), time_va in any::<u64>(),
        ) {
            let mut buf = [0u8; startup::MAX_BLOCK];
            let n = build_storaged_block(&mut buf, mmio_va, dma_va, dma_pa, dma_len, time_va).unwrap();
            let s = startup::decode(&buf[..n]).unwrap();
            prop_assert_eq!(
                s.grant(startup::NAME_VIRTIO_MMIO),
                Some(GrantKind::Region { va: mmio_va, len: MMIO_LEN, pa: 0 })
            );
            prop_assert_eq!(
                s.grant(startup::NAME_DMA),
                Some(GrantKind::Region { va: dma_va, len: dma_len, pa: dma_pa })
            );
            prop_assert_eq!(
                s.grant(startup::NAME_TIME),
                Some(GrantKind::Region { va: time_va, len: TIME_LEN, pa: 0 })
            );
        }

        /// `build_sh01` ↔ the shell rule round-trips any time VA.
        #[test]
        fn build_sh01_round_trips_arbitrary_va(va in any::<u64>()) {
            prop_assert_eq!(parse_sh01(&build_sh01(va)), Some(va));
        }
    }
}
