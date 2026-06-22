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

/// Build the init→storaged startup block (rev1§5.1): magic `"SD02"` followed
/// by five little-endian `u64` fields (MMIO VA, DMA VA, DMA device PA, DMA
/// length, time-page VA). storaged decodes the inverse (its `parse_config`);
/// the format is pinned on both ends (B15C).
fn build_sd02(mmio_va: u64, dma_va: u64, dma_pa: u64, dma_len: u64, time_va: u64) -> [u8; 44] {
    let mut c = [0u8; 44];
    c[..4].copy_from_slice(b"SD02");
    c[4..12].copy_from_slice(&mmio_va.to_le_bytes());
    c[12..20].copy_from_slice(&dma_va.to_le_bytes());
    c[20..28].copy_from_slice(&dma_pa.to_le_bytes());
    c[28..36].copy_from_slice(&dma_len.to_le_bytes());
    c[36..44].copy_from_slice(&time_va.to_le_bytes());
    c
}

/// The shell's cspace slot holding the storage-session channel (rev1§5.1):
/// init `cap_install`s `SESSION_B` here and the startup table names it as the
/// `storage` grant, so the name and the install can never drift.
const SHELL_SESSION_SLOT: u32 = 1;

/// Build the init→shell startup block (rev1§5.1, C1C): the unified `b"EUS1"`
/// named-grant table (`loader::startup`) carrying the standard names the shell
/// holds today — `time` (the read-only page init mapped at `TIME_VA`, as a
/// region grant carrying the VA), `storage` (the session channel at the shell's
/// cspace slot 1), and `root` (the full-rights ref at handle 0 on that session).
/// `stdin`/`stdout` (Design decision 4, C-M9 populates) and `tmp` (Design
/// decision 3, no subtree today) are reserved names, deliberately not emitted.
/// Returns the encoded length, or an `EncodeError` the caller maps to a clean
/// boot failure (refuse-not-crash, rev1§2.7) — never a panic.
fn build_shell_block(out: &mut [u8]) -> Result<usize, loader::startup::EncodeError> {
    use loader::startup::*;
    let mut s = Startup::new();
    s.push_grant(Grant {
        name: NAME_TIME,
        kind: GrantKind::Region {
            va: TIME_VA,
            len: 4096,
            pa: 0,
        },
    })?;
    s.push_grant(Grant {
        name: NAME_STORAGE,
        kind: GrantKind::CapSlot(SHELL_SESSION_SLOT),
    })?;
    s.push_grant(Grant {
        name: NAME_ROOT,
        kind: GrantKind::StorageHandle(0),
    })?;
    encode(&s, out)
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

    let config = build_sd02(MMIO_VA, DMA_VA, dma_pa, DMA_PAGES * 4096, TIME_VA);
    check(
        sys::chan_send(SD_BOOT_A, &config, None),
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

    let mut sh_config = [0u8; loader::startup::MAX_BLOCK];
    let sh_len = match build_shell_block(&mut sh_config) {
        Ok(n) => n,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: encode shell startup block\n");
            sys::exit();
        }
    };
    check(
        sys::chan_send(SH_BOOT_A, &sh_config[..sh_len], None),
        b"sh startup block",
    );
    check(
        sys::cap_install(sh.cspace_slot, SH_BOOT_B, 0),
        b"sh boot install",
    );
    check(
        sys::cap_install(sh.cspace_slot, SESSION_B, SHELL_SESSION_SLOT),
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
    //! B15C/C1C — host tests for init's startup-block *builders* and the RTC
    //! sanity rule (rev1§6 Baseline tier). init produces the SD02 block (still
    //! bespoke until C1B) and the shell block (C1C: the unified `b"EUS1"` table,
    //! `loader::startup`). SD02 round-trips through a local parser mirroring
    //! storaged's rule; the shell block drives the *shared* codec on both ends
    //! (`encode` here, `decode` via `loader::startup`) — no mirror needed.
    use super::*;
    use proptest::prelude::*;

    /// Mirror of storaged's `parse_config` rule.
    fn parse_sd02(buf: &[u8]) -> Option<(u64, u64, u64, u64, u64)> {
        if buf.len() < 44 || &buf[..4] != b"SD02" {
            return None;
        }
        let rd = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
        Some((rd(4), rd(12), rd(20), rd(28), rd(36)))
    }

    #[test]
    fn build_sd02_golden_layout() {
        let c = build_sd02(0x1122_3344_5566_7788, 0xA, 0xB, 0xC, 0xD);
        assert_eq!(c.len(), 44);
        assert_eq!(&c[..4], b"SD02");
        assert_eq!(&c[4..12], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&c[36..44], &0xDu64.to_le_bytes());
    }

    #[test]
    fn build_sd02_round_trips_through_the_storaged_rule() {
        let c = build_sd02(0xA000_0000, 0xA100_0000, 0x4321, 64 * 4096, 0xA300_0000);
        assert_eq!(
            parse_sd02(&c),
            Some((0xA000_0000, 0xA100_0000, 0x4321, 64 * 4096, 0xA300_0000))
        );
    }

    #[test]
    fn shell_block_carries_named_grants() {
        // The init→shell block (C1C) is now the unified `b"EUS1"` table; drive
        // the real shared codec on both ends (encode here, decode via
        // `loader::startup`) — no mirrored hand-parser.
        use loader::startup::*;
        let mut buf = [0u8; MAX_BLOCK];
        let n = build_shell_block(&mut buf).expect("encode shell block");
        let s = decode(&buf[..n]).expect("decode shell block");
        // `time`: the read-only page init mapped at TIME_VA (rev1§2.6).
        assert_eq!(
            s.grant(NAME_TIME),
            Some(GrantKind::Region {
                va: TIME_VA,
                len: 4096,
                pa: 0
            })
        );
        // `storage`: the session channel at the shell's cspace slot 1.
        assert_eq!(s.grant(NAME_STORAGE), Some(GrantKind::CapSlot(1)));
        // `root`: the full-rights ref at handle 0.
        assert_eq!(s.grant(NAME_ROOT), Some(GrantKind::StorageHandle(0)));
        // stdin/stdout (DD4) and tmp (DD3) are reserved but unpopulated in C1.
        assert_eq!(s.grant(NAME_STDIN), None);
        assert_eq!(s.grant(NAME_STDOUT), None);
        assert_eq!(s.grant(NAME_TMP), None);
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

        /// `build_sd02` ↔ the storaged rule round-trips any five fields.
        #[test]
        fn build_sd02_round_trips_arbitrary_fields(
            a in any::<u64>(), b in any::<u64>(), c in any::<u64>(),
            d in any::<u64>(), e in any::<u64>(),
        ) {
            prop_assert_eq!(parse_sd02(&build_sd02(a, b, c, d, e)), Some((a, b, c, d, e)));
        }
    }
}
