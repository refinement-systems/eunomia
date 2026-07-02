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

fn main() {
    // The `bare_metal` cfg alias for the bare-metal aarch64 EL0 build
    // (aarch64-unknown-none / aarch64-unknown-eunomia): the single source of the
    // condition that otherwise repeats as
    // all(target_arch = "aarch64", any(target_os = "none", target_os = "eunomia")).
    // Declared unconditionally so `unexpected_cfgs` recognizes it; emitted only for
    // that target.
    println!("cargo:rustc-check-cfg=cfg(bare_metal)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if arch == "aarch64" && (os == "none" || os == "eunomia") {
        println!("cargo:rustc-cfg=bare_metal");
    }
}
