// SPDX-License-Identifier: 0BSD
//! The Eunomia shell (spec rev2§7): built-ins over a storage session.
//!
//! World (built by init, rev2§5.1): slot 0 = bootstrap channel (first message
//! is the `b"EUS1"` startup block — a `loader::startup` named-grant table from
//! which the shell resolves `time`/`storage`/`root`), slot 1 = storage session
//! (handle 0 = main ref root, full rights), slot 2 = untyped pool for spawning,
//! slot 5 = a read-only time-frame cap the shell re-grants to children. The
//! shell carves slot 3 (a persistent event notification) and slot 4 (a reusable
//! child donation untyped) from the pool, and keeps slots 8.. as a recyclable
//! cap window.
//!
//! `run`/`runloop` spawn a child from the store and reclaim it on exit
//! (rev2§5.1): one donation untyped per child, the whole subtree revoked and
//! the donation reset between spawns — so a process can be run, watched to
//! completion (exit *or* fault), reaped, and its memory and slots reused.
//!
//!   ls [path] · cat <path> · write <path> <text> · rm <path>
//!   snap [msg] · snaps · rollback <id> · sync · help
//!   run <path> [mode] · runloop <path> <count>          (rev2§5.1 spawn/reap)
//!   snapdel <id> · keep <id> · prune <n> · gc · df
//!   date                                              (time page, rev2§2.6)
//!
//! Structure: the syscall-/spawn-bound runtime lives in `runtime` and is
//! validated by the QEMU boot smoke; the pure formatting/parsing/policy logic
//! below is host-tested (rev2§6 Baseline tier). `runtime` is excluded from the
//! host test build because the shell's spawn/clock path depends on `urt::spawn`
//! and `urt::time::cntvct`, which are aarch64-bare-metal only.

// The shell is a std binary: std owns `_start`, the allocator (a
// `urt::Heap` sized by `EUNOMIA_HEAP_BYTES`), and the panic handler — so there is no
// `#![no_std]`/`#![no_main]`. The syscall-/spawn-bound `runtime` and the PAL↔seam bridge
// are target-only (`cfg(not(test))`), so a host `cargo test` builds just the pure logic
// below + `tests` against host std (the rev2§6 Baseline split is preserved).
// `extern crate eunomia_sys;` forces the seam rlib into the link so std's undefined
// `__eunomia_*` symbols resolve (the `__rust_alloc` pattern) on the eunomia target.
#[cfg(not(test))]
extern crate eunomia_sys;

extern crate alloc;

use alloc::vec::Vec;
use loader::startup::{self, Grant, GrantKind};
use storage_server::SnapInfo;

#[cfg(not(test))]
mod runtime;

/// The std entry: hand off to the target-only REPL runtime (which never returns). Under
/// a host `cargo test` this is `cfg`-excluded and the test harness supplies `main`.
#[cfg(not(test))]
fn main() {
    runtime::run()
}

// ---------------------------------------------------------------------------
// Pure, host-testable logic (rev2§6 Baseline tier). No syscalls, spawn,
// or clock — these compile and run on the host, so `cargo test --manifest-path
// user/shell/Cargo.toml` property-tests them directly. `runtime` calls them on
// the target; `tests` exercises them on the host. Formatting is split into a
// pure `fmt_*` core (writes bytes into a buffer) so the output is assertable;
// `runtime`'s `out_*` wrappers add the `sys::debug_write` sink.
// ---------------------------------------------------------------------------

/// Decimal `n` (no padding) appended to `buf`.
pub(crate) fn fmt_num(buf: &mut Vec<u8>, mut n: u64) {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    buf.extend_from_slice(&digits[i..]);
}

/// Zero-padded fixed-width decimal (date/time components) appended to `buf`.
/// If `n` has more than `width` digits only the last `width` are emitted — not
/// reached by [`fmt_utc`] (every field fits its width for the u64-ns range).
pub(crate) fn fmt_num_pad(buf: &mut Vec<u8>, mut n: u64, width: usize) {
    let mut digits = [b'0'; 20];
    let mut i = digits.len();
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    buf.extend_from_slice(&digits[digits.len() - width..]);
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's
/// civil-from-days. Valid for the whole u64-nanosecond range.
pub(crate) fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// UTC nanoseconds → ISO-8601 with nanosecond precision
/// (`2026-06-11T12:34:56.123456789Z`) appended to `buf`. All stored time is
/// UTC; timezones are presentation and this shell presents UTC only (rev2§2.6).
/// Full precision so per-ref strict ordering (rev2§4.7) is visible, not rounded
/// away — the RTC's whole-second base makes sub-second digits relative, not
/// absolute. For any u64 nanosecond the year is 4 digits (≤ 2554), so the
/// output is always the fixed 30-byte shape.
pub(crate) fn fmt_utc(buf: &mut Vec<u8>, ns: u64) {
    let secs = ns / 1_000_000_000;
    let (y, m, d) = civil_from_days(secs / 86_400);
    let tod = secs % 86_400;
    fmt_num_pad(buf, y, 4);
    buf.push(b'-');
    fmt_num_pad(buf, m, 2);
    buf.push(b'-');
    fmt_num_pad(buf, d, 2);
    buf.push(b'T');
    fmt_num_pad(buf, tod / 3600, 2);
    buf.push(b':');
    fmt_num_pad(buf, tod % 3600 / 60, 2);
    buf.push(b':');
    fmt_num_pad(buf, tod % 60, 2);
    buf.push(b'.');
    fmt_num_pad(buf, ns % 1_000_000_000, 9);
    buf.push(b'Z');
}

/// Split a path on `'/'`, dropping empty components (so leading, trailing, and
/// repeated slashes are absorbed). `cas` paths are `Vec<Vec<u8>>`.
///
/// on the eunomia target the shell's file built-ins now pass paths to
/// `std::fs`, which resolves them through the *verified* `eunomia_sys::path` resolver, so
/// this hand splitter is no longer on the target path. It is retained as the host-tested
/// reference for the rev2§4.9 path model (a follow-up shares the verified resolver with
/// it); `allow(dead_code)` covers its target-build orphaning while the tests keep it live.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_path(s: &[u8]) -> Vec<Vec<u8>> {
    s.split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(|c| c.to_vec())
        .collect()
}

/// Decimal parse with reject-on-nondigit and empty → `None`. Shell input, not
/// a wire decoder: no overflow guard (an over-`u64::MAX` digit string wraps in
/// release / panics under debug overflow-checks — out of practical reach for a
/// typed command).
pub(crate) fn parse_u64(arg: &[u8]) -> Option<u64> {
    if arg.is_empty() {
        return None;
    }
    let mut n = 0u64;
    for &b in arg {
        if !b.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (b - b'0') as u64;
    }
    Some(n)
}

/// Lowercase hex, no leading zeros (faulting addresses), appended to `buf`.
pub(crate) fn fmt_hex(buf: &mut Vec<u8>, n: u64) {
    let mut d = [0u8; 16];
    let mut v = n;
    for i in (0..16).rev() {
        d[i] = b"0123456789abcdef"[(v & 0xF) as usize];
        v >>= 4;
    }
    let start = d.iter().position(|&c| c != b'0').unwrap_or(15);
    buf.extend_from_slice(&d[start..]);
}

/// Classify a fault from ESR_EL1 (rev2§5.3): the EC names the kind of abort,
/// the low data-fault-status bits name why. Enough to print
/// `faulted(translation, …)` for the wild-pointer demo without a full ESR
/// table.
pub(crate) fn fault_class(esr: u64) -> &'static [u8] {
    let ec = (esr >> 26) & 0x3F;
    match ec {
        // Instruction / data abort from a lower EL.
        0x20 | 0x21 | 0x24 | 0x25 => match esr & 0x3C {
            0x00 => b"address-size",
            0x04 => b"translation",
            0x08 => b"access-flag",
            0x0C => b"permission",
            _ => b"abort",
        },
        _ => b"exception",
    }
}

/// Retention policy is shell-side (rev2§4.7: the server stores fields, it does
/// not interpret policy): the snapshot ids to delete to keep the newest
/// `keep_n` non-`keep` snapshots. `keep`-class rows (`class == 0`) and tagged
/// rows never appear among the candidates; the victims are the oldest excess,
/// in list order (the server returns rows oldest-first).
pub(crate) fn prune_victims(rows: &[SnapInfo], keep_n: u64) -> Vec<u64> {
    let candidates: Vec<u64> = rows.iter().filter(|r| r.class != 0).map(|r| r.id).collect();
    let excess = candidates.len().saturating_sub(keep_n as usize);
    candidates[..excess].to_vec()
}

// Standard-name resolution for the shell's *own* world (`storage`/`root`/`time`/
// `stdin`/`stdout`/`random_seed`) used to live here, but as a std binary the shell no
// longer resolves them by hand: `eunomia_sys::bootstrap` (via std) attaches the time
// page, seeds `urt::random`, connects the storaged session, and wires stdio over the
// console at bootstrap, and the storage root handle is init's convention (0). The
// child-facing producer `build_child_block` below (which *emits* these names to the
// children the shell spawns) is unchanged and stays host-tested.

/// The time page is one frame (kcore `PAGE`, rev2§2.6). The child reads only the
/// VA; the length is informational on the `REGION` grant (matches init's `TIME_LEN`).
pub(crate) const TIME_LEN: u64 = 4096;

/// Build the shell→child startup block (rev2§5.1): the unified `b"EUS1"`
/// format carrying a `TIME` `REGION` grant for the pre-mapped time page plus the
/// command-line `argv` (`argv[0]` is the program path). The codec is shared with
/// the consumer (selftest's `parse_startup`), so the round-trip drives the real
/// `encode`.
/// Total in the producer direction (rev2§2.7): an over-arena (`> MAX_ARGV`/
/// `> MAX_ENV`) or over-budget (`> MAX_BLOCK`) block returns a clean `EncodeError`
/// the spawn path maps to a `RunErr` — refuse, never panic or silently truncate.
/// `env` carries the shell's inherited environment, forwarded
/// verbatim to the child so its `std::env::vars()` is non-empty (POSIX inheritance).
/// The region carries no new authority: the shell `map`s the time page before start,
/// only the VA travels.
pub(crate) fn build_child_block(
    out: &mut [u8],
    time_va: u64,
    argv: &[&[u8]],
    // the environment inherited from init, forwarded to the child as
    // raw `KEY=VALUE` byte-strings (rev2§5.1). `push_env` refuses past `MAX_ENV` and
    // `encode` past `MAX_BLOCK`, both mapped to a clean spawn refusal.
    env: &[&[u8]],
    // for a thread-capable child, the child cspace slots holding its
    // self-aspace/self-cspace/thread-untyped caps and the base of its working-slot
    // range — emitted as `CapSlot` grants so `eunomia_sys::bootstrap` configures the
    // thread pool. `None` for a non-thread-capable child (no grants, least authority).
    thread_grants: Option<[u32; 4]>,
    // for an fs-capable child, the child cspace slot holding the
    // delegated storaged session — emitted as the `storage` grant, with the ref-root
    // at handle 0 as `root`, so its std `sys/fs` arm connects and serves files. `None`
    // for a non-fs child (no session, least authority).
    storage_slot: Option<u32>,
    // for a child the shell donated its console endpoint to, the child
    // cspace slot holding that endpoint — emitted under both `stdin` and `stdout` so its
    // std `sys/stdio` arm rides the `user/console` channel. stderr resolves to the
    // stdout channel in the child (the terminal case), so no separate `stderr` grant is
    // emitted. `None` for a child without a console (its stdio falls back to the
    // debug-log, its stdin reports EOF — least authority).
    console_slot: Option<u32>,
    // a fresh 256-bit sub-seed the shell drew from its own DRBG for
    // this child (the fork-without-reseed guard). The child seeds `urt::random`
    // from it, unblocking `HashMap`/`fill_bytes`.
    seed: [u64; 4],
) -> Result<usize, startup::EncodeError> {
    let mut s = startup::Startup::new();
    s.push_grant(Grant {
        name: startup::NAME_TIME,
        kind: GrantKind::Region {
            va: time_va,
            len: TIME_LEN,
            pa: 0,
        },
    })?;
    s.push_grant(Grant {
        name: startup::NAME_RANDOM_SEED,
        kind: GrantKind::Seed(seed),
    })?;
    if let Some(slot) = storage_slot {
        s.push_grant(Grant {
            name: startup::NAME_STORAGE,
            kind: GrantKind::CapSlot(slot),
        })?;
        s.push_grant(Grant {
            name: startup::NAME_ROOT,
            kind: GrantKind::StorageHandle(0),
        })?;
    }
    if let Some([self_aspace, self_cspace, thread_untyped, slot_base]) = thread_grants {
        for (name, slot) in [
            (startup::NAME_SELF_ASPACE, self_aspace),
            (startup::NAME_SELF_CSPACE, self_cspace),
            (startup::NAME_THREAD_UNTYPED, thread_untyped),
            (startup::NAME_THREAD_SLOT_BASE, slot_base),
        ] {
            s.push_grant(Grant {
                name,
                kind: GrantKind::CapSlot(slot),
            })?;
        }
    }
    // the donated console endpoint under both `stdin` and `stdout` (one
    // channel, the interactive-console convention). stderr is left to the child's
    // stdout-channel fallback, so a thread-capable child stays within `MAX_GRANTS`.
    if let Some(slot) = console_slot {
        for name in [startup::NAME_STDIN, startup::NAME_STDOUT] {
            s.push_grant(Grant {
                name,
                kind: GrantKind::CapSlot(slot),
            })?;
        }
    }
    for &a in argv {
        s.push_argv(a)?;
    }
    // the inherited environment, so the child's `std::env::vars` is
    // non-empty. Env lives in its own arena (`MAX_ENV`), separate from the grant
    // budget; every byte still counts against `MAX_BLOCK` (enforced by `encode`).
    for &e in env {
        s.push_env(e)?;
    }
    startup::encode(&s, out)
}

#[cfg(test)]
mod tests;
