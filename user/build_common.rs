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

// Shared bare-metal link-arg helpers for the `user/*` build scripts (rev2§5, rev2§7).
//
// The user binaries live in their own mini-workspaces, so this file is pulled into each
// build.rs with `include!("../build_common.rs")` rather than shared as a crate. Each
// build.rs composes the helpers to match its own host/target build shape (a plain `std`
// program, a `no_std`/`no_main` bin with a `cfg(test)` host harness, or a vendored-test
// driver). Every helper carries `#[allow(dead_code)]` because a given build.rs uses only
// the subset it needs.

/// The crate's manifest directory — where its `link.ld` lives.
#[allow(dead_code)]
fn manifest_dir() -> String {
    std::env::var("CARGO_MANIFEST_DIR").unwrap()
}

/// True on the bare-metal aarch64 EL0 target (`aarch64-unknown-none` /
/// `aarch64-unknown-eunomia`), where the custom `link.ld` + EL0 page size apply. A host
/// build (a `cargo test` libtest harness) links with the platform default instead; the
/// EL0 flags would break its `cc`/`clang` link.
#[allow(dead_code)]
fn is_bare_metal() -> bool {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    os == "none" || os == "eunomia"
}

/// Emit the EL0 linker script + page-size flag, scoped to `[[bin]]` targets so a host
/// `cargo test` libtest harness never receives them. Used by the bins that also build a
/// `cfg(test)` host harness (init/selftest/storaged/console).
#[allow(dead_code)]
fn link_el0_image_bins() {
    let dir = manifest_dir();
    println!("cargo:rustc-link-arg-bins=-T{dir}/link.ld");
    println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
}

/// Emit the EL0 linker script + page-size flag for every target artifact — for crates
/// with no host build path (a plain `std` program, or a vendored-test lib driver), or
/// gated behind [`is_bare_metal`] by a crate that early-returns on the host (shell).
#[allow(dead_code)]
fn link_el0_image() {
    let dir = manifest_dir();
    println!("cargo:rustc-link-arg=-T{dir}/link.ld");
    println!("cargo:rustc-link-arg=-zmax-page-size=4096");
}

/// Rebuild when the linker script or this shared helper changes. Every user build script
/// emits a `rerun-if-changed`, which opts out of cargo's default whole-package rescan, so
/// the shared file (outside the package dir) must be named explicitly.
#[allow(dead_code)]
fn rerun_inputs() {
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=../build_common.rs");
}
