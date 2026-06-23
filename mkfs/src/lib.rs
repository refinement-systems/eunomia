//! mkfs — host-side tool to build the initial disk image (rev2§7).
//!
//! Reuses the cas storage engine: format the device, create the `main`
//! ref, populate it from a host directory tree, take snapshot #1. The
//! result is byte-for-byte the same on-disk format the storage server
//! mounts in QEMU.
//!
//! The logic is in this lib so a host `cargo test` can drive the directory
//! walk in-process (rev2§6 Baseline tier); `src/main.rs` is the thin CLI
//! shell over [`run`].

use cas::dev::{BlockDev, FileDev};
use cas::store::{Store, StoreOptions};
use std::ffi::OsStr;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// File mtime as epoch-relative nanoseconds; `0` if it cannot be read.
pub fn mtime_nanos(md: &std::fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The tooling name-acceptance rule (rev2§4.9): tooling enforces the
/// printable-ASCII convention — every byte in `0x20..0x7F` and no `'/'` —
/// even though the format itself only excludes NUL and `'/'`. A name that
/// is not UTF-8, or carries a non-printable/`'/'` byte, is *not acceptable*
/// (`populate` skips it; it is never fatal). Returns the accepted name as
/// `&str` so the caller can use it directly.
pub fn name_acceptable(name: &OsStr) -> Option<&str> {
    let s = name.to_str()?;
    if s.bytes().all(|b| (0x20..0x7F).contains(&b)) && !s.contains('/') {
        Some(s)
    } else {
        None
    }
}

/// `StoreOptions` for the one-shot batch image build (rev2§4.4 — the numbers
/// are tunable). A deliberately modest WAL — *not* drift from the rev2§4.4
/// recommended 64 MiB. Two reasons it is tuned down for the batch tool:
/// recovery replay buffers the whole region and the on-OS server has a few
/// MiB of heap, not gigabytes (streaming replay is future work, tracked in
/// store.rs); and the recommended 64 MiB WAL would not even lay out within
/// the default 64 MiB image. The op-count and staleness *triggers* are
/// likewise pinned off: they are long-running-server memtable mechanisms,
/// meaningless for a single-shot image build that takes one snapshot at the
/// end, and staleness keyed off arbitrary historical host file mtimes would
/// inject nondeterministic intermediate commits. The byte budgets keep their
/// defaults — they bound peak populate memory by content.
pub fn batch_store_options() -> StoreOptions {
    StoreOptions {
        wal_len: 1024 * 1024,
        op_count_bound: u64::MAX,
        staleness_ns: u64::MAX,
        ..StoreOptions::default()
    }
}

/// Recursively walk `src`, writing every accepted regular file into `store`
/// under `prefix`. Entries are sorted by name (the determinism hinge — the
/// mount is a function of the *logical* tree, not host `read_dir` order);
/// names that fail [`name_acceptable`] and non-regular entries are skipped,
/// never fatal (rev2§4.9). Returns the count of regular files written.
///
/// Generic over the [`BlockDev`] backend so host tests can drive it against
/// an in-memory `MemDev` store; `run` instantiates it with `FileDev`.
pub fn populate<D: BlockDev>(
    store: &mut Store<D>,
    src: &Path,
    prefix: &mut Vec<Vec<u8>>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let mut count = 0;
    let mut entries: Vec<_> = std::fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name_str) = name_acceptable(&name) else {
            eprintln!("skipping name (non-UTF-8 or non-printable): {:?}", name);
            continue;
        };
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

/// Build the image: parse args, format the device, create the `main` ref,
/// populate it from the source tree, take snapshot #1. Usage:
/// `mkfs <image.img> <source-dir> [size-MiB (default 64)]`.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: mkfs <image.img> <source-dir> [size-MiB]".into());
    }
    let image = Path::new(&args[1]);
    let source = Path::new(&args[2]);
    let size_mib: u64 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(64);

    let dev = FileDev::create(image, size_mib * 1024 * 1024)?;
    let mut store = Store::format(dev, batch_store_options())?;
    store.create_ref(b"main")?;

    let mut prefix = Vec::new();
    let count = populate(&mut store, source, &mut prefix)?;

    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_nanos() as u64;
    let snap = store.snapshot(
        b"main",
        b"mkfs",
        b"initial image",
        cas::disk::CLASS_KEEP,
        now,
    )?;

    println!(
        "{}: {} files from {}, snapshot #{} on ref \"main\"",
        image.display(),
        count,
        source.display(),
        snap
    );
    Ok(())
}

#[cfg(test)]
mod tests;
