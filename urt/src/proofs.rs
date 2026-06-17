//! Kani harnesses for `urt` (plan §4.7). One remaining subject:
//!
//! - **`time`** — the overflow-safe tick→ns conversion: total (no panic) for
//!   *all* page contents and counter values, and monotone in the counter (the
//!   naive `Δ·10⁹` overflow that proptest catches probabilistically becomes a
//!   proof).
//!
//! The **`slots`** cspace-slot bitmap free-list moved to Verus (plan
//! `doc/plans/3_verus-rewrite_phase7-detail.md` §7c): every property the deleted
//! `check_slots_*` harnesses checked at the bounded CAP=4 is now an unbounded ∀
//! theorem on the real code — see `crate::slots` (`alloc`/`alloc_range`/`free`
//! contracts + the bit-frame lemmas). `time` stays on Kani until 7d.
//!
//! The `time` **seqlock reader** (`TimePage::sample`) is a *concurrency*
//! property — no torn read under a racing writer — which plan §1 assigns to
//! the Loom/Shuttle tier, not Kani (sequential). It stays owned by the
//! existing proptest `torn_writes_are_never_observed` (a real tearing-writer
//! thread); Kani is not the right tool and does not harness it.

#![cfg(kani)]

use crate::time::Sample;

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
