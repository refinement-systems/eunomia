// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

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
