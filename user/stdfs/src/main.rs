// SPDX-License-Identifier: 0BSD
//! The std-port fs GATE fixture: the first std binary
//! to drive the real `sys/fs/eunomia` client against storaged. It exercises the whole
//! file surface the gate names — create/write, read-back, `read_dir`, `rename`,
//! `remove_file`, and `sync_all` — plus the additions: directory/file
//! `metadata` (`is_dir`/`is_file`/`len`) and the errno split (a confinement escape →
//! `PermissionDenied`, a malformed name → `InvalidFilename`) — over the storaged
//! session the shell delegated to it (a fresh session storaged multiplexes, negotiated
//! by the client-side connect handshake at bootstrap), then prints the green marker
//! `STD4 PASS`.
//!
//! It is a real std program — no `#![no_std]`, no `#[panic_handler]`. std owns
//! `_start` (the eunomia PAL) and the panic handler; `extern crate eunomia_sys;`
//! forces the seam rlib into the link so the `__eunomia_*` (incl. `__eunomia_fs_*`)
//! symbols resolve. A failed op `panic!`s (reaped as STATUS_PANIC) or exits with a
//! distinct non-zero code, so the boot harness can tell exactly which step broke.

extern crate eunomia_sys; // links the PAL↔seam bridge (incl. the fs client)

use std::fs;
use std::io::{ErrorKind, Write};

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

    // 2b. the same file named through `.`/`..` resolves to it (the
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
    // A `..` escaping the root handle is a rev2§2.3 confinement violation, so the
    // errno split surfaces it as `PermissionDenied` (distinct from a
    // malformed name, below), never a wire round-trip.
    match fs::read("../escape") {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {}
        other => {
            println!("[stdfs] fs-bad escape kind {other:?}");
            std::process::exit(8);
        }
    }
    // A NUL in a component is unnameable but not an escape → `InvalidFilename`.
    match fs::read("a\0b") {
        Err(e) if e.kind() == ErrorKind::InvalidFilename => {}
        other => {
            println!("[stdfs] fs-bad malformed kind {other:?}");
            std::process::exit(9);
        }
    }
    println!("[stdfs] dotdot resolves; escape->denied, malformed->invalid");

    // 2c. a nameable path too long to frame in one 256-byte message (rev2§3.1): its
    //     components sit within the resolver's 255-byte / 64-depth bounds, so it resolves,
    //     but the encoded `Request` overflows `MAX_MSG`. After the write chunker caps the
    //     data payload the path is the only input that can do this, so the seam reports it
    //     as `InvalidFilename` (ENAMETOOLONG), not an opaque internal error.
    let long = "a".repeat(255);
    let unframable = format!("{long}/{long}");
    match fs::read(&unframable) {
        Err(e) if e.kind() == ErrorKind::InvalidFilename => {}
        other => {
            println!("[stdfs] fs-bad toolong kind {other:?}");
            std::process::exit(12);
        }
    }
    println!("[stdfs] toolong->invalid");

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

    // 3b. metadata: `docs/smoke` is a file of the written length;
    //     `docs` is a directory. The directory type comes from the seam's Stat->List
    //     probe (a directory has no file content, so `Stat` reports it absent and
    //     `List` confirms the directory, rev2§4.9).
    let fm = fs::metadata(path).expect("metadata docs/smoke");
    if !fm.is_file() || fm.is_dir() || fm.len() != content.len() as u64 {
        println!(
            "[stdfs] fs-bad file metadata is_file={} is_dir={} len={}",
            fm.is_file(),
            fm.is_dir(),
            fm.len()
        );
        std::process::exit(10);
    }
    let dm = fs::metadata("docs").expect("metadata docs");
    if !dm.is_dir() || dm.is_file() {
        println!(
            "[stdfs] fs-bad dir metadata is_dir={} is_file={}",
            dm.is_dir(),
            dm.is_file()
        );
        std::process::exit(11);
    }
    println!("[stdfs] metadata ok");

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
