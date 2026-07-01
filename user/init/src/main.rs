//! init — the one process the kernel constructs (rev2§1). Holds all initial
//! authority and wires the running system (rev2§5.2: init is the only
//! binder): it reads the PL031 once and publishes the time page (rev2§2.6),
//! spawns storaged (granting the virtio MMIO window, a DMA region whose
//! device address it reads via phys-read, the session channel, and the
//! time page) and the shell (granting the session's other end, an untyped
//! for spawning, the time page, and the console-by-syscall), each with an
//! explicitly constructed cspace and a rev2§5.1 startup block.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the crate builds as a host harness:
// std and the default test `main` take over, the bare-metal items and
// the ELF `include_bytes!`d at boot are gated out, and only the pure startup-
// block builders + the RTC sanity rule remain. Allow the dead-code / unused-
// import noise the boot-only items leave behind.
#![cfg_attr(test, allow(dead_code, unused_imports))]

use ipc::sys::{self, OBJ_CHANNEL, OBJ_FRAME, PERM_DEVICE, PERM_W, RIGHT_READ};
// The shared startup-block codec (rev2§5.1) — host-buildable, used by both
// `_start` (the producer) and the builder tests.
use loader::startup::{self, Grant, GrantKind};
// `loader::spawn` exists only on the bare-metal target (it is gated to
// `target_os = "none"`/`"eunomia"`), so the import is boot-only — gated out of
// the host test build alongside the `_start` that is its sole user.
#[cfg(not(test))]
use loader::spawn;

#[cfg(not(test))]
static STORAGED_ELF: &[u8] = include_bytes!(env!("STORAGED_ELF_PATH"));
#[cfg(not(test))]
static SHELL_ELF: &[u8] = include_bytes!(env!("SHELL_ELF_PATH"));
// The userspace PL011 console driver, spawned before the shell.
#[cfg(not(test))]
static CONSOLE_ELF: &[u8] = include_bytes!(env!("CONSOLE_ELF_PATH"));

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
// The fs client's session channel (std-port 4.1): storaged multiplexes it as a
// second session, and the shell delegates a copy to each fs-capable child. Both
// ends live in free init slots (19 sits below storaged's spawn scratch at 20; 29
// sits in its 27–29 margin).
const SESSION2_A: u32 = 19; // storaged end (installed at storaged cspace slot 3)
const SESSION2_B: u32 = 29; // shell end (delegatable, at shell cspace slot 7)
const SD_SPAWN_BASE: u32 = 20;
const SH_SPAWN_BASE: u32 = 40;

// ── the console driver ─────────────────────────────────────────────────────
// The PL011 console caps the kernel grants real init (`kernel/src/main.rs`):
// the MMIO frame and the IRQ-handler cap, in the contiguous top pair
// 62/63 — clear of every `spawn::prepare` scratch range by construction
// init delegates both to the console driver at spawn.
const CONSOLE_FRAME: u32 = 62;
const CONSOLE_IRQ: u32 = 63;
// init's working slots for the console spawn, in the free gap above storaged's
// spawn scratch (which reaches slot 26 at 3 segments — ≥3 slots of margin).
const CON_BOOT_A: u32 = 30; // init keeps this end to send the startup block
const CON_BOOT_B: u32 = 31; // the console's bootstrap channel (its slot 0)
const CON_A: u32 = 32; // console end of the console↔shell channel (its slot 1)
const CON_B: u32 = 33; // shell end of the console↔shell channel (shell slot 6)
const CON_NOTIF: u32 = 34; // the console's reactor/IRQ wake notif (its slot 2)
const CON_FRAME_COPY: u32 = 35; // R/W copy of CONSOLE_FRAME, mapped into console
/// The console's spawn scratch (`spawn::prepare`), in the free gap above the
/// shell's spawn scratch (which reaches slot 46 at 3 segments — ≥3 of margin);
/// the window (50..=50+3+nsegments) stays clear of the 62/63 grant.
const CON_SPAWN_BASE: u32 = 50;

/// Where init maps the PL011 register window in the console's aspace. The VA
/// travels in the startup block's `pl011-mmio` region grant (rev2§5.1) — the
/// driver never assumes it (the storaged virtio-mmio precedent).
const PL011_VA: u64 = 0xA000_0000;
/// The PL011 register window is a single 4 KiB frame; its length rides the
/// region grant (the driver reads fixed offsets and ignores the length).
const PL011_LEN: u64 = 4096;

/// The shell cspace slot holding its console-channel endpoint (rev2§5.1):
/// init `cap_install`s `CON_B` here and the startup table names it as **both**
/// `stdin` and `stdout` (the interactive console is one channel under both
/// names). Slots 0–5 are wired/carved by the shell and 8.. is its spawn window,
/// so 6 is free (the shell carves `EVENT_NOTIF`=3 and `DONATION`=4 from its pool).
const SHELL_CONSOLE_SLOT: u32 = 6;

const MMIO_VA: u64 = 0xA000_0000;
/// The virtio-mmio window storaged probes: 32 transports × 0x200 each. Carried
/// in the `virtio-mmio` region grant for completeness — storaged drives its own
/// probe loop and does not consume the length (it is informational).
const MMIO_LEN: u64 = 32 * 0x200;
/// The time page is a single frame (rev2§2.6); its length rides the `time`
/// region grant. storaged `attach`es by VA and ignores the length.
const TIME_LEN: u64 = 4096;
const DMA_VA: u64 = 0xA100_0000;
/// PL031 window in init's own aspace (the one self-mapping in the system).
const RTC_VA: u64 = 0xA200_0000;
/// Where the time page lands in every child; the address still travels in
/// the startup block (the `"time"` grant, rev2§5.1) — never assumed.
const TIME_VA: u64 = 0xA300_0000;
const DMA_PAGES: u64 = 64;

/// 2020-01-01T00:00:00Z. An RTC reading before this code's own era means
/// the device is absent or unbacked, not that it is 1970.
const RTC_MIN_SANE_SECS: u64 = 1_577_836_800;

const RIGHTS_WITH_PHYS: u64 = 0b111;

/// Is the one-shot RTC reading sane? A reading before this code's own era
/// (2020-01-01, `RTC_MIN_SANE_SECS`) or a zero counter frequency means the
/// device is absent/unbacked, not that it is 1970 — a boot failure, since
/// every store timestamp inherits this value (rev2§2.6).
fn rtc_sane(secs: u64, cntfrq: u64) -> bool {
    secs >= RTC_MIN_SANE_SECS && cntfrq != 0
}

/// Build the init→storaged startup block (rev2§5.1): the unified `b"EUS1"`
/// format carrying three `REGION` grants — the virtio MMIO window, the DMA pool
/// (with its device PA, the phys-read path rev2§2.5), and the time page
/// (rev2§2.6). storaged decodes the inverse (its `parse_config`); the codec is
/// shared on both ends. Returns
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
    seed: [u64; 4],
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
    // The per-run entropy seed (std-port 3.4): a fresh sub-seed for this child.
    // storaged keys directories in sorted prolly trees, not `HashMap`s (rev2§4.9),
    // so it does not consume it today — but every child is seeded uniformly.
    s.push_grant(Grant {
        name: startup::NAME_RANDOM_SEED,
        kind: GrantKind::Seed(seed),
    })?;
    startup::encode(&s, out)
}

/// Build the init→console startup block (rev2§5.1): the unified `b"EUS1"`
/// format carrying a single `REGION` grant — the PL011 register window at the VA
/// init pre-mapped it to. The driver decodes the inverse (its `region` helper) to
/// build its `MmioWindow`; the region carries no new authority (init `map`s the
/// frame before start, so only the VA travels). Returns the encoded length or a
/// clean `EncodeError` (the producer maps it to a boot failure — refuse, never
/// panic). The console needs no time page (it never reads the clock).
fn build_console_block(
    out: &mut [u8],
    pl011_va: u64,
    seed: [u64; 4],
) -> Result<usize, startup::EncodeError> {
    let mut s = startup::Startup::new();
    s.push_grant(Grant {
        name: startup::NAME_PL011_MMIO,
        kind: GrantKind::Region {
            va: pl011_va,
            len: PL011_LEN,
            pa: 0,
        },
    })?;
    // A fresh per-run entropy sub-seed (std-port 3.4); the console never uses it
    // (it does no hashing), but every child is seeded uniformly.
    s.push_grant(Grant {
        name: startup::NAME_RANDOM_SEED,
        kind: GrantKind::Seed(seed),
    })?;
    startup::encode(&s, out)
}

/// The shell's cspace slot holding the storage-session channel (rev2§5.1):
/// init `cap_install`s `SESSION_B` here and the startup table names it as the
/// `storage` grant, so the name and the install can never drift.
const SHELL_SESSION_SLOT: u32 = 1;

/// The shell's cspace slot holding the *fs client's* delegatable session channel
/// (std-port 4.1). Unlike `SHELL_SESSION_SLOT` (the shell's own session), this is
/// the second storaged session the shell hands to fs-capable children — the shell
/// copies it into each such child under the `storage` name (`build_child_block`).
/// Slot 7 is free (0 bootstrap, 1 storage, 2 pool, 5 time, 6 console; 3/4 carved).
const SHELL_FS_SESSION_SLOT: u32 = 7;

/// Build the init→shell startup block (rev2§5.1): the unified
/// `b"EUS1"` named-grant table (`loader::startup`) carrying the standard names the
/// shell holds — `time` (the read-only page init mapped at `TIME_VA`, as a region
/// grant carrying the VA), `storage` (the session channel at the shell's cspace
/// slot 1), `root` (the full-rights ref at handle 0 on that session), and
/// `stdin`/`stdout` (both name the one
/// console-channel endpoint at `SHELL_CONSOLE_SLOT`). `tmp` (no
/// subtree today) stays a reserved, unemitted name. Returns the encoded length, or
/// an `EncodeError` the caller maps to a clean boot failure (refuse-not-crash,
/// rev2§2.7) — never a panic.
fn build_shell_block(
    out: &mut [u8],
    seed: [u64; 4],
) -> Result<usize, loader::startup::EncodeError> {
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
    // The shell's per-run entropy seed (std-port 3.4): the shell seeds its own
    // DRBG from it and draws a fresh sub-seed for each child it spawns.
    s.push_grant(Grant {
        name: NAME_RANDOM_SEED,
        kind: GrantKind::Seed(seed),
    })?;
    s.push_grant(Grant {
        name: NAME_STORAGE,
        kind: GrantKind::CapSlot(SHELL_SESSION_SLOT),
    })?;
    s.push_grant(Grant {
        name: NAME_ROOT,
        kind: GrantKind::StorageHandle(0),
    })?;
    // `stdin`/`stdout`/`stderr` (rev2§5.1): all three
    // name the **same** console-channel endpoint in the shell's cspace — "an
    // interactive console is the same channel granted under both names". The
    // shell does all terminal I/O over this channel (input *and*
    // output); an absent grant is fatal in the shell (no debug-scaffold
    // fallback — the no-console negative control). `stderr` shares the endpoint
    // for a terminal (std-port 5.1); it is a distinct name so a pipeline can
    // route it elsewhere without folding it into `stdout`.
    s.push_grant(Grant {
        name: NAME_STDIN,
        kind: GrantKind::CapSlot(SHELL_CONSOLE_SLOT),
    })?;
    s.push_grant(Grant {
        name: NAME_STDOUT,
        kind: GrantKind::CapSlot(SHELL_CONSOLE_SLOT),
    })?;
    s.push_grant(Grant {
        name: NAME_STDERR,
        kind: GrantKind::CapSlot(SHELL_CONSOLE_SLOT),
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

/// One-shot PL031 read (rev2§2.6): map the RTC read-only into our own aspace,
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
    // device (rev2§2.6). Every timestamp in the store inherits this value —
    // fail loudly rather than seed them garbage.
    if !rtc_sane(secs, cntfrq) {
        sys::debug_write(b"[init] FAILED: insane PL031/CNTFRQ read\n");
        sys::exit();
    }
    // The RTC's one-second granularity leaves ±1 s absolute error in
    // wall_base. Polling for a tick edge would shrink it at the cost of
    // up to a second of boot latency — wrong trade for retention rules
    // denominated in hours. Accepted, not polled away (rev2§2.6).
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
    // The fs client's session channel (std-port 4.1): storaged holds SESSION2_A as a
    // second session; the shell holds SESSION2_B and delegates a copy to each
    // fs-capable child it spawns.
    check(
        sys::retype(UNTYPED, OBJ_CHANNEL, 4, SESSION2_A, SESSION2_B),
        b"fs session chan",
    );
    // The console's bootstrap channel (init→console startup block) and the
    // console↔shell channel (rev2§7's "console cap" — one bidirectional channel,
    // granted to the shell under both stdin/stdout).
    check(
        sys::retype(UNTYPED, OBJ_CHANNEL, 4, CON_BOOT_A, CON_BOOT_B),
        b"console boot chan",
    );
    check(
        sys::retype(UNTYPED, OBJ_CHANNEL, 4, CON_A, CON_B),
        b"console chan",
    );

    // ── the time page (rev2§2.6) ────────────────────────────────────────
    // Funded from init's untyped — the rev2§2.5 grant rule in its degenerate,
    // correct form: the supervisor whose liveness dominates everyone's
    // funds the mapping everyone shares, so nobody can fault anybody.
    let (wall_base_ns, cntvct_base, cntfrq) = read_boot_utc();
    // std-port 3.4: seed init's DRBG — the root of the entropy seed-tree. QEMU
    // `virt` offers no good entropy source (rev2§2.6), so this MVP seed is
    // deliberately *predictable* and *non-cryptographic*: it mixes the one-shot
    // RTC wall time with the boot CNTVCT/CNTFRQ. Each child then receives a
    // distinct DRBG-drawn sub-seed (`fresh_seed`), never this value raw; the real
    // entropy source is a deferred backend swap that moves only these bytes'
    // origin, leaving the per-child-reseed contract unchanged.
    urt::random::seed(urt::random::expand_seed(
        (wall_base_ns as u64) ^ cntvct_base.rotate_left(21) ^ cntfrq.rotate_left(43),
    ));
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
    // child. The phys-read bit travels only along this one grant (rev2§2.5).
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
    // The "time" grant (rev2§5.1): a read-only derivation per consumer —
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
        urt::random::fresh_seed(),
    ) {
        Ok(n) => n,
        // The block is built from fixed init constants, so an overflow would be
        // a build-time bug — but refuse cleanly rather than panic (rev2§2.7).
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
    // notification binding (rev2§3.6) — under strict priorities a busy-poll
    // server would starve its clients.
    check(
        sys::retype(UNTYPED, sys::OBJ_NOTIF, 0, SD_NOTIF, 0),
        b"sd notif",
    );
    check(
        sys::chan_bind(SESSION_A, sys::EV_READABLE, SD_NOTIF, 1),
        b"sd bind",
    );
    // The fs client's session wakes the same reactor on a distinct bit (std-port 4.1);
    // storaged's `reactor.register` re-affirms this bind and owns the bit→key mapping.
    check(
        sys::chan_bind(SESSION2_A, sys::EV_READABLE, SD_NOTIF, 2),
        b"sd fs bind",
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
    // storaged cspace slot 3 = the fs client's session channel (its SECOND_SESSION_CHAN).
    check(
        sys::cap_install(sd.cspace_slot, SESSION2_A, 3),
        b"sd fs session install",
    );
    check(spawn::start(&sd, 5).map_or(-1, |_| 0), b"start storaged");

    // ── the console driver ───────────────────────────────────────────
    // Spawned **before** the shell: the shell's first prompt
    // needs a live console to send to. The driver owns the PL011 (its MMIO frame
    // + IRQ cap, granted by the kernel at CONSOLE_FRAME/CONSOLE_IRQ); it delivers
    // RX keystrokes and accepts TX bytes over the console↔shell channel.
    let con = match spawn::prepare(CONSOLE_ELF, UNTYPED, CON_SPAWN_BASE, 8) {
        Ok(p) => p,
        Err(e) => {
            sys::debug_write(b"[init] FAILED: prepare console: ");
            match e {
                spawn::SpawnError::Elf(_) => sys::debug_write(b"elf\n"),
                spawn::SpawnError::Sys(_) => sys::debug_write(b"sys\n"),
                spawn::SpawnError::TooManySegments => sys::debug_write(b"segs\n"),
            };
            sys::exit();
        }
    };
    // The PL011 register window: a phys-capable copy, device-mapped into the
    // driver (the storaged virtio-mmio precedent — a `PERM_DEVICE` mapping of a
    // device frame needs the cap's PHYS right, the authority to map physical
    // device memory, even though the driver only ever reads the registers
    // through the VA). The VA travels in its startup block as a region grant.
    check(
        sys::cap_copy(CONSOLE_FRAME, CON_FRAME_COPY, RIGHTS_WITH_PHYS),
        b"console frame copy",
    );
    check(
        sys::map(
            con.aspace_slot,
            CON_FRAME_COPY,
            PL011_VA,
            PERM_DEVICE | PERM_W,
        ),
        b"map pl011",
    );
    // The console's wake notification: bound by the driver itself — `IrqBind`
    // raises it on an RX interrupt and the IPC reactor binds the channel-readable
    // event to the same notif — so init hands over a bare notif (no `chan_bind`).
    check(
        sys::retype(UNTYPED, sys::OBJ_NOTIF, 0, CON_NOTIF, 0),
        b"console notif",
    );
    let mut con_block = [0u8; startup::MAX_BLOCK];
    let con_len = match build_console_block(&mut con_block, PL011_VA, urt::random::fresh_seed()) {
        Ok(n) => n,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: build console block\n");
            sys::exit();
        }
    };
    check(
        sys::chan_send(CON_BOOT_A, &con_block[..con_len], None),
        b"console startup block",
    );
    // The console's cspace (its `_start` reads fixed slots): 0 = bootstrap
    // channel, 1 = the console↔shell channel, 2 = the wake notif, 3 = the PL011
    // IRQ cap (delegated from CONSOLE_IRQ).
    check(
        sys::cap_install(con.cspace_slot, CON_BOOT_B, 0),
        b"console boot install",
    );
    check(
        sys::cap_install(con.cspace_slot, CON_A, 1),
        b"console chan install",
    );
    check(
        sys::cap_install(con.cspace_slot, CON_NOTIF, 2),
        b"console notif install",
    );
    check(
        sys::cap_install(con.cspace_slot, CONSOLE_IRQ, 3),
        b"console irq install",
    );
    // Priority 6: above storaged (5) and the shell (4), so an interrupt-driven
    // keystroke preempts in-progress server work and reaches the shell promptly.
    // The driver blocks on its reactor otherwise, so it cannot starve them.
    check(spawn::start(&con, 6).map_or(-1, |_| 0), b"start console");

    // ── shell ───────────────────────────────────────────────────────
    // 64-slot cspace: slots 0-4 are wired below / carved by the shell,
    // slot 5 is the re-grantable time cap, and 8.. is the shell's
    // recyclable spawn window (rev2§5.1 reclaim loop).
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
    // extending the rev2§2.6 time grant one hop (init→shell→child, rev2§5.1).
    check(
        sys::cap_copy(TIME_FRAME, TIME_SH_CHILD, RIGHT_READ),
        b"time child copy",
    );
    check(
        sys::cap_install(sh.cspace_slot, TIME_SH_CHILD, 5),
        b"time child install",
    );

    let mut sh_config = [0u8; loader::startup::MAX_BLOCK];
    let sh_len = match build_shell_block(&mut sh_config, urt::random::fresh_seed()) {
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
    // The fs client's delegatable session (std-port 4.1): the shell holds it at slot 7
    // and copies it into each fs-capable child (never used for the shell's own I/O).
    check(
        sys::cap_install(sh.cspace_slot, SESSION2_B, SHELL_FS_SESSION_SLOT),
        b"sh fs session install",
    );
    check(
        sys::cap_install(sh.cspace_slot, UNTYPED2, 2),
        b"sh untyped install",
    );
    // The console-channel endpoint (rev2§7's "console cap"): named in the
    // startup table as both `stdin` and `stdout`. The shell holds it and does
    // its terminal I/O over it.
    check(
        sys::cap_install(sh.cspace_slot, CON_B, SHELL_CONSOLE_SLOT),
        b"sh console install",
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
    //! (rev2§6 Baseline tier). Both producer blocks use the shared
    //! `loader::startup` codec: the init→storaged and init→shell
    //! round-trips drive the real `encode` through the real `decode`, so no
    //! mirrored hand-parser exists on either side.
    use super::*;
    use proptest::prelude::*;

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
            [1, 2, 3, 4],
        )
        .unwrap();
        let s = startup::decode(&buf[..n]).unwrap();
        assert_eq!(s.ngrants, 4);
        assert_eq!(
            s.grant(startup::NAME_RANDOM_SEED),
            Some(GrantKind::Seed([1, 2, 3, 4]))
        );
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
        // The storaged block carries no argv/env (rev2§5.1 fields, empty here).
        assert_eq!(s.nargv, 0);
        assert_eq!(s.nenv, 0);
    }

    #[test]
    fn shell_block_carries_named_grants() {
        // The init→shell block is the unified `b"EUS1"` table; drive
        // the real shared codec on both ends (encode here, decode via
        // `loader::startup`) — no mirrored hand-parser.
        use loader::startup::*;
        let mut buf = [0u8; MAX_BLOCK];
        let n = build_shell_block(&mut buf, [7, 8, 9, 10]).expect("encode shell block");
        let s = decode(&buf[..n]).expect("decode shell block");
        // `random_seed`: the shell's per-run entropy sub-seed (rev2§5.1).
        assert_eq!(
            s.grant(NAME_RANDOM_SEED),
            Some(GrantKind::Seed([7, 8, 9, 10]))
        );
        // `time`: the read-only page init mapped at TIME_VA (rev2§2.6).
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
        // stdin/stdout/stderr: all three name the one console-channel endpoint
        // (rev2§5.1 "same channel under both names"; std-port 5.1 adds stderr for
        // a terminal). `tmp` stays unpopulated.
        assert_eq!(
            s.grant(NAME_STDIN),
            Some(GrantKind::CapSlot(SHELL_CONSOLE_SLOT))
        );
        assert_eq!(
            s.grant(NAME_STDOUT),
            Some(GrantKind::CapSlot(SHELL_CONSOLE_SLOT))
        );
        assert_eq!(
            s.grant(NAME_STDERR),
            Some(GrantKind::CapSlot(SHELL_CONSOLE_SLOT))
        );
        assert_eq!(s.grant(NAME_TMP), None);
    }

    #[test]
    fn console_block_carries_the_pl011_region() {
        // The init→console block carries exactly the PL011 register
        // window as a region grant — the driver builds its `MmioWindow` from the
        // VA. Drive the real shared codec (encode here, decode via the crate).
        let mut buf = [0u8; startup::MAX_BLOCK];
        let n = build_console_block(&mut buf, 0xA000_0000, [5, 6, 7, 8]).unwrap();
        let s = startup::decode(&buf[..n]).unwrap();
        assert_eq!(s.ngrants, 2);
        assert_eq!(
            s.grant(startup::NAME_PL011_MMIO),
            Some(GrantKind::Region {
                va: 0xA000_0000,
                len: PL011_LEN,
                pa: 0
            })
        );
        assert_eq!(
            s.grant(startup::NAME_RANDOM_SEED),
            Some(GrantKind::Seed([5, 6, 7, 8]))
        );
        // No argv/env, and no time page (the console never reads the clock).
        assert_eq!(s.nargv, 0);
        assert_eq!(s.nenv, 0);
        assert_eq!(s.grant(startup::NAME_TIME), None);
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
            let n = build_storaged_block(
                &mut buf, mmio_va, dma_va, dma_pa, dma_len, time_va, [0xAB; 4],
            ).unwrap();
            let s = startup::decode(&buf[..n]).unwrap();
            prop_assert_eq!(
                s.grant(startup::NAME_RANDOM_SEED),
                Some(GrantKind::Seed([0xAB; 4]))
            );
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
    }
}
