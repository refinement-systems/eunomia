//! A spawn/reclaim test subject. Its whole world arrives via the rev2§5.1
//! startup convention: a bootstrap channel in cspace slot 0 whose first
//! queued message is the unified `b"EUS1"` startup block — a `TIME`
//! region grant for the time page the parent mapped in (rev2§2.6) plus an
//! `argv` vector. `argv[1]` (a decimal integer) selects how the program
//! terminates, so one binary witnesses every path the parent's reclaim loop
//! must handle:
//!
//!   mode 0xFF → fault (wild store to an unmapped address): suspended, not
//!               destroyed (rev2§5.3); the parent reads `faulted(...)`.
//!   mode 0xFE → panic: the runtime panic handler exits with STATUS_PANIC,
//!               so the parent reads `panicked`, not `exited(254)`.
//!   mode 0xFD → read the granted time page and confirm a sane UTC clock,
//!               printing `time-ok` / `time-bad`; proves the shell→child
//!               time grant (rev2§2.6) arrived and works, then exits(0).
//!   otherwise → `thread_exit(mode)`: the parent reads `exited(mode)`.
//!
//! It also probes its own `.bss` before writing it. `.bss` is never copied
//! from the ELF (it lies past `filesz`), so its bytes come *only* from the
//! kernel zeroing the frame at retype. When the parent reuses one donation
//! untyped across spawns (rev2§2.5), a kernel that skipped zeroing would let
//! child N+1 read child N's writes here — so a nonzero probe is a
//! cross-spawn leak, and `bss-clean` every iteration is the zeroing proof.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
// Under `cfg(test)` the crate builds as a host harness:
// std and the default test `main` take over, the bare-metal items
// are gated out, and only the pure `parse_startup` decoder remains. Allow the
// dead-code / unused-import noise the boot-only items leave behind.
#![cfg_attr(test, allow(dead_code, unused_imports))]

use ipc::sys;
use loader::startup::{self, GrantKind};

const BOOT_CHAN: u32 = 0;

/// 2020-01-01T00:00:00Z in UTC nanoseconds. A clock reading below this
/// means the time grant never arrived (page absent → 0) or is garbage —
/// init refuses to boot with an RTC older than this, so a real grant is
/// always well past it (rev2§2.6).
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

/// What selftest reads out of the startup block (rev2§5.1): the termination
/// `mode` (from `argv[1]`) and the time-page VA (from the `TIME` region grant,
/// `None` when no clock was granted). See [`parse_startup`] for the decode.
#[derive(Debug, PartialEq)]
struct Boot {
    mode: u8,
    time_va: Option<u64>,
}

/// Decode the unified `b"EUS1"` startup block, shared with the shell's
/// `build_child_block` producer via `loader::startup`. The mode is `argv[1]`
/// (a decimal integer); the clock is the `TIME` region grant's VA. Total over
/// arbitrary bytes (rev2§2.7): `decode` returns `None` on any malformed input,
/// so a non-EUS1 / short / mis-magicked block falls back to the safe default
/// (mode `0`, no clock) — never a panic.
fn parse_startup(buf: &[u8]) -> Boot {
    let Some(s) = startup::decode(buf) else {
        return Boot {
            mode: 0,
            time_va: None,
        };
    };
    let mode = if s.nargv > 1 {
        parse_mode(s.argv[1])
    } else {
        0
    };
    let time_va = match s.grant(startup::NAME_TIME) {
        Some(GrantKind::Region { va, .. }) => Some(va),
        _ => None,
    };
    Boot { mode, time_va }
}

/// Decimal byte-string → `u8`, mirroring the shell's old `parse_u64(..) as u8`
/// (empty / non-digit → `0`). Wrapping arithmetic so it is total on arbitrary
/// argv bytes — the modes used are `0..=255`, but the decoder must not panic.
fn parse_mode(s: &[u8]) -> u8 {
    if s.is_empty() || !s.iter().all(|b| b.is_ascii_digit()) {
        return 0;
    }
    let mut n: u64 = 0;
    for &b in s {
        n = n.wrapping_mul(10).wrapping_add((b - b'0') as u64);
    }
    n as u8
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
    let Boot { mode, time_va } = parse_startup(&buf[..len]);
    // The "time" grant (rev2§2.6): the parent mapped the read-only time page
    // into us and put its VA in the block's TIME region. Attach so `urt::time`
    // can read the clock. Absent (no grant) → no clock, mode 0xFD reports it.
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
    //! Host tests for the unified `b"EUS1"` startup-block decoder
    //! (rev2§6 Baseline tier). The codec is the shared `loader::startup`, so the
    //! consumer (`parse_startup`) is checked against the real `encode` — the
    //! same bytes the shell producer emits, no mirrored hand-parser. The decode
    //! must be total: a short / mis-magicked block falls back to the safe
    //! default (mode `0`, no clock), never a panic (rev2§2.7).
    use super::*;
    use proptest::prelude::*;

    /// Build an EUS1 block the way the shell does: a `TIME` region grant (when
    /// `time_va` is set) and an argv whose `argv[0]` is the path; `mode`, when
    /// present, is the decimal `argv[1]` selftest reads.
    fn block(mode: Option<u8>, time_va: Option<u64>) -> Vec<u8> {
        let mut s = startup::Startup::new();
        if let Some(va) = time_va {
            s.push_grant(startup::Grant {
                name: startup::NAME_TIME,
                kind: GrantKind::Region {
                    va,
                    len: 4096,
                    pa: 0,
                },
            })
            .unwrap();
        }
        s.push_argv(b"selftest").unwrap();
        let mode_s = mode.map(|m| format!("{m}").into_bytes());
        if let Some(ref m) = mode_s {
            s.push_argv(m).unwrap();
        }
        let mut out = [0u8; startup::MAX_BLOCK];
        let n = startup::encode(&s, &mut out).unwrap();
        out[..n].to_vec()
    }

    #[test]
    fn parse_startup_full_block_yields_mode_and_time() {
        assert_eq!(
            parse_startup(&block(Some(0xFD), Some(0xA300_0000))),
            Boot {
                mode: 0xFD,
                time_va: Some(0xA300_0000)
            }
        );
    }

    #[test]
    fn parse_startup_no_mode_arg_is_mode_zero() {
        // argv = [path] only (the runloop child): mode defaults to 0, clock
        // still attaches when the TIME grant is present.
        assert_eq!(
            parse_startup(&block(None, Some(0xA300_0000))),
            Boot {
                mode: 0,
                time_va: Some(0xA300_0000)
            }
        );
    }

    #[test]
    fn parse_startup_no_time_grant_is_clockless() {
        // A block without a TIME grant: mode runs, the clock simply never
        // attaches (mode 0xFD reports `time-bad`).
        assert_eq!(
            parse_startup(&block(Some(0x07), None)),
            Boot {
                mode: 0x07,
                time_va: None
            }
        );
    }

    #[test]
    fn parse_startup_malformed_is_default() {
        // Non-EUS1 / short / mis-magicked → mode 0, no clock (refuse-not-crash).
        let safe = Boot {
            mode: 0,
            time_va: None,
        };
        assert_eq!(parse_startup(&[]), safe);
        assert_eq!(parse_startup(b"ST01\x07"), safe); // the retired magic
        let mut wrong = block(Some(0xFF), Some(1));
        wrong[3] = b'2'; // "EUS2" — bad magic
        assert_eq!(parse_startup(&wrong), safe);
    }

    #[test]
    fn parse_mode_matches_old_truncation() {
        // The shell used `parse_u64(..) as u8`: decimal, empty/non-digit → 0,
        // values > 255 wrap to the low byte.
        assert_eq!(parse_mode(b"254"), 254);
        assert_eq!(parse_mode(b""), 0);
        assert_eq!(parse_mode(b"x9"), 0);
        assert_eq!(parse_mode(b"256"), 0); // 256 as u8
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]

        /// Total over arbitrary bytes: `parse_startup` never panics (rev2§2.7).
        #[test]
        fn parse_startup_is_total(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = parse_startup(&bytes);
        }

        /// An EUS1 block round-trips the mode (decimal argv[1]) and time VA.
        #[test]
        fn parse_startup_round_trips_full_block(mode in any::<u8>(), va in any::<u64>()) {
            prop_assert_eq!(
                parse_startup(&block(Some(mode), Some(va))),
                Boot { mode, time_va: Some(va) }
            );
        }
    }
}
