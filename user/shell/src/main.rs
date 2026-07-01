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

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

extern crate alloc;

use alloc::vec::Vec;
use loader::startup::{self, Grant, GrantKind};
use storage_server::SnapInfo;

#[cfg(not(test))]
mod runtime;

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

// ---------------------------------------------------------------------------
// Standard-name resolution (rev2§5.1). init delivers the shell's world as
// a `loader::startup` named-grant table; `runtime::_start` resolves each name
// once at boot. These are pure (no syscalls) so they are host-tested below; the
// `runtime` callers store the results and read them on every request. An absent
// or wrong-kind grant yields `None` — the caller keeps today's default
// (graceful degradation, e.g. `date` reports "no time grant").
// ---------------------------------------------------------------------------

/// `storage` → the cspace slot holding the storage-session channel.
fn resolve_storage_slot(s: &loader::startup::Startup) -> Option<u32> {
    match s.grant(loader::startup::NAME_STORAGE)? {
        loader::startup::GrantKind::CapSlot(slot) => Some(slot),
        _ => None,
    }
}

/// `root` → the storage handle number for the full-rights ref root.
fn resolve_root_handle(s: &loader::startup::Startup) -> Option<u32> {
    match s.grant(loader::startup::NAME_ROOT)? {
        loader::startup::GrantKind::StorageHandle(h) => Some(h),
        _ => None,
    }
}

/// `stdin` → the cspace slot holding the console-channel endpoint the shell
/// reads keystrokes from (rev2§5.1). The userspace console driver
/// owns the PL011 RX line, so there is no ambient input path, and an absent
/// grant is fatal (no silent `debug_getc` fallback — the driver would have
/// stolen the FIFO from under it); the caller refuses cleanly.
fn resolve_stdin_slot(s: &loader::startup::Startup) -> Option<u32> {
    match s.grant(loader::startup::NAME_STDIN)? {
        loader::startup::GrantKind::CapSlot(slot) => Some(slot),
        _ => None,
    }
}

/// `stdout` → the cspace slot holding the console-channel endpoint the shell
/// writes terminal output to (rev2§5.1). An interactive console is one
/// channel granted under both `stdin` and `stdout` (init points both names at
/// the same slot), so this resolves to the same endpoint as
/// [`resolve_stdin_slot`]. An absent grant is fatal — with the console driver
/// owning the UART there is no ambient `debug_write` path for user-facing I/O.
fn resolve_stdout_slot(s: &loader::startup::Startup) -> Option<u32> {
    match s.grant(loader::startup::NAME_STDOUT)? {
        loader::startup::GrantKind::CapSlot(slot) => Some(slot),
        _ => None,
    }
}

/// `time` → the virtual address of the read-only time page (rev2§2.6).
fn resolve_time_va(s: &loader::startup::Startup) -> Option<u64> {
    match s.grant(loader::startup::NAME_TIME)? {
        loader::startup::GrantKind::Region { va, .. } => Some(va),
        _ => None,
    }
}

/// The time page is one frame (kcore `PAGE`, rev2§2.6). The child reads only the
/// VA; the length is informational on the `REGION` grant (matches init's `TIME_LEN`).
pub(crate) const TIME_LEN: u64 = 4096;

/// Build the shell→child startup block (rev2§5.1): the unified `b"EUS1"`
/// format carrying a `TIME` `REGION` grant for the pre-mapped time page plus the
/// command-line `argv` (`argv[0]` is the program path). The codec is shared with
/// the consumer (selftest's `parse_startup`), so the round-trip drives the real
/// `encode`.
/// Total in the producer direction (rev2§2.7): an over-arena (`> MAX_ARGV`) or
/// over-budget (`> MAX_BLOCK`) block returns a clean `EncodeError` the spawn path
/// maps to a `RunErr` — refuse, never panic or silently truncate. `env` is left
/// empty (defined and round-tripped, unpopulated; rev2§5.1). The region carries
/// no new authority: the shell `map`s the time page before start, only the VA
/// travels.
pub(crate) fn build_child_block(
    out: &mut [u8],
    time_va: u64,
    argv: &[&[u8]],
    // std-port 3.2: for a thread-capable child, the child cspace slots holding its
    // self-aspace/self-cspace/thread-untyped caps and the base of its working-slot
    // range — emitted as `CapSlot` grants so `eunomia_sys::bootstrap` configures the
    // thread pool. `None` for a non-thread-capable child (no grants, least authority).
    thread_grants: Option<[u32; 4]>,
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
    for &a in argv {
        s.push_argv(a)?;
    }
    startup::encode(&s, out)
}

#[cfg(test)]
mod tests;
