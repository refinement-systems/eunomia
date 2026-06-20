//! mkfs — host-side tool to build the initial disk image (rev1§7).
//!
//! Reuses the cas storage engine: format the device, create the `main`
//! ref, populate it from a host directory tree, take snapshot #1. The
//! result is byte-for-byte the same on-disk format the storage server
//! mounts in QEMU.
//!
//! Usage: mkfs <image.img> <source-dir> [size-MiB (default 64)]

use cas::dev::FileDev;
use cas::store::{Store, StoreOptions};
use std::path::Path;
use std::process::ExitCode;
use std::time::UNIX_EPOCH;

fn mtime_nanos(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn populate(
    store: &mut Store<FileDev>,
    src: &Path,
    prefix: &mut Vec<Vec<u8>>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut count = 0;
    let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            eprintln!("skipping non-UTF-8 name: {:?}", name);
            continue;
        };
        // Tooling enforces the printable-ASCII convention (rev1§4.9); the
        // format itself only excludes NUL and '/'.
        if !name_str.bytes().all(|b| (0x20..0x7F).contains(&b)) || name_str.contains('/') {
            eprintln!("skipping non-printable name: {:?}", name);
            continue;
        }
        let md = entry.metadata()?;
        prefix.push(name_str.as_bytes().to_vec());
        if md.is_dir() {
            count += populate(store, &entry.path(), prefix)?;
        } else if md.is_file() {
            let data = std::fs::read(entry.path())?;
            store.write(b"main", prefix, 0, &data, mtime_nanos(&md))?;
            count += 1;
        } else {
            eprintln!("skipping non-regular file: {:?}", entry.path());
        }
        prefix.pop();
    }
    Ok(count)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: mkfs <image.img> <source-dir> [size-MiB]".into());
    }
    let image = Path::new(&args[1]);
    let source = Path::new(&args[2]);
    let size_mib: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(64);

    let dev = FileDev::create(image, size_mib * 1024 * 1024)?;
    // A modest WAL: recovery replay buffers the whole region, and the
    // on-OS server has megabytes of heap, not gigabytes. (Streaming
    // replay remains future work, tracked in store.rs.)
    let opts = StoreOptions { wal_len: 1024 * 1024, ..StoreOptions::default() };
    let mut store = Store::format(dev, opts)?;
    store.create_ref(b"main")?;

    let mut prefix = Vec::new();
    let count = populate(&mut store, source, &mut prefix)?;

    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos() as u64;
    let snap = store.snapshot(b"main", b"mkfs", b"initial image", cas::disk::CLASS_KEEP, now)?;

    println!(
        "{}: {} files from {}, snapshot #{} on ref \"main\"",
        image.display(),
        count,
        source.display(),
        snap
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs: {e}");
            ExitCode::FAILURE
        }
    }
}
