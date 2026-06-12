//! Host-side scratchpad for verification-tool experiments (§6).
//!
//! Currently: a minimal Kani proof harness demonstrating the toolchain
//! works end-to-end (`cargo kani -p scratchpad`). The function under test
//! is deliberately one whose naive form — `(a + b) / 2` — fails the same
//! proof with a u8 overflow counterexample, so the harness is evidence
//! Kani is genuinely checking, not vacuously passing.

/// Midpoint of two u8, rounding down, without overflow.
pub fn midpoint(a: u8, b: u8) -> u8 {
    (a & b) + ((a ^ b) >> 1)
}

/// The midpoint lies between its inputs and `2*m` is within 1 of `a + b`.
pub fn meets_specification(a: u8, b: u8, m: u8) -> bool {
    m >= a.min(b) && m <= a.max(b) && (m as u16 * 2).abs_diff(a as u16 + b as u16) <= 1
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn check_midpoint() {
        let a: u8 = kani::any();
        let b: u8 = kani::any();

        let m = midpoint(a, b);

        assert!(meets_specification(a, b, m));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midpoint_examples() {
        assert_eq!(midpoint(0, 0), 0);
        assert_eq!(midpoint(2, 4), 3);
        assert_eq!(midpoint(255, 255), 255);
        assert_eq!(midpoint(0, 255), 127);
        assert!(meets_specification(7, 200, midpoint(7, 200)));
    }
}
