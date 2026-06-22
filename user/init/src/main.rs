//! init — the one process the kernel constructs (rev1§1). Holds all initial
//! authority and wires the running system (rev1§5.2: init is the only
//! binder): it reads the PL031 once and publishes the time page (rev1§2.6),
//! spawns storaged (granting the virtio MMIO window, a DMA region whose
//! device address it reads via phys-read, the session channel, and the
//! time page) and the shell (granting the session's other end, an untyped
//! for spawning, the time page, and the console-by-syscall), each with an
//! explicitly constructed cspace and a rev1§5.1 startup block.

#![no_std]
#![no_main]

use ipc::sys::{self, OBJ_CHANNEL, OBJ_FRAME, PERM_DEVICE, PERM_W, RIGHT_READ};
use loader::spawn;

static STORAGED_ELF: &[u8] = include_bytes!(env!("STORAGED_ELF_PATH"));
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
/// device again — there is deliberately no RTC driver.
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
    if secs < RTC_MIN_SANE_SECS || cntfrq == 0 {
        sys::debug_write(b"[init] FAILED: insane PL031/CNTFRQ read\n");
        sys::exit();
    }
    // The RTC's one-second granularity leaves ±1 s absolute error in
    // wall_base. Polling for a tick edge would shrink it at the cost of
    // up to a second of boot latency — wrong trade for retention rules
    // denominated in hours. Accepted, not polled away (rev1§2.6).
    ((secs as i64) * 1_000_000_000, cntvct_base, cntfrq)
}

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

    let mut config = [0u8; 44];
    config[..4].copy_from_slice(b"SD02");
    config[4..12].copy_from_slice(&MMIO_VA.to_le_bytes());
    config[12..20].copy_from_slice(&DMA_VA.to_le_bytes());
    config[20..28].copy_from_slice(&dma_pa.to_le_bytes());
    config[28..36].copy_from_slice(&(DMA_PAGES * 4096).to_le_bytes());
    config[36..44].copy_from_slice(&TIME_VA.to_le_bytes());
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

    let mut sh_config = [0u8; 12];
    sh_config[..4].copy_from_slice(b"SH01");
    sh_config[4..12].copy_from_slice(&TIME_VA.to_le_bytes());
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

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[init] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}
