//! The Eunomia shell (spec §7): built-ins over a storage session.
//!
//! World (built by init, §5.1): slot 0 = bootstrap channel (first message
//! is the "SH01" startup block carrying the time-page address, §2.6),
//! slot 1 = storage session (handle 0 = main ref root, full rights),
//! slot 2 = untyped pool for spawning, slot 5 = a read-only time-frame
//! cap the shell re-grants to children. The shell carves slot 3 (a
//! persistent event notification) and slot 4 (a reusable child donation
//! untyped) from the pool, and keeps slots 8.. as a recyclable cap window.
//!
//! `run`/`runloop` spawn a child from the store and reclaim it on exit
//! (§5.1): one donation untyped per child, the whole subtree revoked and
//! the donation reset between spawns — so a process can be run, watched to
//! completion (exit *or* fault), reaped, and its memory and slots reused.
//!
//!   ls [path] · cat <path> · write <path> <text> · rm <path>
//!   snap [msg] · snaps · rollback <id> · sync · help
//!   run <path> [mode] · runloop <path> <count>          (§5.1 spawn/reap)
//!   snapdel <id> · keep <id> · prune <n> · gc · df          (M5)
//!   date                                              (time page, §2.6)

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use ipc::{sys, Reactor, SyscallTransport};
use storage_server::{wire, DirEnt, Request, Response};
use urt::slots::SlotAlloc;
use urt::spawn::{Exit, SpawnRec};

#[global_allocator]
static HEAP: urt::Heap<{ 1024 * 1024 }> = urt::Heap::new();

// Shell cspace (built by init, §5.1): slot 0 = bootstrap channel, slot 1 =
// storage session, slot 2 = the untyped pool for spawning, slot 5 = a
// read-only time cap re-granted per child. The shell carves two persistent
// objects from the pool at startup and keeps slots 8.. as a recyclable
// window for per-child object caps.
const BOOT_CHAN: u32 = 0;
const STORE_CHAN: u32 = 1;
const POOL: u32 = 2;
/// Persistent event notification: the shell's wait point and the target of
/// every child's on-exit/on-fault bindings (§3.6). Carved once; survives
/// each child's revoke (it descends from the pool, not the donation).
const EVENT_NOTIF: u32 = 3;
/// The reusable per-child donation untyped (§5.1). One child's worth of
/// memory; `revoke` + `reset` reclaims it between spawns (§2.5).
const DONATION: u32 = 4;
/// Read-only time-frame cap (granted by init, §2.6). The shell maps a
/// fresh copy into each child's aspace so children can read the clock —
/// the init→shell time grant, one hop further. Lives in pool memory the
/// per-child reclaim never touches.
const SH_TIME: u32 = 5;
const SPAWN_BASE: u32 = 8;
const SPAWN_CAP: usize = 56; // slots 8..64

/// One child's memory: aspace pool + stack + segments + bootstrap channel,
/// with generous slack. The pool (slot 2) is ~100 MiB, and only this one
/// donation is ever outstanding, so 4 MiB costs nothing and never runs short.
const DONATION_BYTES: u64 = 4 * 1024 * 1024;
const CHILD_CSPACE_SLOTS: u64 = 8;
/// Children run below the shell so a blocked-shell, running-child handoff is
/// the common case, and the §5.4 ceiling keeps a child from outranking us.
const CHILD_PRIO: u64 = 3;
/// Where the time page lands in each child's aspace (init's convention,
/// §2.6). Above the ELF (0x8000_0000) and stack (~0x9000_0000); the VA
/// still travels in the ST01 block — never assumed.
const CHILD_TIME_VA: u64 = 0xA300_0000;

/// Notification bits the kernel raises for this child (§5.1). Distinct so the
/// notification *word* tells exit from fault — two sources multiplexed on one
/// notification, the §3.6 bit-group scan. The shell registers each as a source
/// with the IPC reactor (`register_bound`), which owns the scan; the shell is
/// the reactor's first multi-source production consumer. A console-readable
/// source would slot in as a third bit once the console is a channel.
const EXIT_BIT: u64 = 1 << 0;
const FAULT_BIT: u64 = 1 << 1;
/// Reactor source keys for this child's two terminations (`register_bound`).
/// Opaque to the wait loop — either means "terminated, go reap" (the kind is
/// read back from the report). Distinct so the dispatch is genuinely two-source.
const EXIT_KEY: ipc::Key = 0;
const FAULT_KEY: ipc::Key = 1;

fn out(s: &[u8]) {
    sys::debug_write(s);
}

fn out_num(mut n: u64) {
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
    out(&digits[i..]);
}

/// Zero-padded fixed-width decimal (date/time components).
fn out_num_pad(mut n: u64, width: usize) {
    let mut digits = [b'0'; 20];
    let mut i = digits.len();
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out(&digits[digits.len() - width..]);
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's
/// civil-from-days. Valid for the whole u64-nanosecond range.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
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
/// (`2026-06-11T12:34:56.123456789Z`). All stored time is UTC; timezones
/// are presentation and this shell presents UTC only (§2.6). Full
/// precision so per-ref strict ordering (§4.7) is visible, not rounded
/// away — the RTC's whole-second base makes sub-second digits relative,
/// not absolute.
fn out_utc(ns: u64) {
    let secs = ns / 1_000_000_000;
    let (y, m, d) = civil_from_days(secs / 86_400);
    let tod = secs % 86_400;
    out_num_pad(y, 4);
    out(b"-");
    out_num_pad(m, 2);
    out(b"-");
    out_num_pad(d, 2);
    out(b"T");
    out_num_pad(tod / 3600, 2);
    out(b":");
    out_num_pad(tod % 3600 / 60, 2);
    out(b":");
    out_num_pad(tod % 60, 2);
    out(b".");
    out_num_pad(ns % 1_000_000_000, 9);
    out(b"Z");
}

fn request(req: &Request) -> Response {
    let bytes = match wire::encode_request(req) {
        Ok(b) => b,
        Err(_) => return Response::Err(storage_server::ErrorCode::Internal),
    };
    while sys::chan_send(STORE_CHAN, &bytes, None) == sys::ERR_FULL {
        sys::yield_now();
    }
    let mut buf = [0u8; 256];
    loop {
        let (len, _) = sys::chan_recv(STORE_CHAN, buf.as_mut_ptr(), None);
        if len >= 0 {
            return wire::decode_response(&buf[..len as usize])
                .unwrap_or(Response::Err(storage_server::ErrorCode::Internal));
        }
        sys::yield_now();
    }
}

fn parse_path(s: &[u8]) -> Vec<Vec<u8>> {
    s.split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(|c| c.to_vec())
        .collect()
}

/// Read a whole file through size-bounded Read requests.
fn read_file(path: &[u8]) -> Option<Vec<u8>> {
    let p = parse_path(path);
    let mut data = Vec::new();
    loop {
        match request(&Request::Read {
            handle: 0,
            path: p.clone(),
            offset: data.len() as u64,
            len: 160,
        }) {
            Response::Data(chunk) => {
                let done = chunk.len() < 160;
                data.extend_from_slice(&chunk);
                if done {
                    return Some(data);
                }
            }
            Response::NotFound => return None,
            _ => return None,
        }
    }
}

fn err_name(e: storage_server::ErrorCode) -> &'static [u8] {
    use storage_server::ErrorCode::*;
    match e {
        BadHandle => b"bad handle",
        Stale => b"stale handle (revoked)",
        Denied => b"denied",
        BadPath => b"bad path",
        NotADir => b"not a directory",
        ReadOnly => b"read-only",
        NoSuchSnapshot => b"no such snapshot",
        BadTicket => b"bad ticket",
        Internal => b"server error",
        Pinned => b"snapshot pinned by a tag",
        BadOffset => b"bad offset",
    }
}

fn report(resp: Response) {
    match resp {
        Response::Ok => out(b"ok\n"),
        Response::SnapId(id) => {
            out(b"snapshot #");
            out_num(id);
            out(b"\n");
        }
        Response::Err(e) => {
            out(b"error: ");
            out(err_name(e));
            out(b"\n");
        }
        _ => out(b"ok\n"),
    }
}

fn cmd_ls(arg: &[u8]) {
    match request(&Request::List { handle: 0, path: parse_path(arg) }) {
        Response::Listing(ents) => {
            for e in ents {
                match e {
                    DirEnt::Dir { name } => {
                        out(&name);
                        out(b"/\n");
                    }
                    DirEnt::File { name, size } => {
                        out(&name);
                        out(b"  (");
                        out_num(size);
                        out(b" bytes)\n");
                    }
                }
            }
        }
        r => report(r),
    }
}

fn cmd_cat(arg: &[u8]) {
    match read_file(arg) {
        Some(data) => {
            out(&data);
            if data.last() != Some(&b'\n') {
                out(b"\n");
            }
        }
        None => out(b"error: not found\n"),
    }
}

fn cmd_snaps() {
    match request(&Request::ListSnapshots { handle: 0 }) {
        Response::Snapshots(rows) => {
            for r in rows {
                out(b"#");
                out_num(r.id);
                out(match r.class {
                    0 => b"  keep [",
                    2 => b"  eph  [",
                    _ => b"  auto [",
                });
                out(&r.provenance);
                out(b"] ");
                out_utc(r.timestamp);
                out(b" ");
                out(&r.message);
                out(b"\n");
            }
        }
        r => report(r),
    }
}

/// Wall-clock time end to end: two register reads and the time page,
/// zero syscalls, zero IPC on the read path (§2.6).
fn cmd_date() {
    match urt::time::page() {
        Some(p) => {
            out_utc(p.sample().utc_ns_at(urt::time::cntvct()) as u64);
            out(b"\n");
        }
        None => out(b"error: no time grant\n"),
    }
}

fn cmd_gc() {
    match request(&Request::Gc { handle: 0 }) {
        Response::GcReport { live_objects, freed_objects, freed_bytes } => {
            out(b"gc: freed ");
            out_num(freed_objects);
            out(b" objects / ");
            out_num(freed_bytes);
            out(b" bytes, ");
            out_num(live_objects);
            out(b" live\n");
        }
        r => report(r),
    }
}

fn cmd_df() {
    match request(&Request::Statfs { handle: 0 }) {
        Response::Space { total, used, free } => {
            out(b"chunk region: ");
            out_num(used);
            out(b" used / ");
            out_num(free);
            out(b" free of ");
            out_num(total);
            out(b" bytes\n");
        }
        r => report(r),
    }
}

/// Retention policy is shell-side (§4.7: the server stores fields, it
/// does not interpret policy): keep the newest `n` non-`keep` snapshots,
/// delete the rest. `keep`-class and tagged rows survive.
fn cmd_prune(n: u64) {
    let rows = match request(&Request::ListSnapshots { handle: 0 }) {
        Response::Snapshots(rows) => rows,
        r => return report(r),
    };
    let candidates: Vec<u64> =
        rows.iter().filter(|r| r.class != 0).map(|r| r.id).collect();
    let excess = candidates.len().saturating_sub(n as usize);
    let mut deleted = 0u64;
    for &id in &candidates[..excess] {
        match request(&Request::DeleteSnapshot { handle: 0, snap_id: id }) {
            Response::Ok => deleted += 1,
            Response::Err(e) => {
                out(b"#");
                out_num(id);
                out(b": ");
                out(err_name(e));
                out(b"\n");
            }
            _ => {}
        }
    }
    out(b"pruned ");
    out_num(deleted);
    out(b" snapshot(s)\n");
}

fn parse_u64(arg: &[u8]) -> Option<u64> {
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

/// Lowercase hex, no leading zeros (faulting addresses).
fn out_hex(n: u64) {
    let mut d = [0u8; 16];
    let mut v = n;
    for i in (0..16).rev() {
        d[i] = b"0123456789abcdef"[(v & 0xF) as usize];
        v >>= 4;
    }
    let start = d.iter().position(|&c| c != b'0').unwrap_or(15);
    out(&d[start..]);
}

/// Classify a fault from ESR_EL1 (§5.3): the EC names the kind of abort,
/// the low data-fault-status bits name why. Enough to print
/// `faulted(translation, …)` for the wild-pointer demo without a full ESR
/// table.
fn fault_class(esr: u64) -> &'static [u8] {
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

fn print_exit(e: Exit) {
    match e {
        // A panic surfaces as a normal exit carrying the reserved status
        // (§5.1, U2); name it rather than print exited(18446744073709551615).
        Exit::Exited(sys::STATUS_PANIC) => out(b"panicked\n"),
        Exit::Exited(status) => {
            out(b"exited(");
            out_num(status);
            out(b")\n");
        }
        Exit::Faulted { esr, far } => {
            out(b"faulted(");
            out(fault_class(esr));
            out(b", 0x");
            out_hex(far);
            out(b")\n");
        }
    }
}

/// The slots one spawn consumes from the recyclable window — allocated up
/// front, returned as a unit once the child is reaped (or aborted).
struct SpawnSlots {
    range: u32,  // [range, range+span): aspace, tcb, cspace, frames, stack
    span: u32,
    chan_a: u32, // shell's bootstrap endpoint
    chan_b: u32, // child's endpoint (moved into the child's cspace)
    scratch: u32, // staging slot for the moved-in notification copies
    time_copy: u32, // per-child read-only time-page copy (mapped into it)
}

#[derive(Clone, Copy)]
enum RunErr {
    NoSlots,
    BadElf,
    Carve,
    Start,
}

/// Owns the recyclable slot window and drives the §5.1 spawn/reap loop. One
/// child outstanding at a time (the shell is single-threaded), so a single
/// donation untyped, reused, is the whole resource story.
struct Spawner {
    slots: SlotAlloc<1>,
}

impl Spawner {
    fn new() -> Spawner {
        Spawner { slots: SlotAlloc::new(SPAWN_BASE, SPAWN_CAP) }
    }

    /// Spawn `image` with startup mode `mode`, wait for it to terminate,
    /// read its report, then reclaim every resource it held. Returns how it
    /// terminated. The donation untyped and the slot window come back clean
    /// for the next call — this is the whole burn fix.
    fn run_once(&mut self, image: &[u8], mode: u8) -> Result<Exit, RunErr> {
        let img = loader::elf::parse(image).map_err(|_| RunErr::BadElf)?;
        // Loader slot layout: aspace, tcb, cspace, one frame per segment,
        // stack frame (loader/spawn.rs).
        let span = 3 + img.nsegments as u32 + 1;
        let s = SpawnSlots {
            range: self.slots.alloc_range(span).ok_or(RunErr::NoSlots)?,
            span,
            chan_a: self.slots.alloc().ok_or(RunErr::NoSlots)?,
            chan_b: self.slots.alloc().ok_or(RunErr::NoSlots)?,
            scratch: self.slots.alloc().ok_or(RunErr::NoSlots)?,
            time_copy: self.slots.alloc().ok_or(RunErr::NoSlots)?,
        };
        let exit = self.spawn_inner(image, mode, &s);
        // Whether it ran to completion or aborted mid-setup, the donation is
        // now empty (reap revoked it, or abort below did) and these slots
        // with it — return the window to the free list.
        self.free_slots(&s);
        exit
    }

    fn spawn_inner(&mut self, image: &[u8], mode: u8, s: &SpawnSlots) -> Result<Exit, RunErr> {
        // Bootstrap channel and every child object descend from DONATION, so
        // the child is one CDT subtree teardown collapses in one revoke.
        if sys::retype(DONATION, sys::OBJ_CHANNEL, 4, s.chan_a, s.chan_b) < 0 {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        let prepared = match loader::spawn::prepare(image, DONATION, s.range, CHILD_CSPACE_SLOTS) {
            Ok(p) => p,
            Err(_) => {
                self.scrub(s.time_copy);
                return Err(RunErr::BadElf);
            }
        };
        // The "time" grant (§5.1, §2.6): a fresh read-only copy of our time
        // cap, mapped read-only into the child's aspace at CHILD_TIME_VA. The
        // copy lives OUTSIDE the donation, so `scrub`/`reap` must delete it
        // first — the unmap has to precede the revoke that frees the aspace
        // it points into (§2.5 one-mapping-per-cap).
        if sys::cap_copy(SH_TIME, s.time_copy, sys::RIGHT_READ) < 0
            || sys::map(prepared.aspace_slot, s.time_copy, CHILD_TIME_VA, 0) < 0
        {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // Explicit child world (§5.1): bootstrap endpoint in slot 0, startup
        // block ("ST01" + mode + time-page VA) queued before the child runs.
        sys::cap_install(prepared.cspace_slot, s.chan_b, 0);
        let mut block = [0u8; 13];
        block[..4].copy_from_slice(b"ST01");
        block[4] = mode;
        block[5..13].copy_from_slice(&CHILD_TIME_VA.to_le_bytes());
        sys::chan_send(s.chan_a, &block, None);

        let rec = SpawnRec {
            donation: DONATION,
            main_thread: prepared.tcb_slot,
            exit_bit: EXIT_BIT,
            fault_bit: FAULT_BIT,
        };
        // Bind before start, so a child that exits immediately still raises
        // the bit — the lost-wakeup discipline (§3.6).
        if rec.arm(EVENT_NOTIF, s.scratch) < 0 {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // Multiplex this child's termination through the IPC reactor (§3.6/§4.2):
        // the exit and fault bits were bound to the TCB by `rec.arm` (a
        // `thread_bind`, above, before start), so register them as two
        // externally-bound, edge-triggered sources — `register_bound` records
        // only the bit→key dispatch (no channel bind, no poll-once). This is the
        // shell as the reactor's first multi-source production consumer; the wait
        // below never names a notification bit.
        let transport = SyscallTransport;
        let mut reactor = Reactor::new(&transport, EVENT_NOTIF);
        if reactor.register_bound(EXIT_BIT, EXIT_KEY).is_err()
            || reactor.register_bound(FAULT_BIT, FAULT_KEY).is_err()
        {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        if loader::spawn::start(&prepared, CHILD_PRIO).is_err() {
            self.scrub(s.time_copy);
            return Err(RunErr::Start);
        }

        // Block until this child terminates. `wait` returns when a registered
        // source (exit or fault) fires, ignoring any unregistered bit — it
        // absorbs the by-hand bit-group scan the loop here used to do. Both keys
        // mean "go reap"; reap reads back which (exit vs fault) from the report.
        let (key, _signals) = reactor.wait();
        debug_assert!(key == EXIT_KEY || key == FAULT_KEY, "unexpected reactor key");
        // Unmap the time grant before reap's revoke frees the child aspace
        // (§2.5), then read_report strictly before revoke (enforced in reap).
        let _ = sys::cap_delete(s.time_copy);
        Ok(rec.reap())
    }

    /// Collapse a partially-built child and reset the donation (the abort
    /// counterpart of reap). Drops the time grant first (its mapping points
    /// into the aspace the revoke frees, §2.5); harmless if never granted.
    /// Safe with nothing carved: revoke of a childless untyped is a no-op.
    fn scrub(&self, time_copy: u32) {
        let _ = sys::cap_delete(time_copy);
        sys::cap_revoke(DONATION);
        let _ = sys::untyped_reset(DONATION);
    }

    fn free_slots(&mut self, s: &SpawnSlots) {
        self.slots.free_range(s.range, s.span);
        self.slots.free(s.chan_a);
        self.slots.free(s.chan_b);
        self.slots.free(s.scratch);
        self.slots.free(s.time_copy);
    }

    fn available(&self) -> usize {
        self.slots.available()
    }
}

fn run_err(e: RunErr) {
    out(match e {
        RunErr::NoSlots => b"error: out of spawn slots\n" as &[u8],
        RunErr::BadElf => b"error: bad ELF\n",
        RunErr::Carve => b"error: resource carve failed\n",
        RunErr::Start => b"error: start failed\n",
    });
}

fn cmd_run(sp: &mut Spawner, arg: &[u8]) {
    let mut parts = arg.splitn(2, |&b| b == b' ');
    let path = parts.next().unwrap_or(b"");
    let mode = parts.next().and_then(parse_u64).unwrap_or(0) as u8;

    let Some(image) = read_file(path) else {
        out(b"error: not found\n");
        return;
    };
    out(b"loaded ");
    out_num(image.len() as u64);
    out(b" bytes from the store\n");
    match sp.run_once(&image, mode) {
        Ok(exit) => print_exit(exit),
        Err(e) => run_err(e),
    }
}

/// The burn-fix witness: spawn / wait / reclaim a trivial child `n` times in
/// one boot. Un-reclaimed slots die after the first spawn (the window is far
/// smaller than `n`), so a run that reaches `n/n` *is* slot recycling — and
/// the free count returning to its start proves nothing leaked.
fn cmd_runloop(sp: &mut Spawner, arg: &[u8]) {
    let mut parts = arg.splitn(2, |&b| b == b' ');
    let path = parts.next().unwrap_or(b"");
    let Some(n) = parts.next().and_then(parse_u64) else {
        out(b"usage: runloop <path> <count>\n");
        return;
    };
    let Some(image) = read_file(path) else {
        out(b"error: not found\n");
        return;
    };
    let before = sp.available();
    let mut ok = 0u64;
    for _ in 0..n {
        match sp.run_once(&image, 0) {
            Ok(Exit::Exited(0)) => ok += 1,
            Ok(other) => {
                out(b"unexpected: ");
                print_exit(other);
                break;
            }
            Err(e) => {
                run_err(e);
                break;
            }
        }
    }
    out(b"runloop: ");
    out_num(ok);
    out(b"/");
    out_num(n);
    out(b" ok, slots ");
    out_num(sp.available() as u64);
    out(b"/");
    out_num(before as u64);
    out(b"\n");
}

fn dispatch(sp: &mut Spawner, line: &[u8]) {
    let mut parts = line.splitn(2, |&b| b == b' ');
    let cmd = parts.next().unwrap_or(b"");
    let arg = parts.next().unwrap_or(b"").trim_ascii();
    match cmd {
        b"" => {}
        b"help" => out(
            b"ls cat write rm sync run runloop date\nsnap snaps rollback snapdel keep prune gc df help\n",
        ),
        b"date" => cmd_date(),
        b"ls" => cmd_ls(arg),
        b"cat" => cmd_cat(arg),
        b"rm" => report(request(&Request::Unlink { handle: 0, path: parse_path(arg) })),
        b"sync" => report(request(&Request::Sync { handle: 0 })),
        // class 1 = auto: subject to `prune`; promote survivors via `keep`.
        b"snap" => report(request(&Request::Snapshot {
            handle: 0,
            message: arg.to_vec(),
            class: 1,
        })),
        b"snaps" => cmd_snaps(),
        b"rollback" => match parse_u64(arg) {
            Some(id) => report(request(&Request::Rollback { handle: 0, snap_id: id })),
            None => out(b"usage: rollback <id>\n"),
        },
        b"snapdel" => match parse_u64(arg) {
            Some(id) => report(request(&Request::DeleteSnapshot { handle: 0, snap_id: id })),
            None => out(b"usage: snapdel <id>\n"),
        },
        b"keep" => match parse_u64(arg) {
            Some(id) => {
                report(request(&Request::SetClass { handle: 0, snap_id: id, class: 0 }))
            }
            None => out(b"usage: keep <id>\n"),
        },
        b"prune" => match parse_u64(arg) {
            Some(n) => cmd_prune(n),
            None => out(b"usage: prune <keep-newest-n>\n"),
        },
        b"gc" => cmd_gc(),
        b"df" => cmd_df(),
        b"write" => {
            let mut wa = arg.splitn(2, |&b| b == b' ');
            let path = wa.next().unwrap_or(b"");
            let text = wa.next().unwrap_or(b"");
            report(request(&Request::Write {
                handle: 0,
                path: parse_path(path),
                offset: 0,
                data: text.to_vec(),
            }));
        }
        b"run" => cmd_run(sp, arg),
        b"runloop" => cmd_runloop(sp, arg),
        _ => out(b"unknown command (try help)\n"),
    }
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    // The §5.1 startup block, queued by init before this thread started:
    // "SH01" + time-page VA. No grant, no clock — `date` degrades, the
    // store-backed built-ins don't.
    let mut boot = [0u8; 256];
    let (blen, _) = sys::chan_recv(BOOT_CHAN, boot.as_mut_ptr(), None);
    if blen >= 12 && &boot[..4] == b"SH01" {
        let time_va = u64::from_le_bytes(boot[4..12].try_into().unwrap());
        // Safety: init mapped the read-only time page at this address
        // before starting us; the mapping outlives the process.
        unsafe { urt::time::attach(time_va as usize) };
    }

    // Carve the two persistent spawn objects from the pool (slot 2): the
    // event notification every child's death will signal, and one reusable
    // child-sized donation untyped (§5.1). Both sit in pool memory the
    // per-child reclaim never touches.
    if sys::retype(POOL, sys::OBJ_NOTIF, 0, EVENT_NOTIF, 0) < 0
        || sys::retype(POOL, sys::OBJ_UNTYPED, DONATION_BYTES, DONATION, 0) < 0
    {
        out(b"[shell] FATAL: could not carve spawn objects\n");
        sys::exit();
    }
    let mut spawner = Spawner::new();

    out(b"\nEunomia shell - type help\n");
    let mut line = [0u8; 200];
    let mut len = 0usize;
    out(b"eunomia> ");
    loop {
        let c = sys::debug_getc();
        if c < 0 {
            sys::yield_now();
            continue;
        }
        match c as u8 {
            b'\r' | b'\n' => {
                out(b"\n");
                dispatch(&mut spawner, line[..len].trim_ascii());
                len = 0;
                out(b"eunomia> ");
            }
            0x7F | 0x08 => {
                if len > 0 {
                    len -= 1;
                    out(b"\x08 \x08");
                }
            }
            b if (0x20..0x7F).contains(&b) && len < line.len() => {
                line[len] = b;
                len += 1;
                sys::debug_putc(b);
            }
            _ => {}
        }
    }
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    out(b"[shell] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}
