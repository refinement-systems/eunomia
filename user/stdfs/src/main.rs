//! The std-port Phase-4.1 fs GATE fixture (findings #13): the first std binary to
//! drive the real `sys/fs/eunomia` client against storaged. It exercises the whole
//! file surface the gate names — create/write, read-back, `read_dir`, `rename`,
//! `remove_file`, and `sync_all` — over the storaged session the shell delegated to
//! it (a fresh session storaged multiplexes, negotiated by the client-side connect
//! handshake at bootstrap), then prints the green marker `STD4 PASS`.
//!
//! It is a real std program — no `#![no_std]`, no `#[panic_handler]`. std owns
//! `_start` (the eunomia PAL) and the panic handler; `extern crate eunomia_sys;`
//! forces the seam rlib into the link so the `__eunomia_*` (incl. `__eunomia_fs_*`)
//! symbols resolve. A failed op `panic!`s (reaped as STATUS_PANIC) or exits with a
//! distinct non-zero code, so the boot harness can tell exactly which step broke.

extern crate eunomia_sys; // links the PAL↔seam bridge (incl. the fs client)

use std::fs;
use std::io::Write;

fn main() {
    println!("[stdfs] alive");

    let path = "docs/smoke";
    let renamed = "docs/smoke2";
    let content = b"eunomia fs smoke\n";

    // 1. create + write + fsync. Writing `docs/smoke` creates the `docs` directory
    //    and the file as a side effect (rev2§4.9); `sync_all` drives `Sync`.
    {
        let mut f = fs::File::create(path).expect("create docs/smoke");
        f.write_all(content).expect("write docs/smoke");
        f.sync_all().expect("sync_all docs/smoke");
    }
    println!("[stdfs] wrote {} bytes", content.len());

    // 2. read the file back and check the bytes round-trip (the chunked read loop).
    let got = fs::read(path).expect("read docs/smoke");
    if got != content {
        println!("[stdfs] fs-bad read mismatch len={}", got.len());
        std::process::exit(2);
    }
    println!("[stdfs] read back ok");

    // 2b. the same file named through `.`/`..` resolves to it (std-port 4.2, the
    //     verified `eunomia_sys::path::resolve`): `docs/./smoke` and
    //     `docs/../docs/smoke` both resolve to `[docs, smoke]` before the wire, and a
    //     `..` escaping the root handle is refused (rev2§2.3) as a clean error, never
    //     a panic or a wire round-trip.
    for alias in ["docs/./smoke", "docs/../docs/smoke"] {
        let got = fs::read(alias).unwrap_or_else(|e| panic!("read {alias}: {e}"));
        if got != content {
            println!("[stdfs] fs-bad alias {alias} mismatch");
            std::process::exit(7);
        }
    }
    if fs::read("../escape").is_ok() {
        println!("[stdfs] fs-bad escaping `..` accepted");
        std::process::exit(8);
    }
    println!("[stdfs] dotdot resolves + escape refused");

    // 3. read_dir the parent and confirm the entry is listed (the `List` path).
    let mut found = false;
    for entry in fs::read_dir("docs").expect("read_dir docs") {
        let entry = entry.expect("dir entry");
        if entry.file_name() == "smoke" {
            found = true;
        }
    }
    if !found {
        println!("[stdfs] fs-bad readdir missing smoke");
        std::process::exit(3);
    }
    println!("[stdfs] readdir found smoke");

    // 4. rename: the old name resolves away, the new one carries the content.
    fs::rename(path, renamed).expect("rename");
    if fs::metadata(path).is_ok() {
        println!("[stdfs] fs-bad rename src still present");
        std::process::exit(4);
    }
    let got2 = fs::read(renamed).expect("read renamed");
    if got2 != content {
        println!("[stdfs] fs-bad renamed content mismatch");
        std::process::exit(5);
    }
    println!("[stdfs] renamed ok");

    // 5. remove: the file is gone afterward (the `Unlink` path).
    fs::remove_file(renamed).expect("remove");
    if fs::metadata(renamed).is_ok() {
        println!("[stdfs] fs-bad remove still present");
        std::process::exit(6);
    }
    println!("[stdfs] removed ok");

    // Reached only if every op above succeeded.
    println!("STD4 PASS");
}
