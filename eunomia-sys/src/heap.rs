//! The std `System` heap reservation size `N` (std-port 2.2).
//!
//! eunomia's `GlobalAlloc` arm is backed by a fixed `.bss` `urt::Heap<N>` (the
//! Verus-verified `freelist` algorithm). On the MVP there is no demand paging, so
//! the loader commits real frames for the whole `.bss` at spawn — `N` is therefore
//! a per-binary **reservation that equals committed RAM**, not a ceiling.
//!
//! `N` defaults to 1 MiB (matching the userspace shell's current heap) and is
//! overridable per binary at **compile time** via the `EUNOMIA_HEAP_BYTES` env,
//! threaded by `kernel/build.rs`. The override is parsed by a `const fn`, so a
//! malformed value is a build-time error, never a runtime one, and the arena size
//! is fixed before `.bss` is laid out.

/// The std `System` heap reservation in bytes. See the module doc.
///
/// `allow(dead_code)`: the only consumer is the `pal` arm's `urt::Heap<N>` static,
/// which is target-gated (`#![cfg(eunomia/none)]`), so a host build (`cargo test`
/// aside, where the test module below uses it) sees this const as unused.
#[allow(dead_code)]
pub(crate) const HEAP_BYTES: usize = match option_env!("EUNOMIA_HEAP_BYTES") {
    Some(s) => parse_dec(s),
    None => 1 << 20, // 1 MiB
};

/// Parse a non-empty decimal byte count at compile time. Panics in const-eval
/// (a build error) on an empty string, a non-digit, or a `usize` overflow — so a
/// bad `EUNOMIA_HEAP_BYTES` fails the build loudly rather than silently sizing the
/// heap wrong.
///
/// `allow(dead_code)`: reachable only via [`HEAP_BYTES`] (itself dead on a host
/// non-test build, see above) and the test module.
#[allow(dead_code)]
const fn parse_dec(s: &str) -> usize {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        panic!("EUNOMIA_HEAP_BYTES must be a non-empty decimal byte count");
    }
    let mut acc: usize = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            panic!("EUNOMIA_HEAP_BYTES must be decimal digits only");
        }
        let digit = (b - b'0') as usize;
        // Checked, so an overflowing value is a build error, not a wraparound.
        acc = match acc.checked_mul(10) {
            Some(v) => v,
            None => panic!("EUNOMIA_HEAP_BYTES overflows usize"),
        };
        acc = match acc.checked_add(digit) {
            Some(v) => v,
            None => panic!("EUNOMIA_HEAP_BYTES overflows usize"),
        };
        i += 1;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::{parse_dec, HEAP_BYTES};

    #[test]
    fn parses_plain_decimals() {
        assert_eq!(parse_dec("0"), 0);
        assert_eq!(parse_dec("1"), 1);
        assert_eq!(parse_dec("1048576"), 1 << 20);
        assert_eq!(parse_dec("4194304"), 4 << 20);
    }

    #[test]
    fn usable_in_const_context() {
        const N: usize = parse_dec("2048");
        assert_eq!(N, 2048);
    }

    #[test]
    fn default_is_one_mib_when_env_unset() {
        // The shipped default. Guarded so a build that *does* set the env (e.g. a
        // future per-binary override) still passes this test.
        if option_env!("EUNOMIA_HEAP_BYTES").is_none() {
            assert_eq!(HEAP_BYTES, 1 << 20);
        }
    }

    #[test]
    #[should_panic]
    fn rejects_empty() {
        parse_dec("");
    }

    #[test]
    #[should_panic]
    fn rejects_non_digits() {
        parse_dec("4k");
    }

    #[test]
    #[should_panic]
    fn rejects_overflow() {
        parse_dec("99999999999999999999999999999999");
    }
}
