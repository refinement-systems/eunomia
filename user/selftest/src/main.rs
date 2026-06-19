//! A spawn/reclaim test subject. Its whole world arrives via the rev0§5.1
//! startup convention: a bootstrap channel in cspace slot 0 whose first
//! queued message is `"ST01"` + a one-byte mode + the time-page VA the
//! parent mapped in (rev0§2.6). The mode selects how the program terminates,
//! so one binary witnesses every path the parent's reclaim loop must
//! handle:
//!
//!   mode 0xFF → fault (wild store to an unmapped address): suspended, not
//!               destroyed (rev0§5.3); the parent reads `faulted(...)`.
//!   mode 0xFE → panic: the runtime panic handler exits with STATUS_PANIC,
//!               so the parent reads `panicked`, not `exited(254)`.
//!   mode 0xFD → read the granted time page and confirm a sane UTC clock,
//!               printing `time-ok` / `time-bad`; proves the shell→child
//!               time grant (rev0§2.6) arrived and works, then exits(0).
//!   otherwise → `thread_exit(mode)`: the parent reads `exited(mode)`.
//!
//! It also probes its own `.bss` before writing it. `.bss` is never copied
//! from the ELF (it lies past `filesz`), so its bytes come *only* from the
//! kernel zeroing the frame at retype. When the parent reuses one donation
//! untyped across spawns (rev0§2.5), a kernel that skipped zeroing would let
//! child N+1 read child N's writes here — so a nonzero probe is a
//! cross-spawn leak, and `bss-clean` every iteration is the zeroing proof.

#![no_std]
#![no_main]

use ipc::sys;

const BOOT_CHAN: u32 = 0;

/// 2020-01-01T00:00:00Z in UTC nanoseconds. A clock reading below this
/// means the time grant never arrived (page absent → 0) or is garbage —
/// init refuses to boot with an RTC older than this, so a real grant is
/// always well past it (rev0§2.6).
const RTC_MIN_SANE_NS: i64 = 1_577_836_800_000_000_000;

/// Uninitialised .bss probe. Read before any write (below), so it reflects
/// the frame's state at spawn, not ours.
static mut BSS_PROBE: [u8; 4096] = [0; 4096];

/// Sample the probe at a few offsets with volatile reads so the compiler
/// can't assume the .bss-is-zero ABI and fold the check away.
fn probe_bss() -> u8 {
    let base = core::ptr::addr_of!(BSS_PROBE) as *const u8;
    let mut acc = 0u8;
    for off in [0usize, 1024, 2048, 4095] {
        // Safety: `off` is in-bounds of the 4096-byte static.
        acc |= unsafe { core::ptr::read_volatile(base.add(off)) };
    }
    acc
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    if probe_bss() == 0 {
        sys::debug_write(b"[selftest] bss-clean\n");
    } else {
        sys::debug_write(b"[selftest] BSS-LEAK\n");
    }
    // Dirty the probe: a kernel that fails to re-zero on reuse would leak
    // this to the next child carved from the same untyped.
    unsafe {
        let base = core::ptr::addr_of_mut!(BSS_PROBE) as *mut u8;
        for off in 0..4096usize {
            core::ptr::write_volatile(base.add(off), 0xA5);
        }
    }

    let mut buf = [0u8; 256];
    let len = loop {
        let (len, _) = sys::chan_recv(BOOT_CHAN, buf.as_mut_ptr(), None);
        if len >= 0 {
            break len as usize;
        }
        sys::yield_now();
    };
    let is_block = len >= 5 && &buf[..4] == b"ST01";
    let mode = if is_block { buf[4] } else { 0 };
    // The "time" grant (rev0§2.6): the parent mapped the read-only time page
    // into us and put its VA in the block. Attach so `urt::time` can read
    // the clock. Absent (short block) → no clock, mode 0xFD reports it.
    if is_block && len >= 13 {
        let time_va = u64::from_le_bytes(buf[5..13].try_into().unwrap());
        // Safety: the shell established this VA as a live read-only mapping
        // of the time page before queueing the block; it outlives us.
        unsafe { urt::time::attach(time_va as usize) };
    }

    if mode == 0xFD {
        // Read the granted clock and check it is a sane post-2020 UTC time:
        // the seqlock read + tick→ns math working in a spawned child, not
        // just in the shell, is the witness that the grant truly arrived.
        let ok = urt::time::page().is_some() && urt::time::now_utc_ns() > RTC_MIN_SANE_NS;
        sys::debug_write(if ok {
            b"[selftest] time-ok\n"
        } else {
            b"[selftest] time-bad\n"
        });
        sys::thread_exit(0);
    }

    if mode == 0xFF {
        sys::debug_write(b"[selftest] faulting\n");
        // Wild store to an unmapped low VA → translation fault. Volatile so
        // it isn't elided; the loop is unreachable (the fault suspends us).
        unsafe { core::ptr::write_volatile(0xdead_0000 as *mut u64, 0) };
        loop {
            core::hint::spin_loop();
        }
    }
    if mode == 0xFE {
        // Orderly panic: exercises the runtime panic path. The handler
        // exits with STATUS_PANIC, so the parent reads that, not exited(254)
        // — a panic can't pass for a clean stop.
        sys::debug_write(b"[selftest] panicking\n");
        panic!("selftest mode 0xFE");
    }
    sys::thread_exit(mode as u64)
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[selftest] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}
