// SPDX-License-Identifier: 0BSD
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
