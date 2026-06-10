//! init — the one process the kernel constructs (§1). Holds all initial
//! authority and wires the running system (§5.2: init is the only
//! binder): it spawns storaged (granting the virtio MMIO window, a DMA
//! region whose device address it reads via phys-read, and the session
//! channel) and the shell (granting the session's other end, an untyped
//! for spawning, and the console-by-syscall), each with an explicitly
//! constructed cspace and a §5.1 startup block.

#![no_std]
#![no_main]

use ipc::sys::{self, OBJ_CHANNEL, OBJ_FRAME, PERM_DEVICE, PERM_W};
use loader::spawn;

static STORAGED_ELF: &[u8] = include_bytes!(env!("STORAGED_ELF_PATH"));
static SHELL_ELF: &[u8] = include_bytes!(env!("SHELL_ELF_PATH"));

// Kernel-bestowed slots.
const UNTYPED: u32 = 0;
const UNTYPED2: u32 = 2;
const DEVICE_FRAME: u32 = 3;

// Our allocations.
const SD_BOOT_A: u32 = 4;
const SD_BOOT_B: u32 = 5;
const SH_BOOT_A: u32 = 6;
const SH_BOOT_B: u32 = 7;
const SESSION_A: u32 = 8; // storaged end
const SESSION_B: u32 = 9; // shell end
const DEV_COPY: u32 = 10;
const DMA_FRAME: u32 = 11;
const SD_NOTIF: u32 = 30;
const SD_SPAWN_BASE: u32 = 12;
const SH_SPAWN_BASE: u32 = 28;

const MMIO_VA: u64 = 0xA000_0000;
const DMA_VA: u64 = 0xA100_0000;
const DMA_PAGES: u64 = 64;

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

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    sys::debug_write(b"[init] wiring the system\n");

    check(sys::retype(UNTYPED, OBJ_CHANNEL, 4, SD_BOOT_A, SD_BOOT_B), b"sd boot chan");
    check(sys::retype(UNTYPED, OBJ_CHANNEL, 4, SH_BOOT_A, SH_BOOT_B), b"sh boot chan");
    check(sys::retype(UNTYPED, OBJ_CHANNEL, 4, SESSION_A, SESSION_B), b"session chan");

    // ── storaged ────────────────────────────────────────────────────
    let sd = match spawn::prepare(STORAGED_ELF, UNTYPED, SD_SPAWN_BASE, 8) {
        Ok(p) => p,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: prepare storaged\n");
            sys::exit();
        }
    };
    // The MMIO window: a phys-capable copy, device-mapped into the
    // child. The phys-read bit travels only along this one grant (§2.5).
    check(sys::cap_copy(DEVICE_FRAME, DEV_COPY, RIGHTS_WITH_PHYS), b"dev copy");
    check(sys::map(sd.aspace_slot, DEV_COPY, MMIO_VA, PERM_DEVICE | PERM_W), b"map mmio");
    // The DMA pool: ordinary RAM whose PA init reads and tells the
    // driver — the only place a PA crosses into userspace.
    check(sys::retype(UNTYPED, OBJ_FRAME, DMA_PAGES, DMA_FRAME, 0), b"dma frame");
    let dma_pa = check(sys::frame_paddr(DMA_FRAME), b"frame_paddr") as u64;
    check(sys::map(sd.aspace_slot, DMA_FRAME, DMA_VA, PERM_W), b"map dma");

    let mut config = [0u8; 36];
    config[..4].copy_from_slice(b"SD01");
    config[4..12].copy_from_slice(&MMIO_VA.to_le_bytes());
    config[12..20].copy_from_slice(&DMA_VA.to_le_bytes());
    config[20..28].copy_from_slice(&dma_pa.to_le_bytes());
    config[28..36].copy_from_slice(&(DMA_PAGES * 4096).to_le_bytes());
    check(sys::chan_send(SD_BOOT_A, &config, None), b"sd startup block");
    // Block-don't-spin: requests wake storaged through a readable→
    // notification binding (§3.6) — under strict priorities a busy-poll
    // server would starve its clients.
    check(sys::retype(UNTYPED, sys::OBJ_NOTIF, 0, SD_NOTIF, 0), b"sd notif");
    check(sys::chan_bind(SESSION_A, sys::EV_READABLE, SD_NOTIF, 1), b"sd bind");
    check(sys::cap_install(sd.cspace_slot, SD_BOOT_B, 0), b"sd boot install");
    check(sys::cap_install(sd.cspace_slot, SESSION_A, 1), b"sd session install");
    check(sys::cap_install(sd.cspace_slot, SD_NOTIF, 2), b"sd notif install");
    check(spawn::start(&sd, 5).map_or(-1, |_| 0), b"start storaged");

    // ── shell ───────────────────────────────────────────────────────
    let sh = match spawn::prepare(SHELL_ELF, UNTYPED, SH_SPAWN_BASE, 16) {
        Ok(p) => p,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: prepare shell\n");
            sys::exit();
        }
    };
    check(sys::chan_send(SH_BOOT_A, b"startup:shell", None), b"sh startup block");
    check(sys::cap_install(sh.cspace_slot, SH_BOOT_B, 0), b"sh boot install");
    check(sys::cap_install(sh.cspace_slot, SESSION_B, 1), b"sh session install");
    check(sys::cap_install(sh.cspace_slot, UNTYPED2, 2), b"sh untyped install");
    check(spawn::start(&sh, 4).map_or(-1, |_| 0), b"start shell");

    sys::debug_write(b"[init] system up\n");
    sys::exit()
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[init] PANIC\n");
    sys::exit()
}
