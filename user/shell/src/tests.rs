//! B15B — host tests for the shell's pure non-I/O logic (rev1§6 Baseline tier).
//!
//! The shell binary is aarch64 `#![no_std] #![no_main]`; its syscall-/spawn-
//! bound runtime lives in `mod runtime` and is gated out of this (host) build
//! (`#[cfg(not(test))]`), validated instead by the QEMU boot smoke (Design
//! decision 3). What is host-tested here is the syscall-independent core in
//! `main.rs`: the date math (`civil_from_days`, `fmt_utc`), the byte
//! formatters (`fmt_num`/`fmt_num_pad`/`fmt_hex`), the parsers (`parse_path`,
//! `parse_u64`), the fault classifier (`fault_class`), and the retention
//! policy (`prune_victims`).
//!
//! The date properties are anchored by `days_from_civil` — the inverse
//! (days-from-civil) half of Howard Hinnant's algorithm, written independently
//! here so the round-trip is a real check, not a restatement of the code under
//! test. Its golden day numbers come from well-known UNIX epoch-day constants.

use super::*;
use proptest::prelude::*;
use storage_server::SnapInfo;

// The full u64-nanosecond range reaches ~year 2554 (≈ 213 503 days), so the
// year is always 4 digits and `fmt_utc` always emits the fixed 30-byte shape.
const MAX_DAYS: u64 = 213_503;

/// Days since 1970-01-01 for a civil (year, month, day) — the inverse of
/// [`civil_from_days`] (Howard Hinnant). Independent reference; valid for
/// `y >= 1970` (the only range `civil_from_days` produces for `days >= 0`).
fn days_from_civil(y: u64, m: u64, d: u64) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// Buffer-formatting helpers: run a pure `fmt_*` core into a fresh Vec.
fn num(n: u64) -> Vec<u8> {
    let mut b = Vec::new();
    fmt_num(&mut b, n);
    b
}
fn num_pad(n: u64, w: usize) -> Vec<u8> {
    let mut b = Vec::new();
    fmt_num_pad(&mut b, n, w);
    b
}
fn hex(n: u64) -> Vec<u8> {
    let mut b = Vec::new();
    fmt_hex(&mut b, n);
    b
}
fn utc(ns: u64) -> Vec<u8> {
    let mut b = Vec::new();
    fmt_utc(&mut b, ns);
    b
}

// ---------------------------------------------------------------------------
// civil_from_days — golden epoch-day constants + the inverse round-trip
// ---------------------------------------------------------------------------

#[test]
fn civil_from_days_golden() {
    // Well-known UNIX epoch-day numbers.
    assert_eq!(civil_from_days(0), (1970, 1, 1)); // the epoch
    assert_eq!(civil_from_days(10_957), (2000, 1, 1)); // ts 946684800 / 86400
    assert_eq!(civil_from_days(11_016), (2000, 2, 29)); // 2000 is a leap year
    assert_eq!(civil_from_days(24_855), (2038, 1, 19)); // the Y2038 day
    assert_eq!(civil_from_days(47_482), (2100, 1, 1)); // ts 4102444800 / 86400
    assert_eq!(civil_from_days(47_541), (2100, 3, 1)); // 2100 NOT leap: no Feb 29
}

#[test]
fn days_from_civil_reference_is_anchored() {
    // The reference's own anchors (so a bug in it cannot silently bless a bug
    // in `civil_from_days`).
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 1, 1), 10_957);
    assert_eq!(days_from_civil(2000, 2, 29), 11_016);
    assert_eq!(days_from_civil(2100, 3, 1), 47_541);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// civil-from-days is the exact inverse of days-from-civil across the whole
    /// u64-nanosecond-reachable range (covers far-future dates the goldens omit).
    #[test]
    fn civil_from_days_round_trips(days in 0u64..=MAX_DAYS) {
        let (y, m, d) = civil_from_days(days);
        prop_assert!((1..=12).contains(&m), "month out of range: {}", m);
        prop_assert!((1..=31).contains(&d), "day out of range: {}", d);
        prop_assert_eq!(days_from_civil(y, m, d), days);
    }
}

// ---------------------------------------------------------------------------
// fmt_utc — golden ISO-8601 strings + the fixed-shape / round-trip property
// ---------------------------------------------------------------------------

const SEC: u64 = 1_000_000_000; // nanoseconds per second

#[test]
fn fmt_utc_golden() {
    // The epoch.
    assert_eq!(utc(0), b"1970-01-01T00:00:00.000000000Z");
    // Sub-second precision only (full 9 nanosecond digits).
    assert_eq!(utc(123_456_789), b"1970-01-01T00:00:00.123456789Z");
    // Midnight on a round century anchor.
    assert_eq!(
        utc(10_957 * 86_400 * SEC),
        b"2000-01-01T00:00:00.000000000Z"
    );
    // Leap day, end-of-day, full fractional precision.
    let leap = (11_016 * 86_400 + 23 * 3600 + 59 * 60 + 59) * SEC + 987_654_321;
    assert_eq!(utc(leap), b"2000-02-29T23:59:59.987654321Z");
    // Noon on the non-leap century date (no Feb 29 in 2100).
    assert_eq!(
        utc((47_541 * 86_400 + 12 * 3600) * SEC),
        b"2100-03-01T12:00:00.000000000Z"
    );
}

/// Decimal value of a slice of ASCII digits (round-trip parser, independent of
/// the crate's `parse_u64`).
fn num_of(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |a, &c| a * 10 + (c - b'0') as u64)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// `fmt_utc` always emits the fixed `YYYY-MM-DDThh:mm:ss.nnnnnnnnnZ` shape
    /// (30 bytes, separators at fixed indices, digits elsewhere) and parses
    /// back to the input nanoseconds.
    #[test]
    fn fmt_utc_shape_and_round_trip(ns in any::<u64>()) {
        let s = utc(ns);
        prop_assert_eq!(s.len(), 30, "output: {:?}", core::str::from_utf8(&s));
        for (i, &c) in s.iter().enumerate() {
            let expect_sep = match i {
                4 | 7 => Some(b'-'),
                10 => Some(b'T'),
                13 | 16 => Some(b':'),
                19 => Some(b'.'),
                29 => Some(b'Z'),
                _ => None,
            };
            match expect_sep {
                Some(sep) => prop_assert_eq!(c, sep, "wrong separator at {}", i),
                None => prop_assert!(c.is_ascii_digit(), "non-digit at {}: {:?}", i, c as char),
            }
        }
        // Reconstruct the nanoseconds from the rendered fields.
        let y = num_of(&s[0..4]);
        let mo = num_of(&s[5..7]);
        let d = num_of(&s[8..10]);
        let h = num_of(&s[11..13]);
        let mi = num_of(&s[14..16]);
        let se = num_of(&s[17..19]);
        let frac = num_of(&s[20..29]);
        let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
        prop_assert_eq!(secs * SEC + frac, ns);
    }
}

// ---------------------------------------------------------------------------
// fmt_num / fmt_num_pad / fmt_hex — golden bytes
// ---------------------------------------------------------------------------

#[test]
fn fmt_num_golden() {
    assert_eq!(num(0), b"0");
    assert_eq!(num(7), b"7");
    assert_eq!(num(12_345), b"12345");
    assert_eq!(num(u64::MAX), b"18446744073709551615");
}

#[test]
fn fmt_num_pad_golden() {
    assert_eq!(num_pad(5, 2), b"05");
    assert_eq!(num_pad(59, 2), b"59");
    assert_eq!(num_pad(2026, 4), b"2026");
    assert_eq!(num_pad(0, 9), b"000000000");
    assert_eq!(num_pad(123_456_789, 9), b"123456789");
}

#[test]
fn fmt_hex_golden() {
    assert_eq!(hex(0), b"0");
    assert_eq!(hex(0xFF), b"ff");
    assert_eq!(hex(0xDEAD_BEEF), b"deadbeef");
    assert_eq!(hex(0xA300_0000), b"a3000000"); // the CHILD_TIME_VA constant
    assert_eq!(hex(u64::MAX), b"ffffffffffffffff");
}

// ---------------------------------------------------------------------------
// parse_path — goldens + invariants + idempotence
// ---------------------------------------------------------------------------

fn pp(s: &[u8]) -> Vec<Vec<u8>> {
    parse_path(s)
}

#[test]
fn parse_path_golden() {
    assert_eq!(pp(b""), Vec::<Vec<u8>>::new());
    assert_eq!(pp(b"/"), Vec::<Vec<u8>>::new());
    assert_eq!(pp(b"abc"), vec![b"abc".to_vec()]);
    assert_eq!(
        pp(b"a/b/c"),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    // Leading, trailing, and repeated slashes are all absorbed.
    assert_eq!(pp(b"//a///b/"), vec![b"a".to_vec(), b"b".to_vec()]);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// Every component is non-empty and `'/'`-free, and re-joining with `'/'`
    /// then re-parsing is a fixed point (the split is canonical).
    #[test]
    fn parse_path_components_and_idempotent(s in proptest::collection::vec(any::<u8>(), 0..32)) {
        let parsed = parse_path(&s);
        for c in &parsed {
            prop_assert!(!c.is_empty());
            prop_assert!(!c.contains(&b'/'));
        }
        let mut joined = Vec::new();
        for (i, c) in parsed.iter().enumerate() {
            if i > 0 {
                joined.push(b'/');
            }
            joined.extend_from_slice(c);
        }
        prop_assert_eq!(parse_path(&joined), parsed);
    }
}

// ---------------------------------------------------------------------------
// parse_u64 — goldens, rejects, and the format→parse round-trip
// ---------------------------------------------------------------------------

#[test]
fn parse_u64_golden_and_rejects() {
    assert_eq!(parse_u64(b"0"), Some(0));
    assert_eq!(parse_u64(b"12345"), Some(12_345));
    // The in-range boundary; one more digit would overflow (unguarded — see
    // the doc comment on `parse_u64`), so we pin the boundary, not the tail.
    assert_eq!(parse_u64(b"18446744073709551615"), Some(u64::MAX));

    assert_eq!(parse_u64(b""), None); // empty
    assert_eq!(parse_u64(b"12a"), None); // trailing non-digit
    assert_eq!(parse_u64(b"-5"), None); // sign
    assert_eq!(parse_u64(b"+5"), None);
    assert_eq!(parse_u64(b" 5"), None); // leading space
    assert_eq!(parse_u64(b"1 "), None); // trailing space
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// Every u64 round-trips through its decimal rendering.
    #[test]
    fn parse_u64_round_trips(v in any::<u64>()) {
        let mut digits = Vec::new();
        fmt_num(&mut digits, v);
        prop_assert_eq!(parse_u64(&digits), Some(v));
    }

    /// Any input containing a non-digit byte is rejected (never panics).
    #[test]
    fn parse_u64_rejects_nondigits(s in proptest::collection::vec(any::<u8>(), 1..16)) {
        if s.iter().any(|b| !b.is_ascii_digit()) {
            prop_assert_eq!(parse_u64(&s), None);
        }
    }
}

// ---------------------------------------------------------------------------
// fault_class — golden ESR_EL1 vectors (rev1§5.3)
// ---------------------------------------------------------------------------

/// Assemble an ESR_EL1 value from an exception class and data-fault-status code.
fn esr(ec: u64, dfsc: u64) -> u64 {
    (ec << 26) | dfsc
}

#[test]
fn fault_class_golden() {
    // Data abort from a lower EL (EC 0x24), per DFSC branch.
    assert_eq!(fault_class(esr(0x24, 0x00)), b"address-size");
    assert_eq!(fault_class(esr(0x24, 0x04)), b"translation");
    assert_eq!(fault_class(esr(0x24, 0x08)), b"access-flag");
    assert_eq!(fault_class(esr(0x24, 0x0C)), b"permission");
    assert_eq!(fault_class(esr(0x24, 0x11)), b"abort"); // 0x11 & 0x3C = 0x10
                                                        // The mask is 0x3C, so the low two DFSC bits (fault level) don't matter.
    assert_eq!(fault_class(esr(0x24, 0x07)), b"translation"); // 0x07 & 0x3C = 0x04

    // Instruction abort (0x20/0x24) and EL-variant ECs hit the same table.
    assert_eq!(fault_class(esr(0x20, 0x04)), b"translation");
    assert_eq!(fault_class(esr(0x21, 0x0C)), b"permission");
    assert_eq!(fault_class(esr(0x25, 0x08)), b"access-flag");

    // Any other exception class → the fallback.
    assert_eq!(fault_class(esr(0x15, 0)), b"exception"); // SVC
    assert_eq!(fault_class(esr(0x00, 0)), b"exception");
}

// ---------------------------------------------------------------------------
// prune_victims — retention policy (rev1§4.7)
// ---------------------------------------------------------------------------

fn snap(id: u64, class: u8) -> SnapInfo {
    SnapInfo {
        id,
        timestamp: 0,
        provenance: Vec::new(),
        message: Vec::new(),
        class,
    }
}

#[test]
fn prune_victims_golden() {
    // ids 1,3,4,5 are candidates (class != 0); id 2 is keep-class.
    let rows = [snap(1, 1), snap(2, 0), snap(3, 1), snap(4, 2), snap(5, 1)];
    assert_eq!(prune_victims(&rows, 2), vec![1, 3]); // keep newest 2 → delete oldest 2
    assert_eq!(prune_victims(&rows, 0), vec![1, 3, 4, 5]); // keep none → all candidates
    assert_eq!(prune_victims(&rows, 4), Vec::<u64>::new()); // exactly as many kept
    assert_eq!(prune_victims(&rows, 10), Vec::<u64>::new()); // keep more than exist
                                                             // A keep-class row is never selected even if it is the oldest.
    assert!(!prune_victims(&rows, 0).contains(&2));
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// `prune_victims` selects exactly the oldest excess non-keep rows: the
    /// count is `candidates.saturating_sub(keep_n)`, the selection is a prefix
    /// of the candidates (oldest-first), and no `keep`-class row is ever chosen.
    #[test]
    fn prune_victims_selects_oldest_excess(
        classes in proptest::collection::vec(0u8..4, 0..24),
        keep_n in 0u64..28,
    ) {
        // Distinct ids = positions, so a victim id maps back to one row.
        let rows: Vec<SnapInfo> =
            classes.iter().enumerate().map(|(i, &c)| snap(i as u64, c)).collect();
        let candidates: Vec<u64> = classes
            .iter()
            .enumerate()
            .filter(|(_, &c)| c != 0)
            .map(|(i, _)| i as u64)
            .collect();

        let victims = prune_victims(&rows, keep_n);

        // Count: keep the newest `keep_n` of the candidates.
        prop_assert_eq!(victims.len(), candidates.len().saturating_sub(keep_n as usize));
        // Selection is the oldest-first prefix of the candidates.
        prop_assert_eq!(&victims[..], &candidates[..victims.len()]);
        // No keep-class row is ever a victim.
        for &id in &victims {
            prop_assert_ne!(classes[id as usize], 0);
        }
        // Keeping at least as many as exist deletes nothing.
        if keep_n as usize >= candidates.len() {
            prop_assert!(victims.is_empty());
        }
    }
}

// ---------------------------------------------------------------------------
// Negative control (anti-theater): the date oracle must have teeth.
// ---------------------------------------------------------------------------

#[test]
fn reference_has_teeth() {
    // The days↔civil reference is the oracle for the date round-trips. It is
    // independently anchored (above), and it must *reject* a wrong date — so a
    // real `civil_from_days` regression cannot pass the round-trip silently.
    assert_eq!(days_from_civil(2000, 2, 29), 11_016);
    assert_ne!(days_from_civil(2000, 3, 1), 11_016); // a different civil date
    assert_ne!(civil_from_days(11_016), (2000, 3, 1)); // tampered expectation
                                                       // And `fmt_utc`'s output distinguishes a wrong rendering.
    assert_ne!(utc(0), b"1970-01-01T00:00:00.000000001Z");
}

// ---------------------------------------------------------------------------
// C1C — standard-name resolution (rev1§5.1). The shell resolves `storage`,
// `root`, and `time` from the init→shell `loader::startup` table; these pin the
// pure resolvers `runtime::_start` calls. The block is built and round-tripped
// through the *shared* codec, so the test drives the real decode the shell runs.
// ---------------------------------------------------------------------------

#[test]
fn resolve_names_golden() {
    use loader::startup::*;
    let mut s = Startup::new();
    s.push_grant(Grant {
        name: NAME_TIME,
        kind: GrantKind::Region {
            va: 0xA300_0000,
            len: 4096,
            pa: 0,
        },
    })
    .unwrap();
    s.push_grant(Grant {
        name: NAME_STORAGE,
        kind: GrantKind::CapSlot(1),
    })
    .unwrap();
    s.push_grant(Grant {
        name: NAME_ROOT,
        kind: GrantKind::StorageHandle(0),
    })
    .unwrap();
    s.push_grant(Grant {
        name: NAME_STDIN,
        kind: GrantKind::CapSlot(6),
    })
    .unwrap();
    // Round-trip through the shared codec — what init sends, the shell decodes.
    let mut buf = [0u8; MAX_BLOCK];
    let n = encode(&s, &mut buf).unwrap();
    let dec = decode(&buf[..n]).unwrap();
    assert_eq!(resolve_time_va(&dec), Some(0xA300_0000));
    assert_eq!(resolve_storage_slot(&dec), Some(1));
    assert_eq!(resolve_root_handle(&dec), Some(0));
    assert_eq!(resolve_stdin_slot(&dec), Some(6));
}

#[test]
fn resolve_names_absent_yields_none() {
    // An empty table (e.g. a degraded producer) → every name absent, so the
    // caller keeps its default (graceful degradation, rev1§5.1).
    let s = loader::startup::Startup::new();
    assert_eq!(resolve_time_va(&s), None);
    assert_eq!(resolve_storage_slot(&s), None);
    assert_eq!(resolve_root_handle(&s), None);
    // `stdin` absent → None; the runtime treats this as fatal (no fallback).
    assert_eq!(resolve_stdin_slot(&s), None);
}

#[test]
fn resolve_wrong_kind_yields_none() {
    // The oracle has teeth: a name delivered as the wrong kind is not resolvable
    // (a `storage` handle is not a cap slot; a `time` handle is not a region).
    use loader::startup::*;
    let mut s = Startup::new();
    s.push_grant(Grant {
        name: NAME_STORAGE,
        kind: GrantKind::StorageHandle(7),
    })
    .unwrap();
    s.push_grant(Grant {
        name: NAME_TIME,
        kind: GrantKind::CapSlot(9),
    })
    .unwrap();
    assert_eq!(resolve_storage_slot(&s), None);
    assert_eq!(resolve_time_va(&s), None);
}

// ---------------------------------------------------------------------------
// C1D — the shell→child startup-block producer (`build_child_block`). The codec
// is the shared `loader::startup`, so the producer's output is checked by the
// real `decode` — no mirrored hand-parser. The producer must be total
// (rev1§2.7): an over-arena / over-budget block returns `Err`, never a panic or
// silent truncation.
// ---------------------------------------------------------------------------

use loader::startup::{decode, GrantKind, NAME_TIME};

/// Encode a child block and decode it back, asserting it is well-formed.
fn child_block(time_va: u64, argv: &[&[u8]]) -> Vec<u8> {
    let mut out = [0u8; startup::MAX_BLOCK];
    let n = build_child_block(&mut out, time_va, argv).expect("within budget");
    out[..n].to_vec()
}

#[test]
fn build_child_block_round_trips_time_and_argv() {
    let bytes = child_block(0xA300_0000, &[b"bin/selftest", b"254"]);
    let s = decode(&bytes).expect("valid EUS1 block");
    // The TIME grant carries the pre-mapped time-page VA (the page length is
    // informational; the child reads only the VA).
    assert_eq!(
        s.grant(NAME_TIME),
        Some(GrantKind::Region {
            va: 0xA300_0000,
            len: TIME_LEN,
            pa: 0
        })
    );
    // argv round-trips in order: argv[0] the path, argv[1] selftest's mode.
    assert_eq!(s.nargv, 2);
    assert_eq!(s.argv[0], b"bin/selftest");
    assert_eq!(s.argv[1], b"254");
    // env is carried empty (defined, unpopulated — rev1§5.1, DD5).
    assert_eq!(s.nenv, 0);
}

#[test]
fn build_child_block_no_argv_is_just_the_time_grant() {
    // runloop's trivial child: argv = [path] only, no mode.
    let bytes = child_block(0xA300_0000, &[b"bin/selftest"]);
    let s = decode(&bytes).expect("valid EUS1 block");
    assert_eq!(s.nargv, 1);
    assert!(s.grant(NAME_TIME).is_some());
}

#[test]
fn build_child_block_refuses_over_arena() {
    // More argv entries than the arena holds (MAX_ARGV = 8) is refused cleanly,
    // not truncated — the spawn path maps this to RunErr::Startup.
    let many = [b"p" as &[u8]; 9];
    let mut out = [0u8; startup::MAX_BLOCK];
    assert!(build_child_block(&mut out, 0, &many).is_err());
}

#[test]
fn build_child_block_refuses_over_budget() {
    // A single argv string large enough to push the block past MAX_BLOCK (256):
    // 7-byte header + 26-byte TIME grant + (2 + 300) argv ≈ 335 bytes.
    let big = vec![b'x'; 300];
    let mut out = [0u8; startup::MAX_BLOCK];
    assert!(build_child_block(&mut out, 0, &[&big]).is_err());
}

#[test]
fn build_child_block_oracle_has_teeth() {
    // The decode oracle must distinguish argv — a tampered expectation fails.
    let bytes = child_block(0, &[b"prog", b"7"]);
    let s = decode(&bytes).unwrap();
    assert_ne!(s.argv[1], b"8");
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// Any time VA and any u8 mode (rendered as a decimal argv[1]) round-trip
    /// through encode→decode: the TIME grant returns the VA, argv[1] the digits.
    #[test]
    fn build_child_block_round_trips(time_va in any::<u64>(), mode in any::<u8>()) {
        let mode_s = format!("{mode}").into_bytes();
        let bytes = child_block(time_va, &[b"prog", &mode_s]);
        let s = decode(&bytes).unwrap();
        prop_assert_eq!(
            s.grant(NAME_TIME),
            Some(GrantKind::Region { va: time_va, len: TIME_LEN, pa: 0 })
        );
        prop_assert_eq!(s.argv[1], &mode_s[..]);
    }
}
