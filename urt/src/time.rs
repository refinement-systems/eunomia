//! The time page (spec §2.6): wall-clock time with zero syscalls.
//!
//! Init reads the PL031 RTC once at boot and publishes
//! `(seq, wall_base_ns, cntvct_base, cntfrq)` in a read-only frame mapped
//! into every process; wall time is then
//! `wall_base + (CNTVCT − cntvct_base)·10⁹ / cntfrq`, computable by any
//! process from two register reads and this page.
//!
//! `seq` is constant zero today — the page is write-once at boot — but the
//! reader ships seqlock-shaped anyway: if today's readers did plain loads
//! because "the page never changes", deferred clock setting (§8) would
//! mean updating every reader, the exact flag day the field was bought to
//! avoid. The protocol is the deliverable, not the integer.
//!
//! Page ABI (little-endian, offsets within the 4 KiB frame):
//!   [0..8)   seq          u64  (odd = writer mid-update)
//!   [8..16)  wall_base_ns i64  (UTC nanoseconds since the Unix epoch)
//!   [16..24) cntvct_base  u64
//!   [24..32) cntfrq       u64
//! The rest of the frame is reserved-zero; future fields append and are
//! covered by the same seq word.
//!
//! Fields are atomics rather than volatile: relaxed loads compile to plain
//! `ldr` on AArch64, the compiler cannot const-fold a "read-only" mapping,
//! and the host stress test (a writer thread tearing the page on purpose)
//! stays data-race-free under Miri.

use core::sync::atomic::{fence, AtomicI64, AtomicU64, AtomicUsize, Ordering};

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Byte length of the populated page prefix (the four u64-wide fields).
pub const PAGE_PREFIX_BYTES: usize = 32;

#[repr(C)]
pub struct TimePage {
    seq: AtomicU64,
    wall_base_ns: AtomicI64,
    cntvct_base: AtomicU64,
    cntfrq: AtomicU64,
}

// The struct is the page ABI; a layout drift here would misread every
// process's clock.
const _: () = {
    assert!(core::mem::size_of::<TimePage>() == PAGE_PREFIX_BYTES);
    assert!(core::mem::offset_of!(TimePage, seq) == 0);
    assert!(core::mem::offset_of!(TimePage, wall_base_ns) == 8);
    assert!(core::mem::offset_of!(TimePage, cntvct_base) == 16);
    assert!(core::mem::offset_of!(TimePage, cntfrq) == 24);
};

/// One internally-consistent reading of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub wall_base_ns: i64,
    pub cntvct_base: u64,
    pub cntfrq: u64,
}

impl TimePage {
    pub const fn new(wall_base_ns: i64, cntvct_base: u64, cntfrq: u64) -> TimePage {
        TimePage {
            seq: AtomicU64::new(0),
            wall_base_ns: AtomicI64::new(wall_base_ns),
            cntvct_base: AtomicU64::new(cntvct_base),
            cntfrq: AtomicU64::new(cntfrq),
        }
    }

    /// Seqlock read (Boehm's recipe: acquire-load seq, relaxed data loads,
    /// acquire fence, re-check seq). Today seq is always 0 and the loop
    /// runs exactly once; the retry path exists for deferred clock setting
    /// and is exercised by the host stress test.
    pub fn sample(&self) -> Sample {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                // Writer mid-update (future clock setting); spin, the
                // critical section is a handful of stores.
                core::hint::spin_loop();
                continue;
            }
            let wall_base_ns = self.wall_base_ns.load(Ordering::Relaxed);
            let cntvct_base = self.cntvct_base.load(Ordering::Relaxed);
            let cntfrq = self.cntfrq.load(Ordering::Relaxed);
            // The fence orders the relaxed data loads before the seq
            // re-read; without it the re-read could be satisfied first and
            // a torn sample would pass the check.
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Sample { wall_base_ns, cntvct_base, cntfrq };
            }
        }
    }
}

impl Sample {
    /// UTC nanoseconds at counter value `cntvct`.
    ///
    /// `(cntvct − base) · 10⁹` overflows u64 once the delta passes
    /// ~1.8×10¹⁰ ticks — about five minutes of uptime at QEMU virt's
    /// 62.5 MHz — so decompose: whole seconds scale safely for ~292 years
    /// of uptime, and the sub-second remainder is `< cntfrq`, kept exact
    /// by one u128 multiply-divide.
    pub fn utc_ns_at(&self, cntvct: u64) -> i64 {
        // Init validates cntfrq at boot; the guard keeps a corrupt page
        // from panicking every reader in the system.
        let f = self.cntfrq.max(1);
        // The counter is monotone and cntvct_base was sampled at boot, so
        // an earlier cntvct is a caller bug; saturate to "boot time"
        // rather than wrapping into year ~2500.
        let delta = cntvct.saturating_sub(self.cntvct_base);
        let secs = delta / f;
        let frac_ns = (delta % f) as u128 * NANOS_PER_SEC as u128 / f as u128;
        let total = self.wall_base_ns as i128
            + secs as i128 * NANOS_PER_SEC as i128
            + frac_ns as i128;
        // Saturation is ~year 2262 + centuries of uptime — unreachable
        // with the boot-time RTC sanity check, but never wrap silently.
        if total > i64::MAX as i128 {
            i64::MAX
        } else if total < i64::MIN as i128 {
            i64::MIN
        } else {
            total as i64
        }
    }
}

/// Boot-time page image for init's `frame_write`: the seq word is zero
/// (write-once page), fields at the ABI offsets above, LE.
pub fn encode_boot(wall_base_ns: i64, cntvct_base: u64, cntfrq: u64) -> [u8; PAGE_PREFIX_BYTES] {
    let mut buf = [0u8; PAGE_PREFIX_BYTES];
    buf[8..16].copy_from_slice(&wall_base_ns.to_le_bytes());
    buf[16..24].copy_from_slice(&cntvct_base.to_le_bytes());
    buf[24..32].copy_from_slice(&cntfrq.to_le_bytes());
    buf
}

static TIME_PAGE: AtomicUsize = AtomicUsize::new(0);

/// Register the time-page mapping for this process. The address comes
/// from the startup block (the `"time"` grant, §5.1) — never a constant.
///
/// # Safety
/// `va` must be the base of a live `TimePage` mapping (read-only is
/// enough) that stays mapped for the rest of the process's life.
pub unsafe fn attach(va: usize) {
    TIME_PAGE.store(va, Ordering::Release);
}

pub fn page() -> Option<&'static TimePage> {
    let p = TIME_PAGE.load(Ordering::Acquire);
    if p == 0 {
        None
    } else {
        // Safety: attach()'s contract — p is a live mapping for the
        // process lifetime.
        Some(unsafe { &*(p as *const TimePage) })
    }
}

/// The virtual counter — EL0-readable (CNTKCTL_EL1.EL0VCTEN, kernel timer
/// init). Init pairs this with the one-shot RTC read to form the page's
/// `cntvct_base`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn cntvct() -> u64 {
    let v: u64;
    // Safety: the register read has no side effects.
    unsafe { core::arch::asm!("mrs {v}, cntvct_el0", v = out(reg) v, options(nomem, nostack)) };
    v
}

/// The counter frequency, EL0-readable under the same CNTKCTL enable.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn cntfrq() -> u64 {
    let v: u64;
    // Safety: the register read has no side effects.
    unsafe { core::arch::asm!("mrs {v}, cntfrq_el0", v = out(reg) v, options(nomem, nostack)) };
    v
}

/// Current UTC nanoseconds — two register reads and a page read, no
/// syscall, no IPC (§2.6).
///
/// Panics if no time page was attached: a process that asks for wall time
/// without holding the `"time"` grant is mis-wired, not degraded.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn now_utc_ns() -> i64 {
    let page = page().expect("time page not attached");
    page.sample().utc_ns_at(cntvct())
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use proptest::prelude::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[test]
    fn conversion_at_boot_is_wall_base() {
        let s = Sample { wall_base_ns: 1_700_000_000_000_000_000, cntvct_base: 12345, cntfrq: 62_500_000 };
        assert_eq!(s.utc_ns_at(12345), 1_700_000_000_000_000_000);
    }

    #[test]
    fn conversion_survives_the_five_minute_overflow() {
        // Six minutes at 62.5 MHz: delta·10⁹ has already overflowed u64.
        let f = 62_500_000u64;
        let s = Sample { wall_base_ns: 1_700_000_000_000_000_000, cntvct_base: 0, cntfrq: f };
        let delta = 360 * f;
        assert_eq!(s.utc_ns_at(delta), 1_700_000_000_000_000_000 + 360 * 1_000_000_000);
    }

    #[test]
    fn conversion_is_exact_at_one_tick() {
        let s = Sample { wall_base_ns: 0, cntvct_base: 0, cntfrq: 62_500_000 };
        assert_eq!(s.utc_ns_at(1), 16); // 1/62.5 MHz = 16 ns
    }

    #[test]
    fn earlier_counter_saturates_to_wall_base() {
        let s = Sample { wall_base_ns: 1_000, cntvct_base: 500, cntfrq: 1_000_000 };
        assert_eq!(s.utc_ns_at(100), 1_000);
    }

    #[test]
    fn encode_boot_matches_the_page_abi() {
        let buf = encode_boot(-2, 3, 4);
        assert_eq!(&buf[0..8], &0u64.to_le_bytes());
        assert_eq!(&buf[8..16], &(-2i64).to_le_bytes());
        assert_eq!(&buf[16..24], &3u64.to_le_bytes());
        assert_eq!(&buf[24..32], &4u64.to_le_bytes());
    }

    #[test]
    fn write_once_page_reads_back() {
        let p = TimePage::new(7, 8, 9);
        assert_eq!(p.sample(), Sample { wall_base_ns: 7, cntvct_base: 8, cntfrq: 9 });
    }

    /// The retry path, exercised today even though the OS won't write the
    /// page for a long time: a writer tears the page on purpose (odd seq,
    /// staggered field stores), and no sample may ever mix two epochs.
    /// Fields are published as (k, 2k, 3k+1) so any torn combination
    /// violates the invariant.
    #[test]
    fn torn_writes_are_never_observed() {
        let iters: i64 = if cfg!(miri) { 50 } else { 50_000 };
        let page = Arc::new(TimePage::new(0, 0, 1));
        let done = Arc::new(AtomicBool::new(false));

        let writer = {
            let page = Arc::clone(&page);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for k in 1..=iters {
                    // Writer half of the seqlock: odd seq, release fence,
                    // staggered relaxed data stores, even seq with release.
                    page.seq.fetch_add(1, Ordering::Relaxed);
                    fence(Ordering::Release);
                    page.wall_base_ns.store(k, Ordering::Relaxed);
                    std::hint::black_box(()); // widen the torn window
                    page.cntvct_base.store(2 * k as u64, Ordering::Relaxed);
                    std::hint::black_box(());
                    page.cntfrq.store((3 * k + 1) as u64, Ordering::Relaxed);
                    page.seq.fetch_add(1, Ordering::Release);
                    if cfg!(miri) && k % 8 == 0 {
                        std::thread::yield_now();
                    }
                }
                done.store(true, Ordering::Release);
            })
        };

        // Sample-then-check-done (not while-not-done): the writer can
        // finish before this thread's first iteration on a loaded
        // machine, and the test must still take at least one sample.
        loop {
            let finished = done.load(Ordering::Acquire);
            let s = page.sample();
            let k = s.wall_base_ns;
            assert_eq!(s.cntvct_base, 2 * k as u64, "torn sample: {s:?}");
            assert_eq!(s.cntfrq, (3 * k + 1) as u64, "torn sample: {s:?}");
            if finished {
                break;
            }
            if cfg!(miri) {
                std::thread::yield_now();
            }
        }
        let s = page.sample();
        assert_eq!(s, Sample {
            wall_base_ns: iters,
            cntvct_base: 2 * iters as u64,
            cntfrq: (3 * iters + 1) as u64,
        });
        writer.join().unwrap();
    }

    fn reference_ns(s: &Sample, cntvct: u64) -> i64 {
        let f = s.cntfrq.max(1) as i128;
        let delta = cntvct.saturating_sub(s.cntvct_base) as i128;
        let total = s.wall_base_ns as i128 + delta * 1_000_000_000 / f;
        total.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full
        // sweep. Persistence is off under Miri — the lookup path calls
        // getcwd, which isolation forbids.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 512 },
            failure_persistence: if cfg!(miri) { None } else { ProptestConfig::default().failure_persistence },
            .. ProptestConfig::default()
        })]

        /// Frequencies from degenerate to architecturally maximal, deltas
        /// out past several decades of uptime: always exact vs the i128
        /// reference, and never panics.
        #[test]
        fn conversion_matches_wide_reference(
            wall_base_ns in -2_000_000_000_000_000_000i64..=2_500_000_000_000_000_000,
            f_idx in 0usize..4,
            cntvct_base in 0u64..=u64::MAX / 2,
            // Up to ~40 years of uptime at 1 GHz.
            delta in 0u64..=1_300_000_000_000_000_000,
        ) {
            let f = [1u64, 24_000_000, 62_500_000, 1_000_000_000][f_idx];
            let s = Sample { wall_base_ns, cntvct_base, cntfrq: f };
            let got = s.utc_ns_at(cntvct_base.saturating_add(delta));
            prop_assert_eq!(got, reference_ns(&s, cntvct_base.saturating_add(delta)));
        }

        /// Monotone in the counter — a clock that runs backwards between
        /// two reads would re-disorder everything the §4.7 clamp protects.
        #[test]
        fn conversion_is_monotone(
            wall_base_ns in -1_000_000_000_000_000_000i64..=2_000_000_000_000_000_000,
            f_idx in 0usize..4,
            cntvct_base in 0u64..=u64::MAX / 2,
            a in 0u64..=1_300_000_000_000_000_000,
            b in 0u64..=1_300_000_000_000_000_000,
        ) {
            let f = [1u64, 24_000_000, 62_500_000, 1_000_000_000][f_idx];
            let s = Sample { wall_base_ns, cntvct_base, cntfrq: f };
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(
                s.utc_ns_at(cntvct_base.saturating_add(lo))
                    <= s.utc_ns_at(cntvct_base.saturating_add(hi))
            );
        }

        /// Total: arbitrary (even insane) page contents never panic.
        #[test]
        fn conversion_is_total(
            wall_base_ns in any::<i64>(),
            cntvct_base in any::<u64>(),
            cntfrq in any::<u64>(),
            cntvct in any::<u64>(),
        ) {
            let s = Sample { wall_base_ns, cntvct_base, cntfrq };
            let _ = s.utc_ns_at(cntvct);
        }
    }
}
