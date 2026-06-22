//! A spawn/reclaim test subject. Its whole world arrives via the rev1§5.1
//! startup convention: a bootstrap channel in cspace slot 0 whose first
//! queued message is `"ST01"` + a one-byte mode + the time-page VA the
//! parent mapped in (rev1§2.6). The mode selects how the program terminates,
//! so one binary witnesses every path the parent's reclaim loop must
//! handle:
//!
//!   mode 0xFF → fault (wild store to an unmapped address): suspended, not
//!               destroyed (rev1§5.3); the parent reads `faulted(...)`.
//!   mode 0xFE → panic: the runtime panic handler exits with STATUS_PANIC,
//!               so the parent reads `panicked`, not `exited(254)`.
//!   mode 0xFD → read the granted time page and confirm a sane UTC clock,
//!               printing `time-ok` / `time-bad`; proves the shell→child
//!               time grant (rev1§2.6) arrived and works, then exits(0).
//!   otherwise → `thread_exit(mode)`: the parent reads `exited(mode)`.
//!
//! It also probes its own `.bss` before writing it. `.bss` is never copied
//! from the ELF (it lies past `filesz`), so its bytes come *only* from the
//! kernel zeroing the frame at retype. When the parent reuses one donation
//! untyped across spawns (rev1§2.5), a kernel that skipped zeroing would let
//! child N+1 read child N's writes here — so a nonzero probe is a
//! cross-spawn leak, and `bss-clean` every iteration is the zeroing proof.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the crate builds as a host harness (Design decision 2,
// B15C): std and the default test `main` take over, the bare-metal items are
// gated out, and only the pure `parse_st01` decoder remains. Allow the
// dead-code / unused-import noise the boot-only items leave behind.
#![cfg_attr(test, allow(dead_code, unused_imports))]

use ipc::sys;

const BOOT_CHAN: u32 = 0;

/// 2020-01-01T00:00:00Z in UTC nanoseconds. A clock reading below this
/// means the time grant never arrived (page absent → 0) or is garbage —
/// init refuses to boot with an RTC older than this, so a real grant is
/// always well past it (rev1§2.6).
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

/// The parent→selftest startup block (rev1§5.1): magic `"ST01"`, a one-byte
/// mode, then (optionally) the time-page VA as a little-endian `u64`. A decode
/// of an untrusted-shaped message (rev1§2.7): a short or mis-magicked block is
/// *refused* into the safe default (mode `0`, no clock), never a panic. Total
/// over any byte slice — `len >= 5` guards the magic + mode, `len >= 13` guards
/// the VA.
#[derive(Debug, PartialEq)]
struct St01 {
    mode: u8,
    time_va: Option<u64>,
}

fn parse_st01(buf: &[u8]) -> St01 {
    let is_block = buf.len() >= 5 && &buf[..4] == b"ST01";
    let mode = if is_block { buf[4] } else { 0 };
    let time_va = if is_block && buf.len() >= 13 {
        Some(u64::from_le_bytes(buf[5..13].try_into().unwrap()))
    } else {
        None
    };
    St01 { mode, time_va }
}

#[cfg(not(test))]
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
    let St01 { mode, time_va } = parse_st01(&buf[..len]);
    // The "time" grant (rev1§2.6): the parent mapped the read-only time page
    // into us and put its VA in the block. Attach so `urt::time` can read
    // the clock. Absent (short block) → no clock, mode 0xFD reports it.
    if let Some(time_va) = time_va {
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

#[cfg(not(test))]
#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[selftest] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}

#[cfg(test)]
mod tests {
    //! B15C — host tests for the ST01 startup-block decoder (rev1§6 Baseline
    //! tier). The decode must be total: a short / mis-magicked block falls
    //! back to the safe default (mode `0`, no clock), never a panic (rev1§2.7).
    use super::*;
    use proptest::prelude::*;

    /// Build an ST01 block: magic + mode, plus the time VA when `time_va` is set.
    fn st01(mode: u8, time_va: Option<u64>) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"ST01");
        b.push(mode);
        if let Some(va) = time_va {
            b.extend_from_slice(&va.to_le_bytes());
        }
        b
    }

    #[test]
    fn parse_st01_full_block_yields_mode_and_time() {
        let b = st01(0xFD, Some(0xA300_0000));
        assert_eq!(b.len(), 13);
        assert_eq!(
            parse_st01(&b),
            St01 {
                mode: 0xFD,
                time_va: Some(0xA300_0000)
            }
        );
    }

    #[test]
    fn parse_st01_five_byte_block_is_mode_only() {
        // A 5-byte block: mode present, no time VA (len < 13) — current
        // behaviour pinned (mode runs, the clock simply never attaches).
        let b = st01(0x07, None);
        assert_eq!(b.len(), 5);
        assert_eq!(
            parse_st01(&b),
            St01 {
                mode: 0x07,
                time_va: None
            }
        );
    }

    #[test]
    fn parse_st01_short_or_wrong_magic_is_default() {
        // len < 5 → mode 0 / no time (refuse-not-crash).
        assert_eq!(
            parse_st01(&[]),
            St01 {
                mode: 0,
                time_va: None
            }
        );
        assert_eq!(
            parse_st01(b"ST0"),
            St01 {
                mode: 0,
                time_va: None
            }
        );
        // Wrong magic but long enough → still the safe default, no attach.
        let mut wrong = st01(0xFF, Some(1));
        wrong[3] = b'2'; // "ST02"
        assert_eq!(
            parse_st01(&wrong),
            St01 {
                mode: 0,
                time_va: None
            }
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Total over arbitrary bytes: `parse_st01` never panics (rev1§2.7 floor).
        #[test]
        fn parse_st01_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..32)) {
            let _ = parse_st01(&bytes);
        }

        /// A 13-byte ST01 block round-trips mode + time VA.
        #[test]
        fn parse_st01_round_trips_full_block(mode in any::<u8>(), va in any::<u64>()) {
            prop_assert_eq!(
                parse_st01(&st01(mode, Some(va))),
                St01 { mode, time_va: Some(va) }
            );
        }
    }
}
