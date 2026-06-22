//! The QEMU-gated shell runtime (rev1§5.1): the spawn/reap loop, the storage
//! IPC, the REPL, and the bare-metal entry / allocator / panic handler.
//!
//! Every item here is syscall- or spawn-bound, so it is validated by the QEMU
//! boot smoke (`scripts/run-demo.sh`), *not* host-tested (rev1§6 Baseline
//! split, B15 Design decision 3). It is excluded from the host test build
//! (`#[cfg(not(test))] mod runtime;` in `main.rs`) because the shell's spawn
//! and clock paths depend on `urt::spawn` and `urt::time::cntvct`, which are
//! aarch64-bare-metal only (no host stub). The pure formatting/parsing/policy
//! logic these built-ins use lives in `main.rs` and is host-tested there.

use crate::{
    fault_class, fmt_hex, fmt_num, fmt_utc, parse_path, parse_u64, prune_victims,
    resolve_root_handle, resolve_storage_slot, resolve_time_va,
};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ipc::{sys, Reactor, SyscallTransport};
use storage_server::{wire, DirEnt, Request, Response};
use urt::slots::SlotAlloc;
use urt::spawn::{Exit, SpawnRec};

#[global_allocator]
static HEAP: urt::Heap<{ 1024 * 1024 }> = urt::Heap::new();

// Shell cspace (built by init, rev1§5.1): slot 0 = bootstrap channel, slot 1 =
// storage session, slot 2 = the untyped pool for spawning, slot 5 = a
// read-only time cap re-granted per child. The shell carves two persistent
// objects from the pool at startup and keeps slots 8.. as a recyclable
// window for per-child object caps.
const BOOT_CHAN: u32 = 0;
const STORE_CHAN: u32 = 1;
const POOL: u32 = 2;
/// Persistent event notification: the shell's wait point and the target of
/// every child's on-exit/on-fault bindings (rev1§3.6). Carved once; survives
/// each child's revoke (it descends from the pool, not the donation).
const EVENT_NOTIF: u32 = 3;
/// The reusable per-child donation untyped (rev1§5.1). One child's worth of
/// memory; `revoke` + `reset` reclaims it between spawns (rev1§2.5).
const DONATION: u32 = 4;
/// Read-only time-frame cap (granted by init, rev1§2.6). The shell maps a
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
/// the common case, and the rev1§5.4 ceiling keeps a child from outranking us.
const CHILD_PRIO: u64 = 3;
/// Where the time page lands in each child's aspace (init's convention,
/// rev1§2.6). Above the ELF (0x8000_0000) and stack (~0x9000_0000); the VA
/// still travels in the startup block's TIME region grant — never assumed.
const CHILD_TIME_VA: u64 = 0xA300_0000;

/// Notification bits the kernel raises for this child (rev1§5.1). Distinct so the
/// notification *word* tells exit from fault — two sources multiplexed on one
/// notification, the rev1§3.6 bit-group scan. The shell registers each as a source
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

/// The shell's storage authority, resolved from the init→shell `b"EUS1"`
/// named-grant table once in `_start` (rev1§5.1, C1C): `storage` → the session
/// channel slot, `root` → the handle on that session. Defaults match init's
/// convention (`STORE_CHAN`, handle 0) so an absent grant degrades to today's
/// behaviour. The shell is single-threaded (cooperative `yield_now`) and these
/// are written before the REPL runs, so `Relaxed` ordering is sufficient.
static STORE_SLOT: AtomicU32 = AtomicU32::new(STORE_CHAN);
static ROOT_HANDLE: AtomicU32 = AtomicU32::new(0);

/// The cspace slot of the storage-session channel (`storage`, rev1§5.1).
fn store_slot() -> u32 {
    STORE_SLOT.load(Ordering::Relaxed)
}

/// The storage handle for the ref root (`root`, rev1§5.1).
fn root_handle() -> u32 {
    ROOT_HANDLE.load(Ordering::Relaxed)
}

fn out(s: &[u8]) {
    sys::debug_write(s);
}

fn out_num(n: u64) {
    let mut buf = Vec::new();
    fmt_num(&mut buf, n);
    out(&buf);
}

/// UTC nanoseconds → ISO-8601 with nanosecond precision
/// (`2026-06-11T12:34:56.123456789Z`). All stored time is UTC; timezones
/// are presentation and this shell presents UTC only (rev1§2.6). Full
/// precision so per-ref strict ordering (rev1§4.7) is visible, not rounded
/// away — the RTC's whole-second base makes sub-second digits relative,
/// not absolute.
fn out_utc(ns: u64) {
    let mut buf = Vec::new();
    fmt_utc(&mut buf, ns);
    out(&buf);
}

fn request(req: &Request) -> Response {
    let bytes = match wire::encode_request(req) {
        Ok(b) => b,
        Err(_) => return Response::Err(storage_server::ErrorCode::Internal),
    };
    let store = store_slot();
    while sys::chan_send(store, &bytes, None) == sys::ERR_FULL {
        sys::yield_now();
    }
    let mut buf = [0u8; 256];
    loop {
        let (len, _) = sys::chan_recv(store, buf.as_mut_ptr(), None);
        if len >= 0 {
            return wire::decode_response(&buf[..len as usize])
                .unwrap_or(Response::Err(storage_server::ErrorCode::Internal));
        }
        sys::yield_now();
    }
}

/// Read a whole file through size-bounded Read requests.
fn read_file(path: &[u8]) -> Option<Vec<u8>> {
    let p = parse_path(path);
    let mut data = Vec::new();
    loop {
        match request(&Request::Read {
            handle: root_handle(),
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
    match request(&Request::List {
        handle: root_handle(),
        path: parse_path(arg),
    }) {
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
    match request(&Request::ListSnapshots {
        handle: root_handle(),
    }) {
        Response::Snapshots { snaps: rows, .. } => {
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
/// zero syscalls, zero IPC on the read path (rev1§2.6).
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
    match request(&Request::Gc {
        handle: root_handle(),
    }) {
        Response::GcReport {
            live_objects,
            freed_objects,
            freed_bytes,
        } => {
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
    match request(&Request::Statfs {
        handle: root_handle(),
    }) {
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

/// Retention policy is shell-side (rev1§4.7: the server stores fields, it does
/// not interpret policy). [`prune_victims`] selects the ids to delete; this
/// keeps the IPC loop over them. `keep`-class and tagged rows survive.
fn cmd_prune(n: u64) {
    let rows = match request(&Request::ListSnapshots {
        handle: root_handle(),
    }) {
        Response::Snapshots { snaps: rows, .. } => rows,
        r => return report(r),
    };
    let mut deleted = 0u64;
    for id in prune_victims(&rows, n) {
        match request(&Request::DeleteSnapshot {
            handle: root_handle(),
            snap_id: id,
        }) {
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

fn out_hex(n: u64) {
    let mut buf = Vec::new();
    fmt_hex(&mut buf, n);
    out(&buf);
}

fn print_exit(e: Exit) {
    match e {
        // A panic surfaces as a normal exit carrying the reserved status
        // (rev1§5.1); name it rather than print exited(18446744073709551615).
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
    range: u32, // [range, range+span): aspace, tcb, cspace, frames, stack
    span: u32,
    chan_a: u32,    // shell's bootstrap endpoint
    chan_b: u32,    // child's endpoint (moved into the child's cspace)
    scratch: u32,   // staging slot for the moved-in notification copies
    time_copy: u32, // per-child read-only time-page copy (mapped into it)
}

#[derive(Clone, Copy)]
enum RunErr {
    NoSlots,
    BadElf,
    Carve,
    Start,
    /// The startup block could not be encoded (too many argv entries, or the
    /// block would exceed `MAX_BLOCK`) — refused cleanly (rev1§2.7), not a panic.
    Startup,
}

/// Owns the recyclable slot window and drives the rev1§5.1 spawn/reap loop. One
/// child outstanding at a time (the shell is single-threaded), so a single
/// donation untyped, reused, is the whole resource story.
struct Spawner {
    slots: SlotAlloc<1>,
}

impl Spawner {
    fn new() -> Spawner {
        Spawner {
            slots: SlotAlloc::new(SPAWN_BASE, SPAWN_CAP),
        }
    }

    /// Spawn `image` with startup mode `mode`, wait for it to terminate,
    /// read its report, then reclaim every resource it held. Returns how it
    /// terminated. The donation untyped and the slot window come back clean
    /// for the next call — this is the whole burn fix.
    fn run_once(&mut self, image: &[u8], argv: &[&[u8]]) -> Result<Exit, RunErr> {
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
        let exit = self.spawn_inner(image, argv, &s);
        // Whether it ran to completion or aborted mid-setup, the donation is
        // now empty (reap revoked it, or abort below did) and these slots
        // with it — return the window to the free list.
        self.free_slots(&s);
        exit
    }

    fn spawn_inner(
        &mut self,
        image: &[u8],
        argv: &[&[u8]],
        s: &SpawnSlots,
    ) -> Result<Exit, RunErr> {
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
        // The "time" grant (rev1§5.1, rev1§2.6): a fresh read-only copy of our time
        // cap, mapped read-only into the child's aspace at CHILD_TIME_VA. The
        // copy lives OUTSIDE the donation, so `scrub`/`reap` must delete it
        // first — the unmap has to precede the revoke that frees the aspace
        // it points into (rev1§2.5 one-mapping-per-cap).
        if sys::cap_copy(SH_TIME, s.time_copy, sys::RIGHT_READ) < 0
            || sys::map(prepared.aspace_slot, s.time_copy, CHILD_TIME_VA, 0) < 0
        {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // Explicit child world (rev1§5.1): bootstrap endpoint in slot 0, the
        // unified "EUS1" startup block (a TIME region grant for the time page +
        // the command-line argv, C1D) queued before the child runs. An over-budget
        // block is a clean spawn failure, never a panic (rev1§2.7).
        sys::cap_install(prepared.cspace_slot, s.chan_b, 0);
        let mut block = [0u8; loader::startup::MAX_BLOCK];
        let n = match crate::build_child_block(&mut block, CHILD_TIME_VA, argv) {
            Ok(n) => n,
            Err(_) => {
                self.scrub(s.time_copy);
                return Err(RunErr::Startup);
            }
        };
        sys::chan_send(s.chan_a, &block[..n], None);

        let rec = SpawnRec {
            donation: DONATION,
            main_thread: prepared.tcb_slot,
            exit_bit: EXIT_BIT,
            fault_bit: FAULT_BIT,
        };
        // Bind before start, so a child that exits immediately still raises
        // the bit — the lost-wakeup discipline (rev1§3.6).
        if rec.arm(EVENT_NOTIF, s.scratch) < 0 {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // Multiplex this child's termination through the IPC reactor (rev1§3.6):
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
        debug_assert!(
            key == EXIT_KEY || key == FAULT_KEY,
            "unexpected reactor key"
        );
        // Unmap the time grant before reap's revoke frees the child aspace
        // (rev1§2.5), then read_report strictly before revoke (enforced in reap).
        let _ = sys::cap_delete(s.time_copy);
        Ok(rec.reap())
    }

    /// Collapse a partially-built child and reset the donation (the abort
    /// counterpart of reap). Drops the time grant first (its mapping points
    /// into the aspace the revoke frees, rev1§2.5); harmless if never granted.
    /// Safe with nothing carved: revoke of a childless untyped is a no-op.
    fn scrub(&self, time_copy: u32) {
        let _ = sys::cap_delete(time_copy);
        // B9: revoke is now a bounded per-call quantum returning ERR_AGAIN until
        // the subtree is empty; loop to completion (childless donation → one call).
        sys::cap_revoke_all(DONATION);
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
        RunErr::Startup => b"error: startup block rejected\n",
    });
}

fn cmd_run(sp: &mut Spawner, arg: &[u8]) {
    // argv from the command line (rev1§5.1, C1D): whitespace-split tokens,
    // empties dropped. argv[0] is the program path; the rest are arguments the
    // child reads from the startup block (e.g. selftest's mode in argv[1]).
    let argv: Vec<&[u8]> = arg
        .split(|&b| b == b' ')
        .filter(|t| !t.is_empty())
        .collect();
    let path = argv.first().copied().unwrap_or(b"");

    let Some(image) = read_file(path) else {
        out(b"error: not found\n");
        return;
    };
    out(b"loaded ");
    out_num(image.len() as u64);
    out(b" bytes from the store\n");
    match sp.run_once(&image, &argv) {
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
    // No mode argument → the child sees argv = [path] only (selftest defaults to
    // mode 0, a clean exit(0)) — the burn-fix witness wants a trivial child.
    let argv: [&[u8]; 1] = [path];
    for _ in 0..n {
        match sp.run_once(&image, &argv) {
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
        b"rm" => report(request(&Request::Unlink { handle: root_handle(), path: parse_path(arg) })),
        b"sync" => report(request(&Request::Sync { handle: root_handle() })),
        // class 1 = auto: subject to `prune`; promote survivors via `keep`.
        b"snap" => report(request(&Request::Snapshot {
            handle: root_handle(),
            message: arg.to_vec(),
            class: 1,
        })),
        b"snaps" => cmd_snaps(),
        b"rollback" => match parse_u64(arg) {
            Some(id) => report(request(&Request::Rollback { handle: root_handle(), snap_id: id })),
            None => out(b"usage: rollback <id>\n"),
        },
        b"snapdel" => match parse_u64(arg) {
            Some(id) => report(request(&Request::DeleteSnapshot { handle: root_handle(), snap_id: id })),
            None => out(b"usage: snapdel <id>\n"),
        },
        b"keep" => match parse_u64(arg) {
            Some(id) => {
                report(request(&Request::SetClass { handle: root_handle(), snap_id: id, class: 0 }))
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
                handle: root_handle(),
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
    // The rev1§5.1 startup block, queued by init before this thread started: the
    // unified `b"EUS1"` named-grant table (`loader::startup`, C1C). Resolve the
    // standard names `storage`/`root`/`time` once here. A malformed block is
    // refused, not a crash (decode is total, rev1§2.7); an absent name keeps the
    // default — no `time` grant means no clock (`date` degrades), `storage`/`root`
    // default to init's convention.
    let mut boot = [0u8; 256];
    let (blen, _) = sys::chan_recv(BOOT_CHAN, boot.as_mut_ptr(), None);
    if let Some(s) = loader::startup::decode(&boot[..blen.max(0) as usize]) {
        if let Some(slot) = resolve_storage_slot(&s) {
            STORE_SLOT.store(slot, Ordering::Relaxed);
        }
        if let Some(h) = resolve_root_handle(&s) {
            ROOT_HANDLE.store(h, Ordering::Relaxed);
        }
        if let Some(va) = resolve_time_va(&s) {
            // Safety: init mapped the read-only time page at this address
            // before starting us; the mapping outlives the process.
            unsafe { urt::time::attach(va as usize) };
        }
    }

    // Carve the two persistent spawn objects from the pool (slot 2): the
    // event notification every child's death will signal, and one reusable
    // child-sized donation untyped (rev1§5.1). Both sit in pool memory the
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
