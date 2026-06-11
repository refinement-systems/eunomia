//! The Eunomia shell (spec §7): built-ins over a storage session.
//!
//! World (built by init, §5.1): slot 0 = bootstrap channel, slot 1 =
//! storage session (handle 0 = main ref root, full rights), slot 2 =
//! untyped for spawning, slots 8+ free for the spawner.
//!
//!   ls [path] · cat <path> · write <path> <text> · rm <path>
//!   snap [msg] · snaps · rollback <id> · sync · run <path> · help
//!   snapdel <id> · keep <id> · prune <n> · gc · df          (M5)

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use ipc::sys;
use storage_server::{wire, DirEnt, Request, Response};

#[global_allocator]
static HEAP: urt::Heap<{ 1024 * 1024 }> = urt::Heap::new();

const STORE_CHAN: u32 = 1;
const UNTYPED: u32 = 2;
const SPAWN_BASE: u32 = 8;
const RUN_CHAN_A: u32 = 4;
const RUN_CHAN_B: u32 = 5;

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
                out(&r.message);
                out(b"\n");
            }
        }
        r => report(r),
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

fn cmd_run(arg: &[u8]) {
    let Some(image) = read_file(arg) else {
        out(b"error: not found\n");
        return;
    };
    out(b"loaded ");
    out_num(image.len() as u64);
    out(b" bytes from the store\n");

    if sys::retype(UNTYPED, sys::OBJ_CHANNEL, 4, RUN_CHAN_A, RUN_CHAN_B) < 0 {
        out(b"error: channel\n");
        return;
    }
    let prepared = match loader::spawn::prepare(&image, UNTYPED, SPAWN_BASE, 8) {
        Ok(p) => p,
        Err(_) => {
            out(b"error: bad ELF\n");
            sys::cap_delete(RUN_CHAN_A);
            sys::cap_delete(RUN_CHAN_B);
            return;
        }
    };
    // Explicit child world (§5.1): startup block queued, bootstrap
    // channel in slot 0.
    sys::chan_send(RUN_CHAN_A, b"startup:hello", None);
    sys::cap_install(prepared.cspace_slot, RUN_CHAN_B, 0);
    if loader::spawn::start(&prepared, 4).is_err() {
        out(b"error: start\n");
        return;
    }
    let mut buf = [0u8; 256];
    loop {
        let (len, _) = sys::chan_recv(RUN_CHAN_A, buf.as_mut_ptr(), None);
        if len >= 0 {
            out(b"child replied: ");
            out(&buf[..len as usize]);
            out(b"\n");
            break;
        }
        sys::yield_now();
    }
    sys::cap_delete(RUN_CHAN_A);
    // Child slots stay allocated (no slot reuse in the MVP shell): each
    // `run` uses fresh spawn slots only if we rotated them — accept one
    // run per boot for the demo.
}

fn dispatch(line: &[u8]) {
    let mut parts = line.splitn(2, |&b| b == b' ');
    let cmd = parts.next().unwrap_or(b"");
    let arg = parts.next().unwrap_or(b"").trim_ascii();
    match cmd {
        b"" => {}
        b"help" => out(
            b"ls cat write rm sync run\nsnap snaps rollback snapdel keep prune gc df help\n",
        ),
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
        b"run" => cmd_run(arg),
        _ => out(b"unknown command (try help)\n"),
    }
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
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
                dispatch(line[..len].trim_ascii());
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
    sys::exit()
}
