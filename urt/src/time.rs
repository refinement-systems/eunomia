//! The time page (spec rev2§2.6): wall-clock time with zero syscalls.
//!
//! Init reads the PL031 RTC once at boot and publishes
//! `(seq, wall_base_ns, cntvct_base, cntfrq)` in a read-only frame mapped
//! into every process; wall time is then
//! `wall_base + (CNTVCT − cntvct_base)·10⁹ / cntfrq`, computable by any
//! process from two register reads and this page.
//!
//! `seq` is constant zero today — the page is write-once at boot — but the
//! reader ships seqlock-shaped anyway: if today's readers did plain loads
//! because "the page never changes", deferred clock setting (rev2§8) would
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
// The seqlock's atomics come through a single cfg-selected seam: `--cfg loom`
// (the certifying interleaving model) gets loom's instrumented atomics;
// `--cfg shuttle` (the non-certifying breadth-smoke) gets
// shuttle's; every normal/aarch64 build is unchanged. AtomicUsize is needed
// only by the `static TIME_PAGE` page-location cell, which is real-build-only
// (neither model can put an atomic in a `static`); the seqlock protocol both
// models check needs only the four field atomics + fence.
#[cfg(all(not(loom), not(shuttle)))]
use core::sync::atomic::{fence, AtomicI64, AtomicU64, AtomicUsize, Ordering};
#[cfg(loom)]
use loom::sync::atomic::{fence, AtomicI64, AtomicU64, Ordering};
#[cfg(shuttle)]
use shuttle::sync::atomic::{fence, AtomicI64, AtomicU64, Ordering};

// The reader's seqlock spin hint rides the same seam: loom's and shuttle's
// mocked spin_loop yield to their scheduler (a raw core::hint::spin_loop is
// opaque to them and blows loom's branch budget / never preempts under
// shuttle); native keeps the CPU hint.
#[cfg(all(not(loom), not(shuttle)))]
use core::hint::spin_loop;
#[cfg(loom)]
use loom::hint::spin_loop;
#[cfg(shuttle)]
use shuttle::hint::spin_loop;

// Verus: the `verus!{}` macro + ghost vocabulary for the host-side proof of the
// tick→ns conversion (`Sample::utc_ns_at`, below). Erases under every ordinary
// build, like slots.rs.
#[allow(unused_imports)]
use vstd::prelude::*;

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
// process's clock. Skipped under loom/shuttle, whose atomics wrap a model-state
// index and are neither 8 bytes nor ABI-stable — a model build never touches
// the real page layout (it shares a TimePage via the model's Arc).
#[cfg(all(not(loom), not(shuttle)))]
const _: () = {
    assert!(core::mem::size_of::<TimePage>() == PAGE_PREFIX_BYTES);
    assert!(core::mem::offset_of!(TimePage, seq) == 0);
    assert!(core::mem::offset_of!(TimePage, wall_base_ns) == 8);
    assert!(core::mem::offset_of!(TimePage, cntvct_base) == 16);
    assert!(core::mem::offset_of!(TimePage, cntfrq) == 24);
};

verus! {

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// One internally-consistent reading of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub wall_base_ns: i64,
    pub cntvct_base: u64,
    pub cntfrq: u64,
}

/// Clamp a ghost ns value into the `i64` range — the saturation the conversion
/// applies (unreachable in practice, but never wrap silently). Monotone.
spec fn clamp_i64(v: int) -> int {
    if v > i64::MAX as int {
        i64::MAX as int
    } else if v < i64::MIN as int {
        i64::MIN as int
    } else {
        v
    }
}

impl Sample {
    /// The denominator the conversion divides by: `cntfrq` floored to 1, so a
    /// corrupt zero-frequency page never divides by zero. Always `>= 1`.
    pub open spec fn freq(self) -> int {
        if self.cntfrq == 0 {
            1
        } else {
            self.cntfrq as int
        }
    }

    /// The saturating counter delta as a ghost int: an earlier counter than the
    /// boot baseline saturates to 0 (the exec `saturating_sub`).
    pub open spec fn delta_spec(self, cntvct: u64) -> int {
        if cntvct >= self.cntvct_base {
            cntvct as int - self.cntvct_base as int
        } else {
            0
        }
    }

    /// The unclamped ideal value: `wall_base + delta·10⁹ / f`, one mathematical
    /// division. The exec code computes this via an overflow-safe secs/frac
    /// decomposition, proven equal by [`lemma_decompose`]. `closed` because the
    /// body names the private `NANOS_PER_SEC` (a `pub open` body may only name
    /// public items); transparent to in-module proofs, opaque to callers.
    pub closed spec fn ideal_ns(self, cntvct: u64) -> int {
        self.wall_base_ns as int + (self.delta_spec(cntvct) * NANOS_PER_SEC as int) / self.freq()
    }

    /// The functional spec of [`Sample::utc_ns_at`]: the ideal value clamped to
    /// the `i64` range. `closed` (it calls the closed [`Sample::ideal_ns`]).
    pub closed spec fn result_spec(self, cntvct: u64) -> int {
        clamp_i64(self.ideal_ns(cntvct))
    }

    /// UTC nanoseconds at counter value `cntvct`.
    ///
    /// `(cntvct − base) · 10⁹` overflows u64 once the delta passes
    /// ~1.8×10¹⁰ ticks — about five minutes of uptime at QEMU virt's
    /// 62.5 MHz — so decompose: whole seconds scale safely for ~292 years
    /// of uptime, and the sub-second remainder is `< cntfrq`, kept exact
    /// by one u128 multiply-divide.
    ///
    /// **Verified by Verus**: `r == result_spec(cntvct)` — totality (no
    /// overflow/panic ∀ page contents and counter) *and* the functional value,
    /// from which `Sample::lemma_utc_ns_at_monotone` makes monotonicity a
    /// theorem (it turns on two u128 divisions, which the host proptests below
    /// only sample).
    pub fn utc_ns_at(&self, cntvct: u64) -> (r: i64)
        ensures
            r as int == self.result_spec(cntvct),
    {
        // cntfrq floored to 1 (corrupt page must not divide by zero). The old
        // `.max(1)` / `.saturating_sub(..)` are spelled as explicit branches so
        // Verus reasons about them (the std combinators are unspecced); the
        // exec behaviour is identical and the proptests below witness it.
        let f: u64 = if self.cntfrq == 0 {
            1
        } else {
            self.cntfrq
        };
        // The counter is monotone and cntvct_base was sampled at boot, so an
        // earlier cntvct is a caller bug; saturate to "boot time" rather than
        // wrapping into year ~2500.
        let delta: u64 = if cntvct >= self.cntvct_base {
            cntvct - self.cntvct_base
        } else {
            0
        };

        assert(f >= 1);
        let secs: u64 = delta / f;
        let m: u64 = delta % f;

        // secs·10⁹ ≤ u64::MAX·10⁹ ≪ i128::MAX, and m·10⁹ ≤ u64::MAX·10⁹ < u128::MAX
        // — the bounds that make the overflow-safe split's casts non-overflowing.
        proof {
            lemma_u128_frac_fits(m, f);
            lemma_secs_term_fits(secs);
        }

        let frac_ns: u128 = m as u128 * NANOS_PER_SEC as u128 / f as u128;

        // secs·10⁹ + frac_ns == (delta·10⁹)/f, so `total` is exactly ideal_ns.
        proof {
            lemma_decompose(delta, f);
        }

        let total: i128 = self.wall_base_ns as i128 + secs as i128 * NANOS_PER_SEC as i128
            + frac_ns as i128;

        // Saturation is ~year 2262 + centuries of uptime — unreachable with the
        // boot-time RTC sanity check, but never wrap silently.
        if total > i64::MAX as i128 {
            i64::MAX
        } else if total < i64::MIN as i128 {
            i64::MIN
        } else {
            total as i64
        }
    }

    /// Monotone in the counter: `c1 ≤ c2 ⇒ utc_ns_at(c1) ≤ utc_ns_at(c2)`. A
    /// clock that ran backwards between two reads would re-disorder everything
    /// the rev2§4.7 snapshot-timestamp clamp protects. Stated over [`Sample::result_spec`]
    /// (the value [`Sample::utc_ns_at`] returns), so the exec ordering follows.
    pub proof fn lemma_utc_ns_at_monotone(self, c1: u64, c2: u64)
        requires
            c1 <= c2,
        ensures
            self.result_spec(c1) <= self.result_spec(c2),
    {
        let n = NANOS_PER_SEC as int;
        let f = self.freq();
        // delta is monotone in the counter (both branches), so delta·10⁹ is too
        // (×n ≥ 0), and dividing by f > 0 preserves order — ideal_ns is monotone.
        assert(self.delta_spec(c1) <= self.delta_spec(c2));
        vstd::arithmetic::mul::lemma_mul_inequality(self.delta_spec(c1), self.delta_spec(c2), n);
        vstd::arithmetic::div_mod::lemma_div_is_ordered(
            self.delta_spec(c1) * n,
            self.delta_spec(c2) * n,
            f,
        );
        // ideal_ns(c1) ≤ ideal_ns(c2); clamp preserves order.
    }
}

/// The overflow-safe decomposition is exact: `secs·10⁹ + (m·10⁹)/f == (delta·10⁹)/f`
/// where `secs = delta/f`, `m = delta%f`. The crux — it relates two divisions.
/// Reduces to `lemma_hoist_over_denominator` once `delta` is split by
/// `lemma_fundamental_div_mod`.
proof fn lemma_decompose(delta: u64, f: u64)
    requires
        1 <= f,
    ensures
        (delta / f) as int * NANOS_PER_SEC as int + ((delta % f) as int * NANOS_PER_SEC as int) / (
        f as int) == (delta as int * NANOS_PER_SEC as int) / (f as int),
{
    let n = NANOS_PER_SEC as int;
    let d = delta as int;
    let ff = f as int;
    let q = d / ff;
    let r = d % ff;
    // d == q·ff + r.
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod(d, ff);
    // d·n == r·n + (q·n)·ff, so (d·n)/ff hoists q·n out of the denominator.
    assert(d * n == r * n + (q * n) * ff) by (nonlinear_arith)
        requires
            d == q * ff + r,
    ;
    vstd::arithmetic::div_mod::lemma_hoist_over_denominator(r * n, q * n, f as nat);
}

/// `m·10⁹` fits u128 (so the exec multiply does not overflow) for `m < f`.
proof fn lemma_u128_frac_fits(m: u64, f: u64)
    requires
        1 <= f,
    ensures
        m as u128 * NANOS_PER_SEC as u128 <= u128::MAX,
{
    assert(m as int <= u64::MAX as int);
    assert((u64::MAX as int) * (NANOS_PER_SEC as int) <= u128::MAX as int) by (compute);
    vstd::arithmetic::mul::lemma_mul_inequality(m as int, u64::MAX as int, NANOS_PER_SEC as int);
}

/// `secs·10⁹` is non-negative and bounded by `u64::MAX·10⁹` (≈1.8e28), which
/// fits i128 with vast headroom — so the exec i128 multiply and the `total` adds
/// never overflow (the clamp, not an overflow, handles the i64 saturation).
proof fn lemma_secs_term_fits(secs: u64)
    ensures
        0 <= secs as int * NANOS_PER_SEC as int,
        secs as int * NANOS_PER_SEC as int <= u64::MAX as int * NANOS_PER_SEC as int,
{
    vstd::arithmetic::mul::lemma_mul_nonnegative(secs as int, NANOS_PER_SEC as int);
    vstd::arithmetic::mul::lemma_mul_inequality(secs as int, u64::MAX as int, NANOS_PER_SEC as int);
}

} // verus!
impl TimePage {
    // loom's / shuttle's `Atomic*::new` is not `const`, so a model build drops
    // `const`; the body is identical. The real page is built once at boot, where
    // const construction is worth keeping.
    #[cfg(all(not(loom), not(shuttle)))]
    pub const fn new(wall_base_ns: i64, cntvct_base: u64, cntfrq: u64) -> TimePage {
        TimePage {
            seq: AtomicU64::new(0),
            wall_base_ns: AtomicI64::new(wall_base_ns),
            cntvct_base: AtomicU64::new(cntvct_base),
            cntfrq: AtomicU64::new(cntfrq),
        }
    }

    #[cfg(any(loom, shuttle))]
    pub fn new(wall_base_ns: i64, cntvct_base: u64, cntfrq: u64) -> TimePage {
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
                spin_loop();
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
                return Sample {
                    wall_base_ns,
                    cntvct_base,
                    cntfrq,
                };
            }
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

// The page-location indirection (a static atomic cell + an int->pointer cast)
// is real-build-only: neither model's atomics can live in a `static`, and this
// is not the seqlock protocol the models check — they construct a TimePage
// inside the model/run and share it via the model's Arc, never via this
// process-global pointer.
#[cfg(all(not(loom), not(shuttle)))]
static TIME_PAGE: AtomicUsize = AtomicUsize::new(0);

/// Register the time-page mapping for this process. The address comes
/// from the startup block (the `"time"` grant, rev2§5.1) — never a constant.
///
/// # Safety
/// `va` must be the base of a live `TimePage` mapping (read-only is
/// enough) that stays mapped for the rest of the process's life.
#[cfg(all(not(loom), not(shuttle)))]
pub unsafe fn attach(va: usize) {
    TIME_PAGE.store(va, Ordering::Release);
}

#[cfg(all(not(loom), not(shuttle)))]
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
/// syscall, no IPC (rev2§2.6).
///
/// Panics if no time page was attached: a process that asks for wall time
/// without holding the `"time"` grant is mis-wired, not degraded.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn now_utc_ns() -> i64 {
    let page = page().expect("time page not attached");
    page.sample().utc_ns_at(cntvct())
}

// The native tier: conversion proptests (breadth) + the probabilistic
// std-thread seqlock race (also the Miri weak-memory pass). Excluded under
// loom/shuttle, which construct their atomics inside the model/run — the
// std-thread test would build a TimePage outside one and panic. The exhaustive
// ordering proof lives in `loom_tests`, the randomized smoke in `shuttle_tests`
// below.
#[cfg(all(test, not(loom), not(shuttle)))]
mod tests {
    use super::*;
    extern crate std;
    use proptest::prelude::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[test]
    fn conversion_at_boot_is_wall_base() {
        let s = Sample {
            wall_base_ns: 1_700_000_000_000_000_000,
            cntvct_base: 12345,
            cntfrq: 62_500_000,
        };
        assert_eq!(s.utc_ns_at(12345), 1_700_000_000_000_000_000);
    }

    #[test]
    fn conversion_survives_the_five_minute_overflow() {
        // Six minutes at 62.5 MHz: delta·10⁹ has already overflowed u64.
        let f = 62_500_000u64;
        let s = Sample {
            wall_base_ns: 1_700_000_000_000_000_000,
            cntvct_base: 0,
            cntfrq: f,
        };
        let delta = 360 * f;
        assert_eq!(
            s.utc_ns_at(delta),
            1_700_000_000_000_000_000 + 360 * 1_000_000_000
        );
    }

    #[test]
    fn conversion_is_exact_at_one_tick() {
        let s = Sample {
            wall_base_ns: 0,
            cntvct_base: 0,
            cntfrq: 62_500_000,
        };
        assert_eq!(s.utc_ns_at(1), 16); // 1/62.5 MHz = 16 ns
    }

    #[test]
    fn earlier_counter_saturates_to_wall_base() {
        let s = Sample {
            wall_base_ns: 1_000,
            cntvct_base: 500,
            cntfrq: 1_000_000,
        };
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
        assert_eq!(
            p.sample(),
            Sample {
                wall_base_ns: 7,
                cntvct_base: 8,
                cntfrq: 9
            }
        );
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
        assert_eq!(
            s,
            Sample {
                wall_base_ns: iters,
                cntvct_base: 2 * iters as u64,
                cntfrq: (3 * iters + 1) as u64,
            }
        );
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
        /// two reads would re-disorder everything the rev2§4.7 clamp protects.
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

/// Loom proof of the seqlock protocol: under every interleaving of one writer's
/// odd→stagger→even update and one reader's `sample()` — enumerated *around the
/// explicit Acquire/Release fence* — the reader never observes a torn
/// `(k, 2k, 3k+1)` triple. Where the native `torn_writes_are_never_observed`
/// only *hopes* to hit a tear over 50k real races, Loom enumerates the schedules
/// — and flags a missing/weakened fence (a fence-removal negative control
/// confirms it).
///
/// Honest scope. This is **not** a proof over "every C11-permitted
/// reordering": Loom does **not** faithfully model Relaxed-atomic reordering, and
/// the data fields here are loaded/stored `Relaxed`. The seqlock's correctness
/// rests on the explicit `fence(Release)` (writer) / `fence(Acquire)` (reader,
/// `sample()`) pair, and the fence is exactly what Loom *does* model and
/// enumerate interleavings around — so the conclusion holds **via the fence**,
/// not via faithful Relaxed reordering. (Cf. the non-certifying Shuttle note
/// below, which reinterprets the same orderings as SeqCst.)
///
/// Bounded to a single write: the torn-read invariant is per-critical-section,
/// not cumulative, so one writer epoch over the initial one is the whole
/// proof, and it keeps Loom's state space small. Run with
/// `RUSTFLAGS="--cfg loom" cargo test -p urt`.
#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    #[test]
    fn no_torn_sample_under_any_interleaving() {
        loom::model(|| {
            // Initial epoch k=0: (wall, cntvct, cntfrq) = (0, 0, 1).
            let page = Arc::new(TimePage::new(0, 0, 1));

            let writer = {
                let page = Arc::clone(&page);
                thread::spawn(move || {
                    // One seqlock write to epoch k=1 → (1, 2, 4).
                    page.seq.fetch_add(1, Ordering::Relaxed); // odd: writer in
                    fence(Ordering::Release);
                    page.wall_base_ns.store(1, Ordering::Relaxed);
                    page.cntvct_base.store(2, Ordering::Relaxed);
                    page.cntfrq.store(4, Ordering::Relaxed);
                    page.seq.fetch_add(1, Ordering::Release); // even: writer out
                })
            };

            // The invariant `cntvct == 2·wall && cntfrq == 3·wall + 1` holds for
            // both epochs (0,0,1) and (1,2,4); any torn mix of the two breaks it.
            let s = page.sample();
            assert_eq!(
                s.cntvct_base,
                2 * s.wall_base_ns as u64,
                "torn sample: {s:?}"
            );
            assert_eq!(
                s.cntfrq,
                (3 * s.wall_base_ns + 1) as u64,
                "torn sample: {s:?}"
            );

            writer.join().unwrap();
        });
    }
}

/// Shuttle breadth-smoke of the same seqlock model — a *second* scheduler over
/// the same one-writer/one-reader interleavings, structurally identical to
/// `loom_tests`.
///
/// NON-CERTIFYING, and deliberately so: Shuttle models only SeqCst and
/// reinterprets the seqlock's Relaxed/Acquire/Release as SeqCst (it prints a
/// one-time warning to that effect on the first run), so it **cannot witness a
/// torn read** — under SeqCst the seqlock cannot tear. `loom_tests` is the proof
/// of record; this is a randomized-scheduler sanity pass (deadlock / retry-loop
/// / logic smoke) and the template for future IPC Shuttle work. Run with
/// `RUSTFLAGS="--cfg shuttle" cargo test -p urt --lib`.
#[cfg(all(test, shuttle))]
mod shuttle_tests {
    use super::*;
    use shuttle::sync::Arc;
    use shuttle::thread;

    #[test]
    fn no_torn_sample_under_random_schedules() {
        shuttle::check_random(
            || {
                // Initial epoch k=0: (wall, cntvct, cntfrq) = (0, 0, 1).
                let page = Arc::new(TimePage::new(0, 0, 1));

                let writer = {
                    let page = Arc::clone(&page);
                    thread::spawn(move || {
                        // One seqlock write to epoch k=1 → (1, 2, 4).
                        page.seq.fetch_add(1, Ordering::Relaxed); // odd: writer in
                        fence(Ordering::Release);
                        page.wall_base_ns.store(1, Ordering::Relaxed);
                        page.cntvct_base.store(2, Ordering::Relaxed);
                        page.cntfrq.store(4, Ordering::Relaxed);
                        page.seq.fetch_add(1, Ordering::Release); // even: writer out
                    })
                };

                let s = page.sample();
                assert_eq!(
                    s.cntvct_base,
                    2 * s.wall_base_ns as u64,
                    "torn sample: {s:?}"
                );
                assert_eq!(
                    s.cntfrq,
                    (3 * s.wall_base_ns + 1) as u64,
                    "torn sample: {s:?}"
                );

                writer.join().unwrap();
            },
            1000,
        );
    }
}
