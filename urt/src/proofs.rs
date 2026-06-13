//! Kani harnesses for `urt` (plan §4.7). Two subjects:
//!
//! - **`slots`** — the cspace-slot bitmap free-list: every `alloc` hands out a
//!   distinct in-range slot, exhaustion is exact, and a freed slot is reusable
//!   (never double-allocated, never lost).
//! - **`time`** — the overflow-safe tick→ns conversion: total (no panic) for
//!   *all* page contents and counter values, and monotone in the counter (the
//!   naive `Δ·10⁹` overflow that proptest catches probabilistically becomes a
//!   proof).
//!
//! The `time` **seqlock reader** (`TimePage::sample`) is a *concurrency*
//! property — no torn read under a racing writer — which plan §1 assigns to
//! the Loom/Shuttle tier, not Kani (sequential). It stays owned by the
//! existing proptest `torn_writes_are_never_observed` (a real tearing-writer
//! thread); Kani is not the right tool and does not harness it.

#![cfg(kani)]

use crate::slots::SlotAlloc;
use crate::time::Sample;

const BASE: u32 = 100;
const CAP: usize = 4;

/// Draining the allocator hands out exactly `cap` distinct, in-window slots,
/// then stays exhausted — no double-allocation, no over-allocation.
#[kani::proof]
#[kani::unwind(6)]
fn check_slots_alloc_unique() {
    let mut a = SlotAlloc::<1>::new(BASE, CAP);
    let mut got = [0u32; CAP];
    let mut n = 0usize;
    while let Some(s) = a.alloc() {
        assert!(s >= BASE && (s - BASE) < CAP as u32); // in the window
        let mut j = 0;
        while j < n {
            assert!(got[j] != s); // distinct from every prior allocation
            j += 1;
        }
        assert!(n < CAP); // never over-allocates
        got[n] = s;
        n += 1;
    }
    assert!(n == CAP); // hands out exactly cap slots
    assert!(a.alloc().is_none()); // stays exhausted
}

/// A freed slot is handed back out: drain, free a nondet one (then the only
/// free bit), and the next `alloc` returns exactly it.
#[kani::proof]
#[kani::unwind(6)]
fn check_slots_free_reuse() {
    let mut a = SlotAlloc::<1>::new(BASE, CAP);
    let mut got = [0u32; CAP];
    let mut n = 0usize;
    while let Some(s) = a.alloc() {
        got[n] = s;
        n += 1;
    }
    assert!(n == CAP);
    let i: usize = kani::any();
    kani::assume(i < CAP);
    a.free(got[i]);
    assert!(a.alloc() == Some(got[i])); // the lone free slot comes back
}

/// `free`'s double-free contract (`debug_assert!(!is_free)`) fires: a fresh
/// allocator starts all-free, so freeing any slot is a double free.
#[kani::proof]
#[kani::should_panic]
fn check_slots_double_free() {
    let mut a = SlotAlloc::<1>::new(BASE, CAP);
    a.free(BASE);
}

/// `utc_ns_at` is total: no panic / overflow for any page contents or counter
/// value (the u128 decomposition + saturation hold for all of `u64⁴`). This is
/// the proof the §4.7 row highlights — the naive `Δ·10⁹` overflow at ~5 min of
/// uptime, which proptest catches only probabilistically, here can't happen
/// for *any* input. Totality only needs no-overflow, so CBMC need not reason
/// about the division *result* — it terminates in well under a second.
///
/// **Monotonicity is deliberately not a Kani harness.** Proving `c1 ≤ c2 ⇒
/// utc_ns_at(c1) ≤ utc_ns_at(c2)` forces CBMC to relate two u128 *divisions*;
/// with a symbolic `cntfrq` that is outright intractable, and even with a
/// concrete frequency and a bounded counter it did not terminate in many
/// minutes (see `doc/results/8_kani-findings-7.md`, the SOLVER note). It stays
/// owned by the proptest `conversion_is_monotone` — the §4.7 "supplementary"
/// line: Kani takes the overflow proof, proptest keeps the order property.
#[kani::proof]
fn check_time_conversion_total() {
    let s = Sample {
        wall_base_ns: kani::any(),
        cntvct_base: kani::any(),
        cntfrq: kani::any(),
    };
    let cntvct: u64 = kani::any();
    let _ = s.utc_ns_at(cntvct);
}
