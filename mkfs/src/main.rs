//! mkfs — host-side tool to build the initial disk image (rev2§7).
//!
//! Thin CLI shell over [`mkfs::run`]; the walk/format logic lives in the
//! lib so a host `cargo test` can drive it in-process. Usage:
//! `mkfs <image.img> <source-dir> [size-MiB (default 64)]`.

use std::process::ExitCode;

fn main() -> ExitCode {
    match mkfs::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkfs: {e}");
            ExitCode::FAILURE
        }
    }
}
