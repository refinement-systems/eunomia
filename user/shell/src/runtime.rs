//! The QEMU-gated shell runtime (rev2§5.1): the spawn/reap loop, the storage
//! IPC, the REPL, and the bare-metal entry / allocator / panic handler.
//!
//! Every item here is syscall- or spawn-bound, so it is validated by the QEMU
//! boot smoke (`scripts/run-demo.sh`), *not* host-tested (rev2§6 Baseline
//! split). It is excluded from the host test build
//! (`#[cfg(not(test))] mod runtime;` in `main.rs`) because the shell's spawn
//! and clock paths depend on `urt::spawn` and `urt::time::cntvct`, which are
//! aarch64-bare-metal only (no host stub). The pure formatting/parsing/policy
//! logic these built-ins use lives in `main.rs` and is host-tested there.

use crate::{fault_class, fmt_hex, fmt_num, fmt_utc, parse_u64, prune_victims};
use alloc::vec::Vec;
use ipc::{sys, Reactor, SyscallTransport};
use std::io::{Read, Write};
use storage_server::{ErrorCode, Request, Response};
use urt::slots::SlotAlloc;
use urt::spawn::{Exit, SpawnRec};

// The shell is now a std binary (std-port 5.3): std owns `_start`, the allocator
// (a `urt::Heap` sized by `EUNOMIA_HEAP_BYTES`, threaded by `kernel/build.rs`), the
// panic handler, and stdio/time/args/env — so there is no `#[global_allocator]` or
// `#[panic_handler]` here. It keeps raw `urt::spawn`/`loader::spawn` for capability
// spawn/reap (`std::process` cannot model it) and raw `ipc` for the versioned-store
// admin ops (`snap`/`gc`/`df`/… — `std::fs` cannot express them); its plain file
// built-ins ride `std::fs` over the one storaged session `eunomia_sys::fs` connected
// at bootstrap.

// Shell cspace (built by init, rev2§5.1). slot 0 = bootstrap channel and slot 1 =
// storage session are consumed by the std runtime and `eunomia_sys` at bootstrap
// (argv/env/grants; the fs-session connect) — the shell no longer touches them by
// hand. It still references by number the caps init installs for its own
// spawn/console work: slot 2 = the untyped pool, slot 5 = a read-only time cap
// re-granted per child, slot 6 = the console endpoint it donates to children,
// slot 7 = the delegatable fs session. It carves two persistent objects from the
// pool at startup and keeps slots 8.. as a recyclable window for per-child caps.
const POOL: u32 = 2;
/// Persistent event notification: the shell's wait point and the target of
/// every child's on-exit/on-fault bindings (rev2§3.6). Carved once; survives
/// each child's revoke (it descends from the pool, not the donation).
const EVENT_NOTIF: u32 = 3;
/// The reusable per-child donation untyped (rev2§5.1). One child's worth of
/// memory; `revoke` + `reset` reclaims it between spawns (rev2§2.5).
const DONATION: u32 = 4;
/// Read-only time-frame cap (granted by init, rev2§2.6). The shell maps a
/// fresh copy into each child's aspace so children can read the clock —
/// the init→shell time grant, one hop further. Lives in pool memory the
/// per-child reclaim never touches.
const SH_TIME: u32 = 5;
/// The shell's own console-channel endpoint (init's `SHELL_CONSOLE_SLOT`, rev2§5.1):
/// the cap it copies into every child's cspace to donate the foreground terminal.
/// As a std binary the shell's own stdio rides this slot via `eunomia_sys::console`
/// (resolved from the `stdin`/`stdout`/`stderr` grants at bootstrap); this constant
/// is the same init-convention slot, used only as the copy *source* for the child
/// donation in `spawn_inner`. Hardcoded like the other init-installed slots above
/// (a shell↔init co-designed cspace).
const CONSOLE_SLOT: u32 = 6;
const SPAWN_BASE: u32 = 8;
const SPAWN_CAP: usize = 56; // slots 8..64

/// One child's memory: aspace pool + stack + segments + bootstrap channel,
/// with generous slack. The pool (slot 2) is ~100 MiB, and only this one
/// donation is ever outstanding. Sized to cover a thread-capable child's
/// thread-untyped (`urt::thread::THREAD_UNTYPED_BYTES` ≈ 2.1 MiB, incl. the std-port
/// 3.3 per-thread futex park-notifs) on top of the base (std-port 3.2), plus the
/// on-target libtest suites' large `.bss` heap reservation (std-port 6.1:
/// `EUNOMIA_HEAP_BYTES` = 16 MiB, committed at spawn — no demand paging) and their
/// multi-MiB code/data segments. 48 MiB carves comfortably from the ~100 MiB pool and
/// never runs short.
const DONATION_BYTES: u64 = 48 * 1024 * 1024;
/// Default child cspace: slot 0 = bootstrap, the rest a child-carved window.
const CHILD_CSPACE_SLOTS: u64 = 8;

// In-process-threading provisioning (std-port 3.2, scoped/opt-in). A thread-capable
// child gets a wider cspace and, installed at these fixed slots, caps to its own
// aspace (WRITE)/cspace + a thread-untyped, plus a reserved working-slot range —
// named for the child by the `NAME_*` grants (`build_child_block`). Every other
// binary keeps the least-authority default above.
/// A thread-capable child's cspace: slot 0 bootstrap, 1..=3 the self-caps, and
/// `[CHILD_THREAD_SLOT_BASE, +WORKING_SLOTS)` the working range — 4 + 97 = 101 used,
/// rounded up.
const THREAD_CHILD_CSPACE_SLOTS: u64 = 128;
const CHILD_SELF_ASPACE: u32 = 1;
const CHILD_SELF_CSPACE: u32 = 2;
const CHILD_THREAD_UNTYPED: u32 = 3;
const CHILD_THREAD_SLOT_BASE: u32 = 4;

// fs provisioning (std-port 4.1, scoped/opt-in). An fs-capable child receives a copy
// of the shell's *second* storaged session (`SHELL_FS_SESSION_SLOT`, delegated by init)
// installed at `CHILD_STORAGE_SLOT`, plus the `storage`/`root` startup grants, so its
// std `sys/fs` arm can connect and serve files. Every other binary keeps least
// authority (no session). An fs-capable child is not thread-capable, so
// `CHILD_STORAGE_SLOT` (1) does not clash with the thread self-cap slots.
/// The shell cspace slot holding the delegatable storaged session (init installs it;
/// its `SHELL_FS_SESSION_SLOT`). Distinct from the shell's *own* session at slot 1
/// (which `eunomia_sys::fs` connected at bootstrap for the shell's own built-in fs
/// commands) — this one is only ever copied to fs-capable children.
const SHELL_FS_SESSION_SLOT: u32 = 7;
/// The child cspace slot the delegated session lands in, named `storage` in the
/// child's startup block; the root handle is 0 (named `root`).
const CHILD_STORAGE_SLOT: u32 = 1;

// Console provisioning (std-port 5.1): *every* child inherits the shell's console endpoint
// (true foreground-terminal inheritance) — the shell copies its own console cap
// (`CONSOLE_SLOT`) into the child's cspace, named `stdin`/`stdout` in the startup block, so the
// child's std `sys/stdio` arm rides the `user/console` channel instead of the debug-log. stderr
// resolves to the stdout channel in the child (the terminal case — `eunomia_sys::console`), so
// no separate `stderr` grant is pushed and even a thread-capable child stays within
// `MAX_GRANTS` (time + seed + 4 self-caps + 2 console = 8). The slot differs by cspace shape so
// it never clashes with the self-caps/working range: a default (8-slot) child uses a low slot
// past `storage`; a thread-capable (128-slot) child uses the slot just past its working range.
// The donation is best-effort — a child whose console copy fails keeps the debug-log fallback
// rather than failing the spawn (unlike the fs session, which hard-fails).
const CHILD_CONSOLE_SLOT: u32 = 2;
const THREAD_CHILD_CONSOLE_SLOT: u32 = CHILD_THREAD_SLOT_BASE + urt::thread_layout::WORKING_SLOTS;

/// The MVP thread-capability marker: a shell-side allowlist of run paths (the
/// plan's sanctioned fallback — the verified `loader::elf::parse` extracts only
/// PT_LOAD, so an ELF-note marker travelling in the binary is a noted upgrade that
/// avoids touching the verified parser). Only a listed binary is provisioned.
/// The on-target libtest suites (std-port 6.1) are thread-capable: libtest spawns a
/// capture thread per test even at the default serial concurrency, and `alloctests`
/// tests spawn their own threads.
const THREAD_CAPABLE: &[&[u8]] = &[b"bin/stdsmoke", b"bin/coretests", b"bin/alloctests"];

fn is_thread_capable(path: &[u8]) -> bool {
    THREAD_CAPABLE.contains(&path)
}

/// The MVP fs-capability marker (same allowlist mechanism as [`is_thread_capable`]).
/// Only a listed binary is delegated a storaged session (std-port 4.1).
const FS_CAPABLE: &[&[u8]] = &[b"bin/stdfs"];

fn is_fs_capable(path: &[u8]) -> bool {
    FS_CAPABLE.contains(&path)
}

/// On-target library-test suites embedded in the shell's `.rodata` (std-port 6.1),
/// present only under the `libtests` cfg (kernel/build.rs, under EUNOMIA_BUILD_LIBTESTS,
/// passes their ELF paths to the shell build). `run bin/<name>` spawns these from memory
/// instead of loading them over the store: the MVP fs read path reconstructs the whole
/// file per 256-byte request, so a multi-MiB test binary is impractical to load from disk
/// (storaged OOM + O(n²), re-paid per invocation). Empty in a normal build — no size or
/// behavior change.
#[cfg(libtests)]
static EMBEDDED_BINS: &[(&[u8], &[u8])] = &[
    (b"coretests", include_bytes!(env!("CORETESTS_ELF_PATH"))),
    (b"alloctests", include_bytes!(env!("ALLOCTESTS_ELF_PATH"))),
];
#[cfg(not(libtests))]
static EMBEDDED_BINS: &[(&[u8], &[u8])] = &[];

/// The embedded ELF for a `bin/<name>` run path, or `None` to load it from the store.
fn embedded_image(path: &[u8]) -> Option<&'static [u8]> {
    let name = path.strip_prefix(b"bin/").unwrap_or(path);
    EMBEDDED_BINS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|&(_, b)| b)
}
/// Children run below the shell so a blocked-shell, running-child handoff is
/// the common case, and the rev2§5.4 ceiling keeps a child from outranking us.
const CHILD_PRIO: u64 = 3;
/// Where the time page lands in each child's aspace (init's convention,
/// rev2§2.6). Above the ELF (0x8000_0000) and stack (~0x9000_0000); the VA
/// still travels in the startup block's TIME region grant — never assumed.
const CHILD_TIME_VA: u64 = 0xA300_0000;

/// Notification bits the kernel raises for this child (rev2§5.1). Distinct so the
/// notification *word* tells exit from fault — two sources multiplexed on one
/// notification, the rev2§3.6 bit-group scan. The shell registers each as a source
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

/// The storage handle for the ref root (`root`, rev2§5.1). init grants the shell the
/// full-rights ref root at handle 0 (its convention); `eunomia_sys::fs` connected the
/// session against that handle at bootstrap, so the shell's raw admin `Request`s
/// (which `std::fs` cannot express) target handle 0 too.
fn root_handle() -> u32 {
    0
}

/// The inherited environment as raw `KEY=VALUE` byte-strings (rev2§5.1), forwarded to
/// every child the shell spawns (`build_child_block`) — the POSIX inheritance model.
/// `eunomia_sys::bootstrap` stashed it (from the init→shell startup block) for the std
/// `env` arm; the shell reuses that exact byte view rather than re-deriving it from
/// `std::env::vars_os()`, so a non-UTF-8 value round-trips losslessly.
fn shell_env() -> &'static [&'static [u8]] {
    eunomia_sys::bootstrap::env()
}

/// All terminal output (banner, prompt, command results, echo) rides std `stdout`,
/// which `eunomia_sys::console` routes over the `user/console` channel (rev2§5.1) —
/// the shell does *no* ambient debug-UART output. std stdout is line-buffered, so
/// flush after every write (the prompt and per-keystroke echo carry no newline).
fn out(s: &[u8]) {
    let mut so = std::io::stdout();
    let _ = so.write_all(s);
    let _ = so.flush();
}

/// A diagnostic on std `stderr` (routed to the console's stdout channel in a terminal,
/// rev2§5.1) — for the boot FATALs that abort before the REPL. Panic last-words stay
/// on std's own debug-log path (std owns the panic handler now).
fn diag(s: &[u8]) {
    let mut se = std::io::stderr();
    let _ = se.write_all(s);
    let _ = se.flush();
}

fn out_num(n: u64) {
    let mut buf = Vec::new();
    fmt_num(&mut buf, n);
    out(&buf);
}

/// UTC nanoseconds → ISO-8601 with nanosecond precision
/// (`2026-06-11T12:34:56.123456789Z`). All stored time is UTC; timezones
/// are presentation and this shell presents UTC only (rev2§2.6). Full
/// precision so per-ref strict ordering (rev2§4.7) is visible, not rounded
/// away — the RTC's whole-second base makes sub-second digits relative,
/// not absolute.
fn out_utc(ns: u64) {
    let mut buf = Vec::new();
    fmt_utc(&mut buf, ns);
    out(&buf);
}

/// One admin round-trip against storaged over the shell's single storaged session
/// (std-port 5.3): the versioned-store ops (`Snapshot`/`ListSnapshots`/`Rollback`/
/// `DeleteSnapshot`/`SetClass`/`Gc`/`Statfs`/`Sync`) that `std::fs` cannot express.
/// `eunomia_sys::fs` owns the session it connected at bootstrap (the connect
/// handshake + negotiated version), so the shell hands it a raw `Request` and reads
/// back the `Response` — sharing the one session its `std::fs` file ops also ride. A
/// dead/absent session surfaces as an `Internal` error (shown on the console).
fn request(req: &Request) -> Response {
    eunomia_sys::fs::request(req).unwrap_or(Response::Err(ErrorCode::Internal))
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

/// Render a std `io::Error` onto the console as the shell's `error: …` line — the
/// `std::fs` replacement for the raw `Response::Err` → [`err_name`] path (std-port 5.3).
fn out_io_err(e: &std::io::Error) {
    out(b"error: ");
    out(e.to_string().as_bytes());
    out(b"\n");
}

/// A command path argument as `&str` (paths are UTF-8 in practice; the verified
/// `eunomia_sys::path` resolver owns the `.`/`..` and confinement handling on the fs
/// arm). An empty argument means the ref root, rendered `.` so the resolver drops it.
/// A non-UTF-8 path is refused cleanly here (a std `Path` needs valid `str` on eunomia
/// — there is no byte-`OsStr` ext — a disclosed MVP limit, not a crash).
fn path_arg(arg: &[u8]) -> Option<&str> {
    match core::str::from_utf8(arg) {
        Ok("") => Some("."),
        Ok(p) => Some(p),
        Err(_) => {
            out(b"error: invalid path (not utf-8)\n");
            None
        }
    }
}

/// `ls [path]` over `std::fs::read_dir` (std-port 5.3): the listing rides the shared
/// storaged session `eunomia_sys::fs` owns, and the entry kind/size arrive cached in
/// the listing (no per-entry re-probe). Directories print with a trailing `/`.
fn cmd_ls(arg: &[u8]) {
    let Some(path) = path_arg(arg) else { return };
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => return out_io_err(&e),
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => return out_io_err(&e),
        };
        let name = entry.file_name();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out(name.as_encoded_bytes());
            out(b"/\n");
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out(name.as_encoded_bytes());
            out(b"  (");
            out_num(size);
            out(b" bytes)\n");
        }
    }
}

/// `cat <path>` over `std::fs::read` (std-port 5.3). A trailing newline is added when
/// the file does not end in one, matching the pre-std behaviour.
fn cmd_cat(arg: &[u8]) {
    let Some(path) = path_arg(arg) else { return };
    match std::fs::read(path) {
        Ok(data) => {
            out(&data);
            if data.last() != Some(&b'\n') {
                out(b"\n");
            }
        }
        Err(e) => out_io_err(&e),
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

/// Wall-clock time via std `SystemTime` (std-port 5.3): `eunomia_sys` reads the same
/// rev2§2.6 time page under the hood (the granted page + CNTVCT), so `date` is still
/// off the IPC path. init grants the shell the time page, so `now()` never panics here.
fn cmd_date() {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => {
            out_utc(d.as_nanos() as u64);
            out(b"\n");
        }
        Err(_) => out(b"error: clock before epoch\n"),
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

/// Retention policy is shell-side (rev2§4.7: the server stores fields, it does
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
        // (rev2§5.1); name it rather than print exited(18446744073709551615).
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
    /// block would exceed `MAX_BLOCK`) — refused cleanly (rev2§2.7), not a panic.
    Startup,
}

/// Owns the recyclable slot window and drives the rev2§5.1 spawn/reap loop. One
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
    fn run_once(
        &mut self,
        image: &[u8],
        argv: &[&[u8]],
        thread_capable: bool,
        fs_capable: bool,
        inherit_env: bool,
    ) -> Result<Exit, RunErr> {
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
        let exit = self.spawn_inner(image, argv, &s, thread_capable, fs_capable, inherit_env);
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
        thread_capable: bool,
        fs_capable: bool,
        inherit_env: bool,
    ) -> Result<Exit, RunErr> {
        // Bootstrap channel and every child object descend from DONATION, so
        // the child is one CDT subtree teardown collapses in one revoke.
        if sys::retype(DONATION, sys::OBJ_CHANNEL, 4, s.chan_a, s.chan_b) < 0 {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // A thread-capable child needs a wider cspace to hold its self-caps and the
        // per-thread working-slot range (std-port 3.2); every other child keeps the
        // minimal default (least-authority).
        let cspace_slots = if thread_capable {
            THREAD_CHILD_CSPACE_SLOTS
        } else {
            CHILD_CSPACE_SLOTS
        };
        let prepared = match loader::spawn::prepare(image, DONATION, s.range, cspace_slots) {
            Ok(p) => p,
            Err(_) => {
                self.scrub(s.time_copy);
                return Err(RunErr::BadElf);
            }
        };
        // The "time" grant (rev2§5.1, rev2§2.6): a fresh read-only copy of our time
        // cap, mapped read-only into the child's aspace at CHILD_TIME_VA. The
        // copy lives OUTSIDE the donation, so `scrub`/`reap` must delete it
        // first — the unmap has to precede the revoke that frees the aspace
        // it points into (rev2§2.5 one-mapping-per-cap).
        if sys::cap_copy(SH_TIME, s.time_copy, sys::RIGHT_READ) < 0
            || sys::map(prepared.aspace_slot, s.time_copy, CHILD_TIME_VA, 0) < 0
        {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // Explicit child world (rev2§5.1): bootstrap endpoint in slot 0, the
        // unified "EUS1" startup block (a TIME region grant for the time page +
        // the command-line argv) queued before the child runs. An over-budget
        // block is a clean spawn failure, never a panic (rev2§2.7).
        sys::cap_install(prepared.cspace_slot, s.chan_b, 0);
        // Thread-capability (std-port 3.2, scoped): install into the child's own
        // cspace copies of its aspace (WRITE, to map thread stacks) and cspace caps
        // (to name in `thread_start_as`), and a thread-untyped carved from DONATION
        // (so it collapses under the reap revoke like every other child object). Each
        // cap is staged in `s.scratch` and `cap_install`-moved out (leaving it empty
        // for the bind below). All descend from DONATION, so `scrub` reclaims a
        // partial install via its revoke.
        let thread_grants = if thread_capable {
            let install_ok = sys::cap_copy(prepared.aspace_slot, s.scratch, sys::RIGHTS_ALL) >= 0
                && sys::cap_install(prepared.cspace_slot, s.scratch, CHILD_SELF_ASPACE) >= 0
                && sys::cap_copy(prepared.cspace_slot, s.scratch, sys::RIGHTS_ALL) >= 0
                && sys::cap_install(prepared.cspace_slot, s.scratch, CHILD_SELF_CSPACE) >= 0
                && sys::retype(
                    DONATION,
                    sys::OBJ_UNTYPED,
                    urt::thread::THREAD_UNTYPED_BYTES,
                    s.scratch,
                    0,
                ) >= 0
                && sys::cap_install(prepared.cspace_slot, s.scratch, CHILD_THREAD_UNTYPED) >= 0;
            if !install_ok {
                self.scrub(s.time_copy);
                return Err(RunErr::Carve);
            }
            Some([
                CHILD_SELF_ASPACE,
                CHILD_SELF_CSPACE,
                CHILD_THREAD_UNTYPED,
                CHILD_THREAD_SLOT_BASE,
            ])
        } else {
            None
        };
        // std-port 4.1: delegate a copy of the shell's second storaged session to an
        // fs-capable child. It lands in the child's cspace at CHILD_STORAGE_SLOT, named
        // `storage` (+ root handle 0 as `root`) in the startup block, so the child's std
        // `sys/fs` arm connects over it. The copy is a CDT child of the shell's slot-7
        // cap but resides in the child's cspace, so reap's cspace teardown deletes it
        // (the shell's slot 7 is untouched). Staged in `s.scratch`, `cap_install`-moved.
        let storage_slot = if fs_capable {
            let ok = sys::cap_copy(SHELL_FS_SESSION_SLOT, s.scratch, sys::RIGHTS_ALL) >= 0
                && sys::cap_install(prepared.cspace_slot, s.scratch, CHILD_STORAGE_SLOT) >= 0;
            if !ok {
                self.scrub(s.time_copy);
                return Err(RunErr::Carve);
            }
            Some(CHILD_STORAGE_SLOT)
        } else {
            None
        };
        // std-port 5.1: donate the shell's console endpoint to every child so its std
        // `sys/stdio` arm rides the `user/console` channel instead of the debug-log. Copy the
        // shell's own console cap (`CONSOLE_SLOT`, init's convention) into the child's cspace
        // at the console slot, named `stdin`/`stdout` in the block; stderr falls back to the
        // stdout channel in the child. Best-effort: on failure the child keeps the debug-log
        // fallback rather than failing the spawn, and the staged cap is deleted so `s.scratch`
        // is empty for the notif bind below.
        let console_slot = {
            let src = CONSOLE_SLOT;
            let dst = if thread_capable {
                THREAD_CHILD_CONSOLE_SLOT
            } else {
                CHILD_CONSOLE_SLOT
            };
            if sys::cap_copy(src, s.scratch, sys::RIGHTS_ALL) >= 0
                && sys::cap_install(prepared.cspace_slot, s.scratch, dst) >= 0
            {
                Some(dst)
            } else {
                // A partial copy leaves the cap staged in scratch; delete it so scratch is
                // empty for the notif bind below (the child then keeps the debug-log fallback).
                let _ = sys::cap_delete(s.scratch);
                None
            }
        };
        // std-port 3.4: draw a fresh entropy sub-seed for this child from the
        // shell's own DRBG (the fork-without-reseed guard) — never the shell's
        // seed raw. `eunomia_sys::bootstrap` seeded `urt::random` from the shell's
        // `NAME_RANDOM_SEED` grant at bootstrap (the shell still draws from it here).
        let mut block = [0u8; loader::startup::MAX_BLOCK];
        let n = match crate::build_child_block(
            &mut block,
            CHILD_TIME_VA,
            argv,
            // std-port 5.2: forward the shell's inherited environment to the child (POSIX
            // inheritance). The on-target libtest children (std-port 6.1) opt out
            // (`inherit_env == false`): core/alloc tests read no env vars, and dropping the
            // ~38 bytes keeps the 256-byte startup block within budget when several libtest
            // `--skip` filters must ride in argv.
            if inherit_env { shell_env() } else { &[] },
            thread_grants,
            storage_slot,
            console_slot,
            urt::random::fresh_seed(),
        ) {
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
        // the bit — the lost-wakeup discipline (rev2§3.6).
        if rec.arm(EVENT_NOTIF, s.scratch) < 0 {
            self.scrub(s.time_copy);
            return Err(RunErr::Carve);
        }
        // Multiplex this child's termination through the IPC reactor (rev2§3.6):
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
        // source (exit or fault) fires, ignoring any unregistered bit — it owns
        // the bit-group scan, so the loop here does none by hand. Both keys
        // mean "go reap"; reap reads back which (exit vs fault) from the report.
        let (key, _signals) = reactor.wait();
        debug_assert!(
            key == EXIT_KEY || key == FAULT_KEY,
            "unexpected reactor key"
        );
        // Unmap the time grant before reap's revoke frees the child aspace
        // (rev2§2.5), then read_report strictly before revoke (enforced in reap).
        let _ = sys::cap_delete(s.time_copy);
        Ok(rec.reap())
    }

    /// Collapse a partially-built child and reset the donation (the abort
    /// counterpart of reap). Drops the time grant first (its mapping points
    /// into the aspace the revoke frees, rev2§2.5); harmless if never granted.
    /// Safe with nothing carved: revoke of a childless untyped is a no-op.
    fn scrub(&self, time_copy: u32) {
        let _ = sys::cap_delete(time_copy);
        // Revoke is a bounded per-call quantum returning ERR_AGAIN until
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

/// Load a program image from the store via `std::fs::read` (std-port 5.3): the ELF the
/// shell spawns, read over the shared storaged session. `None` — not found, unreadable,
/// or a non-UTF-8 path — is the caller's clean "error: not found".
fn read_image(path: &[u8]) -> Option<Vec<u8>> {
    let path = core::str::from_utf8(path).ok()?;
    std::fs::read(path).ok()
}

fn cmd_run(sp: &mut Spawner, arg: &[u8]) {
    // argv from the command line (rev2§5.1): whitespace-split tokens,
    // empties dropped. argv[0] is the program path; the rest are arguments the
    // child reads from the startup block (e.g. selftest's mode in argv[1]).
    let argv: Vec<&[u8]> = arg
        .split(|&b| b == b' ')
        .filter(|t| !t.is_empty())
        .collect();
    let path = argv.first().copied().unwrap_or(b"");

    // An embedded on-target test suite (std-port 6.1) spawns from `.rodata`; every
    // other binary loads from the store. Both paths hand a byte slice to `run_once`.
    let result = if let Some(image) = embedded_image(path) {
        // Embedded libtest suites opt out of env inheritance to keep the startup block
        // within budget for `--skip` filters (std-port 6.1).
        sp.run_once(
            image,
            &argv,
            is_thread_capable(path),
            is_fs_capable(path),
            false,
        )
    } else {
        let Some(image) = read_image(path) else {
            out(b"error: not found\n");
            return;
        };
        out(b"loaded ");
        out_num(image.len() as u64);
        out(b" bytes from the store\n");
        sp.run_once(
            &image,
            &argv,
            is_thread_capable(path),
            is_fs_capable(path),
            true,
        )
    };
    match result {
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
    let Some(image) = read_image(path) else {
        out(b"error: not found\n");
        return;
    };
    let before = sp.available();
    let mut ok = 0u64;
    // No mode argument → the child sees argv = [path] only (selftest defaults to
    // mode 0, a clean exit(0)) — the burn-fix witness wants a trivial child.
    let argv: [&[u8]; 1] = [path];
    for _ in 0..n {
        // The burn-fix witness runs `selftest` (a no_std child) with no thread self-caps and no
        // fs session — just the console every child now inherits (an unused cap for a no_std
        // child), so each iteration also exercises the copy→reap endpoint-census round trip.
        match sp.run_once(&image, &argv, false, false, true) {
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

/// `rm <path>` over `std::fs::remove_file` (std-port 5.3).
fn cmd_rm(arg: &[u8]) {
    let Some(path) = path_arg(arg) else { return };
    match std::fs::remove_file(path) {
        Ok(()) => out(b"ok\n"),
        Err(e) => out_io_err(&e),
    }
}

/// `write <path> <text>` over `std::fs::write` (std-port 5.3): create-or-overwrite the
/// file with `text`. `File::create`'s truncate is emulated by an unlink (rev2§4.9 has
/// no `set_len`), so a re-`write` replaces the content rather than overlaying it.
fn cmd_write(arg: &[u8]) {
    let mut wa = arg.splitn(2, |&b| b == b' ');
    let path = wa.next().unwrap_or(b"");
    let text = wa.next().unwrap_or(b"");
    let Some(path) = path_arg(path) else { return };
    match std::fs::write(path, text) {
        Ok(()) => out(b"ok\n"),
        Err(e) => out_io_err(&e),
    }
}

/// `mv <from> <to>` over `std::fs::rename` (std-port 5.3). Cross-subtree rename is
/// `EXDEV` by construction (rev2§4.9); within the ref it is a tree move.
fn cmd_mv(arg: &[u8]) {
    let mut ma = arg.splitn(2, |&b| b == b' ');
    let from = ma.next().unwrap_or(b"");
    let to = ma.next().unwrap_or(b"").trim_ascii();
    if from.is_empty() || to.is_empty() {
        out(b"usage: mv <from> <to>\n");
        return;
    }
    let Some(from) = path_arg(from) else { return };
    let Some(to) = path_arg(to) else { return };
    match std::fs::rename(from, to) {
        Ok(()) => out(b"ok\n"),
        Err(e) => out_io_err(&e),
    }
}

fn dispatch(sp: &mut Spawner, line: &[u8]) {
    let mut parts = line.splitn(2, |&b| b == b' ');
    let cmd = parts.next().unwrap_or(b"");
    let arg = parts.next().unwrap_or(b"").trim_ascii();
    match cmd {
        b"" => {}
        b"help" => out(
            b"ls cat write mv rm sync run runloop date\nsnap snaps rollback snapdel keep prune gc df help\n",
        ),
        b"date" => cmd_date(),
        b"ls" => cmd_ls(arg),
        b"cat" => cmd_cat(arg),
        b"rm" => cmd_rm(arg),
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
        b"write" => cmd_write(arg),
        b"mv" => cmd_mv(arg),
        b"run" => cmd_run(sp, arg),
        b"runloop" => cmd_runloop(sp, arg),
        _ => out(b"unknown command (try help)\n"),
    }
}

/// The shell's entry (std-port 5.3). std owns `_start`, the allocator, and the panic
/// handler; `eunomia_sys` at bootstrap has already decoded the rev2§5.1 startup block —
/// argv/env, the time page (`SystemTime`), the DRBG seed (`urt::random`, for per-child
/// sub-seeds), the storaged session (`eunomia_sys::fs` connected it), and
/// stdin/stdout/stderr over the `user/console` channel. So the shell no longer receives
/// or decodes the block, resolves grants, or runs a storage connect handshake by hand.
///
/// `main` (in `main.rs`) calls this after that bootstrap. Its only remaining setup is
/// the spawn machinery init hands it by cspace-slot number: carve the persistent event
/// notification + the reusable donation untyped from the pool (slot 2). It never returns
/// (the REPL loops).
pub fn run() -> ! {
    // Carve the two persistent spawn objects from the pool (slot 2): the event
    // notification every child's death signals, and one reusable child-sized donation
    // untyped (rev2§5.1). Both sit in pool memory the per-child reclaim never touches.
    if sys::retype(POOL, sys::OBJ_NOTIF, 0, EVENT_NOTIF, 0) < 0
        || sys::retype(POOL, sys::OBJ_UNTYPED, DONATION_BYTES, DONATION, 0) < 0
    {
        diag(b"[shell] FATAL: could not carve spawn objects\n");
        std::process::exit(1);
    }
    let mut spawner = Spawner::new();

    out(b"\nEunomia shell - type help\n");
    // The REPL reads keystrokes one byte at a time from std `stdin` (routed over the
    // `user/console` channel by `eunomia_sys::console`) and echoes them itself: the
    // console driver owns the raw UART line and does no echo, so the shell provides the
    // line editing (printable echo, backspace) it always has. `StdinLock` is buffered,
    // so a whole piped/typed line is one console read, then served byte by byte.
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut byte = [0u8; 1];
    let mut line = [0u8; 200];
    let mut len = 0usize;
    out(b"eunomia> ");
    loop {
        match input.read(&mut byte) {
            Ok(1) => {}
            // EOF (peer closed) or a transient read error: park politely and retry — an
            // interactive console never truly ends (the harness kills QEMU at teardown).
            _ => {
                sys::yield_now();
                continue;
            }
        }
        match byte[0] {
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
                out(&[b]);
            }
            _ => {}
        }
    }
}
